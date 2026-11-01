//! Skywave wire types, shared by `skywaved` and `skywave-core`.
//!
//! Encoded as JSON for the MVP. Large content is never inlined; it travels as
//! a [`BlobRef`] and is fetched separately. No type here names a concrete
//! agent (D3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod context_name;
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
///
/// v7: `CommandKind::SetPermissionMode` and `CommandKind::SetModel`; their
/// `…Requested` events, the agent's `…Changed` and `…Refused` answers, and
/// `EventKind::ModelsOffered`.
///
/// v8: `CommandKind::SetEffort` with its `EffortRequested`, `EffortChanged`
/// and `EffortRefused` events, and the levels each model takes in
/// [`ModelChoice::efforts`].
///
/// v9: session contexts. [`Event::context`] stamps every event with the
/// [`ContextId`] it belongs to; `CommandKind::ClearContext` and
/// `CommandKind::SwitchContext` with their `EventKind::ContextStarted` and
/// `EventKind::ContextSwitched`; the first context's name in
/// `EventKind::SessionCreated::context_name`; the context a
/// `CommandKind::SendMessage` was written in; and
/// `RejectReason::{ContextExists, UnknownContext, ContextChanged}`.
pub const PROTOCOL_VERSION: u32 = 9;

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
uuid_id!(
    /// Identifies one context of a session: a span of its log with its own
    /// agent conversation. A session has one current context at a time; its
    /// first context has the session's own id ([`ContextId::first_of`]).
    /// Users see a context's name, never its id. A cleared context's id is
    /// chosen by the client that clears (`CommandKind::ClearContext`).
    ContextId
);

impl ContextId {
    /// The first context of `session`, which has the session's own id.
    pub fn first_of(session: SessionId) -> ContextId {
        ContextId(session.0)
    }
}

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
    /// The context of its session the event belongs to, decided by the
    /// server when it logs the event; never derived from the event's
    /// position. Session-level state (name, archived, settings) folds per
    /// session regardless.
    pub context: ContextId,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A session was created in `workdir`, a directory on the server (D14).
    /// Its first context, which has the session's id, is named
    /// `context_name` by the server.
    SessionCreated {
        workdir: String,
        context_name: String,
    },
    /// A `ClearContext` command started a new, empty context named `name`
    /// and made it current; the event is stamped with the new context. The
    /// agent starts afresh in it.
    ContextStarted {
        by: DeviceId,
        name: String,
    },
    /// A `SwitchContext` command made the context the event is stamped with
    /// current again; the agent resumes that context's conversation.
    ContextSwitched {
        by: DeviceId,
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
    /// A `SetPermissionMode` command asked the agent for `mode`. Answered
    /// by a `PermissionModeChanged` or `PermissionModeRefused` naming the
    /// command; see [`AgentSettings`].
    PermissionModeRequested {
        mode: PermissionMode,
        by: DeviceId,
    },
    /// The agent runs in `mode` from here on. `answers` names the
    /// `SetPermissionMode` command it carries out; `None` when the agent
    /// changed on its own (e.g. leaving plan mode once a plan is approved)
    /// or reported the mode it started in.
    PermissionModeChanged {
        mode: PermissionMode,
        answers: Option<CommandId>,
    },
    /// The agent refused to switch to `mode` and keeps the mode it had.
    /// `answers` names the command refused; `None` when a mode reapplied on
    /// a restart was refused.
    PermissionModeRefused {
        mode: PermissionMode,
        reason: String,
        answers: Option<CommandId>,
    },
    /// A `SetModel` command asked the agent for `model`, one of the ids
    /// [`ModelsOffered`](EventKind::ModelsOffered) lists or a free name.
    ModelRequested {
        model: String,
        by: DeviceId,
    },
    /// The agent runs `model` from here on, as it names the model (which
    /// may differ from the id asked for, e.g. an alias resolved). `answers`
    /// as for `PermissionModeChanged`.
    ModelChanged {
        model: String,
        answers: Option<CommandId>,
    },
    /// The agent refused to run `model` (the id asked for) and keeps the
    /// model it had. `answers` as for `PermissionModeRefused`.
    ModelRefused {
        model: String,
        reason: String,
        answers: Option<CommandId>,
    },
    /// The models the agent offers, logged when they differ from the last
    /// list logged in any session: heads offer the newest list for every
    /// session. At most [`limits::MODELS_MAX`].
    ModelsOffered {
        models: Vec<ModelChoice>,
    },
    /// A `SetEffort` command asked the agent to reason at `effort`.
    EffortRequested {
        effort: Effort,
        by: DeviceId,
    },
    /// The agent reasons at `effort` from here on; `None` when the model it
    /// runs takes no effort level. `answers` as for `PermissionModeChanged`.
    EffortChanged {
        effort: Option<Effort>,
        answers: Option<CommandId>,
    },
    /// The agent refused to reason at `effort` and keeps the level it had.
    /// `answers` as for `PermissionModeRefused`.
    EffortRefused {
        effort: Effort,
        reason: String,
        answers: Option<CommandId>,
    },
}

impl EventKind {
    /// The device whose command logged the event, for events that name one.
    pub fn device(&self) -> Option<DeviceId> {
        match self {
            EventKind::UserMessage { device_id, .. } => Some(*device_id),
            EventKind::ApprovalResolved { by, .. }
            | EventKind::SessionRenamed { by, .. }
            | EventKind::SessionArchived { by, .. }
            | EventKind::PermissionModeRequested { by, .. }
            | EventKind::ModelRequested { by, .. }
            | EventKind::EffortRequested { by, .. }
            | EventKind::ContextStarted { by, .. }
            | EventKind::ContextSwitched { by } => Some(*by),
            EventKind::SessionCreated { .. }
            | EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_)
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. } => None,
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
            | EventKind::SessionArchived { .. }
            | EventKind::PermissionModeRequested { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. } => true,
            EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_)
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. } => false,
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
            | EventKind::TurnUsage(_)
            | EventKind::PermissionModeRequested { .. }
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. } => None,
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
    /// Longest context name (`SessionCreated::context_name`,
    /// `ClearContext`, `ContextStarted`), in chars.
    pub const CONTEXT_NAME_MAX_CHARS: usize = 40;
    /// Longest [`RuleSuggestion::label`](crate::RuleSuggestion::label), in
    /// chars.
    pub const RULE_LABEL_MAX_CHARS: usize = 120;
    /// Most [`RuleSuggestion`](crate::RuleSuggestion)s on one approval.
    pub const RULE_SUGGESTIONS_MAX: usize = 8;
    /// Most [`ModelChoice`](crate::ModelChoice)s in one `ModelsOffered`.
    pub const MODELS_MAX: usize = 32;
    /// Longest model id or name (`SetModel`, `ModelChoice::id`,
    /// `ModelChoice::resolved`, the model events), in chars.
    pub const MODEL_MAX_CHARS: usize = 80;
    /// Longest [`ModelChoice::label`](crate::ModelChoice::label), in chars.
    pub const MODEL_LABEL_MAX_CHARS: usize = 60;
    /// Longest [`ModelChoice::detail`](crate::ModelChoice::detail), in
    /// chars.
    pub const MODEL_DETAIL_MAX_CHARS: usize = 120;
    /// Longest refusal reason (`PermissionModeRefused`, `ModelRefused`), in
    /// chars.
    pub const REFUSAL_MAX_CHARS: usize = 200;
}

/// How freely the agent acts without asking. Agent-agnostic (D3): the
/// adapter maps each onto what its agent calls it, and an agent may refuse
/// one it cannot run in (`PermissionModeRefused`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Asks before any tool no rule allows.
    Ask,
    /// File edits in the workdir run without asking.
    AcceptEdits,
    /// Plans only; the agent asks to leave plan mode with its plan.
    Plan,
    /// The agent decides what to ask about.
    Auto,
    /// Never asks: what no rule allows is denied.
    DontAsk,
    /// Never asks: everything runs.
    Bypass,
}

impl PermissionMode {
    /// Every mode, from the most asking to the least.
    pub const ALL: [PermissionMode; 6] = [
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::Auto,
        PermissionMode::DontAsk,
        PermissionMode::Bypass,
    ];
}

/// How hard the model reasons before answering. Agent-agnostic (D3): the
/// adapter maps each onto what its agent calls it, and an agent may refuse
/// one its model does not take (`EffortRefused`).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    /// Above `High`; `snake_case` alone would name it `x_high`.
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl Effort {
    /// Every level, from the least reasoning to the most.
    pub const ALL: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];
}

/// A model the agent offers. Agent-agnostic (D3): clients show `label`
/// and `detail` and send `id` back in `SetModel`; they never parse them.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelChoice {
    /// What `SetModel` sends, at most [`limits::MODEL_MAX_CHARS`].
    pub id: String,
    /// One line, at most [`limits::MODEL_LABEL_MAX_CHARS`].
    pub label: String,
    /// Optional second line, at most [`limits::MODEL_DETAIL_MAX_CHARS`].
    pub detail: Option<String>,
    /// The model `ModelChanged` names once this choice runs, when the agent
    /// says; several choices may resolve to one model.
    pub resolved: Option<String>,
    /// The effort levels this model takes, least first; empty when it takes
    /// none. Defaults to empty, so a choice logged before v8 still decodes.
    #[serde(default)]
    pub efforts: Vec<Effort>,
}

/// A session's permission mode, model and effort as its log has them: the one rule
/// the server and every client fold (see [`AgentSettings::apply`]).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct AgentSettings {
    pub mode: Setting<PermissionMode>,
    pub model: Setting<String>,
    /// `current: Some(None)` when the running model takes no effort level,
    /// as against `None`, not known yet. Requests and refusals always hold
    /// a level: read them as one with [`Setting::requested_effort`] and
    /// [`Setting::refused_effort`].
    pub effort: Setting<Option<Effort>>,
}

/// One setting of a session's agent, folded from the log.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Setting<T> {
    /// What the agent last said it runs with; `None` until it says.
    pub current: Option<T>,
    /// The newest request no answer has named yet, by its command.
    pub requested: Option<Requested<T>>,
    /// The newest refusal, until the next request or change.
    pub refused: Option<Refusal<T>>,
}

impl<T> Default for Setting<T> {
    fn default() -> Self {
        Setting {
            current: None,
            requested: None,
            refused: None,
        }
    }
}

/// A request for a setting, waiting for the agent's answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Requested<T> {
    pub value: T,
    pub command: CommandId,
    pub by: DeviceId,
}

/// The agent refused `value` with `reason`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Refusal<T> {
    pub value: T,
    pub reason: String,
}

impl<T: Clone> Setting<T> {
    /// What the agent should run with: the open request, else the current
    /// value. What a restarted agent is given.
    pub fn wanted(&self) -> Option<&T> {
        self.requested
            .as_ref()
            .map(|r| &r.value)
            .or(self.current.as_ref())
    }

    fn request(&mut self, value: &T, command: Option<CommandId>, by: DeviceId) {
        self.refused = None;
        self.requested = command.map(|command| Requested {
            value: value.clone(),
            command,
            by,
        });
    }

    fn change(&mut self, value: &T, answers: Option<CommandId>) {
        self.current = Some(value.clone());
        self.refused = None;
        self.settle(answers);
    }

    fn refuse(&mut self, value: &T, reason: &str, answers: Option<CommandId>) {
        self.refused = Some(Refusal {
            value: value.clone(),
            reason: reason.to_owned(),
        });
        self.settle(answers);
    }

    /// An answer naming the open request settles it; any other leaves it.
    fn settle(&mut self, answers: Option<CommandId>) {
        if answers.is_some() && self.requested.as_ref().map(|r| r.command) == answers {
            self.requested = None;
        }
    }
}

impl Setting<Option<Effort>> {
    /// The open request as the level asked for. Every request holds one
    /// (`EffortRequested` carries a plain [`Effort`]), so `None` only when
    /// no request is open.
    pub fn requested_effort(&self) -> Option<Requested<Effort>> {
        let requested = self.requested.as_ref()?;
        Some(Requested {
            value: requested.value?,
            command: requested.command,
            by: requested.by,
        })
    }

    /// The newest refusal as the level refused, as
    /// [`requested_effort`](Self::requested_effort) is for the request.
    pub fn refused_effort(&self) -> Option<Refusal<Effort>> {
        let refused = self.refused.as_ref()?;
        Some(Refusal {
            value: refused.value?,
            reason: refused.reason.clone(),
        })
    }
}

impl AgentSettings {
    /// Folds one event of the session, logged by command `command_id`.
    ///
    /// A request is open until an answer names its command: a change or a
    /// refusal of an older request, or one the agent made on its own,
    /// leaves it open. A newer request replaces it. A change sets the
    /// current value whatever it answers. A refusal keeps the current value
    /// and is shown until the next request or change.
    pub fn apply(&mut self, command_id: Option<CommandId>, kind: &EventKind) {
        match kind {
            EventKind::PermissionModeRequested { mode, by } => {
                self.mode.request(mode, command_id, *by);
            }
            EventKind::PermissionModeChanged { mode, answers } => self.mode.change(mode, *answers),
            EventKind::PermissionModeRefused {
                mode,
                reason,
                answers,
            } => self.mode.refuse(mode, reason, *answers),
            EventKind::ModelRequested { model, by } => self.model.request(model, command_id, *by),
            EventKind::ModelChanged { model, answers } => self.model.change(model, *answers),
            EventKind::ModelRefused {
                model,
                reason,
                answers,
            } => self.model.refuse(model, reason, *answers),
            EventKind::EffortRequested { effort, by } => {
                self.effort.request(&Some(*effort), command_id, *by);
            }
            EventKind::EffortChanged { effort, answers } => self.effort.change(effort, *answers),
            EventKind::EffortRefused {
                effort,
                reason,
                answers,
            } => self.effort.refuse(&Some(*effort), reason, *answers),
            _ => {}
        }
    }

    /// Whether `kind` is one [`apply`](Self::apply) folds.
    pub fn folds(kind: &EventKind) -> bool {
        matches!(
            kind,
            EventKind::PermissionModeRequested { .. }
                | EventKind::PermissionModeChanged { .. }
                | EventKind::PermissionModeRefused { .. }
                | EventKind::ModelRequested { .. }
                | EventKind::ModelChanged { .. }
                | EventKind::ModelRefused { .. }
                | EventKind::EffortRequested { .. }
                | EventKind::EffortChanged { .. }
                | EventKind::EffortRefused { .. }
        )
    }
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
    CreateSession { workdir: String },
    /// Send `text` to the agent. `context` is the session's context the
    /// message was written in; the server refuses it as `ContextChanged`
    /// when that is no longer the current one.
    SendMessage {
        session_id: SessionId,
        context: ContextId,
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
    Interrupt { session_id: SessionId },
    /// Shut down the session's agent, interrupting its turn if one runs.
    /// Accepted whether or not the agent is running; the next message starts
    /// it again.
    StopSession { session_id: SessionId },
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
    /// Ask the session's agent to run in `mode`, starting the agent if it
    /// is not running. Accepted for any known session; the agent's answer
    /// comes as a `PermissionModeChanged` or `PermissionModeRefused` naming
    /// this command.
    SetPermissionMode {
        session_id: SessionId,
        mode: PermissionMode,
    },
    /// Ask the session's agent to run `model`, as [`SetPermissionMode`]
    /// does for a mode. `model` is cut to [`limits::MODEL_MAX_CHARS`]; an
    /// empty one is `UnknownModel`.
    ///
    /// [`SetPermissionMode`]: CommandKind::SetPermissionMode
    SetModel {
        session_id: SessionId,
        model: String,
    },
    /// Ask the session's agent to reason at `effort`, as
    /// [`SetPermissionMode`] does for a mode; the answer is an
    /// `EffortChanged` or `EffortRefused` naming this command.
    ///
    /// [`SetPermissionMode`]: CommandKind::SetPermissionMode
    SetEffort {
        session_id: SessionId,
        effort: Effort,
    },
    /// Start a new, empty context `context` (an id the client chose) named
    /// `name` and make it current: the agent starts afresh. The server logs
    /// the name as [`normalise_context_name`] leaves it, or a generated one
    /// ([`context_name::random`]) when nothing is left. `ContextExists` if
    /// `context` is already a context of any session.
    ClearContext {
        session_id: SessionId,
        context: ContextId,
        name: String,
    },
    /// Make `context`, one of the session's contexts, current again: the
    /// agent resumes it. Absolute: accepted even when it is already
    /// current. `UnknownContext` if the session has no such context.
    SwitchContext {
        session_id: SessionId,
        context: ContextId,
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

/// A context name as `ContextStarted` carries it: control characters
/// stripped, trimmed, and cut on a char boundary to
/// [`limits::CONTEXT_NAME_MAX_CHARS`]. `None` when nothing is left; the
/// server then names the context itself rather than refuse the command.
pub fn normalise_context_name(name: &str) -> Option<String> {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let name: String = clean
        .trim()
        .chars()
        .take(limits::CONTEXT_NAME_MAX_CHARS)
        .collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

/// A model id as `ModelRequested` carries it: control characters stripped,
/// trimmed and cut on a char boundary to [`limits::MODEL_MAX_CHARS`].
/// `None` when nothing is left.
pub fn normalise_model(model: &str) -> Option<String> {
    let clean: String = model.chars().filter(|c| !c.is_control()).collect();
    let model: String = clean.trim().chars().take(limits::MODEL_MAX_CHARS).collect();
    let model = model.trim_end();
    (!model.is_empty()).then(|| model.to_owned())
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
    /// `SetModel.model` is blank.
    UnknownModel,
    /// `ClearContext.context` is already a context, of this session or
    /// another.
    ContextExists,
    /// `SwitchContext.context` is not a context of the session.
    UnknownContext,
    /// `SendMessage.context` is no longer the session's current context: a
    /// device cleared or switched contexts after the message was written.
    ContextChanged,
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

    #[test]
    fn a_request_stays_open_until_an_answer_names_it() {
        let by = DeviceId::new();
        let (first, second) = (CommandId::new(), CommandId::new());
        let mut settings = AgentSettings::default();
        let request = |mode| EventKind::PermissionModeRequested { mode, by };
        let changed = |mode, answers| EventKind::PermissionModeChanged { mode, answers };

        settings.apply(None, &changed(PermissionMode::Ask, None));
        settings.apply(Some(first), &request(PermissionMode::Plan));
        settings.apply(Some(second), &request(PermissionMode::AcceptEdits));
        assert_eq!(settings.mode.wanted(), Some(&PermissionMode::AcceptEdits));
        // The older request's answer changes the mode but leaves the newer open.
        settings.apply(None, &changed(PermissionMode::Plan, Some(first)));
        assert_eq!(settings.mode.current, Some(PermissionMode::Plan));
        assert_eq!(
            settings.mode.requested.as_ref().map(|r| r.command),
            Some(second)
        );
        // So does a change the agent made on its own.
        settings.apply(None, &changed(PermissionMode::Ask, None));
        assert!(settings.mode.requested.is_some());
        settings.apply(None, &changed(PermissionMode::AcceptEdits, Some(second)));
        assert_eq!(settings.mode.requested, None);
        assert_eq!(settings.mode.wanted(), Some(&PermissionMode::AcceptEdits));
    }

    #[test]
    fn a_refusal_keeps_the_current_value_until_the_next_request() {
        let by = DeviceId::new();
        let command = CommandId::new();
        let mut settings = AgentSettings::default();
        settings.apply(
            None,
            &EventKind::ModelChanged {
                model: "m-1".into(),
                answers: None,
            },
        );
        let request = EventKind::ModelRequested {
            model: "bogus".into(),
            by,
        };
        settings.apply(Some(command), &request);
        settings.apply(
            None,
            &EventKind::ModelRefused {
                model: "bogus".into(),
                reason: "not found".into(),
                answers: Some(command),
            },
        );
        assert_eq!(settings.model.current.as_deref(), Some("m-1"));
        assert_eq!(settings.model.wanted().map(String::as_str), Some("m-1"));
        assert_eq!(
            settings.model.refused.as_ref().map(|r| r.reason.as_str()),
            Some("not found")
        );
        settings.apply(Some(CommandId::new()), &request);
        assert_eq!(settings.model.refused, None);
    }

    #[test]
    fn an_effort_request_settles_on_its_change_and_survives_older_answers() {
        let by = DeviceId::new();
        let (first, second) = (CommandId::new(), CommandId::new());
        let mut settings = AgentSettings::default();
        let request = |effort| EventKind::EffortRequested { effort, by };
        let changed = |effort, answers| EventKind::EffortChanged { effort, answers };

        settings.apply(Some(first), &request(Effort::Low));
        settings.apply(Some(second), &request(Effort::XHigh));
        assert_eq!(settings.effort.wanted(), Some(&Some(Effort::XHigh)));
        // The older request's answer changes the level but leaves the newer open.
        settings.apply(None, &changed(Some(Effort::Low), Some(first)));
        assert_eq!(settings.effort.current, Some(Some(Effort::Low)));
        assert_eq!(
            settings.effort.requested_effort(),
            Some(Requested {
                value: Effort::XHigh,
                command: second,
                by
            })
        );
        settings.apply(None, &changed(Some(Effort::XHigh), Some(second)));
        assert_eq!(settings.effort.requested, None);
        assert_eq!(settings.effort.requested_effort(), None);
        assert_eq!(settings.effort.current, Some(Some(Effort::XHigh)));
    }

    #[test]
    fn an_effort_refusal_keeps_the_current_level() {
        let by = DeviceId::new();
        let command = CommandId::new();
        let mut settings = AgentSettings::default();
        settings.apply(
            None,
            &EventKind::EffortChanged {
                effort: Some(Effort::High),
                answers: None,
            },
        );
        settings.apply(
            Some(command),
            &EventKind::EffortRequested {
                effort: Effort::Max,
                by,
            },
        );
        settings.apply(
            None,
            &EventKind::EffortRefused {
                effort: Effort::Max,
                reason: "not on this model".into(),
                answers: Some(command),
            },
        );
        assert_eq!(settings.effort.current, Some(Some(Effort::High)));
        assert_eq!(settings.effort.requested, None);
        assert_eq!(
            settings.effort.refused_effort(),
            Some(Refusal {
                value: Effort::Max,
                reason: "not on this model".into()
            })
        );
    }

    #[test]
    fn a_model_without_effort_folds_to_not_applicable() {
        let mut settings = AgentSettings::default();
        assert_eq!(settings.effort.current, None);
        let kind = EventKind::EffortChanged {
            effort: None,
            answers: None,
        };
        assert!(AgentSettings::folds(&kind));
        settings.apply(None, &kind);
        assert_eq!(settings.effort.current, Some(None));
        assert_eq!(settings.effort.wanted(), Some(&None));
    }

    #[test]
    fn context_events_name_their_device_and_are_logged_by_command() {
        let by = DeviceId::new();
        for kind in [
            EventKind::ContextStarted {
                by,
                name: "velvet walrus".into(),
            },
            EventKind::ContextSwitched { by },
        ] {
            assert_eq!(kind.device(), Some(by), "{kind:?}");
            assert!(kind.logged_by_command(), "{kind:?}");
            assert_eq!(kind.archived_effect(), None, "{kind:?}");
        }
        // Positive control: the classifiers do tell kinds apart.
        let created = EventKind::SessionCreated {
            workdir: "/w".into(),
            context_name: "start".into(),
        };
        assert_eq!(created.device(), None);
        assert!(!EventKind::AgentMessage { text: "x".into() }.logged_by_command());
    }

    #[test]
    fn a_context_name_is_normalised_like_a_session_name() {
        assert_eq!(
            normalise_context_name("  velvet\nwalrus \t"),
            Some("velvetwalrus".to_owned())
        );
        assert_eq!(normalise_context_name(" \n "), None);
        let long = format!("{} tail", "n".repeat(39));
        // Cut at the cap, then the trailing space the cut left is trimmed.
        assert_eq!(normalise_context_name(&long), Some("n".repeat(39)));
        let long = "n".repeat(100);
        assert_eq!(
            normalise_context_name(&long).map(|n| n.chars().count()),
            Some(limits::CONTEXT_NAME_MAX_CHARS)
        );
    }

    #[test]
    fn a_model_is_normalised_like_a_name() {
        assert_eq!(normalise_model("  sonnet\n"), Some("sonnet".to_owned()));
        assert_eq!(normalise_model(" \t "), None);
        let long = "m".repeat(200);
        assert_eq!(
            normalise_model(&long).map(|m| m.chars().count()),
            Some(limits::MODEL_MAX_CHARS)
        );
    }
}
