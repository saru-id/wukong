//! `wukong init`: make this machine ready. Detect its name, write the
//! starter config, initialize the store repo — cloning the remote if
//! one is given and no store exists yet, which is the whole new-machine
//! bootstrap — install the launchd agent, and start the daemon.
//! Idempotent: safe to run again to repair a half-set-up machine.

use std::io::{self, Write as _};
use wukong_core::{Config, Store, paths};

pub fn run() -> anyhow::Result<()> {
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
        if remote_added && !remote_reachable(&config.remote) {
            println!(
                "  note: could not reach {} right now — keeping it; pushes will retry",
                config.remote
            );
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
    if !store_dir.join(".git").exists() && !config.remote.is_empty() {
        // A remote plus no local store = a new machine joining an
        // existing store. Clone, branch off, and offer the restore.
        match Store::clone_from(&config.remote, &store_dir, &config.machine) {
            Ok(store) => {
                let files = store.files().map_or(0, |f| f.len());
                println!(
                    "✓ cloned store from {} (branch {}, {files} file(s))",
                    config.remote, config.machine
                );
                if files > 0 {
                    println!("  bring them onto this machine with:  wukong restore");
                }
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

    crate::launchd::install()?;
    println!("✓ launchd agent installed and started");

    if fresh {
        println!("\nwukong is governing {}.", config.machine);
        println!("Track your first file:  wukong track ~/.zshrc");
        println!("Open the dashboard:     wukong");
    } else {
        println!("\nRepair complete.");
    }
    Ok(())
}

fn detect_machine() -> String {
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
fn remote_reachable(remote: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-remote", "--heads", remote])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn prompt(message: &str) -> String {
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}
