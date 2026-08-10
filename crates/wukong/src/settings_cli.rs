//! The settings verbs. The daemon owns observation and the manifest;
//! this side renders the desired-vs-live view and — for `sync` — runs
//! `defaults write` itself, because writes must go through `cfprefsd`
//! and belong in the user's session, followed by the restarts the
//! corpus prescribes.

use crate::client;
use std::collections::BTreeSet;
use std::io::Write as _;
use wukong_core::ipc::{CaptureChange, Request, Response, SettingEntry};

pub(crate) fn fetch() -> anyhow::Result<(Vec<SettingEntry>, Option<String>)> {
    match client::call(Request::SettingsList)? {
        Response::Settings {
            entries,
            file_domains_dir,
        } => Ok((entries, file_domains_dir)),
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected response"),
    }
}

pub fn list(json: bool) -> anyhow::Result<()> {
    let (entries, _) = fetch()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    let mut drift = 0;
    for e in &entries {
        let mark = match (&e.desired, e.in_sync) {
            (None, _) => "·",
            (Some(_), true) => " ",
            (Some(_), false) => {
                drift += 1;
                "!"
            }
        };
        let value = e
            .desired
            .as_ref()
            .map_or_else(String::new, |v| format!(" = {v}"));
        let label = e.label.as_deref().unwrap_or("");
        println!("{mark} {} {}{value}  {label}", e.domain, e.key);
    }
    println!(
        "\n{} governed · {} recorded · {} drifted   (· observed only, ! drifted)",
        entries.len(),
        entries.iter().filter(|e| e.desired.is_some()).count(),
        drift
    );
    if drift > 0 {
        println!("`wukong settings sync` applies the recorded values");
    }
    Ok(())
}

pub fn diff() -> anyhow::Result<()> {
    let (entries, _) = fetch()?;
    let drifted: Vec<&SettingEntry> = entries
        .iter()
        .filter(|e| e.desired.is_some() && !e.in_sync)
        .collect();
    if drifted.is_empty() {
        println!("every recorded setting matches this machine");
        return Ok(());
    }
    for e in drifted {
        let live = e
            .live
            .as_ref()
            .map_or_else(|| "(unset)".to_string(), ToString::to_string);
        let want = e.desired.as_ref().expect("filtered on desired");
        println!(
            "{} {}: {live} → {want}  {}",
            e.domain,
            e.key,
            e.label.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// Write a plan of settings via `defaults`, then restart what needs
/// it. Shared by `settings sync` and the all-in `wukong sync`.
pub(crate) fn apply(plan: &[&SettingEntry], file_dir: Option<&str>) {
    let mut restarts: BTreeSet<String> = BTreeSet::new();
    let mut applied = 0;
    for e in plan {
        let value = e.desired.as_ref().expect("filtered");
        let (flag, rendered) = value.defaults_args();
        // Sandboxed runs target plist FILES; real runs target domains,
        // which keeps cfprefsd coherent.
        let target = match file_dir {
            Some(dir) => wukong_core::settings::plist_path(std::path::Path::new(dir), &e.domain)
                .to_string_lossy()
                .into_owned(),
            None => e.domain.clone(),
        };
        let ok = std::process::Command::new("defaults")
            .args(["write", &target, &e.key, flag, &rendered])
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            applied += 1;
            if let Some(restart) = &e.restart {
                restarts.insert(restart.clone());
            }
        } else {
            eprintln!("failed: {} {}", e.domain, e.key);
        }
    }
    println!("applied {applied} setting(s)");
    // Restarts only make sense against the real preference domains.
    if file_dir.is_none() {
        for process in restarts {
            let _ = std::process::Command::new("killall").arg(&process).status();
            println!("restarted {process}");
        }
    }
}

pub fn sync(yes: bool) -> anyhow::Result<()> {
    let (entries, file_dir) = fetch()?;
    let plan: Vec<&SettingEntry> = entries
        .iter()
        .filter(|e| e.desired.is_some() && !e.in_sync)
        .collect();
    if plan.is_empty() {
        println!("every recorded setting already matches this machine");
        return Ok(());
    }
    println!("will apply {} setting(s):", plan.len());
    for e in &plan {
        println!(
            "  {} {} = {}",
            e.domain,
            e.key,
            e.desired.as_ref().expect("filtered")
        );
    }
    if !yes && !crate::pkg_cli::confirm("proceed? [y/N] ") {
        println!("nothing applied");
        return Ok(());
    }
    apply(&plan, file_dir.as_deref());
    Ok(())
}

pub fn record(domain: &str, key: &str) -> anyhow::Result<()> {
    crate::say(Request::SettingsRecord {
        domain: domain.to_string(),
        key: key.to_string(),
    })
}

pub fn ignore(domain: &str, key: &str, unignore: bool) -> anyhow::Result<()> {
    crate::say(Request::SettingsIgnore {
        domain: domain.to_string(),
        key: key.to_string(),
        unignore,
    })
}

/// Which half of the capture flow to run — the phases are mutually
/// exclusive, so they are an enum, not a pile of bools.
pub enum CapturePhase {
    /// Snapshot, wait for Enter, diff, offer to record.
    Interactive,
    /// Snapshot only (scripting).
    Start,
    /// Diff against the snapshot (scripting).
    Diff,
}

/// The capture flow.
pub fn capture(phase: &CapturePhase, all: bool, json: bool) -> anyhow::Result<()> {
    match phase {
        CapturePhase::Start => return say_start(),
        CapturePhase::Diff => {
            let changes = fetch_diff()?;
            return present(&changes, all, json).map(|_| ());
        }
        CapturePhase::Interactive => {}
    }
    say_start()?;
    println!("Change the setting now (System Settings, defaults, anywhere).");
    print!("Press Enter to diff… ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let changes = fetch_diff()?;
    let recordable = present(&changes, all, json)?;
    if json || recordable.is_empty() {
        return Ok(());
    }
    print!("Record which? [a = all / Enter = none / numbers like 1,3]: ");
    std::io::stdout().flush()?;
    let mut pick = String::new();
    std::io::stdin().read_line(&mut pick)?;
    let pick = pick.trim();
    let chosen: Vec<&CaptureChange> = if pick.is_empty() {
        Vec::new()
    } else if pick == "a" {
        recordable.clone()
    } else {
        pick.split(',')
            .filter_map(|n| n.trim().parse::<usize>().ok())
            .filter_map(|n| recordable.get(n.wrapping_sub(1)).copied())
            .collect()
    };
    if chosen.is_empty() {
        println!("nothing recorded");
        return Ok(());
    }
    for c in chosen {
        match client::call(Request::SettingsRecord {
            domain: c.domain.clone(),
            key: c.key.clone(),
        })? {
            Response::Ok { message } => println!("{message}"),
            Response::Error { message } => eprintln!("  skipped: {message}"),
            _ => {}
        }
    }
    Ok(())
}

fn say_start() -> anyhow::Result<()> {
    match client::call(Request::SettingsCaptureStart)? {
        Response::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected response"),
    }
}

fn fetch_diff() -> anyhow::Result<Vec<CaptureChange>> {
    match client::call(Request::SettingsCaptureDiff)? {
        Response::CaptureDiff { changes } => Ok(changes),
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected response"),
    }
}

/// Print the diff; return the numbered, recordable signal entries.
fn present(
    changes: &[CaptureChange],
    all: bool,
    json: bool,
) -> anyhow::Result<Vec<&CaptureChange>> {
    if json {
        let shown: Vec<&CaptureChange> = changes.iter().filter(|c| all || !c.noise).collect();
        println!("{}", serde_json::to_string_pretty(&shown)?);
        return Ok(Vec::new());
    }
    let noise_count = changes.iter().filter(|c| c.noise).count();
    let mut recordable = Vec::new();
    for c in changes {
        if c.noise && !all {
            continue;
        }
        let fmt_value = |v: &Option<wukong_core::settings::Value>| {
            v.as_ref()
                .map_or_else(|| "(unset)".to_string(), ToString::to_string)
        };
        let tag = if c.noise { " (noise)" } else { "" };
        if c.after.is_some() && !c.noise {
            recordable.push(c);
            println!(
                "{:2}. {} {}: {} → {}{}  {}",
                recordable.len(),
                c.domain,
                c.key,
                fmt_value(&c.before),
                fmt_value(&c.after),
                tag,
                c.label.as_deref().unwrap_or("")
            );
        } else {
            println!(
                "    {} {}: {} → {}{}",
                c.domain,
                c.key,
                fmt_value(&c.before),
                fmt_value(&c.after),
                tag
            );
        }
    }
    if recordable.is_empty() && !all {
        println!(
            "no setting changes detected ({noise_count} noisy change(s) filtered — --all shows them)"
        );
    } else if noise_count > 0 && !all {
        println!("({noise_count} noisy change(s) filtered — --all shows them)");
    }
    Ok(recordable)
}
