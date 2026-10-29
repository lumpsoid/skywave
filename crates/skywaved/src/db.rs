//! The SQLite database (`skywave.db`) and its schema migrations.
//!
//! `serve` and the admin commands open the same database concurrently
//! (spec §5), so connections use WAL mode and a busy timeout.
//!
//! Migrations are versioned by `PRAGMA user_version`: migration `i` (0-based)
//! moves the schema from version `i` to `i + 1`. Append new migrations; never
//! edit a released one.
//!
//! Column conventions: ids are hyphenated UUID text, timestamps are RFC 3339
//! UTC text, `JSON` columns hold the serde_json encoding of the named
//! `skywave-proto` type.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Error, Result};

/// How long a writer waits for another connection's lock before failing.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const MIGRATIONS: &[&str] = &[
    // v1: initial schema (spec §5).
    r#"
    -- Global event log (D12). seq is assigned by the single writer: the first
    -- event is 1 and each later event is exactly one more.
    CREATE TABLE events (
        seq        INTEGER PRIMARY KEY CHECK (seq >= 1),
        session_id TEXT NOT NULL,
        at         TEXT NOT NULL,
        kind       TEXT NOT NULL CHECK (json_valid(kind))   -- EventKind
    ) STRICT;
    CREATE INDEX events_session ON events (session_id, seq);

    -- Command dedup: a known command_id returns the stored result.
    CREATE TABLE commands (
        command_id TEXT PRIMARY KEY,
        device_id  TEXT NOT NULL,
        result     TEXT NOT NULL CHECK (json_valid(result)) -- CommandResult
    ) STRICT;

    CREATE TABLE sessions (
        id          TEXT PRIMARY KEY,
        workdir     TEXT NOT NULL,
        -- The agent's opaque resume handle; NULL until the agent reports one.
        agent_state TEXT CHECK (agent_state IS NULL OR json_valid(agent_state))
    ) STRICT;

    CREATE TABLE devices (
        id               TEXT PRIMARY KEY,
        name             TEXT NOT NULL,
        cert_fingerprint TEXT NOT NULL UNIQUE,
        paired_at        TEXT NOT NULL,
        revoked_at       TEXT,
        last_seen        TEXT
    ) STRICT;

    -- One-time pairing codes, stored hashed.
    CREATE TABLE pair_codes (
        hash       TEXT PRIMARY KEY,
        expires_at TEXT NOT NULL,
        used       INTEGER NOT NULL DEFAULT 0 CHECK (used IN (0, 1))
    ) STRICT;

    -- Metadata of files in blobs/; sha256 is lowercase hex.
    CREATE TABLE blobs (
        id     TEXT PRIMARY KEY,
        size   INTEGER NOT NULL CHECK (size >= 0),
        sha256 TEXT NOT NULL
    ) STRICT;
    "#,
    // v2: projections that command validation reads (T03). They are derived
    // from `events` and updated in the transaction that appends the event.
    r#"
    -- 1 while the session's agent has a running turn: set by
    -- SessionStateChanged(AgentWorking), cleared by SessionStateChanged(Idle
    -- or Errored) and by TurnInterrupted.
    ALTER TABLE sessions ADD COLUMN
        turn_active INTEGER NOT NULL DEFAULT 0 CHECK (turn_active IN (0, 1));

    -- Approval requests seen in the log.
    CREATE TABLE approvals (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions (id),
        -- Seq of the ApprovalResolved or ApprovalCancelled that closed the
        -- request; NULL while it is pending.
        closed_seq INTEGER
    ) STRICT;
    CREATE INDEX approvals_pending ON approvals (closed_seq) WHERE closed_seq IS NULL;
    "#,
];

/// Schema version this build creates and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// An open, migrated database connection.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (creating if needed) the database at `path` and migrates it to
    /// [`SCHEMA_VERSION`]. Reopening an up-to-date database changes nothing.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "SQLite refused WAL journal mode");
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    /// The underlying connection, for queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The underlying connection, for transactions.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Current `user_version` of the database.
    pub fn schema_version(&self) -> Result<u32> {
        user_version(&self.conn)
    }

    fn migrate(&mut self) -> Result<()> {
        // Fast path without a write lock, so admin commands don't contend
        // with a running `serve` once the schema is current.
        let found = user_version(&self.conn)?;
        check_supported(found)?;
        if found == SCHEMA_VERSION {
            return Ok(());
        }
        // IMMEDIATE takes the write lock up front; re-read the version under
        // it in case another process migrated in between.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = user_version(&tx)?;
        check_supported(found)?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            tracing::info!(from = i, to = i + 1, "migrating database schema");
            tx.execute_batch(sql)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }
}

fn user_version(conn: &Connection) -> Result<u32> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

fn check_supported(found: u32) -> Result<()> {
    if found > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLES: [&str; 7] = [
        "approvals",
        "blobs",
        "commands",
        "devices",
        "events",
        "pair_codes",
        "sessions",
    ];

    fn schema(db: &Db) -> Vec<(String, String)> {
        let mut stmt = db
            .conn()
            .prepare("SELECT name, sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn fresh_database_gets_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let names: Vec<String> = schema(&db).into_iter().map(|(n, _)| n).collect();
        for table in TABLES {
            assert!(names.iter().any(|n| n == table), "missing table {table}");
        }
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn reopen_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let db = Db::open(&path).unwrap();
        db.conn()
            .execute(
                "INSERT INTO events (seq, session_id, at, kind) VALUES (1, 's', 't', '{}')",
                [],
            )
            .unwrap();
        let before = schema(&db);
        drop(db);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(schema(&db), before);
        let count: u32 = db
            .conn()
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn concurrent_connections_share_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let serve = Db::open(&path).unwrap();
        let admin = Db::open(&path).unwrap();
        admin
            .conn()
            .execute(
                "INSERT INTO pair_codes (hash, expires_at) VALUES ('h', 't')",
                [],
            )
            .unwrap();
        let used: i64 = serve
            .conn()
            .query_row("SELECT used FROM pair_codes WHERE hash = 'h'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(used, 0);
    }

    #[test]
    fn rejects_newer_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        drop(Db::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            Db::open(&path),
            Err(Error::SchemaTooNew { found, .. }) if found == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn json_columns_reject_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        let err = db.conn().execute(
            "INSERT INTO events (seq, session_id, at, kind) VALUES (1, 's', 't', 'not json')",
            [],
        );
        assert!(err.is_err());
    }
}
