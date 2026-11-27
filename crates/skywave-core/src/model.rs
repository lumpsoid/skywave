//! The pure client state machine.
//!
//! [`Model`] mirrors the local replica in memory plus the ephemeral state
//! (link, pairing progress, current screen). Actions and engine
//! notifications fold into it without I/O; actions return the [`Effect`]s
//! the [`Core`](crate::Core) must carry out, persistence first. The
//! [`ViewState`] is a pure function of the model.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use skywave_proto::limits::{ATTACHMENTS_MAX, ATTACHMENTS_MAX_TOTAL_BYTES};
use skywave_proto::{
    BackgroundTask, BlobRef, Command, CommandKind, CommandResult, ContentHash, ConversationsResult,
    DeviceInfo, DirsResult, Event, EventBatch, EventKind, Fingerprint, Full, ImportedItem,
    LoginFinishResult, LoginStartResult, PermissionMode, QuestionAnswer, RejectReason, RepoListing,
    RuleSuggestion, SafetyJudgment, ScreenAction, ScreenOutcome, ScreenShot, SessionOrigin,
    SessionState as LogState, TaskId, ToolCall, ToolKind, ToolResult, WorktreeList, answers_fit,
};

use crate::action::{Action, Route};
use crate::address::{ServerAddress, ServerAddresses};
use crate::attachment::{AttachedFile, AttachedImage};
use crate::completion::Completer;
use crate::conversations::Conversations;
use crate::identity::{Identity, Pairing};
use crate::limits::UsageLimits;
use crate::login::AgentLogin;
use crate::projection::{
    BlobFate, FIRST_PROMPT_MAX_CHARS, SessionSummary, TurnRecord, blob_refs, first_line, is_notable,
};
use crate::seam::Request;
use crate::shell_history::ShellHistory;
use crate::snippets::Snippets;
use crate::storage::Storage;
use crate::store::WorkdirStored;
use crate::store::{
    AttachmentRefusal, BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxFile, OutboxStatus,
    ReadMark, SafetyRule, ServerFacts, Settings, Snapshot, StoreError,
};
use crate::tls;
use crate::transport::TransportError;
use crate::view::{
    AgentSetting, AgentView, Answer, AppCapture, AppScreenView, ApprovalId, ApprovalPrompt,
    ApprovalStatus, ApprovalsView, Asks, Attention, BlobChange, BlobCommand, BlobId, BlobPending,
    BlobState, BlobUse, BlobView, BodyView, CallId, ChangedFile, ChangesResult, CommandId,
    ComposerMode, ContextId, ContextView, CropContent, CropView, Decision, Delivery, DeviceId,
    DeviceRef, DeviceStorage, DiffBody, Draft, Entry, ErrorInfo, FailReason, FeedbackSent,
    FileBody, FileChange, FileResult, FullView, IdleAlert, ImportedEntry, LineSpan, Link,
    LinkRejection, ListKind, LoginView, MessageImage, ModelChoice, Outgoing, PairedInfo,
    PairedLink, PairingStatus, PairingView, Pause, PendingApprovalRef, PendingChange,
    PendingSafetyRule, PendingSession, QUESTION_TOOL, Question, QuestionPrompt, RunWhen,
    SHELL_ALERT_AFTER, SafetyFeedbackView, SafetyRuleView, Screen, ScreenRequest,
    ScreenRequestState, SearchScope, SearchView, Seq, ServerAddressesView, SessionId,
    SessionInfoView, SessionListView, SessionRef, SessionRow, SessionState, SessionView,
    SettingChange, SettingsView, ShellAlert, ShellRun, ShellRunState, ShellView, Snippet,
    SnippetId, StorageReport, StorageSession, StorageView, SubagentHeader, SubagentStatus,
    SubagentUsage, TimelineItem, ToolApproval, ToolOutcome, ToolView, UsageLimitsResult,
    UsageLimitsUnavailable, UsageSession, UsageView, ViewState, WaitingApproval, WorkdirBody,
    WorkdirDownload, WorkdirFetch, WorkdirPane, WorkdirView, WorktreeSession, WorktreesView,
};
use crate::workdir::{PushOutcome, WorkdirAnswer, WorkdirAsk, Workdirs, parent};
use crate::worktrees::Worktrees;

/// Work the core must do for an action, in order. Persistence effects come
/// before the requests that make the action visible to the network.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Effect {
    /// Write the command to the outbox, with the files it carries (the
    /// session's draft files it was written from, in order).
    Enqueue(Command, Vec<AttachedFile>),
    SaveBlobPolicy(BlobPolicy),
    /// Keep the permission mode new sessions start in; `None` deletes it.
    SaveDefaultMode(Option<PermissionMode>),
    /// Keep whether new sessions start with the safety check on.
    SaveDefaultSafetyCheck(bool),
    /// Store the pairing's addresses as these, after a switch or a forget.
    SaveServerAddresses(ServerAddresses),
    /// Keep the session's draft in that mode; `None` deletes it.
    SaveDraft((SessionId, ComposerMode), Option<Draft>),
    /// Add the image to the session's agent draft, after the images it holds.
    AttachToDraft(SessionId, AttachedImage),
    /// Take the image with this hash off the session's agent draft.
    DetachFromDraft(SessionId, ContentHash),
    /// Add the file to the session's agent draft, after the files it holds.
    AttachFileToDraft(SessionId, AttachedFile),
    /// Take the file called this off the session's agent draft.
    DetachFileFromDraft(SessionId, String),
    SaveComposerMode(SessionId, ComposerMode),
    SaveReadMark(SessionId, ReadMark),
    SaveSnippet(Snippet),
    DeleteSnippet(SnippetId),
    /// Take the command out of the outbox and, if given, keep the draft, as
    /// one write: the text is never lost nor kept twice.
    Withdraw {
        id: CommandId,
        draft: Option<((SessionId, ComposerMode), Draft)>,
        /// The session's agent draft images after the withdrawal, in order,
        /// written with the rest; `None` leaves them as they are.
        images: Option<(SessionId, Vec<ContentHash>)>,
        /// The session's agent draft files after the withdrawal, in order,
        /// written with the rest; `None` leaves them as they are.
        files: Option<(SessionId, Vec<AttachedFile>)>,
    },
    /// Requeue a message refused for a file already in the workdir, replacing
    /// those files: each `(position, hash)` is the `replace` of that file
    /// ([`Store::requeue_replacing`](crate::Store::requeue_replacing)).
    Requeue {
        id: CommandId,
        replaces: Vec<(usize, ContentHash)>,
    },
    /// Read the session's events from the replica into the model.
    LoadSession(SessionId),
    /// Read a session's events from the replica to learn its waiting
    /// approvals (the Approvals screen).
    LoadApprovals(SessionId),
    /// Delete the session from this device's replica, then from the model
    /// ([`Model::session_deleted`]), and remove its blobs' files.
    DeleteLocally(SessionId),
    /// Clear the replica after the server's log lost events it holds
    /// ([`Store::reset_for_lost_history`](crate::Store::reset_for_lost_history)),
    /// then the model ([`Model::history_reset`]), and remove the blob files.
    ResetReplica,
    /// Hand to the engine once every persistence effect succeeded.
    Request(Request),
}

/// A session's title as the views show it.
struct Title {
    text: String,
    /// `text` is a name the user gave.
    named: bool,
    /// This device's latest rename, until its effect is in the replica.
    rename: Option<Outgoing>,
}

/// A session a search looks through.
pub(crate) struct SearchTarget {
    pub id: SessionId,
    pub title: String,
    /// `title` is a name the user gave, not the session's first prompt.
    pub named: bool,
    pub workdir: Option<String>,
    pub last_activity: DateTime<Utc>,
}

/// The screen shown, with what only that screen keeps: leaving it drops
/// that.
#[derive(Debug, Clone, PartialEq)]
enum Place {
    SessionList(ListPage),
    Archived,
    All,
    Settings,
    ServerAddresses,
    Pairing,
    Usage(Up),
    Storage(Up),
    Worktrees,
    Approvals,
    /// The usage limits page, opened from the session on screen, if any:
    /// back returns there.
    UsageLimits(Option<SessionId>),
    /// The search page; `scope` its one session, if any.
    Search {
        scope: Option<SessionId>,
    },
    Session {
        id: SessionId,
        /// Its events, once loaded from the replica.
        open: Option<OpenSession>,
        scope: Scope,
    },
    /// The session's info page; its events stay loaded, as the session's.
    SessionInfo {
        id: SessionId,
        open: Option<OpenSession>,
    },
    /// The session's workdir.
    Workdir {
        id: SessionId,
        pane: WorkdirPane,
        /// The last directory shown since [`Action::OpenWorkdir`], where
        /// the files pane opens.
        last_dir: String,
    },
    Crop(Box<CropPage>),
}

impl Place {
    /// The active session list, its prompt and picker closed.
    fn session_list() -> Place {
        Place::SessionList(ListPage::default())
    }

    /// Session `id`, its events not loaded yet, shown whole.
    fn session(id: SessionId) -> Place {
        Place::Session {
            id,
            open: None,
            scope: Scope::default(),
        }
    }
}

/// What the active session list keeps while it is on screen. Its request
/// ids are the model's ([`Model::dirs_req`], [`Model::conversations_req`]):
/// they must stay unused across visits, so an answer to a list since left
/// cannot match a request of the one opened after.
#[derive(Debug, Clone, Default, PartialEq)]
struct ListPage {
    /// Workdir completion in the new-session prompt.
    completer: Completer,
    /// The conversation picker.
    conversations: Conversations,
}

/// What of the open session's timeline shows. In memory only:
/// [`Action::Back`] reads where to step out of a subagent to from the log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Scope {
    /// The context [`Action::ViewContext`] narrowed the timeline to.
    context: Option<ContextId>,
    /// The `Agent` call whose subagent [`Action::ViewSubagent`] shows; it
    /// wins over `context`, which shows again once back steps out.
    subagent: Option<CallId>,
}

/// A screen a [`Model::opening`] can be asked from, and open its new
/// session over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpeningFrom {
    SessionList,
    Archived,
    All,
    Session(SessionId),
}

/// The crop page ([`Action::OpenCrop`]), kept while it is on screen.
#[derive(Debug, Clone, PartialEq)]
struct CropPage {
    blob: BlobId,
    session: SessionId,
    holds: BlobUse,
    title: String,
    /// Where back returns to.
    from: CropFrom,
    keep: Vec<LineSpan>,
    lines: LineCount,
}

/// Where the crop page was opened from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CropFrom {
    Session(SessionId),
    /// The storage page, with where back from it goes.
    Storage(Up),
}

/// Where back goes from a page opened from both the session list and
/// Settings (usage, storage): the screen it was opened from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Up {
    #[default]
    SessionList,
    Settings,
}

/// Where the count of the crop page's lines stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineCount {
    /// Not asked: the blob is not held yet.
    Unasked,
    /// [`Request::CountLines`] `req` is on its way.
    Asked(u64),
    Counted(u32),
    Failed(String),
}

impl From<Route> for Place {
    fn from(route: Route) -> Self {
        match route {
            Route::SessionList => Place::session_list(),
            Route::ArchivedSessions => Place::Archived,
            Route::AllSessions => Place::All,
            Route::Settings => Place::Settings,
            Route::ServerAddresses => Place::ServerAddresses,
            Route::Pairing => Place::Pairing,
            Route::Usage => Place::Usage(Up::default()),
            Route::Storage => Place::Storage(Up::default()),
            Route::Worktrees => Place::Worktrees,
            Route::Approvals => Place::Approvals,
            Route::UsageLimits => Place::UsageLimits(None),
        }
    }
}

/// The events of the session on screen, loaded from the replica.
#[derive(Debug, Clone, PartialEq)]
struct OpenSession {
    events: Vec<(Seq, Event)>,
    /// The first event after the read mark as it was when the session
    /// opened, past this device's own messages, if agent output follows
    /// it. Marking read leaves it alone.
    unread_from: Option<Seq>,
}

/// What the model holds of an approval waiting on this device, read from
/// its session's events ([`Model::approvals_loaded`]). The facts are kept
/// raw: what the approval's answers look like is live, and is read at view
/// time.
#[derive(Debug, Clone, PartialEq)]
enum Waiting {
    Approval(Box<ApprovalFacts>),
    Question,
    /// Pending in the replica, but its request is not in the session's
    /// events: cached so it is not read again.
    Unknown,
}

/// An approval's facts as its `ApprovalRequested` event logged them, with
/// the title of the subagent its call runs under.
#[derive(Debug, Clone, PartialEq)]
struct ApprovalFacts {
    call: Option<CallId>,
    from: Option<String>,
    tool: String,
    summary: String,
    input: Option<Full>,
    suggestions: Vec<RuleSuggestion>,
    shell: Option<String>,
}

/// What the model holds of the device's pairing, taken from the record
/// when it is loaded or saved, and the link to its server, which only a
/// paired device has.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Paired {
    pub(crate) device: DeviceId,
    /// The pinned server CA's fingerprint; `None` if its PEM holds no
    /// certificate.
    pub(crate) server: Option<Fingerprint>,
    pub(crate) addresses: ServerAddresses,
    pub(crate) link: PairedLink,
}

impl Paired {
    /// Freshly loaded or saved: the link starts by connecting.
    pub(crate) fn of(pairing: &Pairing) -> Paired {
        Paired {
            device: pairing.device_id(),
            server: tls::first_cert_der(pairing.ca_cert_pem()).map(|ca| Fingerprint::of_der(&ca)),
            addresses: pairing.addresses.clone(),
            link: PairedLink::Connecting,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    /// Who this device is paired as, where its server is and how the link
    /// to it is; `None` while unpaired.
    paired: Option<Paired>,
    pairing: PairingStatus,
    place: Place,
    /// The session list last shown, so back from a session returns to it:
    /// archiving or unarchiving the open session doesn't move the user to
    /// another list.
    last_list: ListKind,
    settings: Settings,
    cursor: Seq,
    /// Most recently written to (highest `last_message_seq`) first, like
    /// the store.
    sessions: Vec<SessionSummary>,
    /// Oldest first, finished entries included.
    outbox: Vec<OutboxEntry>,
    blobs: BTreeMap<BlobId, BlobRecord>,
    /// What the server did to blobs, as stored: an event's blob resolves
    /// through it ([`Model::full_view`]).
    fates: HashMap<BlobId, BlobFate>,
    /// Blobs asked for and not yet taken by a download; in memory only, as
    /// the blob queue forgets un-started requests on a restart too. A
    /// `Failed` blob asked for again shows as requested.
    wanted: HashSet<BlobId>,
    /// Blobs whose download the user stopped ([`Action::StopBlobDownload`]):
    /// nothing asks for them on its own, only a new ask by hand. In memory only.
    stopped: HashSet<BlobId>,
    /// The blob queue paused every download on a full device disk; the
    /// queue decides and reports it ([`Model::set_blobs_disk_full`]).
    blobs_disk_full: bool,
    /// This device's unsent messages and shell commands, as stored.
    drafts: HashMap<(SessionId, ComposerMode), Draft>,
    /// The images of each session's agent draft, in order, as stored.
    draft_images: HashMap<SessionId, Vec<AttachedImage>>,
    /// The non-image files of each session's agent draft, in order, as
    /// stored.
    draft_files: HashMap<SessionId, Vec<AttachedFile>>,
    /// Every known image's metadata, as stored: a queued message's
    /// attachments are looked up here when it is taken back into a draft.
    attachments: HashMap<ContentHash, AttachedImage>,
    /// What each session's composer writes where it is not the agent, as
    /// stored.
    composer_modes: HashMap<SessionId, ComposerMode>,
    /// Bumped whenever the model itself rewrites a draft, so heads know to
    /// replace the text they hold; never goes back, not even on reload.
    draft_rev: u64,
    /// How far this device has read each session, as stored.
    read_marks: HashMap<SessionId, ReadMark>,
    /// This device's saved prompts, as stored.
    snippets: Snippets,
    /// Shell commands from every session, newest first, as logged.
    shell_history: ShellHistory,
    /// Sessions deleted from this device, as stored: their events are
    /// skipped for good.
    deleted: HashSet<SessionId>,
    /// Pending approvals the user was alerted to, or that needed no alert
    /// (answered here), in this process or an earlier one: as stored, plus
    /// what [`Model::apply_batch`] and [`Model::set_link`] hand out to
    /// store.
    alerted: HashSet<ApprovalId>,
    /// How often new approvals came in ([`Attention::alerts`]).
    alerts: u64,
    /// What `alerts` was bumped to for each pending approval that alerted
    /// in this process ([`PendingApprovalRef::alerted_at`]); in memory only.
    alerted_at: HashMap<ApprovalId, u64>,
    /// How often a long shell run of this device's finished
    /// ([`Attention::shell_alerts`]), and the last one; in memory only.
    shell_alerts: u64,
    last_shell: Option<ShellAlert>,
    /// How often a session's agent went from working to idle
    /// ([`Attention::idle_alerts`]); in memory only.
    idle_alerts: u64,
    /// Sessions this device saw working and that have not settled yet; in
    /// memory only.
    seen_working: HashSet<SessionId>,
    /// The sessions that went idle, each with the `idle_alerts` count it
    /// rang at, until they work again ([`Attention::idle`]); in memory only.
    went_idle: HashMap<SessionId, u64>,
    /// Id of the latest [`Request::ListDirs`] for the session list's
    /// completion; never reused.
    dirs_req: u64,
    /// Id of the latest [`Request::ListConversations`] for the session
    /// list's picker; never reused.
    conversations_req: u64,
    /// The storage screen's report; in memory only.
    storage: Storage,
    /// The worktrees screen's list; in memory only.
    worktrees: Worktrees,
    /// The usage limits page: the stored answer, and the read in flight
    /// or failed (in memory only).
    usage_limits: UsageLimits,
    /// This device's login of the server's agent: the flow, its URL and
    /// the code. In memory only, never stored.
    login: AgentLogin,
    /// A [`Action::MoveContext`] whose new session is not in the replica
    /// yet: its command, and the screen it was asked from, still on screen.
    /// In memory only.
    opening: Option<(CommandId, OpeningFrom)>,
    /// The server's devices by name, as stored.
    devices: HashMap<DeviceId, String>,
    /// The models of the newest `ModelsOffered`, as stored.
    models: Vec<ModelChoice>,
    /// The safety rules as stored: every rule ever set, on or off, ordered
    /// by its newest event.
    safety_rules: Vec<SafetyRule>,
    /// Every turn's usage in the replica, in log order: what the usage
    /// screen totals.
    turns: Vec<TurnRecord>,
    /// What this device read of sessions' workdirs: as stored, plus the
    /// reads in flight and failed (in memory only).
    workdirs: Workdirs,
    /// Id of the latest [`Request::CountLines`]; never reused, so the
    /// count of a page since closed or reopened is dropped.
    count_req: u64,
    /// What the Approvals screen knows of each waiting approval, loaded
    /// from its session while that screen is on show
    /// ([`Model::approvals_to_load`]); in memory only.
    waiting_facts: HashMap<ApprovalId, Waiting>,
}

impl Model {
    fn device(&self) -> Option<DeviceId> {
        self.paired.as_ref().map(|p| p.device)
    }

    /// The link as heads see it: `Unpaired` exactly while the device is.
    fn link(&self) -> Link {
        match &self.paired {
            None => Link::Unpaired,
            Some(paired) => paired.link.clone().into(),
        }
    }

    /// The paired link, `None` while unpaired.
    fn paired_link(&self) -> Option<&PairedLink> {
        self.paired.as_ref().map(|p| &p.link)
    }

    /// Sets the paired link; no-op while unpaired.
    fn link_to(&mut self, link: PairedLink) {
        if let Some(paired) = &mut self.paired {
            paired.link = link;
        }
    }

    /// The model after a (cold) start: the replica as stored, `paired` from
    /// the pairing record if the device is paired.
    pub(crate) fn new(snapshot: Snapshot, paired: Option<Paired>) -> Model {
        let mut model = Model {
            paired,
            pairing: PairingStatus::Idle,
            place: Place::session_list(),
            last_list: ListKind::Active,
            settings: Settings::default(),
            cursor: Seq::ZERO,
            sessions: Vec::new(),
            outbox: Vec::new(),
            blobs: BTreeMap::new(),
            fates: HashMap::new(),
            wanted: HashSet::new(),
            stopped: HashSet::new(),
            blobs_disk_full: false,
            drafts: HashMap::new(),
            draft_images: HashMap::new(),
            draft_files: HashMap::new(),
            attachments: HashMap::new(),
            composer_modes: HashMap::new(),
            draft_rev: 0,
            read_marks: HashMap::new(),
            snippets: Snippets::default(),
            shell_history: ShellHistory::default(),
            deleted: HashSet::new(),
            alerted: HashSet::new(),
            alerts: 0,
            alerted_at: HashMap::new(),
            shell_alerts: 0,
            last_shell: None,
            idle_alerts: 0,
            seen_working: HashSet::new(),
            went_idle: HashMap::new(),
            dirs_req: 0,
            conversations_req: 0,
            storage: Storage::default(),
            worktrees: Worktrees::default(),
            usage_limits: UsageLimits::default(),
            login: AgentLogin::default(),
            opening: None,
            devices: HashMap::new(),
            models: Vec::new(),
            safety_rules: Vec::new(),
            turns: Vec::new(),
            workdirs: Workdirs::default(),
            count_req: 0,
            waiting_facts: HashMap::new(),
        };
        // An approval pending at start that no process alerted on yet (one
        // that came in during a catch-up cut short) alerts once online.
        model.reload(snapshot);
        // A turn busy at start alerts when it ends. Only here, not in
        // `reload`: a resync reloads mid-catch-up, and that work is not seen.
        model.seen_working = model
            .sessions
            .iter()
            .filter(|s| s.busy())
            .map(|s| s.id)
            .collect();
        model
    }

    /// Replaces the replica mirror with `snapshot`, keeping link, pairing,
    /// screen and alerts. The open session's events must be reloaded
    /// afterwards.
    pub(crate) fn reload(&mut self, snapshot: Snapshot) {
        self.settings = snapshot.settings;
        self.cursor = snapshot.cursor;
        self.sessions = snapshot.sessions;
        self.outbox = snapshot.outbox;
        self.blobs = snapshot.blobs.into_iter().map(|b| (b.id, b)).collect();
        self.fates = snapshot.fates;
        self.drafts = snapshot.drafts;
        self.draft_images = snapshot.draft_images;
        self.draft_files = snapshot.draft_files;
        self.attachments = snapshot.attachments;
        self.composer_modes = snapshot.composer_modes;
        self.read_marks = snapshot.read_marks;
        self.snippets = Snippets::new(snapshot.snippets);
        self.shell_history = ShellHistory::new(snapshot.shell_history);
        self.deleted = snapshot.deleted;
        self.devices = snapshot.devices;
        self.models = snapshot.models;
        self.safety_rules = snapshot.safety_rules;
        self.turns = snapshot.turns;
        self.workdirs.stored = snapshot.workdirs;
        self.usage_limits.stored = snapshot.usage_limits;
        if let Some((_, open)) = self.loaded_slot() {
            *open = None;
        }
        // Never forget what is still pending: an approval that came in
        // during a catch-up must still alert once it is over, and one
        // alerted on whose mark failed to store must not alert again.
        self.alerted.extend(snapshot.alerted);
        self.forget_resolved_alerts();
    }

    /// Drops what is kept about approvals no longer pending.
    fn forget_resolved_alerts(&mut self) {
        let pending: HashSet<ApprovalId> = self.pending_approvals().map(|(_, id)| id).collect();
        self.alerted.retain(|id| pending.contains(id));
        self.alerted_at.retain(|id, _| pending.contains(id));
    }

    /// The session on screen, whose events the model needs.
    pub(crate) fn open_session(&self) -> Option<SessionId> {
        match &self.place {
            Place::Session { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// The session whose events the screen shows: the open one, or the
    /// one whose info page is on screen.
    pub(crate) fn loaded_session(&self) -> Option<SessionId> {
        match &self.place {
            Place::Session { id, .. } | Place::SessionInfo { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// The session on screen when its events are not loaded: one a create
    /// or a move opened, before its [`Effect::LoadSession`] is served.
    pub(crate) fn unloaded_session(&self) -> Option<SessionId> {
        match &self.place {
            Place::Session { id, open: None, .. } | Place::SessionInfo { id, open: None } => {
                Some(*id)
            }
            _ => None,
        }
    }

    /// Where [`Model::loaded_session`]'s events go once loaded.
    fn loaded_slot(&mut self) -> Option<(SessionId, &mut Option<OpenSession>)> {
        match &mut self.place {
            Place::Session { id, open, .. } | Place::SessionInfo { id, open } => Some((*id, open)),
            _ => None,
        }
    }

    /// [`Model::loaded_session`] with its events, once loaded.
    fn shown_events(&self) -> Option<(SessionId, &OpenSession)> {
        match &self.place {
            Place::Session {
                id,
                open: Some(open),
                ..
            }
            | Place::SessionInfo {
                id,
                open: Some(open),
            } => Some((*id, open)),
            _ => None,
        }
    }

    /// Session `id`'s events, if the screen shows them and they are loaded.
    fn loaded(&self, id: SessionId) -> Option<&OpenSession> {
        self.shown_events()
            .filter(|(shown, _)| *shown == id)
            .map(|(_, open)| open)
    }

    /// Session `id`'s loaded events; none unless [`Model::loaded`].
    fn events(&self, id: SessionId) -> &[(Seq, Event)] {
        self.loaded(id).map_or(&[], |open| open.events.as_slice())
    }

    /// The screen on show as a [`Model::opening`] names it.
    fn opening_from(&self) -> Option<OpeningFrom> {
        match &self.place {
            Place::SessionList(_) => Some(OpeningFrom::SessionList),
            Place::Archived => Some(OpeningFrom::Archived),
            Place::All => Some(OpeningFrom::All),
            Place::Session { id, .. } => Some(OpeningFrom::Session(*id)),
            _ => None,
        }
    }

    // ---- actions ----------------------------------------------------------

    /// Folds a user action into the model. Actions that make no sense in the
    /// current state (empty text, unknown session, answered approval) are
    /// no-ops.
    pub(crate) fn dispatch(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Pair {
                address,
                code,
                fingerprint,
            } => self.pair(address, code, fingerprint),
            Action::SetServerAddress { address } => {
                let Ok(address) = ServerAddress::parse(&address) else {
                    return Vec::new();
                };
                let Some(paired) = &mut self.paired else {
                    return Vec::new();
                };
                if *paired.addresses.current() == address {
                    return Vec::new();
                }
                paired.addresses.switch_to(&address);
                paired.link = PairedLink::Connecting;
                vec![
                    Effect::SaveServerAddresses(paired.addresses.clone()),
                    Effect::Request(Request::Connect),
                ]
            }
            Action::ForgetServerAddress { address } => {
                let Ok(address) = ServerAddress::parse(&address) else {
                    return Vec::new();
                };
                let Some(paired) = &mut self.paired else {
                    return Vec::new();
                };
                if !paired.addresses.forget(&address) {
                    return Vec::new();
                }
                vec![Effect::SaveServerAddresses(paired.addresses.clone())]
            }
            Action::CreateSession {
                workdir,
                conversation,
                worktree,
            } => {
                let workdir = workdir.trim();
                if workdir.is_empty() {
                    return Vec::new();
                }
                if let Place::SessionList(page) = &mut self.place {
                    *page = ListPage::default();
                }
                // No local id check: dispatch has no failure channel, and a
                // silent drop would lose what the user typed. The server's
                // `BadConversation` shows as a failed pending session.
                let at = self.opening_from();
                let session_id = SessionId::new();
                let kind = self.create_command(session_id, workdir, conversation, worktree);
                let mut effects = self.enqueue(kind, Vec::new());
                // Open the new session now, as a creating one: it takes
                // messages before the server logs it.
                if at.is_some() && !effects.is_empty() {
                    self.place = Place::session(session_id);
                    effects.push(Effect::LoadSession(session_id));
                }
                effects
            }
            Action::CreateSessionLike(from) => {
                let opens_here = self.opening_from().is_some_and(|at| match at {
                    OpeningFrom::SessionList | OpeningFrom::Archived | OpeningFrom::All => true,
                    OpeningFrom::Session(id) => id == from,
                });
                if !opens_here {
                    return Vec::new();
                }
                let Some(workdir) = self.summary(from).and_then(|s| s.workdir.clone()) else {
                    return Vec::new();
                };
                let session_id = SessionId::new();
                let mut effects = self.enqueue(
                    CommandKind::CreateSession {
                        session_id,
                        workdir,
                        conversation: None,
                        worktree: None,
                        mode: self.new_session_mode(),
                        safety_check: self.settings.default_safety_check,
                    },
                    Vec::new(),
                );
                // As `CreateSession`: opens now, as a creating session.
                if !effects.is_empty() {
                    self.place = Place::session(session_id);
                    effects.push(Effect::LoadSession(session_id));
                }
                effects
            }
            Action::RecreateSession {
                session_id,
                workdir,
                conversation,
                worktree,
            } => {
                let workdir = workdir.trim();
                // Commands need a paired device, as `enqueue` needs one.
                if workdir.is_empty() || self.device().is_none() {
                    return Vec::new();
                }
                let refused = self.outbox.iter().any(|e| {
                    matches!(
                        &e.command.kind,
                        CommandKind::CreateSession { session_id: id, .. } if *id == session_id
                    ) && matches!(Outgoing::from(&e.status), Outgoing::Failed(_))
                        && e.discardable()
                });
                if !refused {
                    return Vec::new();
                }
                let kind = self.create_command(session_id, workdir, conversation, worktree);
                // The refused create and the messages queued for its session
                // leave the outbox. The messages come back behind the new
                // create, in their outbox order.
                let (gone, kept): (Vec<OutboxEntry>, Vec<OutboxEntry>) =
                    std::mem::take(&mut self.outbox).into_iter().partition(|e| {
                        match &e.command.kind {
                            CommandKind::CreateSession { session_id: id, .. } => *id == session_id,
                            CommandKind::SendMessage { session_id: id, .. } => {
                                *id == session_id && e.discardable()
                            }
                            _ => false,
                        }
                    });
                self.outbox = kept;
                let mut effects = Vec::new();
                let mut messages = Vec::new();
                for entry in gone {
                    effects.push(Effect::Withdraw {
                        id: entry.command.id,
                        draft: None,
                        images: None,
                        files: None,
                    });
                    if matches!(entry.command.kind, CommandKind::SendMessage { .. }) {
                        let files: Vec<AttachedFile> =
                            entry.files.into_iter().map(|held| held.file).collect();
                        messages.push((entry.command.kind, files));
                    }
                }
                let mut queued = self.enqueue(kind, Vec::new());
                for (message, files) in messages {
                    queued.extend(self.enqueue(message, files));
                }
                // Each enqueue ends with an outbox wake-up; one is enough.
                queued.retain(|effect| !matches!(effect, Effect::Request(Request::OutboxChanged)));
                effects.extend(queued);
                effects.push(Effect::Request(Request::OutboxChanged));
                effects
            }
            Action::CompleteWorkdir { input } => {
                if self.device().is_none() {
                    return Vec::new();
                }
                // While the link is down, recent workdirs stand in.
                let online = self.online();
                let Place::SessionList(page) = &mut self.place else {
                    return Vec::new();
                };
                page.completer
                    .complete(input, online, &mut self.dirs_req)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::EndCompletion => {
                if let Place::SessionList(page) = &mut self.place {
                    page.completer.end();
                }
                Vec::new()
            }
            Action::ListConversations => {
                if self.device().is_none() {
                    return Vec::new();
                }
                let online = self.online();
                let Place::SessionList(page) = &mut self.place else {
                    return Vec::new();
                };
                page.conversations
                    .list(online, &mut self.conversations_req)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::EndConversations => {
                if let Place::SessionList(page) = &mut self.place {
                    page.conversations.end();
                }
                Vec::new()
            }
            Action::RefreshStorage => {
                if self.device().is_none() || !matches!(self.place, Place::Storage(_)) {
                    return Vec::new();
                }
                let online = self.online();
                self.storage
                    .refresh(online)
                    .into_iter()
                    .map(Effect::Request)
                    .collect()
            }
            Action::ReclaimServerSpace => {
                if self.device().is_none() || !matches!(self.place, Place::Storage(_)) {
                    return Vec::new();
                }
                let online = self.online();
                self.storage
                    .reclaim(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::ReclaimDeviceSpace => {
                if !matches!(self.place, Place::Storage(_)) {
                    return Vec::new();
                }
                self.storage
                    .reclaim_device()
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::RefreshWorktrees => {
                if self.device().is_none() || self.place != Place::Worktrees {
                    return Vec::new();
                }
                let online = self.online();
                self.worktrees
                    .refresh(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::RefreshUsageLimits => {
                if self.device().is_none()
                    || !matches!(
                        self.place,
                        Place::UsageLimits(_) | Place::SessionInfo { .. }
                    )
                {
                    return Vec::new();
                }
                let online = self.online();
                self.usage_limits
                    .refresh(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::StartAgentLogin => {
                if self.device().is_none() || self.place != Place::Settings {
                    return Vec::new();
                }
                let online = self.online();
                self.login
                    .start(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::FinishAgentLogin { code } => {
                if self.device().is_none() || self.place != Place::Settings {
                    return Vec::new();
                }
                let online = self.online();
                self.login
                    .finish(code, online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::RetryAgentLogin => {
                if self.device().is_none() || self.place != Place::Settings {
                    return Vec::new();
                }
                let online = self.online();
                self.login
                    .retry(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::DismissAgentLogin => {
                self.login.forget();
                Vec::new()
            }
            Action::SendMessage { session_id, text } => {
                let Some(summary) = self.session_or_creating(session_id) else {
                    return Vec::new();
                };
                // The draft's images and files are the message's attachments:
                // core owns them, so the head sends only the text.
                let images = self.draft_images_of(session_id);
                let files = self.draft_files_of(session_id).to_vec();
                if text.trim().is_empty() && images.is_empty() && files.is_empty() {
                    return Vec::new();
                }
                if !within_attachment_caps(images) {
                    return Vec::new();
                }
                let attachments = images.iter().map(|image| image.sha256).collect();
                let text = with_file_lines(&text, &files);
                // The message goes where this device's own clear or switch
                // on its way leads, so it is not refused for that.
                let context = self.target_context(&summary);
                let effects = self.enqueue(
                    CommandKind::SendMessage {
                        session_id,
                        context,
                        text,
                        attachments,
                    },
                    files,
                );
                // The store drops the draft, its images and its files with
                // the enqueue.
                if !effects.is_empty() {
                    self.drafts.remove(&(session_id, ComposerMode::Agent));
                    self.draft_images.remove(&session_id);
                    self.draft_files.remove(&session_id);
                }
                effects
            }
            Action::RunShell {
                session_id,
                command,
                when,
            } => {
                // Over the size limit is the server's to refuse: a refused
                // run can be taken back with its text, a dropped one not.
                if self.summary(session_id).is_none() || command.trim().is_empty() {
                    return Vec::new();
                }
                let effects = self.enqueue(
                    CommandKind::RunShell {
                        session_id,
                        command,
                        when,
                    },
                    Vec::new(),
                );
                // The store drops the shell draft with the enqueue.
                if !effects.is_empty() {
                    self.drafts.remove(&(session_id, ComposerMode::Shell));
                }
                effects
            }
            Action::Screen { session_id, action } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                let covered = matches!(
                    &action,
                    ScreenAction::Capture { device, full }
                        if self.capture_waiting(session_id, device, *full)
                );
                if covered {
                    return Vec::new();
                }
                self.enqueue(CommandKind::Screen { session_id, action }, Vec::new())
            }
            Action::StopShell { session_id, run } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::StopShell { session_id, run }, Vec::new())
            }
            Action::SetComposerMode { session_id, mode } => {
                if self.summary(session_id).is_none() || self.composer_mode(session_id) == mode {
                    return Vec::new();
                }
                match mode {
                    ComposerMode::Agent => self.composer_modes.remove(&session_id),
                    ComposerMode::Shell => self.composer_modes.insert(session_id, mode),
                };
                vec![Effect::SaveComposerMode(session_id, mode)]
            }
            Action::SaveDraft {
                session_id,
                mode,
                text,
                cursor,
                when,
            } => self.save_draft((session_id, mode), text, cursor, when),
            Action::Attach { session_id, image } => {
                if self.session_or_creating(session_id).is_none() {
                    return Vec::new();
                }
                let mut images = self.draft_images_of(session_id).to_vec();
                if images.iter().any(|held| held.sha256 == image.sha256) {
                    return Vec::new();
                }
                images.push(image.clone());
                if !within_attachment_caps(&images) {
                    return Vec::new();
                }
                self.attachments.insert(image.sha256, image.clone());
                self.set_draft_images(session_id, images);
                vec![
                    Effect::AttachToDraft(session_id, image),
                    Effect::Request(Request::DraftImagesChanged),
                ]
            }
            Action::Detach { session_id, sha256 } => {
                let mut images = self.draft_images_of(session_id).to_vec();
                let held = images.len();
                images.retain(|image| image.sha256 != sha256);
                if images.len() == held {
                    return Vec::new();
                }
                self.set_draft_images(session_id, images);
                vec![Effect::DetachFromDraft(session_id, sha256)]
            }
            Action::AttachFile { session_id, file } => {
                if self.session_or_creating(session_id).is_none() || !plain_file_name(&file.name) {
                    return Vec::new();
                }
                let mut files = self.draft_files_of(session_id).to_vec();
                if files.iter().any(|held| held.name == file.name) {
                    return Vec::new();
                }
                files.push(file.clone());
                self.set_draft_files(session_id, files);
                vec![
                    Effect::AttachFileToDraft(session_id, file),
                    Effect::Request(Request::DraftImagesChanged),
                ]
            }
            Action::DetachFile { session_id, name } => {
                let mut files = self.draft_files_of(session_id).to_vec();
                let held = files.len();
                files.retain(|file| file.name != name);
                if files.len() == held {
                    return Vec::new();
                }
                self.set_draft_files(session_id, files);
                vec![Effect::DetachFileFromDraft(session_id, name)]
            }
            Action::SaveSnippet { name, text, kind } => self
                .snippets
                .save(&name, text, kind)
                .map(Effect::SaveSnippet)
                .into_iter()
                .collect(),
            Action::UseSnippet(id) => self
                .snippets
                .used(id)
                .map(Effect::SaveSnippet)
                .into_iter()
                .collect(),
            Action::EditSnippet { id, name, text } => match self.snippets.edit(id, &name, text) {
                Some((edited, gone)) => gone
                    .map(Effect::DeleteSnippet)
                    .into_iter()
                    .chain([Effect::SaveSnippet(edited)])
                    .collect(),
                None => Vec::new(),
            },
            Action::DeleteSnippet(id) => {
                if self.snippets.delete(id) {
                    vec![Effect::DeleteSnippet(id)]
                } else {
                    Vec::new()
                }
            }
            Action::Discard(id) => self.withdraw(id, false),
            Action::EditQueued(id) => self.withdraw(id, true),
            Action::ResendReplacing(id) => self.resend_replacing(id),
            Action::ResolveApproval { id, decision, rule } => {
                let granted = |rule| decision == Decision::Approve && self.offers(id, rule);
                // A question takes answers; only a denial declines it.
                let asks = decision == Decision::Approve && self.asked(id).is_some();
                if !self.can_answer(id) || asks || rule.is_some_and(|rule| !granted(rule)) {
                    return Vec::new();
                }
                self.enqueue(
                    CommandKind::ResolveApproval { id, decision, rule },
                    Vec::new(),
                )
            }
            Action::AnswerQuestion { id, answers } => {
                let fits = self
                    .asked(id)
                    .is_some_and(|questions| answers_fit(questions, &answers));
                if !self.can_answer(id) || !fits {
                    return Vec::new();
                }
                self.enqueue(CommandKind::AnswerQuestion { id, answers }, Vec::new())
            }
            Action::Interrupt { session_id } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::Interrupt { session_id }, Vec::new())
            }
            Action::StopSession { session_id } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::StopSession { session_id }, Vec::new())
            }
            Action::ClearContext { session_id } => self.clear_context(session_id),
            Action::SwitchContext {
                session_id,
                context,
            } => {
                if self.summary(session_id).is_none() || self.moved(session_id, context) {
                    return Vec::new();
                }
                self.enqueue(
                    CommandKind::SwitchContext {
                        session_id,
                        context,
                    },
                    Vec::new(),
                )
            }
            Action::MoveContext {
                session_id,
                context,
            } => self.move_context(session_id, context),
            Action::ViewContext {
                session_id,
                context,
            } => {
                if self.open_session() == Some(session_id)
                    && let Some(scope) = self.scope_mut()
                {
                    scope.context = context;
                }
                Vec::new()
            }
            Action::ViewSubagent { session_id, call } => {
                let known = |call| self.subagent_header(session_id, call).is_some();
                if self.open_session() != Some(session_id) || call.is_some_and(|c| !known(c)) {
                    return Vec::new();
                }
                if let Some(scope) = self.scope_mut() {
                    scope.subagent = call;
                }
                Vec::new()
            }
            Action::RenameSession { session_id, name } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                // As the server will log it, so the title shows it unchanged.
                let name = skywave_proto::normalise_session_name(&name);
                self.enqueue(CommandKind::RenameSession { session_id, name }, Vec::new())
            }
            Action::SetSessionArchived {
                session_id,
                archived,
            } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(
                    CommandKind::SetSessionArchived {
                        session_id,
                        archived,
                    },
                    Vec::new(),
                )
            }
            Action::RemoveWorktree { session_id } => {
                let has_worktree = self
                    .summary(session_id)
                    .is_some_and(|s| s.worktree.is_some());
                if !has_worktree {
                    return Vec::new();
                }
                self.enqueue(CommandKind::RemoveWorktree { session_id }, Vec::new())
            }
            Action::SetPermissionMode { session_id, mode } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(
                    CommandKind::SetPermissionMode { session_id, mode },
                    Vec::new(),
                )
            }
            Action::SetSafetyCheck { session_id, on } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetSafetyCheck { session_id, on }, Vec::new())
            }
            Action::SetSafetyRule {
                session_id,
                words,
                on,
            } => {
                // No session guard, unlike the other session actions: a
                // removal goes to the session that logged the rule, which may
                // be deleted here while the server still keeps it.
                if words.is_empty() || words.iter().any(String::is_empty) {
                    return Vec::new();
                }
                self.enqueue(
                    CommandKind::SetSafetyRule {
                        session_id,
                        words,
                        on,
                    },
                    Vec::new(),
                )
            }
            Action::SafetyCheckFeedback {
                session_id,
                call,
                judgment,
                note,
            } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(
                    CommandKind::SafetyCheckFeedback {
                        session_id,
                        call,
                        judgment,
                        note,
                    },
                    Vec::new(),
                )
            }
            Action::StopTask { session_id, task } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::StopTask { session_id, task }, Vec::new())
            }
            Action::SetModel { session_id, model } => {
                // As the server will log it, so the switch shows it unchanged.
                let Some(model) = skywave_proto::normalise_model(&model) else {
                    return Vec::new();
                };
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetModel { session_id, model }, Vec::new())
            }
            Action::SetEffort { session_id, effort } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetEffort { session_id, effort }, Vec::new())
            }
            Action::DeleteSessionLocally { session_id } => {
                if !self.summary(session_id).is_some_and(|s| self.deletable(s)) {
                    return Vec::new();
                }
                vec![Effect::DeleteLocally(session_id)]
            }
            Action::DeleteSessionEverywhere { session_id } => {
                if !self
                    .summary(session_id)
                    .is_some_and(|s| self.deletable_everywhere(s))
                {
                    return Vec::new();
                }
                self.enqueue(CommandKind::DeleteSession { session_id }, Vec::new())
            }
            Action::OpenSession(id) => {
                if self.device().is_none() || self.session_or_creating(id).is_none() {
                    return Vec::new();
                }
                self.place = Place::session(id);
                vec![Effect::LoadSession(id)]
            }
            Action::OpenSearch(scope) => {
                if self.device().is_none() || scope.is_some_and(|id| self.summary(id).is_none()) {
                    return Vec::new();
                }
                self.place = Place::Search { scope };
                Vec::new()
            }
            Action::OpenNextApproval => match self.next_approval() {
                Some(id) => self.dispatch(Action::OpenSession(id)),
                None => Vec::new(),
            },
            Action::MarkRead {
                session_id,
                through,
            } => self.mark_read(session_id, through),
            Action::OpenWorkdir { session_id } => {
                if self.device().is_none() || self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.place = Place::Workdir {
                    id: session_id,
                    pane: WorkdirPane::Changes,
                    last_dir: String::new(),
                };
                self.show_workdir(WorkdirPane::Changes)
            }
            Action::OpenSessionInfo { session_id } => {
                if self.device().is_none() || self.summary(session_id).is_none() {
                    return Vec::new();
                }
                // The session's events, if loaded, are the page's too.
                let open = match self.loaded_slot() {
                    Some((id, open)) if id == session_id => open.take(),
                    _ => None,
                };
                let load = open.is_none().then_some(Effect::LoadSession(session_id));
                self.place = Place::SessionInfo {
                    id: session_id,
                    open,
                };
                let online = self.online();
                let asked = self.usage_limits.open(online).map(Effect::Request);
                load.into_iter().chain(asked).collect()
            }
            Action::ShowWorkdir(pane) => match self.shown_workdir() {
                Some(_) => self.show_workdir(pane),
                None => Vec::new(),
            },
            Action::RefreshWorkdir => {
                let Some((id, pane)) = self.shown_workdir() else {
                    return Vec::new();
                };
                let ask = pane_ask(id, pane);
                let online = self.online();
                self.workdirs
                    .ask(ask, online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::DownloadWorkdirFile { session_id, path } => {
                if self.device().is_none() || self.summary(session_id).is_none() {
                    return Vec::new();
                }
                let ask = WorkdirAsk::Download {
                    session: session_id,
                    path,
                };
                if self.workdirs.fetch(&ask) == WorkdirFetch::Loading {
                    return Vec::new();
                }
                let online = self.online();
                self.workdirs
                    .ask(ask, online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::CancelWorkdirDownload { session_id, path } => {
                let ask = WorkdirAsk::Download {
                    session: session_id,
                    path,
                };
                if self.workdirs.cancel(&ask) {
                    return Vec::new();
                }
                match self.workdirs.stored.get(&ask) {
                    Some(WorkdirAnswer::File(FileResult::File {
                        content: FileBody::Blob(blob),
                        ..
                    })) => {
                        let id = blob.id;
                        self.stop_blob(id)
                    }
                    _ => Vec::new(),
                }
            }
            Action::PushWorkdirFile {
                session_id,
                source,
                path,
                replace,
            } => {
                if self.device().is_none() || self.summary(session_id).is_none() {
                    return Vec::new();
                }
                let online = self.online();
                self.workdirs
                    .push(session_id, source, path, replace, online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::CancelWorkdirPush { session_id, path } => self
                .workdirs
                .pushing_req(session_id, &path)
                .map(|req| vec![Effect::Request(Request::CancelPush { req })])
                .unwrap_or_default(),
            Action::DownloadBlob(id) => match self.blobs.get(&id).map(|b| &b.status) {
                None | Some(BlobStatus::Available { .. }) => Vec::new(),
                Some(status) => {
                    self.stopped.remove(&id);
                    // A download already running shows its progress instead.
                    if matches!(
                        status,
                        BlobStatus::NotDownloaded
                            | BlobStatus::Stopped { .. }
                            | BlobStatus::Failed { .. }
                    ) {
                        self.wanted.insert(id);
                    }
                    vec![Effect::Request(Request::DownloadBlob(id))]
                }
            },
            Action::StopBlobDownload(id) => self.stop_blob(id),
            // The server decides whether the blob may go (its refusals
            // show as the view's `pending`): this replica may not know of
            // a delete or crop another device sent.
            Action::DeleteBlob(blob) => self.enqueue(CommandKind::DeleteBlob { blob }, Vec::new()),
            // Sent from its own crop page, the page goes where back goes:
            // left open, it would turn to `Gone` once the crop lands.
            Action::CropBlob { blob, keep } => {
                let from_page = matches!(&self.place, Place::Crop(page) if page.blob == blob);
                let mut effects = self.enqueue(CommandKind::CropBlob { blob, keep }, Vec::new());
                if from_page
                    && !effects.is_empty()
                    && let Some(back) = self.back()
                {
                    effects.extend(self.dispatch(back));
                }
                effects
            }
            Action::OpenCrop {
                blob,
                session,
                holds,
                title,
            } => {
                if self.device().is_none() {
                    return Vec::new();
                }
                let from = match &self.place {
                    Place::Storage(up) => CropFrom::Storage(*up),
                    Place::Session { id, .. } => CropFrom::Session(*id),
                    // Reopened over itself, it keeps where back goes.
                    Place::Crop(page) => page.from,
                    _ => CropFrom::Session(session),
                };
                self.place = Place::Crop(Box::new(CropPage {
                    blob,
                    session,
                    holds,
                    title,
                    from,
                    keep: Vec::new(),
                    lines: LineCount::Unasked,
                }));
                // Its count, once held, is asked by `count_wanted`.
                Vec::new()
            }
            Action::SetCropKeep(keep) => {
                if let Place::Crop(page) = &mut self.place {
                    page.keep = keep;
                }
                Vec::new()
            }
            Action::SetBlobPolicy(policy) => {
                if self.settings.blob_policy == policy {
                    return Vec::new();
                }
                self.settings.blob_policy = policy;
                vec![
                    Effect::SaveBlobPolicy(policy),
                    Effect::Request(Request::BlobPolicyChanged(policy)),
                ]
            }
            Action::SetDefaultMode(mode) => {
                if self.settings.default_mode == mode {
                    return Vec::new();
                }
                self.settings.default_mode = mode;
                vec![Effect::SaveDefaultMode(mode)]
            }
            Action::SetDefaultSafetyCheck(on) => {
                if self.settings.default_safety_check == on {
                    return Vec::new();
                }
                self.settings.default_safety_check = on;
                vec![Effect::SaveDefaultSafetyCheck(on)]
            }
            Action::RetryNow => match self.link() {
                Link::Reconnecting { .. } => vec![Effect::Request(Request::RetryNow)],
                // The server may have been updated (or its certificate
                // restored) since: ask it again.
                Link::Rejected(rejection) if rejection.can_retry() => {
                    self.link_to(PairedLink::Connecting);
                    vec![Effect::Request(Request::Connect)]
                }
                _ => Vec::new(),
            },
            // The link shows `Connecting` only once the reset reloaded the
            // model ([`Model::history_reset`]): if it fails, the `Connect`
            // after it is never sent and the link stays rejected.
            Action::ResetReplica => match self.link() {
                Link::Rejected(LinkRejection::HistoryLost { .. }) => {
                    self.place = Place::session_list();
                    self.last_list = ListKind::Active;
                    vec![Effect::ResetReplica, Effect::Request(Request::Connect)]
                }
                _ => Vec::new(),
            },
            Action::Navigate(route) => {
                let from = self.open_session();
                // Back from the crop page keeps where back from storage goes.
                let up = match &self.place {
                    Place::Settings => Up::Settings,
                    Place::Usage(up) | Place::Storage(up) => *up,
                    Place::Crop(page) => match page.from {
                        CropFrom::Storage(up) => up,
                        CropFrom::Session(_) => Up::SessionList,
                    },
                    _ => Up::SessionList,
                };
                let place = match route {
                    Route::UsageLimits => Place::UsageLimits(from),
                    Route::Usage => Place::Usage(up),
                    Route::Storage => Place::Storage(up),
                    route => route.into(),
                };
                self.place = place;
                match self.place {
                    Place::SessionList(_) => self.last_list = ListKind::Active,
                    Place::Archived => self.last_list = ListKind::Archived,
                    Place::All => self.last_list = ListKind::All,
                    _ => {}
                }
                if self.device().is_none() {
                    return Vec::new();
                }
                let online = self.online();
                let asked: Vec<Request> = match self.place {
                    Place::Storage(_) => self.storage.open(online),
                    Place::Worktrees => self.worktrees.open(online).into_iter().collect(),
                    Place::UsageLimits(_) => self.usage_limits.open(online).into_iter().collect(),
                    _ => Vec::new(),
                };
                let loads = self
                    .approvals_to_load()
                    .into_iter()
                    .map(Effect::LoadApprovals);
                asked
                    .into_iter()
                    .map(Effect::Request)
                    .chain(loads)
                    .collect()
            }
            Action::Back => {
                // A subagent shown steps out to the one it runs under, then
                // to the main timeline.
                if let Some((session_id, call)) = self.shown_subagent() {
                    let outer = self
                        .subagent_header(session_id, call)
                        .and_then(|header| header.trail.last().map(|(outer, _)| *outer));
                    if let Some(scope) = self.scope_mut() {
                        scope.subagent = outer;
                    }
                    return Vec::new();
                }
                match self.back() {
                    Some(action) => self.dispatch(action),
                    None => Vec::new(),
                }
            }
        }
    }

    /// What of the open session's timeline shows, while one is on screen.
    fn scope_mut(&mut self) -> Option<&mut Scope> {
        match &mut self.place {
            Place::Session { scope, .. } => Some(scope),
            _ => None,
        }
    }

    /// The subagent [`Action::ViewSubagent`] shows, if its session is on
    /// screen.
    fn shown_subagent(&self) -> Option<(SessionId, CallId)> {
        match &self.place {
            Place::Session {
                id,
                scope:
                    Scope {
                        subagent: Some(call),
                        ..
                    },
                ..
            } if self.device().is_some() && self.summary(*id).is_some() => Some((*id, *call)),
            _ => None,
        }
    }

    /// The subagent `Agent` call `call` of the open session `id` runs, as
    /// the timeline folds it; `None` for any other call.
    fn subagent_header(&self, id: SessionId, call: CallId) -> Option<SubagentHeader> {
        let open = self.loaded(id)?;
        let (items, _) = self.timeline(id, &open.events);
        subagent_header(&items, call)
    }

    /// Where [`Action::Back`] goes from the screen shown, as the action
    /// that goes there; `None` where back leaves the app. Follows
    /// [`Model::screen`]: an unpaired device sees pairing, a session gone
    /// from the replica shows the active list. Back from a session returns
    /// to the list last shown (`last_list`), whether or not the session was
    /// archived meanwhile.
    fn back(&self) -> Option<Action> {
        self.device()?;
        let route = match &self.place {
            Place::SessionList(_) => return None,
            Place::Archived
            | Place::All
            | Place::Settings
            | Place::Pairing
            | Place::Usage(Up::SessionList)
            | Place::Storage(Up::SessionList)
            | Place::Worktrees
            | Place::Approvals => Route::SessionList,
            Place::ServerAddresses | Place::Usage(Up::Settings) | Place::Storage(Up::Settings) => {
                Route::Settings
            }
            Place::UsageLimits(from) => {
                return Some(match from.filter(|id| self.summary(*id).is_some()) {
                    Some(id) => Action::OpenSession(id),
                    None => Action::Navigate(Route::SessionList),
                });
            }
            Place::Search { scope } => {
                return Some(match scope.filter(|id| self.summary(*id).is_some()) {
                    Some(id) => Action::OpenSession(id),
                    None => Action::Navigate(Route::SessionList),
                });
            }
            Place::SessionInfo { id, .. } => {
                self.summary(*id)?;
                return Some(Action::OpenSession(*id));
            }
            Place::Session { id, .. } => {
                self.session_or_creating(*id)?;
                match self.last_list {
                    ListKind::Active => Route::SessionList,
                    ListKind::Archived => Route::ArchivedSessions,
                    ListKind::All => Route::AllSessions,
                }
            }
            Place::Workdir { id, pane, .. } => {
                self.summary(*id)?;
                let up = |path: &str| {
                    Action::ShowWorkdir(WorkdirPane::Dir {
                        path: parent(path).to_owned(),
                    })
                };
                return Some(match pane {
                    WorkdirPane::File { path } => up(path),
                    WorkdirPane::Dir { path } if !path.is_empty() => up(path),
                    WorkdirPane::Diff { .. } => Action::ShowWorkdir(WorkdirPane::Changes),
                    WorkdirPane::Changes | WorkdirPane::Commits | WorkdirPane::Dir { .. } => {
                        Action::OpenSession(*id)
                    }
                });
            }
            Place::Crop(page) => {
                return Some(match page.from {
                    CropFrom::Session(id) if self.summary(id).is_some() => Action::OpenSession(id),
                    CropFrom::Session(_) => Action::Navigate(Route::SessionList),
                    // Asked again: the crop may have changed it.
                    CropFrom::Storage(_) => Action::Navigate(Route::Storage),
                });
            }
        };
        Some(Action::Navigate(route))
    }

    /// Whether the link may answer a request now, as far as the model
    /// knows: not while it waits to reconnect or the server refused it.
    fn online(&self) -> bool {
        !matches!(
            self.paired_link(),
            Some(PairedLink::Reconnecting { .. } | PairedLink::Rejected(_))
        )
    }

    /// The session whose workdir is on screen, and the pane shown.
    fn shown_workdir(&self) -> Option<(SessionId, &WorkdirPane)> {
        match &self.place {
            Place::Workdir { id, pane, .. }
                if self.device().is_some() && self.summary(*id).is_some() =>
            {
                Some((*id, pane))
            }
            _ => None,
        }
    }

    /// Shows `pane` of the workdir on screen: asks for its read if it has
    /// no stored answer and none on its way, and for its content if that is
    /// a blob not downloaded.
    fn show_workdir(&mut self, pane: WorkdirPane) -> Vec<Effect> {
        let Place::Workdir {
            id,
            pane: shown,
            last_dir,
        } = &mut self.place
        else {
            return Vec::new();
        };
        if let WorkdirPane::Dir { path } = &pane {
            last_dir.clone_from(path);
        }
        let ask = pane_ask(*id, &pane);
        *shown = pane;
        let mut requests = Vec::new();
        if self.workdirs.unread(&ask) {
            let online = self.online();
            requests.extend(self.workdirs.ask(ask, online));
        }
        requests.extend(self.want_shown_blob());
        requests.into_iter().map(Effect::Request).collect()
    }

    /// The blob the workdir pane on screen shows: a diff's or a file's.
    fn shown_blob(&self) -> Option<&BlobRef> {
        let (id, pane) = self.shown_workdir()?;
        match (pane, self.workdirs.stored.get(&pane_ask(id, pane))?) {
            (WorkdirPane::Diff { path, untracked }, WorkdirAnswer::Changes(changes)) => {
                match &changed_file(changes, path, *untracked)?.diff {
                    DiffBody::Blob(blob) => Some(blob),
                    DiffBody::Binary | DiffBody::TooLarge { .. } | DiffBody::Omitted => None,
                }
            }
            (
                WorkdirPane::File { .. },
                WorkdirAnswer::File(FileResult::File {
                    content: FileBody::Blob(blob),
                    ..
                }),
            ) => Some(blob),
            _ => None,
        }
    }

    /// Asks for the blob the workdir pane on screen shows, once: if it is
    /// not downloaded, not asked for yet, and did not fail (asking again
    /// after a failure is [`Action::DownloadBlob`]'s).
    fn want_shown_blob(&mut self) -> Option<Request> {
        let id = self.shown_blob()?.id;
        self.want_blob(id)
    }

    /// Asks for blob `id`, once: if it is not downloaded and not asked for
    /// yet. Whether it is on screen does not matter here.
    fn want_blob(&mut self, id: BlobId) -> Option<Request> {
        let record = self.blobs.get(&id)?;
        let not_here = matches!(
            record.status,
            BlobStatus::NotDownloaded | BlobStatus::Stopped { .. }
        );
        if !not_here || self.wanted.contains(&id) || self.stopped.contains(&id) {
            return None;
        }
        self.wanted.insert(id);
        Some(Request::DownloadBlob(id))
    }

    /// Stops `id`'s download if it is asked for or under way
    /// ([`Action::StopBlobDownload`]).
    fn stop_blob(&mut self, id: BlobId) -> Vec<Effect> {
        let Some(record) = self.blobs.get(&id) else {
            return Vec::new();
        };
        let under_way = matches!(record.status, BlobStatus::Downloading { .. });
        if !self.wanted.remove(&id) && !under_way {
            return Vec::new();
        }
        self.stopped.insert(id);
        vec![Effect::Request(Request::StopBlob(id))]
    }

    fn pair(&mut self, address: String, code: String, fingerprint: String) -> Vec<Effect> {
        if matches!(self.pairing, PairingStatus::InProgress { .. }) {
            return Vec::new();
        }
        self.place = Place::Pairing;
        let (code, fingerprint) = (code.trim().to_owned(), fingerprint.trim().to_owned());
        let address = address.trim();
        if address.is_empty() || code.is_empty() || fingerprint.is_empty() {
            self.pairing = PairingStatus::Failed {
                address: address.to_owned(),
                reason: "address, code and fingerprint are all required".into(),
            };
            return Vec::new();
        }
        let address = match ServerAddress::parse(address) {
            Ok(address) => address,
            Err(error) => {
                self.pairing = PairingStatus::Failed {
                    address: address.to_owned(),
                    reason: error.to_string(),
                };
                return Vec::new();
            }
        };
        self.pairing = PairingStatus::InProgress {
            address: address.clone(),
        };
        vec![Effect::Request(Request::Pair {
            address,
            code,
            fingerprint,
        })]
    }

    /// The create command of session `session_id` in `workdir` (trimmed by
    /// the caller). A blank `conversation` or `worktree` is none. The mode is
    /// the device's default, but none with an adopted conversation: the
    /// server refuses a mode with one.
    fn create_command(
        &self,
        session_id: SessionId,
        workdir: &str,
        conversation: Option<String>,
        worktree: Option<String>,
    ) -> CommandKind {
        let conversation = conversation
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        let worktree = worktree
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .map(str::to_owned);
        let mode = match conversation {
            Some(_) => None,
            None => self.new_session_mode(),
        };
        CommandKind::CreateSession {
            session_id,
            workdir: workdir.to_owned(),
            conversation,
            worktree,
            mode,
            safety_check: self.settings.default_safety_check,
        }
    }

    /// Queues a command under a fresh UUIDv7 id, carrying `files`, which the
    /// sender writes into the workdir root before posting. Commands need a
    /// paired device: without one there is nobody to send them to.
    fn enqueue(&mut self, kind: CommandKind, files: Vec<AttachedFile>) -> Vec<Effect> {
        if self.device().is_none() {
            return Vec::new();
        }
        let command = Command {
            id: CommandId::new(),
            kind,
        };
        self.outbox.push(OutboxEntry {
            command: command.clone(),
            status: OutboxStatus::Queued,
            tries: 0,
            first_sent_at: None,
            answered_at: None,
            files: files
                .iter()
                .map(|file| OutboxFile {
                    file: file.clone(),
                    existing: None,
                })
                .collect(),
        });
        vec![
            Effect::Enqueue(command, files),
            Effect::Request(Request::OutboxChanged),
        ]
    }

    /// Queues a clear of the session's context: a fresh id, and a name no
    /// context of it has as far as this device knows.
    fn clear_context(&mut self, session_id: SessionId) -> Vec<Effect> {
        if self.summary(session_id).is_none() {
            return Vec::new();
        }
        let used = self.context_names(session_id);
        let name = fresh_context_name(&used, skywave_proto::context_name::random);
        self.enqueue(
            CommandKind::ClearContext {
                session_id,
                context: ContextId::new(),
                name,
            },
            Vec::new(),
        )
    }

    /// Queues the move of a context the open session lists as movable,
    /// under its own name, behind a clear when messages go to it now: the
    /// server never moves the current context. The new session opens once
    /// the move is in the replica ([`Model::apply_batch`]).
    fn move_context(&mut self, session_id: SessionId, context: ContextId) -> Vec<Effect> {
        let Some(summary) = self.summary(session_id) else {
            return Vec::new();
        };
        let target = self.target_context(summary);
        let events = self.events(session_id);
        let Some(name) = self
            .contexts(session_id, events, target)
            .into_iter()
            .find(|c| c.id == context && c.can_move)
            .map(|c| c.name)
        else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        if context == target {
            effects = self.clear_context(session_id);
            // One wake-up, after the move.
            effects.retain(|effect| matches!(effect, Effect::Enqueue(..)));
        }
        let moved = self.enqueue(
            CommandKind::MoveContext {
                session_id,
                context,
                name,
            },
            Vec::new(),
        );
        if let Some(Effect::Enqueue(command, _)) = moved.first() {
            self.opening = Some((command.id, OpeningFrom::Session(session_id)));
        }
        effects.extend(moved);
        effects
    }

    /// Whether the open session's log has `context` moved away.
    fn moved(&self, session_id: SessionId, context: ContextId) -> bool {
        self.events(session_id)
            .iter()
            .any(|(_, e)| e.context == context && matches!(e.kind, EventKind::ContextMoved { .. }))
    }

    /// Whether an unsettled outbox entry already captures `device` (a full
    /// capture, for `full`), so a new capture of it would only repeat it.
    fn capture_waiting(&self, session_id: SessionId, device: &str, full: bool) -> bool {
        self.outbox.iter().any(|e| {
            !e.status.is_settled()
                && matches!(
                    &e.command.kind,
                    CommandKind::Screen {
                        session_id: queued,
                        action: ScreenAction::Capture { device: waiting, full: waiting_full },
                    } if *queued == session_id && waiting == device && (*waiting_full || !full)
                )
        })
    }

    /// What the session's composer writes on this device.
    fn composer_mode(&self, session_id: SessionId) -> ComposerMode {
        self.composer_modes
            .get(&session_id)
            .copied()
            .unwrap_or_default()
    }

    /// Keeps the session's draft in a mode; blank text drops it, unless a
    /// shell draft keeps a condition picked before typing. An agent draft
    /// runs on nothing: its `when` is always `Always`. Nothing to write
    /// when the draft is unchanged or the session unknown.
    fn save_draft(
        &mut self,
        key: (SessionId, ComposerMode),
        text: String,
        cursor: usize,
        when: RunWhen,
    ) -> Vec<Effect> {
        if self.session_or_creating(key.0).is_none() {
            return Vec::new();
        }
        let when = match key.1 {
            ComposerMode::Agent => RunWhen::Always,
            ComposerMode::Shell => when,
        };
        let draft = (!text.trim().is_empty() || when != RunWhen::Always).then(|| Draft {
            cursor: cursor.min(text.chars().count()),
            text,
            when,
        });
        if self.drafts.get(&key) == draft.as_ref() {
            return Vec::new();
        }
        match &draft {
            Some(draft) => self.drafts.insert(key, draft.clone()),
            None => self.drafts.remove(&key),
        };
        vec![Effect::SaveDraft(key, draft)]
    }

    /// Marks the open session read through `through`, clamped to its newest
    /// event; the notable count at that point comes from its loaded events.
    /// Nothing to write for another session or a mark that would not move
    /// forward.
    fn mark_read(&mut self, session_id: SessionId, through: Seq) -> Vec<Effect> {
        let (Some(open), Some(summary)) = (self.loaded(session_id), self.summary(session_id))
        else {
            return Vec::new();
        };
        let through = through.min(summary.last_seq);
        let read = self
            .read_marks
            .get(&session_id)
            .map_or(Seq::ZERO, |m| m.seq);
        if through <= read {
            return Vec::new();
        }
        let notable = open
            .events
            .iter()
            .take_while(|(seq, _)| *seq <= through)
            .filter(|(_, event)| is_notable(&event.kind))
            .count() as u64;
        let mark = ReadMark {
            seq: through,
            notable,
        };
        self.read_marks.insert(session_id, mark);
        vec![Effect::SaveReadMark(session_id, mark)]
    }

    /// Takes back a [`discardable`](OutboxEntry::discardable) command; with
    /// `edit`, a message goes back into its session's agent draft, ahead of
    /// what the draft held, the cursor after the message; a shell run
    /// likewise into the shell draft, which takes the run's `when`. A
    /// command answered meanwhile is no longer discardable here: nothing
    /// happens. The sender hears of it, so it stops waiting to resend this
    /// one and moves on. Dismissing a failed switch also takes the earlier
    /// failed ones of its kind with it. Taking back a create also takes back
    /// the discardable messages queued for its session and its drafts, and
    /// leaves its page.
    fn withdraw(&mut self, id: CommandId, edit: bool) -> Vec<Effect> {
        let Some(index) = self
            .outbox
            .iter()
            .position(|e| e.command.id == id && e.discardable())
        else {
            return Vec::new();
        };
        // A message is set off from the draft by a blank line, a shell
        // command by a line break: the next line is the next command. The
        // file lines a message added to its text are not the user's, so
        // they come off again.
        let files: Vec<AttachedFile> = self.outbox[index]
            .files
            .iter()
            .map(|held| held.file.clone())
            .collect();
        let (key, text, when, separator, attachments) = match &self.outbox[index].command.kind {
            CommandKind::SendMessage {
                session_id,
                text,
                attachments,
                ..
            } if edit => (
                (*session_id, ComposerMode::Agent),
                without_file_lines(text, &files),
                RunWhen::Always,
                "\n\n",
                attachments.as_slice(),
            ),
            CommandKind::RunShell {
                session_id,
                command,
                when,
            } if edit => (
                (*session_id, ComposerMode::Shell),
                command.clone(),
                *when,
                "\n",
                &[][..],
            ),
            _ if edit => return Vec::new(),
            _ => {
                let created = match &self.outbox[index].command.kind {
                    CommandKind::CreateSession { session_id, .. } => Some(*session_id),
                    _ => None,
                };
                let kind = &self.outbox[index].command.kind;
                let mut ids = Vec::new();
                if matches!(
                    kind,
                    CommandKind::ClearContext { .. }
                        | CommandKind::SwitchContext { .. }
                        | CommandKind::SetPermissionMode { .. }
                        | CommandKind::SetModel { .. }
                        | CommandKind::SetEffort { .. }
                ) {
                    // The timeline shows only the newest command of a kind, so
                    // an earlier failed one would take the dismissed row's place.
                    let session = command_session(kind);
                    ids.extend(
                        self.outbox[..index]
                            .iter()
                            .filter(|e| {
                                std::mem::discriminant(&e.command.kind)
                                    == std::mem::discriminant(kind)
                                    && command_session(&e.command.kind) == session
                                    && e.discardable()
                            })
                            .map(|e| e.command.id),
                    );
                }
                // The dismissed one goes last: a partial failure then leaves
                // the newest row, not an older one.
                ids.push(id);
                self.outbox.retain(|e| !ids.contains(&e.command.id));
                let mut effects: Vec<Effect> = ids
                    .into_iter()
                    .map(|withdrawn| {
                        // A create's withdrawal also empties its session's
                        // draft images and files in the same write.
                        let emptied = created.filter(|_| withdrawn == id);
                        Effect::Withdraw {
                            id: withdrawn,
                            draft: None,
                            images: emptied.map(|session| (session, Vec::new())),
                            files: emptied.map(|session| (session, Vec::new())),
                        }
                    })
                    .collect();
                // The session a refused create named never exists, so the
                // messages queued for it go too, and its page with them.
                if let Some(session_id) = created {
                    let messages: Vec<CommandId> = self
                        .outbox
                        .iter()
                        .filter(|e| {
                            e.discardable()
                                && matches!(
                                    &e.command.kind,
                                    CommandKind::SendMessage { session_id: queued, .. }
                                        if *queued == session_id
                                )
                        })
                        .map(|e| e.command.id)
                        .collect();
                    for message in messages {
                        self.outbox.retain(|e| e.command.id != message);
                        effects.push(Effect::Withdraw {
                            id: message,
                            draft: None,
                            images: None,
                            files: None,
                        });
                    }
                    // Its drafts go with it, in memory and in the store.
                    let drafts: Vec<(SessionId, ComposerMode)> = self
                        .drafts
                        .keys()
                        .filter(|(draft_session, _)| *draft_session == session_id)
                        .copied()
                        .collect();
                    for key in drafts {
                        effects.push(Effect::SaveDraft(key, None));
                    }
                    self.drafts
                        .retain(|(draft_session, _), _| *draft_session != session_id);
                    self.draft_images.remove(&session_id);
                    self.draft_files.remove(&session_id);
                    if self.loaded_session() == Some(session_id) {
                        self.place = Place::SessionList(ListPage::default());
                    }
                }
                effects.push(Effect::Request(Request::OutboxChanged));
                return effects;
            }
        };
        // A blank message adds no lines in front of the draft's text.
        let old = self.drafts.get(&key).map_or("", |old| old.text.as_str());
        let joined = match (text.trim().is_empty(), old.trim().is_empty()) {
            (true, _) => old.to_owned(),
            (false, true) => text.clone(),
            (false, false) => format!("{text}{separator}{old}"),
        };
        let draft = Draft {
            cursor: text.chars().count(),
            text: joined,
            when,
        };
        let restored = (!attachments.is_empty()).then(|| self.restored_images(key.0, attachments));
        let restored_files = (!files.is_empty()).then(|| self.restored_files(key.0, &files));
        self.outbox.remove(index);
        self.drafts.insert(key, draft.clone());
        self.draft_rev += 1;
        let images = restored.map(|images| {
            let hashes: Vec<ContentHash> = images.iter().map(|image| image.sha256).collect();
            self.set_draft_images(key.0, images);
            (key.0, hashes)
        });
        let files = restored_files.map(|files| {
            self.set_draft_files(key.0, files.clone());
            (key.0, files)
        });
        let restored_any = images.is_some() || files.is_some();
        let mut effects = vec![
            Effect::Withdraw {
                id,
                draft: Some((key, draft)),
                images,
                files,
            },
            Effect::Request(Request::OutboxChanged),
        ];
        if restored_any {
            effects.push(Effect::Request(Request::DraftImagesChanged));
        }
        effects
    }

    /// Requeues the message `id` for the files its refusal found in the
    /// workdir: each such file's `replace` becomes the hash found there, and
    /// the message goes out next, keeping its place. Its `tries` and
    /// `first_sent_at` stay. A no-op unless the entry is a message refused
    /// with [`AttachmentRefusal::FileExists`] that found a file.
    fn resend_replacing(&mut self, id: CommandId) -> Vec<Effect> {
        let Some(entry) = self.outbox_entry(id) else {
            return Vec::new();
        };
        let refused =
            entry.status == OutboxStatus::AttachmentRefused(AttachmentRefusal::FileExists);
        let is_message = matches!(entry.command.kind, CommandKind::SendMessage { .. });
        if !refused || !is_message || entry.files.iter().all(|held| held.existing.is_none()) {
            return Vec::new();
        }
        let mut replaces = Vec::new();
        for (position, held) in entry.files.iter_mut().enumerate() {
            if let Some(existing) = held.existing.take() {
                held.file.replace = Some(existing);
                replaces.push((position, existing));
            }
        }
        entry.status = OutboxStatus::Queued;
        entry.answered_at = None;
        vec![
            Effect::Requeue { id, replaces },
            Effect::Request(Request::OutboxChanged),
        ]
    }

    /// The images an edited message puts back into `session_id`'s agent
    /// draft: its own, in order, then the draft's that it does not hold. A
    /// message's image with no metadata known here is skipped.
    fn restored_images(
        &self,
        session_id: SessionId,
        attachments: &[ContentHash],
    ) -> Vec<AttachedImage> {
        let own: Vec<AttachedImage> = attachments
            .iter()
            .filter_map(|sha256| self.attachments.get(sha256).cloned())
            .collect();
        let kept: Vec<AttachedImage> = self
            .draft_images_of(session_id)
            .iter()
            .filter(|held| !own.iter().any(|image| image.sha256 == held.sha256))
            .cloned()
            .collect();
        own.into_iter().chain(kept).collect()
    }

    /// The files an edited message puts back into `session_id`'s agent
    /// draft: its own, in order, then the draft's that it does not hold by
    /// name.
    fn restored_files(&self, session_id: SessionId, own: &[AttachedFile]) -> Vec<AttachedFile> {
        let kept: Vec<AttachedFile> = self
            .draft_files_of(session_id)
            .iter()
            .filter(|held| !own.iter().any(|file| file.name == held.name))
            .cloned()
            .collect();
        own.iter().cloned().chain(kept).collect()
    }

    /// The images the session's agent draft holds, in order.
    fn draft_images_of(&self, session_id: SessionId) -> &[AttachedImage] {
        self.draft_images
            .get(&session_id)
            .map_or(&[][..], Vec::as_slice)
    }

    /// Sets the session's agent draft images; none removes the entry.
    fn set_draft_images(&mut self, session_id: SessionId, images: Vec<AttachedImage>) {
        if images.is_empty() {
            self.draft_images.remove(&session_id);
        } else {
            self.draft_images.insert(session_id, images);
        }
    }

    /// The non-image files the session's agent draft holds, in order.
    fn draft_files_of(&self, session_id: SessionId) -> &[AttachedFile] {
        self.draft_files
            .get(&session_id)
            .map_or(&[][..], Vec::as_slice)
    }

    /// Sets the session's agent draft files; none removes the entry.
    fn set_draft_files(&mut self, session_id: SessionId, files: Vec<AttachedFile>) {
        if files.is_empty() {
            self.draft_files.remove(&session_id);
        } else {
            self.draft_files.insert(session_id, files);
        }
    }

    /// Pending in the replica and not already answered from this device
    /// (an answer refused for itself may be retried, one refused because the
    /// approval is closed on the server may not).
    fn can_answer(&self, id: ApprovalId) -> bool {
        let pending = self
            .sessions
            .iter()
            .any(|s| s.pending_approvals.contains(&id));
        let answered = self
            .local_answer(id, &[], pending)
            .is_some_and(|answer| !answer.retryable);
        pending && !answered
    }

    /// Whether approval `id` offers a rule at index `rule`: requested in the
    /// open session, or cached for the Approvals screen, which has no session
    /// open.
    fn offers(&self, id: ApprovalId, rule: u8) -> bool {
        let in_open_session = self.shown_events().is_some_and(|(_, open)| {
            open.events.iter().any(|(_, event)| {
                matches!(&event.kind, EventKind::ApprovalRequested { id: requested, suggestions, .. }
                    if *requested == id && usize::from(rule) < suggestions.len())
            })
        });
        let cached = matches!(
            self.waiting_facts.get(&id),
            Some(Waiting::Approval(facts)) if usize::from(rule) < facts.suggestions.len()
        );
        in_open_session || cached
    }

    /// The questions of question `id`, asked in the open session; `None`
    /// for a plain approval or one asked elsewhere.
    fn asked(&self, id: ApprovalId) -> Option<&[Question]> {
        self.shown_events()?
            .1
            .events
            .iter()
            .find_map(|(_, event)| match &event.kind {
                EventKind::QuestionAsked {
                    id: asked,
                    questions,
                    ..
                } if *asked == id => Some(questions.as_slice()),
                _ => None,
            })
    }

    /// The session of the approval [`Action::OpenNextApproval`] goes to.
    fn next_approval(&self) -> Option<SessionId> {
        let approvals = self.waiting_approvals();
        let open = self.open_session();
        let after = approvals
            .iter()
            .find(|(session, _)| Some(*session) == open)
            .map(|(_, id)| *id);
        let mut elsewhere = approvals
            .iter()
            .filter(|(session, _)| Some(*session) != open);
        let next = match after {
            Some(after) => elsewhere
                .clone()
                .find(|(_, id)| *id > after)
                .or_else(|| elsewhere.next()),
            None => elsewhere.next(),
        };
        next.map(|(session, _)| *session)
    }

    /// Every approval pending in the replica, with its session.
    fn pending_approvals(&self) -> impl Iterator<Item = (&SessionSummary, ApprovalId)> {
        self.sessions
            .iter()
            .flat_map(|s| s.pending_approvals.iter().map(move |id| (s, *id)))
    }

    /// The pending approvals this device can still answer, oldest first.
    fn waiting_approvals(&self) -> Vec<(SessionId, ApprovalId)> {
        let mut approvals: Vec<_> = self
            .pending_approvals()
            .filter(|(_, id)| self.can_answer(*id))
            .map(|(s, id)| (s.id, id))
            .collect();
        approvals.sort_by_key(|(_, id)| *id);
        approvals
    }

    /// The sessions whose events the Approvals screen must read: those with
    /// a pending approval not cached yet, each once, in the order of the
    /// pending approvals' ids, oldest first. Pending rather than waiting, so
    /// an approval this device answered, then refused for a retry, has its
    /// facts when it waits again. None unless that screen is on show.
    pub(crate) fn approvals_to_load(&self) -> Vec<SessionId> {
        if self.place != Place::Approvals {
            return Vec::new();
        }
        let mut pending: Vec<(SessionId, ApprovalId)> =
            self.pending_approvals().map(|(s, id)| (s.id, id)).collect();
        pending.sort_by_key(|(_, id)| *id);
        let mut sessions = Vec::new();
        for (session, id) in pending {
            if !self.waiting_facts.contains_key(&id) && !sessions.contains(&session) {
                sessions.push(session);
            }
        }
        sessions
    }

    /// Bumps `alerts` once if approvals came in that nobody was alerted to
    /// yet, and forgets those no longer pending. Only online: while catching
    /// up the replica shows the past, and an approval requested and resolved
    /// during that is no news; one still pending at its end alerts then.
    ///
    /// Returns the approvals it marked alerted, to store, so a later
    /// process does not alert for them again.
    fn note_new_approvals(&mut self) -> Vec<(SessionId, ApprovalId)> {
        if !matches!(self.paired_link(), Some(PairedLink::Online { .. })) {
            return Vec::new();
        }
        self.forget_resolved_alerts();
        let marked: Vec<(SessionId, ApprovalId)> = self
            .pending_approvals()
            .map(|(s, id)| (s.id, id))
            .filter(|(_, id)| !self.alerted.contains(id))
            .collect();
        self.alerted.extend(marked.iter().map(|(_, id)| *id));
        // One answered here was seen; it is never news.
        let news: Vec<ApprovalId> = marked
            .iter()
            .map(|(_, id)| *id)
            .filter(|id| self.local_answer(*id, &[], true).is_none())
            .collect();
        if !news.is_empty() {
            self.alerts += 1;
            let at = self.alerts;
            self.alerted_at.extend(news.into_iter().map(|id| (id, at)));
        }
        marked
    }

    // ---- engine notifications -------------------------------------------------

    /// Folds a batch the store has accepted. Mirrors
    /// [`Store::apply_batch`](crate::Store::apply_batch): already-seen events
    /// are skipped and a batch that would leave a gap changes nothing.
    ///
    /// Returns the approvals newly marked alerted, which the caller stores
    /// ([`Store::mark_alerted`](crate::Store::mark_alerted)).
    pub(crate) fn apply_batch(&mut self, batch: &EventBatch) -> Vec<(SessionId, ApprovalId)> {
        if batch.from == Seq::ZERO || batch.from > self.cursor.next() {
            return Vec::new();
        }
        let cursor = self.cursor;
        for (seq, event) in batch.iter().filter(|(seq, _)| *seq > cursor) {
            self.cursor = seq;
            // Offered for every session, as the store keeps them.
            if let EventKind::ModelsOffered { models } = &event.kind {
                self.models.clone_from(models);
            }
            // Kept whatever session said so, as the store keeps them: the
            // newest rule for these words moves to the end.
            if let EventKind::SafetyRuleSet { words, on, .. } = &event.kind {
                self.safety_rules.retain(|rule| rule.words != *words);
                self.safety_rules.push(SafetyRule {
                    words: words.clone(),
                    on: *on,
                    session_id: event.session_id,
                });
            }
            if self.deleted.contains(&event.session_id) {
                continue;
            }
            // As the store does: a tombstone ends its session for the rest of
            // the batch. The seam drops the session after this fold.
            if matches!(event.kind, EventKind::SessionDeleted { .. }) {
                self.deleted.insert(event.session_id);
            }
            if let EventKind::ShellRequested { command, .. } = &event.kind {
                self.shell_history.record(command);
            }
            match self.sessions.iter_mut().find(|s| s.id == event.session_id) {
                Some(summary) => summary.apply(seq, event),
                None => self.sessions.push(SessionSummary::new(seq, event)),
            }
            // Working only as seen online: a catch-up's past is no news.
            if matches!(self.paired_link(), Some(PairedLink::Online { .. }))
                && self
                    .summary(event.session_id)
                    .is_some_and(SessionSummary::busy)
            {
                self.seen_working.insert(event.session_id);
            }
            self.note_shell_finish(event);
            self.turns.extend(TurnRecord::of(seq, event));
            // As the store decides it (`Store::apply_batch`): before a
            // crop's `into` registers, the old blob goes unless a crop
            // finds it held; a blob with a fate never registers again.
            for (blob, fate) in BlobFate::all(&event.kind) {
                let held = matches!(fate, BlobFate::Cropped { .. }) && self.blob_held(blob);
                self.fates.entry(blob).or_insert(fate);
                if !held {
                    self.purge_blob(blob);
                }
            }
            for blob in blob_refs(&event.kind) {
                if self.fates.contains_key(&blob.id) {
                    continue;
                }
                self.blobs.entry(blob.id).or_insert_with(|| BlobRecord {
                    id: blob.id,
                    size: blob.size,
                    sha256: blob.sha256,
                    status: BlobStatus::NotDownloaded,
                    workdir: false,
                });
            }
            if let Some((id, Some(open))) = self.loaded_slot()
                && id == event.session_id
            {
                open.events.push((seq, event.clone()));
            }
            if let Some((command, from)) = self.opening
                && event.command_id == Some(command)
            {
                self.opening = None;
                // A move names its new session from the source's log; that
                // session's `SessionCreated` came first, in this batch or
                // an earlier one.
                let to = match event.kind {
                    EventKind::ContextMoved { to, .. } => to,
                    _ => event.session_id,
                };
                // Its events come from the store: see `unloaded_session`.
                if self.opening_from() == Some(from) {
                    self.place = Place::session(to);
                }
            }
        }
        self.sessions
            .sort_by_key(|a| std::cmp::Reverse(a.last_message_seq));
        self.note_went_idle();
        self.note_new_approvals()
    }

    /// Settles the sessions this device saw working, once the batch is
    /// folded: one no longer busy (or gone) leaves `seen_working`, and one
    /// settled now (idle, not stopped) rings `idle_alerts`. Deciding per
    /// batch means a turn ended and stopped in one batch does not alert.
    /// A session deleted (tombstoned, in this batch or before) is never
    /// settled: its summary is dropped by the seam after the fold, so it
    /// must not ring with no entry to show. Then `went_idle` forgets the
    /// sessions busy again or deleted or gone. Sorted, so the same batch
    /// always gives the same counts.
    fn note_went_idle(&mut self) {
        let mut ended: Vec<SessionId> = self
            .seen_working
            .iter()
            .copied()
            .filter(|id| {
                self.deleted.contains(id) || !self.summary(*id).is_some_and(SessionSummary::busy)
            })
            .collect();
        ended.sort();
        for id in ended {
            self.seen_working.remove(&id);
            let settled = !self.deleted.contains(&id)
                && self
                    .summary(id)
                    .is_some_and(|s| matches!(s.state, LogState::Idle) && !s.stopped);
            if settled {
                self.idle_alerts += 1;
                self.went_idle.insert(id, self.idle_alerts);
            }
        }
        let back: Vec<SessionId> = self
            .went_idle
            .keys()
            .copied()
            .filter(|id| {
                self.deleted.contains(id) || self.summary(*id).is_none_or(SessionSummary::busy)
            })
            .collect();
        for id in back {
            self.went_idle.remove(&id);
        }
    }

    /// Bumps `shell_alerts` for `event` if it finishes a run of this
    /// device's that ran at least [`SHELL_ALERT_AFTER`] (both ends the
    /// server's clock), unless its session is on screen in shell mode. All
    /// of it is read from the `ShellFinished` itself, so it holds for a
    /// session not loaded and a run requested in an earlier batch.
    fn note_shell_finish(&mut self, event: &Event) {
        let EventKind::ShellFinished {
            by,
            started_at: Some(started_at),
            command_line,
            end,
            ..
        } = &event.kind
        else {
            return;
        };
        let session_id = event.session_id;
        let ran = (event.at - *started_at).to_std().unwrap_or_default();
        let watched = self.open_session() == Some(session_id)
            && self.composer_mode(session_id) == ComposerMode::Shell;
        if self.device() != Some(*by) || ran < SHELL_ALERT_AFTER || watched {
            return;
        }
        let Some(title) = self.summary(session_id).map(|s| self.title(s).text) else {
            return;
        };
        self.shell_alerts += 1;
        self.last_shell = Some(ShellAlert {
            session_id,
            title,
            command_line: command_line.clone(),
            end: end.clone(),
        });
    }

    pub(crate) fn session_loaded(&mut self, id: SessionId, events: Vec<(Seq, Event)>) {
        if self.loaded_session() != Some(id) {
            return;
        }
        let read = self.read_marks.get(&id).map_or(Seq::ZERO, |m| m.seq);
        let mut unseen = events.iter().filter(|(seq, _)| *seq > read);
        let first = self.first_unseen(unseen.clone());
        let unread_from = first.filter(|_| unseen.any(|(_, e)| is_notable(&e.kind)));
        if let Some((_, open)) = self.loaded_slot() {
            *open = Some(OpenSession {
                events,
                unread_from,
            });
        }
    }

    /// Caches what the Approvals screen shows of session `id`'s pending
    /// approvals, from its events: each one's facts, a question, or
    /// `Unknown` when its request is not among them (cached, so it is not
    /// read again). Then forgets the cached approvals pending nowhere.
    pub(crate) fn approvals_loaded(&mut self, id: SessionId, events: Vec<(Seq, Event)>) {
        let ids: Vec<ApprovalId> = self
            .summary(id)
            .map(|summary| summary.pending_approvals.clone())
            .unwrap_or_default();
        let (timeline, _) = self.timeline(id, &events);
        for approval in ids {
            let waiting = events
                .iter()
                .find_map(|(_, event)| match &event.kind {
                    EventKind::ApprovalRequested {
                        id: requested,
                        call,
                        tool,
                        summary,
                        input,
                        suggestions,
                        shell,
                    } if *requested == approval => {
                        Some(Waiting::Approval(Box::new(ApprovalFacts {
                            call: *call,
                            from: call
                                .and_then(|call| tool_item(&timeline, call)?.0)
                                .and_then(|outer| subagent_item(&timeline, outer))
                                .map(|(_, view)| view.title.clone()),
                            tool: tool.clone(),
                            summary: summary.clone(),
                            input: input.clone(),
                            suggestions: suggestions.clone(),
                            shell: shell.clone(),
                        })))
                    }
                    EventKind::QuestionAsked { id: asked, .. } if *asked == approval => {
                        Some(Waiting::Question)
                    }
                    _ => None,
                })
                .unwrap_or(Waiting::Unknown);
            self.waiting_facts.insert(approval, waiting);
        }
        let pending: HashSet<ApprovalId> = self.pending_approvals().map(|(_, id)| id).collect();
        self.waiting_facts.retain(|id, _| pending.contains(id));
    }

    /// Where the divider goes in `unseen`: its first event, past any
    /// messages this device sent before the first news or other device's
    /// message. Whoever sent a message here saw what came before it.
    fn first_unseen<'a>(&self, unseen: impl Iterator<Item = &'a (Seq, Event)>) -> Option<Seq> {
        let mut first = None;
        for (seq, event) in unseen {
            match &event.kind {
                EventKind::UserMessage { device_id, .. } => {
                    let sent = event
                        .command_id
                        .is_some_and(|id| self.outbox.iter().any(|e| e.command.id == id));
                    if self.is_own_message(sent, *device_id) {
                        first = None;
                        continue;
                    }
                    return first.or(Some(*seq));
                }
                // Imported history is never news, and a read mark cannot
                // pass it (it is not activity, so `last_seq` stays before
                // it). An adopted session's creation and its history are
                // one unit: the divider goes after its `ImportFinished`,
                // never at or inside it.
                EventKind::Imported { .. } => {}
                EventKind::ImportFinished { .. } => first = None,
                kind => {
                    let at = *first.get_or_insert(*seq);
                    if is_notable(kind) {
                        return Some(at);
                    }
                }
            }
        }
        first
    }

    /// The session `id` as the views name it: untitled once deleted here
    /// or before it is pulled.
    fn session_ref(&self, id: SessionId) -> SessionRef {
        SessionRef {
            id,
            title: self
                .summary(id)
                .filter(|_| !self.deleted.contains(&id))
                .map(|summary| self.title(summary).text),
        }
    }

    /// The device `id` as the views name it.
    fn device_ref(&self, id: DeviceId) -> DeviceRef {
        DeviceRef {
            id,
            name: self.devices.get(&id).cloned(),
            me: self.device() == Some(id),
        }
    }

    /// The server listed its devices: keep their names (already stored).
    pub(crate) fn devices_named(&mut self, devices: &[DeviceInfo]) {
        for device in devices {
            self.devices.insert(device.id, device.name.clone());
        }
    }

    /// What the server's `hello` said of itself (already stored).
    pub(crate) fn server_facts(&mut self, facts: ServerFacts) {
        self.settings.server = facts;
    }

    /// Devices the events name that this device knows no name for.
    pub(crate) fn unnamed_devices(&self, batch: &EventBatch) -> HashSet<DeviceId> {
        batch
            .events
            .iter()
            .filter_map(|event| event.kind.device())
            .filter(|id| !self.devices.contains_key(id))
            .collect()
    }

    /// A logged user message is this device's own if it is still in the
    /// outbox (`sent`) or carries this device's id.
    fn is_own_message(&self, sent: bool, device_id: DeviceId) -> bool {
        sent || self.device() == Some(device_id)
    }

    /// Mirrors [`Store::delete_session_locally`](crate::Store::delete_session_locally),
    /// which returned `blobs`: the session goes for good, with what this
    /// device kept for it. If it was on screen, the list last shown shows
    /// instead, as back would return to.
    pub(crate) fn session_deleted(&mut self, id: SessionId, blobs: &[BlobId]) {
        self.deleted.insert(id);
        self.sessions.retain(|s| s.id != id);
        self.turns.retain(|t| t.session_id != id);
        self.drafts.retain(|(session, _), _| *session != id);
        self.draft_images.remove(&id);
        self.draft_files.remove(&id);
        self.composer_modes.remove(&id);
        self.read_marks.remove(&id);
        // Unanswered ones too, as the store drops them.
        self.outbox
            .retain(|e| command_session(&e.command.kind) != Some(id));
        for blob in blobs {
            self.forget_blob(*blob);
            self.fates.remove(blob);
        }
        self.workdirs.forget(id);
        self.forget_resolved_alerts();
        if matches!(&self.place,
            Place::Session { id: shown, .. }
            | Place::SessionInfo { id: shown, .. }
            | Place::Workdir { id: shown, .. } if *shown == id)
        {
            self.place = match self.last_list {
                ListKind::Active => Place::session_list(),
                ListKind::Archived => Place::Archived,
                ListKind::All => Place::All,
            };
        }
    }

    /// Mirrors [`Store::claim_send`](crate::Store::claim_send).
    pub(crate) fn command_sending(&mut self, id: CommandId, now: DateTime<Utc>) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| !e.status.is_settled()) {
            entry.status = OutboxStatus::Sending;
            entry.tries += 1;
            entry.first_sent_at.get_or_insert(now);
        }
    }

    /// The first result is final, as in the store.
    ///
    /// A delete or crop of a blob this settles as accepted changed what the
    /// server stores, so an open storage screen asks for its report again;
    /// a `RemoveWorktree` settled as accepted changes the worktree list, so
    /// an open worktrees screen asks for it again. That holds whether the
    /// reply or the log settled it.
    pub(crate) fn command_result(
        &mut self,
        id: CommandId,
        result: CommandResult,
        now: DateTime<Utc>,
    ) -> Vec<Request> {
        let Some(entry) = self.outbox_entry(id).filter(|e| !e.status.is_settled()) else {
            return Vec::new();
        };
        let accepted = matches!(result, CommandResult::Accepted { .. });
        let frees = accepted
            && matches!(
                entry.command.kind,
                CommandKind::DeleteBlob { .. } | CommandKind::CropBlob { .. }
            );
        let removes = accepted && matches!(entry.command.kind, CommandKind::RemoveWorktree { .. });
        entry.status = OutboxStatus::Done(result);
        entry.answered_at = Some(now);
        let online = self.online();
        let paired = self.device().is_some();
        match self.place {
            Place::Storage(_) if frees && paired => self.storage.refresh(online),
            Place::Worktrees if removes && paired => {
                self.worktrees.refresh(online).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Mirrors [`Store::record_undeliverable`](crate::Store::record_undeliverable).
    pub(crate) fn command_undeliverable(&mut self, id: CommandId, now: DateTime<Utc>) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| !e.status.is_settled()) {
            entry.status = OutboxStatus::Undeliverable;
            entry.answered_at = Some(now);
        }
    }

    /// Mirrors [`Store::record_attachment_refused`](crate::Store::record_attachment_refused).
    pub(crate) fn command_attachment_refused(
        &mut self,
        id: CommandId,
        reason: AttachmentRefusal,
        now: DateTime<Utc>,
    ) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| !e.status.is_settled()) {
            entry.status = OutboxStatus::AttachmentRefused(reason);
            entry.answered_at = Some(now);
        }
    }

    /// Mirrors [`Store::record_file_exists`](crate::Store::record_file_exists).
    pub(crate) fn command_file_exists(
        &mut self,
        id: CommandId,
        position: usize,
        existing: ContentHash,
        now: DateTime<Utc>,
    ) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| !e.status.is_settled()) {
            entry.status = OutboxStatus::AttachmentRefused(AttachmentRefusal::FileExists);
            entry.answered_at = Some(now);
            if let Some(file) = entry.files.get_mut(position) {
                file.existing = Some(existing);
            }
        }
    }

    /// Returns the approvals newly marked alerted, as
    /// [`apply_batch`](Model::apply_batch). Only a paired device has a
    /// link to set: unpaired, the link stays `Unpaired`.
    pub(crate) fn set_link(&mut self, link: PairedLink) -> Vec<(SessionId, ApprovalId)> {
        self.link_to(link);
        self.note_new_approvals()
    }

    pub(crate) fn dirs_listed(&mut self, req: u64, result: Result<DirsResult, TransportError>) {
        if let Place::SessionList(page) = &mut self.place {
            page.completer.listed(req, result);
        }
    }

    pub(crate) fn conversations_listed(
        &mut self,
        req: u64,
        result: Result<ConversationsResult, TransportError>,
    ) {
        if let Place::SessionList(page) = &mut self.place {
            page.conversations.listed(req, result);
        }
    }

    /// Whether `req` is the usage limits read the page waits on
    /// ([`Seam::usage_limits_answered`](crate::Seam::usage_limits_answered)).
    pub(crate) fn usage_limits_awaited(&self, req: u64) -> bool {
        self.usage_limits.awaited(req)
    }

    /// The awaited usage limits read got no answer to keep.
    pub(crate) fn usage_limits_failed(&mut self, why: UsageLimitsUnavailable) {
        self.usage_limits.failed(why);
    }

    /// The awaited usage limits read's answer, stored.
    pub(crate) fn usage_limits_stored(&mut self, answer: UsageLimitsResult) {
        self.usage_limits.answered(answer);
    }

    /// Whether `req` is the login request the settings screen waits on
    /// ([`Seam::login_finished`](crate::Seam::login_finished)).
    pub(crate) fn login_awaited(&self, req: u64) -> bool {
        self.login.awaited(req)
    }

    /// The answer to login start `req`; dropped unless awaited.
    pub(crate) fn login_started(
        &mut self,
        req: u64,
        result: Result<LoginStartResult, TransportError>,
    ) {
        if self.login.awaited(req) {
            self.login.started(result);
        }
    }

    /// The answer to login finish `req`; dropped unless awaited. A
    /// `LoggedIn` whose hello fields were `stored` shows them as a hello
    /// would: no longer logged out, expiring at its `expires`.
    pub(crate) fn login_finished(
        &mut self,
        req: u64,
        result: Result<LoginFinishResult, TransportError>,
        stored: bool,
    ) {
        if !self.login.awaited(req) {
            return;
        }
        if stored && let Ok(LoginFinishResult::LoggedIn { expires }) = &result {
            self.settings.server.logged_in(*expires);
        }
        self.login.finished(result);
    }

    pub(crate) fn storage_reported(
        &mut self,
        req: u64,
        result: Result<StorageReport, TransportError>,
    ) {
        self.storage.reported(req, result);
    }

    pub(crate) fn device_storage_read(
        &mut self,
        req: u64,
        result: Result<DeviceStorage, StoreError>,
    ) {
        self.storage.device_read(req, result);
    }

    pub(crate) fn storage_reclaimed(
        &mut self,
        req: u64,
        result: Result<StorageReport, TransportError>,
    ) {
        self.storage.reclaimed(req, result);
    }

    pub(crate) fn worktrees_listed(
        &mut self,
        req: u64,
        result: Result<WorktreeList, TransportError>,
    ) {
        self.worktrees.listed(req, result);
    }

    /// Whether `req` is the request `ask` still waits on: anything else is
    /// stale and must touch neither the replica nor the model.
    pub(crate) fn workdir_awaited(&self, req: u64, ask: &WorkdirAsk) -> bool {
        self.workdirs.awaited(req, ask)
    }

    /// The awaited read of `ask` failed (a transport error, git failing):
    /// the stored answer stays.
    pub(crate) fn workdir_failed(&mut self, ask: &WorkdirAsk, reason: String) {
        self.workdirs.failed(ask, reason);
    }

    /// The awaited answer to `ask` was not stored: its session is gone.
    pub(crate) fn workdir_dropped(&mut self, ask: &WorkdirAsk) {
        self.workdirs.dropped(ask);
    }

    /// The answer to the push `req` to `path` of `session`: kept only if it
    /// is still that push's.
    pub(crate) fn workdir_pushed(
        &mut self,
        req: u64,
        session: SessionId,
        path: String,
        result: Result<PushOutcome, TransportError>,
    ) {
        self.workdirs.pushed(req, session, path, result);
    }

    /// Mirrors [`Store::put_workdir`](crate::Store::put_workdir), which
    /// stored the awaited `answer` to `ask`: the answer replaces the last
    /// one, the blobs it registered join the blob map and those it purged
    /// leave it. Returns the downloads of the blobs wanted now: a download
    /// ask's content, whatever is on screen, then the blob the pane on
    /// screen shows.
    pub(crate) fn workdir_stored(
        &mut self,
        ask: WorkdirAsk,
        answer: WorkdirAnswer,
        stored: &WorkdirStored,
    ) -> Vec<Request> {
        for blob in answer.blob_refs() {
            if stored.registered.contains(&blob.id) {
                self.blobs.insert(
                    blob.id,
                    BlobRecord {
                        id: blob.id,
                        size: blob.size,
                        sha256: blob.sha256,
                        status: BlobStatus::NotDownloaded,
                        workdir: true,
                    },
                );
            }
        }
        for blob in &stored.purged {
            self.forget_blob(*blob);
        }
        let download = match (&ask, &answer) {
            (
                WorkdirAsk::Download { .. },
                WorkdirAnswer::File(FileResult::File {
                    content: FileBody::Blob(blob),
                    ..
                }),
            ) => Some(blob.id),
            _ => None,
        };
        self.workdirs.answered(ask, answer);
        // Asked for by hand afresh: a download stopped before may go again.
        if let Some(id) = download {
            self.stopped.remove(&id);
        }
        let downloaded = download.and_then(|id| self.want_blob(id));
        downloaded
            .into_iter()
            .chain(self.want_shown_blob())
            .collect()
    }

    /// Mirrors [`Store::reset_for_server`](crate::Store::reset_for_server),
    /// which left the replica as `snapshot`: nothing of the previous
    /// server's log shows any more, nor what was kept for it — download
    /// requests and failures, alerted approvals, sessions seen working and
    /// idle alerts, listed directories. Heads replace the drafts they hold.
    pub(crate) fn replica_reset(&mut self, snapshot: Snapshot) {
        let had_drafts = !self.drafts.is_empty();
        self.reload(snapshot);
        self.wanted.clear();
        self.stopped.clear();
        self.alerted.clear();
        self.alerted_at.clear();
        self.seen_working.clear();
        self.went_idle.clear();
        // Closed, the request ids kept: an answer from the old server
        // cannot match a later request.
        if let Place::SessionList(page) = &mut self.place {
            *page = ListPage::default();
        }
        self.workdirs.reset();
        // The old server's login flow means nothing to the new one.
        self.login.forget();
        if had_drafts {
            self.draft_rev += 1;
        }
    }

    /// [`Model::replica_reset`] after the server's log lost history: the
    /// link goes on, `Connecting` until the engine reports.
    pub(crate) fn history_reset(&mut self, snapshot: Snapshot) {
        self.replica_reset(snapshot);
        self.link_to(PairedLink::Connecting);
    }

    /// The device paired (again): `paired` is the record just saved.
    pub(crate) fn paired(&mut self, paired: Paired) {
        self.paired = Some(paired);
        self.pairing = PairingStatus::Idle;
        self.place = Place::session_list();
        self.last_list = ListKind::Active;
    }

    /// Mirrors the pairing's addresses as stored, after a failed write.
    pub(crate) fn addresses_stored(&mut self, addresses: ServerAddresses) {
        if let Some(paired) = &mut self.paired {
            paired.addresses = addresses;
        }
    }

    pub(crate) fn pairing_failed(&mut self, reason: String) {
        if let PairingStatus::InProgress { address } = &self.pairing {
            self.pairing = PairingStatus::Failed {
                address: address.to_string(),
                reason,
            };
        }
    }

    /// The blob left the replica: its record goes, with the request for it.
    fn forget_blob(&mut self, id: BlobId) {
        self.blobs.remove(&id);
        self.wanted.remove(&id);
        self.stopped.remove(&id);
    }

    /// The `from` of each crop pending to produce `into`: a `Cropped` fate
    /// naming `into` whose blob still has a record (`Store::pending_crops`).
    fn crops_into(&self, into: BlobId) -> impl Iterator<Item = BlobId> + '_ {
        self.fates
            .iter()
            .filter_map(move |(from, fate)| match fate {
                BlobFate::Cropped { into: made, .. }
                    if made.id == into && self.blobs.contains_key(from) =>
                {
                    Some(*from)
                }
                _ => None,
            })
    }

    /// Whether this device holds `blob`'s bytes, or will: available, or
    /// the `into` of a pending crop (as `Store::apply_batch` decides).
    fn blob_held(&self, blob: BlobId) -> bool {
        match self.blobs.get(&blob) {
            Some(record) => {
                matches!(record.status, BlobStatus::Available { .. })
                    || self.crops_into(blob).next().is_some()
            }
            None => false,
        }
    }

    /// Whether the replica already knows `id`, registered or fated: a batch
    /// that references it cannot register it again.
    pub(crate) fn knows_blob(&self, id: BlobId) -> bool {
        self.blobs.contains_key(&id) || self.fates.contains_key(&id)
    }

    /// `blob` left the replica, and so did any blob a pending crop would
    /// have made it from (as `Store::apply_batch` and
    /// `Store::end_local_crop` purge them).
    fn purge_blob(&mut self, blob: BlobId) {
        let mut next = vec![blob];
        while let Some(blob) = next.pop() {
            next.extend(self.crops_into(blob).collect::<Vec<_>>());
            self.forget_blob(blob);
        }
    }

    /// The store purged these blobs (`Store::end_local_crop`).
    pub(crate) fn blobs_purged(&mut self, ids: &[BlobId]) {
        for id in ids {
            self.forget_blob(*id);
        }
    }

    pub(crate) fn blob_status(&mut self, id: BlobId, status: BlobStatus) {
        // A request the queue has not seen yet may overtake a stop: it
        // still shows as requested.
        if !matches!(status, BlobStatus::Stopped { .. }) {
            self.wanted.remove(&id);
        }
        if let Some(blob) = self.blobs.get_mut(&id) {
            blob.status = status;
        }
    }

    /// The blob queue paused (or resumed) every download because the
    /// device's disk is full; the views say so.
    pub(crate) fn set_blobs_disk_full(&mut self, full: bool) {
        self.blobs_disk_full = full;
    }

    fn outbox_entry(&mut self, id: CommandId) -> Option<&mut OutboxEntry> {
        self.outbox.iter_mut().find(|e| e.command.id == id)
    }

    // ---- view -------------------------------------------------------------

    pub(crate) fn view(&self) -> ViewState {
        ViewState {
            link: self.link(),
            queued: self
                .outbox
                .iter()
                .filter(|e| !e.status.is_settled())
                .count(),
            attention: self.attention(),
            screen: self.screen(),
            can_go_back: self.shown_subagent().is_some() || self.back().is_some(),
            snippets: self.snippets.list().to_vec(),
            shell_history: self.shell_history.list(),
            login_expires: self.settings.server.login_expires,
            agent_login_offered: self.settings.server.agent_login_offered,
            agent_logged_out: self.settings.server.agent_logged_out,
        }
    }

    fn attention(&self) -> Attention {
        let open = self.open_session();
        let approvals: Vec<PendingApprovalRef> = self
            .waiting_approvals()
            .into_iter()
            .filter_map(|(session_id, id)| {
                Some(PendingApprovalRef {
                    id,
                    session_id,
                    title: self.title(self.summary(session_id)?).text,
                    alerted_at: self.alerted_at.get(&id).copied().unwrap_or(0),
                })
            })
            .collect();
        let mut idle: Vec<IdleAlert> = self
            .went_idle
            .iter()
            .filter_map(|(&session_id, &at)| {
                Some(IdleAlert {
                    session_id,
                    title: self.title(self.summary(session_id)?).text,
                    at,
                })
            })
            .collect();
        idle.sort_by_key(|alert| alert.at);
        Attention {
            elsewhere: approvals
                .iter()
                .filter(|a| Some(a.session_id) != open)
                .count(),
            approvals,
            alerts: self.alerts,
            shell_alerts: self.shell_alerts,
            last_shell: self.last_shell.clone(),
            idle_alerts: self.idle_alerts,
            idle,
        }
    }

    /// The Approvals screen: every approval this device can answer, oldest
    /// first, from the facts cached for it. Answers and inputs are read
    /// now, as the live state they are.
    fn approvals_view(&self) -> ApprovalsView {
        let items = self
            .waiting_approvals()
            .into_iter()
            .filter_map(|(session_id, id)| {
                let session_title = self.title(self.summary(session_id)?).text;
                let asks = match self.waiting_facts.get(&id) {
                    Some(Waiting::Approval(facts)) => Asks::Approval(Box::new(ApprovalPrompt {
                        id,
                        call: facts.call,
                        from: facts.from.clone(),
                        tool: facts.tool.clone(),
                        summary: facts.summary.clone(),
                        input: facts.input.as_ref().map(|full| self.full_view(full)),
                        suggestions: facts.suggestions.clone(),
                        shell: facts.shell.clone(),
                        answer: self.local_answer(id, &facts.suggestions, true),
                    })),
                    Some(Waiting::Question) => Asks::Question,
                    Some(Waiting::Unknown) | None => Asks::Unknown,
                };
                Some(WaitingApproval {
                    session_id,
                    session_title,
                    id,
                    asks,
                })
            })
            .collect();
        ApprovalsView { items }
    }

    fn screen(&self) -> Screen {
        if self.device().is_none() {
            return Screen::Pairing(self.pairing_view());
        }
        match &self.place {
            Place::Pairing => Screen::Pairing(self.pairing_view()),
            Place::Settings => Screen::Settings(SettingsView {
                blob_policy: self.settings.blob_policy,
                default_mode: self.settings.default_mode,
                default_safety_check: self.settings.default_safety_check,
                bypass_allowed: self.settings.server.bypass_allowed,
                blobs_disk_full: self.blobs_disk_full,
                server_address: self.paired.as_ref().map(|p| p.addresses.current().clone()),
                other_addresses: self
                    .paired
                    .as_ref()
                    .map(|p| p.addresses.others().to_vec())
                    .unwrap_or_default(),
                device_id: self.device(),
                server_fingerprint: self.paired.as_ref().and_then(|p| p.server),
                login: LoginView {
                    offered: self.settings.server.agent_login_offered,
                    logged_out: self.settings.server.agent_logged_out,
                    expires: self.settings.server.login_expires,
                    flow: self.login.view(),
                },
                safety_rules: self.safety_rule_views(),
            }),
            Place::ServerAddresses => Screen::ServerAddresses(ServerAddressesView {
                current: self.paired.as_ref().map(|p| p.addresses.current().clone()),
                others: self
                    .paired
                    .as_ref()
                    .map(|p| p.addresses.others().to_vec())
                    .unwrap_or_default(),
            }),
            Place::SessionList(page) => {
                Screen::SessionList(self.session_list(ListKind::Active, Some(page)))
            }
            Place::Archived => Screen::SessionList(self.session_list(ListKind::Archived, None)),
            Place::All => Screen::SessionList(self.session_list(ListKind::All, None)),
            Place::Usage(_) => Screen::Usage(self.usage_view()),
            Place::Storage(_) => Screen::Storage(self.storage_view()),
            Place::Worktrees => Screen::Worktrees(self.worktrees_view()),
            Place::Approvals => Screen::Approvals(self.approvals_view()),
            Place::UsageLimits(_) => Screen::UsageLimits(self.usage_limits.view()),
            Place::Session { id, open, scope } => {
                match self.session_view(*id, open.as_ref(), *scope) {
                    Some(view) => Screen::Session(Box::new(view)),
                    None => Screen::SessionList(self.session_list(ListKind::Active, None)),
                }
            }
            Place::SessionInfo { id, open } => match self.session_info_view(*id, open.as_ref()) {
                Some(view) => Screen::SessionInfo(Box::new(view)),
                None => Screen::SessionList(self.session_list(ListKind::Active, None)),
            },
            Place::Workdir { id, pane, last_dir } => match self.workdir_view(*id, pane, last_dir) {
                Some(view) => Screen::Workdir(view),
                None => Screen::SessionList(self.session_list(ListKind::Active, None)),
            },
            Place::Crop(page) => Screen::Crop(self.crop_view(page)),
            // A scope whose session is gone searches every session's page.
            Place::Search { scope } => Screen::Search(SearchView {
                scope: scope.and_then(|id| {
                    self.summary(id).map(|summary| SearchScope {
                        id,
                        title: self.title(summary).text,
                    })
                }),
            }),
        }
    }

    /// The crop page's blob file, if this device holds its bytes now and
    /// the blob is still the current one (no fate since).
    fn crop_path(&self, page: &CropPage) -> Option<&PathBuf> {
        if self.fates.contains_key(&page.blob) {
            return None;
        }
        match &self.blobs.get(&page.blob)?.status {
            BlobStatus::Available { path } => Some(path),
            _ => None,
        }
    }

    fn crop_view(&self, page: &CropPage) -> CropView {
        let content = match (self.crop_path(page), &page.lines) {
            _ if self.fates.contains_key(&page.blob) => CropContent::Gone,
            (Some(path), LineCount::Counted(lines)) => CropContent::Ready {
                path: path.clone(),
                lines: *lines,
            },
            (Some(_), LineCount::Failed(reason)) => CropContent::Unreadable {
                reason: reason.clone(),
            },
            (Some(_), LineCount::Unasked | LineCount::Asked(_)) => CropContent::Counting,
            (None, _) => match self.blobs.get(&page.blob) {
                Some(record) => CropContent::NotHere(
                    self.blob_view(&BlobRef {
                        id: record.id,
                        size: record.size,
                        sha256: record.sha256,
                    })
                    .state,
                ),
                None => CropContent::Gone,
            },
        };
        CropView {
            blob: page.blob,
            session: page.session,
            holds: page.holds,
            title: page.title.clone(),
            content,
            keep: page.keep.clone(),
            pending: self.blob_pending(page.blob),
        }
    }

    /// The count of the crop page's lines, if it is on screen, holds its
    /// blob and has not asked yet: asked once per page.
    pub(crate) fn count_wanted(&mut self) -> Option<Request> {
        self.device()?;
        let Place::Crop(page) = &self.place else {
            return None;
        };
        if page.lines != LineCount::Unasked {
            return None;
        }
        let path = self.crop_path(page)?.clone();
        self.count_req += 1;
        let req = self.count_req;
        if let Place::Crop(page) = &mut self.place {
            page.lines = LineCount::Asked(req);
        }
        Some(Request::CountLines { req, path })
    }

    /// Each device's latest capture in the open session, asked for once
    /// whatever the blob policy: the app screen shows it (D65).
    pub(crate) fn want_captures(&mut self) -> Vec<Request> {
        let Place::Session { id, open, .. } = &self.place else {
            return Vec::new();
        };
        let events = open.as_ref().map_or(&[][..], |open| open.events.as_slice());
        // Only a `Done` image names a capture: without one, skip the fold
        // of the whole log.
        let some_capture = events.iter().any(|(_, event)| {
            matches!(
                event.kind,
                EventKind::ScreenDone {
                    outcome: ScreenOutcome::Done {
                        shot: ScreenShot::Image(_),
                    },
                    ..
                }
            )
        });
        if !some_capture {
            return Vec::new();
        }
        let ids: Vec<BlobId> = self
            .app_screen_view(*id, events)
            .captures
            .iter()
            .filter_map(|capture| match &capture.content {
                FullView::Blob(blob) => Some(blob.id),
                FullView::Inline(_) | FullView::Lost | FullView::Deleted { .. } => None,
            })
            .collect();
        ids.into_iter()
            .filter_map(|id| self.want_blob(id))
            .collect()
    }

    /// The answer to [`Request::CountLines`] `req`; any but the one the
    /// page waits on is ignored.
    pub(crate) fn lines_counted(&mut self, req: u64, result: std::io::Result<u64>) {
        let Place::Crop(page) = &mut self.place else {
            return;
        };
        if page.lines != LineCount::Asked(req) {
            return;
        }
        page.lines = match result.map(u32::try_from) {
            Ok(Ok(lines)) => LineCount::Counted(lines),
            Ok(Err(_)) => LineCount::Failed("too many lines".to_owned()),
            Err(error) => LineCount::Failed(error.to_string()),
        };
    }

    fn workdir_view(
        &self,
        id: SessionId,
        pane: &WorkdirPane,
        last_dir: &str,
    ) -> Option<WorkdirView> {
        let summary = self.summary(id)?;
        let pane = pane.clone();
        let ask = pane_ask(id, &pane);
        let body = match (&pane, self.workdirs.stored.get(&ask)) {
            (WorkdirPane::Changes, Some(WorkdirAnswer::Changes(result))) => {
                WorkdirBody::Changes(result.clone())
            }
            (WorkdirPane::Commits, Some(WorkdirAnswer::Log(result))) => {
                WorkdirBody::Log(result.clone())
            }
            (WorkdirPane::Diff { path, untracked }, Some(WorkdirAnswer::Changes(result))) => {
                let file = changed_file(result, path, *untracked).cloned();
                let blob = file.as_ref().and_then(|file| match &file.diff {
                    DiffBody::Blob(blob) => Some(self.blob_view(blob)),
                    DiffBody::Binary | DiffBody::TooLarge { .. } | DiffBody::Omitted => None,
                });
                WorkdirBody::Diff { file, blob }
            }
            (WorkdirPane::Dir { .. }, Some(WorkdirAnswer::List(result))) => {
                WorkdirBody::Dir(result.clone())
            }
            (WorkdirPane::File { .. }, Some(WorkdirAnswer::File(result))) => {
                let blob = match result {
                    FileResult::File {
                        content: FileBody::Blob(blob),
                        ..
                    } => Some(self.blob_view(blob)),
                    _ => None,
                };
                WorkdirBody::File {
                    result: result.clone(),
                    blob,
                }
            }
            // Never read; a stored answer always answers its ask's kind.
            _ => WorkdirBody::Nothing,
        };
        Some(WorkdirView {
            session: id,
            title: self.title(summary).text,
            workdir: summary.workdir.clone(),
            pane,
            last_dir: last_dir.to_owned(),
            fetch: self.workdirs.fetch(&ask),
            body,
            downloads: self.workdir_downloads(id),
            pushes: self.workdirs.pushes(id),
        })
    }

    /// The downloads of session `id`'s files, one per path, sorted: each
    /// one's read, its stored answer and, if the answer carries a blob, the
    /// blob's view.
    fn workdir_downloads(&self, id: SessionId) -> Vec<WorkdirDownload> {
        self.workdirs
            .download_paths(id)
            .into_iter()
            .map(|path| {
                let ask = WorkdirAsk::Download {
                    session: id,
                    path: path.clone(),
                };
                let fetch = self.workdirs.fetch(&ask);
                let result = match self.workdirs.stored.get(&ask) {
                    Some(WorkdirAnswer::File(result)) => Some(result.clone()),
                    _ => None,
                };
                let blob = match &result {
                    Some(FileResult::File {
                        content: FileBody::Blob(blob),
                        ..
                    }) => Some(self.blob_view(blob)),
                    _ => None,
                };
                WorkdirDownload {
                    path,
                    fetch,
                    result,
                    blob,
                }
            })
            .collect()
    }

    fn pairing_view(&self) -> PairingView {
        PairingView {
            paired: self.paired.as_ref().map(|paired| PairedInfo {
                device_id: paired.device,
                server_address: paired.addresses.current().clone(),
            }),
            status: self.pairing.clone(),
        }
    }

    /// The `kind` list; `page` is what the active list on screen keeps,
    /// none when another place falls back to the list.
    fn session_list(&self, kind: ListKind, page: Option<&ListPage>) -> SessionListView {
        let (listed, other): (Vec<_>, Vec<_>) = self.sessions.iter().partition(|s| {
            kind == ListKind::All || self.archived(s) == (kind == ListKind::Archived)
        });
        let sessions = listed
            .into_iter()
            .map(|s| {
                let title = self.title(s);
                SessionRow {
                    id: s.id,
                    title: title.text,
                    named: title.named,
                    rename: title.rename,
                    workdir: s.workdir.clone(),
                    worktree: s.worktree.clone(),
                    worktree_removed: s.worktree_removed,
                    archived: self.archived(s),
                    state: session_state(s),
                    pending_approvals: s.pending_approvals.len(),
                    last_activity: s.last_activity,
                    has_draft: !self.draft_images_of(s.id).is_empty()
                        || !self.draft_files_of(s.id).is_empty()
                        || [ComposerMode::Agent, ComposerMode::Shell]
                            .iter()
                            .any(|mode| {
                                self.drafts
                                    .get(&(s.id, *mode))
                                    .is_some_and(|d| !d.text.trim().is_empty())
                            }),
                    unread: match self.read_marks.get(&s.id) {
                        Some(mark) => s.notable.saturating_sub(mark.notable),
                        None => s.notable,
                    },
                    cost_usd: s.usage.as_ref().and_then(|u| u.total.cost_usd),
                    deletable: self.deletable(s),
                    deletable_everywhere: self.deletable_everywhere(s),
                    importing: s.importing,
                }
            })
            .collect();
        if kind != ListKind::Active {
            return SessionListView {
                kind,
                sessions,
                other_count: other.len(),
                creating: Vec::new(),
                completion: None,
                conversations: None,
            };
        }
        let creating = self
            .outbox
            .iter()
            .filter_map(|e| self.pending_session(e))
            .collect();
        let completion = page.and_then(|page| {
            page.completer
                .view(self.sessions.iter().filter_map(|s| s.workdir.as_deref()))
        });
        SessionListView {
            kind,
            sessions,
            other_count: other.len(),
            creating,
            completion,
            conversations: page.and_then(|page| page.conversations.view(&self.sessions)),
        }
    }

    fn usage_view(&self) -> UsageView {
        UsageView {
            turns: self.turns.clone(),
            sessions: self
                .sessions
                .iter()
                .map(|s| UsageSession {
                    id: s.id,
                    title: self.title(s).text,
                    workdir: s.workdir.clone(),
                    archived: self.archived(s),
                })
                .collect(),
            deleted: self.deleted.len(),
        }
    }

    fn session_info_view(
        &self,
        id: SessionId,
        open: Option<&OpenSession>,
    ) -> Option<SessionInfoView> {
        let summary = self.summary(id)?;
        let events = open.map_or(&[][..], |open| open.events.as_slice());
        let (timeline, _) = self.timeline(id, events);
        let subagents = timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry: Entry::Tool(view),
                    ..
                } if view.kind == ToolKind::Subagent => subagent_header(&timeline, view.call),
                _ => None,
            })
            .collect();
        Some(SessionInfoView {
            id,
            title: self.title(summary).text,
            workdir: summary.workdir.clone(),
            usage: summary.usage.clone(),
            turns: self
                .turns
                .iter()
                .filter(|t| t.session_id == id)
                .cloned()
                .collect(),
            agent: self.agent_view(summary),
            subagents,
            limits: self.usage_limits.view(),
        })
    }

    fn storage_view(&self) -> StorageView {
        let (status, report, device) = self.storage.shown();
        let sessions = report
            .iter()
            .flat_map(|report| report.sessions.iter().map(|s| s.session))
            .chain(
                device
                    .iter()
                    .flat_map(|device| device.sessions.iter().map(|s| s.session)),
            )
            .filter_map(|id| {
                let summary = self.summary(id)?;
                let session = StorageSession {
                    title: self.title(summary).text,
                    archived: self.archived(summary),
                    deletable: self.deletable(summary),
                    deletable_everywhere: self.deletable_everywhere(summary),
                };
                Some((id, session))
            })
            .collect();
        let gone = report
            .iter()
            .flat_map(|report| {
                let runs = report.shell_runs.iter().filter_map(|r| r.output_blob);
                report.blobs.iter().map(|b| b.id).chain(runs)
            })
            .filter(|blob| self.fates.contains_key(blob))
            .collect();
        StorageView {
            status,
            report,
            sessions,
            pending: self.blob_pendings(),
            gone,
            device,
            device_reclaiming: self.storage.device_reclaiming(),
        }
    }

    fn worktrees_view(&self) -> WorktreesView {
        let (status, list) = self.worktrees.shown();
        let paths = list
            .iter()
            .flat_map(|list| &list.repos)
            .filter_map(|repo| match &repo.listing {
                RepoListing::Worktrees { worktrees, .. } => Some(worktrees),
                RepoListing::Gone | RepoListing::Failed { .. } => None,
            })
            .flatten()
            .map(|entry| entry.path.as_str());
        let sessions: HashMap<String, Vec<WorktreeSession>> = paths
            .filter_map(|path| {
                let running: Vec<WorktreeSession> = self
                    .sessions
                    .iter()
                    .filter(|s| s.workdir.as_deref() == Some(path))
                    .map(|s| WorktreeSession {
                        id: s.id,
                        title: self.title(s).text,
                        archived: self.archived(s),
                        worktree: s.worktree.is_some(),
                        worktree_removed: s.worktree_removed,
                        removal: self.latest_outgoing(|kind| {
                            matches!(kind, CommandKind::RemoveWorktree { session_id } if *session_id == s.id)
                        }),
                    })
                    .collect();
                (!running.is_empty()).then(|| (path.to_owned(), running))
            })
            .collect();
        WorktreesView {
            status,
            list,
            sessions,
        }
    }

    fn session_view(
        &self,
        id: SessionId,
        open: Option<&OpenSession>,
        scope: Scope,
    ) -> Option<SessionView> {
        let summary = self.session_or_creating(id)?;
        let events = open.map_or(&[][..], |open| open.events.as_slice());
        let (mut timeline, rules) = self.timeline(id, events);
        let pending_approval = summary.pending_approval().and_then(|approval| {
            events.iter().find_map(|(_, event)| match &event.kind {
                EventKind::ApprovalRequested {
                    id,
                    call,
                    tool,
                    summary,
                    input,
                    suggestions,
                    shell,
                } if *id == approval => Some(ApprovalPrompt {
                    id: *id,
                    call: *call,
                    from: call
                        .and_then(|call| tool_item(&timeline, call)?.0)
                        .and_then(|outer| subagent_item(&timeline, outer))
                        .map(|(_, view)| view.title.clone()),
                    tool: tool.clone(),
                    summary: summary.clone(),
                    input: input.as_ref().map(|f| self.full_view(f)),
                    suggestions: suggestions.clone(),
                    shell: shell.clone(),
                    answer: self.local_answer(*id, suggestions, true),
                }),
                _ => None,
            })
        });
        let pending_question = summary.pending_approval().and_then(|approval| {
            events.iter().find_map(|(_, event)| match &event.kind {
                EventKind::QuestionAsked {
                    id,
                    call,
                    questions,
                } if *id == approval => Some(QuestionPrompt {
                    id: *id,
                    call: *call,
                    from: call
                        .and_then(|call| tool_item(&timeline, call)?.0)
                        .and_then(|outer| subagent_item(&timeline, outer))
                        .map(|(_, view)| view.title.clone()),
                    questions: questions.clone(),
                    answer: self.local_answer(*id, &[], true),
                    answers: self.local_answers(*id),
                }),
                _ => None,
            })
        });
        let state = session_state(&summary);
        let interrupt = self
            .latest_outgoing(
                |kind| matches!(kind, CommandKind::Interrupt { session_id } if *session_id == id),
            )
            .filter(|outgoing| interrupt_matters(*outgoing, &state));
        let stop = self.latest_outgoing(
            |kind| matches!(kind, CommandKind::StopSession { session_id } if *session_id == id),
        );
        let clear = self.latest_outgoing(
            |kind| matches!(kind, CommandKind::ClearContext { session_id, .. } if *session_id == id),
        );
        let switch = self.latest_outgoing(
            |kind| matches!(kind, CommandKind::SwitchContext { session_id, .. } if *session_id == id),
        );
        let context = self.target_context(&summary);
        let viewing = scope.context;
        let title = self.title(&summary);
        let subagent = scope
            .subagent
            .and_then(|call| subagent_header(&timeline, call));
        // Folded whole first: results, approvals and unanswered calls
        // settle earlier items by index.
        if let Some(header) = &subagent {
            // Shown whole, whatever context `viewing` narrows to.
            timeline.retain(|item| {
                matches!(item, TimelineItem::Logged { parent: Some(call), .. } if *call == header.call)
            });
        } else {
            timeline.retain(|item| {
                !matches!(
                    item,
                    TimelineItem::Logged {
                        parent: Some(_),
                        ..
                    }
                )
            });
        }
        if let Some(viewing) = viewing.filter(|_| subagent.is_none()) {
            timeline.retain(|item| match item {
                TimelineItem::Logged {
                    entry:
                        Entry::ContextStarted { .. }
                        | Entry::ContextSwitched { .. }
                        | Entry::ContextMoved { .. }
                        | Entry::MovedFrom { .. }
                        | Entry::Adopted { .. }
                        | Entry::ImportFinished { .. },
                    ..
                } => false,
                TimelineItem::Logged { context, .. } | TimelineItem::Pending { context, .. } => {
                    *context == viewing
                }
                TimelineItem::Change { .. } => true,
            });
        }
        let contexts = self.contexts(id, events, context);
        if subagent.is_none() && viewing.is_none_or(|v| v == context) {
            timeline.extend(self.pending_changes(&summary, &contexts));
            // Pending messages and switches in the order they were asked for.
            let start = timeline
                .iter()
                .rposition(|item| matches!(item, TimelineItem::Logged { .. }))
                .map_or(0, |at| at + 1);
            timeline[start..].sort_by_key(|item| match item {
                TimelineItem::Pending { command_id, .. }
                | TimelineItem::Change { command_id, .. } => Some(*command_id),
                TimelineItem::Logged { .. } => None,
            });
        }
        let can_clear = contexts.iter().any(|c| c.current && c.messages > 0);
        let contexts = contexts
            .into_iter()
            .filter(|c| c.current || c.messages > 0)
            .collect();
        Some(SessionView {
            id,
            title: title.text,
            named: title.named,
            rename: title.rename,
            archived: self.archived(&summary),
            workdir: summary.workdir.clone(),
            state,
            creating: self.creating(id),
            timeline,
            pending_approval,
            pending_question,
            rules,
            usage: summary.usage.clone(),
            agent: self.agent_view(&summary),
            safety_check: summary.safety_check,
            safety_check_change: self.safety_check_change(id),
            contexts,
            context,
            viewing,
            subagent,
            can_clear,
            clear,
            switch,
            interrupt,
            stop,
            work: summary.work.clone(),
            task_stops: self.task_stops(&summary, events),
            mode: self.composer_mode(id),
            draft: self.drafts.get(&(id, ComposerMode::Agent)).cloned(),
            draft_images: self.draft_images_of(id).to_vec(),
            draft_files: self.draft_files_of(id).to_vec(),
            shell_draft: self.drafts.get(&(id, ComposerMode::Shell)).cloned(),
            draft_rev: self.draft_rev,
            shell: self.shell_view(id, events),
            screen_enabled: self.settings.server.screen_enabled,
            app_screen: self.app_screen_view(id, events),
            last_seq: summary.last_seq,
            unread_from: open.and_then(|open| open.unread_from),
        })
    }

    /// What to call a session: this device's latest rename while it is on
    /// its way (unless it failed), else the session's own title.
    fn title(&self, summary: &SessionSummary) -> Title {
        let rename = self
            .outbox
            .iter()
            .rev()
            .find_map(|e| match &e.command.kind {
                CommandKind::RenameSession { session_id, name } if *session_id == summary.id => {
                    Some((name, Outgoing::from(&e.status)))
                }
                _ => None,
            })
            .filter(|(_, outgoing)| !self.in_replica(*outgoing));
        // Accepted but not yet pulled is still newer than the replica.
        let name = match rename {
            Some((name, outgoing)) if !matches!(outgoing, Outgoing::Failed(_)) => name.as_deref(),
            _ => summary.name.as_deref(),
        };
        Title {
            text: name.map_or_else(|| summary.default_title(), str::to_owned),
            named: name.is_some(),
            rename: rename.map(|(_, outgoing)| outgoing),
        }
    }

    /// Whether a session is on the archived list: the replica's flag, unless
    /// this device's latest archiving of it is on its way (and has not
    /// failed); either way a message or shell run queued here after that
    /// brings it back, as it will in the log.
    fn archived(&self, summary: &SessionSummary) -> bool {
        let id = summary.id;
        let on_its_way = |e: &OutboxEntry| {
            let outgoing = Outgoing::from(&e.status);
            !self.in_replica(outgoing) && !matches!(outgoing, Outgoing::Failed(_))
        };
        let latest = self.outbox.iter().rposition(|e| {
            matches!(e.command.kind, CommandKind::SetSessionArchived { session_id, .. } if session_id == id)
        });
        let archived = match latest.map(|i| &self.outbox[i]) {
            Some(e) if on_its_way(e) => {
                matches!(
                    e.command.kind,
                    CommandKind::SetSessionArchived { archived: true, .. }
                )
            }
            _ => summary.archived,
        };
        let after = latest.map_or(0, |i| i + 1);
        let message = self.outbox[after..].iter().any(|e| {
            on_its_way(e)
                && matches!(
                    &e.command.kind,
                    CommandKind::SendMessage { session_id, .. }
                        | CommandKind::RunShell { session_id, .. }
                        if *session_id == id
                )
        });
        archived && !message
    }

    /// Whether [`Action::DeleteSessionLocally`] may remove the session: it
    /// is archived, and nothing about it is still open here — no pending
    /// approval, no running turn, no command of this device unsettled. An
    /// undeliverable one is settled: it is never sent again.
    fn deletable(&self, summary: &SessionSummary) -> bool {
        let unsettled = self.outbox.iter().any(|e| {
            !e.status.is_settled() && command_session(&e.command.kind) == Some(summary.id)
        });
        self.archived(summary)
            && summary.pending_approvals.is_empty()
            && summary.state != LogState::AgentWorking
            && !unsettled
    }

    /// Whether [`Action::DeleteSessionEverywhere`] may send: `deletable`, and
    /// its worktree is gone, since the server refuses a session whose
    /// worktree is still on disk.
    fn deletable_everywhere(&self, summary: &SessionSummary) -> bool {
        self.deletable(summary) && (summary.worktree.is_none() || summary.worktree_removed)
    }

    /// The device whose request `answers` names, if it was seen.
    fn asker(
        &self,
        asked_by: &HashMap<CommandId, DeviceId>,
        answers: Option<CommandId>,
    ) -> Option<DeviceRef> {
        answers
            .and_then(|command| asked_by.get(&command))
            .map(|device| self.device_ref(*device))
    }

    /// This device's stops still on their way, one per task: the newest
    /// `StopTask` of each task in the session's work, unless its refusal
    /// is in the loaded `events` (the open session's).
    fn task_stops(
        &self,
        summary: &SessionSummary,
        events: &[(Seq, Event)],
    ) -> Vec<(TaskId, Outgoing)> {
        let mut stops: Vec<(TaskId, Outgoing)> = Vec::new();
        for entry in self.outbox.iter().rev() {
            let CommandKind::StopTask { session_id, task } = &entry.command.kind else {
                continue;
            };
            if *session_id != summary.id
                || stops.iter().any(|(t, _)| t == task)
                || !summary.work.tasks.iter().any(|t| t.id == *task)
            {
                continue;
            }
            let outgoing = Outgoing::from(&entry.status);
            let refused = self.in_replica(outgoing)
                && events.iter().any(|(_, e)| {
                    matches!(&e.kind, EventKind::TaskStopRefused { answers, .. }
                        if *answers == Some(entry.command.id))
                });
            if !refused {
                stops.push((task.clone(), outgoing));
            }
        }
        stops.reverse();
        stops
    }

    /// The session's shell runs: the loaded `events` (the open session's)
    /// folded in log order, then this device's runs not in them yet, as
    /// pending timeline items are; each marked stopping by a logged stop
    /// or this device's not refused.
    fn shell_view(&self, id: SessionId, events: &[(Seq, Event)]) -> ShellView {
        let outbox: HashMap<CommandId, &OutboxEntry> =
            self.outbox.iter().map(|e| (e.command.id, e)).collect();
        let mut runs: Vec<ShellRun> = Vec::new();
        let mut cwd = None;
        let find = |runs: &[ShellRun], run: CommandId| -> Option<usize> {
            runs.iter().position(|r| r.run == run)
        };
        for (_, event) in events {
            match &event.kind {
                EventKind::ShellRequested { command, when, by } => {
                    // The server logs each with its command id, the run's.
                    let Some(run) = event.command_id.filter(|run| find(&runs, *run).is_none())
                    else {
                        continue;
                    };
                    let sent = outbox.get(&run);
                    runs.push(ShellRun {
                        run,
                        command: command.clone(),
                        when: *when,
                        by: DeviceRef {
                            me: self.is_own_message(sent.is_some(), *by),
                            ..self.device_ref(*by)
                        },
                        state: ShellRunState::Queued,
                        requested_at: Some(event.at),
                        started_at: None,
                        finished_at: None,
                        tail_or_preview: String::new(),
                        lines: 0,
                        output: None,
                        truncated: false,
                        outgoing: sent.map(|e| Outgoing::from(&e.status)),
                        discardable: sent.is_some_and(|e| e.discardable()),
                        stopping: false,
                        delivery: sent.and_then(|e| Delivery::of(e)),
                    });
                }
                EventKind::ShellStopRequested { run, .. } => {
                    if let Some(i) = find(&runs, *run) {
                        runs[i].stopping = true;
                    }
                }
                EventKind::ShellStarted { run, cwd: started } => {
                    cwd = Some(started.clone());
                    if let Some(i) = find(&runs, *run) {
                        runs[i].state = ShellRunState::Running;
                        runs[i].started_at = Some(event.at);
                    }
                }
                EventKind::ShellOutput {
                    run, tail, lines, ..
                } => {
                    if let Some(r) = find(&runs, *run)
                        .map(|i| &mut runs[i])
                        .filter(|r| r.state == ShellRunState::Running)
                    {
                        r.tail_or_preview.clone_from(tail);
                        r.lines = *lines;
                    }
                }
                EventKind::ShellFinished {
                    run,
                    started_at,
                    end,
                    cwd: ended,
                    preview,
                    lines,
                    output,
                    truncated,
                    ..
                } => {
                    if let Some(ended) = ended {
                        cwd = Some(ended.clone());
                    }
                    if let Some(i) = find(&runs, *run) {
                        let r = &mut runs[i];
                        r.state = ShellRunState::Finished(end.clone());
                        r.started_at = started_at.or(r.started_at);
                        r.finished_at = Some(event.at);
                        r.tail_or_preview.clone_from(preview);
                        r.lines = *lines;
                        r.output = output.as_ref().map(|b| self.fated_view(b));
                        r.truncated = *truncated;
                    }
                }
                _ => {}
            }
        }
        if let Some(me) = self.device() {
            let logged: HashSet<CommandId> = runs.iter().map(|r| r.run).collect();
            runs.extend(self.outbox.iter().filter_map(|e| match &e.command.kind {
                CommandKind::RunShell {
                    session_id,
                    command,
                    when,
                } if *session_id == id && !logged.contains(&e.command.id) => Some(ShellRun {
                    run: e.command.id,
                    command: command.clone(),
                    when: *when,
                    by: DeviceRef {
                        me: true,
                        ..self.device_ref(me)
                    },
                    state: ShellRunState::Pending,
                    requested_at: None,
                    started_at: None,
                    finished_at: None,
                    tail_or_preview: String::new(),
                    lines: 0,
                    output: None,
                    truncated: false,
                    outgoing: Some(Outgoing::from(&e.status)),
                    discardable: e.discardable(),
                    stopping: false,
                    delivery: Delivery::of(e),
                }),
                _ => None,
            }));
        }
        // This device's stops, unless refused: one accepted but not yet
        // pulled is as good as logged.
        for e in &self.outbox {
            if let CommandKind::StopShell { session_id, run } = &e.command.kind
                && *session_id == id
                && !matches!(Outgoing::from(&e.status), Outgoing::Failed(_))
                && let Some(i) = find(&runs, *run)
            {
                runs[i].stopping = true;
            }
        }
        ShellView {
            enabled: self.settings.server.shell_enabled,
            cwd,
            runs,
        }
    }

    /// The session's app screen (D65): the loaded `events` (the open
    /// session's) folded in log order, then this device's requests not in
    /// them yet, as the shell's runs are. A `Devices` listing sets the
    /// devices; a new image sets its requesting device's latest capture.
    fn app_screen_view(&self, id: SessionId, events: &[(Seq, Event)]) -> AppScreenView {
        let outbox: HashMap<CommandId, &OutboxEntry> =
            self.outbox.iter().map(|e| (e.command.id, e)).collect();
        let mut screen = AppScreenView::default();
        let find = |requests: &[ScreenRequest], request: CommandId| -> Option<usize> {
            requests.iter().position(|r| r.request == request)
        };
        for (_, event) in events {
            match &event.kind {
                EventKind::ScreenRequested { action, by } => {
                    // The server logs each with its command id, the request's.
                    let Some(request) = event
                        .command_id
                        .filter(|request| find(&screen.requests, *request).is_none())
                    else {
                        continue;
                    };
                    let sent = outbox.get(&request);
                    screen.requests.push(ScreenRequest {
                        request,
                        action: action.clone(),
                        by: DeviceRef {
                            me: self.is_own_message(sent.is_some(), *by),
                            ..self.device_ref(*by)
                        },
                        state: ScreenRequestState::Queued,
                        requested_at: Some(event.at),
                        finished_at: None,
                        outgoing: sent.map(|e| Outgoing::from(&e.status)),
                        discardable: sent.is_some_and(|e| e.discardable()),
                        delivery: sent.and_then(|e| Delivery::of(e)),
                    });
                }
                EventKind::ScreenDone {
                    request, outcome, ..
                } => {
                    let found = find(&screen.requests, *request);
                    if let Some(i) = found {
                        let r = &mut screen.requests[i];
                        r.state = ScreenRequestState::Done(outcome.clone());
                        r.finished_at = Some(event.at);
                    }
                    match outcome {
                        ScreenOutcome::Devices { devices } => screen.devices = devices.clone(),
                        ScreenOutcome::Done {
                            shot: ScreenShot::Image(image),
                        } => {
                            let Some(i) = found else {
                                continue;
                            };
                            let Some(device) = screen.requests[i].action.device() else {
                                continue;
                            };
                            let capture = AppCapture {
                                device: device.to_owned(),
                                capture: *request,
                                format: image.format,
                                width: image.width,
                                height: image.height,
                                screen_width: image.screen_width,
                                screen_height: image.screen_height,
                                content: self.fated_view(&image.image),
                                taken_at: event.at,
                            };
                            match screen
                                .captures
                                .iter()
                                .position(|c| c.device == capture.device)
                            {
                                Some(slot) => screen.captures[slot] = capture,
                                None => screen.captures.push(capture),
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if let Some(me) = self.device() {
            let logged: HashSet<CommandId> = screen.requests.iter().map(|r| r.request).collect();
            screen
                .requests
                .extend(self.outbox.iter().filter_map(|e| match &e.command.kind {
                    CommandKind::Screen { session_id, action }
                        if *session_id == id && !logged.contains(&e.command.id) =>
                    {
                        Some(ScreenRequest {
                            request: e.command.id,
                            action: action.clone(),
                            by: DeviceRef {
                                me: true,
                                ..self.device_ref(me)
                            },
                            state: ScreenRequestState::Pending,
                            requested_at: None,
                            finished_at: None,
                            outgoing: Some(Outgoing::from(&e.status)),
                            discardable: e.discardable(),
                            delivery: Delivery::of(e),
                        })
                    }
                    _ => None,
                }));
        }
        screen
    }

    /// This device's newest outbox command that `is` takes, with its outgoing
    /// state, while the replica does not hold its result yet.
    fn local(&self, is: &dyn Fn(&CommandKind) -> bool) -> Option<(CommandKind, Outgoing)> {
        self.outbox
            .iter()
            .rev()
            .find(|e| is(&e.command.kind))
            .map(|e| (e.command.kind.clone(), Outgoing::from(&e.status)))
            .filter(|(_, outgoing)| !self.in_replica(*outgoing))
    }

    /// The session's agent settings: the summary's fold, with this device's
    /// switches still on their way in front.
    fn agent_view(&self, summary: &SessionSummary) -> AgentView {
        let id = summary.id;
        let agent = &summary.agent;
        let mode_change = match self.local(
            &|kind| matches!(kind, CommandKind::SetPermissionMode { session_id, .. } if *session_id == id),
        ) {
            Some((CommandKind::SetPermissionMode { mode, .. }, outgoing)) => Some(SettingChange {
                value: mode,
                outgoing: Some(outgoing),
            }),
            _ => agent.mode.requested.as_ref().map(|r| SettingChange {
                value: r.value,
                outgoing: None,
            }),
        };
        let model_change = match self.local(
            &|kind| matches!(kind, CommandKind::SetModel { session_id, .. } if *session_id == id),
        ) {
            Some((CommandKind::SetModel { model, .. }, outgoing)) => Some(SettingChange {
                value: model,
                outgoing: Some(outgoing),
            }),
            _ => agent.model.requested.as_ref().map(|r| SettingChange {
                value: r.value.clone(),
                outgoing: None,
            }),
        };
        let effort_change = match self.local(
            &|kind| matches!(kind, CommandKind::SetEffort { session_id, .. } if *session_id == id),
        ) {
            Some((CommandKind::SetEffort { effort, .. }, outgoing)) => Some(SettingChange {
                value: effort,
                outgoing: Some(outgoing),
            }),
            _ => agent.effort.requested_effort().map(|r| SettingChange {
                value: r.value,
                outgoing: None,
            }),
        };
        AgentView {
            mode: agent.mode.current,
            model: agent.model.current.clone(),
            mode_change,
            model_change,
            effort: agent.effort.current,
            effort_change,
            effort_refused: agent.effort.refused_effort(),
            mode_refused: agent.mode.refused.clone(),
            model_refused: agent.model.refused.clone(),
            models: self.models.clone(),
            bypass_allowed: self.settings.server.bypass_allowed,
        }
    }

    /// The safety rules Settings lists: the rules the log holds on, then the
    /// changes this device sent that the log does not hold yet. Only this
    /// device's newest change for given words counts: an earlier one for the
    /// same words is superseded, whatever its state, so once the newest is in
    /// the replica the row has nothing pending. Rows only pending are listed
    /// in outbox order of their newest change. A change is held once its
    /// accepted seq is folded in: a log position, not the reply's arrival.
    fn safety_rule_views(&self) -> Vec<SafetyRuleView> {
        let mut rows: Vec<SafetyRuleView> = self
            .safety_rules
            .iter()
            .map(|rule| SafetyRuleView {
                words: rule.words.clone(),
                session_id: rule.session_id,
                on: rule.on,
                pending: None,
            })
            .collect();
        let mut newest: Vec<&OutboxEntry> = Vec::new();
        for entry in &self.outbox {
            let CommandKind::SetSafetyRule { words, .. } = &entry.command.kind else {
                continue;
            };
            newest.retain(|kept| match &kept.command.kind {
                CommandKind::SetSafetyRule {
                    words: kept_words, ..
                } => kept_words != words,
                _ => true,
            });
            newest.push(entry);
        }
        for entry in newest {
            let CommandKind::SetSafetyRule {
                session_id,
                words,
                on,
            } = &entry.command.kind
            else {
                continue;
            };
            let outgoing = Outgoing::from(&entry.status);
            let pending = (!self.in_replica(outgoing)).then_some(PendingSafetyRule {
                command: entry.command.id,
                on: *on,
                outgoing,
            });
            match rows.iter_mut().find(|row| row.words == *words) {
                Some(row) => row.pending = pending,
                None => rows.push(SafetyRuleView {
                    words: words.clone(),
                    session_id: *session_id,
                    on: false,
                    pending,
                }),
            }
        }
        rows.retain(|row| row.on || row.pending.is_some());
        rows
    }

    /// The mode a new session starts in: this device's default, except bypass
    /// where the server does not allow it (it would refuse the create).
    fn new_session_mode(&self) -> Option<PermissionMode> {
        self.settings
            .default_mode
            .filter(|mode| *mode != PermissionMode::Bypass || self.settings.server.bypass_allowed)
    }

    /// The session's safety check switch still on its way in front: this
    /// device's newest `SetSafetyCheck` for the session, until the replica
    /// holds its result.
    fn safety_check_change(&self, id: SessionId) -> Option<SettingChange<bool>> {
        match self.local(
            &|kind| matches!(kind, CommandKind::SetSafetyCheck { session_id, .. } if *session_id == id),
        ) {
            Some((CommandKind::SetSafetyCheck { on, .. }, outgoing)) => Some(SettingChange {
                value: on,
                outgoing: Some(outgoing),
            }),
            _ => None,
        }
    }

    /// The latest outbox command matching `kind`, until its effect is in the
    /// replica.
    fn latest_outgoing(&self, kind: impl Fn(&CommandKind) -> bool) -> Option<Outgoing> {
        self.outbox
            .iter()
            .rev()
            .find(|e| kind(&e.command.kind))
            .map(|e| Outgoing::from(&e.status))
            .filter(|outgoing| !self.in_replica(*outgoing))
    }

    /// The session's switches on their way, as timeline items, oldest first:
    /// this device's newest clear and switch of the context until their
    /// effect is in the replica, and each mode, model and effort switch of
    /// this device's while it is on its way or its request is open.
    fn pending_changes(
        &self,
        summary: &SessionSummary,
        contexts: &[ContextView],
    ) -> Vec<TimelineItem> {
        let id = summary.id;
        let newest = |is: &dyn Fn(&CommandKind) -> bool| {
            self.outbox
                .iter()
                .rev()
                .find(|e| is(&e.command.kind))
                .map(|e| {
                    (
                        e.command.id,
                        e.command.kind.clone(),
                        Outgoing::from(&e.status),
                        e.discardable(),
                    )
                })
                .filter(|(_, _, outgoing, _)| !self.in_replica(*outgoing))
        };
        let mut changes: Vec<(CommandId, PendingChange, Option<Outgoing>, bool)> = Vec::new();
        if let Some((command_id, CommandKind::ClearContext { name, .. }, outgoing, discardable)) =
            newest(
                &|kind| matches!(kind, CommandKind::ClearContext { session_id, .. } if *session_id == id),
            )
        {
            changes.push((
                command_id,
                PendingChange::NewContext { name },
                Some(outgoing),
                discardable,
            ));
        }
        if let Some((
            command_id,
            CommandKind::SwitchContext { context, .. },
            outgoing,
            discardable,
        )) = newest(
            &|kind| matches!(kind, CommandKind::SwitchContext { session_id, .. } if *session_id == id),
        ) {
            let name = contexts
                .iter()
                .find(|c| c.id == context)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            changes.push((
                command_id,
                PendingChange::SwitchContext { name },
                Some(outgoing),
                discardable,
            ));
        }
        if let Some((
            command_id,
            CommandKind::SetPermissionMode { mode, .. },
            outgoing,
            discardable,
        )) = newest(
            &|kind| matches!(kind, CommandKind::SetPermissionMode { session_id, .. } if *session_id == id),
        ) {
            let setting = AgentSetting::Mode(mode);
            changes.push((
                command_id,
                PendingChange::Setting(setting),
                Some(outgoing),
                discardable,
            ));
        } else if let Some(r) = summary
            .agent
            .mode
            .requested
            .as_ref()
            .filter(|r| self.device() == Some(r.by))
        {
            let setting = AgentSetting::Mode(r.value);
            changes.push((r.command, PendingChange::Setting(setting), None, false));
        }
        if let Some((command_id, CommandKind::SetModel { model, .. }, outgoing, discardable)) =
            newest(
                &|kind| matches!(kind, CommandKind::SetModel { session_id, .. } if *session_id == id),
            )
        {
            let setting = AgentSetting::Model(model);
            changes.push((
                command_id,
                PendingChange::Setting(setting),
                Some(outgoing),
                discardable,
            ));
        } else if let Some(r) = summary
            .agent
            .model
            .requested
            .as_ref()
            .filter(|r| self.device() == Some(r.by))
        {
            let setting = AgentSetting::Model(r.value.clone());
            changes.push((r.command, PendingChange::Setting(setting), None, false));
        }
        if let Some((command_id, CommandKind::SetEffort { effort, .. }, outgoing, discardable)) =
            newest(
                &|kind| matches!(kind, CommandKind::SetEffort { session_id, .. } if *session_id == id),
            )
        {
            let setting = AgentSetting::Effort(Some(effort));
            changes.push((
                command_id,
                PendingChange::Setting(setting),
                Some(outgoing),
                discardable,
            ));
        } else if let Some(r) = summary
            .agent
            .effort
            .requested_effort()
            .filter(|r| self.device() == Some(r.by))
        {
            let setting = AgentSetting::Effort(Some(r.value));
            changes.push((r.command, PendingChange::Setting(setting), None, false));
        }
        changes.sort_by_key(|(command_id, _, _, _)| *command_id);
        changes
            .into_iter()
            .map(
                |(command_id, change, outgoing, discardable)| TimelineItem::Change {
                    command_id,
                    change,
                    outgoing,
                    discardable,
                },
            )
            .collect()
    }

    /// Where a message sent to the session now goes: the context this
    /// device's newest clear or switch of it makes current while that is on
    /// its way (accepted but not yet pulled included), else the replica's.
    /// A failed one changed nothing; once the newest standing one is in the
    /// replica, the replica says what is current, other devices' later
    /// switches included.
    fn target_context(&self, summary: &SessionSummary) -> ContextId {
        for e in self.outbox.iter().rev() {
            let context = match &e.command.kind {
                CommandKind::ClearContext {
                    session_id,
                    context,
                    ..
                }
                | CommandKind::SwitchContext {
                    session_id,
                    context,
                } if *session_id == summary.id => *context,
                _ => continue,
            };
            let outgoing = Outgoing::from(&e.status);
            if matches!(outgoing, Outgoing::Failed(_)) {
                continue;
            }
            if self.in_replica(outgoing) {
                break;
            }
            return context;
        }
        summary.context
    }

    /// The names of the session's contexts as far as this device knows: the
    /// open session's logged ones and every clear of it in the outbox.
    fn context_names(&self, id: SessionId) -> HashSet<String> {
        let logged = self
            .events(id)
            .iter()
            .filter_map(|(_, event)| match &event.kind {
                EventKind::SessionCreated { context_name, .. } => Some(context_name),
                EventKind::ContextStarted { name, .. } => Some(name),
                _ => None,
            });
        let cleared = self.outbox.iter().filter_map(|e| match &e.command.kind {
            CommandKind::ClearContext {
                session_id, name, ..
            } if *session_id == id => Some(name),
            _ => None,
        });
        logged.chain(cleared).cloned().collect()
    }

    /// The session's contexts in log order, every one of them: those in
    /// `events`, then this device's clears on their way. `current` is where
    /// messages go now; a moved one stays listed, never current.
    fn contexts(
        &self,
        id: SessionId,
        events: &[(Seq, Event)],
        current: ContextId,
    ) -> Vec<ContextView> {
        let mut contexts: Vec<ContextView> = Vec::new();
        let new = |id, name: &str| ContextView {
            id,
            name: name.to_owned(),
            first_prompt: None,
            messages: 0,
            last_activity: None,
            current: false,
            outgoing: None,
            moved_to: None,
            discard_move: None,
            can_move: false,
        };
        let mut logged = HashSet::new();
        // Those its agent worked in: a conversation to fork.
        let mut worked = HashSet::new();
        for (_, event) in events {
            match &event.kind {
                EventKind::SessionCreated { context_name, .. } => {
                    contexts.push(new(event.context, context_name));
                }
                EventKind::ContextStarted { name, .. } => contexts.push(new(event.context, name)),
                EventKind::SessionStateChanged(LogState::AgentWorking) => {
                    worked.insert(event.context);
                }
                _ => {}
            }
            let Some(context) = contexts.iter_mut().find(|c| c.id == event.context) else {
                continue;
            };
            if let EventKind::ContextMoved { to, .. } = &event.kind {
                context.moved_to = Some(self.session_ref(*to));
            }
            // When the server logged it: imported history, logged with the
            // session's creation, moves it no further than that.
            context.last_activity = Some(event.at);
            // An adopted conversation's imported prompts are the first
            // context's messages too, so it is labelled like the session.
            let text = match &event.kind {
                EventKind::UserMessage { text, .. } => {
                    logged.extend(event.command_id);
                    Some(text)
                }
                EventKind::Imported {
                    item: ImportedItem::UserMessage { text, .. },
                    ..
                } => Some(text),
                _ => None,
            };
            if let Some(text) = text {
                context.messages += 1;
                if context.first_prompt.is_none() {
                    context.first_prompt = first_line(text, FIRST_PROMPT_MAX_CHARS);
                }
            }
        }
        for e in &self.outbox {
            let outgoing = Outgoing::from(&e.status);
            if matches!(outgoing, Outgoing::Failed(_)) {
                continue;
            }
            match &e.command.kind {
                CommandKind::ClearContext {
                    session_id,
                    context,
                    name,
                } if *session_id == id
                    && !self.in_replica(outgoing)
                    && !contexts.iter().any(|c| c.id == *context) =>
                {
                    contexts.push(new(*context, name));
                }
                CommandKind::SendMessage {
                    session_id,
                    context,
                    text,
                    ..
                } if *session_id == id && !logged.contains(&e.command.id) => {
                    if let Some(context) = contexts.iter_mut().find(|c| c.id == *context) {
                        context.messages += 1;
                        if context.first_prompt.is_none() {
                            context.first_prompt = first_line(text, FIRST_PROMPT_MAX_CHARS);
                        }
                    }
                }
                _ => {}
            }
        }
        let on_its_way = |e: &OutboxEntry| {
            let outgoing = Outgoing::from(&e.status);
            !self.in_replica(outgoing) && !matches!(outgoing, Outgoing::Failed(_))
        };
        for context in &mut contexts {
            context.current = context.id == current;
            context.outgoing = self.latest_outgoing(|kind| match kind {
                CommandKind::ClearContext {
                    session_id,
                    context: c,
                    ..
                }
                | CommandKind::SwitchContext {
                    session_id,
                    context: c,
                }
                | CommandKind::MoveContext {
                    session_id,
                    context: c,
                    ..
                } => *session_id == id && *c == context.id,
                _ => false,
            });
            let blocked =
                self.outbox
                    .iter()
                    .filter(|e| on_its_way(e))
                    .any(|e| match &e.command.kind {
                        CommandKind::MoveContext {
                            session_id,
                            context: c,
                            ..
                        }
                        | CommandKind::SendMessage {
                            session_id,
                            context: c,
                            ..
                        } => *session_id == id && *c == context.id,
                        _ => false,
                    });
            context.can_move =
                worked.contains(&context.id) && context.moved_to.is_none() && !blocked;
            context.discard_move = self
                .outbox
                .iter()
                .rev()
                .find(|e| {
                    matches!(&e.command.kind, CommandKind::MoveContext { session_id, context: c, .. }
                        if *session_id == id && *c == context.id)
                })
                .filter(|e| e.discardable())
                .map(|e| e.command.id);
        }
        contexts
    }

    /// The sessions a search looks through, in the session list's order:
    /// only `scope`'s when it is given.
    pub(crate) fn search_targets(&self, scope: Option<SessionId>) -> Vec<SearchTarget> {
        self.sessions
            .iter()
            .filter(|summary| scope.is_none_or(|id| summary.id == id))
            .map(|summary| {
                let title = self.title(summary);
                SearchTarget {
                    id: summary.id,
                    title: title.text,
                    named: title.named,
                    workdir: summary.workdir.clone(),
                    last_activity: summary.last_activity,
                }
            })
            .collect()
    }

    /// The session's items a search looks through: its logged items outside
    /// any subagent, in log order.
    pub(crate) fn search_timeline(
        &self,
        id: SessionId,
        events: &[(Seq, Event)],
    ) -> Vec<TimelineItem> {
        self.timeline(id, events)
            .0
            .into_iter()
            .filter(|item| matches!(item, TimelineItem::Logged { parent: None, .. }))
            .collect()
    }

    /// The session's timeline and the rules granted in it, in log order.
    fn timeline(
        &self,
        id: SessionId,
        events: &[(Seq, Event)],
    ) -> (Vec<TimelineItem>, Vec<RuleSuggestion>) {
        let outbox: HashMap<CommandId, &OutboxEntry> =
            self.outbox.iter().map(|e| (e.command.id, e)).collect();
        let mut logged = HashSet::new();
        let mut items = Vec::with_capacity(events.len());
        let mut tools = ToolIndex::default();
        // Each imported call's item and time: kept apart from `tools`, so no
        // approval, unanswered list or subagent status reaches them.
        let mut imported_calls: HashMap<CallId, (usize, Option<DateTime<Utc>>)> = HashMap::new();
        // The device behind each mode or model request, by its command.
        let mut asked_by: HashMap<CommandId, DeviceId> = HashMap::new();
        // Each context's name, and the context current at this point.
        let mut names: HashMap<ContextId, &str> = HashMap::new();
        let mut current = None;
        for (seq, event) in events {
            // Whose item it is: a subagent's events name its call; a
            // result or an approval standing alone goes with its call.
            let parent = match &event.kind {
                EventKind::AgentMessage { parent, .. }
                | EventKind::ToolCall(ToolCall { parent, .. }) => *parent,
                EventKind::ToolResult(ToolResult { call, .. })
                | EventKind::ApprovalRequested { call, .. }
                | EventKind::QuestionAsked { call, .. } => tools.parent(&items, *call),
                _ => None,
            };
            tools.step(&event.kind);
            let entry = match &event.kind {
                // The server logs every message with its command id, the
                // entry's key; one without would have none to show under.
                EventKind::UserMessage {
                    device_id,
                    text,
                    attachments,
                } => event.command_id.map(|command_id| {
                    logged.insert(command_id);
                    let sent = outbox.get(&command_id);
                    Entry::UserMessage {
                        command_id,
                        text: text.clone(),
                        images: attachments
                            .iter()
                            .map(|attachment| MessageImage {
                                media_type: attachment.media_type,
                                content: self.fated_view(&attachment.blob),
                            })
                            .collect(),
                        from: DeviceRef {
                            me: self.is_own_message(sent.is_some(), *device_id),
                            ..self.device_ref(*device_id)
                        },
                        outgoing: sent.map(|e| Outgoing::from(&e.status)),
                        delivery: sent.and_then(|e| Delivery::of(e)),
                    }
                }),
                EventKind::AgentMessage { text, full, .. } => Some(Entry::AgentMessage {
                    text: text.clone(),
                    full: full.as_ref().map(|full| self.full_view(full)),
                }),
                EventKind::ToolCall(
                    tool @ ToolCall {
                        call: Some(call), ..
                    },
                ) if !tools.calls.contains_key(call) => {
                    tools.calls.insert(*call, (items.len(), event.at));
                    let running = ToolOutcome::Running { since: event.at };
                    Some(Entry::Tool(Box::new(self.tool_view(*call, tool, running))))
                }
                // No call id (v1), or one already taken.
                EventKind::ToolCall(ToolCall {
                    name,
                    display,
                    input,
                    ..
                }) => Some(Entry::ToolCall {
                    name: name.clone(),
                    summary: display.title.clone(),
                    input: input.as_ref().map(|f| self.full_view(f)),
                }),
                EventKind::ToolResult(result) => {
                    match tools.call(&mut items, result.call) {
                        Some((_, view, called))
                            if !matches!(view.outcome, ToolOutcome::Done { .. }) =>
                        {
                            // A denied or cancelled call's result only
                            // reports that; the outcome already says it.
                            if matches!(
                                view.outcome,
                                ToolOutcome::Running { .. } | ToolOutcome::NoResult
                            ) {
                                view.outcome = ToolOutcome::Done {
                                    is_error: result.is_error,
                                    status: result.status.clone(),
                                    preview: result.preview.clone(),
                                    lines: result.lines,
                                    output: result.output.as_ref().map(|f| self.full_view(f)),
                                    took: (event.at - called).to_std().unwrap_or_default(),
                                };
                            }
                            None
                        }
                        // No call to fold into, or a second result for one.
                        _ => Some(Entry::ToolResult {
                            summary: result.preview.clone(),
                            output: result.output.as_ref().map(|f| self.full_view(f)),
                        }),
                    }
                }
                EventKind::ApprovalRequested {
                    id,
                    call,
                    tool,
                    summary,
                    input,
                    suggestions,
                    shell,
                } => match tools.offer(*id, suggestions).call(&mut items, *call) {
                    Some((item, view, _)) if view.approval.is_none() => {
                        view.approval = Some(ToolApproval {
                            id: *id,
                            question: false,
                            status: ApprovalStatus::Pending,
                            answer: None,
                            input: input.as_ref().map(|f| self.full_view(f)),
                        });
                        if shell.is_some() {
                            view.shell.clone_from(shell);
                            view.safety_feedback = Some(SafetyFeedbackView {
                                judgment: SafetyJudgment::ShouldApprove,
                                sent: None,
                            });
                        }
                        tools.approvals.insert(*id, item);
                        None
                    }
                    _ => {
                        tools.approvals.insert(*id, items.len());
                        Some(Entry::Approval {
                            id: *id,
                            tool: tool.clone(),
                            summary: summary.clone(),
                            input: input.as_ref().map(|f| self.full_view(f)),
                            status: ApprovalStatus::Pending,
                            answer: None,
                        })
                    }
                },
                EventKind::QuestionAsked {
                    id,
                    call,
                    questions,
                } => match tools.call(&mut items, *call) {
                    Some((item, view, _)) if view.approval.is_none() => {
                        view.approval = Some(ToolApproval {
                            id: *id,
                            question: true,
                            status: ApprovalStatus::Pending,
                            answer: None,
                            input: None,
                        });
                        tools.approvals.insert(*id, item);
                        None
                    }
                    _ => {
                        tools.approvals.insert(*id, items.len());
                        Some(Entry::Approval {
                            id: *id,
                            tool: QUESTION_TOOL.to_owned(),
                            summary: questions
                                .first()
                                .map(|q| q.text.clone())
                                .unwrap_or_default(),
                            input: None,
                            status: ApprovalStatus::Pending,
                            answer: None,
                        })
                    }
                },
                EventKind::QuestionAnswered { id, by, .. } => {
                    let status = ApprovalStatus::Answered {
                        by: self.device_ref(*by),
                    };
                    tools.settle(&mut items, *id, status, None);
                    None
                }
                EventKind::ApprovalResolved {
                    id,
                    decision,
                    by,
                    rule,
                } => {
                    let by = self.device_ref(*by);
                    let ends = (*decision == Decision::Deny)
                        .then(|| ToolOutcome::Denied { by: by.clone() });
                    let status = ApprovalStatus::Resolved {
                        decision: *decision,
                        by,
                        rule: tools.grant(*id, *decision, *rule),
                    };
                    tools.settle(&mut items, *id, status, ends);
                    None
                }
                EventKind::ApprovalCancelled { id, reason } => {
                    let status = ApprovalStatus::Cancelled {
                        reason: reason.clone(),
                    };
                    let ends = ToolOutcome::Cancelled {
                        reason: reason.clone(),
                    };
                    tools.settle(&mut items, *id, status, Some(ends));
                    None
                }
                EventKind::TurnInterrupted => Some(Entry::Interrupted),
                EventKind::SessionStopped => Some(Entry::Stopped),
                EventKind::SessionRenamed { name, by } => Some(Entry::Renamed {
                    name: name.clone(),
                    by: self.device_ref(*by),
                }),
                EventKind::SessionArchived { archived, by } => Some(Entry::Archived {
                    archived: *archived,
                    by: self.device_ref(*by),
                }),
                EventKind::WorktreeRemoved { by } => Some(Entry::WorktreeRemoved {
                    by: self.device_ref(*by),
                }),
                // Calls a turn leaves open stay running until the server
                // logs them unanswered.
                EventKind::SessionStateChanged(state) => match state {
                    LogState::Errored { message } => Some(Entry::AgentErrored {
                        message: message.clone(),
                    }),
                    // Shown in the header.
                    LogState::Idle | LogState::AgentWorking => None,
                },
                EventKind::TurnUsage(usage) => Some(Entry::TurnUsage(usage.clone())),
                // Folded into its call's subagent status (`Tools::step`).
                EventKind::SubagentUsage(_) => None,
                // Folded into the session summary; nothing in the timeline.
                EventKind::BackgroundTasksChanged { .. } | EventKind::SchedulesChanged { .. } => {
                    None
                }
                // A stop shows as its task leaving the set; nothing in the
                // timeline.
                EventKind::TaskStopRequested { .. } | EventKind::TaskStopRefused { .. } => None,
                // The server says these calls will never get a result;
                // nothing in the timeline but the calls' own outcome.
                EventKind::CallsUnanswered { calls } => {
                    tools.unanswered(&mut items, calls);
                    None
                }
                // Shell runs are session-wide, not the agent's: they fold
                // into `SessionView::shell` (`Model::shell_view`) instead.
                EventKind::ShellRequested { .. }
                | EventKind::ShellStopRequested { .. }
                | EventKind::ShellStarted { .. }
                | EventKind::ShellOutput { .. }
                | EventKind::ShellFinished { .. }
                | EventKind::ScreenRequested { .. }
                | EventKind::ScreenDone { .. } => None,
                // Nothing in the timeline: what it changes is the content of
                // the event that referenced the blob.
                EventKind::BlobDeleted { .. } | EventKind::BlobCropped { .. } => None,
                EventKind::FileWritten {
                    path,
                    size,
                    replaced,
                    by,
                    ..
                } => Some(Entry::FileWritten {
                    path: path.clone(),
                    size: *size,
                    replaced: replaced.is_some(),
                    by: self.device_ref(*by),
                }),
                EventKind::Imported { at, item } => self
                    .imported(&mut items, &mut imported_calls, *at, item)
                    .map(|entry| Entry::Imported {
                        at: at.unwrap_or(event.at),
                        entry,
                    }),
                EventKind::ImportFinished { outcome } => Some(Entry::ImportFinished {
                    outcome: outcome.clone(),
                }),
                EventKind::PermissionModeRequested { by, .. }
                | EventKind::ModelRequested { by, .. }
                | EventKind::EffortRequested { by, .. } => {
                    if let Some(command) = event.command_id {
                        asked_by.insert(command, *by);
                    }
                    // Shown in the header until answered.
                    None
                }
                EventKind::PermissionModeChanged { mode, answers } => Some(Entry::AgentSetting {
                    setting: AgentSetting::Mode(*mode),
                    by: self.asker(&asked_by, *answers),
                    refused: None,
                }),
                EventKind::PermissionModeRefused {
                    mode,
                    reason,
                    answers,
                } => Some(Entry::AgentSetting {
                    setting: AgentSetting::Mode(*mode),
                    by: self.asker(&asked_by, *answers),
                    refused: Some(reason.clone()),
                }),
                EventKind::ModelChanged { model, answers } => Some(Entry::AgentSetting {
                    setting: AgentSetting::Model(model.clone()),
                    by: self.asker(&asked_by, *answers),
                    refused: None,
                }),
                EventKind::ModelRefused {
                    model,
                    reason,
                    answers,
                } => Some(Entry::AgentSetting {
                    setting: AgentSetting::Model(model.clone()),
                    by: self.asker(&asked_by, *answers),
                    refused: Some(reason.clone()),
                }),
                EventKind::SessionCreated {
                    workdir,
                    context_name,
                    from,
                    ..
                } => {
                    names.insert(event.context, context_name);
                    current = Some(event.context);
                    // Shown in the header, but for where a moved context
                    // came from, and where a fresh session was created.
                    match from {
                        Some(SessionOrigin::Context(from)) => Some(Entry::MovedFrom {
                            from: self.session_ref(from.session),
                            name: context_name.clone(),
                        }),
                        Some(SessionOrigin::Agent { conversation }) => Some(Entry::Adopted {
                            conversation: conversation.clone(),
                        }),
                        None => Some(Entry::Created {
                            workdir: workdir.clone(),
                        }),
                    }
                }
                EventKind::ContextStarted { by, name } => {
                    names.insert(event.context, name);
                    current = Some(event.context);
                    Some(Entry::ContextStarted {
                        name: name.clone(),
                        by: self.device_ref(*by),
                    })
                }
                // Back to where the session already was: nothing changed.
                EventKind::ContextSwitched { .. } if current == Some(event.context) => None,
                EventKind::ContextSwitched { by } => {
                    current = Some(event.context);
                    Some(Entry::ContextSwitched {
                        name: names
                            .get(&event.context)
                            .copied()
                            .unwrap_or_default()
                            .to_owned(),
                        by: self.device_ref(*by),
                    })
                }
                // Shown in the header.
                EventKind::ModelsOffered { .. } => None,
                EventKind::AgentVersionUnconfirmed { version, confirmed } => {
                    Some(Entry::AgentVersionUnconfirmed {
                        version: version.clone(),
                        confirmed: confirmed.clone(),
                    })
                }
                EventKind::ContextMoved { to, by } => Some(Entry::ContextMoved {
                    name: names
                        .get(&event.context)
                        .copied()
                        .unwrap_or_default()
                        .to_owned(),
                    to: self.session_ref(*to),
                    by: self.device_ref(*by),
                }),
                EventKind::EffortChanged { effort, answers } => Some(Entry::AgentSetting {
                    setting: AgentSetting::Effort(*effort),
                    by: self.asker(&asked_by, *answers),
                    refused: None,
                }),
                EventKind::EffortRefused {
                    effort,
                    reason,
                    answers,
                } => Some(Entry::AgentSetting {
                    setting: AgentSetting::Effort(Some(*effort)),
                    by: self.asker(&asked_by, *answers),
                    refused: Some(reason.clone()),
                }),
                // A safety-check approval shows on its call. One without a
                // call, or whose call is not in the timeline, shows nowhere.
                EventKind::SafetyCheckApproved {
                    call,
                    reason,
                    shell,
                    ..
                } => {
                    if let Some((_, view, _)) = tools.call(&mut items, *call) {
                        view.safety_check = Some(reason.clone());
                        if shell.is_some() {
                            view.shell.clone_from(shell);
                            view.safety_feedback = Some(SafetyFeedbackView {
                                judgment: SafetyJudgment::ShouldAsk,
                                sent: None,
                            });
                        }
                    }
                    None
                }
                // The feedback is the newest on its call, from any device. It
                // is logged, so this device's outbox copy no longer shows.
                EventKind::SafetyCheckFeedback { call, note, .. } => {
                    if let Some(command_id) = event.command_id {
                        logged.insert(command_id);
                    }
                    if let Some((_, view, _)) = tools.call(&mut items, Some(*call))
                        && let Some(feedback) = &mut view.safety_feedback
                    {
                        feedback.sent = Some(FeedbackSent {
                            note: note.clone(),
                            outgoing: None,
                        });
                    }
                    None
                }
                EventKind::SafetyCheckSet { .. }
                | EventKind::SafetyRuleSet { .. }
                | EventKind::SessionDeleted { .. }
                | EventKind::Erased => None,
            };
            if let Some(entry) = entry {
                items.push(TimelineItem::Logged {
                    seq: *seq,
                    at: event.at,
                    context: event.context,
                    parent,
                    entry,
                });
            }
        }
        let tasks = self
            .summary(id)
            .map_or(&[][..], |s| s.work.tasks.as_slice());
        tools.subagents(&mut items, tasks);

        // This device's answers, on the items of the approvals they answer:
        // one that came too late stays in sight once its prompt is gone.
        for (&approval, &item) in &tools.approvals {
            let (status, answer) = match items.get_mut(item) {
                Some(TimelineItem::Logged {
                    entry: Entry::Tool(view),
                    ..
                }) => match view.approval.as_mut() {
                    Some(ToolApproval { status, answer, .. }) => (&*status, answer),
                    None => continue,
                },
                Some(TimelineItem::Logged {
                    entry: Entry::Approval { status, answer, .. },
                    ..
                }) => (&*status, answer),
                _ => continue,
            };
            let offered = tools.offered.get(&approval).map_or(&[][..], Vec::as_slice);
            let pending = *status == ApprovalStatus::Pending;
            *answer = self.local_answer(approval, offered, pending);
        }

        // This device's feedback still on its way; outbox order, so the
        // newest one on a call is the one left.
        for e in &self.outbox {
            let CommandKind::SafetyCheckFeedback {
                session_id,
                call,
                note,
                ..
            } = &e.command.kind
            else {
                continue;
            };
            if *session_id != id || logged.contains(&e.command.id) {
                continue;
            }
            if let Some((_, view, _)) = tools.call(&mut items, Some(*call))
                && let Some(feedback) = &mut view.safety_feedback
            {
                feedback.sent = Some(FeedbackSent {
                    note: note.clone(),
                    outgoing: Some(Outgoing::from(&e.status)),
                });
            }
        }

        items.extend(self.outbox.iter().filter_map(|e| {
            match &e.command.kind {
                CommandKind::SendMessage {
                    session_id,
                    context,
                    text,
                    attachments,
                } if *session_id == id && !logged.contains(&e.command.id) => {
                    Some(TimelineItem::Pending {
                        command_id: e.command.id,
                        context: *context,
                        text: text.clone(),
                        images: attachments
                            .iter()
                            .filter_map(|sha256| self.attachments.get(sha256).cloned())
                            .collect(),
                        files: e.files.clone(),
                        outgoing: Outgoing::from(&e.status),
                        discardable: e.discardable(),
                        delivery: Delivery::of(e),
                    })
                }
                _ => None,
            }
        }));
        (items, tools.granted)
    }

    /// The view of the linked call `call` as `tool` logged it, with
    /// `outcome` and nothing folded in yet.
    fn tool_view(&self, call: CallId, tool: &ToolCall, outcome: ToolOutcome) -> ToolView {
        let ToolCall {
            name,
            display,
            input,
            ..
        } = tool;
        ToolView {
            call,
            name: name.clone(),
            kind: display.kind,
            title: display.title.clone(),
            detail: display.detail.clone(),
            body: display.body.as_ref().map(|body| BodyView {
                format: body.format,
                preview: body.preview.clone(),
                lines: body.lines,
                full: body.full.as_ref().map(|f| self.full_view(f)),
            }),
            input: input.as_ref().map(|f| self.full_view(f)),
            approval: None,
            safety_check: None,
            safety_feedback: None,
            shell: None,
            outcome,
            subagent: None,
        }
    }

    /// The timeline entry of one imported `item` the agent recorded `at`
    /// (`None`: it did not say); `None` for a result folded into its
    /// imported call in `items`, found through `calls`. An imported call
    /// starts with no result, never running: nothing will ever answer it.
    /// What a folded call took is known only when the agent timed both the
    /// call and the result: the log's time is another clock, so a pair
    /// missing either one took zero.
    fn imported(
        &self,
        items: &mut [TimelineItem],
        calls: &mut HashMap<CallId, (usize, Option<DateTime<Utc>>)>,
        at: Option<DateTime<Utc>>,
        item: &ImportedItem,
    ) -> Option<ImportedEntry> {
        match item {
            ImportedItem::UserMessage { text, full } => Some(ImportedEntry::UserMessage {
                text: text.clone(),
                full: full.as_ref().map(|full| self.full_view(full)),
            }),
            ImportedItem::AgentMessage { text, full } => Some(ImportedEntry::AgentMessage {
                text: text.clone(),
                full: full.as_ref().map(|full| self.full_view(full)),
            }),
            // The server gives every imported call a fresh id, so neither
            // a missing nor a repeated one occurs; such a call is not shown.
            ImportedItem::ToolCall(tool) => {
                let call = tool.call.filter(|call| !calls.contains_key(call))?;
                calls.insert(call, (items.len(), at));
                let view = self.tool_view(call, tool, ToolOutcome::NoResult);
                Some(ImportedEntry::Tool(Box::new(view)))
            }
            ImportedItem::ToolResult(result) => {
                let call = result.call.and_then(|call| calls.get(&call).copied());
                if let Some((item, called)) = call
                    && let Some(view) = imported_tool_at(items, item)
                    && matches!(view.outcome, ToolOutcome::NoResult)
                {
                    view.outcome = ToolOutcome::Done {
                        is_error: result.is_error,
                        status: result.status.clone(),
                        preview: result.preview.clone(),
                        lines: result.lines,
                        output: result.output.as_ref().map(|f| self.full_view(f)),
                        took: match (called, at) {
                            (Some(called), Some(at)) => (at - called).to_std().unwrap_or_default(),
                            _ => std::time::Duration::ZERO,
                        },
                    };
                    return None;
                }
                Some(ImportedEntry::ToolResult {
                    summary: result.preview.clone(),
                    output: result.output.as_ref().map(|f| self.full_view(f)),
                })
            }
        }
    }

    /// This device's latest answer to an approval, its rule read from what
    /// the approval `offered`. Only an approval still `pending` in the
    /// replica can take another answer.
    fn local_answer(
        &self,
        approval: ApprovalId,
        offered: &[RuleSuggestion],
        pending: bool,
    ) -> Option<Answer> {
        self.outbox.iter().rev().find_map(|e| match e.command.kind {
            CommandKind::ResolveApproval { id, decision, rule } if id == approval => {
                let outgoing = Outgoing::from(&e.status);
                Some(Answer {
                    decision,
                    rule: rule.and_then(|rule| offered.get(usize::from(rule)).cloned()),
                    outgoing,
                    delivery: Delivery::of(e),
                    retryable: pending
                        && matches!(outgoing, Outgoing::Failed(r) if !r.rules_out_answers()),
                })
            }
            CommandKind::AnswerQuestion { id, .. } if id == approval => {
                let outgoing = Outgoing::from(&e.status);
                Some(Answer {
                    decision: Decision::Approve,
                    rule: None,
                    outgoing,
                    delivery: Delivery::of(e),
                    retryable: pending
                        && matches!(outgoing, Outgoing::Failed(r) if !r.rules_out_answers()),
                })
            }
            _ => None,
        })
    }

    /// The answers of this device's latest answer to question `question`;
    /// empty when it gave none, or its latest declined.
    fn local_answers(&self, question: ApprovalId) -> Vec<QuestionAnswer> {
        let latest = self.outbox.iter().rev().find(|e| match &e.command.kind {
            CommandKind::ResolveApproval { id, .. } | CommandKind::AnswerQuestion { id, .. } => {
                *id == question
            }
            _ => false,
        });
        match latest.map(|e| &e.command.kind) {
            Some(CommandKind::AnswerQuestion { answers, .. }) => answers.clone(),
            _ => Vec::new(),
        }
    }

    /// Whether a command's effect has reached the replica.
    fn in_replica(&self, outgoing: Outgoing) -> bool {
        matches!(outgoing, Outgoing::Accepted(seq) if seq <= self.cursor)
    }

    fn summary(&self, id: SessionId) -> Option<&SessionSummary> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// The page of an outbox entry that is a create the replica does not
    /// have yet, failed ones included; `None` for any other command.
    fn pending_session(&self, e: &OutboxEntry) -> Option<PendingSession> {
        let CommandKind::CreateSession {
            session_id,
            workdir,
            conversation,
            worktree,
            ..
        } = &e.command.kind
        else {
            return None;
        };
        let outgoing = Outgoing::from(&e.status);
        (!self.in_replica(outgoing)).then(|| PendingSession {
            command_id: e.command.id,
            session_id: *session_id,
            workdir: workdir.clone(),
            conversation: conversation.clone(),
            worktree: worktree.clone(),
            outgoing,
            discardable: e.discardable(),
        })
    }

    /// The create of session `id` while the replica lacks the session: the
    /// newest one this device queued for it.
    fn creating(&self, id: SessionId) -> Option<PendingSession> {
        if self.summary(id).is_some() {
            return None;
        }
        self.outbox
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    &e.command.kind,
                    CommandKind::CreateSession { session_id, .. } if *session_id == id
                )
            })
            .and_then(|e| self.pending_session(e))
    }

    /// Session `id`'s summary, or while its create is pending, a stand-in
    /// for it ([`SessionSummary::creating`]). What a message, a draft or an
    /// attachment needs to find its session.
    fn session_or_creating(&self, id: SessionId) -> Option<Cow<'_, SessionSummary>> {
        match self.summary(id) {
            Some(summary) => Some(Cow::Borrowed(summary)),
            None => {
                let pending = self.creating(id)?;
                Some(Cow::Owned(SessionSummary::creating(id, pending.workdir)))
            }
        }
    }

    /// This device's latest delete or crop of `blob` until its effect is in
    /// the replica, as [`Model::title`] treats a rename. Accepted but not
    /// yet pulled still shows; a refusal shows until it is discarded.
    fn blob_pending(&self, blob: BlobId) -> Option<BlobPending> {
        self.outbox
            .iter()
            .rev()
            .filter_map(blob_command)
            .find(|(target, _)| *target == blob)
            .map(|(_, pending)| pending)
            .filter(|pending| !self.in_replica(pending.outgoing))
    }

    /// [`Model::blob_pending`] of every blob that has one.
    fn blob_pendings(&self) -> HashMap<BlobId, BlobPending> {
        let mut latest = HashMap::new();
        // Oldest first, so the latest of a blob's commands is kept.
        for (blob, pending) in self.outbox.iter().filter_map(blob_command) {
            latest.insert(blob, pending);
        }
        latest.retain(|_, pending| !self.in_replica(pending.outgoing));
        latest
    }

    fn blob_view(&self, blob: &BlobRef) -> BlobView {
        let pending = self.blob_pending(blob.id);
        let Some(record) = self.blobs.get(&blob.id) else {
            return BlobView {
                id: blob.id,
                state: BlobState::NotDownloaded { size: blob.size },
                change: None,
                pending,
            };
        };
        let size = record.size;
        let state = match &record.status {
            BlobStatus::NotDownloaded | BlobStatus::Stopped { .. } | BlobStatus::Failed { .. }
                if self.wanted.contains(&blob.id) =>
            {
                BlobState::Requested { size }
            }
            // A stopped download is no download under way: nothing moves
            // until it is asked for, which resumes from its kept bytes.
            BlobStatus::NotDownloaded | BlobStatus::Stopped { .. } => {
                BlobState::NotDownloaded { size }
            }
            BlobStatus::Failed { reason } => BlobState::Failed {
                size,
                reason: reason.clone(),
            },
            BlobStatus::Downloading { got, .. } => BlobState::Downloading {
                got: *got,
                size,
                // A full disk first: only the user can end it. Blob traffic
                // needs the server too; while the link is down the download
                // waits for it.
                paused: if self.blobs_disk_full {
                    Some(Pause::DiskFull)
                } else if matches!(
                    self.paired_link(),
                    Some(PairedLink::Online { .. } | PairedLink::CatchingUp { .. })
                ) {
                    None
                } else {
                    Some(Pause::Offline)
                },
            },
            BlobStatus::Available { path } => BlobState::Available { path: path.clone() },
        };
        BlobView {
            id: blob.id,
            state,
            change: None,
            pending,
        }
    }

    fn full_view(&self, full: &Full) -> FullView {
        match full {
            Full::Blob(blob) => self.fated_view(blob),
            Full::Inline(text) => FullView::Inline(text.clone()),
            Full::Lost => FullView::Lost,
        }
    }

    /// What is left of an event's `blob`, following its fates: nothing once
    /// any step was deleted, else the last crop's blob (or `blob` itself).
    /// Every crop names the then-current blob, so the chain is acyclic;
    /// the walk is bounded all the same.
    fn fated_view(&self, blob: &BlobRef) -> FullView {
        let mut current = blob;
        let mut change = None;
        for _ in 0..=self.fates.len() {
            match self.fates.get(&current.id) {
                None => break,
                Some(BlobFate::Deleted { by }) => {
                    return FullView::Deleted {
                        by: self.device_ref(*by),
                    };
                }
                Some(BlobFate::Cropped {
                    into,
                    kept,
                    lines,
                    by,
                }) => {
                    // The first crop's `lines` is the original's count.
                    let of = match change {
                        Some(BlobChange::Cropped { of, .. }) => of,
                        None => *lines,
                    };
                    change = Some(BlobChange::Cropped {
                        kept: kept.iter().map(|span| span.count).sum(),
                        of,
                        by: self.device_ref(*by),
                    });
                    current = into;
                }
            }
        }
        FullView::Blob(BlobView {
            change,
            ..self.blob_view(current)
        })
    }
}

/// The blob an outbox delete or crop names, and how it stands.
fn blob_command(entry: &OutboxEntry) -> Option<(BlobId, BlobPending)> {
    let (blob, kind) = match &entry.command.kind {
        CommandKind::DeleteBlob { blob } => (*blob, BlobCommand::Delete),
        CommandKind::CropBlob { blob, .. } => (*blob, BlobCommand::Crop),
        _ => return None,
    };
    Some((
        blob,
        BlobPending {
            command: entry.command.id,
            kind,
            outgoing: Outgoing::from(&entry.status),
            discardable: entry.discardable(),
        },
    ))
}

/// The read a workdir pane shows: a diff's is the changes'.
fn pane_ask(session: SessionId, pane: &WorkdirPane) -> WorkdirAsk {
    match pane {
        WorkdirPane::Changes | WorkdirPane::Diff { .. } => WorkdirAsk::Changes { session },
        WorkdirPane::Commits => WorkdirAsk::Log { session },
        WorkdirPane::Dir { path } => WorkdirAsk::List {
            session,
            path: path.clone(),
        },
        WorkdirPane::File { path } => WorkdirAsk::File {
            session,
            path: path.clone(),
        },
    }
}

/// The entry of `changes` for `path`, the untracked one or the other: one
/// path may have both.
fn changed_file<'a>(
    changes: &'a ChangesResult,
    path: &str,
    untracked: bool,
) -> Option<&'a ChangedFile> {
    match changes {
        ChangesResult::Changes(set) => set
            .files
            .iter()
            .find(|f| f.path == path && (f.change == FileChange::Untracked) == untracked),
        _ => None,
    }
}

/// Where the linked tool calls sit while a timeline is built, so their
/// approvals and results fold into them.
#[derive(Default)]
struct ToolIndex {
    /// Each linked call's item and the call's time.
    calls: HashMap<CallId, (usize, DateTime<Utc>)>,
    /// Each approval's item: its call's [`Entry::Tool`] or its own
    /// [`Entry::Approval`].
    approvals: HashMap<ApprovalId, usize>,
    /// What each subagent did, by its `Agent` call.
    steps: HashMap<CallId, Steps>,
    /// The rules each approval offers.
    offered: HashMap<ApprovalId, Vec<RuleSuggestion>>,
    /// The rules granted so far, in log order.
    granted: Vec<RuleSuggestion>,
}

impl ToolIndex {
    /// Approval `id` offers `suggestions`.
    fn offer(&mut self, id: ApprovalId, suggestions: &[RuleSuggestion]) -> &mut Self {
        if !suggestions.is_empty() {
            self.offered.insert(id, suggestions.to_vec());
        }
        self
    }

    /// The rule an answer to approval `id` grants, noted as granted: one it
    /// offered, with an approval.
    fn grant(
        &mut self,
        id: ApprovalId,
        decision: Decision,
        rule: Option<u8>,
    ) -> Option<RuleSuggestion> {
        if decision != Decision::Approve {
            return None;
        }
        let rule = self.offered.get(&id)?.get(usize::from(rule?))?.clone();
        self.granted.push(rule.clone());
        Some(rule)
    }

    /// The linked call `call` names: its item, view and time.
    fn call<'a>(
        &self,
        items: &'a mut [TimelineItem],
        call: Option<CallId>,
    ) -> Option<(usize, &'a mut ToolView, DateTime<Utc>)> {
        let &(item, at) = self.calls.get(&call?)?;
        Some((item, tool_at(items, item)?, at))
    }

    /// An approval was answered or cancelled. A linked call still running
    /// `ends` with it (a denial or cancellation); a standalone approval only
    /// takes the status.
    fn settle(
        &self,
        items: &mut [TimelineItem],
        approval: ApprovalId,
        status: ApprovalStatus,
        ends: Option<ToolOutcome>,
    ) {
        let Some(TimelineItem::Logged { entry, .. }) = self
            .approvals
            .get(&approval)
            .and_then(|&item| items.get_mut(item))
        else {
            return;
        };
        match entry {
            Entry::Tool(view) => {
                if let Some(linked) = view.approval.as_mut() {
                    linked.status = status;
                }
                if let Some(end) = ends
                    && matches!(view.outcome, ToolOutcome::Running { .. })
                {
                    view.outcome = end;
                }
            }
            Entry::Approval { status: own, .. } => *own = status,
            _ => {}
        }
    }

    /// The server logged that `calls` will never get a result: each one
    /// still running has none; a settled one is left alone.
    fn unanswered(&self, items: &mut [TimelineItem], calls: &[CallId]) {
        for call in calls {
            if let Some(&(item, _)) = self.calls.get(call)
                && let Some(view) = tool_at(items, item)
                && matches!(view.outcome, ToolOutcome::Running { .. })
            {
                view.outcome = ToolOutcome::NoResult;
            }
        }
    }

    /// The subagent the linked call `call` belongs to, `None` for the main
    /// agent's or a call not linked.
    fn parent(&self, items: &[TimelineItem], call: Option<CallId>) -> Option<CallId> {
        let &(item, _) = self.calls.get(&call?)?;
        match items.get(item)? {
            TimelineItem::Logged { parent, .. } => *parent,
            TimelineItem::Pending { .. } | TimelineItem::Change { .. } => None,
        }
    }

    /// Notes a subagent's tool call or message as what it did last.
    fn step(&mut self, kind: &EventKind) {
        match kind {
            EventKind::ToolCall(ToolCall {
                parent: Some(parent),
                display,
                ..
            }) => {
                let steps = self.steps.entry(*parent).or_default();
                steps.count += 1;
                steps.last = Some(display.title.clone());
            }
            EventKind::AgentMessage {
                parent: Some(parent),
                text,
                ..
            } => {
                if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
                    self.steps.entry(*parent).or_default().last = Some(line.to_owned());
                }
            }
            EventKind::SubagentUsage(usage) => {
                self.steps.entry(usage.call).or_default().usage = Some(usage.clone());
            }
            _ => {}
        }
    }

    /// Each subagent call takes its status: its steps, and whether it is
    /// still at work, by one of `tasks` running it or else (a foreground
    /// one) by its call still running.
    fn subagents(&self, items: &mut [TimelineItem], tasks: &[BackgroundTask]) {
        for (call, &(item, _)) in &self.calls {
            let Some(view) = tool_at(items, item).filter(|v| v.kind == ToolKind::Subagent) else {
                continue;
            };
            let task = tasks
                .iter()
                .find(|t| t.call == Some(*call))
                .map(|t| t.id.clone());
            let steps = self.steps.get(call);
            view.subagent = Some(SubagentStatus {
                live: task.is_some() || matches!(view.outcome, ToolOutcome::Running { .. }),
                task,
                steps: steps.map_or(0, |s| s.count),
                last: steps.and_then(|s| s.last.clone()),
                usage: steps.and_then(|s| s.usage.clone()),
            });
        }
    }
}

/// What a subagent did so far, while a timeline is built.
#[derive(Default)]
struct Steps {
    /// Its tool calls.
    count: u32,
    /// Its newest tool call's title or message's first line.
    last: Option<String>,
    /// What its agent reported once it finished.
    usage: Option<SubagentUsage>,
}

/// The item of the linked call `call` in a folded timeline: the subagent it
/// belongs to and its view.
fn tool_item(items: &[TimelineItem], call: CallId) -> Option<(Option<CallId>, &ToolView)> {
    items.iter().find_map(|item| match item {
        TimelineItem::Logged {
            parent,
            entry: Entry::Tool(view),
            ..
        } if view.call == call => Some((*parent, view.as_ref())),
        _ => None,
    })
}

/// As [`tool_item`], for a subagent's `Agent` call only.
fn subagent_item(items: &[TimelineItem], call: CallId) -> Option<(Option<CallId>, &ToolView)> {
    tool_item(items, call).filter(|(_, view)| view.kind == ToolKind::Subagent)
}

/// The subagent the `Agent` call `call` runs, from a folded timeline: its
/// trail follows the calls it runs under while they are subagents' too.
fn subagent_header(items: &[TimelineItem], call: CallId) -> Option<SubagentHeader> {
    let (mut parent, view) = subagent_item(items, call)?;
    let mut trail = Vec::new();
    let mut seen = HashSet::from([call]);
    while let Some(outer) = parent
        && seen.insert(outer)
        && let Some((up, outer_view)) = subagent_item(items, outer)
    {
        trail.push((outer, outer_view.title.clone()));
        parent = up;
    }
    trail.reverse();
    Some(SubagentHeader {
        call,
        title: view.title.clone(),
        detail: view.detail.clone(),
        prompt: view.body.clone(),
        status: view.subagent.clone()?,
        trail,
    })
}

fn tool_at(items: &mut [TimelineItem], item: usize) -> Option<&mut ToolView> {
    match items.get_mut(item)? {
        TimelineItem::Logged {
            entry: Entry::Tool(view),
            ..
        } => Some(view.as_mut()),
        _ => None,
    }
}

fn imported_tool_at(items: &mut [TimelineItem], item: usize) -> Option<&mut ToolView> {
    match items.get_mut(item)? {
        TimelineItem::Logged {
            entry:
                Entry::Imported {
                    entry: ImportedEntry::Tool(view),
                    ..
                },
            ..
        } => Some(view.as_mut()),
        _ => None,
    }
}

/// How many names [`fresh_context_name`] draws before it numbers one.
const CONTEXT_NAME_ROLLS: usize = 64;

/// A context name not in `used`: the first `roll` draws that is not, else
/// the last draw numbered (" 2", " 3", …) within
/// [`CONTEXT_NAME_MAX_CHARS`](skywave_proto::limits::CONTEXT_NAME_MAX_CHARS).
fn fresh_context_name(used: &HashSet<String>, mut roll: impl FnMut() -> String) -> String {
    let mut name = roll();
    for _ in 1..CONTEXT_NAME_ROLLS {
        if !used.contains(&name) {
            return name;
        }
        name = roll();
    }
    let mut n = 1;
    let mut fresh = name.clone();
    while used.contains(&fresh) {
        n += 1;
        let suffix = format!(" {n}");
        let room = skywave_proto::limits::CONTEXT_NAME_MAX_CHARS - suffix.chars().count();
        let base: String = name.chars().take(room).collect();
        fresh = format!("{}{suffix}", base.trim_end());
    }
    fresh
}

/// Whether `images` fit one message: at most [`ATTACHMENTS_MAX`] of them,
/// and at most [`ATTACHMENTS_MAX_TOTAL_BYTES`] bytes together.
fn within_attachment_caps(images: &[AttachedImage]) -> bool {
    let bytes = images
        .iter()
        .fold(0u64, |sum, image| sum.saturating_add(image.size));
    images.len() <= ATTACHMENTS_MAX && bytes <= ATTACHMENTS_MAX_TOTAL_BYTES
}

/// Whether `name` is a single plain path component the sender may write
/// into the workdir root: not blank, not `.`, `..` or `.git`, and without a
/// separator or NUL.
fn plain_file_name(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | ".." | ".git") && !name.contains(['/', '\\', '\0'])
}

/// The line one attached file adds to a message: where the sender puts it.
fn file_lines(files: &[AttachedFile]) -> String {
    files
        .iter()
        .map(|file| format!("Attached file: ./{}", file.name))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text a message is sent with: `text`, then a blank line and one line
/// per file (the user's text unchanged). Blank text is the lines alone.
fn with_file_lines(text: &str, files: &[AttachedFile]) -> String {
    if files.is_empty() {
        return text.to_owned();
    }
    let lines = file_lines(files);
    if text.trim().is_empty() {
        lines
    } else {
        format!("{text}\n\n{lines}")
    }
}

/// The text a message was written with, taking off the lines
/// [`with_file_lines`] added for `files`. A message with no such lines, or
/// none of its own, keeps its text.
fn without_file_lines(text: &str, files: &[AttachedFile]) -> String {
    if files.is_empty() {
        return text.to_owned();
    }
    let lines = file_lines(files);
    if text == lines {
        return String::new();
    }
    text.strip_suffix(&format!("\n\n{lines}"))
        .map_or_else(|| text.to_owned(), str::to_owned)
}

/// The session a command names, if it names one. An answer names its
/// approval only; its session is pending that approval meanwhile.
fn command_session(kind: &CommandKind) -> Option<SessionId> {
    match kind {
        CommandKind::SendMessage { session_id, .. }
        | CommandKind::Interrupt { session_id }
        | CommandKind::StopSession { session_id }
        | CommandKind::RenameSession { session_id, .. }
        | CommandKind::SetSessionArchived { session_id, .. }
        | CommandKind::DeleteSession { session_id }
        | CommandKind::RemoveWorktree { session_id }
        | CommandKind::SetPermissionMode { session_id, .. }
        | CommandKind::SetModel { session_id, .. }
        | CommandKind::SetEffort { session_id, .. }
        | CommandKind::SetSafetyCheck { session_id, .. }
        | CommandKind::SetSafetyRule { session_id, .. }
        | CommandKind::SafetyCheckFeedback { session_id, .. }
        | CommandKind::ClearContext { session_id, .. }
        | CommandKind::SwitchContext { session_id, .. }
        | CommandKind::MoveContext { session_id, .. }
        | CommandKind::StopTask { session_id, .. }
        | CommandKind::RunShell { session_id, .. }
        | CommandKind::StopShell { session_id, .. }
        | CommandKind::Screen { session_id, .. } => Some(*session_id),
        CommandKind::CreateSession { .. }
        | CommandKind::ResolveApproval { .. }
        | CommandKind::AnswerQuestion { .. }
        | CommandKind::DeleteBlob { .. }
        | CommandKind::CropBlob { .. } => None,
    }
}

/// A pending approval overrides the logged agent state.
/// Whether this device's interrupt is still worth showing. A refusal
/// for want of a turn means the agent was already not working, as asked;
/// any other failure stops mattering once the agent is not working.
fn interrupt_matters(outgoing: Outgoing, state: &SessionState) -> bool {
    match outgoing {
        Outgoing::Failed(FailReason::Rejected(RejectReason::NoActiveTurn)) => false,
        Outgoing::Failed(_) => matches!(
            state,
            SessionState::AgentWorking | SessionState::AwaitingApproval(_)
        ),
        _ => true,
    }
}

fn session_state(summary: &SessionSummary) -> SessionState {
    if let Some(id) = summary.pending_approval() {
        return SessionState::AwaitingApproval(id);
    }
    match &summary.state {
        // A turn still ending after the stop shows as working; an error wins.
        LogState::Idle if summary.stopped => SessionState::Stopped,
        LogState::Idle if !summary.work.tasks.is_empty() || !summary.work.schedules.is_empty() => {
            SessionState::BackgroundWork
        }
        LogState::Idle => SessionState::Idle,
        LogState::AgentWorking => SessionState::AgentWorking,
        LogState::Errored { message } => SessionState::Errored(ErrorInfo {
            message: message.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use chrono::{TimeDelta, TimeZone, Utc};
    use skywave_proto::{
        Attachment, BlobRef, Decision, Effort, ImageType, ImportOutcome, RejectReason, RuleScope,
        ToolDisplay, ToolKind, ToolResult,
    };

    use super::*;
    use crate::view::{
        CandidateSource, Completion, CompletionStatus, FailReason, LinkRejection, Refusal,
        WorktreesStatus, WorktreesUnavailable,
    };

    /// Paired as `device` at `srv:7443` with a server CA whose DER is
    /// `b"ca"`.
    fn paired_as(device: DeviceId) -> Paired {
        paired_at(device, "srv:7443")
    }

    /// [`paired_as`] at `address`.
    fn paired_at(device: DeviceId, address: &str) -> Paired {
        Paired {
            device,
            server: Some(Fingerprint::of_der(b"ca")),
            addresses: ServerAddresses::new(addr(address)),
            link: PairedLink::Connecting,
        }
    }

    fn addr(text: &str) -> ServerAddress {
        text.parse().unwrap()
    }

    fn addrs(texts: &[&str]) -> Vec<ServerAddress> {
        texts.iter().map(|text| addr(text)).collect()
    }

    /// A paired model over an empty replica, plus a log builder.
    struct Fixture {
        model: Model,
        me: DeviceId,
        events: Vec<Event>,
    }

    impl Fixture {
        fn paired() -> Fixture {
            let me = DeviceId::new();
            Fixture {
                model: Model::new(empty_snapshot(), Some(paired_as(me))),
                me,
                events: Vec::new(),
            }
        }

        /// Appends events to the log and delivers them as one batch; each
        /// one a command logs names a fresh command.
        fn log(&mut self, session_id: SessionId, kinds: Vec<EventKind>) {
            let events = kinds
                .into_iter()
                .map(|kind| (kind.logged_by_command().then(CommandId::new), kind))
                .collect();
            self.log_commands(session_id, events);
        }

        /// Appends events, each logged by the command named with it, and
        /// delivers them as one batch.
        fn log_commands(
            &mut self,
            session_id: SessionId,
            kinds: Vec<(Option<CommandId>, EventKind)>,
        ) {
            let first = ContextId::first_of(session_id);
            let events = kinds
                .into_iter()
                .map(|(command_id, kind)| (command_id, first, kind))
                .collect();
            self.log_stamped(session_id, events);
        }

        /// Appends events of `context`, as [`Fixture::log`] does.
        fn log_in(&mut self, session_id: SessionId, context: ContextId, kinds: Vec<EventKind>) {
            let events = kinds
                .into_iter()
                .map(|kind| (kind.logged_by_command().then(CommandId::new), context, kind))
                .collect();
            self.log_stamped(session_id, events);
        }

        /// Appends events, each logged by the command and in the context
        /// named with it, and delivers them as one batch.
        fn log_stamped(
            &mut self,
            session_id: SessionId,
            kinds: Vec<(Option<CommandId>, ContextId, EventKind)>,
        ) {
            let from = Seq(self.events.len() as u64 + 1);
            let events: Vec<Event> = kinds
                .into_iter()
                .map(|(command_id, context, kind)| {
                    let minute = (self.events.len() % 60) as u32;
                    let event = Event {
                        session_id,
                        at: Utc.with_ymd_and_hms(2026, 10, 29, 10, minute, 0).unwrap(),
                        command_id,
                        context,
                        kind,
                    };
                    self.events.push(event.clone());
                    event
                })
                .collect();
            self.model.apply_batch(&EventBatch { from, events });
        }

        fn session(&mut self, workdir: &str) -> SessionId {
            let id = SessionId::new();
            self.log(
                id,
                vec![EventKind::SessionCreated {
                    workdir: workdir.into(),
                    context_name: "start".into(),
                    from: None,
                    worktree: None,
                }],
            );
            id
        }

        /// Opens a session the way the core does: dispatch, then serve the
        /// `LoadSession` effect from the log.
        fn open(&mut self, id: SessionId) {
            let effects = self.model.dispatch(Action::OpenSession(id));
            assert_eq!(effects, vec![Effect::LoadSession(id)]);
            let events = self
                .events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.session_id == id)
                .map(|(i, e)| (Seq(i as u64 + 1), e.clone()))
                .collect();
            self.model.session_loaded(id, events);
        }

        /// Serves the `LoadApprovals` effect for session `id` from the log,
        /// as [`Fixture::open`] serves `LoadSession`.
        fn load_approvals(&mut self, id: SessionId) {
            let events = self
                .events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.session_id == id)
                .map(|(i, e)| (Seq(i as u64 + 1), e.clone()))
                .collect();
            self.model.approvals_loaded(id, events);
        }

        fn dispatch(&mut self, action: Action) -> Vec<Effect> {
            self.model.dispatch(action)
        }

        fn session_view(&self) -> SessionView {
            match self.model.view().screen {
                Screen::Session(view) => *view,
                other => panic!("expected the session screen, got {other:?}"),
            }
        }

        /// The active list, leaving the open session.
        fn list_view_after_back(&mut self) -> SessionListView {
            self.dispatch(Action::Back);
            self.list_view()
        }

        fn list_view(&self) -> SessionListView {
            match self.model.view().screen {
                Screen::SessionList(view) => view,
                other => panic!("expected the session list, got {other:?}"),
            }
        }
    }

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            cursor: Seq::ZERO,
            sessions: Vec::new(),
            outbox: Vec::new(),
            blobs: Vec::new(),
            fates: HashMap::new(),
            pending_crops: Vec::new(),
            settings: Settings::default(),
            drafts: HashMap::new(),
            draft_images: HashMap::new(),
            draft_files: HashMap::new(),
            attachments: HashMap::new(),
            composer_modes: HashMap::new(),
            read_marks: HashMap::new(),
            deleted: HashSet::new(),
            alerted: HashSet::new(),
            devices: HashMap::new(),
            models: Vec::new(),
            safety_rules: Vec::new(),
            snippets: Vec::new(),
            shell_history: Vec::new(),
            turns: Vec::new(),
            workdirs: HashMap::new(),
            usage_limits: None,
        }
    }

    /// The single command an action queued.
    fn queued(effects: &[Effect]) -> Command {
        match effects {
            [
                Effect::Enqueue(command, _),
                Effect::Request(Request::OutboxChanged),
            ] => command.clone(),
            other => panic!("expected enqueue + outbox wake-up, got {other:?}"),
        }
    }

    /// The session id a queued create names.
    fn created_id(command: &Command) -> SessionId {
        match &command.kind {
            CommandKind::CreateSession { session_id, .. } => *session_id,
            other => panic!("not a create: {other:?}"),
        }
    }

    #[test]
    fn unpaired_shows_pairing_and_tracks_progress() {
        let mut model = Model::new(empty_snapshot(), None);
        let pairing = |model: &Model| match model.view().screen {
            Screen::Pairing(view) => view,
            other => panic!("expected pairing, got {other:?}"),
        };
        assert_eq!(pairing(&model).status, PairingStatus::Idle);
        assert_eq!(pairing(&model).paired, None);
        // No server to connect to, so nothing claims to be connecting.
        assert_eq!(model.view().link, Link::Unpaired);
        assert!(model.dispatch(Action::RetryNow).is_empty());

        // Navigation and commands go nowhere without an identity.
        model.dispatch(Action::Navigate(Route::Settings));
        assert!(matches!(model.view().screen, Screen::Pairing(_)));
        assert!(
            model
                .dispatch(Action::CreateSession {
                    conversation: None,
                    workdir: "/srv/x".into(),
                    worktree: None,
                })
                .is_empty()
        );

        let incomplete = model.dispatch(Action::Pair {
            address: "host:7443".into(),
            code: " ".into(),
            fingerprint: "ab:cd".into(),
        });
        assert!(incomplete.is_empty());
        assert!(matches!(
            pairing(&model).status,
            PairingStatus::Failed { .. }
        ));
        // An address that does not parse fails here, before any request.
        let unparseable = model.dispatch(Action::Pair {
            address: " h:p ".into(),
            code: "123456".into(),
            fingerprint: "ab:cd".into(),
        });
        assert!(unparseable.is_empty());
        assert!(matches!(
            &pairing(&model).status,
            PairingStatus::Failed { address, reason }
                if address == "h:p" && reason.contains("invalid server address")
        ));

        let pair = Action::Pair {
            address: " host:7443 ".into(),
            code: "123456".into(),
            fingerprint: "ab:cd".into(),
        };
        let effects = model.dispatch(pair.clone());
        assert_eq!(
            effects,
            vec![Effect::Request(Request::Pair {
                address: addr("host:7443"),
                code: "123456".into(),
                fingerprint: "ab:cd".into(),
            })]
        );
        let in_progress = PairingStatus::InProgress {
            address: addr("host:7443"),
        };
        assert_eq!(pairing(&model).status, in_progress);
        // A second attempt while one is running is ignored.
        assert!(model.dispatch(pair.clone()).is_empty());

        model.pairing_failed("bad code".into());
        assert_eq!(
            pairing(&model).status,
            PairingStatus::Failed {
                address: "host:7443".into(),
                reason: "bad code".into(),
            }
        );

        assert_eq!(model.dispatch(pair).len(), 1);
        let device = DeviceId::new();
        model.paired(paired_at(device, "host:7443"));
        let view = model.view();
        assert_eq!(view.link, Link::Connecting);
        assert_eq!(
            view.screen,
            Screen::SessionList(SessionListView {
                kind: ListKind::Active,
                sessions: Vec::new(),
                other_count: 0,
                creating: Vec::new(),
                completion: None,
                conversations: None,
            })
        );

        // Re-pair screen shows the current pairing.
        model.dispatch(Action::Navigate(Route::Pairing));
        assert_eq!(
            pairing(&model).paired,
            Some(PairedInfo {
                device_id: device,
                server_address: addr("host:7443"),
            })
        );
    }

    #[test]
    fn a_message_is_written_in_the_current_context() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let send = |f: &mut Fixture| {
            queued(&f.dispatch(Action::SendMessage {
                session_id: sid,
                text: "hello".into(),
            }))
            .kind
        };
        let in_context = |context| CommandKind::SendMessage {
            session_id: sid,
            context,
            text: "hello".into(),
            attachments: Vec::new(),
        };
        assert_eq!(
            send(&mut f),
            in_context(skywave_proto::ContextId::first_of(sid))
        );

        // Another device cleared the context: the next message goes there.
        let fresh = skywave_proto::ContextId::new();
        let started = Event {
            session_id: sid,
            at: Utc::now(),
            command_id: Some(CommandId::new()),
            context: fresh,
            kind: EventKind::ContextStarted {
                by: skywave_proto::DeviceId::new(),
                name: "fresh start".into(),
            },
        };
        let from = Seq(f.events.len() as u64 + 1);
        f.events.push(started.clone());
        f.model.apply_batch(&EventBatch {
            from,
            events: vec![started],
        });
        assert_eq!(send(&mut f), in_context(fresh));
    }

    #[test]
    fn own_message_moves_through_outgoing_states_and_appears_once() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);

        assert!(
            f.dispatch(Action::SendMessage {
                session_id: sid,
                text: "  ".into()
            })
            .is_empty()
        );
        let command = queued(&f.dispatch(Action::SendMessage {
            session_id: sid,
            text: "hello".into(),
        }));
        assert_eq!(
            command.kind,
            CommandKind::SendMessage {
                session_id: sid,
                context: skywave_proto::ContextId::first_of(sid),
                text: "hello".into(),
                attachments: Vec::new(),
            }
        );
        // Queued: the time comes from the command's UUIDv7 id.
        let queued_at = match &f.session_view().timeline[..] {
            [
                TimelineItem::Logged {
                    entry: Entry::Created { .. },
                    ..
                },
                TimelineItem::Pending {
                    delivery: Some(delivery),
                    ..
                },
            ] => delivery.queued_at,
            other => panic!("expected one pending message, got {other:?}"),
        };
        let since = Utc::now() - queued_at;
        assert!(since >= TimeDelta::zero() && since < TimeDelta::seconds(5));
        let pending = |outgoing, delivery| {
            vec![TimelineItem::Pending {
                command_id: command.id,
                context: skywave_proto::ContextId::first_of(sid),
                text: "hello".into(),
                images: Vec::new(),
                files: Vec::new(),
                outgoing,
                discardable: matches!(outgoing, Outgoing::Queued | Outgoing::Sending),
                delivery: Some(delivery),
            }]
        };
        let fresh = Delivery {
            queued_at,
            first_sent_at: None,
            answered_at: None,
            tries: 0,
        };
        assert_eq!(
            f.session_view().timeline[1..],
            pending(Outgoing::Queued, fresh)
        );
        assert_eq!(f.model.view().queued, 1);

        let sent = queued_at + TimeDelta::milliseconds(3200);
        f.model.command_sending(command.id, sent);
        let sending = Delivery {
            first_sent_at: Some(sent),
            tries: 1,
            ..fresh
        };
        assert_eq!(
            f.session_view().timeline[1..],
            pending(Outgoing::Sending, sending)
        );

        // A resend keeps the first send's time.
        f.model
            .command_sending(command.id, sent + TimeDelta::seconds(2));
        let resent = Delivery {
            tries: 2,
            ..sending
        };
        assert_eq!(
            f.session_view().timeline[1..],
            pending(Outgoing::Sending, resent)
        );

        let answered = queued_at + TimeDelta::milliseconds(8100);
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(2) },
            answered,
        );
        let done = Delivery {
            answered_at: Some(answered),
            ..resent
        };
        assert_eq!(
            f.session_view().timeline[1..],
            pending(Outgoing::Accepted(Seq(2)), done)
        );
        assert_eq!(f.model.view().queued, 0);

        let me = f.me;
        f.log_commands(
            sid,
            vec![
                (
                    Some(command.id),
                    EventKind::UserMessage {
                        device_id: me,
                        text: "hello".into(),
                        attachments: Vec::new(),
                    },
                ),
                (None, EventKind::SessionStateChanged(LogState::AgentWorking)),
                (
                    None,
                    EventKind::AgentMessage {
                        parent: None,
                        text: "hi".into(),
                        full: None,
                    },
                ),
            ],
        );
        let view = f.session_view();
        assert_eq!(view.state, SessionState::AgentWorking);
        let entries: Vec<&Entry> = view
            .timeline
            .iter()
            .map(|item| match item {
                TimelineItem::Logged { entry, .. } => entry,
                TimelineItem::Pending { .. } => panic!("message shown twice: {item:?}"),
                TimelineItem::Change { .. } => panic!("unexpected switch on its way: {item:?}"),
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                &Entry::Created {
                    workdir: "/srv/a".into(),
                },
                &Entry::UserMessage {
                    command_id: command.id,
                    text: "hello".into(),
                    images: Vec::new(),
                    from: DeviceRef {
                        id: me,
                        name: None,
                        me: true,
                    },
                    outgoing: Some(Outgoing::Accepted(Seq(2))),
                    delivery: Some(done),
                },
                &Entry::AgentMessage {
                    text: "hi".into(),
                    full: None,
                },
            ]
        );
    }

    #[test]
    fn rejected_command_shows_failed_and_other_devices_messages_are_not_own() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = queued(&f.dispatch(Action::SendMessage {
            session_id: sid,
            text: "x".into(),
        }));
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        // A later result never overrides the first.
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(9) },
            Utc::now(),
        );
        let phone = CommandId::new();
        f.log_commands(
            sid,
            vec![(
                Some(phone),
                EventKind::UserMessage {
                    device_id: DeviceId::new(),
                    text: "from the phone".into(),
                    attachments: Vec::new(),
                },
            )],
        );
        let timeline = f.session_view().timeline;
        assert!(matches!(
            &timeline[1],
            TimelineItem::Logged {
                entry: Entry::UserMessage {
                    command_id,
                    from: DeviceRef { me: false, .. },
                    outgoing: None,
                    delivery: None,
                    ..
                },
                ..
            } if *command_id == phone
        ));
        assert!(matches!(
            &timeline[2],
            TimelineItem::Pending {
                command_id,
                text,
                context: _,
                outgoing: Outgoing::Failed(FailReason::Rejected(RejectReason::UnknownSession)),
                discardable: true,
                delivery: Some(Delivery {
                    first_sent_at: None,
                    answered_at: Some(_),
                    tries: 0,
                    ..
                }),
                ..
            } if *command_id == command.id && text == "x"
        ));
    }

    #[test]
    fn question_prompt_takes_answers_that_fit_or_a_denial() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let question = ApprovalId::new();
        let call = CallId::new();
        let option = |label: &str| skywave_proto::QuestionOption {
            label: label.into(),
            description: format!("{label} bread"),
            preview: None,
        };
        let questions = vec![Question {
            header: "Bread".into(),
            text: "Which bread?".into(),
            options: vec![option("Rye"), option("Spelt")],
            multi_select: false,
        }];
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(call), ToolKind::Other, "Which bread?"),
                EventKind::QuestionAsked {
                    id: question,
                    call: Some(call),
                    questions: questions.clone(),
                },
            ],
        );
        f.open(sid);
        let view = f.session_view();
        assert_eq!(view.state, SessionState::AwaitingApproval(question));
        assert_eq!(view.pending_approval, None);
        assert_eq!(
            view.pending_question,
            Some(QuestionPrompt {
                id: question,
                call: Some(call),
                from: None,
                questions: questions.clone(),
                answer: None,
                answers: Vec::new(),
            })
        );
        let Some(Entry::Tool(tool)) = entries(&f).into_iter().nth(1) else {
            panic!("no tool entry");
        };
        let approval = tool.approval.unwrap();
        assert!(approval.question);
        assert_eq!(approval.status, ApprovalStatus::Pending);

        // A question takes no plain approval, nor answers that do not fit.
        let approve = Action::ResolveApproval {
            id: question,
            decision: Decision::Approve,
            rule: None,
        };
        assert!(f.dispatch(approve).is_empty());
        let answer = |picked: Vec<u8>| skywave_proto::QuestionAnswer {
            picked,
            other: None,
        };
        for answers in [vec![], vec![answer(vec![0, 1])], vec![answer(vec![3])]] {
            assert!(
                f.dispatch(Action::AnswerQuestion {
                    id: question,
                    answers,
                })
                .is_empty()
            );
        }
        let answered = Action::AnswerQuestion {
            id: question,
            answers: vec![answer(vec![1])],
        };
        let command = queued(&f.dispatch(answered.clone()));
        assert_eq!(
            command.kind,
            CommandKind::AnswerQuestion {
                id: question,
                answers: vec![answer(vec![1])],
            }
        );
        // Answered once; a denial no longer applies either.
        assert!(f.dispatch(answered).is_empty());
        let deny = Action::ResolveApproval {
            id: question,
            decision: Decision::Deny,
            rule: None,
        };
        assert!(f.dispatch(deny.clone()).is_empty());
        let prompt = f.session_view().pending_question.unwrap();
        let mine = prompt.answer.unwrap();
        assert_eq!(mine.decision, Decision::Approve);
        assert_eq!(prompt.answers, [answer(vec![1])]);
        assert_eq!(mine.outgoing, Outgoing::Queued);

        // A refusal of the answer itself leaves the question open.
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::BadAnswer,
            },
            Utc::now(),
        );
        assert!(
            f.session_view()
                .pending_question
                .unwrap()
                .answer
                .unwrap()
                .retryable
        );
        queued(&f.dispatch(deny));

        let phone = DeviceId::new();
        f.log(
            sid,
            vec![
                EventKind::QuestionAnswered {
                    id: question,
                    by: phone,
                    answers: vec![answer(vec![0])],
                },
                tool_result(Some(call), false, "Which bread? → Rye"),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        let view = f.session_view();
        assert_eq!(view.pending_question, None);
        assert_eq!(view.state, SessionState::Idle);
        let Some(Entry::Tool(tool)) = entries(&f).into_iter().nth(1) else {
            panic!("no tool entry");
        };
        assert!(matches!(
            tool.approval.unwrap().status,
            ApprovalStatus::Answered { by } if by.id == phone
        ));
        assert!(matches!(
            tool.outcome,
            ToolOutcome::Done {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn a_question_with_no_logged_call_stands_alone() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let question = ApprovalId::new();
        f.log(
            sid,
            vec![EventKind::QuestionAsked {
                id: question,
                call: None,
                questions: vec![Question {
                    header: String::new(),
                    text: "Which bread?".into(),
                    options: vec![skywave_proto::QuestionOption {
                        label: "Rye".into(),
                        description: String::new(),
                        preview: None,
                    }],
                    multi_select: true,
                }],
            }],
        );
        assert_eq!(f.list_view().sessions[0].pending_approvals, 1);
        f.open(sid);
        assert_eq!(
            entries(&f),
            [
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::Approval {
                    id: question,
                    tool: QUESTION_TOOL.into(),
                    summary: "Which bread?".into(),
                    input: None,
                    status: ApprovalStatus::Pending,
                    answer: None,
                }
            ]
        );
    }

    #[test]
    fn the_approvals_screen_loads_each_waiting_session_once_oldest_first() {
        let mut f = Fixture::paired();
        let mut ids = [ApprovalId::new(), ApprovalId::new(), ApprovalId::new()];
        ids.sort();
        let [older, middle, newest] = ids;
        let a = f.session("/srv/a");
        let b = f.session("/srv/b");
        let (agent, gated) = (CallId::new(), CallId::new());
        f.log(
            a,
            vec![
                tool_call(Some(agent), ToolKind::Subagent, "explore"),
                sub_call(Some(gated), agent, ToolKind::Shell, "git status"),
                EventKind::ApprovalRequested {
                    id: newest,
                    call: Some(gated),
                    tool: "bash".into(),
                    summary: "git status".into(),
                    input: None,
                    suggestions: Vec::new(),
                    shell: Some("git status".into()),
                },
            ],
        );
        f.log(
            b,
            vec![
                approval_requested(older, None),
                approval_requested(middle, None),
            ],
        );

        assert_eq!(
            f.dispatch(Action::Navigate(Route::Approvals)),
            [Effect::LoadApprovals(b), Effect::LoadApprovals(a)]
        );
        f.load_approvals(b);
        f.load_approvals(a);
        let view = approvals_view(&f);
        let listed: Vec<_> = view
            .items
            .iter()
            .map(|item| (item.id, item.session_id))
            .collect();
        assert_eq!(listed, [(older, b), (middle, b), (newest, a)]);
        match &view.items[2].asks {
            Asks::Approval(prompt) => {
                assert_eq!(prompt.from.as_deref(), Some("explore"));
                assert_eq!(prompt.shell.as_deref(), Some("git status"));
            }
            other => panic!("expected an approval, got {other:?}"),
        }
        // Each session was read once: nothing is left to load.
        assert!(f.model.approvals_to_load().is_empty());
        assert!(f.dispatch(Action::Navigate(Route::Approvals)).is_empty());
    }

    #[test]
    fn an_approval_arriving_on_the_approvals_screen_is_loaded_and_leaves_once_answered() {
        let mut f = Fixture::paired();
        assert!(f.dispatch(Action::Navigate(Route::Approvals)).is_empty());
        assert!(approvals_view(&f).items.is_empty());

        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        f.log(sid, vec![approval_requested(approval, None)]);
        assert_eq!(f.model.approvals_to_load(), [sid]);
        f.load_approvals(sid);
        let view = approvals_view(&f);
        assert!(matches!(
            view.items.as_slice(),
            [WaitingApproval { asks: Asks::Approval(prompt), .. }] if prompt.id == approval
        ));

        f.dispatch(Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        });
        assert!(approvals_view(&f).items.is_empty());
    }

    #[test]
    fn always_allow_on_the_approvals_screen_queues_its_rule() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        let rule = RuleSuggestion {
            label: "bash(ls)".into(),
            scope: RuleScope::Project,
        };
        f.log(
            sid,
            vec![EventKind::ApprovalRequested {
                id: approval,
                call: None,
                tool: "bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: vec![rule],
                shell: None,
            }],
        );
        assert_eq!(
            f.dispatch(Action::Navigate(Route::Approvals)),
            [Effect::LoadApprovals(sid)]
        );
        f.load_approvals(sid);

        // No session is open here, so the rule is read from the cached facts.
        // One out of range is still not sent.
        assert!(
            f.dispatch(Action::ResolveApproval {
                id: approval,
                decision: Decision::Approve,
                rule: Some(1),
            })
            .is_empty()
        );
        let command = queued(&f.dispatch(Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: Some(0),
        }));
        assert_eq!(
            command.kind,
            CommandKind::ResolveApproval {
                id: approval,
                decision: Decision::Approve,
                rule: Some(0),
            }
        );
        assert!(approvals_view(&f).items.is_empty());
    }

    #[test]
    fn an_answered_approval_s_facts_load_when_it_waits_again() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        f.log(sid, vec![approval_requested(approval, None)]);
        // Answered from this device before the screen is shown: the session
        // has no waiting approval, so only its pending ones can bring it in.
        let command = queued(&f.dispatch(Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        }));
        assert_eq!(
            f.dispatch(Action::Navigate(Route::Approvals)),
            [Effect::LoadApprovals(sid)]
        );
        f.load_approvals(sid);
        assert!(f.model.waiting_facts.contains_key(&approval));
        assert!(approvals_view(&f).items.is_empty());

        // A refused answer that may be retried makes it waiting again, with
        // no batch in between: its facts are already cached.
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownRule,
            },
            Utc::now(),
        );
        assert!(matches!(
            approvals_view(&f).items.as_slice(),
            [WaitingApproval { id, asks: Asks::Approval(prompt), .. }]
                if *id == approval && prompt.id == approval
        ));
    }

    #[test]
    fn a_question_waits_as_a_question() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let question = ApprovalId::new();
        f.log(
            sid,
            vec![EventKind::QuestionAsked {
                id: question,
                call: None,
                questions: vec![Question {
                    header: String::new(),
                    text: "Which bread?".into(),
                    options: Vec::new(),
                    multi_select: false,
                }],
            }],
        );
        assert_eq!(
            f.dispatch(Action::Navigate(Route::Approvals)),
            [Effect::LoadApprovals(sid)]
        );
        f.load_approvals(sid);
        let view = approvals_view(&f);
        assert!(matches!(
            view.items.as_slice(),
            [WaitingApproval { id, asks: Asks::Question, .. }] if *id == question
        ));
    }

    #[test]
    fn approvals_load_only_on_the_approvals_screen() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log(sid, vec![approval_requested(ApprovalId::new(), None)]);
        assert!(
            f.model.approvals_to_load().is_empty(),
            "on the session list"
        );
        assert!(f.dispatch(Action::Navigate(Route::SessionList)).is_empty());
        // The control: the same approval loads once the screen is shown.
        assert_eq!(
            f.dispatch(Action::Navigate(Route::Approvals)),
            [Effect::LoadApprovals(sid)]
        );
    }

    #[test]
    fn approval_prompt_answer_and_resolution_elsewhere() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        let input = BlobRef {
            id: BlobId::new(),
            size: 5000,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                EventKind::ApprovalRequested {
                    id: approval,
                    call: None,
                    tool: "bash".into(),
                    summary: "rm -rf build".into(),
                    input: Some(Full::Blob(input.clone())),
                    suggestions: Vec::new(),
                    shell: None,
                },
            ],
        );
        assert_eq!(
            f.list_view().sessions[0].state,
            SessionState::AwaitingApproval(approval)
        );
        f.open(sid);
        let view = f.session_view();
        assert_eq!(view.state, SessionState::AwaitingApproval(approval));
        let blob = FullView::Blob(BlobView {
            id: input.id,
            state: BlobState::NotDownloaded { size: 5000 },
            change: None,
            pending: None,
        });
        assert_eq!(
            view.pending_approval,
            Some(ApprovalPrompt {
                from: None,
                id: approval,
                call: None,
                tool: "bash".into(),
                summary: "rm -rf build".into(),
                input: Some(blob.clone()),
                suggestions: Vec::new(),
                shell: None,
                answer: None,
            })
        );

        // Unknown approvals cannot be answered; a known one once.
        assert!(
            f.dispatch(Action::ResolveApproval {
                id: ApprovalId::new(),
                decision: Decision::Approve,
                rule: None,
            })
            .is_empty()
        );
        let answer = Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        };
        let command = queued(&f.dispatch(answer.clone()));
        assert!(f.dispatch(answer.clone()).is_empty());
        let prompt = f.session_view().pending_approval.unwrap();
        assert_eq!(
            prompt.answer,
            Some(Answer {
                decision: Decision::Approve,
                rule: None,
                outgoing: Outgoing::Queued,
                delivery: f.model.outbox.last().and_then(Delivery::of),
                retryable: false,
            })
        );

        // Another device answered first: ours is refused, theirs wins.
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::ApprovalAlreadyResolved,
            },
            Utc::now(),
        );
        // Before the replica hears who won, the refusal already says the
        // approval is closed: no answering again.
        let refused = f.session_view().pending_approval.unwrap().answer.unwrap();
        assert_eq!(
            refused.outgoing,
            Outgoing::Failed(FailReason::Rejected(RejectReason::ApprovalAlreadyResolved))
        );
        assert!(!refused.retryable);
        assert!(f.dispatch(answer.clone()).is_empty());
        let phone = DeviceId::new();
        f.log(
            sid,
            vec![
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Deny,
                    by: phone,
                    rule: None,
                },
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        let view = f.session_view();
        assert_eq!(view.state, SessionState::Idle);
        assert_eq!(view.pending_approval, None);
        // The settled item keeps this device's refused answer in sight.
        assert!(view.timeline.contains(&TimelineItem::Logged {
            parent: None,
            seq: Seq(3),
            at: f.events[2].at,
            context: skywave_proto::ContextId::first_of(sid),
            entry: Entry::Approval {
                id: approval,
                tool: "bash".into(),
                summary: "rm -rf build".into(),
                input: Some(blob),
                status: ApprovalStatus::Resolved {
                    decision: Decision::Deny,
                    by: DeviceRef {
                        id: phone,
                        name: None,
                        me: false,
                    },
                    rule: None,
                },
                answer: Some(refused),
            },
        }));
        assert!(
            f.dispatch(answer).is_empty(),
            "resolved approvals are closed"
        );
    }

    /// Only a refusal of the answer itself leaves the approval open to
    /// another answer; a cancelled approval takes none.
    #[test]
    fn a_refused_answer_is_retryable_only_while_the_approval_is_open() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        f.log(
            sid,
            vec![EventKind::ApprovalRequested {
                id: approval,
                call: None,
                tool: "bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: Vec::new(),
                shell: None,
            }],
        );
        f.open(sid);
        let answer = Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        };
        let refuse = |f: &mut Fixture, command: CommandId, reason| {
            f.model
                .command_result(command, CommandResult::Rejected { reason }, Utc::now());
        };

        let first = queued(&f.dispatch(answer.clone()));
        refuse(&mut f, first.id, RejectReason::UnknownRule);
        let prompt = f.session_view().pending_approval.unwrap();
        assert!(prompt.answer.unwrap().retryable);
        let second = queued(&f.dispatch(answer.clone()));

        refuse(&mut f, second.id, RejectReason::ApprovalCancelled);
        let prompt = f.session_view().pending_approval.unwrap();
        assert!(!prompt.answer.unwrap().retryable);
        assert!(f.dispatch(answer).is_empty());

        f.log(
            sid,
            vec![EventKind::ApprovalCancelled {
                id: approval,
                reason: "daemon restarted".into(),
            }],
        );
        let view = f.session_view();
        assert_eq!(view.pending_approval, None);
        let item = view.timeline.iter().find_map(|item| match item {
            TimelineItem::Logged {
                entry: Entry::Approval { status, answer, .. },
                ..
            } => Some((status.clone(), answer.clone().unwrap())),
            _ => None,
        });
        let (status, answer) = item.unwrap();
        assert_eq!(
            status,
            ApprovalStatus::Cancelled {
                reason: "daemon restarted".into()
            }
        );
        assert_eq!(
            answer.outgoing,
            Outgoing::Failed(FailReason::Rejected(RejectReason::ApprovalCancelled))
        );
        assert!(!answer.retryable);
    }

    // A double tap: the first answer is queued, the second sends nothing.
    #[test]
    fn a_second_answer_while_the_first_is_queued_sends_nothing() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        f.log(
            sid,
            vec![EventKind::ApprovalRequested {
                id: approval,
                call: None,
                tool: "bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: Vec::new(),
                shell: None,
            }],
        );
        f.open(sid);
        let approve = Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        };
        let first = queued(&f.dispatch(approve.clone()));
        assert!(matches!(
            first.kind,
            CommandKind::ResolveApproval { id, decision: Decision::Approve, .. } if id == approval
        ));
        assert!(f.dispatch(approve).is_empty());
        let deny = Action::ResolveApproval {
            id: approval,
            decision: Decision::Deny,
            rule: None,
        };
        assert!(f.dispatch(deny).is_empty());

        // The same for a question, in a fresh session.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let question = ApprovalId::new();
        let call = CallId::new();
        let option = |label: &str| skywave_proto::QuestionOption {
            label: label.into(),
            description: format!("{label} bread"),
            preview: None,
        };
        let questions = vec![Question {
            header: "Bread".into(),
            text: "Which bread?".into(),
            options: vec![option("Rye"), option("Spelt")],
            multi_select: false,
        }];
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(call), ToolKind::Other, "Which bread?"),
                EventKind::QuestionAsked {
                    id: question,
                    call: Some(call),
                    questions,
                },
            ],
        );
        f.open(sid);
        let answer = |picked: Vec<u8>| skywave_proto::QuestionAnswer {
            picked,
            other: None,
        };
        let reply = Action::AnswerQuestion {
            id: question,
            answers: vec![answer(vec![1])],
        };
        let queued_answer = queued(&f.dispatch(reply.clone()));
        assert_eq!(
            queued_answer.kind,
            CommandKind::AnswerQuestion {
                id: question,
                answers: vec![answer(vec![1])],
            }
        );
        assert!(f.dispatch(reply).is_empty());
        let deny = Action::ResolveApproval {
            id: question,
            decision: Decision::Deny,
            rule: None,
        };
        assert!(f.dispatch(deny).is_empty());
    }

    #[test]
    fn a_granted_rule_shows_on_every_device_and_counts() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (first, second) = (ApprovalId::new(), ApprovalId::new());
        let edits = RuleSuggestion {
            label: "all edits".into(),
            scope: RuleScope::Session,
        };
        let ls = RuleSuggestion {
            label: "bash(ls)".into(),
            scope: RuleScope::Project,
        };
        let request = |id, suggestions| EventKind::ApprovalRequested {
            id,
            call: None,
            tool: "bash".into(),
            summary: "ls".into(),
            input: None,
            suggestions,
            shell: None,
        };
        f.log(sid, vec![request(first, vec![ls.clone(), edits.clone()])]);
        f.open(sid);
        let prompt = f.session_view().pending_approval.unwrap();
        assert_eq!(prompt.suggestions, [ls.clone(), edits.clone()]);

        // A rule with a denial, or one not offered, is not sent.
        for (decision, rule) in [(Decision::Deny, Some(0)), (Decision::Approve, Some(2))] {
            assert!(
                f.dispatch(Action::ResolveApproval {
                    id: first,
                    decision,
                    rule
                })
                .is_empty(),
                "{decision:?} {rule:?}"
            );
        }
        // This device's answer shows the rule while it is on its way.
        queued(&f.dispatch(Action::ResolveApproval {
            id: first,
            decision: Decision::Approve,
            rule: Some(1),
        }));
        let answer = f.session_view().pending_approval.unwrap().answer.unwrap();
        assert_eq!(answer.rule, Some(edits.clone()));
        assert!(
            f.session_view().rules.is_empty(),
            "not granted until logged"
        );

        // Logged: here, then another device grants one too.
        let phone = DeviceId::new();
        let me = f.me;
        f.log(
            sid,
            vec![
                EventKind::ApprovalResolved {
                    id: first,
                    decision: Decision::Approve,
                    by: me,
                    rule: Some(1),
                },
                request(second, vec![ls.clone()]),
                EventKind::ApprovalResolved {
                    id: second,
                    decision: Decision::Approve,
                    by: phone,
                    rule: Some(0),
                },
            ],
        );
        let view = f.session_view();
        let statuses: Vec<&ApprovalStatus> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry: Entry::Approval { status, .. },
                    ..
                } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [
                &ApprovalStatus::Resolved {
                    decision: Decision::Approve,
                    by: DeviceRef {
                        id: me,
                        name: None,
                        me: true,
                    },
                    rule: Some(edits.clone()),
                },
                &ApprovalStatus::Resolved {
                    decision: Decision::Approve,
                    by: DeviceRef {
                        id: phone,
                        name: None,
                        me: false,
                    },
                    rule: Some(ls.clone()),
                },
            ]
        );
        assert_eq!(view.rules, [edits, ls]);
    }

    fn work_task(id: &str) -> skywave_proto::BackgroundTask {
        skywave_proto::BackgroundTask {
            call: None,
            id: TaskId(id.into()),
            kind: skywave_proto::TaskKind::Shell,
            title: id.into(),
            parent: None,
            ambient: false,
        }
    }

    /// A stop of task `t1` accepted at the end of the log, with `t1` listed.
    fn accepted_stop(f: &mut Fixture, sid: SessionId) -> Command {
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged {
                tasks: vec![work_task("t1")],
            }],
        );
        f.open(sid);
        assert_eq!(f.session_view().work.tasks.len(), 1);
        let command = queued(&f.dispatch(Action::StopTask {
            session_id: sid,
            task: TaskId("t1".into()),
        }));
        assert_eq!(
            command.kind,
            CommandKind::StopTask {
                session_id: sid,
                task: TaskId("t1".into())
            }
        );
        assert_eq!(
            f.session_view().task_stops,
            vec![(TaskId("t1".into()), Outgoing::Queued)]
        );
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::TaskStopRequested {
                    task: TaskId("t1".into()),
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        assert_eq!(
            f.session_view().task_stops,
            vec![(TaskId("t1".into()), Outgoing::Accepted(seq))]
        );
        command
    }

    #[test]
    fn a_task_stop_shows_until_the_task_leaves_the_work() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        accepted_stop(&mut f, sid);
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged { tasks: vec![] }],
        );
        assert_eq!(f.session_view().task_stops, vec![]);
    }

    /// An idle turn with a task or a schedule still running reads as working;
    /// a working turn keeps its own state.
    #[test]
    fn background_work_reads_as_working_while_the_turn_is_idle() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        assert_eq!(f.session_view().state, SessionState::Idle);
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged {
                tasks: vec![work_task("t1")],
            }],
        );
        assert_eq!(f.session_view().state, SessionState::BackgroundWork);
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(
            f.list_view().sessions[0].state,
            SessionState::BackgroundWork
        );
        f.open(sid);
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        assert_eq!(f.session_view().state, SessionState::AgentWorking);
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(f.session_view().state, SessionState::BackgroundWork);
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged { tasks: vec![] }],
        );
        assert_eq!(f.session_view().state, SessionState::Idle);
        let schedule = skywave_proto::Schedule {
            id: skywave_proto::ScheduleId("s1".into()),
            when: "*/5 * * * *".into(),
            recurring: true,
            prompt: "check".into(),
            outlives_agent: None,
        };
        f.log(
            sid,
            vec![EventKind::SchedulesChanged {
                schedules: vec![schedule],
            }],
        );
        assert_eq!(f.session_view().state, SessionState::BackgroundWork);
        f.log(sid, vec![EventKind::SchedulesChanged { schedules: vec![] }]);
        assert_eq!(f.session_view().state, SessionState::Idle);
    }

    #[test]
    fn a_refusal_answering_a_task_stop_clears_it() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let command = accepted_stop(&mut f, sid);
        // Positive control: a refusal answering another command leaves it.
        f.log(
            sid,
            vec![EventKind::TaskStopRefused {
                task: TaskId("t1".into()),
                reason: "no".into(),
                answers: Some(CommandId::new()),
            }],
        );
        assert_eq!(f.session_view().task_stops.len(), 1);
        f.log(
            sid,
            vec![EventKind::TaskStopRefused {
                task: TaskId("t1".into()),
                reason: "no".into(),
                answers: Some(command.id),
            }],
        );
        assert_eq!(f.session_view().task_stops, vec![]);
        // The task is still listed: it can be stopped again.
        assert_eq!(f.session_view().work.tasks.len(), 1);
    }

    #[test]
    fn a_safety_check_switch_shows_until_it_is_logged() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        assert!(!f.session_view().safety_check);

        let command = queued(&f.dispatch(Action::SetSafetyCheck {
            session_id: sid,
            on: true,
        }));
        assert_eq!(
            command.kind,
            CommandKind::SetSafetyCheck {
                session_id: sid,
                on: true
            }
        );
        // On its way: this device's switch, the flag still off.
        let view = f.session_view();
        assert!(!view.safety_check);
        assert_eq!(
            view.safety_check_change,
            Some(SettingChange {
                value: true,
                outgoing: Some(Outgoing::Queued),
            })
        );

        // Logged and accepted: the log holds it, so it is in effect.
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::SafetyCheckSet { on: true, by: me },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        let view = f.session_view();
        assert!(view.safety_check);
        assert_eq!(view.safety_check_change, None);
    }

    /// The safety rules Settings lists; the settings screen must be open.
    fn safety_rules(f: &Fixture) -> Vec<SafetyRuleView> {
        match f.model.view().screen {
            Screen::Settings(view) => view.safety_rules,
            other => panic!("expected settings, got {other:?}"),
        }
    }

    fn prefix(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn a_new_safety_rule_shows_pending_until_logged() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let me = f.me;
        let git = prefix(&["git", "commit"]);
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(safety_rules(&f), Vec::<SafetyRuleView>::new());

        let command = queued(&f.dispatch(Action::SetSafetyRule {
            session_id: sid,
            words: git.clone(),
            on: true,
        }));
        assert_eq!(
            command.kind,
            CommandKind::SetSafetyRule {
                session_id: sid,
                words: git.clone(),
                on: true,
            }
        );
        // On its way: never logged, so off in the log, with the change pending.
        let pending = |outgoing| SafetyRuleView {
            words: git.clone(),
            session_id: sid,
            on: false,
            pending: Some(PendingSafetyRule {
                command: command.id,
                on: true,
                outgoing,
            }),
        };
        assert_eq!(safety_rules(&f), vec![pending(Outgoing::Queued)]);

        // Accepted at a seq the log does not hold yet: still pending.
        let seq = Seq(f.events.len() as u64 + 1);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        assert_eq!(safety_rules(&f), vec![pending(Outgoing::Accepted(seq))]);

        // Logged: the log holds it on, and nothing is pending.
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::SafetyRuleSet {
                    words: git.clone(),
                    on: true,
                    by: me,
                },
            )],
        );
        assert_eq!(
            safety_rules(&f),
            vec![SafetyRuleView {
                words: git,
                session_id: sid,
                on: true,
                pending: None,
            }]
        );
    }

    #[test]
    fn a_failed_safety_rule_change_clears_once_a_later_one_is_logged() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let me = f.me;
        let git = prefix(&["git", "status"]);
        f.dispatch(Action::Navigate(Route::Settings));

        // The add is refused: listed as failed, off in the log.
        let first = queued(&f.dispatch(Action::SetSafetyRule {
            session_id: sid,
            words: git.clone(),
            on: true,
        }));
        f.model.command_result(
            first.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        let failed = SafetyRuleView {
            words: git.clone(),
            session_id: sid,
            on: false,
            pending: Some(PendingSafetyRule {
                command: first.id,
                on: true,
                outgoing: Outgoing::Failed(FailReason::Rejected(RejectReason::UnknownSession)),
            }),
        };
        assert_eq!(safety_rules(&f), vec![failed]);

        // Trusted again: the newer add is accepted and logged, so the failed
        // one no longer shows.
        let second = queued(&f.dispatch(Action::SetSafetyRule {
            session_id: sid,
            words: git.clone(),
            on: true,
        }));
        let seq = Seq(f.events.len() as u64 + 1);
        f.model
            .command_result(second.id, CommandResult::Accepted { seq }, Utc::now());
        f.log_commands(
            sid,
            vec![(
                Some(second.id),
                EventKind::SafetyRuleSet {
                    words: git.clone(),
                    on: true,
                    by: me,
                },
            )],
        );
        assert_eq!(
            safety_rules(&f),
            vec![SafetyRuleView {
                words: git,
                session_id: sid,
                on: true,
                pending: None,
            }]
        );
    }

    #[test]
    fn a_failed_safety_rule_add_can_be_dismissed() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let git = prefix(&["git", "status"]);
        f.dispatch(Action::Navigate(Route::Settings));
        let command = queued(&f.dispatch(Action::SetSafetyRule {
            session_id: sid,
            words: git,
            on: true,
        }));
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        // The failed add is listed, and its command id is what dismisses it.
        let listed = safety_rules(&f);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].pending.as_ref().map(|change| change.command),
            Some(command.id)
        );

        f.dispatch(Action::Discard(command.id));
        assert_eq!(safety_rules(&f), Vec::<SafetyRuleView>::new());
    }

    #[test]
    fn removing_a_safety_rule_lists_it_until_logged_off() {
        let mut f = Fixture::paired();
        let b = f.session("/srv/b");
        let me = f.me;
        let git = prefix(&["git", "commit"]);
        f.log(
            b,
            vec![EventKind::SafetyRuleSet {
                words: git.clone(),
                on: true,
                by: me,
            }],
        );
        f.dispatch(Action::Navigate(Route::Settings));
        let logged = SafetyRuleView {
            words: git.clone(),
            session_id: b,
            on: true,
            pending: None,
        };
        assert_eq!(safety_rules(&f), vec![logged.clone()]);

        // Listed while the removal is on its way: on in the log, change pending.
        let command = queued(&f.dispatch(Action::SetSafetyRule {
            session_id: b,
            words: git.clone(),
            on: false,
        }));
        assert_eq!(
            safety_rules(&f),
            vec![SafetyRuleView {
                pending: Some(PendingSafetyRule {
                    command: command.id,
                    on: false,
                    outgoing: Outgoing::Queued,
                }),
                ..logged.clone()
            }]
        );

        // Accepted and logged off: nothing is listed any more.
        let seq = Seq(f.events.len() as u64 + 1);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        f.log_commands(
            b,
            vec![(
                Some(command.id),
                EventKind::SafetyRuleSet {
                    words: git,
                    on: false,
                    by: me,
                },
            )],
        );
        assert_eq!(safety_rules(&f), Vec::<SafetyRuleView>::new());
    }

    #[test]
    fn safety_rule_rows_skip_off_rules_and_bad_words() {
        let mut f = Fixture::paired();
        let (a, b) = (f.session("/srv/a"), f.session("/srv/b"));
        let me = f.me;
        let (git, cargo) = (prefix(&["git", "commit"]), prefix(&["cargo", "test"]));
        f.log(
            a,
            vec![EventKind::SafetyRuleSet {
                words: git.clone(),
                on: true,
                by: me,
            }],
        );
        f.log(
            b,
            vec![EventKind::SafetyRuleSet {
                words: git,
                on: false,
                by: me,
            }],
        );
        f.log(
            a,
            vec![EventKind::SafetyRuleSet {
                words: cargo.clone(),
                on: true,
                by: me,
            }],
        );
        f.dispatch(Action::Navigate(Route::Settings));
        // Git's newest rule is off: only cargo's on rule is listed.
        assert_eq!(
            safety_rules(&f),
            vec![SafetyRuleView {
                words: cargo,
                session_id: a,
                on: true,
                pending: None,
            }]
        );

        // Neither an empty prefix nor one with an empty word is queued.
        for words in [Vec::new(), prefix(&["git", ""])] {
            let effects = f.dispatch(Action::SetSafetyRule {
                session_id: a,
                words,
                on: true,
            });
            assert!(effects.is_empty());
        }
        assert!(f.model.outbox.is_empty());
    }

    #[test]
    fn a_safety_rule_of_a_session_deleted_here_is_listed_and_removable() {
        let mut f = Fixture::paired();
        let gone = f.session("/srv/gone");
        let me = f.me;
        let git = prefix(&["git", "commit"]);
        f.log(
            gone,
            vec![EventKind::SafetyRuleSet {
                words: git.clone(),
                on: true,
                by: me,
            }],
        );
        f.model.session_deleted(gone, &[]);
        f.dispatch(Action::Navigate(Route::Settings));
        let logged = SafetyRuleView {
            words: git.clone(),
            session_id: gone,
            on: true,
            pending: None,
        };
        assert_eq!(safety_rules(&f), vec![logged.clone()]);

        // The removal still goes to that session, which the server keeps.
        let command = queued(&f.dispatch(Action::SetSafetyRule {
            session_id: gone,
            words: git.clone(),
            on: false,
        }));
        assert_eq!(
            command.kind,
            CommandKind::SetSafetyRule {
                session_id: gone,
                words: git,
                on: false,
            }
        );
        assert_eq!(
            safety_rules(&f),
            vec![SafetyRuleView {
                pending: Some(PendingSafetyRule {
                    command: command.id,
                    on: false,
                    outgoing: Outgoing::Queued,
                }),
                ..logged
            }]
        );
    }

    #[test]
    fn a_mode_switch_shows_until_the_agent_answers_it() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log(
            sid,
            vec![EventKind::PermissionModeChanged {
                mode: PermissionMode::Ask,
                answers: None,
            }],
        );
        f.open(sid);
        assert_eq!(f.session_view().agent.mode, Some(PermissionMode::Ask));

        let command = queued(&f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        }));
        assert_eq!(
            command.kind,
            CommandKind::SetPermissionMode {
                session_id: sid,
                mode: PermissionMode::Plan
            }
        );
        // On its way: this device's command.
        assert_eq!(
            f.session_view().agent.mode_change,
            Some(SettingChange {
                value: PermissionMode::Plan,
                outgoing: Some(Outgoing::Queued),
            })
        );

        // Logged: only the agent's answer is awaited.
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::PermissionModeRequested {
                    mode: PermissionMode::Plan,
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        let agent = f.session_view().agent;
        assert_eq!(agent.mode, Some(PermissionMode::Ask));
        assert_eq!(
            agent.mode_change,
            Some(SettingChange {
                value: PermissionMode::Plan,
                outgoing: None,
            })
        );

        f.log(
            sid,
            vec![EventKind::PermissionModeChanged {
                mode: PermissionMode::Plan,
                answers: Some(command.id),
            }],
        );
        let view = f.session_view();
        assert_eq!(view.agent.mode, Some(PermissionMode::Plan));
        assert_eq!(view.agent.mode_change, None);
        // The timeline names who asked, and the change made on its own.
        let settings: Vec<&Entry> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry: entry @ Entry::AgentSetting { .. },
                    ..
                } => Some(entry),
                _ => None,
            })
            .collect();
        assert_eq!(
            settings,
            [
                &Entry::AgentSetting {
                    setting: AgentSetting::Mode(PermissionMode::Ask),
                    by: None,
                    refused: None,
                },
                &Entry::AgentSetting {
                    setting: AgentSetting::Mode(PermissionMode::Plan),
                    by: Some(DeviceRef {
                        id: me,
                        name: None,
                        me: true,
                    }),
                    refused: None,
                },
            ]
        );
        // Not activity: the session keeps its place.
        assert_eq!(f.list_view_after_back().sessions[0].id, sid);
    }

    /// The switches on their way in the open session's timeline, as
    /// (command, change, delivery).
    fn changes(f: &Fixture) -> Vec<(CommandId, PendingChange, Option<Outgoing>)> {
        f.session_view()
            .timeline
            .into_iter()
            .filter_map(|item| match item {
                TimelineItem::Change {
                    command_id,
                    change,
                    outgoing,
                    ..
                } => Some((command_id, change, outgoing)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_mode_switch_is_a_row_until_the_agent_answers_it() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log(
            sid,
            vec![EventKind::PermissionModeChanged {
                mode: PermissionMode::Ask,
                answers: None,
            }],
        );
        f.open(sid);

        let command = queued(&f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        }));
        let plan = PendingChange::Setting(AgentSetting::Mode(PermissionMode::Plan));
        assert_eq!(
            changes(&f),
            [(command.id, plan.clone(), Some(Outgoing::Queued))]
        );

        // Logged: only the agent's answer is awaited.
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::PermissionModeRequested {
                    mode: PermissionMode::Plan,
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        assert_eq!(changes(&f), [(command.id, plan, None)]);

        f.log(
            sid,
            vec![EventKind::PermissionModeChanged {
                mode: PermissionMode::Plan,
                answers: Some(command.id),
            }],
        );
        assert!(changes(&f).is_empty());
        assert!(f.session_view().timeline.iter().any(|item| matches!(
            item,
            TimelineItem::Logged {
                entry: Entry::AgentSetting {
                    setting: AgentSetting::Mode(PermissionMode::Plan),
                    ..
                },
                ..
            }
        )));
    }

    #[test]
    fn a_refused_mode_switch_stays_a_row_with_its_failure() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = queued(&f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        }));
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::TurnRunning,
            },
            Utc::now(),
        );
        assert_eq!(
            changes(&f),
            [(
                command.id,
                PendingChange::Setting(AgentSetting::Mode(PermissionMode::Plan)),
                Some(Outgoing::Failed(FailReason::Rejected(
                    RejectReason::TurnRunning
                ))),
            )]
        );
    }

    #[test]
    fn a_refused_mode_switch_is_dismissed_and_its_row_goes() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = queued(&f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        }));
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::TurnRunning,
            },
            Utc::now(),
        );
        assert!(f.session_view().timeline.iter().any(|item| matches!(
            item,
            TimelineItem::Change {
                discardable: true,
                ..
            }
        )));
        let effects = f.dispatch(Action::Discard(command.id));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Withdraw { id, .. } if *id == command.id))
        );
        assert!(changes(&f).is_empty());
    }

    #[test]
    fn a_mode_switch_on_its_way_is_not_dismissed() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = queued(&f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        }));
        assert!(f.dispatch(Action::Discard(command.id)).is_empty());
        let rows = changes(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, command.id);
    }

    #[test]
    fn dismissing_a_failed_clear_takes_the_earlier_failed_one_with_it() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let first = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        f.model.command_undeliverable(first.id, Utc::now());
        let second = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        f.model.command_result(
            second.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextExists,
            },
            Utc::now(),
        );
        let rows = changes(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, second.id);
        let effects = f.dispatch(Action::Discard(second.id));
        for gone in [first.id, second.id] {
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Withdraw { id, .. } if *id == gone))
            );
        }
        assert!(changes(&f).is_empty());
        assert_eq!(f.session_view().clear, None);
    }

    #[test]
    fn dismissing_a_failed_clear_leaves_other_kinds_and_sessions() {
        let mut f = Fixture::paired();
        let a = f.session("/srv/a");
        let b = f.session("/srv/b");
        f.open(b);
        let other_clear = queued(&f.dispatch(Action::ClearContext { session_id: b }));
        f.model.command_result(
            other_clear.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextExists,
            },
            Utc::now(),
        );
        f.open(a);
        let model = queued(&f.dispatch(Action::SetModel {
            session_id: a,
            model: "opus".into(),
        }));
        f.model.command_result(
            model.id,
            CommandResult::Rejected {
                reason: RejectReason::TurnRunning,
            },
            Utc::now(),
        );
        let clear = queued(&f.dispatch(Action::ClearContext { session_id: a }));
        f.model.command_result(
            clear.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextExists,
            },
            Utc::now(),
        );
        let effects = f.dispatch(Action::Discard(clear.id));
        assert_eq!(
            effects,
            vec![
                Effect::Withdraw {
                    id: clear.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        let left: Vec<CommandId> = f.model.outbox.iter().map(|e| e.command.id).collect();
        assert_eq!(left, [other_clear.id, model.id]);
    }

    #[test]
    fn a_clear_is_a_row_until_its_divider_is_in_the_replica() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let clear = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        let CommandKind::ClearContext { context, name, .. } = clear.kind.clone() else {
            panic!("expected a clear, got {clear:?}");
        };
        assert_eq!(
            changes(&f),
            [(
                clear.id,
                PendingChange::NewContext { name: name.clone() },
                Some(Outgoing::Queued),
            )]
        );

        let me = f.me;
        f.log_stamped(sid, vec![(Some(clear.id), context, started(&name, me))]);
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(clear.id, CommandResult::Accepted { seq }, Utc::now());
        assert!(changes(&f).is_empty());
        assert!(f.session_view().timeline.iter().any(|item| matches!(
            item,
            TimelineItem::Logged {
                entry: Entry::ContextStarted { .. },
                ..
            }
        )));
    }

    #[test]
    fn a_switch_is_a_row_named_for_its_context_until_pulled() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let first = ContextId::first_of(sid);
        let later = ContextId::new();
        f.log_in(sid, later, vec![started("velvet walrus", DeviceId::new())]);
        f.open(sid);
        let switch = queued(&f.dispatch(Action::SwitchContext {
            session_id: sid,
            context: first,
        }));
        let back = PendingChange::SwitchContext {
            name: "start".into(),
        };
        assert_eq!(
            changes(&f),
            [(switch.id, back.clone(), Some(Outgoing::Queued))]
        );

        f.model.command_sending(switch.id, Utc::now());
        f.model.command_result(
            switch.id,
            CommandResult::Accepted { seq: Seq(3) },
            Utc::now(),
        );
        assert_eq!(
            changes(&f),
            [(switch.id, back, Some(Outgoing::Accepted(Seq(3))))]
        );

        f.log_stamped(
            sid,
            vec![(
                Some(switch.id),
                first,
                EventKind::ContextSwitched { by: f.me },
            )],
        );
        assert!(changes(&f).is_empty());
    }

    #[test]
    fn another_devices_setting_request_is_no_row_here() {
        use skywave_proto::PermissionMode;
        // Another device's request: the header shows it, no row is this device's.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log_commands(
            sid,
            vec![(
                Some(CommandId::new()),
                EventKind::PermissionModeRequested {
                    mode: PermissionMode::Plan,
                    by: DeviceId::new(),
                },
            )],
        );
        f.open(sid);
        assert!(f.session_view().agent.mode_change.is_some());
        assert!(changes(&f).is_empty());

        // This device's request: the same log shape, named by this device.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let command = CommandId::new();
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command),
                EventKind::PermissionModeRequested {
                    mode: PermissionMode::Plan,
                    by: me,
                },
            )],
        );
        f.open(sid);
        assert_eq!(
            changes(&f),
            [(
                command,
                PendingChange::Setting(AgentSetting::Mode(PermissionMode::Plan)),
                None,
            )]
        );
    }

    #[test]
    fn a_switch_on_its_way_is_no_row_while_viewing_an_earlier_context() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let first = ContextId::first_of(sid);
        let later = ContextId::new();
        f.log_in(sid, later, vec![started("velvet walrus", DeviceId::new())]);
        f.open(sid);
        f.dispatch(Action::ViewContext {
            session_id: sid,
            context: Some(first),
        });

        let command = queued(&f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        }));
        // The header shows it whichever context is viewed.
        assert!(f.session_view().agent.mode_change.is_some());
        assert!(changes(&f).is_empty());

        f.dispatch(Action::ViewContext {
            session_id: sid,
            context: Some(later),
        });
        assert_eq!(
            changes(&f),
            [(
                command.id,
                PendingChange::Setting(AgentSetting::Mode(PermissionMode::Plan)),
                Some(Outgoing::Queued),
            )]
        );
    }

    #[test]
    fn model_and_effort_switches_are_rows_until_answered() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log(
            sid,
            vec![
                EventKind::ModelChanged {
                    model: "model-s".into(),
                    answers: None,
                },
                EventKind::EffortChanged {
                    effort: Some(Effort::High),
                    answers: None,
                },
            ],
        );
        f.open(sid);
        let me = f.me;

        let model = queued(&f.dispatch(Action::SetModel {
            session_id: sid,
            model: "haiku".into(),
        }));
        let row = PendingChange::Setting(AgentSetting::Model("haiku".into()));
        assert_eq!(
            changes(&f),
            [(model.id, row.clone(), Some(Outgoing::Queued))]
        );
        // Logged: only the agent's answer is awaited.
        f.log_commands(
            sid,
            vec![(
                Some(model.id),
                EventKind::ModelRequested {
                    model: "haiku".into(),
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(model.id, CommandResult::Accepted { seq }, Utc::now());
        assert_eq!(changes(&f), [(model.id, row, None)]);
        f.log(
            sid,
            vec![EventKind::ModelChanged {
                model: "haiku".into(),
                answers: Some(model.id),
            }],
        );
        assert!(changes(&f).is_empty());

        let effort = queued(&f.dispatch(Action::SetEffort {
            session_id: sid,
            effort: Effort::Max,
        }));
        let row = PendingChange::Setting(AgentSetting::Effort(Some(Effort::Max)));
        assert_eq!(
            changes(&f),
            [(effort.id, row.clone(), Some(Outgoing::Queued))]
        );
        f.log_commands(
            sid,
            vec![(
                Some(effort.id),
                EventKind::EffortRequested {
                    effort: Effort::Max,
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(effort.id, CommandResult::Accepted { seq }, Utc::now());
        assert_eq!(changes(&f), [(effort.id, row, None)]);
        f.log(
            sid,
            vec![EventKind::EffortChanged {
                effort: Some(Effort::Max),
                answers: Some(effort.id),
            }],
        );
        assert!(changes(&f).is_empty());
    }

    #[test]
    fn a_clear_accepted_but_not_pulled_is_still_a_row() {
        use skywave_proto::PermissionMode;
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let clear = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        let CommandKind::ClearContext { context, name, .. } = clear.kind.clone() else {
            panic!("expected a clear, got {clear:?}");
        };
        let row = PendingChange::NewContext { name: name.clone() };
        // Accepted at log position 3, past the replica's cursor (1).
        f.model.command_result(
            clear.id,
            CommandResult::Accepted { seq: Seq(3) },
            Utc::now(),
        );
        assert_eq!(
            changes(&f),
            [(clear.id, row.clone(), Some(Outgoing::Accepted(Seq(3))))]
        );

        // The divider lands at 2: the replica has not reached 3 yet.
        let me = f.me;
        f.log_stamped(sid, vec![(Some(clear.id), context, started(&name, me))]);
        assert_eq!(
            changes(&f),
            [(clear.id, row, Some(Outgoing::Accepted(Seq(3))))]
        );

        // Pulled up to 3: done.
        f.log(
            sid,
            vec![EventKind::PermissionModeChanged {
                mode: PermissionMode::Ask,
                answers: None,
            }],
        );
        assert!(changes(&f).is_empty());
    }

    #[test]
    fn pending_messages_and_switches_keep_the_order_they_were_asked_in() {
        use skywave_proto::PermissionMode;
        // The kinds of the open session's pending items, in timeline order.
        let kinds = |f: &Fixture| -> Vec<&'static str> {
            f.session_view()
                .timeline
                .iter()
                .filter_map(|item| match item {
                    TimelineItem::Pending { .. } => Some("pending"),
                    TimelineItem::Change { .. } => Some("change"),
                    TimelineItem::Logged { .. } => None,
                })
                .collect()
        };

        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        });
        send(&mut f, sid, "hi");
        assert_eq!(kinds(&f), ["change", "pending"]);

        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        send(&mut f, sid, "hi");
        f.dispatch(Action::SetPermissionMode {
            session_id: sid,
            mode: PermissionMode::Plan,
        });
        assert_eq!(kinds(&f), ["pending", "change"]);
    }

    #[test]
    fn a_refused_model_keeps_the_old_one_and_offered_models_reach_every_session() {
        let mut f = Fixture::paired();
        let (a, b) = (f.session("/srv/a"), f.session("/srv/b"));
        let choice = ModelChoice {
            id: "sonnet".into(),
            label: "Sonnet".into(),
            detail: None,
            resolved: Some("model-s".into()),
            efforts: Vec::new(),
        };
        f.log(
            a,
            vec![
                EventKind::ModelsOffered {
                    models: vec![choice.clone()],
                },
                EventKind::ModelChanged {
                    model: "model-s".into(),
                    answers: None,
                },
            ],
        );
        f.open(b);
        assert_eq!(f.session_view().agent.models, std::slice::from_ref(&choice));
        assert!(
            f.dispatch(Action::SetModel {
                session_id: b,
                model: "  ".into(),
            })
            .is_empty(),
            "a blank model is not sent"
        );

        f.open(a);
        let agent = f.session_view().agent;
        assert_eq!(agent.model_label(), Some("Sonnet"));
        let command = queued(&f.dispatch(Action::SetModel {
            session_id: a,
            model: " bogus\n".into(),
        }));
        assert_eq!(
            command.kind,
            CommandKind::SetModel {
                session_id: a,
                model: "bogus".into()
            }
        );
        let me = f.me;
        f.log_commands(
            a,
            vec![
                (
                    Some(command.id),
                    EventKind::ModelRequested {
                        model: "bogus".into(),
                        by: me,
                    },
                ),
                (
                    None,
                    EventKind::ModelRefused {
                        model: "bogus".into(),
                        reason: "not found".into(),
                        answers: Some(command.id),
                    },
                ),
            ],
        );
        let seq = Seq(f.events.len() as u64 - 1);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        let agent = f.session_view().agent;
        assert_eq!(agent.model.as_deref(), Some("model-s"));
        assert_eq!(agent.model_change, None);
        assert_eq!(
            agent.model_refused,
            Some(Refusal {
                value: "bogus".into(),
                reason: "not found".into(),
            })
        );
    }

    #[test]
    fn an_effort_switch_shows_until_answered_and_its_timeline_names_who_asked() {
        let mut f = Fixture::paired();
        let (sid, other) = (f.session("/srv/a"), f.session("/srv/b"));
        assert!(
            f.dispatch(Action::SetEffort {
                session_id: SessionId::new(),
                effort: Effort::High,
            })
            .is_empty(),
            "an unknown session is a no-op"
        );
        f.log(
            sid,
            vec![EventKind::EffortChanged {
                effort: Some(Effort::High),
                answers: None,
            }],
        );
        f.open(sid);
        assert_eq!(f.session_view().agent.effort, Some(Some(Effort::High)));
        let command = queued(&f.dispatch(Action::SetEffort {
            session_id: sid,
            effort: Effort::Max,
        }));
        assert_eq!(
            command.kind,
            CommandKind::SetEffort {
                session_id: sid,
                effort: Effort::Max
            }
        );
        // On its way: this device's command, for this session only.
        assert_eq!(
            f.session_view().agent.effort_change,
            Some(SettingChange {
                value: Effort::Max,
                outgoing: Some(Outgoing::Queued),
            })
        );
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::EffortRequested {
                    effort: Effort::Max,
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        let agent = f.session_view().agent;
        assert_eq!(agent.effort, Some(Some(Effort::High)));
        assert_eq!(
            agent.effort_change,
            Some(SettingChange {
                value: Effort::Max,
                outgoing: None,
            })
        );
        f.log(
            sid,
            vec![EventKind::EffortRefused {
                effort: Effort::Max,
                reason: "kept high".into(),
                answers: Some(command.id),
            }],
        );
        let view = f.session_view();
        assert_eq!(view.agent.effort_change, None);
        assert_eq!(
            view.agent.effort_refused,
            Some(Refusal {
                value: Effort::Max,
                reason: "kept high".into(),
            })
        );
        f.log(
            sid,
            vec![EventKind::EffortChanged {
                effort: None,
                answers: None,
            }],
        );
        let view = f.session_view();
        assert_eq!(view.agent.effort, Some(None));
        let device = Some(DeviceRef {
            id: me,
            name: None,
            me: true,
        });
        let settings: Vec<&Entry> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry: entry @ Entry::AgentSetting { .. },
                    ..
                } => Some(entry),
                _ => None,
            })
            .collect();
        assert_eq!(
            settings,
            [
                &Entry::AgentSetting {
                    setting: AgentSetting::Effort(Some(Effort::High)),
                    by: None,
                    refused: None,
                },
                &Entry::AgentSetting {
                    setting: AgentSetting::Effort(Some(Effort::Max)),
                    by: device,
                    refused: Some("kept high".into()),
                },
                &Entry::AgentSetting {
                    setting: AgentSetting::Effort(None),
                    by: None,
                    refused: None,
                },
            ]
        );
        // The overlay is per session.
        f.open(other);
        assert_eq!(f.session_view().agent.effort_change, None);
    }

    #[test]
    fn efforts_follow_the_pending_model_and_fall_back_to_every_level() {
        let choice = |id: &str, efforts: Vec<Effort>| ModelChoice {
            id: id.into(),
            label: id.into(),
            detail: None,
            resolved: Some(format!("{id}-full")),
            efforts,
        };
        let change = |model: &str| {
            Some(SettingChange {
                value: model.to_owned(),
                outgoing: None,
            })
        };
        let mut agent = AgentView::default();
        assert_eq!(agent.efforts(), Effort::ALL, "no list yet");
        agent.models = vec![
            choice("sonnet", vec![Effort::Low, Effort::High]),
            choice("haiku", Vec::new()),
        ];
        agent.model = Some("sonnet-full".into());
        assert_eq!(agent.efforts(), [Effort::Low, Effort::High]);
        agent.model_change = change("haiku");
        assert!(agent.efforts().is_empty(), "the pending model takes none");
        agent.model_change = change("typed-by-hand");
        assert_eq!(agent.efforts(), Effort::ALL, "a model no choice names");
        agent.model_change = None;
        agent.model = Some("other".into());
        assert_eq!(agent.efforts(), Effort::ALL);
    }

    #[test]
    fn devices_show_by_the_names_the_server_lists() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (phone, approval) = (DeviceId::new(), ApprovalId::new());
        f.log(
            sid,
            vec![
                EventKind::UserMessage {
                    device_id: phone,
                    text: "from the phone".into(),
                    attachments: Vec::new(),
                },
                EventKind::ApprovalRequested {
                    id: approval,
                    call: None,
                    tool: "edit".into(),
                    summary: "main.rs".into(),
                    input: None,
                    suggestions: Vec::new(),
                    shell: None,
                },
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: phone,
                    rule: None,
                },
            ],
        );
        f.open(sid);
        let devices = |f: &Fixture| -> Vec<DeviceRef> {
            f.session_view()
                .timeline
                .iter()
                .filter_map(|item| match item {
                    TimelineItem::Logged {
                        entry: Entry::UserMessage { from: device, .. },
                        ..
                    }
                    | TimelineItem::Logged {
                        entry:
                            Entry::Approval {
                                status: ApprovalStatus::Resolved { by: device, .. },
                                ..
                            },
                        ..
                    } => Some(device.clone()),
                    _ => None,
                })
                .collect()
        };
        // Not listed yet: the end of its id stands in.
        let unnamed = DeviceRef {
            id: phone,
            name: None,
            me: false,
        };
        assert_eq!(devices(&f), [unnamed.clone(), unnamed.clone()]);
        let tail = &phone.0.simple().to_string()[24..];
        assert_eq!(unnamed.label(), format!("device …{tail}"));
        let batch = EventBatch {
            from: Seq(1),
            events: vec![Event {
                session_id: sid,
                at: Utc::now(),
                command_id: Some(CommandId::new()),
                context: skywave_proto::ContextId::first_of(sid),
                kind: EventKind::UserMessage {
                    device_id: phone,
                    text: "again".into(),
                    attachments: Vec::new(),
                },
            }],
        };
        assert_eq!(f.model.unnamed_devices(&batch), HashSet::from([phone]));

        let me = f.me;
        f.model.devices_named(&[
            DeviceInfo {
                id: phone,
                name: "Pixel 8".into(),
            },
            DeviceInfo {
                id: me,
                name: "laptop".into(),
            },
        ]);
        let named = DeviceRef {
            name: Some("Pixel 8".into()),
            ..unnamed
        };
        assert_eq!(devices(&f), [named.clone(), named.clone()]);
        assert_eq!(named.label(), "Pixel 8");
        assert!(f.model.unnamed_devices(&batch).is_empty());
    }

    #[test]
    fn a_refused_interrupt_shows_only_while_the_agent_works() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let refuse = |f: &mut Fixture, reason| {
            let command = queued(&f.dispatch(Action::Interrupt { session_id: sid }));
            f.model
                .command_result(command.id, CommandResult::Rejected { reason }, Utc::now());
        };

        // Nothing to interrupt: the agent already does what was asked.
        refuse(&mut f, RejectReason::NoActiveTurn);
        assert_eq!(f.session_view().interrupt, None);

        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        refuse(&mut f, RejectReason::UnknownSession);
        assert_eq!(
            f.session_view().interrupt,
            Some(Outgoing::Failed(FailReason::Rejected(
                RejectReason::UnknownSession
            )))
        );

        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(f.session_view().interrupt, None, "the turn is over");
    }

    #[test]
    fn cancelled_approval_and_interrupt() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let approval = ApprovalId::new();
        f.log(
            sid,
            vec![EventKind::ApprovalRequested {
                id: approval,
                call: None,
                tool: "edit".into(),
                summary: "main.rs".into(),
                input: None,
                suggestions: Vec::new(),
                shell: None,
            }],
        );
        f.open(sid);
        let command = queued(&f.dispatch(Action::Interrupt { session_id: sid }));
        assert_eq!(f.session_view().interrupt, Some(Outgoing::Queued));
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(3) },
            Utc::now(),
        );
        assert_eq!(f.session_view().interrupt, Some(Outgoing::Accepted(Seq(3))));

        f.log(
            sid,
            vec![
                EventKind::TurnInterrupted,
                EventKind::ApprovalCancelled {
                    id: approval,
                    reason: "turn interrupted".into(),
                },
                EventKind::SessionStateChanged(LogState::Errored {
                    message: "boom".into(),
                }),
            ],
        );
        let view = f.session_view();
        assert_eq!(view.interrupt, None, "its effect is in the replica");
        assert_eq!(
            view.state,
            SessionState::Errored(ErrorInfo {
                message: "boom".into()
            })
        );
        let entries: Vec<Entry> = view
            .timeline
            .into_iter()
            .map(|item| match item {
                TimelineItem::Logged { entry, .. } => entry,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::Approval {
                    id: approval,
                    tool: "edit".into(),
                    summary: "main.rs".into(),
                    input: None,
                    status: ApprovalStatus::Cancelled {
                        reason: "turn interrupted".into()
                    },
                    answer: None,
                },
                Entry::Interrupted,
                Entry::AgentErrored {
                    message: "boom".into()
                },
            ]
        );
    }

    fn message(text: &str) -> EventKind {
        EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    fn rename(session_id: SessionId, name: &str) -> Action {
        Action::RenameSession {
            session_id,
            name: name.into(),
        }
    }

    fn row(f: &Fixture, id: SessionId) -> SessionRow {
        f.list_view()
            .sessions
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
    }

    #[test]
    fn default_title_is_the_first_prompt_else_the_directory_else_the_id() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app/");
        assert_eq!(
            (row(&f, sid).title, row(&f, sid).named),
            ("app".into(), false)
        );
        f.log(
            sid,
            vec![message("fix the flaky test\nin ci"), message("more")],
        );
        assert_eq!(row(&f, sid).title, "fix the flaky test");

        let bare = f.session("/");
        let short = bare.0.simple().to_string()[24..].to_owned();
        assert_eq!(row(&f, bare).title, short);
    }

    #[test]
    fn a_rename_shows_at_once_and_settles_from_the_log() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        // Normalised as the server will log it.
        let command = queued(&f.dispatch(rename(sid, "  flaky\ttests  ")));
        assert_eq!(
            command.kind,
            CommandKind::RenameSession {
                session_id: sid,
                name: Some("flakytests".into()),
            }
        );
        let shown = |f: &Fixture| {
            let row = row(f, sid);
            (row.title, row.named, row.rename)
        };
        assert_eq!(
            shown(&f),
            ("flakytests".into(), true, Some(Outgoing::Queued))
        );
        f.model.command_sending(command.id, Utc::now());
        assert_eq!(shown(&f).2, Some(Outgoing::Sending));
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(2) },
            Utc::now(),
        );
        // Accepted but not pulled yet: still newer than the replica.
        assert_eq!(
            shown(&f),
            ("flakytests".into(), true, Some(Outgoing::Accepted(Seq(2))))
        );
        f.log(
            sid,
            vec![EventKind::SessionRenamed {
                name: Some("flakytests".into()),
                by: f.me,
            }],
        );
        assert_eq!(shown(&f), ("flakytests".into(), true, None));

        // The session view and its timeline say the same.
        f.open(sid);
        let view = f.session_view();
        assert_eq!(
            (view.title.as_str(), view.named, view.rename),
            ("flakytests", true, None)
        );
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/app".into(),
                },
                Entry::Renamed {
                    name: Some("flakytests".into()),
                    by: DeviceRef {
                        id: f.me,
                        name: None,
                        me: true,
                    },
                }
            ]
        );

        // A blank name restores the default title.
        let command = queued(&f.dispatch(rename(sid, " \n ")));
        assert_eq!(
            command.kind,
            CommandKind::RenameSession {
                session_id: sid,
                name: None,
            }
        );
        let view = f.session_view();
        assert_eq!((view.title.as_str(), view.named), ("app", false));
    }

    #[test]
    fn a_failed_rename_drops_its_name() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        let command = queued(&f.dispatch(rename(sid, "gone")));
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        let row = row(&f, sid);
        assert_eq!((row.title.as_str(), row.named), ("app", false));
        assert_eq!(
            row.rename,
            Some(Outgoing::Failed(FailReason::Rejected(
                RejectReason::UnknownSession
            )))
        );
        assert!(
            f.dispatch(rename(SessionId::new(), "x")).is_empty(),
            "unknown here"
        );
    }

    #[test]
    fn renaming_keeps_the_list_order_and_is_not_unread() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        let new = f.session("/srv/new");
        f.log(new, vec![agent("done")]);
        let order = |f: &Fixture| -> Vec<SessionId> {
            f.list_view().sessions.iter().map(|row| row.id).collect()
        };
        assert_eq!(order(&f), vec![new, old]);
        let other = DeviceId::new();
        f.log(
            old,
            vec![EventKind::SessionRenamed {
                name: Some("older".into()),
                by: other,
            }],
        );
        assert_eq!(order(&f), vec![new, old], "a rename is not activity");
        assert_eq!(row(&f, old).title, "older");
        assert_eq!(unread(&f, old), 0);
        // Concurrent renames: the last in log order wins.
        f.log(
            old,
            vec![EventKind::SessionRenamed {
                name: Some("oldest".into()),
                by: f.me,
            }],
        );
        assert_eq!(row(&f, old).title, "oldest");
        assert_eq!(order(&f), vec![new, old]);
        // The alert names the session by its title too.
        f.log(old, vec![approval_requested(ApprovalId::new(), None)]);
        assert_eq!(f.model.view().attention.approvals[0].title, "oldest");
    }

    fn archive(session_id: SessionId, archived: bool) -> Action {
        Action::SetSessionArchived {
            session_id,
            archived,
        }
    }

    fn archived_event(archived: bool, by: DeviceId) -> EventKind {
        EventKind::SessionArchived { archived, by }
    }

    #[test]
    fn remove_worktree_queues_only_for_a_worktree_session() {
        let mut f = Fixture::paired();
        let plain = f.session("/srv/plain");
        assert!(
            f.dispatch(Action::RemoveWorktree { session_id: plain })
                .is_empty(),
            "a session without a worktree has none to remove"
        );

        let in_worktree = SessionId::new();
        f.log(
            in_worktree,
            vec![EventKind::SessionCreated {
                workdir: "/srv/app".into(),
                context_name: "start".into(),
                from: None,
                worktree: Some(skywave_proto::Worktree {
                    repo: "/srv/app".into(),
                    branch: "feat/x".into(),
                }),
            }],
        );
        let command = queued(&f.dispatch(Action::RemoveWorktree {
            session_id: in_worktree,
        }));
        assert_eq!(
            command.kind,
            CommandKind::RemoveWorktree {
                session_id: in_worktree,
            }
        );
    }

    /// The active and the archived list, each most recently active first.
    fn lists(f: &mut Fixture) -> (Vec<SessionId>, Vec<SessionId>) {
        let ids = |view: SessionListView| view.sessions.iter().map(|r| r.id).collect::<Vec<_>>();
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        let archived = f.list_view();
        assert_eq!(archived.kind, ListKind::Archived);
        f.dispatch(Action::Navigate(Route::SessionList));
        let active = f.list_view();
        assert_eq!(active.kind, ListKind::Active);
        assert_eq!(active.other_count, archived.sessions.len());
        assert_eq!(archived.other_count, active.sessions.len());
        (ids(active), ids(archived))
    }

    #[test]
    fn archiving_moves_the_row_at_once_and_settles_from_the_log() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        let new = f.session("/srv/new");
        let command = queued(&f.dispatch(archive(old, true)));
        assert_eq!(
            command.kind,
            CommandKind::SetSessionArchived {
                session_id: old,
                archived: true,
            }
        );
        assert_eq!(lists(&mut f), (vec![new], vec![old]));
        f.model.command_sending(command.id, Utc::now());
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(3) },
            Utc::now(),
        );
        // Accepted but not pulled yet: still newer than the replica.
        assert_eq!(lists(&mut f), (vec![new], vec![old]));
        f.log(old, vec![archived_event(true, f.me)]);
        assert_eq!(lists(&mut f), (vec![new], vec![old]));

        // An archived session opens and works as usual, and says so.
        f.open(old);
        assert!(f.session_view().archived);
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/old".into(),
                },
                Entry::Archived {
                    archived: true,
                    by: DeviceRef {
                        id: f.me,
                        name: None,
                        me: true,
                    },
                }
            ]
        );
        f.open(new);
        assert!(!f.session_view().archived);

        // Brought back from the archived list.
        f.dispatch(Action::Navigate(Route::SessionList));
        queued(&f.dispatch(archive(old, false)));
        assert_eq!(lists(&mut f), (vec![new, old], vec![]));
        assert!(
            f.dispatch(archive(SessionId::new(), true)).is_empty(),
            "unknown here"
        );
    }

    #[test]
    fn archiving_the_open_session_keeps_back_on_the_active_list() {
        let mut f = Fixture::paired();
        let open = f.session("/srv/open");
        f.dispatch(Action::Navigate(Route::SessionList));
        f.open(open);
        queued(&f.dispatch(archive(open, true)));
        assert!(f.session_view().archived);

        f.dispatch(Action::Back);
        assert_eq!(f.list_view().kind, ListKind::Active);
    }

    #[test]
    fn unarchiving_the_open_session_keeps_back_on_the_archived_list() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        queued(&f.dispatch(archive(old, true)));
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        f.open(old);
        queued(&f.dispatch(archive(old, false)));
        assert!(!f.session_view().archived);

        f.dispatch(Action::Back);
        assert_eq!(f.list_view().kind, ListKind::Archived);
    }

    #[test]
    fn the_all_list_shows_both_in_order_and_back_returns_to_it() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        let new = f.session("/srv/new");
        f.dispatch(Action::Navigate(Route::SessionList));
        let before: Vec<SessionId> = f.list_view().sessions.iter().map(|r| r.id).collect();
        assert_eq!(before, vec![new, old]);
        queued(&f.dispatch(archive(old, true)));

        f.dispatch(Action::Navigate(Route::AllSessions));
        let all = f.list_view();
        assert_eq!(all.kind, ListKind::All);
        assert_eq!(
            all.sessions.iter().map(|r| r.id).collect::<Vec<_>>(),
            before
        );
        assert_eq!(
            all.sessions.iter().map(|r| r.archived).collect::<Vec<_>>(),
            vec![false, true]
        );
        assert_eq!(all.other_count, 0);
        assert!(all.creating.is_empty());

        f.open(old);
        f.dispatch(Action::Back);
        assert_eq!(f.list_view().kind, ListKind::All);

        f.dispatch(Action::Navigate(Route::SessionList));
        f.open(old);
        f.dispatch(Action::Back);
        assert_eq!(f.list_view().kind, ListKind::Active);
    }

    #[test]
    fn re_pairing_forgets_the_all_list() {
        let mut f = Fixture::paired();
        let id = f.session("/srv/a");
        f.dispatch(Action::Navigate(Route::AllSessions));
        f.model.paired(paired_at(f.me, "host:7443"));
        f.open(id);
        f.dispatch(Action::Back);
        assert_eq!(f.list_view().kind, ListKind::Active);
    }

    #[test]
    fn deleting_the_open_session_shows_the_list_last_shown() {
        let mut f = Fixture::paired();
        let a = f.session("/srv/a");
        let b = f.session("/srv/b");
        let c = f.session("/srv/c");

        // Opened from the active list, deleted from another device.
        f.dispatch(Action::Navigate(Route::SessionList));
        f.open(a);
        f.model.session_deleted(a, &[]);
        assert_eq!(f.list_view().kind, ListKind::Active);

        queued(&f.dispatch(archive(b, true)));
        f.dispatch(Action::Navigate(Route::AllSessions));
        f.open(b);
        f.model.session_deleted(b, &[]);
        assert_eq!(f.list_view().kind, ListKind::All);

        queued(&f.dispatch(archive(c, true)));
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        f.open(c);
        f.model.session_deleted(c, &[]);
        assert_eq!(f.list_view().kind, ListKind::Archived);
    }

    #[test]
    fn a_removed_worktree_has_a_timeline_entry() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        let by = f.me;
        f.log(old, vec![EventKind::WorktreeRemoved { by }]);
        f.open(old);
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/old".into(),
                },
                Entry::WorktreeRemoved {
                    by: DeviceRef {
                        id: f.me,
                        name: None,
                        me: true,
                    },
                }
            ]
        );
    }

    #[test]
    fn a_written_file_has_a_timeline_entry() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        let by = f.me;
        f.log(
            sid,
            vec![
                EventKind::FileWritten {
                    path: "docs/a.txt".into(),
                    sha256: ContentHash::of(b"a"),
                    size: 1,
                    replaced: None,
                    by,
                },
                EventKind::FileWritten {
                    path: "docs/a.txt".into(),
                    sha256: ContentHash::of(b"b"),
                    size: 1,
                    replaced: Some(ContentHash::of(b"old")),
                    by,
                },
            ],
        );
        f.open(sid);
        let by = DeviceRef {
            id: f.me,
            name: None,
            me: true,
        };
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/app".into(),
                },
                Entry::FileWritten {
                    path: "docs/a.txt".into(),
                    size: 1,
                    replaced: false,
                    by: by.clone(),
                },
                Entry::FileWritten {
                    path: "docs/a.txt".into(),
                    size: 1,
                    replaced: true,
                    by,
                },
            ]
        );
    }

    #[test]
    fn a_failed_archive_reverts() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        let command = queued(&f.dispatch(archive(sid, true)));
        assert_eq!(lists(&mut f), (vec![], vec![sid]));
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        assert_eq!(lists(&mut f), (vec![sid], vec![]));
        // The replica's value stands: archived elsewhere, still archived.
        f.log(sid, vec![archived_event(true, DeviceId::new())]);
        assert_eq!(lists(&mut f), (vec![], vec![sid]));
    }

    #[test]
    fn a_later_message_brings_the_session_back() {
        let send = |sid: SessionId, text: &str| Action::SendMessage {
            session_id: sid,
            text: text.into(),
        };
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        // Archive, then a message, both still queued: active, as the log
        // will have it.
        queued(&f.dispatch(archive(sid, true)));
        let sent = queued(&f.dispatch(send(sid, "more")));
        assert_eq!(lists(&mut f), (vec![sid], vec![]));
        // A failed message brings nothing back.
        f.model.command_result(
            sent.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        assert_eq!(lists(&mut f), (vec![], vec![sid]));

        // Archived by another device: a message queued here brings it back
        // at once.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        f.log(sid, vec![archived_event(true, DeviceId::new())]);
        assert_eq!(lists(&mut f), (vec![], vec![sid]));
        queued(&f.dispatch(send(sid, "back")));
        assert_eq!(lists(&mut f), (vec![sid], vec![]));
        f.open(sid);
        assert!(!f.session_view().archived);

        // A message from any device in the log does the same.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        f.log(sid, vec![archived_event(true, DeviceId::new())]);
        f.log(sid, vec![message("from the phone")]);
        assert_eq!(lists(&mut f), (vec![sid], vec![]));
        // Archiving after a message queued earlier wins.
        queued(&f.dispatch(send(sid, "first")));
        queued(&f.dispatch(archive(sid, true)));
        assert_eq!(lists(&mut f), (vec![], vec![sid]));
    }

    #[test]
    fn a_later_shell_run_brings_the_session_back() {
        let run = |sid: SessionId| Action::RunShell {
            session_id: sid,
            command: "make test".into(),
            when: RunWhen::Always,
        };
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        // Archive, then a run, both still queued: active, as the log will
        // have it once `ShellRequested` is in.
        queued(&f.dispatch(archive(sid, true)));
        let sent = queued(&f.dispatch(run(sid)));
        assert_eq!(lists(&mut f), (vec![sid], vec![]));
        // A refused run brings nothing back.
        f.model.command_result(
            sent.id,
            CommandResult::Rejected {
                reason: RejectReason::ShellDisabled,
            },
            Utc::now(),
        );
        assert_eq!(lists(&mut f), (vec![], vec![sid]));

        // Archived by another device: a run queued here brings it back at
        // once.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        f.log(sid, vec![archived_event(true, DeviceId::new())]);
        assert_eq!(lists(&mut f), (vec![], vec![sid]));
        queued(&f.dispatch(run(sid)));
        assert_eq!(lists(&mut f), (vec![sid], vec![]));
        f.open(sid);
        assert!(!f.session_view().archived);

        // A stop is no run: it brings nothing back.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        f.log(sid, vec![archived_event(true, DeviceId::new())]);
        queued(&f.dispatch(Action::StopShell {
            session_id: sid,
            run: CommandId::new(),
        }));
        assert_eq!(lists(&mut f), (vec![], vec![sid]));
    }

    #[test]
    fn archiving_keeps_the_order_is_not_unread_and_approvals_still_count() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        let mid = f.session("/srv/mid");
        let new = f.session("/srv/new");
        f.log(old, vec![message("go"), agent("done")]);
        f.log(new, vec![agent("done")]);
        assert_eq!(lists(&mut f).0, vec![old, new, mid]);
        let other = DeviceId::new();
        f.log(old, vec![archived_event(true, other)]);
        f.log(mid, vec![archived_event(true, other)]);
        assert_eq!(lists(&mut f), (vec![new], vec![old, mid]));
        assert_eq!(unread(&f, new), 1);
        f.log(old, vec![archived_event(false, other)]);
        assert_eq!(
            lists(&mut f),
            (vec![old, new], vec![mid]),
            "archiving is not activity"
        );
        assert_eq!(unread(&f, old), 1, "nor news");

        // An approval in an archived session still alerts, and Ctrl-G opens
        // it; its row carries the count.
        let approval = ApprovalId::new();
        f.log(mid, vec![approval_requested(approval, None)]);
        assert_eq!(lists(&mut f).1, vec![mid]);
        let attention = f.model.view().attention;
        assert_eq!(attention.approvals[0].session_id, mid);
        assert_eq!(attention.elsewhere, 1);
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        assert_eq!(row(&f, mid).pending_approvals, 1);
        f.dispatch(Action::OpenNextApproval);
        assert_eq!(f.model.open_session(), Some(mid));
    }

    #[test]
    fn only_a_user_message_moves_a_session_up_the_list() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        let new = f.session("/srv/new");
        f.log(
            old,
            vec![
                agent("working"),
                approval_requested(ApprovalId::new(), None),
            ],
        );
        assert_eq!(lists(&mut f).0, vec![new, old], "the agent's activity");
        f.log(old, vec![message("go on")]);
        assert_eq!(lists(&mut f).0, vec![old, new]);
        f.log(new, vec![agent("done")]);
        assert_eq!(lists(&mut f).0, vec![old, new]);
    }

    fn usage(f: &Fixture) -> UsageView {
        match f.model.view().screen {
            Screen::Usage(view) => view,
            other => panic!("expected the usage screen, got {other:?}"),
        }
    }

    #[test]
    fn the_usage_screen_lists_every_turn_until_its_session_is_deleted() {
        let mut f = Fixture::paired();
        let kept = f.session("/srv/kept");
        let gone = f.session("/srv/gone");
        let priced = crate::TurnUsage {
            cost_usd: Some(0.5),
            ..crate::TurnUsage::default()
        };
        f.log(kept, vec![EventKind::TurnUsage(priced.clone())]);
        f.log(
            gone,
            vec![EventKind::TurnUsage(crate::TurnUsage::default())],
        );
        let by = f.me;
        f.log(
            gone,
            vec![EventKind::SessionArchived { archived: true, by }],
        );

        f.dispatch(Action::Navigate(Route::Usage));
        let view = usage(&f);
        let turns: Vec<_> = view.turns.iter().map(|t| (t.seq, t.session_id)).collect();
        assert_eq!(turns, vec![(Seq(3), kept), (Seq(4), gone)]);
        assert_eq!(view.turns[0].usage, priced);
        let archived: Vec<_> = view.sessions.iter().map(|s| (s.id, s.archived)).collect();
        assert_eq!(archived, vec![(gone, true), (kept, false)]);
        assert_eq!(view.deleted, 0);

        f.model.session_deleted(gone, &[]);
        let view = usage(&f);
        assert_eq!(view.turns.len(), 1);
        assert_eq!(view.sessions.len(), 1);
        assert_eq!(view.deleted, 1);
    }

    fn delete(session_id: SessionId) -> Action {
        Action::DeleteSessionLocally { session_id }
    }

    /// Whether the archived list offers to delete the session, and whether
    /// the action would.
    fn deletable(f: &mut Fixture, id: SessionId) -> bool {
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        let shown = row(f, id).deletable;
        let effects = f.dispatch(delete(id));
        assert_eq!(
            effects.is_empty(),
            !shown,
            "the row says what the action does"
        );
        shown
    }

    #[test]
    fn delete_requires_archived() {
        let mut f = Fixture::paired();
        let gone = f.session("/srv/gone");
        let kept = f.session("/srv/kept");
        assert!(f.dispatch(delete(gone)).is_empty(), "active");
        assert!(f.dispatch(delete(SessionId::new())).is_empty(), "unknown");
        assert!(!row(&f, gone).deletable);

        // This device's archiving counts once it is on its way.
        queued(&f.dispatch(archive(gone, true)));
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        assert!(!row(&f, gone).deletable, "but it is not answered yet");
        let command = f.model.outbox[0].command.id;
        f.model
            .command_result(command, CommandResult::Accepted { seq: Seq(3) }, minute(1));
        assert!(deletable(&mut f, gone));
        assert_eq!(f.dispatch(delete(gone)), vec![Effect::DeleteLocally(gone)]);

        // As the core does once the store deleted it: it is gone, with its
        // answered commands, and opened, its list shows instead.
        f.open(gone);
        f.model.session_deleted(gone, &[]);
        assert!(f.model.outbox.is_empty());
        assert_eq!(f.list_view().kind, ListKind::Archived);
        assert_eq!(lists(&mut f), (vec![kept], vec![]));

        // Later activity on it elsewhere never brings it back nor alerts.
        f.model.set_link(ONLINE);
        f.log(gone, vec![archived_event(true, f.me)]);
        f.log(
            gone,
            vec![message("back"), approval_requested(ApprovalId::new(), None)],
        );
        assert_eq!(f.model.cursor, Seq(5));
        assert_eq!(lists(&mut f), (vec![kept], vec![]));
        assert!(f.model.view().attention.approvals.is_empty());
        assert_eq!(alerts(&f), 0);
        assert!(f.dispatch(Action::OpenSession(gone)).is_empty());

        // And a restart keeps it gone.
        let mut model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
        model.apply_batch(&EventBatch {
            from: Seq(6),
            events: vec![Event {
                session_id: gone,
                at: minute(9),
                command_id: Some(CommandId::new()),
                context: skywave_proto::ContextId::first_of(gone),
                kind: message("again"),
            }],
        });
        assert_eq!(
            model.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            [kept]
        );
        assert_eq!(model.cursor, Seq(6));
    }

    /// Whether the archived list offers to delete the session everywhere.
    fn shown_everywhere(f: &mut Fixture, id: SessionId) -> bool {
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        row(f, id).deletable_everywhere
    }

    #[test]
    fn delete_everywhere_needs_archived_and_no_worktree() {
        let mut f = Fixture::paired();
        let active = f.session("/srv/active");
        assert!(!row(&f, active).deletable_everywhere);
        assert!(
            f.dispatch(Action::DeleteSessionEverywhere { session_id: active })
                .is_empty(),
            "active"
        );

        // Archived as the log has it, so it may go everywhere.
        let gone = f.session("/srv/gone");
        f.log(gone, vec![archived_event(true, f.me)]);
        assert!(shown_everywhere(&mut f, gone));
        let command = queued(&f.dispatch(Action::DeleteSessionEverywhere { session_id: gone }));
        assert_eq!(
            command.kind,
            CommandKind::DeleteSession { session_id: gone }
        );
        // Until the server answers, it is neither offered nor sent again.
        assert!(!shown_everywhere(&mut f, gone));
        assert!(
            f.dispatch(Action::DeleteSessionEverywhere { session_id: gone })
                .is_empty(),
            "sent once"
        );

        // Its worktree is still on disk: the server would refuse the delete.
        let in_worktree = SessionId::new();
        f.log(in_worktree, vec![created_in_worktree("/srv/app")]);
        f.log(in_worktree, vec![archived_event(true, f.me)]);
        assert!(!shown_everywhere(&mut f, in_worktree));
        let shown = row(&f, in_worktree);
        assert!(shown.deletable, "locally it could go");
        assert!(
            f.dispatch(Action::DeleteSessionEverywhere {
                session_id: in_worktree
            })
            .is_empty(),
            "but not while the worktree is there"
        );

        // Removed, it may go everywhere.
        f.log(in_worktree, vec![EventKind::WorktreeRemoved { by: f.me }]);
        assert!(shown_everywhere(&mut f, in_worktree));
        let command = queued(&f.dispatch(Action::DeleteSessionEverywhere {
            session_id: in_worktree,
        }));
        assert_eq!(
            command.kind,
            CommandKind::DeleteSession {
                session_id: in_worktree
            }
        );
    }

    #[test]
    fn events_after_a_tombstone_in_its_batch_are_skipped() {
        let mut f = Fixture::paired();
        let gone = f.session("/srv/gone");
        let blob = BlobRef {
            id: BlobId::new(),
            size: 10_000,
            sha256: skywave_proto::ContentHash::of(b"out"),
        };
        let event = |kind| Event {
            session_id: gone,
            at: minute(2),
            command_id: None,
            context: skywave_proto::ContextId::first_of(gone),
            kind,
        };
        // As the store does, the tombstone ends the session for the rest of
        // its batch: the output and the approval after it register nothing.
        let from = f.model.cursor.next();
        let batch = EventBatch {
            from,
            events: vec![
                event(message("before")),
                event(EventKind::SessionDeleted { by: f.me }),
                event(EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "out".into(),
                    lines: 1,
                    output: Some(Full::Blob(blob.clone())),
                })),
                event(approval_requested(ApprovalId::new(), None)),
            ],
        };
        let alerted = f.model.apply_batch(&batch);
        assert_eq!(f.model.cursor, Seq(from.0 + 3), "the batch is folded");
        f.model.session_deleted(gone, &[]);
        assert!(alerted.is_empty(), "nothing after the tombstone alerts");
        assert!(
            !f.model.blobs.contains_key(&blob.id),
            "nor registers a blob"
        );
        assert!(!f.model.sessions.iter().any(|s| s.id == gone));
    }

    #[test]
    fn delete_refused_while_pending_or_running() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        f.log(sid, vec![archived_event(true, DeviceId::new())]);
        assert!(deletable(&mut f, sid));

        // A pending approval, until answered anywhere.
        let approval = ApprovalId::new();
        f.log(sid, vec![approval_requested(approval, None)]);
        assert!(!deletable(&mut f, sid));
        f.log(sid, vec![resolved(approval)]);
        assert!(deletable(&mut f, sid));

        // A running turn, until it ends.
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        assert!(!deletable(&mut f, sid));
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert!(deletable(&mut f, sid));

        // A command from this device, until the server answers it; a
        // refused one counts as answered.
        let command = queued(&f.dispatch(rename(sid, "old work")));
        assert!(!deletable(&mut f, sid));
        f.model.command_sending(command.id, minute(1));
        assert!(!deletable(&mut f, sid));
        let refused = CommandResult::Rejected {
            reason: RejectReason::UnknownSession,
        };
        f.model.command_result(command.id, refused, minute(2));
        assert!(deletable(&mut f, sid));
        // Another session's command does not hold it.
        let other = f.session("/srv/other");
        queued(&f.dispatch(Action::Interrupt { session_id: other }));
        assert!(deletable(&mut f, sid));
    }

    #[test]
    fn blob_states_follow_download_progress() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let output = BlobRef {
            id: BlobId::new(),
            size: 10_000,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        f.log(
            sid,
            vec![
                EventKind::ToolCall(ToolCall {
                    parent: None,
                    call: None,
                    name: "bash".into(),
                    display: ToolDisplay {
                        kind: ToolKind::Other,
                        title: "cargo test".into(),
                        detail: None,
                        body: None,
                    },
                    input: None,
                }),
                EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "40 passed".into(),
                    lines: 1,
                    output: Some(Full::Blob(output.clone())),
                }),
            ],
        );
        f.open(sid);
        let output_state = |f: &Fixture| match f.session_view().timeline.last() {
            Some(TimelineItem::Logged {
                entry:
                    Entry::ToolResult {
                        output: Some(FullView::Blob(blob)),
                        ..
                    },
                ..
            }) => blob.state.clone(),
            other => panic!("expected a tool result, got {other:?}"),
        };
        assert_eq!(output_state(&f), BlobState::NotDownloaded { size: 10_000 });

        assert!(f.dispatch(Action::DownloadBlob(BlobId::new())).is_empty());
        assert_eq!(
            f.dispatch(Action::DownloadBlob(output.id)),
            vec![Effect::Request(Request::DownloadBlob(output.id))]
        );
        // Asked for, it says so until a worker takes it.
        assert_eq!(output_state(&f), BlobState::Requested { size: 10_000 });

        // A given-up download says why; asking again requests it again.
        f.model.blob_status(
            output.id,
            BlobStatus::Failed {
                reason: "blob size mismatch".to_owned(),
            },
        );
        assert_eq!(
            output_state(&f),
            BlobState::Failed {
                size: 10_000,
                reason: "blob size mismatch".into()
            }
        );
        assert_eq!(
            f.dispatch(Action::DownloadBlob(output.id)),
            vec![Effect::Request(Request::DownloadBlob(output.id))]
        );
        assert_eq!(output_state(&f), BlobState::Requested { size: 10_000 });

        // Paused follows the link, with no status change.
        f.model.set_link(PairedLink::Online { latency_ms: 20 });
        let path = PathBuf::from("/data/blobs/x");
        f.model.blob_status(
            output.id,
            BlobStatus::Downloading {
                got: 4096,
                path: path.clone(),
            },
        );
        let downloading = |paused| BlobState::Downloading {
            got: 4096,
            size: 10_000,
            paused,
        };
        assert_eq!(output_state(&f), downloading(None));
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(output_state(&f), downloading(Some(Pause::Offline)));
        // A full disk says so over the link being down, until resumed.
        f.model.set_blobs_disk_full(true);
        assert_eq!(output_state(&f), downloading(Some(Pause::DiskFull)));
        f.model.set_blobs_disk_full(false);
        assert_eq!(output_state(&f), downloading(Some(Pause::Offline)));
        f.model.set_link(PairedLink::CatchingUp {
            have: Seq(1),
            server: Seq(2),
        });
        assert_eq!(output_state(&f), downloading(None));
        f.model.set_blobs_disk_full(true);
        assert_eq!(output_state(&f), downloading(Some(Pause::DiskFull)));
        f.model.set_blobs_disk_full(false);
        // A running download asked for again keeps showing its progress.
        assert_eq!(
            f.dispatch(Action::DownloadBlob(output.id)),
            vec![Effect::Request(Request::DownloadBlob(output.id))]
        );
        assert_eq!(output_state(&f), downloading(None));
        f.model
            .blob_status(output.id, BlobStatus::Available { path: path.clone() });
        assert_eq!(output_state(&f), BlobState::Available { path });
        assert!(f.dispatch(Action::DownloadBlob(output.id)).is_empty());
    }

    #[test]
    fn snippets_are_saved_used_and_deleted_on_any_screen() {
        let mut f = Fixture::paired();
        let save = |name: &str, text: &str| Action::SaveSnippet {
            name: name.into(),
            text: text.into(),
            kind: ComposerMode::Agent,
        };
        let [Effect::SaveSnippet(tests)] = &f.dispatch(save("tests", "run the tests"))[..] else {
            panic!("one snippet to store");
        };
        let tests = tests.clone();
        assert!(f.dispatch(save("", "  ")).is_empty());
        let [Effect::SaveSnippet(commit)] = &f.dispatch(save("", "commit this"))[..] else {
            panic!("one snippet to store");
        };
        let commit = commit.clone();
        let texts = |f: &Fixture| -> Vec<String> {
            f.model
                .view()
                .snippets
                .into_iter()
                .map(|s| s.text)
                .collect()
        };
        assert_eq!(texts(&f), ["commit this", "run the tests"]);

        let used = f.dispatch(Action::UseSnippet(tests.id));
        assert!(matches!(&used[..], [Effect::SaveSnippet(s)] if s.id == tests.id));
        assert_eq!(texts(&f), ["run the tests", "commit this"]);

        assert_eq!(
            f.dispatch(Action::DeleteSnippet(commit.id)),
            vec![Effect::DeleteSnippet(commit.id)]
        );
        assert!(f.dispatch(Action::DeleteSnippet(commit.id)).is_empty());

        let edit = Action::EditSnippet {
            id: tests.id,
            name: "".into(),
            text: "run every test".into(),
        };
        let edited = f.dispatch(edit);
        assert!(matches!(&edited[..], [Effect::SaveSnippet(s)] if s.name.is_none()));
        let texts = |f: &Fixture| -> Vec<String> {
            f.model
                .view()
                .snippets
                .into_iter()
                .map(|s| s.text)
                .collect()
        };
        assert_eq!(texts(&f), ["run every test"]);

        // A reload shows what the store holds.
        let stored = snapshot(&f.model);
        f.model.reload(stored);
        assert_eq!(texts(&f), ["run every test"]);
    }

    fn save_draft(session_id: SessionId, text: &str, cursor: usize) -> Action {
        Action::SaveDraft {
            session_id,
            mode: ComposerMode::Agent,
            text: text.into(),
            cursor,
            when: RunWhen::Always,
        }
    }

    #[test]
    fn save_draft_is_shown_and_send_clears_it() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let other = f.session("/srv/b");
        let draft = Draft {
            text: "héllo".into(),
            cursor: 5,
            when: RunWhen::Always,
        };
        // The cursor is clamped to the text's chars.
        assert_eq!(
            f.dispatch(save_draft(sid, "héllo", 99)),
            vec![Effect::SaveDraft(
                (sid, ComposerMode::Agent),
                Some(draft.clone())
            )]
        );
        let rows = f.list_view().sessions;
        let has_draft = |id| rows.iter().find(|r| r.id == id).unwrap().has_draft;
        assert!(has_draft(sid) && !has_draft(other));
        f.open(sid);
        assert_eq!(f.session_view().draft, Some(draft));

        queued(&f.dispatch(Action::SendMessage {
            session_id: sid,
            text: "héllo".into(),
        }));
        assert_eq!(f.session_view().draft, None, "the enqueue drops it");

        // Whitespace is no draft: saving it deletes the stored one.
        f.dispatch(save_draft(sid, "x", 1));
        assert_eq!(
            f.dispatch(save_draft(sid, " \n ", 2)),
            vec![Effect::SaveDraft((sid, ComposerMode::Agent), None)]
        );
        assert_eq!(f.session_view().draft, None);
    }

    #[test]
    fn unchanged_draft_emits_no_effect() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        assert!(
            f.dispatch(save_draft(sid, "   ", 0)).is_empty(),
            "blank over no draft writes nothing"
        );
        assert!(
            f.dispatch(save_draft(SessionId::new(), "x", 1)).is_empty(),
            "unknown session"
        );
        assert_eq!(f.dispatch(save_draft(sid, "abc", 3)).len(), 1);
        assert!(f.dispatch(save_draft(sid, "abc", 3)).is_empty());
        assert_eq!(
            f.dispatch(save_draft(sid, "abc", 1)),
            vec![Effect::SaveDraft(
                (sid, ComposerMode::Agent),
                Some(Draft {
                    text: "abc".into(),
                    cursor: 1,
                    when: RunWhen::Always,
                })
            )],
            "a moved cursor is a change"
        );
    }

    #[test]
    fn a_new_server_address_is_saved_and_reconnects() {
        let mut model = Model::new(empty_snapshot(), None);
        let set = |address: &str| Action::SetServerAddress {
            address: address.into(),
        };
        assert!(model.dispatch(set("10.0.0.7:7443")).is_empty(), "unpaired");

        let mut f = Fixture::paired();
        f.model.paired(paired_at(f.me, "host:7443"));
        f.model
            .set_link(PairedLink::Rejected(LinkRejection::Revoked));
        assert!(f.dispatch(set(" host:7443 ")).is_empty(), "unchanged");
        assert!(
            f.dispatch(set("HOST")).is_empty(),
            "unchanged, spelt otherwise"
        );
        assert!(f.dispatch(set("not an address:x")).is_empty(), "invalid");
        assert!(f.dispatch(set("  ")).is_empty(), "blank");
        let mut switched = ServerAddresses::new(addr("host:7443"));
        switched.switch_to(&addr("10.0.0.7:7443"));
        assert_eq!(
            f.dispatch(set(" 10.0.0.7:7443 ")),
            vec![
                Effect::SaveServerAddresses(switched),
                Effect::Request(Request::Connect),
            ]
        );
        let view = f.model.view();
        assert_eq!(view.link, Link::Connecting);
        f.dispatch(Action::Navigate(Route::Settings));
        match f.model.view().screen {
            Screen::Settings(settings) => {
                assert_eq!(settings.server_address, Some(addr("10.0.0.7:7443")));
                assert_eq!(settings.other_addresses, addrs(&["host:7443"]));
                assert_eq!(settings.device_id, Some(f.me));
            }
            other => panic!("expected settings, got {other:?}"),
        }
    }

    #[test]
    fn a_left_address_is_kept_to_switch_back_to_or_forget() {
        let addresses = |f: &Fixture| f.model.paired.as_ref().unwrap().addresses.clone();
        let mut f = Fixture::paired();
        f.model.paired(paired_at(f.me, "home:7443"));
        f.dispatch(Action::SetServerAddress {
            address: "vpn:7443".into(),
        });
        f.dispatch(Action::SetServerAddress {
            address: "lan:7443".into(),
        });
        assert_eq!(addresses(&f).others(), addrs(&["home:7443", "vpn:7443"]));

        // Back to one under another spelling: the same address, not a new one.
        f.dispatch(Action::SetServerAddress {
            address: "HOME".into(),
        });
        assert_eq!(addresses(&f).current(), &addr("home:7443"));
        assert_eq!(addresses(&f).others(), addrs(&["vpn:7443", "lan:7443"]));
        f.dispatch(Action::SetServerAddress {
            address: "lan".into(),
        });

        let forget = |address: &str| Action::ForgetServerAddress {
            address: address.into(),
        };
        assert!(f.dispatch(forget("LAN")).is_empty(), "the current one");
        assert!(f.dispatch(forget("other:7443")).is_empty(), "not kept");
        let forgotten = ServerAddresses::with_others(addr("lan"), addrs(&["home:7443"]));
        assert_eq!(
            f.dispatch(forget(" VPN ")),
            vec![Effect::SaveServerAddresses(forgotten.clone())]
        );
        assert_eq!(addresses(&f), forgotten);

        // A failed write puts back what is stored.
        f.model.addresses_stored(ServerAddresses::with_others(
            addr("lan"),
            addrs(&["vpn", "home"]),
        ));
        assert_eq!(addresses(&f).others(), addrs(&["vpn:7443", "home:7443"]));
        f.model.addresses_stored(forgotten);

        f.dispatch(Action::Navigate(Route::ServerAddresses));
        assert_eq!(
            f.model.view().screen,
            Screen::ServerAddresses(ServerAddressesView {
                current: Some(addr("lan:7443")),
                others: addrs(&["home:7443"]),
            })
        );

        // Unpaired, there are no addresses to forget.
        let mut model = Model::new(empty_snapshot(), None);
        assert!(model.dispatch(forget("home")).is_empty());
    }

    #[test]
    fn blob_policy_setting_and_navigation() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::Settings));
        let settings = |f: &Fixture| match f.model.view().screen {
            Screen::Settings(view) => view,
            other => panic!("expected settings, got {other:?}"),
        };
        assert_eq!(settings(&f).blob_policy, BlobPolicy::OnDemand);
        assert_eq!(settings(&f).device_id, Some(f.me));
        assert_eq!(
            settings(&f).server_fingerprint,
            Some(Fingerprint::of_der(b"ca"))
        );
        assert!(
            f.dispatch(Action::SetBlobPolicy(BlobPolicy::OnDemand))
                .is_empty()
        );
        assert_eq!(
            f.dispatch(Action::SetBlobPolicy(BlobPolicy::Eager)),
            vec![
                Effect::SaveBlobPolicy(BlobPolicy::Eager),
                Effect::Request(Request::BlobPolicyChanged(BlobPolicy::Eager)),
            ]
        );
        assert_eq!(settings(&f).blob_policy, BlobPolicy::Eager);

        // Unknown sessions do not open; known ones do, and back is a route.
        assert!(f.dispatch(Action::OpenSession(SessionId::new())).is_empty());
        let sid = f.session("/srv/a");
        f.open(sid);
        assert_eq!(f.session_view().workdir.as_deref(), Some("/srv/a"));
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(f.list_view().sessions[0].id, sid);
    }

    #[test]
    fn new_session_defaults_are_saved_shown_and_sent_on_create() {
        let mut f = Fixture::paired();
        assert_eq!(
            f.dispatch(Action::SetDefaultMode(Some(PermissionMode::Plan))),
            vec![Effect::SaveDefaultMode(Some(PermissionMode::Plan))]
        );
        assert!(
            f.dispatch(Action::SetDefaultMode(Some(PermissionMode::Plan)))
                .is_empty()
        );
        assert_eq!(
            f.dispatch(Action::SetDefaultSafetyCheck(true)),
            vec![Effect::SaveDefaultSafetyCheck(true)]
        );

        f.dispatch(Action::Navigate(Route::Settings));
        match f.model.view().screen {
            Screen::Settings(view) => {
                assert_eq!(view.default_mode, Some(PermissionMode::Plan));
                assert!(view.default_safety_check);
            }
            other => panic!("expected settings, got {other:?}"),
        }
        f.dispatch(Action::Navigate(Route::SessionList));

        let command = queued(
            &f.dispatch(Action::CreateSession {
                conversation: None,
                workdir: "/srv/a".into(),
                worktree: None,
            })[..2],
        );
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: created_id(&command),
                workdir: "/srv/a".into(),
                conversation: None,
                worktree: None,
                mode: Some(PermissionMode::Plan),
                safety_check: true,
            }
        );

        // The server refuses a mode with an adopted conversation.
        let command = queued(
            &f.dispatch(Action::CreateSession {
                conversation: Some("abc".into()),
                workdir: "/srv/a".into(),
                worktree: None,
            })[..2],
        );
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: created_id(&command),
                workdir: "/srv/a".into(),
                conversation: Some("abc".into()),
                worktree: None,
                mode: None,
                safety_check: true,
            }
        );
    }

    #[test]
    fn a_bypass_default_is_sent_only_where_the_server_allows_it() {
        let mut f = Fixture::paired();
        f.dispatch(Action::SetDefaultMode(Some(PermissionMode::Bypass)));
        let mode_sent = |f: &mut Fixture| match queued(
            &f.dispatch(Action::CreateSession {
                conversation: None,
                workdir: "/srv/a".into(),
                worktree: None,
            })[..2],
        )
        .kind
        {
            CommandKind::CreateSession { mode, .. } => mode,
            other => panic!("expected a create, got {other:?}"),
        };
        assert_eq!(mode_sent(&mut f), None);
        f.model.server_facts(ServerFacts {
            bypass_allowed: true,
            ..Default::default()
        });
        assert_eq!(mode_sent(&mut f), Some(PermissionMode::Bypass));
    }

    #[test]
    fn a_session_created_like_another_starts_with_the_defaults() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/a");
        f.dispatch(Action::SetDefaultMode(Some(PermissionMode::AcceptEdits)));
        f.dispatch(Action::SetDefaultSafetyCheck(true));
        f.open(old);
        let effects = f.dispatch(Action::CreateSessionLike(old));
        let command = queued(&effects[..2]);
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: created_id(&command),
                workdir: "/srv/a".into(),
                conversation: None,
                worktree: None,
                mode: Some(PermissionMode::AcceptEdits),
                safety_check: true,
            }
        );
    }

    #[test]
    fn created_session_is_pending_until_it_reaches_the_replica() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        assert!(
            f.dispatch(Action::CreateSession {
                conversation: None,
                workdir: "   ".into(),
                worktree: None,
            })
            .is_empty()
        );
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: " /srv/new ".into(),
            worktree: None,
        });
        let command = queued(&effects[..2]);
        let new = created_id(&command);
        assert_eq!(effects.last(), Some(&Effect::LoadSession(new)));
        // The new session opens at once: the list is read after leaving it.
        f.dispatch(Action::Navigate(Route::SessionList));
        let creating = |f: &Fixture| f.list_view().creating;
        assert_eq!(
            creating(&f),
            vec![PendingSession {
                command_id: command.id,
                session_id: new,
                workdir: "/srv/new".into(),
                conversation: None,
                worktree: None,
                outgoing: Outgoing::Queued,
                discardable: true,
            }]
        );
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(2) },
            Utc::now(),
        );
        assert_eq!(creating(&f)[0].outgoing, Outgoing::Accepted(Seq(2)));
        assert!(!creating(&f)[0].discardable, "accepted is final");
        assert!(f.dispatch(Action::Discard(command.id)).is_empty());

        f.log_commands(
            new,
            vec![(
                Some(command.id),
                EventKind::SessionCreated {
                    workdir: "/srv/new".into(),
                    context_name: "start".into(),
                    from: None,
                    worktree: None,
                },
            )],
        );
        let list = f.list_view();
        assert_eq!(list.creating, Vec::new());
        let ids: Vec<SessionId> = list.sessions.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new, old], "most recently active first");
        assert_eq!(list.sessions[0].state, SessionState::Idle);
    }

    #[test]
    fn a_session_created_like_another_opens_at_once() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/a");
        let list = f.session("/srv/b");
        // Only from the session on screen or a session list.
        f.dispatch(Action::Navigate(Route::Settings));
        assert!(f.dispatch(Action::CreateSessionLike(old)).is_empty());
        f.open(list);
        assert!(f.dispatch(Action::CreateSessionLike(old)).is_empty());
        f.open(old);
        let effects = f.dispatch(Action::CreateSessionLike(old));
        let command = queued(&effects[..2]);
        let new = created_id(&command);
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: new,
                workdir: "/srv/a".into(),
                conversation: None,
                worktree: None,
                mode: None,
                safety_check: false,
            }
        );
        assert_eq!(effects.last(), Some(&Effect::LoadSession(new)));
        assert_eq!(f.model.open_session(), Some(new));
        let creating = f
            .session_view()
            .creating
            .expect("the new session is creating");
        assert_eq!(creating.outgoing, Outgoing::Queued);

        // Another device's session in the same place is not it.
        f.session("/srv/a");
        assert_eq!(f.model.open_session(), Some(new));

        // Logged under the id it names: it stays open, no longer creating.
        let created = EventKind::SessionCreated {
            workdir: "/srv/a".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        f.log_commands(new, vec![(Some(command.id), created.clone())]);
        assert_eq!(f.model.open_session(), Some(new));
        assert_eq!(f.session_view().creating, None);

        // Gone elsewhere meanwhile: stays there.
        f.open(old);
        let effects = f.dispatch(Action::CreateSessionLike(old));
        let command = queued(&effects[..2]);
        f.open(list);
        f.log_commands(
            created_id(&command),
            vec![(Some(command.id), created.clone())],
        );
        assert_eq!(f.model.open_session(), Some(list));

        // From a row of the session list: opens it too.
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSessionLike(old));
        let command = queued(&effects[..2]);
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: created_id(&command),
                workdir: "/srv/a".into(),
                conversation: None,
                worktree: None,
                mode: None,
                safety_check: false,
            }
        );
        assert_eq!(f.model.open_session(), Some(created_id(&command)));

        // Left the list meanwhile: stays where it went.
        f.dispatch(Action::Navigate(Route::ArchivedSessions));
        let effects = f.dispatch(Action::CreateSessionLike(old));
        let command = queued(&effects[..2]);
        f.dispatch(Action::Navigate(Route::Settings));
        f.log_commands(created_id(&command), vec![(Some(command.id), created)]);
        assert!(matches!(f.model.view().screen, Screen::Settings(_)));
    }

    #[test]
    fn a_session_created_from_the_dialog_opens_at_once() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            workdir: "/srv/new".into(),
            conversation: None,
            worktree: None,
        });
        let command = queued(&effects[..2]);
        let new = created_id(&command);
        assert_eq!(effects.last(), Some(&Effect::LoadSession(new)));
        assert_eq!(f.model.open_session(), Some(new));
        let creating = f
            .session_view()
            .creating
            .expect("the new session is creating");
        assert_eq!(creating.outgoing, Outgoing::Queued);

        // Another device's session in the same place is not it.
        f.session("/srv/new");
        assert_eq!(f.model.open_session(), Some(new));

        let created = EventKind::SessionCreated {
            workdir: "/srv/new".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        };
        f.log_commands(new, vec![(Some(command.id), created.clone())]);
        assert_eq!(f.model.open_session(), Some(new));

        // Gone elsewhere meanwhile: stays there.
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            workdir: "/srv/new".into(),
            conversation: None,
            worktree: None,
        });
        let command = queued(&effects[..2]);
        f.dispatch(Action::Navigate(Route::Settings));
        f.log_commands(created_id(&command), vec![(Some(command.id), created)]);
        assert!(matches!(f.model.view().screen, Screen::Settings(_)));
    }

    #[test]
    fn a_create_named_on_its_mode_request_opens_the_session() {
        let mut f = Fixture::paired();
        f.dispatch(Action::SetDefaultMode(Some(PermissionMode::Plan)));
        f.dispatch(Action::SetDefaultSafetyCheck(true));
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        let command = queued(&effects[..2]);
        let new = created_id(&command);
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: new,
                workdir: "/srv/new".into(),
                conversation: None,
                worktree: None,
                mode: Some(PermissionMode::Plan),
                safety_check: true,
            }
        );
        // Open from the moment it is queued, as the server has not logged it.
        assert_eq!(f.model.open_session(), Some(new));

        // As the server logs a create with a mode: the `SessionCreated` and
        // the safety check name no command; the mode request, logged last,
        // names the create.
        let me = f.me;
        f.log_commands(
            new,
            vec![
                (
                    None,
                    EventKind::SessionCreated {
                        workdir: "/srv/new".into(),
                        context_name: "start".into(),
                        from: None,
                        worktree: None,
                    },
                ),
                (None, EventKind::SafetyCheckSet { on: true, by: me }),
                (
                    Some(command.id),
                    EventKind::PermissionModeRequested {
                        mode: PermissionMode::Plan,
                        by: me,
                    },
                ),
            ],
        );
        assert_eq!(f.model.open_session(), Some(new));
        assert_eq!(f.model.unloaded_session(), Some(new));
    }

    #[test]
    fn a_creating_session_takes_messages_and_settles_when_logged() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        let create = queued(&effects[..2]);
        let id = created_id(&create);
        let pending = |f: &Fixture| {
            f.session_view()
                .timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::Pending { .. }))
                .count()
        };

        // The page takes a message before the server has logged the session.
        let message = queued(&f.dispatch(Action::SendMessage {
            session_id: id,
            text: "hi".into(),
        }));
        assert!(matches!(
            &message.kind,
            CommandKind::SendMessage { session_id, context, .. }
                if *session_id == id && *context == ContextId::first_of(id)
        ));
        assert_eq!(pending(&f), 1);

        // A rename needs the session in the replica: nothing is queued.
        assert!(
            f.dispatch(Action::RenameSession {
                session_id: id,
                name: "later".into(),
            })
            .is_empty()
        );

        // Logged: the page is no longer creating, and the message is still
        // waiting for its turn in the log.
        f.log_commands(
            id,
            vec![(
                Some(create.id),
                EventKind::SessionCreated {
                    workdir: "/srv/new".into(),
                    context_name: "start".into(),
                    from: None,
                    worktree: None,
                },
            )],
        );
        assert_eq!(f.session_view().creating, None);
        assert_eq!(pending(&f), 1);
    }

    #[test]
    fn dismissing_a_refused_creation_takes_its_messages_and_leaves_its_page() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        let create = queued(&effects[..2]);
        let id = created_id(&create);
        let message = queued(&f.dispatch(Action::SendMessage {
            session_id: id,
            text: "hi".into(),
        }));
        f.dispatch(save_draft(id, "half a thought", 5));
        f.model.command_sending(create.id, Utc::now());
        f.model.command_result(
            create.id,
            CommandResult::Rejected {
                reason: RejectReason::WorkdirNotFound,
            },
            Utc::now(),
        );
        assert_eq!(
            f.dispatch(Action::Discard(create.id)),
            vec![
                Effect::Withdraw {
                    id: create.id,
                    draft: None,
                    images: Some((id, Vec::new())),
                    files: Some((id, Vec::new())),
                },
                Effect::Withdraw {
                    id: message.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::SaveDraft((id, ComposerMode::Agent), None),
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert!(f.list_view().creating.is_empty());
        assert!(f.model.drafts.is_empty(), "the session's draft went too");
    }

    #[test]
    fn recreating_a_refused_session_requeues_its_messages_behind_it() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        let create = queued(&effects[..2]);
        let id = created_id(&create);
        let one = send(&mut f, id, "one");
        let two = send(&mut f, id, "two");
        f.model.command_sending(create.id, Utc::now());
        f.model.command_result(
            create.id,
            CommandResult::Rejected {
                reason: RejectReason::WorkdirNotFound,
            },
            Utc::now(),
        );
        f.model.command_sending(one.id, Utc::now());
        f.model.command_result(
            one.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );

        let effects = f.dispatch(Action::RecreateSession {
            session_id: id,
            workdir: "/srv/fixed".into(),
            conversation: None,
            worktree: None,
        });
        let withdraw = |command_id: CommandId| Effect::Withdraw {
            id: command_id,
            draft: None,
            images: None,
            files: None,
        };
        // The refused create and its messages go, then the new create and
        // the messages again, behind it, with one outbox wake-up at the end.
        assert_eq!(effects.len(), 7);
        assert_eq!(
            effects[..3],
            [withdraw(create.id), withdraw(one.id), withdraw(two.id)]
        );
        assert_eq!(effects[6], Effect::Request(Request::OutboxChanged));
        let requeued: Vec<Command> = effects[3..]
            .iter()
            .filter_map(|effect| match effect {
                Effect::Enqueue(command, _) => Some(command.clone()),
                _ => None,
            })
            .collect();
        let [again, one_again, two_again]: [Command; 3] = requeued.try_into().unwrap();
        assert!(matches!(
            &again.kind,
            CommandKind::CreateSession { session_id, workdir, .. }
                if *session_id == id && workdir == "/srv/fixed"
        ));
        assert_ne!(one_again.id, one.id);
        assert_eq!(one_again.kind, one.kind);
        assert_ne!(two_again.id, two.id);
        assert_eq!(two_again.kind, two.kind);
        assert_eq!(
            f.session_view().creating.map(|page| page.outgoing),
            Some(Outgoing::Queued)
        );
    }

    #[test]
    fn recreate_does_nothing_unless_the_create_was_refused() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::SessionList));
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        let id = created_id(&queued(&effects[..2]));
        assert!(
            f.dispatch(Action::RecreateSession {
                session_id: id,
                workdir: "/srv/fixed".into(),
                conversation: None,
                worktree: None,
            })
            .is_empty()
        );
    }

    fn send(f: &mut Fixture, session_id: SessionId, text: &str) -> Command {
        queued(&f.dispatch(Action::SendMessage {
            session_id,
            text: text.into(),
        }))
    }

    /// The pending items of the open session, as (text, discardable).
    fn pending(f: &Fixture) -> Vec<(String, bool)> {
        f.session_view()
            .timeline
            .into_iter()
            .filter_map(|item| match item {
                TimelineItem::Pending {
                    text, discardable, ..
                } => Some((text, discardable)),
                TimelineItem::Logged { .. } | TimelineItem::Change { .. } => None,
            })
            .collect()
    }

    #[test]
    fn edit_queued_moves_text_into_draft() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let first = send(&mut f, sid, "fix the tests");
        let second = send(&mut f, sid, "then lint");
        f.dispatch(save_draft(sid, "half typed", 4));
        let rev = f.session_view().draft_rev;

        // The message goes first, the old draft after a blank line; the
        // cursor ends the message. One effect carries both writes.
        let effects = f.dispatch(Action::EditQueued(first.id));
        let draft = Draft {
            text: "fix the tests\n\nhalf typed".into(),
            cursor: "fix the tests".len(),
            when: RunWhen::Always,
        };
        assert_eq!(
            effects,
            vec![
                Effect::Withdraw {
                    id: first.id,
                    draft: Some(((sid, ComposerMode::Agent), draft.clone())),
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        let view = f.session_view();
        assert_eq!(view.draft, Some(draft));
        assert!(view.draft_rev > rev, "heads replace their composer");
        assert_eq!(pending(&f), vec![("then lint".to_owned(), true)]);
        assert_eq!(f.model.view().queued, 1);
        assert!(
            f.dispatch(Action::EditQueued(first.id)).is_empty(),
            "already withdrawn"
        );

        // Without a draft the text is the message alone.
        f.dispatch(save_draft(sid, "", 0));
        let effects = f.dispatch(Action::EditQueued(second.id));
        assert_eq!(
            effects,
            vec![
                Effect::Withdraw {
                    id: second.id,
                    draft: Some((
                        (sid, ComposerMode::Agent),
                        Draft {
                            text: "then lint".into(),
                            cursor: 9,
                            when: RunWhen::Always,
                        }
                    )),
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert_eq!(pending(&f), Vec::new());

        // Discarding drops the message and leaves the draft alone.
        let third = send(&mut f, sid, "never mind");
        let rev = f.session_view().draft_rev;
        assert_eq!(
            f.dispatch(Action::Discard(third.id)),
            vec![
                Effect::Withdraw {
                    id: third.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert_eq!(pending(&f), Vec::new());
        assert_eq!(f.session_view().draft_rev, rev);

        // Only messages go back into a draft.
        let interrupt = queued(&f.dispatch(Action::Interrupt { session_id: sid }));
        assert!(f.dispatch(Action::EditQueued(interrupt.id)).is_empty());
        assert!(f.dispatch(Action::Discard(interrupt.id)).is_empty());
    }

    /// A small PNG image tagged `tag` (its hash is the tag's), `size` bytes.
    fn image(tag: u8, size: u64) -> AttachedImage {
        AttachedImage {
            sha256: ContentHash::of(&[tag]),
            media_type: ImageType::Png,
            size,
            width: 16,
            height: 16,
        }
    }

    /// The images the open session's agent draft shows.
    fn draft_images(f: &Fixture) -> Vec<AttachedImage> {
        f.session_view().draft_images
    }

    /// The images of the open session's first logged user message.
    fn logged_images(f: &Fixture) -> Vec<MessageImage> {
        f.session_view()
            .timeline
            .into_iter()
            .find_map(|item| match item {
                TimelineItem::Logged {
                    entry: Entry::UserMessage { images, .. },
                    ..
                } => Some(images),
                _ => None,
            })
            .expect("the message is logged")
    }

    #[test]
    fn attach_and_detach_change_the_draft_images() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let a = image(1, 100);

        assert_eq!(
            f.dispatch(Action::Attach {
                session_id: sid,
                image: a.clone(),
            }),
            vec![
                Effect::AttachToDraft(sid, a.clone()),
                Effect::Request(Request::DraftImagesChanged),
            ]
        );
        assert_eq!(draft_images(&f), vec![a.clone()]);
        assert!(
            f.dispatch(Action::Attach {
                session_id: sid,
                image: a.clone(),
            })
            .is_empty(),
            "a draft holds an image once"
        );
        assert_eq!(
            f.dispatch(Action::Detach {
                session_id: sid,
                sha256: a.sha256,
            }),
            vec![Effect::DetachFromDraft(sid, a.sha256)]
        );
        assert!(draft_images(&f).is_empty());
        assert!(
            f.dispatch(Action::Detach {
                session_id: sid,
                sha256: a.sha256,
            })
            .is_empty(),
            "already off the draft"
        );
        let unknown = SessionId::new();
        assert!(
            f.dispatch(Action::Attach {
                session_id: unknown,
                image: a,
            })
            .is_empty(),
            "an unknown session has no draft"
        );
    }

    #[test]
    fn a_sent_message_shows_its_images_until_and_after_it_is_logged() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let (a, b) = (image(1, 100), image(2, 200));
        for picked in [&a, &b] {
            f.dispatch(Action::Attach {
                session_id: sid,
                image: picked.clone(),
            });
        }
        let command = send(&mut f, sid, "look");
        let pending_images = |f: &Fixture| match &f.session_view().timeline[..] {
            [
                TimelineItem::Logged {
                    entry: Entry::Created { .. },
                    ..
                },
                TimelineItem::Pending { images, .. },
            ] => images.clone(),
            other => panic!("expected one pending message, got {other:?}"),
        };
        assert_eq!(pending_images(&f), vec![a.clone(), b.clone()]);

        let blob_of = |image: &AttachedImage| BlobRef {
            id: BlobId::new(),
            size: image.size,
            sha256: image.sha256,
        };
        let (blob_a, blob_b) = (blob_of(&a), blob_of(&b));
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::UserMessage {
                    device_id: me,
                    text: "look".into(),
                    attachments: vec![
                        Attachment {
                            blob: blob_a.clone(),
                            media_type: ImageType::Png,
                        },
                        Attachment {
                            blob: blob_b.clone(),
                            media_type: ImageType::Png,
                        },
                    ],
                },
            )],
        );
        let not_downloaded = |blob: &BlobRef| BlobView {
            id: blob.id,
            state: BlobState::NotDownloaded { size: blob.size },
            change: None,
            pending: None,
        };
        assert_eq!(
            logged_images(&f),
            vec![
                MessageImage {
                    media_type: ImageType::Png,
                    content: FullView::Blob(not_downloaded(&blob_a)),
                },
                MessageImage {
                    media_type: ImageType::Png,
                    content: FullView::Blob(not_downloaded(&blob_b)),
                },
            ]
        );
        assert!(
            !f.session_view()
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::Pending { .. })),
            "the logged message replaces the pending one"
        );

        let path = PathBuf::from("/blobs/a.png");
        f.model
            .blob_status(blob_a.id, BlobStatus::Available { path: path.clone() });
        let shown = logged_images(&f);
        assert_eq!(
            shown[0].content.blob().unwrap().state,
            BlobState::Available { path }
        );
        assert_eq!(
            shown[1].content.blob().unwrap().state,
            BlobState::NotDownloaded { size: 200 }
        );
    }

    #[test]
    fn a_deleted_message_image_shows_as_deleted() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let (a, b) = (image(1, 100), image(2, 200));
        let command = send(&mut f, sid, "look");
        let blob_of = |image: &AttachedImage| BlobRef {
            id: BlobId::new(),
            size: image.size,
            sha256: image.sha256,
        };
        let (blob_a, blob_b) = (blob_of(&a), blob_of(&b));
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::UserMessage {
                    device_id: me,
                    text: "look".into(),
                    attachments: vec![
                        Attachment {
                            blob: blob_a.clone(),
                            media_type: ImageType::Png,
                        },
                        Attachment {
                            blob: blob_b.clone(),
                            media_type: ImageType::Png,
                        },
                    ],
                },
            )],
        );
        // Another device deletes the first image only: the second, which
        // nothing deleted, must still show as a blob (the control).
        let other = DeviceId::new();
        f.log(
            sid,
            vec![EventKind::BlobDeleted {
                blob: blob_a.id,
                holds: BlobUse::Message,
                by: other,
            }],
        );

        let shown = logged_images(&f);
        assert!(
            matches!(&shown[0].content, FullView::Deleted { by } if by.id == other),
            "{:?}",
            shown[0]
        );
        assert!(
            matches!(&shown[1].content, FullView::Blob(_)),
            "{:?}",
            shown[1]
        );
    }

    #[test]
    fn an_image_only_draft_marks_the_session() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let a = image(1, 100);
        let has_draft = |f: &Fixture| {
            f.list_view()
                .sessions
                .iter()
                .find(|r| r.id == sid)
                .unwrap()
                .has_draft
        };
        assert!(!has_draft(&f));
        f.dispatch(Action::Attach {
            session_id: sid,
            image: a.clone(),
        });
        assert!(has_draft(&f), "an image with no text is a draft");
        f.dispatch(Action::Detach {
            session_id: sid,
            sha256: a.sha256,
        });
        assert!(!has_draft(&f));
    }

    #[test]
    fn the_draft_refuses_an_image_past_either_cap() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        for tag in 0..8 {
            let effects = f.dispatch(Action::Attach {
                session_id: sid,
                image: image(tag, 1_000),
            });
            assert_eq!(effects.len(), 2, "image {tag} fits");
        }
        assert!(
            f.dispatch(Action::Attach {
                session_id: sid,
                image: image(8, 1_000),
            })
            .is_empty(),
            "a ninth image is past the count cap"
        );
        assert_eq!(draft_images(&f).len(), 8);

        // Bytes count together: two of 4 MB fit in 10 MB, a third of 2.5 MB
        // does not.
        let other = f.session("/srv/b");
        f.open(other);
        for tag in [20, 21] {
            let effects = f.dispatch(Action::Attach {
                session_id: other,
                image: image(tag, 4_000_000),
            });
            assert_eq!(effects.len(), 2, "image {tag} fits");
        }
        assert!(
            f.dispatch(Action::Attach {
                session_id: other,
                image: image(22, 2_500_000),
            })
            .is_empty(),
            "the bytes are past the total cap"
        );
        assert_eq!(draft_images(&f).len(), 2);
    }

    #[test]
    fn a_message_takes_its_draft_images_even_with_blank_text() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let (a, b) = (image(1, 100), image(2, 200));
        f.dispatch(Action::Attach {
            session_id: sid,
            image: a.clone(),
        });
        f.dispatch(Action::Attach {
            session_id: sid,
            image: b.clone(),
        });

        let command = queued(&f.dispatch(Action::SendMessage {
            session_id: sid,
            text: "  ".into(),
        }));
        let CommandKind::SendMessage { attachments, .. } = command.kind else {
            panic!("expected a message");
        };
        assert_eq!(attachments, vec![a.sha256, b.sha256]);
        assert!(draft_images(&f).is_empty(), "sending takes the images");

        assert!(
            f.dispatch(Action::SendMessage {
                session_id: sid,
                text: " ".into(),
            })
            .is_empty(),
            "blank with no images is not sent"
        );
    }

    #[test]
    fn a_draft_past_a_cap_is_not_sent() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        for tag in 0..5 {
            f.dispatch(Action::Attach {
                session_id: sid,
                image: image(tag, 100),
            });
        }
        let first = send(&mut f, sid, "five images");
        for tag in 5..9 {
            f.dispatch(Action::Attach {
                session_id: sid,
                image: image(tag, 100),
            });
        }
        assert_eq!(draft_images(&f).len(), 4);

        // Taken back, the message's five go ahead of the four: nine, past
        // the count cap. Edits are not refused, sending is.
        f.dispatch(Action::EditQueued(first.id));
        assert_eq!(draft_images(&f).len(), 9);
        assert!(
            f.dispatch(Action::SendMessage {
                session_id: sid,
                text: "go".into(),
            })
            .is_empty(),
            "the draft is past the count cap"
        );
        assert_eq!(draft_images(&f).len(), 9);
    }

    #[test]
    fn editing_a_queued_message_puts_its_images_before_the_drafts() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let (a, b, c) = (image(1, 100), image(2, 200), image(3, 300));
        f.dispatch(Action::Attach {
            session_id: sid,
            image: a.clone(),
        });
        let first = send(&mut f, sid, "look");
        f.dispatch(Action::Attach {
            session_id: sid,
            image: b.clone(),
        });
        f.dispatch(Action::Attach {
            session_id: sid,
            image: c.clone(),
        });
        let rev = f.session_view().draft_rev;

        assert_eq!(
            f.dispatch(Action::EditQueued(first.id)),
            vec![
                Effect::Withdraw {
                    id: first.id,
                    draft: Some((
                        (sid, ComposerMode::Agent),
                        Draft {
                            text: "look".into(),
                            cursor: 4,
                            when: RunWhen::Always,
                        }
                    )),
                    images: Some((sid, vec![a.sha256, b.sha256, c.sha256])),
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
                Effect::Request(Request::DraftImagesChanged),
            ]
        );
        let view = f.session_view();
        assert_eq!(view.draft_images, vec![a.clone(), b.clone(), c.clone()]);
        assert!(view.draft_rev > rev, "heads refill their composer");

        // A draft that already holds A keeps one A, at the message's place.
        let other = f.session("/srv/b");
        f.open(other);
        f.dispatch(Action::Attach {
            session_id: other,
            image: a.clone(),
        });
        let again = send(&mut f, other, "again");
        f.dispatch(Action::Attach {
            session_id: other,
            image: a.clone(),
        });
        f.dispatch(Action::Attach {
            session_id: other,
            image: b.clone(),
        });
        let effects = f.dispatch(Action::EditQueued(again.id));
        assert_eq!(
            effects[0],
            Effect::Withdraw {
                id: again.id,
                draft: Some((
                    (other, ComposerMode::Agent),
                    Draft {
                        text: "again".into(),
                        cursor: 5,
                        when: RunWhen::Always,
                    }
                )),
                images: Some((other, vec![a.sha256, b.sha256])),
                files: None,
            }
        );
        assert_eq!(draft_images(&f), vec![a.clone(), b.clone()]);

        // Discarding a message takes its images with it: nothing goes back.
        let third = send(&mut f, other, "never");
        assert!(draft_images(&f).is_empty(), "sending takes the images");
        assert_eq!(
            f.dispatch(Action::Discard(third.id)),
            vec![
                Effect::Withdraw {
                    id: third.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert!(draft_images(&f).is_empty());
    }

    /// A non-image file called `name`, its content tagged `tag`.
    fn draft_file(name: &str, tag: u8) -> AttachedFile {
        AttachedFile {
            sha256: ContentHash::of(&[tag]),
            name: name.into(),
            size: 10,
            replace: None,
        }
    }

    /// The files the open session's agent draft shows.
    fn draft_files(f: &Fixture) -> Vec<AttachedFile> {
        f.session_view().draft_files
    }

    #[test]
    fn attach_and_detach_change_the_draft_files() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let a = draft_file("a.txt", 1);

        assert_eq!(
            f.dispatch(Action::AttachFile {
                session_id: sid,
                file: a.clone(),
            }),
            vec![
                Effect::AttachFileToDraft(sid, a.clone()),
                Effect::Request(Request::DraftImagesChanged),
            ]
        );
        assert_eq!(draft_files(&f), vec![a.clone()]);
        assert_eq!(
            f.dispatch(Action::DetachFile {
                session_id: sid,
                name: "a.txt".into(),
            }),
            vec![Effect::DetachFileFromDraft(sid, "a.txt".into())]
        );
        assert!(draft_files(&f).is_empty());
        assert!(
            f.dispatch(Action::DetachFile {
                session_id: sid,
                name: "a.txt".into(),
            })
            .is_empty(),
            "already off the draft"
        );
        let unknown = SessionId::new();
        assert!(
            f.dispatch(Action::AttachFile {
                session_id: unknown,
                file: a,
            })
            .is_empty(),
            "no draft for an unknown session"
        );
    }

    #[test]
    fn a_draft_refuses_a_second_file_name_or_a_path() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let a = draft_file("a.txt", 1);
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: a.clone(),
        });

        assert!(
            f.dispatch(Action::AttachFile {
                session_id: sid,
                file: draft_file("a.txt", 2),
            })
            .is_empty(),
            "one target per name"
        );
        for name in ["a/b", "a\\b", ".", "..", ".git", ""] {
            assert!(
                f.dispatch(Action::AttachFile {
                    session_id: sid,
                    file: draft_file(name, 3),
                })
                .is_empty(),
                "{name:?} is not a plain file name"
            );
        }
        assert_eq!(draft_files(&f), vec![a]);
    }

    #[test]
    fn a_message_names_its_files_after_the_text() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let (a, b) = (draft_file("a.txt", 1), draft_file("b.bin", 2));
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: a.clone(),
        });
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: b.clone(),
        });

        let effects = f.dispatch(Action::SendMessage {
            session_id: sid,
            text: "look".into(),
        });
        let [
            Effect::Enqueue(command, files),
            Effect::Request(Request::OutboxChanged),
        ] = &effects[..]
        else {
            panic!("expected enqueue + outbox wake-up, got {effects:?}");
        };
        let CommandKind::SendMessage { text, .. } = &command.kind else {
            panic!("expected a message: {:?}", command.kind);
        };
        assert_eq!(
            text,
            "look\n\nAttached file: ./a.txt\nAttached file: ./b.bin"
        );
        assert_eq!(files, &vec![a, b]);
        assert!(draft_files(&f).is_empty(), "sending takes the files");
    }

    #[test]
    fn a_file_only_draft_can_be_sent() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let a = draft_file("a.txt", 1);
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: a.clone(),
        });

        let command = queued(&f.dispatch(Action::SendMessage {
            session_id: sid,
            text: " ".into(),
        }));
        let CommandKind::SendMessage { text, .. } = &command.kind else {
            panic!("expected a message: {:?}", command.kind);
        };
        assert_eq!(text, "Attached file: ./a.txt");
        assert!(draft_files(&f).is_empty());

        // Taken back, the file is the draft's again and its line is off the
        // text, which is left empty.
        assert_eq!(
            f.dispatch(Action::EditQueued(command.id)),
            vec![
                Effect::Withdraw {
                    id: command.id,
                    draft: Some((
                        (sid, ComposerMode::Agent),
                        Draft {
                            text: String::new(),
                            cursor: 0,
                            when: RunWhen::Always,
                        }
                    )),
                    images: None,
                    files: Some((sid, vec![a.clone()])),
                },
                Effect::Request(Request::OutboxChanged),
                Effect::Request(Request::DraftImagesChanged),
            ]
        );
        assert_eq!(draft_files(&f), vec![a]);
    }

    #[test]
    fn editing_a_queued_message_puts_its_files_back() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let (a, b) = (draft_file("a.txt", 1), draft_file("b.txt", 2));
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: a.clone(),
        });
        let first = send(&mut f, sid, "look");
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: b.clone(),
        });

        // The message's file goes ahead of the draft's, and its line comes
        // off the text so a resend does not name it twice.
        assert_eq!(
            f.dispatch(Action::EditQueued(first.id)),
            vec![
                Effect::Withdraw {
                    id: first.id,
                    draft: Some((
                        (sid, ComposerMode::Agent),
                        Draft {
                            text: "look".into(),
                            cursor: 4,
                            when: RunWhen::Always,
                        }
                    )),
                    images: None,
                    files: Some((sid, vec![a.clone(), b.clone()])),
                },
                Effect::Request(Request::OutboxChanged),
                Effect::Request(Request::DraftImagesChanged),
            ]
        );
        assert_eq!(draft_files(&f), vec![a, b]);
    }

    /// The files and outgoing state of the open session's pending message
    /// `command`.
    fn pending_item(f: &Fixture, command: CommandId) -> (Vec<OutboxFile>, Outgoing) {
        f.session_view()
            .timeline
            .into_iter()
            .find_map(|item| match item {
                TimelineItem::Pending {
                    command_id,
                    files,
                    outgoing,
                    ..
                } if command_id == command => Some((files, outgoing)),
                _ => None,
            })
            .expect("the message is pending")
    }

    #[test]
    fn a_found_file_is_kept_on_the_entry() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: draft_file("a.txt", 1),
        });
        let command = send(&mut f, sid, "look");
        let found = ContentHash::of(b"what the workdir holds");

        f.model
            .command_file_exists(command.id, 0, found, Utc::now());

        let (files, outgoing) = pending_item(&f, command.id);
        assert_eq!(files[0].existing, Some(found));
        assert_eq!(
            outgoing,
            Outgoing::Failed(FailReason::AttachmentRefused(AttachmentRefusal::FileExists))
        );
    }

    #[test]
    fn resending_replacing_requeues_with_the_found_hash() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: draft_file("a.txt", 1),
        });
        let command = send(&mut f, sid, "look");
        let found = ContentHash::of(b"what the workdir holds");
        f.model
            .command_file_exists(command.id, 0, found, Utc::now());

        assert_eq!(
            f.dispatch(Action::ResendReplacing(command.id)),
            vec![
                Effect::Requeue {
                    id: command.id,
                    replaces: vec![(0, found)],
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        let (files, outgoing) = pending_item(&f, command.id);
        assert_eq!(outgoing, Outgoing::Queued);
        assert_eq!(files[0].file.replace, Some(found));
        assert_eq!(files[0].existing, None);
        // Requeued, it is no longer a refusal to resend.
        assert!(f.dispatch(Action::ResendReplacing(command.id)).is_empty());
    }

    #[test]
    fn resending_replacing_is_a_no_op_otherwise() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let queued_message = send(&mut f, sid, "queued");
        let missing = send(&mut f, sid, "missing");
        f.model
            .command_attachment_refused(missing.id, AttachmentRefusal::Missing, Utc::now());

        assert!(
            f.dispatch(Action::ResendReplacing(queued_message.id))
                .is_empty()
        );
        assert!(f.dispatch(Action::ResendReplacing(missing.id)).is_empty());
        assert!(
            f.dispatch(Action::ResendReplacing(CommandId::new()))
                .is_empty()
        );
        assert_eq!(
            pending_item(&f, missing.id).1,
            Outgoing::Failed(FailReason::AttachmentRefused(AttachmentRefusal::Missing))
        );
    }

    #[test]
    fn a_pending_message_shows_its_files() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let a = draft_file("a.txt", 1);
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: a.clone(),
        });
        send(&mut f, sid, "look");

        let files = f
            .session_view()
            .timeline
            .into_iter()
            .find_map(|item| match item {
                TimelineItem::Pending { files, .. } => Some(files),
                TimelineItem::Logged { .. } | TimelineItem::Change { .. } => None,
            });
        assert_eq!(
            files,
            Some(vec![OutboxFile {
                file: a,
                existing: None,
            }])
        );
    }

    #[test]
    fn a_file_only_draft_marks_the_session() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let a = draft_file("a.txt", 1);
        let has_draft = |f: &Fixture| {
            f.list_view()
                .sessions
                .iter()
                .find(|r| r.id == sid)
                .unwrap()
                .has_draft
        };
        assert!(!has_draft(&f));
        f.dispatch(Action::AttachFile {
            session_id: sid,
            file: a,
        });
        assert!(has_draft(&f), "a file with no text is a draft");
        f.dispatch(Action::DetachFile {
            session_id: sid,
            name: "a.txt".into(),
        });
        assert!(!has_draft(&f));
    }

    #[test]
    fn a_message_is_discardable_until_accepted() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = send(&mut f, sid, "hello");
        assert_eq!(pending(&f), vec![("hello".to_owned(), true)]);

        // Claimed, it may have reached the server, but taking it back still
        // stops the resends of one that never gets through.
        f.model.command_sending(command.id, Utc::now());
        f.model.command_sending(command.id, Utc::now());
        assert_eq!(pending(&f), vec![("hello".to_owned(), true)]);
        assert_eq!(
            f.dispatch(Action::EditQueued(command.id)),
            vec![
                Effect::Withdraw {
                    id: command.id,
                    draft: Some((
                        (sid, ComposerMode::Agent),
                        Draft {
                            text: "hello".into(),
                            cursor: 5,
                            when: RunWhen::Always,
                        }
                    )),
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert_eq!(pending(&f), Vec::new());
        // Its answer, still on its way, settles nothing.
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(9) },
            Utc::now(),
        );
        assert_eq!(pending(&f), Vec::new());

        // A refusal leaves it discardable; an acceptance does not.
        let refused = send(&mut f, sid, "refused");
        f.model.command_sending(refused.id, Utc::now());
        f.model.command_result(
            refused.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        assert_eq!(pending(&f), vec![("refused".to_owned(), true)]);
        f.dispatch(Action::Discard(refused.id));
        let accepted = send(&mut f, sid, "accepted");
        f.model.command_sending(accepted.id, Utc::now());
        f.model.command_result(
            accepted.id,
            CommandResult::Accepted { seq: Seq(10) },
            Utc::now(),
        );
        assert!(f.dispatch(Action::Discard(accepted.id)).is_empty());
    }

    #[test]
    fn a_created_session_sends_its_trimmed_conversation_unchecked() {
        let mut f = Fixture::paired();
        // The id is minted per create, so each expected kind takes the one sent.
        let create = |f: &mut Fixture, conversation: Option<&str>| {
            let effects = f.dispatch(Action::CreateSession {
                workdir: "/srv/a".into(),
                conversation: conversation.map(str::to_owned),
                worktree: None,
            });
            let command = queued(&effects[..2]);
            (created_id(&command), command.kind)
        };
        let kind = |session_id, conversation: Option<&str>| CommandKind::CreateSession {
            session_id,
            workdir: "/srv/a".into(),
            conversation: conversation.map(str::to_owned),
            worktree: None,
            mode: None,
            safety_check: false,
        };
        let (id, sent) = create(&mut f, Some(" id "));
        assert_eq!(sent, kind(id, Some("id")));
        let (id, sent) = create(&mut f, Some("  "));
        assert_eq!(sent, kind(id, None));
        let (id, sent) = create(&mut f, None);
        assert_eq!(sent, kind(id, None));
        // Not an id the agent could have: still sent, the server decides.
        let (id, sent) = create(&mut f, Some("../x y"));
        assert_eq!(sent, kind(id, Some("../x y")));
    }

    #[test]
    fn creating_a_session_asks_for_a_trimmed_worktree() {
        let mut f = Fixture::paired();
        // The id is minted per create, so each expected kind takes the one sent.
        let create = |f: &mut Fixture, worktree: Option<&str>| {
            let effects = f.dispatch(Action::CreateSession {
                workdir: "/srv/app".into(),
                conversation: None,
                worktree: worktree.map(str::to_owned),
            });
            let command = queued(&effects[..2]);
            (created_id(&command), command.kind)
        };
        let kind = |session_id, worktree: Option<&str>| CommandKind::CreateSession {
            session_id,
            workdir: "/srv/app".into(),
            conversation: None,
            worktree: worktree.map(str::to_owned),
            mode: None,
            safety_check: false,
        };
        let (id, sent) = create(&mut f, Some("  feat/x "));
        assert_eq!(sent, kind(id, Some("feat/x")));
        let (id, sent) = create(&mut f, Some("   "));
        assert_eq!(sent, kind(id, None));
    }

    #[test]
    fn a_refused_conversation_is_a_discardable_failed_creation() {
        let mut f = Fixture::paired();
        let effects = f.dispatch(Action::CreateSession {
            workdir: "/srv/a".into(),
            conversation: Some("nope".into()),
            worktree: None,
        });
        let command = queued(&effects[..2]);
        f.model.command_sending(command.id, Utc::now());
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::BadConversation,
            },
            Utc::now(),
        );
        f.dispatch(Action::Navigate(Route::SessionList));
        let creating = f.list_view().creating;
        assert_eq!(creating.len(), 1);
        assert_eq!(creating[0].workdir, "/srv/a");
        assert!(creating[0].discardable);
        assert!(matches!(creating[0].outgoing, Outgoing::Failed(_)));
        assert_eq!(
            f.dispatch(Action::Discard(command.id)),
            vec![
                Effect::Withdraw {
                    id: command.id,
                    draft: None,
                    images: Some((created_id(&command), Vec::new())),
                    files: Some((created_id(&command), Vec::new())),
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert!(f.list_view().creating.is_empty());
    }

    #[test]
    fn an_adopted_session_starts_with_the_adopted_entry() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/a".into(),
            context_name: "start".into(),
            from: Some(SessionOrigin::Agent {
                conversation: "conv-1".into(),
            }),
            worktree: None,
        };
        f.log_stamped(adopted, vec![(None, ContextId::first_of(adopted), created)]);
        f.log(adopted, vec![message("go on")]);
        f.open(adopted);
        let shown = entries(&f);
        assert_eq!(
            shown.first(),
            Some(&Entry::Adopted {
                conversation: "conv-1".into()
            })
        );
        assert!(
            matches!(shown.get(1), Some(Entry::UserMessage { .. })),
            "{shown:?}"
        );
        assert!(
            !shown.iter().any(|e| matches!(e, Entry::Created { .. })),
            "an adopted session was not created here: {shown:?}"
        );
        // Positive control: a plain session has no such first item.
        let plain = f.session("/srv/b");
        f.log(plain, vec![message("hi")]);
        f.open(plain);
        assert!(
            !entries(&f)
                .iter()
                .any(|e| matches!(e, Entry::Adopted { .. } | Entry::MovedFrom { .. }))
        );
    }

    #[test]
    fn a_fresh_session_starts_with_its_creation() {
        let mut f = Fixture::paired();
        let fresh = f.session("/srv/a");
        f.log(fresh, vec![message("hi")]);
        f.open(fresh);
        assert_eq!(
            entries(&f).first(),
            Some(&Entry::Created {
                workdir: "/srv/a".into()
            })
        );
    }

    #[test]
    fn back_leaves_a_creating_page() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::SessionList));
        f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        // The create opened its page; nothing is logged yet, so no summary.
        assert_eq!(shown(&f.model.view()), "session");
        assert!(f.model.view().can_go_back);
        f.dispatch(Action::Back);
        assert_eq!(shown(&f.model.view()), "active list");
    }

    fn adopted_created() -> EventKind {
        EventKind::SessionCreated {
            workdir: "/srv/a".into(),
            context_name: "start".into(),
            from: Some(SessionOrigin::Agent {
                conversation: "conv-1".into(),
            }),
            worktree: None,
        }
    }

    /// An imported item the agent recorded at `minute` past 08:00 on
    /// 2026-11-01, or with no time.
    fn imported(minute: Option<u32>, item: ImportedItem) -> EventKind {
        EventKind::Imported {
            at: minute.map(|m| Utc.with_ymd_and_hms(2026, 11, 1, 8, m, 0).unwrap()),
            item,
        }
    }

    fn imported_text(user: bool, text: &str) -> ImportedItem {
        let text = text.to_owned();
        if user {
            ImportedItem::UserMessage { text, full: None }
        } else {
            ImportedItem::AgentMessage { text, full: None }
        }
    }

    fn imported_call(call: CallId, title: &str, input: Option<BlobRef>) -> ImportedItem {
        let EventKind::ToolCall(mut tool) = tool_call(Some(call), ToolKind::Shell, title) else {
            unreachable!()
        };
        tool.input = input.map(Full::Blob);
        ImportedItem::ToolCall(tool)
    }

    fn imported_result(call: CallId, preview: &str, output: Option<BlobRef>) -> ImportedItem {
        let EventKind::ToolResult(mut result) = tool_result(Some(call), false, preview) else {
            unreachable!()
        };
        result.output = output.map(Full::Blob);
        ImportedItem::ToolResult(result)
    }

    /// Logs an adopted session as the server does: its `SessionCreated`
    /// and imported `items` naming no command, then the `ImportFinished`
    /// naming `command`, all in the first context.
    fn log_adopted(
        f: &mut Fixture,
        session: SessionId,
        command: CommandId,
        items: Vec<EventKind>,
    ) -> Seq {
        let first = ContextId::first_of(session);
        let outcome = ImportOutcome::Imported {
            items: items.len() as u32,
        };
        let mut events = vec![(None, first, adopted_created())];
        events.extend(items.into_iter().map(|kind| (None, first, kind)));
        events.push((Some(command), first, EventKind::ImportFinished { outcome }));
        f.log_stamped(session, events);
        Seq(f.events.len() as u64)
    }

    #[test]
    fn an_adopted_timeline_shows_the_imported_history_then_the_sessions_own() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        let (done, open) = (CallId::new(), CallId::new());
        let output = BlobRef {
            id: BlobId::new(),
            size: 9000,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        let finished = log_adopted(
            &mut f,
            adopted,
            CommandId::new(),
            vec![
                imported(Some(0), imported_text(true, "fix the build")),
                imported(None, imported_text(false, "looking")),
                imported(Some(2), imported_call(done, "cargo build", None)),
                imported(Some(5), imported_result(done, "ok", Some(output.clone()))),
                imported(Some(6), imported_call(open, "sleep 100", None)),
            ],
        );
        f.log(adopted, vec![message("go on"), agent("on it")]);
        f.open(adopted);
        let view = f.session_view();
        let logged: Vec<(Seq, DateTime<Utc>, Entry)> = view
            .timeline
            .into_iter()
            .map(|item| match item {
                TimelineItem::Logged { seq, at, entry, .. } => (seq, at, entry),
                pending => panic!("nothing pending: {pending:?}"),
            })
            .collect();
        let at = |minute| Utc.with_ymd_and_hms(2026, 11, 1, 8, minute, 0).unwrap();
        let tool = |call, title: &str, outcome| {
            let EventKind::ToolCall(tool) = tool_call(Some(call), ToolKind::Shell, title) else {
                unreachable!()
            };
            ImportedEntry::Tool(Box::new(f.model.tool_view(call, &tool, outcome)))
        };
        let shown: Vec<&Entry> = logged.iter().map(|(_, _, e)| e).collect();
        assert_eq!(
            shown,
            vec![
                &Entry::Adopted {
                    conversation: "conv-1".into()
                },
                &Entry::Imported {
                    at: at(0),
                    entry: ImportedEntry::UserMessage {
                        text: "fix the build".into(),
                        full: None,
                    },
                },
                // No time recorded: when the server logged it.
                &Entry::Imported {
                    at: logged[2].1,
                    entry: ImportedEntry::AgentMessage {
                        text: "looking".into(),
                        full: None,
                    },
                },
                // The call and its result are one row, timed by the agent.
                &Entry::Imported {
                    at: at(2),
                    entry: tool(
                        done,
                        "cargo build",
                        ToolOutcome::Done {
                            is_error: false,
                            status: None,
                            preview: "ok".into(),
                            lines: 1,
                            output: Some(FullView::Blob(BlobView {
                                id: output.id,
                                state: BlobState::NotDownloaded { size: 9000 },
                                change: None,
                                pending: None,
                            })),
                            took: Duration::from_secs(180),
                        },
                    ),
                },
                // Never answered, and never running: nothing waits on it.
                &Entry::Imported {
                    at: at(6),
                    entry: tool(open, "sleep 100", ToolOutcome::NoResult),
                },
                &Entry::ImportFinished {
                    outcome: ImportOutcome::Imported { items: 5 }
                },
                shown[6],
                &Entry::AgentMessage {
                    text: "on it".into(),
                    full: None,
                },
            ]
        );
        assert!(
            matches!(shown[6], Entry::UserMessage { text, .. } if text == "go on"),
            "{shown:?}"
        );
        assert_eq!(logged[5].0, finished);
        // The item is placed at the log's time; the agent's sits in the entry.
        assert_ne!(logged[1].1, at(0));
        // Searched like the session's own.
        let timeline = f.session_view().timeline;
        let hits = |text: &str| -> Vec<usize> {
            crate::search::find(&timeline, &crate::search::Query::new(text), &mut |_| None)
                .hits
                .iter()
                .map(|hit| hit.index)
                .collect()
        };
        assert_eq!(hits("fix the"), [1]);
        assert_eq!(hits("looking"), [2]);
        assert_eq!(hits("cargo"), [3]);
        assert_eq!(hits("sleep"), [4]);
        assert_eq!(hits("go on"), [6], "positive control: a live message");

        // Narrowed to the first context: the imported history stays, its
        // divider goes with the others.
        f.dispatch(Action::ViewContext {
            session_id: adopted,
            context: Some(ContextId::first_of(adopted)),
        });
        let narrowed = entries(&f);
        assert_eq!(narrowed.len(), 6, "{narrowed:?}");
        assert!(matches!(narrowed[0], Entry::Imported { .. }));
        assert!(
            !narrowed
                .iter()
                .any(|e| matches!(e, Entry::ImportFinished { .. } | Entry::Adopted { .. }))
        );
    }

    #[test]
    fn imported_history_is_not_news_and_does_not_move_the_session() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        let mut items = Vec::new();
        for i in 0..250 {
            let call = CallId::new();
            items.push(imported(
                Some(0),
                imported_text(true, &format!("prompt {i}")),
            ));
            items.push(imported(
                Some(1),
                imported_text(false, &format!("answer {i}")),
            ));
            items.push(imported(Some(2), imported_call(call, "ls", None)));
            items.push(imported(Some(3), imported_result(call, "a.txt", None)));
        }
        assert_eq!(items.len(), 1000);
        log_adopted(&mut f, adopted, CommandId::new(), items);
        let summary = f.model.summary(adopted).unwrap().clone();
        assert_eq!(summary.notable, 0);
        assert_eq!(summary.last_seq, Seq(1), "the SessionCreated's");
        assert_eq!(summary.last_activity, f.events[0].at);
        assert!(!summary.importing);
        assert_eq!(unread(&f, adopted), 0);
        f.open(adopted);
        assert_eq!(f.session_view().unread_from, None);
        // Read to the end: the mark stops at the last activity, before the
        // imported history.
        f.dispatch(Action::MarkRead {
            session_id: adopted,
            through: Seq(1002),
        });
        assert_eq!(f.model.read_marks[&adopted].seq, Seq(1));
        f.dispatch(Action::Back);

        // The session's own news after it is unread, and its divider comes
        // after the imported history, not inside it.
        f.log(adopted, vec![agent("back")]);
        let summary = f.model.summary(adopted).unwrap().clone();
        assert_eq!(summary.notable, 1);
        assert_eq!(summary.last_seq, Seq(1003));
        assert_eq!(unread(&f, adopted), 1);
        f.open(adopted);
        assert_eq!(f.session_view().unread_from, Some(Seq(1003)));
    }

    #[test]
    fn a_never_read_adopted_session_shows_news_after_its_import() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        let items = (0..10)
            .map(|i| imported(None, imported_text(i % 2 == 0, &format!("item {i}"))))
            .collect();
        let finished = log_adopted(&mut f, adopted, CommandId::new(), items);
        f.log(adopted, vec![message("go on"), agent("back")]);
        assert!(!f.model.read_marks.contains_key(&adopted));
        f.open(adopted);
        // The first live item after the import, not the `Adopted` row.
        assert_eq!(f.session_view().unread_from, Some(Seq(finished.0 + 1)));
        // Positive control: a plain session never read is new from its start.
        f.dispatch(Action::Back);
        let plain = f.session("/srv/b");
        f.log(plain, vec![agent("hello")]);
        f.open(plain);
        assert_eq!(f.session_view().unread_from, Some(Seq(finished.0 + 3)));
    }

    #[test]
    fn an_imported_call_took_only_what_the_agent_timed() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        let (both, no_result_time, no_call_time) = (CallId::new(), CallId::new(), CallId::new());
        log_adopted(
            &mut f,
            adopted,
            CommandId::new(),
            vec![
                imported(Some(1), imported_call(both, "a", None)),
                imported(Some(4), imported_result(both, "ok", None)),
                // Only one side timed: the other would be the log's clock.
                // A month before the log's time, so the mix would not be
                // cut to zero by being negative.
                EventKind::Imported {
                    at: Some(Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap()),
                    item: imported_call(no_result_time, "b", None),
                },
                imported(None, imported_result(no_result_time, "ok", None)),
                imported(None, imported_call(no_call_time, "c", None)),
                imported(Some(59), imported_result(no_call_time, "ok", None)),
            ],
        );
        f.open(adopted);
        let took: Vec<(CallId, Duration)> = entries(&f)
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Imported {
                    entry: ImportedEntry::Tool(view),
                    ..
                } => match view.outcome {
                    ToolOutcome::Done { took, .. } => Some((view.call, took)),
                    other => panic!("{other:?}"),
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            took,
            [
                (both, Duration::from_secs(180)),
                (no_result_time, Duration::ZERO),
                (no_call_time, Duration::ZERO),
            ]
        );
    }

    #[test]
    fn the_first_imported_prompt_titles_the_session_and_its_first_context() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        log_adopted(
            &mut f,
            adopted,
            CommandId::new(),
            vec![
                imported(Some(0), imported_text(false, "hello, how can I help")),
                imported(Some(1), imported_text(true, "\n  fix the build\nplease")),
                imported(Some(2), imported_text(true, "and the tests")),
            ],
        );
        f.log(adopted, vec![message("now the docs")]);
        assert_eq!(row(&f, adopted).title, "fix the build");
        f.open(adopted);
        let view = f.session_view();
        assert_eq!(view.title, "fix the build");
        let [context] = view.contexts.as_slice() else {
            panic!("{:?}", view.contexts)
        };
        assert_eq!(context.first_prompt.as_deref(), Some("fix the build"));
        assert_eq!(context.messages, 3, "two imported prompts and one sent");
        f.dispatch(Action::Back);
        // Positive control: a plain session's title is its first message.
        let plain = f.session("/srv/b");
        f.log(plain, vec![message("hi there")]);
        assert_eq!(row(&f, plain).title, "hi there");
    }

    #[test]
    fn imported_blobs_are_in_the_replica_like_live_ones() {
        let mut f = Fixture::paired();
        let adopted = SessionId::new();
        let call = CallId::new();
        let (input, output) = (
            BlobRef {
                id: BlobId::new(),
                size: 5000,
                sha256: skywave_proto::ContentHash::of(b""),
            },
            BlobRef {
                id: BlobId::new(),
                size: 7000,
                sha256: skywave_proto::ContentHash::of(b""),
            },
        );
        log_adopted(
            &mut f,
            adopted,
            CommandId::new(),
            vec![
                imported(Some(0), imported_call(call, "ls", Some(input.clone()))),
                imported(
                    Some(1),
                    imported_result(call, "a.txt", Some(output.clone())),
                ),
            ],
        );
        assert!(f.model.blobs.contains_key(&input.id));
        assert!(f.model.blobs.contains_key(&output.id));
    }

    #[test]
    fn an_adopting_creation_is_pending_with_its_conversation_until_its_import_is_in() {
        let mut f = Fixture::paired();
        let effects = f.dispatch(Action::CreateSession {
            workdir: "/srv/a".into(),
            conversation: Some("conv-1".into()),
            worktree: None,
        });
        let command = queued(&effects[..2]);
        let adopted = created_id(&command);
        f.dispatch(Action::Navigate(Route::SessionList));
        let creating = |f: &Fixture| f.list_view().creating;
        assert_eq!(
            creating(&f),
            vec![PendingSession {
                command_id: command.id,
                session_id: adopted,
                workdir: "/srv/a".into(),
                conversation: Some("conv-1".into()),
                worktree: None,
                outgoing: Outgoing::Queued,
                discardable: true,
            }]
        );
        f.model.command_sending(command.id, Utc::now());
        assert_eq!(creating(&f)[0].outgoing, Outgoing::Sending);

        // A long import: the first pull brings the session and part of its
        // history, the command's `ImportFinished` comes in the next.
        let first = ContextId::first_of(adopted);
        let items: Vec<_> = (0..600)
            .map(|i| {
                (
                    None,
                    first,
                    imported(None, imported_text(true, &format!("p {i}"))),
                )
            })
            .collect();
        let finished = Seq(602);
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: finished },
            Utc::now(),
        );
        let mut head = vec![(None, first, adopted_created())];
        head.extend(items[..499].iter().cloned());
        f.log_stamped(adopted, head);
        let list = f.list_view();
        assert_eq!(list.creating.len(), 1, "its seq is not in yet");
        let [listed] = list.sessions.as_slice() else {
            panic!("{list:?}")
        };
        assert!(listed.importing, "{listed:?}");
        assert_eq!(listed.title, "p 0");

        let mut rest = items[499..].to_vec();
        rest.push((
            Some(command.id),
            first,
            EventKind::ImportFinished {
                outcome: ImportOutcome::Imported { items: 600 },
            },
        ));
        f.log_stamped(adopted, rest);
        assert_eq!(f.events.len() as u64, finished.0);
        // Its import is in: the list shows it, no longer creating.
        let list = f.list_view();
        assert_eq!(list.creating, Vec::new());
        assert!(!list.sessions[0].importing);
        // Positive control: a fresh session never imports.
        let plain = f.session("/srv/b");
        assert!(!row(&f, plain).importing);
    }

    #[test]
    fn discard_refused_creation_hides_row() {
        let mut f = Fixture::paired();
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/etc".into(),
            worktree: None,
        });
        let refused = queued(&effects[..2]);
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
            worktree: None,
        });
        let waiting = queued(&effects[..2]);
        f.dispatch(Action::Navigate(Route::SessionList));
        f.model.command_sending(refused.id, Utc::now());
        f.model.command_sending(waiting.id, Utc::now());
        f.model.command_result(
            refused.id,
            CommandResult::Rejected {
                reason: RejectReason::WorkdirNotAllowed,
            },
            Utc::now(),
        );
        // Accepted, its session not yet in the replica: it can't go.
        f.model.command_result(
            waiting.id,
            CommandResult::Accepted { seq: Seq(9) },
            Utc::now(),
        );
        let discardable = |f: &Fixture| -> Vec<(String, bool)> {
            f.list_view()
                .creating
                .into_iter()
                .map(|p| (p.workdir, p.discardable))
                .collect()
        };
        assert_eq!(
            discardable(&f),
            vec![("/etc".to_owned(), true), ("/srv/new".to_owned(), false)]
        );
        assert!(f.dispatch(Action::Discard(waiting.id)).is_empty());
        assert!(
            f.dispatch(Action::EditQueued(refused.id)).is_empty(),
            "only messages are edited"
        );
        assert_eq!(
            f.dispatch(Action::Discard(refused.id)),
            vec![
                Effect::Withdraw {
                    id: refused.id,
                    draft: None,
                    images: Some((created_id(&refused), Vec::new())),
                    files: Some((created_id(&refused), Vec::new())),
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert_eq!(discardable(&f), vec![("/srv/new".to_owned(), false)]);
    }

    #[test]
    fn link_and_replayed_or_gapped_batches() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let before = f.model.clone();
        // Replaying the batch and a batch past a gap change nothing.
        f.model.apply_batch(&EventBatch {
            from: Seq(1),
            events: f.events.clone(),
        });
        f.model.apply_batch(&EventBatch {
            from: Seq(5),
            events: vec![f.events[0].clone()],
        });
        assert_eq!(f.model, before);
        assert_eq!(f.list_view().sessions.len(), 1);
        assert_eq!(f.list_view().sessions[0].id, sid);

        let reconnecting = PairedLink::Reconnecting {
            attempt: 3,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(4),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        };
        f.model.set_link(reconnecting.clone());
        assert_eq!(f.model.view().link, Link::from(reconnecting));
        f.model
            .set_link(PairedLink::Rejected(LinkRejection::Revoked));
        // Local data stays readable while rejected.
        assert_eq!(f.list_view().sessions.len(), 1);
    }

    #[test]
    fn retry_now_only_while_reconnecting() {
        // Unpaired there is nothing to retry, and no link the engine
        // reports makes it paired.
        let mut model = Model::new(empty_snapshot(), None);
        model.set_link(PairedLink::Online { latency_ms: 5 });
        assert_eq!(model.view().link, Link::Unpaired);
        assert!(model.dispatch(Action::RetryNow).is_empty());

        let mut f = Fixture::paired();
        for link in [
            PairedLink::Connecting,
            PairedLink::Online { latency_ms: 5 },
            PairedLink::Rejected(LinkRejection::Revoked),
            PairedLink::Rejected(LinkRejection::BadIdentity),
        ] {
            f.model.set_link(link);
            assert!(f.dispatch(Action::RetryNow).is_empty());
        }
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 2,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(30),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(
            f.dispatch(Action::RetryNow),
            vec![Effect::Request(Request::RetryNow)]
        );
    }

    /// A mismatch may have been the server's, since updated, and a
    /// certificate it presented may be put back: retrying connects again,
    /// where a revoked device or a damaged identity has to re-pair.
    #[test]
    fn retry_now_after_a_protocol_mismatch_connects_again() {
        for rejection in [
            LinkRejection::ProtocolMismatch {
                client: 3,
                server: 2,
            },
            LinkRejection::ServerChanged,
            LinkRejection::NameMismatch,
        ] {
            let mut f = Fixture::paired();
            f.model.set_link(PairedLink::Rejected(rejection));
            assert_eq!(
                f.dispatch(Action::RetryNow),
                vec![Effect::Request(Request::Connect)],
                "{rejection:?}"
            );
            assert_eq!(f.model.view().link, Link::Connecting);
        }
    }

    /// The reset is for a log that lost history only; anywhere else it
    /// would throw the replica away for nothing.
    #[test]
    fn reset_replica_only_after_the_server_lost_history() {
        let mut f = Fixture::paired();
        for link in [
            PairedLink::Connecting,
            PairedLink::Online { latency_ms: 5 },
            PairedLink::Rejected(LinkRejection::Revoked),
            PairedLink::Rejected(LinkRejection::ProtocolMismatch {
                client: 3,
                server: 2,
            }),
        ] {
            f.model.set_link(link);
            assert!(f.dispatch(Action::ResetReplica).is_empty());
        }
        f.model
            .set_link(PairedLink::Rejected(LinkRejection::HistoryLost {
                have: Seq(9),
                server: Seq(4),
            }));
        // Retrying does not get past it.
        assert!(f.dispatch(Action::RetryNow).is_empty());
        assert_eq!(
            f.dispatch(Action::ResetReplica),
            vec![Effect::ResetReplica, Effect::Request(Request::Connect)]
        );
        // Rejected until the reset is stored.
        assert!(matches!(
            f.model.view().link,
            Link::Rejected(LinkRejection::HistoryLost { .. })
        ));
        f.model.history_reset(empty_snapshot());
        assert_eq!(f.model.view().link, Link::Connecting);
    }

    // Tool timeline: calls group their approval and result.

    fn tool_call(call: Option<CallId>, kind: ToolKind, title: &str) -> EventKind {
        EventKind::ToolCall(ToolCall {
            parent: None,
            call,
            name: "agent-tool".into(),
            display: ToolDisplay {
                kind,
                title: title.into(),
                detail: None,
                body: None,
            },
            input: None,
        })
    }

    fn tool_result(call: Option<CallId>, is_error: bool, preview: &str) -> EventKind {
        EventKind::ToolResult(ToolResult {
            call,
            is_error,
            status: None,
            preview: preview.into(),
            lines: 1,
            output: None,
        })
    }

    fn approval_requested(id: ApprovalId, call: Option<CallId>) -> EventKind {
        EventKind::ApprovalRequested {
            id,
            call,
            tool: "agent-tool".into(),
            summary: "the call's title".into(),
            input: None,
            suggestions: Vec::new(),
            shell: None,
        }
    }

    /// The logged entries of the open session, in order.
    fn entries(f: &Fixture) -> Vec<Entry> {
        f.session_view()
            .timeline
            .into_iter()
            .filter_map(|item| match item {
                TimelineItem::Logged { entry, .. } => Some(entry),
                TimelineItem::Pending { .. } | TimelineItem::Change { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_safety_check_approval_shows_on_its_call_and_nowhere_else() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let call = CallId::new();
        f.log(
            sid,
            vec![
                tool_call(Some(call), ToolKind::Shell, "cat notes.txt"),
                EventKind::SafetyCheckApproved {
                    call: Some(call),
                    tool: "agent-tool".into(),
                    summary: "cat notes.txt".into(),
                    reason: "read-only: cat".into(),
                    shell: None,
                },
            ],
        );
        f.open(sid);
        let views = tools(&f);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].safety_check.as_deref(), Some("read-only: cat"));
        assert_eq!(views[0].approval, None, "nobody was asked");

        // Without a call it has nowhere to show.
        f.log(
            sid,
            vec![EventKind::SafetyCheckApproved {
                call: None,
                tool: "agent-tool".into(),
                summary: "ls".into(),
                reason: "read-only: ls".into(),
                shell: None,
            }],
        );
        assert_eq!(entries(&f).len(), 2, "no entry for an unlinked approval");
        assert_eq!(tools(&f)[0].safety_check.as_deref(), Some("read-only: cat"));
    }

    #[test]
    fn safety_feedback_is_offered_on_shell_calls_only() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (asked, approved, unasked, plain) =
            (CallId::new(), CallId::new(), CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                tool_call(Some(asked), ToolKind::Shell, "rm x"),
                EventKind::ApprovalRequested {
                    id: ApprovalId::new(),
                    call: Some(asked),
                    tool: "agent-tool".into(),
                    summary: "rm x".into(),
                    input: None,
                    suggestions: Vec::new(),
                    shell: Some("rm x".into()),
                },
                tool_call(Some(approved), ToolKind::Shell, "ls"),
                EventKind::SafetyCheckApproved {
                    call: Some(approved),
                    tool: "agent-tool".into(),
                    summary: "ls".into(),
                    reason: "read-only: ls".into(),
                    shell: Some("ls".into()),
                },
                // Asked, but no shell command was named: nothing to judge.
                tool_call(Some(unasked), ToolKind::Shell, "curl x"),
                approval_requested(ApprovalId::new(), Some(unasked)),
                tool_call(Some(plain), ToolKind::Shell, "cat notes.txt"),
            ],
        );
        f.open(sid);
        let feedback: Vec<Option<SafetyFeedbackView>> = tools(&f)
            .into_iter()
            .map(|view| view.safety_feedback)
            .collect();
        assert_eq!(
            feedback,
            vec![
                Some(SafetyFeedbackView {
                    judgment: SafetyJudgment::ShouldApprove,
                    sent: None,
                }),
                Some(SafetyFeedbackView {
                    judgment: SafetyJudgment::ShouldAsk,
                    sent: None,
                }),
                None,
                None,
            ]
        );
    }

    #[test]
    fn safety_feedback_shows_until_logged_then_the_log() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let call = CallId::new();
        f.log(
            sid,
            vec![
                tool_call(Some(call), ToolKind::Shell, "rm x"),
                EventKind::ApprovalRequested {
                    id: ApprovalId::new(),
                    call: Some(call),
                    tool: "agent-tool".into(),
                    summary: "rm x".into(),
                    input: None,
                    suggestions: Vec::new(),
                    shell: Some("rm x".into()),
                },
            ],
        );
        f.open(sid);

        let command = queued(&f.dispatch(Action::SafetyCheckFeedback {
            session_id: sid,
            call,
            judgment: SafetyJudgment::ShouldApprove,
            note: Some("fine".into()),
        }));
        assert_eq!(
            command.kind,
            CommandKind::SafetyCheckFeedback {
                session_id: sid,
                call,
                judgment: SafetyJudgment::ShouldApprove,
                note: Some("fine".into()),
            }
        );
        // On its way: this device's feedback, on the call's row.
        assert_eq!(
            tools(&f)[0].safety_feedback,
            Some(SafetyFeedbackView {
                judgment: SafetyJudgment::ShouldApprove,
                sent: Some(FeedbackSent {
                    note: Some("fine".into()),
                    outgoing: Some(Outgoing::Queued),
                }),
            })
        );

        // Logged and accepted: the log holds it, so it is no longer pending.
        let me = f.me;
        f.log_commands(
            sid,
            vec![(
                Some(command.id),
                EventKind::SafetyCheckFeedback {
                    call,
                    judgment: SafetyJudgment::ShouldApprove,
                    note: Some("fine".into()),
                    shell: "rm x".into(),
                    checked_safe: true,
                    checked_reason: "read-only".into(),
                    by: me,
                },
            )],
        );
        let seq = Seq(f.events.len() as u64);
        f.model
            .command_result(command.id, CommandResult::Accepted { seq }, Utc::now());
        assert_eq!(
            tools(&f)[0]
                .safety_feedback
                .as_ref()
                .and_then(|v| v.sent.clone()),
            Some(FeedbackSent {
                note: Some("fine".into()),
                outgoing: None,
            })
        );

        // Another device's feedback after it is the newest, and not pending.
        f.log(
            sid,
            vec![EventKind::SafetyCheckFeedback {
                call,
                judgment: SafetyJudgment::ShouldApprove,
                note: None,
                shell: "rm x".into(),
                checked_safe: true,
                checked_reason: "read-only".into(),
                by: DeviceId::new(),
            }],
        );
        assert_eq!(
            tools(&f)[0]
                .safety_feedback
                .as_ref()
                .and_then(|v| v.sent.clone()),
            Some(FeedbackSent {
                note: None,
                outgoing: None,
            })
        );
    }

    /// A shell row keeps the raw command its approval request or safety
    /// check named, for a head to prefill a rule from. A row neither named
    /// has none.
    #[test]
    fn a_shell_row_keeps_its_command() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (asked, approved, unnamed) = (CallId::new(), CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                tool_call(Some(asked), ToolKind::Shell, "$ git status"),
                EventKind::ApprovalRequested {
                    id: ApprovalId::new(),
                    call: Some(asked),
                    tool: "agent-tool".into(),
                    summary: "git status".into(),
                    input: None,
                    suggestions: Vec::new(),
                    shell: Some("git status".into()),
                },
                tool_call(Some(approved), ToolKind::Shell, "$ ls -la"),
                EventKind::SafetyCheckApproved {
                    call: Some(approved),
                    tool: "agent-tool".into(),
                    summary: "ls -la".into(),
                    reason: "read-only: ls".into(),
                    shell: Some("ls -la".into()),
                },
                tool_call(Some(unnamed), ToolKind::Shell, "$ make"),
                approval_requested(ApprovalId::new(), Some(unnamed)),
            ],
        );
        f.open(sid);
        let shells: Vec<Option<String>> = tools(&f).into_iter().map(|view| view.shell).collect();
        assert_eq!(
            shells,
            vec![Some("git status".into()), Some("ls -la".into()), None]
        );
    }

    #[test]
    fn safety_feedback_needs_a_known_session() {
        let mut f = Fixture::paired();
        let unknown = f.dispatch(Action::SafetyCheckFeedback {
            session_id: SessionId::new(),
            call: CallId::new(),
            judgment: SafetyJudgment::ShouldAsk,
            note: None,
        });
        assert!(unknown.is_empty(), "no command for a session not known");

        // Positive control: the same action for a known session is queued.
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = queued(&f.dispatch(Action::SafetyCheckFeedback {
            session_id: sid,
            call: CallId::new(),
            judgment: SafetyJudgment::ShouldAsk,
            note: None,
        }));
        assert!(matches!(
            command.kind,
            CommandKind::SafetyCheckFeedback { session_id, .. } if session_id == sid
        ));
    }

    #[test]
    fn an_unconfirmed_agent_version_shows_as_an_entry_but_is_no_news() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let before = row(&f, sid);
        f.log(
            sid,
            vec![EventKind::AgentVersionUnconfirmed {
                version: "2.2.0".into(),
                confirmed: "2.1.310".into(),
            }],
        );
        let after = row(&f, sid);
        assert_eq!(
            (after.last_activity, after.unread),
            (before.last_activity, before.unread)
        );
        // Positive control: a message is activity.
        f.log(sid, vec![message("hi")]);
        assert!(row(&f, sid).last_activity > before.last_activity);
        f.open(sid);
        assert_eq!(
            entries(&f)[1],
            Entry::AgentVersionUnconfirmed {
                version: "2.2.0".into(),
                confirmed: "2.1.310".into(),
            }
        );
    }

    fn tools(f: &Fixture) -> Vec<ToolView> {
        entries(f)
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Tool(view) => Some(*view),
                _ => None,
            })
            .collect()
    }

    /// The fixture's time of the n-th logged event.
    fn minute(n: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 29, 10, n, 0).unwrap()
    }

    #[test]
    fn a_call_its_approval_and_its_result_are_one_item() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a"); // minute 0
        let (call, approval) = (CallId::new(), ApprovalId::new());
        let (input, full) = (
            BlobRef {
                id: BlobId::new(),
                size: 9000,
                sha256: skywave_proto::ContentHash::of(b""),
            },
            BlobRef {
                id: BlobId::new(),
                size: 7000,
                sha256: skywave_proto::ContentHash::of(b""),
            },
        );
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                EventKind::ToolCall(ToolCall {
                    parent: None,
                    call: Some(call),
                    name: "Write".into(),
                    display: ToolDisplay {
                        kind: ToolKind::FileWrite,
                        title: "src/lib.rs".into(),
                        detail: Some("new file".into()),
                        body: Some(skywave_proto::Body {
                            format: skywave_proto::BodyFormat::Diff,
                            preview: "+fn main() {}".into(),
                            lines: 300,
                            full: Some(Full::Blob(full.clone())),
                        }),
                    },
                    input: Some(Full::Blob(input.clone())),
                }),
                approval_requested(approval, Some(call)),
            ],
        );
        // The body's whole content is a blob like any other.
        assert!(f.model.blobs.contains_key(&full.id));
        f.open(sid);

        let view = f.session_view();
        assert_eq!(view.pending_approval.as_ref().unwrap().call, Some(call));
        let blob = |b: &BlobRef| {
            FullView::Blob(BlobView {
                id: b.id,
                state: BlobState::NotDownloaded { size: b.size },
                change: None,
                pending: None,
            })
        };
        let waiting = ToolView {
            subagent: None,
            call,
            name: "Write".into(),
            kind: ToolKind::FileWrite,
            title: "src/lib.rs".into(),
            detail: Some("new file".into()),
            body: Some(BodyView {
                format: skywave_proto::BodyFormat::Diff,
                preview: "+fn main() {}".into(),
                lines: 300,
                full: Some(blob(&full)),
            }),
            input: Some(blob(&input)),
            approval: Some(ToolApproval {
                id: approval,
                question: false,
                status: ApprovalStatus::Pending,
                answer: None,
                input: None,
            }),
            safety_check: None,
            safety_feedback: None,
            shell: None,
            outcome: ToolOutcome::Running { since: minute(2) },
        };
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::Tool(Box::new(waiting.clone())),
            ]
        );
        assert!(matches!(
            view.timeline[1],
            TimelineItem::Logged { seq: Seq(3), .. }
        ));

        f.log(
            sid,
            vec![
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: f.me,
                    rule: None,
                },
                EventKind::ToolResult(ToolResult {
                    call: Some(call),
                    is_error: false,
                    status: Some("created".into()),
                    preview: "ok".into(),
                    lines: 1,
                    output: None,
                }),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        let done = ToolView {
            approval: Some(ToolApproval {
                id: approval,
                question: false,
                status: ApprovalStatus::Resolved {
                    decision: Decision::Approve,
                    by: DeviceRef {
                        id: f.me,
                        name: None,
                        me: true,
                    },
                    rule: None,
                },
                answer: None,
                input: None,
            }),
            outcome: ToolOutcome::Done {
                is_error: false,
                status: Some("created".into()),
                preview: "ok".into(),
                lines: 1,
                output: None,
                took: Duration::from_secs(3 * 60),
            },
            ..waiting
        };
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::Tool(Box::new(done)),
            ]
        );
        assert_eq!(f.session_view().pending_approval, None);
    }

    #[test]
    fn an_approval_folded_into_its_call_keeps_its_own_input() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (call, approval, plain) = (CallId::new(), ApprovalId::new(), ApprovalId::new());
        let asked = BlobRef {
            id: BlobId::new(),
            size: 5000,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        let other = CallId::new();
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(call), ToolKind::Shell, "make"),
                EventKind::ApprovalRequested {
                    id: approval,
                    call: Some(call),
                    tool: "agent-tool".into(),
                    summary: "make".into(),
                    input: Some(Full::Blob(asked.clone())),
                    suggestions: Vec::new(),
                    shell: None,
                },
                // Positive control: one asking what its call holds.
                tool_call(Some(other), ToolKind::Shell, "ls"),
                approval_requested(plain, Some(other)),
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Approve,
                    by: f.me,
                    rule: None,
                },
            ],
        );
        f.open(sid);
        let approvals: Vec<_> = tools(&f)
            .into_iter()
            .map(|v| (v.call, v.approval.and_then(|a| a.input)))
            .collect();
        assert_eq!(
            approvals,
            vec![
                (
                    call,
                    Some(FullView::Blob(BlobView {
                        id: asked.id,
                        state: BlobState::NotDownloaded { size: asked.size },
                        change: None,
                        pending: None,
                    }))
                ),
                (other, None),
            ]
        );
    }

    #[test]
    fn parallel_calls_take_their_own_results_in_any_order() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (a, b) = (CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(a), ToolKind::FileRead, "a.txt"),
                tool_call(Some(b), ToolKind::Search, "needle"),
                tool_result(Some(b), false, "3 matches"),
            ],
        );
        f.open(sid);
        let outcomes = |f: &Fixture| -> Vec<(CallId, ToolOutcome)> {
            tools(f).into_iter().map(|v| (v.call, v.outcome)).collect()
        };
        let done = |preview: &str, minutes: u64| ToolOutcome::Done {
            is_error: false,
            status: None,
            preview: preview.into(),
            lines: 1,
            output: None,
            took: Duration::from_secs(minutes * 60),
        };
        assert_eq!(
            outcomes(&f),
            vec![
                (a, ToolOutcome::Running { since: minute(2) }),
                (b, done("3 matches", 1)),
            ]
        );
        f.log(sid, vec![tool_result(Some(a), false, "contents")]);
        assert_eq!(
            outcomes(&f),
            vec![(a, done("contents", 3)), (b, done("3 matches", 1))]
        );
        assert_eq!(entries(&f).len(), 3);
    }

    #[test]
    fn a_denial_and_its_error_result_are_one_denied_item() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (call, approval, phone) = (CallId::new(), ApprovalId::new(), DeviceId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(call), ToolKind::Shell, "rm -rf build"),
                approval_requested(approval, Some(call)),
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Deny,
                    by: phone,
                    rule: None,
                },
                tool_result(Some(call), true, "the user denied this"),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        f.open(sid);
        let entries = entries(&f);
        let [Entry::Created { .. }, Entry::Tool(view)] = entries.as_slice() else {
            panic!("expected one tool item, got {entries:?}");
        };
        assert_eq!(
            view.outcome,
            ToolOutcome::Denied {
                by: DeviceRef {
                    id: phone,
                    name: None,
                    me: false
                }
            }
        );
        assert_eq!(
            view.approval,
            Some(ToolApproval {
                id: approval,
                question: false,
                status: ApprovalStatus::Resolved {
                    decision: Decision::Deny,
                    by: DeviceRef {
                        id: phone,
                        name: None,
                        me: false,
                    },
                    rule: None,
                },
                answer: None,
                input: None,
            })
        );
    }

    #[test]
    fn a_cancelled_approval_cancels_its_call() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (call, approval) = (CallId::new(), ApprovalId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(call), ToolKind::FileEdit, "a.rs"),
                approval_requested(approval, Some(call)),
                EventKind::ApprovalCancelled {
                    id: approval,
                    reason: "daemon restarted".into(),
                },
                EventKind::SessionStateChanged(LogState::Errored {
                    message: "turn lost".into(),
                }),
            ],
        );
        f.open(sid);
        let view = &tools(&f)[0];
        assert_eq!(
            view.outcome,
            ToolOutcome::Cancelled {
                reason: "daemon restarted".into()
            }
        );
        assert_eq!(
            view.approval.as_ref().unwrap().status,
            ApprovalStatus::Cancelled {
                reason: "daemon restarted".into()
            }
        );
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::Tool(Box::new(view.clone())),
                Entry::AgentErrored {
                    message: "turn lost".into()
                }
            ]
        );
    }

    #[test]
    fn a_turn_ending_leaves_its_calls_running_until_logged_unanswered() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (a, b) = (CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(a), ToolKind::Shell, "sleep 1"),
                EventKind::TurnInterrupted,
                EventKind::SessionStateChanged(LogState::Idle),
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(b), ToolKind::Fetch, "https://example.com"),
                EventKind::SessionStateChanged(LogState::Errored {
                    message: "gone".into(),
                }),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        f.open(sid);
        let outcomes =
            |f: &Fixture| -> Vec<ToolOutcome> { tools(f).into_iter().map(|v| v.outcome).collect() };
        // No state change settles a call: a background subagent's may
        // still be at work.
        assert_eq!(
            outcomes(&f),
            vec![
                ToolOutcome::Running { since: minute(2) },
                ToolOutcome::Running { since: minute(6) },
            ]
        );
        f.log(sid, vec![EventKind::CallsUnanswered { calls: vec![a] }]);
        assert_eq!(
            outcomes(&f),
            vec![
                ToolOutcome::NoResult,
                ToolOutcome::Running { since: minute(6) },
            ]
        );
    }

    #[test]
    fn calls_unanswered_ends_exactly_the_named_running_calls() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (named, done, other) = (CallId::new(), CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(named), ToolKind::Subagent, "explore"),
                tool_call(Some(done), ToolKind::Shell, "ls"),
                tool_call(Some(other), ToolKind::Fetch, "https://example.com"),
                tool_result(Some(done), false, "ok"),
                EventKind::CallsUnanswered {
                    calls: vec![named, done, CallId::new()],
                },
            ],
        );
        f.open(sid);
        let views = tools(&f);
        assert_eq!(views[0].outcome, ToolOutcome::NoResult);
        assert!(matches!(views[1].outcome, ToolOutcome::Done { .. }));
        // The control: a running call it does not name stays running.
        assert!(matches!(views[2].outcome, ToolOutcome::Running { .. }));
        assert_eq!(views.len(), 3, "it adds no item");
        assert_eq!(entries(&f).len(), 4);
    }

    #[test]
    fn events_without_a_call_stay_separate_entries() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (approval, stray) = (ApprovalId::new(), CallId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(None, ToolKind::Other, "{\"command\":\"ls\"}"),
                approval_requested(approval, None),
                EventKind::ApprovalResolved {
                    id: approval,
                    decision: Decision::Deny,
                    by: f.me,
                    rule: None,
                },
                tool_result(None, false, "[error] denied"),
                // A result for a call that is not in the log.
                tool_result(Some(stray), false, "orphan"),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        f.open(sid);
        assert_eq!(
            entries(&f),
            vec![
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::ToolCall {
                    name: "agent-tool".into(),
                    summary: "{\"command\":\"ls\"}".into(),
                    input: None,
                },
                Entry::Approval {
                    id: approval,
                    tool: "agent-tool".into(),
                    summary: "the call's title".into(),
                    input: None,
                    status: ApprovalStatus::Resolved {
                        decision: Decision::Deny,
                        by: DeviceRef {
                            id: f.me,
                            name: None,
                            me: true,
                        },
                        rule: None,
                    },
                    answer: None,
                },
                Entry::ToolResult {
                    summary: "[error] denied".into(),
                    output: None,
                },
                Entry::ToolResult {
                    summary: "orphan".into(),
                    output: None,
                },
            ]
        );
    }

    fn agent(text: &str) -> EventKind {
        EventKind::AgentMessage {
            parent: None,
            text: text.into(),
            full: None,
        }
    }

    fn unread(f: &Fixture, id: SessionId) -> u64 {
        f.list_view()
            .sessions
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.unread)
            .expect("listed")
    }

    fn mark_read(id: SessionId, through: u64) -> Action {
        Action::MarkRead {
            session_id: id,
            through: Seq(through),
        }
    }

    #[test]
    fn mark_read_is_monotonic_and_only_for_open_session() {
        let mut f = Fixture::paired();
        let (a, b) = (f.session("/srv/a"), f.session("/srv/b"));
        f.log(a, vec![agent("one"), agent("two")]);
        f.log(b, vec![agent("elsewhere")]);
        assert_eq!((unread(&f, a), unread(&f, b)), (2, 1));

        // Not open: nothing to mark.
        assert!(f.dispatch(mark_read(a, 4)).is_empty());
        f.open(a);
        assert!(f.dispatch(mark_read(b, 5)).is_empty(), "not the open one");

        // Through seq 3 (`a` created, `b` created, "one"): one notable event.
        let mark = |seq, notable| ReadMark {
            seq: Seq(seq),
            notable,
        };
        assert_eq!(
            f.dispatch(mark_read(a, 3)),
            vec![Effect::SaveReadMark(a, mark(3, 1))]
        );
        assert!(f.dispatch(mark_read(a, 3)).is_empty(), "no step back");
        assert!(f.dispatch(mark_read(a, 2)).is_empty(), "no step back");
        // Clamped to the session's newest event, seq 4.
        assert_eq!(
            f.dispatch(mark_read(a, 99)),
            vec![Effect::SaveReadMark(a, mark(4, 2))]
        );
        assert!(f.dispatch(mark_read(a, 100)).is_empty());

        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!((unread(&f, a), unread(&f, b)), (0, 1));
        assert!(f.dispatch(mark_read(a, 4)).is_empty(), "closed");
        f.log(a, vec![agent("three")]);
        assert_eq!(unread(&f, a), 1);
    }

    #[test]
    fn own_messages_never_count_as_unread() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let me = f.me;
        let message = |device_id| EventKind::UserMessage {
            device_id,
            text: "hi".into(),
            attachments: Vec::new(),
        };
        f.log(
            sid,
            vec![
                message(me),
                message(DeviceId::new()),
                tool_call(None, ToolKind::Shell, "ls"),
                tool_result(None, false, "ok"),
                EventKind::SessionStateChanged(LogState::AgentWorking),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        assert_eq!(unread(&f, sid), 0);
        f.open(sid);
        assert_eq!(f.session_view().unread_from, None, "nothing new to show");

        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::Errored {
                message: "boom".into(),
            })],
        );
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(unread(&f, sid), 1, "an error is news");
    }

    #[test]
    fn divider_stays_put_while_marking_read() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log(sid, vec![agent("seen")]);
        f.open(sid);
        assert_eq!(f.session_view().unread_from, Some(Seq(1)), "never read");
        f.dispatch(mark_read(sid, 2));
        f.dispatch(Action::Navigate(Route::SessionList));

        // Away: another device's message, then the agent's answer.
        f.log(
            sid,
            vec![
                EventKind::UserMessage {
                    device_id: DeviceId::new(),
                    text: "go on".into(),
                    attachments: Vec::new(),
                },
                agent("done"),
            ],
        );
        f.open(sid);
        let view = f.session_view();
        assert_eq!(view.unread_from, Some(Seq(3)), "the first unseen event");
        assert_eq!(view.last_seq, Seq(4));

        f.dispatch(mark_read(sid, 4));
        f.log(sid, vec![agent("more")]);
        f.dispatch(mark_read(sid, 5));
        assert_eq!(
            f.session_view().unread_from,
            Some(Seq(3)),
            "fixed while open"
        );

        // Reopened with everything read: no divider.
        f.dispatch(Action::Navigate(Route::SessionList));
        f.open(sid);
        assert_eq!(f.session_view().unread_from, None);
    }

    #[test]
    fn divider_goes_below_own_messages() {
        let mut f = Fixture::paired();
        let me = f.me;
        let message = |device_id| EventKind::UserMessage {
            device_id,
            text: "hi".into(),
            attachments: Vec::new(),
        };
        // Created here: the first message, then the agent's answer.
        let sid = f.session("/srv/a");
        f.log(sid, vec![message(me), agent("hello")]);
        f.open(sid);
        assert_eq!(
            f.session_view().unread_from,
            Some(Seq(3)),
            "past my message"
        );
        f.dispatch(Action::Navigate(Route::SessionList));

        // Another device's message first is news to this one.
        let other = f.session("/srv/b");
        f.log(
            other,
            vec![message(DeviceId::new()), message(me), agent("ok")],
        );
        f.open(other);
        // From the start (seq 4, not shown), so above their message.
        assert_eq!(f.session_view().unread_from, Some(Seq(4)), "theirs");
        f.dispatch(Action::Navigate(Route::SessionList));

        // Mine after theirs, then a tool call and the answer.
        let third = f.session("/srv/c");
        f.log(
            third,
            vec![
                message(me),
                message(DeviceId::new()),
                message(me),
                tool_call(None, ToolKind::Shell, "ls"),
                agent("done"),
            ],
        );
        f.open(third);
        assert_eq!(f.session_view().unread_from, Some(Seq(10)), "theirs");
        f.dispatch(mark_read(third, 13));
        f.dispatch(Action::Navigate(Route::SessionList));

        // Read, then mine: the divider goes on what the agent did next.
        f.log(
            third,
            vec![
                message(me),
                tool_call(None, ToolKind::Shell, "ls"),
                agent("more"),
            ],
        );
        f.open(third);
        assert_eq!(f.session_view().unread_from, Some(Seq(15)), "the tool call");
    }

    const ONLINE: PairedLink = PairedLink::Online { latency_ms: 5 };

    fn catching_up() -> PairedLink {
        PairedLink::CatchingUp {
            have: Seq(1),
            server: Seq(9),
        }
    }

    /// What the store would load for the model's replica.
    fn snapshot(model: &Model) -> Snapshot {
        Snapshot {
            cursor: model.cursor,
            sessions: model.sessions.clone(),
            outbox: model.outbox.clone(),
            blobs: model.blobs.values().cloned().collect(),
            fates: model.fates.clone(),
            pending_crops: Vec::new(),
            settings: model.settings.clone(),
            drafts: model.drafts.clone(),
            draft_images: model.draft_images.clone(),
            draft_files: model.draft_files.clone(),
            attachments: model.attachments.clone(),
            composer_modes: model.composer_modes.clone(),
            read_marks: model.read_marks.clone(),
            deleted: model.deleted.clone(),
            alerted: model.alerted.clone(),
            devices: model.devices.clone(),
            models: model.models.clone(),
            safety_rules: model.safety_rules.clone(),
            snippets: model.snippets.list().to_vec(),
            shell_history: model.shell_history.list().to_vec(),
            turns: model.turns.clone(),
            workdirs: model.workdirs.stored.clone(),
            usage_limits: model.usage_limits.stored.clone(),
        }
    }

    fn alerts(f: &Fixture) -> u64 {
        f.model.view().attention.alerts
    }

    fn resolved(id: ApprovalId) -> EventKind {
        EventKind::ApprovalResolved {
            id,
            decision: Decision::Approve,
            by: DeviceId::new(),
            rule: None,
        }
    }

    #[test]
    fn approvals_alerted_before_the_start_never_alert_again() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        let id = ApprovalId::new();
        f.log(sid, vec![approval_requested(id, None)]);
        assert_eq!(alerts(&f), 1);
        assert_eq!(f.model.view().attention.approvals[0].alerted_at, 1);
        let mut model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
        assert_eq!(model.view().attention.alerts, 0, "counts from zero");
        assert!(model.set_link(ONLINE).is_empty(), "nothing new to store");
        let attention = model.view().attention;
        assert_eq!(attention.alerts, 0);
        assert_eq!(attention.approvals.len(), 1);
        assert_eq!(attention.approvals[0].title, "app");
        assert_eq!(
            attention.approvals[0].alerted_at, 0,
            "an earlier process alerted on it"
        );
    }

    #[test]
    fn safety_rules_fold_live_and_survive_a_restart() {
        let mut f = Fixture::paired();
        let (a, b) = (f.session("/srv/app"), f.session("/srv/api"));
        let by = DeviceId::new();
        let git: Vec<String> = vec!["git".into(), "commit".into()];
        let cargo: Vec<String> = vec!["cargo".into(), "test".into()];
        let set = |words: &Vec<String>, on: bool| EventKind::SafetyRuleSet {
            words: words.clone(),
            on,
            by,
        };
        f.log(a, vec![set(&git, true)]);
        f.log(a, vec![set(&cargo, true)]);
        f.log(b, vec![set(&git, false)]);
        // Git's newest switch came after cargo's, so it moved to the end.
        let expected = vec![
            SafetyRule {
                words: cargo.clone(),
                on: true,
                session_id: a,
            },
            SafetyRule {
                words: git.clone(),
                on: false,
                session_id: b,
            },
        ];
        assert_eq!(f.model.safety_rules, expected, "live fold");
        let model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
        assert_eq!(model.safety_rules, expected, "after a restart");
    }

    #[test]
    fn an_approval_pending_at_start_but_never_alerted_alerts_once_online() {
        // The previous process pulled it in a catch-up it never finished.
        let mut f = Fixture::paired();
        f.model.set_link(catching_up());
        let sid = f.session("/srv/app");
        let (id, answered) = (ApprovalId::new(), ApprovalId::new());
        f.log(
            sid,
            vec![
                approval_requested(id, None),
                approval_requested(answered, None),
            ],
        );
        queued(&f.dispatch(Action::ResolveApproval {
            id: answered,
            decision: Decision::Deny,
            rule: None,
        }));
        let mut model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
        assert_eq!(model.view().attention.alerts, 0, "not before online");
        assert_eq!(model.view().attention.approvals[0].alerted_at, 0);
        assert!(model.set_link(catching_up()).is_empty());
        assert_eq!(model.view().attention.alerts, 0, "nor while catching up");
        let mut marked = model.set_link(ONLINE);
        marked.sort_by_key(|(_, id)| *id);
        let mut both = vec![(sid, id), (sid, answered)];
        both.sort_by_key(|(_, id)| *id);
        assert_eq!(marked, both, "the answered one is stored as seen too");
        let attention = model.view().attention;
        assert_eq!(attention.alerts, 1);
        assert_eq!(
            attention
                .approvals
                .iter()
                .map(|a| (a.id, a.alerted_at))
                .collect::<Vec<_>>(),
            vec![(id, 1)],
            "heads learn the first snapshot's approval alerted"
        );
        model.set_link(catching_up());
        model.set_link(ONLINE);
        assert_eq!(model.view().attention.alerts, 1, "once only");
        assert_eq!(model.view().attention.approvals[0].alerted_at, 1);
    }

    #[test]
    fn an_approval_answered_here_before_the_start_never_alerts() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        let id = ApprovalId::new();
        f.log(sid, vec![approval_requested(id, None)]);
        queued(&f.dispatch(Action::ResolveApproval {
            id,
            decision: Decision::Approve,
            rule: None,
        }));
        let mut model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
        assert_eq!(model.set_link(ONLINE), vec![(sid, id)]);
        assert_eq!(model.view().attention.alerts, 0);
    }

    #[test]
    fn new_approvals_while_online_alert_once_per_batch() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        assert_eq!(alerts(&f), 0);
        f.log(sid, vec![approval_requested(ApprovalId::new(), None)]);
        assert_eq!(alerts(&f), 1);
        // Many at once ring once; nothing new rings nothing.
        f.log(
            sid,
            vec![
                approval_requested(ApprovalId::new(), None),
                approval_requested(ApprovalId::new(), None),
            ],
        );
        assert_eq!(alerts(&f), 2);
        f.log(sid, vec![agent("still waiting")]);
        f.model.set_link(PairedLink::Online { latency_ms: 9 });
        assert_eq!(alerts(&f), 2);
        // Requested and resolved in one batch: no news.
        let quick = ApprovalId::new();
        f.log(sid, vec![approval_requested(quick, None), resolved(quick)]);
        assert_eq!(alerts(&f), 2);
        // A replayed batch changes nothing.
        let before = f.model.clone();
        f.model.apply_batch(&EventBatch {
            from: Seq(1),
            events: f.events.clone(),
        });
        assert_eq!(f.model, before);
    }

    /// The sessions the idle alerts name, each with the count it rang at.
    fn idle(f: &Fixture) -> Vec<(SessionId, u64)> {
        f.model
            .view()
            .attention
            .idle
            .iter()
            .map(|alert| (alert.session_id, alert.at))
            .collect()
    }

    fn idle_alerts(f: &Fixture) -> u64 {
        f.model.view().attention.idle_alerts
    }

    #[test]
    fn a_turn_seen_while_online_alerts_once_when_it_ends() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        assert_eq!(idle_alerts(&f), 0, "still working");
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle_alerts(&f), 1);
        assert_eq!(idle(&f), vec![(sid, 1)]);
        let title = f.model.view().attention.idle[0].title.clone();
        assert_eq!(title, f.list_view().sessions[0].title);
        f.log(sid, vec![agent("still here")]);
        assert_eq!(idle_alerts(&f), 1, "an unrelated message is no news");
        assert_eq!(idle(&f), vec![(sid, 1)]);
    }

    #[test]
    fn working_again_takes_the_session_out_of_idle() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle(&f), vec![(sid, 1)]);
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        assert!(idle(&f).is_empty(), "working again leaves idle");
        assert_eq!(idle_alerts(&f), 1);
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle(&f), vec![(sid, 2)]);
        assert_eq!(idle_alerts(&f), 2);
    }

    #[test]
    fn loops_and_ambient_tasks_are_not_work() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        let schedule = skywave_proto::Schedule {
            id: skywave_proto::ScheduleId("s1".into()),
            when: "*/5 * * * *".into(),
            recurring: true,
            prompt: "check".into(),
            outlives_agent: None,
        };
        let ambient = skywave_proto::BackgroundTask {
            ambient: true,
            ..work_task("t2")
        };
        f.log(
            sid,
            vec![EventKind::SchedulesChanged {
                schedules: vec![schedule],
            }],
        );
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged {
                tasks: vec![ambient],
            }],
        );
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle_alerts(&f), 1, "the turn's end, once");
        f.log(
            sid,
            vec![
                EventKind::SchedulesChanged { schedules: vec![] },
                EventKind::BackgroundTasksChanged { tasks: vec![] },
            ],
        );
        assert_eq!(idle_alerts(&f), 1, "clearing them is no further alert");
    }

    #[test]
    fn a_background_task_holds_the_alert_until_it_ends() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged {
                tasks: vec![work_task("t1")],
            }],
        );
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle_alerts(&f), 0, "the task is still work");
        assert!(idle(&f).is_empty());
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged { tasks: vec![] }],
        );
        assert_eq!(idle_alerts(&f), 1);
        assert_eq!(idle(&f), vec![(sid, 1)]);
    }

    #[test]
    fn a_turn_ended_and_stopped_in_one_batch_does_not_alert() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::Idle),
                EventKind::SessionStopped,
            ],
        );
        assert_eq!(idle_alerts(&f), 0, "stopped, not settled");
        assert!(idle(&f).is_empty());
        let errored = f.session("/srv/api");
        f.log(
            errored,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(
            errored,
            vec![EventKind::SessionStateChanged(LogState::Errored {
                message: "boom".into(),
            })],
        );
        assert_eq!(idle_alerts(&f), 0, "an error is not settled either");
    }

    #[test]
    fn a_session_deleted_with_its_turn_end_does_not_alert() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::Idle),
                EventKind::SessionDeleted { by: f.me },
            ],
        );
        assert_eq!(idle_alerts(&f), 0, "deleted, so not settled");
        assert!(idle(&f).is_empty());
    }

    #[test]
    fn catch_up_alerts_only_for_work_seen_before() {
        // Never online: a turn met only while catching up is past.
        let mut f = Fixture::paired();
        f.model.set_link(catching_up());
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle_alerts(&f), 0);
        assert!(idle(&f).is_empty());
        // Online at the start, then catching up: its end alerts.
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.model.set_link(catching_up());
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle(&f), vec![(sid, 1)]);
    }

    #[test]
    fn work_found_at_start_alerts_when_it_ends() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        // A restart mid-turn: the turn was seen before, not in this process.
        f.model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
        f.model.set_link(catching_up());
        assert!(idle(&f).is_empty(), "nothing has ended yet");
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle(&f), vec![(sid, 1)]);
    }

    #[test]
    fn a_replayed_batch_changes_nothing() {
        let mut f = Fixture::paired();
        f.model.set_link(ONLINE);
        let sid = f.session("/srv/app");
        f.log(
            sid,
            vec![EventKind::SessionStateChanged(LogState::AgentWorking)],
        );
        f.log(sid, vec![EventKind::SessionStateChanged(LogState::Idle)]);
        assert_eq!(idle_alerts(&f), 1);
        let before = f.model.clone();
        f.model.apply_batch(&EventBatch {
            from: Seq(1),
            events: f.events.clone(),
        });
        assert_eq!(f.model, before);
    }

    #[test]
    fn each_approval_tells_the_alert_it_rang_for() {
        let mut f = Fixture::paired();
        f.model.set_link(catching_up());
        let sid = f.session("/srv/app");
        // Answered here while catching up, the answer then refused: it shows
        // again, but was never news.
        let mine = ApprovalId::new();
        f.log(sid, vec![approval_requested(mine, None)]);
        let answer = queued(&f.dispatch(Action::ResolveApproval {
            id: mine,
            decision: Decision::Approve,
            rule: None,
        }));
        f.model.command_result(
            answer.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownRule,
            },
            Utc::now(),
        );
        f.model.set_link(ONLINE);
        let at = |f: &Fixture| {
            let mut at: Vec<_> = f
                .model
                .view()
                .attention
                .approvals
                .iter()
                .map(|a| (a.id, a.alerted_at))
                .collect();
            at.sort();
            at
        };
        assert_eq!(alerts(&f), 0);
        assert_eq!(at(&f), vec![(mine, 0)]);

        let first = ApprovalId::new();
        f.log(sid, vec![approval_requested(first, None)]);
        let (b, c) = (ApprovalId::new(), ApprovalId::new());
        f.log(
            sid,
            vec![approval_requested(b, None), approval_requested(c, None)],
        );
        assert_eq!(alerts(&f), 2);
        let mut want = vec![(mine, 0), (first, 1), (b, 2), (c, 2)];
        want.sort();
        assert_eq!(at(&f), want);

        // Resolved, it is forgotten; a resync keeps the rest.
        f.log(sid, vec![resolved(first)]);
        f.model.reload(snapshot(&f.model));
        want.retain(|(id, _)| *id != first);
        assert_eq!(at(&f), want);
    }

    #[test]
    fn approvals_during_a_catch_up_alert_once_it_ends_if_still_pending() {
        let mut f = Fixture::paired();
        f.model.set_link(catching_up());
        let sid = f.session("/srv/app");
        let (gone, still) = (ApprovalId::new(), ApprovalId::new());
        f.log(sid, vec![approval_requested(gone, None)]);
        f.log(sid, vec![approval_requested(still, None)]);
        f.log(sid, vec![resolved(gone)]);
        assert_eq!(alerts(&f), 0, "nothing while catching up");
        f.model.set_link(ONLINE);
        assert_eq!(alerts(&f), 1);
        f.model.set_link(catching_up());
        f.model.set_link(ONLINE);
        assert_eq!(alerts(&f), 1, "once only");

        // Requested and resolved within one catch-up: never news.
        f.model.set_link(catching_up());
        let brief = ApprovalId::new();
        f.log(sid, vec![approval_requested(brief, None)]);
        f.log(sid, vec![resolved(brief)]);
        f.model.set_link(ONLINE);
        assert_eq!(alerts(&f), 1);
    }

    #[test]
    fn a_resync_keeps_an_unalerted_approval_alertable() {
        let mut f = Fixture::paired();
        f.model.set_link(catching_up());
        let sid = f.session("/srv/app");
        f.log(sid, vec![approval_requested(ApprovalId::new(), None)]);
        f.model.reload(snapshot(&f.model));
        assert_eq!(alerts(&f), 0, "a resync alerts nothing by itself");
        f.model.set_link(ONLINE);
        assert_eq!(alerts(&f), 1);
        f.model.reload(snapshot(&f.model));
        f.log(sid, vec![agent("more")]);
        assert_eq!(alerts(&f), 1, "nor does it forget what was alerted");
    }

    #[test]
    fn an_approval_answered_here_never_alerts() {
        let mut f = Fixture::paired();
        f.model.set_link(catching_up());
        let sid = f.session("/srv/app");
        let id = ApprovalId::new();
        f.log(sid, vec![approval_requested(id, None)]);
        queued(&f.dispatch(Action::ResolveApproval {
            id,
            decision: Decision::Deny,
            rule: None,
        }));
        f.model.set_link(ONLINE);
        let attention = f.model.view().attention;
        assert_eq!(attention.alerts, 0);
        assert!(attention.approvals.is_empty(), "answered: nothing to do");
    }

    #[test]
    fn open_next_approval_goes_oldest_first_and_cycles() {
        let mut f = Fixture::paired();
        let mut ids = [ApprovalId::new(), ApprovalId::new(), ApprovalId::new()];
        ids.sort();
        let (s1, s2, s3) = (f.session("/a"), f.session("/b"), f.session("/c"));
        // Logged out of order: the order is the approvals' age.
        f.log(s2, vec![approval_requested(ids[1], None)]);
        f.log(s3, vec![approval_requested(ids[2], None)]);
        f.log(s1, vec![approval_requested(ids[0], None)]);
        let attention = f.model.view().attention;
        let order: Vec<_> = attention.approvals.iter().map(|a| a.id).collect();
        assert_eq!(order, ids);
        assert_eq!(attention.elsewhere, 3, "no session open");

        let mut visited = Vec::new();
        for _ in 0..4 {
            let effects = f.dispatch(Action::OpenNextApproval);
            let [Effect::LoadSession(id)] = effects[..] else {
                panic!("expected a session to load, got {effects:?}");
            };
            f.open(id);
            visited.push(id);
        }
        assert_eq!(visited, [s1, s2, s3, s1]);
        assert_eq!(f.model.view().attention.elsewhere, 2);

        // Only the open session waits: nowhere to go.
        f.log(s2, vec![resolved(ids[1])]);
        f.log(s3, vec![resolved(ids[2])]);
        assert!(f.dispatch(Action::OpenNextApproval).is_empty());
        assert_eq!(f.model.view().attention.elsewhere, 0);
        assert_eq!(f.session_view().id, s1);
    }

    fn complete(f: &mut Fixture, input: &str) -> Option<(u64, String)> {
        let effects = f.dispatch(Action::CompleteWorkdir {
            input: input.into(),
        });
        match effects.as_slice() {
            [] => None,
            [Effect::Request(Request::ListDirs { req, dir })] => Some((*req, dir.clone())),
            other => panic!("expected a listing request, got {other:?}"),
        }
    }

    fn completion(f: &Fixture) -> Completion {
        f.list_view().completion.expect("a completion")
    }

    fn candidates(f: &Fixture) -> Vec<(String, CandidateSource)> {
        completion(f)
            .candidates
            .into_iter()
            .map(|c| (c.path, c.source))
            .collect()
    }

    fn listed(dir: &str, names: &[&str]) -> Result<DirsResult, TransportError> {
        Ok(DirsResult::Listed {
            dir: dir.into(),
            names: names.iter().map(|&n| n.to_owned()).collect(),
            truncated: false,
        })
    }

    #[test]
    fn completion_shows_recents_until_the_server_listing_replaces_them() {
        let mut f = Fixture::paired();
        f.session("/srv/api");
        f.session("/srv/app");
        f.session("/srv/api");
        f.session("/home/me");
        assert_eq!(f.list_view().completion, None);

        let (req, dir) = complete(&mut f, "/srv/a").unwrap();
        assert_eq!(dir, "/srv/");
        assert_eq!(completion(&f).status, CompletionStatus::Loading);
        assert_eq!(
            candidates(&f),
            vec![
                ("/srv/api".to_owned(), CandidateSource::Recent),
                ("/srv/app".to_owned(), CandidateSource::Recent),
            ],
            "distinct, most recently active first"
        );

        f.model
            .dirs_listed(req, listed("/srv", &[".cache", "api", "app", "apt", "web"]));
        let done = completion(&f);
        assert_eq!(done.status, CompletionStatus::Ready { truncated: false });
        assert_eq!(
            candidates(&f),
            ["/srv/api", "/srv/app", "/srv/apt"].map(|p| (p.to_owned(), CandidateSource::Server))
        );
        assert_eq!(done.fill().as_deref(), Some("/srv/ap"));

        // Typing within the dir filters the same listing.
        assert_eq!(complete(&mut f, "/srv/apt"), None);
        assert_eq!(completion(&f).fill().as_deref(), Some("/srv/apt/"));
        // Completing the same input again asks again, showing the old
        // listing meanwhile.
        let (again, _) = complete(&mut f, "/srv/apt").unwrap();
        assert!(again > req);
        assert_eq!(completion(&f).status, CompletionStatus::Loading);
        assert_eq!(candidates(&f).len(), 1);

        // Closing the prompt forgets everything.
        assert!(f.dispatch(Action::EndCompletion).is_empty());
        assert_eq!(f.list_view().completion, None);
        f.model.dirs_listed(again, listed("/srv", &["apt"]));
        assert_eq!(f.list_view().completion, None);
    }

    #[test]
    fn a_stale_listing_is_ignored() {
        let mut f = Fixture::paired();
        let (old, _) = complete(&mut f, "/srv/").unwrap();
        let (new, dir) = complete(&mut f, "/home/").unwrap();
        assert_eq!(dir, "/home/");
        f.model.dirs_listed(old, listed("/srv", &["api"]));
        assert_eq!(completion(&f).status, CompletionStatus::Loading);
        assert_eq!(candidates(&f), Vec::new());
        f.model.dirs_listed(new, listed("/home", &["me"]));
        assert_eq!(
            candidates(&f),
            vec![("/home/me".to_owned(), CandidateSource::Server)]
        );
    }

    #[test]
    fn offline_or_failed_completion_falls_back_to_recents() {
        let mut f = Fixture::paired();
        f.session("/srv/api");
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(complete(&mut f, "/srv/"), None, "nothing is asked offline");
        assert_eq!(completion(&f).status, CompletionStatus::Unavailable);
        assert_eq!(
            candidates(&f),
            vec![("/srv/api".to_owned(), CandidateSource::Recent)]
        );

        // Back online; an old server without the endpoint answers 404.
        f.model.set_link(PairedLink::Online { latency_ms: 5 });
        let (req, _) = complete(&mut f, "/srv/").unwrap();
        f.model.dirs_listed(req, Err(TransportError::NotFound));
        assert_eq!(completion(&f).status, CompletionStatus::Unavailable);
        assert_eq!(candidates(&f).len(), 1);
        // A failed listing is asked for again on the next edit.
        assert!(complete(&mut f, "/srv/a").is_some());
    }

    #[test]
    fn relative_input_and_dirs_outside_the_roots_offer_the_roots() {
        let mut f = Fixture::paired();
        f.session("/srv/code/web");
        let (req, dir) = complete(&mut f, "co").unwrap();
        assert_eq!(dir, "", "the roots");
        f.model.dirs_listed(
            req,
            Ok(DirsResult::Roots {
                roots: vec!["/srv/code".into(), "/opt/tools".into()],
            }),
        );
        assert_eq!(
            candidates(&f),
            vec![
                ("/srv/code/web".to_owned(), CandidateSource::Recent),
                ("/srv/code".to_owned(), CandidateSource::Server),
            ],
            "recent workdirs first, then the roots not among them"
        );

        let (req, dir) = complete(&mut f, "/s").unwrap();
        assert_eq!(dir, "/");
        f.model.dirs_listed(
            req,
            Ok(DirsResult::NotAllowed {
                roots: vec!["/srv/code".into(), "/opt/tools".into()],
            }),
        );
        assert_eq!(completion(&f).fill().as_deref(), Some("/srv/code/"));
    }

    #[test]
    fn empty_input_offers_the_recents_before_the_roots() {
        let mut f = Fixture::paired();
        f.session("/home/me");
        f.session("/srv/code/web");
        let (req, dir) = complete(&mut f, "").unwrap();
        assert_eq!(dir, "", "the roots");
        assert_eq!(
            candidates(&f),
            vec![
                ("/srv/code/web".to_owned(), CandidateSource::Recent),
                ("/home/me".to_owned(), CandidateSource::Recent),
            ],
            "the recents show before the roots arrive, newest first"
        );

        f.model.dirs_listed(
            req,
            Ok(DirsResult::Roots {
                roots: vec!["/srv/code".into(), "/home/me".into()],
            }),
        );
        assert_eq!(
            candidates(&f),
            vec![
                ("/srv/code/web".to_owned(), CandidateSource::Recent),
                ("/home/me".to_owned(), CandidateSource::Recent),
                ("/srv/code".to_owned(), CandidateSource::Server),
            ],
            "a root that is also recent is not listed twice"
        );
    }

    #[test]
    fn completion_needs_the_session_list() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(complete(&mut f, "/srv/"), None);
        f.dispatch(Action::Navigate(Route::SessionList));
        assert!(complete(&mut f, "/srv/").is_some());
        // Creating the session ends the completion.
        let effects = f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/x".into(),
            worktree: None,
        });
        queued(&effects[..2]);
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(f.list_view().completion, None);

        // Leaving the list ends the completion; an answer asked before it
        // left is stale once the prompt completes again.
        let (old, _) = complete(&mut f, "/srv/").unwrap();
        f.dispatch(Action::Navigate(Route::Settings));
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(f.list_view().completion, None);
        let (new, _) = complete(&mut f, "/srv/").unwrap();
        assert_ne!(new, old);
        f.model.dirs_listed(old, listed("/srv", &["api"]));
        assert_eq!(completion(&f).status, CompletionStatus::Loading);
        assert_eq!(candidates(&f), Vec::new());
    }

    use crate::view::{
        ConversationsStatus, ConversationsUnavailable, StorageStatus, StorageUnavailable,
    };
    use skywave_proto::OutsideConversation;

    /// Opens the conversation picker; the request it sends, if any.
    fn list_conversations(f: &mut Fixture) -> Option<u64> {
        match f.dispatch(Action::ListConversations).as_slice() {
            [] => None,
            [Effect::Request(Request::ListConversations { req })] => Some(*req),
            other => panic!("expected a conversations request, got {other:?}"),
        }
    }

    fn picker(f: &Fixture) -> crate::view::ConversationPicker {
        f.list_view().conversations.expect("an open picker")
    }

    fn outside(id: &str, workdir: &str, session: Option<SessionId>) -> OutsideConversation {
        OutsideConversation {
            id: id.into(),
            workdir: workdir.into(),
            title: Some(format!("title of {id}")),
            first_prompt: Some("fix the build".into()),
            last_active: Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap(),
            messages: 4,
            session,
        }
    }

    fn conversations(
        list: Vec<OutsideConversation>,
        truncated: bool,
    ) -> Result<ConversationsResult, TransportError> {
        Ok(ConversationsResult::Listed {
            conversations: list,
            truncated,
        })
    }

    /// Logs a session that adopted `conversation`.
    fn adopting_session(f: &mut Fixture, conversation: &str) -> SessionId {
        let id = SessionId::new();
        f.log(
            id,
            vec![EventKind::SessionCreated {
                workdir: "/srv/a".into(),
                context_name: "start".into(),
                from: Some(SessionOrigin::Agent {
                    conversation: conversation.into(),
                }),
                worktree: None,
            }],
        );
        id
    }

    #[test]
    fn the_picker_asks_on_open_and_shows_the_latest_answer() {
        let mut f = Fixture::paired();
        assert_eq!(f.list_view().conversations, None);
        let old = list_conversations(&mut f).expect("asked on open");
        assert_eq!(picker(&f).status, ConversationsStatus::Loading);
        assert_eq!(picker(&f).items, Vec::new());

        // Opening it again asks again; the older answer is then stale.
        let new = list_conversations(&mut f).unwrap();
        assert!(new > old);
        f.model.conversations_listed(
            old,
            conversations(vec![outside("old", "/srv/a", None)], false),
        );
        assert_eq!(picker(&f).status, ConversationsStatus::Loading, "stale");
        assert_eq!(picker(&f).items, Vec::new());

        f.model.conversations_listed(
            new,
            conversations(vec![outside("c1", "/srv/b", None)], true),
        );
        let shown = picker(&f);
        assert_eq!(shown.status, ConversationsStatus::Ready { truncated: true });
        assert_eq!(
            shown.items,
            vec![crate::view::ConversationItem {
                id: "c1".into(),
                workdir: "/srv/b".into(),
                title: Some("title of c1".into()),
                first_prompt: Some("fix the build".into()),
                last_active: Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap(),
                messages: 4,
                session: None,
                continued_in: Vec::new(),
            }]
        );

        // A stale answer after a good one changes nothing either.
        f.model
            .conversations_listed(old, Err(TransportError::Timeout));
        assert_eq!(picker(&f), shown);

        // Closing forgets the listing, and a late answer does not reopen it.
        assert!(f.dispatch(Action::EndConversations).is_empty());
        assert_eq!(f.list_view().conversations, None);
        f.model.conversations_listed(
            new,
            conversations(vec![outside("c1", "/srv/b", None)], false),
        );
        assert_eq!(f.list_view().conversations, None);
    }

    #[test]
    fn the_picker_offline_is_unavailable_at_once_and_asks_nothing() {
        let mut f = Fixture::paired();
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        let offline = ConversationsStatus::Unavailable(ConversationsUnavailable::Offline);
        assert_eq!(list_conversations(&mut f), None, "nothing is asked offline");
        assert_eq!(picker(&f).status, offline);
        // Nothing was queued for later: coming back online asks nothing.
        f.model.set_link(PairedLink::Online { latency_ms: 5 });
        assert_eq!(picker(&f).status, offline);
        // Asking again online does.
        let req = list_conversations(&mut f).expect("asked online");
        f.model
            .conversations_listed(req, Ok(ConversationsResult::Unavailable));
        assert_eq!(
            picker(&f).status,
            ConversationsStatus::Unavailable(ConversationsUnavailable::AgentCannotList)
        );
        // The engine answers so while the link is down.
        let req = list_conversations(&mut f).unwrap();
        f.model
            .conversations_listed(req, Err(crate::workdir::not_connected()));
        assert_eq!(picker(&f).status, offline);
        let req = list_conversations(&mut f).unwrap();
        f.model
            .conversations_listed(req, Err(TransportError::BadStatus(503)));
        assert_eq!(
            picker(&f).status,
            ConversationsStatus::Unavailable(ConversationsUnavailable::Failed(
                TransportError::BadStatus(503).to_string()
            ))
        );
    }

    fn storage_view(f: &Fixture) -> StorageView {
        match f.model.view().screen {
            Screen::Storage(view) => view,
            other => panic!("expected the storage screen, got {other:?}"),
        }
    }

    fn approvals_view(f: &Fixture) -> ApprovalsView {
        match f.model.view().screen {
            Screen::Approvals(view) => view,
            other => panic!("expected the approvals screen, got {other:?}"),
        }
    }

    /// The server's storage request among `effects`, if one is asked.
    fn storage_req(effects: &[Effect]) -> Option<u64> {
        effects.iter().find_map(|effect| match effect {
            Effect::Request(Request::Storage { req }) => Some(*req),
            _ => None,
        })
    }

    /// This device's storage request among `effects`, if one is asked.
    fn device_req(effects: &[Effect]) -> Option<u64> {
        effects.iter().find_map(|effect| match effect {
            Effect::Request(Request::DeviceStorage { req }) => Some(*req),
            _ => None,
        })
    }

    fn device_storage(database: u64) -> DeviceStorage {
        DeviceStorage {
            database,
            reclaimable: 0,
            blobs: 0,
            attachments: 0,
            sessions: Vec::new(),
        }
    }

    fn storage_report(sessions: &[SessionId]) -> StorageReport {
        StorageReport {
            database: 4096,
            reclaimable: 0,
            event_blobs: 0,
            workdir_blobs: 0,
            shell_running: 0,
            log_file: None,
            free: Some(1 << 30),
            min_free: 0,
            sessions: sessions
                .iter()
                .map(|&session| skywave_proto::SessionStorage {
                    session,
                    events: 1,
                    event_bytes: 100,
                    blobs: 0,
                    blob_bytes: 0,
                })
                .collect(),
            blobs: Vec::new(),
            shell_runs: Vec::new(),
        }
    }

    #[test]
    fn the_storage_screen_asks_on_opening_and_on_refresh() {
        let mut f = Fixture::paired();
        let known = f.session("/srv/a");
        let unknown = SessionId::new();
        assert_eq!(storage_req(&f.dispatch(Action::RefreshStorage)), None);

        let first = storage_req(&f.dispatch(Action::Navigate(Route::Storage))).unwrap();
        assert_eq!(storage_view(&f).status, StorageStatus::Loading);
        assert_eq!(storage_view(&f).report, None);
        let report = storage_report(&[known, unknown]);
        f.model.storage_reported(first, Ok(report.clone()));
        let view = storage_view(&f);
        assert_eq!(view.status, StorageStatus::Ready);
        assert_eq!(view.report, Some(report.clone()));
        // A session not in this replica has no entry.
        assert_eq!(view.sessions.len(), 1);
        assert_eq!(view.sessions[&known].title, "a");

        // A refresh keeps the report until its answer, and a failure keeps it.
        let second = storage_req(&f.dispatch(Action::RefreshStorage)).unwrap();
        assert_ne!(second, first);
        assert_eq!(storage_view(&f).status, StorageStatus::Loading);
        f.model
            .storage_reported(second, Err(TransportError::BadStatus(503)));
        let view = storage_view(&f);
        assert_eq!(
            view.status,
            StorageStatus::Unavailable(StorageUnavailable::Failed(
                TransportError::BadStatus(503).to_string()
            ))
        );
        assert_eq!(view.report, Some(report));

        // Offline nothing is asked.
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(storage_req(&f.dispatch(Action::RefreshStorage)), None);
        assert_eq!(
            storage_view(&f).status,
            StorageStatus::Unavailable(StorageUnavailable::Offline)
        );

        // Elsewhere a refresh asks nothing.
        f.model.set_link(PairedLink::Online { latency_ms: 5 });
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(storage_req(&f.dispatch(Action::RefreshStorage)), None);
    }

    #[test]
    fn the_storage_screen_asks_for_this_devices_report_too() {
        let mut f = Fixture::paired();
        let effects = f.dispatch(Action::Navigate(Route::Storage));
        assert_eq!(effects.len(), 2, "{effects:?}");
        assert!(storage_req(&effects).is_some());
        let device = device_req(&effects).unwrap();
        assert_eq!(storage_view(&f).device, None);
        f.model
            .device_storage_read(device, Ok(device_storage(4096)));
        assert_eq!(storage_view(&f).device, Some(device_storage(4096)));

        // A refresh asks for it again and keeps the one shown until the answer.
        let again = device_req(&f.dispatch(Action::RefreshStorage)).unwrap();
        assert_ne!(again, device);
        assert_eq!(storage_view(&f).device, Some(device_storage(4096)));
        // The answer to the request before is ignored.
        f.model.device_storage_read(device, Ok(device_storage(1)));
        assert_eq!(storage_view(&f).device, Some(device_storage(4096)));
        f.model.device_storage_read(again, Ok(device_storage(8192)));
        assert_eq!(storage_view(&f).device, Some(device_storage(8192)));

        // Offline only this device's report is asked.
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        let offline = f.dispatch(Action::RefreshStorage);
        assert_eq!(storage_req(&offline), None);
        assert!(device_req(&offline).is_some(), "{offline:?}");
        assert_eq!(offline.len(), 1, "{offline:?}");
    }

    fn reclaim_req(effects: &[Effect]) -> Option<u64> {
        match effects {
            [] => None,
            [Effect::Request(Request::ReclaimStorage { req })] => Some(*req),
            other => panic!("expected a reclaim request, got {other:?}"),
        }
    }

    #[test]
    fn reclaiming_asks_on_the_storage_screen_only() {
        let mut f = Fixture::paired();
        assert_eq!(reclaim_req(&f.dispatch(Action::ReclaimServerSpace)), None);

        let first = storage_req(&f.dispatch(Action::Navigate(Route::Storage))).unwrap();
        f.model.storage_reported(first, Ok(storage_report(&[])));
        let reclaim = reclaim_req(&f.dispatch(Action::ReclaimServerSpace)).unwrap();
        assert_eq!(storage_view(&f).status, StorageStatus::Reclaiming);
        assert_eq!(storage_view(&f).report, Some(storage_report(&[])));
        f.model.storage_reclaimed(reclaim, Ok(storage_report(&[])));
        assert_eq!(storage_view(&f).status, StorageStatus::Ready);

        // Elsewhere a reclaim asks nothing.
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(reclaim_req(&f.dispatch(Action::ReclaimServerSpace)), None);
    }

    fn device_reclaim_req(effects: &[Effect]) -> Option<u64> {
        match effects {
            [] => None,
            [Effect::Request(Request::ReclaimDeviceStorage { req })] => Some(*req),
            other => panic!("expected a device reclaim request, got {other:?}"),
        }
    }

    #[test]
    fn reclaiming_this_device_asks_on_the_storage_screen_only() {
        let mut f = Fixture::paired();
        assert_eq!(
            device_reclaim_req(&f.dispatch(Action::ReclaimDeviceSpace)),
            None
        );

        let effects = f.dispatch(Action::Navigate(Route::Storage));
        let device = device_req(&effects).unwrap();
        f.model
            .device_storage_read(device, Ok(device_storage(4096)));
        let reclaim = device_reclaim_req(&f.dispatch(Action::ReclaimDeviceSpace)).unwrap();
        assert!(storage_view(&f).device_reclaiming);
        // Nothing else is asked while it runs.
        assert_eq!(
            device_reclaim_req(&f.dispatch(Action::ReclaimDeviceSpace)),
            None
        );
        assert_eq!(device_req(&f.dispatch(Action::RefreshStorage)), None);
        f.model
            .device_storage_read(reclaim, Ok(device_storage(2048)));
        assert_eq!(storage_view(&f).device, Some(device_storage(2048)));
        assert!(!storage_view(&f).device_reclaiming);

        // Elsewhere a reclaim asks nothing.
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(
            device_reclaim_req(&f.dispatch(Action::ReclaimDeviceSpace)),
            None
        );
    }

    fn worktrees_view(f: &Fixture) -> WorktreesView {
        match f.model.view().screen {
            Screen::Worktrees(view) => view,
            other => panic!("expected the worktrees screen, got {other:?}"),
        }
    }

    fn worktrees_req(effects: &[Effect]) -> Option<u64> {
        match effects {
            [] => None,
            [Effect::Request(Request::Worktrees { req })] => Some(*req),
            other => panic!("expected a worktrees request, got {other:?}"),
        }
    }

    /// One repository, `/srv/app`, whose worktrees are at `paths`.
    fn worktree_list(paths: &[&str]) -> skywave_proto::WorktreeList {
        skywave_proto::WorktreeList {
            at: Utc::now(),
            repos: vec![skywave_proto::RepoWorktrees {
                repo: "/srv/app".into(),
                listing: skywave_proto::RepoListing::Worktrees {
                    worktrees: paths
                        .iter()
                        .map(|path| skywave_proto::WorktreeEntry {
                            path: (*path).into(),
                            branch: Some("feat/x".into()),
                            head: None,
                            main: false,
                            missing: false,
                            dirty: Some(false),
                            merged: true,
                        })
                        .collect(),
                    more: false,
                },
            }],
        }
    }

    /// A session created in a skywave worktree of `/srv/app`, in `workdir`.
    fn created_in_worktree(workdir: &str) -> EventKind {
        EventKind::SessionCreated {
            workdir: workdir.into(),
            context_name: "start".into(),
            from: None,
            worktree: Some(skywave_proto::Worktree {
                repo: "/srv/app".into(),
                branch: "feat/x".into(),
            }),
        }
    }

    #[test]
    fn the_worktrees_screen_asks_on_opening_and_on_refresh() {
        let mut f = Fixture::paired();
        assert_eq!(worktrees_req(&f.dispatch(Action::RefreshWorktrees)), None);

        let first = worktrees_req(&f.dispatch(Action::Navigate(Route::Worktrees))).unwrap();
        let view = worktrees_view(&f);
        assert_eq!(view.status, WorktreesStatus::Loading);
        assert_eq!(view.list, None);
        let list = worktree_list(&["/srv/app-a"]);
        f.model.worktrees_listed(first, Ok(list.clone()));
        let view = worktrees_view(&f);
        assert_eq!(view.status, WorktreesStatus::Ready);
        assert_eq!(view.list, Some(list.clone()));

        // A refresh keeps the list until its answer, and a failure keeps it.
        let second = worktrees_req(&f.dispatch(Action::RefreshWorktrees)).unwrap();
        assert_ne!(second, first);
        assert_eq!(worktrees_view(&f).status, WorktreesStatus::Loading);
        assert_eq!(worktrees_view(&f).list, Some(list.clone()));
        f.model
            .worktrees_listed(second, Err(TransportError::BadStatus(503)));
        let view = worktrees_view(&f);
        assert_eq!(
            view.status,
            WorktreesStatus::Unavailable(WorktreesUnavailable::Failed(
                TransportError::BadStatus(503).to_string()
            ))
        );
        assert_eq!(view.list, Some(list));

        // Offline nothing is asked.
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(worktrees_req(&f.dispatch(Action::RefreshWorktrees)), None);
        assert_eq!(
            worktrees_view(&f).status,
            WorktreesStatus::Unavailable(WorktreesUnavailable::Offline)
        );

        // Elsewhere a refresh asks nothing.
        f.model.set_link(PairedLink::Online { latency_ms: 5 });
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(worktrees_req(&f.dispatch(Action::RefreshWorktrees)), None);
    }

    #[test]
    fn the_worktrees_screen_names_the_sessions_in_each_worktree() {
        let mut f = Fixture::paired();
        let (p, q) = ("/srv/app-a", "/srv/app-b");
        let a = SessionId::new();
        f.log(a, vec![created_in_worktree(p)]);
        let b = SessionId::new();
        f.log(b, vec![created_in_worktree(p)]);
        queued(&f.dispatch(archive(b, true)));
        let c = f.session("/srv/other");

        let first = worktrees_req(&f.dispatch(Action::Navigate(Route::Worktrees))).unwrap();
        f.model.worktrees_listed(first, Ok(worktree_list(&[p, q])));
        let view = worktrees_view(&f);
        // The replica's order: most recently active first, so B (logged
        // after A) leads.
        let running = &view.sessions[p];
        let ids: Vec<SessionId> = running.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![b, a]);
        assert!(running[0].archived);
        assert!(!running[1].archived && running[1].worktree && !running[1].worktree_removed);
        assert!(!view.sessions.contains_key(q));
        assert!(view.sessions.values().flatten().all(|s| s.id != c));

        // Removing it shows as queued on its row, and nowhere else.
        let remove = queued(&f.dispatch(Action::RemoveWorktree { session_id: a }));
        assert_eq!(remove.kind, CommandKind::RemoveWorktree { session_id: a });
        let view = worktrees_view(&f);
        assert_eq!(view.sessions[p][0].removal, None);
        assert_eq!(view.sessions[p][1].removal, Some(Outgoing::Queued));

        // Its title is the one the session list shows.
        let title = view.sessions[p][1].title.clone();
        f.dispatch(Action::Back);
        let row = f
            .list_view()
            .sessions
            .into_iter()
            .find(|r| r.id == a)
            .unwrap();
        assert_eq!(title, row.title);
    }

    #[test]
    fn an_accepted_remove_worktree_refreshes_an_open_worktrees_screen() {
        let mut f = Fixture::paired();
        let a = SessionId::new();
        f.log(a, vec![created_in_worktree("/srv/app-a")]);
        let first = worktrees_req(&f.dispatch(Action::Navigate(Route::Worktrees))).unwrap();

        // A refusal changed nothing on the server.
        let refused = queued(&f.dispatch(Action::RemoveWorktree { session_id: a }));
        let rejected = CommandResult::Rejected {
            reason: RejectReason::ShellRunning,
        };
        assert!(
            f.model
                .command_result(refused.id, rejected, Utc::now())
                .is_empty()
        );

        let removed = queued(&f.dispatch(Action::RemoveWorktree { session_id: a }));
        let Some(Request::Worktrees { req }) = accept(&mut f, removed.id, 5) else {
            panic!("expected a worktrees request");
        };
        assert_ne!(req, first);
        assert_eq!(worktrees_view(&f).status, WorktreesStatus::Loading);
        // Only the first answer counts.
        assert_eq!(accept(&mut f, removed.id, 5), None);

        // Closed, nothing is asked.
        f.dispatch(Action::Navigate(Route::Settings));
        let closed = queued(&f.dispatch(Action::RemoveWorktree { session_id: a }));
        assert_eq!(accept(&mut f, closed.id, 6), None);
    }

    fn usage_limits_view(f: &Fixture) -> crate::view::UsageLimitsView {
        match f.model.view().screen {
            Screen::UsageLimits(view) => view,
            other => panic!("expected the usage limits page, got {other:?}"),
        }
    }

    fn usage_limits_req(effects: &[Effect]) -> Option<u64> {
        match effects {
            [] => None,
            [Effect::Request(Request::UsageLimits { req })] => Some(*req),
            other => panic!("expected a usage limits request, got {other:?}"),
        }
    }

    fn limits_read(percent: u32) -> UsageLimitsResult {
        UsageLimitsResult::Read {
            at: Utc::now(),
            limits: vec![skywave_proto::UsageLimit {
                window: skywave_proto::LimitWindow::Weekly,
                scope: None,
                percent: Some(percent),
                severity: None,
                resets_at: None,
            }],
        }
    }

    #[test]
    fn the_usage_limits_page_asks_while_nothing_is_stored_and_on_refresh() {
        use crate::view::{UsageLimitsStatus, UsageLimitsUnavailable};

        let mut f = Fixture::paired();
        assert_eq!(
            usage_limits_req(&f.dispatch(Action::RefreshUsageLimits)),
            None,
            "not on the page"
        );
        let first = usage_limits_req(&f.dispatch(Action::Navigate(Route::UsageLimits))).unwrap();
        assert_eq!(usage_limits_view(&f).status, UsageLimitsStatus::Loading);
        assert!(f.model.usage_limits_awaited(first));
        f.model.usage_limits_stored(limits_read(40));
        let view = usage_limits_view(&f);
        assert_eq!(view.status, UsageLimitsStatus::Idle);
        let stored = view.answer.unwrap();

        // Stored: reopening shows it and asks nothing.
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(
            usage_limits_req(&f.dispatch(Action::Navigate(Route::UsageLimits))),
            None
        );
        assert_eq!(usage_limits_view(&f).answer, Some(stored.clone()));

        // A refresh asks; a failure keeps the stored answer.
        let second = usage_limits_req(&f.dispatch(Action::RefreshUsageLimits)).unwrap();
        assert_ne!(second, first);
        assert!(!f.model.usage_limits_awaited(first));
        f.model
            .usage_limits_failed(UsageLimitsUnavailable::Failed("not logged in".into()));
        let view = usage_limits_view(&f);
        assert_eq!(
            view.status,
            UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Failed("not logged in".into()))
        );
        assert_eq!(view.answer, Some(stored.clone()));

        // Offline nothing is asked, and the stored answer still shows.
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(
            usage_limits_req(&f.dispatch(Action::RefreshUsageLimits)),
            None
        );
        let view = usage_limits_view(&f);
        assert_eq!(
            view.status,
            UsageLimitsStatus::Unavailable(UsageLimitsUnavailable::Offline)
        );
        assert_eq!(view.answer, Some(stored));
    }

    #[test]
    fn back_from_the_usage_limits_page_returns_where_it_was_opened() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.model.usage_limits.stored = Some(limits_read(10));
        f.open(sid);
        f.dispatch(Action::Navigate(Route::UsageLimits));
        assert!(f.model.view().can_go_back);
        f.dispatch(Action::Back);
        assert_eq!(f.model.open_session(), Some(sid));

        f.dispatch(Action::Navigate(Route::SessionList));
        f.dispatch(Action::Navigate(Route::UsageLimits));
        f.dispatch(Action::Back);
        assert!(matches!(f.model.place, Place::SessionList(_)));
    }

    const LOGIN_URL: &str = "https://login.example/authorize?state=abc";

    fn login_view(f: &Fixture) -> crate::view::LoginView {
        match f.model.view().screen {
            Screen::Settings(view) => view.login,
            other => panic!("expected settings, got {other:?}"),
        }
    }

    fn login_start_req(effects: &[Effect]) -> Option<u64> {
        match effects {
            [] => None,
            [Effect::Request(Request::LoginStart { req })] => Some(*req),
            other => panic!("expected a login start, got {other:?}"),
        }
    }

    fn login_finish_req(effects: &[Effect]) -> Option<(u64, skywave_proto::LoginFinish)> {
        match effects {
            [] => None,
            [Effect::Request(Request::LoginFinish { req, finish })] => Some((*req, finish.clone())),
            other => panic!("expected a login finish, got {other:?}"),
        }
    }

    fn code(code: &str) -> Action {
        Action::FinishAgentLogin {
            code: crate::LoginCode::new(code),
        }
    }

    /// A paired fixture on the settings screen with a login open on `flow`.
    fn login_opened(flow: skywave_proto::LoginFlowId) -> Fixture {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::Settings));
        let req = login_start_req(&f.dispatch(Action::StartAgentLogin)).unwrap();
        f.model.login_started(
            req,
            Ok(LoginStartResult::Open {
                flow,
                url: LOGIN_URL.into(),
            }),
        );
        f
    }

    #[test]
    fn a_login_code_is_sent_only_for_an_open_flow_on_the_settings_screen() {
        use crate::view::LoginFlowView;

        let mut f = Fixture::paired();
        assert_eq!(
            login_start_req(&f.dispatch(Action::StartAgentLogin)),
            None,
            "not on the settings screen"
        );
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(login_finish_req(&f.dispatch(code("a#b"))), None, "idle");
        assert_eq!(login_finish_req(&f.dispatch(Action::RetryAgentLogin)), None);
        let first = login_start_req(&f.dispatch(Action::StartAgentLogin)).unwrap();
        assert_eq!(login_view(&f).flow, LoginFlowView::Starting);
        assert_eq!(
            login_finish_req(&f.dispatch(code("a#b"))),
            None,
            "still starting"
        );

        // Only the latest start's answer is taken.
        let second = login_start_req(&f.dispatch(Action::StartAgentLogin)).unwrap();
        let flow = skywave_proto::LoginFlowId::random();
        let open = |flow| {
            Ok(LoginStartResult::Open {
                flow,
                url: LOGIN_URL.into(),
            })
        };
        f.model
            .login_started(first, open(skywave_proto::LoginFlowId::random()));
        assert_eq!(login_view(&f).flow, LoginFlowView::Starting);
        f.model.login_started(second, open(flow));
        assert_eq!(
            login_view(&f).flow,
            LoginFlowView::Open {
                url: LOGIN_URL.into(),
                refused: None,
            }
        );

        // Off the settings screen nothing is sent; back on it, the code goes.
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(login_finish_req(&f.dispatch(code("a#b"))), None);
        f.dispatch(Action::Navigate(Route::Settings));
        let (_, finish) = login_finish_req(&f.dispatch(code("a#b"))).unwrap();
        assert_eq!(finish.flow, flow);
        assert_eq!(finish.code, "a#b");

        // Dismissed: forgotten, and nothing is sent for it.
        assert_eq!(f.dispatch(Action::DismissAgentLogin), vec![]);
        assert_eq!(login_view(&f).flow, LoginFlowView::Idle);
        assert_eq!(login_finish_req(&f.dispatch(code("a#b"))), None);
    }

    #[test]
    fn a_logged_in_answer_shows_the_agent_logged_in_without_a_hello() {
        use crate::view::{LoginFlowView, LoginOutcome};

        let flow = skywave_proto::LoginFlowId::random();
        let mut f = login_opened(flow);
        f.model.server_facts(ServerFacts {
            agent_login_offered: true,
            agent_logged_out: true,
            ..ServerFacts::default()
        });
        let view = login_view(&f);
        assert!(view.offered && view.logged_out);
        assert_eq!(view.expires, None);

        let (stale, _) = login_finish_req(&f.dispatch(code("a#b"))).unwrap();
        f.model
            .login_finished(stale, Err(TransportError::Timeout), true);
        let (req, _) = login_finish_req(&f.dispatch(Action::RetryAgentLogin)).unwrap();
        let expires: DateTime<Utc> = "2027-01-01T00:00:00Z".parse().unwrap();
        let logged_in = || {
            Ok(LoginFinishResult::LoggedIn {
                expires: Some(expires),
            })
        };
        // The superseded finish's answer changes nothing.
        f.model.login_finished(stale, logged_in(), true);
        assert!(login_view(&f).logged_out);
        f.model.login_finished(req, logged_in(), true);
        let view = login_view(&f);
        assert_eq!(view.flow, LoginFlowView::Done(LoginOutcome::LoggedIn));
        assert!(!view.logged_out);
        assert_eq!(view.expires, Some(expires));
        let state = f.model.view();
        assert!(!state.agent_logged_out);
        assert_eq!(state.login_expires, Some(expires));
    }

    #[test]
    fn pairing_with_another_server_forgets_the_login() {
        use crate::view::LoginFlowView;

        let mut f = login_opened(skywave_proto::LoginFlowId::random());
        let (req, _) = login_finish_req(&f.dispatch(code("a#b"))).unwrap();
        f.model.replica_reset(empty_snapshot());
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(login_view(&f).flow, LoginFlowView::Idle);
        assert!(
            !f.model.login_awaited(req),
            "the old server's answer is dropped"
        );
        f.model
            .login_finished(req, Ok(LoginFinishResult::LoggedIn { expires: None }), true);
        assert_eq!(login_view(&f).flow, LoginFlowView::Idle);
        let next = login_start_req(&f.dispatch(Action::StartAgentLogin)).unwrap();
        assert!(next > req, "request ids are never reused");
    }

    fn crop(blob: BlobId) -> Action {
        Action::CropBlob {
            blob,
            keep: vec![skywave_proto::LineSpan { first: 0, count: 1 }],
        }
    }

    /// The first request the answer asks: the screen's own, before this
    /// device's storage report that a storage refresh asks with it.
    fn accept(f: &mut Fixture, id: CommandId, seq: u64) -> Option<Request> {
        f.model
            .command_result(id, CommandResult::Accepted { seq: Seq(seq) }, Utc::now())
            .into_iter()
            .next()
    }

    #[test]
    fn a_blob_delete_or_crop_queues_offline_and_is_discardable_until_accepted() {
        let mut f = Fixture::paired();
        f.model.set_link(PairedLink::Reconnecting {
            attempt: 1,
            retry_at: Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        let blob = BlobId::new();
        let delete = queued(&f.dispatch(Action::DeleteBlob(blob)));
        assert_eq!(delete.kind, CommandKind::DeleteBlob { blob });
        let cropped = queued(&f.dispatch(crop(blob)));
        assert!(matches!(cropped.kind, CommandKind::CropBlob { blob: b, .. } if b == blob));
        assert!(f.model.outbox.iter().all(OutboxEntry::discardable));

        // Accepted, it can no longer be taken back.
        accept(&mut f, cropped.id, 9);
        assert!(f.dispatch(Action::Discard(cropped.id)).is_empty());
        assert_eq!(
            f.dispatch(Action::Discard(delete.id)),
            vec![
                Effect::Withdraw {
                    id: delete.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        let left: Vec<CommandId> = f.model.outbox.iter().map(|e| e.command.id).collect();
        assert_eq!(left, vec![cropped.id]);
    }

    /// Two tool outputs logged in an open session, and the first's blob.
    fn two_outputs(f: &mut Fixture) -> (SessionId, BlobId) {
        let sid = f.session("/srv/a");
        let output = |preview: &str| {
            let blob = BlobRef {
                id: BlobId::new(),
                size: 10,
                sha256: skywave_proto::ContentHash::of(b""),
            };
            let id = blob.id;
            let kind = EventKind::ToolResult(ToolResult {
                call: None,
                is_error: false,
                status: None,
                preview: preview.into(),
                lines: 1,
                output: Some(Full::Blob(blob)),
            });
            (id, kind)
        };
        let ((a, first), (_, second)) = (output("a"), output("b"));
        f.log(sid, vec![first, second]);
        f.open(sid);
        (sid, a)
    }

    /// The `pending` of each tool output in the open session.
    fn output_pending(f: &Fixture) -> Vec<Option<BlobPending>> {
        f.session_view()
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged {
                    entry:
                        Entry::ToolResult {
                            output: Some(FullView::Blob(blob)),
                            ..
                        },
                    ..
                } => Some(blob.pending),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_refused_blob_delete_shows_as_failed_on_that_blob() {
        let mut f = Fixture::paired();
        let (_, output) = two_outputs(&mut f);
        assert_eq!(output_pending(&f), vec![None, None]);

        let delete = queued(&f.dispatch(Action::DeleteBlob(output)));
        let mut pending = BlobPending {
            command: delete.id,
            kind: BlobCommand::Delete,
            outgoing: Outgoing::Queued,
            discardable: true,
        };
        assert_eq!(output_pending(&f), vec![Some(pending), None]);
        let rejected = CommandResult::Rejected {
            reason: RejectReason::BlobGone,
        };
        f.model.command_result(delete.id, rejected, Utc::now());
        pending.outgoing = Outgoing::Failed(FailReason::Rejected(RejectReason::BlobGone));
        assert_eq!(output_pending(&f), vec![Some(pending), None]);

        // Accepted, it shows until pulled and can't be taken back.
        let other = queued(&f.dispatch(Action::DeleteBlob(BlobId::new())));
        accept(&mut f, other.id, 99);
        let accepted = f.model.blob_pendings();
        assert!(
            accepted
                .values()
                .any(|p| p.command == other.id && !p.discardable)
        );
    }

    #[test]
    fn a_refused_crop_is_discarded_from_its_view_and_the_blob_is_free_again() {
        let mut f = Fixture::paired();
        let (sid, output) = two_outputs(&mut f);
        let cropped = queued(&f.dispatch(crop(output)));
        let rejected = CommandResult::Rejected {
            reason: RejectReason::BadCrop,
        };
        f.model.command_result(cropped.id, rejected, Utc::now());
        let [Some(pending), None] = output_pending(&f)[..] else {
            panic!("expected the first output's crop pending");
        };
        assert_eq!(
            pending,
            BlobPending {
                command: cropped.id,
                kind: BlobCommand::Crop,
                outgoing: Outgoing::Failed(FailReason::Rejected(RejectReason::BadCrop)),
                discardable: true,
            }
        );
        // The storage screen sees the same.
        f.dispatch(Action::Navigate(Route::Storage));
        assert_eq!(storage_view(&f).pending, HashMap::from([(output, pending)]));

        // The view's id is all a head needs to take it back.
        assert_eq!(
            f.dispatch(Action::Discard(pending.command)),
            vec![
                Effect::Withdraw {
                    id: cropped.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        assert_eq!(storage_view(&f).pending, HashMap::new());
        f.open(sid);
        assert_eq!(output_pending(&f), vec![None, None]);
    }

    #[test]
    fn an_accepted_blob_command_refreshes_an_open_storage_screen() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let first = storage_req(&f.dispatch(Action::Navigate(Route::Storage))).unwrap();

        let delete = queued(&f.dispatch(Action::DeleteBlob(BlobId::new())));
        let Some(Request::Storage { req }) = accept(&mut f, delete.id, 5) else {
            panic!("expected a storage request");
        };
        assert_ne!(req, first);
        assert_eq!(storage_view(&f).status, StorageStatus::Loading);
        // Only the first answer counts.
        assert_eq!(accept(&mut f, delete.id, 5), None);

        // A refusal changed nothing on the server; nor did a rename.
        let refused = queued(&f.dispatch(crop(BlobId::new())));
        let rejected = CommandResult::Rejected {
            reason: RejectReason::BlobGone,
        };
        assert!(
            f.model
                .command_result(refused.id, rejected, Utc::now())
                .is_empty()
        );
        let rename = queued(&f.dispatch(Action::RenameSession {
            session_id: sid,
            name: "x".into(),
        }));
        assert_eq!(accept(&mut f, rename.id, 6), None);

        let cropped = queued(&f.dispatch(crop(BlobId::new())));
        assert!(matches!(
            accept(&mut f, cropped.id, 7),
            Some(Request::Storage { .. })
        ));

        // Closed, nothing is asked.
        f.dispatch(Action::Navigate(Route::Settings));
        let closed = queued(&f.dispatch(Action::DeleteBlob(BlobId::new())));
        assert_eq!(accept(&mut f, closed.id, 8), None);
    }

    #[test]
    fn the_storage_screen_marks_gone_each_listed_blob_the_log_deletes_or_crops() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (theirs, mine, run, kept, unlisted) = (
            BlobId::new(),
            BlobId::new(),
            BlobId::new(),
            BlobId::new(),
            BlobId::new(),
        );
        let first = storage_req(&f.dispatch(Action::Navigate(Route::Storage))).unwrap();
        let mut report = storage_report(&[sid]);
        report.blobs = [theirs, mine, kept]
            .map(|id| skywave_proto::BlobStorage {
                id,
                size: 10,
                session: sid,
                seq: Seq(1),
                holds: BlobUse::ToolOutput,
            })
            .into();
        report.shell_runs = vec![skywave_proto::ShellRunStorage {
            run: CommandId::new(),
            session: sid,
            command_line: "make".into(),
            event_bytes: 10,
            output: 10,
            output_blob: Some(run),
        }];
        f.model.storage_reported(first, Ok(report));
        assert_eq!(storage_view(&f).gone, HashSet::new());

        // Another device deletes one blob and crops the run's output, and
        // a blob the report doesn't list goes too; no request of this
        // device is involved, and nothing asks the server again.
        let other = DeviceId::new();
        f.log(
            sid,
            vec![
                EventKind::BlobDeleted {
                    blob: theirs,
                    holds: BlobUse::ToolOutput,
                    by: other,
                },
                EventKind::BlobCropped {
                    blob: run,
                    holds: BlobUse::ShellOutput,
                    into: BlobRef {
                        id: BlobId::new(),
                        size: 5,
                        sha256: skywave_proto::ContentHash::of(b""),
                    },
                    kept: vec![skywave_proto::LineSpan { first: 0, count: 1 }],
                    lines: 2,
                    by: other,
                },
                EventKind::BlobDeleted {
                    blob: unlisted,
                    holds: BlobUse::ToolOutput,
                    by: other,
                },
            ],
        );
        let view = storage_view(&f);
        assert_eq!(view.status, StorageStatus::Ready);
        assert_eq!(view.gone, HashSet::from([theirs, run]));

        // This device's own delete, accepted: the refresh it asks for is
        // still out when the log brings the delete, which ends its
        // pending; the row stays gone until the new report.
        let delete = queued(&f.dispatch(Action::DeleteBlob(mine)));
        let at = f.events.len() as u64 + 1;
        assert!(matches!(
            accept(&mut f, delete.id, at),
            Some(Request::Storage { .. })
        ));
        assert!(storage_view(&f).pending.contains_key(&mine));
        f.log_commands(
            sid,
            vec![(
                Some(delete.id),
                EventKind::BlobDeleted {
                    blob: mine,
                    holds: BlobUse::ToolOutput,
                    by: f.me,
                },
            )],
        );
        let view = storage_view(&f);
        assert_eq!(view.status, StorageStatus::Loading);
        assert!(!view.pending.contains_key(&mine));
        assert_eq!(view.gone, HashSet::from([theirs, run, mine]));
        assert!(!view.gone.contains(&kept));
    }

    #[test]
    fn the_picker_needs_a_paired_device_on_the_session_list() {
        let mut model = Model::new(empty_snapshot(), None);
        assert!(model.dispatch(Action::ListConversations).is_empty());

        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(list_conversations(&mut f), None);
        f.dispatch(Action::Navigate(Route::SessionList));
        let old = list_conversations(&mut f).unwrap();
        // Leaving the list ends the picker; an answer asked before it left
        // is stale once it opens again.
        f.dispatch(Action::Navigate(Route::Settings));
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(f.list_view().conversations, None);
        let new = list_conversations(&mut f).unwrap();
        assert_ne!(new, old);
        f.model.conversations_listed(
            old,
            conversations(vec![outside("c1", "/srv/b", None)], false),
        );
        assert_eq!(picker(&f).status, ConversationsStatus::Loading);
        assert_eq!(picker(&f).items, Vec::new());
    }

    #[test]
    fn picker_items_resolve_against_the_replica() {
        let mut f = Fixture::paired();
        let first = adopting_session(&mut f, "c1");
        let other = adopting_session(&mut f, "c2");
        let again = adopting_session(&mut f, "c1");
        let running = f.session("/srv/run");
        let deleted = f.session("/srv/gone");
        f.model.session_deleted(deleted, &[]);
        let unpulled = SessionId::new();

        let req = list_conversations(&mut f).unwrap();
        f.model.conversations_listed(
            req,
            conversations(
                vec![
                    outside("c1", "/srv/a", Some(running)),
                    outside("c2", "/srv/a", Some(deleted)),
                    outside("c3", "/srv/a", Some(unpulled)),
                ],
                false,
            ),
        );
        let items = picker(&f).items;
        let links: Vec<_> = items
            .iter()
            .map(|i| (i.id.as_str(), i.session, i.continued_in.clone()))
            .collect();
        assert_eq!(
            links,
            vec![
                // Both adopters, most recently active first.
                ("c1", Some(running), vec![again, first]),
                // A session deleted here shows no link.
                ("c2", None, vec![other]),
                // Nor does one not pulled yet.
                ("c3", None, Vec::new()),
            ]
        );
    }

    #[test]
    fn continuing_a_pick_ends_the_picker_and_sends_it_verbatim() {
        let mut f = Fixture::paired();
        let req = list_conversations(&mut f).unwrap();
        let workdir = "/srv/my project";
        let id = "0d6b2c1e-1b1a-4c1e-9f0e-6a2b8c3d4e5f";
        f.model
            .conversations_listed(req, conversations(vec![outside(id, workdir, None)], false));
        let pick = picker(&f).items.remove(0);

        let effects = f.dispatch(Action::CreateSession {
            workdir: pick.workdir,
            conversation: Some(pick.id),
            worktree: None,
        });
        let command = queued(&effects[..2]);
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                session_id: created_id(&command),
                workdir: workdir.into(),
                conversation: Some(id.into()),
                worktree: None,
                mode: None,
                safety_check: false,
            }
        );
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(f.list_view().conversations, None);
    }

    /// What a view shows, for the back tests: the screen and which list.
    fn shown(view: &ViewState) -> &'static str {
        match &view.screen {
            Screen::Pairing(_) => "pairing",
            Screen::Settings(_) => "settings",
            Screen::ServerAddresses(_) => "server addresses",
            Screen::Usage(_) => "usage",
            Screen::Storage(_) => "storage",
            Screen::Worktrees(_) => "worktrees",
            Screen::Approvals(_) => "approvals",
            Screen::Search(_) => "search",
            Screen::UsageLimits(_) => "usage limits",
            Screen::SessionInfo(_) => "session info",
            Screen::Workdir(_) => "workdir",
            Screen::Crop(_) => "crop",
            Screen::Session(v) if v.archived => "archived session",
            Screen::Session(_) => "session",
            Screen::SessionList(l) if l.kind == ListKind::Archived => "archived list",
            Screen::SessionList(_) => "active list",
        }
    }

    /// From what `go` shows, back goes to `to`, and `can_go_back` says
    /// whether it goes anywhere.
    fn assert_back(f: &mut Fixture, go: impl FnOnce(&mut Fixture), from: &str, to: &str) {
        go(f);
        let before = f.model.view();
        assert_eq!(shown(&before), from);
        assert_eq!(before.can_go_back, from != to, "can_go_back on {from}");
        assert!(f.dispatch(Action::Back).is_empty());
        assert_eq!(shown(&f.model.view()), to, "back from {from}");
    }

    #[test]
    fn back_goes_to_the_parent_screen_and_stops_at_the_root() {
        let mut f = Fixture::paired();
        let active = f.session("/srv/a");
        let archived = f.session("/srv/b");
        queued(&f.dispatch(archive(archived, true)));

        let nav = |route| move |f: &mut Fixture| drop(f.dispatch(Action::Navigate(route)));
        assert_back(
            &mut f,
            nav(Route::SessionList),
            "active list",
            "active list",
        );
        assert_back(
            &mut f,
            nav(Route::ArchivedSessions),
            "archived list",
            "active list",
        );
        assert_back(&mut f, nav(Route::Settings), "settings", "active list");
        assert_back(
            &mut f,
            nav(Route::ServerAddresses),
            "server addresses",
            "settings",
        );
        // Usage and storage opened from Settings go back to Settings, so they
        // are opened from the session list here (see the test below).
        let from_list = |route| {
            move |f: &mut Fixture| {
                f.dispatch(Action::Navigate(Route::SessionList));
                f.dispatch(Action::Navigate(route));
            }
        };
        assert_back(&mut f, from_list(Route::Usage), "usage", "active list");
        assert_back(&mut f, from_list(Route::Storage), "storage", "active list");
        assert_back(&mut f, nav(Route::Worktrees), "worktrees", "active list");
        assert_back(&mut f, nav(Route::Approvals), "approvals", "active list");
        // Re-pairing a paired device can be cancelled.
        assert_back(&mut f, nav(Route::Pairing), "pairing", "active list");
        // A session goes back to the list it is on.
        assert_back(&mut f, |f| f.open(active), "session", "active list");
        assert_back(
            &mut f,
            |f| f.open(archived),
            "archived session",
            "active list",
        );
    }

    #[test]
    fn back_from_usage_and_storage_returns_to_settings_where_opened_there() {
        let mut f = Fixture::paired();
        let session = f.session("/srv/a");
        let from_settings = |route| {
            move |f: &mut Fixture| {
                f.dispatch(Action::Navigate(Route::Settings));
                f.dispatch(Action::Navigate(route));
            }
        };
        assert_back(&mut f, from_settings(Route::Usage), "usage", "settings");
        assert_back(&mut f, from_settings(Route::Storage), "storage", "settings");

        // Through the crop page and back, storage still returns to Settings.
        from_settings(Route::Storage)(&mut f);
        f.dispatch(Action::OpenCrop {
            blob: BlobId::new(),
            session,
            holds: BlobUse::ToolOutput,
            title: "Bash ls".into(),
        });
        assert_eq!(shown(&f.model.view()), "crop");
        // Back to storage asks for its reports again.
        f.dispatch(Action::Back);
        assert_back(&mut f, |_| {}, "storage", "settings");
    }

    #[test]
    fn back_goes_nowhere_while_unpaired() {
        let mut model = Model::new(empty_snapshot(), None);
        model.dispatch(Action::Navigate(Route::Settings));
        assert!(!model.view().can_go_back);
        assert!(model.dispatch(Action::Back).is_empty());
        assert!(matches!(model.view().screen, Screen::Pairing(_)));
    }

    #[test]
    fn back_from_a_session_gone_from_the_replica_goes_nowhere() {
        let mut f = Fixture::paired();
        // The open session's summary is missing: the active list shows.
        f.model.place = Place::session(SessionId::new());
        let view = f.model.view();
        assert_eq!(shown(&view), "active list");
        assert!(!view.can_go_back);
    }

    // ---- contexts -------------------------------------------------------

    fn started(name: &str, by: DeviceId) -> EventKind {
        EventKind::ContextStarted {
            by,
            name: name.into(),
        }
    }

    fn context_of(command: &Command) -> ContextId {
        match &command.kind {
            CommandKind::SendMessage { context, .. }
            | CommandKind::ClearContext { context, .. }
            | CommandKind::SwitchContext { context, .. } => *context,
            other => panic!("no context in {other:?}"),
        }
    }

    /// The open session's contexts as (name, messages, current).
    fn contexts(f: &Fixture) -> Vec<(String, usize, bool)> {
        f.session_view()
            .contexts
            .into_iter()
            .map(|c| (c.name, c.messages, c.current))
            .collect()
    }

    #[test]
    fn a_cleared_context_is_where_messages_go_at_once() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let first = ContextId::first_of(sid);
        assert!(!f.session_view().can_clear, "nothing to clear yet");
        assert_eq!(contexts(&f), [("start".to_owned(), 0, true)]);
        let hello = send(&mut f, sid, "hello");
        assert_eq!(context_of(&hello), first);
        assert!(f.session_view().can_clear, "a message on its way counts");

        let clear = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        let CommandKind::ClearContext {
            session_id,
            context,
            name,
        } = clear.kind.clone()
        else {
            panic!("expected a clear, got {clear:?}");
        };
        assert_eq!(session_id, sid);
        assert_ne!(context, first);
        assert_ne!(name, "start");
        assert_eq!(
            skywave_proto::normalise_context_name(&name).as_deref(),
            Some(name.as_str()),
            "named as the server will log it"
        );
        let view = f.session_view();
        assert_eq!(view.context, context);
        assert_eq!(view.clear, Some(Outgoing::Queued));
        assert!(!view.can_clear, "the new context has no message yet");
        assert_eq!(
            view.contexts
                .iter()
                .map(|c| (c.id, c.messages, c.current, c.outgoing))
                .collect::<Vec<_>>(),
            [
                (first, 1, false, None),
                (context, 0, true, Some(Outgoing::Queued))
            ]
        );
        assert_eq!(view.contexts[1].name, name);
        assert_eq!(view.contexts[1].last_activity, None);

        let fresh = send(&mut f, sid, "fresh");
        assert_eq!(context_of(&fresh), context);
        let view = f.session_view();
        assert!(view.can_clear);
        let pending: Vec<_> = view
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Pending { context, text, .. } => Some((text.as_str(), *context)),
                TimelineItem::Logged { .. } | TimelineItem::Change { .. } => None,
            })
            .collect();
        assert_eq!(pending, [("hello", first), ("fresh", context)]);
        assert!(
            f.dispatch(Action::ClearContext {
                session_id: SessionId::new()
            })
            .is_empty(),
            "unknown here"
        );
    }

    #[test]
    fn a_failed_clear_leaves_messages_where_they_went() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let first = ContextId::first_of(sid);
        let clear = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        // Queued after the clear, so written for its context.
        let stale = send(&mut f, sid, "after the clear");
        assert_eq!(context_of(&stale), context_of(&clear));
        f.model.command_result(
            clear.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextExists,
            },
            Utc::now(),
        );
        let view = f.session_view();
        assert_eq!(view.context, first, "a failed clear changed nothing");
        assert_eq!(
            view.clear,
            Some(Outgoing::Failed(FailReason::Rejected(
                RejectReason::ContextExists
            )))
        );
        assert_eq!(contexts(&f), [("start".to_owned(), 0, true)]);
        assert_eq!(context_of(&send(&mut f, sid, "next")), first);
    }

    #[test]
    fn an_accepted_switch_leads_until_it_is_pulled() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let first = ContextId::first_of(sid);
        let later = ContextId::new();
        f.log_in(sid, later, vec![started("velvet walrus", DeviceId::new())]);
        f.open(sid);
        assert_eq!(f.session_view().context, later);
        let switch = queued(&f.dispatch(Action::SwitchContext {
            session_id: sid,
            context: first,
        }));
        assert_eq!(f.session_view().switch, Some(Outgoing::Queued));
        f.model.command_sending(switch.id, Utc::now());
        f.model.command_result(
            switch.id,
            CommandResult::Accepted { seq: Seq(3) },
            Utc::now(),
        );
        assert_eq!(f.model.cursor, Seq(2), "not pulled yet");
        let view = f.session_view();
        assert_eq!(
            (view.context, view.switch),
            (first, Some(Outgoing::Accepted(Seq(3))))
        );
        assert_eq!(context_of(&send(&mut f, sid, "in the window")), first);
        f.log_stamped(
            sid,
            vec![(
                Some(switch.id),
                first,
                EventKind::ContextSwitched { by: f.me },
            )],
        );
        let view = f.session_view();
        assert_eq!((view.context, view.switch), (first, None));
        // Now the replica leads: another device's later switch wins.
        f.log_in(
            sid,
            later,
            vec![EventKind::ContextSwitched {
                by: DeviceId::new(),
            }],
        );
        assert_eq!(context_of(&send(&mut f, sid, "after")), later);
    }

    #[test]
    fn the_context_list_reads_the_log() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let first = ContextId::first_of(sid);
        let (empty, second) = (ContextId::new(), ContextId::new());
        let other = DeviceId::new();
        f.log(sid, vec![message("fix the test\nwith detail"), agent("ok")]);
        f.log_in(sid, empty, vec![started("velvet walrus", other)]);
        f.log_in(
            sid,
            second,
            vec![
                started("calm owl", other),
                message("second"),
                message("more"),
            ],
        );
        f.log(sid, vec![EventKind::ContextSwitched { by: other }]);
        f.open(sid);
        send(&mut f, sid, "queued here");
        let view = f.session_view();
        assert_eq!(view.context, first);
        assert!(view.can_clear);
        // Log order; the empty context that is not current is not offered.
        assert_eq!(
            contexts(&f),
            [
                ("start".to_owned(), 2, true),
                ("calm owl".to_owned(), 2, false)
            ]
        );
        assert_eq!(
            view.contexts
                .iter()
                .map(|c| (c.id, c.first_prompt.as_deref()))
                .collect::<Vec<_>>(),
            [(first, Some("fix the test")), (second, Some("second"))]
        );
        assert_eq!(view.contexts[1].last_activity, Some(f.events[6].at));
        assert_eq!(view.contexts[0].last_activity, Some(f.events[7].at));

        // Back in the empty one: it is offered, as current, and nothing
        // there to clear.
        f.log_in(sid, empty, vec![EventKind::ContextSwitched { by: other }]);
        assert_eq!(
            contexts(&f),
            [
                ("start".to_owned(), 2, false),
                ("velvet walrus".to_owned(), 0, true),
                ("calm owl".to_owned(), 2, false)
            ]
        );
        assert!(!f.session_view().can_clear);
    }

    #[test]
    fn a_cleared_name_is_none_the_session_has() {
        let used: HashSet<String> = ["amber fox", "calm owl"].map(str::to_owned).into();
        let mut rolls = ["amber fox", "calm owl", "amber fox", "dusky elk"].into_iter();
        assert_eq!(
            fresh_context_name(&used, || rolls.next().unwrap().to_owned()),
            "dusky elk",
            "used names are drawn again"
        );
        // A generator that only ever repeats: the name is numbered.
        assert_eq!(
            fresh_context_name(&used, || "amber fox".to_owned()),
            "amber fox 2"
        );
        let mut more = used.clone();
        more.insert("amber fox 2".into());
        assert_eq!(
            fresh_context_name(&more, || "amber fox".to_owned()),
            "amber fox 3"
        );
        let long = "x".repeat(skywave_proto::limits::CONTEXT_NAME_MAX_CHARS);
        let used: HashSet<String> = [long.clone()].into();
        let numbered = fresh_context_name(&used, || long.clone());
        assert_eq!(
            numbered.chars().count(),
            skywave_proto::limits::CONTEXT_NAME_MAX_CHARS
        );
        assert!(numbered.ends_with(" 2"));

        // What a clear avoids: the names logged and those of clears queued.
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.log_in(sid, ContextId::new(), vec![started("velvet walrus", f.me)]);
        f.open(sid);
        let clear = queued(&f.dispatch(Action::ClearContext { session_id: sid }));
        let CommandKind::ClearContext { name, .. } = clear.kind else {
            unreachable!()
        };
        assert!(!["start", "velvet walrus"].contains(&name.as_str()));
        assert_eq!(
            f.model.context_names(sid),
            ["start".to_owned(), "velvet walrus".to_owned(), name].into()
        );
    }

    #[test]
    fn dividers_mark_where_the_context_changed_and_viewing_one_narrows_the_timeline() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (a, b) = (ContextId::first_of(sid), ContextId::new());
        let laptop = DeviceId::new();
        f.log(sid, vec![message("a1")]);
        f.log_in(
            sid,
            b,
            vec![started("velvet walrus", laptop), message("b1")],
        );
        f.log(sid, vec![EventKind::ContextSwitched { by: f.me }]);
        // B's agent, stopped by the switch, ends its turn after it.
        f.log_in(sid, b, vec![EventKind::TurnInterrupted]);
        f.log(
            sid,
            vec![message("a2"), EventKind::ContextSwitched { by: laptop }],
        );
        f.open(sid);
        send(&mut f, sid, "a3");
        let texts = |f: &Fixture| -> Vec<String> {
            f.session_view()
                .timeline
                .iter()
                .map(|item| match item {
                    TimelineItem::Logged { entry, .. } => match entry {
                        Entry::UserMessage { text, .. } => text.clone(),
                        Entry::ContextStarted { name, by } => {
                            format!("started {name} by {}", if by.me { "me" } else { "laptop" })
                        }
                        Entry::ContextSwitched { name, by } => {
                            format!("back to {name} by {}", if by.me { "me" } else { "laptop" })
                        }
                        Entry::Interrupted => "interrupted".to_owned(),
                        other => format!("{other:?}"),
                    },
                    TimelineItem::Pending { text, .. } => format!("pending {text}"),
                    TimelineItem::Change { .. } => panic!("unexpected switch on its way"),
                })
                .collect()
        };
        // The second switch, to the context already current, has no entry.
        assert_eq!(
            texts(&f),
            [
                "Created { workdir: \"/srv/a\" }",
                "a1",
                "started velvet walrus by laptop",
                "b1",
                "back to start by me",
                "interrupted",
                "a2",
                "pending a3"
            ]
        );
        let stamps: Vec<ContextId> =
            f.session_view()
                .timeline
                .iter()
                .map(|item| match item {
                    TimelineItem::Logged { context, .. }
                    | TimelineItem::Pending { context, .. } => *context,
                    TimelineItem::Change { .. } => panic!("unexpected switch on its way"),
                })
                .collect();
        assert_eq!(stamps, [a, a, b, b, a, b, a, a]);

        let view = |f: &mut Fixture, context| {
            f.dispatch(Action::ViewContext {
                session_id: sid,
                context,
            })
        };
        assert!(view(&mut f, Some(b)).is_empty());
        assert_eq!(f.session_view().viewing, Some(b));
        assert_eq!(f.session_view().context, a, "viewing moves no message");
        assert_eq!(texts(&f), ["b1", "interrupted"], "B's late end is B's");
        let find = |f: &Fixture, q: &str| {
            crate::search::find(
                &f.session_view().timeline,
                &crate::search::Query::new(q),
                &mut |_| None,
            )
            .hits
            .len()
        };
        assert_eq!(
            (find(&f, "b1"), find(&f, "a1"), find(&f, "walrus")),
            (1, 0, 0)
        );
        view(&mut f, Some(a));
        assert_eq!(
            texts(&f),
            ["Created { workdir: \"/srv/a\" }", "a1", "a2", "pending a3"],
            "not B's end"
        );
        assert_eq!((find(&f, "b1"), find(&f, "a1")), (0, 1));
        view(&mut f, None);
        assert_eq!(texts(&f).len(), 8);
        assert_eq!(find(&f, "walrus"), 1, "the whole session's dividers");

        // Kept only while the session stays open.
        view(&mut f, Some(b));
        f.dispatch(Action::Back);
        f.open(sid);
        assert_eq!(f.session_view().viewing, None);
        assert!(view(&mut f, Some(b)).is_empty());
        f.dispatch(Action::Navigate(Route::Settings));
        assert!(
            view(&mut f, Some(b)).is_empty(),
            "only the session on screen"
        );
        f.open(sid);
        assert_eq!(f.session_view().viewing, None);
    }

    #[test]
    fn a_message_refused_for_a_changed_context_goes_back_to_the_draft() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let stale = send(&mut f, sid, "written before");
        assert_eq!(context_of(&stale), ContextId::first_of(sid));
        // Another device cleared meanwhile, so the server refuses it.
        let fresh = ContextId::new();
        f.log_in(sid, fresh, vec![started("velvet walrus", DeviceId::new())]);
        f.model.command_sending(stale.id, Utc::now());
        f.model.command_result(
            stale.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextChanged,
            },
            Utc::now(),
        );
        let refused = Outgoing::Failed(FailReason::Rejected(RejectReason::ContextChanged));
        assert!(f.session_view().timeline.iter().any(|item| matches!(
            item,
            TimelineItem::Pending { command_id, outgoing, discardable: true, .. }
                if *command_id == stale.id && *outgoing == refused
        )));
        let effects = f.dispatch(Action::EditQueued(stale.id));
        assert!(
            matches!(&effects[..], [Effect::Withdraw { id, draft: Some(_), .. }, _] if *id == stale.id)
        );
        let view = f.session_view();
        assert_eq!(
            view.draft.map(|d| d.text).as_deref(),
            Some("written before")
        );
        assert!(pending(&f).is_empty());
        assert_eq!(context_of(&send(&mut f, sid, "written before")), fresh);
    }

    // ---- moving a context ---------------------------------------------

    fn working() -> EventKind {
        EventKind::SessionStateChanged(LogState::AgentWorking)
    }

    /// The open session's context `id`, as its picker lists it.
    fn context_row(f: &Fixture, id: ContextId) -> ContextView {
        f.session_view()
            .contexts
            .into_iter()
            .find(|c| c.id == id)
            .expect("listed")
    }

    fn move_context(session_id: SessionId, context: ContextId) -> Action {
        Action::MoveContext {
            session_id,
            context,
        }
    }

    /// The new session's `SessionCreated` as the daemon logs it for a move
    /// of `context` out of `from`.
    fn moved_in(f: &mut Fixture, new: SessionId, from: SessionId, context: ContextId) {
        let created = EventKind::SessionCreated {
            workdir: "/srv/a".into(),
            context_name: "start".into(),
            from: Some(SessionOrigin::Context(skywave_proto::ContextRef {
                session: from,
                context,
            })),
            worktree: None,
        };
        assert!(!created.logged_by_command());
        f.log_stamped(new, vec![(None, ContextId::first_of(new), created)]);
    }

    #[test]
    fn moving_the_current_context_clears_it_first_and_opens_the_new_session() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let start = ContextId::first_of(sid);
        f.log(sid, vec![message("fix it"), working(), agent("done")]);
        f.open(sid);
        f.dispatch(mark_read(sid, 4));
        assert!(context_row(&f, start).can_move);

        let effects = f.dispatch(move_context(sid, start));
        let [
            Effect::Enqueue(clear, _),
            Effect::Enqueue(moved, _),
            Effect::Request(Request::OutboxChanged),
        ] = &effects[..]
        else {
            panic!("expected a clear, then a move, then one wake-up: {effects:?}");
        };
        let CommandKind::ClearContext {
            session_id,
            context: fresh,
            name: fresh_name,
        } = clear.kind.clone()
        else {
            panic!("expected a clear first, got {clear:?}");
        };
        assert_eq!(session_id, sid);
        assert_ne!(fresh_name, "start");
        assert_eq!(
            moved.kind,
            CommandKind::MoveContext {
                session_id: sid,
                context: start,
                name: "start".into()
            },
            "under the moved context's own name"
        );
        assert_eq!(
            f.model.opening,
            Some((moved.id, OpeningFrom::Session(sid))),
            "the move's id, not the clear's"
        );
        let view = f.session_view();
        assert_eq!(view.context, fresh, "the session goes on in a fresh one");
        let row = context_row(&f, start);
        assert_eq!(
            (row.outgoing, row.can_move, row.moved_to, row.current),
            (Some(Outgoing::Queued), false, None, false)
        );
        assert!(
            f.dispatch(move_context(sid, start)).is_empty(),
            "its move is on its way"
        );

        // The server logs the clear, the new session, then the move; this
        // pull splits between the last two.
        let new = SessionId::new();
        let at = f.events.len() as u64 + 1;
        for (command, seq) in [(clear.id, at), (moved.id, at + 2)] {
            f.model.command_sending(command, Utc::now());
            f.model.command_result(
                command,
                CommandResult::Accepted { seq: Seq(seq) },
                Utc::now(),
            );
        }
        f.log_stamped(
            sid,
            vec![(Some(clear.id), fresh, started(&fresh_name, f.me))],
        );
        moved_in(&mut f, new, sid, start);
        assert_eq!(f.model.open_session(), Some(sid), "the move is not in yet");
        f.log_stamped(
            sid,
            vec![(
                Some(moved.id),
                start,
                EventKind::ContextMoved { to: new, by: f.me },
            )],
        );
        assert_eq!(f.model.open_session(), Some(new), "follows the move's `to`");
        assert_eq!(f.model.unloaded_session(), Some(new));
        assert_eq!(f.model.opening, None);

        // Neither the clear nor the move is activity or news in the source;
        // the new session sorts first.
        let list = f.list_view_after_back();
        let ids: Vec<SessionId> = list.sessions.iter().map(|s| s.id).collect();
        assert_eq!(ids, [new, sid]);
        assert_eq!(unread(&f, sid), 0);
        f.log(sid, vec![agent("later")]);
        assert_eq!(unread(&f, sid), 1, "news still counts");

        f.open(new);
        let a = SessionRef {
            id: new,
            title: Some("a".into()),
        };
        assert_eq!(
            entries(&f).first(),
            Some(&Entry::MovedFrom {
                from: SessionRef {
                    id: sid,
                    title: Some("fix it".into())
                },
                name: "start".into()
            })
        );
        f.open(sid);
        let row = context_row(&f, start);
        assert_eq!(
            (row.moved_to, row.can_move, row.outgoing, row.messages),
            (Some(a.clone()), false, None, 1),
            "a moved context stays listed"
        );
        let me = f.model.device_ref(f.me);
        let dividers: Vec<Entry> = entries(&f)
            .into_iter()
            .filter(|e| !matches!(e, Entry::UserMessage { .. } | Entry::AgentMessage { .. }))
            .collect();
        assert_eq!(
            dividers,
            [
                Entry::Created {
                    workdir: "/srv/a".into(),
                },
                Entry::ContextStarted {
                    name: fresh_name.clone(),
                    by: me.clone()
                },
                Entry::ContextMoved {
                    name: "start".into(),
                    to: a,
                    by: me
                }
            ],
            "the clear's divider, then the move's"
        );
        let find = |f: &Fixture, q: &str| {
            crate::search::find(
                &f.session_view().timeline,
                &crate::search::Query::new(q),
                &mut |_| None,
            )
            .hits
            .len()
        };
        assert_eq!(find(&f, "start"), 1, "the move's divider, by name");

        // Never current again here; another context still switches.
        assert!(
            f.dispatch(Action::SwitchContext {
                session_id: sid,
                context: start
            })
            .is_empty()
        );
        queued(&f.dispatch(Action::SwitchContext {
            session_id: sid,
            context: fresh,
        }));

        // Viewing it: its history, no dividers.
        f.dispatch(Action::ViewContext {
            session_id: sid,
            context: Some(start),
        });
        let texts: Vec<Entry> = entries(&f);
        assert!(
            texts.iter().all(|e| matches!(
                e,
                Entry::Created { .. } | Entry::UserMessage { .. } | Entry::AgentMessage { .. }
            )),
            "{texts:?}"
        );
        assert_eq!(texts.len(), 4, "created, fix it, done, later");
    }

    #[test]
    fn a_context_not_current_moves_alone_and_only_opens_from_its_session() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let list = f.session("/srv/b");
        let start = ContextId::first_of(sid);
        f.log(sid, vec![message("fix it"), working()]);
        f.log_in(sid, ContextId::new(), vec![started("velvet walrus", f.me)]);
        f.open(sid);
        let moved = queued(&f.dispatch(move_context(sid, start)));
        assert_eq!(
            moved.kind,
            CommandKind::MoveContext {
                session_id: sid,
                context: start,
                name: "start".into()
            }
        );
        // Gone elsewhere meanwhile: stays there.
        f.open(list);
        let new = SessionId::new();
        moved_in(&mut f, new, sid, start);
        f.log_stamped(
            sid,
            vec![(
                Some(moved.id),
                start,
                EventKind::ContextMoved { to: new, by: f.me },
            )],
        );
        assert_eq!(f.model.open_session(), Some(list));
    }

    #[test]
    fn a_context_moves_once_its_agent_worked_and_this_devices_messages_are_in() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let start = ContextId::first_of(sid);
        f.log(sid, vec![message("fix it")]);
        f.open(sid);
        let row = context_row(&f, start);
        assert_eq!((row.messages, row.can_move), (1, false), "no agent ran");
        assert!(f.dispatch(move_context(sid, start)).is_empty());
        f.log(sid, vec![working()]);
        assert!(context_row(&f, start).can_move);

        // A message of this device on its way, accepted but not pulled
        // included, would go out before the clear: no move meanwhile.
        let hello = send(&mut f, sid, "hello");
        assert!(!context_row(&f, start).can_move, "queued");
        assert!(f.dispatch(move_context(sid, start)).is_empty());
        f.model.command_sending(hello.id, Utc::now());
        let seq = Seq(f.events.len() as u64 + 1);
        f.model
            .command_result(hello.id, CommandResult::Accepted { seq }, Utc::now());
        assert!(!context_row(&f, start).can_move, "accepted, not pulled");
        f.log_commands(
            sid,
            vec![(
                Some(hello.id),
                EventKind::UserMessage {
                    device_id: f.me,
                    text: "hello".into(),
                    attachments: Vec::new(),
                },
            )],
        );
        assert!(context_row(&f, start).can_move, "in the replica");

        // A refused one goes nowhere: it does not hold the move up.
        let refused = send(&mut f, sid, "refused");
        f.model.command_sending(refused.id, Utc::now());
        f.model.command_result(
            refused.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextChanged,
            },
            Utc::now(),
        );
        assert!(context_row(&f, start).can_move);
        // Nor does one to another context.
        let walrus = ContextId::new();
        f.log_in(sid, walrus, vec![started("velvet walrus", f.me)]);
        send(&mut f, sid, "to walrus");
        assert!(context_row(&f, start).can_move);
    }

    /// An adopted context's imported history is not its agent's work: the
    /// server has no handle to fork until a live turn ran (D32), so the move
    /// is not offered; adopting the conversation again continues it instead.
    #[test]
    fn an_adopted_context_with_only_imported_history_is_not_offered_a_move() {
        let mut f = Fixture::paired();
        let sid = SessionId::new();
        let start = ContextId::first_of(sid);
        let items = vec![
            imported(Some(1), imported_text(true, "fix it")),
            imported(Some(2), imported_text(false, "done")),
        ];
        log_adopted(&mut f, sid, CommandId::new(), items);
        f.log_in(sid, ContextId::new(), vec![started("velvet walrus", f.me)]);
        f.open(sid);
        let row = context_row(&f, start);
        assert_eq!((row.messages, row.can_move), (1, false), "imported only");
        assert!(f.dispatch(move_context(sid, start)).is_empty());
        // Positive control: once its agent worked in it, it moves.
        f.log_stamped(
            sid,
            vec![(
                None,
                start,
                EventKind::SessionStateChanged(LogState::AgentWorking),
            )],
        );
        assert!(context_row(&f, start).can_move);
    }

    #[test]
    fn a_move_refused_for_the_current_context_shows_failed_and_can_go() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let start = ContextId::first_of(sid);
        f.log(sid, vec![message("fix it"), working()]);
        f.open(sid);
        let effects = f.dispatch(move_context(sid, start));
        let [Effect::Enqueue(clear, _), Effect::Enqueue(moved, _), _] = &effects[..] else {
            panic!("{effects:?}");
        };
        let fresh = context_of(clear);
        f.model.command_sending(clear.id, Utc::now());
        let seq = Seq(f.events.len() as u64 + 1);
        f.model
            .command_result(clear.id, CommandResult::Accepted { seq }, Utc::now());
        f.log_stamped(sid, vec![(Some(clear.id), fresh, started("x", f.me))]);
        // Another device switched back before the move arrived.
        f.log(
            sid,
            vec![EventKind::ContextSwitched {
                by: DeviceId::new(),
            }],
        );
        let refused = Outgoing::Failed(FailReason::Rejected(RejectReason::ContextCurrent));
        f.model.command_sending(moved.id, Utc::now());
        f.model.command_result(
            moved.id,
            CommandResult::Rejected {
                reason: RejectReason::ContextCurrent,
            },
            Utc::now(),
        );
        assert_eq!(f.model.summary(sid).unwrap().context, start);
        assert_eq!(f.session_view().context, start, "the replica's");
        let row = context_row(&f, start);
        assert_eq!(
            (row.outgoing, row.discard_move, row.moved_to),
            (Some(refused), Some(moved.id), None)
        );
        assert!(row.can_move, "a refused move can be asked again");
        assert_eq!(
            f.dispatch(Action::Discard(moved.id)),
            [
                Effect::Withdraw {
                    id: moved.id,
                    draft: None,
                    images: None,
                    files: None,
                },
                Effect::Request(Request::OutboxChanged),
            ]
        );
        let row = context_row(&f, start);
        assert_eq!((row.outgoing, row.discard_move), (None, None));
    }

    /// A tool call of the subagent `parent` runs.
    fn sub_call(call: Option<CallId>, parent: CallId, kind: ToolKind, title: &str) -> EventKind {
        match tool_call(call, kind, title) {
            EventKind::ToolCall(c) => EventKind::ToolCall(ToolCall {
                parent: Some(parent),
                ..c
            }),
            _ => unreachable!("a tool call"),
        }
    }

    fn sub_agent(parent: CallId, text: &str) -> EventKind {
        EventKind::AgentMessage {
            parent: Some(parent),
            text: text.into(),
            full: None,
        }
    }

    fn view_subagent(session_id: SessionId, call: Option<CallId>) -> Action {
        Action::ViewSubagent { session_id, call }
    }

    /// The shown timeline's logged items, one word each: a tool call's
    /// title, a message's text, a standalone result's summary.
    fn shown_items(f: &Fixture) -> Vec<String> {
        entries(f)
            .into_iter()
            .map(|entry| match entry {
                Entry::Tool(view) => view.title,
                Entry::AgentMessage { text, .. } => text,
                Entry::ToolResult { summary, .. } => format!("result {summary}"),
                Entry::Approval { summary, .. } => format!("approval {summary}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// Outer subagent `a` (the main agent's) runs inner `b`, which runs
    /// shell call `c`, gated by approval `ap`; `m` is the main agent's own.
    struct Nested {
        sid: SessionId,
        a: CallId,
        b: CallId,
        c: CallId,
        m: CallId,
        ap: ApprovalId,
    }

    fn nested(f: &mut Fixture) -> Nested {
        let sid = f.session("/srv/a");
        let (a, b, c, m, ap) = (
            CallId::new(),
            CallId::new(),
            CallId::new(),
            CallId::new(),
            ApprovalId::new(),
        );
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                agent("main says"),
                tool_call(Some(a), ToolKind::Subagent, "outer"),
                sub_call(Some(b), a, ToolKind::Subagent, "inner"),
                sub_agent(a, "outer says"),
                sub_call(Some(c), b, ToolKind::Shell, "ls"),
                approval_requested(ap, Some(c)),
                EventKind::ApprovalResolved {
                    id: ap,
                    decision: Decision::Approve,
                    by: f.me,
                    rule: None,
                },
                tool_result(Some(c), false, "listing"),
                // A second result of a subagent's call stands alone, with it.
                tool_result(Some(c), false, "again"),
                tool_call(Some(m), ToolKind::Shell, "pwd"),
            ],
        );
        f.open(sid);
        Nested {
            sid,
            a,
            b,
            c,
            m,
            ap,
        }
    }

    #[test]
    fn each_agent_shows_only_its_own_items() {
        let mut f = Fixture::paired();
        let n = nested(&mut f);
        let view = f.session_view();
        assert_eq!(view.subagent, None);
        assert_eq!(
            shown_items(&f),
            [
                "Created { workdir: \"/srv/a\" }",
                "main says",
                "outer",
                "pwd"
            ]
        );

        f.dispatch(view_subagent(n.sid, Some(n.a)));
        assert_eq!(shown_items(&f), ["inner", "outer says"]);
        let header = f.session_view().subagent.expect("outer shown");
        assert_eq!((header.call, header.title.as_str()), (n.a, "outer"));
        assert_eq!(header.trail, []);

        f.dispatch(view_subagent(n.sid, Some(n.b)));
        // The approval and both results go where their call is.
        assert_eq!(shown_items(&f), ["ls", "result again"]);
        let c = &tools(&f)[0];
        assert_eq!(c.call, n.c);
        assert_eq!(c.approval.as_ref().map(|a| a.id), Some(n.ap));
        assert!(matches!(&c.outcome, ToolOutcome::Done { preview, .. } if preview == "listing"));
        let header = f.session_view().subagent.expect("inner shown");
        assert_eq!(header.trail, [(n.a, "outer".to_owned())]);

        // Back to the main view.
        f.dispatch(view_subagent(n.sid, None));
        assert_eq!(f.session_view().subagent, None);
        assert_eq!(
            shown_items(&f),
            [
                "Created { workdir: \"/srv/a\" }",
                "main says",
                "outer",
                "pwd"
            ]
        );
    }

    #[test]
    fn back_steps_out_through_the_subagents_to_the_session_list() {
        let mut f = Fixture::paired();
        let n = nested(&mut f);
        f.dispatch(view_subagent(n.sid, Some(n.b)));
        let shown = |f: &Fixture| f.session_view().subagent.map(|h| h.call);
        assert_eq!(shown(&f), Some(n.b));
        for to in [Some(n.a), None] {
            assert!(f.model.view().can_go_back);
            assert!(f.dispatch(Action::Back).is_empty());
            assert_eq!(shown(&f), to);
        }
        assert!(f.model.view().can_go_back);
        f.list_view_after_back();
    }

    #[test]
    fn viewing_a_subagent_needs_its_call_in_the_open_session() {
        let mut f = Fixture::paired();
        let n = nested(&mut f);
        let other = f.session("/srv/b");
        let shown = |f: &Fixture| f.session_view().subagent.map(|h| h.call);
        // Not a subagent's call, or not one logged.
        for call in [n.m, n.c, CallId::new()] {
            f.dispatch(view_subagent(n.sid, Some(call)));
            assert_eq!(shown(&f), None);
        }
        // Another session than the one open.
        f.dispatch(view_subagent(other, Some(n.a)));
        assert_eq!(shown(&f), None);
        // The control: the same call in the open session shows; an ignored
        // action then leaves it shown.
        f.dispatch(view_subagent(n.sid, Some(n.a)));
        assert_eq!(shown(&f), Some(n.a));
        f.dispatch(view_subagent(n.sid, Some(n.m)));
        assert_eq!(shown(&f), Some(n.a));

        // Opening the session again, or leaving it, shows the main view.
        f.open(n.sid);
        assert_eq!(shown(&f), None);
        f.dispatch(view_subagent(n.sid, Some(n.a)));
        f.dispatch(Action::Navigate(Route::Settings));
        f.open(n.sid);
        assert_eq!(shown(&f), None);
        assert_eq!(
            shown_items(&f),
            [
                "Created { workdir: \"/srv/a\" }",
                "main says",
                "outer",
                "pwd"
            ]
        );
    }

    #[test]
    fn search_finds_only_the_shown_agents_items() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let a = CallId::new();
        f.log(
            sid,
            vec![
                agent("needle in main"),
                tool_call(Some(a), ToolKind::Subagent, "explore"),
                sub_agent(a, "needle in subagent"),
            ],
        );
        f.open(sid);
        let found = |f: &Fixture| -> Vec<String> {
            let view = f.session_view();
            let query = crate::search::Query::new("needle");
            crate::search::find(&view.timeline, &query, &mut |_| None)
                .hits
                .into_iter()
                .map(|hit| match &view.timeline[hit.index] {
                    TimelineItem::Logged {
                        entry: Entry::AgentMessage { text, .. },
                        ..
                    } => text.clone(),
                    other => panic!("hit {other:?}"),
                })
                .collect()
        };
        assert_eq!(found(&f), ["needle in main"]);
        f.dispatch(view_subagent(sid, Some(a)));
        assert_eq!(found(&f), ["needle in subagent"]);
    }

    #[test]
    fn a_subagents_messages_are_not_news_but_its_approvals_are() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (a, c) = (CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                tool_call(Some(a), ToolKind::Subagent, "explore"),
                sub_agent(a, "looking"),
                sub_agent(a, "found it"),
            ],
        );
        assert_eq!(unread(&f, sid), 0);
        // The control: the main agent's message counts.
        f.log(sid, vec![agent("done")]);
        assert_eq!(unread(&f, sid), 1);
        f.log(
            sid,
            vec![
                sub_call(Some(c), a, ToolKind::Shell, "ls"),
                approval_requested(ApprovalId::new(), Some(c)),
            ],
        );
        assert_eq!(unread(&f, sid), 2);
        // Marking read recounts the same way.
        f.open(sid);
        let last = f.session_view().last_seq;
        f.dispatch(mark_read(sid, last.0));
        f.dispatch(Action::Back);
        assert_eq!(unread(&f, sid), 0);
        f.log(sid, vec![sub_agent(a, "more")]);
        assert_eq!(unread(&f, sid), 0);
    }

    fn subagent_task(id: &str, call: CallId) -> skywave_proto::BackgroundTask {
        skywave_proto::BackgroundTask {
            call: Some(call),
            kind: skywave_proto::TaskKind::Subagent,
            ..work_task(id)
        }
    }

    fn status_of(f: &Fixture, call: CallId) -> SubagentStatus {
        f.session_view()
            .timeline
            .iter()
            .find_map(|item| match item {
                TimelineItem::Logged {
                    entry: Entry::Tool(view),
                    ..
                } if view.call == call => view.subagent.clone(),
                _ => None,
            })
            .expect("a subagent's call")
    }

    fn session_info(f: &Fixture) -> SessionInfoView {
        match f.model.view().screen {
            Screen::SessionInfo(view) => *view,
            other => panic!("expected the session info page, got {other:?}"),
        }
    }

    #[test]
    fn the_session_info_page_lists_every_subagent_and_follows_the_log() {
        use crate::view::UsageLimitsStatus;

        let mut f = Fixture::paired();
        let n = nested(&mut f);
        let turn = crate::TurnUsage {
            tokens_in: Some(1_200),
            model: Some("opus".into()),
            ..crate::TurnUsage::default()
        };
        f.log(n.sid, vec![EventKind::TurnUsage(turn.clone())]);

        // Opened over the session: its events stay loaded, and the limits
        // are asked for while none are stored.
        let effects = f.dispatch(Action::OpenSessionInfo { session_id: n.sid });
        assert_eq!(usage_limits_req(&effects).map(|_| ()), Some(()));
        let view = session_info(&f);
        assert_eq!(view.workdir.as_deref(), Some("/srv/a"));
        assert_eq!(view.usage.as_ref().unwrap().last_turn, turn);
        assert_eq!(view.turns.len(), 1);
        assert_eq!(view.limits.status, UsageLimitsStatus::Loading);
        let calls = |view: &SessionInfoView| -> Vec<(CallId, String)> {
            view.subagents
                .iter()
                .map(|s| (s.call, s.title.clone()))
                .collect()
        };
        assert_eq!(
            calls(&view),
            [(n.a, "outer".to_owned()), (n.b, "inner".to_owned())]
        );
        assert_eq!(view.subagents[1].trail, [(n.a, "outer".to_owned())]);

        // A subagent launched while the page is on screen joins the list.
        let late = CallId::new();
        f.log(
            n.sid,
            vec![tool_call(Some(late), ToolKind::Subagent, "late")],
        );
        assert_eq!(calls(&session_info(&f)).len(), 3);
        assert_eq!(
            usage_limits_req(&f.dispatch(Action::RefreshUsageLimits)).map(|_| ()),
            Some(())
        );

        // Back returns to the session; from elsewhere the page loads the
        // session's events itself.
        assert_eq!(f.model.back(), Some(Action::OpenSession(n.sid)));
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(
            f.dispatch(Action::OpenSessionInfo { session_id: n.sid })
                .first(),
            Some(&Effect::LoadSession(n.sid))
        );
        assert_eq!(f.model.loaded_session(), Some(n.sid));
        assert!(
            f.dispatch(Action::OpenSessionInfo {
                session_id: SessionId::new()
            })
            .is_empty()
        );
    }

    #[test]
    fn a_subagents_status_counts_its_steps_and_follows_its_task() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (bg, fg, step, own) = (CallId::new(), CallId::new(), CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(bg), ToolKind::Subagent, "background"),
                tool_result(Some(bg), false, "Async agent launched"),
                EventKind::BackgroundTasksChanged {
                    tasks: vec![subagent_task("t1", bg)],
                },
                sub_call(Some(step), bg, ToolKind::Shell, "ls"),
                sub_call(None, bg, ToolKind::Shell, "unlinked"),
                sub_agent(bg, "\n  found it  \nand more"),
                tool_call(Some(own), ToolKind::Shell, "pwd"),
                EventKind::SessionStateChanged(LogState::Idle),
            ],
        );
        f.open(sid);
        assert_eq!(
            status_of(&f, bg),
            SubagentStatus {
                task: Some(TaskId("t1".into())),
                live: true,
                steps: 2,
                last: Some("found it".into()),
                usage: None,
            }
        );
        // Its own outcome is how its launch went.
        assert!(matches!(tools(&f)[0].outcome, ToolOutcome::Done { .. }));
        assert_eq!(tools(&f)[1].subagent, None, "not a subagent's call");
        // Its task leaving ends it; its figures come outside any turn.
        let figures = SubagentUsage {
            call: bg,
            duration_ms: Some(2_770),
            tool_uses: Some(2),
            context_tokens: Some(10_592),
        };
        f.log(
            sid,
            vec![
                EventKind::SubagentUsage(figures.clone()),
                EventKind::BackgroundTasksChanged { tasks: vec![] },
            ],
        );
        assert_eq!(
            status_of(&f, bg),
            SubagentStatus {
                task: None,
                live: false,
                steps: 2,
                last: Some("found it".into()),
                usage: Some(figures),
            }
        );

        // A foreground one works while its call runs.
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(fg), ToolKind::Subagent, "foreground"),
            ],
        );
        let idle = SubagentStatus {
            task: None,
            live: true,
            steps: 0,
            last: None,
            usage: None,
        };
        assert_eq!(status_of(&f, fg), idle);
        // Its figures come before its result, inside the turn.
        let figures = SubagentUsage {
            call: fg,
            duration_ms: Some(4_284),
            tool_uses: Some(1),
            context_tokens: Some(11_850),
        };
        f.log(
            sid,
            vec![
                EventKind::SubagentUsage(figures.clone()),
                tool_result(Some(fg), false, "report"),
            ],
        );
        assert_eq!(
            status_of(&f, fg),
            SubagentStatus {
                live: false,
                usage: Some(figures),
                ..idle
            }
        );
        assert_eq!(status_of(&f, bg).usage.unwrap().call, bg, "each its own");
    }

    #[test]
    fn a_subagents_approval_names_it_and_shows_its_item_in_its_view() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (a, c, main) = (CallId::new(), CallId::new(), CallId::new());
        let (sub_ap, main_ap) = (ApprovalId::new(), ApprovalId::new());
        f.log(
            sid,
            vec![
                tool_call(Some(a), ToolKind::Subagent, "explore"),
                sub_call(Some(c), a, ToolKind::Shell, "rm -rf target"),
                approval_requested(sub_ap, Some(c)),
            ],
        );
        f.open(sid);
        let prompt = f.session_view().pending_approval.expect("pending");
        assert_eq!((prompt.id, prompt.call), (sub_ap, Some(c)));
        assert_eq!(prompt.from.as_deref(), Some("explore"));
        // The main view shows the prompt without its item; the subagent's
        // view shows both.
        assert!(tools(&f).iter().all(|v| v.call != c));
        f.dispatch(view_subagent(sid, Some(a)));
        assert_eq!(f.session_view().pending_approval, Some(prompt));
        assert!(tools(&f).iter().any(|v| v.call == c));

        f.log(
            sid,
            vec![
                resolved(sub_ap),
                tool_call(Some(main), ToolKind::Shell, "ls"),
                approval_requested(main_ap, Some(main)),
            ],
        );
        let prompt = f.session_view().pending_approval.expect("pending");
        assert_eq!((prompt.id, prompt.from), (main_ap, None));
    }

    /// The workdir screen: reads on request, kept per ask.
    mod workdir {
        use std::sync::mpsc::Receiver;

        use skywave_proto::{
            BlobId as ProtoBlobId, ChangeSet, Commit, CommitLog, DirEntry, EntryKind, ListResult,
            LogResult, WriteFileResult,
        };

        use super::*;
        use crate::identity::PairedCredentials;
        use crate::view::{PushState, WorkdirPush};
        use crate::{Core, Seam};

        fn blob(size: u64) -> BlobRef {
            BlobRef {
                id: ProtoBlobId::new(),
                size,
                sha256: skywave_proto::ContentHash::of(b""),
            }
        }

        fn changed(path: &str, change: FileChange, diff: DiffBody) -> ChangedFile {
            ChangedFile {
                path: path.into(),
                from: None,
                change,
                added: Some(1),
                removed: Some(0),
                diff,
            }
        }

        fn changes(files: Vec<ChangedFile>) -> WorkdirAnswer {
            WorkdirAnswer::Changes(ChangesResult::Changes(ChangeSet {
                at: Utc::now(),
                branch: Some("main".into()),
                head: Some("abc1234".into()),
                total: files.len() as u32,
                files,
            }))
        }

        fn file(path: &str, content: FileBody) -> WorkdirAnswer {
            WorkdirAnswer::File(FileResult::File {
                at: Utc::now(),
                path: path.into(),
                size: 4,
                content,
            })
        }

        fn listed(path: &str, names: &[&str]) -> WorkdirAnswer {
            WorkdirAnswer::List(ListResult::Listed {
                at: Utc::now(),
                path: path.into(),
                entries: names
                    .iter()
                    .map(|name| DirEntry {
                        name: (*name).into(),
                        kind: EntryKind::File,
                        size: Some(1),
                    })
                    .collect(),
                truncated: false,
            })
        }

        fn logged(subject: &str) -> WorkdirAnswer {
            WorkdirAnswer::Log(LogResult::Log(CommitLog {
                at: Utc::now(),
                branch: Some("main".into()),
                commits: vec![Commit {
                    sha: "f".repeat(40),
                    short: "fffffff".into(),
                    author: "Ada".into(),
                    time: Some(Utc::now()),
                    subject: subject.into(),
                }],
                more: false,
            }))
        }

        fn requests(effects: Vec<Effect>) -> Vec<Request> {
            effects
                .into_iter()
                .map(|effect| match effect {
                    Effect::Request(request) => request,
                    other => panic!("expected only requests, got {other:?}"),
                })
                .collect()
        }

        /// The one workdir read `effects` ask for.
        fn asked(effects: Vec<Effect>) -> (u64, WorkdirAsk) {
            match &requests(effects)[..] {
                [Request::Workdir { req, ask }] => (*req, ask.clone()),
                other => panic!("expected one workdir read, got {other:?}"),
            }
        }

        /// Stores `answer` to the awaited `req` as the seam does, the
        /// replica registering every blob the model did not know.
        fn answer(
            f: &mut Fixture,
            req: u64,
            ask: WorkdirAsk,
            answer: WorkdirAnswer,
        ) -> Vec<Request> {
            assert!(f.model.workdir_awaited(req, &ask), "{req} awaited");
            let registered = answer
                .blob_refs()
                .iter()
                .map(|b| b.id)
                .filter(|id| !f.model.blobs.contains_key(id))
                .collect();
            let stored = WorkdirStored {
                registered,
                purged: Vec::new(),
            };
            f.model.workdir_stored(ask, answer, &stored)
        }

        fn view(f: &Fixture) -> WorkdirView {
            match f.model.view().screen {
                Screen::Workdir(view) => view,
                other => panic!("expected the workdir screen, got {other:?}"),
            }
        }

        fn offline() -> PairedLink {
            PairedLink::Reconnecting {
                attempt: 1,
                retry_at: Utc::now(),
                offline_since: Utc::now(),
                reason: crate::view::LinkFailure::Unreachable,
            }
        }

        fn not_connected() -> String {
            crate::workdir::not_connected().to_string()
        }

        #[test]
        fn workdir_first_show_asks_and_a_stored_show_does_not() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            assert_eq!(ask, WorkdirAsk::Changes { session: id });
            let shown = view(&f);
            assert_eq!(shown.title, "a");
            assert_eq!(shown.workdir.as_deref(), Some("/srv/a"));
            assert_eq!(shown.pane, WorkdirPane::Changes);
            assert_eq!(shown.fetch, WorkdirFetch::Loading);
            assert_eq!(shown.body, WorkdirBody::Nothing);
            // Showing it again while the read is on its way asks nothing.
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Changes))
                    .is_empty()
            );

            let set = changes(vec![changed(
                "a.rs",
                FileChange::Modified,
                DiffBody::Binary,
            )]);
            assert!(answer(&mut f, req, ask.clone(), set.clone()).is_empty());
            let shown = view(&f);
            assert_eq!(shown.fetch, WorkdirFetch::Idle);
            let WorkdirAnswer::Changes(result) = set else {
                unreachable!()
            };
            assert_eq!(shown.body, WorkdirBody::Changes(result));

            // Stored: leaving and coming back asks nothing.
            f.dispatch(Action::Navigate(Route::SessionList));
            assert!(
                f.dispatch(Action::OpenWorkdir { session_id: id })
                    .is_empty()
            );
            assert_eq!(view(&f).fetch, WorkdirFetch::Idle);
            // A directory never read is asked for on its first show, once.
            let (req, ask) =
                asked(f.dispatch(Action::ShowWorkdir(WorkdirPane::Dir { path: "src".into() })));
            assert_eq!(
                ask,
                WorkdirAsk::List {
                    session: id,
                    path: "src".into()
                }
            );
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Dir { path: "src".into() }))
                    .is_empty()
            );
            let dir = listed("src", &["lib.rs"]);
            answer(&mut f, req, ask, dir.clone());
            let WorkdirAnswer::List(result) = dir else {
                unreachable!()
            };
            assert_eq!(view(&f).body, WorkdirBody::Dir(result));
            assert_eq!(view(&f).last_dir, "src");
        }

        #[test]
        fn workdir_the_commits_pane_asks_the_log_once_and_refreshes_it() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask, changes(Vec::new()));

            let (req, ask) = asked(f.dispatch(Action::ShowWorkdir(WorkdirPane::Commits)));
            assert_eq!(ask, WorkdirAsk::Log { session: id });
            let shown = view(&f);
            assert_eq!(shown.pane, WorkdirPane::Commits);
            assert_eq!(shown.fetch, WorkdirFetch::Loading);
            assert_eq!(shown.body, WorkdirBody::Nothing);
            // Showing it again while the read is on its way asks nothing.
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Commits))
                    .is_empty()
            );

            let log = logged("Add the log");
            assert!(answer(&mut f, req, ask.clone(), log.clone()).is_empty());
            let shown = view(&f);
            assert_eq!(shown.fetch, WorkdirFetch::Idle);
            let WorkdirAnswer::Log(result) = log else {
                unreachable!()
            };
            assert_eq!(shown.body, WorkdirBody::Log(result));

            // Stored: showing it again asks nothing, and refreshing asks again.
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Commits))
                    .is_empty()
            );
            let (again, asked_again) = asked(f.dispatch(Action::RefreshWorkdir));
            assert_eq!(asked_again, WorkdirAsk::Log { session: id });
            assert!(again > req, "a fresh request id");
            assert_eq!(view(&f).fetch, WorkdirFetch::Loading);

            // The changes pane is stored too: switching back asks nothing.
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Changes))
                    .is_empty()
            );
        }

        #[test]
        fn workdir_a_failed_log_keeps_the_stored_log() {
            let live = Live::new();
            live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            let (req, ask) = live.asked(Action::ShowWorkdir(WorkdirPane::Commits));
            let good = logged("Add the log");
            live.seam
                .workdir_answered(req, ask.clone(), Ok(good.clone()))
                .unwrap();
            let WorkdirAnswer::Log(result) = good.clone() else {
                unreachable!()
            };
            assert_eq!(live.view().body, WorkdirBody::Log(result.clone()));

            let (req, _) = live.asked(Action::RefreshWorkdir);
            let failed = WorkdirAnswer::Log(LogResult::Failed {
                reason: "git timed out".into(),
            });
            live.seam
                .workdir_answered(req, ask.clone(), Ok(failed))
                .unwrap();
            let shown = live.view();
            assert_eq!(shown.fetch, WorkdirFetch::Failed("git timed out".into()));
            assert_eq!(shown.body, WorkdirBody::Log(result));
            assert_eq!(live.stored()[&ask], good, "the store kept the good one");
        }

        #[test]
        fn workdir_refresh_asks_again_and_a_diff_refreshes_the_changes() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask.clone(), changes(Vec::new()));

            let (again, asked_again) = asked(f.dispatch(Action::RefreshWorkdir));
            assert_eq!(asked_again, ask);
            assert!(again > req, "a fresh request id");
            assert!(!f.model.workdir_awaited(req, &ask), "the old one is stale");
            assert_eq!(view(&f).fetch, WorkdirFetch::Loading);
            assert_ne!(view(&f).body, WorkdirBody::Nothing, "the stored body stays");

            // A diff pane reads the stored changes and refreshes them.
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
                    path: "a.rs".into(),
                    untracked: false,
                }))
                .is_empty()
            );
            let (_, diff_ask) = asked(f.dispatch(Action::RefreshWorkdir));
            assert_eq!(diff_ask, ask);
            // Not on the workdir screen, refresh does nothing.
            f.dispatch(Action::Navigate(Route::SessionList));
            assert!(f.dispatch(Action::RefreshWorkdir).is_empty());
        }

        #[test]
        fn workdir_offline_refresh_fails_at_once_and_keeps_the_stored_body() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask, changes(Vec::new()));
            let stored = view(&f).body;

            f.model.set_link(offline());
            assert!(
                f.dispatch(Action::RefreshWorkdir).is_empty(),
                "nothing asked"
            );
            let shown = view(&f);
            assert_eq!(shown.fetch, WorkdirFetch::Failed(not_connected()));
            assert_eq!(shown.body, stored);
            // A pane never read fails at once too, with nothing to show.
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Dir {
                    path: String::new()
                }))
                .is_empty()
            );
            let shown = view(&f);
            assert_eq!(shown.fetch, WorkdirFetch::Failed(not_connected()));
            assert_eq!(shown.body, WorkdirBody::Nothing);

            // Back online, a refresh asks again and clears the failure.
            f.model.set_link(ONLINE);
            asked(f.dispatch(Action::RefreshWorkdir));
            assert_eq!(view(&f).fetch, WorkdirFetch::Loading);
        }

        #[test]
        fn workdir_download_asks_afresh_and_wants_its_blob_off_screen() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let other = f.session("/srv/b");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask, changes(Vec::new()));
            let download = |path: &str| WorkdirAsk::Download {
                session: id,
                path: path.into(),
            };

            // Asked for, and in flight: a second dispatch asks nothing.
            let (req, ask) = asked(f.dispatch(Action::DownloadWorkdirFile {
                session_id: id,
                path: "big.bin".into(),
            }));
            assert_eq!(ask, download("big.bin"));
            assert!(
                f.dispatch(Action::DownloadWorkdirFile {
                    session_id: id,
                    path: "big.bin".into(),
                })
                .is_empty(),
                "in flight"
            );
            let shown = view(&f);
            assert_eq!(shown.downloads.len(), 1);
            assert_eq!(shown.downloads[0].path, "big.bin");
            assert_eq!(shown.downloads[0].fetch, WorkdirFetch::Loading);
            assert_eq!(shown.downloads[0].result, None);

            // The content is wanted though the Changes pane is on screen.
            let big = blob(4);
            assert_eq!(
                answer(
                    &mut f,
                    req,
                    ask,
                    file("big.bin", FileBody::Blob(big.clone()))
                ),
                vec![Request::DownloadBlob(big.id)]
            );
            let shown = view(&f);
            assert_eq!(shown.downloads[0].fetch, WorkdirFetch::Idle);
            assert_eq!(
                shown.downloads[0].blob.as_ref().map(|b| &b.state),
                Some(&BlobState::Requested { size: 4 })
            );

            // Asked afresh once more; the blob is wanted already, so nothing.
            let (req, ask) = asked(f.dispatch(Action::DownloadWorkdirFile {
                session_id: id,
                path: "big.bin".into(),
            }));
            assert_eq!(ask, download("big.bin"));
            assert!(
                answer(
                    &mut f,
                    req,
                    ask,
                    file("big.bin", FileBody::Blob(big.clone()))
                )
                .is_empty(),
                "already wanted"
            );

            // Over the cap: no blob is wanted, and the entry says so. Listed
            // by path, not by when it was asked for.
            let (req, ask) = asked(f.dispatch(Action::DownloadWorkdirFile {
                session_id: id,
                path: "a.bin".into(),
            }));
            assert!(answer(&mut f, req, ask, file("a.bin", FileBody::TooLarge)).is_empty());
            let shown = view(&f);
            let paths: Vec<&str> = shown.downloads.iter().map(|d| d.path.as_str()).collect();
            assert_eq!(paths, ["a.bin", "big.bin"]);
            assert!(matches!(
                shown.downloads[0].result,
                Some(FileResult::File {
                    content: FileBody::TooLarge,
                    ..
                })
            ));
            assert_eq!(shown.downloads[0].blob, None);

            // Another session's download is not this session's.
            asked(f.dispatch(Action::DownloadWorkdirFile {
                session_id: other,
                path: "other.bin".into(),
            }));
            assert_eq!(view(&f).downloads.len(), 2);

            // An unknown session: nothing.
            assert!(
                f.dispatch(Action::DownloadWorkdirFile {
                    session_id: SessionId::new(),
                    path: "big.bin".into(),
                })
                .is_empty()
            );

            // Offline: asked for nothing, and the entry fails at once.
            f.model.set_link(offline());
            assert!(
                f.dispatch(Action::DownloadWorkdirFile {
                    session_id: id,
                    path: "offline.bin".into(),
                })
                .is_empty()
            );
            let shown = view(&f);
            let entry = shown
                .downloads
                .iter()
                .find(|d| d.path == "offline.bin")
                .expect("listed");
            assert_eq!(entry.fetch, WorkdirFetch::Failed(not_connected()));
            assert_eq!(entry.result, None);
        }

        /// The one push `effects` ask for, with its `req`.
        fn push_asked(effects: Vec<Effect>) -> (u64, Request) {
            match &requests(effects)[..] {
                [request @ Request::PushWorkdirFile { req, .. }] => (*req, request.clone()),
                other => panic!("expected one push, got {other:?}"),
            }
        }

        #[test]
        fn workdir_push_asks_once_per_path_and_keeps_its_outcome() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let other = f.session("/srv/b");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask, changes(Vec::new()));
            let push = |session: SessionId, path: &str| Action::PushWorkdirFile {
                session_id: session,
                source: PathBuf::from("/home/me/notes.md"),
                path: path.into(),
                replace: None,
            };
            let pushing = |path: &str| WorkdirPush {
                path: path.into(),
                source: PathBuf::from("/home/me/notes.md"),
                state: PushState::Pushing,
            };

            // Asked for once, naming the file, the path and the session.
            let (req, request) = push_asked(f.dispatch(push(id, "docs/notes.md")));
            assert_eq!(
                request,
                Request::PushWorkdirFile {
                    req,
                    session: id,
                    source: PathBuf::from("/home/me/notes.md"),
                    path: "docs/notes.md".into(),
                    replace: None,
                }
            );

            // In flight: a second push to the path asks nothing.
            assert!(
                f.dispatch(push(id, "docs/notes.md")).is_empty(),
                "in flight"
            );
            assert_eq!(view(&f).pushes, [pushing("docs/notes.md")]);

            // An answer to another req is stale: nothing changes.
            f.model.workdir_pushed(
                req + 100,
                id,
                "docs/notes.md".into(),
                Err(TransportError::Timeout),
            );
            assert_eq!(view(&f).pushes, [pushing("docs/notes.md")]);

            // The awaited answer is kept.
            let written = PushOutcome::Written(WriteFileResult::Exists {
                sha256: ContentHash::of(b"old"),
                size: 3,
            });
            f.model
                .workdir_pushed(req, id, "docs/notes.md".into(), Ok(written.clone()));
            assert_eq!(
                view(&f).pushes,
                [WorkdirPush {
                    state: PushState::Done(written),
                    ..pushing("docs/notes.md")
                }]
            );

            // Pushed again once done: asked afresh, and the old req's answer
            // is dropped.
            let (again, _) = push_asked(f.dispatch(push(id, "docs/notes.md")));
            assert_ne!(again, req);
            f.model.workdir_pushed(
                req,
                id,
                "docs/notes.md".into(),
                Err(TransportError::Timeout),
            );
            assert_eq!(view(&f).pushes, [pushing("docs/notes.md")]);

            // Another session's push is not in this view.
            push_asked(f.dispatch(push(other, "other.md")));
            assert_eq!(view(&f).pushes, [pushing("docs/notes.md")]);

            // An unknown session: nothing.
            assert!(
                f.dispatch(push(SessionId::new(), "x.md")).is_empty(),
                "unknown"
            );

            // Offline: asked for nothing, and the entry fails at once.
            f.model.set_link(offline());
            assert!(f.dispatch(push(id, "offline.md")).is_empty());
            let shown = view(&f).pushes;
            let entry = shown
                .iter()
                .find(|p| p.path == "offline.md")
                .expect("listed");
            assert_eq!(entry.state, PushState::Failed(not_connected()));
        }

        #[test]
        fn workdir_a_push_cancels_only_while_pushing() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask, changes(Vec::new()));
            let push = |session: SessionId, path: &str| Action::PushWorkdirFile {
                session_id: session,
                source: PathBuf::from("/home/me/notes.md"),
                path: path.into(),
                replace: None,
            };
            let cancel = |session: SessionId, path: &str| Action::CancelWorkdirPush {
                session_id: session,
                path: path.into(),
            };

            // Nothing to cancel before a push.
            assert!(f.dispatch(cancel(id, "docs/notes.md")).is_empty());

            // Cancelling asks the engine to stop the push; it still shows as
            // pushing until the engine answers.
            let (req, _) = push_asked(f.dispatch(push(id, "docs/notes.md")));
            assert_eq!(
                requests(f.dispatch(cancel(id, "docs/notes.md"))),
                [Request::CancelPush { req }]
            );
            assert_eq!(view(&f).pushes[0].state, PushState::Pushing);

            // The engine's answer ends the push; cancelling it again is a no-op.
            f.model
                .workdir_pushed(req, id, "docs/notes.md".into(), Ok(PushOutcome::Cancelled));
            assert_eq!(
                view(&f).pushes[0].state,
                PushState::Done(PushOutcome::Cancelled)
            );
            assert!(f.dispatch(cancel(id, "docs/notes.md")).is_empty());

            // A cancelled push does not block its path: pushing again asks.
            push_asked(f.dispatch(push(id, "docs/notes.md")));
            assert_eq!(view(&f).pushes[0].state, PushState::Pushing);
        }

        #[test]
        fn workdir_a_failure_keeps_the_stored_snapshot() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask.clone(), changes(Vec::new()));
            let stored = view(&f).body;
            let (req, _) = asked(f.dispatch(Action::RefreshWorkdir));
            assert!(f.model.workdir_awaited(req, &ask));
            f.model.workdir_failed(&ask, "timed out".into());
            let shown = view(&f);
            assert_eq!(shown.fetch, WorkdirFetch::Failed("timed out".into()));
            assert_eq!(shown.body, stored);
            assert!(!f.model.workdir_awaited(req, &ask), "answered");
        }

        #[test]
        fn workdir_diff_and_file_panes_want_an_undownloaded_blob_once() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let diff = blob(10);
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            let set = changes(vec![changed(
                "a.rs",
                FileChange::Modified,
                DiffBody::Blob(diff.clone()),
            )]);
            // The changes pane shows no blob, so wants none.
            assert!(answer(&mut f, req, ask, set).is_empty());
            assert!(!f.model.wanted.contains(&diff.id));

            let pane = WorkdirPane::Diff {
                path: "a.rs".into(),
                untracked: false,
            };
            assert_eq!(
                requests(f.dispatch(Action::ShowWorkdir(pane.clone()))),
                [Request::DownloadBlob(diff.id)]
            );
            let WorkdirBody::Diff {
                blob: Some(shown), ..
            } = view(&f).body
            else {
                panic!("expected the diff");
            };
            assert_eq!(shown.state, BlobState::Requested { size: 10 });
            // Shown again, or after the changes: asked for once.
            assert!(f.dispatch(Action::ShowWorkdir(pane.clone())).is_empty());
            f.dispatch(Action::ShowWorkdir(WorkdirPane::Changes));
            assert!(f.dispatch(Action::ShowWorkdir(pane.clone())).is_empty());
            // A failed download is asked for again by hand only.
            f.model.blob_status(
                diff.id,
                BlobStatus::Failed {
                    reason: "size mismatch".into(),
                },
            );
            f.dispatch(Action::Back);
            assert!(f.dispatch(Action::ShowWorkdir(pane)).is_empty());

            // A file pane asks for its content as its answer arrives.
            let content = blob(4);
            let (req, ask) = asked(f.dispatch(Action::ShowWorkdir(WorkdirPane::File {
                path: "b.txt".into(),
            })));
            assert_eq!(
                answer(
                    &mut f,
                    req,
                    ask,
                    file("b.txt", FileBody::Blob(content.clone()))
                ),
                [Request::DownloadBlob(content.id)]
            );
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::File {
                    path: "b.txt".into()
                }))
                .is_empty()
            );
            // An answer arriving for a pane not on screen wants nothing.
            let other = blob(5);
            let (req, ask) = asked(f.dispatch(Action::ShowWorkdir(WorkdirPane::File {
                path: "c.txt".into(),
            })));
            f.dispatch(Action::ShowWorkdir(WorkdirPane::Changes));
            assert!(
                answer(
                    &mut f,
                    req,
                    ask,
                    file("c.txt", FileBody::Blob(other.clone()))
                )
                .is_empty()
            );
            assert!(!f.model.wanted.contains(&other.id));
        }

        #[test]
        fn workdir_a_stopped_blob_is_asked_for_again_only_by_hand() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let diff = blob(10);
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            let set = changes(vec![changed(
                "a.rs",
                FileChange::Modified,
                DiffBody::Blob(diff.clone()),
            )]);
            assert!(answer(&mut f, req, ask, set).is_empty());

            let pane = WorkdirPane::Diff {
                path: "a.rs".into(),
                untracked: false,
            };
            assert_eq!(
                requests(f.dispatch(Action::ShowWorkdir(pane.clone()))),
                [Request::DownloadBlob(diff.id)]
            );
            // Stopped by hand: the request goes out, the blob shows as not
            // downloaded, and nothing asks for it again on its own.
            assert_eq!(
                requests(f.dispatch(Action::StopBlobDownload(diff.id))),
                [Request::StopBlob(diff.id)]
            );
            let WorkdirBody::Diff {
                blob: Some(shown), ..
            } = view(&f).body
            else {
                panic!("expected the diff");
            };
            assert_eq!(shown.state, BlobState::NotDownloaded { size: 10 });
            assert!(f.dispatch(Action::ShowWorkdir(pane.clone())).is_empty());
            f.dispatch(Action::ShowWorkdir(WorkdirPane::Changes));
            assert!(f.dispatch(Action::ShowWorkdir(pane.clone())).is_empty());
            // Stopping again, with nothing asked for or under way, does nothing.
            assert!(f.dispatch(Action::StopBlobDownload(diff.id)).is_empty());
            // Asked for by hand: it goes again and shows as requested.
            assert_eq!(
                requests(f.dispatch(Action::DownloadBlob(diff.id))),
                [Request::DownloadBlob(diff.id)]
            );
            let WorkdirBody::Diff {
                blob: Some(shown), ..
            } = view(&f).body
            else {
                panic!("expected the diff");
            };
            assert_eq!(shown.state, BlobState::Requested { size: 10 });
        }

        #[test]
        fn workdir_a_download_cancelled_while_asked_or_downloading_stops() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask, changes(Vec::new()));
            let download = || Action::DownloadWorkdirFile {
                session_id: id,
                path: "big.bin".into(),
            };
            let cancel = || Action::CancelWorkdirDownload {
                session_id: id,
                path: "big.bin".into(),
            };

            // Asked, and in flight: the cancel drops the ask, which shows as
            // failed, and the late answer is stale.
            let (req, ask) = asked(f.dispatch(download()));
            assert!(f.dispatch(cancel()).is_empty());
            assert_eq!(
                view(&f).downloads[0].fetch,
                WorkdirFetch::Failed("cancelled".into())
            );
            assert!(!f.model.workdir_awaited(req, &ask));
            // Nothing in flight any more: a second cancel does nothing.
            assert!(f.dispatch(cancel()).is_empty());

            // Asked afresh and answered with a blob: the content downloads.
            let (req, ask) = asked(f.dispatch(download()));
            let big = blob(4);
            assert_eq!(
                answer(
                    &mut f,
                    req,
                    ask,
                    file("big.bin", FileBody::Blob(big.clone()))
                ),
                vec![Request::DownloadBlob(big.id)]
            );
            // Downloading: the cancel stops it, and the blob shows as not
            // downloaded.
            assert_eq!(requests(f.dispatch(cancel())), [Request::StopBlob(big.id)]);
            assert_eq!(
                view(&f).downloads[0].blob.as_ref().map(|b| &b.state),
                Some(&BlobState::NotDownloaded { size: 4 })
            );
        }

        #[test]
        fn workdir_diff_picks_the_untracked_or_the_tracked_entry() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (gone, kept) = (blob(1), blob(2));
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            // `git rm --cached f` with `f` still on disk.
            let deleted = changed("f", FileChange::Deleted, DiffBody::Blob(gone.clone()));
            let untracked = changed("f", FileChange::Untracked, DiffBody::Blob(kept.clone()));
            answer(
                &mut f,
                req,
                ask,
                changes(vec![deleted.clone(), untracked.clone()]),
            );
            let shown = |f: &mut Fixture, untracked| {
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
                    path: "f".into(),
                    untracked,
                }));
                match view(f).body {
                    WorkdirBody::Diff { file, blob } => (file, blob.map(|b| b.id)),
                    other => panic!("expected a diff, got {other:?}"),
                }
            };
            assert_eq!(shown(&mut f, false), (Some(deleted), Some(gone.id)));
            assert_eq!(shown(&mut f, true), (Some(untracked), Some(kept.id)));
            // A path the changes do not list shows no file.
            f.dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
                path: "g".into(),
                untracked: false,
            }));
            assert_eq!(
                view(&f).body,
                WorkdirBody::Diff {
                    file: None,
                    blob: None
                }
            );
        }

        #[test]
        fn workdir_back_walks_every_pane_to_the_session() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            f.dispatch(Action::OpenWorkdir { session_id: id });
            let back = |f: &mut Fixture| {
                assert!(f.model.view().can_go_back);
                let effects = f.dispatch(Action::Back);
                (f.model.view().screen, effects)
            };
            let pane = |f: &mut Fixture| view(f).pane;
            let dir = |path: &str| WorkdirPane::Dir { path: path.into() };

            f.dispatch(Action::ShowWorkdir(WorkdirPane::File {
                path: "src/core/lib.rs".into(),
            }));
            back(&mut f);
            assert_eq!(pane(&mut f), dir("src/core"));
            back(&mut f);
            assert_eq!(pane(&mut f), dir("src"));
            back(&mut f);
            assert_eq!(pane(&mut f), dir(""));
            let (screen, effects) = back(&mut f);
            assert!(matches!(screen, Screen::Session(_)), "the session");
            assert_eq!(effects, [Effect::LoadSession(id)]);

            f.dispatch(Action::OpenWorkdir { session_id: id });
            f.dispatch(Action::ShowWorkdir(WorkdirPane::File {
                path: "top.rs".into(),
            }));
            back(&mut f);
            assert_eq!(
                pane(&mut f),
                dir(""),
                "a top-level file's parent is the root"
            );
            f.dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
                path: "a.rs".into(),
                untracked: true,
            }));
            back(&mut f);
            assert_eq!(pane(&mut f), WorkdirPane::Changes);
            let (screen, _) = back(&mut f);
            assert!(matches!(screen, Screen::Session(_)), "the session");
        }

        #[test]
        fn workdir_session_deleted_while_open_falls_back_to_the_list() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let other = f.session("/srv/b");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: other }));
            answer(&mut f, req, ask.clone(), changes(Vec::new()));
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            f.model.session_deleted(id, &[]);
            assert_eq!(shown(&f.model.view()), "active list");
            assert!(!f.model.workdir_awaited(req, &ask), "its read is forgotten");
            // Another session's reads stay.
            assert!(
                f.model
                    .workdirs
                    .stored
                    .contains_key(&WorkdirAsk::Changes { session: other })
            );
            assert!(
                f.dispatch(Action::OpenWorkdir { session_id: id })
                    .is_empty()
            );
            assert!(
                f.dispatch(Action::ShowWorkdir(WorkdirPane::Changes))
                    .is_empty()
            );
        }

        #[test]
        fn workdir_answers_survive_a_reload_and_a_reset_forgets_the_reads() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let (req, ask) = asked(f.dispatch(Action::OpenWorkdir { session_id: id }));
            answer(&mut f, req, ask.clone(), changes(Vec::new()));
            let (req, _) = asked(f.dispatch(Action::RefreshWorkdir));
            // A restart shows the stored answer and asks nothing.
            let mut model = Model::new(snapshot(&f.model), Some(paired_as(f.me)));
            assert!(
                model
                    .dispatch(Action::OpenWorkdir { session_id: id })
                    .is_empty()
            );
            let Screen::Workdir(shown) = model.view().screen else {
                panic!("expected the workdir screen");
            };
            assert_eq!(shown.fetch, WorkdirFetch::Idle);
            assert_eq!(shown.body, view(&f).body);
            f.model.replica_reset(empty_snapshot());
            assert!(!f.model.workdir_awaited(req, &ask), "an old server's read");
        }

        /// A detached core over a real replica with one session, playing
        /// the engine through its request channel.
        struct Live {
            _dir: tempfile::TempDir,
            core: Core,
            seam: Seam,
            requests: Receiver<Request>,
            session: SessionId,
        }

        impl Live {
            fn new() -> Live {
                let dir = tempfile::tempdir().unwrap();
                let core = Core::open_detached(dir.path()).unwrap();
                let seam = core.seam();
                let requests = seam.take_requests().unwrap();
                seam.pairing_succeeded(&"srv:7443".parse().unwrap(), credentials("CA"))
                    .unwrap();
                let session = SessionId::new();
                seam.apply_events(&EventBatch {
                    from: Seq(1),
                    events: vec![Event {
                        session_id: session,
                        at: Utc::now(),
                        command_id: None,
                        context: ContextId::first_of(session),
                        kind: EventKind::SessionCreated {
                            workdir: "/srv".into(),
                            context_name: "start".into(),
                            from: None,
                            worktree: None,
                        },
                    }],
                })
                .unwrap();
                let live = Live {
                    _dir: dir,
                    core,
                    seam,
                    requests,
                    session,
                };
                live.requests();
                live
            }

            fn requests(&self) -> Vec<Request> {
                self.requests.try_iter().collect()
            }

            /// Dispatches `action`; the one workdir read it asks for.
            fn asked(&self, action: Action) -> (u64, WorkdirAsk) {
                self.core.dispatch(action).unwrap();
                match &self.requests()[..] {
                    [Request::Workdir { req, ask }] => (*req, ask.clone()),
                    other => panic!("expected one workdir read, got {other:?}"),
                }
            }

            fn stored(&self) -> HashMap<WorkdirAsk, WorkdirAnswer> {
                crate::client::lock(&self.core.shared.state)
                    .store
                    .workdirs()
                    .unwrap()
            }

            fn view(&self) -> WorkdirView {
                match self.core.view().screen {
                    Screen::Workdir(view) => view,
                    other => panic!("expected the workdir screen, got {other:?}"),
                }
            }
        }

        fn credentials(ca: &str) -> PairedCredentials {
            PairedCredentials {
                device_id: DeviceId::new(),
                device_key_pem: "KEY".into(),
                device_cert_pem: "CERT".into(),
                ca_cert_pem: ca.into(),
            }
        }

        /// The reply to a delete is lost; the log settles it, and the open
        /// storage screen asks again all the same.
        #[test]
        fn a_blob_delete_the_log_settles_refreshes_an_open_storage_screen() {
            let live = Live::new();
            live.core
                .dispatch(Action::Navigate(Route::Storage))
                .unwrap();
            assert!(
                live.requests()
                    .iter()
                    .any(|r| matches!(r, Request::Storage { .. }))
            );
            let output = blob(10);
            live.core.dispatch(Action::DeleteBlob(output.id)).unwrap();
            live.requests();
            let outbox = crate::client::lock(&live.core.shared.state)
                .store
                .outbox()
                .unwrap();
            let delete = outbox.last().unwrap().command.id;

            let event = |command_id, kind| Event {
                session_id: live.session,
                at: Utc::now(),
                command_id,
                context: ContextId::first_of(live.session),
                kind,
            };
            live.seam
                .apply_events(&EventBatch {
                    from: Seq(2),
                    events: vec![
                        event(
                            None,
                            EventKind::ToolResult(ToolResult {
                                call: None,
                                is_error: false,
                                status: None,
                                preview: "big".into(),
                                lines: 1,
                                output: Some(Full::Blob(output.clone())),
                            }),
                        ),
                        event(
                            Some(delete),
                            EventKind::BlobDeleted {
                                blob: output.id,
                                holds: BlobUse::ToolOutput,
                                by: DeviceId::new(),
                            },
                        ),
                    ],
                })
                .unwrap();
            assert!(
                live.requests()
                    .iter()
                    .any(|r| matches!(r, Request::Storage { .. })),
                "the log-settled delete asks for the report again"
            );
        }

        #[test]
        fn workdir_a_transport_error_and_a_failed_keep_the_stored_snapshot() {
            let live = Live::new();
            let diff = blob(10);
            let set = changes(vec![changed(
                "a.rs",
                FileChange::Modified,
                DiffBody::Blob(diff.clone()),
            )]);
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            live.seam
                .workdir_answered(req, ask.clone(), Ok(set.clone()))
                .unwrap();
            let stored = live.view().body;

            let (req, _) = live.asked(Action::RefreshWorkdir);
            live.seam
                .workdir_answered(req, ask.clone(), Err(TransportError::Timeout))
                .unwrap();
            let shown = live.view();
            assert_eq!(
                shown.fetch,
                WorkdirFetch::Failed(TransportError::Timeout.to_string())
            );
            assert_eq!(shown.body, stored);

            let (req, _) = live.asked(Action::RefreshWorkdir);
            let failed = WorkdirAnswer::Changes(ChangesResult::Failed {
                reason: "git timed out".into(),
            });
            live.seam
                .workdir_answered(req, ask.clone(), Ok(failed))
                .unwrap();
            let shown = live.view();
            assert_eq!(shown.fetch, WorkdirFetch::Failed("git timed out".into()));
            assert_eq!(shown.body, stored);
            assert_eq!(live.stored()[&ask], set, "the store kept the good one");
            assert!(live.seam.blob(diff.id).unwrap().is_some(), "and its blob");
            assert!(live.requests().is_empty(), "nothing purged");
        }

        #[test]
        fn workdir_a_stale_answer_reaches_neither_model_nor_store() {
            let live = Live::new();
            let (old, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            let (new, _) = live.asked(Action::RefreshWorkdir);
            let newer = changes(vec![changed("new.rs", FileChange::Added, DiffBody::Binary)]);
            live.seam
                .workdir_answered(new, ask.clone(), Ok(newer.clone()))
                .unwrap();
            // The slow older answer arrives last.
            let stale = blob(3);
            let older = changes(vec![changed(
                "old.rs",
                FileChange::Added,
                DiffBody::Blob(stale.clone()),
            )]);
            live.seam
                .workdir_answered(old, ask.clone(), Ok(older))
                .unwrap();
            assert_eq!(live.stored()[&ask], newer);
            assert!(live.seam.blob(stale.id).unwrap().is_none(), "no blob row");
            let WorkdirAnswer::Changes(result) = newer else {
                unreachable!()
            };
            assert_eq!(live.view().body, WorkdirBody::Changes(result));
            assert_eq!(live.view().fetch, WorkdirFetch::Idle);
            // A stale failure does not show either.
            live.seam
                .workdir_answered(old, ask, Err(TransportError::Timeout))
                .unwrap();
            assert_eq!(live.view().fetch, WorkdirFetch::Idle);
        }

        #[test]
        fn workdir_an_available_blob_survives_a_refresh_with_the_same_id() {
            let live = Live::new();
            let content = blob(4);
            let answer = file("a.txt", FileBody::Blob(content.clone()));
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            assert_eq!(
                ask,
                WorkdirAsk::Changes {
                    session: live.session
                }
            );
            live.seam
                .workdir_answered(req, ask, Ok(changes(Vec::new())))
                .unwrap();
            let (req, ask) = live.asked(Action::ShowWorkdir(WorkdirPane::File {
                path: "a.txt".into(),
            }));
            live.seam
                .workdir_answered(req, ask.clone(), Ok(answer.clone()))
                .unwrap();
            assert_eq!(live.requests(), [Request::DownloadBlob(content.id)]);
            let path = PathBuf::from("/blobs/a");
            live.seam
                .blob_status(content.id, BlobStatus::Available { path: path.clone() })
                .unwrap();

            let (req, _) = live.asked(Action::RefreshWorkdir);
            live.seam.workdir_answered(req, ask, Ok(answer)).unwrap();
            assert!(live.requests().is_empty(), "nothing fetched nor purged");
            let WorkdirBody::File {
                blob: Some(shown), ..
            } = live.view().body
            else {
                panic!("expected the file");
            };
            assert_eq!(shown.state, BlobState::Available { path: path.clone() });
            assert_eq!(
                live.seam.blob(content.id).unwrap().unwrap().status,
                BlobStatus::Available { path }
            );
        }

        #[test]
        fn workdir_a_refresh_purges_what_nothing_names_any_more() {
            let live = Live::new();
            let (old, new) = (blob(3), blob(4));
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            let diff = |blob: &BlobRef| {
                changes(vec![changed(
                    "a.rs",
                    FileChange::Modified,
                    DiffBody::Blob(blob.clone()),
                )])
            };
            live.seam
                .workdir_answered(req, ask.clone(), Ok(diff(&old)))
                .unwrap();
            let dir = crate::blobs::blob_dir(&live.seam.data_dir());
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join(old.id.0.to_string());
            std::fs::write(&file, b"old").unwrap();

            let (req, _) = live.asked(Action::RefreshWorkdir);
            live.seam
                .workdir_answered(req, ask, Ok(diff(&new)))
                .unwrap();
            assert_eq!(live.requests(), [Request::BlobsPurged(vec![old.id])]);
            assert!(!file.exists(), "its file is gone");
            assert!(live.seam.blob(old.id).unwrap().is_none());
            // The model's blob map learned both in the same update.
            let state = crate::client::lock(&live.core.shared.state);
            assert!(!state.model.blobs.contains_key(&old.id));
            assert_eq!(
                state
                    .model
                    .blobs
                    .get(&new.id)
                    .map(|b| (b.workdir, b.status.clone())),
                Some((true, BlobStatus::NotDownloaded))
            );
        }

        /// Workdir refreshes purge and register blob ids without end; each
        /// purge drops the blob's lease, so the leases kept stay those of
        /// blobs in the replica, however long the run. A lease read before
        /// any of those purges still fails once the same id is back, and
        /// the current registration's lease still reports.
        #[test]
        fn workdir_refreshes_keep_only_the_leases_of_blobs_still_there() {
            let live = Live::new();
            let recurring = blob(3);
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            let diff = |blob: &BlobRef| {
                changes(vec![changed(
                    "a.rs",
                    FileChange::Modified,
                    DiffBody::Blob(blob.clone()),
                )])
            };
            live.seam
                .workdir_answered(req, ask.clone(), Ok(diff(&recurring)))
                .unwrap();
            let (_, first) = live.seam.leased_blob(recurring.id).unwrap().unwrap();
            let held = || crate::client::lock(&live.core.shared.state).leases_held();
            assert_eq!(held(), 1);

            for round in 0..50 {
                // Another blob, then the recurring id named again: each
                // answer purges the one before, and a download reads each.
                let other = blob(4);
                for current in [&other, &recurring] {
                    let (req, _) = live.asked(Action::RefreshWorkdir);
                    live.seam
                        .workdir_answered(req, ask.clone(), Ok(diff(current)))
                        .unwrap();
                    assert_eq!(live.requests().len(), 1, "one blob purged");
                    live.seam.leased_blob(current.id).unwrap().unwrap();
                    assert_eq!(held(), 1, "round {round}: only the blob shown has a lease");
                }
            }

            let downloading = BlobStatus::Downloading {
                got: 1,
                path: "/blobs/a.part".into(),
            };
            assert!(
                matches!(
                    live.seam
                        .leased_blob_status(recurring.id, first, downloading.clone()),
                    Err(crate::CoreError::Store(
                        crate::store::StoreError::UnknownBlob(_)
                    ))
                ),
                "the first registration's download is refused"
            );
            assert_eq!(
                live.seam.blob(recurring.id).unwrap().unwrap().status,
                BlobStatus::NotDownloaded
            );
            let (_, current) = live.seam.leased_blob(recurring.id).unwrap().unwrap();
            live.seam
                .leased_blob_status(recurring.id, current, downloading.clone())
                .unwrap();
            assert_eq!(
                live.seam.blob(recurring.id).unwrap().unwrap().status,
                downloading
            );
            // An id nothing registers gets no lease at all.
            assert!(live.seam.leased_blob(blob(5).id).unwrap().is_none());
            assert_eq!(held(), 1);
        }

        /// The tool outputs of the session on screen, in log order.
        fn outputs(view: crate::ViewState) -> Vec<FullView> {
            let Screen::Session(session) = view.screen else {
                panic!("expected the session screen");
            };
            session
                .timeline
                .into_iter()
                .filter_map(|item| match item {
                    TimelineItem::Logged {
                        entry: Entry::ToolResult { output, .. },
                        ..
                    } => output,
                    _ => None,
                })
                .collect()
        }

        /// A long message's whole text shows as its blob, live or imported,
        /// and as deleted once the server deleted it; a whole message has
        /// none.
        #[test]
        fn a_long_messages_blob_shows_and_its_delete_too() {
            let live = Live::new();
            let other = DeviceId::new();
            let (agent, imported) = (blob(40_000), blob(30_000));
            let event = |kind| Event {
                session_id: live.session,
                at: Utc::now(),
                command_id: None,
                context: ContextId::first_of(live.session),
                kind,
            };
            live.seam
                .apply_events(&EventBatch {
                    from: Seq(2),
                    events: vec![
                        event(EventKind::Imported {
                            at: None,
                            item: ImportedItem::UserMessage {
                                text: "log:".into(),
                                full: Some(Full::Blob(imported.clone())),
                            },
                        }),
                        event(EventKind::AgentMessage {
                            text: "# Report".into(),
                            parent: None,
                            full: Some(Full::Blob(agent.clone())),
                        }),
                        event(EventKind::AgentMessage {
                            text: "short".into(),
                            parent: None,
                            full: None,
                        }),
                    ],
                })
                .unwrap();
            live.core
                .dispatch(Action::OpenSession(live.session))
                .unwrap();
            let fulls = |view: crate::ViewState| -> Vec<Option<FullView>> {
                let Screen::Session(session) = view.screen else {
                    panic!("expected the session screen");
                };
                session
                    .timeline
                    .into_iter()
                    .filter_map(|item| match item {
                        TimelineItem::Logged {
                            entry:
                                Entry::AgentMessage { full, .. }
                                | Entry::Imported {
                                    entry: ImportedEntry::UserMessage { full, .. },
                                    ..
                                },
                            ..
                        } => Some(full),
                        _ => None,
                    })
                    .collect()
            };
            let shown = fulls(live.core.view());
            assert!(
                matches!(&shown[..], [
                    Some(FullView::Blob(i)),
                    Some(FullView::Blob(a)),
                    None,
                ] if i.id == imported.id && a.id == agent.id
                    && a.state == BlobState::NotDownloaded { size: 40_000 }),
                "{shown:?}"
            );

            live.seam
                .apply_events(&EventBatch {
                    from: Seq(5),
                    events: vec![event(EventKind::BlobDeleted {
                        blob: agent.id,
                        holds: BlobUse::Message,
                        by: other,
                    })],
                })
                .unwrap();
            let shown = fulls(live.core.view());
            assert!(
                matches!(&shown[1], Some(FullView::Deleted { by }) if by.id == other),
                "{shown:?}"
            );
            assert!(matches!(&shown[0], Some(FullView::Blob(_))), "{shown:?}");
        }

        /// A server delete or crop purges the old blob in the update that
        /// applies it, held or downloading: its row, its files and the
        /// model's record go, the queue hears of it, a download holding a
        /// lease on it fails its next report, and the views resolve the
        /// event's blob through the fate. The store and the model agree.
        #[test]
        fn blob_fates_purge_through_the_seam_and_show_in_the_views() {
            let live = Live::new();
            let me = crate::client::lock(&live.core.shared.state)
                .model
                .device()
                .unwrap();
            let other = DeviceId::new();
            let (kept, going) = (blob(400), blob(800));
            let result = |output: &BlobRef| ToolResult {
                call: None,
                is_error: false,
                status: None,
                preview: "out".into(),
                lines: 40,
                output: Some(Full::Blob(output.clone())),
            };
            let event = |kind| Event {
                session_id: live.session,
                at: Utc::now(),
                command_id: None,
                context: ContextId::first_of(live.session),
                kind,
            };
            live.seam
                .apply_events(&EventBatch {
                    from: Seq(2),
                    events: vec![
                        event(EventKind::ToolResult(result(&kept))),
                        event(EventKind::ToolResult(result(&going))),
                    ],
                })
                .unwrap();
            live.core
                .dispatch(Action::OpenSession(live.session))
                .unwrap();
            live.requests();

            // One copy is here, the other half downloaded under a lease.
            let dir = crate::blobs::blob_dir(&live.seam.data_dir());
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join(kept.id.0.to_string());
            let part = dir.join(format!("{}.part", going.id.0));
            std::fs::write(&file, b"kept").unwrap();
            std::fs::write(&part, b"go").unwrap();
            live.seam
                .blob_status(kept.id, BlobStatus::Available { path: file.clone() })
                .unwrap();
            let (_, lease) = live.seam.leased_blob(going.id).unwrap().unwrap();
            let downloading = BlobStatus::Downloading {
                got: 2,
                path: part.clone(),
            };
            live.seam
                .leased_blob_status(going.id, lease, downloading.clone())
                .unwrap();

            let into = blob(10);
            let applied = live
                .seam
                .apply_events(&EventBatch {
                    from: Seq(4),
                    events: vec![
                        event(EventKind::BlobDeleted {
                            blob: kept.id,
                            holds: BlobUse::ToolOutput,
                            by: other,
                        }),
                        event(EventKind::BlobCropped {
                            blob: going.id,
                            holds: BlobUse::ToolOutput,
                            into: into.clone(),
                            kept: vec![
                                LineSpan { first: 0, count: 2 },
                                LineSpan { first: 2, count: 3 },
                            ],
                            lines: 40,
                            by: me,
                        }),
                    ],
                })
                .unwrap();
            assert_eq!(applied.blobs, [into.id]);
            assert_eq!(
                live.requests(),
                [Request::BlobsPurged(vec![kept.id, going.id])]
            );
            assert!(!file.exists() && !part.exists(), "both files are gone");
            assert!(live.seam.blob(kept.id).unwrap().is_none());
            assert!(live.seam.blob(going.id).unwrap().is_none());
            assert!(
                live.seam
                    .leased_blob_status(going.id, lease, downloading)
                    .is_err(),
                "the download stops at its next report"
            );

            let shown = outputs(live.core.view());
            let FullView::Deleted { by } = &shown[0] else {
                panic!("expected the tombstone, got {:?}", shown[0]);
            };
            assert_eq!((by.id, by.me), (other, false));
            let FullView::Blob(cropped) = &shown[1] else {
                panic!("expected the cropped blob, got {:?}", shown[1]);
            };
            assert_eq!(cropped.id, into.id);
            assert_eq!(cropped.state, BlobState::NotDownloaded { size: 10 });
            let Some(BlobChange::Cropped {
                kept: lines,
                of,
                by,
            }) = &cropped.change
            else {
                panic!("expected the crop, got {cropped:?}");
            };
            assert_eq!((*lines, *of, by.me), (5, 40, true));

            // The model as rebuilt from the store shows the same.
            {
                let mut state = crate::client::lock(&live.core.shared.state);
                assert!(!state.model.blobs.contains_key(&kept.id));
                assert!(!state.model.blobs.contains_key(&going.id));
                state.resync();
            }
            assert_eq!(outputs(live.core.view()), shown);
        }

        /// A crop of a held blob keeps it for the local crop: the batch
        /// queues no fetch of what the crop makes, the views show the crop
        /// already, and the old blob goes when the crop ends. Deleting what
        /// a pending crop of a crop makes purges the whole chain. The
        /// store and the model agree at every step.
        #[test]
        fn blob_fates_a_held_blob_waits_for_its_local_crop() {
            use crate::store::PendingCrop;

            let live = Live::new();
            let by = DeviceId::new();
            let (one, two) = (blob(400), blob(800));
            let result = |output: &BlobRef| ToolResult {
                call: None,
                is_error: false,
                status: None,
                preview: "out".into(),
                lines: 40,
                output: Some(Full::Blob(output.clone())),
            };
            let event = |kind| Event {
                session_id: live.session,
                at: Utc::now(),
                command_id: None,
                context: ContextId::first_of(live.session),
                kind,
            };
            let crop = |blob: &BlobRef, into: &BlobRef| {
                event(EventKind::BlobCropped {
                    blob: blob.id,
                    holds: BlobUse::ToolOutput,
                    into: into.clone(),
                    kept: vec![LineSpan { first: 0, count: 2 }],
                    lines: 40,
                    by,
                })
            };
            live.seam
                .apply_events(&EventBatch {
                    from: Seq(2),
                    events: vec![
                        event(EventKind::ToolResult(result(&one))),
                        event(EventKind::ToolResult(result(&two))),
                    ],
                })
                .unwrap();
            live.core
                .dispatch(Action::OpenSession(live.session))
                .unwrap();
            let dir = crate::blobs::blob_dir(&live.seam.data_dir());
            std::fs::create_dir_all(&dir).unwrap();
            let file = |blob: &BlobRef| dir.join(blob.id.0.to_string());
            for held in [&one, &two] {
                std::fs::write(file(held), b"held").unwrap();
                live.seam
                    .blob_status(held.id, BlobStatus::Available { path: file(held) })
                    .unwrap();
            }
            live.requests();
            // What the model holds, and that the store agrees.
            let agreed = || {
                let mut state = crate::client::lock(&live.core.shared.state);
                let mut ids: Vec<_> = state.model.blobs.keys().copied().collect();
                state.resync();
                let mut again: Vec<_> = state.model.blobs.keys().copied().collect();
                ids.sort();
                again.sort();
                assert_eq!(ids, again, "the store and the model agree");
                ids
            };

            // `one` cropped, `two` cropped twice, in one batch.
            let (one_into, middle, last) = (blob(10), blob(20), blob(5));
            let applied = live
                .seam
                .apply_events(&EventBatch {
                    from: Seq(4),
                    events: vec![
                        crop(&one, &one_into),
                        crop(&two, &middle),
                        crop(&middle, &last),
                    ],
                })
                .unwrap();
            let pending = [
                PendingCrop {
                    into: one_into.id,
                    from: one.id,
                },
                PendingCrop {
                    into: middle.id,
                    from: two.id,
                },
                PendingCrop {
                    into: last.id,
                    from: middle.id,
                },
            ];
            assert_eq!(applied.local_crops, pending);
            assert!(applied.blobs.is_empty(), "nothing to fetch");
            assert!(applied.purged.is_empty());
            assert!(live.requests().is_empty());
            assert_eq!(live.seam.pending_crops().unwrap(), pending);
            let mut held = vec![one.id, two.id, one_into.id, middle.id, last.id];
            held.sort();
            assert_eq!(agreed(), held);
            // The views show the crops already, never the held content.
            let shown: Vec<_> = outputs(live.core.view())
                .iter()
                .map(|output| output.blob().map(|b| (b.id, b.state.clone())))
                .collect();
            assert_eq!(
                shown,
                [
                    Some((one_into.id, BlobState::NotDownloaded { size: 10 })),
                    Some((last.id, BlobState::NotDownloaded { size: 5 })),
                ]
            );

            // `one`'s crop ends: `one` goes as a server purge does, once.
            live.seam.local_crop_ended(pending[0]).unwrap();
            assert_eq!(live.requests(), [Request::BlobsPurged(vec![one.id])]);
            assert!(!file(&one).exists());
            assert!(live.seam.blob(one.id).unwrap().is_none());
            live.seam.local_crop_ended(pending[0]).unwrap();
            assert!(live.requests().is_empty(), "no longer pending");
            assert_eq!(live.seam.pending_crops().unwrap(), pending[1..]);

            // The server deletes the end of the chain before it is made:
            // every blob that would have made it goes too.
            let applied = live
                .seam
                .apply_events(&EventBatch {
                    from: Seq(7),
                    events: vec![event(EventKind::BlobDeleted {
                        blob: last.id,
                        holds: BlobUse::ToolOutput,
                        by,
                    })],
                })
                .unwrap();
            assert_eq!(applied.purged, [last.id, middle.id, two.id]);
            assert!(applied.local_crops.is_empty());
            assert_eq!(
                live.requests(),
                [Request::BlobsPurged(vec![last.id, middle.id, two.id])]
            );
            assert!(!file(&two).exists());
            assert!(live.seam.pending_crops().unwrap().is_empty());
            assert_eq!(agreed(), [one_into.id]);
            assert!(matches!(
                outputs(live.core.view())[1],
                FullView::Deleted { .. }
            ));
        }

        #[test]
        fn workdir_an_answer_for_a_session_gone_writes_no_row() {
            // Deleted from this device while the read was in flight.
            let live = Live::new();
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            live.seam
                .apply_events(&EventBatch {
                    from: Seq(2),
                    events: vec![Event {
                        session_id: live.session,
                        at: Utc::now(),
                        command_id: Some(CommandId::new()),
                        context: ContextId::first_of(live.session),
                        kind: EventKind::SessionArchived {
                            archived: true,
                            by: DeviceId::new(),
                        },
                    }],
                })
                .unwrap();
            live.core
                .dispatch(Action::DeleteSessionLocally {
                    session_id: live.session,
                })
                .unwrap();
            let content = blob(4);
            live.seam
                .workdir_answered(
                    req,
                    ask,
                    Ok(changes(vec![changed(
                        "a",
                        FileChange::Added,
                        DiffBody::Blob(content.clone()),
                    )])),
                )
                .unwrap();
            assert!(live.stored().is_empty());
            assert!(live.seam.blob(content.id).unwrap().is_none());

            // Re-paired with another server while the read was in flight.
            let live = Live::new();
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            live.seam
                .pairing_succeeded(&"other:7443".parse().unwrap(), credentials("ANOTHER CA"))
                .unwrap();
            live.seam
                .workdir_answered(req, ask, Ok(changes(Vec::new())))
                .unwrap();
            assert!(live.stored().is_empty());
        }

        #[test]
        fn workdir_eager_never_queues_a_workdir_blob_but_opening_its_pane_wants_it() {
            let live = Live::new();
            live.core
                .dispatch(Action::SetBlobPolicy(BlobPolicy::Eager))
                .unwrap();
            live.requests();
            let diff = blob(10);
            let (req, ask) = live.asked(Action::OpenWorkdir {
                session_id: live.session,
            });
            live.seam
                .workdir_answered(
                    req,
                    ask,
                    Ok(changes(vec![changed(
                        "a.rs",
                        FileChange::Modified,
                        DiffBody::Blob(diff.clone()),
                    )])),
                )
                .unwrap();
            assert!(live.requests().is_empty(), "no download asked for");
            // What the engine's eager queue reads: marked, so skipped.
            let record = live.seam.blob(diff.id).unwrap().unwrap();
            assert!(record.workdir);
            live.core
                .dispatch(Action::ShowWorkdir(WorkdirPane::Diff {
                    path: "a.rs".into(),
                    untracked: false,
                }))
                .unwrap();
            assert_eq!(live.requests(), [Request::DownloadBlob(diff.id)]);
        }

        /// A tool output blob is no workdir blob, so the eager policy
        /// still has one to download beside them.
        #[test]
        fn workdir_blobs_and_event_blobs_stay_apart_in_the_blob_map() {
            let mut f = Fixture::paired();
            let id = f.session("/srv/a");
            let output = blob(7);
            f.log(
                id,
                vec![EventKind::ToolResult(ToolResult {
                    call: None,
                    is_error: false,
                    status: None,
                    preview: "out".into(),
                    lines: 1,
                    output: Some(Full::Blob(output.clone())),
                })],
            );
            let content = blob(4);
            f.dispatch(Action::OpenWorkdir { session_id: id });
            let (req, ask) =
                asked(f.dispatch(Action::ShowWorkdir(WorkdirPane::File { path: "a".into() })));
            answer(&mut f, req, ask, file("a", FileBody::Blob(content.clone())));
            assert!(!f.model.blobs[&output.id].workdir);
            assert!(f.model.blobs[&content.id].workdir);
        }

        mod crop {
            use super::*;

            /// A tool output in the live session, opened on its crop page.
            fn opened(live: &Live, output: &BlobRef) {
                live.seam
                    .apply_events(&EventBatch {
                        from: Seq(2),
                        events: vec![Event {
                            session_id: live.session,
                            at: Utc::now(),
                            command_id: None,
                            context: ContextId::first_of(live.session),
                            kind: EventKind::ToolResult(ToolResult {
                                call: None,
                                is_error: false,
                                status: None,
                                preview: "out".into(),
                                lines: 3,
                                output: Some(Full::Blob(output.clone())),
                            }),
                        }],
                    })
                    .unwrap();
                live.core
                    .dispatch(Action::OpenSession(live.session))
                    .unwrap();
                live.core
                    .dispatch(Action::OpenCrop {
                        blob: output.id,
                        session: live.session,
                        holds: BlobUse::ToolOutput,
                        title: "Bash ls".into(),
                    })
                    .unwrap();
            }

            fn page(live: &Live) -> CropView {
                match live.core.view().screen {
                    Screen::Crop(view) => view,
                    other => panic!("expected the crop page, got {other:?}"),
                }
            }

            /// Writes `content` as the blob's downloaded file.
            fn held(live: &Live, output: &BlobRef, content: &[u8]) -> PathBuf {
                let dir = crate::blobs::blob_dir(&live.seam.data_dir());
                std::fs::create_dir_all(&dir).unwrap();
                let path = dir.join(output.id.0.to_string());
                std::fs::write(&path, content).unwrap();
                live.seam
                    .blob_status(output.id, BlobStatus::Available { path: path.clone() })
                    .unwrap();
                path
            }

            /// The one line count the last change asked for.
            fn count_asked(live: &Live, path: &std::path::Path) -> u64 {
                match &live.requests()[..] {
                    [Request::CountLines { req, path: asked }] if asked == path => *req,
                    other => panic!("expected one line count of {path:?}, got {other:?}"),
                }
            }

            /// Opening the page under `OnDemand` downloads nothing and says
            /// the content isn't here; the download it offers shows its
            /// progress there, and the content once held is counted once,
            /// off the model, with a stale count dropped.
            #[test]
            fn crop_page_downloads_nothing_then_counts_what_it_downloads() {
                let live = Live::new();
                live.core
                    .dispatch(Action::SetBlobPolicy(BlobPolicy::OnDemand))
                    .unwrap();
                let output = blob(16);
                opened(&live, &output);
                assert!(
                    !live.requests().iter().any(|r| matches!(
                        r,
                        Request::DownloadBlob(_) | Request::CountLines { .. }
                    )),
                    "opening asks for nothing"
                );
                let shown = page(&live);
                assert_eq!(
                    (shown.blob, shown.session, shown.holds, shown.title.as_str()),
                    (output.id, live.session, BlobUse::ToolOutput, "Bash ls")
                );
                assert_eq!(
                    shown.content,
                    CropContent::NotHere(BlobState::NotDownloaded { size: 16 })
                );
                assert!(shown.keep.is_empty());

                live.core.dispatch(Action::DownloadBlob(output.id)).unwrap();
                assert_eq!(live.requests(), [Request::DownloadBlob(output.id)]);
                assert_eq!(
                    page(&live).content,
                    CropContent::NotHere(BlobState::Requested { size: 16 })
                );
                live.seam
                    .blob_status(
                        output.id,
                        BlobStatus::Downloading {
                            got: 4,
                            path: live.seam.data_dir().join("part"),
                        },
                    )
                    .unwrap();
                assert!(matches!(
                    page(&live).content,
                    CropContent::NotHere(BlobState::Downloading {
                        got: 4,
                        size: 16,
                        ..
                    })
                ));
                assert!(live.requests().is_empty(), "nothing to count yet");

                let content = b"one\ntwo\nthree\n";
                let path = held(&live, &output, content);
                let req = count_asked(&live, &path);
                assert_eq!(page(&live).content, CropContent::Counting);
                live.seam.lines_counted(req + 1, Ok(99));
                assert_eq!(page(&live).content, CropContent::Counting, "a stale count");
                let counted = skywave_proto::crop::count(std::fs::File::open(&path).unwrap());
                live.seam.lines_counted(req, counted);
                assert_eq!(
                    page(&live).content,
                    CropContent::Ready {
                        path: path.clone(),
                        lines: 3
                    }
                );
                live.core
                    .dispatch(Action::SetCropKeep(vec![LineSpan { first: 0, count: 1 }]))
                    .unwrap();
                assert!(live.requests().is_empty(), "counted once");
                assert_eq!(page(&live).keep, [LineSpan { first: 0, count: 1 }]);

                // Back to the item it was opened from.
                live.core.dispatch(Action::Back).unwrap();
                assert!(matches!(live.core.view().screen, Screen::Session(_)));
            }

            /// A delete from another device while the page is open leaves
            /// nothing to crop; opened from the storage screen, back
            /// returns there.
            #[test]
            fn crop_page_is_gone_once_its_blob_is_deleted_elsewhere() {
                let live = Live::new();
                let output = blob(16);
                opened(&live, &output);
                // `opened` passes through the session; open from storage.
                live.core
                    .dispatch(Action::Navigate(Route::Storage))
                    .unwrap();
                live.core
                    .dispatch(Action::OpenCrop {
                        blob: output.id,
                        session: live.session,
                        holds: BlobUse::ToolOutput,
                        title: "Bash ls".into(),
                    })
                    .unwrap();
                live.requests();
                let path = held(&live, &output, b"a\nb");
                let req = count_asked(&live, &path);
                live.seam.lines_counted(req, Ok(2));
                assert_eq!(page(&live).content, CropContent::Ready { path, lines: 2 });

                live.seam
                    .apply_events(&EventBatch {
                        from: Seq(3),
                        events: vec![Event {
                            session_id: live.session,
                            at: Utc::now(),
                            command_id: None,
                            context: ContextId::first_of(live.session),
                            kind: EventKind::BlobDeleted {
                                blob: output.id,
                                holds: BlobUse::ToolOutput,
                                by: DeviceId::new(),
                            },
                        }],
                    })
                    .unwrap();
                assert_eq!(page(&live).content, CropContent::Gone);

                live.core.dispatch(Action::Back).unwrap();
                assert!(matches!(live.core.view().screen, Screen::Storage(_)));
            }

            /// A crop of the page's blob, once queued, leaves the page as
            /// back does: to the session, or to the storage screen. A crop
            /// of another blob leaves it open.
            #[test]
            fn crop_page_is_left_once_its_crop_is_queued() {
                let keep = vec![LineSpan { first: 0, count: 1 }];
                let queued = |live: &Live| {
                    live.seam
                        .pending_outbox()
                        .unwrap()
                        .iter()
                        .filter(|e| matches!(e.command.kind, CommandKind::CropBlob { .. }))
                        .count()
                };

                let live = Live::new();
                let output = blob(16);
                opened(&live, &output);
                live.core
                    .dispatch(Action::CropBlob {
                        blob: BlobId::new(),
                        keep: keep.clone(),
                    })
                    .unwrap();
                assert_eq!(queued(&live), 1);
                page(&live);
                live.core
                    .dispatch(Action::CropBlob {
                        blob: output.id,
                        keep: keep.clone(),
                    })
                    .unwrap();
                assert_eq!(queued(&live), 2);
                assert!(matches!(
                    live.core.view().screen,
                    Screen::Session(view) if view.id == live.session
                ));

                // Opened from storage, back there, which asks again.
                live.core
                    .dispatch(Action::Navigate(Route::Storage))
                    .unwrap();
                live.core
                    .dispatch(Action::OpenCrop {
                        blob: output.id,
                        session: live.session,
                        holds: BlobUse::ToolOutput,
                        title: "Bash ls".into(),
                    })
                    .unwrap();
                live.requests();
                live.core
                    .dispatch(Action::CropBlob {
                        blob: output.id,
                        keep,
                    })
                    .unwrap();
                assert!(matches!(live.core.view().screen, Screen::Storage(_)));
                assert!(
                    live.requests()
                        .iter()
                        .any(|r| matches!(r, Request::Storage { .. })),
                    "the storage screen asks again"
                );
            }
        }
    }
}
