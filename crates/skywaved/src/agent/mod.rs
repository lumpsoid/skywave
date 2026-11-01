//! The seam between `skywaved` and a coding agent (spec §5, D3, D4).
//!
//! An [`AgentAdapter`] starts or resumes one [`AgentSession`] per Skywave
//! session. The session takes the supervisor's requests (send a message,
//! answer an approval, interrupt, shut down) and reports what the agent does
//! as a stream of agent-agnostic [`AdapterEvent`]s. An [`EventMapper`] turns
//! those into log [`EventKind`](skywave_proto::EventKind)s, cutting what an
//! adapter shows to the protocol's caps and moving what was cut into blobs.
//!
//! **Tool calls.** An adapter reports a tool call with its agent's own
//! [`AgentCallId`], the full input and, when it knows the tool, a
//! [`ToolDisplay`] with uncut content; the result and any approval for the
//! call name the same [`AgentCallId`]. Adapters never cut or blob anything:
//! the mapper alone applies `skywave_proto::limits`.
//!
//! Everything agent-specific stays behind this seam: the agent's own
//! approval request ids are opaque [`AgentRequestId`]s, and its resume state
//! (for example its own session id) is an opaque [`ResumeHandle`] persisted
//! per session context in `contexts.agent_state` with
//! [`store_resume_handle`] / [`load_resume_handle`]. A context moved into
//! a new session leaves that session's first context its origin
//! ([`load_origin`]), whose conversation the new one continues as a fork.
//! What the agent was
//! granted for the whole Skywave session (for example lasting permission
//! rules) is opaque [`Grants`], persisted per session in
//! `sessions.agent_grants` with [`store_grants`] / [`load_grants`] and given
//! to every start of every context of that session.
//!
//! **Protocol between supervisor and session.**
//! - [`AgentAdapter::start`] runs the [`Conversation`] in `spec`: a fresh
//!   one, the context's own resumed with its stored handle, or a fork of
//!   the origin's. The adapter reports the handle to persist as
//!   [`AdapterEvent::ResumeHandle`] once the context's conversation exists
//!   and before the first turn ends (for a fork, the copy's handle, as for
//!   a fresh start); a process stopped before any message reports none, so
//!   the context's next start is fresh (or a fork) again. `spec.grants` are the session's grants; when they
//!   change the adapter reports the whole new set as
//!   [`AdapterEvent::Grants`].
//! - A turn begins with a user message and ends with exactly one
//!   [`AdapterEvent::TurnEnded`]. A message sent while a turn runs is queued
//!   by the agent and starts the next turn. What the turn took (time,
//!   tokens, cost, model) may come just before as [`AdapterEvent::TurnUsage`],
//!   per-turn figures: an adapter whose agent reports running totals turns
//!   them into the turn's share.
//! - An approval blocks the agent until [`AgentSession::answer_approval`] (D8).
//!   It may offer lasting rules ([`RuleSuggestion`]s, labelled by the
//!   adapter); an approval can grant one of them by its index, which only
//!   the adapter maps back to what its agent understands.
//!   [`AgentSession::interrupt`] ends the current turn; approvals pending at
//!   that point are reported as [`AdapterEvent::ApprovalCancelled`] (or are
//!   cancelled by the mapper when the turn ends), followed by
//!   `TurnEnded(Interrupted)`. The session stays usable for the next turn.
//! - [`AgentSession::set_permission_mode`], [`AgentSession::set_model`] and
//!   [`AgentSession::set_effort`] work at any time, mid-turn and with an
//!   approval pending too. Each request gets exactly one answer, in the
//!   order asked: `PermissionModeSet` / `PermissionModeRefused` (`ModelSet`
//!   / `ModelRefused`, `EffortSet` / `EffortRefused`), unless the agent
//!   exits first. What the agent runs with otherwise (at start, at a turn, a
//!   change it made on its own) is reported as `PermissionMode` / `Model` /
//!   `Effort`, as often as the agent says, and the models it offers as
//!   `ModelsOffered`. [`StartSpec`] names the mode, model and effort to
//!   start with; the adapter applies them as requests, so each is answered
//!   the same way.
//! - Work the agent runs outside a turn (background shells, subagents) is
//!   reported as [`AdapterEvent::BackgroundTasks`], the whole live set, as
//!   often as it changes. An adapter reports the empty set once the agent
//!   is ready, since nothing is reported at start; the mapper logs only
//!   changes.
//! - The stream ends with [`AdapterEvent::Exited`] when the agent stops, on
//!   [`AgentSession::shutdown`] or on its own.
//!
//! The scripted [`fake::FakeAgent`] implements the trait for deterministic
//! tests; [`claude_code`] holds the real adapter, the only module that names
//! the agent it drives (D3).

pub mod claude_code;
pub mod fake;
mod mapper;

use std::future::Future;
use std::path::PathBuf;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use skywave_proto::{ContextId, Decision, SessionId};
pub use skywave_proto::{
    Effort, ModelChoice, PermissionMode, RuleScope, RuleSuggestion, TaskKind, TurnUsage,
};
use tokio::sync::mpsc;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::event_log::parse_id;

pub use mapper::EventMapper;
pub(crate) use mapper::bounded_models;
pub use skywave_proto::{BodyFormat, ToolKind};

/// The agent's own id for one approval request (e.g. a control request id).
///
/// Chosen by the agent and meaningful only to its adapter; the log names
/// approvals by [`ApprovalId`](skywave_proto::ApprovalId), and the
/// [`EventMapper`] keeps the mapping between the two.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct AgentRequestId(pub String);

impl From<&str> for AgentRequestId {
    fn from(id: &str) -> Self {
        AgentRequestId(id.to_owned())
    }
}

/// The agent's own id for one tool call (e.g. a tool-use id).
///
/// Meaningful only to its adapter; it links a call to its result and its
/// approval. The [`EventMapper`] gives each one a
/// [`CallId`](skywave_proto::CallId) for the log, valid until the turn ends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct AgentCallId(pub String);

impl From<&str> for AgentCallId {
    fn from(id: &str) -> Self {
        AgentCallId(id.to_owned())
    }
}

/// One background task of the agent, uncut. The ids are the agent's own,
/// opaque; the [`EventMapper`] cuts the rest for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTask {
    pub id: String,
    pub kind: TaskKind,
    pub title: String,
    /// The id of the subagent task that launched this one, if any.
    pub parent: Option<String>,
    /// Not activity (see [`skywave_proto::BackgroundTask::ambient`]).
    pub ambient: bool,
}

/// How to show a tool call, uncut. Agent-agnostic (D3): the adapter maps its
/// agent's tool onto a [`ToolKind`] and hands over full content; the
/// [`EventMapper`] cuts it to the protocol's caps and stores what was cut.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDisplay {
    pub kind: ToolKind,
    /// Main line, e.g. a path or a command. Only its first line is shown.
    pub title: String,
    /// Optional second line, e.g. a description.
    pub detail: Option<String>,
    /// Content worth showing beneath the title, e.g. a diff, in full.
    pub body: Option<DisplayBody>,
    /// Title, detail and body together show all of the call's input. When
    /// false (the display leaves fields out), or when the mapper cuts any of
    /// them, the input is kept as a blob.
    pub shows_all_input: bool,
}

/// Full content shown beneath a tool call's title.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayBody {
    pub format: BodyFormat,
    pub text: String,
}

/// How a tool call ended, uncut.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultDisplay {
    /// The tool failed or was denied.
    pub is_error: bool,
    /// Short outcome, e.g. `exit 1`, `3 matches`; cut to one short line.
    pub status: Option<String>,
    /// What to preview, uncut, when it is not `output` itself, e.g. the
    /// tool's own output without notes meant for the model. `None` previews
    /// `output`. When set and different, `output` is kept as a blob.
    pub preview: Option<String>,
    /// The whole output.
    pub output: String,
}

/// Whatever an adapter needs to resume an agent session after a restart.
///
/// Opaque JSON: only the adapter that produced it reads it. Stored per
/// session context, in `contexts.agent_state`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(transparent)]
pub struct ResumeHandle(pub serde_json::Value);

/// What an agent was granted for the whole Skywave session, across its
/// contexts (for example lasting permission rules).
///
/// Opaque JSON like [`ResumeHandle`]: only the adapter that produced it
/// reads it. Stored per session, in `sessions.agent_grants`, and never
/// shared between sessions.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct Grants(pub serde_json::Value);

/// Which conversation a starting agent runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Conversation {
    /// A new, empty conversation.
    Fresh,
    /// The context's own conversation, from its handle.
    Resume(ResumeHandle),
    /// A copy of another conversation (the handle of the context this one
    /// continues), which becomes this context's own: the adapter reports
    /// the new conversation's handle as for a fresh start.
    Fork(ResumeHandle),
}

/// How to start an agent session.
#[derive(Debug, Clone, PartialEq)]
pub struct StartSpec {
    pub session_id: SessionId,
    /// The session context the agent runs in. For logging and the fake's
    /// keying only: adapters must not interpret it (the context's
    /// conversation is `conversation`).
    pub context: ContextId,
    /// Canonical working directory of the session (D14); the agent runs here.
    pub workdir: PathBuf,
    /// The conversation to run in `context`.
    pub conversation: Conversation,
    /// The grants stored for the session, whichever context starts; `None`
    /// until the agent has reported any.
    pub grants: Option<Grants>,
    /// The mode to run in, asked for once the agent is up; `None` leaves
    /// the agent's own.
    pub mode: Option<PermissionMode>,
    /// The model to run, asked for like `mode`.
    pub model: Option<String>,
    /// The reasoning effort to run at, asked for like `mode`.
    pub effort: Option<Effort>,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    /// Stopped by [`AgentSession::interrupt`].
    Interrupted,
    /// The agent gave up on the turn with an error.
    Failed(String),
}

/// What an agent session reports, in order. Agent-agnostic (D3).
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterEvent {
    /// The agent began working on a turn.
    TurnStarted,
    /// Text the agent wrote for the user.
    Text(String),
    /// The agent invoked tool `name` with `input`. `call` is the agent's id
    /// for it, if it has one; `display` is how to show it, or `None` for a
    /// generic display derived from `input`.
    ToolCall {
        call: Option<AgentCallId>,
        name: String,
        input: serde_json::Value,
        display: Option<ToolDisplay>,
    },
    /// A tool call ended; `call` names it, if the agent said which.
    ToolResult {
        call: Option<AgentCallId>,
        result: ResultDisplay,
    },
    /// The agent is blocked until [`AgentSession::answer_approval`] for
    /// `request` (D8). `call` is the tool call the approval gates, if known.
    /// `suggestions` are the lasting rules the agent offers to grant with an
    /// approval, labelled uncut; an answer names one by its index.
    ApprovalRequested {
        request: AgentRequestId,
        call: Option<AgentCallId>,
        tool: String,
        input: serde_json::Value,
        suggestions: Vec<RuleSuggestion>,
    },
    /// The agent withdrew `request`; an answer to it is no longer read.
    ApprovalCancelled {
        request: AgentRequestId,
        reason: String,
    },
    /// What the turn took, figures for this turn alone; reported at most
    /// once a turn, just before its `TurnEnded`, and only with something in
    /// it. Optional: an agent that measures nothing never sends it.
    TurnUsage(TurnUsage),
    /// The current turn is over.
    TurnEnded(TurnOutcome),
    /// The handle to persist for resuming this context; replaces any earlier.
    ResumeHandle(ResumeHandle),
    /// The grants to persist for the session, the whole set; replaces any
    /// earlier.
    Grants(Grants),
    /// The agent runs in `mode`, as it reported unasked (at start, at a
    /// turn, or a change of its own). May repeat what it said before.
    PermissionMode(PermissionMode),
    /// The answer to the oldest unanswered mode request: the agent switched.
    PermissionModeSet(PermissionMode),
    /// The answer to the oldest unanswered mode request: the agent refused
    /// `mode` and keeps the one it had.
    PermissionModeRefused {
        mode: PermissionMode,
        reason: String,
    },
    /// The agent runs `model`, as it reported unasked; may repeat.
    Model(String),
    /// The answer to the oldest unanswered model request: the agent runs
    /// `model` now, as it names it.
    ModelSet(String),
    /// The answer to the oldest unanswered model request: the agent refused
    /// `model` (as asked) and keeps the one it had.
    ModelRefused { model: String, reason: String },
    /// The agent runs at `effort`, as it reported unasked; `None` when its
    /// model takes no effort level. May repeat.
    Effort(Option<Effort>),
    /// The answer to the oldest unanswered effort request: the agent runs at
    /// `effort` now (`None`: its model takes no effort level).
    EffortSet(Option<Effort>),
    /// The answer to the oldest unanswered effort request: the agent refused
    /// `effort` and keeps the one it had.
    EffortRefused { effort: Effort, reason: String },
    /// The models the agent offers, uncut.
    ModelsOffered(Vec<ModelChoice>),
    /// Every live background task, in the agent's order; replaces the set
    /// reported before. May repeat it.
    BackgroundTasks(Vec<AgentTask>),
    /// The agent stopped; no further events follow. `error` is set when it
    /// stopped abnormally (crash, startup failure).
    Exited { error: Option<String> },
}

/// Receiving end of an agent session's [`AdapterEvent`] stream.
pub type AdapterEvents = mpsc::Receiver<AdapterEvent>;

/// Why an adapter could not do what the supervisor asked.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The agent has exited; the session takes no more requests.
    #[error("agent session has ended")]
    Closed,
    /// The agent could not be started.
    #[error("cannot start agent: {0}")]
    Start(String),
    /// The resume handle is unusable; starting fresh would lose history.
    #[error("cannot resume agent session: {0}")]
    Resume(String),
    #[error("agent i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Starts agent sessions. One adapter per daemon, shared by all sessions.
pub trait AgentAdapter: Send + Sync + 'static {
    type Session: AgentSession;

    /// Starts the agent for one session in `spec.conversation` (fresh,
    /// resumed or forked) and returns the session with its event stream.
    /// An unusable handle to resume or fork fails with
    /// [`AgentError::Resume`]; an adapter that cannot fork fails a fork
    /// with [`AgentError::Start`].
    fn start(
        &self,
        spec: StartSpec,
    ) -> impl Future<Output = Result<(Self::Session, AdapterEvents), AgentError>> + Send;
}

/// One running agent session.
///
/// Requests return once the agent has been told; results arrive as
/// [`AdapterEvent`]s. After the agent exits every request fails with
/// [`AgentError::Closed`].
pub trait AgentSession: Send + 'static {
    /// Sends a user message; it starts a turn, or the next one if a turn runs.
    fn send_message(&mut self, text: String)
    -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Answers approval `request`; with an approval, `rule` grants the
    /// request's suggestion with that index too (the log only holds a rule
    /// the request offered, never with a denial). An answer to a request
    /// that is no longer pending is dropped by the adapter.
    fn answer_approval(
        &mut self,
        request: AgentRequestId,
        decision: Decision,
        rule: Option<u8>,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Stops the current turn, if any; a no-op when idle.
    fn interrupt(&mut self) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to run in `mode`; answered by `PermissionModeSet` or
    /// `PermissionModeRefused`.
    fn set_permission_mode(
        &mut self,
        mode: PermissionMode,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to run `model`; answered by `ModelSet` or
    /// `ModelRefused`.
    fn set_model(&mut self, model: String) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Asks the agent to run at `effort`; answered by `EffortSet` or
    /// `EffortRefused`.
    fn set_effort(&mut self, effort: Effort)
    -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Stops the agent; its event stream ends with [`AdapterEvent::Exited`].
    /// Interrupt first to stop a running turn cleanly; a turn still running
    /// without an interrupt is abandoned, not ended, and its approvals are
    /// left for the caller.
    fn shutdown(self) -> impl Future<Output = Result<(), AgentError>> + Send;
}

/// Persists `handle` as session context `context`'s resume handle.
pub fn store_resume_handle(db: &Db, context: ContextId, handle: &ResumeHandle) -> Result<()> {
    let changed = db
        .conn()
        .prepare_cached("UPDATE contexts SET agent_state = ?2 WHERE id = ?1")?
        .execute((context.0.to_string(), serde_json::to_string(handle)?))?;
    if changed == 0 {
        return Err(Error::UnknownContext(context));
    }
    Ok(())
}

/// The resume handle stored for session context `context`; `None` until
/// the agent has reported one.
pub fn load_resume_handle(db: &Db, context: ContextId) -> Result<Option<ResumeHandle>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT agent_state FROM contexts WHERE id = ?1")?
        .query_row([context.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownContext(context)),
        Some(json) => Ok(json.as_deref().map(serde_json::from_str).transpose()?),
    }
}

/// The context of another session that session context `context`
/// continues, its origin: set on the first context of a session a context
/// was moved into, `None` for any other.
pub fn load_origin(db: &Db, context: ContextId) -> Result<Option<ContextId>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT origin FROM contexts WHERE id = ?1")?
        .query_row([context.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownContext(context)),
        Some(id) => Ok(id.as_deref().map(parse_id).transpose()?),
    }
}

/// Persists `grants` as session `session`'s grants, replacing any earlier.
pub fn store_grants(db: &Db, session: SessionId, grants: &Grants) -> Result<()> {
    let changed = db
        .conn()
        .prepare_cached("UPDATE sessions SET agent_grants = ?2 WHERE id = ?1")?
        .execute((session.0.to_string(), serde_json::to_string(grants)?))?;
    if changed == 0 {
        return Err(Error::UnknownSession(session));
    }
    Ok(())
}

/// The grants stored for session `session`; `None` until the agent has
/// reported any.
pub fn load_grants(db: &Db, session: SessionId) -> Result<Option<Grants>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT agent_grants FROM sessions WHERE id = ?1")?
        .query_row([session.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownSession(session)),
        Some(json) => Ok(json.as_deref().map(serde_json::from_str).transpose()?),
    }
}
