//! Setup, in two shapes. `ensure_ready` is the invisible one: the
//! first real command on a fresh machine sets everything up silently
//! (config, store, daemon — local-only, zero questions) so "setup"
//! stops being a step anyone performs. `wukong init` remains as the
//! explicit verb — the interactive repair-and-automation path that
//! also prompts for a remote and offers adopt/sync — but nobody has
//! to know it exists.

use std::io::{self, Write as _};
use wukong_core::{Config, Store, paths};

pub fn run(yes: bool) -> anyhow::Result<()> {
    let mut config = match Config::load() {
        Ok(Some(config)) => config,
        Ok(None) => Config::default(),
        Err(e) => anyhow::bail!("{e}\nfix the file (or delete it) and run `wukong init` again"),
    };
    let fresh = config.source.is_none();

    if config.machine.is_empty() {
        config.machine = detect_machine();
    }
    let mut remote_added = false;
    if config.remote.is_empty() {
        config.remote = prompt("Store remote (git URL; press Enter for local-only): ");
        remote_added = !config.remote.is_empty();
    }
    if !config.remote.is_empty() && !remote_reachable(&config.remote) {
        if yes {
            // Unattended runs can't wait on a browser; keep the remote
            // and let pushes retry.
            println!(
                "  note: could not reach {} right now — keeping it; pushes will retry",
                config.remote
            );
        } else {
            ssh_wizard(&config.remote);
        }
    }
    if fresh {
        // A fresh machine gets the fully-commented starter: the config
        // file is its own manual.
        let path = Config::write_starter(&config.machine, &config.remote)?;
        config.source = Some(path);
    } else if remote_added {
        // An existing file is edited surgically — comments survive.
        config.persist_remote(&config.remote)?;
    }
    println!("✓ config at {}", paths::display(&paths::config_file()));

    let store_dir = paths::store_dir();
    let mut cloned_files = 0usize;
    if !store_dir.join(".git").exists() && !config.remote.is_empty() {
        // A remote plus no local store = a new machine joining an
        // existing store. Clone, branch off, and offer the restore.
        match Store::clone_from(&config.remote, &store_dir, &config.machine) {
            Ok(store) => {
                cloned_files = store.files().map_or(0, |f| f.len())
                    + store.shared().files().map_or(0, |f| f.len());
                println!(
                    "✓ cloned store from {} (branch {}, {} file(s) incl. shared)",
                    config.remote, config.machine, cloned_files
                );
            }
            Err(e) => {
                // Do NOT fall back to a fresh store: if the remote has
                // real history and merely failed to clone (network,
                // auth), a fresh store would diverge from it forever.
                anyhow::bail!(
                    "cloning {} failed: {e}\n\
                     fix the remote or your access and run `wukong init` again\n\
                     (for a brand-new empty store, create the repo first or leave the remote blank)",
                    config.remote
                );
            }
        }
    } else {
        Store::open(&store_dir, &config.machine)?;
    }
    println!(
        "✓ store repo at {} (branch {})",
        paths::display(&store_dir),
        config.machine
    );

    start_daemon()?;

    // The right next step, offered here so setup is ONE command. Both
    // paths show their full plan and take one confirmation (--yes
    // accepts everything, for unattended installs).
    if wait_for_daemon() {
        if cloned_files > 0 {
            println!("\nThe store already has this machine's world — syncing it on:");
            crate::sync::run(yes, false)?;
        } else if fresh {
            println!("\nTaking in what's already on this machine:");
            crate::adopt::run(yes)?;
        }
    } else {
        println!("  note: daemon not answering yet — `wukong sync` or `wukong adopt` when it is");
    }

    if fresh {
        println!("\nwukong is governing {}.", config.machine);
        println!("Open the dashboard:  wukong");
    } else {
        println!("\nRepair complete.");
    }
    Ok(())
}

/// Everything a fresh machine needs, with zero questions: config
/// (local-only), store, daemon. The first real command calls this so
/// setup stops being a step. Returns true when it actually set up.
pub fn ensure_ready() -> anyhow::Result<bool> {
    if !matches!(Config::load(), Ok(None)) {
        return Ok(false);
    }
    println!("First run — setting this machine up…");
    let machine = detect_machine();
    Config::write_starter(&machine, "")?;
    Store::open(&paths::store_dir(), &machine)?;
    start_daemon()?;
    wait_for_daemon();
    println!("✓ config, store, and daemon ready (machine: {machine}, local-only)");
    println!(
        "  `wukong remote <url>` adds backup/sync whenever you're ready
"
    );
    Ok(true)
}

/// The one-time welcome when the FIRST command was the bare dashboard:
/// two skippable questions, then out of the way forever.
pub fn first_run_welcome() {
    use std::io::IsTerminal as _;
    if !io::stdin().is_terminal() {
        return;
    }
    let answer = prompt("Adopt this machine's dotfiles and installed packages now? [Y/n] ");
    if !answer.eq_ignore_ascii_case("n") {
        let _ = crate::adopt::run(true);
    }
    let url = prompt(
        "
Backup/sync remote (git URL; Enter to skip): ",
    );
    if !url.is_empty() {
        let _ = crate::remote::set(&url);
    }
    println!();
}

/// Start the daemon: launchd in real life, a direct sibling spawn
/// under `WUKONG_NO_AGENT` (the drills' escape hatch — pid in the state
/// dir so sandboxes can stop and restart it).
pub(crate) fn start_daemon() -> anyhow::Result<()> {
    if std::env::var_os("WUKONG_NO_AGENT").is_some() {
        let daemon = std::env::current_exe()?
            .parent()
            .map(|d| d.join("wukongd"))
            .filter(|p| p.is_file())
            .ok_or_else(|| anyhow::anyhow!("wukongd not found beside wukong"))?;
        paths::ensure_private_dir(&paths::state_dir())?;
        let child = std::process::Command::new(daemon)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        std::fs::write(
            paths::state_dir().join("wukongd.pid"),
            child.id().to_string(),
        )?;
        println!("✓ daemon started directly (WUKONG_NO_AGENT)");
    } else {
        crate::launchd::install()?;
        println!("✓ launchd agent installed and started");
    }
    Ok(())
}

/// Bounce the daemon so it rereads the config (a newly attached
/// remote, most importantly).
pub(crate) fn restart_daemon() {
    if std::env::var_os("WUKONG_NO_AGENT").is_some() {
        let pidfile = paths::state_dir().join("wukongd.pid");
        if let Ok(pid) = std::fs::read_to_string(&pidfile) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let _ = start_daemon();
    } else if crate::launchd::agent_path().exists() {
        crate::launchd::kickstart();
    }
    wait_for_daemon();
}

/// The launchd agent was just kicked; give the socket a moment.
pub(crate) fn wait_for_daemon() -> bool {
    for _ in 0..25 {
        if crate::client::connected() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

pub(crate) fn detect_machine() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this-mac".to_string())
        .to_lowercase()
        .replace(' ', "-")
}

/// A quick, prompt-free reachability check; failure is advisory only
/// (the machine may simply be offline right now).
pub(crate) fn remote_reachable(remote: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-remote", "--heads", remote])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A clean Mac has no way to reach a private git remote yet. Guide
/// the whole path here — find or create an SSH key, hand over the
/// public half, re-probe — instead of failing with a git error and
/// homework.
pub(crate) fn ssh_wizard(remote: &str) {
    println!("  {remote} is not reachable from this machine yet.");
    let ssh_dir = paths::home().join(".ssh");
    let pubkey = ["id_ed25519.pub", "id_rsa.pub", "id_ecdsa.pub"]
        .iter()
        .map(|name| ssh_dir.join(name))
        .find(|p| p.is_file());
    let pubkey = pubkey.or_else(|| {
        let answer = prompt("  No SSH key found — generate one for this machine? [Y/n] ");
        if answer.eq_ignore_ascii_case("n") {
            return None;
        }
        let key = ssh_dir.join("id_ed25519");
        let made = std::process::Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .arg("-C")
            .arg(format!("{}@wukong", detect_machine()))
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        made.then(|| key.with_extension("pub"))
    });
    let Some(pubkey) = pubkey else {
        println!("  keeping the remote; pushes will retry once access works");
        return;
    };
    if let Ok(key_line) = std::fs::read_to_string(&pubkey) {
        println!(
            "
  This machine's public key:

    {}",
            key_line.trim()
        );
        if remote.contains("github.com") {
            println!(
                "
  Add it at: https://github.com/settings/keys"
            );
        } else {
            println!(
                "
  Add it to your git host's authorized keys."
            );
        }
    }
    for attempt in 0..3 {
        let answer = prompt(
            "
  Press Enter once added (or type skip): ",
        );
        if answer.eq_ignore_ascii_case("skip") {
            break;
        }
        if remote_reachable(remote) {
            println!("  ✓ remote reachable");
            return;
        }
        if attempt < 2 {
            println!("  still unreachable — keys can take a few seconds to activate");
        }
    }
    println!("  keeping the remote; pushes will retry once access works");
}

pub(crate) fn prompt(message: &str) -> String {
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}
