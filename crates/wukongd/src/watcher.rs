//! The filesystem watcher: `notify` (FSEvents on macOS) feeds raw
//! change paths into an async channel. It is idle-cheap — the kernel
//! wakes us only when something under a watched root actually moves.
//! Debouncing and gating happen downstream in the engine; this module
//! only turns OS events into a clean stream of touched paths.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub struct FsWatcher {
    watcher: RecommendedWatcher,
}

impl FsWatcher {
    /// Start watching; touched paths arrive on the returned receiver.
    pub fn start() -> anyhow::Result<(Self, mpsc::UnboundedReceiver<PathBuf>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        })?;
        Ok((Self { watcher }, rx))
    }

    /// Watch a path. Files are watched non-recursively; directories
    /// (the sentinel dirs, ~/.config) recursively. Missing paths are
    /// skipped quietly — a sentinel that doesn't exist yet is fine.
    pub fn watch(&mut self, path: &Path) {
        if !path.exists() {
            return;
        }
        let mode = if path.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let _ = self.watcher.watch(path, mode);
    }
}
