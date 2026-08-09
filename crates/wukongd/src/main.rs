//! wukongd: the governor daemon. One tokio task multiplexes the
//! streams — filesystem signals (debounced into commits), a debounce
//! timer, a push timer, and push completions — while a unix-socket
//! server answers clients. The engine is the single owner of all
//! state; this file is just the event loop and the socket. The only
//! work that leaves the loop is the network push, which runs on a
//! blocking task so a slow remote can never wedge the governor.
//!
//! Client tasks parse and encode JSON at the edge; the loop itself
//! speaks only typed `Request`s. Signals become a message like
//! everything else, so shutdown is a normal exit from the loop with
//! cleanup in one place.

mod engine;
mod notify_user;
mod watcher;

use engine::Engine;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use watcher::WatchSignal;
use wukong_core::ipc::{Envelope, PROTOCOL_VERSION, Request, Response};
use wukong_core::{Config, paths};

/// Full-rescan heartbeat: belt-and-braces for FSEvents gaps across
/// sleep/wake on a machine nobody is watching.
#[allow(clippy::duration_suboptimal_units)] // Duration::from_hours is unstable
const RESCAN_INTERVAL: Duration = Duration::from_secs(6 * 3600);

enum Msg {
    Fs(std::path::PathBuf),
    Rescan,
    Client {
        req: Request,
        reply: tokio::sync::oneshot::Sender<Response>,
    },
    DebounceTick,
    PushTick,
    PushDone(Result<(), String>),
    Shutdown,
}

/// `launchd` never rotates the log it redirects our stderr into; a
/// long-lived daemon must cap it itself. Truncate-keeping-tail at
/// startup (`KeepAlive` restarts make startup a regular event).
/// `launchd` opens the file `O_APPEND`, so writes continue correctly
/// at the new, smaller end.
fn cap_log_size() {
    const MAX: u64 = 5 * 1024 * 1024;
    const KEEP: usize = 512 * 1024;
    let log = paths::state_dir().join("wukongd.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > MAX)
        && let Ok(data) = std::fs::read(&log)
    {
        let tail = &data[data.len().saturating_sub(KEEP)..];
        let _ = std::fs::write(&log, tail);
    }
}

/// Load the config or explain why the daemon cannot run.
fn load_config_or_exit() -> Config {
    match Config::load() {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!("wukongd: not initialized — run `wukong init` first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("wukongd: {e}");
            std::process::exit(1);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = load_config_or_exit();
    cap_log_size();

    // Single instance, atomically: an OS-level file lock held for the
    // process lifetime. The connect probe alone is a TOCTOU — two
    // simultaneous startups both pass it, and the second silently
    // steals the socket.
    let socket = paths::socket_file();
    if let Some(dir) = socket.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(socket.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            eprintln!("wukongd: another instance is already running");
            std::process::exit(1);
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(anyhow::anyhow!("cannot take the instance lock: {e}"));
        }
    }

    let mut engine = Engine::new(config, &paths::db_file(), &paths::store_dir())?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    // Clients whose `wukong push` joined an in-flight push: they get
    // the real result when it lands, not an early "in progress" lie.
    let mut push_waiters: Vec<tokio::sync::oneshot::Sender<Response>> = Vec::new();

    // Filesystem watcher → Msg::Fs / Msg::Rescan.
    let (mut fs_watcher, fs_rx) = watcher::FsWatcher::start()?;
    for (root, recursive) in engine.initial_watch_roots() {
        fs_watcher.watch(&root, recursive);
    }
    bridge_watcher(fs_rx, tx.clone());

    // Debounce + push timers, plus a slow full-rescan heartbeat:
    // belt-and-braces for FSEvents gaps across sleep/wake on a machine
    // nobody is watching. Unchanged content settles into no-ops.
    spawn_timer(tx.clone(), Duration::from_secs(1), || Msg::DebounceTick);
    let push_interval = Duration::from_secs(engine.config.push_interval_secs.max(10));
    spawn_timer(tx.clone(), push_interval, || Msg::PushTick);
    spawn_timer(tx.clone(), RESCAN_INTERVAL, || Msg::Rescan);

    serve_socket(&socket, tx.clone())?;

    spawn_signal_handler(tx.clone());

    let notify_on = engine.config.notifications;

    // Catch anything installed or changed while the daemon was down —
    // those count as new items worth a notification too.
    let startup_items = engine.reconcile();
    if notify_on && startup_items > 0 {
        notify_user::inbox(startup_items);
    }

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
                    start_push(&mut engine, tx.clone());
                }
            }
            Msg::PushDone(result) => {
                let response = match &result {
                    Ok(()) => Response::Ok {
                        message: "pushed".to_string(),
                    },
                    Err(e) => Response::Error {
                        message: format!("push failed: {e}"),
                    },
                };
                for waiter in push_waiters.drain(..) {
                    let _ = waiter.send(response.clone());
                }
                engine.finish_push(result);
            }
            Msg::Client { req, reply } => {
                match req {
                    // Every push reply — initiator or joiner — flows
                    // through push_waiters and is answered by PushDone
                    // with the push's actual outcome. One path.
                    Request::PushNow if engine.remote_configured() => {
                        push_waiters.push(reply);
                        if !engine.push_in_flight() {
                            start_push(&mut engine, tx.clone());
                        }
                    }
                    Request::PushNow => {
                        let _ = reply.send(Response::Error {
                            message: "no remote configured — set one in config.toml and restart"
                                .to_string(),
                        });
                    }
                    req => {
                        let _ = reply.send(engine.handle(req));
                    }
                }
            }
            Msg::Shutdown => break,
        }
        // Watch requests come from client commands AND from the engine's
        // own event handling — a sentinel promoted to a directory during
        // `touch`, Homebrew appearing during a reconcile tick. Drain
        // after every message, not just client ones, or those wait for
        // the next CLI invocation to be watched (free when empty).
        for (dir, recursive) in engine.drain_watch_requests() {
            fs_watcher.watch(&dir, recursive);
        }
    }

    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Run the push on the blocking pool; completion returns to the loop
/// as `PushDone`, which answers every waiting client from the actual
/// result.
fn start_push(engine: &mut Engine, tx: mpsc::UnboundedSender<Msg>) {
    let store = engine.begin_push();
    tokio::task::spawn_blocking(move || {
        let result = store.push().map_err(|e| e.to_string());
        let _ = tx.send(Msg::PushDone(result));
    });
}

/// Signals are just another message; the loop exits and cleans up.
fn spawn_signal_handler(tx: mpsc::UnboundedSender<Msg>) {
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler installs");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        let _ = tx.send(Msg::Shutdown);
    });
}

/// Forward watcher signals into the engine loop's mailbox.
fn bridge_watcher(mut fs_rx: mpsc::UnboundedReceiver<WatchSignal>, tx: mpsc::UnboundedSender<Msg>) {
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

/// Bind the unix socket (0600 in the 0700 state dir) and accept
/// clients forever, backing off on accept errors instead of spinning.
fn serve_socket(socket: &std::path::Path, tx: mpsc::UnboundedSender<Msg>) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve_client(stream, tx.clone()));
                }
                Err(e) => {
                    eprintln!("wukongd: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
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

/// One connected client: JSON lines in, JSON lines out. Parsing and
/// version checks happen here, at the edge — the loop never sees a
/// malformed request.
async fn serve_client(stream: tokio::net::UnixStream, tx: mpsc::UnboundedSender<Msg>) {
    let (read, mut write) = stream.into_split();
    // Bound what one connection may buffer: a client that streams
    // forever without a newline must not grow the daemon unboundedly.
    let mut lines = BufReader::new(read.take(1024 * 1024)).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let response = match parse(&line) {
            Ok(req) => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(Msg::Client {
                        req,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    break;
                }
                match reply_rx.await {
                    Ok(response) => response,
                    Err(_) => break,
                }
            }
            Err(response) => *response,
        };
        let mut encoded = serde_json::to_string(&response)
            .unwrap_or_else(|_| r#"{"res":"error","message":"encode failed"}"#.to_string());
        encoded.push('\n');
        if write.write_all(encoded.as_bytes()).await.is_err() {
            break;
        }
    }
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
