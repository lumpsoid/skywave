#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::{TimeZone, Utc};
use skywave_core::{Core, CoreError, DeviceId, PairedCredentials, Seam};
use skywave_present::policy::DRAFT_SAVE_AFTER;
use skywave_proto::{CommandId, CommandKind, Event, EventBatch, EventKind};

use super::*;
use crate::signals::{
    ActionIntent, ListKindView, RouteView, RowKind, ScreenView, ShellPromptView, ToolState,
};

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
    /// This device.
    me: DeviceId,
    /// Every action dispatched, in order.
    actions: Vec<Action>,
    /// Results not handed to the head yet.
    done: Vec<Dispatched>,
}

#[test]
fn an_adopted_session_starts_with_a_divider_row() {
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let new = SessionId::new();
    let events = vec![Event {
        session_id: new,
        at,
        command_id: None,
        context: skywave_proto::ContextId::first_of(new),
        kind: EventKind::SessionCreated {
            workdir: "/srv/adopted".into(),
            context_name: "start".into(),
            from: Some(skywave_proto::SessionOrigin::Agent {
                conversation: "3f2a-91bc".into(),
            }),
        },
    }];
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events,
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::OpenSession {
            session_id: new.0.to_string(),
        },
    });
    rig.results();
    rig.view();
    let rows: Vec<_> = rig
        .rows()
        .iter()
        .map(|row| (row.kind, row.text.clone(), row.selectable))
        .collect();
    assert_eq!(
        rows,
        [(
            RowKind::ContextDivider,
            "Continues agent conversation 3f2a-91bc from outside Skywave".to_owned(),
            false
        )]
    );
}

/// An event of `session` logged at `at`.
fn event_at(session: SessionId, at: chrono::DateTime<Utc>, kind: EventKind) -> Event {
    Event {
        session_id: session,
        at,
        command_id: None,
        context: skywave_proto::ContextId::first_of(session),
        kind,
    }
}

/// The created event of a session that adopted `conversation`.
fn adopted_created(conversation: &str) -> EventKind {
    EventKind::SessionCreated {
        workdir: "/srv/adopted".into(),
        context_name: "start".into(),
        from: Some(skywave_proto::SessionOrigin::Agent {
            conversation: conversation.into(),
        }),
    }
}

#[test]
fn imported_history_has_its_own_row_kinds_at_the_agents_time() {
    use skywave_proto::{CallId, ImportOutcome, ImportedItem, ToolKind, ToolResult};
    let mut rig = Rig::new();
    let logged = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let agent_at = Utc.with_ymd_and_hms(2026, 10, 30, 9, 15, 0).unwrap();
    let new = SessionId::new();
    let call = CallId::new();
    let EventKind::ToolCall(tool_call) = a_call(None, call, "Bash", ToolKind::Shell, "cargo test")
    else {
        unreachable!()
    };
    let result = |call| ToolResult {
        call,
        is_error: false,
        status: Some("exit 0".into()),
        preview: "ok".into(),
        lines: 1,
        output: None,
    };
    let imported = |item| EventKind::Imported {
        at: Some(agent_at),
        item,
    };
    let kinds = vec![
        adopted_created("3f2a"),
        imported(ImportedItem::UserMessage {
            text: "fix the build".into(),
        }),
        imported(ImportedItem::ToolCall(tool_call)),
        imported(ImportedItem::ToolResult(result(Some(call)))),
        imported(ImportedItem::ToolResult(result(None))),
        imported(ImportedItem::AgentMessage {
            text: "fixed".into(),
        }),
        EventKind::ImportFinished {
            outcome: ImportOutcome::Imported { items: 5 },
        },
        EventKind::AgentMessage {
            parent: None,
            text: "live again".into(),
        },
    ];
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: kinds
                .into_iter()
                .map(|kind| event_at(new, logged, kind))
                .collect(),
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::OpenSession {
            session_id: new.0.to_string(),
        },
    });
    rig.results();
    rig.view();
    let rows: Vec<_> = rig
        .rows()
        .iter()
        .map(|row| {
            (
                row.kind,
                row.mark.clone(),
                row.author.clone(),
                row.text.clone(),
                row.tool.as_ref().map(|t| (t.name.clone(), t.state)),
            )
        })
        .collect();
    let imported = Some("imported".to_owned());
    assert_eq!(
        rows,
        [
            (
                RowKind::ContextDivider,
                None,
                None,
                "Continues agent conversation 3f2a from outside Skywave".into(),
                None
            ),
            (
                RowKind::ImportedUserMessage,
                imported.clone(),
                Some("you".into()),
                "fix the build".into(),
                None
            ),
            (
                RowKind::ImportedTool,
                imported.clone(),
                None,
                String::new(),
                Some(("Bash".into(), ToolState::Done))
            ),
            (
                RowKind::ImportedTool,
                imported.clone(),
                None,
                String::new(),
                Some(("result".into(), ToolState::Plain))
            ),
            (
                RowKind::ImportedAgentMessage,
                imported,
                Some("agent".into()),
                "fixed".into(),
                None
            ),
            (
                RowKind::ContextDivider,
                None,
                None,
                "Continued in Skywave from here".into(),
                None
            ),
            (
                RowKind::AgentMessage,
                None,
                Some("agent".into()),
                "live again".into(),
                None
            ),
        ]
    );
    // Imported rows show the agent's time, the session's own the log's.
    let times: Vec<_> = rig.rows().iter().map(|r| r.time_label.clone()).collect();
    assert_eq!(times[0].as_deref(), Some("12:01"));
    assert_eq!(times[1].as_deref(), Some("09:15"));
    assert_eq!(times[2], None, "same minute as the row above");
    assert_eq!(times[5].as_deref(), Some("12:01"));
    assert_eq!(rig.rows()[1].at_ms, Some(agent_at.timestamp_millis()));
    // Imported messages select (copy), like the session's own.
    assert!(rig.rows()[1].selectable);
}

#[test]
fn an_import_that_found_nothing_says_so_in_its_divider() {
    use skywave_proto::ImportOutcome;
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let new = SessionId::new();
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![
                event_at(new, at, adopted_created("3f2a")),
                event_at(
                    new,
                    at,
                    EventKind::ImportFinished {
                        outcome: ImportOutcome::NotFound,
                    },
                ),
            ],
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::OpenSession {
            session_id: new.0.to_string(),
        },
    });
    rig.results();
    rig.view();
    assert_eq!(
        rig.rows()[1].text,
        "history not found · continued in Skywave from here"
    );
    assert_eq!(rig.rows()[1].kind, RowKind::ContextDivider);
}

impl Rig {
    /// The session list as the head last sent it.
    fn list(&self) -> ScreenView {
        match &self.head.sent.screen.as_ref().unwrap().screen {
            list @ ScreenView::SessionList { .. } => list.clone(),
            other => panic!("not the list: {other:?}"),
        }
    }

    /// The conversation picker as the head last sent it.
    fn conversations(&self) -> Option<crate::signals::ConversationsView> {
        match self.list() {
            ScreenView::SessionList { conversations, .. } => conversations,
            _ => unreachable!(),
        }
    }

    /// Back to the session list, online.
    fn online_on_the_list(&mut self) {
        self.seam
            .set_link(skywave_core::Link::Online { latency_ms: 5 });
        self.intent(GuiIntent::Act {
            action: ActionIntent::Back,
        });
        self.results();
        self.view();
    }

    /// Answers the newest conversation listing the core asked for with
    /// `listed`.
    fn answer_conversations(
        &mut self,
        requests: &std::sync::mpsc::Receiver<skywave_core::Request>,
        listed: Vec<skywave_proto::OutsideConversation>,
        truncated: bool,
    ) {
        let req = requests
            .try_iter()
            .filter_map(|r| match r {
                skywave_core::Request::ListConversations { req } => Some(req),
                _ => None,
            })
            .last()
            .expect("the core asked for the conversations");
        self.seam.conversations_listed(
            req,
            Ok(skywave_proto::ConversationsResult::Listed {
                conversations: listed,
                truncated,
            }),
        );
        self.view();
    }
}

fn outside(id: &str, session: Option<SessionId>) -> skywave_proto::OutsideConversation {
    skywave_proto::OutsideConversation {
        id: id.into(),
        workdir: "/home/me/personal/skywave".into(),
        title: Some(format!("title of {id}")),
        first_prompt: Some("fix the build".into()),
        last_active: Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap(),
        messages: 42,
        session,
    }
}

#[test]
fn the_conversation_picker_lists_and_continues_one() {
    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    assert_eq!(rig.conversations(), None);

    rig.intent(GuiIntent::ConversationsOpen);
    assert_eq!(rig.actions.last(), Some(&Action::ListConversations));
    rig.results();
    rig.view();
    let loading = rig.conversations().expect("open while it lists");
    assert_eq!(loading.status_label.as_deref(), Some("listing…"));
    assert_eq!(
        loading.empty_label.as_deref(),
        Some("Asking the server for its agent's conversations…")
    );
    assert!(!loading.unavailable);

    rig.answer_conversations(
        &requests,
        vec![outside("aaa", Some(rig.session)), outside("bbb", None)],
        true,
    );
    let ready = rig.conversations().unwrap();
    assert_eq!(
        ready.status_label.as_deref(),
        Some("only the newest are listed")
    );
    assert_eq!(ready.empty_label, None);
    let row = &ready.rows[0];
    assert_eq!(row.conversation_id, "aaa");
    assert_eq!(row.title, "title of aaa");
    assert_eq!(row.detail, "…/personal/skywave · 2026-10-01 · 42 messages");
    assert_eq!(row.session_id, Some(rig.session.0.to_string()));
    assert!(
        row.link_label
            .as_deref()
            .is_some_and(|l| l.starts_with("already in Skywave: ")),
        "{row:?}"
    );
    assert_eq!(ready.rows[1].session_id, None);
    assert_eq!(ready.rows[1].link_label, None);

    // One no longer listed does nothing.
    let outs = rig.intent(GuiIntent::ConversationPick {
        conversation_id: "gone".into(),
    });
    assert!(outs.is_empty());
    assert!(queued_creations(&rig).is_empty());

    rig.intent(GuiIntent::ConversationPick {
        conversation_id: "bbb".into(),
    });
    assert_eq!(
        rig.actions.last(),
        Some(&Action::CreateSession {
            workdir: "/home/me/personal/skywave".into(),
            conversation: Some("bbb".into()),
        })
    );
    rig.results();
    rig.view();
    assert_eq!(rig.conversations(), None, "taking one closes the picker");
    // The pending creation says it imports.
    let ScreenView::SessionList { creating, .. } = rig.list() else {
        unreachable!()
    };
    assert_eq!(creating.len(), 1);
    assert_eq!(
        creating[0].note.as_deref(),
        Some("will import when online"),
        "{creating:?}"
    );
}

#[test]
fn a_picked_conversation_closes_the_new_session_prompt() {
    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::CreateOpen);
    rig.intent(GuiIntent::ConversationsOpen);
    rig.results();
    rig.view();
    rig.answer_conversations(&requests, vec![outside("aaa", None)], false);
    let ScreenView::SessionList { create, .. } = rig.list() else {
        unreachable!()
    };
    assert!(create.is_some(), "the prompt stays under the picker");
    rig.intent(GuiIntent::ConversationPick {
        conversation_id: "aaa".into(),
    });
    rig.results();
    rig.view();
    let ScreenView::SessionList { create, .. } = rig.list() else {
        unreachable!()
    };
    assert_eq!(create, None);
}

#[test]
fn closing_the_conversation_picker_keeps_the_prompt() {
    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::CreateOpen);
    rig.intent(GuiIntent::ConversationsOpen);
    rig.results();
    rig.view();
    rig.answer_conversations(&requests, Vec::new(), false);
    assert_eq!(
        rig.conversations().unwrap().empty_label.as_deref(),
        Some("The server's agent has no conversations under the roots.")
    );
    rig.intent(GuiIntent::ConversationsClose);
    assert_eq!(rig.actions.last(), Some(&Action::EndConversations));
    rig.results();
    rig.view();
    assert_eq!(rig.conversations(), None);
    let ScreenView::SessionList { create, .. } = rig.list() else {
        unreachable!()
    };
    assert!(create.is_some());
    // Closed, a second close dispatches nothing.
    let before = rig.actions.len();
    rig.intent(GuiIntent::ConversationsClose);
    assert_eq!(rig.actions.len(), before);
}

#[test]
fn offline_the_conversation_picker_says_why_it_is_empty() {
    let mut rig = Rig::new();
    rig.seam.set_link(skywave_core::Link::Reconnecting {
        attempt: 1,
        retry_at: Utc::now() + chrono::TimeDelta::seconds(2),
        offline_since: Utc::now(),
    });
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    rig.intent(GuiIntent::ConversationsOpen);
    rig.results();
    rig.view();
    let picker = rig.conversations().unwrap();
    assert!(picker.unavailable);
    assert_eq!(picker.status_label, None);
    assert_eq!(
        picker.empty_label.as_deref(),
        Some("offline · the server lists its conversations once connected")
    );
    assert!(picker.rows.is_empty());
}

#[test]
fn the_conversation_picker_opens_only_over_the_active_list() {
    let mut rig = Rig::new();
    // On the session screen.
    let outs = rig.intent(GuiIntent::ConversationsOpen);
    assert!(outs.is_empty());
    assert!(!rig.actions.contains(&Action::ListConversations));
}

#[test]
fn an_adopted_session_row_says_its_history_is_importing() {
    use skywave_proto::ImportOutcome;
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let new = SessionId::new();
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![event_at(new, at, adopted_created("3f2a"))],
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    let importing = |rig: &Rig| match rig.list() {
        ScreenView::SessionList { rows, .. } => rows
            .iter()
            .map(|r| (r.session_id.clone(), r.importing_label.clone()))
            .collect::<Vec<_>>(),
        _ => unreachable!(),
    };
    let label = |rig: &Rig, id: SessionId| {
        importing(rig)
            .into_iter()
            .find(|(s, _)| *s == id.0.to_string())
            .unwrap()
            .1
    };
    assert_eq!(label(&rig, new).as_deref(), Some("importing history…"));
    assert_eq!(label(&rig, rig.session), None);
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![event_at(
                new,
                at,
                EventKind::ImportFinished {
                    outcome: ImportOutcome::Unavailable,
                },
            )],
        })
        .unwrap();
    rig.view();
    assert_eq!(label(&rig, new), None);
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
        let me = DeviceId::new();
        seam.pairing_succeeded(
            "srv:7443",
            PairedCredentials {
                device_id: me,
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
            from: None,
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
            me,
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

    /// The shell prompt; the composer must be in shell mode.
    fn prompt(&self) -> ShellPromptView {
        self.composer().prompt.expect("no shell prompt")
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
                Action::RunShell { .. } => "run shell",
                Action::StopShell { .. } => "stop shell",
                Action::SetComposerMode { .. } => "set mode",
                Action::Discard(_) => "discard",
                Action::SaveSnippet { .. } => "save snippet",
                Action::UseSnippet(_) => "use snippet",
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
    let key = rig.head.sent.timeline.as_ref().unwrap().2[0].key.clone();
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
        ["save draft", "use snippet"],
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
fn the_usage_screen_totals_what_usage_show_picks() {
    use crate::signals::{UsageGroupingView, UsageRowView, UsageSpanView};

    let mut rig = Rig::new();
    let turn = |cost| Event {
        session_id: rig.session,
        at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap(),
        command_id: None,
        context: skywave_proto::ContextId::first_of(rig.session),
        kind: EventKind::TurnUsage(skywave_proto::TurnUsage {
            tokens_in: Some(3_100),
            tokens_out: Some(812),
            cost_usd: cost,
            model: Some("model-a".into()),
            ..skywave_proto::TurnUsage::default()
        }),
    };
    let events = vec![turn(Some(1.27)), turn(None)];
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events,
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Usage,
        },
    });
    rig.results();
    rig.intent(GuiIntent::UsageShow {
        span: UsageSpanView::All,
        grouping: UsageGroupingView::Workdir,
    });
    rig.view();
    let row = |label: &str, cost: &str| UsageRowView {
        label: label.into(),
        archived: false,
        cost_label: cost.into(),
        tokens_label: "6.2k in · 1.6k out".into(),
        turns_label: "2 turns".into(),
    };
    match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Usage {
            span,
            grouping,
            total,
            rows,
            empty_label,
            note,
        } => {
            assert_eq!(
                (*span, *grouping),
                (UsageSpanView::All, UsageGroupingView::Workdir)
            );
            assert_eq!(*total, row("Total", "$1.27 + 1 unpriced"));
            assert_eq!(*rows, [row("/srv/work", "$1.27 + 1 unpriced")]);
            assert_eq!(*empty_label, None);
            assert!(note.contains("deleted from this device"), "{note}");
        }
        other => panic!("not the usage screen: {other:?}"),
    }
}

#[test]
fn a_tapped_candidate_fills_the_create_prompt() {
    let mut rig = Rig::new();
    let kind = EventKind::SessionCreated {
        workdir: "/srv/other".into(),
        context_name: "start".into(),
        from: None,
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
    /// the head the view that follows; what it sent Dart then.
    fn log(&mut self, kinds: Vec<EventKind>) -> Vec<Out> {
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
        self.view()
    }

    /// The rows the head last sent Dart.
    fn rows(&self) -> &[TimelineRow] {
        &self.head.sent.timeline.as_ref().unwrap().2
    }
}

#[test]
fn a_context_cleared_or_switched_is_a_divider_row() {
    let mut rig = Rig::new();
    let walrus = skywave_proto::ContextId::new();
    let me = rig.seam.identity().unwrap().device_id();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let event = |context, kind| Event {
        session_id: rig.session,
        at,
        command_id: Some(CommandId::new()),
        context,
        kind,
    };
    let events = vec![
        event(
            walrus,
            EventKind::ContextStarted {
                by: DeviceId::new(),
                name: "velvet walrus".into(),
            },
        ),
        event(
            skywave_proto::ContextId::first_of(rig.session),
            EventKind::ContextSwitched { by: me },
        ),
    ];
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events,
        })
        .unwrap();
    rig.view();
    let rows: Vec<_> = rig
        .rows()
        .iter()
        .map(|row| (row.kind, row.text.clone(), row.selectable))
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, RowKind::ContextDivider);
    assert!(
        rows[0]
            .1
            .starts_with("New context · velvet walrus · by device …"),
        "{rows:?}"
    );
    assert_eq!(
        rows[1],
        (
            RowKind::ContextDivider,
            "Back to start · by you".to_owned(),
            false
        )
    );
}

#[test]
fn a_moved_context_is_a_divider_row() {
    let mut rig = Rig::new();
    let me = rig.seam.identity().unwrap().device_id();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let new = SessionId::new();
    let start = skywave_proto::ContextId::first_of(rig.session);
    // As the server logs a move: the new session first, then the move in
    // the source.
    let events = vec![
        Event {
            session_id: new,
            at,
            command_id: None,
            context: skywave_proto::ContextId::first_of(new),
            kind: EventKind::SessionCreated {
                workdir: "/srv/forked".into(),
                context_name: "start".into(),
                from: Some(skywave_proto::SessionOrigin::Context(
                    skywave_proto::ContextRef {
                        session: rig.session,
                        context: start,
                    },
                )),
            },
        },
        Event {
            session_id: rig.session,
            at,
            command_id: Some(CommandId::new()),
            context: start,
            kind: EventKind::ContextMoved { to: new, by: me },
        },
    ];
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events,
        })
        .unwrap();
    rig.view();
    let rows = |rig: &Rig| -> Vec<_> {
        rig.rows()
            .iter()
            .map(|row| (row.kind, row.text.clone(), row.selectable))
            .collect()
    };
    assert_eq!(
        rows(&rig),
        [(
            RowKind::ContextDivider,
            "start went on in “forked” · by you".to_owned(),
            false
        )]
    );

    rig.intent(GuiIntent::Act {
        action: ActionIntent::OpenSession {
            session_id: new.0.to_string(),
        },
    });
    rig.results();
    rig.view();
    assert_eq!(
        rows(&rig),
        [(
            RowKind::ContextDivider,
            "Continues start from “work”".to_owned(),
            false
        )]
    );
}

impl Rig {
    fn open_session(&self) -> crate::signals::SessionScreen {
        match &self.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Session { session } => session.clone(),
            other => panic!("not a session: {other:?}"),
        }
    }
}

#[test]
fn context_intents_map_to_actions_and_the_screen_lists_the_contexts() {
    let mut rig = Rig::new();
    let first = skywave_proto::ContextId::first_of(rig.session);
    let before = rig.open_session();
    assert!(!before.can_clear, "nothing to clear in an empty context");
    assert_eq!(before.contexts.len(), 1, "{:?}", before.contexts);
    assert!(before.contexts[0].current);
    assert_eq!(before.context_id, first.0.to_string());
    assert_eq!(before.viewing_hint, None);

    rig.log(vec![EventKind::UserMessage {
        device_id: DeviceId::new(),
        text: "fix the flaky test\nplease".into(),
    }]);
    let session = rig.open_session();
    assert!(session.can_clear, "positive control");
    assert_eq!(session.contexts[0].label, "start · fix the flaky test");

    rig.intent(GuiIntent::Act {
        action: ActionIntent::ClearContext {
            session_id: rig.session.0.to_string(),
        },
    });
    assert_eq!(
        rig.actions,
        [Action::ClearContext {
            session_id: rig.session
        }]
    );
    rig.actions.clear();
    rig.results();
    rig.view();
    let session = rig.open_session();
    assert_eq!(session.clear_label.as_deref(), Some("new context queued"));
    assert_eq!(session.contexts.len(), 2, "{:?}", session.contexts);
    let (old, new) = (&session.contexts[0], &session.contexts[1]);
    assert!(!old.current && new.current);
    assert_eq!(new.outgoing_label.as_deref(), Some("queued"));
    assert_eq!(session.context_id, new.context_id);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::ViewContext {
            session_id: rig.session.0.to_string(),
            context_id: Some(old.context_id.clone()),
        },
    });
    assert_eq!(
        rig.actions,
        [Action::ViewContext {
            session_id: rig.session,
            context: Some(first)
        }]
    );
    rig.actions.clear();
    rig.results();
    rig.view();
    let session = rig.open_session();
    assert_eq!(session.viewing_id, Some(first.0.to_string()));
    let hint = session.viewing_hint.expect("viewing an earlier context");
    assert!(
        hint.starts_with("Viewing start — messages go to "),
        "{hint}"
    );

    rig.intent(GuiIntent::Act {
        action: ActionIntent::SwitchContext {
            session_id: rig.session.0.to_string(),
            context_id: first.0.to_string(),
        },
    });
    assert_eq!(
        rig.actions,
        [Action::SwitchContext {
            session_id: rig.session,
            context: first
        }]
    );
}

#[test]
fn a_context_moves_once_the_core_offers_it_and_a_queued_move_can_be_discarded() {
    let mut rig = Rig::new();
    let start = skywave_proto::ContextId::first_of(rig.session);
    rig.log(vec![EventKind::UserMessage {
        device_id: DeviceId::new(),
        text: "fix it".into(),
    }]);
    let row = rig.open_session().contexts[0].clone();
    assert!(!row.can_move, "negative control: no turn answered yet");
    assert!(
        row.move_refusal
            .as_deref()
            .unwrap()
            .starts_with("can't move yet"),
        "{row:?}"
    );
    assert_eq!((row.moved_label, row.discard_move), (None, None));

    rig.log(vec![
        EventKind::SessionStateChanged(skywave_proto::SessionState::AgentWorking),
        EventKind::AgentMessage {
            parent: None,
            text: "done".into(),
        },
    ]);
    let row = rig.open_session().contexts[0].clone();
    assert!(row.can_move, "positive control: {row:?}");
    assert_eq!(row.move_refusal, None);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::MoveContext {
            session_id: rig.session.0.to_string(),
            context_id: start.0.to_string(),
        },
    });
    assert_eq!(
        rig.actions,
        [Action::MoveContext {
            session_id: rig.session,
            context: start
        }]
    );
    rig.actions.clear();
    rig.results();
    rig.view();
    let row = rig
        .open_session()
        .contexts
        .iter()
        .find(|c| c.context_id == start.0.to_string())
        .unwrap()
        .clone();
    let command = row.discard_move.clone().expect("the queued move");
    assert!(!row.can_move, "{row:?}");

    rig.intent(GuiIntent::Act {
        action: ActionIntent::Discard {
            command_id: command.clone(),
        },
    });
    assert_eq!(
        rig.actions,
        [Action::Discard(
            crate::intents::id("command", &command).unwrap()
        )]
    );
}

#[test]
fn a_moved_context_row_names_where_it_went() {
    let mut rig = Rig::new();
    let me = rig.seam.identity().unwrap().device_id();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let new = SessionId::new();
    let start = skywave_proto::ContextId::first_of(rig.session);
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![
                Event {
                    session_id: new,
                    at,
                    command_id: None,
                    context: skywave_proto::ContextId::first_of(new),
                    kind: EventKind::SessionCreated {
                        workdir: "/srv/forked".into(),
                        context_name: "start".into(),
                        from: Some(skywave_proto::SessionOrigin::Context(
                            skywave_proto::ContextRef {
                                session: rig.session,
                                context: start,
                            },
                        )),
                    },
                },
                Event {
                    session_id: rig.session,
                    at,
                    command_id: Some(CommandId::new()),
                    context: start,
                    kind: EventKind::ContextMoved { to: new, by: me },
                },
            ],
        })
        .unwrap();
    rig.view();
    let row = rig.open_session().contexts[0].clone();
    assert_eq!(row.moved_to_session_id, Some(new.0.to_string()));
    assert_eq!(row.moved_label.as_deref(), Some("moved → “forked”"));
    assert!(!row.can_move);
    assert_eq!(row.move_refusal, None, "moved says so in its label");
}

#[test]
fn a_malformed_context_id_is_an_error_not_an_action() {
    let mut rig = Rig::new();
    let outs = rig.intent(GuiIntent::Act {
        action: ActionIntent::SwitchContext {
            session_id: rig.session.0.to_string(),
            context_id: "not-an-id".into(),
        },
    });
    assert!(rig.actions.is_empty());
    assert_eq!(errors(&outs), ["not a context id: \"not-an-id\""]);
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
            parent: None,
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

/// The `CreateSession` commands the core has queued, as (workdir,
/// conversation).
fn queued_creations(rig: &Rig) -> Vec<(String, Option<String>)> {
    rig.seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.command.kind {
            CommandKind::CreateSession {
                workdir,
                conversation,
                ..
            } => Some((workdir, conversation)),
            _ => None,
        })
        .collect()
}

fn open_create_prompt(rig: &mut Rig) {
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    rig.intent(GuiIntent::CreateOpen);
}

#[test]
fn accepting_with_a_conversation_id_adopts_it() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/app".into(),
    });
    rig.intent(GuiIntent::CreateConversationTyped {
        conversation: "  abc-123 ".into(),
    });
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(
        rig.actions.last(),
        Some(&Action::CreateSession {
            workdir: "/srv/app".into(),
            conversation: Some("  abc-123 ".into()),
        })
    );
    assert_eq!(
        queued_creations(&rig),
        [("/srv/app".to_owned(), Some("abc-123".to_owned()))]
    );
}

#[test]
fn accepting_with_the_conversation_blank_starts_fresh() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/app".into(),
    });
    rig.intent(GuiIntent::CreateConversationTyped {
        conversation: "   ".into(),
    });
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(queued_creations(&rig), [("/srv/app".to_owned(), None)]);
}

#[test]
fn accepting_a_candidate_leaves_the_conversation_alone() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    rig.intent(GuiIntent::CreateConversationTyped {
        conversation: "abc-123".into(),
    });
    // The seeded session's workdir is a recent candidate.
    rig.intent(GuiIntent::CompletionComplete);
    rig.results();
    rig.view();
    let before = rig.actions.len();
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(
        rig.actions.len(),
        before,
        "taking a candidate dispatches nothing"
    );
    assert!(queued_creations(&rig).is_empty());
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(queued_creations(&rig).len(), 1);
    assert_eq!(queued_creations(&rig)[0].1.as_deref(), Some("abc-123"));
}

#[test]
fn the_conversation_field_does_nothing_while_the_prompt_is_closed() {
    let mut rig = Rig::new();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    let outs = rig.intent(GuiIntent::CreateConversationTyped {
        conversation: "abc-123".into(),
    });
    assert!(outs.is_empty());
    // A prompt opened afterwards starts blank.
    rig.intent(GuiIntent::CreateOpen);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/app".into(),
    });
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(queued_creations(&rig), [("/srv/app".to_owned(), None)]);
}

/// A call `call` of `kind` titled `title`, made by the subagent `parent`
/// starts (`None`: the main agent).
fn a_call(
    parent: Option<skywave_proto::CallId>,
    call: skywave_proto::CallId,
    name: &str,
    kind: skywave_proto::ToolKind,
    title: &str,
) -> EventKind {
    use skywave_proto::{ToolCall, ToolDisplay};
    EventKind::ToolCall(ToolCall {
        parent,
        call: Some(call),
        name: name.into(),
        display: ToolDisplay {
            kind,
            title: title.into(),
            detail: None,
            body: None,
        },
        input: None,
    })
}

fn snapshots(outs: &[Out]) -> Vec<Vec<String>> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Timeline(TimelineUpdate::Snapshot { items, .. }) => {
                Some(items.iter().map(|row| row.key.clone()).collect())
            }
            _ => None,
        })
        .collect()
}

fn upserts(outs: &[Out]) -> usize {
    outs.iter()
        .filter(|out| matches!(out, Out::Timeline(TimelineUpdate::Upserted { .. })))
        .count()
}

impl Rig {
    fn view_subagent(&mut self, call: Option<skywave_proto::CallId>) -> Vec<Out> {
        self.intent(GuiIntent::Act {
            action: ActionIntent::ViewSubagent {
                session_id: self.session.0.to_string(),
                call: call.map(|call| call.0.to_string()),
            },
        });
        self.results();
        self.view()
    }

    /// The facts of the row of the tool named `name`.
    fn tool_row(&self, name: &str) -> crate::signals::ToolFacts {
        self.rows()
            .iter()
            .find_map(|row| row.tool.clone().filter(|tool| tool.name == name))
            .unwrap_or_else(|| panic!("no {name} row in {:?}", self.rows()))
    }
}

#[test]
fn a_subagent_row_opens_its_view_and_switching_views_sends_a_snapshot() {
    use skywave_proto::{CallId, ToolKind};
    let mut rig = Rig::new();
    let (agent, step, read) = (CallId::new(), CallId::new(), CallId::new());
    rig.log(vec![
        a_call(None, agent, "Agent", ToolKind::Subagent, "Explore sync"),
        a_call(Some(agent), step, "Bash", ToolKind::Shell, "ls src"),
        a_call(None, read, "Read", ToolKind::FileRead, "/srv/work/a.rs"),
    ]);
    let launch = rig.tool_row("Agent");
    assert_eq!(launch.opens, Some(agent.0.to_string()));
    assert_eq!(launch.status.as_deref(), Some("running · 1 step · ls src"));
    // The main agent's other call, in the same timeline, opens nothing.
    let other = rig.tool_row("Read");
    assert_eq!((other.opens, other.status), (None, None));
    let main_keys: Vec<String> = rig.rows().iter().map(|r| r.key.clone()).collect();

    // Typed and not saved yet: only the hub's copy of Dart's text has it.
    rig.edit("half a thought");
    assert!(!rig.kinds().contains(&"save draft"), "{:?}", rig.actions);
    let rev = rig.composer().rev;

    let outs = rig.view_subagent(Some(agent));
    let shown = snapshots(&outs);
    assert_eq!(
        shown.len(),
        1,
        "one snapshot of the subagent's rows: {outs:?}"
    );
    assert_eq!(shown[0].len(), 1, "its one step: {shown:?}");
    assert_eq!(rig.tool_row("Bash").opens, None);
    let session = rig.open_session();
    let header = session.subagent.expect("a subagent's view");
    assert_eq!(header.call, agent.0.to_string());
    assert_eq!(header.title, "Explore sync");
    assert_eq!(header.trail, "work");
    assert_eq!(header.status_label, "running · 1 step · ls src");
    assert!(header.live);
    // A foreground subagent has no task to stop.
    assert_eq!(header.task_id, None);
    // The subagent page's new field takes the text under a new rev.
    let composer = rig.composer();
    assert_eq!(composer.text, "half a thought");
    assert!(composer.rev > rev, "{} after {rev}", composer.rev);
    assert_eq!(composer.to_label.as_deref(), Some("to the main agent"));

    // Positive control: staying in the view, a new step is a diff.
    let outs = rig.log(vec![a_call(
        Some(agent),
        CallId::new(),
        "Grep",
        ToolKind::Search,
        "fn sync",
    )]);
    assert!(snapshots(&outs).is_empty(), "{outs:?}");
    assert_eq!(upserts(&outs), 1, "{outs:?}");
    let rev = rig.composer().rev;

    let outs = rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    assert!(snapshots(&outs).is_empty(), "the core has not moved yet");
    rig.results();
    let outs = rig.view();
    assert_eq!(snapshots(&outs), [main_keys]);
    assert_eq!(rig.open_session().subagent, None);
    let composer = rig.composer();
    assert_eq!(composer.text, "half a thought");
    assert!(composer.rev > rev);
    assert_eq!(composer.to_label, None);
}

#[test]
fn a_background_subagent_opens_from_its_task_and_says_where_its_approval_is() {
    use skywave_proto::{ApprovalId, CallId, TaskId, TaskKind, ToolKind, ToolResult};
    let mut rig = Rig::new();
    let (agent, step) = (CallId::new(), CallId::new());
    let shell = skywave_proto::BackgroundTask {
        id: TaskId("t0".into()),
        kind: TaskKind::Shell,
        title: "npm test".into(),
        parent: None,
        call: None,
        ambient: false,
    };
    rig.log(vec![
        a_call(None, agent, "Agent", ToolKind::Subagent, "Explore sync"),
        EventKind::ToolResult(ToolResult {
            call: Some(agent),
            is_error: false,
            status: None,
            preview: "Async agent launched".into(),
            lines: 1,
            output: None,
        }),
        EventKind::BackgroundTasksChanged {
            tasks: vec![
                shell.clone(),
                skywave_proto::BackgroundTask {
                    id: TaskId("t1".into()),
                    kind: TaskKind::Subagent,
                    title: "Explore sync".into(),
                    call: Some(agent),
                    ..shell
                },
            ],
        },
        a_call(
            Some(agent),
            step,
            "Write",
            ToolKind::FileWrite,
            "src/sync.rs",
        ),
        EventKind::ApprovalRequested {
            id: ApprovalId::new(),
            call: Some(step),
            tool: "Write".into(),
            summary: "src/sync.rs".into(),
            input: None,
            suggestions: Vec::new(),
        },
    ]);
    let session = rig.open_session();
    let calls: Vec<_> = session
        .tasks
        .iter()
        .map(|t| (t.task_id.as_str(), t.call.clone()))
        .collect();
    assert_eq!(calls, [("t0", None), ("t1", Some(agent.0.to_string()))]);
    // The launch is done; the subagent still runs.
    let launch = rig.tool_row("Agent");
    assert_eq!(launch.state, crate::signals::ToolState::Running);
    assert_eq!(
        launch.status.as_deref(),
        Some("running · 1 step · src/sync.rs")
    );
    let approval = session.approval.expect("the subagent's approval");
    assert_eq!(
        approval.from.as_deref(),
        Some("from subagent: Explore sync · open it to see the call")
    );

    rig.view_subagent(Some(agent));
    let session = rig.open_session();
    assert_eq!(session.subagent.unwrap().task_id.as_deref(), Some("t1"));
    assert_eq!(session.stop_headings.stop_task, "Stop this subagent?");
    assert_eq!(
        session.approval.unwrap().from.as_deref(),
        Some("from subagent: Explore sync")
    );
}

#[test]
fn nothing_is_marked_read_from_a_subagents_view() {
    use skywave_proto::{CallId, ToolKind};
    let mut rig = Rig::new();
    let agent = CallId::new();
    rig.log(vec![a_call(
        None,
        agent,
        "Agent",
        ToolKind::Subagent,
        "Explore sync",
    )]);
    rig.view_subagent(Some(agent));
    rig.actions.clear();
    rig.intent(GuiIntent::FollowingChanged { following: true });
    rig.log(vec![a_call(
        Some(agent),
        CallId::new(),
        "Bash",
        ToolKind::Shell,
        "ls",
    )]);
    // Past the throttle, so an earlier mark cannot be what holds it back.
    rig.wait(skywave_present::policy::READ_MARK_EVERY * 2);
    assert!(!rig.kinds().contains(&"mark read"), "{:?}", rig.actions);
    // Positive control: the main view, following, marks.
    rig.view_subagent(None);
    rig.intent(GuiIntent::FollowingChanged { following: true });
    assert!(rig.kinds().contains(&"mark read"), "{:?}", rig.actions);
}

impl Rig {
    /// The server says whether it runs shell commands, as its `hello` does.
    fn shell_enabled(&mut self, shell: bool) {
        self.seam
            .hello_received(&skywave_proto::HelloResponse {
                protocol_version: skywave_proto::PROTOCOL_VERSION,
                server_id: skywave_proto::ServerId::new(),
                device_id: self.me,
                head: Seq::ZERO,
                devices: Vec::new(),
                shell,
            })
            .unwrap();
        self.view();
    }

    /// Switches the open session's composer into shell mode, as the core
    /// takes it.
    fn switch_to_shell(&mut self) {
        self.shell_enabled(true);
        self.intent(GuiIntent::ToggleShellMode {
            session_id: self.session.0.to_string(),
        });
        self.results();
        self.view();
        assert_eq!(self.composer().mode, ComposerModeView::Shell);
        self.actions.clear();
    }

    /// Logs a shell event of the run `run` in the open session at `at`,
    /// and shows the head the view that follows; what it sent Dart then.
    fn log_shell(
        &mut self,
        run: CommandId,
        at: chrono::DateTime<Utc>,
        kind: EventKind,
    ) -> Vec<Out> {
        let from = Seq(self.seam.cursor().unwrap().0 + 1);
        let event = Event {
            session_id: self.session,
            at,
            command_id: kind.logged_by_command().then_some(run),
            context: skywave_proto::ContextId::first_of(self.session),
            kind,
        };
        self.seam
            .apply_events(&EventBatch {
                from,
                events: vec![event],
            })
            .unwrap();
        self.view()
    }

    /// The shell runs the head last sent Dart.
    fn runs(&self) -> &[crate::signals::ShellRunRow] {
        &self.head.sent.shell.as_ref().unwrap().1
    }

    fn last_action(&self) -> &Action {
        self.actions.last().unwrap()
    }
}

fn shell_updates(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Shell(ShellUpdate::Snapshot { runs, .. }) => Some(format!(
                "snapshot {:?}",
                runs.iter().map(|r| r.command.as_str()).collect::<Vec<_>>()
            )),
            Out::Shell(ShellUpdate::Upserted { run, index, .. }) => Some(format!(
                "upsert {}@{index} {}",
                run.command, run.state_label
            )),
            Out::Shell(ShellUpdate::Removed { .. }) => Some("removed".to_owned()),
            _ => None,
        })
        .collect()
}

#[test]
fn shell_mode_is_refused_while_the_server_runs_no_shell_commands() {
    let mut rig = Rig::new();
    assert!(!rig.composer().shell_enabled, "off until a server said so");
    let outs = rig.intent(GuiIntent::ToggleShellMode {
        session_id: rig.session.0.to_string(),
    });
    assert!(rig.actions.is_empty(), "{:?}", rig.actions);
    assert!(outs.iter().any(|o| matches!(
        o,
        Out::Effect(PlatformEffect::Notice { text }) if text == "shell commands are off on this server"
    )));
    // Positive control: on once the server says so.
    rig.shell_enabled(true);
    assert!(rig.composer().shell_enabled);
    rig.intent(GuiIntent::ToggleShellMode {
        session_id: rig.session.0.to_string(),
    });
    assert_eq!(rig.kinds(), ["set mode"]);
}

#[test]
fn toggling_saves_the_agent_draft_first_and_loads_the_shell_draft_once_switched() {
    let mut rig = Rig::new();
    rig.shell_enabled(true);
    rig.core
        .dispatch(Action::SaveDraft {
            session_id: rig.session,
            mode: ComposerMode::Shell,
            text: "cargo test".into(),
            cursor: 5,
            when: RunWhen::OnSuccess,
        })
        .unwrap();
    rig.view();
    rig.edit("half a message");
    assert!(rig.actions.is_empty(), "still within the debounce");
    let rev = rig.composer().rev;
    rig.intent(GuiIntent::ToggleShellMode {
        session_id: rig.session.0.to_string(),
    });
    assert_eq!(rig.kinds(), ["save draft", "set mode"]);
    assert!(matches!(
        &rig.actions[0],
        Action::SaveDraft { mode: ComposerMode::Agent, text, .. } if text == "half a message"
    ));
    assert!(matches!(
        &rig.actions[1],
        Action::SetComposerMode {
            mode: ComposerMode::Shell,
            ..
        }
    ));
    // Not swapped on the intent: the core has not switched yet.
    assert_eq!(rig.composer().mode, ComposerModeView::Agent);
    assert_eq!(rig.text(), "half a message");
    // Typed while the switch is on its way: still the agent's.
    rig.edit("half a message, more");

    rig.results();
    rig.actions.clear();
    rig.view();
    assert!(
        matches!(
            rig.actions.as_slice(),
            [Action::SaveDraft { mode: ComposerMode::Agent, text, .. }] if text == "half a message, more"
        ),
        "{:?}",
        rig.actions
    );
    let composer = rig.composer();
    assert_eq!(composer.mode, ComposerModeView::Shell);
    assert_eq!(
        (composer.text.as_str(), composer.cursor_utf16),
        ("cargo test", 5)
    );
    assert!(composer.rev > rev, "Dart's field is not reset");
    assert_eq!(
        composer.prompt,
        Some(ShellPromptView {
            cwd: None,
            when_label: Some("if ok ✓".into()),
            sigil: "$".into(),
        })
    );
    let Screen::Session(view) = rig.core.view().screen else {
        panic!("left the session");
    };
    assert_eq!(view.draft.unwrap().text, "half a message, more");

    // Back out works with the server's shell off, and swaps back.
    rig.shell_enabled(false);
    rig.actions.clear();
    rig.intent(GuiIntent::ToggleShellMode {
        session_id: rig.session.0.to_string(),
    });
    assert!(matches!(
        rig.last_action(),
        Action::SetComposerMode {
            mode: ComposerMode::Agent,
            ..
        }
    ));
    rig.results();
    rig.view();
    let composer = rig.composer();
    assert_eq!(composer.mode, ComposerModeView::Agent);
    assert_eq!(composer.text, "half a message, more");
    assert_eq!(composer.prompt, None);
}

#[test]
fn a_shell_submit_runs_on_the_chip_condition_which_then_resets() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    let id = rig.session.0.to_string();
    rig.edit("make test");
    rig.intent(GuiIntent::CycleCondition {
        session_id: id.clone(),
    });
    assert_eq!(rig.prompt().when_label.as_deref(), Some("if ok ✓"));
    rig.intent(GuiIntent::CycleCondition {
        session_id: id.clone(),
    });
    assert_eq!(rig.prompt().when_label.as_deref(), Some("if failed ✗"));
    // The condition is saved with the draft.
    rig.wait(DRAFT_SAVE_AFTER + Duration::from_millis(10));
    assert!(matches!(
        rig.last_action(),
        Action::SaveDraft { mode: ComposerMode::Shell, when: RunWhen::OnFailure, text, .. }
            if text == "make test"
    ));
    rig.results();
    rig.actions.clear();

    rig.submit();
    assert_eq!(rig.kinds(), ["run shell"]);
    assert!(matches!(
        rig.last_action(),
        Action::RunShell { command, when: RunWhen::OnFailure, .. } if command == "make test"
    ));
    rig.results();
    let composer = rig.composer();
    assert_eq!(composer.text, "");
    assert_eq!(
        composer.prompt.unwrap().when_label,
        None,
        "the chip resets after a send"
    );

    // Nothing to run: no action, and a picked condition stays.
    rig.actions.clear();
    rig.intent(GuiIntent::CycleCondition { session_id: id });
    rig.submit();
    assert!(!rig.kinds().contains(&"run shell"), "{:?}", rig.actions);
    assert_eq!(rig.prompt().when_label.as_deref(), Some("if ok ✓"));
}

#[test]
fn shell_history_recalls_commands_not_prompts() {
    let mut rig = Rig::new();
    rig.edit("a prompt");
    rig.submit();
    rig.results();
    rig.view();
    rig.switch_to_shell();
    rig.edit("ls -la");
    rig.submit();
    rig.results();
    rig.view();
    rig.edit("half");
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().text, "ls -la");
    assert_eq!(rig.composer().history_label.as_deref(), Some("1 of 1"));
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().text, "ls -la", "no prompt among them");
    rig.intent(GuiIntent::HistoryNewer);
    assert_eq!(rig.composer().text, "half");
}

#[test]
fn the_stops_take_back_a_pending_run_and_stop_a_running_one() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    assert!(!rig.composer().can_stop, "nothing to stop");
    let id = rig.session.0.to_string();
    rig.intent(GuiIntent::StopShell {
        session_id: id.clone(),
    });
    assert!(rig.actions.is_empty(), "{:?}", rig.actions);

    // This device's run, not sent yet: the stop takes it back.
    rig.edit("sleep 60");
    rig.submit();
    rig.results();
    rig.view();
    // The sent clear is saved first, so only the stops dispatch below.
    rig.wait(DRAFT_SAVE_AFTER + Duration::from_millis(10));
    rig.results();
    let row = rig.runs()[0].clone();
    assert_eq!(row.state, crate::signals::ShellRunKind::Pending);
    assert!(row.can_stop && row.discardable, "{row:?}");
    assert!(rig.composer().can_stop);
    rig.actions.clear();
    rig.intent(GuiIntent::StopShell {
        session_id: id.clone(),
    });
    assert_eq!(rig.kinds(), ["discard"]);
    rig.actions.clear();
    rig.intent(GuiIntent::StopShellRun {
        session_id: id.clone(),
        run_id: row.key.clone(),
    });
    assert_eq!(rig.kinds(), ["discard"], "the row's own stop too");
    rig.results();
    rig.view();
    assert!(rig.runs().is_empty());

    // Another device's run, running: stopped, then stopping.
    let run = CommandId::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    rig.log_shell(
        run,
        at,
        EventKind::ShellRequested {
            command: "make".into(),
            when: RunWhen::Always,
            by: DeviceId::new(),
        },
    );
    rig.log_shell(
        run,
        at,
        EventKind::ShellStarted {
            run,
            cwd: "/srv/work".into(),
        },
    );
    let row = rig.runs()[0].clone();
    assert_eq!(row.state, crate::signals::ShellRunKind::Running);
    assert!(row.by_label.is_some_and(|by| by.starts_with("by device")));
    let prompt = rig.prompt();
    assert_eq!(
        (prompt.cwd.as_deref(), prompt.sigil.as_str()),
        (Some("work"), "$")
    );
    rig.actions.clear();
    rig.intent(GuiIntent::StopShellRun {
        session_id: id.clone(),
        run_id: row.key.clone(),
    });
    assert!(matches!(
        rig.last_action(),
        Action::StopShell { run: r, .. } if *r == run
    ));
    rig.actions.clear();
    rig.intent(GuiIntent::StopShell {
        session_id: id.clone(),
    });
    assert_eq!(rig.kinds(), ["stop shell"]);
    rig.results();
    rig.view();
    let row = rig.runs()[0].clone();
    assert!(row.stopping && !row.can_stop, "{row:?}");
    assert!(
        row.state_label.ends_with(" · stopping"),
        "{}",
        row.state_label
    );
    assert!(!rig.composer().can_stop, "nothing left to stop");
    rig.actions.clear();
    rig.intent(GuiIntent::StopShellRun {
        session_id: id,
        run_id: row.key,
    });
    assert!(rig.actions.is_empty(), "stopped once: {:?}", rig.actions);
}

#[test]
fn shell_runs_go_out_as_a_snapshot_then_keyed_diffs() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    let run = CommandId::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    rig.log_shell(
        run,
        at,
        EventKind::ShellRequested {
            command: "make".into(),
            when: RunWhen::OnSuccess,
            by: DeviceId::new(),
        },
    );
    let row = rig.runs()[0].clone();
    assert_eq!(row.key, run.0.to_string(), "keyed by the run id");
    assert_eq!(row.when_label.as_deref(), Some("if ok ✓"));
    let outs = rig.log_shell(
        run,
        at + chrono::Duration::seconds(1),
        EventKind::ShellStarted {
            run,
            cwd: "/srv/work".into(),
        },
    );
    let outs = shell_updates(&outs);
    assert_eq!(outs.len(), 1, "{outs:?}");
    assert!(outs[0].starts_with("upsert make@0 running"), "{outs:?}");
    // Nothing changed: nothing goes out.
    let outs = rig.intent(GuiIntent::FollowingChanged { following: false });
    assert!(shell_updates(&outs).is_empty());

    // The app asks again: a snapshot.
    let outs = rig.intent(GuiIntent::AppReady);
    assert_eq!(shell_updates(&outs), [r#"snapshot ["make"]"#]);
    // Another session: a snapshot of its runs.
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    let outs = rig.view();
    assert_eq!(shell_updates(&outs), ["snapshot []"]);
}

#[test]
fn a_long_shell_run_of_this_devices_notifies() {
    let mut rig = Rig::new();
    let run = CommandId::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let me = rig.me;
    rig.log_shell(
        run,
        at,
        EventKind::ShellRequested {
            command: "make test".into(),
            when: RunWhen::Always,
            by: me,
        },
    );
    rig.log_shell(
        run,
        at,
        EventKind::ShellStarted {
            run,
            cwd: "/srv/work".into(),
        },
    );
    let from = Seq(rig.seam.cursor().unwrap().0 + 1);
    rig.seam
        .apply_events(&EventBatch {
            from,
            events: vec![Event {
                session_id: rig.session,
                at: at + chrono::Duration::seconds(11),
                command_id: None,
                context: skywave_proto::ContextId::first_of(rig.session),
                kind: EventKind::ShellFinished {
                    run,
                    by: me,
                    started_at: Some(at),
                    command_line: "make test".into(),
                    end: skywave_proto::ShellEnd::Exited { code: 1 },
                    cwd: None,
                    preview: String::new(),
                    lines: 0,
                    output: None,
                    truncated: false,
                },
            }],
        })
        .unwrap();
    let view = rig.core.view();
    let outs = rig.head.on_view(view.clone(), rig.clock);
    let notified: Vec<_> = outs
        .iter()
        .filter_map(|o| match o {
            Out::Effect(PlatformEffect::Notify {
                title,
                body,
                session_id,
            }) => Some((title.clone(), body.clone(), session_id.clone())),
            _ => None,
        })
        .collect();
    let title = rig.open_session().title;
    assert_eq!(
        notified,
        [(
            title.clone(),
            format!("make test · exit 1 · {title}"),
            Some(rig.session.0.to_string())
        )]
    );
    // Once only.
    let outs = rig.head.on_view(view.clone(), rig.clock);
    assert!(
        !outs
            .iter()
            .any(|o| matches!(o, Out::Effect(PlatformEffect::Notify { .. })))
    );
    // A head whose first snapshot already counts it still says so.
    let mut fresh = Head::new();
    let outs = fresh.on_view(view, rig.clock);
    assert!(
        outs.iter()
            .any(|o| matches!(o, Out::Effect(PlatformEffect::Notify { .. })))
    );
}

#[test]
fn shell_mode_lists_and_saves_shell_snippets_and_a_long_press_runs_one() {
    let mut rig = Rig::new();
    rig.edit("an agent prompt");
    rig.intent(GuiIntent::SaveComposerSnippet {
        session_id: rig.session.0.to_string(),
        name: String::new(),
    });
    rig.switch_to_shell();
    rig.edit("cargo build");
    let outs = rig.intent(GuiIntent::SaveComposerSnippet {
        session_id: rig.session.0.to_string(),
        name: "build".into(),
    });
    assert!(matches!(
        rig.last_action(),
        Action::SaveSnippet { kind: ComposerMode::Shell, text, .. } if text == "cargo build"
    ));
    assert!(outs.iter().any(|o| matches!(
        o,
        Out::Effect(PlatformEffect::Notice { text }) if text == "saved as a command"
    )));
    rig.results();
    rig.view();
    let snippets = rig.open_session().snippets;
    assert_eq!(
        snippets
            .iter()
            .map(|s| s.label.as_str())
            .collect::<Vec<_>>(),
        ["build"],
        "only the shell kind"
    );

    rig.edit("half");
    rig.actions.clear();
    rig.intent(GuiIntent::RunSnippet {
        session_id: rig.session.0.to_string(),
        snippet_id: snippets[0].snippet_id.clone(),
    });
    assert_eq!(rig.kinds(), ["run shell", "save draft", "use snippet"]);
    assert!(matches!(
        &rig.actions[0],
        Action::RunShell { command, when: RunWhen::Always, .. } if command == "cargo build"
    ));
    rig.results();
    rig.view();
    assert_eq!(rig.text(), "half", "the composer keeps its text");
    let Screen::Session(view) = rig.core.view().screen else {
        panic!("left the session");
    };
    assert_eq!(
        view.shell_draft.map(|d| d.text).as_deref(),
        Some("half"),
        "and so does the core's draft"
    );

    // Back in agent mode, the agent's.
    rig.intent(GuiIntent::ToggleShellMode {
        session_id: rig.session.0.to_string(),
    });
    rig.results();
    rig.view();
    let snippets = rig.open_session().snippets;
    assert_eq!(
        snippets
            .iter()
            .map(|s| s.label.as_str())
            .collect::<Vec<_>>(),
        ["an agent prompt"]
    );
}

impl Rig {
    /// Logs run `run` of `command` by this device as started at `at`.
    fn started_run(&mut self, run: CommandId, command: &str, at: chrono::DateTime<Utc>) {
        let me = self.me;
        self.log_shell(
            run,
            at,
            EventKind::ShellRequested {
                command: command.into(),
                when: RunWhen::Always,
                by: me,
            },
        );
        self.log_shell(
            run,
            at,
            EventKind::ShellStarted {
                run,
                cwd: "/srv/work".into(),
            },
        );
    }

    /// A finished run of `command` by this device, with `preview` of its
    /// `lines` lines and, when the preview does not hold them, `output`.
    fn finished_run(
        &mut self,
        command: &str,
        preview: &str,
        lines: u32,
        output: Option<skywave_proto::BlobRef>,
    ) -> CommandId {
        let run = CommandId::new();
        let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
        self.started_run(run, command, at);
        let me = self.me;
        self.log_shell(
            run,
            at + chrono::Duration::seconds(2),
            EventKind::ShellFinished {
                run,
                by: me,
                started_at: Some(at),
                command_line: command.into(),
                end: skywave_proto::ShellEnd::Exited { code: 0 },
                cwd: None,
                preview: preview.into(),
                lines,
                truncated: output.is_some(),
                output,
            },
        );
        run
    }

    fn expand_run(&mut self, run: CommandId) -> Vec<Out> {
        self.intent(GuiIntent::ExpandShellRun {
            session_id: self.session.0.to_string(),
            run_id: run.0.to_string(),
        })
    }

    fn copy_run(&mut self, run: CommandId) -> Vec<Out> {
        self.intent(GuiIntent::CopyShellOutput {
            session_id: self.session.0.to_string(),
            run_id: run.0.to_string(),
        })
    }
}

fn copied(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Effect(PlatformEffect::CopyToClipboard { text }) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_expanded_run_carries_its_whole_downloaded_output_stripped() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    let blob = skywave_proto::BlobId::new();
    let output = skywave_proto::BlobRef { id: blob, size: 40 };
    let run = rig.finished_run("make test", "line 3\nline 4", 4, Some(output));
    let row = rig.runs()[0].clone();
    assert!(!row.expanded && row.whole.is_none(), "{row:?}");

    // Expanding downloads it; until it is here there is nothing whole.
    rig.expand_run(run);
    assert_eq!(rig.actions, [Action::DownloadBlob(blob)]);
    let row = rig.runs()[0].clone();
    assert!(row.expanded, "{row:?}");
    assert_eq!(row.whole, None);
    // Copying before it is here copies nothing, not the preview.
    assert!(copied(&rig.copy_run(run)).is_empty());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    std::fs::write(&path, b"\x1b[1mline 1\x1b[0m\nline\t2\nline 3\nline 4\n").unwrap();
    rig.seam
        .blob_status(blob, skywave_core::BlobStatus::Available { path })
        .unwrap();
    rig.view();
    let row = rig.runs()[0].clone();
    assert_eq!(
        row.whole.as_deref(),
        Some("line 1\nline    2\nline 3\nline 4\n")
    );
    assert_eq!(row.whole_note, None, "all of it shows");
    assert_eq!(
        copied(&rig.copy_run(run)),
        ["line 1\nline\t2\nline 3\nline 4\n"]
    );

    // Collapsed, the row sends only its tail again.
    rig.intent(GuiIntent::CollapseShellRun {
        session_id: rig.session.0.to_string(),
        run_id: run.0.to_string(),
    });
    let row = rig.runs()[0].clone();
    assert!(!row.expanded && row.whole.is_none(), "{row:?}");
    assert_eq!(row.tail, "line 3\nline 4");
}

#[test]
fn an_expanded_run_without_a_blob_shows_its_own_text_and_what_is_missing() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    let whole = rig.finished_run("ls", "a\nb", 2, None);
    rig.expand_run(whole);
    assert!(rig.actions.is_empty(), "nothing to download");
    let row = rig.runs()[0].clone();
    assert_eq!((row.whole.as_deref(), row.whole_note), (Some("a\nb"), None));
    assert_eq!(copied(&rig.copy_run(whole)), ["a\nb"]);

    // A run still going has only its newest lines so far.
    let run = CommandId::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 5, 0).unwrap();
    rig.started_run(run, "make", at);
    rig.log_shell(
        run,
        at,
        EventKind::ShellOutput {
            run,
            tail: "y\nz".into(),
            lines: 30,
            bytes: 300,
        },
    );
    rig.expand_run(run);
    let row = rig.runs()[1].clone();
    assert_eq!(row.whole.as_deref(), Some("y\nz"));
    assert_eq!(
        row.whole_note.as_deref(),
        Some(
            "… 28 earlier lines not shown · the newest lines so far; the whole output once it finishes"
        )
    );
}

mod workdir {
    use skywave_core::{
        BlobId, BlobState, BlobView, ChangeSet, ChangedFile, ChangesResult, DiffBody, DirEntry,
        EntryKind, FileBody, FileChange, FileResult, LinkTarget, ListResult, Screen, WorkdirBody,
        WorkdirFetch, WorkdirPane, WorkdirView,
    };
    use skywave_present::workdir::NOT_FETCHED;
    use skywave_proto::BlobRef;

    use super::*;
    use crate::signals::{
        BlobStateKind, WorkdirChangeKind, WorkdirLine, WorkdirLineKind, WorkdirPaneKind,
        WorkdirScreen,
    };

    fn taken() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap()
    }

    /// The workdir screen the head sends for `body` on `pane`, the clock
    /// an hour after [`taken`].
    fn shown(
        rig: &mut Rig,
        pane: WorkdirPane,
        fetch: WorkdirFetch,
        body: WorkdirBody,
    ) -> WorkdirScreen {
        rig.clock.wall_ms = (taken() + chrono::Duration::hours(1)).timestamp_millis();
        let mut view = rig.core.view();
        view.screen = Screen::Workdir(WorkdirView {
            session: rig.session,
            title: "fix the build".into(),
            workdir: Some("/srv/work".into()),
            pane,
            last_dir: "src".into(),
            fetch,
            body,
        });
        let outs = rig.head.on_view(view, rig.clock);
        rig.apply(outs);
        match &rig.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Workdir { workdir } => workdir.clone(),
            other => panic!("not the workdir screen: {other:?}"),
        }
    }

    fn file(path: &str, change: FileChange, diff: DiffBody) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            from: None,
            change,
            added: Some(1),
            removed: Some(1),
            diff,
        }
    }

    /// A downloaded blob holding `text`, in the rig's directory.
    fn available(rig: &Rig, text: &str) -> BlobView {
        let path = rig._dir.path().join(format!("blob-{}", BlobId::new().0));
        std::fs::write(&path, text).unwrap();
        BlobView {
            id: BlobId::new(),
            state: BlobState::Available { path },
        }
    }

    fn line(kind: WorkdirLineKind, text: &str) -> WorkdirLine {
        WorkdirLine {
            kind,
            text: text.into(),
        }
    }

    #[test]
    fn the_workdir_changes_pane_lists_files_and_carries_no_content() {
        let mut rig = Rig::new();
        let set = ChangeSet {
            at: taken(),
            branch: Some("main".into()),
            head: Some("1a2b3c4".into()),
            files: vec![
                file("a.txt", FileChange::Modified, DiffBody::Omitted),
                file("new.txt", FileChange::Untracked, DiffBody::Binary),
            ],
            total: 3,
        };
        let screen = shown(
            &mut rig,
            WorkdirPane::Changes,
            WorkdirFetch::Idle,
            WorkdirBody::Changes(ChangesResult::Changes(set)),
        );
        assert_eq!(screen.pane, WorkdirPaneKind::Changes);
        assert_eq!(screen.title, "fix the build");
        assert_eq!(screen.last_dir, "src");
        assert_eq!(screen.header, "main @ 1a2b3c4 · taken 12:00:00");
        assert_eq!(screen.wording, None);
        assert_eq!(screen.fetch_label, None);
        assert_eq!(screen.notes, ["Showing 2 of 3 changed files."]);
        let rows: Vec<_> = screen
            .changes
            .iter()
            .map(|r| {
                (
                    r.change,
                    r.mark.as_str(),
                    r.path.as_str(),
                    r.counts_label.as_deref(),
                    r.untracked,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    WorkdirChangeKind::Modified,
                    "M",
                    "a.txt",
                    Some("+1 \u{2212}1"),
                    false
                ),
                (
                    WorkdirChangeKind::Untracked,
                    "?",
                    "new.txt",
                    Some("binary"),
                    true
                ),
            ]
        );
        assert!(screen.entries.is_empty());
        assert_eq!(screen.content, None);
    }

    #[test]
    fn the_workdir_screen_words_not_git_never_read_and_a_failed_read() {
        let mut rig = Rig::new();
        let screen = shown(
            &mut rig,
            WorkdirPane::Changes,
            WorkdirFetch::Failed("offline".into()),
            WorkdirBody::Changes(ChangesResult::NotGit),
        );
        assert_eq!(
            screen.wording.as_deref(),
            Some("Not a git repository. Browse the files under Files.")
        );
        assert_eq!(screen.header, "Changes");
        assert_eq!(
            screen.fetch_label.as_deref(),
            Some("Refresh failed: offline")
        );
        assert!(screen.fetch_failed);

        let screen = shown(
            &mut rig,
            WorkdirPane::Changes,
            WorkdirFetch::Idle,
            WorkdirBody::Nothing,
        );
        assert_eq!(screen.wording.as_deref(), Some(NOT_FETCHED));
        assert!(!screen.fetch_failed);

        // Loading says so in the fetch label, not as the wording.
        let screen = shown(
            &mut rig,
            WorkdirPane::Changes,
            WorkdirFetch::Loading,
            WorkdirBody::Nothing,
        );
        assert_eq!(screen.wording, None);
        assert_eq!(screen.fetch_label.as_deref(), Some("Loading\u{2026}"));
    }

    #[test]
    fn the_workdir_diff_carries_its_lines_with_kinds_from_the_whole_diff() {
        let mut rig = Rig::new();
        let blob = available(
            &rig,
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n---x\n+++y\n same\n",
        );
        let changed = file(
            "a.txt",
            FileChange::Modified,
            DiffBody::Blob(BlobRef {
                id: blob.id,
                size: 80,
            }),
        );
        let pane = WorkdirPane::Diff {
            path: "a.txt".into(),
            untracked: false,
        };
        let screen = shown(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            WorkdirBody::Diff {
                file: Some(changed.clone()),
                blob: Some(blob.clone()),
            },
        );
        assert_eq!(screen.pane, WorkdirPaneKind::Diff);
        assert_eq!(screen.path, "a.txt");
        assert_eq!(screen.header, "M a.txt  +1 \u{2212}1");
        assert_eq!(screen.wording, None);
        let content = screen.content.unwrap();
        assert_eq!(content.blob.state, BlobStateKind::Available);
        assert_eq!(content.blob.blob_id, blob.id.0.to_string());
        assert_eq!(content.cut, None);
        assert_eq!(
            content.lines,
            [
                line(WorkdirLineKind::FileHeader, "diff --git a/a.txt b/a.txt"),
                line(WorkdirLineKind::FileHeader, "--- a/a.txt"),
                line(WorkdirLineKind::FileHeader, "+++ b/a.txt"),
                line(WorkdirLineKind::Hunk, "@@ -1,2 +1,2 @@"),
                // Inside a hunk these are content, not headers.
                line(WorkdirLineKind::Removed, "---x"),
                line(WorkdirLineKind::Added, "+++y"),
                line(WorkdirLineKind::Context, " same"),
            ]
        );

        // Before it is here: its download, no lines.
        let waiting = BlobView {
            id: blob.id,
            state: BlobState::Failed {
                size: 80,
                reason: "offline".into(),
            },
        };
        let screen = shown(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            WorkdirBody::Diff {
                file: Some(changed),
                blob: Some(waiting),
            },
        );
        let content = screen.content.unwrap();
        assert!(content.blob.can_download);
        assert!(content.lines.is_empty());

        // A file gone from the changes says so, with no content.
        let screen = shown(
            &mut rig,
            pane,
            WorkdirFetch::Idle,
            WorkdirBody::Diff {
                file: None,
                blob: None,
            },
        );
        assert!(screen.wording.unwrap().contains("no longer in the changes"));
        assert_eq!(screen.content, None);
    }

    #[test]
    fn the_workdir_dir_lists_entries_with_their_paths_and_no_content() {
        let mut rig = Rig::new();
        let entry = |name: &str, kind, size| DirEntry {
            name: name.into(),
            kind,
            size,
        };
        let screen = shown(
            &mut rig,
            WorkdirPane::Dir { path: "src".into() },
            WorkdirFetch::Idle,
            WorkdirBody::Dir(ListResult::Listed {
                at: taken(),
                path: "src".into(),
                entries: vec![
                    entry("b.rs", EntryKind::File, Some(812)),
                    entry(
                        "l",
                        EntryKind::Link {
                            to: LinkTarget::Dir,
                        },
                        None,
                    ),
                    entry("a", EntryKind::Dir, None),
                ],
                truncated: false,
            }),
        );
        assert_eq!(screen.pane, WorkdirPaneKind::Dir);
        assert_eq!(screen.header, "./src · taken 12:00:00");
        assert!(screen.can_go_up);
        let rows: Vec<_> = screen
            .entries
            .iter()
            .map(|e| {
                (
                    e.name.as_str(),
                    e.detail.as_deref(),
                    e.opens,
                    e.path.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("a/", None, WorkdirPaneKind::Dir, "src/a"),
                (
                    "l",
                    Some("\u{2192} directory"),
                    WorkdirPaneKind::Dir,
                    "src/l"
                ),
                ("b.rs", Some("812 B"), WorkdirPaneKind::File, "src/b.rs"),
            ]
        );
        assert!(screen.changes.is_empty());
        assert_eq!(screen.content, None);

        // The workdir itself has no way up.
        let screen = shown(
            &mut rig,
            WorkdirPane::Dir {
                path: String::new(),
            },
            WorkdirFetch::Idle,
            WorkdirBody::Dir(ListResult::NotFound),
        );
        assert!(!screen.can_go_up);
        assert!(screen.wording.unwrap().starts_with("Not found"));
    }

    #[test]
    fn the_workdir_file_shows_plain_lines_and_opens_whole() {
        let mut rig = Rig::new();
        let blob = available(&rig, "fn main() {}\n-not a diff\n");
        let pane = WorkdirPane::File {
            path: "src/main.rs".into(),
        };
        let screen = shown(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            WorkdirBody::File {
                result: FileResult::File {
                    at: taken(),
                    path: "src/main.rs".into(),
                    size: 25,
                    content: FileBody::Blob(BlobRef {
                        id: blob.id,
                        size: 25,
                    }),
                },
                blob: Some(blob.clone()),
            },
        );
        assert_eq!(screen.header, "src/main.rs  25 B · taken 12:00:00");
        assert_eq!(
            screen.content.unwrap().lines,
            [
                line(WorkdirLineKind::Plain, "fn main() {}"),
                line(WorkdirLineKind::Plain, "-not a diff"),
            ]
        );

        let outs = rig.intent(GuiIntent::OpenFull {
            blob_id: blob.id.0.to_string(),
        });
        let BlobState::Available { path } = &blob.state else {
            unreachable!()
        };
        assert!(
            outs.iter().any(|out| matches!(
                out,
                Out::Effect(PlatformEffect::OpenFile { path: p }) if *p == path.display().to_string()
            )),
            "{outs:?}"
        );

        let screen = shown(
            &mut rig,
            pane,
            WorkdirFetch::Idle,
            WorkdirBody::File {
                result: FileResult::NotAFile,
                blob: None,
            },
        );
        assert_eq!(
            screen.wording.as_deref(),
            Some("A FIFO, socket or device: not opened.")
        );
        assert_eq!(screen.content, None);
    }

    #[test]
    fn the_workdir_intents_dispatch_their_actions() {
        let mut rig = Rig::new();
        rig.intent(GuiIntent::Act {
            action: ActionIntent::OpenWorkdir {
                session_id: rig.session.0.to_string(),
            },
        });
        rig.intent(GuiIntent::Act {
            action: ActionIntent::ShowWorkdir {
                pane: WorkdirPaneKind::Diff,
                path: "a.txt".into(),
                untracked: true,
            },
        });
        rig.intent(GuiIntent::Act {
            action: ActionIntent::ShowWorkdir {
                pane: WorkdirPaneKind::Dir,
                path: "src".into(),
                untracked: false,
            },
        });
        rig.intent(GuiIntent::Act {
            action: ActionIntent::RefreshWorkdir,
        });
        let workdir: Vec<_> = rig
            .actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::OpenWorkdir { .. } | Action::ShowWorkdir(_) | Action::RefreshWorkdir
                )
            })
            .cloned()
            .collect();
        assert_eq!(
            workdir,
            [
                Action::OpenWorkdir {
                    session_id: rig.session
                },
                Action::ShowWorkdir(WorkdirPane::Diff {
                    path: "a.txt".into(),
                    untracked: true
                }),
                Action::ShowWorkdir(WorkdirPane::Dir { path: "src".into() }),
                Action::RefreshWorkdir,
            ]
        );
    }
}
