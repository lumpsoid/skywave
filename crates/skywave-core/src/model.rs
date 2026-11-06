//! The pure client state machine.
//!
//! [`Model`] mirrors the local replica in memory plus the ephemeral state
//! (link, pairing progress, current screen). Actions and engine
//! notifications fold into it without I/O; actions return the [`Effect`]s
//! the [`Core`](crate::Core) must carry out, persistence first. The
//! [`ViewState`] is a pure function of the model.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use skywave_proto::{
    BackgroundTask, BlobRef, Command, CommandKind, CommandResult, ConversationsResult, DeviceInfo,
    DirsResult, Event, EventBatch, EventKind, Fingerprint, Full, ImportedItem, RejectReason,
    RuleSuggestion, SessionOrigin, SessionState as LogState, TaskId, ToolCall, ToolKind,
    ToolResult,
};

use crate::action::{Action, Route};
use crate::address::ServerAddress;
use crate::completion::Completer;
use crate::conversations::Conversations;
use crate::identity::Identity;
use crate::projection::{
    BlobFate, FIRST_PROMPT_MAX_CHARS, SessionSummary, TurnRecord, blob_refs, first_line, is_notable,
};
use crate::seam::Request;
use crate::snippets::Snippets;
use crate::storage::Storage;
use crate::store::WorkdirStored;
use crate::store::{
    BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxStatus, ReadMark, Settings, Snapshot,
};
use crate::tls;
use crate::transport::TransportError;
use crate::view::{
    AgentSetting, AgentView, Answer, ApprovalId, ApprovalPrompt, ApprovalStatus, Attention,
    BlobChange, BlobId, BlobState, BlobView, BodyView, CallId, ChangedFile, ChangesResult,
    CommandId, ComposerMode, ContextId, ContextView, Decision, Delivery, DeviceId, DeviceRef,
    DiffBody, Draft, Entry, ErrorInfo, FailReason, FileBody, FileChange, FileResult, FullView,
    ImportedEntry, Link, LinkRejection, ListKind, ModelChoice, Outgoing, PairedInfo, PairingStatus,
    PairingView, PendingApprovalRef, PendingSession, RunWhen, SHELL_ALERT_AFTER, Screen, Seq,
    SessionId, SessionListView, SessionRef, SessionRow, SessionState, SessionView, SettingChange,
    SettingsView, ShellAlert, ShellRun, ShellRunState, ShellView, Snippet, SnippetId,
    StorageReport, StorageView, SubagentHeader, SubagentStatus, TimelineItem, ToolApproval,
    ToolOutcome, ToolView, UsageSession, UsageView, ViewState, WorkdirBody, WorkdirPane,
    WorkdirView,
};
use crate::workdir::{WorkdirAnswer, WorkdirAsk, Workdirs, parent};

/// Work the core must do for an action, in order. Persistence effects come
/// before the requests that make the action visible to the network.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Effect {
    /// Write the command to the outbox.
    Enqueue(Command),
    SaveBlobPolicy(BlobPolicy),
    SaveServerAddress(String),
    /// Keep the session's draft in that mode; `None` deletes it.
    SaveDraft((SessionId, ComposerMode), Option<Draft>),
    SaveComposerMode(SessionId, ComposerMode),
    SaveReadMark(SessionId, ReadMark),
    SaveSnippet(Snippet),
    DeleteSnippet(SnippetId),
    /// Take the command out of the outbox and, if given, keep the draft, as
    /// one write: the text is never lost nor kept twice.
    Withdraw {
        id: CommandId,
        draft: Option<((SessionId, ComposerMode), Draft)>,
    },
    /// Read the session's events from the replica into the model.
    LoadSession(SessionId),
    /// Delete the session from this device's replica, then from the model
    /// ([`Model::session_deleted`]), and remove its blobs' files.
    DeleteLocally(SessionId),
    /// Clear the replica after the server's log lost events it holds
    /// ([`Store::reset_for_lost_history`](crate::Store::reset_for_lost_history)),
    /// then the model ([`Model::replica_reset`]), and remove the blob files.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    SessionList,
    Archived,
    Settings,
    Pairing,
    Usage,
    Storage,
    Session(SessionId),
    /// The session's workdir, on [`Model::workdir_pane`].
    Workdir(SessionId),
}

impl From<Route> for Place {
    fn from(route: Route) -> Self {
        match route {
            Route::SessionList => Place::SessionList,
            Route::ArchivedSessions => Place::Archived,
            Route::Settings => Place::Settings,
            Route::Pairing => Place::Pairing,
            Route::Usage => Place::Usage,
            Route::Storage => Place::Storage,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct OpenSession {
    id: SessionId,
    events: Vec<(Seq, Event)>,
    /// The first event after the read mark as it was when the session
    /// opened, past this device's own messages, if agent output follows
    /// it. Marking read leaves it alone.
    unread_from: Option<Seq>,
}

/// Who this device is paired as, taken from its identity when it is
/// loaded or saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Paired {
    pub(crate) device: DeviceId,
    /// The pinned server CA's fingerprint; `None` if its PEM holds no
    /// certificate.
    pub(crate) server: Option<Fingerprint>,
}

impl Paired {
    pub(crate) fn of(identity: &impl Identity) -> Paired {
        Paired {
            device: identity.device_id(),
            server: tls::first_cert_der(identity.ca_cert_pem()).map(|ca| Fingerprint::of_der(&ca)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    /// Who this device is paired as; `None` while unpaired.
    paired: Option<Paired>,
    pairing: PairingStatus,
    link: Link,
    place: Place,
    settings: Settings,
    cursor: Seq,
    /// Most recently active (highest `last_seq`) first, like the store.
    sessions: Vec<SessionSummary>,
    /// Oldest first, finished entries included.
    outbox: Vec<OutboxEntry>,
    blobs: BTreeMap<BlobId, BlobRecord>,
    /// What the server did to blobs, as stored: an event's blob resolves
    /// through it ([`Model::full_view`]).
    fates: HashMap<BlobId, BlobFate>,
    /// Blobs asked for and not yet taken by a download; in memory only, as
    /// the blob queue forgets un-started requests on a restart too.
    wanted: HashSet<BlobId>,
    /// Why the last download of a blob was given up; in memory only.
    failed: HashMap<BlobId, String>,
    /// This device's unsent messages and shell commands, as stored.
    drafts: HashMap<(SessionId, ComposerMode), Draft>,
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
    /// Sessions deleted from this device, as stored: their events are
    /// skipped for good.
    deleted: HashSet<SessionId>,
    /// Events of the session on screen, loaded from the replica.
    open: Option<OpenSession>,
    /// The context [`Action::ViewContext`] narrowed the session on screen
    /// to; gone once another screen shows. In memory only.
    viewing: Option<(SessionId, ContextId)>,
    /// The `Agent` call whose subagent [`Action::ViewSubagent`] shows in
    /// the session on screen; gone once another screen shows. In memory
    /// only: [`Action::Back`] reads where to step out to from the log.
    subagent: Option<(SessionId, CallId)>,
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
    /// Workdir completion in the new-session prompt; in memory only.
    completer: Completer,
    /// The conversation picker on the session list; in memory only.
    conversations: Conversations,
    /// The storage screen's report; in memory only.
    storage: Storage,
    /// A [`Action::CreateSessionLike`] or [`Action::MoveContext`] whose
    /// new session is not in the replica yet: its command, and the session
    /// it was asked from, still on screen. In memory only.
    opening: Option<(CommandId, SessionId)>,
    /// The server's devices by name, as stored.
    devices: HashMap<DeviceId, String>,
    /// The models of the newest `ModelsOffered`, as stored.
    models: Vec<ModelChoice>,
    /// Every turn's usage in the replica, in log order: what the usage
    /// screen totals.
    turns: Vec<TurnRecord>,
    /// What this device read of sessions' workdirs: as stored, plus the
    /// reads in flight and failed (in memory only).
    workdirs: Workdirs,
    /// The pane the workdir screen shows; reset by [`Action::OpenWorkdir`].
    workdir_pane: WorkdirPane,
    /// The last directory the workdir screen showed, where its files pane
    /// opens; reset by [`Action::OpenWorkdir`].
    last_dir: String,
}

impl Model {
    fn device(&self) -> Option<DeviceId> {
        self.paired.map(|p| p.device)
    }

    /// The model after a (cold) start: the replica as stored, `paired` from
    /// the identity if the device is paired.
    pub(crate) fn new(snapshot: Snapshot, paired: Option<Paired>) -> Model {
        let mut model = Model {
            paired,
            pairing: PairingStatus::Idle,
            link: match paired {
                Some(_) => Link::Connecting,
                None => Link::Unpaired,
            },
            place: Place::SessionList,
            settings: Settings::default(),
            cursor: Seq::ZERO,
            sessions: Vec::new(),
            outbox: Vec::new(),
            blobs: BTreeMap::new(),
            fates: HashMap::new(),
            wanted: HashSet::new(),
            failed: HashMap::new(),
            drafts: HashMap::new(),
            composer_modes: HashMap::new(),
            draft_rev: 0,
            read_marks: HashMap::new(),
            snippets: Snippets::default(),
            deleted: HashSet::new(),
            open: None,
            viewing: None,
            subagent: None,
            alerted: HashSet::new(),
            alerts: 0,
            alerted_at: HashMap::new(),
            shell_alerts: 0,
            last_shell: None,
            completer: Completer::default(),
            conversations: Conversations::default(),
            storage: Storage::default(),
            opening: None,
            devices: HashMap::new(),
            models: Vec::new(),
            turns: Vec::new(),
            workdirs: Workdirs::default(),
            workdir_pane: WorkdirPane::Changes,
            last_dir: String::new(),
        };
        // An approval pending at start that no process alerted on yet (one
        // that came in during a catch-up cut short) alerts once online.
        model.reload(snapshot);
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
        self.composer_modes = snapshot.composer_modes;
        self.read_marks = snapshot.read_marks;
        self.snippets = Snippets::new(snapshot.snippets);
        self.deleted = snapshot.deleted;
        self.devices = snapshot.devices;
        self.models = snapshot.models;
        self.turns = snapshot.turns;
        self.workdirs.stored = snapshot.workdirs;
        self.open = None;
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
        match self.place {
            Place::Session(id) => Some(id),
            _ => None,
        }
    }

    /// The session on screen when its events are not loaded: one
    /// [`Model::apply_batch`] opened for a [`Action::CreateSessionLike`].
    pub(crate) fn unloaded_session(&self) -> Option<SessionId> {
        self.open_session()
            .filter(|id| self.open.as_ref().is_none_or(|open| open.id != *id))
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
                let address = address.trim();
                if self.device().is_none()
                    || self.settings.server_address.as_deref() == Some(address)
                    || ServerAddress::parse(address).is_err()
                {
                    return Vec::new();
                }
                self.settings.server_address = Some(address.to_owned());
                self.link = Link::Connecting;
                vec![
                    Effect::SaveServerAddress(address.to_owned()),
                    Effect::Request(Request::Connect),
                ]
            }
            Action::CreateSession {
                workdir,
                conversation,
            } => {
                let workdir = workdir.trim();
                if workdir.is_empty() {
                    return Vec::new();
                }
                self.completer.end();
                self.conversations.end();
                // No local id check: dispatch has no failure channel, and a
                // silent drop would lose what the user typed. The server's
                // `BadConversation` shows as a failed pending session.
                let conversation = conversation
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned);
                self.enqueue(CommandKind::CreateSession {
                    workdir: workdir.to_owned(),
                    conversation,
                })
            }
            Action::CreateSessionLike(from) => {
                let Some(workdir) = self
                    .summary(from)
                    .and_then(|s| s.workdir.clone())
                    .filter(|_| self.place == Place::Session(from))
                else {
                    return Vec::new();
                };
                let effects = self.enqueue(CommandKind::CreateSession {
                    workdir,
                    conversation: None,
                });
                if let Some(Effect::Enqueue(command)) = effects.first() {
                    self.opening = Some((command.id, from));
                }
                effects
            }
            Action::CompleteWorkdir { input } => {
                if self.device().is_none() || self.place != Place::SessionList {
                    return Vec::new();
                }
                // While the link is down, recent workdirs stand in.
                let online = self.online();
                self.completer
                    .complete(input, online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::EndCompletion => {
                self.completer.end();
                Vec::new()
            }
            Action::ListConversations => {
                if self.device().is_none() || self.place != Place::SessionList {
                    return Vec::new();
                }
                let online = self.online();
                self.conversations
                    .list(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::EndConversations => {
                self.conversations.end();
                Vec::new()
            }
            Action::RefreshStorage => {
                if self.device().is_none() || self.place != Place::Storage {
                    return Vec::new();
                }
                let online = self.online();
                self.storage
                    .refresh(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::SendMessage { session_id, text } => {
                let Some(summary) = self.summary(session_id) else {
                    return Vec::new();
                };
                if text.trim().is_empty() {
                    return Vec::new();
                }
                // The message goes where this device's own clear or switch
                // on its way leads, so it is not refused for that.
                let context = self.target_context(summary);
                let effects = self.enqueue(CommandKind::SendMessage {
                    session_id,
                    context,
                    text,
                });
                // The store drops the draft with the enqueue.
                if !effects.is_empty() {
                    self.drafts.remove(&(session_id, ComposerMode::Agent));
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
                let effects = self.enqueue(CommandKind::RunShell {
                    session_id,
                    command,
                    when,
                });
                // The store drops the shell draft with the enqueue.
                if !effects.is_empty() {
                    self.drafts.remove(&(session_id, ComposerMode::Shell));
                }
                effects
            }
            Action::StopShell { session_id, run } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::StopShell { session_id, run })
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
            Action::ResolveApproval { id, decision, rule } => {
                let granted = |rule| decision == Decision::Approve && self.offers(id, rule);
                if !self.can_answer(id) || rule.is_some_and(|rule| !granted(rule)) {
                    return Vec::new();
                }
                self.enqueue(CommandKind::ResolveApproval { id, decision, rule })
            }
            Action::Interrupt { session_id } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::Interrupt { session_id })
            }
            Action::StopSession { session_id } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::StopSession { session_id })
            }
            Action::ClearContext { session_id } => self.clear_context(session_id),
            Action::SwitchContext {
                session_id,
                context,
            } => {
                if self.summary(session_id).is_none() || self.moved(session_id, context) {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SwitchContext {
                    session_id,
                    context,
                })
            }
            Action::MoveContext {
                session_id,
                context,
            } => self.move_context(session_id, context),
            Action::ViewContext {
                session_id,
                context,
            } => {
                if self.place == Place::Session(session_id) {
                    self.viewing = context.map(|context| (session_id, context));
                }
                Vec::new()
            }
            Action::ViewSubagent { session_id, call } => {
                if self.place != Place::Session(session_id) {
                    return Vec::new();
                }
                match call {
                    None => self.subagent = None,
                    Some(call) if self.subagent_header(session_id, call).is_some() => {
                        self.subagent = Some((session_id, call));
                    }
                    Some(_) => {}
                }
                Vec::new()
            }
            Action::RenameSession { session_id, name } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                // As the server will log it, so the title shows it unchanged.
                let name = skywave_proto::normalise_session_name(&name);
                self.enqueue(CommandKind::RenameSession { session_id, name })
            }
            Action::SetSessionArchived {
                session_id,
                archived,
            } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetSessionArchived {
                    session_id,
                    archived,
                })
            }
            Action::SetPermissionMode { session_id, mode } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetPermissionMode { session_id, mode })
            }
            Action::StopTask { session_id, task } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::StopTask { session_id, task })
            }
            Action::SetModel { session_id, model } => {
                // As the server will log it, so the switch shows it unchanged.
                let Some(model) = skywave_proto::normalise_model(&model) else {
                    return Vec::new();
                };
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetModel { session_id, model })
            }
            Action::SetEffort { session_id, effort } => {
                if self.summary(session_id).is_none() {
                    return Vec::new();
                }
                self.enqueue(CommandKind::SetEffort { session_id, effort })
            }
            Action::DeleteSessionLocally { session_id } => {
                if !self.summary(session_id).is_some_and(|s| self.deletable(s)) {
                    return Vec::new();
                }
                vec![Effect::DeleteLocally(session_id)]
            }
            Action::OpenSession(id) => {
                if self.device().is_none() || self.summary(id).is_none() {
                    return Vec::new();
                }
                self.place = Place::Session(id);
                self.open = None;
                self.viewing = None;
                self.subagent = None;
                self.completer.end();
                self.conversations.end();
                vec![Effect::LoadSession(id)]
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
                self.place = Place::Workdir(session_id);
                self.open = None;
                self.viewing = None;
                self.subagent = None;
                self.completer.end();
                self.conversations.end();
                self.last_dir = String::new();
                self.show_workdir(session_id, WorkdirPane::Changes)
            }
            Action::ShowWorkdir(pane) => match self.shown_workdir() {
                Some(id) => self.show_workdir(id, pane),
                None => Vec::new(),
            },
            Action::RefreshWorkdir => {
                let Some(id) = self.shown_workdir() else {
                    return Vec::new();
                };
                let ask = pane_ask(id, &self.workdir_pane);
                let online = self.online();
                self.workdirs
                    .ask(ask, online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::DownloadBlob(id) => match self.blobs.get(&id).map(|b| &b.status) {
                None | Some(BlobStatus::Available { .. }) => Vec::new(),
                Some(status) => {
                    // A download already running shows its progress instead.
                    if *status == BlobStatus::NotDownloaded {
                        self.wanted.insert(id);
                    }
                    self.failed.remove(&id);
                    vec![Effect::Request(Request::DownloadBlob(id))]
                }
            },
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
            Action::RetryNow => match self.link {
                Link::Reconnecting { .. } => vec![Effect::Request(Request::RetryNow)],
                // The server may have been updated (or its certificate
                // restored) since: ask it again.
                Link::Rejected(rejection) if rejection.can_retry() => {
                    self.link = Link::Connecting;
                    vec![Effect::Request(Request::Connect)]
                }
                _ => Vec::new(),
            },
            Action::ResetReplica => match self.link {
                Link::Rejected(LinkRejection::HistoryLost { .. }) => {
                    self.place = Place::SessionList;
                    self.open = None;
                    self.viewing = None;
                    self.subagent = None;
                    self.link = Link::Connecting;
                    vec![Effect::ResetReplica, Effect::Request(Request::Connect)]
                }
                _ => Vec::new(),
            },
            Action::Navigate(route) => {
                self.place = route.into();
                self.open = None;
                self.viewing = None;
                self.subagent = None;
                self.completer.end();
                self.conversations.end();
                if self.place != Place::Storage || self.device().is_none() {
                    return Vec::new();
                }
                let online = self.online();
                self.storage
                    .open(online)
                    .map(Effect::Request)
                    .into_iter()
                    .collect()
            }
            Action::Back => {
                // A subagent shown steps out to the one it runs under, then
                // to the main timeline.
                if let Some((session_id, call)) = self.shown_subagent() {
                    self.subagent = self
                        .subagent_header(session_id, call)
                        .and_then(|header| header.trail.last().map(|(outer, _)| *outer))
                        .map(|outer| (session_id, outer));
                    return Vec::new();
                }
                match self.back() {
                    Some(action) => self.dispatch(action),
                    None => Vec::new(),
                }
            }
        }
    }

    /// The subagent [`Action::ViewSubagent`] shows, if its session is on
    /// screen.
    fn shown_subagent(&self) -> Option<(SessionId, CallId)> {
        self.subagent.filter(|(id, _)| {
            self.device().is_some()
                && self.place == Place::Session(*id)
                && self.summary(*id).is_some()
        })
    }

    /// The subagent `Agent` call `call` of the open session `id` runs, as
    /// the timeline folds it; `None` for any other call.
    fn subagent_header(&self, id: SessionId, call: CallId) -> Option<SubagentHeader> {
        let open = self.open.as_ref().filter(|open| open.id == id)?;
        let (items, _) = self.timeline(id, &open.events);
        subagent_header(&items, call)
    }

    /// Where [`Action::Back`] goes from the screen shown, as the action
    /// that goes there; `None` where back leaves the app. Follows
    /// [`Model::screen`]: an unpaired device sees pairing, a session gone
    /// from the replica shows the active list.
    fn back(&self) -> Option<Action> {
        self.device()?;
        let route = match self.place {
            Place::SessionList => return None,
            Place::Archived | Place::Settings | Place::Pairing | Place::Usage | Place::Storage => {
                Route::SessionList
            }
            Place::Session(id) => {
                if self.archived(self.summary(id)?) {
                    Route::ArchivedSessions
                } else {
                    Route::SessionList
                }
            }
            Place::Workdir(id) => {
                self.summary(id)?;
                let up = |path: &str| {
                    Action::ShowWorkdir(WorkdirPane::Dir {
                        path: parent(path).to_owned(),
                    })
                };
                return Some(match &self.workdir_pane {
                    WorkdirPane::File { path } => up(path),
                    WorkdirPane::Dir { path } if !path.is_empty() => up(path),
                    WorkdirPane::Diff { .. } => Action::ShowWorkdir(WorkdirPane::Changes),
                    WorkdirPane::Changes | WorkdirPane::Dir { .. } => Action::OpenSession(id),
                });
            }
        };
        Some(Action::Navigate(route))
    }

    /// Whether the link may answer a request now, as far as the model
    /// knows: not while it waits to reconnect or the server refused it.
    fn online(&self) -> bool {
        !matches!(self.link, Link::Reconnecting { .. } | Link::Rejected(_))
    }

    /// The session whose workdir is on screen.
    fn shown_workdir(&self) -> Option<SessionId> {
        match self.place {
            Place::Workdir(id) if self.device().is_some() && self.summary(id).is_some() => Some(id),
            _ => None,
        }
    }

    /// Shows `pane` of session `id`'s workdir: asks for its read if it has
    /// no stored answer and none on its way, and for its content if that is
    /// a blob not downloaded.
    fn show_workdir(&mut self, id: SessionId, pane: WorkdirPane) -> Vec<Effect> {
        if let WorkdirPane::Dir { path } = &pane {
            self.last_dir.clone_from(path);
        }
        let ask = pane_ask(id, &pane);
        self.workdir_pane = pane;
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
        let id = self.shown_workdir()?;
        match (
            &self.workdir_pane,
            self.workdirs
                .stored
                .get(&pane_ask(id, &self.workdir_pane))?,
        ) {
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
        let record = self.blobs.get(&id)?;
        if record.status != BlobStatus::NotDownloaded
            || self.wanted.contains(&id)
            || self.failed.contains_key(&id)
        {
            return None;
        }
        self.wanted.insert(id);
        Some(Request::DownloadBlob(id))
    }

    fn pair(&mut self, address: String, code: String, fingerprint: String) -> Vec<Effect> {
        if matches!(self.pairing, PairingStatus::InProgress { .. }) {
            return Vec::new();
        }
        self.place = Place::Pairing;
        self.viewing = None;
        self.subagent = None;
        let (address, code, fingerprint) = (
            address.trim().to_owned(),
            code.trim().to_owned(),
            fingerprint.trim().to_owned(),
        );
        if address.is_empty() || code.is_empty() || fingerprint.is_empty() {
            self.pairing = PairingStatus::Failed {
                address,
                reason: "address, code and fingerprint are all required".into(),
            };
            return Vec::new();
        }
        self.pairing = PairingStatus::InProgress {
            address: address.clone(),
        };
        vec![Effect::Request(Request::Pair {
            address,
            code,
            fingerprint,
        })]
    }

    /// Queues a command under a fresh UUIDv7 id. Commands need a paired
    /// device: without one there is nobody to send them to.
    fn enqueue(&mut self, kind: CommandKind) -> Vec<Effect> {
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
        });
        vec![
            Effect::Enqueue(command),
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
        self.enqueue(CommandKind::ClearContext {
            session_id,
            context: ContextId::new(),
            name,
        })
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
        let events = self
            .open
            .as_ref()
            .filter(|open| open.id == session_id)
            .map_or(&[][..], |open| open.events.as_slice());
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
            effects.retain(|effect| matches!(effect, Effect::Enqueue(_)));
        }
        let moved = self.enqueue(CommandKind::MoveContext {
            session_id,
            context,
            name,
        });
        if let Some(Effect::Enqueue(command)) = moved.first() {
            self.opening = Some((command.id, session_id));
        }
        effects.extend(moved);
        effects
    }

    /// Whether the open session's log has `context` moved away.
    fn moved(&self, session_id: SessionId, context: ContextId) -> bool {
        self.open
            .iter()
            .filter(|open| open.id == session_id)
            .flat_map(|open| &open.events)
            .any(|(_, e)| e.context == context && matches!(e.kind, EventKind::ContextMoved { .. }))
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
        if self.summary(key.0).is_none() {
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
        let (Some(open), Some(summary)) = (
            self.open.as_ref().filter(|o| o.id == session_id),
            self.summary(session_id),
        ) else {
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
    /// one and moves on.
    fn withdraw(&mut self, id: CommandId, edit: bool) -> Vec<Effect> {
        let Some(index) = self
            .outbox
            .iter()
            .position(|e| e.command.id == id && e.discardable())
        else {
            return Vec::new();
        };
        // A message is set off from the draft by a blank line, a shell
        // command by a line break: the next line is the next command.
        let (key, text, when, separator) = match &self.outbox[index].command.kind {
            CommandKind::SendMessage {
                session_id, text, ..
            } if edit => (
                (*session_id, ComposerMode::Agent),
                text,
                RunWhen::Always,
                "\n\n",
            ),
            CommandKind::RunShell {
                session_id,
                command,
                when,
            } if edit => ((*session_id, ComposerMode::Shell), command, *when, "\n"),
            _ if edit => return Vec::new(),
            _ => {
                self.outbox.remove(index);
                return vec![
                    Effect::Withdraw { id, draft: None },
                    Effect::Request(Request::OutboxChanged),
                ];
            }
        };
        let joined = match self.drafts.get(&key) {
            Some(old) if !old.text.trim().is_empty() => format!("{text}{separator}{}", old.text),
            _ => text.clone(),
        };
        let draft = Draft {
            cursor: text.chars().count(),
            text: joined,
            when,
        };
        self.outbox.remove(index);
        self.drafts.insert(key, draft.clone());
        self.draft_rev += 1;
        vec![
            Effect::Withdraw {
                id,
                draft: Some((key, draft)),
            },
            Effect::Request(Request::OutboxChanged),
        ]
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

    /// Whether approval `id`, requested in the open session, offers a rule
    /// at index `rule`.
    fn offers(&self, id: ApprovalId, rule: u8) -> bool {
        self.open.as_ref().is_some_and(|open| {
            open.events.iter().any(|(_, event)| {
                matches!(&event.kind, EventKind::ApprovalRequested { id: requested, suggestions, .. }
                    if *requested == id && usize::from(rule) < suggestions.len())
            })
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

    /// Bumps `alerts` once if approvals came in that nobody was alerted to
    /// yet, and forgets those no longer pending. Only online: while catching
    /// up the replica shows the past, and an approval requested and resolved
    /// during that is no news; one still pending at its end alerts then.
    ///
    /// Returns the approvals it marked alerted, to store, so a later
    /// process does not alert for them again.
    fn note_new_approvals(&mut self) -> Vec<(SessionId, ApprovalId)> {
        if !matches!(self.link, Link::Online { .. }) {
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
            if self.deleted.contains(&event.session_id) {
                continue;
            }
            match self.sessions.iter_mut().find(|s| s.id == event.session_id) {
                Some(summary) => summary.apply(seq, event),
                None => self.sessions.push(SessionSummary::new(seq, event)),
            }
            self.note_shell_finish(event);
            self.turns.extend(TurnRecord::of(seq, event));
            // As the store decides it (`Store::apply_batch`): before a
            // crop's `into` registers, the old blob goes unless a crop
            // finds it held; a blob with a fate never registers again.
            if let Some((blob, fate)) = BlobFate::of(&event.kind) {
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
            if let Some(open) = self.open.as_mut().filter(|o| o.id == event.session_id) {
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
                if self.place == Place::Session(from) {
                    self.place = Place::Session(to);
                    self.open = None;
                    self.viewing = None;
                    self.subagent = None;
                }
            }
        }
        self.sessions.sort_by(|a, b| b.last_seq.cmp(&a.last_seq));
        self.note_new_approvals()
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
        if self.place == Place::Session(id) {
            let read = self.read_marks.get(&id).map_or(Seq::ZERO, |m| m.seq);
            let mut unseen = events.iter().filter(|(seq, _)| *seq > read);
            let first = self.first_unseen(unseen.clone());
            let unread_from = first.filter(|_| unseen.any(|(_, e)| is_notable(&e.kind)));
            self.open = Some(OpenSession {
                id,
                events,
                unread_from,
            });
        }
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

    /// The server said whether it runs shell commands (already stored).
    pub(crate) fn shell_enabled(&mut self, enabled: bool) {
        self.settings.shell_enabled = enabled;
    }

    /// The server said whether it allows bypass mode (already stored).
    pub(crate) fn bypass_allowed(&mut self, allowed: bool) {
        self.settings.bypass_allowed = allowed;
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
    /// device kept for it. If it was on screen, its list shows instead.
    pub(crate) fn session_deleted(&mut self, id: SessionId, blobs: &[BlobId]) {
        self.deleted.insert(id);
        self.sessions.retain(|s| s.id != id);
        self.turns.retain(|t| t.session_id != id);
        self.drafts.retain(|(session, _), _| *session != id);
        self.composer_modes.remove(&id);
        self.read_marks.remove(&id);
        self.outbox
            .retain(|e| e.status.is_pending() || command_session(&e.command.kind) != Some(id));
        for blob in blobs {
            self.forget_blob(*blob);
            self.fates.remove(blob);
        }
        self.workdirs.forget(id);
        self.forget_resolved_alerts();
        if self.place == Place::Session(id) || self.place == Place::Workdir(id) {
            self.place = Place::Archived;
            self.open = None;
            self.viewing = None;
            self.subagent = None;
        }
    }

    /// Mirrors [`Store::claim_send`](crate::Store::claim_send).
    pub(crate) fn command_sending(&mut self, id: CommandId, now: DateTime<Utc>) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| e.status.is_pending()) {
            entry.status = OutboxStatus::Sending;
            entry.tries += 1;
            entry.first_sent_at.get_or_insert(now);
        }
    }

    /// The first result is final, as in the store.
    pub(crate) fn command_result(
        &mut self,
        id: CommandId,
        result: CommandResult,
        now: DateTime<Utc>,
    ) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| e.status.is_pending()) {
            entry.status = OutboxStatus::Done(result);
            entry.answered_at = Some(now);
        }
    }

    /// Mirrors [`Store::record_undeliverable`](crate::Store::record_undeliverable).
    pub(crate) fn command_undeliverable(&mut self, id: CommandId, now: DateTime<Utc>) {
        if let Some(entry) = self.outbox_entry(id).filter(|e| e.status.is_pending()) {
            entry.status = OutboxStatus::Undeliverable;
            entry.answered_at = Some(now);
        }
    }

    /// Returns the approvals newly marked alerted, as
    /// [`apply_batch`](Model::apply_batch).
    pub(crate) fn set_link(&mut self, link: Link) -> Vec<(SessionId, ApprovalId)> {
        self.link = link;
        self.note_new_approvals()
    }

    pub(crate) fn dirs_listed(&mut self, req: u64, result: Result<DirsResult, TransportError>) {
        self.completer.listed(req, result);
    }

    pub(crate) fn conversations_listed(
        &mut self,
        req: u64,
        result: Result<ConversationsResult, TransportError>,
    ) {
        self.conversations.listed(req, result);
    }

    pub(crate) fn storage_reported(
        &mut self,
        req: u64,
        result: Result<StorageReport, TransportError>,
    ) {
        self.storage.reported(req, result);
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

    /// Mirrors [`Store::put_workdir`](crate::Store::put_workdir), which
    /// stored the awaited `answer` to `ask`: the answer replaces the last
    /// one, the blobs it registered join the blob map and those it purged
    /// leave it. Returns the download of the blob the pane on screen now
    /// shows, if it is wanted.
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
        self.workdirs.answered(ask, answer);
        self.want_shown_blob().into_iter().collect()
    }

    /// Mirrors [`Store::reset_for_server`](crate::Store::reset_for_server),
    /// which left the replica as `snapshot`: nothing of the previous
    /// server's log shows any more, nor what was kept for it — download
    /// requests and failures, alerted approvals, listed directories. Heads replace the drafts they hold.
    pub(crate) fn replica_reset(&mut self, snapshot: Snapshot) {
        let had_drafts = !self.drafts.is_empty();
        self.reload(snapshot);
        self.wanted.clear();
        self.failed.clear();
        self.alerted.clear();
        self.alerted_at.clear();
        self.completer = Completer::default();
        // Ended, not reset: the request ids stay unused, so an answer from
        // the old server cannot match a later request.
        self.conversations.end();
        self.workdirs.reset();
        if had_drafts {
            self.draft_rev += 1;
        }
    }

    pub(crate) fn paired(&mut self, paired: Paired, address: String) {
        self.paired = Some(paired);
        self.settings.server_address = Some(address);
        self.pairing = PairingStatus::Idle;
        self.place = Place::SessionList;
        self.open = None;
        self.viewing = None;
        self.subagent = None;
        self.link = Link::Connecting;
    }

    pub(crate) fn pairing_failed(&mut self, reason: String) {
        if let PairingStatus::InProgress { address } = &self.pairing {
            self.pairing = PairingStatus::Failed {
                address: address.clone(),
                reason,
            };
        }
    }

    /// The blob left the replica: its record goes, with the request for it
    /// and why it last failed.
    fn forget_blob(&mut self, id: BlobId) {
        self.blobs.remove(&id);
        self.wanted.remove(&id);
        self.failed.remove(&id);
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
        self.wanted.remove(&id);
        self.failed.remove(&id);
        if let Some(blob) = self.blobs.get_mut(&id) {
            blob.status = status;
        }
    }

    /// The download of `id` was given up: it is `NotDownloaded` again, and
    /// says why until it is asked for again.
    pub(crate) fn blob_failed(&mut self, id: BlobId, reason: String) {
        self.blob_status(id, BlobStatus::NotDownloaded);
        if self.blobs.contains_key(&id) {
            self.failed.insert(id, reason);
        }
    }

    fn outbox_entry(&mut self, id: CommandId) -> Option<&mut OutboxEntry> {
        self.outbox.iter_mut().find(|e| e.command.id == id)
    }

    // ---- view -------------------------------------------------------------

    pub(crate) fn view(&self) -> ViewState {
        ViewState {
            link: self.link.clone(),
            queued: self.outbox.iter().filter(|e| e.status.is_pending()).count(),
            attention: self.attention(),
            screen: self.screen(),
            can_go_back: self.shown_subagent().is_some() || self.back().is_some(),
            snippets: self.snippets.list().to_vec(),
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
        Attention {
            elsewhere: approvals
                .iter()
                .filter(|a| Some(a.session_id) != open)
                .count(),
            approvals,
            alerts: self.alerts,
            shell_alerts: self.shell_alerts,
            last_shell: self.last_shell.clone(),
        }
    }

    fn screen(&self) -> Screen {
        if self.device().is_none() {
            return Screen::Pairing(self.pairing_view());
        }
        match self.place {
            Place::Pairing => Screen::Pairing(self.pairing_view()),
            Place::Settings => Screen::Settings(SettingsView {
                blob_policy: self.settings.blob_policy,
                server_address: self.settings.server_address.clone(),
                device_id: self.device(),
                server_fingerprint: self.paired.and_then(|p| p.server),
            }),
            Place::SessionList => Screen::SessionList(self.session_list(ListKind::Active)),
            Place::Archived => Screen::SessionList(self.session_list(ListKind::Archived)),
            Place::Usage => Screen::Usage(self.usage_view()),
            Place::Storage => Screen::Storage(self.storage_view()),
            Place::Session(id) => match self.session_view(id) {
                Some(view) => Screen::Session(Box::new(view)),
                None => Screen::SessionList(self.session_list(ListKind::Active)),
            },
            Place::Workdir(id) => match self.workdir_view(id) {
                Some(view) => Screen::Workdir(view),
                None => Screen::SessionList(self.session_list(ListKind::Active)),
            },
        }
    }

    fn workdir_view(&self, id: SessionId) -> Option<WorkdirView> {
        let summary = self.summary(id)?;
        let pane = self.workdir_pane.clone();
        let ask = pane_ask(id, &pane);
        let body = match (&pane, self.workdirs.stored.get(&ask)) {
            (WorkdirPane::Changes, Some(WorkdirAnswer::Changes(result))) => {
                WorkdirBody::Changes(result.clone())
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
            last_dir: self.last_dir.clone(),
            fetch: self.workdirs.fetch(&ask),
            body,
        })
    }

    fn pairing_view(&self) -> PairingView {
        PairingView {
            paired: self.device().map(|device_id| PairedInfo {
                device_id,
                server_address: self.settings.server_address.clone(),
            }),
            status: self.pairing.clone(),
        }
    }

    fn session_list(&self, kind: ListKind) -> SessionListView {
        let (listed, other): (Vec<_>, Vec<_>) = self
            .sessions
            .iter()
            .partition(|s| self.archived(s) == (kind == ListKind::Archived));
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
                    state: session_state(s),
                    pending_approvals: s.pending_approvals.len(),
                    last_activity: s.last_activity,
                    has_draft: [ComposerMode::Agent, ComposerMode::Shell]
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
                    importing: s.importing,
                }
            })
            .collect();
        if kind == ListKind::Archived {
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
            .filter_map(|e| match &e.command.kind {
                CommandKind::CreateSession {
                    workdir,
                    conversation,
                } => {
                    let outgoing = Outgoing::from(&e.status);
                    (!self.in_replica(outgoing)).then(|| PendingSession {
                        command_id: e.command.id,
                        workdir: workdir.clone(),
                        conversation: conversation.clone(),
                        outgoing,
                        discardable: e.discardable(),
                    })
                }
                _ => None,
            })
            .collect();
        let completion = self
            .completer
            .view(self.sessions.iter().filter_map(|s| s.workdir.as_deref()));
        SessionListView {
            kind,
            sessions,
            other_count: other.len(),
            creating,
            completion,
            conversations: self.conversations.view(&self.sessions),
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

    fn storage_view(&self) -> StorageView {
        let (status, report) = self.storage.shown();
        let titles = report
            .iter()
            .flat_map(|report| report.sessions.iter().map(|s| s.session))
            .filter_map(|id| Some((id, self.title(self.summary(id)?).text)))
            .collect();
        StorageView {
            status,
            report,
            titles,
        }
    }

    fn session_view(&self, id: SessionId) -> Option<SessionView> {
        let summary = self.summary(id)?;
        let open = self.open.as_ref().filter(|open| open.id == id);
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
                    answer: self.local_answer(*id, suggestions, true),
                }),
                _ => None,
            })
        });
        let state = session_state(summary);
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
        let context = self.target_context(summary);
        let viewing = self
            .viewing
            .filter(|(session, _)| *session == id)
            .map(|(_, context)| context);
        let title = self.title(summary);
        let subagent = self
            .subagent
            .filter(|(session, _)| *session == id)
            .and_then(|(_, call)| subagent_header(&timeline, call));
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
            });
        }
        let contexts = self.contexts(id, events, context);
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
            archived: self.archived(summary),
            workdir: summary.workdir.clone(),
            state,
            timeline,
            pending_approval,
            rules,
            usage: summary.usage.clone(),
            agent: self.agent_view(summary),
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
            task_stops: self.task_stops(summary, events),
            mode: self.composer_mode(id),
            draft: self.drafts.get(&(id, ComposerMode::Agent)).cloned(),
            shell_draft: self.drafts.get(&(id, ComposerMode::Shell)).cloned(),
            draft_rev: self.draft_rev,
            shell: self.shell_view(id, events),
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
    /// approval, no running turn, no command of this device unanswered.
    fn deletable(&self, summary: &SessionSummary) -> bool {
        let unanswered = self
            .outbox
            .iter()
            .any(|e| e.status.is_pending() && command_session(&e.command.kind) == Some(summary.id));
        self.archived(summary)
            && summary.pending_approvals.is_empty()
            && summary.state != LogState::AgentWorking
            && !unanswered
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
            enabled: self.settings.shell_enabled,
            cwd,
            runs,
        }
    }

    /// The session's agent settings: the summary's fold, with this device's
    /// switches still on their way in front.
    fn agent_view(&self, summary: &SessionSummary) -> AgentView {
        let id = summary.id;
        let agent = &summary.agent;
        let local = |is: &dyn Fn(&CommandKind) -> bool| {
            self.outbox
                .iter()
                .rev()
                .find(|e| is(&e.command.kind))
                .map(|e| (e.command.kind.clone(), Outgoing::from(&e.status)))
                .filter(|(_, outgoing)| !self.in_replica(*outgoing))
        };
        let mode_change = match local(
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
        let model_change = match local(
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
        let effort_change = match local(
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
            bypass_allowed: self.settings.bypass_allowed,
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
            .open
            .iter()
            .filter(|open| open.id == id)
            .flat_map(|open| &open.events)
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
                    item: ImportedItem::UserMessage { text },
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
                | EventKind::ApprovalRequested { call, .. } => tools.parent(&items, *call),
                _ => None,
            };
            tools.step(&event.kind);
            let entry = match &event.kind {
                // The server logs every message with its command id, the
                // entry's key; one without would have none to show under.
                EventKind::UserMessage { device_id, text } => event.command_id.map(|command_id| {
                    logged.insert(command_id);
                    let sent = outbox.get(&command_id);
                    Entry::UserMessage {
                        command_id,
                        text: text.clone(),
                        from: DeviceRef {
                            me: self.is_own_message(sent.is_some(), *device_id),
                            ..self.device_ref(*device_id)
                        },
                        outgoing: sent.map(|e| Outgoing::from(&e.status)),
                        delivery: sent.and_then(|e| Delivery::of(e)),
                    }
                }),
                EventKind::AgentMessage { text, .. } => {
                    Some(Entry::AgentMessage { text: text.clone() })
                }
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
                } => match tools.offer(*id, suggestions).call(&mut items, *call) {
                    Some((item, view, _)) if view.approval.is_none() => {
                        view.approval = Some(ToolApproval {
                            id: *id,
                            status: ApprovalStatus::Pending,
                            answer: None,
                        });
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
                | EventKind::ShellFinished { .. } => None,
                // Nothing in the timeline: what it changes is the content of
                // the event that referenced the blob.
                EventKind::BlobDeleted { .. } | EventKind::BlobCropped { .. } => None,
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
                    context_name, from, ..
                } => {
                    names.insert(event.context, context_name);
                    current = Some(event.context);
                    // Shown in the header, but for where a moved context
                    // came from.
                    match from {
                        Some(SessionOrigin::Context(from)) => Some(Entry::MovedFrom {
                            from: self.session_ref(from.session),
                            name: context_name.clone(),
                        }),
                        Some(SessionOrigin::Agent { conversation }) => Some(Entry::Adopted {
                            conversation: conversation.clone(),
                        }),
                        None => None,
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

        items.extend(self.outbox.iter().filter_map(|e| match &e.command.kind {
            CommandKind::SendMessage {
                session_id,
                context,
                text,
            } if *session_id == id && !logged.contains(&e.command.id) => {
                Some(TimelineItem::Pending {
                    command_id: e.command.id,
                    context: *context,
                    text: text.clone(),
                    outgoing: Outgoing::from(&e.status),
                    discardable: e.discardable(),
                    delivery: Delivery::of(e),
                })
            }
            _ => None,
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
            ImportedItem::UserMessage { text } => {
                Some(ImportedEntry::UserMessage { text: text.clone() })
            }
            ImportedItem::AgentMessage { text } => {
                Some(ImportedEntry::AgentMessage { text: text.clone() })
            }
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
            _ => None,
        })
    }

    /// Whether a command's effect has reached the replica.
    fn in_replica(&self, outgoing: Outgoing) -> bool {
        matches!(outgoing, Outgoing::Accepted(seq) if seq <= self.cursor)
    }

    fn summary(&self, id: SessionId) -> Option<&SessionSummary> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn blob_view(&self, blob: &BlobRef) -> BlobView {
        let Some(record) = self.blobs.get(&blob.id) else {
            return BlobView {
                id: blob.id,
                state: BlobState::NotDownloaded { size: blob.size },
                change: None,
            };
        };
        let size = record.size;
        let state = match &record.status {
            BlobStatus::NotDownloaded => match self.failed.get(&blob.id) {
                Some(reason) => BlobState::Failed {
                    size,
                    reason: reason.clone(),
                },
                None if self.wanted.contains(&blob.id) => BlobState::Requested { size },
                None => BlobState::NotDownloaded { size },
            },
            BlobStatus::Downloading { got, .. } => BlobState::Downloading {
                got: *got,
                size,
                // Blob traffic needs the server too; while the link is down
                // the download waits for it.
                paused: !matches!(self.link, Link::Online { .. } | Link::CatchingUp { .. }),
            },
            BlobStatus::Available { path } => BlobState::Available { path: path.clone() },
        };
        BlobView {
            id: blob.id,
            state,
            change: None,
        }
    }

    fn full_view(&self, full: &Full) -> FullView {
        match full {
            Full::Blob(blob) => self.fated_view(blob),
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

/// The read a workdir pane shows: a diff's is the changes'.
fn pane_ask(session: SessionId, pane: &WorkdirPane) -> WorkdirAsk {
    match pane {
        WorkdirPane::Changes | WorkdirPane::Diff { .. } => WorkdirAsk::Changes { session },
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
            TimelineItem::Pending { .. } => None,
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
            } => {
                if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
                    self.steps.entry(*parent).or_default().last = Some(line.to_owned());
                }
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

/// The session a command names, if it names one. An answer names its
/// approval only; its session is pending that approval meanwhile.
fn command_session(kind: &CommandKind) -> Option<SessionId> {
    match kind {
        CommandKind::SendMessage { session_id, .. }
        | CommandKind::Interrupt { session_id }
        | CommandKind::StopSession { session_id }
        | CommandKind::RenameSession { session_id, .. }
        | CommandKind::SetSessionArchived { session_id, .. }
        | CommandKind::SetPermissionMode { session_id, .. }
        | CommandKind::SetModel { session_id, .. }
        | CommandKind::SetEffort { session_id, .. }
        | CommandKind::ClearContext { session_id, .. }
        | CommandKind::SwitchContext { session_id, .. }
        | CommandKind::MoveContext { session_id, .. }
        | CommandKind::StopTask { session_id, .. }
        | CommandKind::RunShell { session_id, .. }
        | CommandKind::StopShell { session_id, .. } => Some(*session_id),
        CommandKind::CreateSession { .. }
        | CommandKind::ResolveApproval { .. }
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
        BlobRef, Decision, Effort, ImportOutcome, RejectReason, RuleScope, ToolDisplay, ToolKind,
        ToolResult,
    };

    use super::*;
    use crate::view::{
        CandidateSource, Completion, CompletionStatus, FailReason, LinkRejection, Refusal,
    };

    /// Paired as `device` with a server CA whose DER is `b"ca"`.
    fn paired_as(device: DeviceId) -> Paired {
        Paired {
            device,
            server: Some(Fingerprint::of_der(b"ca")),
        }
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
            composer_modes: HashMap::new(),
            read_marks: HashMap::new(),
            deleted: HashSet::new(),
            alerted: HashSet::new(),
            devices: HashMap::new(),
            models: Vec::new(),
            snippets: Vec::new(),
            turns: Vec::new(),
            workdirs: HashMap::new(),
        }
    }

    /// The single command an action queued.
    fn queued(effects: &[Effect]) -> Command {
        match effects {
            [
                Effect::Enqueue(command),
                Effect::Request(Request::OutboxChanged),
            ] => command.clone(),
            other => panic!("expected enqueue + outbox wake-up, got {other:?}"),
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
                    workdir: "/srv/x".into()
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

        let pair = Action::Pair {
            address: " host:7443 ".into(),
            code: "123456".into(),
            fingerprint: "ab:cd".into(),
        };
        let effects = model.dispatch(pair.clone());
        assert_eq!(
            effects,
            vec![Effect::Request(Request::Pair {
                address: "host:7443".into(),
                code: "123456".into(),
                fingerprint: "ab:cd".into(),
            })]
        );
        let in_progress = PairingStatus::InProgress {
            address: "host:7443".into(),
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
        model.paired(paired_as(device), "host:7443".into());
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
                server_address: Some("host:7443".into()),
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
                text: "hello".into()
            }
        );
        // Queued: the time comes from the command's UUIDv7 id.
        let queued_at = match &f.session_view().timeline[..] {
            [
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
        assert_eq!(f.session_view().timeline, pending(Outgoing::Queued, fresh));
        assert_eq!(f.model.view().queued, 1);

        let sent = queued_at + TimeDelta::milliseconds(3200);
        f.model.command_sending(command.id, sent);
        let sending = Delivery {
            first_sent_at: Some(sent),
            tries: 1,
            ..fresh
        };
        assert_eq!(
            f.session_view().timeline,
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
            f.session_view().timeline,
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
            f.session_view().timeline,
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
                    },
                ),
                (None, EventKind::SessionStateChanged(LogState::AgentWorking)),
                (
                    None,
                    EventKind::AgentMessage {
                        parent: None,
                        text: "hi".into(),
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
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                &Entry::UserMessage {
                    command_id: command.id,
                    text: "hello".into(),
                    from: DeviceRef {
                        id: me,
                        name: None,
                        me: true,
                    },
                    outgoing: Some(Outgoing::Accepted(Seq(2))),
                    delivery: Some(done),
                },
                &Entry::AgentMessage { text: "hi".into() },
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
                },
            )],
        );
        let timeline = f.session_view().timeline;
        assert!(matches!(
            &timeline[0],
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
            &timeline[1],
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
            } if *command_id == command.id && text == "x"
        ));
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
                },
                EventKind::ApprovalRequested {
                    id: approval,
                    call: None,
                    tool: "edit".into(),
                    summary: "main.rs".into(),
                    input: None,
                    suggestions: Vec::new(),
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
            vec![Entry::Renamed {
                name: Some("flakytests".into()),
                by: DeviceRef {
                    id: f.me,
                    name: None,
                    me: true,
                },
            }]
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
            vec![Entry::Archived {
                archived: true,
                by: DeviceRef {
                    id: f.me,
                    name: None,
                    me: true,
                },
            }]
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
        f.log(old, vec![agent("done")]);
        f.log(new, vec![agent("done")]);
        assert_eq!(lists(&mut f).0, vec![new, old, mid]);
        let other = DeviceId::new();
        f.log(old, vec![archived_event(true, other)]);
        f.log(mid, vec![archived_event(true, other)]);
        assert_eq!(lists(&mut f), (vec![new], vec![old, mid]));
        assert_eq!(unread(&f, new), 1);
        f.log(old, vec![archived_event(false, other)]);
        assert_eq!(
            lists(&mut f),
            (vec![new, old], vec![mid]),
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
        f.model
            .blob_failed(output.id, "blob size mismatch".to_owned());
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
        f.model.set_link(Link::Online { latency_ms: 20 });
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
        assert_eq!(output_state(&f), downloading(false));
        f.model.set_link(Link::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        assert_eq!(output_state(&f), downloading(true));
        f.model.set_link(Link::CatchingUp {
            have: Seq(1),
            server: Seq(2),
        });
        assert_eq!(output_state(&f), downloading(false));
        // A running download asked for again keeps showing its progress.
        assert_eq!(
            f.dispatch(Action::DownloadBlob(output.id)),
            vec![Effect::Request(Request::DownloadBlob(output.id))]
        );
        assert_eq!(output_state(&f), downloading(false));
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
        f.model.paired(paired_as(f.me), "host:7443".into());
        f.model.set_link(Link::Rejected(LinkRejection::Revoked));
        assert!(f.dispatch(set(" host:7443 ")).is_empty(), "unchanged");
        assert!(f.dispatch(set("not an address:x")).is_empty(), "invalid");
        assert!(f.dispatch(set("  ")).is_empty(), "blank");
        assert_eq!(
            f.dispatch(set(" 10.0.0.7:7443 ")),
            vec![
                Effect::SaveServerAddress("10.0.0.7:7443".into()),
                Effect::Request(Request::Connect),
            ]
        );
        let view = f.model.view();
        assert_eq!(view.link, Link::Connecting);
        f.dispatch(Action::Navigate(Route::Settings));
        match f.model.view().screen {
            Screen::Settings(settings) => {
                assert_eq!(settings.server_address.as_deref(), Some("10.0.0.7:7443"));
                assert_eq!(settings.device_id, Some(f.me));
            }
            other => panic!("expected settings, got {other:?}"),
        }
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
    fn created_session_is_pending_until_it_reaches_the_replica() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/old");
        assert!(
            f.dispatch(Action::CreateSession {
                conversation: None,
                workdir: "   ".into()
            })
            .is_empty()
        );
        let command = queued(&f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: " /srv/new ".into(),
        }));
        let creating = |f: &Fixture| f.list_view().creating;
        assert_eq!(
            creating(&f),
            vec![PendingSession {
                command_id: command.id,
                workdir: "/srv/new".into(),
                conversation: None,
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

        let new = f.session("/srv/new");
        let list = f.list_view();
        assert_eq!(list.creating, Vec::new());
        let ids: Vec<SessionId> = list.sessions.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new, old], "most recently active first");
        assert_eq!(list.sessions[0].state, SessionState::Idle);
    }

    #[test]
    fn a_session_created_like_the_open_one_opens_once_it_is_logged() {
        let mut f = Fixture::paired();
        let old = f.session("/srv/a");
        let list = f.session("/srv/b");
        // Only from the session on screen.
        assert!(f.dispatch(Action::CreateSessionLike(old)).is_empty());
        f.open(old);
        let command = queued(&f.dispatch(Action::CreateSessionLike(old)));
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                workdir: "/srv/a".into(),
                conversation: None,
            }
        );
        assert_eq!(f.model.unloaded_session(), None);

        // Another device's session in the same place is not it.
        f.session("/srv/a");
        assert_eq!(f.model.open_session(), Some(old));

        let new = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/a".into(),
            context_name: "start".into(),
            from: None,
        };
        f.log_commands(new, vec![(Some(command.id), created.clone())]);
        assert_eq!(f.model.open_session(), Some(new));
        assert_eq!(f.model.unloaded_session(), Some(new));

        // Gone elsewhere meanwhile: stays there.
        f.open(old);
        let command = queued(&f.dispatch(Action::CreateSessionLike(old)));
        f.open(list);
        f.log_commands(SessionId::new(), vec![(Some(command.id), created)]);
        assert_eq!(f.model.open_session(), Some(list));
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
                TimelineItem::Logged { .. } => None,
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
        let create = |f: &mut Fixture, conversation: Option<&str>| {
            queued(&f.dispatch(Action::CreateSession {
                workdir: "/srv/a".into(),
                conversation: conversation.map(str::to_owned),
            }))
            .kind
        };
        let kind = |conversation: Option<&str>| CommandKind::CreateSession {
            workdir: "/srv/a".into(),
            conversation: conversation.map(str::to_owned),
        };
        assert_eq!(create(&mut f, Some(" id ")), kind(Some("id")));
        assert_eq!(create(&mut f, Some("  ")), kind(None));
        assert_eq!(create(&mut f, None), kind(None));
        // Not an id the agent could have: still sent, the server decides.
        assert_eq!(create(&mut f, Some("../x y")), kind(Some("../x y")));
    }

    #[test]
    fn a_refused_conversation_is_a_discardable_failed_creation() {
        let mut f = Fixture::paired();
        let command = queued(&f.dispatch(Action::CreateSession {
            workdir: "/srv/a".into(),
            conversation: Some("nope".into()),
        }));
        f.model.command_sending(command.id, Utc::now());
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::BadConversation,
            },
            Utc::now(),
        );
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

    fn adopted_created() -> EventKind {
        EventKind::SessionCreated {
            workdir: "/srv/a".into(),
            context_name: "start".into(),
            from: Some(SessionOrigin::Agent {
                conversation: "conv-1".into(),
            }),
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
            ImportedItem::UserMessage { text }
        } else {
            ImportedItem::AgentMessage { text }
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
                        text: "fix the build".into()
                    },
                },
                // No time recorded: when the server logged it.
                &Entry::Imported {
                    at: logged[2].1,
                    entry: ImportedEntry::AgentMessage {
                        text: "looking".into()
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
                    text: "on it".into()
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
        let command = queued(&f.dispatch(Action::CreateSession {
            workdir: "/srv/a".into(),
            conversation: Some("conv-1".into()),
        }));
        let creating = |f: &Fixture| f.list_view().creating;
        assert_eq!(
            creating(&f),
            vec![PendingSession {
                command_id: command.id,
                workdir: "/srv/a".into(),
                conversation: Some("conv-1".into()),
                outgoing: Outgoing::Queued,
                discardable: true,
            }]
        );
        f.model.command_sending(command.id, Utc::now());
        assert_eq!(creating(&f)[0].outgoing, Outgoing::Sending);

        // A long import: the first pull brings the session and part of its
        // history, the command's `ImportFinished` comes in the next.
        let adopted = SessionId::new();
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
        let refused = queued(&f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/etc".into(),
        }));
        let waiting = queued(&f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/new".into(),
        }));
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

        let reconnecting = Link::Reconnecting {
            attempt: 3,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(4),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        };
        f.model.set_link(reconnecting.clone());
        assert_eq!(f.model.view().link, reconnecting);
        f.model.set_link(Link::Rejected(LinkRejection::Revoked));
        // Local data stays readable while rejected.
        assert_eq!(f.list_view().sessions.len(), 1);
    }

    #[test]
    fn retry_now_only_while_reconnecting() {
        let mut f = Fixture::paired();
        for link in [
            Link::Unpaired,
            Link::Connecting,
            Link::Online { latency_ms: 5 },
            Link::Rejected(LinkRejection::Revoked),
            Link::Rejected(LinkRejection::BadIdentity),
        ] {
            f.model.set_link(link);
            assert!(f.dispatch(Action::RetryNow).is_empty());
        }
        f.model.set_link(Link::Reconnecting {
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
            f.model.set_link(Link::Rejected(rejection));
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
            Link::Connecting,
            Link::Online { latency_ms: 5 },
            Link::Rejected(LinkRejection::Revoked),
            Link::Rejected(LinkRejection::ProtocolMismatch {
                client: 3,
                server: 2,
            }),
        ] {
            f.model.set_link(link);
            assert!(f.dispatch(Action::ResetReplica).is_empty());
        }
        f.model.set_link(Link::Rejected(LinkRejection::HistoryLost {
            have: Seq(9),
            server: Seq(4),
        }));
        // Retrying does not get past it.
        assert!(f.dispatch(Action::RetryNow).is_empty());
        assert_eq!(
            f.dispatch(Action::ResetReplica),
            vec![Effect::ResetReplica, Effect::Request(Request::Connect)]
        );
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
        }
    }

    /// The logged entries of the open session, in order.
    fn entries(f: &Fixture) -> Vec<Entry> {
        f.session_view()
            .timeline
            .into_iter()
            .filter_map(|item| match item {
                TimelineItem::Logged { entry, .. } => Some(entry),
                TimelineItem::Pending { .. } => None,
            })
            .collect()
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
            entries(&f)[0],
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
                status: ApprovalStatus::Pending,
                answer: None,
            }),
            outcome: ToolOutcome::Running { since: minute(2) },
        };
        assert_eq!(entries(&f), vec![Entry::Tool(Box::new(waiting.clone()))]);
        assert!(matches!(
            view.timeline[0],
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
        assert_eq!(entries(&f), vec![Entry::Tool(Box::new(done))]);
        assert_eq!(f.session_view().pending_approval, None);
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
        assert_eq!(entries(&f).len(), 2);
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
        let [Entry::Tool(view)] = entries.as_slice() else {
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
        assert_eq!(entries(&f).len(), 3);
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

    const ONLINE: Link = Link::Online { latency_ms: 5 };

    fn catching_up() -> Link {
        Link::CatchingUp {
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
            composer_modes: model.composer_modes.clone(),
            read_marks: model.read_marks.clone(),
            deleted: model.deleted.clone(),
            alerted: model.alerted.clone(),
            devices: model.devices.clone(),
            models: model.models.clone(),
            snippets: model.snippets.list().to_vec(),
            turns: model.turns.clone(),
            workdirs: model.workdirs.stored.clone(),
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
        f.model.set_link(Link::Online { latency_ms: 9 });
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
        f.model.set_link(Link::Reconnecting {
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
        f.model.set_link(Link::Online { latency_ms: 5 });
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
                ("/srv/code".to_owned(), CandidateSource::Server),
                ("/srv/code/web".to_owned(), CandidateSource::Recent),
            ]
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
    fn completion_needs_the_session_list() {
        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(complete(&mut f, "/srv/"), None);
        f.dispatch(Action::Navigate(Route::SessionList));
        assert!(complete(&mut f, "/srv/").is_some());
        // Creating the session ends the completion.
        queued(&f.dispatch(Action::CreateSession {
            conversation: None,
            workdir: "/srv/x".into(),
        }));
        assert_eq!(f.list_view().completion, None);
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
        f.model.set_link(Link::Reconnecting {
            attempt: 1,
            retry_at: chrono::Utc::now() + chrono::TimeDelta::seconds(1),
            offline_since: chrono::Utc::now(),
            reason: crate::view::LinkFailure::Unreachable,
        });
        let offline = ConversationsStatus::Unavailable(ConversationsUnavailable::Offline);
        assert_eq!(list_conversations(&mut f), None, "nothing is asked offline");
        assert_eq!(picker(&f).status, offline);
        // Nothing was queued for later: coming back online asks nothing.
        f.model.set_link(Link::Online { latency_ms: 5 });
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

    fn storage_req(effects: &[Effect]) -> Option<u64> {
        match effects {
            [] => None,
            [Effect::Request(Request::Storage { req })] => Some(*req),
            other => panic!("expected a storage request, got {other:?}"),
        }
    }

    fn storage_report(sessions: &[SessionId]) -> StorageReport {
        StorageReport {
            database: 4096,
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
        // A session not in this replica has no title.
        assert_eq!(view.titles.len(), 1);
        assert!(view.titles.contains_key(&known));

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
        f.model.set_link(Link::Reconnecting {
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
        f.model.set_link(Link::Online { latency_ms: 5 });
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(storage_req(&f.dispatch(Action::RefreshStorage)), None);
    }

    #[test]
    fn the_picker_needs_a_paired_device_on_the_session_list() {
        let mut model = Model::new(empty_snapshot(), None);
        assert!(model.dispatch(Action::ListConversations).is_empty());

        let mut f = Fixture::paired();
        f.dispatch(Action::Navigate(Route::Settings));
        assert_eq!(list_conversations(&mut f), None);
        f.dispatch(Action::Navigate(Route::SessionList));
        assert!(list_conversations(&mut f).is_some());
        // Leaving the list ends the picker.
        f.dispatch(Action::Navigate(Route::Settings));
        f.dispatch(Action::Navigate(Route::SessionList));
        assert_eq!(f.list_view().conversations, None);
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

        let command = queued(&f.dispatch(Action::CreateSession {
            workdir: pick.workdir,
            conversation: Some(pick.id),
        }));
        assert_eq!(
            command.kind,
            CommandKind::CreateSession {
                workdir: workdir.into(),
                conversation: Some(id.into()),
            }
        );
        assert_eq!(f.list_view().conversations, None);
    }

    /// What a view shows, for the back tests: the screen and which list.
    fn shown(view: &ViewState) -> &'static str {
        match &view.screen {
            Screen::Pairing(_) => "pairing",
            Screen::Settings(_) => "settings",
            Screen::Usage(_) => "usage",
            Screen::Storage(_) => "storage",
            Screen::Workdir(_) => "workdir",
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
        assert_back(&mut f, nav(Route::Usage), "usage", "active list");
        assert_back(&mut f, nav(Route::Storage), "storage", "active list");
        // Re-pairing a paired device can be cancelled.
        assert_back(&mut f, nav(Route::Pairing), "pairing", "active list");
        // A session goes back to the list it is on.
        assert_back(&mut f, |f| f.open(active), "session", "active list");
        assert_back(
            &mut f,
            |f| f.open(archived),
            "archived session",
            "archived list",
        );
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
        f.model.place = Place::Session(SessionId::new());
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
                TimelineItem::Logged { .. } => None,
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
                })
                .collect()
        };
        // The second switch, to the context already current, has no entry.
        assert_eq!(
            texts(&f),
            [
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
                })
                .collect();
        assert_eq!(stamps, [a, b, b, a, b, a, a]);

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
        assert_eq!(texts(&f), ["a1", "a2", "pending a3"], "not B's end");
        assert_eq!((find(&f, "b1"), find(&f, "a1")), (0, 1));
        view(&mut f, None);
        assert_eq!(texts(&f).len(), 7);
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
            matches!(&effects[..], [Effect::Withdraw { id, draft: Some(_) }, _] if *id == stale.id)
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
            Effect::Enqueue(clear),
            Effect::Enqueue(moved),
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
            Some((moved.id, sid)),
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
            texts
                .iter()
                .all(|e| matches!(e, Entry::UserMessage { .. } | Entry::AgentMessage { .. })),
            "{texts:?}"
        );
        assert_eq!(texts.len(), 3, "fix it, done, later");
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
        let [Effect::Enqueue(clear), Effect::Enqueue(moved), _] = &effects[..] else {
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
                    draft: None
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
                Entry::AgentMessage { text } => text,
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
        assert_eq!(shown_items(&f), ["main says", "outer", "pwd"]);

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
        assert_eq!(shown_items(&f), ["main says", "outer", "pwd"]);
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
        assert_eq!(shown_items(&f), ["main says", "outer", "pwd"]);
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
                        entry: Entry::AgentMessage { text },
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
            }
        );
        // Its own outcome is how its launch went.
        assert!(matches!(tools(&f)[0].outcome, ToolOutcome::Done { .. }));
        assert_eq!(tools(&f)[1].subagent, None, "not a subagent's call");
        // Its task leaving ends it.
        f.log(
            sid,
            vec![EventKind::BackgroundTasksChanged { tasks: vec![] }],
        );
        assert_eq!(
            status_of(&f, bg),
            SubagentStatus {
                task: None,
                live: false,
                steps: 2,
                last: Some("found it".into()),
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
        };
        assert_eq!(status_of(&f, fg), idle);
        f.log(sid, vec![tool_result(Some(fg), false, "report")]);
        assert_eq!(
            status_of(&f, fg),
            SubagentStatus {
                live: false,
                ..idle
            }
        );
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
            BlobId as ProtoBlobId, BlobUse, ChangeSet, DirEntry, EntryKind, LineSpan, ListResult,
            ToolResult,
        };

        use super::*;
        use crate::identity::PairedCredentials;
        use crate::view::{WorkdirFetch, WorkdirView};
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

        fn offline() -> Link {
            Link::Reconnecting {
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
            f.model.blob_failed(diff.id, "size mismatch".into());
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
            assert_eq!(shown(&f.model.view()), "archived list");
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
                seam.pairing_succeeded("srv:7443", credentials("CA"))
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
                .pairing_succeeded("other:7443", credentials("ANOTHER CA"))
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
    }
}
