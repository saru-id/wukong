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
//! Cellar whose `INSTALL_RECEIPT.json` says `installed_on_request` — that
//! single bit separates what you asked for from the dependency chaff.
//! Casks are Caskroom directories; apps are `.app` bundles in
//! /Applications. All three are cheap directory reads, fit for a
//! debounced reconcile.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    Npm,
    Pnpm,
    Bun,
    Cargo,
    Pipx,
    Uv,
}

impl Provider {
    /// Every provider, in display order.
    pub const ALL: [Provider; 9] = [
        Provider::Formula,
        Provider::Cask,
        Provider::App,
        Provider::Npm,
        Provider::Pnpm,
        Provider::Bun,
        Provider::Cargo,
        Provider::Pipx,
        Provider::Uv,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Formula => "formula",
            Provider::Cask => "cask",
            Provider::App => "app",
            Provider::Npm => "npm",
            Provider::Pnpm => "pnpm",
            Provider::Bun => "bun",
            Provider::Cargo => "cargo",
            Provider::Pipx => "pipx",
            Provider::Uv => "uv",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// The command that installs one package via this provider —
    /// `None` for App, which wukong can only remember.
    #[must_use]
    pub fn install_args(self, name: &str) -> Option<Vec<String>> {
        let argv: &[&str] = match self {
            Provider::Formula => &["brew", "install"],
            Provider::Cask => &["brew", "install", "--cask"],
            Provider::App => return None,
            Provider::Npm => &["npm", "install", "-g"],
            Provider::Pnpm => &["pnpm", "add", "-g"],
            Provider::Bun => &["bun", "add", "-g"],
            Provider::Cargo => &["cargo", "install"],
            Provider::Pipx => &["pipx", "install"],
            Provider::Uv => &["uv", "tool", "install"],
        };
        Some(
            argv.iter()
                .map(ToString::to_string)
                .chain([name.to_string()])
                .collect(),
        )
    }

    /// The command that uninstalls one package via this provider.
    #[must_use]
    pub fn uninstall_args(self, name: &str) -> Option<Vec<String>> {
        let argv: &[&str] = match self {
            Provider::Formula => &["brew", "uninstall"],
            Provider::Cask => &["brew", "uninstall", "--cask"],
            Provider::App => return None,
            Provider::Npm => &["npm", "uninstall", "-g"],
            Provider::Pnpm => &["pnpm", "remove", "-g"],
            Provider::Bun => &["bun", "remove", "-g"],
            Provider::Cargo => &["cargo", "uninstall"],
            Provider::Pipx => &["pipx", "uninstall"],
            Provider::Uv => &["uv", "tool", "uninstall"],
        };
        Some(
            argv.iter()
                .map(ToString::to_string)
                .chain([name.to_string()])
                .collect(),
        )
    }
}

/// `provider:name` — the inbox subject and event-log spelling.
#[must_use]
pub fn subject(provider: Provider, name: &str) -> String {
    format!("{}:{name}", provider.as_str())
}

#[must_use]
pub fn parse_subject(s: &str) -> Option<(Provider, &str)> {
    let (p, name) = s.split_once(':')?;
    Some((Provider::parse(p)?, name))
}

/// The manifest: what this machine should have, and what it should
/// never be asked about again — keyed by provider, so a new provider
/// is a table entry, not a schema change. `BTreeMap`/`BTreeSet` keep
/// the file sorted and diffs one line per change.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Manifest {
    /// provider → wanted package names.
    pub packages: BTreeMap<String, BTreeSet<String>>,
    /// provider → names never offered again.
    pub ignore: BTreeMap<String, BTreeSet<String>>,
}

impl Manifest {
    pub fn load(store_dir: &Path) -> Result<Option<Self>, String> {
        let path = store_dir.join(MANIFEST_REL);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read package manifest: {e}")),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("package manifest does not parse: {e}"))
    }

    pub fn save(&self, store_dir: &Path) -> std::io::Result<()> {
        let path = store_dir.join(MANIFEST_REL);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    #[must_use]
    pub fn contains(&self, provider: Provider, name: &str) -> bool {
        self.packages
            .get(provider.as_str())
            .is_some_and(|set| set.contains(name))
    }

    #[must_use]
    pub fn ignored(&self, provider: Provider, name: &str) -> bool {
        self.ignore
            .get(provider.as_str())
            .is_some_and(|set| set.contains(name))
    }

    /// Add to the wanted set (and drop any standing ignore — an
    /// explicit add outranks an old "never ask").
    pub fn add(&mut self, provider: Provider, name: &str) -> bool {
        if let Some(ignored) = self.ignore.get_mut(provider.as_str()) {
            ignored.remove(name);
        }
        self.packages
            .entry(provider.as_str().to_string())
            .or_default()
            .insert(name.to_string())
    }

    pub fn remove(&mut self, provider: Provider, name: &str) -> bool {
        self.packages
            .get_mut(provider.as_str())
            .is_some_and(|set| set.remove(name))
    }

    /// Permanent opt-out: never offer this package again.
    pub fn add_ignore(&mut self, provider: Provider, name: &str) -> bool {
        if let Some(wanted) = self.packages.get_mut(provider.as_str()) {
            wanted.remove(name);
        }
        self.ignore
            .entry(provider.as_str().to_string())
            .or_default()
            .insert(name.to_string())
    }

    pub fn remove_ignore(&mut self, provider: Provider, name: &str) -> bool {
        self.ignore
            .get_mut(provider.as_str())
            .is_some_and(|set| set.remove(name))
    }

    /// Every manifest entry in provider display order.
    #[must_use]
    pub fn entries(&self) -> Vec<(Provider, String)> {
        let mut out = Vec::new();
        for provider in Provider::ALL {
            if let Some(names) = self.packages.get(provider.as_str()) {
                for name in names {
                    out.push((provider, name.clone()));
                }
            }
        }
        out
    }
}

/// One place a provider's installs can be observed, plus HOW to read
/// it. A new provider is a root kind and a command table — nothing
/// else changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// Homebrew Cellar: receipts separate requests from dependencies.
    Cellar,
    /// Plain directory-per-package (Caskroom, pipx venvs, uv tools).
    DirNames,
    /// `.app` bundles, named without the extension.
    AppBundles,
    /// A global `node_modules`: subdirectories, `@scope/name` expanded,
    /// `.bin` skipped.
    NodeModules,
    /// cargo's `.crates.toml` registry inside the given directory.
    CratesToml,
}

#[derive(Debug, Clone)]
pub struct ProviderRoot {
    pub provider: Provider,
    pub kind: RootKind,
    pub path: PathBuf,
}

impl ProviderRoot {
    /// What the daemon should watch for this root, and how. Global
    /// `node_modules` trees are watched recursively (they are small and
    /// installs mutate deep paths); everything else observes direct
    /// children only.
    #[must_use]
    pub fn watch(&self) -> (PathBuf, bool) {
        (self.path.clone(), self.kind == RootKind::NodeModules)
    }

    /// Everything installed under this root right now. `None` when
    /// the root cannot be enumerated — one failed read must not turn
    /// the whole manifest into "package gone" offers.
    #[must_use]
    pub fn installed(&self) -> Option<BTreeSet<String>> {
        match self.kind {
            RootKind::Cellar => installed_formulae(&self.path),
            RootKind::DirNames => list_dirs(&self.path),
            RootKind::AppBundles => installed_apps(&self.path),
            RootKind::NodeModules => node_modules(&self.path),
            RootKind::CratesToml => crates_toml(&self.path),
        }
    }
}

/// The full set of observation roots for this machine.
#[derive(Debug, Clone, Default)]
pub struct Roots(pub Vec<ProviderRoot>);

impl Roots {
    #[must_use]
    pub fn installed(&self) -> Vec<(Provider, BTreeSet<String>)> {
        self.0
            .iter()
            .filter_map(|root| root.installed().map(|set| (root.provider, set)))
            .collect()
    }

    #[must_use]
    pub fn watch_roots(&self) -> Vec<(PathBuf, bool)> {
        self.0.iter().map(ProviderRoot::watch).collect()
    }

    #[must_use]
    pub fn cellar(&self) -> Option<&Path> {
        self.0
            .iter()
            .find(|r| r.kind == RootKind::Cellar)
            .map(|r| r.path.as_path())
    }
}

/// Detect every observable root. Fixed paths are preferred; npm and
/// pnpm keep their global root wherever they please, so those two are
/// asked ONCE, here at startup — never during reconcile. `overrides`
/// (config `[packages.roots]`) pins any provider to a path, which is
/// also how sandboxed runs point wukong at fake trees.
#[must_use]
pub fn detect_roots(
    brew_prefix: Option<&Path>,
    applications_dir: Option<&Path>,
    overrides: &BTreeMap<String, PathBuf>,
    home: &Path,
) -> Roots {
    let mut roots = Vec::new();
    // Defaults are LAZY: an override must fully suppress the default's
    // cost (asking npm for its root forks a process).
    let mut push =
        |provider: Provider, kind: RootKind, default: &mut dyn FnMut() -> Option<PathBuf>| {
            let path = overrides.get(provider.as_str()).cloned().or_else(default);
            if let Some(path) = path
                && path.is_dir()
            {
                roots.push(ProviderRoot {
                    provider,
                    kind,
                    path,
                });
            }
        };

    let prefix = brew_prefix.map(Path::to_path_buf).or_else(|| {
        ["/opt/homebrew", "/usr/local"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.join("Cellar").is_dir() || p.join("Caskroom").is_dir())
    });
    push(Provider::Formula, RootKind::Cellar, &mut || {
        prefix.as_ref().map(|p| p.join("Cellar"))
    });
    push(Provider::Cask, RootKind::DirNames, &mut || {
        prefix.as_ref().map(|p| p.join("Caskroom"))
    });
    push(Provider::App, RootKind::AppBundles, &mut || {
        Some(applications_dir.map_or_else(|| PathBuf::from("/Applications"), Path::to_path_buf))
    });
    push(Provider::Npm, RootKind::NodeModules, &mut || {
        global_root("npm", &["root", "-g"])
    });
    push(Provider::Pnpm, RootKind::NodeModules, &mut || {
        global_root("pnpm", &["root", "-g"])
    });
    push(Provider::Bun, RootKind::NodeModules, &mut || {
        Some(home.join(".bun/install/global/node_modules"))
    });
    push(Provider::Cargo, RootKind::CratesToml, &mut || {
        Some(home.join(".cargo"))
    });
    push(Provider::Pipx, RootKind::DirNames, &mut || {
        [".local/pipx/venvs", ".local/share/pipx/venvs"]
            .iter()
            .map(|rel| home.join(rel))
            .find(|p| p.is_dir())
    });
    push(Provider::Uv, RootKind::DirNames, &mut || {
        Some(home.join(".local/share/uv/tools"))
    });
    Roots(roots)
}

/// Ask a tool for its global root — startup only, and only when the
/// tool exists on PATH.
fn global_root(binary: &str, args: &[&str]) -> Option<PathBuf> {
    let out = std::process::Command::new(binary)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    path.is_dir().then_some(path)
}

#[derive(Debug, Deserialize)]
struct Receipt {
    #[serde(default)]
    installed_on_request: bool,
}

/// Formulae present in the Cellar that were installed on request —
/// dependencies never surface. Reads receipts, shells nothing.
#[must_use]
pub fn installed_formulae(cellar: &Path) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let entries = std::fs::read_dir(cellar).ok()?;
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
    Some(out)
}

/// Is anything in the Cellar still mid-install — a formula directory
/// with no receipt yet? A pour in progress must re-arm the reconcile
/// or the finished install slips past unnoticed (its receipt lands
/// too deep for the non-recursive watch).
#[must_use]
pub fn unsettled_formulae(cellar: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(cellar) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            return false;
        }
        !std::fs::read_dir(entry.path())
            .into_iter()
            .flatten()
            .flatten()
            .any(|version| version.path().join("INSTALL_RECEIPT.json").is_file())
    })
}

/// Casks are Caskroom directories.
#[must_use]
pub fn installed_casks(caskroom: &Path) -> Option<BTreeSet<String>> {
    list_dirs(caskroom)
}

/// Apps are `.app` bundles, named without the extension.
#[must_use]
pub fn installed_apps(applications: &Path) -> Option<BTreeSet<String>> {
    let entries = std::fs::read_dir(applications).ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".app").map(str::to_string)
            })
            .collect(),
    )
}

/// Global `node_modules`: package dirs, `@scope/name` expanded, `.bin`
/// and hidden entries skipped.
fn node_modules(dir: &Path) -> Option<BTreeSet<String>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut out = BTreeSet::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        if let Some(scope) = name.strip_prefix('@') {
            let Ok(scoped) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for pkg in scoped.flatten() {
                let pkg_name = pkg.file_name().to_string_lossy().into_owned();
                if !pkg_name.starts_with('.') && pkg.path().is_dir() {
                    out.insert(format!("@{scope}/{pkg_name}"));
                }
            }
        } else {
            out.insert(name);
        }
    }
    Some(out)
}

/// cargo's own registry of installed binaries: `.crates.toml`, whose
/// `[v1]` keys are "name version (source)".
fn crates_toml(cargo_home: &Path) -> Option<BTreeSet<String>> {
    let text = match std::fs::read_to_string(cargo_home.join(".crates.toml")) {
        Ok(t) => t,
        // No file = no cargo installs yet; an empty set is the truth.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(BTreeSet::new()),
        Err(_) => return None,
    };
    let doc: toml::Table = toml::from_str(&text).ok()?;
    let v1 = doc.get("v1")?.as_table()?;
    Some(
        v1.keys()
            .filter_map(|entry| entry.split_whitespace().next())
            .map(ToString::to_string)
            .collect(),
    )
}

fn list_dirs(dir: &Path) -> Option<BTreeSet<String>> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect(),
    )
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
        let found = installed_formulae(tmp.path()).unwrap();
        assert_eq!(found.into_iter().collect::<Vec<_>>(), vec!["jq"]);
    }

    #[test]
    fn apps_strip_extension_and_skip_nonapps() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("Raycast.app")).unwrap();
        std::fs::create_dir_all(tmp.path().join("Utilities")).unwrap();
        let found = installed_apps(tmp.path()).unwrap();
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

        let loaded = Manifest::load(tmp.path()).unwrap().unwrap();
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

#[cfg(test)]
mod detector_tests {
    use super::*;

    #[test]
    fn node_modules_expands_scopes_and_skips_bin() {
        let tmp = tempfile::TempDir::new().unwrap();
        for dir in ["typescript", "@biomejs/biome", ".bin", ".hidden"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        let found = node_modules(tmp.path()).unwrap();
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec!["@biomejs/biome", "typescript"]
        );
    }

    #[test]
    fn crates_toml_reads_cargo_installs() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".crates.toml"),
            "[v1]\n\"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"rg\"]\n\"fd-find 10.0.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"fd\"]\n",
        )
        .unwrap();
        let found = crates_toml(tmp.path()).unwrap();
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec!["fd-find", "ripgrep"]
        );
        // No file yet = truthfully empty, not an error.
        let empty = tempfile::TempDir::new().unwrap();
        assert_eq!(crates_toml(empty.path()).unwrap().len(), 0);
    }

    #[test]
    fn provider_command_tables_are_complete() {
        for provider in Provider::ALL {
            let install = provider.install_args("x");
            let uninstall = provider.uninstall_args("x");
            assert_eq!(install.is_some(), uninstall.is_some(), "{provider:?}");
            if provider == Provider::App {
                assert!(install.is_none());
            } else {
                assert!(install.unwrap().len() >= 3);
            }
        }
    }

    #[test]
    fn overridden_roots_win_and_absent_paths_disable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let npm = tmp.path().join("npmroot");
        std::fs::create_dir_all(&npm).unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("npm".to_string(), npm.clone());
        overrides.insert("cargo".to_string(), tmp.path().join("absent"));
        let roots = detect_roots(
            Some(tmp.path().join("nobrew").as_path()),
            Some(tmp.path().join("noapps").as_path()),
            &overrides,
            tmp.path().join("home").as_path(),
        );
        let npm_root = roots.0.iter().find(|r| r.provider == Provider::Npm);
        assert_eq!(npm_root.map(|r| r.path.clone()), Some(npm));
        assert!(!roots.0.iter().any(|r| r.provider == Provider::Cargo));
    }
}
