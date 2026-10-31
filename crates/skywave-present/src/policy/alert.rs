//! When the user is alerted that an approval came in, and the text naming
//! it. How the alert goes out (a bell, a desktop notification) is the
//! head's; what it names is the session, never what the agent wants to
//! run.

use skywave_core::Attention;

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

#[cfg(test)]
mod tests {
    use skywave_core::{ApprovalId, PendingApprovalRef, SessionId};

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
        }
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
