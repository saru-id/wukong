//! The daemon's memory: SQLite at `~/.local/share/wukong/wukong.db`.
//! Three tables — the append-only event log, the tracked-file roster,
//! and the inbox. Migrations are idempotent CREATEs; the schema is
//! young enough to grow additively.

use crate::events::{Event, InboxItem, Resolution};
use rusqlite::{Connection, params};
use std::collections::HashMap;
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
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
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
                path      TEXT PRIMARY KEY,
                added_ts  TEXT NOT NULL,
                last_hash TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS inbox (
                id          INTEGER PRIMARY KEY,
                ts          TEXT NOT NULL,
                kind        TEXT NOT NULL,
                subject     TEXT NOT NULL,
                detail      TEXT NOT NULL DEFAULT '',
                body        TEXT NOT NULL DEFAULT '',
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
            );",
        )?;
        // Additive migration: the meta column (finding fingerprints as
        // JSON) arrived after v0.1.0 databases existed.
        let has_meta = conn
            .prepare("SELECT 1 FROM pragma_table_info('inbox') WHERE name = 'meta'")?
            .exists([])?;
        if !has_meta {
            conn.execute_batch("ALTER TABLE inbox ADD COLUMN meta TEXT NOT NULL DEFAULT ''")?;
        }
        // Keep the event log from growing without bound.
        conn.execute(
            "DELETE FROM events WHERE id <= (SELECT COALESCE(MAX(id), 0) - 10000 FROM events)",
            [],
        )?;
        Ok(Self { conn })
    }

    // ---- Events --------------------------------------------------------

    pub fn record(&self, kind: &str, subject: &str, detail: &str) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT INTO events (ts, kind, subject, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now(), kind, subject, detail],
        )?;
        Ok(())
    }

    pub fn events(&self, limit: usize) -> Result<Vec<Event>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, kind, subject, detail FROM events ORDER BY id DESC LIMIT ?1")?;
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

    // ---- Tracked files -------------------------------------------------

    pub fn track(&self, path: &str) -> Result<bool, DbError> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO tracked (path, added_ts) VALUES (?1, ?2)",
            params![path, now()],
        )?;
        Ok(inserted > 0)
    }

    pub fn untrack(&self, path: &str) -> Result<bool, DbError> {
        Ok(self
            .conn
            .execute("DELETE FROM tracked WHERE path = ?1", params![path])?
            > 0)
    }

    pub fn tracked(&self) -> Result<Vec<String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM tracked ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn is_tracked(&self, path: &str) -> Result<bool, DbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tracked WHERE path = ?1",
            params![path],
            |row| row.get::<_, i64>(0),
        )? > 0)
    }

    pub fn set_hash(&self, path: &str, hash: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE tracked SET last_hash = ?2 WHERE path = ?1",
            params![path, hash],
        )?;
        Ok(())
    }

    pub fn hash_of(&self, path: &str) -> Result<Option<String>, DbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_hash FROM tracked WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?)
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

    pub fn pkg_state(&self, provider: &str) -> Result<Vec<String>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM pkg_state WHERE provider = ?1 ORDER BY name")?;
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

    /// Resolve any open inbox item with this kind and subject — used
    /// when an explicit CLI action supersedes a pending offer.
    pub fn inbox_resolve_open(
        &self,
        kind: &str,
        subject: &str,
        resolution: Resolution,
    ) -> Result<bool, DbError> {
        Ok(self.conn.execute(
            "UPDATE inbox SET resolved = 1, resolution = ?3, resolved_ts = ?4
             WHERE kind = ?1 AND subject = ?2 AND resolved = 0",
            params![kind, subject, resolution.as_str(), now()],
        )? > 0)
    }

    // ---- Inbox ---------------------------------------------------------

    /// Add an inbox item, or refresh the body of an existing open item
    /// with the same kind and subject — a sentinel that keeps changing
    /// is one conversation, not twelve. `meta` carries the findings'
    /// fingerprints as JSON so a resolution can persist them.
    pub fn inbox_add(
        &self,
        kind: &str,
        subject: &str,
        detail: &str,
        body: &str,
        meta: &str,
    ) -> Result<InboxOutcome, DbError> {
        let updated = self.conn.execute(
            "UPDATE inbox SET ts = ?1, detail = ?2, body = ?3, meta = ?4
             WHERE kind = ?5 AND subject = ?6 AND resolved = 0",
            params![now(), detail, body, meta, kind, subject],
        )?;
        if updated > 0 {
            return Ok(InboxOutcome::Refreshed);
        }
        self.conn.execute(
            "INSERT INTO inbox (ts, kind, subject, detail, body, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now(), kind, subject, detail, body, meta],
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
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM inbox WHERE resolved = 0", [], |row| {
                row.get::<_, i64>(0)
            })? as usize)
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
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
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
        assert!(db.track(".zshrc").unwrap());
        assert!(!db.track(".zshrc").unwrap()); // idempotent
        assert!(db.is_tracked(".zshrc").unwrap());
        db.set_hash(".zshrc", "abc").unwrap();
        assert_eq!(db.hash_of(".zshrc").unwrap().as_deref(), Some("abc"));
        assert!(db.untrack(".zshrc").unwrap());
        assert!(db.hash_of(".zshrc").unwrap().is_none());
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
    fn v010_database_gains_meta_column() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("old.db");
        // A pre-meta inbox table, as v0.1.0 created it.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE inbox (
                id INTEGER PRIMARY KEY, ts TEXT NOT NULL, kind TEXT NOT NULL,
                subject TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '',
                body TEXT NOT NULL DEFAULT '', resolved INTEGER NOT NULL DEFAULT 0,
                resolution TEXT, resolved_ts TEXT
            );
            INSERT INTO inbox (ts, kind, subject) VALUES ('t', 'sentinel', '.zshrc');",
        )
        .unwrap();
        drop(conn);
        let db = Db::open(&path).unwrap();
        let open = db.inbox_open().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].meta, "");
    }

    #[test]
    fn inbox_dedupes_open_items_by_subject() {
        let db = db();
        assert_eq!(
            db.inbox_add("sentinel", ".zprofile", "changed", "v1", "")
                .unwrap(),
            InboxOutcome::New
        );
        assert_eq!(
            db.inbox_add("sentinel", ".zprofile", "changed again", "v2", "[\"fp1\"]")
                .unwrap(),
            InboxOutcome::Refreshed
        );
        let open = db.inbox_open().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].body, "v2");
        assert_eq!(open[0].meta, "[\"fp1\"]");

        assert!(db.inbox_resolve(open[0].id, Resolution::Ignore).unwrap());
        assert_eq!(db.inbox_count().unwrap(), 0);
        // Resolved item stays resolved; a new change opens a new item.
        assert_eq!(
            db.inbox_add("sentinel", ".zprofile", "later", "v3", "")
                .unwrap(),
            InboxOutcome::New
        );
    }
}
