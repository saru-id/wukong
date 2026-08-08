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
    let fresh = config.machine.is_empty();

    if config.machine.is_empty() {
        config.machine = detect_machine();
    }
    if config.remote.is_empty() {
        config.remote = prompt("Store remote (git URL; press Enter for local-only): ");
        if !config.remote.is_empty() && !remote_reachable(&config.remote) {
            println!(
                "  note: could not reach {} right now — keeping it; pushes will retry",
                config.remote
            );
        }
    }
    config.save()?;
    println!("✓ config at {}", paths::display(&paths::config_file()));

    let store_dir = paths::store_dir();
    if !store_dir.join(".git").exists() && !config.remote.is_empty() {
        // A remote plus no local store = a new machine joining an
        // existing store. Clone, branch off, and offer the restore.
        match Store::clone_from(&config.remote, &store_dir, &config.machine) {
            Ok(store) => {
                let files = store.files().map(|f| f.len()).unwrap_or(0);
                println!(
                    "✓ cloned store from {} (branch {}, {files} file(s))",
                    config.remote, config.machine
                );
                if files > 0 {
                    println!("  bring them onto this machine with:  wukong restore");
                }
            }
            Err(e) => {
                // An empty or brand-new remote clones nothing — fall
                // back to a fresh local store that will push to it.
                println!("  note: clone failed ({e}); starting a fresh store");
                Store::open(&store_dir, &config.machine)?;
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
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn prompt(message: &str) -> String {
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}
