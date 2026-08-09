//! The settings verbs. The daemon owns observation and the manifest;
//! this side renders the desired-vs-live view and — for `sync` — runs
//! `defaults write` itself, because writes must go through `cfprefsd`
//! and belong in the user's session, followed by the restarts the
//! corpus prescribes.

use crate::client;
use std::collections::BTreeSet;
use wukong_core::ipc::{Request, Response, SettingEntry};

fn fetch() -> anyhow::Result<(Vec<SettingEntry>, Option<String>)> {
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
    let mut restarts: BTreeSet<String> = BTreeSet::new();
    let mut applied = 0;
    for e in &plan {
        let value = e.desired.as_ref().expect("filtered");
        let (flag, rendered) = value.defaults_args();
        // Sandboxed runs target plist FILES; real runs target domains,
        // which keeps cfprefsd coherent.
        let target = match &file_dir {
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
