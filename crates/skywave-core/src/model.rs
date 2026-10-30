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
    BlobRef, Command, CommandKind, CommandResult, DirsResult, Event, EventBatch, EventKind,
    RuleSuggestion, SessionState as LogState, ToolCall,
};

use crate::action::{Action, Route};
use crate::completion::Completer;
use crate::projection::{SessionSummary, blob_refs, is_notable};
use crate::seam::Request;
use crate::store::{
    BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxStatus, ReadMark, Settings, Snapshot,
};
use crate::transport::TransportError;
use crate::view::{
    Answer, ApprovalId, ApprovalPrompt, ApprovalStatus, Attention, BlobId, BlobState, BlobView,
    BodyView, CallId, CommandId, Decision, Delivery, DeviceId, Draft, Entry, ErrorInfo, Link,
    ListKind, Outgoing, PairedInfo, PairingStatus, PairingView, PendingApprovalRef, PendingSession,
    Screen, Seq, SessionId, SessionListView, SessionRow, SessionState, SessionView, SettingsView,
    TimelineItem, ToolApproval, ToolOutcome, ToolView, ViewState,
};

/// Work the core must do for an action, in order. Persistence effects come
/// before the requests that make the action visible to the network.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Effect {
    /// Write the command to the outbox.
    Enqueue(Command),
    SaveBlobPolicy(BlobPolicy),
    /// Keep the session's draft; `None` deletes it.
    SaveDraft(SessionId, Option<Draft>),
    SaveReadMark(SessionId, ReadMark),
    /// Take the command out of the outbox and, if given, keep the draft, as
    /// one write: the text is never lost nor kept twice.
    Withdraw {
        id: CommandId,
        draft: Option<(SessionId, Draft)>,
    },
    /// Read the session's events from the replica into the model.
    LoadSession(SessionId),
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
    Session(SessionId),
}

impl From<Route> for Place {
    fn from(route: Route) -> Self {
        match route {
            Route::SessionList => Place::SessionList,
            Route::ArchivedSessions => Place::Archived,
            Route::Settings => Place::Settings,
            Route::Pairing => Place::Pairing,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct OpenSession {
    id: SessionId,
    events: Vec<(Seq, Event)>,
    /// The first event after the read mark as it was when the session
    /// opened, if agent output follows it. Marking read leaves it alone.
    unread_from: Option<Seq>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    device: Option<DeviceId>,
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
    /// Blobs asked for and not yet taken by a download; in memory only, as
    /// the blob queue forgets un-started requests on a restart too.
    wanted: HashSet<BlobId>,
    /// Why the last download of a blob was given up; in memory only.
    failed: HashMap<BlobId, String>,
    /// This device's unsent messages, as stored.
    drafts: HashMap<SessionId, Draft>,
    /// Bumped whenever the model itself rewrites a draft, so heads know to
    /// replace the text they hold; never goes back, not even on reload.
    draft_rev: u64,
    /// How far this device has read each session, as stored.
    read_marks: HashMap<SessionId, ReadMark>,
    /// Events of the session on screen, loaded from the replica.
    open: Option<OpenSession>,
    /// Pending approvals the user was alerted to, or that needed no alert
    /// (pending at start, answered here); in memory only.
    alerted: HashSet<ApprovalId>,
    /// How often new approvals came in ([`Attention::alerts`]).
    alerts: u64,
    /// Workdir completion in the new-session prompt; in memory only.
    completer: Completer,
}

impl Model {
    /// The model after a (cold) start: the replica as stored, `device` from
    /// the identity if the device is paired.
    pub(crate) fn new(snapshot: Snapshot, device: Option<DeviceId>) -> Model {
        let mut model = Model {
            device,
            pairing: PairingStatus::Idle,
            link: Link::Connecting,
            place: Place::SessionList,
            settings: Settings::default(),
            cursor: Seq::ZERO,
            sessions: Vec::new(),
            outbox: Vec::new(),
            blobs: BTreeMap::new(),
            wanted: HashSet::new(),
            failed: HashMap::new(),
            drafts: HashMap::new(),
            draft_rev: 0,
            read_marks: HashMap::new(),
            open: None,
            alerted: HashSet::new(),
            alerts: 0,
            completer: Completer::default(),
        };
        model.reload(snapshot);
        // What was pending before the start is not news.
        model.alerted = model.pending_approvals().map(|(_, id)| id).collect();
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
        self.drafts = snapshot.drafts;
        self.read_marks = snapshot.read_marks;
        self.open = None;
        // Only forget: an approval that came in during a catch-up must still
        // alert once it is over.
        let pending: HashSet<ApprovalId> = self.pending_approvals().map(|(_, id)| id).collect();
        self.alerted.retain(|id| pending.contains(id));
    }

    /// The session on screen, whose events the model needs.
    pub(crate) fn open_session(&self) -> Option<SessionId> {
        match self.place {
            Place::Session(id) => Some(id),
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
            Action::CreateSession { workdir } => {
                let workdir = workdir.trim();
                if workdir.is_empty() {
                    return Vec::new();
                }
                self.completer.end();
                self.enqueue(CommandKind::CreateSession {
                    workdir: workdir.to_owned(),
                })
            }
            Action::CompleteWorkdir { input } => {
                if self.device.is_none() || self.place != Place::SessionList {
                    return Vec::new();
                }
                // While the link is down, recent workdirs stand in.
                let online = !matches!(self.link, Link::Reconnecting { .. } | Link::Rejected(_));
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
            Action::SendMessage { session_id, text } => {
                if text.trim().is_empty() || self.summary(session_id).is_none() {
                    return Vec::new();
                }
                let effects = self.enqueue(CommandKind::SendMessage { session_id, text });
                // The store drops the draft with the enqueue.
                if !effects.is_empty() {
                    self.drafts.remove(&session_id);
                }
                effects
            }
            Action::SaveDraft {
                session_id,
                text,
                cursor,
            } => self.save_draft(session_id, text, cursor),
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
            Action::OpenSession(id) => {
                if self.device.is_none() || self.summary(id).is_none() {
                    return Vec::new();
                }
                self.place = Place::Session(id);
                self.open = None;
                self.completer.end();
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
                _ => Vec::new(),
            },
            Action::Navigate(route) => {
                self.place = route.into();
                self.open = None;
                self.completer.end();
                Vec::new()
            }
        }
    }

    fn pair(&mut self, address: String, code: String, fingerprint: String) -> Vec<Effect> {
        if matches!(self.pairing, PairingStatus::InProgress { .. }) {
            return Vec::new();
        }
        self.place = Place::Pairing;
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
        if self.device.is_none() {
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

    /// Keeps the session's draft; blank text drops it. Nothing to write when
    /// the draft is unchanged or the session unknown.
    fn save_draft(&mut self, session_id: SessionId, text: String, cursor: usize) -> Vec<Effect> {
        if self.summary(session_id).is_none() {
            return Vec::new();
        }
        let draft = (!text.trim().is_empty()).then(|| Draft {
            cursor: cursor.min(text.chars().count()),
            text,
        });
        if self.drafts.get(&session_id) == draft.as_ref() {
            return Vec::new();
        }
        match &draft {
            Some(draft) => self.drafts.insert(session_id, draft.clone()),
            None => self.drafts.remove(&session_id),
        };
        vec![Effect::SaveDraft(session_id, draft)]
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
    /// `edit`, a message goes back into its session's draft, ahead of what
    /// the draft held, the cursor after the message. The state lock is the
    /// one the engine's claim takes, so a command it claimed meanwhile is no
    /// longer discardable here: nothing happens.
    fn withdraw(&mut self, id: CommandId, edit: bool) -> Vec<Effect> {
        let Some(index) = self
            .outbox
            .iter()
            .position(|e| e.command.id == id && e.discardable())
        else {
            return Vec::new();
        };
        let draft = match &self.outbox[index].command.kind {
            CommandKind::SendMessage { session_id, text } if edit => {
                let joined = match self.drafts.get(session_id) {
                    Some(old) => format!("{text}\n\n{}", old.text),
                    None => text.clone(),
                };
                let draft = Draft {
                    cursor: text.chars().count(),
                    text: joined,
                };
                Some((*session_id, draft))
            }
            _ if edit => return Vec::new(),
            _ => None,
        };
        self.outbox.remove(index);
        if let Some((session_id, draft)) = &draft {
            self.drafts.insert(*session_id, draft.clone());
            self.draft_rev += 1;
        }
        vec![Effect::Withdraw { id, draft }]
    }

    /// Pending in the replica and not already answered from this device
    /// (a refused answer may be retried).
    fn can_answer(&self, id: ApprovalId) -> bool {
        let pending = self
            .sessions
            .iter()
            .any(|s| s.pending_approvals.contains(&id));
        let answered = self
            .local_answer(id, &[])
            .is_some_and(|answer| !matches!(answer.outgoing, Outgoing::Failed(_)));
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
    fn note_new_approvals(&mut self) {
        if !matches!(self.link, Link::Online { .. }) {
            return;
        }
        let pending: Vec<ApprovalId> = self.pending_approvals().map(|(_, id)| id).collect();
        self.alerted.retain(|id| pending.contains(id));
        let mut new = false;
        for id in pending {
            // One answered here was seen; it is never news.
            if self.alerted.insert(id) && self.local_answer(id, &[]).is_none() {
                new = true;
            }
        }
        if new {
            self.alerts += 1;
        }
    }

    // ---- engine notifications -------------------------------------------------

    /// Folds a batch the store has accepted. Mirrors
    /// [`Store::apply_batch`](crate::Store::apply_batch): already-seen events
    /// are skipped and a batch that would leave a gap changes nothing.
    pub(crate) fn apply_batch(&mut self, batch: &EventBatch) {
        if batch.from == Seq::ZERO || batch.from > self.cursor.next() {
            return;
        }
        let cursor = self.cursor;
        for (seq, event) in batch.iter().filter(|(seq, _)| *seq > cursor) {
            match self.sessions.iter_mut().find(|s| s.id == event.session_id) {
                Some(summary) => summary.apply(seq, event),
                None => self.sessions.push(SessionSummary::new(seq, event)),
            }
            for blob in blob_refs(&event.kind) {
                self.blobs.entry(blob.id).or_insert_with(|| BlobRecord {
                    id: blob.id,
                    size: blob.size,
                    status: BlobStatus::NotDownloaded,
                });
            }
            if let Some(open) = self.open.as_mut().filter(|o| o.id == event.session_id) {
                open.events.push((seq, event.clone()));
            }
            self.cursor = seq;
        }
        self.sessions.sort_by(|a, b| b.last_seq.cmp(&a.last_seq));
        self.note_new_approvals();
    }

    pub(crate) fn session_loaded(&mut self, id: SessionId, events: Vec<(Seq, Event)>) {
        if self.place == Place::Session(id) {
            let read = self.read_marks.get(&id).map_or(Seq::ZERO, |m| m.seq);
            let mut unseen = events.iter().filter(|(seq, _)| *seq > read);
            let first = unseen.clone().next().map(|(seq, _)| *seq);
            let unread_from = first.filter(|_| unseen.any(|(_, e)| is_notable(&e.kind)));
            self.open = Some(OpenSession {
                id,
                events,
                unread_from,
            });
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

    pub(crate) fn set_link(&mut self, link: Link) {
        self.link = link;
        self.note_new_approvals();
    }

    pub(crate) fn dirs_listed(&mut self, req: u64, result: Result<DirsResult, TransportError>) {
        self.completer.listed(req, result);
    }

    pub(crate) fn paired(&mut self, device: DeviceId, address: String) {
        self.device = Some(device);
        self.settings.server_address = Some(address);
        self.pairing = PairingStatus::Idle;
        self.place = Place::SessionList;
        self.open = None;
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
        }
    }

    fn screen(&self) -> Screen {
        if self.device.is_none() {
            return Screen::Pairing(self.pairing_view());
        }
        match self.place {
            Place::Pairing => Screen::Pairing(self.pairing_view()),
            Place::Settings => Screen::Settings(SettingsView {
                blob_policy: self.settings.blob_policy,
                server_address: self.settings.server_address.clone(),
                device_id: self.device,
            }),
            Place::SessionList => Screen::SessionList(self.session_list(ListKind::Active)),
            Place::Archived => Screen::SessionList(self.session_list(ListKind::Archived)),
            Place::Session(id) => match self.session_view(id) {
                Some(view) => Screen::Session(Box::new(view)),
                None => Screen::SessionList(self.session_list(ListKind::Active)),
            },
        }
    }

    fn pairing_view(&self) -> PairingView {
        PairingView {
            paired: self.device.map(|device_id| PairedInfo {
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
                    has_draft: self.drafts.contains_key(&s.id),
                    unread: match self.read_marks.get(&s.id) {
                        Some(mark) => s.notable.saturating_sub(mark.notable),
                        None => s.notable,
                    },
                    cost_usd: s.usage.as_ref().and_then(|u| u.total.cost_usd),
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
            };
        }
        let creating = self
            .outbox
            .iter()
            .filter_map(|e| match &e.command.kind {
                CommandKind::CreateSession { workdir } => {
                    let outgoing = Outgoing::from(&e.status);
                    (!self.in_replica(outgoing)).then(|| PendingSession {
                        command_id: e.command.id,
                        workdir: workdir.clone(),
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
        }
    }

    fn session_view(&self, id: SessionId) -> Option<SessionView> {
        let summary = self.summary(id)?;
        let open = self.open.as_ref().filter(|open| open.id == id);
        let events = open.map_or(&[][..], |open| open.events.as_slice());
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
                    tool: tool.clone(),
                    summary: summary.clone(),
                    input: input.as_ref().map(|b| self.blob_view(b)),
                    suggestions: suggestions.clone(),
                    answer: self.local_answer(*id, suggestions),
                }),
                _ => None,
            })
        });
        let interrupt = self.latest_outgoing(
            |kind| matches!(kind, CommandKind::Interrupt { session_id } if *session_id == id),
        );
        let stop = self.latest_outgoing(
            |kind| matches!(kind, CommandKind::StopSession { session_id } if *session_id == id),
        );
        let title = self.title(summary);
        let (timeline, rules) = self.timeline(id, events);
        Some(SessionView {
            id,
            title: title.text,
            named: title.named,
            rename: title.rename,
            archived: self.archived(summary),
            workdir: summary.workdir.clone(),
            state: session_state(summary),
            timeline,
            pending_approval,
            rules,
            usage: summary.usage.clone(),
            interrupt,
            stop,
            draft: self.drafts.get(&id).cloned(),
            draft_rev: self.draft_rev,
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
    /// failed); either way a message queued here after that brings it back,
    /// as it will in the log.
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
                && matches!(&e.command.kind, CommandKind::SendMessage { session_id, .. } if *session_id == id)
        });
        archived && !message
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
        for (seq, event) in events {
            let entry = match &event.kind {
                EventKind::UserMessage {
                    command_id,
                    device_id,
                    text,
                } => {
                    logged.insert(*command_id);
                    let sent = outbox.get(command_id);
                    Some(Entry::UserMessage {
                        command_id: *command_id,
                        text: text.clone(),
                        from: *device_id,
                        own: sent.is_some() || self.device == Some(*device_id),
                        outgoing: sent.map(|e| Outgoing::from(&e.status)),
                        delivery: sent.and_then(|e| Delivery::of(e)),
                    })
                }
                EventKind::AgentMessage { text } => {
                    Some(Entry::AgentMessage { text: text.clone() })
                }
                EventKind::ToolCall(ToolCall {
                    call: Some(call),
                    name,
                    display,
                    input,
                }) if !tools.calls.contains_key(call) => {
                    tools.calls.insert(*call, (items.len(), event.at));
                    tools.open.push(items.len());
                    Some(Entry::Tool(Box::new(ToolView {
                        call: *call,
                        name: name.clone(),
                        kind: display.kind,
                        title: display.title.clone(),
                        detail: display.detail.clone(),
                        body: display.body.as_ref().map(|body| BodyView {
                            format: body.format,
                            preview: body.preview.clone(),
                            lines: body.lines,
                            full: body.full.as_ref().map(|b| self.blob_view(b)),
                        }),
                        input: input.as_ref().map(|b| self.blob_view(b)),
                        approval: None,
                        outcome: ToolOutcome::Running { since: event.at },
                    })))
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
                    input: input.as_ref().map(|b| self.blob_view(b)),
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
                                    output: result.output.as_ref().map(|b| self.blob_view(b)),
                                    took: (event.at - called).to_std().unwrap_or_default(),
                                };
                            }
                            None
                        }
                        // No call to fold into, or a second result for one.
                        _ => Some(Entry::ToolResult {
                            summary: result.preview.clone(),
                            output: result.output.as_ref().map(|b| self.blob_view(b)),
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
                            input: input.as_ref().map(|b| self.blob_view(b)),
                            status: ApprovalStatus::Pending,
                        })
                    }
                },
                EventKind::ApprovalResolved {
                    id,
                    decision,
                    by,
                    rule,
                } => {
                    let by_me = self.device == Some(*by);
                    let status = ApprovalStatus::Resolved {
                        decision: *decision,
                        by: *by,
                        by_me,
                        rule: tools.grant(*id, *decision, *rule),
                    };
                    let ends = (*decision == Decision::Deny)
                        .then_some(ToolOutcome::Denied { by: *by, by_me });
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
                    by: *by,
                    own: self.device == Some(*by),
                }),
                EventKind::SessionArchived { archived, by } => Some(Entry::Archived {
                    archived: *archived,
                    by: *by,
                    own: self.device == Some(*by),
                }),
                EventKind::SessionStateChanged(state) => {
                    // Every state change ends a turn or starts the next.
                    tools.end_turn(&mut items);
                    match state {
                        LogState::Errored { message } => Some(Entry::AgentErrored {
                            message: message.clone(),
                        }),
                        // Shown in the header.
                        LogState::Idle | LogState::AgentWorking => None,
                    }
                }
                EventKind::TurnUsage(usage) => Some(Entry::TurnUsage(usage.clone())),
                // Shown in the header.
                EventKind::SessionCreated { .. } => None,
            };
            if let Some(entry) = entry {
                items.push(TimelineItem::Logged {
                    seq: *seq,
                    at: event.at,
                    entry,
                });
            }
        }

        items.extend(self.outbox.iter().filter_map(|e| match &e.command.kind {
            CommandKind::SendMessage { session_id, text }
                if *session_id == id && !logged.contains(&e.command.id) =>
            {
                Some(TimelineItem::Pending {
                    command_id: e.command.id,
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

    /// This device's latest answer to an approval, its rule read from what
    /// the approval `offered`.
    fn local_answer(&self, approval: ApprovalId, offered: &[RuleSuggestion]) -> Option<Answer> {
        self.outbox.iter().rev().find_map(|e| match e.command.kind {
            CommandKind::ResolveApproval { id, decision, rule } if id == approval => Some(Answer {
                decision,
                rule: rule.and_then(|rule| offered.get(usize::from(rule)).cloned()),
                outgoing: Outgoing::from(&e.status),
                delivery: Delivery::of(e),
            }),
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
        BlobView { id: blob.id, state }
    }
}

/// Where the linked tool calls sit while a timeline is built, so their
/// approvals and results fold into them (spec §7 "Tool timeline").
#[derive(Default)]
struct ToolIndex {
    /// Each linked call's item and the call's time.
    calls: HashMap<CallId, (usize, DateTime<Utc>)>,
    /// Each approval's item: its call's [`Entry::Tool`] or its own
    /// [`Entry::Approval`].
    approvals: HashMap<ApprovalId, usize>,
    /// Items of the calls made since the last turn boundary.
    open: Vec<usize>,
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

    /// The turn ended or the next one started: calls still running never
    /// got a result.
    fn end_turn(&mut self, items: &mut [TimelineItem]) {
        for item in self.open.drain(..) {
            if let Some(view) = tool_at(items, item)
                && matches!(view.outcome, ToolOutcome::Running { .. })
            {
                view.outcome = ToolOutcome::NoResult;
            }
        }
    }
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

/// A pending approval overrides the logged agent state.
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
    use std::time::{Duration, Instant};

    use chrono::{TimeDelta, TimeZone, Utc};
    use skywave_proto::{
        BlobRef, Decision, RejectReason, RuleScope, ToolDisplay, ToolKind, ToolResult,
    };

    use super::*;
    use crate::view::{CandidateSource, Completion, CompletionStatus, LinkRejection};

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
                model: Model::new(empty_snapshot(), Some(me)),
                me,
                events: Vec::new(),
            }
        }

        /// Appends events to the log and delivers them as one batch.
        fn log(&mut self, session_id: SessionId, kinds: Vec<EventKind>) {
            let from = Seq(self.events.len() as u64 + 1);
            let events: Vec<Event> = kinds
                .into_iter()
                .map(|kind| {
                    let minute = (self.events.len() % 60) as u32;
                    let event = Event {
                        session_id,
                        at: Utc.with_ymd_and_hms(2026, 10, 29, 10, minute, 0).unwrap(),
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
            settings: Settings::default(),
            drafts: HashMap::new(),
            read_marks: HashMap::new(),
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

        // Navigation and commands go nowhere without an identity.
        model.dispatch(Action::Navigate(Route::Settings));
        assert!(matches!(model.view().screen, Screen::Pairing(_)));
        assert!(
            model
                .dispatch(Action::CreateSession {
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
        model.paired(device, "host:7443".into());
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
                text: "hello".into(),
                outgoing,
                discardable: outgoing == Outgoing::Queued,
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
        f.log(
            sid,
            vec![
                EventKind::UserMessage {
                    command_id: command.id,
                    device_id: me,
                    text: "hello".into(),
                },
                EventKind::SessionStateChanged(LogState::AgentWorking),
                EventKind::AgentMessage { text: "hi".into() },
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
                    from: me,
                    own: true,
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
        f.log(
            sid,
            vec![EventKind::UserMessage {
                command_id: phone,
                device_id: DeviceId::new(),
                text: "from the phone".into(),
            }],
        );
        let timeline = f.session_view().timeline;
        assert!(matches!(
            &timeline[0],
            TimelineItem::Logged {
                entry: Entry::UserMessage {
                    command_id,
                    own: false,
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
                outgoing: Outgoing::Failed(RejectReason::UnknownSession),
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
                    input: Some(input.clone()),
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
        let blob = BlobView {
            id: input.id,
            state: BlobState::NotDownloaded { size: 5000 },
        };
        assert_eq!(
            view.pending_approval,
            Some(ApprovalPrompt {
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
        assert!(view.timeline.contains(&TimelineItem::Logged {
            seq: Seq(3),
            at: f.events[2].at,
            entry: Entry::Approval {
                id: approval,
                tool: "bash".into(),
                summary: "rm -rf build".into(),
                input: Some(blob),
                status: ApprovalStatus::Resolved {
                    decision: Decision::Deny,
                    by: phone,
                    by_me: false,
                    rule: None,
                },
            },
        }));
        assert!(
            f.dispatch(answer).is_empty(),
            "resolved approvals are closed"
        );
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
                    by: me,
                    by_me: true,
                    rule: Some(edits.clone()),
                },
                &ApprovalStatus::Resolved {
                    decision: Decision::Approve,
                    by: phone,
                    by_me: false,
                    rule: Some(ls.clone()),
                },
            ]
        );
        assert_eq!(view.rules, [edits, ls]);
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
            command_id: CommandId::new(),
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
                by: f.me,
                own: true,
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
            Some(Outgoing::Failed(RejectReason::UnknownSession))
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
                by: f.me,
                own: true,
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

    #[test]
    fn blob_states_follow_download_progress() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let output = BlobRef {
            id: BlobId::new(),
            size: 10_000,
        };
        f.log(
            sid,
            vec![
                EventKind::ToolCall(ToolCall {
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
                    output: Some(output.clone()),
                }),
            ],
        );
        f.open(sid);
        let output_state = |f: &Fixture| match f.session_view().timeline.last() {
            Some(TimelineItem::Logged {
                entry:
                    Entry::ToolResult {
                        output: Some(blob), ..
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
            next_in: Duration::from_secs(1),
            offline_since: Instant::now(),
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

    fn save_draft(session_id: SessionId, text: &str, cursor: usize) -> Action {
        Action::SaveDraft {
            session_id,
            text: text.into(),
            cursor,
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
        };
        // The cursor is clamped to the text's chars.
        assert_eq!(
            f.dispatch(save_draft(sid, "héllo", 99)),
            vec![Effect::SaveDraft(sid, Some(draft.clone()))]
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
            vec![Effect::SaveDraft(sid, None)]
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
                sid,
                Some(Draft {
                    text: "abc".into(),
                    cursor: 1
                })
            )],
            "a moved cursor is a change"
        );
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
                workdir: "   ".into()
            })
            .is_empty()
        );
        let command = queued(&f.dispatch(Action::CreateSession {
            workdir: " /srv/new ".into(),
        }));
        let creating = |f: &Fixture| f.list_view().creating;
        assert_eq!(
            creating(&f),
            vec![PendingSession {
                command_id: command.id,
                workdir: "/srv/new".into(),
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
        };
        assert_eq!(
            effects,
            vec![Effect::Withdraw {
                id: first.id,
                draft: Some((sid, draft.clone())),
            }]
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
            vec![Effect::Withdraw {
                id: second.id,
                draft: Some((
                    sid,
                    Draft {
                        text: "then lint".into(),
                        cursor: 9,
                    }
                )),
            }]
        );
        assert_eq!(pending(&f), Vec::new());

        // Discarding drops the message and leaves the draft alone.
        let third = send(&mut f, sid, "never mind");
        let rev = f.session_view().draft_rev;
        assert_eq!(
            f.dispatch(Action::Discard(third.id)),
            vec![Effect::Withdraw {
                id: third.id,
                draft: None,
            }]
        );
        assert_eq!(pending(&f), Vec::new());
        assert_eq!(f.session_view().draft_rev, rev);

        // Only messages go back into a draft.
        let interrupt = queued(&f.dispatch(Action::Interrupt { session_id: sid }));
        assert!(f.dispatch(Action::EditQueued(interrupt.id)).is_empty());
        assert!(f.dispatch(Action::Discard(interrupt.id)).is_empty());
    }

    #[test]
    fn claimed_message_is_not_discardable() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        f.open(sid);
        let command = send(&mut f, sid, "hello");
        assert_eq!(pending(&f), vec![("hello".to_owned(), true)]);

        // Once claimed it may have reached the server: it stays, even after
        // a failed attempt puts it back in the queue (a restart does that).
        f.model.command_sending(command.id, Utc::now());
        assert_eq!(pending(&f), vec![("hello".to_owned(), false)]);
        assert!(f.dispatch(Action::Discard(command.id)).is_empty());
        assert!(f.dispatch(Action::EditQueued(command.id)).is_empty());
        f.model.outbox_entry(command.id).unwrap().status = OutboxStatus::Queued;
        assert_eq!(pending(&f), vec![("hello".to_owned(), false)]);
        assert!(f.dispatch(Action::Discard(command.id)).is_empty());

        // A refusal settles it: the message can be dismissed or edited.
        f.model.command_result(
            command.id,
            CommandResult::Rejected {
                reason: RejectReason::UnknownSession,
            },
            Utc::now(),
        );
        assert_eq!(pending(&f), vec![("hello".to_owned(), true)]);
        assert_eq!(f.dispatch(Action::EditQueued(command.id)).len(), 1);
        assert_eq!(f.session_view().draft.unwrap().text, "hello");
    }

    #[test]
    fn discard_refused_creation_hides_row() {
        let mut f = Fixture::paired();
        let refused = queued(&f.dispatch(Action::CreateSession {
            workdir: "/etc".into(),
        }));
        let waiting = queued(&f.dispatch(Action::CreateSession {
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
            vec![Effect::Withdraw {
                id: refused.id,
                draft: None,
            }]
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
            next_in: Duration::from_secs(4),
            offline_since: Instant::now(),
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
            Link::Connecting,
            Link::Online { latency_ms: 5 },
            Link::Rejected(LinkRejection::Revoked),
        ] {
            f.model.set_link(link);
            assert!(f.dispatch(Action::RetryNow).is_empty());
        }
        f.model.set_link(Link::Reconnecting {
            attempt: 2,
            next_in: Duration::from_secs(30),
            offline_since: Instant::now(),
        });
        assert_eq!(
            f.dispatch(Action::RetryNow),
            vec![Effect::Request(Request::RetryNow)]
        );
    }

    // Tool timeline (spec §7): calls group their approval and result.

    fn tool_call(call: Option<CallId>, kind: ToolKind, title: &str) -> EventKind {
        EventKind::ToolCall(ToolCall {
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
            },
            BlobRef {
                id: BlobId::new(),
                size: 7000,
            },
        );
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                EventKind::ToolCall(ToolCall {
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
                            full: Some(full.clone()),
                        }),
                    },
                    input: Some(input.clone()),
                }),
                approval_requested(approval, Some(call)),
            ],
        );
        // The body's whole content is a blob like any other.
        assert!(f.model.blobs.contains_key(&full.id));
        f.open(sid);

        let view = f.session_view();
        assert_eq!(view.pending_approval.as_ref().unwrap().call, Some(call));
        let blob = |b: &BlobRef| BlobView {
            id: b.id,
            state: BlobState::NotDownloaded { size: b.size },
        };
        let waiting = ToolView {
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
                    by: f.me,
                    by_me: true,
                    rule: None,
                },
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
                by: phone,
                by_me: false
            }
        );
        assert_eq!(
            view.approval,
            Some(ToolApproval {
                id: approval,
                status: ApprovalStatus::Resolved {
                    decision: Decision::Deny,
                    by: phone,
                    by_me: false,
                    rule: None,
                },
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
    fn a_turn_ending_without_a_result_leaves_no_result() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/a");
        let (a, b, c) = (CallId::new(), CallId::new(), CallId::new());
        f.log(
            sid,
            vec![
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(a), ToolKind::Shell, "sleep 1"),
                EventKind::TurnInterrupted,
                EventKind::SessionStateChanged(LogState::Idle),
                // A later turn starting also ends what the last one left.
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(b), ToolKind::Fetch, "https://example.com"),
                EventKind::SessionStateChanged(LogState::AgentWorking),
                tool_call(Some(c), ToolKind::Subagent, "explore"),
            ],
        );
        f.open(sid);
        let outcomes: Vec<ToolOutcome> = tools(&f).into_iter().map(|v| v.outcome).collect();
        assert_eq!(
            outcomes,
            vec![
                ToolOutcome::NoResult,
                ToolOutcome::NoResult,
                ToolOutcome::Running { since: minute(8) },
            ]
        );
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
                        by: f.me,
                        by_me: true,
                        rule: None,
                    },
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
        EventKind::AgentMessage { text: text.into() }
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
            command_id: CommandId::new(),
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
                    command_id: CommandId::new(),
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
            settings: model.settings.clone(),
            drafts: model.drafts.clone(),
            read_marks: model.read_marks.clone(),
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
    fn approvals_pending_at_start_never_alert() {
        let mut f = Fixture::paired();
        let sid = f.session("/srv/app");
        f.log(sid, vec![approval_requested(ApprovalId::new(), None)]);
        let mut model = Model::new(snapshot(&f.model), Some(f.me));
        model.set_link(ONLINE);
        let attention = model.view().attention;
        assert_eq!(attention.alerts, 0);
        assert_eq!(attention.approvals.len(), 1);
        assert_eq!(attention.approvals[0].title, "app");
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
            next_in: Duration::from_secs(1),
            offline_since: Instant::now(),
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
            workdir: "/srv/x".into(),
        }));
        assert_eq!(f.list_view().completion, None);
    }
}
