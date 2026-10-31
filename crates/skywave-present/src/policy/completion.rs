//! Completing a new session's workdir from the core's [`Completion`]:
//! asking, filling in once the candidates settle, and ending it. Whether a
//! path is acceptable is the server's call (D14).

use skywave_core::{Action, Candidate, Completion, CompletionStatus};

/// The workdir prompt's completion: whether the candidate list is open,
/// which candidate is highlighted, and whether a completion asked for
/// waits for its candidates to settle. The input text is the head's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompletionSettle {
    /// The candidate list is open, showing the core's [`Completion`].
    open: bool,
    /// Highlighted candidate in the list.
    selected: usize,
    /// Completion was asked for: once the core's candidates for this input
    /// are settled, fill it in. Any other input cancels it.
    waiting: bool,
}

impl CompletionSettle {
    pub fn new() -> CompletionSettle {
        CompletionSettle::default()
    }

    /// The candidate list is open.
    pub fn open(&self) -> bool {
        self.open
    }

    /// The highlighted candidate's index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Asks the core to complete `input` (Tab with no candidates listed):
    /// the list opens, and the completion fills in once settled.
    pub fn complete(&mut self, input: &str) -> Action {
        self.waiting = true;
        self.open = true;
        self.selected = 0;
        Action::CompleteWorkdir {
            input: input.to_owned(),
        }
    }

    /// Any input but asking to complete: a completion waiting to fill in
    /// no longer does.
    pub fn input(&mut self) {
        self.waiting = false;
    }

    /// `input` was edited: while the list is open, complete it again.
    pub fn edited(&mut self, input: &str) -> Option<Action> {
        self.waiting = false;
        self.open.then(|| {
            self.selected = 0;
            Action::CompleteWorkdir {
                input: input.to_owned(),
            }
        })
    }

    /// Text was pasted: the candidates were for the text before it.
    pub fn pasted(&mut self) {
        self.open = false;
        self.waiting = false;
    }

    /// Closes the list, keeping the prompt.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Highlights the candidate above.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Highlights the candidate below, of `len`.
    pub fn down(&mut self, len: usize) {
        self.selected = (self.selected + 1).min(len.saturating_sub(1));
    }

    /// Takes the highlighted one of the listed `candidates`: the input
    /// becomes its path with a trailing `/`, and the list closes.
    pub fn choose(&mut self, candidates: &[Candidate]) -> Option<String> {
        let chosen = candidates.get(self.selected.min(candidates.len().checked_sub(1)?))?;
        self.open = false;
        Some(format!("{}/", chosen.path))
    }

    /// Applies a new `completion` to `input`. One asked for that settled
    /// for this input fills in: the only candidate fills in and the list
    /// closes; several extend the common prefix and stay listed. The
    /// highlight stays within the candidates.
    pub fn settle(&mut self, input: &mut String, completion: &Completion) {
        if self.waiting
            && completion.input == *input
            && completion.status != CompletionStatus::Loading
        {
            self.waiting = false;
            if let Some(fill) = completion.fill() {
                *input = fill;
            }
            self.open = completion.candidates.len() != 1;
            self.selected = 0;
        }
        self.selected = self
            .selected
            .min(completion.candidates.len().saturating_sub(1));
    }

    /// The prompt is cancelled: the core's completion, if it has one
    /// (`completing`), ends.
    pub fn cancel(&mut self, completing: bool) -> Option<Action> {
        *self = CompletionSettle::default();
        completing.then_some(Action::EndCompletion)
    }
}

#[cfg(test)]
mod tests {
    use skywave_core::CandidateSource;

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
    fn one_candidate_fills_in() {
        let mut settle = CompletionSettle::new();
        let mut input = "/srv/w".to_owned();
        assert_eq!(
            settle.complete(&input),
            Action::CompleteWorkdir {
                input: "/srv/w".into()
            }
        );
        // Still loading, or for other input: nothing yet.
        settle.settle(
            &mut input,
            &completion("/srv/w", &["/srv/work"], CompletionStatus::Loading),
        );
        settle.settle(&mut input, &completion("/srv/", &["/srv/work"], READY));
        assert_eq!(input, "/srv/w");

        settle.settle(&mut input, &completion("/srv/w", &["/srv/work"], READY));
        assert_eq!(input, "/srv/work/");
        assert!(!settle.open());
        // Settled once only.
        settle.settle(
            &mut input,
            &completion("/srv/work/", &["/srv/work/x"], READY),
        );
        assert_eq!(input, "/srv/work/");
    }

    #[test]
    fn several_candidates_extend_the_common_prefix() {
        let mut settle = CompletionSettle::new();
        let mut input = "/srv/".to_owned();
        settle.complete(&input);
        let listed = completion("/srv/", &["/srv/work-a", "/srv/work-b"], READY);
        settle.settle(&mut input, &listed);
        assert_eq!(input, "/srv/work-");
        assert!(settle.open());

        settle.down(listed.candidates.len());
        settle.down(listed.candidates.len());
        assert_eq!(settle.selected(), 1);
        assert_eq!(
            settle.choose(&listed.candidates),
            Some("/srv/work-b/".into())
        );
        assert!(!settle.open());
    }

    #[test]
    fn an_edit_completes_again_while_listed() {
        let mut settle = CompletionSettle::new();
        assert_eq!(settle.edited("/s"), None, "not listed");
        settle.complete("/s");
        settle.down(3);
        assert_eq!(
            settle.edited("/sr"),
            Some(Action::CompleteWorkdir {
                input: "/sr".into()
            })
        );
        assert_eq!(settle.selected(), 0);
        // The edit cancelled the fill.
        let mut input = "/sr".to_owned();
        settle.settle(&mut input, &completion("/sr", &["/srv"], READY));
        assert_eq!(input, "/sr");
    }

    #[test]
    fn cancel_ends_the_completion() {
        let mut settle = CompletionSettle::new();
        settle.complete("/s");
        assert_eq!(settle.cancel(true), Some(Action::EndCompletion));
        assert!(!settle.open());
        assert_eq!(settle.cancel(false), None);
    }
}
