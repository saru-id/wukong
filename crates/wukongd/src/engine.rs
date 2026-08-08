//! The governor's brain. Owns the database, the store, and the config;
//! turns raw file events into debounced, gated, committed history; and
//! answers the IPC requests the clients send. Everything that mutates
//! state funnels through here so there is one writer and no locks to
//! reason about beyond the engine's own `&mut self`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wukong_core::db::InboxOutcome;
use wukong_core::events::{EventKind, InboxKind, Resolution};
use wukong_core::gate::{self, GateVerdict};
use wukong_core::ipc::{Request, Response, StatusInfo, TrackedFile};
use wukong_core::{Config, Db, Store, paths};

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
    /// Directories the loop should start watching — filled when a new
    /// file is tracked, drained by main after each request.
    watch_requests: Vec<PathBuf>,
}

impl Engine {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let db = Db::open(&paths::db_file())?;
        let store = Store::open(&paths::store_dir(), &config.machine)?;
        if !config.remote.is_empty() {
            store.ensure_remote(&config.remote)?;
        }
        db.record(EventKind::DAEMON_STARTED, &config.machine, "")?;
        Ok(Self {
            config,
            db,
            store,
            started: Instant::now(),
            pending: HashMap::new(),
            last_commit: None,
            last_push: None,
            dirty: false,
            watch_requests: Vec::new(),
        })
    }

    /// Directories to watch at startup: each sentinel (or, when it does
    /// not exist yet, its parent so creation is seen) and the parent of
    /// every already-tracked file (so a re-launched daemon keeps
    /// watching them). Returned as a de-duplicated set.
    pub fn initial_watch_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for sentinel in self.config.sentinel_paths() {
            if sentinel.exists() {
                roots.push(sentinel);
            } else if let Some(parent) = sentinel.parent() {
                roots.push(parent.to_path_buf());
            }
        }
        for rel in self.db.tracked().unwrap_or_default() {
            if let Some(parent) = paths::from_store_rel(Path::new(&rel)).parent() {
                roots.push(parent.to_path_buf());
            }
        }
        dedup(roots)
    }

    /// New watch roots requested since the last drain.
    pub fn drain_watch_requests(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.watch_requests)
    }

    fn request_watch(&mut self, dir: &Path) {
        if !self.watch_requests.contains(&dir.to_path_buf()) {
            self.watch_requests.push(dir.to_path_buf());
        }
    }

    /// Every touched path the watcher reports. Cheap: just remembers
    /// the path and its time; the real work waits for the debounce to
    /// settle in `tick`.
    pub fn touch(&mut self, path: PathBuf) {
        // Ignore churn inside the store itself and obvious noise.
        if path.starts_with(self.store.dir()) || is_noise(&path) {
            return;
        }
        if self.is_relevant(&path) {
            self.pending.insert(path, Instant::now());
        }
    }

    /// A touched path matters if it is tracked, or lives under a
    /// sentinel root (so we can offer to track it).
    fn is_relevant(&self, path: &Path) -> bool {
        let rel = paths::store_rel(path).to_string_lossy().into_owned();
        if self.db.is_tracked(&rel).unwrap_or(false) {
            return true;
        }
        self.config
            .sentinel_paths()
            .iter()
            .any(|s| path == s || path.starts_with(s))
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
        let tracked = self.db.is_tracked(&rel).unwrap_or(false);

        if tracked {
            self.commit_tracked(path, &rel)
        } else {
            self.offer_sentinel(path, &rel)
        }
    }

    fn commit_tracked(&mut self, path: &Path, rel: &str) -> usize {
        let Ok(bytes) = std::fs::read(path) else {
            // Deleted: drop it from the mirror and commit the removal.
            let _ = self.store.remove(path);
            if let Ok(Some(sha)) = self.store.commit(&format!("{rel}: removed")) {
                self.after_commit(rel, &sha, "removed");
            }
            return 0;
        };
        let content = String::from_utf8_lossy(&bytes);

        match gate::scan(path, &content) {
            GateVerdict::Clean => {
                let _ = self.store.mirror_in(path, &bytes);
                let summary = change_summary(&self.store, path, &content);
                if let Ok(Some(sha)) = self.store.commit(&format!("{rel}: {summary}")) {
                    self.after_commit(rel, &sha, &summary);
                }
                0
            }
            GateVerdict::Quarantine(findings) => {
                let body =
                    quarantine_body(&findings, &self.store.diff_against_live(path, &content));
                let outcome = self
                    .db
                    .inbox_add(
                        InboxKind::QUARANTINE,
                        rel,
                        &format!("{} secret finding(s) held", findings.len()),
                        &body,
                    )
                    .unwrap_or(InboxOutcome::Refreshed);
                let _ = self.db.record(
                    EventKind::QUARANTINED,
                    rel,
                    &format!("{} finding(s)", findings.len()),
                );
                usize::from(outcome == InboxOutcome::New)
            }
            GateVerdict::Forbidden(_) => 0, // never should have been tracked
        }
    }

    fn offer_sentinel(&mut self, path: &Path, rel: &str) -> usize {
        // Only files, and only ones that still exist.
        if !path.is_file() {
            return 0;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return 0;
        };
        let content = String::from_utf8_lossy(&bytes);
        let body = self.store.diff_against_live(path, &content);
        let outcome = self
            .db
            .inbox_add(
                InboxKind::SENTINEL,
                rel,
                "changed — start tracking it?",
                &body,
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

    /// Push if there is anything to push and a remote is configured.
    /// Called on the push timer; cheap when clean.
    pub fn maybe_push(&mut self) {
        if self.config.remote.is_empty() || !self.dirty {
            return;
        }
        match self.store.push() {
            Ok(()) => {
                self.last_push = Some(now());
                self.dirty = false;
                let _ = self.db.record(EventKind::PUSHED, self.store.branch(), "");
            }
            Err(err) => {
                let _ = self
                    .db
                    .record(EventKind::PUSHED, "failed", &err.to_string());
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
            Request::PushNow => {
                self.dirty = true;
                self.maybe_push();
                Response::Ok {
                    message: "pushed".to_string(),
                }
            }
        }
    }

    fn status(&self) -> Response {
        Response::Status(StatusInfo {
            machine: self.config.machine.clone(),
            remote: self.config.remote.clone(),
            tracked: self.db.tracked().map(|t| t.len()).unwrap_or(0),
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
        let live = resolve(path);
        if !live.is_file() {
            return Response::Error {
                message: format!("{} is not a file", paths::display(&live)),
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
                // Watch its directory so future edits are seen live.
                if let Some(parent) = live.parent() {
                    self.request_watch(parent);
                }
                // Commit whatever is clean now; a quarantine lands in the
                // inbox but the file stays tracked.
                self.commit_tracked(&live, &rel);
                Response::Ok {
                    message: format!("tracking {}", paths::display(&live)),
                }
            }
            Err(e) => err(e),
        }
    }

    fn untrack(&mut self, path: &str) -> Response {
        let live = resolve(path);
        let rel = paths::store_rel(&live).to_string_lossy().into_owned();
        match self.db.untrack(&rel) {
            Ok(true) => {
                let _ = self.store.remove(&live);
                if let Ok(Some(_)) = self.store.commit(&format!("{rel}: untracked")) {
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
        // Sentinel + approve means "start tracking".
        if item.kind == InboxKind::SENTINEL && resolution == Resolution::Approve {
            let live = paths::from_store_rel(Path::new(&item.subject));
            let _ = self.db.track(&item.subject);
            let _ = self
                .db
                .record(EventKind::TRACKED, &item.subject, "from inbox");
            if let Some(parent) = live.parent() {
                self.request_watch(parent);
            }
            self.commit_tracked(&live, &item.subject);
        }
        // Quarantine + approve/redact commits the held change.
        if item.kind == InboxKind::QUARANTINE
            && matches!(resolution, Resolution::Approve | Resolution::Redact)
        {
            let live = paths::from_store_rel(Path::new(&item.subject));
            if let Ok(bytes) = std::fs::read(&live) {
                let stored = if resolution == Resolution::Redact {
                    redact(&String::from_utf8_lossy(&bytes)).into_bytes()
                } else {
                    bytes
                };
                let _ = self.store.mirror_in(&live, &stored);
                if let Ok(Some(sha)) = self
                    .store
                    .commit(&format!("{}: approved from inbox", item.subject))
                {
                    self.after_commit(&item.subject, &sha, "approved");
                }
            }
        }
        let _ = self.db.inbox_resolve(id, resolution);
        let _ = self
            .db
            .record(EventKind::RESOLVED, &item.subject, resolution.as_str());
        Response::Ok {
            message: format!("resolved {} ({})", item.subject, resolution.as_str()),
        }
    }
}

fn dedup(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

/// Turn user input into a real, canonical path: expand `~/`, make it
/// absolute, then resolve symlinks on the parent dir (so a path handed
/// in through the `/var` → `/private/var` symlink matches the canonical
/// form the watcher reports). Rejoining the file name keeps it working
/// even when the file itself has just been deleted.
fn resolve(path: &str) -> PathBuf {
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
    match (expanded.parent(), expanded.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|canon| canon.join(name))
            .unwrap_or(expanded),
        _ => expanded,
    }
}

/// A one-line human summary of a change for the commit message.
fn change_summary(store: &Store, path: &Path, content: &str) -> String {
    let diff = store.diff_against_live(path, content);
    if diff.is_empty() {
        return "updated".to_string();
    }
    let added = diff.lines().filter(|l| l.starts_with('+')).count();
    let removed = diff.lines().filter(|l| l.starts_with('-')).count();
    match (added, removed) {
        (a, 0) => format!("+{a} lines"),
        (0, r) => format!("-{r} lines"),
        (a, r) => format!("+{a}/-{r} lines"),
    }
}

fn quarantine_body(findings: &[gate::Finding], diff: &str) -> String {
    let mut body = String::from("Held by the secret gate:\n");
    for f in findings {
        body.push_str(&format!("  line {}: {} — {}\n", f.line, f.rule, f.excerpt));
    }
    body.push('\n');
    body.push_str(diff);
    body
}

/// Mask any gate-recognized secret in stored content (the redact path).
fn redact(content: &str) -> String {
    content
        .lines()
        .map(|line| match gate::scan(Path::new("redact"), line) {
            GateVerdict::Quarantine(f) => f
                .first()
                .map(|finding| finding.excerpt.clone())
                .unwrap_or_else(|| line.to_string()),
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn is_noise(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    name.ends_with('~')
        || name.ends_with(".swp")
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
