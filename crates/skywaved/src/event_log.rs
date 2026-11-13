//! The global event log (D12) and the projections command handling reads.
//!
//! Every event goes through [`append_in`], inside a write transaction begun
//! with `BEGIN IMMEDIATE`. It takes the next seq as `max(seq) + 1` under the
//! database write lock, so seqs are contiguous however many connections
//! write, and it updates the `sessions` / `approvals` / `open_calls`
//! projections in the same transaction, so they never disagree with the log.
//!
//! What a daemon start reads per session costs what is open, not the log's
//! length: a projection, or a session's newest event of one type through
//! the `events_session_type` index (`MAX(seq)` over one type, which reads
//! one index entry; an `IN` over two types would read them all). The
//! queries are named constants so a test checks their plans.
//!
//! Two writers share one [`EventLog`] per process, each on its own [`Db`]
//! connection:
//! - the HTTPS API (T06) calls [`EventLog::handle`] for commands and
//!   [`EventLog::read`] / [`EventLog::wait_after`] for `GET /v1/events`;
//! - the agent supervisor (T10) appends agent events with
//!   [`EventLog::append`] and [`EventLog::cancel_approval`]. It learns about
//!   new commands from the log itself: every accepted command is an event
//!   (`UserMessage`, `ApprovalResolved`, `TurnInterrupted`, `SessionStopped`,
//!   `SessionRenamed`, `SessionArchived`, `SessionCreated`,
//!   `PermissionModeRequested`, `ModelRequested`, `EffortRequested`,
//!   `ContextStarted`, `ContextSwitched`, `ContextMoved`; a `MoveContext`
//!   also logs the new session's `SessionCreated` right before its own
//!   event), so it follows the log with [`EventLog::subscribe`] and
//!   [`EventLog::read`] like any other reader.
//!
//! The shell runner appends the daemon's shell events with
//! [`EventLog::shell_event`] only, and follows `ShellRequested` /
//! `ShellStopRequested` in the log the same way. The `shell_runs`
//! projection tracks each run from its `ShellRequested` to its
//! `ShellFinished`.
//!
//! The wake-up notifier is in-process: only writers holding this
//! [`EventLog`] wake waiters. Admin commands never append events.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use skywave_proto::{
    AgentSettings, AgentWork, ApprovalId, BlobId, BlobUse, CallId, CommandId, ContextId, DeviceId,
    Event, EventBatch, EventKind, LogId, LogRun, ModelChoice, Question, RunId, RunWhen, RunsSince,
    Seq, SessionId, SessionState, ShellEnd, ToolCall, ToolResult, Worktree, limits,
};
use tokio::sync::watch;
use uuid::Uuid;

use crate::blobs::BlobStore;
use crate::db::Db;
use crate::disk::Disk;
use crate::error::{Error, Result};
use crate::storage;
use crate::worktree;

/// Batch size used when `GET /v1/events` names no `limit`.
pub const DEFAULT_BATCH_LIMIT: u32 = 500;
/// Largest batch [`EventLog::read`] returns, whatever the caller asks for.
pub const MAX_BATCH_LIMIT: u32 = 5000;
/// Most bytes of stored events in one batch [`EventLog::read`] returns, so a
/// response stays bounded in memory on the server and the device whatever
/// the events hold. The stored size (the kind's JSON and the row's ids and
/// time) is within a few hundred bytes per event of the wire size. An event
/// larger than this still goes, alone, so a reader always gets past it.
pub const BATCH_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Highest seq the log can hold: seqs are SQLite integers. A cursor past it
/// names no position in any log, so `GET /v1/events` refuses it.
pub const MAX_SEQ: Seq = Seq(i64::MAX as u64);

/// Handle to the global event log. Cheap to clone; share one per process.
#[derive(Debug, Clone)]
pub struct EventLog {
    inner: Arc<Inner>,
    /// Whether `RunShell` and `StopShell` are accepted ([`EventLog::with_shell`]).
    /// Beside `inner`, not in it, so the builder needs no unique `Arc`.
    shell_enabled: bool,
    /// Whether a `SetPermissionMode` may ask for bypass
    /// ([`EventLog::with_bypass`]). Beside `inner` like `shell_enabled`.
    bypass_allowed: bool,
    /// Whether the data dir's disk is low ([`EventLog::with_disk`]); shared
    /// with whatever checks it. Beside `inner` like `shell_enabled`.
    disk: Disk,
    /// Where a `DeleteBlob` removes the file of the blob it deleted, once
    /// committed ([`EventLog::with_blobs`]). Beside `inner` like
    /// `shell_enabled`.
    blobs: Option<BlobStore>,
}

#[derive(Debug)]
struct Inner {
    /// Seq of the newest committed event.
    head: watch::Sender<Seq>,
    /// Directories sessions may run in (D14), as configured.
    roots: Vec<PathBuf>,
    /// The log ([`Db::log_id`]) and this run of it.
    log: LogId,
    run: RunId,
    /// The server stops once its running work ends
    /// ([`EventLog::set_stopping`]).
    stopping: AtomicBool,
}

/// What the server knows about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    /// Canonical path of the session's working directory.
    pub workdir: String,
    /// The agent has a running turn that has not been interrupted.
    pub turn_active: bool,
    /// The context the session runs in now.
    pub context: ContextId,
    /// The context of the running turn while `turn_active`; it differs from
    /// `context` after a switch until the old agent's turn ends.
    pub turn_context: Option<ContextId>,
}

impl SessionInfo {
    /// The context the running turn belongs to, which an `Interrupt` of it
    /// is stamped with: not the current one after a switch until the old
    /// agent reports the turn over. A running turn always has its context
    /// recorded (`SessionStateChanged(AgentWorking)` sets both, the v7
    /// migration filled it in); were it missing, the current context is the
    /// best guess.
    pub fn running_turn_context(&self) -> ContextId {
        self.turn_context.unwrap_or(self.context)
    }
}

/// An approval request no device has answered yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingApproval {
    pub id: ApprovalId,
    pub session_id: SessionId,
}

/// What [`EventLog::shell_event`] logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellAppended {
    /// The event as given, at this seq.
    Appended(Seq),
    /// A `ShellFinished { end: Stopped }` instead of the `ShellStarted`
    /// given, at this seq: a stop was requested before the run started.
    StoppedInstead(Seq),
}

/// A shell run as the log has it, before its `ShellFinished`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    /// The `RunShell` command's id.
    pub run: CommandId,
    pub session_id: SessionId,
    /// Seq of its `ShellRequested`.
    pub seq: Seq,
    pub when: RunWhen,
    /// The command, verbatim.
    pub command: String,
    /// The device that asked for it.
    pub by: DeviceId,
    /// Seq of the first `ShellStopRequested` naming it.
    pub stop_seq: Option<Seq>,
    /// Seq of its `ShellStarted`; `None` when it never started.
    pub started_seq: Option<Seq>,
    /// The `at` of its `ShellStarted`, which its `ShellFinished::started_at`
    /// copies; set exactly when `started_seq` is.
    pub started_at: Option<DateTime<Utc>>,
}

/// The shell runner's starting point ([`EventLog::unfinished_shell_runs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRecovery {
    /// The head the runs were read at.
    pub head: Seq,
    /// Every run with no `ShellFinished`, in seq order.
    pub runs: Vec<ShellRun>,
}

/// A session's shell history as a new run needs it ([`EventLog::shell_status`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellStatus {
    /// The end of the newest finished run that was not `Skipped`.
    pub last: Option<ShellEnd>,
    /// The `cwd` of the newest finished run that knew one.
    pub cwd: Option<String>,
}

impl ShellStatus {
    /// Whether the last status is success, which a session with no such run
    /// counts as.
    pub fn succeeded(&self) -> bool {
        self.last.as_ref().is_none_or(ShellEnd::is_success)
    }
}

/// `ShellFinished::command_line` for `command`: its first line, cut to
/// [`limits::FIRST_PROMPT_MAX_CHARS`] chars.
pub fn shell_command_line(command: &str) -> String {
    let line = command.trim_start().lines().next().unwrap_or("").trim_end();
    line.chars().take(limits::FIRST_PROMPT_MAX_CHARS).collect()
}

impl EventLog {
    /// A log over the database `db` belongs to; `roots` are the configured
    /// session roots (`Config::roots`). Each open is a new run of the log
    /// ([`EventLog::run`]), recorded in `log_runs` with the head it starts
    /// from: open it once per process, before anything appends.
    pub fn open(db: &Db, roots: Vec<PathBuf>) -> Result<EventLog> {
        let log = db.log_id()?;
        let run = RunId::new();
        // One statement: the head and the record of the run that starts
        // from it cannot fall apart.
        let start: i64 = db.conn().query_row(
            "INSERT INTO log_runs (run_id, start)
             SELECT ?1, COALESCE(MAX(seq), 0) FROM events
             RETURNING start",
            [run.0.to_string()],
            |r| r.get(0),
        )?;
        let (head, _) = watch::channel(seq_from_sql(start)?);
        Ok(EventLog {
            inner: Arc::new(Inner {
                head,
                roots,
                log,
                run,
                stopping: AtomicBool::new(false),
            }),
            shell_enabled: false,
            bypass_allowed: false,
            disk: Disk::default(),
            blobs: None,
        })
    }

    /// This log removing a deleted blob's file from `blobs` once the
    /// `DeleteBlob` that deleted it commits. [`EventLog::open`] has no
    /// store: its deletes remove the row only, and the file is swept at the
    /// next start ([`BlobStore::sweep`]). The API sets the store it serves
    /// blobs from. Set it before cloning, like [`EventLog::with_shell`].
    pub fn with_blobs(self, blobs: BlobStore) -> EventLog {
        EventLog {
            blobs: Some(blobs),
            ..self
        }
    }

    /// The store deleted blobs' files are removed from, if this log has one.
    pub(crate) fn blobs(&self) -> Option<&BlobStore> {
        self.blobs.as_ref()
    }

    /// This log watching `disk`: while it is low, commands that start new
    /// work are refused `DiskLow` and the supervisor pauses its agents.
    /// [`EventLog::open`] has a disk that is never low. Set it before
    /// cloning, like [`EventLog::with_shell`].
    pub fn with_disk(self, disk: Disk) -> EventLog {
        EventLog { disk, ..self }
    }

    /// The disk this log watches.
    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    /// This log with shell commands on or off; [`EventLog::open`] has them
    /// off, and `RunShell` / `StopShell` are then refused `ShellDisabled`.
    /// Set it before cloning: clones made earlier keep their own flag.
    pub fn with_shell(self, enabled: bool) -> EventLog {
        EventLog {
            shell_enabled: enabled,
            ..self
        }
    }

    /// Whether this log accepts shell commands.
    pub fn shell_enabled(&self) -> bool {
        self.shell_enabled
    }

    /// This log with bypass mode allowed or not (`agent.allow_bypass`);
    /// [`EventLog::open`] does not allow it, and a `SetPermissionMode`
    /// asking for [`PermissionMode::Bypass`](skywave_proto::PermissionMode)
    /// is then refused `BypassDisabled`. Set it before cloning, like
    /// [`EventLog::with_shell`].
    pub fn with_bypass(self, allowed: bool) -> EventLog {
        EventLog {
            bypass_allowed: allowed,
            ..self
        }
    }

    /// Whether this log accepts a switch to bypass mode.
    pub fn bypass_allowed(&self) -> bool {
        self.bypass_allowed
    }

    /// The log and this run of it, as `hello` reports them to a device
    /// that last saw run `since`. Reads the database, not the head watch.
    pub fn run(&self, db: &Db, since: Option<RunId>) -> Result<LogRun> {
        let since = match since {
            None => None,
            Some(since) => Some(runs_since_in(db.conn(), since)?),
        };
        Ok(LogRun {
            log: self.inner.log,
            run: self.inner.run,
            since,
        })
    }

    /// This run of the server, minted when the log was opened.
    pub fn run_id(&self) -> RunId {
        self.inner.run
    }

    /// Marks the server as stopping once its running work ends, or no
    /// longer: while it is, commands that start new work are refused
    /// `ServerStopping`. Shared by every clone. A command checks it inside
    /// its write transaction, so once this returns, a writer that takes the
    /// write lock after it sees no command accepted under the old value.
    pub fn set_stopping(&self, stopping: bool) {
        self.inner.stopping.store(stopping, Ordering::SeqCst);
    }

    /// Whether the server is stopping once its running work ends.
    pub fn stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    /// Seq of the newest event; [`Seq::ZERO`] when the log is empty.
    pub fn head(&self) -> Seq {
        *self.inner.head.borrow()
    }

    /// Watches the head seq. It changes after every committed append.
    pub fn subscribe(&self) -> watch::Receiver<Seq> {
        self.inner.head.subscribe()
    }

    /// Waits until the head passes `after` or `timeout` elapses, and returns
    /// the head. Returns at once when events after `after` already exist.
    /// The result is `<= after` only on timeout.
    pub async fn wait_after(&self, after: Seq, timeout: Duration) -> Seq {
        let mut rx = self.subscribe();
        // `wait_for` fails only if the sender is dropped, which `self` prevents.
        let _ = tokio::time::timeout(timeout, async {
            rx.wait_for(|head| *head > after).await.map(|_| ())
        })
        .await;
        self.head()
    }

    /// Events with seq greater than `after`, at most `limit` of them (capped
    /// at [`MAX_BATCH_LIMIT`], at least 1), and only as many as fit in
    /// [`BATCH_MAX_BYTES`], but always the first. A batch is a contiguous
    /// run from `after + 1`; an empty one has `from = after + 1`.
    pub fn read(&self, db: &Db, after: Seq, limit: u32) -> Result<EventBatch> {
        let limit = limit.clamp(1, MAX_BATCH_LIMIT);
        let mut stmt = db.conn().prepare_cached(
            "SELECT seq, session_id, at, command_id, context, kind FROM events
             WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![seq_to_sql(after), limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let from = after.next();
        let mut events = Vec::new();
        let mut bytes = 0;
        for row in rows {
            let (seq, session_id, at, command_id, context, kind) = row?;
            let size = session_id.len()
                + at.len()
                + command_id.as_ref().map_or(0, String::len)
                + context.len()
                + kind.len();
            bytes += size;
            if !events.is_empty() && bytes > BATCH_MAX_BYTES {
                break;
            }
            let expected = from.0 + events.len() as u64;
            if seq_from_sql(seq)? != Seq(expected) {
                return Err(Error::Corrupt(format!(
                    "event log gap: expected seq {expected}, found {seq}"
                )));
            }
            events.push(Event {
                session_id: parse_id(&session_id)?,
                at: parse_time(&at)?,
                command_id: command_id.as_deref().map(parse_id).transpose()?,
                context: parse_id(&context)?,
                kind: serde_json::from_str(&kind)?,
            });
        }
        Ok(EventBatch { from, events })
    }

    /// Appends an agent-produced event to `session_id` and returns its seq.
    ///
    /// Accepts the kinds the agent supervisor owns: `AgentMessage`,
    /// `ToolCall`, `ToolResult`, `ApprovalRequested` (with a new id),
    /// `SessionStateChanged`, `TurnUsage`, the agent's answers about its
    /// mode, model and effort, and `ModelsOffered`. Kinds that only
    /// commands produce, the shell events (use [`EventLog::shell_event`]),
    /// and `ApprovalCancelled` (use [`EventLog::cancel_approval`]), are
    /// refused with
    /// [`Error::InvalidAppend`], as is an unknown session or a `context`
    /// that is not one of the session's.
    ///
    /// The event is stamped with `context`: the context of the agent
    /// process that produced it, or of the command the supervisor answers
    /// with no process (architecture.md "Server"). Nothing defaults to the current context.
    pub fn append(
        &self,
        db: &mut Db,
        session_id: SessionId,
        context: ContextId,
        kind: EventKind,
    ) -> Result<Seq> {
        match &kind {
            EventKind::AgentMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
            | EventKind::ApprovalRequested { .. }
            | EventKind::QuestionAsked { .. }
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_)
            | EventKind::SubagentUsage(_)
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::AgentVersionUnconfirmed { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
            | EventKind::BackgroundTasksChanged { .. }
            | EventKind::SchedulesChanged { .. }
            | EventKind::TaskStopRefused { .. }
            | EventKind::CallsUnanswered { .. } => {}
            EventKind::SessionCreated { .. }
            | EventKind::UserMessage { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::QuestionAnswered { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. }
            | EventKind::WorktreeRemoved { .. }
            | EventKind::PermissionModeRequested { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::TaskStopRequested { .. }
            // The shell runner, not the agent, logs the daemon's shell
            // events.
            | EventKind::ShellRequested { .. }
            | EventKind::ShellStopRequested { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::ShellFinished { .. }
            // The adopting `CreateSession` logs the import, not the agent.
            | EventKind::Imported { .. }
            | EventKind::ImportFinished { .. }
            | EventKind::BlobDeleted { .. }
            | EventKind::BlobCropped { .. } => {
                return Err(Error::InvalidAppend(format!(
                    "{} is not an agent event",
                    kind_name(&kind)
                )));
            }
        }
        let tx = begin(db)?;
        if session_in(&tx, session_id)?.is_none() {
            return Err(Error::InvalidAppend(format!(
                "unknown session {}",
                session_id.0
            )));
        }
        if context_session_in(&tx, context)? != Some(session_id) {
            return Err(Error::InvalidAppend(format!(
                "context {} is not a context of session {}",
                context.0, session_id.0
            )));
        }
        if let EventKind::ApprovalRequested { id, .. } | EventKind::QuestionAsked { id, .. } = &kind
            && approval_in(&tx, *id)?.is_some()
        {
            return Err(Error::InvalidAppend(format!(
                "approval {} was already requested",
                id.0
            )));
        }
        let seq = append_in(&tx, session_id, context, None, &kind)?;
        tx.commit()?;
        self.published(seq);
        Ok(seq)
    }

    /// Appends `ApprovalCancelled { id, reason }` if approval `id` is still
    /// pending, and returns its seq. `None` when the approval is unknown or
    /// already resolved or cancelled: an answer that committed first wins
    /// (D5), so the supervisor treats `None` as "nothing to cancel". The
    /// event is stamped with the approval's context.
    pub fn cancel_approval(
        &self,
        db: &mut Db,
        id: ApprovalId,
        reason: &str,
    ) -> Result<Option<Seq>> {
        let tx = begin(db)?;
        let Some(ApprovalRow {
            session_id,
            context,
            closed: None,
            ..
        }) = approval_in(&tx, id)?
        else {
            return Ok(None);
        };
        let kind = EventKind::ApprovalCancelled {
            id,
            reason: reason.to_owned(),
        };
        let seq = append_in(&tx, session_id, context, None, &kind)?;
        tx.commit()?;
        self.published(seq);
        Ok(Some(seq))
    }

    /// Appends a daemon shell event (`ShellStarted`, `ShellOutput`,
    /// `ShellFinished`) for the run it names, the shell runner's only way
    /// into the log. One transaction reads the run's session and that
    /// session's current context, which stamps the event, checks the run and
    /// appends, so no `ClearContext` can slip in between.
    ///
    /// A `ShellStarted` for a run a `ShellStopRequested` already names is
    /// not logged: its `ShellFinished { end: Stopped }` is, and the return
    /// says so; the runner must then not start (or must kill) the shell.
    ///
    /// Refused with [`Error::InvalidAppend`]: any other kind, a run no
    /// `RunShell` logged, a run that is over (has its `ShellFinished`), and a
    /// second `ShellStarted`.
    pub fn shell_event(&self, db: &mut Db, kind: EventKind) -> Result<ShellAppended> {
        let run = match &kind {
            EventKind::ShellStarted { run, .. }
            | EventKind::ShellOutput { run, .. }
            | EventKind::ShellFinished { run, .. } => *run,
            _ => {
                return Err(Error::InvalidAppend(format!(
                    "{} is not a daemon shell event",
                    kind_name(&kind)
                )));
            }
        };
        let tx = begin(db)?;
        let Some(row) = shell_run_in(&tx, run)? else {
            return Err(Error::InvalidAppend(format!("unknown shell run {}", run.0)));
        };
        if row.finished_seq.is_some() {
            return Err(Error::InvalidAppend(format!("shell run {} is over", run.0)));
        }
        let started = matches!(kind, EventKind::ShellStarted { .. });
        if started && row.run.started_seq.is_some() {
            return Err(Error::InvalidAppend(format!(
                "shell run {} already started",
                run.0
            )));
        }
        let Some(session) = session_in(&tx, row.run.session_id)? else {
            return Err(Error::Corrupt(format!(
                "shell run {} of unknown session {}",
                run.0, row.run.session_id.0
            )));
        };
        let stopped = started && row.run.stop_seq.is_some();
        let kind = if stopped {
            EventKind::ShellFinished {
                run,
                by: row.run.by,
                started_at: None,
                command_line: shell_command_line(&row.run.command),
                end: ShellEnd::Stopped,
                cwd: None,
                preview: String::new(),
                lines: 0,
                output: None,
                truncated: false,
            }
        } else {
            kind
        };
        let seq = append_in(&tx, row.run.session_id, session.context, None, &kind)?;
        tx.commit()?;
        self.published(seq);
        Ok(if stopped {
            ShellAppended::StoppedInstead(seq)
        } else {
            ShellAppended::Appended(seq)
        })
    }

    /// Every shell run not yet over, oldest first, and the head, read in one
    /// transaction: the shell runner's starting point after a daemon start.
    /// It follows the log from `head` on.
    pub fn unfinished_shell_runs(&self, db: &Db) -> Result<ShellRecovery> {
        let tx = db.conn().unchecked_transaction()?;
        let head = head_in(&tx)?;
        let mut stmt = tx.prepare_cached(&unfinished_runs_sql())?;
        let runs = stmt
            .query_map([], ShellRunSql::from_row)?
            .map(|row| Ok(row?.parse()?.run))
            .collect::<Result<Vec<_>>>()?;
        drop(stmt);
        tx.finish()?;
        Ok(ShellRecovery { head, runs })
    }

    /// Session `id`'s last shell status and last known cwd as its finished
    /// runs have them; nothing for an unknown session or one with none.
    pub fn shell_status(&self, db: &Db, id: SessionId) -> Result<ShellStatus> {
        let tx = db.conn().unchecked_transaction()?;
        let session = id.0.to_string();
        let last: Option<String> = tx
            .prepare_cached(
                "SELECT run_end FROM shell_runs
                 WHERE session_id = ?1 AND finished_seq IS NOT NULL
                   AND json_extract(run_end, '$.kind') <> 'skipped'
                 ORDER BY finished_seq DESC LIMIT 1",
            )?
            .query_row([&session], |r| r.get(0))
            .optional()?;
        let cwd: Option<String> = tx
            .prepare_cached(
                "SELECT cwd FROM shell_runs
                 WHERE session_id = ?1 AND finished_seq IS NOT NULL AND cwd IS NOT NULL
                 ORDER BY finished_seq DESC LIMIT 1",
            )?
            .query_row([&session], |r| r.get(0))
            .optional()?;
        tx.finish()?;
        Ok(ShellStatus {
            last: last.as_deref().map(serde_json::from_str).transpose()?,
            cwd,
        })
    }

    /// Every approval still waiting for an answer, oldest first. On daemon
    /// start the supervisor cancels each one (architecture.md "Server").
    pub fn pending_approvals(&self, db: &Db) -> Result<Vec<PendingApproval>> {
        let mut stmt = db.conn().prepare_cached(
            "SELECT id, session_id FROM approvals WHERE closed_seq IS NULL ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, session_id) = row?;
            Ok(PendingApproval {
                id: parse_id(&id)?,
                session_id: parse_id(&session_id)?,
            })
        })
        .collect()
    }

    /// The session `id`, if it exists.
    pub fn session(&self, db: &Db, id: SessionId) -> Result<Option<SessionInfo>> {
        session_in(db.conn(), id)
    }

    /// The git worktree session `id` runs in, as its `SessionCreated` names
    /// it; `None` for a session that is not in one, or an unknown session.
    pub fn worktree_of(&self, db: &Db, id: SessionId) -> Result<Option<Worktree>> {
        worktree_in(db.conn(), id)
    }

    /// Whether session `id` has logged a `WorktreeRemoved`: its worktree was
    /// removed by a command already, whatever is in that directory now.
    pub fn worktree_removed(&self, db: &Db, id: SessionId) -> Result<bool> {
        worktree_removed_in(db.conn(), id)
    }

    /// Whether a session in `workdir` has work the supervisor has not seen
    /// through: a running turn, a message handed to its agent whose turn has
    /// not started, or a message the supervisor has not handled yet.
    pub fn workdir_turn_busy(&self, db: &Db, workdir: &str) -> Result<bool> {
        workdir_turn_busy_in(db.conn(), workdir)
    }

    /// Whether a session in `workdir` has a shell command queued or running
    /// (a `RunShell` with no `ShellFinished` yet).
    pub fn workdir_shell_busy(&self, db: &Db, workdir: &str) -> Result<bool> {
        workdir_shell_busy_in(db.conn(), workdir)
    }

    /// Every session, oldest first.
    pub fn sessions(&self, db: &Db) -> Result<Vec<SessionInfo>> {
        let mut stmt = db.conn().prepare_cached(
            "SELECT id, workdir, turn_active, current_context, turn_context
             FROM sessions ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, SessionRow::from_row(r, 1)?))
        })?;
        rows.map(|row| {
            let (id, session) = row?;
            session.info(parse_id(&id)?)
        })
        .collect()
    }

    /// Session `id`'s permission mode and model as its log has them; the
    /// default when it has none or is unknown.
    pub fn agent_settings(&self, db: &Db, id: SessionId) -> Result<AgentSettings> {
        agent_settings_in(db.conn(), id)
    }

    /// Session `id`'s agent's background tasks and schedules as its log has
    /// them; empty when it has none or is unknown.
    pub fn agent_work(&self, db: &Db, id: SessionId) -> Result<AgentWork> {
        agent_work_in(db.conn(), id)
    }

    /// Session `id`'s calls still open in its log, grouped by the context
    /// each was logged in, in log order (contexts by their first open call).
    /// A logged `ToolCall` is open until a `ToolResult` links to it or a
    /// `CallsUnanswered` names it, the mapper's definition of answered. On
    /// daemon start the supervisor names them (architecture.md "Server").
    /// Read from the `open_calls` projection, so it costs the calls open.
    pub fn open_calls(&self, db: &Db, id: SessionId) -> Result<Vec<(ContextId, Vec<CallId>)>> {
        let mut stmt = db.conn().prepare_cached(OPEN_CALLS_SQL)?;
        let rows = stmt.query_map([id.0.to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut open: Vec<(ContextId, Vec<CallId>)> = Vec::new();
        for row in rows {
            let (call, context) = row?;
            let (call, context) = (parse_id(&call)?, parse_id(&context)?);
            match open.iter_mut().find(|(c, _)| *c == context) {
                Some((_, calls)) => calls.push(call),
                None => open.push((context, vec![call])),
            }
        }
        Ok(open)
    }

    /// Seq of the event command `id` logged, if it was accepted.
    pub fn command_seq(&self, db: &Db, id: CommandId) -> Result<Option<Seq>> {
        let seq: Option<i64> = db
            .conn()
            .prepare_cached(COMMAND_SEQ_SQL)?
            .query_row([id.0.to_string()], |r| r.get(0))
            .optional()?;
        seq.map(seq_from_sql).transpose()
    }

    /// The models of the newest `ModelsOffered` in any session, if any.
    pub fn offered_models(&self, db: &Db) -> Result<Option<Vec<ModelChoice>>> {
        let kind: Option<String> = db
            .conn()
            .prepare_cached(OFFERED_MODELS_SQL)?
            .query_row([], |r| r.get(0))
            .optional()?;
        match kind.as_deref().map(serde_json::from_str).transpose()? {
            Some(EventKind::ModelsOffered { models }) => Ok(Some(models)),
            _ => Ok(None),
        }
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.inner.roots
    }

    /// Wakes waiters after a commit that appended up to `seq`. Writers on
    /// other connections can commit out of order with their notifications,
    /// so the head only moves forward.
    pub(crate) fn published(&self, seq: Seq) {
        self.inner.head.send_if_modified(|head| {
            let newer = seq > *head;
            if newer {
                *head = seq;
            }
            newer
        });
    }
}

/// Begins a write transaction holding the database write lock, so seq
/// assignment and validation see no concurrent writer.
pub(crate) fn begin(db: &mut Db) -> Result<rusqlite::Transaction<'_>> {
    Ok(db
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?)
}

/// Appends one event at the next seq and updates the projections. Must run
/// inside a transaction from [`begin`]; preconditions are the caller's.
/// `context` is the context of the session the event belongs to
/// ([`Event::context`]); `command` is the command logging it
/// ([`Event::command_id`]), `None` for agent and daemon events.
pub(crate) fn append_in(
    conn: &Connection,
    session_id: SessionId,
    context: ContextId,
    command: Option<CommandId>,
    kind: &EventKind,
) -> Result<Seq> {
    let seq = head_in(conn)?.next();
    let sql_seq = seq_to_sql(seq);
    let session = session_id.0.to_string();
    let context = context.0.to_string();
    let at = Utc::now().to_rfc3339_opts(SecondsFormat::AutoSi, true);
    conn.prepare_cached(
        "INSERT INTO events (seq, session_id, at, command_id, context, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        sql_seq,
        session,
        at,
        command.map(|id| id.0.to_string()),
        context,
        serde_json::to_string(kind)?
    ])?;
    // Which session's event holds each blob, for the storage report. An
    // event blob is written for one event only; a repeat keeps the first.
    // A crop's new blob stands for the event that held the old one: its
    // arm below writes it, since a row here would win the `OR IGNORE`.
    let blobs = match kind {
        EventKind::BlobCropped { .. } => None,
        _ => Some(kind.blobs()),
    };
    for (holds, blob) in blobs.into_iter().flatten() {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO event_blobs (blob_id, session_id, seq, holds)
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            blob.id.0.to_string(),
            session,
            sql_seq,
            storage::holds_to_sql(holds)
        ])?;
    }

    match kind {
        // The sessions row first: the contexts row refers to it. A moved
        // context's new session, or an adopting one, keeps what its first
        // context continues as `origin`, the `SessionOrigin` as JSON.
        EventKind::SessionCreated { workdir, from, .. } => {
            conn.execute(
                "INSERT INTO sessions (id, workdir, current_context) VALUES (?1, ?2, ?3)",
                params![session, workdir, context],
            )?;
            let origin = from.as_ref().map(serde_json::to_string).transpose()?;
            conn.execute(
                "INSERT INTO contexts (id, session_id, origin) VALUES (?1, ?2, ?3)",
                params![context, session, origin],
            )?;
        }
        // Logged after the new session's `SessionCreated`, so its row exists.
        EventKind::ContextMoved { to, .. } => {
            conn.execute(
                "UPDATE contexts SET moved_to = ?2 WHERE id = ?1",
                params![context, to.0.to_string()],
            )?;
        }
        EventKind::ContextStarted { .. } => {
            conn.execute(
                "INSERT INTO contexts (id, session_id) VALUES (?1, ?2)",
                params![context, session],
            )?;
            conn.execute(
                "UPDATE sessions SET current_context = ?2 WHERE id = ?1",
                params![session, context],
            )?;
        }
        EventKind::ContextSwitched { .. } => {
            conn.execute(
                "UPDATE sessions SET current_context = ?2 WHERE id = ?1",
                params![session, context],
            )?;
        }
        EventKind::SessionStateChanged(state) => {
            let active = matches!(state, SessionState::AgentWorking);
            conn.execute(
                "UPDATE sessions SET turn_active = ?2, turn_context = ?3 WHERE id = ?1",
                params![session, active, active.then_some(&context)],
            )?;
        }
        EventKind::TurnInterrupted => {
            conn.execute(
                "UPDATE sessions SET turn_active = 0, turn_context = NULL WHERE id = ?1",
                [&session],
            )?;
        }
        EventKind::ApprovalRequested {
            id, suggestions, ..
        } => {
            conn.execute(
                "INSERT INTO approvals (id, session_id, context, rules) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id.0.to_string(),
                    session,
                    context,
                    i64::try_from(suggestions.len()).unwrap_or(i64::MAX)
                ],
            )?;
        }
        EventKind::QuestionAsked { id, questions, .. } => {
            conn.execute(
                "INSERT INTO approvals (id, session_id, context, questions) VALUES (?1, ?2, ?3, ?4)",
                params![id.0.to_string(), session, context, serde_json::to_string(questions)?],
            )?;
        }
        EventKind::ApprovalResolved { id, .. }
        | EventKind::QuestionAnswered { id, .. }
        | EventKind::ApprovalCancelled { id, .. } => {
            conn.execute(
                "UPDATE approvals SET closed_seq = ?2 WHERE id = ?1 AND closed_seq IS NULL",
                params![id.0.to_string(), sql_seq],
            )?;
        }
        EventKind::PermissionModeRequested { .. }
        | EventKind::PermissionModeChanged { .. }
        | EventKind::PermissionModeRefused { .. }
        | EventKind::ModelRequested { .. }
        | EventKind::ModelChanged { .. }
        | EventKind::ModelRefused { .. }
        | EventKind::EffortRequested { .. }
        | EventKind::EffortChanged { .. }
        | EventKind::EffortRefused { .. } => {
            let mut settings = agent_settings_in(conn, session_id)?;
            settings.apply(command, kind);
            conn.execute(
                "UPDATE sessions SET agent_settings = ?2 WHERE id = ?1",
                params![session, serde_json::to_string(&settings)?],
            )?;
        }
        EventKind::BackgroundTasksChanged { .. } | EventKind::SchedulesChanged { .. } => {
            let mut work = agent_work_in(conn, session_id)?;
            work.apply(kind);
            conn.execute(
                "UPDATE sessions SET agent_work = ?2 WHERE id = ?1",
                params![session, serde_json::to_string(&work)?],
            )?;
        }
        // A stopped turn ends with the state change the agent reports. No
        // archived flag is kept: nothing on the server reads it, clients
        // fold `EventKind::archived_effect` themselves.
        // A call is open from its `ToolCall` until its session answers it.
        // A repeated `ToolCall` keeps the first; an answer logged before
        // its call (no mapper does) closes nothing.
        EventKind::ToolCall(ToolCall {
            call: Some(call), ..
        }) => {
            conn.prepare_cached(
                "INSERT OR IGNORE INTO open_calls (session_id, call, context, seq)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![session, call.0.to_string(), context, sql_seq])?;
        }
        EventKind::ToolResult(ToolResult {
            call: Some(call), ..
        }) => close_call(conn, &session, *call)?,
        EventKind::CallsUnanswered { calls } => {
            for call in calls {
                close_call(conn, &session, *call)?;
            }
        }
        EventKind::UserMessage { .. }
        | EventKind::AgentMessage { .. }
        | EventKind::ToolCall(_)
        | EventKind::ToolResult(_)
        | EventKind::SessionStopped
        | EventKind::SessionRenamed { .. }
        | EventKind::SessionArchived { .. }
        | EventKind::WorktreeRemoved { .. }
        | EventKind::TurnUsage(_)
        | EventKind::SubagentUsage(_)
        | EventKind::ModelsOffered { .. }
        | EventKind::AgentVersionUnconfirmed { .. }
        | EventKind::TaskStopRequested { .. }
        | EventKind::TaskStopRefused { .. }
        | EventKind::ShellOutput { .. } => {}
        // Imported history is a record: an imported call opens nothing.
        EventKind::Imported { .. } | EventKind::ImportFinished { .. } => {}
        // `DeleteBlob` removed the `blobs` row before appending it. The
        // blob's `event_blobs` row stays, naming the event that referenced
        // it: how a later command tells `BlobGone` from `UnknownBlob`.
        EventKind::BlobDeleted { .. } => {}
        // `CropBlob` recorded `into` and removed `blob`'s `blobs` row before
        // appending it. `into` takes `blob`'s `event_blobs` row's session,
        // seq and holds: it is the content of the event that referenced
        // `blob`, so the storage report (a shell run's output, found by its
        // `ShellFinished` seq) and a later crop or delete find it there.
        EventKind::BlobCropped { blob, into, .. } => {
            let rows = conn
                .prepare_cached(
                    "INSERT INTO event_blobs (blob_id, session_id, seq, holds)
                     SELECT ?2, session_id, seq, holds FROM event_blobs WHERE blob_id = ?1",
                )?
                .execute(params![blob.0.to_string(), into.id.0.to_string()])?;
            if rows != 1 {
                return Err(Error::InvalidAppend(format!(
                    "BlobCropped names blob {}, which no event referenced",
                    blob.0
                )));
            }
        }
        // The run's id is the command's: a `ShellRequested` naming none
        // has no run to track.
        EventKind::ShellRequested { when, .. } => {
            let Some(run) = command else {
                return Err(Error::InvalidAppend(
                    "ShellRequested names no command".to_owned(),
                ));
            };
            conn.prepare_cached(
                "INSERT INTO shell_runs (command_id, session_id, seq, run_when)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                run.0.to_string(),
                session,
                sql_seq,
                when_to_sql(*when)
            ])?;
        }
        // The first stop is the one that counts; a later one changes nothing.
        EventKind::ShellStopRequested { run, .. } => {
            conn.prepare_cached(
                "UPDATE shell_runs SET stop_seq = COALESCE(stop_seq, ?2) WHERE command_id = ?1",
            )?
            .execute(params![run.0.to_string(), sql_seq])?;
        }
        EventKind::ShellStarted { run, .. } => {
            conn.prepare_cached("UPDATE shell_runs SET started_seq = ?2 WHERE command_id = ?1")?
                .execute(params![run.0.to_string(), sql_seq])?;
        }
        EventKind::ShellFinished { run, end, cwd, .. } => {
            conn.prepare_cached(
                "UPDATE shell_runs SET finished_seq = ?2, run_end = ?3, cwd = ?4
                 WHERE command_id = ?1",
            )?
            .execute(params![
                run.0.to_string(),
                sql_seq,
                serde_json::to_string(end)?,
                cwd
            ])?;
        }
    }
    Ok(seq)
}

/// Removes `call` from the session's open calls, if it is one.
fn close_call(conn: &Connection, session: &str, call: CallId) -> Result<()> {
    conn.prepare_cached("DELETE FROM open_calls WHERE session_id = ?1 AND call = ?2")?
        .execute(params![session, call.0.to_string()])?;
    Ok(())
}

/// [`EventLog::open_calls`]: the session's open calls in log order.
const OPEN_CALLS_SQL: &str =
    "SELECT call, context FROM open_calls WHERE session_id = ?1 ORDER BY seq";

/// [`EventLog::command_seq`], through `events_command`.
const COMMAND_SEQ_SQL: &str = "SELECT seq FROM events WHERE command_id = ?1";

/// [`EventLog::offered_models`]: each session's newest `ModelsOffered`,
/// then the newest of those, so it reads an index entry per session.
const OFFERED_MODELS_SQL: &str = "SELECT kind FROM events WHERE seq = (
     SELECT MAX((SELECT MAX(seq) FROM events
                 WHERE session_id = sessions.id
                   AND json_extract(kind, '$.type') = 'models_offered'))
     FROM sessions)";

/// [`last_state_event_in`]: the newer of the session's newest
/// `SessionStateChanged` and newest `TurnInterrupted`. Seqs start at 1, so
/// 0 stands in for none (a two-argument `MAX` is NULL if either is).
const LAST_STATE_EVENT_SQL: &str = "SELECT kind, context FROM events WHERE seq = MAX(
     COALESCE((SELECT MAX(seq) FROM events WHERE session_id = ?1
               AND json_extract(kind, '$.type') = 'session_state_changed'), 0),
     COALESCE((SELECT MAX(seq) FROM events WHERE session_id = ?1
               AND json_extract(kind, '$.type') = 'turn_interrupted'), 0))";

/// [`last_work_context_in`], as [`LAST_STATE_EVENT_SQL`] for its types.
const LAST_WORK_CONTEXT_SQL: &str = "SELECT context FROM events WHERE seq = MAX(
     COALESCE((SELECT MAX(seq) FROM events WHERE session_id = ?1
               AND json_extract(kind, '$.type') = 'background_tasks_changed'), 0),
     COALESCE((SELECT MAX(seq) FROM events WHERE session_id = ?1
               AND json_extract(kind, '$.type') = 'schedules_changed'), 0))";

/// [`worktree_in`]: the session's `SessionCreated`, the only event that
/// names a worktree.
const WORKTREE_SQL: &str = "SELECT kind FROM events WHERE session_id = ?1
     AND json_extract(kind, '$.type') = 'session_created'";

/// [`worktree_removed_in`]: whether the session has a `WorktreeRemoved`.
const WORKTREE_REMOVED_SQL: &str = "SELECT EXISTS (SELECT 1 FROM events
     WHERE session_id = ?1 AND json_extract(kind, '$.type') = 'worktree_removed')";

/// Whether the newest worktree made at `workdir` was removed: the
/// directory is gone for every session there, until a worktree is made at that
/// path again.
const WORKDIR_WORKTREE_REMOVED_SQL: &str = "SELECT EXISTS (SELECT 1 FROM events
     WHERE session_id = (
         SELECT c.session_id FROM events c JOIN sessions s ON s.id = c.session_id
         WHERE s.workdir = ?1
           AND json_extract(c.kind, '$.type') = 'session_created'
           AND json_extract(c.kind, '$.worktree') IS NOT NULL
         ORDER BY c.seq DESC LIMIT 1)
       AND json_extract(kind, '$.type') = 'worktree_removed')";

/// [`EventLog::workdir_turn_busy`]: a session in the directory that runs a
/// turn, has a message handed to its agent (`supervisor_owed`), or has a
/// `UserMessage` past its supervisor's position: the held seq if the session
/// is held, else the cursor.
const WORKDIR_TURN_BUSY_SQL: &str = "SELECT EXISTS (SELECT 1 FROM sessions s
     WHERE s.workdir = ?1 AND (s.turn_active = 1
       OR EXISTS (SELECT 1 FROM supervisor_owed o WHERE o.session_id = s.id)
       OR EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id
             AND json_extract(e.kind, '$.type') = 'user_message'
             AND e.seq > COALESCE(
                 (SELECT h.seq FROM supervisor_held h WHERE h.session_id = s.id),
                 (SELECT c.seq FROM supervisor_cursor c WHERE c.id = 1)))))";

/// [`EventLog::workdir_shell_busy`]: a shell run of a session in the
/// directory with no `ShellFinished`, queued or running.
const WORKDIR_SHELL_BUSY_SQL: &str = "SELECT EXISTS (SELECT 1 FROM shell_runs r
     JOIN sessions s ON s.id = r.session_id
     WHERE s.workdir = ?1 AND r.finished_seq IS NULL)";

/// The context of the session's newest `BackgroundTasksChanged` or
/// `SchedulesChanged`.
pub(crate) fn last_work_context_in(
    conn: &Connection,
    session_id: SessionId,
) -> Result<Option<ContextId>> {
    let context: Option<String> = conn
        .prepare_cached(LAST_WORK_CONTEXT_SQL)?
        .query_row([session_id.0.to_string()], |r| r.get(0))
        .optional()?;
    context.map(|c| parse_id(&c)).transpose()
}

fn when_to_sql(when: RunWhen) -> &'static str {
    match when {
        RunWhen::Always => "always",
        RunWhen::OnSuccess => "on_success",
        RunWhen::OnFailure => "on_failure",
    }
}

fn when_from_sql(text: &str) -> Result<RunWhen> {
    match text {
        "always" => Ok(RunWhen::Always),
        "on_success" => Ok(RunWhen::OnSuccess),
        "on_failure" => Ok(RunWhen::OnFailure),
        other => Err(Error::Corrupt(format!("shell run when {other:?}"))),
    }
}

/// A `shell_runs` row with what its `ShellRequested` asked, as
/// [`ShellRunSql::from_row`] reads it.
const SHELL_RUN_SELECT: &str = "SELECT r.command_id, r.session_id, r.seq, r.run_when,
        json_extract(e.kind, '$.command'), json_extract(e.kind, '$.by'),
        r.stop_seq, r.started_seq, r.finished_seq, s.at
     FROM shell_runs r JOIN events e ON e.seq = r.seq
     LEFT JOIN events s ON s.seq = r.started_seq";

/// [`EventLog::unfinished_shell_runs`], through `shell_runs_unfinished`.
fn unfinished_runs_sql() -> String {
    format!("{SHELL_RUN_SELECT} WHERE r.finished_seq IS NULL ORDER BY r.seq")
}

/// A `shell_runs` row's columns before its ids are parsed.
struct ShellRunSql {
    run: String,
    session_id: String,
    seq: i64,
    when: String,
    command: String,
    by: String,
    stop_seq: Option<i64>,
    started_seq: Option<i64>,
    finished_seq: Option<i64>,
    started_at: Option<String>,
}

impl ShellRunSql {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ShellRunSql> {
        Ok(ShellRunSql {
            run: r.get(0)?,
            session_id: r.get(1)?,
            seq: r.get(2)?,
            when: r.get(3)?,
            command: r.get(4)?,
            by: r.get(5)?,
            stop_seq: r.get(6)?,
            started_seq: r.get(7)?,
            finished_seq: r.get(8)?,
            started_at: r.get(9)?,
        })
    }

    fn parse(self) -> Result<ShellRunRow> {
        Ok(ShellRunRow {
            run: ShellRun {
                run: parse_id(&self.run)?,
                session_id: parse_id(&self.session_id)?,
                seq: seq_from_sql(self.seq)?,
                when: when_from_sql(&self.when)?,
                command: self.command,
                by: parse_id(&self.by)?,
                stop_seq: self.stop_seq.map(seq_from_sql).transpose()?,
                started_seq: self.started_seq.map(seq_from_sql).transpose()?,
                started_at: self.started_at.as_deref().map(parse_time).transpose()?,
            },
            finished_seq: self.finished_seq.map(seq_from_sql).transpose()?,
        })
    }
}

/// A shell run as `shell_runs` has it, finished or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellRunRow {
    pub run: ShellRun,
    /// Seq of its `ShellFinished`; set, the run is over.
    pub finished_seq: Option<Seq>,
}

/// Shell run `id` as the log has it; `None` if no `RunShell` logged it.
pub(crate) fn shell_run_in(conn: &Connection, id: CommandId) -> Result<Option<ShellRunRow>> {
    conn.prepare_cached(&format!("{SHELL_RUN_SELECT} WHERE r.command_id = ?1"))?
        .query_row([id.0.to_string()], ShellRunSql::from_row)
        .optional()?
        .map(ShellRunSql::parse)
        .transpose()
}

fn agent_settings_in(conn: &Connection, id: SessionId) -> Result<AgentSettings> {
    let json: Option<Option<String>> = conn
        .prepare_cached("SELECT agent_settings FROM sessions WHERE id = ?1")?
        .query_row([id.0.to_string()], |r| r.get(0))
        .optional()?;
    Ok(json
        .flatten()
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

pub(crate) fn agent_work_in(conn: &Connection, id: SessionId) -> Result<AgentWork> {
    let json: Option<Option<String>> = conn
        .prepare_cached("SELECT agent_work FROM sessions WHERE id = ?1")?
        .query_row([id.0.to_string()], |r| r.get(0))
        .optional()?;
    Ok(json
        .flatten()
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

/// Seq of the newest event in `conn`'s view of the log.
/// What became of run `since` of this log ([`RunsSince`]).
fn runs_since_in(conn: &Connection, since: RunId) -> Result<RunsSince> {
    let position: Option<i64> = conn
        .query_row(
            "SELECT position FROM log_runs WHERE run_id = ?1",
            [since.0.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(position) = position else {
        return Ok(RunsSince::Unknown);
    };
    let next: Option<i64> = conn.query_row(
        "SELECT MIN(start) FROM log_runs WHERE position > ?1",
        [position],
        |r| r.get(0),
    )?;
    Ok(RunsSince::Known {
        next_start: next.map(seq_from_sql).transpose()?,
    })
}

pub(crate) fn head_in(conn: &Connection) -> Result<Seq> {
    let max: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
    seq_from_sql(max)
}

pub(crate) fn session_in(conn: &Connection, id: SessionId) -> Result<Option<SessionInfo>> {
    conn.prepare_cached(
        "SELECT workdir, turn_active, current_context, turn_context FROM sessions WHERE id = ?1",
    )?
    .query_row([id.0.to_string()], |r| SessionRow::from_row(r, 0))
    .optional()?
    .map(|row| row.info(id))
    .transpose()
}

/// [`EventLog::worktree_of`]: the worktree its `SessionCreated` names. A
/// session with no such event is unknown here, so `None`.
pub(crate) fn worktree_in(conn: &Connection, id: SessionId) -> Result<Option<Worktree>> {
    let kind: Option<String> = conn
        .prepare_cached(WORKTREE_SQL)?
        .query_row([id.0.to_string()], |r| r.get(0))
        .optional()?;
    let Some(kind) = kind else {
        return Ok(None);
    };
    let EventKind::SessionCreated { worktree, .. } = serde_json::from_str(&kind)? else {
        return Err(Error::Corrupt(format!("session_created row for {id:?}")));
    };
    Ok(worktree)
}

/// [`EventLog::worktree_removed`]: whether the session's events include a
/// `WorktreeRemoved`.
pub(crate) fn worktree_removed_in(conn: &Connection, id: SessionId) -> Result<bool> {
    let removed = conn
        .prepare_cached(WORKTREE_REMOVED_SQL)?
        .query_row([id.0.to_string()], |r| r.get(0))?;
    Ok(removed)
}

/// [`WORKDIR_WORKTREE_REMOVED_SQL`]: whether the newest worktree made at
/// `workdir` (a session's `workdir`) has a `WorktreeRemoved`.
pub(crate) fn workdir_worktree_removed_in(conn: &Connection, workdir: &str) -> Result<bool> {
    let removed = conn
        .prepare_cached(WORKDIR_WORKTREE_REMOVED_SQL)?
        .query_row([workdir], |r| r.get(0))?;
    Ok(removed)
}

/// [`EventLog::workdir_turn_busy`]: whether any session in `workdir` has
/// [`WORKDIR_TURN_BUSY_SQL`]'s work queued or running.
pub(crate) fn workdir_turn_busy_in(conn: &Connection, workdir: &str) -> Result<bool> {
    let busy = conn
        .prepare_cached(WORKDIR_TURN_BUSY_SQL)?
        .query_row([workdir], |r| r.get(0))?;
    Ok(busy)
}

/// [`EventLog::workdir_shell_busy`]: whether any session in `workdir` has a
/// shell run with no `ShellFinished`.
pub(crate) fn workdir_shell_busy_in(conn: &Connection, workdir: &str) -> Result<bool> {
    let busy = conn
        .prepare_cached(WORKDIR_SHELL_BUSY_SQL)?
        .query_row([workdir], |r| r.get(0))?;
    Ok(busy)
}

/// Records `intent`, a worktree [`worktree::create`] is about to add, for
/// `command`. The create holds [`worktree`]'s lock, so a row with the same
/// repository and branch is one whose branch is gone: replaced, and its
/// attempt's later [`forget_worktree_intent_in`] then matches nothing.
pub(crate) fn record_worktree_intent_in(
    conn: &Connection,
    intent: &worktree::Intent,
    command: CommandId,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO worktree_intents
             (repo, branch, target, start, command, attempt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            intent.repo,
            intent.branch,
            intent.target,
            intent.start,
            command.0.to_string(),
            intent.attempt,
        ],
    )?;
    Ok(())
}

/// Deletes the intent row of `branch` in `repo` if `attempt` wrote it. A row
/// a later attempt has replaced is that attempt's, and is kept.
pub(crate) fn forget_worktree_intent_in(
    conn: &Connection,
    repo: &str,
    branch: &str,
    attempt: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM worktree_intents WHERE repo = ?1 AND branch = ?2 AND attempt = ?3",
        params![repo, branch, attempt],
    )?;
    Ok(())
}

/// The session's newest `SessionStateChanged` or `TurnInterrupted`, with
/// the context it is stamped with.
pub(crate) fn last_state_event_in(
    conn: &Connection,
    session_id: SessionId,
) -> Result<Option<(EventKind, ContextId)>> {
    let row: Option<(String, String)> = conn
        .prepare_cached(LAST_STATE_EVENT_SQL)?
        .query_row([session_id.0.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((kind, context)) = row else {
        return Ok(None);
    };
    Ok(Some((serde_json::from_str(&kind)?, parse_id(&context)?)))
}

/// A `sessions` row's columns before its ids are parsed.
struct SessionRow {
    workdir: String,
    turn_active: bool,
    context: String,
    turn_context: Option<String>,
}

impl SessionRow {
    /// Reads `workdir, turn_active, current_context, turn_context` from
    /// column `first` on.
    fn from_row(r: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<SessionRow> {
        Ok(SessionRow {
            workdir: r.get(first)?,
            turn_active: r.get(first + 1)?,
            context: r.get(first + 2)?,
            turn_context: r.get(first + 3)?,
        })
    }

    fn info(self, id: SessionId) -> Result<SessionInfo> {
        Ok(SessionInfo {
            id,
            workdir: self.workdir,
            turn_active: self.turn_active,
            context: parse_id(&self.context)?,
            turn_context: self.turn_context.as_deref().map(parse_id).transpose()?,
        })
    }
}

/// The session context `id` belongs to; `None` if it is no context of any
/// session.
pub(crate) fn context_session_in(conn: &Connection, id: ContextId) -> Result<Option<SessionId>> {
    let session: Option<String> = conn
        .prepare_cached("SELECT session_id FROM contexts WHERE id = ?1")?
        .query_row([id.0.to_string()], |r| r.get(0))
        .optional()?;
    session.as_deref().map(parse_id).transpose()
}

/// What the server knows about one session context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextRow {
    pub session_id: SessionId,
    /// The session a `ContextMoved` sent it on as; set, it is never current
    /// or started here again.
    pub moved_to: Option<SessionId>,
    /// The agent stored a resume handle for it, so it has a conversation to
    /// continue. Every context the log shows with a turn has one: the
    /// adapters store the handle before `AgentWorking` is logged.
    pub has_handle: bool,
}

/// Context `id` as the server has it; `None` if it is no context of any
/// session.
pub(crate) fn context_in(conn: &Connection, id: ContextId) -> Result<Option<ContextRow>> {
    let row: Option<(String, Option<String>, bool)> = conn
        .prepare_cached(
            "SELECT session_id, moved_to, agent_state IS NOT NULL
             FROM contexts WHERE id = ?1",
        )?
        .query_row([id.0.to_string()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    row.map(|(session, moved_to, has_handle)| {
        Ok(ContextRow {
            session_id: parse_id(&session)?,
            moved_to: moved_to.as_deref().map(parse_id).transpose()?,
            has_handle,
        })
    })
    .transpose()
}

/// What the log says of one approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalRow {
    pub session_id: SessionId,
    /// The context the request was logged in.
    pub context: ContextId,
    /// How it was closed; `None` while it is pending.
    pub closed: Option<Closed>,
    /// How many rule suggestions the request offers.
    pub rules: usize,
    /// The questions of a `QuestionAsked`; `None` for a plain approval.
    pub questions: Option<Vec<Question>>,
}

/// How an approval request was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// By an `ApprovalResolved` or a `QuestionAnswered`: an answer won.
    Resolved,
    /// By an `ApprovalCancelled`: no answer ever will.
    Cancelled,
}

/// An `approvals` row as SQL has it: session, context, the type tag of
/// the event that closed it, rules, questions.
type ApprovalSql = (String, String, Option<String>, i64, Option<String>);

/// Approval `id` as the log has it; `None` if unknown.
pub(crate) fn approval_in(conn: &Connection, id: ApprovalId) -> Result<Option<ApprovalRow>> {
    // The type tag of the event at `closed_seq` tells an answer from a
    // cancel.
    let row: Option<ApprovalSql> = conn
        .query_row(
            "SELECT a.session_id, a.context, json_extract(e.kind, '$.type'), a.rules, a.questions
             FROM approvals a LEFT JOIN events e ON e.seq = a.closed_seq
             WHERE a.id = ?1",
            [id.0.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    row.map(|(session, context, closed_by, rules, questions)| {
        Ok(ApprovalRow {
            session_id: parse_id(&session)?,
            context: parse_id(&context)?,
            closed: closed_by.map(|kind| match kind.as_str() {
                "approval_cancelled" => Closed::Cancelled,
                _ => Closed::Resolved,
            }),
            rules: usize::try_from(rules).unwrap_or(0),
            questions: questions.as_deref().map(serde_json::from_str).transpose()?,
        })
    })
    .transpose()
}

/// What the log says of one event blob: the event that referenced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventBlobRow {
    pub session_id: SessionId,
    /// The context the referencing event was logged in.
    pub context: ContextId,
    pub holds: BlobUse,
    /// Whether the blob is still stored: `false` once deleted or cropped.
    pub stored: bool,
}

/// Event blob `id` as the log has it; `None` when no event referenced it
/// (a workdir blob, or an id never minted).
pub(crate) fn event_blob_in(conn: &Connection, id: BlobId) -> Result<Option<EventBlobRow>> {
    let row: Option<(String, String, String, bool)> = conn
        .query_row(
            "SELECT o.session_id, e.context, o.holds, b.id IS NOT NULL
             FROM event_blobs o JOIN events e ON e.seq = o.seq
             LEFT JOIN blobs b ON b.id = o.blob_id
             WHERE o.blob_id = ?1",
            [id.0.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    row.map(|(session, context, holds, stored)| {
        Ok(EventBlobRow {
            session_id: parse_id(&session)?,
            context: parse_id(&context)?,
            holds: storage::holds_from_sql(&holds)?,
            stored,
        })
    })
    .transpose()
}

/// Deletes event blob `id`'s `blobs` row, keeping its `event_blobs` row.
/// The file is the caller's to remove, after commit.
pub(crate) fn delete_blob_row_in(conn: &Connection, id: BlobId) -> Result<()> {
    conn.execute("DELETE FROM blobs WHERE id = ?1", [id.0.to_string()])?;
    Ok(())
}

fn kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::SessionCreated { .. } => "SessionCreated",
        EventKind::UserMessage { .. } => "UserMessage",
        EventKind::AgentMessage { .. } => "AgentMessage",
        EventKind::ToolCall { .. } => "ToolCall",
        EventKind::ToolResult { .. } => "ToolResult",
        EventKind::ApprovalRequested { .. } => "ApprovalRequested",
        EventKind::ApprovalResolved { .. } => "ApprovalResolved",
        EventKind::ApprovalCancelled { .. } => "ApprovalCancelled",
        EventKind::QuestionAsked { .. } => "QuestionAsked",
        EventKind::QuestionAnswered { .. } => "QuestionAnswered",
        EventKind::TurnInterrupted => "TurnInterrupted",
        EventKind::SessionStopped => "SessionStopped",
        EventKind::SessionRenamed { .. } => "SessionRenamed",
        EventKind::SessionArchived { .. } => "SessionArchived",
        EventKind::WorktreeRemoved { .. } => "WorktreeRemoved",
        EventKind::SessionStateChanged(_) => "SessionStateChanged",
        EventKind::TurnUsage(_) => "TurnUsage",
        EventKind::SubagentUsage(_) => "SubagentUsage",
        EventKind::PermissionModeRequested { .. } => "PermissionModeRequested",
        EventKind::PermissionModeChanged { .. } => "PermissionModeChanged",
        EventKind::PermissionModeRefused { .. } => "PermissionModeRefused",
        EventKind::ModelRequested { .. } => "ModelRequested",
        EventKind::ModelChanged { .. } => "ModelChanged",
        EventKind::ModelRefused { .. } => "ModelRefused",
        EventKind::ModelsOffered { .. } => "ModelsOffered",
        EventKind::AgentVersionUnconfirmed { .. } => "AgentVersionUnconfirmed",
        EventKind::EffortRequested { .. } => "EffortRequested",
        EventKind::EffortChanged { .. } => "EffortChanged",
        EventKind::EffortRefused { .. } => "EffortRefused",
        EventKind::ContextStarted { .. } => "ContextStarted",
        EventKind::ContextSwitched { .. } => "ContextSwitched",
        EventKind::ContextMoved { .. } => "ContextMoved",
        EventKind::BackgroundTasksChanged { .. } => "BackgroundTasksChanged",
        EventKind::SchedulesChanged { .. } => "SchedulesChanged",
        EventKind::TaskStopRequested { .. } => "TaskStopRequested",
        EventKind::TaskStopRefused { .. } => "TaskStopRefused",
        EventKind::CallsUnanswered { .. } => "CallsUnanswered",
        EventKind::ShellRequested { .. } => "ShellRequested",
        EventKind::ShellStopRequested { .. } => "ShellStopRequested",
        EventKind::ShellStarted { .. } => "ShellStarted",
        EventKind::ShellOutput { .. } => "ShellOutput",
        EventKind::ShellFinished { .. } => "ShellFinished",
        EventKind::Imported { .. } => "Imported",
        EventKind::ImportFinished { .. } => "ImportFinished",
        EventKind::BlobDeleted { .. } => "BlobDeleted",
        EventKind::BlobCropped { .. } => "BlobCropped",
    }
}

/// SQLite integers are i64; no log reaches `i64::MAX` events.
fn seq_to_sql(seq: Seq) -> i64 {
    i64::try_from(seq.0).unwrap_or(i64::MAX)
}

pub(crate) fn seq_from_sql(seq: i64) -> Result<Seq> {
    u64::try_from(seq)
        .map(Seq)
        .map_err(|_| Error::Corrupt(format!("negative seq {seq}")))
}

pub(crate) fn parse_id<T: From<Uuid>>(text: &str) -> Result<T> {
    Uuid::parse_str(text)
        .map(T::from)
        .map_err(|e| Error::Corrupt(format!("id {text:?}: {e}")))
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::Corrupt(format!("timestamp {text:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use skywave_proto::{ToolDisplay, ToolKind};

    use super::*;

    /// A log whose one session has `history` answered calls and finished
    /// shell runs behind the few events each recovery query looks for: an
    /// open call, an unfinished run, and a state, work, models and command
    /// event, all logged before the history. Returns the session and the
    /// command.
    fn seeded(db: &mut Db, history: usize) -> (SessionId, CommandId) {
        let session = SessionId::new();
        let context = ContextId(session.0);
        let command = CommandId::new();
        let tx = begin(db).unwrap();
        let log = |command: Option<CommandId>, kind: EventKind| {
            append_in(&tx, session, context, command, &kind).unwrap();
        };
        let call = |call| {
            EventKind::ToolCall(ToolCall {
                call: Some(call),
                parent: None,
                name: "exec".into(),
                display: ToolDisplay {
                    kind: ToolKind::Shell,
                    title: "$ true".into(),
                    detail: None,
                    body: None,
                },
                input: None,
            })
        };
        let shell = || EventKind::ShellRequested {
            command: "true".into(),
            when: RunWhen::Always,
            by: DeviceId::new(),
        };
        log(
            Some(command),
            EventKind::SessionCreated {
                workdir: "/w".into(),
                context_name: "start".into(),
                from: None,
                worktree: None,
            },
        );
        log(
            None,
            EventKind::SessionStateChanged(SessionState::AgentWorking),
        );
        log(
            None,
            EventKind::BackgroundTasksChanged { tasks: Vec::new() },
        );
        log(None, EventKind::ModelsOffered { models: Vec::new() });
        log(None, call(CallId::new()));
        log(Some(CommandId::new()), shell());
        for _ in 0..history {
            let id = CallId::new();
            log(None, call(id));
            log(
                None,
                EventKind::ToolResult(ToolResult {
                    call: Some(id),
                    is_error: false,
                    status: None,
                    preview: "ok".into(),
                    lines: 1,
                    output: None,
                }),
            );
            let run = CommandId::new();
            log(Some(run), shell());
            log(
                None,
                EventKind::ShellFinished {
                    run,
                    by: DeviceId::new(),
                    started_at: None,
                    command_line: "true".into(),
                    end: ShellEnd::Exited { code: 0 },
                    cwd: None,
                    preview: String::new(),
                    lines: 0,
                    output: None,
                    truncated: false,
                },
            );
        }
        tx.commit().unwrap();
        (session, command)
    }

    /// Every query a daemon start makes per session or over the log, with
    /// its parameters. `owed states` reads the events after seq 1, the
    /// `SessionCreated`, as `supervisor_cursor.accounted` bounds it.
    fn recovery_queries(
        session: SessionId,
        command: CommandId,
    ) -> Vec<(&'static str, String, Vec<String>)> {
        let s = session.0.to_string();
        vec![
            ("open calls", OPEN_CALLS_SQL.into(), vec![s.clone()]),
            ("last state", LAST_STATE_EVENT_SQL.into(), vec![s.clone()]),
            ("last work", LAST_WORK_CONTEXT_SQL.into(), vec![s.clone()]),
            ("offered models", OFFERED_MODELS_SQL.into(), vec![]),
            (
                "command seq",
                COMMAND_SEQ_SQL.into(),
                vec![command.0.to_string()],
            ),
            ("unfinished runs", unfinished_runs_sql(), vec![]),
            (
                "owed states",
                crate::supervisor::OWED_STATES_SQL.into(),
                vec![s, "1".into()],
            ),
        ]
    }

    /// The VM steps SQLite took to run `sql` to its end, and the rows it
    /// returned: a count of the work done, deterministic where a timing is
    /// not.
    fn steps(db: &Db, sql: &str, params: &[String]) -> (i32, usize) {
        let mut stmt = db.conn().prepare(sql).unwrap();
        let mut rows = stmt.query(rusqlite::params_from_iter(params)).unwrap();
        let mut count = 0;
        while rows.next().unwrap().is_some() {
            count += 1;
        }
        drop(rows);
        (stmt.get_status(rusqlite::StatementStatus::VmStep), count)
    }

    fn plan(db: &Db, sql: &str) -> Vec<String> {
        let mut stmt = db
            .conn()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let unbound = vec![rusqlite::types::Null; stmt.parameter_count()];
        stmt.query_map(rusqlite::params_from_iter(unbound), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    /// Whether a query plan reads a table that grows with the history end
    /// to end: a `SCAN`, not a `SEARCH`, unless through the partial index
    /// that holds only what is not over.
    fn scans_history(plan: &[String]) -> bool {
        plan.iter().any(|step| {
            let Some(table) = step.strip_prefix("SCAN ") else {
                return false;
            };
            let table = table.split(' ').next().unwrap_or_default();
            // `e`, `s` and `r` are the aliases `SHELL_RUN_SELECT` gives them.
            ["events", "e", "s", "shell_runs", "r", "open_calls"].contains(&table)
                && !step.ends_with("USING INDEX shell_runs_unfinished")
        })
    }

    #[test]
    fn recovery_queries_use_an_index() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        let (session, command) = seeded(&mut db, 1);
        for (name, sql, _) in recovery_queries(session, command) {
            let plan = plan(&db, &sql);
            assert!(!scans_history(&plan), "{name} scans: {plan:#?}");
        }
        // Positive control: the scan `offered_models` made before v18.
        let old = plan(
            &db,
            "SELECT kind FROM events WHERE json_extract(kind, '$.type') = 'models_offered' \
             ORDER BY seq DESC LIMIT 1",
        );
        assert!(scans_history(&old), "{old:#?}");
    }

    /// Each recovery query does the same work, and finds the same row, over
    /// a log a thousand times longer behind what it looks for.
    #[test]
    fn recovery_queries_cost_the_same_whatever_the_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut short = Db::open(&tmp.path().join("short.db")).unwrap();
        let mut long = Db::open(&tmp.path().join("long.db")).unwrap();
        let (s_session, s_command) = seeded(&mut short, 2);
        let (l_session, l_command) = seeded(&mut long, 2000);
        let short_queries = recovery_queries(s_session, s_command);
        let long_queries = recovery_queries(l_session, l_command);
        for ((name, sql, s_params), (_, _, l_params)) in short_queries.iter().zip(&long_queries) {
            let (s_steps, s_rows) = steps(&short, sql, s_params);
            let (l_steps, l_rows) = steps(&long, sql, l_params);
            assert_eq!((s_rows, l_rows), (1, 1), "{name} finds its row");
            assert_eq!(l_steps, s_steps, "{name} steps");
        }
        // Positive control: the log scan `open_calls` made before v18 reads
        // every call and answer in the session, even with the index.
        let old = "WITH answered (call) AS (
                 SELECT json_extract(kind, '$.call') FROM events
                 WHERE session_id = ?1 AND json_extract(kind, '$.type') = 'tool_result'
                 UNION
                 SELECT calls.value FROM events, json_each(events.kind, '$.calls') AS calls
                 WHERE session_id = ?1
                   AND json_extract(events.kind, '$.type') = 'calls_unanswered'
             )
             SELECT json_extract(kind, '$.call'), context FROM events
             WHERE session_id = ?1 AND json_extract(kind, '$.type') = 'tool_call'
               AND json_extract(kind, '$.call') NOT IN
                   (SELECT call FROM answered WHERE call IS NOT NULL)
             ORDER BY seq";
        let (s_steps, _) = steps(&short, old, &[s_session.0.to_string()]);
        let (l_steps, _) = steps(&long, old, &[l_session.0.to_string()]);
        assert!(l_steps > s_steps * 100, "{s_steps} then {l_steps}");
    }

    /// A row another attempt replaced is that attempt's: forgetting with the
    /// first attempt's token keeps it, and only the replacing attempt's removes
    /// it. Both attempts are one command's (a resend's), so the command cannot
    /// tell them apart.
    #[test]
    fn a_replaced_worktree_intent_is_forgotten_only_by_its_own_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("skywave.db")).unwrap();
        let conn = db.conn();
        let intent = |attempt: &str| worktree::Intent {
            repo: "/r".into(),
            branch: "feat/x".into(),
            target: "/r.worktrees/feat/x".into(),
            start: "abc".into(),
            attempt: attempt.into(),
        };
        let rows = || -> i64 {
            conn.query_row("SELECT COUNT(*) FROM worktree_intents", [], |r| r.get(0))
                .unwrap()
        };
        let command = CommandId::new();
        let (a, b) = (intent("a"), intent("b"));
        record_worktree_intent_in(conn, &a, command).unwrap();
        // Positive control: A's row is there to be forgotten.
        assert_eq!(rows(), 1);
        // The resend's attempt records the same repository and branch, replacing A's row.
        record_worktree_intent_in(conn, &b, command).unwrap();
        forget_worktree_intent_in(conn, &a.repo, &a.branch, &a.attempt).unwrap();
        assert_eq!(rows(), 1, "A's late forget removed B's row");
        forget_worktree_intent_in(conn, &b.repo, &b.branch, &b.attempt).unwrap();
        assert_eq!(rows(), 0);
    }
}
