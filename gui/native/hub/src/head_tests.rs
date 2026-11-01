#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::{TimeZone, Utc};
use skywave_core::{Core, CoreError, DeviceId, PairedCredentials, Seam};
use skywave_present::policy::DRAFT_SAVE_AFTER;
use skywave_proto::{CommandId, CommandKind, Event, EventBatch, EventKind};

use super::*;
use crate::signals::{ActionIntent, ListKindView, RouteView, ScreenView};

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
            context_name: "start".into(),
        };
        seam.apply_events(&EventBatch {
            from: Seq(seam.cursor().unwrap().0 + 1),
            events: vec![Event {
                session_id: session,
                at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap(),
                command_id: kind.logged_by_command().then(CommandId::new),
                context: skywave_proto::ContextId::first_of(session),
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
        rig.intent(GuiIntent::Act {
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

    fn intent(&mut self, intent: GuiIntent) -> Vec<Out> {
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
        self.intent(GuiIntent::DraftEdited {
            session_id: self.session.0.to_string(),
            text: text.into(),
            cursor_utf16: u32::try_from(text.encode_utf16().count()).unwrap(),
        });
    }

    fn submit(&mut self) -> Vec<Out> {
        self.intent(GuiIntent::ComposerSubmit {
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
    let outs = rig.intent(GuiIntent::Act {
        action: ActionIntent::Interrupt {
            session_id: "not-a-uuid".into(),
        },
    });
    assert_eq!(errors(&outs), ["not a session id: \"not-a-uuid\""]);
    let outs = rig.intent(GuiIntent::ExpandItem { key: "s:x".into() });
    assert_eq!(errors(&outs).len(), 1);
    let outs = rig.intent(GuiIntent::OpenFull {
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
    rig.intent(GuiIntent::Act {
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
fn backgrounding_saves_an_unsaved_draft_at_once_before_any_suspend() {
    let mut rig = Rig::new();
    rig.edit("unsent");
    assert!(rig.actions.is_empty(), "still within the debounce");
    let outs = rig.head.on_intent(GuiIntent::AppBackgrounded, rig.clock);
    let save = outs
        .iter()
        .position(|out| matches!(out, Out::Dispatch(_, Action::SaveDraft { .. })));
    let suspend = outs.iter().position(|out| matches!(out, Out::Suspend));
    assert!(save.is_some(), "no save in {outs:?}");
    assert!(suspend.is_none_or(|s| save < Some(s)), "suspended first");
    rig.apply(outs);
    assert_eq!(rig.kinds(), ["save draft"]);
    assert!(matches!(
        &rig.actions[0],
        Action::SaveDraft { text, .. } if text == "unsent"
    ));
    rig.results();
    let Screen::Session(view) = rig.core.view().screen else {
        panic!("left the session");
    };
    assert_eq!(view.draft.map(|d| d.text).as_deref(), Some("unsent"));
}

#[test]
fn backgrounding_without_an_unsaved_draft_dispatches_nothing() {
    let mut rig = Rig::new();
    rig.intent(GuiIntent::AppBackgrounded);
    assert!(rig.actions.is_empty(), "{:?}", rig.actions);
    rig.edit("saved");
    rig.wait(DRAFT_SAVE_AFTER + Duration::from_millis(10));
    rig.results();
    rig.actions.clear();
    rig.intent(GuiIntent::AppBackgrounded);
    assert!(rig.actions.is_empty(), "{:?}", rig.actions);
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
    rig.intent(GuiIntent::DraftEdited {
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
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().text, "first");
    assert_eq!(rig.composer().history_label.as_deref(), Some("1 of 1"));
    rig.intent(GuiIntent::HistoryNewer);
    assert_eq!(rig.composer().text, "half");
    assert_eq!(rig.composer().history_label, None);
}

#[test]
fn snippets_are_saved_from_rows_and_the_composer_and_inserted_at_the_cursor() {
    let mut rig = Rig::new();
    rig.edit("run the tests");
    rig.submit();
    rig.results();
    rig.view();
    let key = rig.head.sent.timeline.as_ref().unwrap().1[0].key.clone();
    let outs = rig.intent(GuiIntent::SaveSnippet {
        key,
        name: "tests".into(),
    });
    assert!(outs.iter().any(|o| matches!(
        o,
        Out::Effect(PlatformEffect::Notice { text }) if text == "saved as a snippet"
    )));
    rig.edit("commit this\nand push");
    rig.intent(GuiIntent::SaveComposerSnippet {
        session_id: rig.session.0.to_string(),
        name: " ".into(),
    });
    rig.view();
    let snippets = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Session { session } => session.snippets.clone(),
        other => panic!("not a session: {other:?}"),
    };
    let listed: Vec<_> = snippets(&rig)
        .into_iter()
        .map(|s| (s.label, s.detail))
        .collect();
    assert_eq!(
        listed,
        [
            ("commit this".to_owned(), None),
            ("tests".to_owned(), Some("run the tests".to_owned())),
        ]
    );

    rig.intent(GuiIntent::DraftEdited {
        session_id: rig.session.0.to_string(),
        text: "😀: now".into(),
        cursor_utf16: 3,
    });
    let tests = snippets(&rig)[1].snippet_id.clone();
    rig.actions.clear();
    rig.intent(GuiIntent::InsertSnippet {
        session_id: rig.session.0.to_string(),
        snippet_id: tests,
    });
    assert_eq!(rig.text(), "😀:run the tests now");
    assert_eq!(rig.head.session.as_ref().unwrap().composer.cursor, 15);
    assert_eq!(
        rig.composer().text,
        "😀:run the tests now",
        "the field is set"
    );
    assert_eq!(
        rig.kinds(),
        ["save draft", "other"],
        "the draft is kept first"
    );
    rig.view();
    assert_eq!(snippets(&rig)[0].label, "tests", "used, so first");
}

#[test]
fn app_ready_resends_everything() {
    let mut rig = Rig::new();
    assert!(rig.wait(Duration::ZERO).is_empty(), "nothing changed");
    let outs = rig.intent(GuiIntent::AppReady);
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
    head.on_intent(GuiIntent::PairPasted { text: uri }, clock);
    let filled = form(&head);
    assert_eq!(
        (filled.address.as_str(), filled.code.as_str()),
        ("host:7443", "ABCDE12345")
    );
    assert_eq!(filled.fingerprint, fp.to_string());
    assert!(filled.rev > before.rev);

    let outs = head.on_intent(
        GuiIntent::PairPasted {
            text: "skywave://pair?addr=x".into(),
        },
        clock,
    );
    assert_eq!(errors(&outs).len(), 1);
    assert_eq!(form(&head), filled, "a malformed URI is not form text");
    let outs = head.on_intent(
        GuiIntent::PairPasted {
            text: "host".into(),
        },
        clock,
    );
    assert!(outs.is_empty(), "plain text is the field's");
}

#[test]
fn a_scanned_pairing_uri_fills_the_form_and_any_other_code_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open_detached(dir.path()).unwrap();
    let clock = Clock::read();
    let mut head = Head::new();
    head.on_view(core.view(), clock);
    let form = |head: &Head| match &head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Pairing { form, .. } => form.clone(),
        other => panic!("not pairing: {other:?}"),
    };
    let fp = skywave_core::Fingerprint::of_der(b"ca");
    let uri = format!(
        "skywave://pair?addr=host%3A7443&code=ABCDE12345&fp={}",
        fp.to_hex()
    );
    head.on_intent(GuiIntent::PairScanned { text: uri }, clock);
    let filled = form(&head);
    assert_eq!(filled.address, "host:7443");

    let outs = head.on_intent(
        GuiIntent::PairScanned {
            text: "https://example.com".into(),
        },
        clock,
    );
    assert_eq!(errors(&outs).len(), 1, "a scan is never a field's text");
    assert_eq!(form(&head), filled);
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

#[test]
fn the_list_title_leaves_the_counts_to_the_toggle() {
    let mut rig = Rig::new();
    let list = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::SessionList {
            kind,
            title,
            rows,
            other_count,
            ..
        } => (*kind, title.clone(), rows.len(), *other_count),
        other => panic!("not the list: {other:?}"),
    };
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    assert_eq!(list(&rig), (ListKindView::Active, "Sessions".into(), 1, 0));
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::ArchivedSessions,
        },
    });
    rig.results();
    rig.view();
    assert_eq!(
        list(&rig),
        (ListKindView::Archived, "Sessions".into(), 0, 1)
    );
}

#[test]
fn a_tapped_candidate_fills_the_create_prompt() {
    let mut rig = Rig::new();
    let kind = EventKind::SessionCreated {
        workdir: "/srv/other".into(),
        context_name: "start".into(),
    };
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![Event {
                session_id: SessionId::new(),
                at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 5, 0).unwrap(),
                command_id: kind.logged_by_command().then(CommandId::new),
                context: skywave_proto::ContextId::first_of(SessionId::new()),
                kind,
            }],
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    rig.intent(GuiIntent::CreateOpen);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/".into(),
    });
    rig.intent(GuiIntent::CompletionComplete);
    rig.results();
    rig.view();
    let prompt = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::SessionList { create, .. } => create.clone().unwrap(),
        other => panic!("not the list: {other:?}"),
    };
    let listed = prompt(&rig).popup.expect("the candidates are listed");
    let paths: Vec<_> = listed.candidates.iter().map(|c| c.path.clone()).collect();
    assert!(paths.len() >= 2, "both recent workdirs: {paths:?}");

    let outs = rig.intent(GuiIntent::CompletionPick { index: 99 });
    assert!(outs.is_empty(), "a stale tap does nothing");
    let before = prompt(&rig).input_rev;
    rig.intent(GuiIntent::CompletionPick { index: 1 });
    let after = prompt(&rig);
    assert_eq!(after.input, format!("{}/", paths[1]));
    assert!(after.input_rev > before);
    assert!(after.popup.is_none(), "taking one closes the list");
}

impl Rig {
    /// Logs `kinds` as the agent's events in the open session, and shows
    /// the head the view that follows.
    fn log(&mut self, kinds: Vec<EventKind>) {
        let from = Seq(self.seam.cursor().unwrap().0 + 1);
        let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
        let events = kinds
            .into_iter()
            .map(|kind| Event {
                session_id: self.session,
                at,
                command_id: None,
                context: skywave_proto::ContextId::first_of(self.session),
                kind,
            })
            .collect();
        self.seam
            .apply_events(&EventBatch { from, events })
            .unwrap();
        self.view();
    }

    /// The rows the head last sent Dart.
    fn rows(&self) -> &[TimelineRow] {
        &self.head.sent.timeline.as_ref().unwrap().1
    }
}

/// An edit's call and result as skywaved logs them for the stub agent:
/// a diff body whole in its preview, the input as a blob, and a result
/// with no preview whose whole text is a 70 B blob.
fn an_edit() -> (Vec<EventKind>, skywave_proto::BlobId) {
    use skywave_proto::{BlobId, BlobRef, Body, CallId, ToolCall, ToolDisplay, ToolResult};
    let call = CallId::new();
    let output = BlobId::new();
    let kinds = vec![
        EventKind::ToolCall(ToolCall {
            call: Some(call),
            name: "Edit".into(),
            display: ToolDisplay {
                kind: skywave_proto::ToolKind::FileEdit,
                title: "/srv/work/demo.txt".into(),
                detail: None,
                body: Some(Body {
                    format: skywave_proto::BodyFormat::Diff,
                    preview: "-old\n+new\n".into(),
                    lines: 2,
                    full: None,
                }),
            },
            input: Some(BlobRef {
                id: BlobId::new(),
                size: 180,
            }),
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(call),
            is_error: false,
            status: Some("updated +1 -1".into()),
            preview: String::new(),
            lines: 1,
            output: Some(BlobRef {
                id: output,
                size: 70,
            }),
        }),
    ];
    (kinds, output)
}

#[test]
fn expanding_an_edit_downloads_its_output() {
    let mut rig = Rig::new();
    let (kinds, output) = an_edit();
    rig.log(kinds);
    let key = rig.rows().last().unwrap().key.clone();
    rig.intent(GuiIntent::ExpandItem { key });
    assert!(
        rig.actions
            .iter()
            .any(|a| matches!(a, Action::DownloadBlob(id) if *id == output)),
        "dispatched {:?}",
        rig.actions
    );
}

#[test]
fn an_edit_expanded_before_its_result_downloads_the_output_when_it_comes() {
    let mut rig = Rig::new();
    let (mut kinds, output) = an_edit();
    let result = kinds.pop().unwrap();
    // E.g. the approval panel shows the call it gates, expanded.
    rig.log(kinds);
    let key = rig.rows().last().unwrap().key.clone();
    rig.intent(GuiIntent::ExpandItem { key });
    rig.results();
    rig.view();
    rig.actions.clear();
    rig.log(vec![result]);
    assert!(
        matches!(rig.actions.as_slice(), [Action::DownloadBlob(id)] if *id == output),
        "dispatched {:?}",
        rig.actions
    );
    // Asked for once, not on every view while the request is in flight.
    rig.view();
    rig.results();
    rig.view();
    assert_eq!(rig.actions.len(), 1, "dispatched {:?}", rig.actions);
}

#[test]
fn an_expanded_section_says_its_blob_state_once() {
    let mut rig = Rig::new();
    let (kinds, output) = an_edit();
    rig.log(kinds);
    let key = rig.rows().last().unwrap().key.clone();
    rig.intent(GuiIntent::ExpandItem { key: key.clone() });
    let row = rig.rows().last().unwrap();
    assert!(row.expanded);
    let section = row
        .sections
        .iter()
        .find(|s| {
            s.blob
                .as_ref()
                .is_some_and(|b| b.blob_id == output.0.to_string())
        })
        .unwrap();
    // The label says where the download stands; the note has nothing to add.
    assert_eq!(section.note, None);
    assert_eq!(
        section.blob.as_ref().unwrap().label,
        "not downloaded · 70 B"
    );
    rig.results();
    rig.view();
    let section = &rig.rows().last().unwrap().sections[1];
    assert_eq!(section.note, None);
    assert_eq!(
        section.blob.as_ref().unwrap().label,
        "requested · waiting for a slot · 70 B"
    );
}

impl Rig {
    /// The agent settings as the head last told Dart.
    fn agent(&self) -> crate::signals::AgentSettingsView {
        match &self.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Session { session } => session.agent.clone(),
            other => panic!("not a session: {other:?}"),
        }
    }
}

#[test]
fn a_mode_switch_shows_as_waiting_and_a_refused_model_as_its_reason() {
    use skywave_proto::{ModelChoice, PermissionMode};
    let mut rig = Rig::new();
    assert_eq!(rig.agent().mode_label, "mode unknown", "not said yet");
    assert_eq!(rig.agent().model_label, "model unknown", "not said yet");
    rig.log(vec![
        EventKind::PermissionModeChanged {
            mode: PermissionMode::Ask,
            answers: None,
        },
        EventKind::ModelsOffered {
            models: vec![ModelChoice {
                id: "sonnet".into(),
                label: "Sonnet".into(),
                detail: Some("fast".into()),
                resolved: Some("model-s".into()),
                efforts: Vec::new(),
            }],
        },
        EventKind::ModelChanged {
            model: "model-s".into(),
            answers: None,
        },
    ]);
    let agent = rig.agent();
    assert_eq!(agent.mode_label, "ask");
    assert_eq!(agent.model_label, "Sonnet");
    assert_eq!(agent.modes.len(), 6);
    assert!(agent.modes[0].current);
    assert!(agent.modes[5].needs_confirmation);
    assert!(agent.models[0].current);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::SetPermissionMode {
            session_id: rig.session.0.to_string(),
            mode: crate::signals::PermissionModeView::Plan,
        },
    });
    assert_eq!(
        rig.actions,
        [Action::SetPermissionMode {
            session_id: rig.session,
            mode: PermissionMode::Plan,
        }]
    );
    rig.results();
    rig.view();
    let agent = rig.agent();
    assert_eq!(agent.mode_label, "ask → plan");
    assert!(agent.modes[2].pending && !agent.modes[0].pending);

    rig.log(vec![EventKind::ModelRefused {
        model: "big".into(),
        reason: "no model named".into(),
        answers: None,
    }]);
    let agent = rig.agent();
    assert_eq!(agent.model_label, "Sonnet");
    assert_eq!(
        agent.model_refusal.as_deref(),
        Some("big refused: no model named")
    );
    assert_eq!(agent.mode_refusal, None);
}

#[test]
fn a_model_asked_for_before_the_agent_names_one_reads_unknown() {
    let mut rig = Rig::new();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::SetModel {
            session_id: rig.session.0.to_string(),
            model: "opus".into(),
        },
    });
    rig.results();
    rig.view();
    assert_eq!(rig.agent().model_label, "unknown → opus");
    assert_eq!(rig.agent().mode_label, "mode unknown");
}

#[test]
fn an_effort_reads_unknown_then_its_level_and_its_switch_as_waiting() {
    use skywave_proto::{Effort, ModelChoice};
    let mut rig = Rig::new();
    assert_eq!(rig.agent().effort_label, "effort unknown", "not said yet");
    rig.log(vec![
        EventKind::ModelsOffered {
            models: vec![ModelChoice {
                id: "sonnet".into(),
                label: "Sonnet".into(),
                detail: None,
                resolved: Some("model-s".into()),
                efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            }],
        },
        EventKind::ModelChanged {
            model: "model-s".into(),
            answers: None,
        },
        EventKind::EffortChanged {
            effort: Some(Effort::High),
            answers: None,
        },
    ]);
    let agent = rig.agent();
    assert_eq!(agent.effort_label, "effort high");
    let flags = |a: &crate::signals::AgentSettingsView| {
        a.efforts
            .iter()
            .map(|o| (o.label.clone(), o.current, o.pending))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        flags(&agent),
        [
            ("low".to_owned(), false, false),
            ("medium".to_owned(), false, false),
            ("high".to_owned(), true, false),
        ]
    );

    rig.log(vec![EventKind::EffortRefused {
        effort: Effort::Max,
        reason: "not for this model".into(),
        answers: None,
    }]);
    assert_eq!(
        rig.agent().effort_refusal.as_deref(),
        Some("effort max refused: not for this model")
    );

    rig.intent(GuiIntent::Act {
        action: ActionIntent::SetEffort {
            session_id: rig.session.0.to_string(),
            effort: crate::signals::EffortView::Low,
        },
    });
    assert_eq!(
        rig.actions,
        [Action::SetEffort {
            session_id: rig.session,
            effort: Effort::Low,
        }]
    );
    rig.results();
    rig.view();
    let agent = rig.agent();
    assert_eq!(agent.effort_label, "high → low");
    assert_eq!(
        flags(&agent),
        [
            ("low".to_owned(), false, true),
            ("medium".to_owned(), false, false),
            ("high".to_owned(), true, false),
        ]
    );

    // The server never took it: the switch stops waiting and says why.
    let entry = rig
        .seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .find(|e| matches!(e.command.kind, CommandKind::SetEffort { .. }))
        .unwrap();
    rig.seam.command_undeliverable(entry.command.id).unwrap();
    rig.view();
    let agent = rig.agent();
    assert_eq!(agent.effort_label, "effort high");
    assert_eq!(
        agent.effort_refusal.as_deref(),
        Some("low failed: the server could not take it")
    );
    assert!(agent.efforts.iter().all(|o| !o.pending));
}

#[test]
fn a_model_that_takes_no_effort_reads_no_effort_with_nothing_to_pick() {
    use skywave_proto::ModelChoice;
    let mut rig = Rig::new();
    rig.log(vec![
        EventKind::ModelsOffered {
            models: vec![ModelChoice {
                id: "haiku".into(),
                label: "Haiku".into(),
                detail: None,
                resolved: Some("model-h".into()),
                efforts: Vec::new(),
            }],
        },
        EventKind::ModelChanged {
            model: "model-h".into(),
            answers: None,
        },
        EventKind::EffortChanged {
            effort: None,
            answers: None,
        },
    ]);
    let agent = rig.agent();
    assert_eq!(agent.effort_label, "no effort");
    assert!(agent.efforts.is_empty());
}
