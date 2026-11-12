//! Skywave wire types, shared by `skywaved` and `skywave-core`.
//!
//! Encoded as JSON for the MVP. Large content is never inlined; it travels as
//! a [`BlobRef`] and is fetched separately. No type here names a concrete
//! agent (D3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod content_hash;
pub mod context_name;
pub mod crop;
mod fingerprint;

pub use content_hash::{ContentHash, InvalidContentHash};
pub use fingerprint::{Fingerprint, InvalidFingerprint};

/// Wire protocol version. Bump on any incompatible change to these types.
///
/// v2: tool calls carry a [`CallId`] and a bounded [`ToolDisplay`] instead of
/// a free-form summary.
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
///
/// v10: moving a context into a new session. `CommandKind::MoveContext`
/// with its `EventKind::ContextMoved`, the [`ContextRef`] a moved context
/// went on from in `EventKind::SessionCreated::from`, and
/// `RejectReason::{ContextMoved, ContextCurrent, NoConversation}`.
///
/// v11: background work. `EventKind::BackgroundTasksChanged` and
/// `EventKind::SchedulesChanged`, each the whole set of the agent's
/// [`BackgroundTask`]s or [`Schedule`]s, folded into [`AgentWork`];
/// `CommandKind::StopTask` with its `EventKind::{TaskStopRequested,
/// TaskStopRefused}` and `RejectReason::UnknownTask`.
///
/// v12: adopting a conversation the agent had outside Skywave.
/// `CommandKind::CreateSession::conversation`, the agent's own id for it;
/// [`SessionOrigin`] in `EventKind::SessionCreated::from`, which now names
/// either a moved context or such a conversation; and
/// `RejectReason::BadConversation`.
///
/// v13: subagent transcripts. `parent` on `EventKind::AgentMessage` and
/// [`ToolCall`], the `Agent` call that started the subagent speaking
/// (`None` for the main agent); [`BackgroundTask::call`]; and
/// `EventKind::CallsUnanswered`, calls that will never get a result. v1 tool
/// shapes no longer decode, superseding the v2 note above. v12 databases are
/// deleted, not migrated.
///
/// v14: shell commands. `CommandKind::{RunShell, StopShell}` with
/// [`RunWhen`]; `EventKind::{ShellRequested, ShellStopRequested,
/// ShellStarted, ShellOutput, ShellFinished}` and [`ShellEnd`];
/// `RejectReason::{ShellDisabled, UnknownRun, RunFinished,
/// BadShellCommand}`; [`limits::SHELL_COMMAND_MAX_BYTES`]; and `shell` on
/// [`HelloResponse`].
///
/// v15: reading a session's workdir. `GET` [`path::WORKDIR_CHANGES`],
/// [`path::WORKDIR_LIST`] and [`path::WORKDIR_FILE`], each taking a
/// [`WorkdirQuery`] and answering [`ChangesResult`] (with [`ChangeSet`],
/// [`ChangedFile`], [`FileChange`], [`DiffBody`]), [`ListResult`] (with
/// [`DirEntry`], [`EntryKind`], [`LinkTarget`]) or [`FileResult`] (with
/// [`FileBody`]); and [`limits::WORKDIR_CHANGES_MAX`],
/// [`limits::WORKDIR_DIFF_MAX_BYTES`], [`limits::WORKDIR_DIFFS_MAX_BYTES`],
/// [`limits::WORKDIR_FILE_MAX_BYTES`] and [`limits::WORKDIR_LIST_MAX`].
///
/// v16: outside conversations. `EventKind::Imported` (an [`ImportedItem`]
/// of an adopted conversation's history) and `EventKind::ImportFinished`
/// (with [`ImportOutcome`]); `GET` [`path::CONVERSATIONS`] answering
/// [`ConversationsResult`] (with [`OutsideConversation`]); and
/// [`limits::CONVERSATIONS_MAX`].
///
/// v17: bypass by opt-in. `bypass` on [`HelloResponse`] and
/// `RejectReason::BypassDisabled`.
///
/// v18: bounded messages. [`limits::MESSAGE_MAX_BYTES`] and
/// `RejectReason::MessageTooLong`. A log that went backwards: `log` on
/// [`HelloResponse`] ([`LogRun`], [`LogId`], [`RunId`], [`RunsSince`]),
/// asked about with [`HelloQuery`]; the run a device expects, on every
/// other request, in the [`RUN_HEADER`] header (412 when it is not the
/// server's). `EventKind::AgentVersionUnconfirmed` with
/// [`limits::AGENT_VERSION_MAX_CHARS`]: an agent version the adapter was
/// not checked against. `sha256` on [`BlobRef`] ([`ContentHash`]): what a
/// device checks a downloaded blob against before it is available.
///
/// v19: content the server could not keep. [`Full`] in place of a bare
/// [`BlobRef`] on `ToolCall::input`, `Body::full`, `ToolResult::output`
/// and `ApprovalRequested::input`: `Full::Lost` when the preview is cut and
/// its whole content could not be stored.
///
/// v20: [`RejectReason::DiskLow`]: a command that would start new work,
/// refused while the server's disk is below its free-space floor.
///
/// v21: [`RejectReason::ServerStopping`]: a command that would start new
/// work, refused while the server waits for its running turns and shell
/// runs to end before it stops.
///
/// v22: the storage view. `GET` [`path::STORAGE`] answering
/// [`StorageReport`] (with [`SessionStorage`], [`BlobStorage`],
/// [`BlobUse`] and [`ShellRunStorage`]), and
/// [`limits::STORAGE_TOP_MAX`].
///
/// v23: storage actions. `CommandKind::DeleteBlob` logged as
/// `EventKind::BlobDeleted`, and `CommandKind::CropBlob` (keeping
/// `LineSpan`s) logged as `EventKind::BlobCropped`; the refusals
/// `UnknownBlob`, `BlobGone`, `NotCroppable` and `BadCrop`; and
/// `output_blob` on [`ShellRunStorage`], the run's blob a device acts on.
///
/// v24: [`Full::Inline`]: a tool result's whole output of at most
/// [`limits::INLINE_MAX_BYTES`] travels in the event instead of a blob.
/// Long messages: `full` on `EventKind::AgentMessage` and on
/// `ImportedItem::UserMessage` and `ImportedItem::AgentMessage`, the whole
/// text as a [`BlobUse::Message`] blob of a message longer than
/// [`limits::MESSAGE_WHOLE_MAX_BYTES`], whose `text` is then its first
/// [`limits::MESSAGE_PREVIEW_MAX_BYTES`].
/// Compressed transport ([`encoding`]): event batches and blob bodies as
/// zstd frames when asked for, blob ranges counting decoded bytes.
///
/// v25: how full a context is and what a subagent ran. `context_tokens` and
/// `context_window` on [`TurnUsage`], and `EventKind::SubagentUsage`
/// ([`SubagentUsage`]): a finished subagent's own figures, by its call.
///
/// v26: questions the agent asks with options to pick from:
/// `EventKind::QuestionAsked` ([`Question`]) and `QuestionAnswered`
/// ([`QuestionAnswer`]), sharing an approval's ids and life;
/// `CommandKind::AnswerQuestion` and `RejectReason::BadAnswer`.
///
/// v27: the agent's usage limits. `GET` [`path::USAGE_LIMITS`] answering
/// [`UsageLimitsResult`] (with [`UsageLimit`], [`LimitWindow`] and
/// [`LimitSeverity`]).
///
/// v28: `login_expires` on [`HelloResponse`]: when the agent's login
/// expires, so heads warn before the agent stops starting.
///
/// v29: logging the agent in from a device. `agent_login_offered` and
/// `agent_logged_out` on [`HelloResponse`]; `POST` [`path::AGENT_LOGIN_START`]
/// answering [`LoginStartResult`] (with [`LoginFlowId`]) and `POST`
/// [`path::AGENT_LOGIN_FINISH`] taking [`LoginFinish`] and answering
/// [`LoginFinishResult`]; [`limits::LOGIN_URL_MAX_BYTES`] and
/// [`limits::LOGIN_CODE_MAX_BYTES`].
///
/// v30: image attachments on user messages: [`ImageType`], [`Attachment`],
/// [`BlobUse::Attachment`], `RejectReason`s `UnknownAttachment`,
/// `TooManyAttachments` and `AttachmentsTooLarge`, the
/// `limits::ATTACHMENT*` consts, and `HEAD` and `PUT`
/// [`path::ATTACHMENT_ROUTE`] answering [`UploadResult`].
///
/// v31: the workdir commit log: `GET` [`path::WORKDIR_LOG`] answering
/// [`LogResult`] ([`CommitLog`], [`Commit`]) and [`limits::WORKDIR_LOG_MAX`].
pub const PROTOCOL_VERSION: u32 = 31;

/// The oldest [`PROTOCOL_VERSION`] whose stored data (the server's database,
/// a device's replica) this build migrates (D43). Each later version has
/// one migration step on each side, from the version before it, and a bump
/// does not compile until both have one. Data from before it is refused.
pub const MIGRATED_SINCE: u32 = 23;

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

uuid_id!(
    /// Identifies one server event log: minted when the server's database
    /// is created and kept in it, so a new database is a new log, while a
    /// copy restored from a backup keeps its id ([`LogRun`]).
    LogId
);
uuid_id!(
    /// Identifies one run of the server process: minted at every start
    /// ([`LogRun`]).
    RunId
);

impl ContextId {
    /// The first context of `session`, which has the session's own id.
    pub fn first_of(session: SessionId) -> ContextId {
        ContextId(session.0)
    }
}

/// One context of one session, named across sessions: the context a moved
/// context went on from ([`SessionOrigin::Context`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextRef {
    pub session: SessionId,
    pub context: ContextId,
}

/// What a new session's first context continues
/// (`EventKind::SessionCreated::from`). A session continues at most one
/// thing, so this is one choice rather than two optional fields.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionOrigin {
    /// A context another session moved here (`MoveContext`, D30).
    Context(ContextRef),
    /// A conversation the agent had outside Skywave, by the agent's own id.
    Agent { conversation: String },
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

    /// The seq directly after this one. Saturates at `u64::MAX` rather than
    /// panicking: no log reaches that position (the server stores seqs as
    /// SQLite integers, at most `i64::MAX`), so only a bogus cursor gets
    /// there, and the server rejects that one at the edge.
    pub fn next(self) -> Seq {
        Seq(self.0.saturating_add(1))
    }
}

/// Reference to content kept as a blob, outside the event.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BlobRef {
    pub id: BlobId,
    pub size: u64,
    /// The content's hash: a device makes a download available only once
    /// its bytes hash to this.
    pub sha256: ContentHash,
}

/// An image type the agent accepts. The server and the device both sniff
/// it from the bytes ([`ImageType::sniff`]); no one trusts a declared type.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageType {
    /// The type the leading bytes of an image name, or `None` if they name
    /// none of the four (or are too short to tell).
    pub fn sniff(bytes: &[u8]) -> Option<ImageType> {
        if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
            Some(ImageType::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(ImageType::Jpeg)
        } else if bytes.starts_with(b"GIF8") {
            Some(ImageType::Gif)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(ImageType::Webp)
        } else {
            None
        }
    }

    /// The MIME type of this image type.
    pub fn mime(self) -> &'static str {
        match self {
            ImageType::Png => "image/png",
            ImageType::Jpeg => "image/jpeg",
            ImageType::Gif => "image/gif",
            ImageType::Webp => "image/webp",
        }
    }
}

/// An image on a user message, kept as its own blob (D44: never shared
/// between messages).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Attachment {
    pub blob: BlobRef,
    pub media_type: ImageType,
}

/// The whole of some content a preview or summary does not hold all of.
/// Where it is optional, `None` means there is nothing more: the preview
/// holds it all.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Full {
    /// Kept as a blob.
    Blob(BlobRef),
    /// Carried here, in the event: at most [`limits::INLINE_MAX_BYTES`],
    /// too small to be worth a blob.
    Inline(String),
    /// The server could not store it (a full disk): only the preview or
    /// summary is left, and heads say so.
    Lost,
}

impl Full {
    /// The blob holding the whole content, unless it is inline or lost.
    pub fn blob(&self) -> Option<&BlobRef> {
        match self {
            Full::Blob(blob) => Some(blob),
            Full::Inline(_) | Full::Lost => None,
        }
    }
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
    /// and the daemon produce. Every accepted command logs exactly one event
    /// naming it, the last it appends, so it is that command's
    /// `Accepted { seq }` too: a device whose reply was lost settles the
    /// command from the log, and already has whatever else the command
    /// appended. `MoveContext` also logs the new session's `SessionCreated`
    /// before it.
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
    ///
    /// `from` is what the first context continues, `None` for a fresh
    /// conversation. [`SessionOrigin::Context`]: a context another session
    /// moved here; the daemon logs it for a `MoveContext`, never a command
    /// of its own (the command's event is the source's `ContextMoved`).
    /// [`SessionOrigin::Agent`]: a conversation the agent had outside
    /// Skywave, adopted by the `CreateSession` that logs it. Events stored
    /// before v12 carry a bare [`ContextRef`] in `from` and no longer
    /// decode; such databases are deleted, not migrated.
    SessionCreated {
        workdir: String,
        context_name: String,
        from: Option<SessionOrigin>,
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
    /// A `MoveContext` command sent the context the event is stamped with
    /// on as session `to`, whose `SessionCreated` comes right before it: it
    /// is never current here again.
    ContextMoved {
        to: SessionId,
        by: DeviceId,
    },
    UserMessage {
        device_id: DeviceId,
        text: String,
        /// The message's images, in the order the user attached them, each
        /// its own blob (`BlobUse::Attachment`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// `text` is the whole message unless `full` is set: then it is the
    /// message's head ([`limits::MESSAGE_PREVIEW_MAX_BYTES`]) and `full`
    /// the whole, a [`BlobUse::Message`] blob (or [`Full::Lost`]).
    AgentMessage {
        text: String,
        /// The `Agent` call that started the subagent speaking; `None` for
        /// the main agent.
        parent: Option<CallId>,
        /// The whole message, when it is longer than
        /// [`limits::MESSAGE_WHOLE_MAX_BYTES`]. Never [`Full::Inline`].
        full: Option<Full>,
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
        input: Option<Full>,
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
    /// The agent asks the user `questions` and is blocked until a device
    /// answers (D8): an approval whose answer is a choice. It shares an
    /// approval's ids and life: `QuestionAnswered` answers it,
    /// `ApprovalResolved` with [`Decision::Deny`] declines it, and
    /// `ApprovalCancelled` withdraws it. `call` is the [`ToolCall`] that
    /// asks, as on `ApprovalRequested`. `questions` holds at least one and
    /// at most [`limits::QUESTIONS_MAX`].
    QuestionAsked {
        id: ApprovalId,
        call: Option<CallId>,
        questions: Vec<Question>,
    },
    /// First answer wins (D5), as for `ApprovalResolved`: `answers` holds
    /// one per question, in order ([`answers_fit`]).
    QuestionAnswered {
        id: ApprovalId,
        by: DeviceId,
        answers: Vec<QuestionAnswer>,
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
    /// A subagent finished, and its agent reported what it ran: at most once
    /// a subagent, inside a turn or outside any. Not notable.
    SubagentUsage(SubagentUsage),
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
    /// The agent process just started runs `version`, not `confirmed`, the
    /// version the server's adapter was checked against: the agent's
    /// protocol may have changed under it, so a turn can misbehave (e.g.
    /// stay working until stopped). Logged once per agent process that
    /// reports such a version, in the context it was started for. Each is
    /// at most [`limits::AGENT_VERSION_MAX_CHARS`]. A warning to show, not
    /// notable, not activity.
    AgentVersionUnconfirmed {
        version: String,
        confirmed: String,
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
    /// The agent's live background tasks after a change, in the agent's
    /// order: the whole set, replacing the last one logged (see
    /// [`AgentWork::apply`]). At most [`limits::TASKS_MAX`]. Not notable.
    BackgroundTasksChanged {
        tasks: Vec<BackgroundTask>,
    },
    /// The agent's scheduled wakeups as it last reported them: the whole
    /// set, replacing the last one logged. Best effort and information
    /// only; at most [`limits::SCHEDULES_MAX`]. Not notable.
    SchedulesChanged {
        schedules: Vec<Schedule>,
    },
    /// A `StopTask` command asked the agent to stop its background `task`.
    /// Success shows as the task leaving the next `BackgroundTasksChanged`;
    /// a `TaskStopRefused` naming the command answers otherwise. Not
    /// notable.
    TaskStopRequested {
        task: TaskId,
        by: DeviceId,
    },
    /// The agent did not stop `task`, or no agent ran to stop it. `answers`
    /// names the `StopTask` command refused. Not notable.
    TaskStopRefused {
        task: TaskId,
        reason: String,
        answers: Option<CommandId>,
    },
    /// These calls will never get a result: the server logs it when it
    /// knows (e.g. their subagent ended). Clients mark each named call
    /// still running as having no result and leave settled ones alone. Not
    /// notable, not activity.
    CallsUnanswered {
        calls: Vec<CallId>,
    },
    /// A `RunShell` command asked the server to run `command` in the
    /// session's workdir, once `when` matches the session's last status.
    /// The command's own id names the run in every later shell event.
    /// Activity, not notable; brings an archived session back.
    ShellRequested {
        command: String,
        when: RunWhen,
        by: DeviceId,
    },
    /// A `StopShell` command asked the server to stop the shell `run` (the
    /// `RunShell` command's id). Its `ShellFinished` answers it. Not
    /// notable, not activity.
    ShellStopRequested {
        run: CommandId,
        by: DeviceId,
    },
    /// The daemon started the shell `run` in `cwd`, the directory it
    /// really starts in. Not notable, not activity.
    ShellStarted {
        run: CommandId,
        cwd: String,
    },
    /// Progress of the shell `run`, logged by the daemon: `tail` is the
    /// last lines within [`limits::PREVIEW_MAX_LINES`] and
    /// [`limits::PREVIEW_MAX_BYTES`], ANSI stripped ([`strip_ansi`]);
    /// `lines` and `bytes` are the totals so far. Not notable, not
    /// activity.
    ShellOutput {
        run: CommandId,
        tail: String,
        lines: u32,
        bytes: u64,
    },
    /// The shell `run` is over, logged by the daemon. `preview` is the
    /// output's tail within the preview caps, ANSI stripped, invalid UTF-8
    /// decoded lossily and cut on a char boundary; `output` holds the whole
    /// raw output when the preview does not (`truncated`). `lines` is the
    /// total line count. `cwd` is where the shell ended, `None` when not
    /// known. `by` (the requester), `started_at` (the `at` of the run's
    /// `ShellStarted`, `None` when it never started) and `command_line`
    /// (the command's first line, cut to [`limits::FIRST_PROMPT_MAX_CHARS`])
    /// are copied from earlier events so a device can alert from this one.
    /// Not notable, not activity.
    ShellFinished {
        run: CommandId,
        by: DeviceId,
        started_at: Option<DateTime<Utc>>,
        command_line: String,
        end: ShellEnd,
        cwd: Option<String>,
        preview: String,
        lines: u32,
        output: Option<BlobRef>,
        truncated: bool,
    },
    /// One item of the history of the conversation an adopting
    /// `CreateSession` took over ([`SessionOrigin::Agent`]), as the agent
    /// kept it outside Skywave; logged by the daemon, in the conversation's
    /// order, between the `SessionCreated` and its `ImportFinished`. `at` is
    /// when the agent recorded it, `None` when it did not say. A record, not
    /// a live event: it is not delivered to the agent, not notable, not
    /// activity, and opens no call. `item` stays nested, so the top-level
    /// `type` of an imported call is `imported`, never `tool_call`.
    Imported {
        at: Option<DateTime<Utc>>,
        item: ImportedItem,
    },
    /// The import an adopting `CreateSession` ran is over: the command's own
    /// event, logged after every `Imported` it brought. Logged for every
    /// adopting create, whatever the outcome, so heads always know where
    /// the imported history ends. Not notable, not activity.
    ImportFinished {
        outcome: ImportOutcome,
    },
    /// `by`'s `DeleteBlob` deleted the event blob `blob` from the server:
    /// it can no longer be downloaded. `holds` is what it held. The event
    /// that referenced it keeps its preview. Logged in the session and
    /// context of that event. Not notable, not activity.
    BlobDeleted {
        blob: BlobId,
        holds: BlobUse,
        by: DeviceId,
    },
    /// `by`'s `CropBlob` replaced the event blob `blob` with `into`, a new
    /// blob holding only the lines `kept` names ([`crop::crop`]); `blob` is
    /// gone from the server. `holds` is what both hold (`ToolOutput` or
    /// `ShellOutput`), `lines` how many lines `blob` had. The event that
    /// referenced `blob` keeps its preview and now stands for `into`.
    /// Logged in the session and context of that event. Not notable, not
    /// activity.
    BlobCropped {
        blob: BlobId,
        holds: BlobUse,
        into: BlobRef,
        kept: Vec<LineSpan>,
        lines: u32,
        by: DeviceId,
    },
}

/// Lines `first .. first + count` (0-based) of a blob, cut at `b'\n'`
/// ([`crop`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineSpan {
    pub first: u32,
    pub count: u32,
}

/// What an `EventKind::Imported` carries: the parts of an outside
/// conversation a head shows. Subagent lines are not imported, so a call's
/// `parent` is always `None`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImportedItem {
    /// `text` and `full` as on `EventKind::AgentMessage`.
    UserMessage {
        text: String,
        full: Option<Full>,
    },
    /// `text` and `full` as on `EventKind::AgentMessage`.
    AgentMessage {
        text: String,
        full: Option<Full>,
    },
    ToolCall(ToolCall),
    /// Links by `call` to an imported `ToolCall`.
    ToolResult(ToolResult),
}

/// How the import of an adopted conversation ended
/// (`EventKind::ImportFinished::outcome`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportOutcome {
    /// `items` `Imported` events were logged before this one.
    Imported { items: u32 },
    /// The agent keeps no such conversation for the session's workdir.
    NotFound,
    /// Reading the conversation failed; `reason` is bounded text
    /// ([`limits::REFUSAL_MAX_CHARS`]).
    Failed { reason: String },
    /// The server's agent cannot read conversations it had outside Skywave.
    Unavailable,
}

/// When a shell run starts, matched against the session's last status
/// (`RunShell::when`). Absent on the wire means [`RunWhen::Always`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunWhen {
    #[default]
    Always,
    OnSuccess,
    OnFailure,
}

/// How a shell run ended (`ShellFinished::end`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellEnd {
    /// The shell exited with `code`; only `0` is success.
    Exited { code: i32 },
    /// The shell was killed by `signal`.
    Signalled { signal: i32 },
    /// A `StopShell` stopped it.
    Stopped,
    /// The run's `when` did not match the session's last status; it never
    /// started.
    Skipped,
    /// The daemon lost track of it (e.g. it restarted mid-run).
    Lost,
    /// It could not be started, for `reason`.
    NotStarted { reason: String },
}

impl ShellEnd {
    /// Whether the run succeeded: only `Exited { code: 0 }` does.
    pub fn is_success(&self) -> bool {
        matches!(self, ShellEnd::Exited { code: 0 })
    }
}

impl EventKind {
    /// The device whose command logged the event, for events that name one.
    pub fn device(&self) -> Option<DeviceId> {
        match self {
            EventKind::UserMessage { device_id, .. } => Some(*device_id),
            EventKind::ApprovalResolved { by, .. }
            | EventKind::QuestionAnswered { by, .. }
            | EventKind::SessionRenamed { by, .. }
            | EventKind::SessionArchived { by, .. }
            | EventKind::PermissionModeRequested { by, .. }
            | EventKind::ModelRequested { by, .. }
            | EventKind::EffortRequested { by, .. }
            | EventKind::ContextStarted { by, .. }
            | EventKind::ContextSwitched { by }
            | EventKind::ContextMoved { by, .. }
            | EventKind::TaskStopRequested { by, .. }
            | EventKind::ShellRequested { by, .. }
            | EventKind::ShellStopRequested { by, .. }
            | EventKind::BlobDeleted { by, .. }
            | EventKind::BlobCropped { by, .. } => Some(*by),
            // The daemon logs it, but copies the requester's device so a
            // device can tell its own runs apart from this one event.
            EventKind::ShellFinished { by, .. } => Some(*by),
            EventKind::SessionCreated { .. }
            | EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::QuestionAsked { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
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
            | EventKind::CallsUnanswered { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::Imported { .. }
            | EventKind::ImportFinished { .. } => None,
        }
    }

    /// Whether only a command logs this event, so it carries the
    /// [`Event::command_id`] of the command that did. The agent and the
    /// daemon log the rest, `ApprovalCancelled` included. Only a fresh
    /// `SessionCreated` is its `CreateSession`'s own event: a moved
    /// context's ([`SessionOrigin::Context`]) `MoveContext` names itself on
    /// the `ContextMoved` that follows, and an adopting
    /// ([`SessionOrigin::Agent`]) `CreateSession` on the `ImportFinished`
    /// that ends its imported history.
    pub fn logged_by_command(&self) -> bool {
        match self {
            EventKind::SessionCreated { from, .. } => from.is_none(),
            EventKind::UserMessage { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::QuestionAnswered { .. }
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
            | EventKind::ShellRequested { .. }
            | EventKind::ShellStopRequested { .. }
            | EventKind::ImportFinished { .. }
            | EventKind::BlobDeleted { .. }
            | EventKind::BlobCropped { .. } => true,
            EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalCancelled { .. }
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
            | EventKind::CallsUnanswered { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::ShellFinished { .. }
            | EventKind::Imported { .. } => false,
        }
    }

    /// What the event does to its session's archived flag, the one rule the
    /// server and every client fold: `SessionArchived` sets it, a
    /// `UserMessage` or `ShellRequested` from any device brings the session
    /// back, anything else leaves it alone (`None`).
    pub fn archived_effect(&self) -> Option<bool> {
        match self {
            EventKind::SessionArchived { archived, .. } => Some(*archived),
            EventKind::UserMessage { .. } | EventKind::ShellRequested { .. } => Some(false),
            EventKind::SessionCreated { .. }
            | EventKind::AgentMessage { .. }
            | EventKind::ToolCall(_)
            | EventKind::ToolResult(_)
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::QuestionAsked { .. }
            | EventKind::QuestionAnswered { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_)
            | EventKind::SubagentUsage(_)
            | EventKind::PermissionModeRequested { .. }
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::AgentVersionUnconfirmed { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::BackgroundTasksChanged { .. }
            | EventKind::SchedulesChanged { .. }
            | EventKind::TaskStopRequested { .. }
            | EventKind::TaskStopRefused { .. }
            | EventKind::CallsUnanswered { .. }
            | EventKind::ShellStopRequested { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::ShellFinished { .. }
            | EventKind::Imported { .. }
            | EventKind::ImportFinished { .. }
            | EventKind::BlobDeleted { .. }
            | EventKind::BlobCropped { .. } => None,
        }
    }

    /// The blobs the event references, with what each holds: a tool call's
    /// input and the whole of its display body, a result's output, an
    /// approval's input, a finished shell run's whole output, a long
    /// message's whole text, a user message's images; the same for an
    /// imported call, result or message. A [`Full::Inline`] or
    /// [`Full::Lost`] references none. A `BlobCropped` yields its `into`,
    /// the blob it made, with what it holds.
    pub fn blobs(&self) -> impl Iterator<Item = (BlobUse, &BlobRef)> {
        fn full(holds: BlobUse, full: &Option<Full>) -> Option<(BlobUse, &BlobRef)> {
            full.as_ref().and_then(Full::blob).map(|b| (holds, b))
        }
        fn body(display: &ToolDisplay) -> Option<(BlobUse, &BlobRef)> {
            let body = display.body.as_ref()?;
            full(BlobUse::ToolBody, &body.full)
        }
        let (first, second) = match self {
            EventKind::ToolCall(ToolCall { input, display, .. })
            | EventKind::Imported {
                item: ImportedItem::ToolCall(ToolCall { input, display, .. }),
                ..
            } => (full(BlobUse::ToolInput, input), body(display)),
            EventKind::ToolResult(ToolResult { output, .. })
            | EventKind::Imported {
                item: ImportedItem::ToolResult(ToolResult { output, .. }),
                ..
            } => (full(BlobUse::ToolOutput, output), None),
            EventKind::ApprovalRequested { input, .. } => {
                (full(BlobUse::ApprovalInput, input), None)
            }
            EventKind::ShellFinished { output, .. } => {
                (output.as_ref().map(|b| (BlobUse::ShellOutput, b)), None)
            }
            EventKind::BlobCropped { holds, into, .. } => (Some((*holds, into)), None),
            EventKind::AgentMessage { full: whole, .. }
            | EventKind::Imported {
                item:
                    ImportedItem::UserMessage { full: whole, .. }
                    | ImportedItem::AgentMessage { full: whole, .. },
                ..
            } => (full(BlobUse::Message, whole), None),
            EventKind::SessionCreated { .. }
            | EventKind::UserMessage { .. }
            | EventKind::ApprovalResolved { .. }
            | EventKind::ApprovalCancelled { .. }
            | EventKind::QuestionAsked { .. }
            | EventKind::QuestionAnswered { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionStopped
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. }
            | EventKind::SessionStateChanged(_)
            | EventKind::TurnUsage(_)
            | EventKind::SubagentUsage(_)
            | EventKind::PermissionModeRequested { .. }
            | EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelRequested { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::AgentVersionUnconfirmed { .. }
            | EventKind::EffortRequested { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
            | EventKind::ContextStarted { .. }
            | EventKind::ContextSwitched { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::BackgroundTasksChanged { .. }
            | EventKind::SchedulesChanged { .. }
            | EventKind::TaskStopRequested { .. }
            | EventKind::TaskStopRefused { .. }
            | EventKind::CallsUnanswered { .. }
            | EventKind::ShellRequested { .. }
            | EventKind::ShellStopRequested { .. }
            | EventKind::ShellStarted { .. }
            | EventKind::ShellOutput { .. }
            | EventKind::ImportFinished { .. }
            // It names a blob it deleted; it references none.
            | EventKind::BlobDeleted { .. } => (None, None),
        };
        let attachments: &[Attachment] = match self {
            EventKind::UserMessage { attachments, .. } => attachments,
            _ => &[],
        };
        first
            .into_iter()
            .chain(second)
            .chain(attachments.iter().map(|a| (BlobUse::Attachment, &a.blob)))
    }
}

/// What an event's blob holds ([`EventKind::blobs`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BlobUse {
    /// A tool call's whole input.
    ToolInput,
    /// The whole of a tool call's display body (e.g. a diff).
    ToolBody,
    /// A tool result's whole output.
    ToolOutput,
    /// An approval request's whole input.
    ApprovalInput,
    /// A finished shell run's whole output.
    ShellOutput,
    /// A long message's whole text (`full` on `EventKind::AgentMessage`
    /// and on an imported message). Not croppable: a message cut to some of
    /// its lines says something else.
    Message,
    /// An image attached to a user message; deletable, not croppable.
    Attachment,
}

/// Size caps on the bounded text in tool events. Producers (the server)
/// enforce them; renderers should still clip.
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
    /// Most UTF-8 bytes of a tool result's whole output carried in its
    /// event ([`Full::Inline`](crate::Full::Inline)) rather than as a blob.
    /// A blob costs a file, two fsyncs and a row on the server, and under
    /// the eager policy a request per device, each with a round trip on a
    /// bad link and headers worth hundreds of bytes; below a few KiB that
    /// costs more than the bytes it keeps out of the log. 4 KiB holds the
    /// common small results whole (an edit's note to the model, a short
    /// read, a grep's file list) while capping what every device pulls per
    /// result at eight previews' worth.
    pub const INLINE_MAX_BYTES: usize = 4096;
    /// Longest message (`EventKind::AgentMessage`, an imported message)
    /// whose text travels whole in its event, in UTF-8 bytes. Messages are
    /// the conversation every head shows unasked, so they get four times a
    /// tool output's [`INLINE_MAX_BYTES`]: 16 KiB is several screens, which
    /// holds nearly every reply whole. Past it is a report or a dump, which
    /// would otherwise reach every device as event data however rarely it
    /// is read; it travels as a [`MESSAGE_PREVIEW_MAX_BYTES`] head and a
    /// blob. A typed `SendMessage` is bounded by [`MESSAGE_MAX_BYTES`]
    /// instead, and its `UserMessage` stays whole: the server hands that
    /// text to the agent.
    pub const MESSAGE_WHOLE_MAX_BYTES: usize = 16 * 1024;
    /// Most UTF-8 bytes of a long message's head, its `text` beside the
    /// blob that holds it whole: about a screen, enough to tell what the
    /// message is about. Cut at the last line break in it, if any, else on
    /// a char boundary.
    pub const MESSAGE_PREVIEW_MAX_BYTES: usize = 2048;
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
    /// Most [`Question`](crate::Question)s in one `QuestionAsked`.
    pub const QUESTIONS_MAX: usize = 8;
    /// Most [`QuestionOption`](crate::QuestionOption)s of one question.
    pub const QUESTION_OPTIONS_MAX: usize = 8;
    /// Longest [`Question::header`](crate::Question::header), in chars.
    pub const QUESTION_HEADER_MAX_CHARS: usize = 40;
    /// Longest [`Question::text`](crate::Question::text), in chars.
    pub const QUESTION_TEXT_MAX_CHARS: usize = 1000;
    /// Longest [`QuestionOption::label`](crate::QuestionOption::label), in
    /// chars.
    pub const OPTION_LABEL_MAX_CHARS: usize = 120;
    /// Longest
    /// [`QuestionOption::description`](crate::QuestionOption::description),
    /// in chars.
    pub const OPTION_DESCRIPTION_MAX_CHARS: usize = 500;
    /// Longest [`QuestionOption::preview`](crate::QuestionOption::preview),
    /// in bytes; a longer one is cut at the last line break within it, if
    /// any, else on a char boundary.
    pub const OPTION_PREVIEW_MAX_BYTES: usize = 4096;
    /// Longest [`QuestionAnswer::other`](crate::QuestionAnswer::other), in
    /// bytes.
    pub const ANSWER_TEXT_MAX_BYTES: usize = 8192;
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
    /// Longest version in `AgentVersionUnconfirmed`, in chars.
    pub const AGENT_VERSION_MAX_CHARS: usize = 40;
    /// Longest refusal reason (`PermissionModeRefused`, `ModelRefused`,
    /// `TaskStopRefused`), in chars.
    pub const REFUSAL_MAX_CHARS: usize = 200;
    /// Most [`BackgroundTask`](crate::BackgroundTask)s in one
    /// `BackgroundTasksChanged`; past it the producer keeps the first ones
    /// in the agent's order and drops the rest.
    pub const TASKS_MAX: usize = 32;
    /// Most [`Schedule`](crate::Schedule)s in one `SchedulesChanged`; past
    /// it the producer keeps the first ones in the agent's order and drops
    /// the rest.
    pub const SCHEDULES_MAX: usize = 16;
    /// Longest [`TaskId`](crate::TaskId) or [`ScheduleId`](crate::ScheduleId),
    /// in chars. Agents mint short ids (a handful of chars); the producer
    /// drops a task or schedule whose id is longer rather than cut it, as a
    /// cut id would no longer name it.
    pub const TASK_ID_MAX_CHARS: usize = 64;
    /// Longest [`Schedule::when`](crate::Schedule::when), in chars.
    pub const SCHEDULE_WHEN_MAX_CHARS: usize = 60;
    /// Longest agent conversation id (`CreateSession::conversation`,
    /// [`SessionOrigin::Agent`](crate::SessionOrigin::Agent)), in chars.
    /// Agents mint short ids (a Claude Code session id is a 36-char UUID);
    /// the server refuses a longer one rather than cut it, as a cut id
    /// would name a different conversation.
    pub const CONVERSATION_ID_MAX_CHARS: usize = 128;
    /// Longest first line of a session's first message kept for its title,
    /// and of a shell command in `ShellFinished::command_line`, in chars.
    pub const FIRST_PROMPT_MAX_CHARS: usize = 200;
    /// Longest `RunShell::command`, in UTF-8 bytes; the server refuses a
    /// longer one as `BadShellCommand` rather than cut it, as a cut command
    /// would run something else.
    pub const SHELL_COMMAND_MAX_BYTES: usize = 8192;
    /// Longest `SendMessage::text`, in UTF-8 bytes; the server refuses a
    /// longer one as `MessageTooLong` rather than cut it, as a cut message
    /// would ask something else. Generous for anything typed or pasted (a
    /// few thousand lines of code); it bounds the log a client minting new
    /// command ids can write per request, which the HTTP body limit (2 MB)
    /// alone leaves near 2 MB.
    pub const MESSAGE_MAX_BYTES: usize = 256 * 1024;
    /// Most [`Attachment`](crate::Attachment)s on one `SendMessage`; past it
    /// the server refuses with `TooManyAttachments`.
    pub const ATTACHMENTS_MAX: usize = 8;
    /// Largest [`Attachment`](crate::Attachment), in raw bytes, once the
    /// device has shrunk it. The API takes an image up to 5 MiB of base64,
    /// 3,932,160 raw bytes; this keeps a margin. The device shrinks images
    /// before this applies.
    pub const ATTACHMENT_MAX_BYTES: u64 = 3_750_000;
    /// Most raw bytes of one message's [`Attachment`](crate::Attachment)s
    /// together. Bounds one message's upload and the images the agent's
    /// conversation re-sends each turn.
    pub const ATTACHMENTS_MAX_TOTAL_BYTES: u64 = 10_000_000;
    /// Longest [`LoginStartResult::Open`](crate::LoginStartResult::Open)
    /// URL, in UTF-8 bytes. The server answers a longer one as `Failed`
    /// rather than cut it, as a cut URL opens nothing. Real ones are a few
    /// hundred bytes.
    pub const LOGIN_URL_MAX_BYTES: usize = 4 * 1024;
    /// Longest [`LoginFinish::code`](crate::LoginFinish::code), in UTF-8
    /// bytes; the server refuses a longer one with a 400.
    pub const LOGIN_CODE_MAX_BYTES: usize = 1024;
    /// Most [`ChangedFile`](crate::ChangedFile)s in one
    /// [`ChangeSet`](crate::ChangeSet); the rest are only counted in
    /// [`ChangeSet::total`](crate::ChangeSet::total).
    pub const WORKDIR_CHANGES_MAX: usize = 500;
    /// Most [`Commit`](crate::Commit)s in one [`CommitLog`](crate::CommitLog);
    /// [`CommitLog::more`](crate::CommitLog::more) says older ones exist.
    pub const WORKDIR_LOG_MAX: usize = 50;
    /// Largest diff of one changed file, in bytes; past it the file's diff
    /// is [`DiffBody::TooLarge`](crate::DiffBody::TooLarge).
    pub const WORKDIR_DIFF_MAX_BYTES: u64 = 1024 * 1024;
    /// Most diff bytes in one [`ChangeSet`](crate::ChangeSet); files past it
    /// get [`DiffBody::Omitted`](crate::DiffBody::Omitted). Bounds the
    /// server's disk and memory per snapshot, not the link: diffs travel as
    /// blobs fetched on opening.
    pub const WORKDIR_DIFFS_MAX_BYTES: u64 = 8 * 1024 * 1024;
    /// Largest file served by `GET /v1/workdir/file`, in bytes; past it the
    /// content is [`FileBody::TooLarge`](crate::FileBody::TooLarge).
    pub const WORKDIR_FILE_MAX_BYTES: u64 = 4 * 1024 * 1024;
    /// Most [`DirEntry`](crate::DirEntry)s in one
    /// [`ListResult::Listed`](crate::ListResult::Listed); past it the
    /// listing is `truncated`.
    pub const WORKDIR_LIST_MAX: usize = 1000;
    /// Most [`OutsideConversation`](crate::OutsideConversation)s in one
    /// [`ConversationsResult::Listed`](crate::ConversationsResult::Listed);
    /// past it the listing is `truncated`.
    pub const CONVERSATIONS_MAX: usize = 100;
    /// Most blobs and most shell runs in one
    /// [`StorageReport`](crate::StorageReport): the largest.
    pub const STORAGE_TOP_MAX: usize = 50;
    /// Most [`LineSpan`](crate::LineSpan)s one
    /// [`CropBlob`](crate::CommandKind::CropBlob) keeps.
    pub const CROP_SPANS_MAX: usize = 64;
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
    /// Never asks: everything runs. Only on a server that allows it
    /// ([`HelloResponse::bypass`]); elsewhere asking for it is
    /// `BypassDisabled`.
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

/// Identifies one background task of a session's agent: the agent's own
/// id, opaque to clients, which send it back to name the task. At most
/// [`limits::TASK_ID_MAX_CHARS`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TaskId(pub String);

/// What kind of work a [`BackgroundTask`] is. Agent-agnostic (D3).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// A shell command, or a watch on one's output.
    Shell,
    /// Another agent working for this one.
    Subagent,
    Other,
}

/// Work the agent runs outside a turn, as it reported it last.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: TaskId,
    pub kind: TaskKind,
    /// One line, at most [`limits::TITLE_MAX_CHARS`].
    pub title: String,
    /// The subagent task that launched this one, if any.
    pub parent: Option<TaskId>,
    /// The `Agent` call a [`TaskKind::Subagent`] task runs; `None` for
    /// shells and when unknown.
    pub call: Option<CallId>,
    /// Not activity: no busy indicator, and it does not keep the agent
    /// alive on its own.
    pub ambient: bool,
}

/// Identifies one scheduled wakeup of a session's agent: the agent's own
/// id, opaque to clients. At most [`limits::TASK_ID_MAX_CHARS`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ScheduleId(pub String);

/// A prompt the agent scheduled for itself, as it reported it last.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: ScheduleId,
    /// When it fires, in the agent's own words (e.g. a cron expression);
    /// clients show it and never parse it. At most
    /// [`limits::SCHEDULE_WHEN_MAX_CHARS`].
    pub when: String,
    pub recurring: bool,
    /// One line, at most [`limits::TITLE_MAX_CHARS`].
    pub prompt: String,
    /// Whether it is saved with the workdir and fires in later agents too;
    /// `None` when the agent does not say.
    pub outlives_agent: Option<bool>,
}

/// A session's agent's background work as its log has it: the one rule the
/// server and every client fold (see [`AgentWork::apply`]).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentWork {
    pub tasks: Vec<BackgroundTask>,
    pub schedules: Vec<Schedule>,
}

impl AgentWork {
    /// Folds one event of the session: each `…Changed` replaces its whole
    /// set, the last event wins; anything else leaves the work alone.
    pub fn apply(&mut self, kind: &EventKind) {
        match kind {
            EventKind::BackgroundTasksChanged { tasks } => self.tasks = tasks.clone(),
            EventKind::SchedulesChanged { schedules } => self.schedules = schedules.clone(),
            _ => {}
        }
    }

    /// Whether `kind` is one [`apply`](Self::apply) folds.
    pub fn folds(kind: &EventKind) -> bool {
        matches!(
            kind,
            EventKind::BackgroundTasksChanged { .. } | EventKind::SchedulesChanged { .. }
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

/// A question the agent asks the user, with options to pick from
/// (`EventKind::QuestionAsked`). Agent-agnostic (D3): the server cuts it to
/// the [`limits`], and an answer names options by index.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// A short tag for the question, e.g. `Bread`, at most
    /// [`limits::QUESTION_HEADER_MAX_CHARS`]; may be empty.
    pub header: String,
    /// The question, at most [`limits::QUESTION_TEXT_MAX_CHARS`].
    pub text: String,
    /// What to pick from, in the agent's order, at most
    /// [`limits::QUESTION_OPTIONS_MAX`].
    pub options: Vec<QuestionOption>,
    /// Any number of options may be picked, not exactly one.
    pub multi_select: bool,
}

/// One option of a [`Question`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    /// One line, at most [`limits::OPTION_LABEL_MAX_CHARS`].
    pub label: String,
    /// What picking it means, at most
    /// [`limits::OPTION_DESCRIPTION_MAX_CHARS`]; may be empty.
    pub description: String,
    /// What it would look like (a mockup, a snippet), several lines shown
    /// as written, at most [`limits::OPTION_PREVIEW_MAX_BYTES`].
    pub preview: Option<String>,
}

/// The answer to one [`Question`]: the options picked, by index, and text
/// of the user's own instead of (or, multi-select, besides) an option.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuestionAnswer {
    /// Indexes into [`Question::options`], ascending, each at most once.
    pub picked: Vec<u8>,
    /// Not blank, at most [`limits::ANSWER_TEXT_MAX_BYTES`].
    pub other: Option<String>,
}

impl Question {
    /// Whether `answer` answers this question: every pick names an option,
    /// ascending and once each; `other`, when given, is not blank and
    /// within [`limits::ANSWER_TEXT_MAX_BYTES`]; a single-select question
    /// takes exactly one of an option or `other`, a multi-select one at
    /// least one of either.
    pub fn takes(&self, answer: &QuestionAnswer) -> bool {
        let named = answer.picked.windows(2).all(|w| w[0] < w[1])
            && answer
                .picked
                .last()
                .is_none_or(|last| usize::from(*last) < self.options.len());
        let other = answer.other.as_ref().is_none_or(|text| {
            !text.trim().is_empty() && text.len() <= limits::ANSWER_TEXT_MAX_BYTES
        });
        let given = answer.picked.len() + usize::from(answer.other.is_some());
        let count = if self.multi_select {
            given >= 1
        } else {
            given == 1
        };
        named && other && count
    }
}

/// Whether `answers` answer `questions`: one each, in order, each one
/// [`Question::takes`].
pub fn answers_fit(questions: &[Question], answers: &[QuestionAnswer]) -> bool {
    questions.len() == answers.len() && questions.iter().zip(answers).all(|(q, a)| q.takes(a))
}

/// The agent invoked a tool.
///
/// `name` is the agent's own tool name, carried as data for display; clients
/// must not branch on it, only on [`ToolDisplay::kind`]. `input` is the full
/// input, when the display does not show all of it ([`Full::Lost`] when the
/// server could not keep it).
///
/// `parent` is the `Agent` call that started the subagent making this call;
/// `None` for the main agent. A nested subagent chains through the inner
/// `Agent` call's own `parent`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub call: Option<CallId>,
    pub parent: Option<CallId>,
    pub name: String,
    pub display: ToolDisplay,
    pub input: Option<Full>,
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

/// A bounded preview of some content, with the full content as a blob
/// (or the word it was lost).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Body {
    pub format: BodyFormat,
    /// Head of the content: at most [`limits::PREVIEW_MAX_LINES`] lines and
    /// [`limits::PREVIEW_MAX_BYTES`] bytes, with real newlines.
    pub preview: String,
    /// Line count of the whole content.
    pub lines: u32,
    /// The whole content, when `preview` does not hold all of it.
    pub full: Option<Full>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    Text,
    /// A unified diff.
    Diff,
}

/// A tool finished.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// The [`ToolCall`] this answers; `None` when the agent reported a
    /// result for no logged call.
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
    /// The whole output, when `preview` does not hold all of it: inline
    /// when it is at most [`limits::INLINE_MAX_BYTES`], else a blob.
    pub output: Option<Full>,
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
    /// How full the context is as the turn ends: the tokens the next request
    /// will read. Not a sum over the turn. `None` when the agent reported no
    /// fill (a cleared context's turn, an interrupt while text streamed).
    pub context_tokens: Option<u64>,
    /// How many tokens the context of the turn's model holds.
    pub context_window: Option<u64>,
}

impl TurnUsage {
    /// The agent reported nothing at all.
    pub fn is_empty(&self) -> bool {
        *self == TurnUsage::default()
    }
}

/// What a finished subagent ran, as its agent reported it when it ended.
/// Agent-agnostic (D3). A figure the agent does not report is `None`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SubagentUsage {
    /// The call that ran the subagent.
    pub call: CallId,
    /// Wall-clock time it ran, in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// The tool calls it made.
    #[serde(default)]
    pub tool_uses: Option<u64>,
    /// How full its own context was when it ended. Not what it spent.
    #[serde(default)]
    pub context_tokens: Option<u64>,
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
        /// The agent's own id for a conversation it had outside Skywave, in
        /// `workdir`; the session's first context continues a copy of it.
        /// `None`: a fresh conversation.
        conversation: Option<String>,
    },
    /// Send `text` to the agent. `context` is the session's context the
    /// message was written in; the server refuses it as `ContextChanged`
    /// when that is no longer the current one, and as `MessageTooLong` when
    /// `text` is over [`limits::MESSAGE_MAX_BYTES`]. `attachments` are
    /// images the device staged beforehand; the server refuses an image it
    /// has none staged for as `UnknownAttachment`.
    SendMessage {
        session_id: SessionId,
        context: ContextId,
        text: String,
        /// The sha256 of each image to attach, in order, each staged
        /// beforehand with `PUT` [`path::ATTACHMENT_ROUTE`]; the server
        /// refuses one it has none staged for as `UnknownAttachment`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<ContentHash>,
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
    /// Answer a question (`EventKind::QuestionAsked`), one answer per
    /// question in order. The server refuses answers that do not fit
    /// ([`answers_fit`]) as `BadAnswer`. To decline, deny it with
    /// `ResolveApproval`.
    AnswerQuestion {
        id: ApprovalId,
        answers: Vec<QuestionAnswer>,
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
    /// Continue `context`, a context of the session with a conversation
    /// that is not its current one, as a new session in the session's
    /// workdir. The new session's first context is named `name`, as
    /// [`ClearContext`] names one, and continues the conversation;
    /// `context` stays in the source, moved, never current again. The
    /// server logs the new session's `SessionCreated` (naming no command),
    /// then the source's `ContextMoved`, which names this command and is
    /// its `Accepted { seq }`.
    ///
    /// [`ClearContext`]: CommandKind::ClearContext
    MoveContext {
        session_id: SessionId,
        context: ContextId,
        name: String,
    },
    /// Ask the session's agent to stop its background `task`, one the
    /// server last logged in a `BackgroundTasksChanged` (`UnknownTask`
    /// otherwise). Never starts the agent. Logged as a `TaskStopRequested`;
    /// success shows as the task leaving the set, a refusal as a
    /// `TaskStopRefused` naming this command.
    StopTask { session_id: SessionId, task: TaskId },
    /// Run `command` with the server's shell in the session's workdir, once
    /// `when` matches the session's last status (absent: `Always`). This
    /// command's id is the run's id: every later shell event names the run
    /// by it. Logged as a `ShellRequested`. `BadShellCommand` when blank
    /// after trimming or over [`limits::SHELL_COMMAND_MAX_BYTES`];
    /// `ShellDisabled` when the server has shell commands off.
    RunShell {
        session_id: SessionId,
        command: String,
        #[serde(default)]
        when: RunWhen,
    },
    /// Stop the shell `run` (its `RunShell` command's id). Logged as a
    /// `ShellStopRequested`; its `ShellFinished` answers it. `UnknownRun`
    /// when the session has no such run, `RunFinished` when it is over.
    StopShell {
        session_id: SessionId,
        run: CommandId,
    },
    /// Delete the event blob `blob` from the server. The preview in its
    /// event stays. Logged as `BlobDeleted`. `UnknownBlob` when no event
    /// references it, `BlobGone` when it was already deleted or cropped.
    /// Taken while the disk is low or the server is stopping: it frees
    /// space.
    DeleteBlob { blob: BlobId },
    /// Replace the event blob `blob` with the lines `keep` names, as a new
    /// blob. Only a tool output or a shell run's output. Logged as
    /// `BlobCropped`. Refused like `DeleteBlob`, plus `NotCroppable` for a
    /// blob that holds something else and `BadCrop` for spans that do not
    /// fit its lines ([`crop::check_spans`], [`crop::crop`]). Taken while
    /// the disk is low or the server is stopping: it frees space.
    CropBlob { blob: BlobId, keep: Vec<LineSpan> },
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

/// Terminal output as shell previews carry it (`ShellOutput::tail`,
/// `ShellFinished::preview`): escape sequences removed (CSI such as colours
/// and cursor moves; OSC such as titles and `OSC 8` links, whose text
/// stays; the DCS, SOS, PM and APC strings, such as inline images; and any
/// other `ESC` sequence or lone `ESC`). A sequence cut short by a char it
/// cannot hold (a CSI by a newline or text, a string by an `ESC` other than
/// `ESC \`) ends there and that char is kept or parsed anew. 8-bit C1
/// controls are not recognised. Then carriage-return
/// overwrites collapsed: a line rewritten with `\r` keeps only its last
/// non-empty write, the way a progress bar ends on screen. A `\r\n` is a
/// plain newline. Everything else, UTF-8 included, is kept as is.
pub fn strip_ansi(text: &str) -> String {
    const ESC: char = '\u{1b}';
    const BEL: char = '\u{7}';
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != ESC {
            plain.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: parameter and intermediate bytes (0x20-0x3F), then one
            // final byte (0x40-0x7E). Anything else cuts it short and is
            // left for the outer loop: a new ESC, a newline, text.
            Some('[') => {
                chars.next();
                while chars
                    .next_if(|c| ('\u{20}'..='\u{3f}').contains(c))
                    .is_some()
                {}
                chars.next_if(|c| ('\u{40}'..='\u{7e}').contains(c));
            }
            // String sequences: OSC `]`, DCS `P`, SOS `X`, PM `^` and APC
            // `_` run to BEL or the string terminator `ESC \`. Any other ESC
            // cuts the string short and starts a sequence of its own.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                chars.next();
                let mut belled = false;
                while let Some(c) = chars.next_if(|c| *c != ESC) {
                    if c == BEL {
                        belled = true;
                        break;
                    }
                }
                if !belled {
                    let mut ahead = chars.clone();
                    if ahead.next() == Some(ESC) && ahead.next() == Some('\\') {
                        chars.next();
                        chars.next();
                    }
                }
            }
            // Any other sequence: intermediate bytes, then one final byte.
            // An ESC followed by neither is dropped alone.
            _ => {
                while chars
                    .next_if(|c| ('\u{20}'..='\u{2f}').contains(c))
                    .is_some()
                {}
                chars.next_if(|c| ('\u{30}'..='\u{7e}').contains(c));
            }
        }
    }
    let mut out = String::with_capacity(plain.len());
    for (i, line) in plain.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        let last = line.rsplit('\r').find(|write| !write.is_empty());
        out.push_str(last.unwrap_or(""));
    }
    out
}

/// An agent conversation id as [`SessionOrigin::Agent`] carries it: trimmed,
/// and otherwise exactly as sent. `None` when it is blank, longer than
/// [`limits::CONVERSATION_ID_MAX_CHARS`], or holds whitespace or a control
/// character; the server refuses it as `BadConversation`. Unlike
/// [`normalise_model`], nothing is cut or stripped: a changed id would name
/// a different conversation.
pub fn check_conversation_id(id: &str) -> Option<&str> {
    let id = id.trim();
    let fits = !id.is_empty()
        && id.chars().count() <= limits::CONVERSATION_ID_MAX_CHARS
        && !id.chars().any(|c| c.is_whitespace() || c.is_control());
    fits.then_some(id)
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
    /// An `AnswerQuestion` whose answers do not fit the questions
    /// ([`answers_fit`]) or that names a plain approval, or a
    /// `ResolveApproval` approving a question (a question takes answers;
    /// denying declines it).
    BadAnswer,
    /// `SetModel.model` is blank.
    UnknownModel,
    /// `ClearContext.context` is already a context, of this session or
    /// another.
    ContextExists,
    /// `SwitchContext.context` or `MoveContext.context` is not a context of
    /// the session.
    UnknownContext,
    /// `SendMessage.context` is no longer the session's current context: a
    /// device cleared or switched contexts after the message was written.
    ContextChanged,
    /// `SwitchContext.context` or `MoveContext.context` was already moved
    /// to a session of its own.
    ContextMoved,
    /// `MoveContext.context` is the session's current context.
    ContextCurrent,
    /// `MoveContext.context` has no conversation to continue: the agent
    /// never ran in it.
    NoConversation,
    /// `CreateSession.conversation` is blank after trimming, longer than
    /// [`limits::CONVERSATION_ID_MAX_CHARS`], or contains whitespace or
    /// control characters. Whether the conversation exists is not checked:
    /// the agent settles that when the session first starts.
    BadConversation,
    /// `StopTask.task` is not among the session's live background tasks:
    /// it already ended, or never ran.
    UnknownTask,
    /// A `RunShell` or `StopShell` while the server has shell commands off
    /// ([`HelloResponse::shell`]).
    ShellDisabled,
    /// `StopShell.run` names no shell run of the session.
    UnknownRun,
    /// `StopShell.run` is already over.
    RunFinished,
    /// `RunShell.command` is blank after trimming, or longer than
    /// [`limits::SHELL_COMMAND_MAX_BYTES`].
    BadShellCommand,
    /// A `SetPermissionMode` asking for [`PermissionMode::Bypass`] while the
    /// server does not allow it ([`HelloResponse::bypass`]).
    BypassDisabled,
    /// `SendMessage.text` is longer than [`limits::MESSAGE_MAX_BYTES`].
    MessageTooLong,
    /// A command that would start new work (a message, a session, a
    /// context, a setting, a shell run) while the server's disk is below
    /// its free-space floor: the server has paused its agents until space
    /// is freed. Control (interrupts, stops, approval answers) is still
    /// taken. Send it again once the server has room. Also a `CropBlob`
    /// whose new blob found no room to be written.
    DiskLow,
    /// A command that would start new work (the same ones as [`DiskLow`](
    /// RejectReason::DiskLow)) while the server is stopping once its
    /// running turns and shell runs have ended. Control is still taken, so
    /// they can finish. Send it again once the server is back.
    ServerStopping,
    /// `DeleteBlob.blob` (or `CropBlob.blob`) is referenced by no event: a
    /// workdir blob, a lost one, or an id the server never minted.
    UnknownBlob,
    /// `DeleteBlob.blob` (or `CropBlob.blob`) was referenced by an event,
    /// but it has already been deleted or cropped.
    BlobGone,
    /// `CropBlob.blob` holds neither a tool output nor a shell run's output.
    NotCroppable,
    /// `CropBlob.keep` does not fit the blob: empty, unsorted, overlapping,
    /// a span of no lines, more than [`limits::CROP_SPANS_MAX`] spans, a
    /// span past the blob's last line, or every line kept.
    BadCrop,
    /// A `SendMessage` names an image hash the server has no staged upload
    /// for (never uploaded, or evicted); upload it and send again.
    UnknownAttachment,
    /// A `SendMessage` with more than [`limits::ATTACHMENTS_MAX`] attachments.
    TooManyAttachments,
    /// A `SendMessage` whose attachments together exceed
    /// [`limits::ATTACHMENTS_MAX_TOTAL_BYTES`].
    AttachmentsTooLarge,
}

impl RejectReason {
    /// Whether this refusal of a `ResolveApproval` or `AnswerQuestion`
    /// rules out every answer: the approval is unknown, already answered,
    /// or cancelled. Answering again is pointless; only a refusal of the
    /// answer itself ([`RejectReason::UnknownRule`],
    /// [`RejectReason::BadAnswer`]) leaves the approval open to another.
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
    /// Read alone before the rest is decoded, so a device can tell a
    /// server of another version from a broken one: keep its name and
    /// type in every version.
    pub protocol_version: u32,
    pub server_id: ServerId,
    /// The device the caller's certificate belongs to.
    pub device_id: DeviceId,
    /// Seq of the newest event; [`Seq::ZERO`] when the log is empty.
    pub head: Seq,
    /// Which log this is and where this run of the server took it up: a
    /// device tells from it whether the log still holds what it pulled.
    pub log: LogRun,
    /// Every device ever paired with the server, revoked ones included:
    /// the log names devices by id only, and its old events still name
    /// the revoked.
    pub devices: Vec<DeviceInfo>,
    /// Whether the server runs shell commands (`RunShell`); when off it
    /// refuses them as `ShellDisabled`.
    pub shell: bool,
    /// Whether the server's operator allows [`PermissionMode::Bypass`];
    /// when not, a `SetPermissionMode` asking for it is `BypassDisabled`.
    pub bypass: bool,
    /// When the login the agent runs as expires, read as the `hello` is
    /// answered; `None` when the agent's login does not expire or the
    /// server cannot tell. Past it the agent cannot start until someone
    /// logs it in again on the server.
    pub login_expires: Option<DateTime<Utc>>,
    /// Whether a device may log the agent in ([`path::AGENT_LOGIN_START`]);
    /// false when the agent has no login a device can run, or a login the
    /// server's environment sets takes precedence over one it would make.
    pub agent_login_offered: bool,
    /// Whether the server's latest definite word from the agent is "not
    /// logged in"; false when it is logged in or the server does not know
    /// yet. Reported even when [`Self::agent_login_offered`] is false.
    pub agent_logged_out: bool,
}

/// Query string of `GET /v1/hello`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HelloQuery {
    /// The run of the server this device last said `hello` to, so the
    /// answer can say what became of it ([`LogRun::since`]).
    pub since: Option<RunId>,
}

/// Request header naming the run of the server ([`LogRun::run`]) a device
/// expects to reach, as a hyphenated UUID. A device sends it on every
/// request once a `hello` told it the run, except the `hello` that asks
/// about a run with [`HelloQuery::since`]: that one is how it learns the
/// current run. The server answers a request whose header names another
/// run with 412 Precondition Failed and does nothing else with it, so a
/// request that reaches a server rolled back without the device noticing
/// (its connection to the old run never closed) changes nothing there.
/// Without the header nothing is checked; a malformed one is a 400.
///
/// A header rather than a query or body field: it applies to every
/// endpoint alike, whatever its query or body, and a command's body stays
/// the command the server dedups.
pub const RUN_HEADER: &str = "skywave-run";

/// The server's log as this run of the server serves it
/// ([`HelloResponse::log`]).
///
/// The log is append-only, so within one `log` it only ever loses events
/// at its end: a crash that drops the last commits, or the database
/// restored from an older backup. Either happens with the server down, and
/// every run of the server is recorded in the database with the head it
/// started from, so the runs after a loss start below it, and a restore
/// also takes away the record of the runs after the backup. A device that
/// last saw run `since` therefore knows the log lost events it holds when
/// that run is gone from the record, or a run after it started below what
/// the device has seen, even after the log has grown past it again.
/// Another `log` is another history altogether.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRun {
    pub log: LogId,
    pub run: RunId,
    /// What became of the run the query named; `None` when it named none.
    pub since: Option<RunsSince>,
}

/// What became of a run of the server ([`HelloQuery::since`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunsSince {
    /// This log has no record of it: another log, or one restored from
    /// before that run.
    Unknown,
    /// Recorded. `next_start` is the lowest head any later run started
    /// from; `None` when it is the current run.
    Known { next_start: Option<Seq> },
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

/// Response to `GET /v1/conversations` (no query), always 200: every
/// outcome is a domain answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ConversationsResult {
    /// The conversations the agent keeps for workdirs under the configured
    /// roots, at most [`limits::CONVERSATIONS_MAX`]. `truncated` when the
    /// cap or the scan's deadline cut the listing short.
    Listed {
        conversations: Vec<OutsideConversation>,
        truncated: bool,
    },
    /// The server's agent cannot read conversations it had outside Skywave.
    Unavailable,
}

/// One conversation of a [`ConversationsResult::Listed`]: one a
/// `CreateSession` can adopt by its `id` (`CreateSession::conversation`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutsideConversation {
    /// The agent's id for it, at most [`limits::CONVERSATION_ID_MAX_CHARS`].
    pub id: String,
    /// The directory it ran in, on the server.
    pub workdir: String,
    /// The agent's title for it, cut to [`limits::TITLE_MAX_CHARS`].
    pub title: Option<String>,
    /// The first line of its first message, cut to
    /// [`limits::FIRST_PROMPT_MAX_CHARS`].
    pub first_prompt: Option<String>,
    /// When it last changed.
    pub last_active: DateTime<Utc>,
    /// User and agent messages in it.
    pub messages: u32,
    /// The Skywave session one of whose contexts runs this conversation.
    pub session: Option<SessionId>,
}

/// Response to `GET /v1/storage` (no query): what the server's data takes
/// on its disk, read when asked. Sizes are bytes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StorageReport {
    /// The database file with its write-ahead log: the event log, its
    /// projections and everything else the server records.
    pub database: u64,
    /// Blobs events reference ([`EventKind::blobs`]).
    pub event_blobs: u64,
    /// Blobs workdir reads keep for their latest answers.
    pub workdir_blobs: u64,
    /// Output of shell runs still running, not yet a blob.
    pub shell_running: u64,
    /// The server's log file; `None` when it logs to stderr.
    pub log_file: Option<u64>,
    /// Free space on the data directory's file system; `None` when it
    /// could not be read.
    pub free: Option<u64>,
    /// The free-space floor below which the server pauses its agents
    /// (`[disk] min_free`); 0 when off.
    pub min_free: u64,
    /// Every session in the log, largest first.
    pub sessions: Vec<SessionStorage>,
    /// The largest event blobs, largest first, at most
    /// [`limits::STORAGE_TOP_MAX`].
    pub blobs: Vec<BlobStorage>,
    /// The shell runs taking the most, largest first, at most
    /// [`limits::STORAGE_TOP_MAX`].
    pub shell_runs: Vec<ShellRunStorage>,
}

/// One session of a [`StorageReport`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionStorage {
    pub session: SessionId,
    /// Its events in the log.
    pub events: u64,
    /// The stored JSON of those events: their share of the database.
    pub event_bytes: u64,
    /// Its events' blobs: how many and their size.
    pub blobs: u64,
    pub blob_bytes: u64,
}

/// One blob of a [`StorageReport`], named by the event referencing it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BlobStorage {
    pub id: BlobId,
    pub size: u64,
    pub session: SessionId,
    /// The event referencing it.
    pub seq: Seq,
    pub holds: BlobUse,
}

/// One shell run of a [`StorageReport`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ShellRunStorage {
    /// The `RunShell` command's id.
    pub run: CommandId,
    pub session: SessionId,
    /// The command's first line, cut to [`limits::FIRST_PROMPT_MAX_CHARS`].
    pub command_line: String,
    /// The stored JSON of its progress events (`ShellOutput`).
    pub event_bytes: u64,
    /// Its whole output's blob, once finished; 0 when it has none.
    pub output: u64,
    /// That blob, the one a `DeleteBlob` names; `None` when it has none
    /// or the blob was deleted.
    pub output_blob: Option<BlobId>,
}

/// Identifies one login a device started ([`LoginStartResult::Open`]); a
/// [`LoginFinish`] names it. Random (UUIDv4), not time-ordered: it is the
/// only thing tying a finish to its start, so it must not be guessable.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct LoginFlowId(pub Uuid);

impl LoginFlowId {
    /// A fresh random id.
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Response to `POST /v1/agent-login/start` (no body), always 200: every
/// outcome is a domain answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum LoginStartResult {
    /// The user opens `url`, logs in there and pastes the code it shows
    /// back with a [`LoginFinish`] naming `flow`. The URL is at most
    /// [`limits::LOGIN_URL_MAX_BYTES`].
    Open { flow: LoginFlowId, url: String },
    /// Another login is being started or finished; try again after it.
    Busy,
    /// The server's agent has no login a device can run
    /// ([`HelloResponse::agent_login_offered`] is false).
    Unavailable,
    /// Starting the login failed; `reason` is bounded text
    /// ([`limits::REFUSAL_MAX_CHARS`]).
    Failed { reason: String },
}

/// Body of `POST /v1/agent-login/finish`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct LoginFinish {
    /// The flow [`LoginStartResult::Open`] named.
    pub flow: LoginFlowId,
    /// The code the login page showed, at most
    /// [`limits::LOGIN_CODE_MAX_BYTES`]. A secret: never logged, so
    /// `Debug` prints `<redacted>` in its place.
    pub code: String,
}

impl std::fmt::Debug for LoginFinish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginFinish")
            .field("flow", &self.flow)
            .field("code", &format_args!("<redacted>"))
            .finish()
    }
}

/// Response to `POST /v1/agent-login/finish`, always 200: every outcome is
/// a domain answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum LoginFinishResult {
    /// The agent is logged in. `expires` is when that login expires, as
    /// [`HelloResponse::login_expires`] would report it now.
    LoggedIn { expires: Option<DateTime<Utc>> },
    /// The server has no login under that flow: never started, already
    /// finished, or given up on.
    NoFlow,
    /// Finishing the login failed; `reason` is bounded text
    /// ([`limits::REFUSAL_MAX_CHARS`]).
    Failed { reason: String },
}

/// Response to `PUT` [`path::ATTACHMENT_ROUTE`], always 200: every outcome
/// is a domain answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum UploadResult {
    /// The image is staged, or already was. `size` is in bytes and
    /// `media_type` is the sniffed type.
    Stored { size: u64, media_type: ImageType },
    /// Over [`limits::ATTACHMENT_MAX_BYTES`]; nothing stored.
    TooLarge,
    /// Not PNG, JPEG, GIF or WEBP.
    NotAnImage,
    /// The body does not hash to the path's sha256.
    HashMismatch,
    /// The server's disk is low; nothing stored.
    DiskLow,
}

/// Response to `GET /v1/usage-limits` (no query), always 200: every outcome
/// is a domain answer. The limits belong to the account the server's agent
/// runs as, not to a session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum UsageLimitsResult {
    /// The limits the agent reported at `at`, in its order; empty when the
    /// account is under none.
    Read {
        at: DateTime<Utc>,
        limits: Vec<UsageLimit>,
    },
    /// The server's agent cannot report usage limits.
    Unavailable,
    /// Asking the agent failed; `reason` is bounded text
    /// ([`limits::REFUSAL_MAX_CHARS`]).
    Failed { reason: String },
}

/// One limit of a [`UsageLimitsResult::Read`]. A figure the agent did not
/// report is `None`, never 0.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct UsageLimit {
    pub window: LimitWindow,
    /// What the limit covers when it is not all use, e.g. one model's name,
    /// cut to [`limits::TITLE_MAX_CHARS`].
    pub scope: Option<String>,
    /// How much of it is used, in percent; past 100 when overdrawn.
    pub percent: Option<u32>,
    pub severity: Option<LimitSeverity>,
    /// When the window starts over.
    pub resets_at: Option<DateTime<Utc>>,
}

/// The span a [`UsageLimit`] counts use over.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitWindow {
    /// A few hours from the first use in it.
    Session,
    Weekly,
}

/// How close the agent says a [`UsageLimit`] is to stopping it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitSeverity {
    Normal,
    Warning,
}

/// Query string of `GET /v1/workdir/{changes,log,list,file}`. Names a session,
/// not a raw path: the server resolves the root from the session's workdir,
/// so a device reads only workdirs that sessions run in.
///
/// Outcomes every workdir request shares, folded into each result enum:
/// `UnknownSession` (no such session on the server), `WorkdirGone` (the
/// workdir no longer exists, no longer canonicalises to the stored path, or
/// left the roots), `NotAllowed` (`path` is absolute, or its canonical target
/// is not under the canonical workdir) and `NotFound` (`path` does not
/// exist; list and file only).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkdirQuery {
    pub session: SessionId,
    /// Relative to the session's workdir, `/` separated, empty for the
    /// workdir itself; ignored by `changes` and `log`.
    #[serde(default)]
    pub path: String,
}

/// Response to `GET /v1/workdir/changes`, always 200: every outcome is a
/// domain answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ChangesResult {
    /// The work tree's changes against `HEAD`, plus untracked files.
    Changes(ChangeSet),
    /// The workdir is in no git work tree.
    NotGit,
    /// git is missing or exited unexpectedly; `reason` is bounded text.
    Failed {
        reason: String,
    },
    UnknownSession,
    WorkdirGone,
}

/// One snapshot of what changed in a workdir.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ChangeSet {
    /// Server clock when the snapshot was taken.
    pub at: DateTime<Utc>,
    /// The checked-out branch; `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// Short commit of `HEAD`; `None` on an unborn branch.
    pub head: Option<String>,
    /// Sorted by path, at most [`limits::WORKDIR_CHANGES_MAX`].
    ///
    /// `path` alone is not unique here: `git rm --cached f` with `f` still on
    /// disk gives a `Deleted` `f` and an `Untracked` `f`. An entry is
    /// identified by its path and whether it is `Untracked`.
    pub files: Vec<ChangedFile>,
    /// Files changed in all; more than `files.len()` when capped.
    pub total: u32,
}

/// One changed file in a [`ChangeSet`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ChangedFile {
    /// Relative to the workdir.
    pub path: String,
    /// Old path of a rename.
    pub from: Option<String>,
    pub change: FileChange,
    /// Lines added; `None` for a binary file.
    pub added: Option<u32>,
    /// Lines removed; `None` for a binary file.
    pub removed: Option<u32>,
    pub diff: DiffBody,
}

/// How a [`ChangedFile`] changed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
    Untracked,
    Conflicted,
}

/// Where a [`ChangedFile`]'s diff is.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "diff", rename_all = "snake_case")]
pub enum DiffBody {
    /// The unified diff, fetched as a blob.
    Blob(BlobRef),
    /// git found the file binary; there is no text diff.
    Binary,
    /// The diff is `bytes` long, past [`limits::WORKDIR_DIFF_MAX_BYTES`].
    TooLarge { bytes: u64 },
    /// Left out: the snapshot reached [`limits::WORKDIR_DIFFS_MAX_BYTES`].
    Omitted,
}

/// Response to `GET /v1/workdir/log`, always 200: every outcome is a
/// domain answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum LogResult {
    /// The newest commits on `HEAD`.
    Log(CommitLog),
    /// The workdir is in no git work tree.
    NotGit,
    /// git is missing or exited unexpectedly; `reason` is bounded text.
    Failed {
        reason: String,
    },
    UnknownSession,
    WorkdirGone,
}

/// The commits a workdir's `HEAD` points into, newest first.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CommitLog {
    /// Server clock when the log was read.
    pub at: DateTime<Utc>,
    /// The checked-out branch; `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// At most [`limits::WORKDIR_LOG_MAX`], newest first; empty on an
    /// unborn branch.
    pub commits: Vec<Commit>,
    /// Older commits exist past `commits`.
    pub more: bool,
}

/// One commit in a [`CommitLog`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Full object name.
    pub sha: String,
    /// Abbreviated as git abbreviates it in this repo.
    pub short: String,
    /// Author name.
    pub author: String,
    /// Author date; `None` when the commit's is missing or out of range.
    pub time: Option<DateTime<Utc>>,
    /// First line of the message.
    pub subject: String,
}

/// Response to `GET /v1/workdir/list`, always 200: every outcome is a domain
/// answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ListResult {
    /// The entries of the directory at `path`, the canonical path relative
    /// to the workdir. `truncated` when [`limits::WORKDIR_LIST_MAX`] cut the
    /// listing short.
    Listed {
        at: DateTime<Utc>,
        path: String,
        entries: Vec<DirEntry>,
        truncated: bool,
    },
    NotADirectory,
    UnknownSession,
    WorkdirGone,
    NotAllowed,
    NotFound,
}

/// One entry of a [`ListResult::Listed`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Size in bytes, for a file.
    pub size: Option<u64>,
}

/// What a [`DirEntry`] is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
    /// A FIFO, socket or device.
    Other,
    /// A symlink, and what it leads to.
    Link {
        to: LinkTarget,
    },
}

/// Where a symlink in a workdir leads.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkTarget {
    Dir,
    File,
    /// The target is not under the workdir.
    Outside,
    /// The target does not exist.
    Dangling,
}

/// Response to `GET /v1/workdir/file`, always 200: every outcome is a domain
/// answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum FileResult {
    /// The file at `path`, the canonical path relative to the workdir.
    File {
        at: DateTime<Utc>,
        path: String,
        size: u64,
        content: FileBody,
    },
    IsADirectory,
    /// A FIFO, socket or device: never opened, as a FIFO read blocks
    /// forever.
    NotAFile,
    UnknownSession,
    WorkdirGone,
    NotAllowed,
    NotFound,
}

/// Where a [`FileResult::File`]'s content is.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "content", rename_all = "snake_case")]
pub enum FileBody {
    /// The bytes, fetched as a blob.
    Blob(BlobRef),
    /// Not text; not served.
    Binary,
    /// Past [`limits::WORKDIR_FILE_MAX_BYTES`].
    TooLarge,
}

/// HTTP paths of the v1 API.
pub mod path {
    use crate::{BlobId, ContentHash};

    pub const PAIR: &str = "/v1/pair";
    pub const HELLO: &str = "/v1/hello";
    pub const COMMANDS: &str = "/v1/commands";
    pub const EVENTS: &str = "/v1/events";
    pub const DIRS: &str = "/v1/dirs";
    /// The agent's conversations outside Skywave: [`crate::ConversationsResult`].
    pub const CONVERSATIONS: &str = "/v1/conversations";
    /// What the server's data takes on its disk: [`crate::StorageReport`].
    pub const STORAGE: &str = "/v1/storage";
    /// The agent's usage limits: [`crate::UsageLimitsResult`].
    pub const USAGE_LIMITS: &str = "/v1/usage-limits";
    /// Start logging the agent in: [`crate::LoginStartResult`].
    pub const AGENT_LOGIN_START: &str = "/v1/agent-login/start";
    /// Finish a login with the code the user pasted:
    /// [`crate::LoginFinish`] answering [`crate::LoginFinishResult`].
    pub const AGENT_LOGIN_FINISH: &str = "/v1/agent-login/finish";
    /// What changed in a session's workdir: [`crate::ChangesResult`].
    pub const WORKDIR_CHANGES: &str = "/v1/workdir/changes";
    /// The newest commits on a session's workdir `HEAD`: [`crate::LogResult`].
    pub const WORKDIR_LOG: &str = "/v1/workdir/log";
    /// One directory of a session's workdir: [`crate::ListResult`].
    pub const WORKDIR_LIST: &str = "/v1/workdir/list";
    /// One file of a session's workdir: [`crate::FileResult`].
    pub const WORKDIR_FILE: &str = "/v1/workdir/file";
    /// Route template for blob downloads; `{id}` is a [`BlobId`].
    pub const BLOB_ROUTE: &str = "/v1/blobs/{id}";

    /// Concrete path for downloading one blob.
    pub fn blob(id: BlobId) -> String {
        format!("/v1/blobs/{}", id.0)
    }

    /// Route template for staging an image, `HEAD` and `PUT`; `{sha256}` is
    /// a [`ContentHash`] in hex.
    pub const ATTACHMENT_ROUTE: &str = "/v1/attachments/{sha256}";

    /// Concrete path for staging one image.
    pub fn attachment(sha256: ContentHash) -> String {
        format!("/v1/attachments/{}", sha256.to_hex())
    }
}

/// Transport compression of event batches and blob bodies (D45).
///
/// A request that sends `Accept-Encoding: zstd` ([`ZSTD`]) gets a body of
/// at least [`COMPRESS_MIN_BYTES`] from `GET` [`path::EVENTS`] or a blob
/// download as one zstd frame, with `Content-Encoding: zstd`; a shorter
/// body, or a request that does not ask, gets the bytes as they are. A
/// batch's frame is sent whole, with its `Content-Length`; a blob's is
/// streamed as it is made, without one. The frame is the transport's alone: hashes,
/// sizes and progress all count the decoded bytes.
///
/// A blob's `Range` and `Content-Range` count decoded bytes too, unlike
/// plain HTTP (where a range of a `Content-Encoding` counts encoded ones):
/// a resumed download asks for the decoded bytes from where its `.part`
/// file ends, and the server compresses exactly that range as a fresh
/// frame. A compressed blob response names its decoded length in
/// [`DECODED_LENGTH_HEADER`].
pub mod encoding {
    /// The `Accept-Encoding` and `Content-Encoding` token: one zstd frame.
    pub const ZSTD: &str = "zstd";
    /// Bodies shorter than this travel uncompressed: a frame's overhead
    /// and the CPU would buy nothing on them.
    pub const COMPRESS_MIN_BYTES: u64 = 1024;
    /// On a compressed blob response, the number of decoded bytes its
    /// frame holds: the blob's size for a 200, the range's length for a
    /// 206 (which `Content-Range` also gives).
    pub const DECODED_LENGTH_HEADER: &str = "skywave-decoded-length";

    /// Whether an `Accept-Encoding` value lists [`ZSTD`] (any weight but
    /// an explicit `q=0`).
    pub fn accepts_zstd(accept_encoding: &str) -> bool {
        accept_encoding.split(',').any(|coding| {
            let mut parts = coding.split(';').map(str::trim);
            parts
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case(ZSTD))
                && !parts.any(|param| {
                    param
                        .strip_prefix("q=")
                        .and_then(|q| q.parse::<f32>().ok())
                        .is_some_and(|q| q == 0.0)
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_encoding_lists_zstd() {
        use encoding::accepts_zstd;
        for yes in ["zstd", "gzip, zstd", "ZSTD;q=0.5", " br , zstd ; q=1"] {
            assert!(accepts_zstd(yes), "{yes}");
        }
        for no in ["", "gzip", "zstdx", "zstd;q=0", "zstd; q=0.0", "identity"] {
            assert!(!accepts_zstd(no), "{no}");
        }
    }

    #[test]
    fn seq_next_saturates_instead_of_overflowing() {
        assert_eq!(Seq::ZERO.next(), Seq(1));
        assert_eq!(Seq(u64::MAX).next(), Seq(u64::MAX));
    }

    #[test]
    fn a_long_messages_blob_holds_a_message() {
        let blob = BlobRef {
            id: BlobId::new(),
            size: 40_000,
            sha256: ContentHash::of(b""),
        };
        let agent = |full| EventKind::AgentMessage {
            text: "# Report".into(),
            parent: None,
            full,
        };
        let imported = |item| EventKind::Imported { at: None, item };
        for kind in [
            agent(Some(Full::Blob(blob.clone()))),
            imported(ImportedItem::UserMessage {
                text: "log:".into(),
                full: Some(Full::Blob(blob.clone())),
            }),
            imported(ImportedItem::AgentMessage {
                text: "# Report".into(),
                full: Some(Full::Blob(blob.clone())),
            }),
        ] {
            let uses: Vec<_> = kind.blobs().collect();
            assert_eq!(uses, [(BlobUse::Message, &blob)], "{kind:?}");
        }
        // A whole or lost message references none.
        assert_eq!(agent(None).blobs().count(), 0);
        assert_eq!(agent(Some(Full::Lost)).blobs().count(), 0);
        // A user message yields its images, in order, each as an attachment.
        let (a, b) = (
            Attachment {
                blob: BlobRef {
                    id: BlobId::new(),
                    size: 4,
                    sha256: ContentHash::of(b"png1"),
                },
                media_type: ImageType::Png,
            },
            Attachment {
                blob: BlobRef {
                    id: BlobId::new(),
                    size: 4,
                    sha256: ContentHash::of(b"jpg2"),
                },
                media_type: ImageType::Jpeg,
            },
        );
        let with_images = EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: "look".into(),
            attachments: vec![a.clone(), b.clone()],
        };
        let uses: Vec<_> = with_images.blobs().collect();
        assert_eq!(
            uses,
            [
                (BlobUse::Attachment, &a.blob),
                (BlobUse::Attachment, &b.blob)
            ]
        );
        let text_only = EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: "look".into(),
            attachments: Vec::new(),
        };
        assert_eq!(text_only.blobs().count(), 0);
    }

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
            attachments: Vec::new(),
        };
        assert_eq!(message.archived_effect(), Some(false));
        for kind in [
            EventKind::AgentMessage {
                parent: None,
                text: "x".into(),
                full: None,
            },
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
    fn calls_unanswered_is_an_agent_event_with_no_device_or_archived_effect() {
        let kind = EventKind::CallsUnanswered {
            calls: vec![CallId::new()],
        };
        assert_eq!(kind.device(), None);
        assert!(!kind.logged_by_command());
        assert_eq!(kind.archived_effect(), None);
    }

    #[test]
    fn imported_history_is_the_daemons_and_its_end_the_commands() {
        let imported = EventKind::Imported {
            at: None,
            item: ImportedItem::UserMessage {
                text: "hi".into(),
                full: None,
            },
        };
        let finished = EventKind::ImportFinished {
            outcome: ImportOutcome::Imported { items: 1 },
        };
        // (kind, logged by a command)
        for (kind, by_command) in [(&imported, false), (&finished, true)] {
            assert_eq!(kind.device(), None, "{kind:?}");
            assert_eq!(kind.logged_by_command(), by_command, "{kind:?}");
            // An imported user message is history: it brings nothing back.
            assert_eq!(kind.archived_effect(), None, "{kind:?}");
        }
    }

    #[test]
    fn shell_events_name_their_device_and_who_logs_them() {
        let by = DeviceId::new();
        let run = CommandId::new();
        let requested = EventKind::ShellRequested {
            command: "make".into(),
            when: RunWhen::Always,
            by,
        };
        let stop = EventKind::ShellStopRequested { run, by };
        let started = EventKind::ShellStarted {
            run,
            cwd: "/w".into(),
        };
        let output = EventKind::ShellOutput {
            run,
            tail: "x".into(),
            lines: 1,
            bytes: 1,
        };
        let finished = EventKind::ShellFinished {
            run,
            by,
            started_at: None,
            command_line: "make".into(),
            end: ShellEnd::Exited { code: 0 },
            cwd: None,
            preview: String::new(),
            lines: 0,
            output: None,
            truncated: false,
        };
        // (kind, device, logged by a command, archived effect)
        let cases = [
            (&requested, Some(by), true, Some(false)),
            (&stop, Some(by), true, None),
            (&started, None, false, None),
            (&output, None, false, None),
            (&finished, Some(by), false, None),
        ];
        for (kind, device, by_command, archived) in cases {
            assert_eq!(kind.device(), device, "{kind:?}");
            assert_eq!(kind.logged_by_command(), by_command, "{kind:?}");
            assert_eq!(kind.archived_effect(), archived, "{kind:?}");
        }
    }

    #[test]
    fn only_a_zero_exit_is_success() {
        assert!(ShellEnd::Exited { code: 0 }.is_success());
        for end in [
            ShellEnd::Exited { code: 1 },
            ShellEnd::Signalled { signal: 9 },
            ShellEnd::Stopped,
            ShellEnd::Skipped,
            ShellEnd::Lost,
            ShellEnd::NotStarted { reason: "x".into() },
        ] {
            assert!(!end.is_success(), "{end:?}");
        }
    }

    #[test]
    fn strip_ansi_drops_colours_and_cursor_moves() {
        assert_eq!(strip_ansi("\x1b[1;31merror\x1b[0m: bad"), "error: bad");
        assert_eq!(strip_ansi("a\x1b[2K\x1b[1Ab\x1b[10;20Hc"), "abc");
    }

    #[test]
    fn strip_ansi_keeps_the_text_of_an_osc_8_link() {
        let link = "see \x1b]8;;https://x.test/\x1b\\the docs\x1b]8;;\x1b\\ now";
        assert_eq!(strip_ansi(link), "see the docs now");
        assert_eq!(strip_ansi("\x1b]0;title\x07done"), "done");
    }

    #[test]
    fn strip_ansi_drops_the_payload_of_string_sequences() {
        // APC (a kitty inline image), DCS, SOS and PM, by `ESC \` or BEL.
        assert_eq!(strip_ansi("a\x1b_Ga=T,f=100;iVBORw0K\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1bP1$r0m\x1b\\b"), "ab");
        assert_eq!(strip_ansi("a\x1bXsos\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b^pm\x1b\\b"), "ab");
    }

    #[test]
    fn strip_ansi_parses_the_escape_that_cuts_a_string_short() {
        assert_eq!(strip_ansi("\x1b]0;title\x1b[31mred"), "red");
        assert_eq!(strip_ansi("\x1b_payload\x1b[0mok"), "ok");
        // After a BEL, a following `ESC \` is a sequence of its own.
        assert_eq!(strip_ansi("\x1b]0;t\x07\x1b\\x"), "x");
    }

    #[test]
    fn strip_ansi_ends_a_csi_at_a_char_it_cannot_hold() {
        assert_eq!(strip_ansi("\x1b[31\x1b[0mx"), "x");
        assert_eq!(strip_ansi("\x1b[1\nfoo"), "\nfoo");
        assert_eq!(strip_ansi("\x1b[1世界"), "世界");
    }

    #[test]
    fn strip_ansi_drops_a_lone_escape() {
        assert_eq!(strip_ansi("a\x1b"), "a");
        assert_eq!(strip_ansi("a\x1b7b\x1b(Bc"), "abc");
        // An ESC before a char that starts no sequence goes alone.
        assert_eq!(strip_ansi("a\x1b\nb"), "a\nb");
    }

    #[test]
    fn strip_ansi_collapses_carriage_return_overwrites() {
        assert_eq!(
            strip_ansi("start\n 10%\r 50%\r100%\ndone\r\n"),
            "start\n100%\ndone\n"
        );
        // A trailing `\r` leaves the last write on screen.
        assert_eq!(strip_ansi("\r 10%\r 99%\r"), " 99%");
        assert_eq!(strip_ansi("a\r\nb"), "a\nb");
    }

    #[test]
    fn strip_ansi_keeps_utf8_intact() {
        let text = "héllo → 世界 🚀";
        assert_eq!(strip_ansi(text), text);
        assert_eq!(strip_ansi("\x1b[32m✓\x1b[0m 世界"), "✓ 世界");
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
            EventKind::ContextMoved {
                to: SessionId::new(),
                by,
            },
        ] {
            assert_eq!(kind.device(), Some(by), "{kind:?}");
            assert!(kind.logged_by_command(), "{kind:?}");
            assert_eq!(kind.archived_effect(), None, "{kind:?}");
        }
        // Positive control: the classifiers do tell kinds apart.
        let created = EventKind::SessionCreated {
            workdir: "/w".into(),
            context_name: "start".into(),
            from: None,
        };
        assert_eq!(created.device(), None);
        assert!(created.logged_by_command());
        assert!(
            !EventKind::AgentMessage {
                parent: None,
                text: "x".into(),
                full: None,
            }
            .logged_by_command()
        );
    }

    #[test]
    fn a_moved_contexts_session_created_is_logged_by_the_daemon() {
        // The `MoveContext` names itself on the source's `ContextMoved`, the
        // last event it appends; the new session's `SessionCreated` before it
        // names no command.
        let session = SessionId::new();
        let moved = EventKind::SessionCreated {
            workdir: "/w".into(),
            context_name: "fork".into(),
            from: Some(SessionOrigin::Context(ContextRef {
                session,
                context: ContextId::new(),
            })),
        };
        assert!(!moved.logged_by_command());
        assert_eq!(moved.device(), None);
        assert_eq!(moved.archived_effect(), None);
    }

    #[test]
    fn an_adopting_session_created_is_logged_by_the_daemon() {
        // The adopting `CreateSession` names itself on the `ImportFinished`
        // after the imported history, the last event it appends; its
        // `SessionCreated` names no command.
        let adopted = EventKind::SessionCreated {
            workdir: "/w".into(),
            context_name: "start".into(),
            from: Some(SessionOrigin::Agent {
                conversation: "9b2c1e4a-5f3d-4c8e-a1b2-c3d4e5f6a7b8".into(),
            }),
        };
        assert!(!adopted.logged_by_command());
        assert_eq!(adopted.device(), None);
        assert_eq!(adopted.archived_effect(), None);
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

    #[test]
    fn sniff_recognises_each_image_type_from_its_header() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0];
        let gif = *b"GIF89a";
        let webp = *b"RIFF\x24\x00\x00\x00WEBPVP8 ";
        assert_eq!(ImageType::sniff(&png), Some(ImageType::Png));
        assert_eq!(ImageType::sniff(&jpeg), Some(ImageType::Jpeg));
        assert_eq!(ImageType::sniff(&gif), Some(ImageType::Gif));
        assert_eq!(ImageType::sniff(&webp), Some(ImageType::Webp));
    }

    #[test]
    fn sniff_refuses_what_is_not_one_of_the_four() {
        assert_eq!(ImageType::sniff(b"%PDF-1.4"), None);
        assert_eq!(ImageType::sniff(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(ImageType::sniff(b"RIFF\x00\x00\x00\x00WEB"), None);
        assert_eq!(ImageType::sniff(&[]), None);
    }

    #[test]
    fn an_image_type_has_its_mime_string() {
        assert_eq!(ImageType::Png.mime(), "image/png");
        assert_eq!(ImageType::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageType::Gif.mime(), "image/gif");
        assert_eq!(ImageType::Webp.mime(), "image/webp");
    }
}
