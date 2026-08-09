//! The package verbs. `wukong install` is the whole pitch: brew does
//! the installing (its output streams through untouched), wukong does
//! the remembering. The CLI runs brew client-side so interactive
//! output stays native; the daemon owns the manifest. If the daemon
//! happens to be down, the install still works — the next reconcile
//! offers the package for adoption, so nothing is ever lost silently.

use crate::client;
use wukong_core::ipc::{PkgEntry, Request, Response};
use wukong_core::pkg::Provider;

/// The installable providers, as a clap value enum. App is absent on
/// purpose: wukong can only remember drag-installed apps.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ViaArg {
    Formula,
    Cask,
    Npm,
    Pnpm,
    Bun,
    Cargo,
    Pipx,
    Uv,
}

impl From<ViaArg> for Provider {
    fn from(via: ViaArg) -> Self {
        match via {
            ViaArg::Formula => Provider::Formula,
            ViaArg::Cask => Provider::Cask,
            ViaArg::Npm => Provider::Npm,
            ViaArg::Pnpm => Provider::Pnpm,
            ViaArg::Bun => Provider::Bun,
            ViaArg::Cargo => Provider::Cargo,
            ViaArg::Pipx => Provider::Pipx,
            ViaArg::Uv => Provider::Uv,
        }
    }
}

/// Run a provider's own CLI with output streaming to the terminal.
fn run_tool(args: &[String]) -> anyhow::Result<()> {
    let (bin, rest) = args.split_first().expect("command table is never empty");
    let status = std::process::Command::new(bin)
        .args(rest)
        .status()
        .map_err(|_| anyhow::anyhow!("{bin} is not installed"))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

pub fn install(name: &str, provider: Provider, no_track: bool) -> anyhow::Result<()> {
    let args = provider
        .install_args(name)
        .ok_or_else(|| anyhow::anyhow!("{} cannot install", provider.as_str()))?;
    run_tool(&args)?;
    // Even an untracked install is ACKNOWLEDGED to the daemon —
    // otherwise the watcher would offer it for adoption seconds after
    // the user explicitly opted out.
    record(provider, name, false, no_track);
    Ok(())
}

pub fn rm(name: &str, provider: Provider) -> anyhow::Result<()> {
    let args = provider
        .uninstall_args(name)
        .ok_or_else(|| anyhow::anyhow!("{} cannot uninstall", provider.as_str()))?;
    run_tool(&args)?;
    record(provider, name, true, false);
    Ok(())
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

pub fn list(json: bool) -> anyhow::Result<()> {
    let entries = fetch()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
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

pub fn sync(yes: bool, dry_run: bool) -> anyhow::Result<()> {
    let missing: Vec<PkgEntry> = fetch()?.into_iter().filter(|e| !e.installed).collect();
    if missing.is_empty() {
        println!("everything in the manifest is installed");
        return Ok(());
    }
    let (installable, apps): (Vec<_>, Vec<_>) = missing
        .into_iter()
        .partition(|e| e.provider.install_args("x").is_some());

    if !installable.is_empty() {
        println!("will run:");
        for e in &installable {
            let args = e.provider.install_args(&e.name).expect("partitioned");
            println!("  {}", args.join(" "));
        }
        if dry_run {
            println!("(dry run — nothing executed)");
        } else {
            if !yes && !confirm("proceed? [y/N] ") {
                println!("nothing installed");
                return Ok(());
            }
            for e in &installable {
                run_tool(&e.provider.install_args(&e.name).expect("partitioned"))?;
            }
            println!("installed {} package(s)", installable.len());
        }
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

pub fn ignore(name: &str, provider: Provider, unignore: bool) -> anyhow::Result<()> {
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
