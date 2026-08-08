//! `~/.config/wukong/config.toml` — everything a different person on a
//! different Mac would need to change. Absent file means defaults;
//! `wukong init` writes the starter.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// This machine's name — the store branch it pushes to. Defaults
    /// to the ComputerName at init time.
    pub machine: String,
    /// Where the store pushes (empty = local-only, push disabled).
    pub remote: String,
    /// Seconds a file must sit quiet before its change is committed.
    pub debounce_secs: u64,
    /// Seconds between push attempts when there is something to push.
    pub push_interval_secs: u64,
    /// Untracked paths the daemon keeps an eye on for side effects;
    /// changes land in the inbox as "track this?" candidates.
    pub sentinels: Vec<String>,
    /// macOS notification on new inbox items.
    pub notifications: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            machine: String::new(),
            remote: String::new(),
            debounce_secs: 2,
            push_interval_secs: 300,
            sentinels: default_sentinels(),
            notifications: true,
        }
    }
}

/// The usual suspects for installer side effects: shell startup files,
/// git identity, the tool configs under ~/.config, launchd agents, and
/// the PATH drop-in directory.
fn default_sentinels() -> Vec<String> {
    [
        "~/.zshrc",
        "~/.zprofile",
        "~/.zshenv",
        "~/.profile",
        "~/.bashrc",
        "~/.gitconfig",
        "~/.config",
        "~/Library/LaunchAgents",
        "/etc/paths.d",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(paths::config_file())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = paths::config_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    /// Sentinel entries expanded to absolute paths.
    pub fn sentinel_paths(&self) -> Vec<PathBuf> {
        self.sentinels
            .iter()
            .map(|s| match s.strip_prefix("~/") {
                Some(rel) => paths::home().join(rel),
                None => PathBuf::from(s),
            })
            .collect()
    }
}
