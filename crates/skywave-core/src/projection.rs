//! Per-session summaries derived from the event log.
//!
//! The session list reads these instead of replaying every event. The fold is
//! pure: the store persists its result in the same transaction that stores
//! the events, so the projection never runs ahead of or behind the log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skywave_proto::{
    AgentSettings, AgentWork, ApprovalId, BlobRef, ContextId, Event, EventKind, Seq, SessionId,
    SessionState, ToolCall, ToolResult, TurnUsage,
};

/// What the session list needs to know about one session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: SessionId,
    /// Server directory the session runs in (D14). `None` only if the
    /// session's events arrived without its `SessionCreated`, which a
    /// contiguous log replayed from seq 1 never does.
    pub workdir: Option<String>,
    /// Last state the server recorded for the agent.
    pub state: SessionState,
    /// Approvals requested and not yet resolved or cancelled, oldest first.
    pub pending_approvals: Vec<ApprovalId>,
    /// Time of the session's newest event.
    pub last_activity: DateTime<Utc>,
    /// Seq of the session's newest event.
    pub last_seq: Seq,
    /// The agent was stopped on request and no message has come since.
    pub stopped: bool,
    /// How many of the session's events are [notable](is_notable): what
    /// unread counts subtract a read mark from.
    pub notable: u64,
    /// The name the newest `SessionRenamed` gave it; `None` for the default
    /// title.
    pub name: Option<String>,
    /// First line of the session's first message, at most
    /// [`FIRST_PROMPT_MAX_CHARS`]; the default title's first choice.
    pub first_prompt: Option<String>,
    /// Hidden from the active list, as the log's last
    /// [`archived_effect`](EventKind::archived_effect) left it: archived by
    /// `SessionArchived`, back with the next message.
    pub archived: bool,
    /// What the session's turns took, as their agent reported it; `None`
    /// until a turn reports anything.
    pub usage: Option<SessionUsage>,
    /// The agent's permission mode and model, folded with
    /// [`AgentSettings::apply`].
    pub agent: AgentSettings,
    /// The agent's background tasks and schedules, folded with
    /// [`AgentWork::apply`]: each set as its last event left it.
    pub work: AgentWork,
    /// The session's current context: its first (which has the session's
    /// id) until a `ContextStarted` or `ContextSwitched` names another.
    pub context: ContextId,
}

/// What a session's turns took: the newest turn's figures and the sums over
/// every turn that reported any. Agent-agnostic (D3); folded from
/// `TurnUsage` events, which hold per-turn figures, so this only adds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// The newest turn that reported usage, as reported.
    pub last_turn: TurnUsage,
    /// Each figure summed over the turns that reported it; `None` while no
    /// turn has. `model` is the newest one named.
    pub total: TurnUsage,
}

impl SessionUsage {
    /// Usage after a first `turn`.
    pub fn new(turn: &TurnUsage) -> Self {
        SessionUsage {
            last_turn: turn.clone(),
            total: turn.clone(),
        }
    }

    /// Folds in the next `turn`.
    pub fn add(&mut self, turn: &TurnUsage) {
        fn sum<T: std::ops::Add<Output = T> + Copy>(
            total: Option<T>,
            turn: Option<T>,
        ) -> Option<T> {
            match (total, turn) {
                (Some(total), Some(turn)) => Some(total + turn),
                (total, turn) => total.or(turn),
            }
        }
        self.last_turn = turn.clone();
        let total = &mut self.total;
        total.duration_ms = sum(total.duration_ms, turn.duration_ms);
        total.tokens_in = sum(total.tokens_in, turn.tokens_in);
        total.tokens_out = sum(total.tokens_out, turn.tokens_out);
        total.cost_usd = sum(total.cost_usd, turn.cost_usd);
        if turn.model.is_some() {
            total.model.clone_from(&turn.model);
        }
    }
}

/// Longest [`SessionSummary::first_prompt`] kept, in chars. The store's
/// backfill migration cuts at the same length.
pub const FIRST_PROMPT_MAX_CHARS: usize = 200;
/// Longest first prompt shown as a title, in chars, before an ellipsis.
pub const TITLE_PROMPT_MAX_CHARS: usize = 48;

impl SessionSummary {
    /// Summary of a session whose first seen event is `event` at `seq`.
    pub fn new(seq: Seq, event: &Event) -> Self {
        let mut summary = SessionSummary {
            id: event.session_id,
            workdir: None,
            state: SessionState::Idle,
            pending_approvals: Vec::new(),
            last_activity: event.at,
            last_seq: seq,
            stopped: false,
            notable: 0,
            name: None,
            first_prompt: None,
            archived: false,
            usage: None,
            agent: AgentSettings::default(),
            work: AgentWork::default(),
            context: ContextId::first_of(event.session_id),
        };
        summary.apply(seq, event);
        summary
    }

    /// Folds one event of this session into the summary.
    ///
    /// A rename only sets the name, an archive only the archived flag, a
    /// mode or model event only the agent settings, a change of background
    /// tasks or schedules only the agent's work, a context cleared or
    /// switched only the current context, and the models an agent offers
    /// or a context moved away nothing here: none is activity, so they move
    /// neither `last_seq` nor `last_activity` and the session keeps its
    /// place in the list.
    pub fn apply(&mut self, seq: Seq, event: &Event) {
        debug_assert_eq!(event.session_id, self.id, "event of another session");
        if let Some(archived) = event.kind.archived_effect() {
            self.archived = archived;
        }
        if AgentSettings::folds(&event.kind) {
            self.agent.apply(event.command_id, &event.kind);
            return;
        }
        if AgentWork::folds(&event.kind) {
            self.work.apply(&event.kind);
            return;
        }
        match &event.kind {
            EventKind::SessionRenamed { name, .. } => {
                self.name = name.clone();
                return;
            }
            EventKind::ContextStarted { .. } | EventKind::ContextSwitched { .. } => {
                self.context = event.context;
                return;
            }
            EventKind::SessionArchived { .. }
            | EventKind::ModelsOffered { .. }
            | EventKind::ContextMoved { .. } => return,
            _ => {}
        }
        self.last_seq = seq;
        self.last_activity = event.at;
        if is_notable(&event.kind) {
            self.notable += 1;
        }
        match &event.kind {
            EventKind::SessionCreated { workdir, .. } => {
                self.workdir = Some(workdir.clone());
                self.context = event.context;
            }
            EventKind::SessionStateChanged(state) => self.state = state.clone(),
            EventKind::ApprovalRequested { id, .. } => {
                if !self.pending_approvals.contains(id) {
                    self.pending_approvals.push(*id);
                }
            }
            EventKind::ApprovalResolved { id, .. } | EventKind::ApprovalCancelled { id, .. } => {
                self.pending_approvals.retain(|pending| pending != id);
            }
            // The next message starts the agent again.
            EventKind::UserMessage { text, .. } => {
                self.stopped = false;
                if self.first_prompt.is_none() {
                    self.first_prompt = first_line(text, FIRST_PROMPT_MAX_CHARS);
                }
            }
            EventKind::SessionStopped => self.stopped = true,
            EventKind::TurnUsage(turn) => match &mut self.usage {
                Some(usage) => usage.add(turn),
                None => self.usage = Some(SessionUsage::new(turn)),
            },
            EventKind::AgentMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. }
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
            | EventKind::ContextSwitched { .. }
            | EventKind::ContextMoved { .. }
            | EventKind::BackgroundTasksChanged { .. }
            | EventKind::SchedulesChanged { .. } => {}
        }
    }

    /// The approval the session is blocked on, if any (the oldest one).
    pub fn pending_approval(&self) -> Option<ApprovalId> {
        self.pending_approvals.first().copied()
    }

    /// What to call the session wherever it is shown: its name, else the
    /// [default title](Self::default_title).
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.default_title())
    }

    /// The title of an unnamed session: the first line of its first
    /// message, cut to [`TITLE_PROMPT_MAX_CHARS`] with an ellipsis, else
    /// the last component of its directory, else the end of its id.
    pub fn default_title(&self) -> String {
        if let Some(prompt) = self.first_prompt.as_deref()
            && let Some(line) = first_line(prompt, usize::MAX)
        {
            return if line.chars().count() > TITLE_PROMPT_MAX_CHARS {
                let head: String = line.chars().take(TITLE_PROMPT_MAX_CHARS - 1).collect();
                format!("{}…", head.trim_end())
            } else {
                line
            };
        }
        self.workdir
            .as_deref()
            .and_then(|dir| dir.trim_end_matches('/').rsplit('/').next())
            .filter(|name| !name.is_empty())
            .map_or_else(
                || self.id.0.simple().to_string()[24..].to_owned(),
                str::to_owned,
            )
    }
}

/// The first line of `text` with leading blank lines skipped, trimmed and
/// cut to `max` chars; `None` if `text` is blank.
pub(crate) fn first_line(text: &str, max: usize) -> Option<String> {
    let line = text.trim_start().lines().next()?.trim_end();
    Some(line.chars().take(max).collect())
}

/// Whether an event is news for whoever was away: the agent's output (a
/// message, an approval request, an error). Messages from any device never
/// are. The store's backfill migration counts the same kinds.
pub fn is_notable(kind: &EventKind) -> bool {
    match kind {
        EventKind::AgentMessage { .. } | EventKind::ApprovalRequested { .. } => true,
        EventKind::SessionStateChanged(state) => matches!(state, SessionState::Errored { .. }),
        EventKind::SessionCreated { .. }
        | EventKind::UserMessage { .. }
        | EventKind::ToolCall(_)
        | EventKind::ToolResult(_)
        | EventKind::ApprovalResolved { .. }
        | EventKind::ApprovalCancelled { .. }
        | EventKind::TurnInterrupted
        | EventKind::SessionStopped
        | EventKind::SessionRenamed { .. }
        | EventKind::SessionArchived { .. }
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
        | EventKind::ContextSwitched { .. }
        | EventKind::ContextMoved { .. }
        | EventKind::BackgroundTasksChanged { .. }
        | EventKind::SchedulesChanged { .. } => false,
    }
}

/// The blobs an event references: a tool call's input and the whole of its
/// display body, a result's output, an approval's input.
pub(crate) fn blob_refs(kind: &EventKind) -> impl Iterator<Item = &BlobRef> {
    let (first, second) = match kind {
        EventKind::ToolCall(ToolCall { input, display, .. }) => (
            input.as_ref(),
            display.body.as_ref().and_then(|body| body.full.as_ref()),
        ),
        EventKind::ApprovalRequested { input, .. } => (input.as_ref(), None),
        EventKind::ToolResult(ToolResult { output, .. }) => (output.as_ref(), None),
        EventKind::SessionCreated { .. }
        | EventKind::UserMessage { .. }
        | EventKind::AgentMessage { .. }
        | EventKind::ApprovalResolved { .. }
        | EventKind::ApprovalCancelled { .. }
        | EventKind::TurnInterrupted
        | EventKind::SessionStopped
        | EventKind::SessionRenamed { .. }
        | EventKind::SessionArchived { .. }
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
        | EventKind::ContextSwitched { .. }
        | EventKind::ContextMoved { .. }
        | EventKind::BackgroundTasksChanged { .. }
        | EventKind::SchedulesChanged { .. } => (None, None),
    };
    first.into_iter().chain(second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use skywave_proto::{Decision, DeviceId};

    fn event(session_id: SessionId, minute: u32, kind: EventKind) -> Event {
        Event {
            session_id,
            at: Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap(),
            command_id: kind.logged_by_command().then(skywave_proto::CommandId::new),
            context: ContextId::first_of(session_id),
            kind,
        }
    }

    #[test]
    fn the_current_context_follows_starts_and_switches() {
        let sid = SessionId::new();
        let first = ContextId::first_of(sid);
        let mut summary = SessionSummary::new(
            Seq(1),
            &event(
                sid,
                0,
                EventKind::SessionCreated {
                    workdir: "/srv/x".into(),
                    context_name: "start".into(),
                    from: None,
                },
            ),
        );
        assert_eq!(summary.context, first);
        let by = DeviceId::new();
        let fresh = ContextId::new();
        let in_context = |context, kind| Event {
            context,
            ..event(sid, 1, kind)
        };
        summary.apply(
            Seq(2),
            &in_context(
                fresh,
                EventKind::ContextStarted {
                    by,
                    name: "fresh".into(),
                },
            ),
        );
        assert_eq!(summary.context, fresh);
        // Other events leave it, whatever their stamp.
        summary.apply(
            Seq(3),
            &in_context(first, EventKind::AgentMessage { text: "x".into() }),
        );
        assert_eq!(summary.context, fresh);
        summary.apply(
            Seq(4),
            &in_context(first, EventKind::ContextSwitched { by }),
        );
        assert_eq!(summary.context, first);
    }

    #[test]
    fn folds_workdir_state_and_approvals() {
        let sid = SessionId::new();
        let (a1, a2) = (ApprovalId::new(), ApprovalId::new());
        let created = event(
            sid,
            0,
            EventKind::SessionCreated {
                workdir: "/srv/x".into(),
                context_name: "start".into(),
                from: None,
            },
        );
        let mut summary = SessionSummary::new(Seq(1), &created);
        assert_eq!(summary.workdir.as_deref(), Some("/srv/x"));
        assert_eq!(summary.state, SessionState::Idle);

        let steps = [
            EventKind::SessionStateChanged(SessionState::AgentWorking),
            EventKind::ApprovalRequested {
                id: a1,
                call: None,
                tool: "bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions: Vec::new(),
            },
            EventKind::ApprovalRequested {
                id: a2,
                call: None,
                tool: "bash".into(),
                summary: "pwd".into(),
                input: None,
                suggestions: Vec::new(),
            },
        ];
        for (i, kind) in steps.into_iter().enumerate() {
            let n = u32::try_from(i).unwrap() + 1;
            summary.apply(Seq(u64::from(n) + 1), &event(sid, n, kind));
        }
        assert_eq!(summary.state, SessionState::AgentWorking);
        assert_eq!(summary.pending_approval(), Some(a1));
        assert_eq!(summary.notable, 2, "both approval requests are news");

        let resolved = EventKind::ApprovalResolved {
            id: a1,
            decision: Decision::Approve,
            by: DeviceId::new(),
            rule: None,
        };
        summary.apply(Seq(5), &event(sid, 5, resolved));
        assert_eq!(summary.pending_approval(), Some(a2));

        let cancelled = EventKind::ApprovalCancelled {
            id: a2,
            reason: "daemon restarted".into(),
        };
        summary.apply(Seq(6), &event(sid, 6, cancelled));
        assert_eq!(summary.pending_approval(), None);
        assert_eq!(summary.last_seq, Seq(6));
        assert_eq!(
            summary.last_activity,
            event(sid, 6, EventKind::TurnInterrupted).at
        );
    }

    #[test]
    fn title_is_the_directory_name_else_the_id() {
        let sid = SessionId::new();
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, EventKind::TurnInterrupted));
        let short = sid.0.simple().to_string()[24..].to_owned();
        assert_eq!(summary.title(), short);
        for (workdir, title) in [("/srv/app", "app"), ("/srv/app/", "app"), ("app", "app")] {
            summary.workdir = Some(workdir.into());
            assert_eq!(summary.title(), title);
        }
        summary.workdir = Some("/".into());
        assert_eq!(summary.title(), short);
    }

    #[test]
    fn title_is_the_name_else_the_first_prompt_else_the_directory() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert_eq!(summary.title(), "app");

        let message = |text: &str| EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: text.into(),
        };
        summary.apply(
            Seq(2),
            &event(sid, 1, message("\n  fix the flaky test  \nin ci")),
        );
        assert_eq!(summary.first_prompt.as_deref(), Some("fix the flaky test"));
        summary.apply(Seq(3), &event(sid, 2, message("and then this")));
        assert_eq!(
            summary.title(),
            "fix the flaky test",
            "the first prompt stays"
        );

        let rename = |name: Option<&str>| EventKind::SessionRenamed {
            name: name.map(str::to_owned),
            by: DeviceId::new(),
        };
        summary.apply(Seq(4), &event(sid, 3, rename(Some("flaky"))));
        assert_eq!(summary.title(), "flaky");
        assert_eq!(summary.default_title(), "fix the flaky test");
        summary.apply(Seq(5), &event(sid, 4, rename(None)));
        assert_eq!(
            summary.title(),
            "fix the flaky test",
            "no name restores the default"
        );

        summary.first_prompt = Some("x".repeat(60));
        assert_eq!(summary.title(), format!("{}…", "x".repeat(47)));
        assert_eq!(summary.title().chars().count(), TITLE_PROMPT_MAX_CHARS);
    }

    #[test]
    fn a_rename_is_not_activity() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let renamed = EventKind::SessionRenamed {
            name: Some("x".into()),
            by: DeviceId::new(),
        };
        assert!(!is_notable(&renamed));
        summary.apply(Seq(9), &event(sid, 30, renamed));
        assert_eq!(summary.name.as_deref(), Some("x"));
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(
            summary.last_activity,
            event(sid, 0, EventKind::TurnInterrupted).at
        );
        assert_eq!(summary.notable, 0);
    }

    #[test]
    fn a_context_cleared_or_switched_is_not_activity() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        let first = ContextId::first_of(sid);
        assert_eq!(summary.context, first);
        let by = DeviceId::new();
        let cleared = ContextId::new();
        let started = EventKind::ContextStarted {
            by,
            name: "velvet walrus".into(),
        };
        assert!(!is_notable(&started));
        summary.apply(
            Seq(5),
            &Event {
                context: cleared,
                ..event(sid, 20, started)
            },
        );
        assert_eq!(summary.context, cleared);
        summary.apply(
            Seq(9),
            &Event {
                context: first,
                ..event(sid, 30, EventKind::ContextSwitched { by })
            },
        );
        assert_eq!(summary.context, first);
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(
            summary.last_activity,
            event(sid, 0, EventKind::TurnInterrupted).at
        );
        assert_eq!(summary.notable, 0);
        // A positive control: a message is activity.
        let message = EventKind::UserMessage {
            device_id: by,
            text: "hi".into(),
        };
        summary.apply(Seq(10), &event(sid, 40, message));
        assert_eq!(summary.last_seq, Seq(10));
    }

    #[test]
    fn archiving_is_not_activity_and_a_message_brings_it_back() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert!(!summary.archived);
        let archive = |archived| EventKind::SessionArchived {
            archived,
            by: DeviceId::new(),
        };
        assert!(!is_notable(&archive(true)));
        summary.apply(Seq(5), &event(sid, 10, archive(true)));
        assert!(summary.archived);
        assert_eq!(summary.last_seq, Seq(1));
        assert_eq!(summary.notable, 0);
        summary.apply(Seq(6), &event(sid, 11, archive(true)));
        assert!(summary.archived, "absolute: a repeat does not flip it");
        let message = EventKind::UserMessage {
            device_id: DeviceId::new(),
            text: "back".into(),
        };
        summary.apply(Seq(7), &event(sid, 12, message));
        assert!(!summary.archived);
        assert_eq!(summary.last_seq, Seq(7));
        summary.apply(Seq(8), &event(sid, 13, archive(true)));
        summary.apply(Seq(9), &event(sid, 14, archive(false)));
        assert!(!summary.archived);
    }

    #[test]
    fn turn_usage_sums_what_each_turn_reports() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
            context_name: "start".into(),
            from: None,
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert_eq!(summary.usage, None);

        let first = TurnUsage {
            duration_ms: Some(12_400),
            tokens_in: Some(3_100),
            tokens_out: Some(812),
            cost_usd: Some(0.25),
            model: Some("model-a".into()),
        };
        let kind = EventKind::TurnUsage(first.clone());
        assert!(!is_notable(&kind));
        assert_eq!(kind.archived_effect(), None);
        summary.apply(Seq(2), &event(sid, 1, kind));
        let usage = summary.usage.clone().unwrap();
        assert_eq!((&usage.last_turn, &usage.total), (&first, &first));
        assert_eq!(summary.last_seq, Seq(2), "part of the turn's end");
        assert_eq!(summary.notable, 0);

        // A turn missing figures adds nothing to them, and an earlier model
        // stays until another is named.
        let second = TurnUsage {
            duration_ms: Some(600),
            cost_usd: Some(0.5),
            ..TurnUsage::default()
        };
        summary.apply(Seq(3), &event(sid, 2, EventKind::TurnUsage(second.clone())));
        let third = TurnUsage {
            tokens_out: Some(188),
            model: Some("model-b".into()),
            ..TurnUsage::default()
        };
        summary.apply(Seq(4), &event(sid, 3, EventKind::TurnUsage(third.clone())));
        let usage = summary.usage.unwrap();
        assert_eq!(usage.last_turn, third);
        assert_eq!(
            usage.total,
            TurnUsage {
                duration_ms: Some(13_000),
                tokens_in: Some(3_100),
                tokens_out: Some(1_000),
                cost_usd: Some(0.75),
                model: Some("model-b".into()),
            }
        );
    }

    #[test]
    fn a_tool_call_references_its_input_and_its_whole_body() {
        let blob = |size| BlobRef {
            id: skywave_proto::BlobId::new(),
            size,
        };
        let (input, full) = (blob(10), blob(20));
        let call = EventKind::ToolCall(ToolCall {
            call: Some(skywave_proto::CallId::new()),
            name: "Edit".into(),
            display: skywave_proto::ToolDisplay {
                kind: skywave_proto::ToolKind::FileEdit,
                title: "a.rs".into(),
                detail: None,
                body: Some(skywave_proto::Body {
                    format: skywave_proto::BodyFormat::Diff,
                    preview: "-a\n+b".into(),
                    lines: 2,
                    full: Some(full.clone()),
                }),
            },
            input: Some(input.clone()),
        });
        assert_eq!(blob_refs(&call).collect::<Vec<_>>(), vec![&input, &full]);
        assert_eq!(blob_refs(&EventKind::TurnInterrupted).count(), 0);
    }
}
