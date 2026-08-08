//! The wire between wukongd and its clients: one JSON line in, one
//! JSON line out, over the unix socket. The version rides in every
//! request so a stale daemon refuses politely instead of misparsing.

use crate::events::{Event, InboxItem, Resolution};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub req: Request,
}

impl Envelope {
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
    Track { path: String },
    Untrack { path: String },
    TrackedList,
    InboxList,
    InboxResolve { id: i64, resolution: Resolution },
    Events { limit: usize },
    PushNow,
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
    Error {
        message: String,
    },
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
