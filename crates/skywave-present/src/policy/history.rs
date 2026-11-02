//! Recalling earlier prompts into the composer, newest first, with what
//! was being written kept aside and restored past the newest.

/// What the composer shows after a [`PromptHistory::newer`] step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recall<'a, S> {
    /// An earlier prompt.
    Prompt(&'a str),
    /// Past the newest: what was being written before browsing, as
    /// stashed. Browsing has ended.
    Stash(S),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Browse<S> {
    /// Taken when browsing starts, so new prompts do not shift `index`.
    list: Vec<String>,
    /// The prompt shown; 0 is the newest.
    index: usize,
    /// What was being written before browsing.
    stash: S,
}

/// Browsing [`SessionView::prompts`](skywave_core::SessionView::prompts),
/// or with an instance of its own
/// [`ShellView::commands`](skywave_core::ShellView::commands). `S` is the head's composer state (text and
/// cursor) kept aside while browsing. The prompt shown is the draft: a head
/// saves it like typed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistory<S> {
    browse: Option<Browse<S>>,
}

impl<S> Default for PromptHistory<S> {
    fn default() -> Self {
        PromptHistory { browse: None }
    }
}

impl<S> PromptHistory<S> {
    pub fn new() -> PromptHistory<S> {
        PromptHistory::default()
    }

    /// The next older prompt to show, if there is one. The first step
    /// takes the list `source` gives, newest first (a session's prompts, or
    /// its shell commands for the shell composer), and keeps what `stash`
    /// gives aside.
    pub fn older<'v>(
        &mut self,
        source: impl FnOnce() -> Vec<&'v str>,
        stash: impl FnOnce() -> S,
    ) -> Option<&str> {
        if let Some(browse) = &mut self.browse {
            if browse.index + 1 == browse.list.len() {
                return None;
            }
            browse.index += 1;
        } else {
            let list: Vec<String> = source().into_iter().map(str::to_owned).collect();
            if list.is_empty() {
                return None;
            }
            self.browse = Some(Browse {
                list,
                index: 0,
                stash: stash(),
            });
        }
        let browse = self.browse.as_ref()?;
        Some(&browse.list[browse.index])
    }

    /// The next newer prompt to show, or past the newest the stash;
    /// `None` while not browsing.
    pub fn newer(&mut self) -> Option<Recall<'_, S>> {
        let browse = self.browse.as_mut()?;
        if browse.index == 0 {
            return self.browse.take().map(|b| Recall::Stash(b.stash));
        }
        browse.index -= 1;
        let browse = self.browse.as_ref()?;
        Some(Recall::Prompt(&browse.list[browse.index]))
    }

    /// Any edit (or a rewritten draft) ends browsing: the shown prompt
    /// stays as the text being written.
    pub fn end(&mut self) {
        self.browse = None;
    }

    /// While browsing: the shown prompt's place, 1 the newest, and how
    /// many there are.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.browse.as_ref().map(|b| (b.index + 1, b.list.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::{message, session};

    #[test]
    fn up_and_down_restore_the_stash() {
        let view = session(vec![message(1, "first"), message(2, "second")]);
        let mut history = PromptHistory::new();
        assert_eq!(history.newer(), None, "not browsing");
        assert_eq!(
            history.older(|| view.prompts(), || "unsent"),
            Some("second")
        );
        assert_eq!(history.position(), Some((1, 2)));
        assert_eq!(history.older(|| view.prompts(), || "other"), Some("first"));
        assert_eq!(
            history.older(|| view.prompts(), || "other"),
            None,
            "the oldest"
        );
        assert_eq!(history.position(), Some((2, 2)));
        assert_eq!(history.newer(), Some(Recall::Prompt("second")));
        assert_eq!(history.newer(), Some(Recall::Stash("unsent")));
        assert_eq!(history.position(), None);
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn an_edit_ends_browsing() {
        let view = session(vec![message(1, "first")]);
        let mut history = PromptHistory::new();
        assert_eq!(history.older(|| view.prompts(), || "unsent"), Some("first"));
        history.end();
        assert_eq!(history.position(), None);
        assert_eq!(history.newer(), None, "the stash is gone");
    }

    #[test]
    fn shell_commands_are_a_source_of_their_own() {
        use skywave_core::{
            CommandId, DeviceId, DeviceRef, RunWhen, ShellEnd, ShellRun, ShellRunState,
        };
        let mut view = session(vec![message(1, "a prompt")]);
        view.shell.runs = ["ls", "make test"]
            .into_iter()
            .map(|command| ShellRun {
                run: CommandId::new(),
                command: command.into(),
                when: RunWhen::Always,
                by: DeviceRef {
                    id: DeviceId::new(),
                    name: None,
                    me: true,
                },
                state: ShellRunState::Finished(ShellEnd::Exited { code: 0 }),
                requested_at: None,
                started_at: None,
                finished_at: None,
                tail_or_preview: String::new(),
                lines: 0,
                output: None,
                truncated: false,
                outgoing: None,
                discardable: false,
                stopping: false,
                delivery: None,
            })
            .collect();
        let mut shell = PromptHistory::new();
        assert_eq!(
            shell.older(|| view.shell.commands(), || ""),
            Some("make test")
        );
        assert_eq!(shell.older(|| view.shell.commands(), || ""), Some("ls"));
        assert_eq!(shell.older(|| view.shell.commands(), || ""), None);
    }

    #[test]
    fn nothing_to_recall_does_not_browse() {
        let view = session(Vec::new());
        let mut history = PromptHistory::new();
        assert_eq!(history.older(|| view.prompts(), || "unsent"), None);
        assert_eq!(history.position(), None);
    }
}
