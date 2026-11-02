//! Head-side policies every head drives the same way, as clock-free
//! controllers: none reads the clock or does I/O. Time comes in as `now`;
//! each says what to do (core [`Action`](skywave_core::Action)s to
//! dispatch, an alert, text to show) and, where it waits on time, when to
//! wake it next (`next_wake`), so a terminal loop's timeout and a hub's
//! single timer arm both work from it.
//!
//! - [`DraftSaver`]: debounced draft saves, forced before leaving, retried
//!   on failure.
//! - [`ReadMarker`]: throttled read marks while the newest item is on
//!   screen, forced on leaving.
//! - [`AlertWatcher`]: an alert when new approvals come in.
//! - [`ShellAlertWatcher`]: a notice when a long shell run of this
//!   device's finished.
//! - [`PromptHistory`]: recalling earlier prompts, or shell commands, with
//!   a stash.
//! - [`CompletionSettle`]: workdir completion.
//! - [`TimelineSearch`]: the timeline search's query, matches and current
//!   match.
//!
//! And small decisions: [`unanswered`], [`dismissable`],
//! [`looks_like_pair_uri`], [`shell_stop`].

mod alert;
mod completion;
mod draft;
mod history;
mod read;
mod search;
mod shell;

pub use alert::{Alert, AlertWatcher, ShellAlertWatcher, ShellNotice, TITLE_MAX, sanitise};
pub use completion::CompletionSettle;
pub use draft::{DRAFT_SAVE_AFTER, DraftSaver};
pub use history::{PromptHistory, Recall};
pub use read::{READ_MARK_EVERY, ReadMarker};
pub use search::{Jump, TimelineSearch};
pub use shell::shell_stop;

use skywave_core::{AgentWork, ApprovalPrompt, Outgoing, PendingSession, SessionListView};

/// This device has not answered `prompt`, or its answer was refused in a
/// way that leaves it open to another: it may be answered (again), and a
/// rule list opened for it stays up.
pub fn unanswered(prompt: &ApprovalPrompt) -> bool {
    prompt.answer.as_ref().is_none_or(|answer| answer.retryable)
}

/// The refused session creation a dismissal takes off the list: the
/// oldest.
pub fn dismissable(list: &SessionListView) -> Option<&PendingSession> {
    list.creating
        .iter()
        .find(|p| p.discardable && matches!(p.outgoing, Outgoing::Failed(_)))
}

/// Whether stopping the agent is worth a confirmation: it runs a
/// background task (an ambient one is not activity) or has a schedule. The
/// same rule covers Stop agent, clear, switch and move of the current
/// context, which all stop the agent. A schedule is paused, not ended, by
/// the stop; it still earns the warning.
pub fn stop_needs_confirmation(work: &AgentWork) -> bool {
    work.tasks.iter().any(|task| !task.ambient) || !work.schedules.is_empty()
}

/// Whether `text` is meant as a pairing URI (architecture.md "Pairing"), so a malformed one
/// is reported rather than typed into a field.
pub fn looks_like_pair_uri(text: &str) -> bool {
    let scheme = "skywave://";
    text.trim()
        .get(..scheme.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{TimeZone, Utc};
    use skywave_core::{
        CommandId, ContextId, DeviceId, DeviceRef, Entry, Seq, SessionId, SessionState,
        SessionView, TimelineItem,
    };

    use super::*;

    #[test]
    fn a_stop_is_worth_confirming_for_activity_only() {
        use skywave_core::{BackgroundTask, Schedule, ScheduleId, TaskId, TaskKind};
        let task = |ambient| BackgroundTask {
            call: None,
            id: TaskId("t".into()),
            kind: TaskKind::Other,
            title: "t".into(),
            parent: None,
            ambient,
        };
        let schedule = |outlives_agent| Schedule {
            id: ScheduleId("s".into()),
            when: "5m".into(),
            recurring: false,
            prompt: "p".into(),
            outlives_agent,
        };
        let work = |tasks, schedules| AgentWork { tasks, schedules };
        assert!(!stop_needs_confirmation(&work(vec![], vec![])));
        assert!(!stop_needs_confirmation(&work(vec![task(true)], vec![])));
        assert!(stop_needs_confirmation(&work(vec![task(false)], vec![])));
        assert!(stop_needs_confirmation(&work(vec![], vec![schedule(None)])));
        assert!(stop_needs_confirmation(&work(
            vec![],
            vec![schedule(Some(true))]
        )));
    }

    pub fn session(timeline: Vec<TimelineItem>) -> SessionView {
        let last_seq = timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged { seq, .. } => Some(*seq),
                TimelineItem::Pending { .. } => None,
            })
            .max()
            .unwrap_or(Seq(0));
        SessionView {
            subagent: None,
            id: SessionId::new(),
            title: "x".into(),
            named: false,
            rename: None,
            archived: false,
            workdir: None,
            state: SessionState::Idle,
            timeline,
            pending_approval: None,
            rules: Vec::new(),
            usage: None,
            agent: Default::default(),
            contexts: Vec::new(),
            context: ContextId::new(),
            viewing: None,
            can_clear: false,
            clear: None,
            switch: None,
            interrupt: None,
            stop: None,
            work: Default::default(),
            task_stops: Vec::new(),
            mode: skywave_core::ComposerMode::Agent,
            draft: None,
            shell_draft: None,
            draft_rev: 0,
            shell: Default::default(),
            last_seq,
            unread_from: None,
        }
    }

    pub fn logged(seq: u64, entry: Entry) -> TimelineItem {
        TimelineItem::Logged {
            parent: None,
            seq: Seq(seq),
            at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            context: ContextId::new(),
            entry,
        }
    }

    pub fn message(seq: u64, text: &str) -> TimelineItem {
        logged(
            seq,
            Entry::UserMessage {
                command_id: CommandId::new(),
                text: text.to_owned(),
                from: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: true,
                },
                outgoing: None,
                delivery: None,
            },
        )
    }

    #[test]
    fn pair_uris_are_told_by_their_scheme() {
        assert!(looks_like_pair_uri("  SkyWave://host?x"));
        assert!(!looks_like_pair_uri("skywave:/host"));
        assert!(!looks_like_pair_uri("é"));
    }
}
