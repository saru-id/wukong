//! The wire between wukongd and its clients: one JSON line in, one
//! JSON line out, over the unix socket. The version rides in every
//! request so a stale daemon refuses politely instead of misparsing.

use crate::events::{Event, InboxItem, Resolution};
use crate::pkg::Provider;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 3;

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
