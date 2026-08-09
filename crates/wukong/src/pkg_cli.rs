//! The package verbs. `wukong install` is the whole pitch: brew does
//! the installing (its output streams through untouched), wukong does
//! the remembering. The CLI runs brew client-side so interactive
//! output stays native; the daemon owns the manifest. If the daemon
//! happens to be down, the install still works — the next reconcile
//! offers the package for adoption, so nothing is ever lost silently.

use crate::client;
use wukong_core::ipc::{PkgEntry, Request, Response};
use wukong_core::pkg::Provider;

pub fn install(name: &str, cask: bool, no_track: bool) -> anyhow::Result<()> {
    brew_run(&if cask {
        vec!["install", "--cask", name]
    } else {
        vec!["install", name]
    })?;
    // Even an untracked install is ACKNOWLEDGED to the daemon —
    // otherwise the watcher would offer it for adoption seconds after
    // the user explicitly opted out.
    record(provider_of(cask), name, false, no_track);
    Ok(())
}

pub fn rm(name: &str, cask: bool) -> anyhow::Result<()> {
    brew_run(&if cask {
        vec!["uninstall", "--cask", name]
    } else {
        vec!["uninstall", name]
    })?;
    record(provider_of(cask), name, true, false);
    Ok(())
}

fn provider_of(cask: bool) -> Provider {
    if cask {
        Provider::Cask
    } else {
        Provider::Formula
    }
}

fn record(provider: Provider, name: &str, remove: bool, observe_only: bool) {
    let req = Request::PkgRecord {
        provider,
        name: name.to_string(),
        remove,
        observe_only,
    };
    match client::call(req) {
        Ok(Response::Ok { message }) => println!("{message}"),
        Ok(Response::Error { message }) => eprintln!("warning: {message}"),
        Ok(_) => {}
        Err(_) => eprintln!(
            "note: wukongd is not running — brew succeeded, and the daemon \
             will offer {name} for adoption when it's back"
        ),
    }
}

/// Run brew with output streaming straight to the user's terminal.
fn brew_run(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("brew")
        .args(args)
        .status()
        .map_err(|_| anyhow::anyhow!("brew is not installed (https://brew.sh)"))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

pub fn list() -> anyhow::Result<()> {
    let entries = fetch()?;
    if entries.is_empty() {
        println!("the manifest is empty — `wukong install <pkg>` to begin,");
        println!("or `wukong pkg adopt-installed` to take in what's already here");
        return Ok(());
    }
    let mut missing = 0;
    for e in &entries {
        let mark = if e.installed {
            " "
        } else {
            missing += 1;
            "!"
        };
        println!("{mark} {:24} {}", e.name, e.provider.as_str());
    }
    if missing > 0 {
        println!("\n{missing} missing — `wukong pkg sync` installs them");
    }
    Ok(())
}

pub fn sync(yes: bool) -> anyhow::Result<()> {
    let missing: Vec<PkgEntry> = fetch()?.into_iter().filter(|e| !e.installed).collect();
    if missing.is_empty() {
        println!("everything in the manifest is installed");
        return Ok(());
    }
    let (brewable, apps): (Vec<_>, Vec<_>) = missing
        .into_iter()
        .partition(|e| e.provider != Provider::App);

    if !brewable.is_empty() {
        println!("will install via brew:");
        for e in &brewable {
            println!("  {} ({})", e.name, e.provider.as_str());
        }
        if !yes && !confirm("proceed? [y/N] ") {
            println!("nothing installed");
            return Ok(());
        }
        for e in &brewable {
            let mut args = vec!["install"];
            if e.provider == Provider::Cask {
                args.push("--cask");
            }
            args.push(&e.name);
            brew_run(&args)?;
        }
        println!("installed {} package(s)", brewable.len());
    }
    if !apps.is_empty() {
        println!("\napps wukong remembers but cannot install — grab these yourself:");
        for e in &apps {
            println!("  {}", e.name);
        }
    }
    Ok(())
}

pub fn adopt_installed() -> anyhow::Result<()> {
    crate::say(Request::PkgAdoptInstalled)
}

pub fn ignore(name: &str, cask: bool, app: bool, unignore: bool) -> anyhow::Result<()> {
    let provider = if app {
        Provider::App
    } else {
        provider_of(cask)
    };
    crate::say(Request::PkgIgnore {
        provider,
        name: name.to_string(),
        unignore,
    })
}

fn fetch() -> anyhow::Result<Vec<PkgEntry>> {
    match client::call(Request::PkgList)? {
        Response::Packages { entries } => Ok(entries),
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected response"),
    }
}

pub(crate) fn confirm(prompt: &str) -> bool {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}
