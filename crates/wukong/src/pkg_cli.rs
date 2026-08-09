//! The package verbs. `wukong install` is the whole pitch: brew does
//! the installing (its output streams through untouched), wukong does
//! the remembering. The CLI runs brew client-side so interactive
//! output stays native; the daemon owns the manifest. If the daemon
//! happens to be down, the install still works — the next reconcile
//! offers the package for adoption, so nothing is ever lost silently.

use crate::client;
use wukong_core::ipc::{PkgEntry, Request, Response};
use wukong_core::pkg::Provider;

/// The installable providers, as a clap value enum. App and Mas are
/// absent on purpose: wukong can only remember drag-installed apps,
/// and App Store installs go through the store (then get offered for
/// adoption).
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ViaArg {
    Formula,
    Cask,
    Npm,
    Pnpm,
    Bun,
    Cargo,
    Go,
    Gem,
    Pipx,
    Uv,
    Dotnet,
    Pub,
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
            ViaArg::Go => Provider::Go,
            ViaArg::Gem => Provider::Gem,
            ViaArg::Pipx => Provider::Pipx,
            ViaArg::Uv => Provider::Uv,
            ViaArg::Dotnet => Provider::Dotnet,
            ViaArg::Pub => Provider::Pub,
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
    let args = provider.uninstall_args(name).ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no uninstall command — delete it yourself, and the \
             daemon will offer to drop {name} from the manifest",
            provider.as_str()
        )
    })?;
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
        let version = e.version.as_deref().unwrap_or("");
        println!(
            "{mark} {:36} {:14} {}",
            e.name,
            version,
            e.provider.as_str()
        );
    }
    if missing > 0 {
        println!("\n{missing} missing — `wukong pkg sync` installs them");
    }
    Ok(())
}

pub fn providers(json: bool) -> anyhow::Result<()> {
    let entries = match client::call(Request::PkgProviders)? {
        Response::Providers { entries } => entries,
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected response"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    for e in &entries {
        let state = if e.active { "watching" } else { "off" };
        let count = e
            .count
            .map_or_else(|| "   -".to_string(), |n| format!("{n:>4}"));
        let path = e.path.as_deref().unwrap_or("(not found)");
        println!(
            "{:8} {state:8} {count}  {path}  ({})",
            e.provider.as_str(),
            e.origin
        );
    }
    println!(
        "\n{} of {} providers active — [packages.roots] in the config pins or disables any of them",
        entries.iter().filter(|e| e.active).count(),
        entries.len()
    );
    Ok(())
}

/// The exact command that would install a missing entry — `None`
/// where wukong can only remember it (drag-installed apps, App Store
/// apps whose id was never captured).
fn plan_for(e: &PkgEntry) -> Option<Vec<String>> {
    match e.provider {
        Provider::App => None,
        Provider::Mas => e.id.as_deref().and_then(|id| e.provider.install_args(id)),
        _ => e.provider.install_args(&e.name),
    }
}

pub fn sync(yes: bool, dry_run: bool) -> anyhow::Result<()> {
    let missing: Vec<PkgEntry> = fetch()?.into_iter().filter(|e| !e.installed).collect();
    if missing.is_empty() {
        println!("everything in the manifest is installed");
        return Ok(());
    }
    let (planned, by_hand): (Vec<_>, Vec<_>) = missing
        .into_iter()
        .map(|e| {
            let plan = plan_for(&e);
            (e, plan)
        })
        .partition(|(_, plan)| plan.is_some());

    if !planned.is_empty() {
        println!("will run:");
        let mut last_provider = None;
        for (e, plan) in &planned {
            if last_provider != Some(e.provider) {
                println!("  # {}", e.provider.as_str());
                last_provider = Some(e.provider);
            }
            println!("  {}", plan.as_deref().expect("partitioned").join(" "));
        }
        if dry_run {
            println!("(dry run — nothing executed)");
        } else {
            if !yes && !confirm("proceed? [y/N] ") {
                println!("nothing installed");
                return Ok(());
            }
            for (_, plan) in &planned {
                run_tool(plan.as_deref().expect("partitioned"))?;
            }
            println!("installed {} package(s)", planned.len());
        }
    }
    let (store, dragged): (Vec<_>, Vec<_>) = by_hand
        .iter()
        .partition(|(e, _)| e.provider == Provider::Mas);
    if !store.is_empty() {
        println!("\nApp Store apps with no recorded id — install from the store:");
        for (e, _) in &store {
            println!("  {}", e.name);
        }
    }
    if !dragged.is_empty() {
        println!("\napps wukong remembers but cannot install — grab these yourself:");
        for (e, _) in &dragged {
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
