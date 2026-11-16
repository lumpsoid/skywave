//! The storage report (`GET /v1/storage`): what the data dir takes on its
//! disk, read when a device asks.
//!
//! **Sources.** The file sizes are read from the file system: the database
//! with its WAL, running shell output in `shell/`, the log file. Blob sizes
//! are the `blobs` rows: workdir blobs are those `workdir_blobs` names,
//! event blobs every other. A session's share is its events' stored JSON
//! and the blobs `event_blobs` (written with each append) ties to them; a
//! shell run's is its `ShellOutput` events and its output, the finished
//! run's blob or the running one's file. A deleted event blob has no
//! `blobs` row, so it counts nowhere; its `event_blobs` row stays. A
//! cropped one's replacement has an `event_blobs` row of its own, with the
//! seq, session and holds of the event that referenced the original, so it
//! counts where the original did.
//!
//! **Cost.** Summing a session's events reads every event, so a report
//! costs the log's length. It runs on a connection of its own, one at a
//! time, in one read transaction (a consistent picture, and appends go on
//! beside it under WAL), and SQLite stops it at the request's deadline
//! ([`Error::StorageDeadline`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode};
use skywave_proto::{
    BlobId, BlobStorage, BlobUse, CommandId, SessionId, SessionStorage, ShellRunStorage,
    StorageReport, limits,
};
use uuid::Uuid;

use crate::agent::mapper::cut_line;
use crate::data_dir::DataDir;
use crate::db::Db;
use crate::disk::Disk;
use crate::error::{Error, Result};
use crate::event_log::seq_from_sql;

/// How long a report may run, from the request's start.
pub const REPORT_DEADLINE: Duration = Duration::from_secs(8);
/// How often a report waiting for another to finish tries the lock again.
const LOCK_POLL: Duration = Duration::from_millis(10);
/// SQLite virtual machine steps between deadline checks.
const DEADLINE_STEPS: std::ffi::c_int = 10_000;

/// `event_blobs.holds` for `holds`.
pub(crate) fn holds_to_sql(holds: BlobUse) -> &'static str {
    match holds {
        BlobUse::ToolInput => "tool_input",
        BlobUse::ToolBody => "tool_body",
        BlobUse::ToolOutput => "tool_output",
        BlobUse::ApprovalInput => "approval_input",
        BlobUse::ShellOutput => "shell_output",
        BlobUse::Message => "message",
        BlobUse::Attachment => "attachment",
    }
}

/// The [`BlobUse`] an `event_blobs.holds` names.
pub(crate) fn holds_from_sql(text: &str) -> Result<BlobUse> {
    Ok(match text {
        "tool_input" => BlobUse::ToolInput,
        "tool_body" => BlobUse::ToolBody,
        "tool_output" => BlobUse::ToolOutput,
        "approval_input" => BlobUse::ApprovalInput,
        "shell_output" => BlobUse::ShellOutput,
        "message" => BlobUse::Message,
        "attachment" => BlobUse::Attachment,
        other => return Err(Error::Corrupt(format!("event_blobs.holds {other:?}"))),
    })
}

/// Reads storage reports over its own database connection, one at a time.
/// See the module docs.
pub struct StorageReader {
    db: Mutex<Db>,
    dir: DataDir,
    log_file: Option<PathBuf>,
}

impl StorageReader {
    /// A reader over `db`, a connection of its own to `dir`'s database;
    /// `log_file` is where the server logs, `None` for stderr.
    pub fn new(db: Db, dir: DataDir, log_file: Option<PathBuf>) -> StorageReader {
        StorageReader {
            db: Mutex::new(db),
            dir,
            log_file,
        }
    }

    /// The report as of now, free space read from `disk`. Blocking.
    ///
    /// [`Error::StorageDeadline`] when another report still holds the
    /// connection at `deadline`, or this one runs past it.
    pub fn report(&self, disk: &Disk, deadline: Instant) -> Result<StorageReport> {
        let mut db = loop {
            match self.db.try_lock() {
                Ok(db) => break db,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Error::StorageDeadline);
                    }
                    std::thread::sleep(LOCK_POLL);
                }
            }
        };
        if Instant::now() >= deadline {
            return Err(Error::StorageDeadline);
        }
        let conn = db.conn_mut();
        conn.progress_handler(DEADLINE_STEPS, Some(move || Instant::now() >= deadline))?;
        let read = read(conn, &self.dir);
        conn.progress_handler(DEADLINE_STEPS, None::<fn() -> bool>)?;
        let mut report = read.map_err(|e| match e {
            Error::Sqlite(rusqlite::Error::SqliteFailure(f, _))
                if f.code == ErrorCode::OperationInterrupted =>
            {
                Error::StorageDeadline
            }
            e => e,
        })?;
        report.log_file = self.log_file.as_deref().map(file_size);
        report.free = disk.free();
        report.min_free = disk.floor();
        Ok(report)
    }
}

/// Everything but the log file and the free space, in one read
/// transaction.
fn read(conn: &mut Connection, dir: &DataDir) -> Result<StorageReport> {
    let tx = conn.transaction()?;
    let db_path = dir.db_path();
    let database = file_size(&db_path) + file_size(&with_suffix(&db_path, "-wal"));
    let (event_blobs, workdir_blobs): (i64, i64) = tx.query_row(
        "SELECT COALESCE(SUM(size) FILTER (WHERE w.blob_id IS NULL), 0),
                COALESCE(SUM(size) FILTER (WHERE w.blob_id IS NOT NULL), 0)
         FROM blobs LEFT JOIN (SELECT DISTINCT blob_id FROM workdir_blobs) w
           ON w.blob_id = blobs.id",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let shell_dir = dir.shell_dir();
    let running = running_output(&shell_dir);

    let mut sessions: HashMap<String, SessionStorage> = HashMap::new();
    let mut stmt = tx.prepare(
        "SELECT session_id, COUNT(*), SUM(LENGTH(CAST(kind AS BLOB)))
         FROM events WHERE session_id IN (SELECT id FROM sessions WHERE deletion IS NULL)
         GROUP BY session_id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let storage = SessionStorage {
            session: session_id(&id)?,
            events: count(row.get(1)?),
            event_bytes: count(row.get(2)?),
            blobs: 0,
            blob_bytes: 0,
        };
        sessions.insert(id, storage);
    }
    drop(rows);
    drop(stmt);
    let mut stmt = tx.prepare(
        "SELECT e.session_id, COUNT(*), SUM(b.size)
         FROM event_blobs e JOIN blobs b ON b.id = e.blob_id GROUP BY e.session_id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        // Every event blob's session has events.
        if let Some(storage) = sessions.get_mut(&id) {
            storage.blobs = count(row.get(1)?);
            storage.blob_bytes = count(row.get(2)?);
        }
    }
    drop(rows);
    drop(stmt);

    let mut blobs = Vec::new();
    let mut stmt = tx.prepare(
        "SELECT e.blob_id, b.size, e.session_id, e.seq, e.holds
         FROM event_blobs e JOIN blobs b ON b.id = e.blob_id
         ORDER BY b.size DESC, e.seq LIMIT ?1",
    )?;
    let mut rows = stmt.query([limits::STORAGE_TOP_MAX as i64])?;
    while let Some(row) = rows.next()? {
        blobs.push(BlobStorage {
            id: BlobId(uuid(&row.get::<_, String>(0)?)?),
            size: count(row.get(1)?),
            session: session_id(&row.get::<_, String>(2)?)?,
            seq: seq_from_sql(row.get(3)?)?,
            holds: holds_from_sql(&row.get::<_, String>(4)?)?,
        });
    }
    drop(rows);
    drop(stmt);

    let mut shell_runs = shell_runs(&tx)?;
    tx.commit()?;
    for run in &mut shell_runs {
        if let Some(size) = running.get(&run.run.0) {
            run.output = *size;
        }
    }
    shell_runs.sort_by(|a, b| {
        (b.event_bytes + b.output)
            .cmp(&(a.event_bytes + a.output))
            .then(a.run.0.cmp(&b.run.0))
    });
    shell_runs.truncate(limits::STORAGE_TOP_MAX);

    let mut sessions: Vec<SessionStorage> = sessions.into_values().collect();
    sessions.sort_by(|a, b| {
        (b.event_bytes + b.blob_bytes)
            .cmp(&(a.event_bytes + a.blob_bytes))
            .then(a.session.0.cmp(&b.session.0))
    });
    Ok(StorageReport {
        database,
        event_blobs: count(event_blobs),
        workdir_blobs: count(workdir_blobs),
        shell_running: running.values().sum(),
        log_file: None,
        free: None,
        min_free: 0,
        sessions,
        blobs,
        shell_runs,
    })
}

/// Every shell run with its progress events' and finished output's size.
/// The progress events are read per session through `events_session_type`.
fn shell_runs(tx: &rusqlite::Transaction<'_>) -> Result<Vec<ShellRunStorage>> {
    let mut progress: HashMap<String, u64> = HashMap::new();
    let mut stmt = tx.prepare(
        "SELECT json_extract(kind, '$.run'), SUM(LENGTH(CAST(kind AS BLOB))) FROM events
         WHERE session_id = ?1 AND json_extract(kind, '$.type') = 'shell_output'
         GROUP BY 1",
    )?;
    let sessions: Vec<String> = tx
        .prepare("SELECT DISTINCT session_id FROM shell_runs")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for session in sessions {
        let mut rows = stmt.query([&session])?;
        while let Some(row) = rows.next()? {
            let run: Option<String> = row.get(0)?;
            if let Some(run) = run {
                progress.insert(run, count(row.get(1)?));
            }
        }
    }
    drop(stmt);
    // Only a stored output: a deleted one is neither counted nor offered.
    // A cropped run has an `event_blobs` row per crop at its `ShellFinished`
    // seq, and only the last crop's blob is stored, so the stored one is
    // picked before the join: joining the rows would list the run once each.
    let mut stmt = tx.prepare(
        "SELECT r.command_id, r.session_id, json_extract(e.kind, '$.command'), o.size, o.id
         FROM shell_runs r JOIN events e ON e.seq = r.seq
         LEFT JOIN (SELECT b.id, b.size, x.seq FROM event_blobs x JOIN blobs b ON b.id = x.blob_id
                    WHERE x.holds = 'shell_output') o ON o.seq = r.finished_seq",
    )?;
    let mut rows = stmt.query([])?;
    let mut runs = Vec::new();
    while let Some(row) = rows.next()? {
        let run: String = row.get(0)?;
        let command: Option<String> = row.get(2)?;
        let output: Option<i64> = row.get(3)?;
        let output_blob: Option<String> = row.get(4)?;
        runs.push(ShellRunStorage {
            run: CommandId(uuid(&run)?),
            session: session_id(&row.get::<_, String>(1)?)?,
            command_line: cut_line(
                command.as_deref().unwrap_or_default(),
                limits::FIRST_PROMPT_MAX_CHARS,
            )
            .0,
            event_bytes: progress.get(&run).copied().unwrap_or(0),
            output: output.map_or(0, count),
            output_blob: output_blob.as_deref().map(uuid).transpose()?.map(BlobId),
        });
    }
    Ok(runs)
}

/// The size of each running shell run's output file in `shell_dir`
/// (`<run>.out`), by run id.
fn running_output(shell_dir: &Path) -> HashMap<Uuid, u64> {
    let Ok(entries) = std::fs::read_dir(shell_dir) else {
        return HashMap::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let run = Uuid::parse_str(name.to_str()?.strip_suffix(".out")?).ok()?;
            Some((run, entry.metadata().ok()?.len()))
        })
        .collect()
}

/// `path`'s size, 0 when it cannot be read (or does not exist).
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// A SQL count or sum, never negative.
fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|e| Error::Corrupt(format!("id {text:?}: {e}")))
}

fn session_id(text: &str) -> Result<SessionId> {
    uuid(text).map(SessionId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    fn reader() -> (tempfile::TempDir, StorageReader) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let db = Db::open(&storage.dir.db_path()).unwrap();
        let log = tmp.path().join("skywaved.log");
        std::fs::write(&log, [0u8; 42]).unwrap();
        (tmp, StorageReader::new(db, storage.dir, Some(log)))
    }

    #[test]
    fn an_empty_log_reports_its_files() {
        let (_tmp, reader) = reader();
        let soon = Instant::now() + REPORT_DEADLINE;
        let report = reader.report(&Disk::default(), soon).unwrap();
        assert!(report.database > 0);
        assert_eq!(report.log_file, Some(42));
        assert!(report.sessions.is_empty() && report.blobs.is_empty());
    }

    #[test]
    fn a_passed_deadline_reads_nothing() {
        let (_tmp, reader) = reader();
        assert!(matches!(
            reader.report(&Disk::default(), Instant::now()),
            Err(Error::StorageDeadline)
        ));
    }

    #[test]
    fn a_report_waits_for_the_one_running_only_until_its_deadline() {
        let (_tmp, reader) = reader();
        let _running = reader.db.lock().unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        assert!(matches!(
            reader.report(&Disk::default(), deadline),
            Err(Error::StorageDeadline)
        ));
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn holds_round_trips_through_sql() {
        for holds in [
            BlobUse::ToolInput,
            BlobUse::ToolBody,
            BlobUse::ToolOutput,
            BlobUse::ApprovalInput,
            BlobUse::ShellOutput,
            BlobUse::Message,
            BlobUse::Attachment,
        ] {
            assert_eq!(holds_from_sql(holds_to_sql(holds)).unwrap(), holds);
            // The column's CHECK names the wire's own words.
            assert_eq!(serde_json::to_value(holds).unwrap(), holds_to_sql(holds));
        }
    }
}
