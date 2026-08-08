//! The launchd LaunchAgent: keeps wukongd alive across logins without
//! a polling loop of its own. We write the plist, then drive it with
//! `launchctl`. Everything degrades to a clear message rather than a
//! panic when launchctl is unavailable (a non-macOS host, say).

use crate::DaemonAction;
use std::path::PathBuf;

const LABEL: &str = "id.saru.wukongd";

pub fn agent_path() -> PathBuf {
    wukong_core::paths::home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn daemon_binary() -> PathBuf {
    // The wukongd binary sits beside this one in a normal install.
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("wukongd")))
        .unwrap_or_else(|| PathBuf::from("wukongd"))
}

pub fn install() -> anyhow::Result<()> {
    let plist = plist();
    let path = agent_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, plist)?;
    // bootout first so a re-init reloads cleanly, then bootstrap.
    let domain = format!("gui/{}", uid());
    let _ = launchctl(&["bootout", &domain, path.to_str().unwrap_or_default()]);
    launchctl(&["bootstrap", &domain, path.to_str().unwrap_or_default()])?;
    launchctl(&["enable", &format!("{domain}/{LABEL}")])?;
    Ok(())
}

pub fn run(action: DaemonAction) -> anyhow::Result<()> {
    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{LABEL}");
    match action {
        DaemonAction::Start => {
            if !agent_path().exists() {
                install()?;
            }
            launchctl(&["kickstart", "-k", &service])?;
            println!("wukongd started");
        }
        DaemonAction::Stop => {
            launchctl(&["bootout", &service])?;
            println!("wukongd stopped");
        }
        DaemonAction::Restart => {
            launchctl(&["kickstart", "-k", &service])?;
            println!("wukongd restarted");
        }
        DaemonAction::Status => {
            let running = crate::client::connected();
            println!(
                "wukongd is {}",
                if running { "running" } else { "not running" }
            );
        }
    }
    Ok(())
}

fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("launchctl")
        .args(args)
        .output()?;
    // launchctl returns non-zero for already-loaded / already-out; the
    // caller's intent is idempotent, so only surface real spawn errors
    // and a stderr worth reading.
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.trim().is_empty() && !stderr.contains("No such process") {
            // Non-fatal: report but keep going.
            eprintln!(
                "launchctl {}: {}",
                args.first().unwrap_or(&""),
                stderr.trim()
            );
        }
    }
    Ok(())
}

fn uid() -> u32 {
    // getuid without unsafe: read from the environment launchd sets, or
    // fall back to `id -u`.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(501)
}

fn plist() -> String {
    let bin = daemon_binary();
    let bin = bin.to_string_lossy();
    let home = wukong_core::paths::home();
    let log = home.join(".local/state/wukong/wukongd.log");
    let log = log.to_string_lossy();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>{bin}</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}
