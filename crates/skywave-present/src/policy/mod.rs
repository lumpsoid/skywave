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
//! - [`PromptHistory`]: recalling earlier prompts with a stash.
//! - [`CompletionSettle`]: workdir completion.
//! - [`TimelineSearch`]: the timeline search's query, matches and current
//!   match.
//!
//! And small decisions: [`unanswered`], [`dismissable`],
//! [`looks_like_pair_uri`].

mod alert;
mod completion;
mod draft;
mod history;
mod read;
mod search;

pub use alert::{Alert, AlertWatcher, TITLE_MAX, sanitise};
pub use completion::CompletionSettle;
pub use draft::{DRAFT_SAVE_AFTER, DraftSaver};
pub use history::{PromptHistory, Recall};
pub use read::{READ_MARK_EVERY, ReadMarker};
pub use search::{Jump, TimelineSearch};

use skywave_core::{ApprovalPrompt, Outgoing, PendingSession, SessionListView};

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

/// Whether `text` is meant as a pairing URI (spec §6), so a malformed one
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
        CommandId, DeviceId, DeviceRef, Entry, Seq, SessionId, SessionState, SessionView,
        TimelineItem,
    };

    use super::*;

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
            interrupt: None,
            stop: None,
            draft: None,
            draft_rev: 0,
            last_seq,
            unread_from: None,
        }
    }

    pub fn logged(seq: u64, entry: Entry) -> TimelineItem {
        TimelineItem::Logged {
            seq: Seq(seq),
            at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
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
