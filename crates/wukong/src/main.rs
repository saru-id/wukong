//! wukong: the command-line and terminal face of the governor. Bare
//! `wukong` opens the TUI dashboard; the verbs are thin wrappers over
//! the daemon socket, so scripts and prompts get the same governor the
//! dashboard drives.

mod client;
mod init;
mod launchd;
mod tui;

use clap::{Parser, Subcommand};
use wukong_core::events::Resolution;
use wukong_core::ipc::{Request, Response};

#[derive(Parser)]
#[command(
    name = "wukong",
    version,
    about = "Your system's governor: dotfiles, packages, and settings, watched and remembered."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Set up wukong on this machine (config, store, launchd agent).
    Init,
    /// Start tracking a file — its changes commit automatically.
    Track { path: String },
    /// Stop tracking a file.
    Untrack { path: String },
    /// Show what wukong is watching and where it stands.
    Status,
    /// List tracked files.
    Files,
    /// Show the review inbox.
    Inbox,
    /// Resolve an inbox item: approve, redact, or ignore.
    Resolve {
        id: i64,
        #[arg(value_parser = parse_resolution)]
        resolution: Resolution,
    },
    /// Push the store now.
    Push,
    /// Copy stored files back to their live locations (new-machine
    /// bootstrap). No path restores everything.
    Restore {
        path: Option<String>,
        /// Overwrite live files that differ from the stored copy.
        #[arg(long)]
        force: bool,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Check the health of the whole setup.
    Doctor,
}

#[derive(Subcommand, Clone, Copy)]
pub enum DaemonAction {
    Start,
    Stop,
    Restart,
    Status,
}

fn parse_resolution(s: &str) -> Result<Resolution, String> {
    Resolution::parse(s).ok_or_else(|| "expected approve, redact, or ignore".to_string())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(),
        Some(Command::Init) => init::run(),
        Some(Command::Track { path }) => say(Request::Track { path }),
        Some(Command::Untrack { path }) => say(Request::Untrack { path }),
        Some(Command::Status) => status(),
        Some(Command::Files) => files(),
        Some(Command::Inbox) => inbox(),
        Some(Command::Resolve { id, resolution }) => say(Request::InboxResolve { id, resolution }),
        Some(Command::Push) => say(Request::PushNow),
        Some(Command::Restore { path, force }) => say(Request::Restore { path, force }),
        Some(Command::Daemon { action }) => launchd::run(action),
        Some(Command::Doctor) => doctor(),
    }
}

/// Fire a request whose success is just a message.
fn say(req: Request) -> anyhow::Result<()> {
    match client::call(req)? {
        Response::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        other => {
            println!("{other:?}");
            Ok(())
        }
    }
}

fn status() -> anyhow::Result<()> {
    let Response::Status(s) = client::call(Request::Status)? else {
        anyhow::bail!("unexpected response");
    };
    println!("machine   {}", s.machine);
    println!(
        "remote    {}",
        if s.remote.is_empty() {
            "(local only)"
        } else {
            &s.remote
        }
    );
    println!("tracked   {} files", s.tracked);
    println!(
        "inbox     {} item{}",
        s.inbox,
        if s.inbox == 1 { "" } else { "s" }
    );
    println!("unpushed  {} commit(s)", s.unpushed);
    println!("uptime    {}", human_secs(s.uptime_secs));
    Ok(())
}

fn files() -> anyhow::Result<()> {
    let Response::Tracked { files } = client::call(Request::TrackedList)? else {
        anyhow::bail!("unexpected response");
    };
    if files.is_empty() {
        println!("nothing tracked yet — `wukong track ~/.zshrc`");
    }
    for f in files {
        let mark = if f.exists { " " } else { "!" };
        println!("{mark} {}", f.display);
    }
    Ok(())
}

fn inbox() -> anyhow::Result<()> {
    let Response::Inbox { items } = client::call(Request::InboxList)? else {
        anyhow::bail!("unexpected response");
    };
    if items.is_empty() {
        println!("inbox is clear");
        return Ok(());
    }
    for item in items {
        println!("#{}  {}  {}", item.id, item.subject, item.detail);
    }
    println!("\nresolve with `wukong resolve <id> approve|redact|ignore`");
    Ok(())
}

fn doctor() -> anyhow::Result<()> {
    use wukong_core::{Config, paths};
    let check = |ok: bool, label: &str| println!("{} {label}", if ok { "✓" } else { "✗" });

    let config = match Config::load() {
        Ok(Some(config)) => {
            check(true, "config parses");
            config
        }
        Ok(None) => {
            check(false, "config missing — run `wukong init`");
            Config::default()
        }
        Err(e) => {
            check(false, &format!("config broken: {e}"));
            Config::default()
        }
    };
    check(!config.machine.is_empty(), "initialized (machine name set)");
    check(
        paths::store_dir().join(".git").exists(),
        "store repo exists",
    );
    check(!config.remote.is_empty(), "remote configured");
    check(client::connected(), "daemon running");
    check(launchd::agent_path().exists(), "launchd agent installed");
    if client::connected()
        && let Ok(Response::Status(s)) = client::call(Request::Status)
    {
        println!(
            "\n{} tracked · {} inbox · {} unpushed",
            s.tracked, s.inbox, s.unpushed
        );
    }
    Ok(())
}

fn human_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}
