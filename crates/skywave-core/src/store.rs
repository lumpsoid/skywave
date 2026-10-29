//! The local replica: one SQLite database in the core's data directory
//! (spec §7, D6).
//!
//! Holds the replicated event log, projected session summaries, the outbox,
//! the pull cursor, blob rows and settings. Startup always reads everything
//! back from here ([`Store::load`]), so a cold start is the normal start.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use skywave_proto::{BlobId, Command, CommandId, CommandResult, Event, EventBatch, Seq, SessionId};
use uuid::Uuid;

use crate::projection::{SessionSummary, blob_ref};

/// File name of the database inside the core's data directory.
pub const DB_FILE: &str = "core.db";

/// Schema migrations; entry `i` moves the schema from version `i` to `i + 1`.
/// The schema version is SQLite's `user_version`.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE events (
    seq INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    event TEXT NOT NULL
);
CREATE INDEX events_by_session ON events (session_id, seq);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workdir TEXT,
    state TEXT NOT NULL,
    pending_approvals TEXT NOT NULL,
    last_activity TEXT NOT NULL,
    last_seq INTEGER NOT NULL
);

CREATE TABLE cursor (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    seq INTEGER NOT NULL
);
INSERT INTO cursor (id, seq) VALUES (0, 0);

CREATE TABLE outbox (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    command_id TEXT NOT NULL UNIQUE,
    command TEXT NOT NULL,
    sending INTEGER NOT NULL DEFAULT 0,
    result TEXT
);

CREATE TABLE blobs (
    id TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('not_downloaded', 'downloading', 'available')),
    got INTEGER,
    path TEXT
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#];

/// Highest schema version this build understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot create data directory {path}: {source}")]
    DataDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database schema version {found} is newer than supported version {SCHEMA_VERSION}")]
    NewerSchema { found: i64 },
    /// The batch starts after `cursor + 1`; applying it would leave a hole.
    #[error("event batch from {from:?} leaves a gap after cursor {cursor:?}")]
    Gap { cursor: Seq, from: Seq },
    /// Seq 0 is "before everything" and never names an event.
    #[error("event batch starts at seq 0")]
    BatchFromZero,
    #[error("value {0} does not fit an SQLite integer")]
    OutOfRange(u64),
    #[error("command {0:?} is not in the outbox")]
    UnknownCommand(CommandId),
    #[error("blob {0:?} is not known")]
    UnknownBlob(BlobId),
    #[error("path {0} is not valid UTF-8")]
    NonUtf8Path(PathBuf),
}

/// Whether blobs download alongside metadata or on explicit intent (D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlobPolicy {
    Eager,
    #[default]
    OnDemand,
}

/// User settings kept in the replica.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub blob_policy: BlobPolicy,
    /// Address of the paired server, e.g. `host:port`; `None` before pairing.
    pub server_address: Option<String>,
}

/// Delivery state of a queued command.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboxStatus {
    /// Persisted locally, not yet handed to the transport.
    Queued,
    /// Handed to the transport, no result yet.
    Sending,
    /// The server's answer; final.
    Done(CommandResult),
}

impl OutboxStatus {
    /// Whether the command still needs to be (re)sent.
    pub fn is_pending(&self) -> bool {
        !matches!(self, OutboxStatus::Done(_))
    }
}

/// One command in the outbox. The full [`Command`] is kept so a resend uses
/// the same [`CommandId`] (D7).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub command: Command,
    pub status: OutboxStatus,
}

/// Local state of one blob.
#[derive(Debug, Clone, PartialEq)]
pub enum BlobStatus {
    NotDownloaded,
    /// `got` bytes of the content are in the partial file at `path`.
    Downloading {
        got: u64,
        path: PathBuf,
    },
    /// Complete content is at `path`.
    Available {
        path: PathBuf,
    },
}

/// A blob the replica knows of, registered when an event references it.
#[derive(Debug, Clone, PartialEq)]
pub struct BlobRecord {
    pub id: BlobId,
    pub size: u64,
    pub status: BlobStatus,
}

/// Result of [`Store::apply_batch`].
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Cursor after the batch.
    pub cursor: Seq,
    /// Number of events newly stored; 0 when the batch was already applied.
    pub new_events: usize,
    /// Sessions whose summaries changed.
    pub sessions: Vec<SessionId>,
}

/// Everything the replica holds except the events themselves, as read on
/// startup.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub cursor: Seq,
    /// Most recently active first.
    pub sessions: Vec<SessionSummary>,
    /// Oldest first.
    pub outbox: Vec<OutboxEntry>,
    pub blobs: Vec<BlobRecord>,
    pub settings: Settings,
}

/// Handle to the replica database.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the replica in `data_dir` and migrates it
    /// to the current schema.
    ///
    /// Commands left in `Sending` by a previous run go back to `Queued`:
    /// nothing is in flight after a restart, and the server dedups the
    /// resend (D7).
    pub fn open(data_dir: &Path) -> Result<Store, StoreError> {
        fs::create_dir_all(data_dir).map_err(|source| StoreError::DataDir {
            path: data_dir.to_path_buf(),
            source,
        })?;
        let conn = Connection::open(data_dir.join(DB_FILE))?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        // Every user action must survive power loss once dispatch returns.
        conn.pragma_update(None, "synchronous", "FULL")?;
        Store::init(conn)
    }

    /// A replica that lives only in memory, for tests of components above
    /// the store.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Store, StoreError> {
        migrate(&mut conn)?;
        conn.execute("UPDATE outbox SET sending = 0 WHERE result IS NULL", [])?;
        Ok(Store { conn })
    }

    /// Reads back the whole replica state except the event log.
    pub fn load(&self) -> Result<Snapshot, StoreError> {
        Ok(Snapshot {
            cursor: self.cursor()?,
            sessions: self.sessions()?,
            outbox: self.outbox()?,
            blobs: self.blobs()?,
            settings: self.settings()?,
        })
    }

    // ---- events, sessions, cursor -------------------------------------

    /// Seq of the newest applied event; [`Seq::ZERO`] for a fresh replica.
    pub fn cursor(&self) -> Result<Seq, StoreError> {
        read_cursor(&self.conn)
    }

    /// Stores the batch's new events, updates the session projection and
    /// registers referenced blobs, then advances the cursor — all in one
    /// transaction.
    ///
    /// Idempotent: events at or below the cursor are skipped, so replaying a
    /// batch or an overlapping range is a no-op for the old part. A batch
    /// starting after `cursor + 1` is rejected with [`StoreError::Gap`] and
    /// changes nothing.
    pub fn apply_batch(&mut self, batch: &EventBatch) -> Result<Applied, StoreError> {
        if batch.from == Seq::ZERO {
            return Err(StoreError::BatchFromZero);
        }
        let tx = self.conn.transaction()?;
        let cursor = read_cursor(&tx)?;
        if batch.from > cursor.next() {
            return Err(StoreError::Gap {
                cursor,
                from: batch.from,
            });
        }

        let mut touched: BTreeMap<SessionId, SessionSummary> = BTreeMap::new();
        let mut new_cursor = cursor;
        let mut new_events = 0;
        for (seq, event) in batch.iter().filter(|(seq, _)| *seq > cursor) {
            tx.execute(
                "INSERT INTO events (seq, session_id, event) VALUES (?1, ?2, ?3)",
                params![
                    seq_sql(seq)?,
                    event.session_id.0.to_string(),
                    to_json(event)?
                ],
            )?;
            match touched.get_mut(&event.session_id) {
                Some(summary) => summary.apply(seq, event),
                None => {
                    let summary = match read_session(&tx, event.session_id)? {
                        Some(mut summary) => {
                            summary.apply(seq, event);
                            summary
                        }
                        None => SessionSummary::new(seq, event),
                    };
                    touched.insert(event.session_id, summary);
                }
            }
            if let Some(blob) = blob_ref(&event.kind) {
                tx.execute(
                    "INSERT INTO blobs (id, size, state) VALUES (?1, ?2, 'not_downloaded')
                     ON CONFLICT (id) DO NOTHING",
                    params![blob.id.0.to_string(), u64_sql(blob.size)?],
                )?;
            }
            new_cursor = seq;
            new_events += 1;
        }
        for summary in touched.values() {
            write_session(&tx, summary)?;
        }
        if new_cursor != cursor {
            tx.execute(
                "UPDATE cursor SET seq = ?1 WHERE id = 0",
                [seq_sql(new_cursor)?],
            )?;
        }
        tx.commit()?;
        Ok(Applied {
            cursor: new_cursor,
            new_events,
            sessions: touched.into_keys().collect(),
        })
    }

    /// All session summaries, most recently active first.
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY last_seq DESC"
        ))?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn session(&self, id: SessionId) -> Result<Option<SessionSummary>, StoreError> {
        read_session(&self.conn, id)
    }

    /// The session's events in log order.
    pub fn session_events(&self, id: SessionId) -> Result<Vec<(Seq, Event)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, event FROM events WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([id.0.to_string()], |row| {
            Ok((seq_col(row, 0)?, json_col(row, 1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- outbox ---------------------------------------------------------

    /// Persists a command as `Queued`. Enqueuing a [`CommandId`] that is
    /// already in the outbox changes nothing.
    pub fn enqueue(&self, command: &Command) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO outbox (command_id, command) VALUES (?1, ?2)
             ON CONFLICT (command_id) DO NOTHING",
            params![command.id.0.to_string(), to_json(command)?],
        )?;
        Ok(())
    }

    /// Every outbox entry, oldest first, including finished ones.
    pub fn outbox(&self) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox("")
    }

    /// Entries still awaiting a result, oldest first: the send order.
    pub fn pending_outbox(&self) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox("WHERE result IS NULL")
    }

    fn query_outbox(&self, filter: &str) -> Result<Vec<OutboxEntry>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT command, sending, result FROM outbox {filter} ORDER BY position"
        ))?;
        let rows = stmt.query_map([], |row| {
            let command = json_col(row, 0)?;
            let sending: bool = row.get(1)?;
            let result: Option<CommandResult> = opt_json_col(row, 2)?;
            let status = match (result, sending) {
                (Some(result), _) => OutboxStatus::Done(result),
                (None, true) => OutboxStatus::Sending,
                (None, false) => OutboxStatus::Queued,
            };
            Ok(OutboxEntry { command, status })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Marks a pending command as handed to the transport. A command that
    /// already has its result stays `Done`.
    pub fn mark_sending(&self, id: CommandId) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE outbox SET sending = 1 WHERE command_id = ?1 AND result IS NULL",
            [id.0.to_string()],
        )?;
        if changed == 0 {
            self.require_command(id)?;
        }
        Ok(())
    }

    /// Records the server's answer. The first recorded result is kept; the
    /// server returns the same one for every resend anyway.
    pub fn record_result(&self, id: CommandId, result: &CommandResult) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE outbox SET result = ?2, sending = 0 WHERE command_id = ?1 AND result IS NULL",
            params![id.0.to_string(), to_json(result)?],
        )?;
        if changed == 0 {
            self.require_command(id)?;
        }
        Ok(())
    }

    fn require_command(&self, id: CommandId) -> Result<(), StoreError> {
        let known = self
            .conn
            .query_row(
                "SELECT 1 FROM outbox WHERE command_id = ?1",
                [id.0.to_string()],
                |_| Ok(()),
            )
            .optional()?;
        known.ok_or(StoreError::UnknownCommand(id))
    }

    // ---- blobs ------------------------------------------------------------

    pub fn blobs(&self) -> Result<Vec<BlobRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, size, state, got, path FROM blobs ORDER BY id")?;
        let rows = stmt.query_map([], blob_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn blob(&self, id: BlobId) -> Result<Option<BlobRecord>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, size, state, got, path FROM blobs WHERE id = ?1",
                [id.0.to_string()],
                blob_from_row,
            )
            .optional()?)
    }

    pub fn set_blob_status(&self, id: BlobId, status: &BlobStatus) -> Result<(), StoreError> {
        let (state, got, path) = match status {
            BlobStatus::NotDownloaded => ("not_downloaded", None, None),
            BlobStatus::Downloading { got, path } => {
                ("downloading", Some(u64_sql(*got)?), Some(path_sql(path)?))
            }
            BlobStatus::Available { path } => ("available", None, Some(path_sql(path)?)),
        };
        let changed = self.conn.execute(
            "UPDATE blobs SET state = ?2, got = ?3, path = ?4 WHERE id = ?1",
            params![id.0.to_string(), state, got, path],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownBlob(id));
        }
        Ok(())
    }

    // ---- settings -----------------------------------------------------------

    pub fn settings(&self) -> Result<Settings, StoreError> {
        let mut settings = Settings::default();
        let mut stmt = self.conn.prepare("SELECT key, value FROM settings")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            match key.as_str() {
                KEY_BLOB_POLICY => {
                    settings.blob_policy = match value.as_str() {
                        "eager" => BlobPolicy::Eager,
                        "on_demand" => BlobPolicy::OnDemand,
                        _ => {
                            return Err(
                                bad_value(1, Type::Text, format!("blob policy {value:?}")).into()
                            );
                        }
                    }
                }
                KEY_SERVER_ADDRESS => settings.server_address = Some(value),
                // Keys written by a newer build are left alone.
                _ => {}
            }
        }
        Ok(settings)
    }

    pub fn set_blob_policy(&self, policy: BlobPolicy) -> Result<(), StoreError> {
        let value = match policy {
            BlobPolicy::Eager => "eager",
            BlobPolicy::OnDemand => "on_demand",
        };
        self.put_setting(KEY_BLOB_POLICY, Some(value))
    }

    pub fn set_server_address(&self, address: Option<&str>) -> Result<(), StoreError> {
        self.put_setting(KEY_SERVER_ADDRESS, address)
    }

    fn put_setting(&self, key: &str, value: Option<&str>) -> Result<(), StoreError> {
        match value {
            Some(value) => self.conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?,
            None => self
                .conn
                .execute("DELETE FROM settings WHERE key = ?1", [key])?,
        };
        Ok(())
    }
}

const KEY_BLOB_POLICY: &str = "blob_policy";
const KEY_SERVER_ADDRESS: &str = "server_address";

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    let found: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::NewerSchema { found });
    }
    let start = usize::try_from(found).unwrap_or(0);
    for script in &MIGRATIONS[start..] {
        tx.execute_batch(script)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn read_cursor(conn: &Connection) -> Result<Seq, StoreError> {
    Ok(
        conn.query_row("SELECT seq FROM cursor WHERE id = 0", [], |row| {
            seq_col(row, 0)
        })?,
    )
}

const SESSION_COLUMNS: &str = "id, workdir, state, pending_approvals, last_activity, last_seq";

fn read_session(conn: &Connection, id: SessionId) -> Result<Option<SessionSummary>, StoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
            [id.0.to_string()],
            session_from_row,
        )
        .optional()?)
}

fn write_session(tx: &Transaction<'_>, summary: &SessionSummary) -> Result<(), StoreError> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ),
        params![
            summary.id.0.to_string(),
            summary.workdir,
            to_json(&summary.state)?,
            to_json(&summary.pending_approvals)?,
            summary.last_activity.to_rfc3339(),
            seq_sql(summary.last_seq)?,
        ],
    )?;
    Ok(())
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<SessionSummary> {
    let last_activity: String = row.get(4)?;
    let last_activity = DateTime::parse_from_rfc3339(&last_activity)
        .map_err(|e| bad_value(4, Type::Text, e))?
        .with_timezone(&Utc);
    Ok(SessionSummary {
        id: SessionId(uuid_col(row, 0)?),
        workdir: row.get(1)?,
        state: json_col(row, 2)?,
        pending_approvals: json_col(row, 3)?,
        last_activity,
        last_seq: seq_col(row, 5)?,
    })
}

fn blob_from_row(row: &Row<'_>) -> rusqlite::Result<BlobRecord> {
    let state: String = row.get(2)?;
    let got: Option<i64> = row.get(3)?;
    let path: Option<String> = row.get(4)?;
    let status = match (state.as_str(), got, path) {
        ("not_downloaded", _, _) => BlobStatus::NotDownloaded,
        ("downloading", Some(got), Some(path)) => BlobStatus::Downloading {
            got: u64::try_from(got).map_err(|e| bad_value(3, Type::Integer, e))?,
            path: path.into(),
        },
        ("available", _, Some(path)) => BlobStatus::Available { path: path.into() },
        _ => {
            return Err(bad_value(
                2,
                Type::Text,
                format!("blob state {state:?} with missing columns"),
            ));
        }
    };
    let size: i64 = row.get(1)?;
    Ok(BlobRecord {
        id: BlobId(uuid_col(row, 0)?),
        size: u64::try_from(size).map_err(|e| bad_value(1, Type::Integer, e))?,
        status,
    })
}

// ---- column conversions ------------------------------------------------------

fn bad_value(
    idx: usize,
    ty: Type,
    err: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, ty, err.into())
}

fn uuid_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<Uuid> {
    let text: String = row.get(idx)?;
    Uuid::parse_str(&text).map_err(|e| bad_value(idx, Type::Text, e))
}

fn seq_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<Seq> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value)
        .map(Seq)
        .map_err(|e| bad_value(idx, Type::Integer, e))
}

fn json_col<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    serde_json::from_str(&text).map_err(|e| bad_value(idx, Type::Text, e))
}

fn opt_json_col<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<T>> {
    let text: Option<String> = row.get(idx)?;
    text.map(|text| serde_json::from_str(&text).map_err(|e| bad_value(idx, Type::Text, e)))
        .transpose()
}

fn to_json(value: &impl Serialize) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

fn seq_sql(seq: Seq) -> Result<i64, StoreError> {
    u64_sql(seq.0)
}

fn u64_sql(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::OutOfRange(value))
}

fn path_sql(path: &Path) -> Result<&str, StoreError> {
    path.to_str()
        .ok_or_else(|| StoreError::NonUtf8Path(path.to_path_buf()))
}
