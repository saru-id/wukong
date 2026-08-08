//! wukongd: the governor daemon. One tokio task multiplexes the
//! streams — filesystem signals (debounced into commits), a debounce
//! timer, a push timer, and push completions — while a unix-socket
//! server answers clients. The engine is the single owner of all
//! state; this file is just the event loop and the socket. The only
//! work that leaves the loop is the network push, which runs on a
//! blocking task so a slow remote can never wedge the governor.

mod engine;
mod notify_user;
mod watcher;

use engine::Engine;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use watcher::WatchSignal;
use wukong_core::ipc::{Envelope, PROTOCOL_VERSION, Request, Response};
use wukong_core::{Config, paths};

enum Msg {
    Fs(std::path::PathBuf),
    Rescan,
    Client {
        line: String,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    DebounceTick,
    PushTick,
    PushDone(Result<(), String>),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = match Config::load() {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!("wukongd: not initialized — run `wukong init` first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("wukongd: {e}");
            std::process::exit(1);
        }
    };

    // Single instance: if a daemon already answers on the socket, this
    // one must not steal it — launchd KeepAlive plus a manual start
    // would otherwise run two engines over one store.
    let socket = paths::socket_file();
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        eprintln!("wukongd: another instance is already running");
        std::process::exit(1);
    }

    let mut engine = Engine::new(config, &paths::db_file(), &paths::store_dir())?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Filesystem watcher → Msg::Fs / Msg::Rescan.
    let (mut fs_watcher, mut fs_rx) = watcher::FsWatcher::start()?;
    for (root, recursive) in engine.initial_watch_roots() {
        fs_watcher.watch(&root, recursive);
    }
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(signal) = fs_rx.recv().await {
                let msg = match signal {
                    WatchSignal::Touched(path) => Msg::Fs(path),
                    WatchSignal::Rescan => Msg::Rescan,
                };
                let _ = tx.send(msg);
            }
        });
    }

    // Debounce + push timers.
    spawn_timer(tx.clone(), Duration::from_secs(1), || Msg::DebounceTick);
    let push_interval = Duration::from_secs(engine.config.push_interval_secs.max(10));
    spawn_timer(tx.clone(), push_interval, || Msg::PushTick);

    // Socket server → Msg::Client. The state dir and socket are ours
    // alone: 0700 on the dir, 0600 on the socket.
    if let Some(dir) = socket.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(handle_client(stream, tx.clone()));
            }
        });
    }

    // Clean shutdown removes the socket.
    {
        let socket = socket.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            let _ = std::fs::remove_file(&socket);
            std::process::exit(0);
        });
    }

    let notify_on = engine.config.notifications;

    // Catch anything installed or changed while the daemon was down.
    engine.reconcile();

    // The one loop that owns the engine.
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Fs(path) => engine.touch(path),
            Msg::Rescan => engine.rescan(),
            Msg::DebounceTick => {
                let new_items = engine.tick();
                if notify_on && new_items > 0 {
                    notify_user::inbox(new_items);
                }
            }
            Msg::PushTick => {
                if engine.wants_push() {
                    start_push(&mut engine, tx.clone(), None);
                }
            }
            Msg::PushDone(result) => engine.finish_push(result),
            Msg::Client { line, reply } => {
                match parse(&line) {
                    Ok(Request::PushNow) => handle_push_now(&mut engine, tx.clone(), reply),
                    Ok(req) => {
                        let _ = reply.send(encode(engine.handle(req)));
                    }
                    Err(response) => {
                        let _ = reply.send(encode(*response));
                    }
                }
                // Tracking a new file asks the loop to watch its dir.
                for dir in engine.drain_watch_requests() {
                    fs_watcher.watch(&dir, false);
                }
            }
        }
    }
    Ok(())
}

/// Run the push on the blocking pool; completion returns to the loop
/// as a message, and the optional client reply is answered truthfully
/// from the actual result.
fn start_push(
    engine: &mut Engine,
    tx: mpsc::UnboundedSender<Msg>,
    reply: Option<tokio::sync::oneshot::Sender<String>>,
) {
    let store = engine.begin_push();
    tokio::task::spawn_blocking(move || {
        let result = store.push().map_err(|e| e.to_string());
        if let Some(reply) = reply {
            let response = match &result {
                Ok(()) => Response::Ok {
                    message: "pushed".to_string(),
                },
                Err(e) => Response::Error {
                    message: format!("push failed: {e}"),
                },
            };
            let _ = reply.send(encode(response));
        }
        let _ = tx.send(Msg::PushDone(result));
    });
}

fn handle_push_now(
    engine: &mut Engine,
    tx: mpsc::UnboundedSender<Msg>,
    reply: tokio::sync::oneshot::Sender<String>,
) {
    let response = if !engine.remote_configured() {
        Some(Response::Error {
            message: "no remote configured — set one in config.toml and restart".to_string(),
        })
    } else if engine.push_in_flight() {
        Some(Response::Ok {
            message: "push already in progress".to_string(),
        })
    } else {
        None
    };
    match response {
        Some(r) => {
            let _ = reply.send(encode(r));
        }
        None => start_push(engine, tx, Some(reply)),
    }
}

fn spawn_timer<F>(tx: mpsc::UnboundedSender<Msg>, period: Duration, make: F)
where
    F: Fn() -> Msg + Send + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if tx.send(make()).is_err() {
                break;
            }
        }
    });
}

fn parse(line: &str) -> Result<Request, Box<Response>> {
    match serde_json::from_str::<Envelope>(line) {
        Ok(env) if env.v == PROTOCOL_VERSION => Ok(env.req),
        Ok(_) => Err(Box::new(Response::Error {
            message: "protocol version mismatch — restart wukongd".to_string(),
        })),
        Err(e) => Err(Box::new(Response::Error {
            message: format!("bad request: {e}"),
        })),
    }
}

fn encode(response: Response) -> String {
    serde_json::to_string(&response).unwrap_or_else(|_| "{\"res\":\"error\"}".to_string())
}

async fn handle_client(stream: tokio::net::UnixStream, tx: mpsc::UnboundedSender<Msg>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if tx
            .send(Msg::Client {
                line,
                reply: reply_tx,
            })
            .is_err()
        {
            break;
        }
        if let Ok(mut response) = reply_rx.await {
            response.push('\n');
            if write.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    }
}
