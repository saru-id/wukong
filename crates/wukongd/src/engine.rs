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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wukong_core::db::InboxOutcome;
use wukong_core::events::{EventKind, InboxKind, Resolution};
use wukong_core::gate::{self, Finding, GateVerdict};
use wukong_core::ipc::{Request, Response, StatusInfo, TrackedFile};
use wukong_core::{Config, Db, Store, paths};

/// Inbox bodies are evidence, not archives.
const BODY_MAX_LINES: usize = 300;
const BODY_MAX_BYTES: usize = 16 * 1024;

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
    /// Directories the loop should start watching — filled when a new
    /// file is tracked, drained by main after each request.
    watch_requests: Vec<PathBuf>,
    /// Canonical live paths of tracked files: the hot-path roster.
    tracked_live: HashSet<PathBuf>,
    sentinel_files: Vec<PathBuf>,
    sentinel_dirs: Vec<PathBuf>,
    excludes: Vec<PathBuf>,
}

impl Engine {
    /// Paths are injected so tests can run an engine against a tempdir.
    pub fn new(config: Config, db_path: &Path, store_dir: &Path) -> anyhow::Result<Self> {
        let db = Db::open(db_path)?;
        let store = Store::open(store_dir, &config.machine)?;
        if !config.remote.is_empty() {
            store.ensure_remote(&config.remote)?;
        }
        db.record(EventKind::DAEMON_STARTED, &config.machine, "")?;
        let tracked_live = db
            .tracked()?
            .iter()
            .map(|rel| paths::from_store_rel(Path::new(rel)))
            .collect();
        let (mut sentinel_files, mut sentinel_dirs) = (Vec::new(), Vec::new());
        for s in config.sentinel_paths() {
            if s.is_dir() {
                sentinel_dirs.push(s);
            } else {
                sentinel_files.push(s);
            }
        }
        let excludes = config.exclude_paths();
        Ok(Self {
            config,
            db,
            store,
            started: Instant::now(),
            pending: HashMap::new(),
            last_commit: None,
            last_push: None,
            dirty: false,
            push_in_flight: false,
            watch_requests: Vec::new(),
            tracked_live,
            sentinel_files,
            sentinel_dirs,
            excludes,
        })
    }

    /// What to watch at startup, with an explicit recursion mode per
    /// root. File sentinels and tracked files are covered by watching
    /// their parent directory NON-recursively — that survives editors'
    /// atomic renames and files that don't exist yet, and it never
    /// escalates to watching all of `$HOME` recursively the way the
    /// old parent-fallback did. Only true directory sentinels
    /// (~/.config, ~/Library/LaunchAgents) watch recursively.
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
        let mut out: Vec<(PathBuf, bool)> = roots.into_iter().collect();
        out.sort();
        out
    }

    /// New watch roots requested since the last drain (non-recursive).
    pub fn drain_watch_requests(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.watch_requests)
    }

    fn request_watch(&mut self, dir: &Path) {
        if !self.watch_requests.iter().any(|d| d == dir) {
            self.watch_requests.push(dir.to_path_buf());
        }
    }

    /// Every touched path the watcher reports. Cheap: in-memory set
    /// lookups, then remember the path and its time; the real work
    /// waits for the debounce to settle in `tick`.
    pub fn touch(&mut self, path: PathBuf) {
        if path.starts_with(self.store.dir()) || is_noise(&path) {
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
            .iter()
            .filter(|(_, t)| t.elapsed() >= debounce)
            .map(|(p, _)| p.clone())
            .collect();
        let mut new_inbox = 0;
        for path in ready {
            self.pending.remove(&path);
            new_inbox += self.settle(&path);
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

    /// The gated commit flow for a tracked file:
    ///
    /// 1. scan — every finding, every line
    /// 2. findings without a stored allowance → quarantine, no commit
    /// 3. allowed findings → approved ones stay, redacted ones are
    ///    masked in the stored copy (the live file is never touched)
    /// 4. paranoia: the to-be-stored content is re-scanned; anything
    ///    unexpected holds the commit rather than trusting the mask
    fn commit_tracked(&mut self, path: &Path, rel: &str) -> usize {
        let Ok(bytes) = std::fs::read(path) else {
            // Deleted: drop it from the mirror and commit the removal.
            let _ = self.store.remove(path);
            let rel_path = paths::store_rel(path);
            if let Ok(Some(sha)) = self.store.commit(&rel_path, &format!("{rel}: removed")) {
                self.after_commit(rel, &sha, "removed");
            }
            return 0;
        };
        let content = String::from_utf8_lossy(&bytes);

        let findings = match gate::scan(path, &content) {
            GateVerdict::Clean => Vec::new(),
            GateVerdict::Quarantine(f) => f,
            GateVerdict::Forbidden(why) => {
                // Reachable only for files tracked before the name
                // became forbidden. Loud, not silent.
                let _ = self.db.record(EventKind::HELD, rel, why);
                return 0;
            }
        };
        let allowances = self.db.allowances_for(rel).unwrap_or_default();
        let new: Vec<&Finding> = findings
            .iter()
            .filter(|f| !allowances.contains_key(&f.fingerprint))
            .collect();
        if !new.is_empty() {
            return self.quarantine(path, rel, &content, &new);
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
        let stored: Vec<u8> = if needs_mask {
            let masked = gate::mask_findings(&content, &findings, |f| {
                !must_redact.contains(f.fingerprint.as_str())
            });
            // Trust nothing: the stored copy must scan clean apart
            // from deliberately approved fingerprints.
            if let GateVerdict::Quarantine(left) = gate::scan(path, &masked) {
                let unexpected = left
                    .iter()
                    .any(|f| allowances.get(&f.fingerprint).map(String::as_str) != Some("approve"));
                if unexpected {
                    let _ = self
                        .db
                        .record(EventKind::HELD, rel, "redaction verification failed");
                    return self.quarantine(path, rel, &content, &left.iter().collect::<Vec<_>>());
                }
            }
            masked.into_bytes()
        } else {
            bytes
        };

        // Summary must be computed against the OLD stored copy, so it
        // runs before mirror_in overwrites it.
        let stored_text = String::from_utf8_lossy(&stored).into_owned();
        let summary = change_summary(&self.store, path, &stored_text);
        let Ok(rel_path) = self.store.mirror_in(path, &stored) else {
            return 0;
        };
        if let Ok(Some(sha)) = self.store.commit(&rel_path, &format!("{rel}: {summary}")) {
            self.after_commit(rel, &sha, &summary);
        }
        0
    }

    fn quarantine(&mut self, path: &Path, rel: &str, content: &str, new: &[&Finding]) -> usize {
        let diff = self.store.diff_against_live(path, content);
        let body = quarantine_body(new, &diff);
        let meta = fingerprint_json(new);
        let outcome = self
            .db
            .inbox_add(
                InboxKind::QUARANTINE,
                rel,
                &format!("{} secret finding(s) held", new.len()),
                &body,
                &meta,
            )
            .unwrap_or(InboxOutcome::Refreshed);
        let _ = self.db.record(
            EventKind::QUARANTINED,
            rel,
            &format!("{} finding(s)", new.len()),
        );
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
        let Ok(bytes) = std::fs::read(path) else {
            return 0;
        };
        let content = String::from_utf8_lossy(&bytes);
        if matches!(gate::scan(path, &content), GateVerdict::Forbidden(_)) {
            return 0;
        }
        let body = truncate_body(&gate::mask_all(
            &self.store.diff_against_live(path, &content),
        ));
        let outcome = self
            .db
            .inbox_add(
                InboxKind::SENTINEL,
                rel,
                "changed — start tracking it?",
                &body,
                "",
            )
            .unwrap_or(InboxOutcome::Refreshed);
        let _ = self.db.record(EventKind::SENTINEL, rel, "");
        usize::from(outcome == InboxOutcome::New)
    }

    fn after_commit(&mut self, rel: &str, sha: &str, summary: &str) {
        let _ = self.db.set_hash(rel, sha);
        let _ = self.db.record(EventKind::COMMITTED, rel, summary);
        self.last_commit = Some(sha.to_string());
        self.dirty = true;
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
                self.dirty = false;
                let _ = self.db.record(EventKind::PUSHED, self.store.branch(), "");
            }
            Err(err) => {
                let _ = self
                    .db
                    .record(EventKind::PUSH_FAILED, self.store.branch(), &err);
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
            Request::Track { path } => self.track(&path),
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
            Request::Restore { path, force } => self.restore(path.as_deref(), force),
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
            last_push: self.last_push.clone(),
            unpushed: self.store.unpushed(!self.config.remote.is_empty()),
            uptime_secs: self.started.elapsed().as_secs(),
        })
    }

    /// Track a live path: resolve it, refuse forbidden files, mirror
    /// and commit the current content immediately.
    fn track(&mut self, path: &str) -> Response {
        let live = resolve_path(path);
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
        let bytes = match std::fs::read(&live) {
            Ok(b) => b,
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        };
        let content = String::from_utf8_lossy(&bytes);
        if let GateVerdict::Forbidden(why) = gate::scan(&live, &content) {
            return Response::Error {
                message: format!("refused: {} ({why})", paths::display(&live)),
            };
        }
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        match self.db.track(&rel) {
            Ok(_) => {
                let _ = self.db.record(EventKind::TRACKED, &rel, "");
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
            self.request_watch(parent);
        }
    }

    fn untrack(&mut self, path: &str) -> Response {
        let live = resolve_path(path);
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        match self.db.untrack(&rel) {
            Ok(true) => {
                self.tracked_live.remove(&live);
                let _ = self.store.remove(&live);
                let rel_path = paths::store_rel(&live);
                if let Ok(Some(_)) = self.store.commit(&rel_path, &format!("{rel}: untracked")) {
                    self.dirty = true;
                }
                let _ = self.db.record(EventKind::UNTRACKED, &rel, "");
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
            Ok(paths) => Response::Tracked {
                files: paths
                    .into_iter()
                    .map(|rel| {
                        let live = paths::from_store_rel(Path::new(&rel));
                        TrackedFile {
                            display: paths::display(&live),
                            exists: live.exists(),
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

        // Guard before anything is resolved: approving a sentinel offer
        // for a forbidden-named file is refused outright (offers skip
        // them now, but items may predate that rule).
        if item.kind == InboxKind::SENTINEL
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
        let _ = self.db.inbox_resolve(id, resolution);
        let _ = self
            .db
            .record(EventKind::RESOLVED, &item.subject, resolution.as_str());

        // Sentinel + approve means "start tracking".
        if item.kind == InboxKind::SENTINEL && resolution == Resolution::Approve {
            let _ = self.db.track(&item.subject);
            let _ = self
                .db
                .record(EventKind::TRACKED, &item.subject, "from inbox");
            self.adopt(&live);
            self.commit_tracked(&live, &item.subject);
        }

        // Quarantine + approve/redact: persist the resolution per
        // fingerprint, then run the normal gated flow. If the file has
        // grown NEW secrets since the item was filed, the flow
        // quarantines them instead of blindly committing — approval
        // covers exactly the findings it was granted for.
        if item.kind == InboxKind::QUARANTINE
            && matches!(resolution, Resolution::Approve | Resolution::Redact)
        {
            for fp in fingerprints_from_json(&item.meta) {
                let _ = self.db.allow(&item.subject, &fp, resolution.as_str());
            }
            self.commit_tracked(&live, &item.subject);
        }

        Response::Ok {
            message: format!("resolved {} ({})", item.subject, resolution.as_str()),
        }
    }

    /// Copy stored files back to their live locations and track them —
    /// the new-machine bootstrap. Existing files that differ are
    /// skipped unless forced.
    fn restore(&mut self, path: Option<&str>, force: bool) -> Response {
        let rels: Vec<PathBuf> = match path {
            Some(p) => vec![paths::store_rel(&resolve_path(p))],
            None => match self.store.files() {
                Ok(files) => files,
                Err(e) => return err(e),
            },
        };
        if rels.is_empty() {
            return Response::Error {
                message: "the store has no files to restore".to_string(),
            };
        }
        let (mut restored, mut skipped) = (0usize, Vec::new());
        for rel in rels {
            let rel_str = rel.to_string_lossy().into_owned();
            let stored = match std::fs::read(self.store.dir().join(&rel)) {
                Ok(b) => b,
                Err(_) => {
                    skipped.push(format!("{rel_str} (not in store)"));
                    continue;
                }
            };
            let live = paths::from_store_rel(&rel);
            let differs = std::fs::read(&live).map(|b| b != stored).unwrap_or(false);
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
            if std::fs::write(&live, &stored).is_err() {
                skipped.push(format!("{} (write failed)", paths::display(&live)));
                continue;
            }
            let _ = self.db.track(&rel_str);
            self.adopt(&live);
            let _ = self.db.record(EventKind::RESTORED, &rel_str, "");
            restored += 1;
        }
        let mut message = format!("restored {restored} file(s)");
        if !skipped.is_empty() {
            message.push_str(&format!("\nskipped:\n  {}", skipped.join("\n  ")));
        }
        Response::Ok { message }
    }
}

/// Turn user input into a real, canonical path: expand `~/`, make it
/// absolute, resolve symlinks. Everything stored or compared against
/// watcher events must be canonical.
fn resolve_path(path: &str) -> PathBuf {
    let expanded = match path.strip_prefix("~/") {
        Some(rel) => paths::home().join(rel),
        None => {
            let p = PathBuf::from(path);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            }
        }
    };
    paths::canonicalize_lenient(&expanded)
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
    for f in findings {
        body.push_str(&format!("  line {}: {} — {}\n", f.line, f.rule, f.excerpt));
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
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

fn err(e: impl std::fmt::Display) -> Response {
    Response::Error {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine wired to a tempdir, with a zero debounce so `tick`
    /// settles immediately. `_guard` keeps the tempdir alive.
    struct Rig {
        engine: Engine,
        home: PathBuf,
        _guard: tempfile::TempDir,
    }

    fn rig() -> Rig {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let config = Config {
            machine: "testbox".to_string(),
            debounce_secs: 0,
            ..Config::default()
        };
        let engine = Engine::new(config, &root.join("wukong.db"), &root.join("store")).unwrap();
        Rig {
            engine,
            home,
            _guard: tmp,
        }
    }

    fn track(rig: &mut Rig, name: &str, content: &str) -> PathBuf {
        let file = rig.home.join(name);
        std::fs::write(&file, content).unwrap();
        let resp = rig.engine.track(file.to_str().unwrap());
        assert!(matches!(resp, Response::Ok { .. }), "{resp:?}");
        file
    }

    fn edit_and_settle(rig: &mut Rig, file: &Path, content: &str) -> usize {
        std::fs::write(file, content).unwrap();
        rig.engine.touch(file.to_path_buf());
        rig.engine.tick()
    }

    fn store_content(rig: &Rig, file: &Path) -> Option<String> {
        std::fs::read_to_string(rig.engine.store.dir().join(paths::store_rel(file))).ok()
    }

    const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz012345";

    #[test]
    fn clean_edit_commits_with_real_summary() {
        let mut rig = rig();
        let file = track(&mut rig, ".zshrc", "export A=1\n");
        edit_and_settle(&mut rig, &file, "export A=1\nexport B=2\n");
        assert_eq!(
            store_content(&rig, &file).as_deref(),
            Some("export A=1\nexport B=2\n")
        );
        let events = rig.engine.db.events(10).unwrap();
        let commit = events
            .iter()
            .find(|e| e.kind == EventKind::COMMITTED && e.detail != "updated")
            .expect("commit with a real summary");
        assert_eq!(commit.detail, "+1 lines");
    }

    #[test]
    fn secret_edit_quarantines_with_masked_body() {
        let mut rig = rig();
        let file = track(&mut rig, ".zshrc", "export A=1\n");
        let new_items =
            edit_and_settle(&mut rig, &file, &format!("export A=1\nexport T={SECRET}\n"));
        assert_eq!(new_items, 1);
        // Store still has the old content — the secret never landed.
        assert_eq!(store_content(&rig, &file).as_deref(), Some("export A=1\n"));
        // And the inbox evidence is masked.
        let item = &rig.engine.db.inbox_open().unwrap()[0];
        assert!(!item.body.contains(SECRET), "body leaks: {}", item.body);
        assert!(!item.meta.is_empty());
    }

    #[test]
    fn approve_persists_no_requarantine_on_next_edit() {
        let mut rig = rig();
        let file = track(&mut rig, ".zshrc", "export A=1\n");
        edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\n"));
        let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
        rig.engine.resolve(item_id, Resolution::Approve);
        // Approved: committed as-is.
        assert_eq!(
            store_content(&rig, &file).as_deref(),
            Some(&*format!("export T={SECRET}\n"))
        );
        // The same token in a later edit does NOT re-quarantine.
        let new_items =
            edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\nexport B=2\n"));
        assert_eq!(new_items, 0);
        assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
        assert!(store_content(&rig, &file).unwrap().contains("export B=2"));
    }

    #[test]
    fn redact_masks_store_leaves_live_alone() {
        let mut rig = rig();
        let file = track(&mut rig, ".zshrc", "export A=1\n");
        let live_content = format!("export T={SECRET}\n");
        edit_and_settle(&mut rig, &file, &live_content);
        let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
        rig.engine.resolve(item_id, Resolution::Redact);
        let stored = store_content(&rig, &file).unwrap();
        assert!(!stored.contains(SECRET), "store leaks: {stored}");
        assert!(stored.contains("ghp_……45"), "{stored}");
        // Live file untouched.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), live_content);
        // And the redaction is sticky across future edits.
        let new_items =
            edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\nexport C=3\n"));
        assert_eq!(new_items, 0);
        let stored = store_content(&rig, &file).unwrap();
        assert!(!stored.contains(SECRET));
        assert!(stored.contains("export C=3"));
    }

    #[test]
    fn approve_does_not_cover_new_secrets() {
        let mut rig = rig();
        let file = track(&mut rig, ".zshrc", "export A=1\n");
        edit_and_settle(&mut rig, &file, &format!("export T={SECRET}\n"));
        let item_id = rig.engine.db.inbox_open().unwrap()[0].id;
        // A second, different secret sneaks in before the approval.
        let rotated = "ghp_zyxwvutsrqponmlkjihgfedcba543210";
        std::fs::write(&file, format!("export T={SECRET}\nexport U={rotated}\n")).unwrap();
        rig.engine.resolve(item_id, Resolution::Approve);
        // The new secret must be quarantined, not committed.
        let stored = store_content(&rig, &file).unwrap_or_default();
        assert!(!stored.contains(rotated), "new secret leaked: {stored}");
        assert_eq!(rig.engine.db.inbox_count().unwrap(), 1);
    }

    #[test]
    fn forbidden_sentinel_changes_are_not_offered() {
        let mut rig = rig();
        let creds = rig.home.join("credentials.json");
        std::fs::write(&creds, "{\"token\": \"whatever\"}").unwrap();
        // Simulate a sentinel-routed settle for an untracked file.
        let rel = paths::store_rel(&creds).to_string_lossy().into_owned();
        let offered = rig.engine.offer_sentinel(&creds, &rel);
        assert_eq!(offered, 0);
        assert_eq!(rig.engine.db.inbox_count().unwrap(), 0);
    }

    #[test]
    fn track_refuses_forbidden() {
        let mut rig = rig();
        let env = rig.home.join(".env");
        std::fs::write(&env, "SECRET=x").unwrap();
        let resp = rig.engine.track(env.to_str().unwrap());
        assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
    }

    #[test]
    fn restore_round_trips_and_tracks() {
        let mut rig = rig();
        let file = track(&mut rig, ".gitconfig", "[user]\n\tname = s\n");
        std::fs::remove_file(&file).unwrap();
        let resp = rig.engine.restore(None, false);
        let Response::Ok { message } = resp else {
            panic!("restore failed");
        };
        assert!(message.contains("restored 1 file(s)"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[user]\n\tname = s\n"
        );
        assert!(rig.engine.tracked_live.contains(&file));
    }
}
