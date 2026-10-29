//! Per-session summaries derived from the event log.
//!
//! The session list reads these instead of replaying every event. The fold is
//! pure: the store persists its result in the same transaction that stores
//! the events, so the projection never runs ahead of or behind the log.

use chrono::{DateTime, Utc};
use skywave_proto::{ApprovalId, BlobRef, Event, EventKind, Seq, SessionId, SessionState};

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
}

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
        };
        summary.apply(seq, event);
        summary
    }

    /// Folds one event of this session into the summary.
    pub fn apply(&mut self, seq: Seq, event: &Event) {
        debug_assert_eq!(event.session_id, self.id, "event of another session");
        self.last_seq = seq;
        self.last_activity = event.at;
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
            EventKind::UserMessage { .. }
            | EventKind::AgentMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
            | EventKind::TurnInterrupted => {}
        }
    }

    /// The approval the session is blocked on, if any (the oldest one).
    pub fn pending_approval(&self) -> Option<ApprovalId> {
        self.pending_approvals.first().copied()
    }
}

/// The blob an event references, if any.
pub(crate) fn blob_ref(kind: &EventKind) -> Option<&BlobRef> {
    match kind {
        EventKind::ToolCall { input, .. } | EventKind::ApprovalRequested { input, .. } => {
            input.as_ref()
        }
        EventKind::ToolResult { output, .. } => output.as_ref(),
        EventKind::SessionCreated { .. }
        | EventKind::UserMessage { .. }
        | EventKind::AgentMessage { .. }
        | EventKind::ApprovalResolved { .. }
        | EventKind::ApprovalCancelled { .. }
        | EventKind::TurnInterrupted
        | EventKind::SessionStateChanged(_) => None,
    }
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
                tool: "bash".into(),
                summary: "ls".into(),
                input: None,
            },
            EventKind::ApprovalRequested {
                id: a2,
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
}
