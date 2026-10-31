//! Skywave wire types, shared by `skywaved` and `skywave-core`.
//!
//! Encoded as JSON for the MVP. Large content is never inlined; it travels as
//! a [`BlobRef`] and is fetched separately. No type here names a concrete
//! agent (D3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod fingerprint;

pub use fingerprint::{Fingerprint, InvalidFingerprint};

/// Wire protocol version. Bump on any incompatible change to these types.
///
/// v2: tool calls carry a [`CallId`] and a bounded [`ToolDisplay`] instead of
/// a free-form summary; v1 log events still decode (see [`ToolCall`],
/// [`ToolResult`]).
///
/// v3: `CommandKind::StopSession` and `EventKind::SessionStopped`; a v2
/// client cannot decode the new event.
///
/// v4: `CommandKind::RenameSession` and `EventKind::SessionRenamed`;
/// `CommandKind::SetSessionArchived` and `EventKind::SessionArchived`;
/// `suggestions` on `EventKind::ApprovalRequested`, the `rule` granted with
/// `CommandKind::ResolveApproval` and `EventKind::ApprovalResolved`, and
/// `RejectReason::UnknownRule`; `EventKind::TurnUsage`. One v4 carries
/// every wire change of its release; no compatibility layer: the exact
/// version check at `hello` rejects older clients.
///
/// v5: `RejectReason::ApprovalCancelled`, so an answer that arrives after
/// its approval was cancelled is not reported as answered elsewhere.
///
/// v6: `devices` on [`HelloResponse`], so a device names the others by the
/// names they paired with; `command_id` on [`Event`], the command that
/// logged it, so a pulled event settles its command when the reply was lost
/// (it moves there from `EventKind::UserMessage`).
pub const PROTOCOL_VERSION: u32 = 6;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A fresh, time-ordered (UUIDv7) id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

uuid_id!(
    /// Identifies a session. Its events live in the server's global log (D12).
    SessionId
);
uuid_id!(
    /// Client-generated command id; the server dedups on it, so resends are safe.
    CommandId
);
uuid_id!(
    /// Identifies a paired device.
    DeviceId
);
uuid_id!(
    /// Identifies a pending approval request.
    ApprovalId
);
uuid_id!(
    /// Identifies one tool call within a session, linking its [`ToolCall`],
    /// [`ToolResult`] and approval. Minted by the server when it logs the
    /// call; the agent's own call ids never reach clients (D4).
    CallId
);
uuid_id!(
    /// Identifies a server; reported by `GET /v1/hello`.
    ServerId
);
uuid_id!(
    /// Identifies a blob of large content fetched separately from events.
    BlobId
);

/// Position in the server's global event log (D12).
///
/// One log spans every session. Seqs are contiguous: the first event is
/// `Seq(1)` and each later event is exactly one more. [`Seq::ZERO`] is the
/// position before the first event, so a fresh cursor is `Seq::ZERO`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// Before the first event; the cursor of a device that has seen nothing.
    pub const ZERO: Seq = Seq(0);

    /// The seq directly after this one.
    pub fn next(self) -> Seq {
        Seq(self.0 + 1)
    }
}

/// Reference to large content. Content is always referenced, never inlined.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BlobRef {
    pub id: BlobId,
    pub size: u64,
}

/// One entry in the global event log.
///
/// Carries no seq on the wire: its seq follows from its position in an
/// [`EventBatch`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Event {
    pub session_id: SessionId,
    pub at: DateTime<Utc>,
    /// The command that logged the event; `None` for the events the agent
    /// and the daemon produce. Every accepted command logs exactly one
    /// event, so it is that command's `Accepted { seq }` too: a device whose
    /// reply was lost settles the command from the log.
    pub command_id: Option<CommandId>,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A session was created in `workdir`, a directory on the server (D14).
    SessionCreated {
        workdir: String,
    },
    UserMessage {
        device_id: DeviceId,
        text: String,
    },
    AgentMessage {
        text: String,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// The agent is blocked until a device answers (D8).
    ///
    /// `call` is the [`ToolCall`] the approval gates; `summary` is then that
    /// call's title and `input` is `None` unless the request differs from the
    /// call's input. `call` is `None` in v1 events and for an approval with
    /// no logged call.
    ApprovalRequested {
        id: ApprovalId,
        #[serde(default)]
        call: Option<CallId>,
        tool: String,
        summary: String,
        input: Option<BlobRef>,
        /// Lasting rules the agent offers to grant with an approval, at most
        /// [`limits::RULE_SUGGESTIONS_MAX`]; an answer names one by its
        /// index. Empty in events from before v4.
        #[serde(default)]
        suggestions: Vec<RuleSuggestion>,
    },
    /// First answer wins (D5). `rule` is the index of the suggestion of the
    /// request granted along with an approval; never set with a denial.
    ApprovalResolved {
        id: ApprovalId,
        decision: Decision,
        by: DeviceId,
        #[serde(default)]
        rule: Option<u8>,
    },
    /// The request can no longer be answered, e.g. the daemon restarted.
    ApprovalCancelled {
        id: ApprovalId,
        reason: String,
    },
    /// The agent's current turn was stopped by an `Interrupt` command.
    TurnInterrupted,
    /// The session's agent was told to shut down by a `StopSession` command.
    /// A running turn is interrupted first. The session stays: the next
    /// message starts the agent again, resumed.
    SessionStopped,
    /// The session's name was set by a `RenameSession` command; `None`
    /// restores the default title. The last rename in log order wins.
    /// Renaming is not activity.
    SessionRenamed {
        name: Option<String>,
        by: DeviceId,
    },
    /// The session was archived or brought back by a `SetSessionArchived`
    /// command. It carries the absolute value, never a toggle; see
    /// [`EventKind::archived_effect`] for the whole rule. Archiving is not
    /// activity and does not stop the agent.
    SessionArchived {
        archived: bool,
        by: DeviceId,
    },
    SessionStateChanged(SessionState),
    /// What the turn that is ending took, as the agent reported it; logged
    /// just before the turn's closing `SessionStateChanged`, at most once a
    /// turn. Not notable.
    TurnUsage(TurnUsage),
}

impl EventKind {
    /// The device whose command logged the event, for events that name one.
    pub fn device(&self) -> Option<DeviceId> {
        match self {
            EventKind::UserMessage { device_id, .. } => Some(*device_id),
            EventKind::ApprovalResolved { by, .. }
            | EventKind::SessionRenamed { by, .. }
            | EventKind::SessionArchived { by, .. } => Some(*by),
            EventKind::SessionCreated { .. }
            | EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_) => None,
        }
    }

    /// Whether only a command logs this kind, so an event of it carries
    /// the [`Event::command_id`] of the command that did. The agent and the
    /// daemon log the rest, `ApprovalCancelled` included.
    pub fn logged_by_command(&self) -> bool {
        match self {
            EventKind::SessionCreated { .. }
            | EventKind::UserMessage { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. } => true,
            EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_) => false,
        }
    }

    /// What the event does to its session's archived flag, the one rule the
    /// server and every client fold: `SessionArchived` sets it, a
    /// `UserMessage` from any device brings the session back, anything else
    /// leaves it alone (`None`).
    pub fn archived_effect(&self) -> Option<bool> {
        match self {
            EventKind::SessionArchived { archived, .. } => Some(*archived),
            EventKind::UserMessage { .. } => Some(false),
            EventKind::SessionCreated { .. }
            | EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_) => None,
        }
    }
}

/// Size caps on the bounded text in tool events. Producers (the server)
/// enforce them; v1 events decoded for compatibility may exceed them, so
/// renderers should still clip.
pub mod limits {
    /// Longest [`ToolDisplay::title`](crate::ToolDisplay::title), in chars.
    pub const TITLE_MAX_CHARS: usize = 120;
    /// Longest [`ToolDisplay::detail`](crate::ToolDisplay::detail), in chars.
    pub const DETAIL_MAX_CHARS: usize = 120;
    /// Longest [`ToolResult::status`](crate::ToolResult::status), in chars.
    pub const STATUS_MAX_CHARS: usize = 60;
    /// Most lines in a preview ([`Body::preview`](crate::Body::preview),
    /// [`ToolResult::preview`](crate::ToolResult::preview)).
    pub const PREVIEW_MAX_LINES: usize = 8;
    /// Most UTF-8 bytes in a preview.
    pub const PREVIEW_MAX_BYTES: usize = 512;
    /// Longest session name (`RenameSession`, `SessionRenamed`), in chars.
    pub const SESSION_NAME_MAX_CHARS: usize = 80;
    /// Longest [`RuleSuggestion::label`](crate::RuleSuggestion::label), in
    /// chars.
    pub const RULE_LABEL_MAX_CHARS: usize = 120;
    /// Most [`RuleSuggestion`](crate::RuleSuggestion)s on one approval.
    pub const RULE_SUGGESTIONS_MAX: usize = 8;
}

/// A lasting permission rule an agent offers to grant along with an
/// approval, e.g. "allow edits this session". Agent-agnostic (D3): the
/// server mints the label from what the agent offers; clients show it and
/// never parse it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuleSuggestion {
    /// One line, at most [`limits::RULE_LABEL_MAX_CHARS`].
    pub label: String,
    pub scope: RuleScope,
}

/// How far a granted rule reaches.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    /// This session only.
    Session,
    /// Every session in the project (the workdir) on the server.
    Project,
    /// Every session of the server's user.
    User,
}

/// The agent invoked a tool.
///
/// `name` is the agent's own tool name, carried as data for display; clients
/// must not branch on it, only on [`ToolDisplay::kind`]. `input` is the full
/// input as a blob, when it is worth fetching.
///
/// **v1 compatibility.** A v1 `tool_call` has a free-form `summary` and no
/// `call` or `display`; it decodes as `call: None` and a display of kind
/// [`ToolKind::Other`] whose title is the summary. A v2 server always sets
/// `call`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(try_from = "compat::ToolCallWire")]
pub struct ToolCall {
    pub call: Option<CallId>,
    pub name: String,
    pub display: ToolDisplay,
    pub input: Option<BlobRef>,
}

/// What a client shows for a tool call, decided by the server (D4).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolDisplay {
    pub kind: ToolKind,
    /// One line, at most [`limits::TITLE_MAX_CHARS`], e.g. a path or command.
    /// A [`ToolKind::Shell`] title is the command after a `$ ` prompt, e.g.
    /// `$ cargo test`, and so is the body standing in for a cut one; heads
    /// copying the command drop the prompt.
    pub title: String,
    /// Optional second line, at most [`limits::DETAIL_MAX_CHARS`].
    pub detail: Option<String>,
    /// Content worth showing beneath the title, e.g. a diff.
    pub body: Option<Body>,
}

/// What a tool does, for choosing an icon and layout. Agent-agnostic (D3):
/// the server maps each agent tool onto one of these. An unknown kind from a
/// newer server decodes as [`ToolKind::Other`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    FileWrite,
    FileEdit,
    FileRead,
    Shell,
    Search,
    Fetch,
    Subagent,
    #[serde(other)]
    Other,
}

/// A bounded preview of some content, with the full content as a blob.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Body {
    pub format: BodyFormat,
    /// Head of the content: at most [`limits::PREVIEW_MAX_LINES`] lines and
    /// [`limits::PREVIEW_MAX_BYTES`] bytes, with real newlines.
    pub preview: String,
    /// Line count of the whole content.
    pub lines: u32,
    /// The whole content, when `preview` does not hold all of it.
    pub full: Option<BlobRef>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    Text,
    /// A unified diff.
    Diff,
}

/// A tool finished.
///
/// **v1 compatibility.** A v1 `tool_result` has only `summary` and `output`;
/// it decodes as `call: None`, `is_error: false`, `status: None`, the summary
/// verbatim as `preview` and its line count as `lines`. (A v1 server marked
/// failures with a `[error] ` summary prefix; it stays in the preview rather
/// than being guessed into `is_error`.)
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(try_from = "compat::ToolResultWire")]
pub struct ToolResult {
    /// The [`ToolCall`] this answers; `None` in v1 events or when the agent
    /// reported a result for no logged call.
    pub call: Option<CallId>,
    pub is_error: bool,
    /// Short outcome, at most [`limits::STATUS_MAX_CHARS`], e.g. `created`,
    /// `exit 1`, `3 matches`.
    pub status: Option<String>,
    /// Head of the output: at most [`limits::PREVIEW_MAX_LINES`] lines and
    /// [`limits::PREVIEW_MAX_BYTES`] bytes, with real newlines.
    pub preview: String,
    /// Line count of the whole output.
    pub lines: u32,
    /// The whole output, when `preview` does not hold all of it.
    pub output: Option<BlobRef>,
}

/// Decoders that accept both v1 and v2 tool events.
mod compat {
    use serde::Deserialize;

    use crate::{BlobRef, CallId, ToolCall, ToolDisplay, ToolKind, ToolResult};

    /// A `tool_call` as v1 (`summary`) or v2 (`call`, `display`) wrote it.
    #[derive(Deserialize)]
    pub(crate) struct ToolCallWire {
        #[serde(default)]
        call: Option<CallId>,
        name: String,
        #[serde(default)]
        display: Option<ToolDisplay>,
        /// v1 only.
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        input: Option<BlobRef>,
    }

    impl TryFrom<ToolCallWire> for ToolCall {
        type Error = &'static str;

        fn try_from(wire: ToolCallWire) -> Result<Self, Self::Error> {
            let display = match (wire.display, wire.summary) {
                (Some(display), _) => display,
                (None, Some(summary)) => ToolDisplay {
                    kind: ToolKind::Other,
                    title: summary,
                    detail: None,
                    body: None,
                },
                (None, None) => return Err("tool_call needs `display` (v2) or `summary` (v1)"),
            };
            Ok(ToolCall {
                call: wire.call,
                name: wire.name,
                display,
                input: wire.input,
            })
        }
    }

    /// A `tool_result` as v1 (`summary`) or v2 (`preview`, `lines`) wrote it.
    #[derive(Deserialize)]
    pub(crate) struct ToolResultWire {
        #[serde(default)]
        call: Option<CallId>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        preview: Option<String>,
        #[serde(default)]
        lines: Option<u32>,
        /// v1 only.
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        output: Option<BlobRef>,
    }

    impl TryFrom<ToolResultWire> for ToolResult {
        type Error = &'static str;

        fn try_from(wire: ToolResultWire) -> Result<Self, Self::Error> {
            let (preview, lines) = match (wire.preview, wire.lines, wire.summary) {
                (Some(preview), Some(lines), _) => (preview, lines),
                (None, None, Some(summary)) => {
                    let lines = u32::try_from(summary.lines().count()).unwrap_or(u32::MAX);
                    (summary, lines)
                }
                _ => return Err("tool_result needs `preview` and `lines` (v2) or `summary` (v1)"),
            };
            Ok(ToolResult {
                call: wire.call,
                is_error: wire.is_error,
                status: wire.status,
                preview,
                lines,
                output: wire.output,
            })
        }
    }
}

/// What one turn took, as the agent reported it when the turn ended.
/// Agent-agnostic (D3): figures for this turn alone, never running totals.
/// A figure the agent does not report is `None`, never 0; heads omit it.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct TurnUsage {
    /// Wall-clock time of the turn in milliseconds, time spent waiting for
    /// approvals included.
    pub duration_ms: Option<u64>,
    /// Tokens the model read over the turn's requests, cached ones included.
    pub tokens_in: Option<u64>,
    /// Tokens the model wrote over the turn's requests.
    pub tokens_out: Option<u64>,
    /// What the turn cost, in US dollars, as the agent priced it.
    pub cost_usd: Option<f64>,
    /// The model that ran the turn, as the agent names it.
    pub model: Option<String>,
}

impl TurnUsage {
    /// The agent reported nothing at all.
    pub fn is_empty(&self) -> bool {
        *self == TurnUsage::default()
    }
}

/// Agent state as recorded in the log.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    AgentWorking,
    Errored { message: String },
}

/// Answer to an approval request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Deny,
}

/// Body of `GET /v1/events`: consecutive events starting at seq `from`.
///
/// Seqs are contiguous (D12), so the i-th event has seq `from + i`. An empty
/// batch (the long-poll timed out) has `from = after + 1`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EventBatch {
    pub from: Seq,
    pub events: Vec<Event>,
}

impl EventBatch {
    /// Each event paired with its seq.
    pub fn iter(&self) -> impl Iterator<Item = (Seq, &Event)> {
        (self.from.0..).map(Seq).zip(&self.events)
    }

    /// Seq of the last event, or `None` for an empty batch. A client's
    /// cursor advances to this after applying the batch.
    pub fn last(&self) -> Option<Seq> {
        let n = u64::try_from(self.events.len()).ok()?.checked_sub(1)?;
        Some(Seq(self.from.0 + n))
    }
}

/// A client intent sent to the server (`POST /v1/commands`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Command {
    pub id: CommandId,
    pub kind: CommandKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandKind {
    /// Start a session in `workdir`, which must lie under one of the
    /// server's configured roots (D14).
    CreateSession {
        workdir: String,
    },
    SendMessage {
        session_id: SessionId,
        text: String,
    },
    /// Answer an approval. `rule` grants the request's suggestion with
    /// that index along with an approval; the server refuses it with a
    /// denial or an index the request does not offer.
    ResolveApproval {
        id: ApprovalId,
        decision: Decision,
        #[serde(default)]
        rule: Option<u8>,
    },
    /// Stop the agent's current turn in this session.
    Interrupt {
        session_id: SessionId,
    },
    /// Shut down the session's agent, interrupting its turn if one runs.
    /// Accepted whether or not the agent is running; the next message starts
    /// it again.
    StopSession {
        session_id: SessionId,
    },
    /// Name the session; `None` (or a name that is empty once normalised)
    /// restores the default title. The server logs the name as
    /// [`normalise_session_name`] leaves it. Accepted for any known session,
    /// even with the same name.
    RenameSession {
        session_id: SessionId,
        name: Option<String>,
    },
    /// Archive the session (`true`) or bring it back (`false`). Absolute,
    /// so a resend or a repeat cannot flip it twice; accepted for any known
    /// session, even when it is already so.
    SetSessionArchived {
        session_id: SessionId,
        archived: bool,
    },
}

/// A session name as `SessionRenamed` carries it: control characters
/// (newlines included) stripped, trimmed, and cut on a char boundary to
/// [`limits::SESSION_NAME_MAX_CHARS`]. `None` when nothing is left, which
/// restores the default title.
pub fn normalise_session_name(name: &str) -> Option<String> {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let name: String = clean
        .trim()
        .chars()
        .take(limits::SESSION_NAME_MAX_CHARS)
        .collect();
    // The cut may end on whitespace.
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Response to `POST /v1/commands`. Resending a [`CommandId`] returns the
/// original result, so it is stable across retries.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CommandResult {
    /// The command's event was placed in the log at `seq`.
    Accepted { seq: Seq },
    /// The command was refused for good; resending gives the same answer.
    Rejected { reason: RejectReason },
}

/// Why the server refused a command.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// `CreateSession.workdir` is outside the configured roots (D14).
    WorkdirNotAllowed,
    /// `CreateSession.workdir` does not exist or is not a directory.
    WorkdirNotFound,
    UnknownSession,
    UnknownApproval,
    /// Another answer already won (D5).
    ApprovalAlreadyResolved,
    /// The approval was cancelled before the answer arrived: its turn
    /// ended, its agent was stopped, or the daemon restarted. No answer can
    /// apply any more.
    ApprovalCancelled,
    /// `Interrupt` arrived while the session had no running turn.
    NoActiveTurn,
    /// `ResolveApproval.rule` came with a denial, or names no suggestion of
    /// the approval.
    UnknownRule,
}

impl RejectReason {
    /// Whether this refusal of a `ResolveApproval` rules out every answer:
    /// the approval is unknown, already answered, or cancelled. Answering
    /// again is pointless; only a refusal of the answer itself
    /// ([`RejectReason::UnknownRule`]) leaves the approval open to another.
    pub fn rules_out_answers(self) -> bool {
        matches!(
            self,
            RejectReason::UnknownApproval
                | RejectReason::ApprovalAlreadyResolved
                | RejectReason::ApprovalCancelled
        )
    }
}

/// Body of `POST /v1/pair`, the only endpoint reachable without a client
/// certificate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PairRequest {
    /// One-time code printed by `skywaved pair`.
    pub code: String,
    /// PEM-encoded certificate signing request for the device's keypair.
    pub csr_pem: String,
    /// Human-readable device name, shown by `skywaved devices list`.
    pub device_name: String,
}

/// Response to `POST /v1/pair`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PairResponse {
    pub device_id: DeviceId,
    /// PEM-encoded device certificate signed by the server's CA.
    pub device_cert_pem: String,
    /// PEM-encoded CA certificate; the device pins it for later connections.
    pub ca_cert_pem: String,
}

/// Response to `GET /v1/hello`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HelloResponse {
    pub protocol_version: u32,
    pub server_id: ServerId,
    /// The device the caller's certificate belongs to.
    pub device_id: DeviceId,
    /// Seq of the newest event; [`Seq::ZERO`] when the log is empty.
    pub head: Seq,
    /// Every device ever paired with the server, revoked ones included:
    /// the log names devices by id only, and its old events still name
    /// the revoked.
    pub devices: Vec<DeviceInfo>,
}

/// A paired device as other devices show it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    /// The name it paired with ([`PairRequest::device_name`]).
    pub name: String,
}

/// Query string of `GET /v1/events`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EventsQuery {
    /// Return events with seq greater than this.
    pub after: Seq,
    /// Seconds to block when no events are available; absent means no wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u32>,
    /// Maximum number of events to return; absent leaves it to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Query string of `GET /v1/dirs`: which server directory to list for
/// workdir completion. Additive to v3: an older server answers 404.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DirsQuery {
    /// Absolute directory to list; empty asks for the configured roots.
    #[serde(default)]
    pub dir: String,
}

/// Response to `GET /v1/dirs`, always 200: every outcome is a domain answer.
///
/// Only directories under the configured roots are ever named (D14). The
/// server does no prefix filtering; the client filters what it shows.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DirsResult {
    /// `dir` was empty: the canonical roots that exist.
    Roots { roots: Vec<String> },
    /// The subdirectories of `dir` (canonical), sorted. `truncated` when the
    /// scan or reply cap cut the listing short.
    Listed {
        dir: String,
        names: Vec<String>,
        truncated: bool,
    },
    /// `dir` is relative or outside every root; the roots are offered
    /// instead.
    NotAllowed { roots: Vec<String> },
    /// `dir` does not exist or is not a directory.
    NotFound,
}

/// HTTP paths of the v1 API.
pub mod path {
    use crate::BlobId;

    pub const PAIR: &str = "/v1/pair";
    pub const HELLO: &str = "/v1/hello";
    pub const COMMANDS: &str = "/v1/commands";
    pub const EVENTS: &str = "/v1/events";
    pub const DIRS: &str = "/v1/dirs";
    /// Route template for blob downloads; `{id}` is a [`BlobId`].
    pub const BLOB_ROUTE: &str = "/v1/blobs/{id}";

    /// Concrete path for downloading one blob.
    pub fn blob(id: BlobId) -> String {
        format!("/v1/blobs/{}", id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_effect_sets_on_archive_and_clears_on_a_message() {
        let by = DeviceId::new();
        for archived in [true, false] {
            assert_eq!(
                EventKind::SessionArchived { archived, by }.archived_effect(),
                Some(archived)
            );
        }
        let message = EventKind::UserMessage {
            device_id: by,
            text: "back".into(),
        };
        assert_eq!(message.archived_effect(), Some(false));
        for kind in [
            EventKind::AgentMessage { text: "x".into() },
            EventKind::SessionStopped,
            EventKind::SessionRenamed {
                name: Some("x".into()),
                by,
            },
            EventKind::SessionStateChanged(SessionState::Idle),
        ] {
            assert_eq!(kind.archived_effect(), None, "{kind:?}");
        }
    }
}
