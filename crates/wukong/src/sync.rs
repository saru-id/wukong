//! `wukong sync`: make this machine match the store. One plan covering
//! files, packages, and settings; one confirmation; `--dry-run` stops
//! at the plan. The scoped verbs (`restore`, `pkg sync`,
//! `settings sync`) stay for à la carte use — this is the whole
//! answer, and the entire new-machine bootstrap.

use crate::{client, pkg_cli, settings_cli};
use wukong_core::ipc::{PkgEntry, Request, Response, SettingEntry};

pub fn run(yes: bool, dry_run: bool) -> anyhow::Result<()> {
    // FILES — the daemon reports what restore would do. The first call
    // also fails fast when the daemon is down.
    println!("files");
    let mut file_work = false;
    match client::call(Request::Restore {
        path: None,
        force: false,
        dry_run: true,
    })? {
        Response::Ok { message } => {
            file_work = !message.starts_with("0 to restore");
            for line in message.lines() {
                println!("  {line}");
            }
        }
        Response::Error { message } => println!("  {message}"),
        _ => anyhow::bail!("unexpected response"),
    }

    // PACKAGES — the exact commands, grouped by provider, plus the
    // remember-only checklist.
    println!("packages");
    let missing: Vec<PkgEntry> = pkg_cli::fetch()?
        .into_iter()
        .filter(|e| !e.installed)
        .collect();
    let mut planned: Vec<(PkgEntry, Vec<String>)> = Vec::new();
    let mut by_hand: Vec<PkgEntry> = Vec::new();
    for e in missing {
        match pkg_cli::plan_for(&e) {
            Some(plan) => planned.push((e, plan)),
            None => by_hand.push(e),
        }
    }
    if planned.is_empty() && by_hand.is_empty() {
        println!("  everything in the manifest is installed");
    }
    for (_, plan) in &planned {
        println!("  {}", plan.join(" "));
    }
    for e in &by_hand {
        println!("  {} — install this one yourself", e.name);
    }

    // SETTINGS — recorded values this machine has drifted from.
    println!("settings");
    let settings_plan: Option<(Vec<SettingEntry>, Option<String>)> = match settings_cli::fetch() {
        Ok((entries, file_dir)) => Some((
            entries
                .into_iter()
                .filter(|e| e.desired.is_some() && !e.in_sync)
                .collect(),
            file_dir,
        )),
        Err(e) => {
            println!("  {e}");
            None
        }
    };
    if let Some((plan, _)) = &settings_plan {
        if plan.is_empty() {
            println!("  every recorded setting already matches");
        }
        for e in plan {
            println!(
                "  {} {} = {}",
                e.domain,
                e.key,
                e.desired.as_ref().expect("filtered")
            );
        }
    }

    let setting_work = settings_plan.as_ref().is_some_and(|(p, _)| !p.is_empty());
    if dry_run {
        println!("(dry run — nothing executed)");
        return Ok(());
    }
    if !file_work && planned.is_empty() && !setting_work {
        println!("\nthis machine matches the store");
        return Ok(());
    }
    if !yes && !pkg_cli::confirm("\napply all of it? [y/N] ") {
        println!("nothing changed");
        return Ok(());
    }

    if file_work {
        match client::call(Request::Restore {
            path: None,
            force: false,
            dry_run: false,
        })? {
            Response::Ok { message } | Response::Error { message } => println!("{message}"),
            _ => {}
        }
    }
    for (_, plan) in &planned {
        pkg_cli::run_tool(plan)?;
    }
    if let Some((plan, file_dir)) = &settings_plan
        && !plan.is_empty()
    {
        settings_cli::apply(&plan.iter().collect::<Vec<_>>(), file_dir.as_deref());
    }
    println!("\nsynced — `wukong status` for the state of things");
    Ok(())
}
