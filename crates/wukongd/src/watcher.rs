//! The filesystem watcher: `notify` (`FSEvents` on macOS) feeds raw
//! change signals into an async channel. It is idle-cheap — the kernel
//! wakes us only when something under a watched root actually moves.
//! Debouncing and gating happen downstream in the engine; this module
//! only turns OS events into a clean stream of signals — including the
//! "I lost events, rescan" signal, which must never be dropped.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub enum WatchSignal {
    Touched(PathBuf),
    /// The OS event queue overflowed or the backend demands a rescan;
    /// the engine should treat every watched file as possibly changed.
    Rescan,
}

pub struct FsWatcher {
    watcher: RecommendedWatcher,
}

impl FsWatcher {
    /// Start watching; signals arrive on the returned receiver.
    pub fn start() -> anyhow::Result<(Self, mpsc::UnboundedReceiver<WatchSignal>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(event) => {
                    if event.need_rescan() {
                        let _ = tx.send(WatchSignal::Rescan);
                    }
                    for path in event.paths {
                        let _ = tx.send(WatchSignal::Touched(path));
                    }
                }
                Err(_) => {
                    let _ = tx.send(WatchSignal::Rescan);
                }
            })?;
        Ok((Self { watcher }, rx))
    }

    /// Watch a path with an explicit mode. Non-recursive is the norm —
    /// parent directories of tracked files and file sentinels; only
    /// deliberate directory sentinels (~/.config) watch recursively.
    /// Missing paths are skipped quietly.
    pub fn watch(&mut self, path: &Path, recursive: bool) {
        if !path.exists() {
            return;
        }
        let mode = if recursive && path.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let _ = self.watcher.watch(path, mode);
    }
}
