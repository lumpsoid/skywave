//! Skywave client core: all client logic lives here; heads only render.
//!
//! Heads use [`Core::open`], [`Core::dispatch`] with an [`Action`], and
//! [`Core::subscribe`] to receive [`ViewState`] snapshots (architecture.md "Client core"). They
//! depend on this crate only, never on `skywave-proto`. `Core::open` also
//! starts the sync engine ([`EngineConfig`]), which pairs, pulls the log,
//! drains the outbox and reconnects; heads that background call
//! [`Core::suspend`] and [`Core::resume`].
//!
//! - [`view`]: the snapshot types heads render.
//! - [`action`]: the user intents heads dispatch.
//! - [`seam`]: the engine-side API for transport, sync and blob downloads.
//! - [`store`]: the local replica (SQLite) — events, session projection,
//!   outbox, cursor, blobs, settings.
//! - [`projection`]: the pure fold from events to session summaries.
//! - [`identity`]: the device's pairing — its credentials behind
//!   [`Identity`] and the server's addresses — as one stored record.
//! - [`transport`]: the [`Transport`] trait to the server and its typed
//!   errors; [`HttpsTransport`] is the mTLS implementation.
//! - blob downloads ([`blob_dir`]) follow the [`BlobPolicy`] apart from
//!   event sync.
//! - [`workdir`]: reads of a session's workdir and the answers the replica
//!   keeps.
//! - [`search`]: what a search of the open session's timeline matches.
//! - [`pairing`]: the pairing client, plus code, fingerprint and pairing
//!   URI parsing for heads.

pub mod action;
mod address;
pub mod attachment;
mod backoff;
mod blobs;
mod client;
mod completion;
mod conversations;
mod encoding;
mod engine;
mod https;
pub mod identity;
mod limits;
mod lines;
pub mod lock;
mod login;
mod model;
pub mod pairing;
pub mod projection;
pub mod seam;
pub mod search;
mod shell_history;
mod snippets;
mod storage;
pub mod store;
mod tls;
pub mod transport;
pub mod view;
pub mod workdir;
mod worktrees;

pub use action::{Action, Route};
pub use address::{AddressError, DEFAULT_PORT, ServerAddress, ServerAddresses};
pub use backoff::{Backoff, Jitter};
pub use blobs::{blob_dir, read_head};
pub use client::{Core, CoreError, ShutdownError, SubscriptionId};
pub use engine::{
    CONVERSATIONS_TIMEOUT, Connector, DEFAULT_WAIT_SECS, EngineConfig, LOGIN_TIMEOUT,
    MAX_SERVER_ERRORS, STORAGE_TIMEOUT, USAGE_LIMITS_TIMEOUT, WALL_CHECK, WORKTREES_TIMEOUT,
    WallClock, default_device_name,
};
pub use https::{HttpsTransport, TransportConfig};
pub use identity::{DeviceKey, Identity, IdentityError, PairedCredentials, Pairing};
pub use lines::LineIndex;
pub use lock::LockError;
pub use login::LoginCode;
pub use pairing::{
    PairError, PairUri, Paired, normalize_code, pair, pair_and_report, parse_pair_uri,
};
pub use projection::{BlobFate, ContextFill, SessionSummary, SessionUsage, TurnRecord};
pub use seam::{Request, Seam};
pub use search::Query;
pub use skywave_proto::PROTOCOL_VERSION;
pub use skywave_proto::{ContentHash, Fingerprint, ImageType};
pub use store::{
    Applied, AttachmentRefusal, BlobPolicy, BlobRecord, BlobStatus, LogCheck, OutboxEntry,
    OutboxStatus, PendingCrop, ReadMark, ServerFacts, Settings, Snapshot, Store, StoreError,
};
pub use transport::{
    BlobBody, BlobDownload, BoxFuture, ExpectedRun, Transport, TransportError, UploadFile,
};
pub use view::{
    AgentSetting, AgentView, AgentWork, Answer, ApprovalId, ApprovalPrompt, ApprovalStatus,
    Attention, BackgroundTask, BlobChange, BlobCommand, BlobId, BlobPending, BlobState, BlobView,
    BodyFormat, BodyView, CallId, Candidate, CandidateSource, CommandId, Completion,
    CompletionStatus, ComposerMode, ContextId, ContextView, ConversationItem, ConversationPicker,
    ConversationsStatus, ConversationsUnavailable, CropContent, CropView, Decision, Delivery,
    DeviceId, DeviceRef, Draft, Effort, Entry, ErrorInfo, FailReason, FullView, ImportOutcome,
    ImportedEntry, Link, LinkFailure, LinkRejection, ListKind, MessageImage, ModelChoice, Outgoing,
    PairedInfo, PairedLink, PairingStatus, PairingView, Pause, PendingApprovalRef, PendingSession,
    PermissionMode, PushState, QUESTION_TOOL, Question, QuestionAnswer, QuestionOption,
    QuestionPrompt, Refusal, RejectReason, RuleScope, RuleSuggestion, RunWhen, SHELL_ALERT_AFTER,
    Schedule, ScheduleId, Screen, Seq, ServerAddressesView, SessionId, SessionListView, SessionRef,
    SessionRow, SessionState, SessionView, SettingChange, SettingsView, ShellAlert, ShellEnd,
    ShellRun, ShellRunState, ShellView, Snippet, SnippetId, SubagentHeader, SubagentStatus,
    SubagentUsage, TaskId, TaskKind, TimelineItem, ToolApproval, ToolKind, ToolOutcome, ToolView,
    TurnUsage, UsageSession, UsageView, ViewState, WorkdirBody, WorkdirDownload, WorkdirFetch,
    WorkdirPane, WorkdirPush, WorkdirView,
};
pub use view::{
    BlobStorage, BlobUse, LineSpan, SessionStorage, ShellRunStorage, StorageReport, StorageStatus,
    StorageUnavailable, StorageView,
};
pub use view::{
    ChangeSet, ChangedFile, ChangesResult, DiffBody, DirEntry, EntryKind, FileBody, FileChange,
    FileResult, LinkTarget, ListResult,
};
pub use view::{
    LimitSeverity, LimitWindow, SessionInfoView, UsageLimit, UsageLimitsResult, UsageLimitsStatus,
    UsageLimitsUnavailable, UsageLimitsView,
};
pub use view::{LoginCodeRefused, LoginFlowView, LoginNoAnswer, LoginOutcome, LoginView};
pub use view::{WorktreeSession, WorktreesStatus, WorktreesUnavailable, WorktreesView};
