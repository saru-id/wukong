//! Package governance: the manifest and the detectors.
//!
//! The manifest is a plain TOML file of lists — what this machine is
//! supposed to have — living INSIDE the store repo at
//! `__wukong__/packages.toml`, so it rides the same commit/push/history
//! pipeline as every dotfile and syncs per machine branch. Metadata
//! (who added what, when) is the event log's and git's job; the file
//! stays human-editable and diff-clean.
//!
//! Detection never asks a package manager what is installed. Every
//! provider leaves receipts on disk — Cellar `INSTALL_RECEIPT.json`,
//! Caskroom dirs, `.app` bundles (App Store ones carry `_MASReceipt`),
//! global `node_modules` trees, cargo's `.crates.toml`, the module
//! path Go embeds in every binary, gemspec files, pipx/uv venvs,
//! dotnet's `.store`, pub's `global_packages` — and wukong reads those
//! directly. Cheap file reads, fit for a debounced reconcile.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    Mas,
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

impl Provider {
    /// Every provider, in display order.
    pub const ALL: [Provider; 14] = [
        Provider::Formula,
        Provider::Cask,
        Provider::App,
        Provider::Mas,
        Provider::Npm,
        Provider::Pnpm,
        Provider::Bun,
        Provider::Cargo,
        Provider::Go,
        Provider::Gem,
        Provider::Pipx,
        Provider::Uv,
        Provider::Dotnet,
        Provider::Pub,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Formula => "formula",
            Provider::Cask => "cask",
            Provider::App => "app",
            Provider::Mas => "mas",
            Provider::Npm => "npm",
            Provider::Pnpm => "pnpm",
            Provider::Bun => "bun",
            Provider::Cargo => "cargo",
            Provider::Go => "go",
            Provider::Gem => "gem",
            Provider::Pipx => "pipx",
            Provider::Uv => "uv",
            Provider::Dotnet => "dotnet",
            Provider::Pub => "pub",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// The command that installs one package via this provider.
    /// `None` for App (drag-installed; wukong can only remember). For
    /// Mas the argument is the App Store id, not the name — `sync`
    /// resolves it from the manifest's ids table.
    #[must_use]
    pub fn install_args(self, name: &str) -> Option<Vec<String>> {
        let argv: &[&str] = match self {
            Provider::Formula => &["brew", "install"],
            Provider::Cask => &["brew", "install", "--cask"],
            Provider::App => return None,
            Provider::Mas => &["mas", "install"],
            Provider::Npm => &["npm", "install", "-g"],
            Provider::Pnpm => &["pnpm", "add", "-g"],
            Provider::Bun => &["bun", "add", "-g"],
            Provider::Cargo => &["cargo", "install"],
            Provider::Go => {
                return Some(vec![
                    "go".to_string(),
                    "install".to_string(),
                    format!("{name}@latest"),
                ]);
            }
            Provider::Gem => &["gem", "install", "--user-install"],
            Provider::Pipx => &["pipx", "install"],
            Provider::Uv => &["uv", "tool", "install"],
            Provider::Dotnet => &["dotnet", "tool", "install", "--global"],
            Provider::Pub => &["dart", "pub", "global", "activate"],
        };
        Some(
            argv.iter()
                .map(ToString::to_string)
                .chain([name.to_string()])
                .collect(),
        )
    }

    /// The command that uninstalls one package via this provider.
    /// `None` where no such command exists: App and Mas (delete the
    /// app) and Go (delete the binary from the go bin dir).
    #[must_use]
    pub fn uninstall_args(self, name: &str) -> Option<Vec<String>> {
        let argv: &[&str] = match self {
            Provider::Formula => &["brew", "uninstall"],
            Provider::Cask => &["brew", "uninstall", "--cask"],
            Provider::App | Provider::Mas | Provider::Go => return None,
            Provider::Npm => &["npm", "uninstall", "-g"],
            Provider::Pnpm => &["pnpm", "remove", "-g"],
            Provider::Bun => &["bun", "remove", "-g"],
            Provider::Cargo => &["cargo", "uninstall"],
            Provider::Gem => &["gem", "uninstall"],
            Provider::Pipx => &["pipx", "uninstall"],
            Provider::Uv => &["uv", "tool", "uninstall"],
            Provider::Dotnet => &["dotnet", "tool", "uninstall", "--global"],
            Provider::Pub => &["dart", "pub", "global", "deactivate"],
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
    pub packages: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// provider → names never offered again.
    pub ignore: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// provider → name → external install id. Only the App Store
    /// needs one today (`mas install` takes the numeric id, not the
    /// name); captured at adoption when Spotlight knows it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ids: BTreeMap<String, BTreeMap<String, String>>,
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
        self.drop_id(provider, name);
        self.packages
            .get_mut(provider.as_str())
            .is_some_and(|set| set.remove(name))
    }

    /// Permanent opt-out: never offer this package again.
    pub fn add_ignore(&mut self, provider: Provider, name: &str) -> bool {
        if let Some(wanted) = self.packages.get_mut(provider.as_str()) {
            wanted.remove(name);
        }
        self.drop_id(provider, name);
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

    pub fn set_id(&mut self, provider: Provider, name: &str, id: &str) {
        self.ids
            .entry(provider.as_str().to_string())
            .or_default()
            .insert(name.to_string(), id.to_string());
    }

    #[must_use]
    pub fn id_of(&self, provider: Provider, name: &str) -> Option<&str> {
        self.ids
            .get(provider.as_str())
            .and_then(|m| m.get(name))
            .map(String::as_str)
    }

    fn drop_id(&mut self, provider: Provider, name: &str) {
        if let Some(map) = self.ids.get_mut(provider.as_str()) {
            map.remove(name);
            if map.is_empty() {
                self.ids.remove(provider.as_str());
            }
        }
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

/// What is installed under one root: name → version, where the
/// receipt encodes a version at all.
pub type Installed = BTreeMap<String, Option<String>>;

/// One place a provider's installs can be observed, plus HOW to read
/// it. A new provider is a root kind and a command table — nothing
/// else changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// Homebrew Cellar: receipts separate requests from dependencies.
    Cellar,
    /// Directory-per-package with version subdirectories (Caskroom,
    /// dotnet's `.store`).
    VersionedDirs,
    /// Plain directory-per-package, no version on disk (uv tools,
    /// pub `global_packages`).
    DirNames,
    /// pipx venvs: directory-per-package with a metadata file that
    /// knows the version.
    PipxVenvs,
    /// `.app` bundles, named without the extension; ones carrying a
    /// `_MASReceipt` are the App Store's, the rest are drag-installs.
    AppBundles,
    /// A global `node_modules`: subdirectories, `@scope/name` expanded,
    /// `.bin` skipped, versions from each `package.json`.
    NodeModules,
    /// cargo's `.crates.toml` registry inside the given directory.
    CratesToml,
    /// A go bin dir: every binary names its own module path in the
    /// build info Go embeds.
    GoBin,
    /// A ruby gem home: `specifications/*.gemspec` file names.
    GemSpecs,
}

/// How a root's path was determined — shown in `pkg providers` so
/// "why isn't X watched" is answerable from any shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootOrigin {
    /// The provider's well-known location.
    Fixed,
    /// Discovered — a filesystem search, or the tool was asked once
    /// at startup.
    Probed,
    /// Pinned by `[packages.roots]` in the config.
    Override,
}

impl RootOrigin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RootOrigin::Fixed => "fixed",
            RootOrigin::Probed => "probed",
            RootOrigin::Override => "override",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRoot {
    pub provider: Provider,
    pub kind: RootKind,
    pub path: PathBuf,
    pub origin: RootOrigin,
}

/// What one go binary resolves to: its module path and version, or
/// nothing (not a Go binary).
type GoModule = Option<(String, Option<String>)>;

/// Module-path lookups for go binaries, memoized on (mtime, size) —
/// reading build info means reading the whole binary, which must not
/// happen again on every reconcile.
#[derive(Debug, Clone, Default)]
pub struct GoCache(HashMap<PathBuf, ((SystemTime, u64), GoModule)>);

impl GoCache {
    fn module_of(&mut self, path: &Path) -> GoModule {
        let meta = std::fs::metadata(path).ok()?;
        let stamp = (meta.modified().ok()?, meta.len());
        if let Some((cached, result)) = self.0.get(path)
            && *cached == stamp
        {
            return result.clone();
        }
        let result = std::fs::read(path)
            .ok()
            .and_then(|bytes| crate::gobuild::read(&bytes));
        self.0.insert(path.to_path_buf(), (stamp, result.clone()));
        result
    }
}

impl ProviderRoot {
    /// What the daemon should watch for this root, and how. Global
    /// `node_modules` trees are watched recursively (they are small and
    /// installs mutate deep paths); everything else observes direct
    /// children only. Gem homes watch the `specifications` dir where
    /// the receipts actually land.
    #[must_use]
    pub fn watch(&self) -> (PathBuf, bool) {
        if self.kind == RootKind::GemSpecs {
            let spec = self.path.join("specifications");
            let target = if spec.is_dir() {
                spec
            } else {
                self.path.clone()
            };
            return (target, false);
        }
        (self.path.clone(), self.kind == RootKind::NodeModules)
    }

    /// Everything installed under this root right now, per provider —
    /// one Applications dir yields both App Store and drag-installed
    /// apps. `None` when the root cannot be enumerated: one failed
    /// read must not turn the whole manifest into "package gone"
    /// offers.
    #[must_use]
    pub fn installed(&self, go_cache: &mut GoCache) -> Option<Vec<(Provider, Installed)>> {
        Some(match self.kind {
            RootKind::Cellar => vec![(self.provider, installed_formulae(&self.path)?)],
            RootKind::VersionedDirs => vec![(self.provider, versioned_dirs(&self.path)?)],
            RootKind::DirNames => vec![(self.provider, dir_names(&self.path)?)],
            RootKind::PipxVenvs => vec![(self.provider, pipx_venvs(&self.path)?)],
            RootKind::AppBundles => {
                let (apps, mas) = apps_split(&self.path)?;
                vec![(Provider::App, apps), (Provider::Mas, mas)]
            }
            RootKind::NodeModules => vec![(self.provider, node_modules(&self.path)?)],
            RootKind::CratesToml => vec![(self.provider, crates_toml(&self.path)?)],
            RootKind::GoBin => vec![(self.provider, go_bin(&self.path, go_cache))],
            RootKind::GemSpecs => vec![(self.provider, gem_specs(&self.path)?)],
        })
    }
}

/// One row of the providers table: every provider, active or not, and
/// why.
#[derive(Debug, Clone)]
pub struct RootStatus {
    pub provider: Provider,
    pub path: Option<PathBuf>,
    pub origin: RootOrigin,
    pub active: bool,
}

/// The full set of observation roots for this machine: the active
/// ones the daemon watches, plus the disabled ones kept for the
/// providers table.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    pub active: Vec<ProviderRoot>,
    disabled: Vec<(Provider, Option<PathBuf>, RootOrigin)>,
    go_cache: GoCache,
}

impl Roots {
    #[must_use]
    pub fn installed(&mut self) -> Vec<(Provider, Installed)> {
        let mut cache = std::mem::take(&mut self.go_cache);
        let out = self
            .active
            .iter()
            .filter_map(|root| root.installed(&mut cache))
            .flatten()
            .collect();
        self.go_cache = cache;
        out
    }

    #[must_use]
    pub fn watch_roots(&self) -> Vec<(PathBuf, bool)> {
        self.active.iter().map(ProviderRoot::watch).collect()
    }

    #[must_use]
    pub fn cellar(&self) -> Option<&Path> {
        self.active
            .iter()
            .find(|r| r.kind == RootKind::Cellar)
            .map(|r| r.path.as_path())
    }

    /// The Applications dir, when active — where an adopted App Store
    /// app's id can be looked up.
    #[must_use]
    pub fn applications(&self) -> Option<&Path> {
        self.active
            .iter()
            .find(|r| r.kind == RootKind::AppBundles)
            .map(|r| r.path.as_path())
    }

    /// One row per provider. Mas rides the Applications root.
    #[must_use]
    pub fn status(&self) -> Vec<RootStatus> {
        Provider::ALL
            .into_iter()
            .map(|provider| {
                let serves = |r: &&ProviderRoot| {
                    r.provider == provider
                        || (provider == Provider::Mas && r.kind == RootKind::AppBundles)
                };
                if let Some(root) = self.active.iter().find(serves) {
                    return RootStatus {
                        provider,
                        path: Some(root.path.clone()),
                        origin: root.origin,
                        active: true,
                    };
                }
                let lookup = if provider == Provider::Mas {
                    Provider::App
                } else {
                    provider
                };
                let (path, origin) = self
                    .disabled
                    .iter()
                    .find(|(p, _, _)| *p == lookup)
                    .map_or((None, RootOrigin::Fixed), |(_, path, origin)| {
                        (path.clone(), *origin)
                    });
                RootStatus {
                    provider,
                    path,
                    origin,
                    active: false,
                }
            })
            .collect()
    }
}

/// Detect every observable root. Fixed paths are preferred; npm and
/// pnpm keep their global root wherever they please, so those two are
/// asked ONCE, here at startup — never during reconcile. `overrides`
/// (config `[packages.roots]`) pins any provider to a path, which is
/// also how sandboxed runs point wukong at fake trees; an override at
/// a nonexistent path disables the provider.
#[must_use]
pub fn detect_roots(
    brew_prefix: Option<&Path>,
    applications_dir: Option<&Path>,
    overrides: &BTreeMap<String, PathBuf>,
    home: &Path,
) -> Roots {
    let mut roots = Roots::default();
    let (prefix, brew_origin) = match brew_prefix {
        Some(p) => (Some(p.to_path_buf()), RootOrigin::Override),
        None => (
            ["/opt/homebrew", "/usr/local"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.join("Cellar").is_dir() || p.join("Caskroom").is_dir()),
            RootOrigin::Probed,
        ),
    };
    let (apps_dir, apps_origin) = match applications_dir {
        Some(p) => (p.to_path_buf(), RootOrigin::Override),
        None => (PathBuf::from("/Applications"), RootOrigin::Fixed),
    };
    push_root(
        &mut roots,
        overrides,
        Provider::Formula,
        RootKind::Cellar,
        brew_origin,
        &mut || prefix.as_ref().map(|p| p.join("Cellar")),
    );
    push_root(
        &mut roots,
        overrides,
        Provider::Cask,
        RootKind::VersionedDirs,
        brew_origin,
        &mut || prefix.as_ref().map(|p| p.join("Caskroom")),
    );
    // Mas rides the Applications root: same directory, receipt-split.
    push_root(
        &mut roots,
        overrides,
        Provider::App,
        RootKind::AppBundles,
        apps_origin,
        &mut || Some(apps_dir.clone()),
    );
    language_roots(&mut roots, overrides, home);
    roots
}

/// The language/ecosystem providers, all rooted somewhere under (or
/// probed from) the home directory.
fn language_roots(roots: &mut Roots, overrides: &BTreeMap<String, PathBuf>, home: &Path) {
    push_root(
        roots,
        overrides,
        Provider::Npm,
        RootKind::NodeModules,
        RootOrigin::Probed,
        &mut || global_root("npm", &["root", "-g"]),
    );
    push_root(
        roots,
        overrides,
        Provider::Pnpm,
        RootKind::NodeModules,
        RootOrigin::Probed,
        &mut || global_root("pnpm", &["root", "-g"]),
    );
    push_root(
        roots,
        overrides,
        Provider::Bun,
        RootKind::NodeModules,
        RootOrigin::Fixed,
        &mut || Some(home.join(".bun/install/global/node_modules")),
    );
    push_root(
        roots,
        overrides,
        Provider::Cargo,
        RootKind::CratesToml,
        RootOrigin::Fixed,
        &mut || Some(home.join(".cargo")),
    );
    push_root(
        roots,
        overrides,
        Provider::Go,
        RootKind::GoBin,
        RootOrigin::Fixed,
        &mut || {
            std::env::var_os("GOBIN")
                .map(PathBuf::from)
                .or_else(|| Some(home.join("go/bin")))
        },
    );
    // Gem.user_dir is ~/.gem/<engine>/<ruby version> on every ruby
    // install method; take the newest version dir.
    push_root(
        roots,
        overrides,
        Provider::Gem,
        RootKind::GemSpecs,
        RootOrigin::Probed,
        &mut || newest_dir(&home.join(".gem/ruby")),
    );
    push_root(
        roots,
        overrides,
        Provider::Pipx,
        RootKind::PipxVenvs,
        RootOrigin::Probed,
        &mut || {
            [".local/pipx/venvs", ".local/share/pipx/venvs"]
                .iter()
                .map(|rel| home.join(rel))
                .find(|p| p.is_dir())
        },
    );
    push_root(
        roots,
        overrides,
        Provider::Uv,
        RootKind::DirNames,
        RootOrigin::Fixed,
        &mut || Some(home.join(".local/share/uv/tools")),
    );
    push_root(
        roots,
        overrides,
        Provider::Dotnet,
        RootKind::VersionedDirs,
        RootOrigin::Fixed,
        &mut || Some(home.join(".dotnet/tools/.store")),
    );
    push_root(
        roots,
        overrides,
        Provider::Pub,
        RootKind::DirNames,
        RootOrigin::Fixed,
        &mut || Some(home.join(".pub-cache/global_packages")),
    );
}

/// One provider's root: the override wins (and its cost suppresses
/// the default's — defaults are LAZY because some fork), an existing
/// dir activates, anything else lands in the disabled table.
fn push_root(
    roots: &mut Roots,
    overrides: &BTreeMap<String, PathBuf>,
    provider: Provider,
    kind: RootKind,
    origin: RootOrigin,
    default: &mut dyn FnMut() -> Option<PathBuf>,
) {
    let (path, origin) = match overrides.get(provider.as_str()) {
        Some(p) => (Some(p.clone()), RootOrigin::Override),
        None => (default(), origin),
    };
    match path {
        Some(path) if path.is_dir() => roots.active.push(ProviderRoot {
            provider,
            kind,
            path,
            origin,
        }),
        path => roots.disabled.push((provider, path, origin)),
    }
}

/// Ask a tool for its global root — startup only, and only when the
/// tool exists on PATH. Hard 10s wall clock: a wedged package manager
/// (a broken shim, a node runtime waiting on something) costs its
/// provider, NEVER the daemon's startup.
fn global_root(binary: &str, args: &[&str]) -> Option<PathBuf> {
    use std::io::Read as _;
    let mut child = std::process::Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                let path = PathBuf::from(out.trim());
                return path.is_dir().then_some(path);
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn newest_dir(parent: &Path) -> Option<PathBuf> {
    std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .max_by_key(std::fs::DirEntry::file_name)
        .map(|e| e.path())
}

#[derive(Debug, Deserialize)]
struct Receipt {
    #[serde(default)]
    installed_on_request: bool,
}

/// Formulae present in the Cellar that were installed on request —
/// dependencies never surface. Reads receipts, shells nothing.
#[must_use]
pub fn installed_formulae(cellar: &Path) -> Option<Installed> {
    let mut out = Installed::new();
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
            let version = newest_dir(&entry.path())
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
            out.insert(name, version);
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

/// Apps split by receipt: `.app` bundles named without the extension,
/// App Store ones (carrying `_MASReceipt`) separated from
/// drag-installs, versions from each `Info.plist`.
fn apps_split(applications: &Path) -> Option<(Installed, Installed)> {
    let entries = std::fs::read_dir(applications).ok()?;
    let mut apps = Installed::new();
    let mut mas = Installed::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".app") else {
            continue;
        };
        let contents = entry.path().join("Contents");
        let version = plist::Value::from_file(contents.join("Info.plist"))
            .ok()
            .and_then(|v| {
                v.as_dictionary()?
                    .get("CFBundleShortVersionString")?
                    .as_string()
                    .map(str::to_string)
            });
        if contents.join("_MASReceipt/receipt").is_file() {
            mas.insert(name.to_string(), version);
        } else {
            apps.insert(name.to_string(), version);
        }
    }
    Some((apps, mas))
}

/// Global `node_modules`: package dirs, `@scope/name` expanded, `.bin`
/// and hidden entries skipped, versions from each `package.json`.
fn node_modules(dir: &Path) -> Option<Installed> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut out = Installed::new();
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
                    out.insert(format!("@{scope}/{pkg_name}"), node_version(&pkg.path()));
                }
            }
        } else {
            out.insert(name, node_version(&entry.path()));
        }
    }
    Some(out)
}

fn node_version(pkg_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

/// cargo's own registry of installed binaries: `.crates.toml`, whose
/// `[v1]` keys are "name version (source)".
fn crates_toml(cargo_home: &Path) -> Option<Installed> {
    let text = match std::fs::read_to_string(cargo_home.join(".crates.toml")) {
        Ok(t) => t,
        // No file = no cargo installs yet; an empty set is the truth.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Installed::new()),
        Err(_) => return None,
    };
    let doc: toml::Table = toml::from_str(&text).ok()?;
    let v1 = doc.get("v1")?.as_table()?;
    Some(
        v1.keys()
            .filter_map(|entry| {
                let mut words = entry.split_whitespace();
                let name = words.next()?.to_string();
                Some((name, words.next().map(str::to_string)))
            })
            .collect(),
    )
}

/// A go bin dir: every Go binary names its own module path.
/// Non-Go files simply don't parse and stay ungoverned.
fn go_bin(dir: &Path, cache: &mut GoCache) -> Installed {
    let mut out = Installed::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_file() {
            continue;
        }
        if let Some((module, version)) = cache.module_of(&entry.path()) {
            out.insert(module, version);
        }
    }
    out
}

/// A gem home: `specifications/*.gemspec`, named `name-version` with
/// an optional platform suffix.
fn gem_specs(gem_home: &Path) -> Option<Installed> {
    let dir = gem_home.join("specifications");
    if !dir.exists() {
        // A gem home with no user installs yet — truthfully empty.
        return Some(Installed::new());
    }
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                let stem = file.strip_suffix(".gemspec")?;
                Some(gem_name_version(stem))
            })
            .collect(),
    )
}

/// "nokogiri-1.16.0-arm64-darwin" → ("nokogiri", "1.16.0"): the name
/// ends at the first hyphen-then-digit, the version at the platform
/// suffix.
fn gem_name_version(stem: &str) -> (String, Option<String>) {
    let split = stem
        .match_indices('-')
        .find(|(i, _)| stem.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit));
    match split {
        Some((i, _)) => (
            stem[..i].to_string(),
            stem[i + 1..].split('-').next().map(str::to_string),
        ),
        None => (stem.to_string(), None),
    }
}

/// pipx venvs: a dir per package, version in `pipx_metadata.json`.
fn pipx_venvs(dir: &Path) -> Option<Installed> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .map(|name| {
                let version = pipx_version(&dir.join(&name));
                (name, version)
            })
            .collect(),
    )
}

fn pipx_version(venv: &Path) -> Option<String> {
    let text = std::fs::read_to_string(venv.join("pipx_metadata.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("main_package")?
        .get("package_version")?
        .as_str()
        .map(str::to_string)
}

/// Directory-per-package with a version subdirectory inside each.
fn versioned_dirs(dir: &Path) -> Option<Installed> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .map(|name| {
                let version = newest_dir(&dir.join(&name))
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
                (name, version)
            })
            .collect(),
    )
}

fn dir_names(dir: &Path) -> Option<Installed> {
    let entries = std::fs::read_dir(dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .map(|name| (name, None))
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
            let vdir = root.join(name).join("1.7.1");
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
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![("jq".to_string(), Some("1.7.1".to_string()))]
        );
    }

    #[test]
    fn apps_split_by_mas_receipt_with_versions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dragged = tmp.path().join("Raycast.app/Contents");
        std::fs::create_dir_all(&dragged).unwrap();
        std::fs::write(
            dragged.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleShortVersionString</key><string>1.70.2</string>
</dict></plist>"#,
        )
        .unwrap();
        let bought = tmp.path().join("Bought.app/Contents/_MASReceipt");
        std::fs::create_dir_all(&bought).unwrap();
        std::fs::write(bought.join("receipt"), b"sealed by apple").unwrap();
        std::fs::create_dir_all(tmp.path().join("Utilities")).unwrap();

        let (apps, mas) = apps_split(tmp.path()).unwrap();
        assert_eq!(
            apps.into_iter().collect::<Vec<_>>(),
            vec![("Raycast".to_string(), Some("1.70.2".to_string()))]
        );
        assert_eq!(
            mas.into_iter().collect::<Vec<_>>(),
            vec![("Bought".to_string(), None)]
        );
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
        // Sorted output → stable one-line diffs. No ids table until
        // one exists.
        assert!(
            text.find("jq").unwrap() < text.find("ripgrep").unwrap(),
            "{text}"
        );
        assert!(!text.contains("ids"), "{text}");

        let loaded = Manifest::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, m);
        assert!(loaded.contains(Provider::Formula, "jq"));
        assert!(loaded.ignored(Provider::App, "Safari"));
    }

    #[test]
    fn ids_ride_along_and_clean_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut m = Manifest::default();
        m.add(Provider::Mas, "Xcode");
        m.set_id(Provider::Mas, "Xcode", "497799835");
        m.save(tmp.path()).unwrap();
        let loaded = Manifest::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.id_of(Provider::Mas, "Xcode"), Some("497799835"));

        let mut m = loaded;
        m.remove(Provider::Mas, "Xcode");
        assert_eq!(m.id_of(Provider::Mas, "Xcode"), None);
        assert!(m.ids.is_empty());

        m.add(Provider::Mas, "Xcode");
        m.set_id(Provider::Mas, "Xcode", "497799835");
        m.add_ignore(Provider::Mas, "Xcode");
        assert!(m.ids.is_empty(), "ignore must drop the id too");
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
        let go = subject(Provider::Go, "github.com/junegunn/fzf");
        assert_eq!(
            parse_subject(&go),
            Some((Provider::Go, "github.com/junegunn/fzf"))
        );
    }
}

#[cfg(test)]
mod detector_tests {
    use super::*;

    #[test]
    fn node_modules_expands_scopes_and_reads_versions() {
        let tmp = tempfile::TempDir::new().unwrap();
        for dir in ["typescript", "@biomejs/biome", ".bin", ".hidden"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(
            tmp.path().join("typescript/package.json"),
            r#"{"name":"typescript","version":"5.5.2"}"#,
        )
        .unwrap();
        let found = node_modules(tmp.path()).unwrap();
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![
                ("@biomejs/biome".to_string(), None),
                ("typescript".to_string(), Some("5.5.2".to_string()))
            ]
        );
    }

    #[test]
    fn crates_toml_reads_cargo_installs_with_versions() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".crates.toml"),
            "[v1]\n\"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"rg\"]\n",
        )
        .unwrap();
        let found = crates_toml(tmp.path()).unwrap();
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![("ripgrep".to_string(), Some("14.1.0".to_string()))]
        );
        // No file yet = truthfully empty, not an error.
        let empty = tempfile::TempDir::new().unwrap();
        assert_eq!(crates_toml(empty.path()).unwrap().len(), 0);
    }

    #[test]
    fn go_bin_reads_embedded_module_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("fzf"),
            crate::gobuild::synthesize("github.com/junegunn/fzf", "v0.46.1"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("not-go"), b"some script").unwrap();
        let mut cache = GoCache::default();
        let found = go_bin(tmp.path(), &mut cache);
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![(
                "github.com/junegunn/fzf".to_string(),
                Some("v0.46.1".to_string())
            )]
        );
        // Second read comes from the cache (same answer, no re-parse).
        assert_eq!(go_bin(tmp.path(), &mut cache).len(), 1);
    }

    #[test]
    fn gem_names_split_off_version_and_platform() {
        assert_eq!(
            gem_name_version("nokogiri-1.16.0-arm64-darwin"),
            ("nokogiri".to_string(), Some("1.16.0".to_string()))
        );
        assert_eq!(
            gem_name_version("aws-sdk-s3-1.140.0"),
            ("aws-sdk-s3".to_string(), Some("1.140.0".to_string()))
        );
        assert_eq!(gem_name_version("weird"), ("weird".to_string(), None));

        let tmp = tempfile::TempDir::new().unwrap();
        let spec = tmp.path().join("specifications");
        std::fs::create_dir_all(&spec).unwrap();
        std::fs::write(spec.join("rails-7.1.3.gemspec"), b"Gem::Specification").unwrap();
        let found = gem_specs(tmp.path()).unwrap();
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![("rails".to_string(), Some("7.1.3".to_string()))]
        );
        // A gem home with no specifications dir is truthfully empty.
        let bare = tempfile::TempDir::new().unwrap();
        assert_eq!(gem_specs(bare.path()).unwrap().len(), 0);
    }

    #[test]
    fn versioned_dirs_read_the_version_subdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("raycast/1.70.0")).unwrap();
        let found = versioned_dirs(tmp.path()).unwrap();
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![("raycast".to_string(), Some("1.70.0".to_string()))]
        );
    }

    #[test]
    fn provider_command_tables_are_complete() {
        for provider in Provider::ALL {
            let install = provider.install_args("x");
            let uninstall = provider.uninstall_args("x");
            match provider {
                // Drag-installed: wukong can only remember.
                Provider::App => assert!(install.is_none() && uninstall.is_none()),
                // Installable, but removal is "delete it yourself".
                Provider::Mas | Provider::Go => {
                    assert!(install.is_some() && uninstall.is_none());
                }
                _ => {
                    assert!(install.unwrap().len() >= 3, "{provider:?}");
                    assert!(uninstall.is_some(), "{provider:?}");
                }
            }
        }
        let go = Provider::Go.install_args("github.com/x/y").unwrap();
        assert_eq!(go.last().unwrap(), "github.com/x/y@latest");
    }

    #[test]
    fn overridden_roots_win_and_absent_paths_disable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let npm = tmp.path().join("npmroot");
        std::fs::create_dir_all(&npm).unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("npm".to_string(), npm.clone());
        overrides.insert("cargo".to_string(), tmp.path().join("absent"));
        // Hermetic: pnpm would otherwise FORK the real tool.
        overrides.insert("pnpm".to_string(), tmp.path().join("absent"));
        let roots = detect_roots(
            Some(tmp.path().join("nobrew").as_path()),
            Some(tmp.path().join("noapps").as_path()),
            &overrides,
            tmp.path().join("home").as_path(),
        );
        let npm_root = roots.active.iter().find(|r| r.provider == Provider::Npm);
        assert_eq!(npm_root.map(|r| r.path.clone()), Some(npm));
        assert_eq!(npm_root.map(|r| r.origin), Some(RootOrigin::Override));
        assert!(!roots.active.iter().any(|r| r.provider == Provider::Cargo));
    }

    #[test]
    fn status_reports_every_provider_and_mas_rides_apps() {
        let tmp = tempfile::TempDir::new().unwrap();
        let apps = tmp.path().join("Applications");
        std::fs::create_dir_all(&apps).unwrap();
        // Hermetic: npm/pnpm defaults fork the REAL tools (a wedged
        // shim once hung this very test for two hours).
        let mut overrides = BTreeMap::new();
        overrides.insert("npm".to_string(), tmp.path().join("absent"));
        overrides.insert("pnpm".to_string(), tmp.path().join("absent"));
        let roots = detect_roots(
            Some(tmp.path().join("nobrew").as_path()),
            Some(&apps),
            &overrides,
            tmp.path().join("home").as_path(),
        );
        let status = roots.status();
        assert_eq!(status.len(), Provider::ALL.len());
        let mas = status.iter().find(|s| s.provider == Provider::Mas).unwrap();
        assert!(mas.active);
        assert_eq!(mas.path.as_deref(), Some(apps.as_path()));
        let formula = status
            .iter()
            .find(|s| s.provider == Provider::Formula)
            .unwrap();
        assert!(!formula.active, "override at a nonexistent dir disables");
    }
}
