//! The shared shapes: log events, inbox items, and how an inbox item
//! can be resolved. Kinds are real enums on the write side; rows read
//! back from the database carry the string form, so a row whose kind
//! this binary doesn't recognize still displays fine.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: String,
    pub kind: String,
    pub subject: String,
    pub detail: String,
}

/// Everything the governor writes to its log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    DaemonStarted,
    Tracked,
    Untracked,
    Committed,
    Pushed,
    PushFailed,
    Quarantined,
    SentinelChanged,
    Resolved,
    Restored,
    Held,
    PkgInstalled,
    PkgRemoved,
    PkgAdopted,
    PkgIgnored,
    PkgGone,
}

impl EventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DaemonStarted => "daemon-started",
            Self::Tracked => "tracked",
            Self::Untracked => "untracked",
            Self::Committed => "committed",
            Self::Pushed => "pushed",
            Self::PushFailed => "push-failed",
            Self::Quarantined => "quarantined",
            Self::SentinelChanged => "sentinel-changed",
            Self::Resolved => "inbox-resolved",
            Self::Restored => "restored",
            Self::Held => "held",
            Self::PkgInstalled => "pkg-installed",
            Self::PkgRemoved => "pkg-removed",
            Self::PkgAdopted => "pkg-adopted",
            Self::PkgIgnored => "pkg-ignored",
            Self::PkgGone => "pkg-gone",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub subject: String,
    pub detail: String,
    /// The evidence: a diff, masked findings, or a file excerpt.
    /// Always passes through the gate's masking before storage.
    pub body: String,
    /// Machine-readable extras — for quarantines, the findings'
    /// fingerprints as a JSON array.
    pub meta: String,
}

impl InboxItem {
    /// The typed kind, when this binary knows it.
    #[must_use]
    pub fn kind(&self) -> Option<InboxKind> {
        InboxKind::parse(&self.kind)
    }
}

/// What an inbox item is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxKind {
    /// A tracked file's change was held by the secret gate.
    Quarantine,
    /// An untracked sentinel changed — track it?
    Sentinel,
    /// A package appeared outside wukong — adopt it? (`ignore` on
    /// package items is PERMANENT: it lands on the manifest's ignore
    /// list and the package is never offered again.)
    Package,
    /// A manifest package vanished outside wukong — drop it?
    PackageGone,
}

impl InboxKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quarantine => "quarantine",
            Self::Sentinel => "sentinel",
            Self::Package => "package",
            Self::PackageGone => "package-gone",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "quarantine" => Some(Self::Quarantine),
            "sentinel" => Some(Self::Sentinel),
            "package" => Some(Self::Package),
            "package-gone" => Some(Self::PackageGone),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    /// Quarantine: commit as-is, forever (fingerprint allowance).
    /// Sentinel: start tracking. Package: adopt into the manifest.
    Approve,
    /// Quarantine only: mask the finding in every stored copy, forever;
    /// the live file is never touched.
    Redact,
    /// Quarantine/sentinel: drop the item; it may return when the file
    /// next changes. Package: PERMANENT opt-out via the manifest.
    Ignore,
}

impl Resolution {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Redact => "redact",
            Self::Ignore => "ignore",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Self::Approve),
            "redact" => Some(Self::Redact),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }
}
