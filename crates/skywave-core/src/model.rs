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
    BlobRef, Command, CommandKind, CommandResult, Event, EventBatch, EventKind,
    SessionState as LogState, ToolCall,
};

use crate::action::{Action, Route};
use crate::projection::{SessionSummary, blob_refs};
use crate::seam::Request;
use crate::store::{
    BlobPolicy, BlobRecord, BlobStatus, OutboxEntry, OutboxStatus, Settings, Snapshot,
};
use crate::view::{
    Answer, ApprovalId, ApprovalPrompt, ApprovalStatus, BlobId, BlobState, BlobView, BodyView,
    CallId, CommandId, Decision, DeviceId, Draft, Entry, ErrorInfo, Link, Outgoing, PairedInfo,
    PairingStatus, PairingView, PendingSession, Screen, Seq, SessionId, SessionListView,
    SessionRow, SessionState, SessionView, SettingsView, TimelineItem, ToolApproval, ToolOutcome,
    ToolView, ViewState,
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
    /// Read the session's events from the replica into the model.
    LoadSession(SessionId),
    /// Hand to the engine once every persistence effect succeeded.
    Request(Request),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    SessionList,
    Settings,
    Pairing,
    Session(SessionId),
}

impl From<Route> for Place {
    fn from(route: Route) -> Self {
        match route {
            Route::SessionList => Place::SessionList,
            Route::Settings => Place::Settings,
            Route::Pairing => Place::Pairing,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct OpenSession {
    id: SessionId,
    events: Vec<(Seq, Event)>,
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
    /// This device's unsent messages, as stored.
    drafts: HashMap<SessionId, Draft>,
    /// Events of the session on screen, loaded from the replica.
    open: Option<OpenSession>,
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
            drafts: HashMap::new(),
            open: None,
        };
        model.reload(snapshot);
        model
    }

    /// Replaces the replica mirror with `snapshot`, keeping link, pairing
    /// and screen. The open session's events must be reloaded afterwards.
    pub(crate) fn reload(&mut self, snapshot: Snapshot) {
        self.settings = snapshot.settings;
        self.cursor = snapshot.cursor;
        self.sessions = snapshot.sessions;
        self.outbox = snapshot.outbox;
        self.blobs = snapshot.blobs.into_iter().map(|b| (b.id, b)).collect();
        self.drafts = snapshot.drafts;
        self.open = None;
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
                self.enqueue(CommandKind::CreateSession {
                    workdir: workdir.to_owned(),
                })
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
            Action::ResolveApproval { id, decision } => {
                if !self.can_answer(id) {
                    return Vec::new();
                }
                self.enqueue(CommandKind::ResolveApproval { id, decision })
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
            Action::OpenSession(id) => {
                if self.device.is_none() || self.summary(id).is_none() {
                    return Vec::new();
                }
                self.place = Place::Session(id);
                self.open = None;
                vec![Effect::LoadSession(id)]
            }
            Action::DownloadBlob(id) => match self.blobs.get(&id) {
                Some(blob) if !matches!(blob.status, BlobStatus::Available { .. }) => {
                    vec![Effect::Request(Request::DownloadBlob(id))]
                }
                _ => Vec::new(),
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

    /// Pending in the replica and not already answered from this device
    /// (a refused answer may be retried).
    fn can_answer(&self, id: ApprovalId) -> bool {
        let pending = self
            .sessions
            .iter()
            .any(|s| s.pending_approvals.contains(&id));
        let answered = self
            .local_answer(id)
            .is_some_and(|answer| !matches!(answer.outgoing, Outgoing::Failed(_)));
        pending && !answered
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
    }

    pub(crate) fn session_loaded(&mut self, id: SessionId, events: Vec<(Seq, Event)>) {
        if self.place == Place::Session(id) {
            self.open = Some(OpenSession { id, events });
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
        if let Some(blob) = self.blobs.get_mut(&id) {
            blob.status = status;
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
            screen: self.screen(),
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
            Place::SessionList => Screen::SessionList(self.session_list()),
            Place::Session(id) => match self.session_view(id) {
                Some(view) => Screen::Session(Box::new(view)),
                None => Screen::SessionList(self.session_list()),
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

    fn session_list(&self) -> SessionListView {
        let sessions = self
            .sessions
            .iter()
            .map(|s| SessionRow {
                id: s.id,
                workdir: s.workdir.clone(),
                state: session_state(s),
                pending_approvals: s.pending_approvals.len(),
                last_activity: s.last_activity,
                has_draft: self.drafts.contains_key(&s.id),
            })
            .collect();
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
                    })
                }
                _ => None,
            })
            .collect();
        SessionListView { sessions, creating }
    }

    fn session_view(&self, id: SessionId) -> Option<SessionView> {
        let summary = self.summary(id)?;
        let events = match &self.open {
            Some(open) if open.id == id => open.events.as_slice(),
            _ => &[],
        };
        let pending_approval = summary.pending_approval().and_then(|approval| {
            events.iter().find_map(|(_, event)| match &event.kind {
                EventKind::ApprovalRequested {
                    id,
                    call,
                    tool,
                    summary,
                    input,
                } if *id == approval => Some(ApprovalPrompt {
                    id: *id,
                    call: *call,
                    tool: tool.clone(),
                    summary: summary.clone(),
                    input: input.as_ref().map(|b| self.blob_view(b)),
                    answer: self.local_answer(*id),
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
        Some(SessionView {
            id,
            workdir: summary.workdir.clone(),
            state: session_state(summary),
            timeline: self.timeline(id, events),
            pending_approval,
            interrupt,
            stop,
            draft: self.drafts.get(&id).cloned(),
        })
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

    fn timeline(&self, id: SessionId, events: &[(Seq, Event)]) -> Vec<TimelineItem> {
        let outbox: HashMap<CommandId, Outgoing> = self
            .outbox
            .iter()
            .map(|e| (e.command.id, Outgoing::from(&e.status)))
            .collect();
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
                    let outgoing = outbox.get(command_id).copied();
                    Some(Entry::UserMessage {
                        command_id: *command_id,
                        text: text.clone(),
                        from: *device_id,
                        own: outgoing.is_some() || self.device == Some(*device_id),
                        outgoing,
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
                } => match tools.call(&mut items, *call) {
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
                EventKind::ApprovalResolved { id, decision, by } => {
                    let by_me = self.device == Some(*by);
                    let status = ApprovalStatus::Resolved {
                        decision: *decision,
                        by: *by,
                        by_me,
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
                })
            }
            _ => None,
        }));
        items
    }

    /// This device's latest answer to an approval.
    fn local_answer(&self, approval: ApprovalId) -> Option<Answer> {
        self.outbox.iter().rev().find_map(|e| match e.command.kind {
            CommandKind::ResolveApproval { id, decision } if id == approval => Some(Answer {
                decision,
                outgoing: Outgoing::from(&e.status),
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
        let state = match self.blobs.get(&blob.id) {
            Some(record) => BlobState::from(record),
            None => BlobState::NotDownloaded { size: blob.size },
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
}

impl ToolIndex {
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

    use chrono::{TimeZone, Utc};
    use skywave_proto::{BlobRef, Decision, RejectReason, ToolDisplay, ToolKind, ToolResult};

    use super::*;
    use crate::view::LinkRejection;

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
                sessions: Vec::new(),
                creating: Vec::new(),
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
        let pending = |outgoing| {
            vec![TimelineItem::Pending {
                command_id: command.id,
                text: "hello".into(),
                outgoing,
            }]
        };
        assert_eq!(f.session_view().timeline, pending(Outgoing::Queued));
        assert_eq!(f.model.view().queued, 1);

        f.model.command_sending(command.id, Utc::now());
        assert_eq!(f.session_view().timeline, pending(Outgoing::Sending));

        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(2) },
            Utc::now(),
        );
        assert_eq!(
            f.session_view().timeline,
            pending(Outgoing::Accepted(Seq(2)))
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
                    ..
                },
                ..
            } if *command_id == phone
        ));
        assert_eq!(
            timeline[1],
            TimelineItem::Pending {
                command_id: command.id,
                text: "x".into(),
                outgoing: Outgoing::Failed(RejectReason::UnknownSession),
            }
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
                answer: None,
            })
        );

        // Unknown approvals cannot be answered; a known one once.
        assert!(
            f.dispatch(Action::ResolveApproval {
                id: ApprovalId::new(),
                decision: Decision::Approve
            })
            .is_empty()
        );
        let answer = Action::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
        };
        let command = queued(&f.dispatch(answer.clone()));
        assert!(f.dispatch(answer.clone()).is_empty());
        let prompt = f.session_view().pending_approval.unwrap();
        assert_eq!(
            prompt.answer,
            Some(Answer {
                decision: Decision::Approve,
                outgoing: Outgoing::Queued,
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
                },
            },
        }));
        assert!(
            f.dispatch(answer).is_empty(),
            "resolved approvals are closed"
        );
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
        let path = PathBuf::from("/data/blobs/x");
        f.model.blob_status(
            output.id,
            BlobStatus::Downloading {
                got: 4096,
                path: path.clone(),
            },
        );
        assert_eq!(
            output_state(&f),
            BlobState::Downloading {
                got: 4096,
                size: 10_000
            }
        );
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
            }]
        );
        f.model.command_result(
            command.id,
            CommandResult::Accepted { seq: Seq(2) },
            Utc::now(),
        );
        assert_eq!(creating(&f)[0].outgoing, Outgoing::Accepted(Seq(2)));

        let new = f.session("/srv/new");
        let list = f.list_view();
        assert_eq!(list.creating, Vec::new());
        let ids: Vec<SessionId> = list.sessions.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![new, old], "most recently active first");
        assert_eq!(list.sessions[0].state, SessionState::Idle);
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
}
