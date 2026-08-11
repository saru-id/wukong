//! `wukong remote`: the backup/sync remote as a late-bindable choice.
//! A machine starts local-only and fully working; attaching a remote
//! afterwards is safe by construction — machine branches can't collide
//! (each machine owns its own) and the shared branch folds in on the
//! rebase path. So the question moves from setup time to whenever the
//! user is ready.

use wukong_core::{Config, Store, paths};

pub fn show() -> anyhow::Result<()> {
    let config = Config::load()
        .map_err(|e| anyhow::anyhow!(e))?
        .unwrap_or_default();
    if config.remote.is_empty() {
        println!("local-only — no remote configured");
        println!("add one: wukong remote <url>");
    } else {
        println!("{}", config.remote);
    }
    Ok(())
}

pub fn set(url: &str) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    let config = Config::load()
        .map_err(|e| anyhow::anyhow!(e))?
        .ok_or_else(|| anyhow::anyhow!("this machine isn't set up yet — type `wukong` first"))?;
    config.persist_remote(url)?;
    let store = Store::open(&paths::store_dir(), &config.machine)?;
    store.ensure_remote(url)?;

    if !crate::init::remote_reachable(url) {
        if std::io::stdin().is_terminal() {
            crate::init::ssh_wizard(url);
        } else {
            println!("note: {url} is not reachable right now — pushes will retry");
        }
    }

    // Probe BEFORE the daemon restarts: the moment it comes back it
    // may push, and the honest answer is what was there when the user
    // attached.
    let heads = ls_remote_heads(url);
    if heads.as_deref().is_some_and(|h| !h.is_empty()) {
        // The remote already has a store: fold the shared lane in now
        // so `wukong sync` is truthful immediately.
        let _ = store.refresh_shared();
    }

    // The daemon holds its config in memory; bounce it so pushes know
    // where to go from this moment on.
    crate::init::restart_daemon();

    match heads {
        Some(heads) if !heads.is_empty() => {
            println!("✓ remote attached — it already holds a store");
            println!("  `wukong sync` brings this machine up to match (shared lane included)");
        }
        Some(_) => {
            println!("✓ remote attached (empty repository)");
            println!("  this machine's history pushes on its own — or right now: wukong push");
        }
        None => {
            println!("✓ remote recorded — unreachable right now; pushes will retry");
        }
    }
    Ok(())
}

/// What branches the remote holds, or `None` when unreachable.
fn ls_remote_heads(url: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["ls-remote", "--heads", url])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        )
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
