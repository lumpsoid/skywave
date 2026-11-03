//! The global event log (D12) and the projections command handling reads.
//!
//! Every event goes through [`append_in`], inside a write transaction begun
//! with `BEGIN IMMEDIATE`. It takes the next seq as `max(seq) + 1` under the
//! database write lock, so seqs are contiguous however many connections
//! write, and it updates the `sessions` / `approvals` projections in the same
//! transaction, so they never disagree with the log.
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
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use skywave_proto::{
    AgentSettings, AgentWork, ApprovalId, CallId, CommandId, ContextId, DeviceId, Event,
    EventBatch, EventKind, LogId, LogRun, ModelChoice, RunId, RunWhen, RunsSince, Seq, SessionId,
    SessionState, ShellEnd, limits,
};
use tokio::sync::watch;
use uuid::Uuid;

use crate::db::Db;
use crate::error::{Error, Result};

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
            }),
            shell_enabled: false,
            bypass_allowed: false,
        })
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
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_)
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
            | EventKind::BackgroundTasksChanged { .. }
            | EventKind::SchedulesChanged { .. }
            | EventKind::TaskStopRefused { .. }
            | EventKind::CallsUnanswered { .. } => {}
            EventKind::SessionCreated { .. }
            | EventKind::UserMessage { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. }
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
            | EventKind::ImportFinished { .. } => {
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
        if let EventKind::ApprovalRequested { id, .. } = &kind
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
        let mut stmt = tx.prepare_cached(&format!(
            "{SHELL_RUN_SELECT} WHERE r.finished_seq IS NULL ORDER BY r.seq"
        ))?;
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
    pub fn open_calls(&self, db: &Db, id: SessionId) -> Result<Vec<(ContextId, Vec<CallId>)>> {
        let mut stmt = db.conn().prepare_cached(
            "WITH answered (call) AS (
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
             ORDER BY seq",
        )?;
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
            .prepare_cached("SELECT seq FROM events WHERE command_id = ?1")?
            .query_row([id.0.to_string()], |r| r.get(0))
            .optional()?;
        seq.map(seq_from_sql).transpose()
    }

    /// The models of the newest `ModelsOffered` in any session, if any.
    pub fn offered_models(&self, db: &Db) -> Result<Option<Vec<ModelChoice>>> {
        let kind: Option<String> = db
            .conn()
            .prepare_cached(
                "SELECT kind FROM events WHERE json_extract(kind, '$.type') = 'models_offered' \
                 ORDER BY seq DESC LIMIT 1",
            )?
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
        EventKind::ApprovalResolved { id, .. } | EventKind::ApprovalCancelled { id, .. } => {
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
        EventKind::UserMessage { .. }
        | EventKind::AgentMessage { .. }
        | EventKind::ToolCall { .. }
        | EventKind::ToolResult { .. }
        | EventKind::SessionStopped
        | EventKind::SessionRenamed { .. }
        | EventKind::SessionArchived { .. }
        | EventKind::TurnUsage(_)
        | EventKind::ModelsOffered { .. }
        | EventKind::TaskStopRequested { .. }
        | EventKind::TaskStopRefused { .. }
        | EventKind::CallsUnanswered { .. }
        | EventKind::ShellOutput { .. } => {}
        // Imported history is a record: an imported call opens nothing, and
        // `open_calls` never sees it, as it selects on the top-level `type`.
        EventKind::Imported { .. } | EventKind::ImportFinished { .. } => {}
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

/// The session's newest `SessionStateChanged` or `TurnInterrupted`, with
/// the context it is stamped with.
pub(crate) fn last_state_event_in(
    conn: &Connection,
    session_id: SessionId,
) -> Result<Option<(EventKind, ContextId)>> {
    let row: Option<(String, String)> = conn
        .prepare_cached(
            "SELECT kind, context FROM events WHERE session_id = ?1 \
             AND json_extract(kind, '$.type') IN ('session_state_changed', 'turn_interrupted') \
             ORDER BY seq DESC LIMIT 1",
        )?
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ApprovalRow {
    pub session_id: SessionId,
    /// The context the request was logged in.
    pub context: ContextId,
    /// How it was closed; `None` while it is pending.
    pub closed: Option<Closed>,
    /// How many rule suggestions the request offers.
    pub rules: usize,
}

/// How an approval request was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// By an `ApprovalResolved`: an answer won.
    Resolved,
    /// By an `ApprovalCancelled`: no answer ever will.
    Cancelled,
}

/// Approval `id` as the log has it; `None` if unknown.
pub(crate) fn approval_in(conn: &Connection, id: ApprovalId) -> Result<Option<ApprovalRow>> {
    // The type tag of the event at `closed_seq` tells an answer from a
    // cancel.
    let row: Option<(String, String, Option<String>, i64)> = conn
        .query_row(
            "SELECT a.session_id, a.context, json_extract(e.kind, '$.type'), a.rules
             FROM approvals a LEFT JOIN events e ON e.seq = a.closed_seq
             WHERE a.id = ?1",
            [id.0.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    row.map(|(session, context, closed_by, rules)| {
        Ok(ApprovalRow {
            session_id: parse_id(&session)?,
            context: parse_id(&context)?,
            closed: closed_by.map(|kind| match kind.as_str() {
                "approval_cancelled" => Closed::Cancelled,
                _ => Closed::Resolved,
            }),
            rules: usize::try_from(rules).unwrap_or(0),
        })
    })
    .transpose()
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
        EventKind::TurnInterrupted => "TurnInterrupted",
        EventKind::SessionStopped => "SessionStopped",
        EventKind::SessionRenamed { .. } => "SessionRenamed",
        EventKind::SessionArchived { .. } => "SessionArchived",
        EventKind::SessionStateChanged(_) => "SessionStateChanged",
        EventKind::TurnUsage(_) => "TurnUsage",
        EventKind::PermissionModeRequested { .. } => "PermissionModeRequested",
        EventKind::PermissionModeChanged { .. } => "PermissionModeChanged",
        EventKind::PermissionModeRefused { .. } => "PermissionModeRefused",
        EventKind::ModelRequested { .. } => "ModelRequested",
        EventKind::ModelChanged { .. } => "ModelChanged",
        EventKind::ModelRefused { .. } => "ModelRefused",
        EventKind::ModelsOffered { .. } => "ModelsOffered",
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
    }
}

/// SQLite integers are i64; no log reaches `i64::MAX` events.
fn seq_to_sql(seq: Seq) -> i64 {
    i64::try_from(seq.0).unwrap_or(i64::MAX)
}

fn seq_from_sql(seq: i64) -> Result<Seq> {
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
