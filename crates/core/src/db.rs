//! The daemon's memory: SQLite at `~/.local/share/wukong/wukong.db`.
//! Three tables — the append-only event log, the tracked-file roster,
//! and the inbox. Migrations are idempotent CREATEs; the schema is
//! young enough to grow additively.

use crate::events::{Event, InboxItem, Resolution};
use rusqlite::{Connection, params};
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
            );",
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

    // ---- Inbox ---------------------------------------------------------

    /// Add an inbox item, or refresh the body of an existing open item
    /// with the same kind and subject — a sentinel that keeps changing
    /// is one conversation, not twelve.
    pub fn inbox_add(
        &self,
        kind: &str,
        subject: &str,
        detail: &str,
        body: &str,
    ) -> Result<InboxOutcome, DbError> {
        let updated = self.conn.execute(
            "UPDATE inbox SET ts = ?1, detail = ?2, body = ?3
             WHERE kind = ?4 AND subject = ?5 AND resolved = 0",
            params![now(), detail, body, kind, subject],
        )?;
        if updated > 0 {
            return Ok(InboxOutcome::Refreshed);
        }
        self.conn.execute(
            "INSERT INTO inbox (ts, kind, subject, detail, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), kind, subject, detail, body],
        )?;
        Ok(InboxOutcome::New)
    }

    pub fn inbox_open(&self) -> Result<Vec<InboxItem>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, kind, subject, detail, body FROM inbox
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
            "SELECT id, ts, kind, subject, detail, body FROM inbox
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
    fn inbox_dedupes_open_items_by_subject() {
        let db = db();
        assert_eq!(
            db.inbox_add("sentinel", ".zprofile", "changed", "v1")
                .unwrap(),
            InboxOutcome::New
        );
        assert_eq!(
            db.inbox_add("sentinel", ".zprofile", "changed again", "v2")
                .unwrap(),
            InboxOutcome::Refreshed
        );
        let open = db.inbox_open().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].body, "v2");

        assert!(db.inbox_resolve(open[0].id, Resolution::Ignore).unwrap());
        assert_eq!(db.inbox_count().unwrap(), 0);
        // Resolved item stays resolved; a new change opens a new item.
        assert_eq!(
            db.inbox_add("sentinel", ".zprofile", "later", "v3")
                .unwrap(),
            InboxOutcome::New
        );
    }
}
