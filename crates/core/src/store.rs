//! The mirror store: a plain git repository at
//! `~/.local/share/wukong/store` holding a copy of every tracked file
//! under its `$HOME`-relative path, committed by wukong on the
//! machine's own branch. The live files are the source of truth; the
//! store is the durable, historied, pushable shadow. All git work
//! shells out — the store stays a completely ordinary repo you can
//! open with any tool.

use crate::paths;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },
}

/// Strip credentials embedded in URLs (`https://user:token@host`) from
/// text that ends up in error messages, event rows, or logs.
fn redact_userinfo(text: &str) -> String {
    static USERINFO: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"://[^/@\s]+@").expect("compiles"));
    USERINFO.replace_all(text, "://…@").into_owned()
}

/// An orphaned diff scratch older than this is certainly not in use.
#[allow(clippy::duration_suboptimal_units)] // Duration::from_hours is unstable
const SCRATCH_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// A push that neither finishes nor fails within this window is
/// killed so the push slot frees up.
#[allow(clippy::duration_suboptimal_units)] // Duration::from_mins is unstable
const PUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Distinguishes concurrent diff scratch files; pid alone is not
/// unique enough (test threads, a future multi-engine world).
static SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    branch: String,
}

impl Store {
    /// Open (initializing if needed) the store on this machine's
    /// branch. Identity is repo-local so commits read as wukong's, and
    /// is (re)applied on every open so cloned stores get it too.
    pub fn open(dir: &Path, machine: &str) -> Result<Self, StoreError> {
        let store = Self {
            dir: dir.to_path_buf(),
            branch: machine.to_string(),
        };
        if !dir.join(".git").exists() {
            paths::ensure_private_dir(dir).map_err(|source| StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            store.git(&["init", "-b", machine])?;
        }
        store.git(&["config", "user.name", "wukong"])?;
        store.git(&["config", "user.email", &format!("wukong@{machine}")])?;
        // The store never wants an editor, a pager, or signing — and
        // never auto-gc, which could repack under a concurrent push.
        store.git(&["config", "commit.gpgsign", "false"])?;
        store.git(&["config", "gc.auto", "0"])?;
        // Sweep diff scratch files an earlier crash left behind: they
        // hold raw live content. Only STALE ones — a live diff (any
        // process; tests run many) finishes in milliseconds, so an
        // hour-old scratch file is certainly orphaned.
        if let Ok(entries) = std::fs::read_dir(paths::state_dir()) {
            for entry in entries.flatten() {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > SCRATCH_MAX_AGE));
                if stale
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("diff-scratch-")
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(store)
    }

    /// Bootstrap a store by cloning an existing remote, then switch to
    /// this machine's branch (creating it from the clone's HEAD if the
    /// machine is new — its history starts where another machine's
    /// left off, which is exactly what `restore` wants).
    pub fn clone_from(remote: &str, dir: &Path, machine: &str) -> Result<Self, StoreError> {
        if let Some(parent) = dir.parent() {
            paths::ensure_private_dir(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let out = Command::new("git")
            .args(["clone", "--quiet", remote])
            .arg(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|source| StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        if !out.status.success() {
            return Err(StoreError::Git {
                args: redact_userinfo(&format!("clone {remote}")),
                stderr: redact_userinfo(String::from_utf8_lossy(&out.stderr).trim()),
            });
        }
        let store = Self::open(dir, machine)?;
        // A machine re-bootstrapping onto its OWN existing branch must
        // resume that branch, not reset it to whatever the clone's
        // default HEAD was (which would restore another machine's
        // files and make every future push non-fast-forward).
        let own_branch = format!("refs/remotes/origin/{machine}");
        if store
            .git(&["show-ref", "--verify", "--quiet", &own_branch])
            .is_ok()
        {
            let upstream = format!("origin/{machine}");
            store.git(&["checkout", "-q", "-B", machine, &upstream])?;
        } else {
            store.git(&["checkout", "-q", "-B", machine])?;
        }
        Ok(store)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    fn git(&self, args: &[&str]) -> Result<String, StoreError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            // The store must be immune to the user's global git
            // machinery: no hooks firing on wukong's commits, no
            // global excludes silently swallowing a dotfile.
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.excludesFile=/dev/null",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|source| StoreError::Io {
                path: self.dir.clone(),
                source,
            })?;
        if !out.status.success() {
            return Err(StoreError::Git {
                args: redact_userinfo(&args.join(" ")),
                stderr: redact_userinfo(String::from_utf8_lossy(&out.stderr).trim()),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Copy a live file into the mirror. Returns the stored relative
    /// path. Content arrives as bytes the caller already read (and
    /// already passed through the gate).
    pub fn mirror_in(&self, live: &Path, content: &[u8]) -> Result<PathBuf, StoreError> {
        let rel = paths::store_rel(live);
        let target = self.dir.join(&rel);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&target, content).map_err(|source| StoreError::Io {
            path: target.clone(),
            source,
        })?;
        Ok(rel)
    }

    pub fn remove(&self, live: &Path) -> Result<(), StoreError> {
        let target = self.dir.join(paths::store_rel(live));
        match std::fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                path: target,
                source,
            }),
        }
    }

    /// Stage one path and commit if it actually changed. Staging is
    /// scoped so an unrelated half-mirrored file can never ride along
    /// under this commit's message.
    pub fn commit(&self, rel: &Path, message: &str) -> Result<Option<String>, StoreError> {
        // :(literal) keeps glob metacharacters in file names from
        // widening the pathspec — a scoped commit must stay scoped.
        let spec = format!(":(literal){}", rel.to_string_lossy());
        self.git(&["add", "-A", "--", &spec])?;
        if self
            .git(&["status", "--porcelain", "--", &spec])?
            .is_empty()
        {
            return Ok(None);
        }
        self.git(&["commit", "-q", "-m", message, "--", &spec])?;
        Ok(Some(self.git(&["rev-parse", "--short", "HEAD"])?))
    }

    /// The diff between the stored copy and live content, for inbox
    /// display. The scratch file lives in wukong's own 0700 state dir,
    /// never in the world-readable system temp dir — live content can
    /// hold secrets.
    pub fn diff_against_live(&self, live: &Path, live_content: &str) -> String {
        let stored = self.dir.join(paths::store_rel(live));
        let state = paths::state_dir();
        if paths::ensure_private_dir(&state).is_err() {
            return String::new();
        }
        let n = SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = state.join(format!("diff-scratch-{}-{n}", std::process::id()));
        if std::fs::write(&tmp, live_content).is_err() {
            return String::new();
        }
        let out = Command::new("git")
            .args(["diff", "--no-index", "--no-color", "--unified=3"])
            .arg(if stored.exists() {
                stored.as_path()
            } else {
                Path::new("/dev/null")
            })
            .arg(&tmp)
            .output();
        let _ = std::fs::remove_file(&tmp);
        match out {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                // Drop the noisy header lines; keep hunks.
                text.lines()
                    .skip_while(|l| !l.starts_with("@@"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Err(_) => String::new(),
        }
    }

    /// Commits not yet on the remote branch; 0 when up to date or no
    /// upstream yet (the first push carries everything).
    #[must_use]
    pub fn unpushed(&self, remote_configured: bool) -> usize {
        if !remote_configured {
            return 0;
        }
        self.git(&[
            "rev-list",
            "--count",
            &format!("origin/{0}..{0}", self.branch),
        ])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            // No remote-tracking ref yet: everything is unpushed.
            self.git(&["rev-list", "--count", "HEAD"])
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
    }

    /// Every file in the mirror, as store-relative paths.
    pub fn files(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(self
            .git(&["ls-files"])?
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect())
    }

    pub fn ensure_remote(&self, url: &str) -> Result<(), StoreError> {
        match self.git(&["remote", "get-url", "origin"]) {
            Ok(current) if current == url => Ok(()),
            Ok(_) => self.git(&["remote", "set-url", "origin", url]).map(drop),
            Err(_) => self.git(&["remote", "add", "origin", url]).map(drop),
        }
    }

    /// Push with a hard wall-clock timeout. A remote that accepts the
    /// connection and then stalls must not park the push slot forever —
    /// `push_in_flight` only clears when this returns.
    pub fn push(&self) -> Result<(), StoreError> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["push", "-q", "-u", "origin", &self.branch])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "GIT_SSH_COMMAND",
                "ssh -o BatchMode=yes -o ConnectTimeout=30",
            )
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|source| StoreError::Io {
                path: self.dir.clone(),
                source,
            })?;
        let deadline = std::time::Instant::now() + PUSH_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read as _;
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    if status.success() {
                        return Ok(());
                    }
                    return Err(StoreError::Git {
                        args: "push".to_string(),
                        stderr: redact_userinfo(stderr.trim()),
                    });
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(StoreError::Git {
                            args: "push".to_string(),
                            stderr: "timed out after 120s".to_string(),
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Err(source) => {
                    return Err(StoreError::Io {
                        path: self.dir.clone(),
                        source,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_commit_cycle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(&tmp.path().join("store"), "testbox").unwrap();

        // Nothing to commit on an empty store.
        assert!(store.commit(Path::new("."), "empty").unwrap().is_none());

        let live = paths::home().join(".wukong-test-zshrc");
        let rel = store.mirror_in(&live, b"export A=1\n").unwrap();
        let sha = store.commit(&rel, "zshrc: initial").unwrap();
        assert!(sha.is_some());

        // Identical content → no new commit.
        store.mirror_in(&live, b"export A=1\n").unwrap();
        assert!(store.commit(&rel, "zshrc: same").unwrap().is_none());

        // Changed content → commits again.
        store.mirror_in(&live, b"export A=2\n").unwrap();
        assert!(store.commit(&rel, "zshrc: changed").unwrap().is_some());

        // A stray file in the store never rides along on a scoped commit.
        std::fs::write(store.dir().join("stray-file"), "oops").unwrap();
        store.mirror_in(&live, b"export A=2b\n").unwrap();
        store.commit(&rel, "zshrc: scoped").unwrap();
        let shown = store
            .git(&["show", "--stat", "--name-only", "HEAD"])
            .unwrap();
        assert!(!shown.contains("stray-file"), "{shown}");
        std::fs::remove_file(store.dir().join("stray-file")).unwrap();

        // Diff shows the change against newer live content.
        let diff = store.diff_against_live(&live, "export A=3\n");
        assert!(diff.contains("-export A=2b"), "{diff}");
        assert!(diff.contains("+export A=3"), "{diff}");

        store.remove(&live).unwrap();
        assert!(store.commit(&rel, "zshrc: removed").unwrap().is_some());
    }
}
