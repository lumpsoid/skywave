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
//!   `PermissionModeRequested`, `ModelRequested`, `EffortRequested`),
//!   so it follows the log with [`EventLog::subscribe`] and
//!   [`EventLog::read`] like any other reader.
//!
//! The wake-up notifier is in-process: only writers holding this
//! [`EventLog`] wake waiters. Admin commands never append events.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use skywave_proto::{
    AgentSettings, ApprovalId, CommandId, Event, EventBatch, EventKind, ModelChoice, Seq,
    SessionId, SessionState,
};
use tokio::sync::watch;
use uuid::Uuid;

use crate::db::Db;
use crate::error::{Error, Result};

/// Batch size used when `GET /v1/events` names no `limit`.
pub const DEFAULT_BATCH_LIMIT: u32 = 500;
/// Largest batch [`EventLog::read`] returns, whatever the caller asks for.
pub const MAX_BATCH_LIMIT: u32 = 5000;

/// Handle to the global event log. Cheap to clone; share one per process.
#[derive(Debug, Clone)]
pub struct EventLog {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Seq of the newest committed event.
    head: watch::Sender<Seq>,
    /// Directories sessions may run in (D14), as configured.
    roots: Vec<PathBuf>,
}

/// What the server knows about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    /// Canonical path of the session's working directory.
    pub workdir: String,
    /// The agent has a running turn that has not been interrupted.
    pub turn_active: bool,
}

/// An approval request no device has answered yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingApproval {
    pub id: ApprovalId,
    pub session_id: SessionId,
}

impl EventLog {
    /// A log over the database `db` belongs to; `roots` are the configured
    /// session roots (`Config::roots`).
    pub fn open(db: &Db, roots: Vec<PathBuf>) -> Result<EventLog> {
        let (head, _) = watch::channel(head_in(db.conn())?);
        Ok(EventLog {
            inner: Arc::new(Inner { head, roots }),
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
    /// at [`MAX_BATCH_LIMIT`], at least 1). An empty batch has
    /// `from = after + 1`.
    pub fn read(&self, db: &Db, after: Seq, limit: u32) -> Result<EventBatch> {
        let limit = limit.clamp(1, MAX_BATCH_LIMIT);
        let mut stmt = db.conn().prepare_cached(
            "SELECT seq, session_id, at, command_id, kind FROM events
             WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![seq_to_sql(after), limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let from = after.next();
        let mut events = Vec::new();
        for row in rows {
            let (seq, session_id, at, command_id, kind) = row?;
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
    /// commands produce, and `ApprovalCancelled` (use
    /// [`EventLog::cancel_approval`]), are refused with
    /// [`Error::InvalidAppend`], as is an unknown session.
    pub fn append(&self, db: &mut Db, session_id: SessionId, kind: EventKind) -> Result<Seq> {
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
            | EventKind::EffortRefused { .. } => {}
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
            | EventKind::EffortRequested { .. } => {
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
        if let EventKind::ApprovalRequested { id, .. } = &kind
            && approval_in(&tx, *id)?.is_some()
        {
            return Err(Error::InvalidAppend(format!(
                "approval {} was already requested",
                id.0
            )));
        }
        let seq = append_in(&tx, session_id, None, &kind)?;
        tx.commit()?;
        self.published(seq);
        Ok(seq)
    }

    /// Appends `ApprovalCancelled { id, reason }` if approval `id` is still
    /// pending, and returns its seq. `None` when the approval is unknown or
    /// already resolved or cancelled: an answer that committed first wins
    /// (D5), so the supervisor treats `None` as "nothing to cancel".
    pub fn cancel_approval(
        &self,
        db: &mut Db,
        id: ApprovalId,
        reason: &str,
    ) -> Result<Option<Seq>> {
        let tx = begin(db)?;
        let Some(ApprovalRow {
            session_id,
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
        let seq = append_in(&tx, session_id, None, &kind)?;
        tx.commit()?;
        self.published(seq);
        Ok(Some(seq))
    }

    /// Every approval still waiting for an answer, oldest first. On daemon
    /// start the supervisor cancels each one (spec §5).
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
        let mut stmt = db
            .conn()
            .prepare_cached("SELECT id, workdir, turn_active FROM sessions ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get::<_, bool>(2)?))
        })?;
        rows.map(|row| {
            let (id, workdir, turn_active) = row?;
            Ok(SessionInfo {
                id: parse_id(&id)?,
                workdir,
                turn_active,
            })
        })
        .collect()
    }

    /// Session `id`'s permission mode and model as its log has them; the
    /// default when it has none or is unknown.
    pub fn agent_settings(&self, db: &Db, id: SessionId) -> Result<AgentSettings> {
        agent_settings_in(db.conn(), id)
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
/// `command` is the command logging it ([`Event::command_id`]), `None` for
/// agent and daemon events.
pub(crate) fn append_in(
    conn: &Connection,
    session_id: SessionId,
    command: Option<CommandId>,
    kind: &EventKind,
) -> Result<Seq> {
    let seq = head_in(conn)?.next();
    let sql_seq = seq_to_sql(seq);
    let session = session_id.0.to_string();
    let at = Utc::now().to_rfc3339_opts(SecondsFormat::AutoSi, true);
    conn.prepare_cached(
        "INSERT INTO events (seq, session_id, at, command_id, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        sql_seq,
        session,
        at,
        command.map(|id| id.0.to_string()),
        serde_json::to_string(kind)?
    ])?;

    match kind {
        EventKind::SessionCreated { workdir } => {
            conn.execute(
                "INSERT INTO sessions (id, workdir) VALUES (?1, ?2)",
                params![session, workdir],
            )?;
        }
        EventKind::SessionStateChanged(state) => {
            let active = matches!(state, SessionState::AgentWorking);
            conn.execute(
                "UPDATE sessions SET turn_active = ?2 WHERE id = ?1",
                params![session, active],
            )?;
        }
        EventKind::TurnInterrupted => {
            conn.execute(
                "UPDATE sessions SET turn_active = 0 WHERE id = ?1",
                [&session],
            )?;
        }
        EventKind::ApprovalRequested {
            id, suggestions, ..
        } => {
            conn.execute(
                "INSERT INTO approvals (id, session_id, rules) VALUES (?1, ?2, ?3)",
                params![
                    id.0.to_string(),
                    session,
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
        | EventKind::ModelsOffered { .. } => {}
    }
    Ok(seq)
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

/// Seq of the newest event in `conn`'s view of the log.
pub(crate) fn head_in(conn: &Connection) -> Result<Seq> {
    let max: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
    seq_from_sql(max)
}

pub(crate) fn session_in(conn: &Connection, id: SessionId) -> Result<Option<SessionInfo>> {
    Ok(conn
        .query_row(
            "SELECT workdir, turn_active FROM sessions WHERE id = ?1",
            [id.0.to_string()],
            |r| {
                Ok(SessionInfo {
                    id,
                    workdir: r.get(0)?,
                    turn_active: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// What the log says of one approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ApprovalRow {
    pub session_id: SessionId,
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
    let row: Option<(String, Option<String>, i64)> = conn
        .query_row(
            "SELECT a.session_id, json_extract(e.kind, '$.type'), a.rules
             FROM approvals a LEFT JOIN events e ON e.seq = a.closed_seq
             WHERE a.id = ?1",
            [id.0.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(session, closed_by, rules)| {
        Ok(ApprovalRow {
            session_id: parse_id(&session)?,
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
