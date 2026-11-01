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
    // v3: the agent supervisor's read position in the log (T10).
    r#"
    -- Seq of the last event the agent supervisor has handled; one row. A
    -- database that predates the supervisor starts at its current head:
    -- nothing logged before had an agent to deliver to.
    CREATE TABLE supervisor_cursor (
        id  INTEGER PRIMARY KEY CHECK (id = 1),
        seq INTEGER NOT NULL CHECK (seq >= 0)
    ) STRICT;
    INSERT INTO supervisor_cursor (id, seq) SELECT 1, COALESCE(MAX(seq), 0) FROM events;
    "#,
    // v4: what `ResolveApproval.rule` is checked against (protocol v4).
    r#"
    -- How many rule suggestions the request offers; an answer's rule must
    -- index one. Requests logged before v4 offered none.
    ALTER TABLE approvals ADD COLUMN
        rules INTEGER NOT NULL DEFAULT 0 CHECK (rules >= 0);
    "#,
    // v5: `Event::command_id` (protocol v6). Events logged before carry
    // none.
    r#"
    -- The command that logged the event; NULL for agent and daemon events.
    ALTER TABLE events ADD COLUMN command_id TEXT;
    "#,
    // v6: what the supervisor gives a starting agent (protocol v7).
    r#"
    -- The session's permission mode and model as the log has them, folded
    -- with `AgentSettings::apply`; NULL until the first such event.
    ALTER TABLE sessions ADD COLUMN
        agent_settings TEXT CHECK (agent_settings IS NULL OR json_valid(agent_settings));
    "#,
    // v7: session contexts (protocol v9). A session holds an ordered list
    // of contexts, each with its own resume handle; every event and
    // approval names the context it belongs to. A session's first context
    // has the session's own id, so existing data moves into one context per
    // session. SQLite adds a NOT NULL column only with a constant default:
    // the '' defaults stay in the schema, so every INSERT must name these
    // columns.
    r#"
    -- The contexts of every session, in the order they entered the log
    -- (rowid). Names live in the log only: nothing on the server reads them.
    CREATE TABLE contexts (
        id          TEXT PRIMARY KEY,
        session_id  TEXT NOT NULL REFERENCES sessions (id),
        -- The agent's opaque resume handle for this context; NULL until the
        -- agent reports one.
        agent_state TEXT CHECK (agent_state IS NULL OR json_valid(agent_state))
    ) STRICT;
    CREATE INDEX contexts_session ON contexts (session_id);
    INSERT INTO contexts (id, session_id, agent_state)
        SELECT id, id, agent_state FROM sessions ORDER BY rowid;

    -- The context the session runs in now: set by SessionCreated,
    -- ContextStarted and ContextSwitched.
    ALTER TABLE sessions ADD COLUMN current_context TEXT NOT NULL DEFAULT '';
    UPDATE sessions SET current_context = id;
    -- The context of the turn turn_active describes: set with it by
    -- SessionStateChanged(AgentWorking), cleared with it.
    ALTER TABLE sessions ADD COLUMN turn_context TEXT;
    UPDATE sessions SET turn_context = id WHERE turn_active = 1;
    -- Opaque grants the agent keeps for the whole session, across contexts.
    ALTER TABLE sessions ADD COLUMN
        agent_grants TEXT CHECK (agent_grants IS NULL OR json_valid(agent_grants));
    ALTER TABLE sessions DROP COLUMN agent_state;

    -- The context the event belongs to (Event::context).
    ALTER TABLE events ADD COLUMN context TEXT NOT NULL DEFAULT '';
    UPDATE events SET context = session_id;
    UPDATE events SET kind = json_set(kind, '$.context_name', 'start')
        WHERE json_extract(kind, '$.type') = 'session_created';

    -- The context the approval was requested in; its answer is stamped so.
    ALTER TABLE approvals ADD COLUMN context TEXT NOT NULL DEFAULT '';
    UPDATE approvals SET context = session_id;
    "#,
    // v8: moving a context into a new session (protocol v10). Both columns
    // are NULL for every context that exists before it.
    r#"
    -- The context this one continues (SessionCreated.from): its conversation
    -- is forked on the context's first start, until it has a handle of its own.
    ALTER TABLE contexts ADD COLUMN origin TEXT REFERENCES contexts (id);
    -- The session a ContextMoved sent the context to; set, it is never current
    -- or started again.
    ALTER TABLE contexts ADD COLUMN moved_to TEXT REFERENCES sessions (id);
    "#,
    // v9: the agent's background work (protocol v11).
    r#"
    -- The session's agent's background tasks and schedules as the log has
    -- them, folded with `AgentWork::apply`; NULL until the first such event.
    ALTER TABLE sessions ADD COLUMN
        agent_work TEXT CHECK (agent_work IS NULL OR json_valid(agent_work));
    "#,
    // v10: adopting an outside agent conversation (protocol v12). NULL for
    // every context that exists before it.
    r#"
    -- The agent's own id of an outside conversation this context continues
    -- (SessionCreated.from = Agent): it is forked on the context's first start,
    -- until the context has a handle of its own. NULL for every other context.
    -- Not a reference to anything here, and not a ResumeHandle: only the
    -- adapter knows a handle's shape (D3).
    ALTER TABLE contexts ADD COLUMN adopted TEXT;
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

    /// Every row that breaks the context stamp invariant, described; empty
    /// when it holds. The invariant: every event, approval and session
    /// names a context (never the `''` the v7 defaults leave behind an
    /// `INSERT` that forgot the column), and that context is a `contexts`
    /// row of the same session. Test support: the daemon's test fixtures
    /// check it after every test.
    #[doc(hidden)]
    pub fn context_stamp_violations(&self) -> Result<Vec<String>> {
        // Each query yields (table, row key, context) for a bad row.
        const CHECKS: [&str; 4] = [
            "SELECT 'events', CAST(seq AS TEXT), context FROM events e WHERE NOT EXISTS
               (SELECT 1 FROM contexts c WHERE c.id = e.context AND c.session_id = e.session_id)",
            "SELECT 'approvals', id, context FROM approvals a WHERE NOT EXISTS
               (SELECT 1 FROM contexts c WHERE c.id = a.context AND c.session_id = a.session_id)",
            "SELECT 'sessions.current_context', id, current_context FROM sessions s
               WHERE NOT EXISTS (SELECT 1 FROM contexts c
                 WHERE c.id = s.current_context AND c.session_id = s.id)",
            "SELECT 'sessions.turn_context', id, turn_context FROM sessions s
               WHERE turn_context IS NOT NULL AND NOT EXISTS (SELECT 1 FROM contexts c
                 WHERE c.id = s.turn_context AND c.session_id = s.id)",
        ];
        let mut found = Vec::new();
        for sql in CHECKS {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(format!(
                    "{} {}: context {:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?
                ))
            })?;
            for row in rows {
                found.push(row?);
            }
        }
        Ok(found)
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

    const TABLES: [&str; 9] = [
        "approvals",
        "blobs",
        "commands",
        "contexts",
        "devices",
        "events",
        "pair_codes",
        "sessions",
        "supervisor_cursor",
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

    /// A v6 database, from before session contexts, moves each session into
    /// one context with the session's id.
    #[test]
    fn v7_moves_every_session_into_its_first_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let (a, b) = (
            "01920000-0000-7000-8000-00000000000a",
            "01920000-0000-7000-8000-00000000000b",
        );
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..6] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 6).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, agent_state, turn_active)
                    VALUES ('{a}', '/w/a', '{{"session":"agent-a"}}', 1),
                           ('{b}', '/w/b', NULL, 0);
                INSERT INTO events (seq, session_id, at, command_id, kind) VALUES
                    (1, '{a}', 't', 'c1', '{{"type":"session_created","workdir":"/w/a"}}'),
                    (2, '{b}', 't', 'c2', '{{"type":"session_created","workdir":"/w/b"}}'),
                    (3, '{a}', 't', NULL, '{{"type":"session_state_changed","state":"agent_working"}}'),
                    (4, '{a}', 't', NULL, '{{"type":"approval_requested","id":"p1","tool":"x","summary":"y","input":null}}');
                INSERT INTO approvals (id, session_id, closed_seq) VALUES ('p1', '{a}', NULL);
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let conn = db.conn();
        let rows = |sql: &str| -> Vec<Vec<Option<String>>> {
            let mut stmt = conn.prepare(sql).unwrap();
            let n = stmt.column_count();
            stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let s = |v: &str| Some(v.to_owned());

        // One context per session, with its id and the handle moved there.
        assert_eq!(
            rows("SELECT id, session_id, agent_state FROM contexts ORDER BY rowid"),
            [
                vec![s(a), s(a), s(r#"{"session":"agent-a"}"#)],
                vec![s(b), s(b), None],
            ]
        );
        assert_eq!(
            rows("SELECT current_context, turn_context, agent_grants FROM sessions ORDER BY rowid"),
            [vec![s(a), s(a), None], vec![s(b), None, None]]
        );
        let columns = rows("SELECT name FROM pragma_table_info('sessions')");
        assert!(!columns.contains(&vec![s("agent_state")]), "{columns:?}");
        assert!(columns.contains(&vec![s("agent_grants")]), "{columns:?}");

        // Every event and approval is stamped with its session's id.
        assert_eq!(
            rows("SELECT context FROM events ORDER BY seq"),
            [vec![s(a)], vec![s(b)], vec![s(a)], vec![s(a)]]
        );
        assert_eq!(rows("SELECT context FROM approvals"), [vec![s(a)]]);
        // The first contexts of old sessions are named "start"; other kinds
        // are untouched.
        assert_eq!(
            rows("SELECT kind FROM events ORDER BY seq"),
            [
                vec![s(
                    r#"{"type":"session_created","workdir":"/w/a","context_name":"start"}"#
                )],
                vec![s(
                    r#"{"type":"session_created","workdir":"/w/b","context_name":"start"}"#
                )],
                vec![s(
                    r#"{"type":"session_state_changed","state":"agent_working"}"#
                )],
                vec![s(
                    r#"{"type":"approval_requested","id":"p1","tool":"x","summary":"y","input":null}"#
                )],
            ]
        );
        assert_eq!(db.context_stamp_violations().unwrap(), Vec::<String>::new());
    }

    /// A v7 database, from before moving contexts, gains `contexts.origin`
    /// and `contexts.moved_to`, NULL for every context it has.
    #[test]
    fn v8_adds_origin_and_moved_to_to_every_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let (a, c) = (
            "01920000-0000-7000-8000-00000000000a",
            "01920000-0000-7000-8000-00000000000c",
        );
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..7] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 7).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context) VALUES ('{a}', '/w/a', '{c}');
                INSERT INTO contexts (id, session_id, agent_state)
                    VALUES ('{a}', '{a}', '{{"session":"agent-a"}}'), ('{c}', '{a}', NULL);
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT id, session_id, agent_state, origin, moved_to FROM contexts ORDER BY rowid",
            )
            .unwrap();
        let rows: Vec<[Option<String>; 5]> = stmt
            .query_map([], |r| {
                Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            rows,
            [
                [s(a), s(a), s(r#"{"session":"agent-a"}"#), None, None],
                [s(c), s(a), None, None, None],
            ]
        );
        let current: String = db
            .conn()
            .query_row("SELECT current_context FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(current, c);
        assert_eq!(db.context_stamp_violations().unwrap(), Vec::<String>::new());
    }

    /// A v8 database, from before background work, gains
    /// `sessions.agent_work`, NULL for every session it has, and the column
    /// takes JSON only.
    #[test]
    fn v9_adds_agent_work_to_every_session() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let a = "01920000-0000-7000-8000-00000000000a";
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..8] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 8).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context, agent_settings)
                    VALUES ('{a}', '/w/a', '{a}', '{{}}');
                INSERT INTO contexts (id, session_id) VALUES ('{a}', '{a}');
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let row: (String, Option<String>, Option<String>) = db
            .conn()
            .query_row(
                "SELECT workdir, agent_settings, agent_work FROM sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("/w/a".to_owned(), Some("{}".to_owned()), None));
        let set = |json: &str| {
            db.conn()
                .execute("UPDATE sessions SET agent_work = ?1", [json])
        };
        assert!(set(r#"{"tasks":[],"schedules":[]}"#).is_ok());
        assert!(set("not json").is_err());
    }

    /// A v9 database, from before adopting outside conversations, gains
    /// `contexts.adopted`, NULL for every context it has; what the rows had
    /// is kept.
    #[test]
    fn v10_adds_adopted_to_every_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skywave.db");
        let (a, b) = (
            "01920000-0000-7000-8000-00000000000a",
            "01920000-0000-7000-8000-00000000000b",
        );
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..9] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
            conn.execute_batch(&format!(
                r#"
                INSERT INTO sessions (id, workdir, current_context)
                    VALUES ('{a}', '/w/a', '{a}'), ('{b}', '/w/a', '{b}');
                INSERT INTO contexts (id, session_id, agent_state, origin)
                    VALUES ('{a}', '{a}', '{{"session":"agent-a"}}', NULL),
                           ('{b}', '{b}', NULL, '{a}');
                "#
            ))
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db
            .conn()
            .prepare("SELECT id, agent_state, origin, adopted FROM contexts ORDER BY rowid")
            .unwrap();
        let rows: Vec<[Option<String>; 4]> = stmt
            .query_map([], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            rows,
            [
                [s(a), s(r#"{"session":"agent-a"}"#), None, None],
                [s(b), None, s(a), None],
            ]
        );
    }

    #[test]
    fn the_stamp_check_names_rows_without_a_context_of_their_session() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        db.conn()
            .execute_batch(
                "INSERT INTO sessions (id, workdir, current_context) VALUES ('a', '/', 'a'),
                                                                         ('b', '/', 'b');
                 INSERT INTO contexts (id, session_id) VALUES ('a', 'a'), ('b', 'b');
                 INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (1, 'a', 't', 'a', '{}');",
            )
            .unwrap();
        assert_eq!(db.context_stamp_violations().unwrap(), Vec::<String>::new());
        db.conn()
            .execute_batch(
                "INSERT INTO events (seq, session_id, at, kind) VALUES (2, 'a', 't', '{}');
                 INSERT INTO events (seq, session_id, at, context, kind)
                     VALUES (3, 'a', 't', 'b', '{}');
                 INSERT INTO approvals (id, session_id) VALUES ('p', 'a');
                 UPDATE sessions SET current_context = 'a', turn_context = 'a' WHERE id = 'b';",
            )
            .unwrap();
        assert_eq!(
            db.context_stamp_violations().unwrap(),
            [
                r#"events 2: context """#,
                r#"events 3: context "b""#,
                r#"approvals p: context """#,
                r#"sessions.current_context b: context "a""#,
                r#"sessions.turn_context b: context "a""#,
            ]
        );
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
