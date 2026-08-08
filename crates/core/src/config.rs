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
    /// to the `ComputerName` at init time.
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
    /// Paths under sentinel roots that are never offered for tracking
    /// (wukong's own config, cache-like churn).
    pub exclude: Vec<String>,
    /// Package governance knobs.
    pub packages: Packages,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Packages {
    /// Watch brew and /Applications, offer adoptions in the inbox.
    pub enabled: bool,
    /// Override the auto-detected Homebrew prefix (tests, odd setups).
    pub brew_prefix: Option<PathBuf>,
    /// Override /Applications (tests).
    pub applications_dir: Option<PathBuf>,
}

impl Default for Packages {
    fn default() -> Self {
        Self {
            enabled: true,
            brew_prefix: None,
            applications_dir: None,
        }
    }
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
            exclude: vec!["~/.config/wukong".to_string()],
            packages: Packages::default(),
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
    /// `Ok(None)` = no config file yet (fresh machine). A file that
    /// exists but fails to parse is an error the user must see — the
    /// old behavior of silently falling back to defaults turned a typo
    /// into a mystifying "not initialized".
    pub fn load() -> Result<Option<Self>, String> {
        let path = paths::config_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", paths::display(&path))),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} does not parse: {e}", paths::display(&path)))
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = paths::config_file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    /// Sentinel entries as canonical absolute paths — `/etc` is a
    /// symlink into `/private` on macOS, and the watcher reports real
    /// paths, so comparisons must happen on the canonical form.
    #[must_use]
    pub fn sentinel_paths(&self) -> Vec<PathBuf> {
        expand_all(&self.sentinels)
    }

    /// Exclude entries, canonical like the sentinels.
    #[must_use]
    pub fn exclude_paths(&self) -> Vec<PathBuf> {
        expand_all(&self.exclude)
    }

    /// Detector roots honoring config overrides.
    #[must_use]
    pub fn pkg_roots(&self) -> crate::pkg::PkgRoots {
        crate::pkg::PkgRoots::detect(
            self.packages.brew_prefix.as_deref(),
            self.packages.applications_dir.as_deref(),
        )
    }
}

fn expand_all(entries: &[String]) -> Vec<PathBuf> {
    entries
        .iter()
        .map(|s| {
            let raw = match s.strip_prefix("~/") {
                Some(rel) => paths::home().join(rel),
                None => PathBuf::from(s),
            };
            paths::canonicalize_lenient(&raw)
        })
        .collect()
}
