//! Conversation only: a session's timeline without the agent's tool calls,
//! so the messages stand out to someone skimming back through a long turn.
//! The user switches it on and off; nothing here guesses when to.
//!
//! A head narrows every snapshot of the open session through
//! [`ConversationOnly::narrow`] before it looks at the timeline, so its
//! selection, turns and search all work on what is on screen.

use std::mem;

use skywave_core::{ApprovalStatus, Entry, ImportedEntry, SessionView, TimelineItem};

use crate::sections::{ItemKey, selectable};

/// Whether `item` shows while only the conversation does: everything but
/// tool calls and their results. A call whose approval is pending stays,
/// as the user has to answer it.
pub fn in_conversation(item: &TimelineItem) -> bool {
    let TimelineItem::Logged { entry, .. } = item else {
        return true;
    };
    match entry {
        Entry::Tool(view) => view
            .approval
            .as_ref()
            .is_some_and(|approval| matches!(approval.status, ApprovalStatus::Pending)),
        Entry::ToolCall { .. }
        | Entry::ToolResult { .. }
        | Entry::Imported {
            entry: ImportedEntry::Tool(_) | ImportedEntry::ToolResult { .. },
            ..
        } => false,
        _ => true,
    }
}

/// Whether the open session shows only its conversation and, while it
/// does, the whole timeline of the snapshot last narrowed, to put back.
#[derive(Debug, Clone, Default)]
pub struct ConversationOnly {
    on: bool,
    full: Vec<TimelineItem>,
}

impl ConversationOnly {
    pub fn on(&self) -> bool {
        self.on
    }

    /// Narrows a new snapshot of the open session to the conversation while
    /// it is on; leaves it whole while off.
    pub fn narrow(&mut self, view: &mut SessionView) {
        if self.on {
            self.full = mem::take(&mut view.timeline);
            view.timeline = self
                .full
                .iter()
                .filter(|i| in_conversation(i))
                .cloned()
                .collect();
        }
    }

    /// Switches it on or off, narrowing the open session's `view` (`None`
    /// off a session) or putting its whole timeline back.
    pub fn toggle(&mut self, view: Option<&mut SessionView>) {
        self.on = !self.on;
        match view {
            Some(view) if !self.on => view.timeline = mem::take(&mut self.full),
            Some(view) => self.narrow(view),
            None => self.full.clear(),
        }
    }

    /// What stands in for the item `key` names now that only the
    /// conversation shows: the item itself if it shows, else the nearest
    /// selectable item that shows before it, else after it. Keeps the
    /// selection and the scroll near where they were when the conversation
    /// narrows; `key` unchanged while off or if it is not in the timeline.
    pub fn stand_in(&self, key: ItemKey) -> Option<ItemKey> {
        let full = &self.full;
        let Some(at) = full.iter().position(|item| ItemKey::of(item) == key) else {
            return Some(key);
        };
        if !self.on || in_conversation(&full[at]) {
            return Some(key);
        }
        let shown = |item: &&TimelineItem| in_conversation(item) && selectable(item).is_some();
        full[..at]
            .iter()
            .rev()
            .find(shown)
            .or_else(|| full[at..].iter().find(shown))
            .map(ItemKey::of)
    }
}

#[cfg(test)]
mod tests {
    use skywave_core::{ApprovalId, CallId, ToolApproval, ToolKind, ToolOutcome, ToolView};

    use super::*;
    use crate::policy::tests::{logged, message, session};

    fn tool(seq: u64, status: Option<ApprovalStatus>) -> TimelineItem {
        logged(
            seq,
            Entry::Tool(Box::new(ToolView {
                call: CallId::new(),
                name: "Bash".into(),
                kind: ToolKind::Shell,
                title: "$ make".into(),
                detail: None,
                body: None,
                input: None,
                approval: status.map(|status| ToolApproval {
                    id: ApprovalId::new(),
                    status,
                    answer: None,
                }),
                outcome: ToolOutcome::Running {
                    since: chrono::Utc::now(),
                },
                subagent: None,
            })),
        )
    }

    fn agent(seq: u64, text: &str) -> TimelineItem {
        logged(
            seq,
            Entry::AgentMessage {
                text: text.to_owned(),
            },
        )
    }

    fn seqs(view: &SessionView) -> Vec<u64> {
        view.timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Logged { seq, .. } => Some(seq.0),
                TimelineItem::Pending { .. } => None,
            })
            .collect()
    }

    #[test]
    fn tools_hide_but_one_waiting_on_its_approval() {
        assert!(in_conversation(&message(1, "go")));
        assert!(in_conversation(&agent(2, "done")));
        assert!(!in_conversation(&tool(3, None)));
        assert!(in_conversation(&tool(4, Some(ApprovalStatus::Pending))));
        assert!(!in_conversation(&tool(
            5,
            Some(ApprovalStatus::Cancelled { reason: "x".into() })
        )));
    }

    #[test]
    fn toggling_narrows_each_snapshot_and_puts_the_timeline_back() {
        let timeline = || {
            vec![
                message(1, "go"),
                tool(2, None),
                tool(3, None),
                agent(4, "done"),
            ]
        };
        let mut only = ConversationOnly::default();
        let mut view = session(timeline());
        only.narrow(&mut view);
        assert_eq!(seqs(&view), [1, 2, 3, 4], "off leaves it whole");

        only.toggle(Some(&mut view));
        assert!(only.on());
        assert_eq!(seqs(&view), [1, 4]);

        let mut next = session(timeline());
        next.timeline.push(tool(5, None));
        only.narrow(&mut next);
        assert_eq!(seqs(&next), [1, 4]);

        only.toggle(Some(&mut next));
        assert!(!only.on());
        assert_eq!(seqs(&next), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_hidden_item_hands_over_to_the_message_before_it() {
        let mut only = ConversationOnly::default();
        let mut view = session(vec![tool(1, None), message(2, "go"), tool(3, None)]);
        only.toggle(Some(&mut view));
        let key = |seq| ItemKey::Logged(skywave_core::Seq(seq));
        assert_eq!(only.stand_in(key(3)), Some(key(2)));
        assert_eq!(only.stand_in(key(1)), Some(key(2)), "none before: after");
        assert_eq!(only.stand_in(key(2)), Some(key(2)));
        only.toggle(Some(&mut view));
        assert_eq!(only.stand_in(key(3)), Some(key(3)));
    }
}
