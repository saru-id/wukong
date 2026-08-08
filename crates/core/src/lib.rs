//! The domain of wukong, the system governor: configuration, the
//! mirror store, the secret gate, the event log, and the IPC contract
//! between the daemon and its clients. Nothing here spawns a watcher
//! or binds a socket — the daemon and CLI compose these pieces; this
//! crate stays testable from `cargo nextest` alone.

pub mod config;
pub mod db;
pub mod events;
pub mod gate;
pub mod ipc;
pub mod paths;
pub mod store;

pub use config::Config;
pub use db::Db;
pub use events::{Event, EventKind, InboxItem, InboxKind, Resolution};
pub use gate::{Finding, GateVerdict, scan};
pub use store::Store;
