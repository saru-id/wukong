//! The daemon's memory: `SQLite` at `~/.local/share/wukong/wukong.db`.
//! Three tables — the append-only event log, the tracked-file roster,
//! and the inbox. The schema is created complete on open; the first
//! real migration will introduce migration machinery, not before.

use crate::events::{Event, EventKind, InboxItem, InboxKind, Resolution};
use rusqlite::{Connection, params};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(dir) = path.parent() {
            crate::paths::ensure_private_dir(dir)?;
        }
        let conn = Connection::open(path)?;
        // The database holds quarantine evidence; owner-only, like
        // everything else wukong writes.
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                id      INTEGER PRIMARY KEY,
                ts      TEXT NOT NULL,
                kind    TEXT NOT NULL,
                subject TEXT NOT NULL,
                detail  TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS tracked (
                path         TEXT PRIMARY KEY,
                added_ts     TEXT NOT NULL,
                sealed       INTEGER NOT NULL DEFAULT 0,
                shared       INTEGER NOT NULL DEFAULT 0,
                content_hash TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS inbox (
                id          INTEGER PRIMARY KEY,
                ts          TEXT NOT NULL,
                kind        TEXT NOT NULL,
                subject     TEXT NOT NULL,
                detail      TEXT NOT NULL DEFAULT '',
                body        TEXT NOT NULL DEFAULT '',
                meta        TEXT NOT NULL DEFAULT '',
                resolved    INTEGER NOT NULL DEFAULT 0,
                resolution  TEXT,
                resolved_ts TEXT
            );
            CREATE TABLE IF NOT EXISTS allowances (
                path        TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                action      TEXT NOT NULL,
                created_ts  TEXT NOT NULL,
                PRIMARY KEY (path, fingerprint)
            );
            CREATE TABLE IF NOT EXISTS pkg_state (
                provider TEXT NOT NULL,
                name     TEXT NOT NULL,
                PRIMARY KEY (provider, name)
            );
            CREATE TABLE IF NOT EXISTS settings_state (
                domain TEXT NOT NULL,
                key    TEXT NOT NULL,
                value  TEXT NOT NULL,
                PRIMARY KEY (domain, key)
            );",
        )?;
        let db = Self { conn };
        db.prune_events()?;
        Ok(db)
    }

    /// Keep the event log from growing without bound. Called at open
    /// and periodically by the daemon — a long-lived process must not
    /// depend on restarts for hygiene.
    pub fn prune_events(&self) -> Result<(), DbError> {
        self.conn.execute(
            "DELETE FROM events WHERE id <= (SELECT COALESCE(MAX(id), 0) - 10000 FROM events)",
            [],
        )?;
        Ok(())
    }

    // ---- Events --------------------------------------------------------

    pub fn record(&self, kind: EventKind, subject: &str, detail: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO events (ts, kind, subject, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now(), kind.as_str(), subject, detail],
        )?;
        Ok(())
    }

    pub fn events(&self, limit: usize) -> Result<Vec<Event>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, kind, subject, detail FROM events ORDER BY id DESC LIMIT ?1")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![limit], |row| {
            Ok(Event {
                ts: row.get(0)?,
                kind: row.get(1)?,
                subject: row.get(2)?,
                detail: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Timestamp of the most recent event of one kind and subject —
    /// how the health checks bound their own nagging.
    pub fn last_event_for(
        &self,
        kind: EventKind,
        subject: &str,
    ) -> Result<Option<String>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT ts FROM events WHERE kind = ?1 AND subject = ?2 ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![kind.as_str(), subject], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    /// Timestamp of the most recent event of one kind — how `status`
    /// answers "when did we last push" across daemon restarts.
    pub fn last_event(&self, kind: EventKind) -> Result<Option<String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts FROM events WHERE kind = ?1 ORDER BY id DESC LIMIT 1")?;
        let mut rows = stmt.query_map(params![kind.as_str()], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    // ---- Tracked files -------------------------------------------------

    pub fn track(&self, path: &str, sealed: bool) -> Result<bool, DbError> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO tracked (path, added_ts, sealed) VALUES (?1, ?2, ?3)",
            params![path, now(), i64::from(sealed)],
        )?;
        if inserted == 0 && sealed {
            // Tracking an already-tracked file as sealed upgrades it.
            self.set_sealed(path, true)?;
        }
        Ok(inserted > 0)
    }

    pub fn set_shared(&self, path: &str, shared: bool) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE tracked SET shared = ?2 WHERE path = ?1",
            params![path, i64::from(shared)],
        )?;
        Ok(())
    }

    pub fn set_sealed(&self, path: &str, sealed: bool) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE tracked SET sealed = ?2 WHERE path = ?1",
            params![path, i64::from(sealed)],
        )?;
        Ok(())
    }

    /// Plaintext content hash for a sealed file — the determinism
    /// guard (age ciphertext differs every encryption).
    pub fn set_content_hash(&self, path: &str, hash: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE tracked SET content_hash = ?2 WHERE path = ?1",
            params![path, hash],
        )?;
        Ok(())
    }

    pub fn content_hash(&self, path: &str) -> Result<Option<String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_hash FROM tracked WHERE path = ?1")?;
        let mut rows = stmt.query_map(params![path], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    pub fn untrack(&self, path: &str) -> Result<bool, DbError> {
        Ok(self
            .conn
            .execute("DELETE FROM tracked WHERE path = ?1", params![path])?
            > 0)
    }

    /// Every tracked file as (store-relative path, sealed, shared).
    pub fn tracked(&self) -> Result<Vec<(String, bool, bool)>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, sealed, shared FROM tracked ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, i64>(2)? != 0,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- Allowances ----------------------------------------------------

    /// Record a sticky resolution for one finding: `approve` (the
    /// secret may be committed as-is) or `redact` (mask it in every
    /// future stored copy). Consulted by the engine on every scan, so
    /// resolving a long-lived token once is enough.
    pub fn allow(&self, path: &str, fingerprint: &str, action: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO allowances (path, fingerprint, action, created_ts)
             VALUES (?1, ?2, ?3, ?4)",
            params![path, fingerprint, action, now()],
        )?;
        Ok(())
    }

    /// fingerprint → action for one file.
    pub fn allowances_for(&self, path: &str) -> Result<HashMap<String, String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT fingerprint, action FROM allowances WHERE path = ?1")?;
        let rows = stmt.query_map(params![path], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- Package state -------------------------------------------------
    // The last ACKNOWLEDGED set of installed packages per provider.
    // Reconcile compares reality against this to fire offers only on
    // transitions; resolving/recording updates it.

    pub fn pkg_state(&self, provider: &str) -> Result<BTreeSet<String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM pkg_state WHERE provider = ?1")?;
        let rows = stmt.query_map(params![provider], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn pkg_state_add(&self, provider: &str, name: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO pkg_state (provider, name) VALUES (?1, ?2)",
            params![provider, name],
        )?;
        Ok(())
    }

    pub fn pkg_state_remove(&self, provider: &str, name: &str) -> Result<(), DbError> {
        self.conn.execute(
            "DELETE FROM pkg_state WHERE provider = ?1 AND name = ?2",
            params![provider, name],
        )?;
        Ok(())
    }

    // ---- Settings state ------------------------------------------------
    // Last ACKNOWLEDGED value per governed setting; reconcile offers
    // only on transitions against this.

    pub fn settings_state(&self) -> Result<HashMap<(String, String), String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT domain, key, value FROM settings_state")?;
        let rows = stmt.query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn settings_state_set(&self, domain: &str, key: &str, value: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings_state (domain, key, value) VALUES (?1, ?2, ?3)",
            params![domain, key, value],
        )?;
        Ok(())
    }

    pub fn settings_state_remove(&self, domain: &str, key: &str) -> Result<(), DbError> {
        self.conn.execute(
            "DELETE FROM settings_state WHERE domain = ?1 AND key = ?2",
            params![domain, key],
        )?;
        Ok(())
    }

    /// Resolve any open inbox item with this kind and subject — used
    /// when an explicit CLI action supersedes a pending offer.
    pub fn inbox_resolve_open(
        &self,
        kind: InboxKind,
        subject: &str,
        resolution: Resolution,
    ) -> Result<bool, DbError> {
        Ok(self.conn.execute(
            "UPDATE inbox SET resolved = 1, resolution = ?3, resolved_ts = ?4
             WHERE kind = ?1 AND subject = ?2 AND resolved = 0",
            params![kind.as_str(), subject, resolution.as_str(), now()],
        )? > 0)
    }

    // ---- Inbox ---------------------------------------------------------

    /// Add an inbox item, or refresh the body of an existing open item
    /// with the same kind and subject — a sentinel that keeps changing
    /// is one conversation, not twelve. `meta` carries the findings'
    /// fingerprints as JSON so a resolution can persist them.
    pub fn inbox_add(
        &self,
        kind: InboxKind,
        subject: &str,
        detail: &str,
        body: &str,
        meta: &str,
    ) -> Result<InboxOutcome, DbError> {
        let updated = self.conn.execute(
            "UPDATE inbox SET ts = ?1, detail = ?2, body = ?3, meta = ?4
             WHERE kind = ?5 AND subject = ?6 AND resolved = 0",
            params![now(), detail, body, meta, kind.as_str(), subject],
        )?;
        if updated > 0 {
            return Ok(InboxOutcome::Refreshed);
        }
        self.conn.execute(
            "INSERT INTO inbox (ts, kind, subject, detail, body, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now(), kind.as_str(), subject, detail, body, meta],
        )?;
        Ok(InboxOutcome::New)
    }

    pub fn inbox_open(&self) -> Result<Vec<InboxItem>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, kind, subject, detail, body, meta FROM inbox
             WHERE resolved = 0 ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(InboxItem {
                id: row.get(0)?,
                ts: row.get(1)?,
                kind: row.get(2)?,
                subject: row.get(3)?,
                detail: row.get(4)?,
                body: row.get(5)?,
                meta: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn inbox_count(&self) -> Result<usize, DbError> {
        let n: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM inbox WHERE resolved = 0", [], |row| {
                    row.get(0)
                })?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    pub fn inbox_get(&self, id: i64) -> Result<Option<InboxItem>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, kind, subject, detail, body, meta FROM inbox
             WHERE id = ?1 AND resolved = 0",
        )?;
        let mut rows = stmt.query_map(params![id], |row| {
            Ok(InboxItem {
                id: row.get(0)?,
                ts: row.get(1)?,
                kind: row.get(2)?,
                subject: row.get(3)?,
                detail: row.get(4)?,
                body: row.get(5)?,
                meta: row.get(6)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn inbox_resolve(&self, id: i64, resolution: Resolution) -> Result<bool, DbError> {
        Ok(self.conn.execute(
            "UPDATE inbox SET resolved = 1, resolution = ?2, resolved_ts = ?3
             WHERE id = ?1 AND resolved = 0",
            params![id, resolution.as_str(), now()],
        )? > 0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxOutcome {
    New,
    Refreshed,
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(&tempfile::TempDir::new().unwrap().path().join("t.db")).unwrap()
    }

    #[test]
    fn tracked_roster_round_trips() {
        let db = db();
        assert!(db.track(".zshrc", false).unwrap());
        assert!(!db.track(".zshrc", false).unwrap()); // idempotent
        assert_eq!(
            db.tracked().unwrap(),
            vec![(".zshrc".to_string(), false, false)]
        );
        // Re-tracking sealed upgrades in place.
        db.track(".zshrc", true).unwrap();
        assert_eq!(
            db.tracked().unwrap(),
            vec![(".zshrc".to_string(), true, false)]
        );
        db.set_content_hash(".zshrc", "abc123").unwrap();
        assert_eq!(
            db.content_hash(".zshrc").unwrap().as_deref(),
            Some("abc123")
        );
        assert!(db.untrack(".zshrc").unwrap());
        assert!(db.tracked().unwrap().is_empty());
    }

    #[test]
    fn allowances_round_trip() {
        let db = db();
        db.allow(".zshrc", "aabbccdd", "approve").unwrap();
        db.allow(".zshrc", "11223344", "redact").unwrap();
        db.allow(".gitconfig", "99999999", "approve").unwrap();
        let a = db.allowances_for(".zshrc").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a.get("aabbccdd").map(String::as_str), Some("approve"));
        assert_eq!(a.get("11223344").map(String::as_str), Some("redact"));
        // Re-resolving overwrites the action.
        db.allow(".zshrc", "aabbccdd", "redact").unwrap();
        let a = db.allowances_for(".zshrc").unwrap();
        assert_eq!(a.get("aabbccdd").map(String::as_str), Some("redact"));
    }

    #[test]
    fn inbox_dedupes_open_items_by_subject() {
        let db = db();
        assert_eq!(
            db.inbox_add(InboxKind::Sentinel, ".zprofile", "changed", "v1", "")
                .unwrap(),
            InboxOutcome::New
        );
        assert_eq!(
            db.inbox_add(
                InboxKind::Sentinel,
                ".zprofile",
                "changed again",
                "v2",
                "[\"fp1\"]"
            )
            .unwrap(),
            InboxOutcome::Refreshed
        );
        let open = db.inbox_open().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].body, "v2");
        assert_eq!(open[0].meta, "[\"fp1\"]");

        assert!(db.inbox_resolve(open[0].id, Resolution::Skip).unwrap());
        assert_eq!(db.inbox_count().unwrap(), 0);
        // Resolved item stays resolved; a new change opens a new item.
        assert_eq!(
            db.inbox_add(InboxKind::Sentinel, ".zprofile", "later", "v3", "")
                .unwrap(),
            InboxOutcome::New
        );
    }
}
