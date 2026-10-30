//! Per-session summaries derived from the event log.
//!
//! The session list reads these instead of replaying every event. The fold is
//! pure: the store persists its result in the same transaction that stores
//! the events, so the projection never runs ahead of or behind the log.

use chrono::{DateTime, Utc};
use skywave_proto::{
    ApprovalId, BlobRef, Event, EventKind, Seq, SessionId, SessionState, ToolCall, ToolResult,
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
        };
        summary.apply(seq, event);
        summary
    }

    /// Folds one event of this session into the summary.
    ///
    /// A rename only sets the name and an archive only the archived flag:
    /// neither is activity, so they move neither `last_seq` nor
    /// `last_activity` and the session keeps its place in the list.
    pub fn apply(&mut self, seq: Seq, event: &Event) {
        debug_assert_eq!(event.session_id, self.id, "event of another session");
        if let Some(archived) = event.kind.archived_effect() {
            self.archived = archived;
        }
        match &event.kind {
            EventKind::SessionRenamed { name, .. } => {
                self.name = name.clone();
                return;
            }
            EventKind::SessionArchived { .. } => return,
            _ => {}
        }
        self.last_seq = seq;
        self.last_activity = event.at;
        if is_notable(&event.kind) {
            self.notable += 1;
        }
        match &event.kind {
            EventKind::SessionCreated { workdir } => self.workdir = Some(workdir.clone()),
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
            EventKind::AgentMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
            | EventKind::TurnInterrupted
            | EventKind::SessionRenamed { .. }
            | EventKind::SessionArchived { .. } => {}
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
fn first_line(text: &str, max: usize) -> Option<String> {
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
        | EventKind::SessionArchived { .. } => false,
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
        | EventKind::SessionStateChanged(_) => (None, None),
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
            kind,
        }
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
            },
            EventKind::ApprovalRequested {
                id: a2,
                call: None,
                tool: "bash".into(),
                summary: "pwd".into(),
                input: None,
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
        };
        let mut summary = SessionSummary::new(Seq(1), &event(sid, 0, created));
        assert_eq!(summary.title(), "app");

        let message = |text: &str| EventKind::UserMessage {
            command_id: skywave_proto::CommandId::new(),
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
    fn archiving_is_not_activity_and_a_message_brings_it_back() {
        let sid = SessionId::new();
        let created = EventKind::SessionCreated {
            workdir: "/srv/app".into(),
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
            command_id: skywave_proto::CommandId::new(),
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
