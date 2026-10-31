#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::{TimeZone, Utc};
use skywave_core::{Core, CoreError, DeviceId, PairedCredentials, Seam};
use skywave_present::policy::DRAFT_SAVE_AFTER;
use skywave_proto::{CommandId, CommandKind, Event, EventBatch, EventKind};

use super::*;
use crate::signals::{ActionIntent, ScreenView};

/// The head over a detached, paired core, with one session open.
/// Dispatches go to the core one by one, in order, as the dispatcher
/// thread takes them; their results come back when asked for.
struct Rig {
    _dir: tempfile::TempDir,
    core: Core,
    seam: Seam,
    head: Head,
    clock: Clock,
    session: SessionId,
    /// Every action dispatched, in order.
    actions: Vec<Action>,
    /// Results not handed to the head yet.
    done: Vec<Dispatched>,
}

impl Rig {
    fn new() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open_detached(dir.path()).unwrap();
        let seam = core.seam();
        core.dispatch(Action::Pair {
            address: "srv:7443".into(),
            code: "ABCDE12345".into(),
            fingerprint: "ab:cd:ef".into(),
        })
        .unwrap();
        seam.pairing_succeeded(
            "srv:7443",
            PairedCredentials {
                device_id: DeviceId::new(),
                device_key_pem: "KEY".into(),
                device_cert_pem: "CERT".into(),
                ca_cert_pem: "CA".into(),
            },
        )
        .unwrap();
        let session = SessionId::new();
        let kind = EventKind::SessionCreated {
            workdir: "/srv/work".into(),
        };
        seam.apply_events(&EventBatch {
            from: Seq(seam.cursor().unwrap().0 + 1),
            events: vec![Event {
                session_id: session,
                at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap(),
                command_id: kind.logged_by_command().then(CommandId::new),
                kind,
            }],
        })
        .unwrap();
        let mut rig = Rig {
            _dir: dir,
            core,
            seam,
            head: Head::new(),
            clock: Clock::read(),
            session,
            actions: Vec::new(),
            done: Vec::new(),
        };
        rig.view();
        rig.intent(Intent::Act {
            action: ActionIntent::OpenSession {
                session_id: session.0.to_string(),
            },
        });
        rig.results();
        rig.view();
        rig.actions.clear();
        rig
    }

    fn apply(&mut self, outs: Vec<Out>) -> Vec<Out> {
        let mut rest = Vec::new();
        for out in outs {
            match out {
                Out::Dispatch(tag, action) => {
                    self.actions.push(action.clone());
                    let result = self.core.dispatch(action);
                    self.done.push(Dispatched { tag, result });
                }
                other => rest.push(other),
            }
        }
        rest
    }

    fn view(&mut self) -> Vec<Out> {
        let outs = self.head.on_view(self.core.view(), self.clock);
        self.apply(outs)
    }

    fn intent(&mut self, intent: Intent) -> Vec<Out> {
        let outs = self.head.on_intent(intent, self.clock);
        self.apply(outs)
    }

    fn results(&mut self) -> Vec<Out> {
        let mut all = Vec::new();
        for done in std::mem::take(&mut self.done) {
            let outs = self.head.on_dispatched(done, self.clock);
            all.extend(self.apply(outs));
        }
        all
    }

    fn wait(&mut self, d: Duration) -> Vec<Out> {
        self.clock = self.clock.after(d);
        let outs = self.head.on_wake(self.clock);
        self.apply(outs)
    }

    fn edit(&mut self, text: &str) {
        self.intent(Intent::DraftEdited {
            session_id: self.session.0.to_string(),
            text: text.into(),
            cursor_utf16: u32::try_from(text.encode_utf16().count()).unwrap(),
        });
    }

    fn submit(&mut self) -> Vec<Out> {
        self.intent(Intent::ComposerSubmit {
            session_id: self.session.0.to_string(),
        })
    }

    /// The composer as the head last told Dart.
    fn composer(&self) -> ComposerView {
        match &self.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Session { session } => session.composer.clone(),
            other => panic!("not a session: {other:?}"),
        }
    }

    fn text(&self) -> String {
        self.head.session.as_ref().unwrap().composer.text.clone()
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.actions
            .iter()
            .map(|a| match a {
                Action::SaveDraft { .. } => "save draft",
                Action::SendMessage { .. } => "send",
                Action::MarkRead { .. } => "mark read",
                Action::Back => "back",
                _ => "other",
            })
            .collect()
    }

    fn queued_messages(&self) -> Vec<String> {
        self.seam
            .pending_outbox()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.command.kind {
                CommandKind::SendMessage { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }
}

fn errors(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Effect(PlatformEffect::Error { text }) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_malformed_id_is_an_error_not_an_action() {
    let mut rig = Rig::new();
    let outs = rig.intent(Intent::Act {
        action: ActionIntent::Interrupt {
            session_id: "not-a-uuid".into(),
        },
    });
    assert_eq!(errors(&outs), ["not a session id: \"not-a-uuid\""]);
    let outs = rig.intent(Intent::ExpandItem { key: "s:x".into() });
    assert_eq!(errors(&outs).len(), 1);
    let outs = rig.intent(Intent::OpenFull {
        blob_id: "?".into(),
    });
    assert_eq!(errors(&outs).len(), 1);
    assert!(rig.actions.is_empty());
}

#[test]
fn an_idle_draft_is_saved_before_the_message_that_follows() {
    let mut rig = Rig::new();
    rig.edit("hello");
    assert!(rig.actions.is_empty(), "saved per keystroke");
    rig.wait(DRAFT_SAVE_AFTER + Duration::from_millis(10));
    rig.submit();
    assert_eq!(rig.kinds(), ["save draft", "send"]);
    rig.results();
    rig.view();
    // Applied in that order: the enqueue dropped the saved draft.
    assert_eq!(rig.queued_messages(), ["hello"]);
    let Screen::Session(view) = rig.core.view().screen else {
        panic!("left the session");
    };
    assert_eq!(view.draft, None);
}

#[test]
fn leaving_saves_the_draft_first() {
    let mut rig = Rig::new();
    rig.edit("unsent");
    rig.intent(Intent::Act {
        action: ActionIntent::Back,
    });
    let kinds = rig.kinds();
    assert_eq!(kinds.first(), Some(&"save draft"));
    assert_eq!(kinds.last(), Some(&"back"));
    rig.results();
    rig.view();
    let Screen::SessionList(list) = rig.core.view().screen else {
        panic!("still in the session");
    };
    assert!(list.sessions[0].has_draft);
}

#[test]
fn the_composer_clears_only_once_the_send_is_taken() {
    let mut rig = Rig::new();
    rig.edit("hello");
    rig.submit();
    assert_eq!(rig.kinds(), ["send"]);
    // Dispatched, no result yet: the text stays.
    assert_eq!(rig.text(), "hello");
    let rev = rig.composer().rev;
    rig.results();
    let composer = rig.composer();
    assert_eq!((composer.text.as_str(), composer.cursor_utf16), ("", 0));
    assert!(composer.rev > rev, "Dart's field is not reset");
}

#[test]
fn a_refused_send_keeps_the_text_and_is_not_resent() {
    let mut rig = Rig::new();
    rig.edit("hello");
    rig.submit();
    // The core refused it (Err: nothing was persisted).
    let tag = rig.done.remove(0).tag;
    let outs = rig.head.on_dispatched(
        Dispatched {
            tag,
            result: Err(CoreError::Runtime(std::io::Error::other("disk full"))),
        },
        rig.clock,
    );
    assert_eq!(errors(&outs).len(), 1, "{outs:?}");
    assert_eq!(rig.text(), "hello");
    rig.wait(Duration::from_secs(5));
    rig.results();
    let sends = rig.kinds().into_iter().filter(|k| *k == "send").count();
    assert_eq!(sends, 1);
}

#[test]
fn a_failed_draft_save_is_retried() {
    let mut rig = Rig::new();
    rig.edit("draft");
    rig.wait(DRAFT_SAVE_AFTER + Duration::from_millis(10));
    assert_eq!(rig.kinds(), ["save draft"]);
    let tag = rig.done.remove(0).tag;
    let outs = rig.head.on_dispatched(
        Dispatched {
            tag,
            result: Err(CoreError::Runtime(std::io::Error::other("disk full"))),
        },
        rig.clock,
    );
    assert_eq!(errors(&outs).len(), 1);
    assert!(rig.head.next_wake(rig.clock).is_some());
    rig.wait(DRAFT_SAVE_AFTER + Duration::from_millis(10));
    assert_eq!(rig.kinds(), ["save draft", "save draft"]);
}

#[test]
fn an_emoji_cursor_crosses_in_utf16() {
    let mut rig = Rig::new();
    rig.intent(Intent::DraftEdited {
        session_id: rig.session.0.to_string(),
        text: "a😀b".into(),
        cursor_utf16: 3,
    });
    assert_eq!(rig.head.session.as_ref().unwrap().composer.cursor, 2);
}

#[test]
fn history_recalls_and_restores_the_stash() {
    let mut rig = Rig::new();
    rig.edit("first");
    rig.submit();
    rig.results();
    rig.view();
    rig.edit("half");
    rig.intent(Intent::HistoryOlder);
    assert_eq!(rig.composer().text, "first");
    assert_eq!(rig.composer().history_label.as_deref(), Some("1 of 1"));
    rig.intent(Intent::HistoryNewer);
    assert_eq!(rig.composer().text, "half");
    assert_eq!(rig.composer().history_label, None);
}

#[test]
fn app_ready_resends_everything() {
    let mut rig = Rig::new();
    assert!(rig.wait(Duration::ZERO).is_empty(), "nothing changed");
    let outs = rig.intent(Intent::AppReady);
    assert!(outs.iter().any(|o| matches!(o, Out::Link(_))));
    assert!(outs.iter().any(|o| matches!(o, Out::Screen(_))));
    assert!(outs.iter().any(|o| matches!(o, Out::Search(_))));
    assert!(
        outs.iter()
            .any(|o| matches!(o, Out::Timeline(TimelineUpdate::Snapshot { .. })))
    );
}

#[test]
fn a_pasted_pairing_uri_fills_the_form_and_a_malformed_one_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open_detached(dir.path()).unwrap();
    let clock = Clock::read();
    let mut head = Head::new();
    head.on_view(core.view(), clock);
    let form = |head: &Head| match &head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Pairing { form, .. } => form.clone(),
        other => panic!("not pairing: {other:?}"),
    };
    let before = form(&head);
    let fp = skywave_core::Fingerprint::of_der(b"ca");
    let uri = format!(
        "skywave://pair?addr=host%3A7443&code=ABCDE12345&fp={}",
        fp.to_hex()
    );
    head.on_intent(Intent::PairPasted { text: uri }, clock);
    let filled = form(&head);
    assert_eq!(
        (filled.address.as_str(), filled.code.as_str()),
        ("host:7443", "ABCDE12345")
    );
    assert_eq!(filled.fingerprint, fp.to_string());
    assert!(filled.rev > before.rev);

    let outs = head.on_intent(
        Intent::PairPasted {
            text: "skywave://pair?addr=x".into(),
        },
        clock,
    );
    assert_eq!(errors(&outs).len(), 1);
    assert_eq!(form(&head), filled, "a malformed URI is not form text");
    let outs = head.on_intent(
        Intent::PairPasted {
            text: "host".into(),
        },
        clock,
    );
    assert!(outs.is_empty(), "plain text is the field's");
}

#[test]
fn a_new_approval_notifies_but_the_first_snapshot_does_not() {
    let mut rig = Rig::new();
    let mut view = rig.core.view();
    view.attention.alerts += 1;
    view.attention
        .approvals
        .push(skywave_core::PendingApprovalRef {
            id: skywave_core::ApprovalId::new(),
            session_id: rig.session,
            title: "work\u{7}".into(),
            alerted_at: view.attention.alerts,
        });
    let outs = rig.head.on_view(view, rig.clock);
    let notified: Vec<_> = outs
        .iter()
        .filter_map(|o| match o {
            Out::Effect(PlatformEffect::Notify { title, body, .. }) => Some((title, body)),
            _ => None,
        })
        .collect();
    assert_eq!(
        notified,
        [(&"work".to_owned(), &"approval needed · work".to_owned())]
    );
    // A head that starts at that count does not alert.
    let mut fresh = Head::new();
    let outs = fresh.on_view(rig.head.view.clone().unwrap(), rig.clock);
    assert!(
        !outs
            .iter()
            .any(|o| matches!(o, Out::Effect(PlatformEffect::Notify { .. })))
    );
}
