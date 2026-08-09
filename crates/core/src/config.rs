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
    /// Where this config was loaded from — the only place `save` will
    /// write. A config built in memory (tests) has nowhere to persist,
    /// and must never guess its way to the real user's file.
    #[serde(skip)]
    pub source: Option<PathBuf>,
    /// Paths under sentinel roots that are never offered for tracking
    /// (wukong's own config, cache-like churn).
    pub exclude: Vec<String>,
    /// Package governance knobs.
    pub packages: Packages,
    /// Settings governance knobs.
    pub settings: Settings,
    /// Sealed-lane knobs.
    pub seal: Seal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Seal {
    /// Keep the age identity in this file instead of the macOS
    /// Keychain (sandboxed runs, or your own key discipline).
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Watch preference domains, offer changes for recording.
    pub enabled: bool,
    /// Override ~/Library/Preferences (sandboxed runs). When set,
    /// `defaults` writes target plist FILES under this directory.
    pub preferences_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            preferences_dir: None,
        }
    }
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
    /// Pin any provider's observation root to a path (also how
    /// sandboxed runs point wukong at fake trees).
    pub roots: std::collections::BTreeMap<String, PathBuf>,
}

impl Default for Packages {
    fn default() -> Self {
        Self {
            enabled: true,
            brew_prefix: None,
            applications_dir: None,
            roots: std::collections::BTreeMap::new(),
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
            source: None,
            exclude: vec!["~/.config/wukong".to_string()],
            packages: Packages::default(),
            settings: Settings::default(),
            seal: Seal::default(),
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
            .map(|mut config: Self| {
                config.source = Some(path.clone());
                Some(config)
            })
            .map_err(|e| format!("{} does not parse: {e}", paths::display(&path)))
    }

    /// Write the fully-commented starter config — the config file is
    /// its own manual. Fresh machines only; existing files are edited
    /// surgically (comment-preserving) by the `persist_*` methods.
    pub fn write_starter(machine: &str, remote: &str) -> std::io::Result<std::path::PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let path = paths::config_file();
        if let Some(dir) = path.parent() {
            paths::ensure_private_dir(dir)?;
        }
        std::fs::write(&path, starter_toml(machine, remote))?;
        // The remote URL may carry credentials; owner-only.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(path)
    }

    /// Append one entry to `exclude` in the on-disk file, preserving
    /// every comment and all formatting (`toml_edit`, as cargo does).
    /// A config without an on-disk source (tests) persists nothing.
    pub fn persist_exclude(&self, entry: &str) -> std::io::Result<()> {
        self.edit_source(|doc| {
            let excludes = doc["exclude"].or_insert(toml_edit::array());
            if let Some(arr) = excludes.as_array_mut()
                && !arr.iter().any(|v| v.as_str() == Some(entry))
            {
                arr.push(entry);
            }
        })
    }

    /// Set `remote` in the on-disk file, comment-preserving.
    pub fn persist_remote(&self, remote: &str) -> std::io::Result<()> {
        self.edit_source(|doc| {
            doc["remote"] = toml_edit::value(remote);
        })
    }

    fn edit_source(&self, edit: impl FnOnce(&mut toml_edit::DocumentMut)) -> std::io::Result<()> {
        let Some(path) = self.source.clone() else {
            return Err(std::io::Error::other(
                "config has no on-disk source — nothing persisted",
            ));
        };
        let text = std::fs::read_to_string(&path)?;
        let mut doc: toml_edit::DocumentMut = text.parse().map_err(std::io::Error::other)?;
        edit(&mut doc);
        std::fs::write(&path, doc.to_string())
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
    pub fn pkg_roots(&self) -> crate::pkg::Roots {
        crate::pkg::detect_roots(
            self.packages.brew_prefix.as_deref(),
            self.packages.applications_dir.as_deref(),
            &self.packages.roots,
            paths::home(),
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

/// The starter config: every key present, every key explained. This
/// text and `Config::default()` are kept in agreement by a test.
#[must_use]
pub fn starter_toml(machine: &str, remote: &str) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"# wukong — the governor's configuration.
# The daemon reads this at startup: run `wukong daemon restart` after
# editing. wukong itself edits this file surgically (comments survive).

# This machine's name — also the store branch it commits and pushes to.
machine = "{machine}"

# Where the store syncs: a private git remote. Empty means local-only
# (no pushes). Changing it takes effect on daemon restart.
remote = "{remote}"

# Seconds a file must sit quiet before its change is committed.
debounce_secs = 2

# Seconds between automatic pushes when there is something to push.
push_interval_secs = 300

# Untracked paths watched for side effects (an installer editing your
# shell profile, a new launchd agent). Changes appear in the inbox as
# "track this?" offers. A file entry watches that file; a directory
# entry watches its whole subtree.
sentinels = [
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

# Paths never offered for tracking. `wukong exclude <path>` (or 'x' on
# an offer in the dashboard) appends here. Tracked files always win
# over excludes.
exclude = ["~/.config/wukong"]

# macOS notification when new inbox items arrive.
notifications = true

# Package governance: watch every provider's install receipts, offer
# installs for adoption, keep the synced manifest. Providers: brew
# formulae and casks, /Applications (App Store apps recognized by
# receipt), npm/pnpm/bun globals, cargo and go binaries, gems,
# pipx/uv tools, dotnet tools, pub globals. `wukong pkg providers`
# shows what's active on this machine.
[packages]
enabled = true
# brew_prefix = "/opt/homebrew"      # override auto-detection
# applications_dir = "/Applications" # override the default

# Pin any provider's root, or disable one by pointing it at a path
# that doesn't exist. Keys: formula cask app npm pnpm bun cargo go
# gem pipx uv dotnet pub.
# [packages.roots]
# npm = "/opt/node/lib/node_modules"
# gem = "~/.gem/ruby/3.3.0"

# Settings governance: watch a curated set of macOS defaults, offer
# changes for recording, apply the manifest with `wukong settings sync`.
[settings]
enabled = true
# preferences_dir = "/tmp/prefs"     # override ~/Library/Preferences

# The sealed lane: files whose secrets sync as age ciphertext only.
# The private identity lives in the macOS Keychain by default.
[seal]
# identity_file = "~/somewhere/age.key"  # file instead of Keychain
"#,
        machine = esc(machine),
        remote = esc(remote),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_config_parses_and_matches_defaults() {
        let text = starter_toml("testbox", "");
        let parsed: Config = toml::from_str(&text).unwrap();
        let defaults = Config::default();
        assert_eq!(parsed.machine, "testbox");
        assert_eq!(parsed.debounce_secs, defaults.debounce_secs);
        assert_eq!(parsed.push_interval_secs, defaults.push_interval_secs);
        assert_eq!(parsed.sentinels, defaults.sentinels);
        assert_eq!(parsed.exclude, defaults.exclude);
        assert_eq!(parsed.notifications, defaults.notifications);
        assert_eq!(parsed.packages, defaults.packages);
        assert_eq!(parsed.settings, defaults.settings);
        assert_eq!(parsed.seal, defaults.seal);
    }

    #[test]
    fn surgical_edits_preserve_comments() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, starter_toml("testbox", "")).unwrap();
        let mut config: Config = toml::from_str(&starter_toml("testbox", "")).unwrap();
        config.source = Some(path.clone());

        config.persist_exclude("~/.config/noisyapp").unwrap();
        config.persist_exclude("~/.config/noisyapp").unwrap(); // idempotent
        config.persist_remote("git@example.com:store.git").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# wukong — the governor's configuration."));
        assert!(text.contains("# Seconds a file must sit quiet"));
        assert_eq!(text.matches("noisyapp").count(), 1);
        assert!(text.contains(r#"remote = "git@example.com:store.git""#));
        // And it still parses into the same shape.
        let reparsed: Config = toml::from_str(&text).unwrap();
        assert!(reparsed.exclude.contains(&"~/.config/noisyapp".to_string()));
        assert_eq!(reparsed.remote, "git@example.com:store.git");
    }
}
