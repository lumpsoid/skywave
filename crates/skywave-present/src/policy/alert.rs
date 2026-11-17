//! When the user is alerted that an approval came in, that a long shell run
//! of theirs finished, or that a session's agent went idle, and the text
//! naming it. How the alert goes out (a bell, a desktop notification) is the
//! head's; an approval alert names the session, never what the agent wants
//! to run. A shell alert names the command the user ran themselves.

use skywave_core::{Attention, RunWhen, SessionId};

use crate::format::shell_end_label;
use crate::text::bidi_escape;

/// Longest session title an alert or notice names, in `char`s.
pub const TITLE_MAX: usize = 100;

/// `text` safe to name in an alert, a terminal escape sequence included,
/// and on one line: control characters (C0, DEL, C1) and `;` dropped, a
/// bidirectional formatting character escaped as [`clean`](crate::text::clean) does
/// (`<U+202E>`), at most [`TITLE_MAX`] `char`s kept. The cap counts the
/// escapes and never cuts one in half: one that would not fit ends the
/// title.
pub fn sanitise(text: &str) -> String {
    let mut out = String::new();
    let mut len = 0;
    for c in text.chars().filter(|c| !c.is_control() && *c != ';') {
        let escape = bidi_escape(c);
        let width = escape.as_ref().map_or(1, |e| e.chars().count());
        if len + width > TITLE_MAX {
            break;
        }
        len += width;
        match escape {
            Some(escape) => out.push_str(&escape),
            None => out.push(c),
        }
    }
    out
}

/// New approvals came in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// The newest pending approval's session title, [`sanitise`]d; `None`
    /// when none is pending any more.
    pub title: Option<String>,
    /// The newest pending approval's session; `None` when none is pending
    /// any more.
    pub session_id: Option<SessionId>,
}

/// What a new snapshot changed about approvals waiting on the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlertUpdate {
    /// New approvals came in: alert once.
    pub alert: Option<Alert>,
    /// Sessions that had approvals pending in the last snapshot and have
    /// none now (answered or cancelled from any device, or the session is
    /// gone), in the order they were waiting: a head that keeps an alert
    /// per session takes it down. A head that only rings a bell ignores it.
    pub cleared: Vec<SessionId>,
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

/// Watches [`Attention::alerts`] for growth, and which sessions have
/// approvals pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertWatcher {
    /// The count last seen.
    seen: u64,
    /// The sessions with approvals pending in the last snapshot, each once,
    /// in the order of their oldest one.
    waiting: Vec<SessionId>,
}

impl AlertWatcher {
    /// Starts from the head's `first` snapshot: what it already counts
    /// needs no alert. Its waiting sessions are tracked, so one answered
    /// later is [cleared](AlertUpdate::cleared) even though this head never
    /// alerted on it (an earlier process may have).
    pub fn new(first: &Attention) -> AlertWatcher {
        AlertWatcher {
            seen: first.alerts,
            waiting: waiting(first),
        }
    }

    /// What a new snapshot's `attention` asks for: an alert whenever the
    /// count grew past what was seen, and the sessions no longer waiting.
    pub fn observe(&mut self, attention: &Attention) -> AlertUpdate {
        let grew = attention.alerts > self.seen;
        self.seen = self.seen.max(attention.alerts);
        let now = waiting(attention);
        let cleared = std::mem::replace(&mut self.waiting, now)
            .into_iter()
            .filter(|id| !self.waiting.contains(id))
            .collect();
        let newest = attention.approvals.last();
        AlertUpdate {
            alert: grew.then(|| Alert {
                title: newest.map(|a| sanitise(&a.title)),
                session_id: newest.map(|a| a.session_id),
            }),
            cleared,
        }
    }
}

/// The sessions `attention` has approvals pending in, each once.
fn waiting(attention: &Attention) -> Vec<SessionId> {
    let mut out: Vec<SessionId> = Vec::new();
    for approval in &attention.approvals {
        if !out.contains(&approval.session_id) {
            out.push(approval.session_id);
        }
    }
    out
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

/// A session whose agent went from working to idle; `title` is
/// [`sanitise`]d.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleNotice {
    pub session_id: SessionId,
    pub title: String,
}

impl IdleNotice {
    /// The notice as a line of text: `agent idle · <title>`.
    pub fn notice(&self) -> String {
        format!("agent idle · {}", self.title)
    }
}

/// What a new snapshot changed about sessions whose agent went idle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdleUpdate {
    /// Sessions that rang since the last snapshot, one notice each, in the
    /// order they rang.
    pub notices: Vec<IdleNotice>,
    /// Sessions idle in the last snapshot that are not idle now (they
    /// worked again, or are gone), in the order they were idle: a head that
    /// keeps a notice per session takes it down.
    pub cleared: Vec<SessionId>,
}

/// Watches [`Attention::idle_alerts`] for growth, and which sessions are
/// idle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdleWatcher {
    /// The count last seen.
    seen: u64,
    /// The sessions in the last snapshot's [`Attention::idle`], in order.
    idle: Vec<SessionId>,
}

impl IdleWatcher {
    /// Starts from 0, not from the head's first snapshot, like
    /// [`ShellAlertWatcher::new`]: what the core counted is news.
    pub fn new() -> IdleWatcher {
        IdleWatcher::default()
    }

    /// What a new snapshot's `attention` asks for: a notice for each session
    /// that rang past what was seen, and the sessions no longer idle.
    pub fn observe(&mut self, attention: &Attention) -> IdleUpdate {
        let notices = attention
            .idle
            .iter()
            .filter(|alert| alert.at > self.seen)
            .map(|alert| IdleNotice {
                session_id: alert.session_id,
                title: sanitise(&alert.title),
            })
            .collect();
        self.seen = self.seen.max(attention.idle_alerts);
        let now = attention
            .idle
            .iter()
            .map(|alert| alert.session_id)
            .collect();
        let cleared = std::mem::replace(&mut self.idle, now)
            .into_iter()
            .filter(|id| !self.idle.contains(id))
            .collect();
        IdleUpdate { notices, cleared }
    }
}

#[cfg(test)]
mod tests {
    use skywave_core::{
        ApprovalId, IdleAlert, PendingApprovalRef, SessionId, ShellAlert, ShellEnd,
    };

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
            idle_alerts: 0,
            idle: Vec::new(),
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
        assert_eq!(AlertWatcher::new(&first).observe(&first).alert, None);
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
        assert_eq!(watcher.observe(&first), AlertUpdate::default());
    }

    #[test]
    fn alerts_once_per_growth_naming_the_newest() {
        let mut watcher = AlertWatcher::new(&attention(0, &[]));
        let snapshot = attention(1, &["a", "/srv/we\u{1b}]0;ird\u{9b};x"]);
        let alert = watcher.observe(&snapshot).alert;
        assert_eq!(
            alert,
            Some(Alert {
                title: Some("/srv/we]0irdx".into()),
                session_id: Some(snapshot.approvals[1].session_id),
            })
        );
        assert_eq!(
            alert.map(|a| a.notice()),
            Some("approval needed · /srv/we]0irdx".into())
        );
        assert_eq!(watcher.observe(&snapshot).alert, None, "once");
        // Resolved during a catch-up: the count did not grow.
        assert_eq!(watcher.observe(&attention(1, &[])).alert, None);

        let alert = watcher.observe(&attention(2, &[])).alert;
        assert_eq!(
            alert.as_ref().map(Alert::notice),
            Some("approval needed".into())
        );
        assert_eq!(alert.and_then(|a| a.session_id), None);
    }

    fn pending(session_id: SessionId, alerted_at: u64) -> PendingApprovalRef {
        PendingApprovalRef {
            id: ApprovalId::new(),
            session_id,
            title: "t".into(),
            alerted_at,
        }
    }

    fn waiting_on(alerts: u64, approvals: Vec<PendingApprovalRef>) -> Attention {
        Attention {
            approvals,
            alerts,
            ..Attention::default()
        }
    }

    #[test]
    fn a_session_is_cleared_once_when_its_last_approval_goes() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let a1 = pending(a, 1);
        let a2 = pending(a, 1);
        let b1 = pending(b, 2);
        let mut watcher = AlertWatcher::new(&Attention::default());
        let update = watcher.observe(&waiting_on(1, vec![a1.clone(), a2.clone()]));
        assert_eq!(update.alert.and_then(|a| a.session_id), Some(a));
        assert_eq!(update.cleared, []);
        let update = watcher.observe(&waiting_on(2, vec![a1.clone(), a2.clone(), b1.clone()]));
        assert_eq!(update.alert.and_then(|a| a.session_id), Some(b));
        assert_eq!(update.cleared, [], "nothing answered yet");

        // One of `a`'s two answered: it still waits.
        let update = watcher.observe(&waiting_on(2, vec![a2.clone(), b1.clone()]));
        assert_eq!(update, AlertUpdate::default());
        // Its last answered: `a` alone is cleared, `b` still waits.
        let update = watcher.observe(&waiting_on(2, vec![b1.clone()]));
        assert_eq!(update.alert, None);
        assert_eq!(update.cleared, [a]);
        assert_eq!(
            watcher.observe(&waiting_on(2, vec![b1])).cleared,
            [],
            "once"
        );
        assert_eq!(watcher.observe(&waiting_on(2, vec![])).cleared, [b]);
        assert_eq!(watcher.observe(&waiting_on(2, vec![])).cleared, []);
    }

    #[test]
    fn the_first_snapshot_clears_nothing_but_its_sessions_clear_later() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let first = waiting_on(0, vec![pending(a, 0), pending(b, 0)]);
        let mut watcher = AlertWatcher::new(&first);
        assert_eq!(watcher.observe(&first), AlertUpdate::default());
        // Both went in one snapshot (say, during a catch-up): both cleared,
        // in the order they waited, with no alert.
        assert_eq!(
            watcher.observe(&Attention::default()),
            AlertUpdate {
                alert: None,
                cleared: vec![a, b],
            }
        );
    }

    #[test]
    fn titles_are_capped() {
        assert_eq!(sanitise(&"é".repeat(150)).chars().count(), TITLE_MAX);
    }

    #[test]
    fn bidi_controls_in_titles_show_as_escapes() {
        assert_eq!(
            sanitise("fix \u{202E}gnp.exe;\u{2066}x\u{2069}"),
            "fix <U+202E>gnp.exe<U+2066>x<U+2069>"
        );
        let alert = AlertWatcher::new(&attention(0, &[]))
            .observe(&attention(1, &["a\u{202E}b"]))
            .alert;
        assert_eq!(
            alert.map(|a| a.notice()).as_deref(),
            Some("approval needed · a<U+202E>b")
        );
    }

    #[test]
    fn the_cap_counts_escapes_and_never_splits_one() {
        // 95 chars, then an 8-char escape that would end at 103: it is left
        // out whole rather than cut to `<U+2`.
        let title = format!("{}\u{202E}tail", "a".repeat(TITLE_MAX - 5));
        assert_eq!(sanitise(&title), "a".repeat(TITLE_MAX - 5));
        // One that fits exactly is kept.
        let title = format!("{}\u{2066}tail", "a".repeat(TITLE_MAX - 8));
        let kept = sanitise(&title);
        assert_eq!(kept, format!("{}<U+2066>", "a".repeat(TITLE_MAX - 8)));
        assert_eq!(kept.chars().count(), TITLE_MAX);
        assert_eq!(sanitise(&"\u{202E}".repeat(50)).chars().count(), 96);
    }

    fn idled(session_id: SessionId, title: &str, at: u64) -> IdleAlert {
        IdleAlert {
            session_id,
            title: title.into(),
            at,
        }
    }

    fn idle_on(idle_alerts: u64, idle: Vec<IdleAlert>) -> Attention {
        Attention {
            idle_alerts,
            idle,
            ..Attention::default()
        }
    }

    fn notice(session_id: SessionId, title: &str) -> IdleNotice {
        IdleNotice {
            session_id,
            title: title.into(),
        }
    }

    #[test]
    fn idle_notices_once_per_session_that_rang() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut watcher = IdleWatcher::new();
        assert_eq!(
            watcher.observe(&Attention::default()),
            IdleUpdate::default()
        );
        let both = idle_on(2, vec![idled(a, "alpha", 1), idled(b, "beta", 2)]);
        assert_eq!(
            watcher.observe(&both),
            IdleUpdate {
                notices: vec![notice(a, "alpha"), notice(b, "beta")],
                cleared: vec![],
            }
        );
        assert_eq!(watcher.observe(&both), IdleUpdate::default(), "once");
    }

    #[test]
    fn a_session_counted_in_the_first_snapshot_still_notifies() {
        let a = SessionId::new();
        let mut watcher = IdleWatcher::new();
        assert_eq!(
            watcher
                .observe(&idle_on(1, vec![idled(a, "alpha", 1)]))
                .notices,
            [notice(a, "alpha")]
        );
    }

    #[test]
    fn a_session_working_again_is_cleared_once() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let mut watcher = IdleWatcher::new();
        watcher.observe(&idle_on(2, vec![idled(a, "alpha", 1), idled(b, "beta", 2)]));
        // `a` works again; `b` is still idle and is not cleared.
        assert_eq!(
            watcher.observe(&idle_on(2, vec![idled(b, "beta", 2)])),
            IdleUpdate {
                notices: vec![],
                cleared: vec![a],
            }
        );
        assert_eq!(
            watcher.observe(&idle_on(2, vec![idled(b, "beta", 2)])),
            IdleUpdate::default(),
            "once"
        );
        assert_eq!(
            watcher.observe(&idle_on(2, vec![])).cleared,
            [b],
            "gone too"
        );
        assert_eq!(watcher.observe(&idle_on(2, vec![])), IdleUpdate::default());
    }

    #[test]
    fn an_idle_notice_names_the_sanitised_title() {
        let a = SessionId::new();
        let mut watcher = IdleWatcher::new();
        let update = watcher.observe(&idle_on(1, vec![idled(a, "fix\u{1b}[2J; the tests", 1)]));
        assert_eq!(
            update.notices[0].notice(),
            format!("agent idle · {}", sanitise("fix\u{1b}[2J; the tests"))
        );
        assert_eq!(update.notices[0].notice(), "agent idle · fix[2J the tests");
    }
}
