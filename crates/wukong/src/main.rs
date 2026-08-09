//! wukong: the command-line and terminal face of the governor. Bare
//! `wukong` opens the TUI dashboard; the verbs are thin wrappers over
//! the daemon socket, so scripts and prompts get the same governor the
//! dashboard drives.

mod adopt;
mod client;
mod init;
mod launchd;
mod pkg_cli;
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
    /// Install via brew and remember it in the manifest.
    Install {
        name: String,
        /// Install as a cask (GUI app).
        #[arg(long)]
        cask: bool,
        /// Install without recording ("don't track this one").
        #[arg(long)]
        no_track: bool,
    },
    /// Uninstall via brew and drop it from the manifest.
    Rm {
        name: String,
        #[arg(long)]
        cask: bool,
    },
    /// Package manifest: list, sync, adopt, ignore.
    Pkg {
        #[command(subcommand)]
        action: PkgAction,
    },
    /// Start tracking files — their changes commit automatically.
    Track {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Find this machine's well-known dotfiles and track them all.
    AdoptDotfiles {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Stop offering anything under a path (sentinel noise valve).
    Exclude { path: String },
    /// Show what changed: live file vs the stored copy.
    Diff { path: String },
    /// The store's commit history for a tracked file.
    Log {
        path: String,
        /// How many commits to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
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

#[derive(Subcommand)]
pub enum PkgAction {
    /// Manifest entries and whether each is actually installed.
    List,
    /// Install everything in the manifest that's missing.
    Sync {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Bulk-adopt everything brew currently has on request.
    AdoptInstalled,
    /// Never offer this package again.
    Ignore {
        name: String,
        #[arg(long)]
        cask: bool,
        #[arg(long)]
        app: bool,
    },
    /// Allow a previously ignored package to be offered again.
    Unignore {
        name: String,
        #[arg(long)]
        cask: bool,
        #[arg(long)]
        app: bool,
    },
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
        Some(Command::Install {
            name,
            cask,
            no_track,
        }) => pkg_cli::install(&name, cask, no_track),
        Some(Command::Rm { name, cask }) => pkg_cli::rm(&name, cask),
        Some(Command::Pkg { action }) => match action {
            PkgAction::List => pkg_cli::list(),
            PkgAction::Sync { yes } => pkg_cli::sync(yes),
            PkgAction::AdoptInstalled => pkg_cli::adopt_installed(),
            PkgAction::Ignore { name, cask, app } => pkg_cli::ignore(&name, cask, app, false),
            PkgAction::Unignore { name, cask, app } => pkg_cli::ignore(&name, cask, app, true),
        },
        Some(Command::Track { paths }) => {
            for path in paths {
                say(Request::Track { path })?;
            }
            Ok(())
        }
        Some(Command::AdoptDotfiles { yes }) => adopt::run(yes),
        Some(Command::Exclude { path }) => say(Request::Exclude { path }),
        Some(Command::Diff { path }) => say(Request::Diff { path }),
        Some(Command::Log { path, limit }) => say(Request::FileLog { path, limit }),
        Some(Command::Untrack { path }) => say(Request::Untrack { path }),
        Some(Command::Status) => status(),
        Some(Command::Files) => files(),
        Some(Command::Inbox) => inbox(),
        Some(Command::Resolve { id, resolution }) => say(Request::InboxResolve { id, resolution }),
        Some(Command::Push) => say(Request::PushNow),
        Some(Command::Restore { path, force }) => say(Request::Restore { path, force }),
        Some(Command::Daemon { action }) => launchd::run(action),
        Some(Command::Doctor) => {
            doctor();
            Ok(())
        }
    }
}

/// Fire a request whose success is just a message.
pub fn say(req: Request) -> anyhow::Result<()> {
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
    println!("last push {}", age_of(s.last_push.as_deref()));
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

fn doctor() {
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
    if !config.remote.is_empty() {
        check(remote_reachable(&config.remote), "remote reachable");
    }
    check(client::connected(), "daemon running");
    check(launchd::agent_path().exists(), "launchd agent installed");
    if client::connected()
        && let Ok(Response::Status(s)) = client::call(Request::Status)
    {
        println!(
            "\n{} tracked · {} inbox · {} unpushed · last push {}",
            s.tracked,
            s.inbox,
            s.unpushed,
            age_of(s.last_push.as_deref())
        );
    }
}

/// "2h ago" from an RFC3339 timestamp — the remote-machine question
/// is "is it still syncing", and a raw timestamp doesn't answer it.
fn age_of(ts: Option<&str>) -> String {
    let Some(ts) = ts else {
        return "(never)".to_string();
    };
    let Ok(then) = ts.parse::<jiff::Timestamp>() else {
        return ts.to_string();
    };
    let secs = jiff::Timestamp::now().duration_since(then).as_secs();
    match u64::try_from(secs) {
        Ok(secs) => format!("{} ago", human_secs(secs)),
        Err(_) => ts.to_string(),
    }
}

/// Prompt-free, fast-failing reachability probe.
fn remote_reachable(remote: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-remote", "--heads", remote])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        )
        .output()
        .is_ok_and(|o| o.status.success())
}

fn human_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}
