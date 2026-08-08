//! Package governance: the manifest and the detectors.
//!
//! The manifest is a plain TOML file of lists — what this machine is
//! supposed to have — living INSIDE the store repo at
//! `__wukong__/packages.toml`, so it rides the same commit/push/history
//! pipeline as every dotfile and syncs per machine branch. Metadata
//! (who added what, when) is the event log's and git's job; the file
//! stays human-editable and diff-clean.
//!
//! Detection never shells out to brew. A formula is a directory in the
//! Cellar whose INSTALL_RECEIPT.json says `installed_on_request` — that
//! single bit separates what you asked for from the dependency chaff.
//! Casks are Caskroom directories; apps are `.app` bundles in
//! /Applications. All three are cheap directory reads, fit for a
//! debounced reconcile.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where the manifest lives inside the store repo. The `__wukong__`
/// namespace can never collide with a mirrored `$HOME` path (that
/// prefix is reserved alongside `__abs__`).
pub const MANIFEST_REL: &str = "__wukong__/packages.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Formula,
    Cask,
    App,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Formula => "formula",
            Provider::Cask => "cask",
            Provider::App => "app",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "formula" => Some(Provider::Formula),
            "cask" => Some(Provider::Cask),
            "app" => Some(Provider::App),
            _ => None,
        }
    }
}

/// `provider:name` — the inbox subject and event-log spelling.
pub fn subject(provider: Provider, name: &str) -> String {
    format!("{}:{name}", provider.as_str())
}

pub fn parse_subject(s: &str) -> Option<(Provider, &str)> {
    let (p, name) = s.split_once(':')?;
    Some((Provider::parse(p)?, name))
}

/// The manifest: what this machine should have, and what it should
/// never be asked about again. BTreeSets keep the file sorted so diffs
/// stay one-line-per-change.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Manifest {
    pub formulae: BTreeSet<String>,
    pub casks: BTreeSet<String>,
    pub apps: BTreeSet<String>,
    pub ignore: Ignore,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Ignore {
    pub formulae: BTreeSet<String>,
    pub casks: BTreeSet<String>,
    pub apps: BTreeSet<String>,
}

impl Manifest {
    pub fn load(store_dir: &Path) -> Self {
        std::fs::read_to_string(store_dir.join(MANIFEST_REL))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, store_dir: &Path) -> std::io::Result<()> {
        let path = store_dir.join(MANIFEST_REL);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    fn set(&mut self, provider: Provider) -> &mut BTreeSet<String> {
        match provider {
            Provider::Formula => &mut self.formulae,
            Provider::Cask => &mut self.casks,
            Provider::App => &mut self.apps,
        }
    }

    fn ignore_set(&mut self, provider: Provider) -> &mut BTreeSet<String> {
        match provider {
            Provider::Formula => &mut self.ignore.formulae,
            Provider::Cask => &mut self.ignore.casks,
            Provider::App => &mut self.ignore.apps,
        }
    }

    pub fn contains(&self, provider: Provider, name: &str) -> bool {
        match provider {
            Provider::Formula => self.formulae.contains(name),
            Provider::Cask => self.casks.contains(name),
            Provider::App => self.apps.contains(name),
        }
    }

    pub fn ignored(&self, provider: Provider, name: &str) -> bool {
        match provider {
            Provider::Formula => self.ignore.formulae.contains(name),
            Provider::Cask => self.ignore.casks.contains(name),
            Provider::App => self.ignore.apps.contains(name),
        }
    }

    /// Add to the wanted set (and drop any standing ignore — an
    /// explicit add outranks an old "never ask").
    pub fn add(&mut self, provider: Provider, name: &str) -> bool {
        self.ignore_set(provider).remove(name);
        self.set(provider).insert(name.to_string())
    }

    pub fn remove(&mut self, provider: Provider, name: &str) -> bool {
        self.set(provider).remove(name)
    }

    /// Permanent opt-out: never offer this package again.
    pub fn add_ignore(&mut self, provider: Provider, name: &str) -> bool {
        self.set(provider).remove(name);
        self.ignore_set(provider).insert(name.to_string())
    }

    pub fn remove_ignore(&mut self, provider: Provider, name: &str) -> bool {
        self.ignore_set(provider).remove(name)
    }

    pub fn entries(&self) -> Vec<(Provider, String)> {
        let mut out = Vec::new();
        for n in &self.formulae {
            out.push((Provider::Formula, n.clone()));
        }
        for n in &self.casks {
            out.push((Provider::Cask, n.clone()));
        }
        for n in &self.apps {
            out.push((Provider::App, n.clone()));
        }
        out
    }
}

/// Where the detectors look. Injectable so tests (and the live drill)
/// can point them at a fake tree; production auto-detects.
#[derive(Debug, Clone)]
pub struct PkgRoots {
    pub cellar: Option<PathBuf>,
    pub caskroom: Option<PathBuf>,
    pub applications: Option<PathBuf>,
}

impl PkgRoots {
    /// Auto-detect: Apple Silicon prefix first, then Intel. A missing
    /// brew simply yields no brew roots — package governance degrades
    /// to app observation.
    pub fn detect(brew_prefix: Option<&Path>, applications_dir: Option<&Path>) -> Self {
        let prefix = brew_prefix.map(Path::to_path_buf).or_else(|| {
            ["/opt/homebrew", "/usr/local"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.join("Cellar").is_dir() || p.join("Caskroom").is_dir())
        });
        let existing = |p: PathBuf| if p.is_dir() { Some(p) } else { None };
        Self {
            cellar: prefix.as_ref().and_then(|p| existing(p.join("Cellar"))),
            caskroom: prefix.as_ref().and_then(|p| existing(p.join("Caskroom"))),
            applications: existing(
                applications_dir
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/Applications")),
            ),
        }
    }

    pub fn watch_roots(&self) -> Vec<PathBuf> {
        [&self.cellar, &self.caskroom, &self.applications]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct Receipt {
    #[serde(default)]
    installed_on_request: bool,
}

/// Formulae present in the Cellar that were installed on request —
/// dependencies never surface. Reads receipts, shells nothing.
pub fn installed_formulae(cellar: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(cellar) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        let requested = std::fs::read_dir(entry.path())
            .into_iter()
            .flatten()
            .flatten()
            .any(|version| {
                std::fs::read_to_string(version.path().join("INSTALL_RECEIPT.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Receipt>(&text).ok())
                    .is_some_and(|r| r.installed_on_request)
            });
        if requested {
            out.insert(name);
        }
    }
    out
}

/// Casks are Caskroom directories.
pub fn installed_casks(caskroom: &Path) -> BTreeSet<String> {
    list_dirs(caskroom)
}

/// Apps are `.app` bundles, named without the extension.
pub fn installed_apps(applications: &Path) -> BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(applications) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".app").map(str::to_string)
        })
        .collect()
}

fn list_dirs(dir: &Path) -> BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cellar(root: &Path) {
        // jq: requested. oniguruma: a dependency. broken: no receipt.
        for (name, receipt) in [
            ("jq", Some(r#"{"installed_on_request":true,"source":{}}"#)),
            ("oniguruma", Some(r#"{"installed_on_request":false}"#)),
            ("broken", None),
        ] {
            let vdir = root.join(name).join("1.0.0");
            std::fs::create_dir_all(&vdir).unwrap();
            if let Some(r) = receipt {
                std::fs::write(vdir.join("INSTALL_RECEIPT.json"), r).unwrap();
            }
        }
    }

    #[test]
    fn receipts_separate_requests_from_dependencies() {
        let tmp = tempfile::TempDir::new().unwrap();
        fake_cellar(tmp.path());
        let found = installed_formulae(tmp.path());
        assert_eq!(found.into_iter().collect::<Vec<_>>(), vec!["jq"]);
    }

    #[test]
    fn apps_strip_extension_and_skip_nonapps() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("Raycast.app")).unwrap();
        std::fs::create_dir_all(tmp.path().join("Utilities")).unwrap();
        let found = installed_apps(tmp.path());
        assert_eq!(found.into_iter().collect::<Vec<_>>(), vec!["Raycast"]);
    }

    #[test]
    fn manifest_round_trips_and_sorts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut m = Manifest::default();
        assert!(m.add(Provider::Formula, "ripgrep"));
        assert!(m.add(Provider::Formula, "jq"));
        assert!(!m.add(Provider::Formula, "jq")); // idempotent
        m.add(Provider::Cask, "raycast");
        m.add_ignore(Provider::App, "Safari");
        m.save(tmp.path()).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(MANIFEST_REL)).unwrap();
        // Sorted output → stable one-line diffs.
        assert!(
            text.find("jq").unwrap() < text.find("ripgrep").unwrap(),
            "{text}"
        );

        let loaded = Manifest::load(tmp.path());
        assert_eq!(loaded, m);
        assert!(loaded.contains(Provider::Formula, "jq"));
        assert!(loaded.ignored(Provider::App, "Safari"));
    }

    #[test]
    fn add_outranks_ignore_and_ignore_evicts() {
        let mut m = Manifest::default();
        m.add_ignore(Provider::Formula, "jq");
        assert!(m.ignored(Provider::Formula, "jq"));
        m.add(Provider::Formula, "jq");
        assert!(m.contains(Provider::Formula, "jq"));
        assert!(!m.ignored(Provider::Formula, "jq"));
        m.add_ignore(Provider::Formula, "jq");
        assert!(!m.contains(Provider::Formula, "jq"));
    }

    #[test]
    fn subject_round_trips() {
        let s = subject(Provider::Cask, "raycast");
        assert_eq!(s, "cask:raycast");
        assert_eq!(parse_subject(&s), Some((Provider::Cask, "raycast")));
        assert_eq!(parse_subject("nope"), None);
    }
}
