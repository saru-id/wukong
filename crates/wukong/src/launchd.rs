//! The launchd `LaunchAgent`: keeps wukongd alive across logins without
//! a polling loop of its own. We write the plist, then drive it with
//! `launchctl`. Bootout of a not-loaded agent is tolerated (that's the
//! idempotent path); everything else fails loudly — a ✓ the user reads
//! must mean the agent is actually running.

use crate::DaemonAction;
use std::path::PathBuf;

const LABEL: &str = "id.saru.wukongd";

/// Restart the agent's daemon in place (used after config changes and
/// updates). True when launchctl accepted the kick.
pub fn kickstart() -> bool {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/{LABEL}")])
        .status()
        .is_ok_and(|s| s.success())
}

/// Is the installed agent older than the definition wukong now writes?
/// Only asks about things whose absence breaks something: today, the
/// PATH that lets the daemon find tools outside launchd's own.
#[must_use]
pub fn agent_is_stale() -> bool {
    let Ok(existing) = std::fs::read_to_string(agent_path()) else {
        return false;
    };
    !existing.contains("EnvironmentVariables")
}

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
    let domain = format!("gui/{}", uid()?);
    let _ = launchctl(&["bootout", &domain, path.to_str().unwrap_or_default()]);
    launchctl(&["bootstrap", &domain, path.to_str().unwrap_or_default()])?;
    launchctl(&["enable", &format!("{domain}/{LABEL}")])?;
    Ok(())
}

pub fn run(action: DaemonAction) -> anyhow::Result<()> {
    let domain = format!("gui/{}", uid()?);
    let service = format!("{domain}/{LABEL}");
    match action {
        DaemonAction::Start => {
            // Always (re)bootstrap: after `daemon stop` the plist is
            // still on disk but the service is booted out, and a bare
            // kickstart would fail against an unregistered service.
            install()?;
            launchctl(&["kickstart", "-k", &service])?;
            println!("wukongd started");
        }
        DaemonAction::Stop => {
            launchctl(&["bootout", &service])?;
            println!("wukongd stopped");
        }
        DaemonAction::Restart => {
            // Rewrite the plist first: restart exists to pick changes
            // up, and the agent's own definition is one of the things
            // that can have changed under it.
            install()?;
            launchctl(&["kickstart", "-k", &service])?;
            println!("wukongd restarted");
        }
        DaemonAction::Status => {
            // systemctl-is-active convention: scriptable exit codes.
            if crate::client::connected() {
                println!("wukongd is running");
            } else {
                println!("wukongd is not running");
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// Leave the machine as if wukong had never been here — except the
/// user's data, which goes only with an explicit `--purge`, and the
/// remote store, which is never touched.
pub fn uninstall(purge: bool, yes: bool) -> anyhow::Result<()> {
    use wukong_core::paths;
    let plist = agent_path();
    if plist.exists() {
        // Guarded by plist existence so a sandboxed run (drills, a
        // machine that never installed the agent) cannot bootout a
        // real service.
        let domain = format!("gui/{}", uid()?);
        launchctl_lenient(&["bootout", &domain, plist.to_str().unwrap_or_default()]);
        std::fs::remove_file(&plist)?;
        println!("✓ daemon stopped, launchd agent removed");
    } else {
        println!("· no launchd agent installed");
    }

    let data = [
        ("config", paths::config_dir()),
        ("store + database", paths::data_dir()),
        ("socket + log", paths::state_dir()),
    ];
    if purge {
        if !yes
            && !crate::pkg_cli::confirm(
                "delete the config, the database, and the LOCAL store repository? \
The remote store is untouched. [y/N] ",
            )
        {
            println!("kept everything");
            return Ok(());
        }
        for (label, dir) in data {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => println!("✓ removed {label} ({})", paths::display(&dir)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("could not remove {}: {e}", paths::display(&dir)),
            }
        }
        println!("the remote store (if configured) is untouched");
    } else {
        println!("kept (remove with `wukong uninstall --purge`):");
        for (label, dir) in data {
            println!("  {label}: {}", paths::display(&dir));
        }
    }
    if let Ok(bin) = std::env::current_exe() {
        println!(
            "binaries left in place — remove {} and wukongd beside it yourself",
            paths::display(&bin)
        );
    }
    Ok(())
}

/// launchctl where failure is acceptable (bootout of a not-loaded
/// agent during uninstall).
fn launchctl_lenient(args: &[&str]) {
    let _ = std::process::Command::new("launchctl").args(args).output();
}

fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("launchctl")
        .args(args)
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!(
            "launchctl {} failed{}",
            args.first().unwrap_or(&""),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(())
}

fn uid() -> anyhow::Result<u32> {
    // No getuid without unsafe; `id -u` is the portable spelling.
    let out = std::process::Command::new("id").arg("-u").output()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("could not determine uid"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The PATH the agent runs with.
///
/// launchd gives an agent `/usr/bin:/bin:/usr/sbin:/sbin` and nothing
/// else, so a daemon that asks a tool where it keeps its packages —
/// `npm root -g`, `pnpm root -g` — never finds one. Those two providers
/// reported "not found" on every machine where the daemon runs under
/// launchd, which is every real installation; they worked only when a
/// daemon was started from a shell, which is exactly what the drills
/// do. Both Homebrew prefixes are named because a plist cannot ask
/// which architecture it is on.
fn agent_path_env() -> String {
    let home = wukong_core::paths::home().to_string_lossy().into_owned();
    format!(
        "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:\
         {home}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    )
}

fn plist() -> String {
    let bin = daemon_binary();
    let bin = xml_escape(&bin.to_string_lossy());
    let log = wukong_core::paths::state_dir().join("wukongd.log");
    let log = xml_escape(&log.to_string_lossy());
    let path = xml_escape(&agent_path_env());
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
    <key>LowPriorityBackgroundIO</key><true/>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict><key>PATH</key><string>{path}</string></dict>
</dict>
</plist>
"#
    )
}
