//! The shared shapes: log events, inbox items, and how an inbox item
//! can be resolved. String-typed kinds keep the schema additive; the
//! constants below are the vocabulary both binaries speak.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: String,
    pub kind: String,
    pub subject: String,
    pub detail: String,
}

/// Event kinds written to the log.
pub mod kind {
    pub const DAEMON_STARTED: &str = "daemon-started";
    pub const TRACKED: &str = "tracked";
    pub const UNTRACKED: &str = "untracked";
    pub const COMMITTED: &str = "committed";
    pub const PUSHED: &str = "pushed";
    pub const QUARANTINED: &str = "quarantined";
    pub const SENTINEL: &str = "sentinel-changed";
    pub const RESOLVED: &str = "inbox-resolved";
}

pub use kind as EventKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub subject: String,
    pub detail: String,
    /// The evidence: a diff, masked findings, or a file excerpt.
    pub body: String,
}

/// Inbox item kinds.
pub mod inbox_kind {
    /// A tracked file's change was held by the secret gate.
    pub const QUARANTINE: &str = "quarantine";
    /// An untracked sentinel changed — track it?
    pub const SENTINEL: &str = "sentinel";
}

pub use inbox_kind as InboxKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    /// Quarantine: commit as-is. Sentinel: start tracking.
    Approve,
    /// Quarantine only: commit with the findings masked in the stored
    /// copy; the live file is never touched.
    Redact,
    /// Drop the item; for sentinels, stop suggesting until it changes
    /// again.
    Ignore,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Approve => "approve",
            Resolution::Redact => "redact",
            Resolution::Ignore => "ignore",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Resolution::Approve),
            "redact" => Some(Resolution::Redact),
            "ignore" => Some(Resolution::Ignore),
            _ => None,
        }
    }
}
