//! The governor's brain. Owns the database, the store, and the config;
//! turns raw file events into debounced, gated, committed history; and
//! answers the IPC requests the clients send. Everything that mutates
//! state funnels through here so there is one writer and no locks to
//! reason about beyond the engine's own `&mut self`.
//!
//! The hot path is allocation-light on purpose: `touch` runs for every
//! filesystem event and consults only in-memory sets — the tracked
//! roster and the precomputed sentinel lists — never the database.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wukong_core::db::InboxOutcome;
use wukong_core::events::{EventKind, InboxKind, Resolution};
use wukong_core::gate::{self, Finding, GateVerdict};
use wukong_core::ipc::{Request, Response, StatusInfo, TrackedFile};
use wukong_core::pkg::{Manifest, Provider, Roots};
use wukong_core::{Config, Db, Store, paths};

/// Inbox bodies are evidence, not archives.
const BODY_MAX_LINES: usize = 300;
const BODY_MAX_BYTES: usize = 16 * 1024;
/// A tracked file larger than this is skipped with a logged error —
/// the governor is for dotfiles, not disk images.
const MAX_TRACKED_BYTES: usize = 10 * 1024 * 1024;
/// Sentinel offers never read files larger than this.
const MAX_SENTINEL_BYTES: u64 = 1024 * 1024;
/// Cap on fingerprints stored per inbox item.
const MAX_META_FINGERPRINTS: usize = 100;

mod packages;
mod settings;

#[cfg(test)]
mod tests;

#[allow(clippy::struct_excessive_bools)] // independent runtime state bits, not a config surface
pub struct Engine {
    pub config: Config,
    db: Db,
    store: Store,
    started: Instant,
    /// Pending debounced paths → when they were last touched.
    pending: HashMap<PathBuf, Instant>,
    last_commit: Option<String>,
    last_push: Option<String>,
    dirty: bool,
    push_in_flight: bool,
    /// Monotonic commit counter; `begin_push` snapshots it so a commit
    /// landing during an in-flight push is never marked as pushed.
    commits: u64,
    push_snapshot: u64,
    /// Cached `git rev-list` count so `status` doesn't fork git on
    /// every TUI poll.
    unpushed: usize,
    /// True when the on-disk manifest exists but failed to parse —
    /// saving over it would erase the real one, so saves are refused.
    manifest_poisoned: bool,
    /// (dir, recursive) watch roots the loop should start watching —
    /// filled when files are tracked or a sentinel is promoted to a
    /// directory, drained by main after each message batch.
    watch_requests: Vec<(PathBuf, bool)>,
    /// Canonical live paths of tracked files: the hot-path roster.
    tracked_live: HashSet<PathBuf>,
    /// The subset stored as age ciphertext.
    sealed_live: HashSet<PathBuf>,
    /// The subset living in the shared lane — mirrored on the `shared`
    /// branch every machine overlays.
    shared_files: HashSet<PathBuf>,
    /// The public recipient every sealed commit encrypts to, loaded
    /// from the store (created on first seal).
    recipient: Option<String>,
    sentinel_files: Vec<PathBuf>,
    sentinel_dirs: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
    /// Package governance: the synced manifest, where the detectors
    /// look, and a debounce mark for the next reconcile.
    manifest: Manifest,
    /// The shared lane's manifest: packages every machine wants.
    shared_manifest: Manifest,
    pkg_roots: Roots,
    pkg_watch: Vec<(PathBuf, bool)>,
    pkg_dirty: Option<Instant>,
    /// The reconcile's snapshot of what's installed — `pkg_list` serves
    /// this instead of walking the Cellar per request.
    pkg_installed: Vec<(Provider, wukong_core::pkg::Installed)>,
    /// When the Cellar first showed a formula dir without a receipt
    /// (a pour in progress); bounds the re-arm loop.
    pkg_unsettled_since: Option<Instant>,
    /// Settings governance: desired state, where preferences live
    /// (None = disabled), and the reconcile debounce mark.
    settings_manifest: wukong_core::settings::SettingsManifest,
    /// Settings every machine wants; this machine's manifest wins
    /// per key.
    shared_settings: wukong_core::settings::SettingsManifest,
    settings_poisoned: bool,
    prefs_dir: Option<PathBuf>,
    settings_dirty: Option<Instant>,
    /// An in-flight capture snapshot: every top-level scalar pref key,
    /// held in memory only, consumed by the diff.
    capture: Option<(Instant, PrefsSnapshot)>,
}

/// The tracked roster and its sealed and shared subsets, as canonical
/// live paths.
type Roster = (HashSet<PathBuf>, HashSet<PathBuf>, HashSet<PathBuf>);
fn roster(db: &Db) -> anyhow::Result<Roster> {
    let rows = db.tracked()?;
    let tracked = rows
        .iter()
        .map(|(rel, _, _)| paths::from_store_rel(Path::new(rel)))
        .collect();
    let sealed = rows
        .iter()
        .filter(|(_, sealed, _)| *sealed)
        .map(|(rel, _, _)| paths::from_store_rel(Path::new(rel)))
        .collect();
    let shared = rows
        .iter()
        .filter(|(_, _, shared)| *shared)
        .map(|(rel, _, _)| paths::from_store_rel(Path::new(rel)))
        .collect();
    Ok((tracked, sealed, shared))
}

/// The shared lane's package and settings manifests, from its
/// worktree. Unreadable manifests read as empty — the shared lane
/// must never take the daemon down.
fn load_shared_manifests(store: &Store) -> (Manifest, wukong_core::settings::SettingsManifest) {
    let dir = store.shared().dir().to_path_buf();
    (
        Manifest::load(&dir).unwrap_or_default().unwrap_or_default(),
        wukong_core::settings::SettingsManifest::load(&dir)
            .unwrap_or_default()
            .unwrap_or_default(),
    )
}

/// Every (domain, key) → scalar value at one moment in time.
type PrefsSnapshot = std::collections::BTreeMap<(String, String), wukong_core::settings::Value>;

impl Engine {
    /// Paths are injected so tests can run an engine against a tempdir.
    pub fn new(config: Config, db_path: &Path, store_dir: &Path) -> anyhow::Result<Self> {
        let db = Db::open(db_path)?;
        let store = Store::open(store_dir, &config.machine)?;
        if !config.remote.is_empty() {
            store.ensure_remote(&config.remote)?;
        }
        db.record(EventKind::DaemonStarted, &config.machine, "")?;
        let (tracked_live, sealed_live, shared_files) = roster(&db)?;
        let recipient = std::fs::read_to_string(store.dir().join(wukong_core::seal::RECIPIENT_REL))
            .ok()
            .map(|s| s.trim().to_string());
        let (mut sentinel_files, mut sentinel_dirs) = (Vec::new(), Vec::new());
        for s in config.sentinel_paths() {
            if s.is_dir() {
                sentinel_dirs.push(s);
            } else {
                sentinel_files.push(s);
            }
        }
        let excludes = config.exclude_paths();
        let (manifest, manifest_poisoned) = match Manifest::load(store.dir()) {
            Ok(m) => (m.unwrap_or_default(), false),
            Err(e) => {
                eprintln!("wukongd: {e} — package manifest is READ-ONLY until fixed");
                (Manifest::default(), true)
            }
        };
        let pkg_roots = if config.packages.enabled {
            config.pkg_roots()
        } else {
            Roots::default()
        };
        let pkg_watch: Vec<(PathBuf, bool)> = pkg_roots
            .watch_roots()
            .iter()
            .map(|(r, recursive)| (paths::canonicalize_lenient(r), *recursive))
            .collect();
        let (settings_manifest, settings_poisoned) =
            match wukong_core::settings::SettingsManifest::load(store.dir()) {
                Ok(m) => (m.unwrap_or_default(), false),
                Err(e) => {
                    soft(Err::<(), _>(format!(
                        "{e} — settings manifest is READ-ONLY until fixed"
                    )));
                    (wukong_core::settings::SettingsManifest::default(), true)
                }
            };
        let prefs_dir = if config.settings.enabled {
            let dir = config
                .settings
                .preferences_dir
                .clone()
                .unwrap_or_else(|| paths::home().join("Library/Preferences"));
            Some(paths::canonicalize_lenient(&dir))
        } else {
            None
        };
        // Unpushed commits can exist at startup (a push interval that
        // never fired before shutdown); derive dirtiness from git, not
        // from a fresh bool, or they'd sit local until the next edit.
        let remote_configured = !config.remote.is_empty();
        let unpushed =
            store.unpushed(remote_configured) + store.shared().unpushed(remote_configured);
        let (shared_manifest, shared_settings) = load_shared_manifests(&store);
        Ok(Self {
            dirty: remote_configured && unpushed > 0,
            unpushed,
            config,
            db,
            store,
            started: Instant::now(),
            pending: HashMap::new(),
            last_commit: None,
            last_push: None,
            push_in_flight: false,
            commits: 0,
            push_snapshot: 0,
            manifest_poisoned,
            watch_requests: Vec::new(),
            tracked_live,
            sealed_live,
            shared_files,
            recipient,
            sentinel_files,
            sentinel_dirs,
            excludes,
            manifest,
            shared_manifest,
            shared_settings,
            pkg_roots,
            pkg_watch,
            pkg_dirty: None,
            pkg_installed: Vec::new(),
            pkg_unsettled_since: None,
            settings_manifest,
            settings_poisoned,
            prefs_dir,
            settings_dirty: None,
            capture: None,
        })
    }

    /// What to watch at startup, with an explicit recursion mode per
    /// root. File sentinels and tracked files are covered by watching
    /// their parent directory NON-recursively — that survives editors'
    /// atomic renames and files that don't exist yet, and it must
    /// never escalate to watching `$HOME` recursively — the parent of
    /// a missing ~/.profile is $HOME itself, and recursion there would
    /// tail every build tree the user owns. Only true directory
    /// sentinels (~/.config, ~/Library/LaunchAgents) watch recursively.
    pub fn initial_watch_roots(&self) -> Vec<(PathBuf, bool)> {
        let mut roots: HashMap<PathBuf, bool> = HashMap::new();
        for dir in &self.sentinel_dirs {
            roots.insert(dir.clone(), true);
        }
        for file in &self.sentinel_files {
            if let Some(parent) = file.parent() {
                roots.entry(parent.to_path_buf()).or_insert(false);
            }
        }
        for live in &self.tracked_live {
            if let Some(parent) = live.parent() {
                roots.entry(parent.to_path_buf()).or_insert(false);
            }
        }
        for (root, recursive) in &self.pkg_watch {
            let entry = roots.entry(root.clone()).or_insert(*recursive);
            *entry = *entry || *recursive;
        }
        if let Some(prefs) = &self.prefs_dir {
            roots.entry(prefs.clone()).or_insert(false);
        }
        let mut out: Vec<(PathBuf, bool)> = roots.into_iter().collect();
        out.sort();
        out
    }

    /// New watch roots requested since the last drain.
    pub fn drain_watch_requests(&mut self) -> Vec<(PathBuf, bool)> {
        std::mem::take(&mut self.watch_requests)
    }

    fn request_watch(&mut self, dir: &Path, recursive: bool) {
        if !self.watch_requests.iter().any(|(d, _)| d == dir) {
            self.watch_requests.push((dir.to_path_buf(), recursive));
        }
    }

    /// Every touched path the watcher reports. Cheap: in-memory set
    /// lookups, then remember the path and its time; the real work
    /// waits for the debounce to settle in `tick`.
    pub fn touch(&mut self, path: PathBuf) {
        if path.starts_with(self.store.dir()) || is_noise(&path) {
            return;
        }
        // A sentinel classified as a file at startup (because it did
        // not exist yet) may turn out to be a directory: promote it so
        // its children are governed from now on.
        if let Some(ix) = self.sentinel_files.iter().position(|f| f == &path)
            && path.is_dir()
        {
            let dir = self.sentinel_files.swap_remove(ix);
            self.request_watch(&dir, true);
            self.sentinel_dirs.push(dir);
            return;
        }
        // Preference churn marks a settings reconcile — cfprefsd
        // rewrites plists constantly, so per-path settling would be
        // all noise; a debounced set-level diff is quiet by design.
        if let Some(prefs) = &self.prefs_dir
            && (path.parent() == Some(prefs.as_path()) || path == *prefs)
        {
            self.settings_dirty = Some(Instant::now());
            return;
        }
        // Package-root churn marks a reconcile instead of a per-path
        // settle — packages are reconciled as a set, not file by file.
        if self
            .pkg_watch
            .iter()
            .any(|(root, _)| path.starts_with(root))
        {
            self.pkg_dirty = Some(Instant::now());
            return;
        }
        let relevant = self.tracked_live.contains(&path)
            || (self.under_sentinel(&path) && !self.excluded(&path));
        if relevant {
            self.pending.insert(path, Instant::now());
        }
    }

    /// The watcher lost events (queue overflow, forced rescan): treat
    /// every tracked file and file sentinel as possibly changed. Cheap
    /// — unchanged content settles into a no-op commit.
    pub fn rescan(&mut self) {
        let now = Instant::now();
        let all: Vec<PathBuf> = self
            .tracked_live
            .iter()
            .chain(self.sentinel_files.iter())
            .cloned()
            .collect();
        for path in all {
            self.pending.insert(path, now);
        }
        // Lost events may include package or settings transitions —
        // and new providers may have appeared since startup.
        self.redetect_roots();
        self.pkg_dirty = Some(now);
        self.settings_dirty = Some(now);
        // Other machines may have pushed to the shared lane; fold it
        // in (mirror only — live files change through `wukong sync`,
        // never behind the user's back) and reload the shared
        // manifests it may have replaced.
        if self.remote_configured() {
            match self.store.refresh_shared() {
                Ok(true) => {
                    self.reload_shared_manifests();
                    soft(self.db.record(
                        EventKind::Shared,
                        "shared",
                        "updates from another machine — `wukong sync` applies them",
                    ));
                }
                Ok(false) => {}
                Err(e) => soft(Err::<(), _>(e)),
            }
        }
    }

    /// Re-read the shared package and settings manifests after the
    /// shared branch moved.
    fn reload_shared_manifests(&mut self) {
        let dir = self.store.shared().dir().to_path_buf();
        match Manifest::load(&dir) {
            Ok(m) => self.shared_manifest = m.unwrap_or_default(),
            Err(e) => soft(Err::<(), _>(e)),
        }
        match wukong_core::settings::SettingsManifest::load(&dir) {
            Ok(m) => self.shared_settings = m.unwrap_or_default(),
            Err(e) => soft(Err::<(), _>(e)),
        }
    }

    fn under_sentinel(&self, path: &Path) -> bool {
        self.sentinel_files.iter().any(|s| s == path)
            || self.sentinel_dirs.iter().any(|d| path.starts_with(d))
    }

    fn excluded(&self, path: &Path) -> bool {
        self.excludes.iter().any(|e| path.starts_with(e))
    }

    /// Process any pending paths whose debounce window has elapsed.
    /// Returns how many new inbox items appeared (for notifications).
    pub fn tick(&mut self) -> usize {
        let debounce = Duration::from_secs(self.config.debounce_secs);
        let ready: Vec<PathBuf> = self
            .pending
            .extract_if(|_, touched| touched.elapsed() >= debounce)
            .map(|(path, _)| path)
            .collect();
        let mut new_inbox = 0;
        for path in ready {
            new_inbox += self.settle(&path);
        }
        if self.pkg_dirty.is_some_and(|t| t.elapsed() >= debounce) {
            new_inbox += self.reconcile();
        }
        if self.settings_dirty.is_some_and(|t| t.elapsed() >= debounce) {
            new_inbox += self.reconcile_settings();
        }
        new_inbox
    }

    /// A file has stopped changing. If tracked, mirror + gate + commit.
    /// If an untracked sentinel, offer it to the inbox.
    fn settle(&mut self, path: &Path) -> usize {
        let rel = paths::store_rel(path).to_string_lossy().into_owned();
        if self.tracked_live.contains(path) {
            self.commit_tracked(path, &rel)
        } else {
            self.offer_sentinel(path, &rel)
        }
    }

    /// The store a live file's mirror belongs to — a cheap clone, so
    /// callers keep exclusive access to the engine.
    fn lane(&self, live: &Path) -> Store {
        if self.shared_files.contains(live) {
            self.store.shared()
        } else {
            self.store.clone()
        }
    }

    /// Read a tracked live file for committing. `None` means the read
    /// itself concluded the flow: a true deletion commits the removal;
    /// unreadability and the size cap are recorded, never guessed at.
    fn read_for_commit(&mut self, path: &Path, rel: &str) -> Option<Vec<u8>> {
        match std::fs::read(path) {
            Ok(b) if b.len() > MAX_TRACKED_BYTES => {
                soft(Err::<(), _>(format!(
                    "{rel}: {} bytes exceeds the {MAX_TRACKED_BYTES}-byte tracked-file cap — not committed",
                    b.len()
                )));
                None
            }
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Actually deleted: drop it from the mirror and commit
                // the removal.
                let lane = self.lane(path);
                soft(lane.remove(path));
                let rel_path = paths::store_rel(path);
                self.commit_in(&lane, &rel_path, &format!("{rel}: removed"), rel, "removed");
                None
            }
            Err(e) => {
                // EACCES, EIO, EMFILE… are NOT deletions. Removing the
                // mirror here would commit a phantom delete.
                soft(Err::<(), _>(format!("read {rel}: {e}")));
                None
            }
        }
    }

    /// The gated commit flow for a tracked file:
    ///
    /// 1. scan — every finding, every line
    /// 2. findings without a stored allowance → quarantine, no commit
    /// 3. allowed findings → approved ones stay, redacted ones are
    ///    masked in the stored copy (the live file is never touched)
    /// 4. paranoia: the to-be-stored content is re-scanned; anything
    ///    unexpected holds the commit rather than trusting the mask
    fn commit_tracked(&mut self, path: &Path, rel: &str) -> usize {
        let Some(bytes) = self.read_for_commit(path, rel) else {
            return 0;
        };
        if self.sealed_live.contains(path) {
            return self.commit_sealed(path, rel, &bytes);
        }
        let scanned = gate::scan_bytes(path, &bytes);
        let content = scanned.text.as_str();

        let findings = match scanned.verdict {
            GateVerdict::Clean => Vec::new(),
            GateVerdict::Quarantine(f) => f,
            GateVerdict::Forbidden(why) => {
                // Reachable only for files tracked before the name
                // became forbidden. Loud, not silent.
                soft(self.db.record(EventKind::Held, rel, why));
                return 0;
            }
        };
        let allowances = self.db.allowances_for(rel).unwrap_or_else(|e| {
            // Fail CLOSED: with no allowances readable, everything
            // quarantines rather than slipping through.
            soft(Err::<(), _>(e));
            HashMap::new()
        });
        let new: Vec<&Finding> = findings
            .iter()
            .filter(|f| !allowances.contains_key(&f.fingerprint))
            .collect();
        if !new.is_empty() {
            return self.quarantine(path, rel, content, &new);
        }

        // Everything present is allowed; mask the redact-flagged spans
        // in the stored copy only.
        let must_redact: HashSet<&str> = allowances
            .iter()
            .filter(|(_, action)| action.as_str() == "redact")
            .map(|(fp, _)| fp.as_str())
            .collect();
        let needs_mask = findings
            .iter()
            .any(|f| must_redact.contains(f.fingerprint.as_str()));
        if needs_mask && scanned.reencoded {
            // The scan text is not byte-identical to the file (UTF-16
            // or lossy). Masking would write corrupted content; hold
            // instead of guessing.
            soft(self.db.record(
                EventKind::Held,
                rel,
                "redaction unsupported for re-encoded content",
            ));
            return self.quarantine(path, rel, content, &findings.iter().collect::<Vec<_>>());
        }
        let stored: Vec<u8> = if needs_mask {
            let masked = gate::mask_findings(content, &findings, |f| {
                !must_redact.contains(f.fingerprint.as_str())
            });
            // Trust nothing: the stored copy must scan clean apart
            // from deliberately approved fingerprints.
            if let GateVerdict::Quarantine(left) = gate::scan(path, &masked) {
                let unexpected = left
                    .iter()
                    .any(|f| allowances.get(&f.fingerprint).map(String::as_str) != Some("approve"));
                if unexpected {
                    soft(
                        self.db
                            .record(EventKind::Held, rel, "redaction verification failed"),
                    );
                    return self.quarantine(path, rel, content, &left.iter().collect::<Vec<_>>());
                }
            }
            masked.into_bytes()
        } else {
            bytes
        };

        // Summary must be computed against the OLD stored copy, so it
        // runs before mirror_in overwrites it.
        let stored_text = String::from_utf8_lossy(&stored).into_owned();
        let lane = self.lane(path);
        let mut summary = change_summary(&lane, path, &stored_text);
        // Audit trail: allow-marked lines carry exempted secrets.
        let allowed = gate::allow_marker_count(content);
        if allowed > 0 {
            let _ = write!(summary, " ({allowed} allow-marked)");
        }
        let rel_path = match lane.mirror_in(path, &stored) {
            Ok(rel_path) => rel_path,
            Err(e) => {
                soft(Err::<(), _>(e));
                return 0;
            }
        };
        self.commit_in(
            &lane,
            &rel_path,
            &format!("{rel}: {summary}"),
            rel,
            &summary,
        );
        0
    }

    /// Commit one path and record the outcome — failures included,
    /// loudly: a commit that silently doesn't happen is drift.
    fn commit_scoped(&mut self, rel_path: &Path, message: &str, rel: &str, summary: &str) {
        let store = self.store.clone();
        self.commit_in(&store, rel_path, message, rel, summary);
    }

    /// The lane-aware commit: same bookkeeping whichever branch takes
    /// the commit.
    fn commit_in(
        &mut self,
        store: &Store,
        rel_path: &Path,
        message: &str,
        rel: &str,
        summary: &str,
    ) {
        match store.commit(rel_path, message) {
            Ok(Some(sha)) => self.after_commit(rel, &sha, summary),
            Ok(None) => {}
            Err(e) => {
                soft(self.db.record(EventKind::Held, rel, "commit failed"));
                soft(Err::<(), _>(e));
            }
        }
    }

    /// The sealed lane: no gate scan (the whole point is that secrets
    /// live here), no plaintext in the store — and a content-hash
    /// guard, because age ciphertext differs on every encryption and
    /// would otherwise commit forever.
    fn commit_sealed(&mut self, path: &Path, rel: &str, plaintext: &[u8]) -> usize {
        let hash = wukong_core::seal::content_hash(plaintext);
        if self.db.content_hash(rel).ok().flatten().as_deref() == Some(hash.as_str()) {
            return 0;
        }
        let Some(recipient) = self.ensure_recipient() else {
            soft(Err::<(), _>(format!(
                "{rel}: no seal recipient — not committed"
            )));
            return 0;
        };
        let sealed = match wukong_core::seal::encrypt(&recipient, plaintext) {
            Ok(bytes) => bytes,
            Err(e) => {
                soft(Err::<(), _>(e));
                return 0;
            }
        };
        let lane = self.lane(path);
        match lane.mirror_in(path, &sealed) {
            Ok(rel_path) => {
                self.commit_in(
                    &lane,
                    &rel_path,
                    &format!("{rel}: sealed update"),
                    rel,
                    "sealed",
                );
                soft(self.db.set_content_hash(rel, &hash));
            }
            Err(e) => soft(Err::<(), _>(e)),
        }
        0
    }

    /// The recipient, creating the whole key pair on first use: the
    /// identity goes to the configured store (Keychain by default),
    /// the public recipient into the store repo so every clone can
    /// encrypt.
    fn ensure_recipient(&mut self) -> Option<String> {
        if let Some(r) = &self.recipient {
            return Some(r.clone());
        }
        let id_store = wukong_core::seal::IdentityStore::from_config(
            self.config.seal.identity_file.as_deref(),
        );
        match id_store.load() {
            Ok(Some(_)) => {
                // An identity exists but the recipient file is missing
                // (fresh clone without the recipient? repaired store):
                // we cannot derive the recipient from here without the
                // identity API — regenerate is WRONG. Ask the user.
                soft(Err::<(), _>(
                    "seal identity exists but __wukong__/age.recipient is missing — run `wukong seal-key status`",
                ));
                None
            }
            Ok(None) => {
                let (identity, recipient) = wukong_core::seal::generate();
                if let Err(e) = id_store.save(&identity) {
                    soft(Err::<(), _>(e));
                    return None;
                }
                let path = self.store.dir().join(wukong_core::seal::RECIPIENT_REL);
                if let Some(dir) = path.parent() {
                    soft(std::fs::create_dir_all(dir));
                }
                soft(std::fs::write(&path, format!("{recipient}\n")));
                self.commit_scoped(
                    Path::new(wukong_core::seal::RECIPIENT_REL),
                    "seal: recipient established",
                    "seal",
                    "recipient established",
                );
                soft(
                    self.db
                        .record(EventKind::Sealed, "seal", "key pair created"),
                );
                self.recipient = Some(recipient.clone());
                Some(recipient)
            }
            Err(e) => {
                soft(Err::<(), _>(e));
                None
            }
        }
    }

    /// Move a tracked file between the machine and shared lanes: the
    /// stored bytes move as-is (ciphertext stays ciphertext), each
    /// lane commits its half of the move, and the roster follows.
    fn share(&mut self, path: &str, undo: bool) -> Response {
        let live = paths::resolve_input(path);
        if !self.tracked_live.contains(&live) {
            return Response::Error {
                message: format!(
                    "{} is not tracked — `wukong track --shared` does both at once",
                    paths::display(&live)
                ),
            };
        }
        if self.shared_files.contains(&live) != undo {
            return Response::Ok {
                message: format!(
                    "{} is already in the {} lane",
                    paths::display(&live),
                    if undo { "machine" } else { "shared" }
                ),
            };
        }
        let rel_path = paths::store_rel(&live);
        let rel = rel_path.to_string_lossy().into_owned();
        let (from, to) = if undo {
            (self.store.shared(), self.store.clone())
        } else {
            (self.store.clone(), self.store.shared())
        };
        let bytes = match std::fs::read(from.dir().join(&rel_path)) {
            Ok(b) => b,
            Err(e) => {
                return Response::Error {
                    message: format!("no stored copy to move: {e}"),
                };
            }
        };
        if let Err(e) = to.mirror_in(&live, &bytes) {
            return err(e);
        }
        let lane_name = if undo { "machine" } else { "shared" };
        self.commit_in(
            &to,
            &rel_path,
            &format!("{rel}: joins the {lane_name} lane"),
            &rel,
            "lane change",
        );
        soft(from.remove(&live));
        self.commit_in(
            &from,
            &rel_path,
            &format!("{rel}: moved to the {lane_name} lane"),
            &rel,
            "lane change",
        );
        soft(self.db.set_shared(&rel, !undo));
        if undo {
            self.shared_files.remove(&live);
        } else {
            self.shared_files.insert(live.clone());
        }
        soft(self.db.record(EventKind::Shared, &rel, lane_name));
        let mut message = format!(
            "{} now syncs to the {lane_name} lane",
            paths::display(&live)
        );
        if !undo && self.sealed_live.contains(&live) {
            message.push_str(
                "\nnote: it is sealed — every machine needs this store's seal identity \
                 (`wukong seal-key export` / `import`)",
            );
        }
        Response::Ok { message }
    }

    /// Convert a tracked file to the sealed lane and commit ciphertext.
    fn seal(&mut self, path: &str) -> Response {
        let live = paths::resolve_input(path);
        if !self.tracked_live.contains(&live) {
            return Response::Error {
                message: format!(
                    "{} is not tracked — `wukong track --sealed` does both at once",
                    paths::display(&live)
                ),
            };
        }
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        soft(self.db.set_sealed(&rel, true));
        soft(self.db.set_content_hash(&rel, "")); // force the first sealed commit
        self.sealed_live.insert(live.clone());
        soft(self.db.record(EventKind::Sealed, &rel, ""));
        self.commit_tracked(&live, &rel);
        Response::Ok {
            message: format!(
                "{} is sealed — the store holds only ciphertext from now on",
                paths::display(&live)
            ),
        }
    }

    /// Back to the plaintext lane — through the gate, which may
    /// quarantine what the seal was hiding.
    fn unseal(&mut self, path: &str) -> Response {
        let live = paths::resolve_input(path);
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        if !self.sealed_live.remove(&live) {
            return Response::Error {
                message: format!("{} is not sealed", paths::display(&live)),
            };
        }
        soft(self.db.set_sealed(&rel, false));
        soft(self.db.set_content_hash(&rel, ""));
        soft(self.db.record(EventKind::Unsealed, &rel, ""));
        let held = self.commit_tracked(&live, &rel);
        Response::Ok {
            message: if held > 0 {
                format!(
                    "{} unsealed — its secrets are now HELD in the inbox for review",
                    paths::display(&live)
                )
            } else {
                format!("{} unsealed and committed plaintext", paths::display(&live))
            },
        }
    }

    fn quarantine(&mut self, path: &Path, rel: &str, content: &str, new: &[&Finding]) -> usize {
        let diff = self.lane(path).diff_against_live(path, content);
        let body = quarantine_body(new, &diff);
        // A pathological file can carry thousands of findings; the row
        // must stay bounded.
        let meta = fingerprint_json(&new[..new.len().min(MAX_META_FINGERPRINTS)]);
        let outcome = self
            .db
            .inbox_add(
                InboxKind::Quarantine,
                rel,
                &format!("{} secret finding(s) held", new.len()),
                &body,
                &meta,
            )
            .unwrap_or_else(refreshed);
        soft(self.db.record(
            EventKind::Quarantined,
            rel,
            &format!("{} finding(s)", new.len()),
        ));
        usize::from(outcome == InboxOutcome::New)
    }

    fn offer_sentinel(&mut self, path: &Path, rel: &str) -> usize {
        // Only files, only ones that still exist, and never files whose
        // name is forbidden — offering ~/.config/foo/credentials.json
        // would put its contents in the inbox and invite tracking a
        // file the gate must refuse.
        if !path.is_file() {
            return 0;
        }
        // Sentinel offers are unsolicited; a huge cache artifact must
        // not be read wholesale into the event loop.
        if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_SENTINEL_BYTES) {
            return 0;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return 0;
        };
        let scanned = gate::scan_bytes(path, &bytes);
        if matches!(scanned.verdict, GateVerdict::Forbidden(_)) {
            return 0;
        }
        let body = truncate_body(&gate::mask_all(
            &self.store.diff_against_live(path, &scanned.text),
        ));
        let outcome = self
            .db
            .inbox_add(
                InboxKind::Sentinel,
                rel,
                "changed — start tracking it?",
                &body,
                "",
            )
            .unwrap_or_else(refreshed);
        soft(self.db.record(EventKind::SentinelChanged, rel, ""));
        usize::from(outcome == InboxOutcome::New)
    }

    fn after_commit(&mut self, rel: &str, sha: &str, summary: &str) {
        soft(self.db.record(EventKind::Committed, rel, summary));
        self.last_commit = Some(sha.to_string());
        self.commits += 1;
        // "Unpushed" only means something when there is somewhere to
        // push; a local-only config would otherwise count forever.
        if self.remote_configured() {
            self.unpushed += 1;
            self.dirty = true;
        }
    }

    // ---- Push (runs off-loop; the engine only tracks state) ------------

    pub fn wants_push(&self) -> bool {
        !self.config.remote.is_empty() && self.dirty && !self.push_in_flight
    }

    pub fn push_in_flight(&self) -> bool {
        self.push_in_flight
    }

    pub fn remote_configured(&self) -> bool {
        !self.config.remote.is_empty()
    }

    /// Hand out a store clone for a blocking push task and mark the
    /// push in flight so timers don't double-fire.
    pub fn begin_push(&mut self) -> Store {
        self.push_in_flight = true;
        self.store.clone()
    }

    pub fn finish_push(&mut self, result: Result<(), String>) {
        self.push_in_flight = false;
        match result {
            Ok(()) => {
                self.last_push = Some(now());
                // Only clean if nothing landed while the push ran — a
                // commit made mid-push is NOT on the remote yet.
                self.dirty = self.commits != self.push_snapshot;
                self.unpushed = self.store.unpushed(self.remote_configured())
                    + self.store.shared().unpushed(self.remote_configured());
                soft(self.db.record(EventKind::Pushed, self.store.branch(), ""));
                soft(self.db.prune_events());
            }
            Err(err) => {
                soft(
                    self.db
                        .record(EventKind::PushFailed, self.store.branch(), &err),
                );
            }
        }
    }

    // ---- IPC -----------------------------------------------------------

    pub fn handle(&mut self, req: Request) -> Response {
        match req {
            Request::Ping => Response::Pong {
                version: wukong_core::ipc::PROTOCOL_VERSION,
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            Request::Status => self.status(),
            Request::Track {
                path,
                sealed,
                shared,
            } => self.track(&path, sealed, shared),
            Request::Share { path, undo } => self.share(&path, undo),
            Request::PkgShare {
                provider,
                name,
                undo,
            } => self.pkg_share(provider, &name, undo),
            Request::SettingShare { domain, key, undo } => self.setting_share(&domain, &key, undo),
            Request::Untrack { path } => self.untrack(&path),
            Request::TrackedList => self.tracked_list(),
            Request::InboxList => match self.db.inbox_open() {
                Ok(items) => Response::Inbox { items },
                Err(e) => err(e),
            },
            Request::InboxResolve { id, resolution } => self.resolve(id, resolution),
            Request::Events { limit } => match self.db.events(limit) {
                Ok(events) => Response::Events { events },
                Err(e) => err(e),
            },
            Request::Restore {
                path,
                force,
                dry_run,
            } => {
                if dry_run {
                    self.restore_plan()
                } else {
                    self.restore(path.as_deref(), force)
                }
            }
            Request::PkgRecord {
                provider,
                name,
                remove,
                shared,
                observe_only,
            } => self.pkg_record(provider, &name, remove, shared, observe_only),
            Request::PkgList => self.pkg_list(),
            Request::PkgProviders => self.pkg_providers(),
            Request::PkgIgnore {
                provider,
                name,
                unignore,
            } => self.pkg_ignore(provider, &name, unignore),
            Request::PkgAdoptInstalled => self.pkg_adopt_installed(),
            Request::Seal { path } => self.seal(&path),
            Request::Unseal { path } => self.unseal(&path),
            Request::SettingsList => self.settings_list(),
            Request::SettingsRecord { domain, key } => self.settings_record(&domain, &key),
            Request::SettingsIgnore {
                domain,
                key,
                unignore,
            } => self.settings_ignore(&domain, &key, unignore),
            Request::SettingsCaptureStart => self.capture_start(),
            Request::SettingsCaptureDiff => self.capture_diff(),
            Request::Exclude { path } => self.exclude(&path),
            Request::Diff { path } => self.diff(&path),
            Request::FileLog { path, limit } => self.file_log(&path, limit),
            // Push is orchestrated by the daemon loop so it can run off
            // this thread; reaching here means a wiring bug.
            Request::PushNow => Response::Error {
                message: "internal: push must be handled by the daemon loop".to_string(),
            },
        }
    }

    fn status(&self) -> Response {
        Response::Status(StatusInfo {
            machine: self.config.machine.clone(),
            remote: self.config.remote.clone(),
            tracked: self.tracked_live.len(),
            inbox: self.db.inbox_count().unwrap_or(0),
            last_commit: self.last_commit.clone(),
            last_push: self
                .last_push
                .clone()
                .or_else(|| self.db.last_event(EventKind::Pushed).ok().flatten()),
            unpushed: self.unpushed,
            uptime_secs: self.started.elapsed().as_secs(),
        })
    }

    /// Track a live path: resolve it, refuse forbidden files (unless
    /// sealed — ciphertext-only storage is exactly what makes a
    /// forbidden name safe to govern), mirror and commit immediately.
    fn track(&mut self, path: &str, sealed: bool, shared: bool) -> Response {
        let live = paths::resolve_input(path);
        if !live.is_file() {
            return Response::Error {
                message: format!("{} is not a file", paths::display(&live)),
            };
        }
        if live.starts_with(paths::data_dir()) {
            return Response::Error {
                message: "refused: that lives inside wukong's own data directory".to_string(),
            };
        }
        if paths::is_reserved(&live) {
            return Response::Error {
                message: format!("refused: {} is a reserved path", paths::display(&live)),
            };
        }
        let bytes = match std::fs::read(&live) {
            Ok(b) => b,
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        };
        let content = String::from_utf8_lossy(&bytes);
        if !sealed && let GateVerdict::Forbidden(why) = gate::scan(&live, &content) {
            return Response::Error {
                message: format!(
                    "refused: {} ({why}) — `wukong track --sealed` stores it as ciphertext only",
                    paths::display(&live)
                ),
            };
        }
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        match self.db.track(&rel, sealed) {
            Ok(_) => {
                soft(
                    self.db
                        .record(EventKind::Tracked, &rel, if sealed { "sealed" } else { "" }),
                );
                if sealed {
                    self.sealed_live.insert(live.clone());
                }
                if shared {
                    soft(self.db.set_shared(&rel, true));
                    self.shared_files.insert(live.clone());
                }
                self.adopt(&live);
                // Commit whatever is clean now; a quarantine lands in
                // the inbox but the file stays tracked.
                self.commit_tracked(&live, &rel);
                Response::Ok {
                    message: format!("tracking {}", paths::display(&live)),
                }
            }
            Err(e) => err(e),
        }
    }

    /// Roster + watch bookkeeping shared by every path that starts
    /// tracking a file.
    fn adopt(&mut self, live: &Path) {
        self.tracked_live.insert(live.to_path_buf());
        if let Some(parent) = live.parent() {
            self.request_watch(parent, false);
        }
    }

    fn untrack(&mut self, path: &str) -> Response {
        let live = paths::resolve_input(path);
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        match self.db.untrack(&rel) {
            Ok(true) => {
                let lane = self.lane(&live);
                self.tracked_live.remove(&live);
                self.sealed_live.remove(&live);
                self.shared_files.remove(&live);
                soft(lane.remove(&live));
                let rel_path = paths::store_rel(&live);
                self.commit_in(
                    &lane,
                    &rel_path,
                    &format!("{rel}: untracked"),
                    &rel,
                    "untracked",
                );
                soft(self.db.record(EventKind::Untracked, &rel, ""));
                Response::Ok {
                    message: format!("stopped tracking {}", paths::display(&live)),
                }
            }
            Ok(false) => Response::Error {
                message: format!("{} was not tracked", paths::display(&live)),
            },
            Err(e) => err(e),
        }
    }

    fn tracked_list(&self) -> Response {
        match self.db.tracked() {
            Ok(rows) => Response::Tracked {
                files: rows
                    .into_iter()
                    .map(|(rel, sealed, shared)| {
                        let live = paths::from_store_rel(Path::new(&rel));
                        TrackedFile {
                            display: paths::display(&live),
                            exists: live.exists(),
                            sealed,
                            shared,
                            path: rel,
                        }
                    })
                    .collect(),
            },
            Err(e) => err(e),
        }
    }

    fn resolve(&mut self, id: i64, resolution: Resolution) -> Response {
        let Ok(Some(item)) = self.db.inbox_get(id) else {
            return Response::Error {
                message: format!("inbox item {id} not found"),
            };
        };
        let live = paths::from_store_rel(Path::new(&item.subject));
        let kind = item.kind();

        // Package items: redact has no meaning, and the actions edit
        // the manifest rather than the mirror.
        if let Some(kind @ (InboxKind::Package | InboxKind::PackageGone)) = kind {
            return self.resolve_package(id, kind, &item.subject, resolution);
        }
        // Setting items likewise; the offered value rides in meta.
        if kind == Some(InboxKind::Setting) {
            return self.resolve_setting(id, &item.meta, resolution);
        }
        if resolution == Resolution::Seal && kind != Some(InboxKind::Quarantine) {
            return Response::Error {
                message: "seal applies to quarantined secrets — for a sentinel offer, \
`wukong track --sealed` the file instead"
                    .to_string(),
            };
        }
        if resolution == Resolution::Redact && kind != Some(InboxKind::Quarantine) {
            return Response::Error {
                message: "redact applies to quarantined secrets only".to_string(),
            };
        }
        // A secret can't be waved off forever; skip holds it out of
        // git, which is the honest version of "never".
        if resolution == Resolution::Never && kind == Some(InboxKind::Quarantine) {
            return Response::Error {
                message: "a quarantine takes approve, redact, seal, or skip — \
skip keeps the change out of git"
                    .to_string(),
            };
        }
        // Sentinel + never = exclude the path: one word, one meaning.
        // exclude() persists it and resolves this item along with every
        // other open offer under the path.
        if resolution == Resolution::Never && kind == Some(InboxKind::Sentinel) {
            return self.exclude(&paths::display(&live));
        }

        // Guard at the moment of consequence: offers skip forbidden
        // names, but the denylist can grow between an offer and its
        // approval — re-check before tracking, or a forbidden file
        // would sit tracked-but-never-committing.
        if kind == Some(InboxKind::Sentinel)
            && resolution == Resolution::Approve
            && let Ok(bytes) = std::fs::read(&live)
            && let GateVerdict::Forbidden(why) = gate::scan(&live, &String::from_utf8_lossy(&bytes))
        {
            return Response::Error {
                message: format!("refused: {} ({why})", paths::display(&live)),
            };
        }

        // Close the item FIRST. The actions below can produce a fresh
        // quarantine for the same file; if this item were still open,
        // the new evidence would dedupe into it and be resolved away
        // with it.
        soft(self.db.inbox_resolve(id, resolution));
        soft(
            self.db
                .record(EventKind::Resolved, &item.subject, resolution.as_str()),
        );

        // Sentinel + approve means "start tracking".
        if kind == Some(InboxKind::Sentinel) && resolution == Resolution::Approve {
            soft(self.db.track(&item.subject, false));
            soft(
                self.db
                    .record(EventKind::Tracked, &item.subject, "from inbox"),
            );
            self.adopt(&live);
            self.commit_tracked(&live, &item.subject);
        }

        // Quarantine + approve/redact: persist the resolution per
        // fingerprint, then run the normal gated flow. If the file has
        // grown NEW secrets since the item was filed, the flow
        // quarantines them instead of blindly committing — approval
        // covers exactly the findings it was granted for.
        if kind == Some(InboxKind::Quarantine)
            && matches!(resolution, Resolution::Approve | Resolution::Redact)
        {
            for fp in fingerprints_from_json(&item.meta) {
                soft(self.db.allow(&item.subject, &fp, resolution.as_str()));
            }
            self.commit_tracked(&live, &item.subject);
        }

        // Quarantine + seal: the whole file moves to the ciphertext
        // lane; the held findings become moot (plaintext never reaches
        // git again).
        if kind == Some(InboxKind::Quarantine) && resolution == Resolution::Seal {
            let rel = item.subject.clone();
            soft(self.db.set_sealed(&rel, true));
            soft(self.db.set_content_hash(&rel, ""));
            self.sealed_live.insert(live.clone());
            soft(self.db.record(EventKind::Sealed, &rel, "from inbox"));
            self.commit_tracked(&live, &rel);
        }

        Response::Ok {
            message: format!("resolved {} ({})", item.subject, resolution.as_str()),
        }
    }

    /// The sentinel noise valve: stop offering anything under a path,
    /// now and permanently. In-memory immediately, persisted to config
    /// (when the config has an on-disk source), and every open sentinel
    /// offer under the path is resolved away. Tracked files are never
    /// affected — tracking always outranks excludes.
    fn exclude(&mut self, path: &str) -> Response {
        let canon = paths::resolve_input(path);
        if !self.excludes.contains(&canon) {
            self.excludes.push(canon.clone());
        }
        self.pending.retain(|p, _| !p.starts_with(&canon));
        if let Ok(items) = self.db.inbox_open() {
            for item in items {
                let live = paths::from_store_rel(Path::new(&item.subject));
                if item.kind() == Some(InboxKind::Sentinel) && live.starts_with(&canon) {
                    soft(self.db.inbox_resolve(item.id, Resolution::Skip));
                }
            }
        }
        let display = paths::display(&canon);
        if !self.config.exclude.contains(&display) {
            self.config.exclude.push(display.clone());
        }
        soft(self.config.persist_exclude(&display));
        Response::Ok {
            message: format!("excluded {display} — nothing under it will be offered again"),
        }
    }

    /// Live file vs stored copy. Raw, unmasked: this is the owner
    /// reading their own file at their own terminal, exactly like
    /// `git diff` would.
    fn diff(&mut self, path: &str) -> Response {
        let live = paths::resolve_input(path);
        if !self.tracked_live.contains(&live) {
            return Response::Error {
                message: format!("{} is not tracked", paths::display(&live)),
            };
        }
        let bytes = match std::fs::read(&live) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        };
        if self.sealed_live.contains(&live) {
            let rel = paths::store_rel(&live).to_string_lossy().into_owned();
            let live_hash = wukong_core::seal::content_hash(&bytes);
            let synced =
                self.db.content_hash(&rel).ok().flatten().as_deref() == Some(live_hash.as_str());
            return Response::Ok {
                message: if synced {
                    format!(
                        "{} is sealed; content matches the store",
                        paths::display(&live)
                    )
                } else {
                    format!(
                        "{} is sealed; content DIFFERS from the store (plaintext diff withheld)",
                        paths::display(&live)
                    )
                },
            };
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let diff = self.store.diff_against_live(&live, &content);
        Response::Ok {
            message: if diff.is_empty() {
                format!("{} matches the store", paths::display(&live))
            } else {
                diff
            },
        }
    }

    /// The store's history for one tracked file.
    fn file_log(&mut self, path: &str, limit: usize) -> Response {
        let live = paths::resolve_input(path);
        if !self.tracked_live.contains(&live) {
            return Response::Error {
                message: format!("{} is not tracked", paths::display(&live)),
            };
        }
        match self
            .store
            .log(&paths::store_rel(&live), limit.clamp(1, 500))
        {
            Ok(log) if log.is_empty() => Response::Ok {
                message: "no commits yet".to_string(),
            },
            Ok(log) => Response::Ok { message: log },
            Err(e) => err(e),
        }
    }

    /// Copy stored files back to their live locations and track them —
    /// the new-machine bootstrap. Existing files that differ are
    /// skipped unless forced.
    /// The would-do report for `wukong sync`: what restore would
    /// create, what it would refuse to overwrite, what already
    /// matches. Reads everything, writes nothing.
    fn restore_plan(&self) -> Response {
        let sources = match self.restore_sources() {
            Ok(sources) => sources,
            Err(e) => return err(e),
        };
        let (mut create, mut in_sync) = (0usize, 0usize);
        let mut held = Vec::new();
        for (rel, lane) in sources {
            if rel.starts_with("__wukong__") {
                continue;
            }
            let Ok(stored) = std::fs::read(lane.dir().join(&rel)) else {
                continue;
            };
            let live = paths::from_store_rel(&rel);
            if wukong_core::seal::is_sealed(&stored) {
                // Plaintext comparison would need the key; report the
                // sealed file as work if the live copy is absent.
                if live.is_file() {
                    in_sync += 1;
                } else {
                    create += 1;
                }
                continue;
            }
            match std::fs::read(&live) {
                Err(_) => create += 1,
                Ok(b) if b == stored => in_sync += 1,
                Ok(_) => held.push(paths::display(&live)),
            }
        }
        let mut message = format!("{create} to restore, {in_sync} already match");
        if !held.is_empty() {
            let _ = write!(
                message,
                "\ndiffer on this machine (restore --force overwrites):\n  {}",
                held.join("\n  ")
            );
        }
        Response::Ok { message }
    }

    /// Every restorable (rel, lane) pair: the machine branch, then
    /// shared files the machine branch doesn't shadow.
    fn restore_sources(&self) -> Result<Vec<(PathBuf, Store)>, wukong_core::store::StoreError> {
        let machine: Vec<PathBuf> = self.store.files()?;
        let covered: HashSet<PathBuf> = machine.iter().cloned().collect();
        let shared = self.store.shared();
        let mut out: Vec<(PathBuf, Store)> = machine
            .into_iter()
            .map(|rel| (rel, self.store.clone()))
            .collect();
        for rel in shared.files()? {
            if !covered.contains(&rel) {
                out.push((rel, shared.clone()));
            }
        }
        Ok(out)
    }

    fn restore(&mut self, path: Option<&str>, force: bool) -> Response {
        let sources: Vec<(PathBuf, Store)> = match path {
            Some(p) => {
                let rel = paths::store_rel(&paths::resolve_input(p));
                // A single file restores from whichever lane holds it;
                // the machine branch shadows shared.
                let lane = if self.store.dir().join(&rel).is_file() {
                    self.store.clone()
                } else {
                    self.store.shared()
                };
                vec![(rel, lane)]
            }
            None => match self.restore_sources() {
                Ok(sources) => sources,
                Err(e) => return err(e),
            },
        };
        if sources.is_empty() {
            return Response::Error {
                message: "the store has no files to restore".to_string(),
            };
        }
        let (mut restored, mut skipped) = (0usize, Vec::new());
        for (rel, lane) in sources {
            // wukong's own artifacts (the manifest) are store state,
            // not live files — never "restore" them into $HOME.
            if rel.starts_with("__wukong__") {
                continue;
            }
            let rel_str = rel.to_string_lossy().into_owned();
            let Ok(mut stored) = std::fs::read(lane.dir().join(&rel)) else {
                skipped.push(format!("{rel_str} (not in store)"));
                continue;
            };
            if wukong_core::seal::is_sealed(&stored) {
                let id_store = wukong_core::seal::IdentityStore::from_config(
                    self.config.seal.identity_file.as_deref(),
                );
                let Some(identity) = id_store.load().ok().flatten() else {
                    skipped.push(format!(
                        "{rel_str} (sealed — import the key with `wukong seal-key import`)"
                    ));
                    continue;
                };
                match wukong_core::seal::decrypt(&identity, &stored) {
                    Ok(plain) => stored = plain,
                    Err(e) => {
                        skipped.push(format!("{rel_str} (decrypt failed: {e})"));
                        continue;
                    }
                }
            }
            let live = paths::from_store_rel(&rel);
            let differs = std::fs::read(&live).is_ok_and(|b| b != stored);
            if differs && !force {
                skipped.push(format!(
                    "{} (differs; --force overwrites)",
                    paths::display(&live)
                ));
                continue;
            }
            if let Some(dir) = live.parent()
                && std::fs::create_dir_all(dir).is_err()
            {
                skipped.push(format!("{} (cannot create parent)", paths::display(&live)));
                continue;
            }
            if write_private(&live, &stored).is_err() {
                skipped.push(format!("{} (write failed)", paths::display(&live)));
                continue;
            }
            let was_sealed = wukong_core::seal::is_sealed(
                &std::fs::read(lane.dir().join(&rel)).unwrap_or_default(),
            );
            soft(self.db.track(&rel_str, was_sealed));
            if was_sealed {
                self.sealed_live.insert(live.clone());
            }
            if lane.branch() == wukong_core::store::SHARED_BRANCH {
                soft(self.db.set_shared(&rel_str, true));
                self.shared_files.insert(live.clone());
            }
            self.adopt(&live);
            soft(self.db.record(EventKind::Restored, &rel_str, ""));
            restored += 1;
        }
        let mut message = format!("restored {restored} file(s)");
        if !skipped.is_empty() {
            let _ = write!(message, "\nskipped:\n  {}", skipped.join("\n  "));
        }
        Response::Ok { message }
    }
}

/// A one-line human summary for the commit message, computed BEFORE
/// the mirror is overwritten with the new content.
fn change_summary(store: &Store, path: &Path, new_stored: &str) -> String {
    let diff = store.diff_against_live(path, new_stored);
    if diff.is_empty() {
        return "updated".to_string();
    }
    let added = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let removed = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    match (added, removed) {
        (0, 0) => "updated".to_string(),
        (a, 0) => format!("+{a} lines"),
        (0, r) => format!("-{r} lines"),
        (a, r) => format!("+{a}/-{r} lines"),
    }
}

fn quarantine_body(findings: &[&Finding], diff: &str) -> String {
    let mut body = String::from("Held by the secret gate:\n");
    for f in findings.iter().take(20) {
        let _ = writeln!(body, "  line {}: {} — {}", f.line, f.rule, f.excerpt);
    }
    if findings.len() > 20 {
        let _ = writeln!(body, "  (… and {} more)", findings.len() - 20);
    }
    body.push('\n');
    // The diff is evidence, so it is masked — the database and the TUI
    // never hold a raw secret.
    body.push_str(&truncate_body(&gate::mask_all(diff)));
    body
}

fn fingerprint_json(findings: &[&Finding]) -> String {
    let fps: Vec<&str> = findings.iter().map(|f| f.fingerprint.as_str()).collect();
    serde_json::to_string(&fps).unwrap_or_default()
}

fn fingerprints_from_json(meta: &str) -> Vec<String> {
    serde_json::from_str(meta).unwrap_or_default()
}

fn truncate_body(text: &str) -> String {
    if text.len() <= BODY_MAX_BYTES && text.lines().count() <= BODY_MAX_LINES {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines().take(BODY_MAX_LINES) {
        if out.len() + line.len() > BODY_MAX_BYTES {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("(… truncated)");
    out
}

/// Restored files come back owner-only: dotfiles default private.
/// `mode` in `OpenOptions` applies only when the file is created, so a
/// forced overwrite of an existing file sets permissions explicitly.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

fn is_noise(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        || name.starts_with(".#")
        || name == ".DS_Store"
        || path.components().any(|c| c.as_os_str() == ".git")
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

/// The explicit policy for operations the daemon survives without:
/// log line, keep governing. Silence would hide a sick database; a
/// panic would kill the governor over a log entry.
fn soft<T, E: std::fmt::Display>(result: Result<T, E>) {
    if let Err(e) = result {
        crate::logging::emit(format_args!("error: {e}"));
    }
}

/// Degraded inbox outcome for an unreadable inbox: no notification,
/// but the failure is on the record.
#[allow(clippy::needless_pass_by_value)] // shape fixed by unwrap_or_else
fn refreshed(e: wukong_core::db::DbError) -> InboxOutcome {
    crate::logging::emit(format_args!("error: {e}"));
    InboxOutcome::Refreshed
}

fn err(e: impl std::fmt::Display) -> Response {
    Response::Error {
        message: e.to_string(),
    }
}
