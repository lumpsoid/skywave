//! When the user is alerted that an approval came in, or that a long shell
//! run of theirs finished, and the text naming it. How the alert goes out
//! (a bell, a desktop notification) is the head's; an approval alert names
//! the session, never what the agent wants to run. A shell alert names the
//! command the user ran themselves.

use skywave_core::{Attention, RunWhen};

use crate::format::shell_end_label;

/// Longest session title an alert or notice names, in `char`s.
pub const TITLE_MAX: usize = 100;

/// `text` safe to name in an alert, a terminal escape sequence included,
/// and on one line: control characters (C0, DEL, C1) and `;` dropped, at
/// most [`TITLE_MAX`] `char`s kept.
pub fn sanitise(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && *c != ';')
        .take(TITLE_MAX)
        .collect()
}

/// New approvals came in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// The newest pending approval's session title, [`sanitise`]d; `None`
    /// when none is pending any more.
    pub title: Option<String>,
}

impl Alert {
    /// The alert as a line of text: `approval needed · <title>`.
    pub fn notice(&self) -> String {
        match &self.title {
            Some(title) => format!("approval needed · {title}"),
            None => "approval needed".to_owned(),
        }
    }
}

/// Watches [`Attention::alerts`] for growth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertWatcher {
    /// The count last seen.
    seen: u64,
}

impl AlertWatcher {
    /// Starts from the head's `first` snapshot: what it already counts
    /// needs no alert.
    pub fn new(first: &Attention) -> AlertWatcher {
        AlertWatcher { seen: first.alerts }
    }

    /// The alert a new snapshot's `attention` asks for: one whenever the
    /// count grew past what was seen.
    pub fn observe(&mut self, attention: &Attention) -> Option<Alert> {
        let grew = attention.alerts > self.seen;
        self.seen = self.seen.max(attention.alerts);
        grew.then(|| Alert {
            title: attention.approvals.last().map(|a| sanitise(&a.title)),
        })
    }
}

/// A long shell run of this device's finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellNotice {
    /// The command's first line, [`sanitise`]d.
    pub command_line: String,
    /// How it ended, as [`shell_end_label`] says it.
    pub end: String,
    /// Its session's title, [`sanitise`]d.
    pub title: String,
}

impl ShellNotice {
    /// The notice as a line of text: `make test · exit 1 · <title>`.
    pub fn notice(&self) -> String {
        format!("{} · {} · {}", self.command_line, self.end, self.title)
    }
}

/// Watches [`Attention::shell_alerts`] for growth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShellAlertWatcher {
    /// The count last seen.
    seen: u64,
}

impl ShellAlertWatcher {
    /// Starts from 0, not from the head's first snapshot: a run that
    /// finished in a catch-up before that snapshot is still news.
    pub fn new() -> ShellAlertWatcher {
        ShellAlertWatcher::default()
    }

    /// The notice a new snapshot's `attention` asks for: one whenever the
    /// count grew past what was seen, naming the last run it counted.
    pub fn observe(&mut self, attention: &Attention) -> Option<ShellNotice> {
        let grew = attention.shell_alerts > self.seen;
        self.seen = self.seen.max(attention.shell_alerts);
        let last = attention.last_shell.as_ref().filter(|_| grew)?;
        Some(ShellNotice {
            command_line: sanitise(&last.command_line),
            // A run that alerts has started, so it was never skipped and
            // its condition does not matter to the wording.
            end: shell_end_label(&last.end, RunWhen::Always),
            title: sanitise(&last.title),
        })
    }
}

#[cfg(test)]
mod tests {
    use skywave_core::{ApprovalId, PendingApprovalRef, SessionId, ShellAlert, ShellEnd};

    use super::*;

    fn attention(alerts: u64, titles: &[&str]) -> Attention {
        Attention {
            approvals: titles
                .iter()
                .map(|title| PendingApprovalRef {
                    id: ApprovalId::new(),
                    session_id: SessionId::new(),
                    title: (*title).to_owned(),
                    alerted_at: alerts,
                })
                .collect(),
            elsewhere: 0,
            alerts,
            shell_alerts: 0,
            last_shell: None,
        }
    }

    fn shell(alerts: u64, command_line: &str, end: ShellEnd) -> Attention {
        Attention {
            shell_alerts: alerts,
            last_shell: Some(ShellAlert {
                session_id: SessionId::new(),
                title: "fix\u{1b}[2J the tests".into(),
                command_line: command_line.into(),
                end,
            }),
            ..Attention::default()
        }
    }

    #[test]
    fn a_shell_alert_already_counted_in_the_first_snapshot_still_alerts() {
        // A catch-up at start finished the run before the head's first
        // snapshot: the count is 1 there, and it is news.
        let first = shell(1, "make test", ShellEnd::Exited { code: 1 });
        let mut watcher = ShellAlertWatcher::new();
        let notice = watcher.observe(&first);
        assert_eq!(
            notice.as_ref().map(ShellNotice::notice).as_deref(),
            Some("make test · exit 1 · fix[2J the tests")
        );
        assert_eq!(watcher.observe(&first), None, "once");
        // The approval watcher does start from its first snapshot; the
        // shell one must not.
        assert_eq!(AlertWatcher::new(&first).observe(&first), None);
    }

    #[test]
    fn shell_alerts_once_per_growth_naming_the_last_run() {
        let mut watcher = ShellAlertWatcher::new();
        assert_eq!(watcher.observe(&Attention::default()), None);
        let alert = watcher.observe(&shell(
            1,
            "sleep 12;\u{7}",
            ShellEnd::Signalled { signal: 9 },
        ));
        assert_eq!(
            alert.map(|n| n.notice()).as_deref(),
            Some("sleep 12 · killed by signal 9 · fix[2J the tests")
        );
        assert_eq!(
            watcher.observe(&shell(1, "sleep 12", ShellEnd::Stopped)),
            None
        );
        assert_eq!(
            watcher
                .observe(&shell(3, "cargo build", ShellEnd::Exited { code: 0 }))
                .map(|n| n.notice())
                .as_deref(),
            Some("cargo build · exit 0 · fix[2J the tests"),
            "two finishes in one snapshot alert once, for the last"
        );
    }

    #[test]
    fn the_first_snapshot_never_alerts() {
        let first = attention(2, &["old"]);
        let mut watcher = AlertWatcher::new(&first);
        assert_eq!(watcher.observe(&first), None);
    }

    #[test]
    fn alerts_once_per_growth_naming_the_newest() {
        let mut watcher = AlertWatcher::new(&attention(0, &[]));
        let alert = watcher.observe(&attention(1, &["a", "/srv/we\u{1b}]0;ird\u{9b};x"]));
        assert_eq!(
            alert,
            Some(Alert {
                title: Some("/srv/we]0irdx".into())
            })
        );
        assert_eq!(
            alert.map(|a| a.notice()),
            Some("approval needed · /srv/we]0irdx".into())
        );
        assert_eq!(watcher.observe(&attention(1, &["a"])), None, "once");
        // Resolved during a catch-up: the count did not grow.
        assert_eq!(watcher.observe(&attention(1, &[])), None);

        let alert = watcher.observe(&attention(2, &[]));
        assert_eq!(
            alert.as_ref().map(Alert::notice),
            Some("approval needed".into())
        );
    }

    #[test]
    fn titles_are_capped() {
        assert_eq!(sanitise(&"é".repeat(150)).chars().count(), TITLE_MAX);
    }
}
