//! The workdir prompt's candidate list, shown whenever the prompt is open:
//! no row is highlighted until ↓ (up from the first row clears it). Enter,
//! a tap, or Tab on a highlighted row descends into it: the input becomes
//! its path and `/`, and the core completes that. Esc clears the highlight
//! before it cancels. Rows for an earlier input (a directory still loading)
//! are shown but cannot be highlighted or chosen. The input text is the
//! head's and changes only on a user action, never on a reply.

use skywave_core::{Action, Candidate, Completion};

/// The candidates of `completion` that can be chosen for `input`: none
/// while the shown ones are for an earlier input (a new directory loads).
pub fn choosable<'a>(input: &str, completion: Option<&'a Completion>) -> &'a [Candidate] {
    match completion {
        Some(completion) if completion.input == input => &completion.candidates,
        _ => &[],
    }
}

/// The workdir prompt's highlight in its candidate list. Every operation
/// that takes a completion works on its choosable candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkdirPicker {
    /// The highlighted row, an index into [`choosable`].
    highlighted: Option<usize>,
}

impl WorkdirPicker {
    pub fn new() -> WorkdirPicker {
        WorkdirPicker::default()
    }

    /// The prompt opened on `input`: nothing highlighted; ask for its candidates.
    pub fn start(&mut self, input: &str) -> Action {
        self.highlighted = None;
        Action::CompleteWorkdir {
            input: input.to_owned(),
        }
    }

    /// The user edited the input: nothing highlighted; complete it again
    /// (the core lists only when the directory changes).
    pub fn edited(&mut self, input: &str) -> Action {
        self.highlighted = None;
        Action::CompleteWorkdir {
            input: input.to_owned(),
        }
    }

    /// The highlighted row's index among the choosable candidates.
    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    /// Up: from the first row, back to no highlight.
    pub fn up(&mut self) {
        self.highlighted = self.highlighted.and_then(|index| index.checked_sub(1));
    }

    /// Down, within the choosable candidates: the first row from no
    /// highlight, else one lower, clamped. Nothing to highlight is a no-op.
    pub fn down(&mut self, input: &str, completion: Option<&Completion>) {
        let Some(last) = choosable(input, completion).len().checked_sub(1) else {
            return;
        };
        self.highlighted = Some(self.highlighted.map_or(0, |index| (index + 1).min(last)));
    }

    /// Clears the highlight; whether there was one.
    pub fn unhighlight(&mut self) -> bool {
        self.highlighted.take().is_some()
    }

    /// Enter: descend into the highlighted candidate. The new input (its
    /// path and `/`) and the action completing it; `None` without a
    /// highlight on a choosable candidate.
    pub fn choose(
        &mut self,
        input: &str,
        completion: Option<&Completion>,
    ) -> Option<(String, Action)> {
        let index = self.highlighted?;
        let path = &choosable(input, completion).get(index)?.path;
        let next = format!("{}/", path);
        self.highlighted = None;
        Some((next.clone(), Action::CompleteWorkdir { input: next }))
    }

    /// A tap on row `index`: as `choose` with that row highlighted; `None`
    /// if `index` is not a choosable candidate.
    pub fn pick(
        &mut self,
        index: usize,
        input: &str,
        completion: Option<&Completion>,
    ) -> Option<(String, Action)> {
        choosable(input, completion).get(index)?;
        self.highlighted = Some(index);
        self.choose(input, completion)
    }

    /// Tab: descend into the highlighted candidate; else fill in
    /// `Completion::fill` when the shown candidates are for `input`. The
    /// new input if it changed, and always the action completing the
    /// resulting input (an unchanged one makes the core list again).
    pub fn complete(
        &mut self,
        input: &str,
        completion: Option<&Completion>,
    ) -> (Option<String>, Action) {
        if let Some((next, action)) = self.choose(input, completion) {
            return (Some(next), action);
        }
        let filled = completion
            .filter(|completion| completion.input == input)
            .and_then(Completion::fill)
            .filter(|fill| fill.as_str() != input);
        match filled {
            Some(fill) => {
                self.highlighted = None;
                (Some(fill.clone()), Action::CompleteWorkdir { input: fill })
            }
            None => (
                None,
                Action::CompleteWorkdir {
                    input: input.to_owned(),
                },
            ),
        }
    }

    /// A new completion is shown: the highlight stays within the
    /// choosable candidates, and goes when there are none.
    pub fn settle(&mut self, input: &str, completion: Option<&Completion>) {
        let Some(last) = choosable(input, completion).len().checked_sub(1) else {
            self.highlighted = None;
            return;
        };
        self.highlighted = self.highlighted.map(|index| index.min(last));
    }

    /// The prompt is cancelled: the core's completion, if it has one
    /// (`completing`), ends.
    pub fn cancel(&mut self, completing: bool) -> Option<Action> {
        *self = WorkdirPicker::default();
        completing.then_some(Action::EndCompletion)
    }
}

#[cfg(test)]
mod tests {
    use skywave_core::{CandidateSource, CompletionStatus};

    use super::*;

    fn completion(input: &str, paths: &[&str], status: CompletionStatus) -> Completion {
        Completion {
            input: input.to_owned(),
            candidates: paths
                .iter()
                .map(|path| Candidate {
                    path: (*path).to_owned(),
                    source: CandidateSource::Server,
                })
                .collect(),
            status,
        }
    }

    const READY: CompletionStatus = CompletionStatus::Ready { truncated: false };

    #[test]
    fn nothing_is_highlighted_until_down() {
        let mut picker = WorkdirPicker::new();
        let listed = completion("/srv/", &["/srv/api", "/srv/web"], READY);
        let shown = Some(&listed);
        assert_eq!(
            picker.start("/srv/"),
            Action::CompleteWorkdir {
                input: "/srv/".into()
            }
        );
        assert_eq!(picker.highlighted(), None);
        assert_eq!(picker.choose("/srv/", shown), None);
        picker.down("/srv/", shown);
        assert_eq!(picker.highlighted(), Some(0));
        picker.down("/srv/", shown);
        picker.down("/srv/", shown);
        assert_eq!(picker.highlighted(), Some(1));
        picker.up();
        assert_eq!(picker.highlighted(), Some(0));
        picker.up();
        assert_eq!(picker.highlighted(), None);
        picker.up();
        assert_eq!(picker.highlighted(), None);
    }

    #[test]
    fn enter_descends_into_the_highlighted_directory() {
        let mut picker = WorkdirPicker::new();
        let listed = completion("/srv/", &["/srv/api", "/srv/web"], READY);
        picker.start("/srv/");
        picker.down("/srv/", Some(&listed));
        picker.down("/srv/", Some(&listed));
        assert_eq!(
            picker.choose("/srv/", Some(&listed)),
            Some((
                "/srv/web/".into(),
                Action::CompleteWorkdir {
                    input: "/srv/web/".into()
                }
            ))
        );
        assert_eq!(picker.highlighted(), None);
    }

    #[test]
    fn a_tap_descends_into_its_row() {
        let mut picker = WorkdirPicker::new();
        let listed = completion("/srv/", &["/srv/api", "/srv/web"], READY);
        assert_eq!(
            picker.pick(1, "/srv/", Some(&listed)),
            Some((
                "/srv/web/".into(),
                Action::CompleteWorkdir {
                    input: "/srv/web/".into()
                }
            ))
        );
        assert_eq!(picker.pick(5, "/srv/", Some(&listed)), None);
    }

    #[test]
    fn stale_rows_cannot_be_highlighted_or_chosen() {
        let mut picker = WorkdirPicker::new();
        let stale = completion(
            "/srv/",
            &["/srv/api", "/srv/web"],
            CompletionStatus::Loading,
        );
        assert_eq!(choosable("/srv/", Some(&stale)).len(), 2);
        let input = "/srv/api/";
        assert!(choosable(input, Some(&stale)).is_empty());
        picker.down(input, Some(&stale));
        assert_eq!(picker.highlighted(), None);
        assert_eq!(picker.pick(0, input, Some(&stale)), None);
        assert_eq!(picker.choose(input, Some(&stale)), None);
    }

    #[test]
    fn tab_fills_the_common_prefix_or_the_only_directory() {
        let mut picker = WorkdirPicker::new();
        let two = completion("/srv/w", &["/srv/work-a", "/srv/work-b"], READY);
        assert_eq!(
            picker.complete("/srv/w", Some(&two)),
            (
                Some("/srv/work-".into()),
                Action::CompleteWorkdir {
                    input: "/srv/work-".into()
                }
            )
        );
        let one = completion("/srv/w", &["/srv/work"], READY);
        assert_eq!(
            picker.complete("/srv/w", Some(&one)),
            (
                Some("/srv/work/".into()),
                Action::CompleteWorkdir {
                    input: "/srv/work/".into()
                }
            )
        );
    }

    #[test]
    fn tab_with_nothing_to_fill_asks_again() {
        let mut picker = WorkdirPicker::new();
        let empty = completion("/srv/x", &[], READY);
        assert_eq!(
            picker.complete("/srv/x", Some(&empty)),
            (
                None,
                Action::CompleteWorkdir {
                    input: "/srv/x".into()
                }
            )
        );
        let stale = completion("/srv/", &["/srv/work"], READY);
        assert_eq!(
            picker.complete("/srv/x", Some(&stale)),
            (
                None,
                Action::CompleteWorkdir {
                    input: "/srv/x".into()
                }
            )
        );
        // The same candidates fill in for their own input.
        assert_eq!(
            picker.complete("/srv/", Some(&stale)),
            (
                Some("/srv/work/".into()),
                Action::CompleteWorkdir {
                    input: "/srv/work/".into()
                }
            )
        );
    }

    #[test]
    fn tab_takes_the_highlighted_row() {
        let mut picker = WorkdirPicker::new();
        let listed = completion("/srv/", &["/srv/api", "/srv/web"], READY);
        picker.start("/srv/");
        picker.down("/srv/", Some(&listed));
        assert_eq!(
            picker.complete("/srv/", Some(&listed)),
            (
                Some("/srv/api/".into()),
                Action::CompleteWorkdir {
                    input: "/srv/api/".into()
                }
            )
        );
        assert_eq!(picker.highlighted(), None);
    }

    #[test]
    fn settle_keeps_the_highlight_within_the_candidates() {
        let mut picker = WorkdirPicker::new();
        let three = completion("/srv/", &["/srv/a", "/srv/b", "/srv/c"], READY);
        for _ in 0..3 {
            picker.down("/srv/", Some(&three));
        }
        assert_eq!(picker.highlighted(), Some(2));
        let one = completion("/srv/", &["/srv/a"], READY);
        picker.settle("/srv/", Some(&one));
        assert_eq!(picker.highlighted(), Some(0));
        let none = completion("/srv/", &[], READY);
        picker.settle("/srv/", Some(&none));
        assert_eq!(picker.highlighted(), None);
        picker.down("/srv/", Some(&one));
        assert_eq!(picker.highlighted(), Some(0));
        picker.settle("/srv/x", Some(&one));
        assert_eq!(picker.highlighted(), None);
    }

    #[test]
    fn edited_clears_the_highlight() {
        let mut picker = WorkdirPicker::new();
        let listed = completion("/srv/", &["/srv/api", "/srv/web"], READY);
        picker.start("/srv/");
        picker.down("/srv/", Some(&listed));
        assert_eq!(picker.highlighted(), Some(0));
        assert_eq!(
            picker.edited("/srv/w"),
            Action::CompleteWorkdir {
                input: "/srv/w".into()
            }
        );
        assert_eq!(picker.highlighted(), None);
    }

    #[test]
    fn esc_clears_the_highlight_then_cancels() {
        let mut picker = WorkdirPicker::new();
        let listed = completion("/srv/", &["/srv/api", "/srv/web"], READY);
        picker.start("/srv/");
        picker.down("/srv/", Some(&listed));
        assert!(picker.unhighlight());
        assert!(!picker.unhighlight());
        assert_eq!(picker.cancel(true), Some(Action::EndCompletion));
        assert_eq!(picker.cancel(false), None);
    }
}
