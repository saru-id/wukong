//! `wukong init`: make this machine ready. Detect its name, write the
//! starter config, initialize the store repo, install the launchd
//! agent, and start the daemon. Idempotent — safe to run again to
//! repair a half-set-up machine.

use std::io::{self, Write as _};
use wukong_core::{Config, Store, paths};

pub fn run() -> anyhow::Result<()> {
    let mut config = Config::load();
    let fresh = config.machine.is_empty();

    if config.machine.is_empty() {
        config.machine = detect_machine();
    }
    if config.remote.is_empty() {
        config.remote = prompt(&format!(
            "Store remote (git URL, blank for local-only) [{}]: ",
            suggested_remote(&config.machine)
        ));
    }
    config.save()?;
    println!("✓ config at {}", paths::display(&paths::config_file()));

    Store::open(&paths::store_dir(), &config.machine)?;
    println!(
        "✓ store repo at {} (branch {})",
        paths::display(&paths::store_dir()),
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

fn suggested_remote(_machine: &str) -> String {
    "git@github.com:you/wukong-store.git".to_string()
}

fn prompt(message: &str) -> String {
    print!("{message}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    line.trim().to_string()
}
