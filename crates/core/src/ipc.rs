//! The wire between wukongd and its clients: one JSON line in, one
//! JSON line out, over the unix socket. The version rides in every
//! request so a stale daemon refuses politely instead of misparsing.

use crate::events::{Event, InboxItem, Resolution};
use crate::pkg::Provider;
use crate::settings;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub req: Request,
}

impl Envelope {
    #[must_use]
    pub fn new(req: Request) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            req,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    Status,
    Track {
        path: String,
    },
    Untrack {
        path: String,
    },
    TrackedList,
    InboxList,
    InboxResolve {
        id: i64,
        resolution: Resolution,
    },
    Events {
        limit: usize,
    },
    PushNow,
    /// Copy stored files back to their live locations (new-machine
    /// bootstrap). No path = all stored files.
    Restore {
        path: Option<String>,
        force: bool,
    },
    /// The CLI ran the provider (brew) itself; record the outcome in
    /// the manifest. `remove` = it was uninstalled.
    PkgRecord {
        provider: Provider,
        name: String,
        remove: bool,
        /// Acknowledge the install in package state WITHOUT adding it
        /// to the manifest — the `--no-track` opt-out. Without this
        /// acknowledgement the watcher would offer the package for
        /// adoption seconds after the user declined to track it.
        #[serde(default)]
        observe_only: bool,
    },
    /// Manifest entries with their live state.
    PkgList,
    /// Permanent opt-out (or undo it with `unignore`).
    PkgIgnore {
        provider: Provider,
        name: String,
        unignore: bool,
    },
    /// Bulk-adopt everything currently installed on request (formulae
    /// and casks; apps stay offer-driven).
    PkgAdoptInstalled,
    /// Stop offering anything under this path (sentinel noise valve).
    /// Applied immediately, persisted to config, and any open offers
    /// under the path are resolved away.
    Exclude {
        path: String,
    },
    /// Live file vs stored copy, as a unified diff.
    Diff {
        path: String,
    },
    /// Governed settings with desired vs live values.
    SettingsList,
    /// Record a setting's CURRENT live value as the desired value.
    SettingsRecord {
        domain: String,
        key: String,
    },
    /// Never offer this setting again (or allow it again).
    SettingsIgnore {
        domain: String,
        key: String,
        unignore: bool,
    },
    /// Snapshot every top-level scalar preference key (capture phase 1).
    SettingsCaptureStart,
    /// Diff reality against the snapshot (capture phase 2). One-shot:
    /// the snapshot is consumed.
    SettingsCaptureDiff,
    /// The store's commit history for one tracked file.
    FileLog {
        path: String,
        limit: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "res", rename_all = "kebab-case")]
pub enum Response {
    Pong {
        version: u32,
        daemon_version: String,
    },
    Status(StatusInfo),
    Ok {
        message: String,
    },
    Tracked {
        files: Vec<TrackedFile>,
    },
    Inbox {
        items: Vec<InboxItem>,
    },
    Events {
        events: Vec<Event>,
    },
    Packages {
        entries: Vec<PkgEntry>,
    },
    CaptureDiff {
        changes: Vec<CaptureChange>,
    },
    Settings {
        entries: Vec<SettingEntry>,
        /// When set, `defaults` must target files under this directory
        /// instead of real domains (sandboxed runs).
        file_domains_dir: Option<String>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgEntry {
    pub provider: Provider,
    pub name: String,
    pub installed: bool,
}

/// One key that changed between capture snapshot and diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureChange {
    pub domain: String,
    pub key: String,
    pub before: Option<settings::Value>,
    pub after: Option<settings::Value>,
    /// App furniture (window state, timestamps…) rather than a
    /// setting; hidden unless asked for.
    pub noise: bool,
    /// Human label when the corpus knows this key.
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingEntry {
    pub domain: String,
    pub key: String,
    /// Human label when the corpus knows this setting.
    pub label: Option<String>,
    /// Process to restart after applying, when the corpus knows.
    pub restart: Option<String>,
    pub desired: Option<settings::Value>,
    pub live: Option<settings::Value>,
    /// True when desired is set and live semantically matches it.
    pub in_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub machine: String,
    pub remote: String,
    pub tracked: usize,
    pub inbox: usize,
    pub last_commit: Option<String>,
    pub last_push: Option<String>,
    pub unpushed: usize,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedFile {
    pub path: String,
    /// Pretty live-path form ("~/.zshrc").
    pub display: String,
    pub exists: bool,
}
