//! First-run onboarding: find the dotfiles a machine already has and
//! track them in one motion. The candidate list is curated, single-file
//! configs only (wukong tracks files, not trees), and every file still
//! passes the secret gate individually — a token in the real .zshrc
//! quarantines that file without blocking the rest.

use crate::client;
use wukong_core::ipc::{Request, Response};
use wukong_core::paths;

/// Well-known single-file configs, `~/`-relative. Deliberately no
/// credential-bearing files (`gh/hosts.yml`, key material — the gate's
/// forbidden list would refuse them anyway).
const CANDIDATES: &[&str] = &[
    // Shell startup.
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".profile",
    ".bashrc",
    ".bash_profile",
    ".inputrc",
    ".hushlogin",
    // Git.
    ".gitconfig",
    ".gitignore_global",
    ".config/git/config",
    ".config/git/ignore",
    // Editors and terminals.
    ".vimrc",
    ".ideavimrc",
    ".tmux.conf",
    ".wezterm.lua",
    ".config/starship.toml",
    ".config/ghostty/config",
    ".config/kitty/kitty.conf",
    ".config/alacritty/alacritty.toml",
    ".config/helix/config.toml",
    ".config/zed/settings.json",
    ".config/fish/config.fish",
    ".config/nushell/config.nu",
    // Tooling.
    ".ssh/config",
    ".cargo/config.toml",
    ".config/atuin/config.toml",
    ".config/bat/config",
    ".config/direnv/direnv.toml",
    ".config/mise/config.toml",
    ".config/karabiner/karabiner.json",
];

pub fn run(yes: bool) -> anyhow::Result<()> {
    let tracked: Vec<String> = match client::call(Request::TrackedList)? {
        Response::Tracked { files } => files.into_iter().map(|f| f.display).collect(),
        Response::Error { message } => anyhow::bail!(message),
        _ => anyhow::bail!("unexpected response"),
    };

    let found: Vec<String> = CANDIDATES
        .iter()
        .filter(|rel| paths::home().join(rel).is_file())
        .map(|rel| format!("~/{rel}"))
        .filter(|display| !tracked.contains(display))
        .collect();

    if found.is_empty() {
        println!("nothing new to adopt — every known dotfile is tracked or absent");
        return Ok(());
    }
    println!("found {} dotfile(s) to track:", found.len());
    for f in &found {
        println!("  {f}");
    }
    if !yes && !crate::pkg_cli::confirm("track them all? [y/N] ") {
        println!("nothing tracked");
        return Ok(());
    }
    for f in &found {
        match client::call(Request::Track {
            path: f.clone(),
            sealed: false,
        })? {
            Response::Ok { message } => println!("{message}"),
            Response::Error { message } => eprintln!("  skipped: {message}"),
            _ => {}
        }
    }
    Ok(())
}
