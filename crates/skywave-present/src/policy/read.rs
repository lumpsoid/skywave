//! When the open session is marked read: what is on screen at the bottom
//! is seen, but an agent streaming many small batches must not cost a
//! durable write each.

use std::time::{Duration, Instant};

use skywave_core::{Action, Seq, SessionId};

/// Least time between two read marks of the open session while it follows
/// new items: an agent streaming many small batches must not cost a durable
/// write each. Leaving the session or quitting marks at once.
pub const READ_MARK_EVERY: Duration = Duration::from_secs(2);

/// Throttles the open session's read marks. Where the timeline is scrolled
/// is the head's: it says whether the newest item is on screen
/// (`following`), with the session's
/// [`last_seq`](skywave_core::SessionView::last_seq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadMarker {
    session_id: SessionId,
    /// What the last mark marked read through, and when.
    marked: Option<(Seq, Instant)>,
}

impl ReadMarker {
    pub fn new(session_id: SessionId) -> ReadMarker {
        ReadMarker {
            session_id,
            marked: None,
        }
    }

    /// Something to mark: the newest item is on screen (`following`; false
    /// too while the opening scroll is not settled), so everything up to
    /// `last_seq` is seen, and that is past what was last marked read.
    fn unmarked(&self, last_seq: Seq, following: bool) -> bool {
        following && self.marked.is_none_or(|(seq, _)| last_seq > seq)
    }

    /// The mark to dispatch, through `last_seq`, when there is something to
    /// mark: at once if `force` (leaving the session, quitting) or for the
    /// first mark, else at most once per [`READ_MARK_EVERY`].
    pub fn mark(
        &mut self,
        now: Instant,
        last_seq: Seq,
        following: bool,
        force: bool,
    ) -> Option<Action> {
        if !self.unmarked(last_seq, following) {
            return None;
        }
        if let Some((_, at)) = self.marked
            && !force
            && now.saturating_duration_since(at) < READ_MARK_EVERY
        {
            return None;
        }
        self.marked = Some((last_seq, now));
        Some(Action::MarkRead {
            session_id: self.session_id,
            through: last_seq,
        })
    }

    /// When an unforced [`ReadMarker::mark`] has something to mark: `now`
    /// for a first mark; `None` while there is nothing to mark.
    pub fn next_wake(&self, now: Instant, last_seq: Seq, following: bool) -> Option<Instant> {
        if !self.unmarked(last_seq, following) {
            return None;
        }
        Some(self.marked.map_or(now, |(_, at)| at + READ_MARK_EVERY))
    }

    /// Whether what was seen is marked read (forced) before dispatching
    /// `action`: before anything that leaves the session.
    pub fn marks_before(action: &Action) -> bool {
        matches!(
            action,
            Action::Navigate(_) | Action::Back | Action::OpenSession(_) | Action::OpenNextApproval
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through(action: Option<Action>) -> Option<u64> {
        match action? {
            Action::MarkRead { through, .. } => Some(through.0),
            other => panic!("expected a read mark, got {other:?}"),
        }
    }

    #[test]
    fn marks_are_throttled_while_following() {
        let mut marker = ReadMarker::new(SessionId::new());
        let t0 = Instant::now();
        assert_eq!(
            marker.next_wake(t0, Seq(3), true),
            Some(t0),
            "first: at once"
        );
        assert_eq!(through(marker.mark(t0, Seq(3), true, false)), Some(3));
        assert_eq!(marker.next_wake(t0, Seq(3), true), None, "all marked");

        // More news comes in: the next mark waits out the throttle.
        assert_eq!(
            marker.next_wake(t0, Seq(4), true),
            Some(t0 + READ_MARK_EVERY)
        );
        let half = t0 + READ_MARK_EVERY / 2;
        assert_eq!(marker.mark(half, Seq(4), true, false), None);
        assert_eq!(
            through(marker.mark(t0 + READ_MARK_EVERY, Seq(4), true, false)),
            Some(4)
        );
    }

    #[test]
    fn nothing_is_marked_while_not_following() {
        let mut marker = ReadMarker::new(SessionId::new());
        let t0 = Instant::now();
        assert_eq!(marker.next_wake(t0, Seq(3), false), None);
        assert_eq!(marker.mark(t0, Seq(3), false, false), None);
        assert_eq!(marker.mark(t0, Seq(3), false, true), None, "even forced");
    }

    #[test]
    fn leaving_marks_at_once() {
        let mut marker = ReadMarker::new(SessionId::new());
        let t0 = Instant::now();
        assert!(marker.mark(t0, Seq(3), true, false).is_some());
        let soon = t0 + Duration::from_millis(1);
        assert_eq!(marker.mark(soon, Seq(5), true, false), None);
        assert_eq!(through(marker.mark(soon, Seq(5), true, true)), Some(5));

        assert!(ReadMarker::marks_before(&Action::Back));
        assert!(ReadMarker::marks_before(&Action::OpenNextApproval));
        assert!(!ReadMarker::marks_before(&Action::RetryNow));
    }
}
