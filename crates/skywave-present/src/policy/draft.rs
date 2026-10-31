//! When the composer's text is saved as the session's draft (D5: per
//! device): after a short idle, before anything that may leave the
//! session, and again on the idle cadence while saving fails.

use std::time::{Duration, Instant};

use skywave_core::{Action, SessionId, SessionView};

/// How old the first unsaved composer edit gets before the draft is saved
/// anyway: at most this much typing is lost to a crash. Saves are durable
/// writes, so never one per keystroke.
pub const DRAFT_SAVE_AFTER: Duration = Duration::from_secs(1);

/// Debounces the open session's draft saves. The head reports edits and
/// save failures; the saver says when to save and when to wake for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSaver {
    session_id: SessionId,
    /// The core's [`SessionView::draft_rev`] the composer was filled at.
    draft_rev: u64,
    /// When the first edit the core has not saved yet was made; `None`
    /// while the core has the text and cursor as shown.
    dirty_since: Option<Instant>,
}

impl DraftSaver {
    /// The saver for `view`, whose composer starts as the core's draft.
    pub fn new(view: &SessionView) -> DraftSaver {
        DraftSaver {
            session_id: view.id,
            draft_rev: view.draft_rev,
            dirty_since: None,
        }
    }

    /// The composer's text or cursor changed at `now`.
    pub fn edited(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    /// The composer shows edits the core has not saved.
    pub fn unsaved(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// Whether the core rewrote the draft itself since the composer was
    /// filled (`draft_rev`, e.g. a queued message taken back to edit): the
    /// head then refills its composer from [`SessionView::draft`], and
    /// what it showed counts as saved.
    pub fn refill(&mut self, view: &SessionView) -> bool {
        if view.draft_rev == self.draft_rev {
            return false;
        }
        self.draft_rev = view.draft_rev;
        self.dirty_since = None;
        true
    }

    /// The save to dispatch for the composer's `text` and `cursor` (in
    /// `char`s) when it has unsaved edits: always if `force` (leaving the
    /// session, quitting, any action but a sent message), else once the
    /// first of them is [`DRAFT_SAVE_AFTER`] old at `now`. The edits count
    /// as saved from here; report a failure with [`DraftSaver::failed`].
    pub fn save(&mut self, now: Instant, force: bool, text: &str, cursor: usize) -> Option<Action> {
        let since = self.dirty_since?;
        if !force && now.saturating_duration_since(since) < DRAFT_SAVE_AFTER {
            return None;
        }
        self.dirty_since = None;
        Some(Action::SaveDraft {
            session_id: self.session_id,
            text: text.to_owned(),
            cursor,
        })
    }

    /// Dispatching a [`DraftSaver::save`] failed at `now`: its edits are
    /// unsaved again, due one [`DRAFT_SAVE_AFTER`] from `now` so a save that
    /// keeps failing is retried at that pace. An edit made since the save
    /// was taken has marked them unsaved already and keeps its own time.
    pub fn failed(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    /// When an unforced [`DraftSaver::save`] has something to save; `None`
    /// while nothing is unsaved.
    pub fn next_wake(&self) -> Option<Instant> {
        Some(self.dirty_since? + DRAFT_SAVE_AFTER)
    }

    /// Whether the draft is saved (forced) before dispatching `action`:
    /// before anything but a message, whose enqueue drops the draft anyway,
    /// so leaving the session (or anything else) keeps it.
    pub fn flushes_before(action: &Action) -> bool {
        !matches!(action, Action::SendMessage { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::session;

    fn saved(action: Option<Action>) -> Option<String> {
        match action? {
            Action::SaveDraft { text, .. } => Some(text),
            other => panic!("expected a draft save, got {other:?}"),
        }
    }

    #[test]
    fn idle_draft_is_saved_once() {
        let view = session(Vec::new());
        let mut saver = DraftSaver::new(&view);
        let t0 = Instant::now();
        assert_eq!(saver.next_wake(), None);
        saver.edited(t0);
        // Later edits keep the first one's time.
        saver.edited(t0 + DRAFT_SAVE_AFTER / 2);
        assert_eq!(saver.next_wake(), Some(t0 + DRAFT_SAVE_AFTER));
        let just_before = t0 + DRAFT_SAVE_AFTER - Duration::from_millis(1);
        assert_eq!(saver.save(just_before, false, "abc", 3), None);

        let at = t0 + DRAFT_SAVE_AFTER;
        assert_eq!(
            saver.save(at, false, "abc", 3),
            Some(Action::SaveDraft {
                session_id: view.id,
                text: "abc".into(),
                cursor: 3,
            })
        );
        assert_eq!(saver.next_wake(), None);
        let later = at + 10 * DRAFT_SAVE_AFTER;
        assert_eq!(saver.save(later, false, "abc", 3), None, "saved once");
        assert_eq!(saver.save(later, true, "abc", 3), None, "even when forced");
    }

    #[test]
    fn forced_flush_saves_at_once() {
        let view = session(Vec::new());
        let mut saver = DraftSaver::new(&view);
        let t0 = Instant::now();
        saver.edited(t0);
        assert_eq!(saved(saver.save(t0, true, "x", 1)), Some("x".into()));
        assert!(!saver.unsaved());

        let send = Action::SendMessage {
            session_id: view.id,
            text: "x".into(),
        };
        assert!(!DraftSaver::flushes_before(&send));
        assert!(DraftSaver::flushes_before(&Action::Back));
        assert!(DraftSaver::flushes_before(&Action::RetryNow));
    }

    #[test]
    fn failed_save_is_retried_on_the_idle_cadence() {
        let view = session(Vec::new());
        let mut saver = DraftSaver::new(&view);
        let t0 = Instant::now();
        saver.edited(t0);
        let t = t0 + DRAFT_SAVE_AFTER;
        assert!(saver.save(t, false, "abc", 3).is_some());
        saver.failed(t);

        // Still unsaved, due one cadence after the failure, not at once.
        assert_eq!(saver.next_wake(), Some(t + DRAFT_SAVE_AFTER));
        assert_eq!(saver.save(t, false, "abc", 3), None, "no tight retry");
        let t2 = t + DRAFT_SAVE_AFTER;
        assert_eq!(saved(saver.save(t2, false, "abc", 3)), Some("abc".into()));
        assert_eq!(saver.next_wake(), None);
    }

    #[test]
    fn edit_during_a_save_is_not_lost() {
        let view = session(Vec::new());
        let mut saver = DraftSaver::new(&view);
        let t0 = Instant::now();
        saver.edited(t0);
        let t = t0 + DRAFT_SAVE_AFTER;

        // A save that works does not mark the later edit saved.
        assert_eq!(saved(saver.save(t, false, "ab", 2)), Some("ab".into()));
        let edit = t + Duration::from_millis(10);
        saver.edited(edit);
        assert_eq!(saver.next_wake(), Some(edit + DRAFT_SAVE_AFTER));

        // A save that fails keeps the edit's own due time, not the
        // failure's.
        let t2 = edit + DRAFT_SAVE_AFTER;
        assert!(saver.save(t2, false, "abc", 3).is_some());
        let edit2 = t2 + Duration::from_millis(10);
        saver.edited(edit2);
        saver.failed(t2 + Duration::from_millis(20));
        assert_eq!(saver.next_wake(), Some(edit2 + DRAFT_SAVE_AFTER));
        assert_eq!(
            saved(saver.save(edit2 + DRAFT_SAVE_AFTER, false, "abcd", 4)),
            Some("abcd".into())
        );
    }

    #[test]
    fn a_rewritten_draft_refills_and_counts_as_saved() {
        let mut view = session(Vec::new());
        let mut saver = DraftSaver::new(&view);
        assert!(!saver.refill(&view));
        saver.edited(Instant::now());
        view.draft_rev += 1;
        assert!(saver.refill(&view));
        assert!(!saver.unsaved());
        assert!(!saver.refill(&view), "once per rewrite");
    }
}
