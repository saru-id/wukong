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

pub struct Store {
    dir: PathBuf,
    branch: String,
}

impl Store {
    /// Open (initializing if needed) the store on this machine's
    /// branch. Identity is repo-local so commits read as wukong's.
    pub fn open(dir: &Path, machine: &str) -> Result<Self, StoreError> {
        let store = Self {
            dir: dir.to_path_buf(),
            branch: machine.to_string(),
        };
        if !dir.join(".git").exists() {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            store.git(&["init", "-b", machine])?;
            store.git(&["config", "user.name", "wukong"])?;
            store.git(&["config", "user.email", &format!("wukong@{machine}")])?;
            // The store never wants an editor, a pager, or signing.
            store.git(&["config", "commit.gpgsign", "false"])?;
        }
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    fn git(&self, args: &[&str]) -> Result<String, StoreError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|source| StoreError::Io {
                path: self.dir.clone(),
                source,
            })?;
        if !out.status.success() {
            return Err(StoreError::Git {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
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

    /// Stage everything and commit if anything actually changed.
    pub fn commit(&self, message: &str) -> Result<Option<String>, StoreError> {
        self.git(&["add", "-A"])?;
        if self.git(&["status", "--porcelain"])?.is_empty() {
            return Ok(None);
        }
        self.git(&["commit", "-q", "-m", message])?;
        Ok(Some(self.git(&["rev-parse", "--short", "HEAD"])?))
    }

    /// The diff between the stored copy and live content, for inbox
    /// display. Uses git's word-level machinery via a temp blob-less
    /// `--no-index` diff.
    pub fn diff_against_live(&self, live: &Path, live_content: &str) -> String {
        let stored = self.dir.join(paths::store_rel(live));
        let tmp = std::env::temp_dir().join(format!("wukong-diff-{}", std::process::id()));
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

    pub fn ensure_remote(&self, url: &str) -> Result<(), StoreError> {
        match self.git(&["remote", "get-url", "origin"]) {
            Ok(current) if current == url => Ok(()),
            Ok(_) => self.git(&["remote", "set-url", "origin", url]).map(drop),
            Err(_) => self.git(&["remote", "add", "origin", url]).map(drop),
        }
    }

    pub fn push(&self) -> Result<(), StoreError> {
        self.git(&["push", "-q", "-u", "origin", &self.branch])
            .map(drop)
    }

    /// Restore one stored file to its live location (bootstrap flow).
    pub fn restore(&self, rel: &Path) -> Result<PathBuf, StoreError> {
        let source = self.dir.join(rel);
        let live = paths::from_store_rel(rel);
        if let Some(dir) = live.parent() {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        std::fs::copy(&source, &live).map_err(|source_err| StoreError::Io {
            path: source.clone(),
            source: source_err,
        })?;
        Ok(live)
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
        assert!(store.commit("empty").unwrap().is_none());

        let live = paths::home().join(".wukong-test-zshrc");
        store.mirror_in(&live, b"export A=1\n").unwrap();
        let sha = store.commit("zshrc: initial").unwrap();
        assert!(sha.is_some());

        // Identical content → no new commit.
        store.mirror_in(&live, b"export A=1\n").unwrap();
        assert!(store.commit("zshrc: same").unwrap().is_none());

        // Changed content → commits again.
        store.mirror_in(&live, b"export A=2\n").unwrap();
        assert!(store.commit("zshrc: changed").unwrap().is_some());

        // Diff shows the change against newer live content.
        let diff = store.diff_against_live(&live, "export A=3\n");
        assert!(diff.contains("-export A=2"), "{diff}");
        assert!(diff.contains("+export A=3"), "{diff}");

        store.remove(&live).unwrap();
        assert!(store.commit("zshrc: removed").unwrap().is_some());
    }
}
