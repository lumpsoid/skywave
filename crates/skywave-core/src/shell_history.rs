//! This device's view of every session's shell commands ([`ShellHistory`]),
//! newest first, each once. Rebuilt from the log on load and kept current as
//! batches apply.

use std::sync::Arc;

/// Most distinct commands kept.
pub const SHELL_HISTORY_MAX: usize = 1000;

/// The shell commands of every session, newest first, each once. The list
/// is shared, so handing it to a view is cheap.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ShellHistory {
    commands: Arc<Vec<String>>,
}

impl ShellHistory {
    /// Keeps `commands` as given, newest first, at most [`SHELL_HISTORY_MAX`].
    pub(crate) fn new(mut commands: Vec<String>) -> ShellHistory {
        commands.truncate(SHELL_HISTORY_MAX);
        ShellHistory {
            commands: Arc::new(commands),
        }
    }

    /// Moves `command` to the front, as just run, dropping its older run and
    /// the oldest command past the cap.
    pub(crate) fn record(&mut self, command: &str) {
        let commands = Arc::make_mut(&mut self.commands);
        if let Some(at) = commands.iter().position(|c| c == command) {
            commands.remove(at);
        }
        commands.insert(0, command.to_owned());
        commands.truncate(SHELL_HISTORY_MAX);
    }

    /// The commands, newest first.
    pub(crate) fn list(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeat_moves_to_the_front() {
        let mut history = ShellHistory::new(vec!["ls".to_owned(), "make".to_owned()]);
        history.record("make");
        assert_eq!(*history.list(), ["make", "ls"]);
        history.record("cargo test");
        assert_eq!(*history.list(), ["cargo test", "make", "ls"]);
    }

    #[test]
    fn past_the_cap_the_oldest_goes() {
        let full = (0..SHELL_HISTORY_MAX).map(|i| format!("cmd {i}")).collect();
        let mut history = ShellHistory::new(full);
        history.record("new");

        let list = history.list();
        assert_eq!(list.len(), SHELL_HISTORY_MAX);
        assert_eq!(list[0], "new");
        assert!(list.contains(&"cmd 0".to_owned()), "the newest stays");
        assert!(
            !list.contains(&format!("cmd {}", SHELL_HISTORY_MAX - 1)),
            "the oldest goes"
        );
    }
}
