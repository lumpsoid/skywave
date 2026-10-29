//! The seam between `skywaved` and a coding agent (spec §5, D3, D4).
//!
//! An [`AgentAdapter`] starts or resumes one [`AgentSession`] per Skywave
//! session. The session takes the supervisor's requests (send a message,
//! answer an approval, interrupt, shut down) and reports what the agent does
//! as a stream of agent-agnostic [`AdapterEvent`]s. An [`EventMapper`] turns
//! those into log [`EventKind`](skywave_proto::EventKind)s, moving large
//! payloads into blobs.
//!
//! Everything agent-specific stays behind this seam: the agent's own
//! approval request ids are opaque [`AgentRequestId`]s, and its resume state
//! (for example its own session id) is an opaque [`ResumeHandle`] persisted in
//! `sessions.agent_state` with [`store_resume_handle`] /
//! [`load_resume_handle`].
//!
//! **Protocol between supervisor and session.**
//! - [`AgentAdapter::start`] with `resume: None` starts a fresh agent session;
//!   with the stored handle it resumes one after a restart. The adapter
//!   reports the handle to persist as [`AdapterEvent::ResumeHandle`], as early
//!   as it knows it (before the first turn ends).
//! - A turn begins with a user message and ends with exactly one
//!   [`AdapterEvent::TurnEnded`]. A message sent while a turn runs is queued
//!   by the agent and starts the next turn.
//! - An approval blocks the agent until [`AgentSession::answer_approval`] (D8).
//!   [`AgentSession::interrupt`] ends the current turn; approvals pending at
//!   that point are reported as [`AdapterEvent::ApprovalCancelled`] (or are
//!   cancelled by the mapper when the turn ends), followed by
//!   `TurnEnded(Interrupted)`. The session stays usable for the next turn.
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
use skywave_proto::{Decision, SessionId};
use tokio::sync::mpsc;

use crate::db::Db;
use crate::error::{Error, Result};

pub use mapper::{DEFAULT_BLOB_THRESHOLD, EventMapper, SUMMARY_MAX_CHARS};

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

/// Whatever an adapter needs to resume an agent session after a restart.
///
/// Opaque JSON: only the adapter that produced it reads it. Stored in
/// `sessions.agent_state`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(transparent)]
pub struct ResumeHandle(pub serde_json::Value);

/// How to start an agent session.
#[derive(Debug, Clone, PartialEq)]
pub struct StartSpec {
    pub session_id: SessionId,
    /// Canonical working directory of the session (D14); the agent runs here.
    pub workdir: PathBuf,
    /// The handle stored for this session, to resume it; `None` starts fresh.
    pub resume: Option<ResumeHandle>,
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
    /// The agent invoked a tool with `input`.
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    /// Output of a tool call; `is_error` when the tool failed or was denied.
    ToolResult { output: String, is_error: bool },
    /// The agent is blocked until [`AgentSession::answer_approval`] for
    /// `request` (D8).
    ApprovalRequested {
        request: AgentRequestId,
        tool: String,
        input: serde_json::Value,
    },
    /// The agent withdrew `request`; an answer to it is no longer read.
    ApprovalCancelled {
        request: AgentRequestId,
        reason: String,
    },
    /// The current turn is over.
    TurnEnded(TurnOutcome),
    /// The handle to persist for resuming this session; replaces any earlier.
    ResumeHandle(ResumeHandle),
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

    /// Starts (or, with `spec.resume`, resumes) the agent for one session and
    /// returns the session with its event stream.
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

    /// Answers approval `request`. An answer to a request that is no longer
    /// pending is dropped by the adapter.
    fn answer_approval(
        &mut self,
        request: AgentRequestId,
        decision: Decision,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Stops the current turn, if any; a no-op when idle.
    fn interrupt(&mut self) -> impl Future<Output = Result<(), AgentError>> + Send;

    /// Stops the agent; its event stream ends with [`AdapterEvent::Exited`].
    /// Interrupt first to stop a running turn cleanly.
    fn shutdown(self) -> impl Future<Output = Result<(), AgentError>> + Send;
}

/// Persists `handle` as session `session_id`'s resume handle.
pub fn store_resume_handle(db: &Db, session_id: SessionId, handle: &ResumeHandle) -> Result<()> {
    let changed = db
        .conn()
        .prepare_cached("UPDATE sessions SET agent_state = ?2 WHERE id = ?1")?
        .execute((session_id.0.to_string(), serde_json::to_string(handle)?))?;
    if changed == 0 {
        return Err(Error::UnknownSession(session_id));
    }
    Ok(())
}

/// The resume handle stored for session `session_id`; `None` until the agent
/// has reported one.
pub fn load_resume_handle(db: &Db, session_id: SessionId) -> Result<Option<ResumeHandle>> {
    let row: Option<Option<String>> = db
        .conn()
        .prepare_cached("SELECT agent_state FROM sessions WHERE id = ?1")?
        .query_row([session_id.0.to_string()], |r| r.get(0))
        .optional()?;
    match row {
        None => Err(Error::UnknownSession(session_id)),
        Some(json) => Ok(json.as_deref().map(serde_json::from_str).transpose()?),
    }
}
