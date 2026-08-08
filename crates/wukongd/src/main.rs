//! wukongd: the governor daemon. One tokio task multiplexes three
//! streams — filesystem events (debounced into commits), a debounce
//! timer, and a push timer — while a unix-socket server answers
//! clients. The engine is the single owner of all state; this file is
//! just the event loop and the socket.

mod engine;
mod notify_user;
mod watcher;

use engine::Engine;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use wukong_core::ipc::{Envelope, PROTOCOL_VERSION, Response};
use wukong_core::{Config, paths};

enum Msg {
    Fs(std::path::PathBuf),
    Client {
        line: String,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    DebounceTick,
    PushTick,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = Config::load();
    if config.machine.is_empty() {
        eprintln!("wukongd: not initialized — run `wukong init` first");
        std::process::exit(1);
    }

    let mut engine = Engine::new(config)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();

    // Filesystem watcher → Msg::Fs.
    let (mut fs_watcher, mut fs_rx) = watcher::FsWatcher::start()?;
    for root in engine.initial_watch_roots() {
        fs_watcher.watch(&root);
    }
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(path) = fs_rx.recv().await {
                let _ = tx.send(Msg::Fs(path));
            }
        });
    }

    // Debounce + push timers.
    spawn_timer(tx.clone(), Duration::from_secs(1), || Msg::DebounceTick);
    let push_interval = Duration::from_secs(engine.config.push_interval_secs.max(10));
    spawn_timer(tx.clone(), push_interval, || Msg::PushTick);

    // Socket server → Msg::Client.
    let socket = paths::socket_file();
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
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

    // The one loop that owns the engine.
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Fs(path) => engine.touch(path),
            Msg::DebounceTick => {
                let new_items = engine.tick();
                if notify_on && new_items > 0 {
                    notify_user::inbox(new_items);
                }
            }
            Msg::PushTick => engine.maybe_push(),
            Msg::Client { line, reply } => {
                let _ = reply.send(process(&mut engine, &line));
                // Tracking a new file asks the loop to watch its dir.
                for dir in engine.drain_watch_requests() {
                    fs_watcher.watch(&dir);
                }
            }
        }
    }
    Ok(())
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

fn process(engine: &mut Engine, line: &str) -> String {
    let response = match serde_json::from_str::<Envelope>(line) {
        Ok(env) if env.v == PROTOCOL_VERSION => engine.handle(env.req),
        Ok(_) => Response::Error {
            message: "protocol version mismatch — restart wukongd".to_string(),
        },
        Err(e) => Response::Error {
            message: format!("bad request: {e}"),
        },
    };
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
