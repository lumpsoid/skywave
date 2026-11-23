#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::{TimeZone, Utc};
use skywave_core::attachment;
use skywave_core::{
    CaptureFormat, ContentHash, Core, CoreError, DeviceId, PairedCredentials, ScreenAction,
    ScreenDevice, ScreenImage, ScreenOutcome, ScreenShot, Seam,
};
use skywave_present::policy::DRAFT_SAVE_AFTER;
use skywave_proto::{
    CommandId, CommandKind, CommandResult, DirsResult, Event, EventBatch, EventKind, RejectReason,
};

use super::*;
use crate::lifecycle::{Holder, Target};
use crate::signals::{
    ActionIntent, ComposerFile, CreatePrompt, ListKindView, LoginFlowState, NotificationsView,
    PastedCode, RouteView, RowKind, ScreenView, ShellPromptView, ToolState, WorktreeSessionView,
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
            worktree: None,
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
        worktree: None,
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
            full: None,
        }),
        imported(ImportedItem::ToolCall(tool_call)),
        imported(ImportedItem::ToolResult(result(Some(call)))),
        imported(ImportedItem::ToolResult(result(None))),
        imported(ImportedItem::AgentMessage {
            text: "fixed".into(),
            full: None,
        }),
        EventKind::ImportFinished {
            outcome: ImportOutcome::Imported { items: 5 },
        },
        EventKind::AgentMessage {
            parent: None,
            text: "live again".into(),
            full: None,
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
            .set_link(skywave_core::PairedLink::Online { latency_ms: 5 });
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
            worktree: None,
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
    rig.seam.set_link(skywave_core::PairedLink::Reconnecting {
        attempt: 1,
        retry_at: Utc::now() + chrono::TimeDelta::seconds(2),
        offline_since: Utc::now(),
        reason: skywave_core::LinkFailure::Unreachable,
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

/// A server with another CA gets the re-pair notice, saying why and
/// offering a retry; the banner says it is a re-pair. Dismissed, the list
/// shows until the link is fine again; a damaged identity has no retry.
#[test]
fn an_identity_fault_sends_the_re_pair_notice() {
    use skywave_core::{LinkRejection, PairedLink};
    let mut rig = Rig::new();
    rig.online_on_the_list();
    assert_eq!(rig.head.sent.screen.as_ref().unwrap().repair, None);

    rig.seam
        .set_link(PairedLink::Rejected(LinkRejection::ServerChanged));
    rig.view();
    let repair = rig
        .head
        .sent
        .screen
        .as_ref()
        .unwrap()
        .repair
        .clone()
        .unwrap();
    assert_eq!(repair.title, "The server's identity changed.");
    assert!(repair.can_retry);
    let banner = rig.head.sent.link.as_ref().unwrap();
    assert_eq!(banner.state, crate::signals::LinkState::RepairNeeded);
    assert!(banner.rejected);

    rig.intent(GuiIntent::RepairDismiss);
    assert_eq!(rig.head.sent.screen.as_ref().unwrap().repair, None);
    rig.list();

    rig.seam
        .set_link(PairedLink::Rejected(LinkRejection::BadIdentity));
    rig.view();
    assert_eq!(
        rig.head.sent.screen.as_ref().unwrap().repair,
        None,
        "dismissed until the link is fine again"
    );
    rig.seam.set_link(PairedLink::Connecting);
    rig.view();
    rig.seam
        .set_link(PairedLink::Rejected(LinkRejection::BadIdentity));
    rig.view();
    let repair = rig
        .head
        .sent
        .screen
        .as_ref()
        .unwrap()
        .repair
        .clone()
        .unwrap();
    assert_eq!(repair.title, "This device's identity is damaged.");
    assert!(!repair.can_retry);
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

#[test]
fn the_list_groups_worktree_sessions_by_repo() {
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let (in_worktree, plain) = (SessionId::new(), SessionId::new());
    let created =
        |workdir: &str, worktree: Option<skywave_proto::Worktree>| EventKind::SessionCreated {
            workdir: workdir.into(),
            context_name: "start".into(),
            from: None,
            worktree,
        };
    let feature = skywave_proto::Worktree {
        repo: "/srv/app".into(),
        branch: "feat-x".into(),
    };
    for (session, kind) in [
        (
            in_worktree,
            created("/srv/app.worktrees/feat-x", Some(feature)),
        ),
        (plain, created("/srv/app", None)),
    ] {
        rig.seam
            .apply_events(&EventBatch {
                from: Seq(rig.seam.cursor().unwrap().0 + 1),
                events: vec![event_at(session, at, kind)],
            })
            .unwrap();
    }
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    let rows = match rig.list() {
        ScreenView::SessionList { rows, .. } => rows,
        _ => unreachable!(),
    };
    let position = |id: SessionId| {
        rows.iter()
            .position(|r| r.session_id == id.0.to_string())
            .unwrap()
    };
    let grouping = |id: SessionId| {
        let row = &rows[position(id)];
        (
            row.group.as_str(),
            row.group_name.as_str(),
            row.branch.as_deref(),
        )
    };
    assert_eq!(grouping(in_worktree), ("/srv/app", "app", Some("feat-x")));
    assert_eq!(grouping(plain), ("/srv/app", "app", None));
    assert_eq!(grouping(rig.session), ("/srv/work", "work", None));
    assert_eq!(position(in_worktree).abs_diff(position(plain)), 1);
}

#[test]
fn a_session_row_says_whether_it_may_be_deleted_everywhere() {
    let mut rig = Rig::new();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    // The row of the session on `list`, which is the active or archived one.
    let row = |rig: &Rig| match rig.list() {
        ScreenView::SessionList { rows, .. } => rows
            .into_iter()
            .find(|r| r.session_id == rig.session.0.to_string())
            .unwrap(),
        _ => unreachable!(),
    };
    let live = row(&rig);
    assert!(!live.archived && !live.deletable && !live.deletable_everywhere);

    // Archived in the log, with nothing waiting and no worktree: deletable
    // everywhere. (An archive sent from here would stay unsettled with no
    // server to answer it, and an unsettled command blocks deletion.)
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 2, 0).unwrap();
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![event_at(
                rig.session,
                at,
                EventKind::SessionArchived {
                    archived: true,
                    by: rig.me,
                },
            )],
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::ArchivedSessions,
        },
    });
    rig.results();
    rig.view();
    let archived = row(&rig);
    assert!(archived.archived && archived.deletable && archived.deletable_everywhere);
}

impl Rig {
    fn new() -> Rig {
        Rig::on(Head::new(Prefs::default()))
    }

    /// A rig whose head acts as on a phone, whatever this was built for.
    fn mobile() -> Rig {
        Rig::on(Head::with_target(Target::Mobile))
    }

    fn on(mut head: Head) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        head.set_data_dir(dir.path().to_path_buf());
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
            &"srv:7443".parse().unwrap(),
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
            worktree: None,
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
            head,
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

    /// Carries out the preparations in `outs` as `run` does, here and now,
    /// handing each result to the head. What that adds is applied; the rest
    /// of `outs` is returned.
    fn prepare(&mut self, outs: Vec<Out>) -> Vec<Out> {
        let mut rest = Vec::new();
        for out in outs {
            match out {
                Out::Prepare { session_id, source } => {
                    let result = crate::attach::prepare_attachment(self._dir.path(), source);
                    let outs = self.head.on_prepared(session_id, result, self.clock);
                    rest.extend(self.apply(outs));
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
                Action::Attach { .. } => "attach",
                Action::Detach { .. } => "detach",
                Action::AttachFile { .. } => "attach file",
                Action::DetachFile { .. } => "detach file",
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
fn stopping_saves_an_unsaved_draft_at_once() {
    let mut rig = Rig::new();
    rig.edit("typed then closed");
    assert!(rig.actions.is_empty(), "still within the debounce");
    let outs = rig.head.on_stop(rig.clock);
    assert!(
        outs.iter().all(|out| matches!(out, Out::Dispatch(..))),
        "only dispatches once Dart may be gone: {outs:?}"
    );
    rig.apply(outs);
    assert_eq!(rig.kinds(), ["save draft"]);
    rig.results();
    let Screen::Session(view) = rig.core.view().screen else {
        panic!("left the session");
    };
    assert_eq!(
        view.draft.map(|d| d.text).as_deref(),
        Some("typed then closed")
    );
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
fn a_fresh_head_holds_the_link_on_screen_without_the_service() {
    for head in [
        Head::new(Prefs::default()),
        Head::with_target(Target::Mobile),
    ] {
        assert!(head.awake().holds(Holder::Visible));
        assert!(!head.awake().holds(Holder::Service));
    }
    assert_eq!(
        *Head::new(Prefs::default()).awake(),
        Awake::new(Target::BUILT)
    );
}

/// The kinds of what `outs` does to the core, in order: the dispatched
/// actions and the suspends and resumes.
fn core_calls(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Dispatch(_, action) => Some(format!("{action:?}")),
            Out::Suspend => Some("Suspend".into()),
            Out::Resume => Some("Resume".into()),
            _ => None,
        })
        .map(|s| {
            s.split([' ', '(', '{'])
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

#[test]
fn a_phone_saves_then_suspends_when_backgrounded_without_the_service() {
    let mut rig = Rig::mobile();
    rig.edit("unsent");
    let outs = rig.head.on_intent(GuiIntent::AppBackgrounded, rig.clock);
    assert_eq!(core_calls(&outs), ["SaveDraft", "Suspend"]);
    let outs = rig.head.on_intent(GuiIntent::AppForegrounded, rig.clock);
    assert_eq!(core_calls(&outs), ["Resume", "RetryNow"]);
}

#[test]
fn a_phone_with_the_service_running_saves_but_does_not_suspend_when_backgrounded() {
    let mut rig = Rig::mobile();
    let outs = rig
        .head
        .on_intent(GuiIntent::BackgroundService { running: true }, rig.clock);
    assert_eq!(core_calls(&outs), Vec::<String>::new());
    rig.edit("unsent");
    let outs = rig.head.on_intent(GuiIntent::AppBackgrounded, rig.clock);
    assert_eq!(core_calls(&outs), ["SaveDraft"]);
}

#[test]
fn the_service_stopping_while_backgrounded_suspends_and_starting_resumes() {
    let mut rig = Rig::mobile();
    rig.head
        .on_intent(GuiIntent::BackgroundService { running: true }, rig.clock);
    rig.head.on_intent(GuiIntent::AppBackgrounded, rig.clock);
    let outs = rig
        .head
        .on_intent(GuiIntent::BackgroundService { running: false }, rig.clock);
    assert_eq!(core_calls(&outs), ["Suspend"]);
    let outs = rig
        .head
        .on_intent(GuiIntent::BackgroundService { running: true }, rig.clock);
    assert_eq!(core_calls(&outs), ["Resume", "RetryNow"]);
}

/// The service kept the link up off screen, maybe in a long backoff wait;
/// the user looking is when that wait costs, so it is cut short.
#[test]
fn a_phone_foregrounded_while_the_service_holds_retries_without_resuming() {
    let mut rig = Rig::mobile();
    rig.head
        .on_intent(GuiIntent::BackgroundService { running: true }, rig.clock);
    rig.head.on_intent(GuiIntent::AppBackgrounded, rig.clock);
    let outs = rig.head.on_intent(GuiIntent::AppForegrounded, rig.clock);
    assert_eq!(core_calls(&outs), ["RetryNow"]);
}

/// After the Activity is recreated, `onResume` reports the app on screen
/// again after `onShow` already did.
#[test]
fn a_phone_foregrounded_twice_resumes_once() {
    let mut rig = Rig::mobile();
    rig.head.on_intent(GuiIntent::AppBackgrounded, rig.clock);
    let outs = rig.head.on_intent(GuiIntent::AppForegrounded, rig.clock);
    assert_eq!(core_calls(&outs), ["Resume", "RetryNow"]);
    let outs = rig.head.on_intent(GuiIntent::AppForegrounded, rig.clock);
    assert_eq!(core_calls(&outs), Vec::<String>::new());
}

#[test]
fn a_desktop_never_suspends_whatever_the_reports() {
    let mut rig = Rig::on(Head::with_target(Target::Desktop));
    let mut outs = Vec::new();
    for intent in [
        GuiIntent::AppBackgrounded,
        GuiIntent::BackgroundService { running: true },
        GuiIntent::BackgroundService { running: false },
        GuiIntent::AppForegrounded,
        GuiIntent::AppForegrounded,
        GuiIntent::AppBackgrounded,
    ] {
        outs.extend(rig.head.on_intent(intent, rig.clock));
    }
    assert_eq!(core_calls(&outs), Vec::<String>::new());
}

const ON: Prefs = Prefs {
    stay_connected: true,
    plain_list: false,
    notifications: NotificationPrefs::ON,
};

impl Rig {
    /// A rig whose head acts as on an Android phone, whatever this was
    /// built for, with `prefs` read at boot.
    fn android(prefs: Prefs) -> Rig {
        Rig::on(Head::with_platform(Target::Mobile, true, prefs))
    }

    /// Opens the settings screen.
    fn settings(&mut self) {
        self.intent(GuiIntent::Act {
            action: ActionIntent::Navigate {
                route: RouteView::Settings,
            },
        });
        self.results();
        self.view();
    }
}

/// The background service starts and stops `outs` ask Dart for, in order.
fn service_asks(outs: &[Out]) -> Vec<bool> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Effect(PlatformEffect::BackgroundService { run }) => Some(*run),
            _ => None,
        })
        .collect()
}

/// The preferences `outs` writes, in order.
fn saved(outs: &[Out]) -> Vec<Prefs> {
    outs.iter()
        .filter_map(|out| match out {
            Out::SavePrefs(prefs) => Some(*prefs),
            _ => None,
        })
        .collect()
}

/// The Stay connected switch on the settings screen `outs` sends.
fn stay_shown(outs: &[Out]) -> Option<Option<bool>> {
    outs.iter().find_map(|out| match out {
        Out::Screen(ScreenUpdate {
            screen: ScreenView::Settings { stay_connected, .. },
            ..
        }) => Some(*stay_connected),
        _ => None,
    })
}

#[test]
fn switching_stay_connected_on_saves_it_and_starts_the_service() {
    let mut rig = Rig::android(Prefs::default());
    let outs = rig.intent(GuiIntent::SetStayConnected { on: true });
    assert_eq!(saved(&outs), [ON]);
    assert_eq!(service_asks(&outs), [true]);
    assert_eq!(core_calls(&outs), Vec::<String>::new());
    // Asking is not running: only Kotlin's report holds the link.
    assert!(!rig.head.awake().holds(Holder::Service));
}

#[test]
fn switching_stay_connected_off_keeps_the_service_holding_until_it_stopped() {
    let mut rig = Rig::android(ON);
    rig.intent(GuiIntent::BackgroundService { running: true });
    let outs = rig.intent(GuiIntent::AppBackgrounded);
    assert_eq!(core_calls(&outs), Vec::<String>::new());
    let outs = rig.intent(GuiIntent::SetStayConnected { on: false });
    assert_eq!(saved(&outs), [Prefs::default()]);
    assert_eq!(service_asks(&outs), [false]);
    assert_eq!(core_calls(&outs), Vec::<String>::new());
    assert!(rig.head.awake().holds(Holder::Service));
    let outs = rig.intent(GuiIntent::BackgroundService { running: false });
    assert_eq!(core_calls(&outs), ["Suspend"]);
}

#[test]
fn with_stay_connected_on_ready_and_foregrounded_ask_for_the_service_again() {
    let mut rig = Rig::android(ON);
    assert_eq!(service_asks(&rig.intent(GuiIntent::AppReady)), [true]);
    rig.intent(GuiIntent::AppBackgrounded);
    let outs = rig.intent(GuiIntent::AppForegrounded);
    assert_eq!(service_asks(&outs), [true]);
    // Already on screen: asked again all the same, a no-op for a running
    // service.
    assert_eq!(
        service_asks(&rig.intent(GuiIntent::AppForegrounded)),
        [true]
    );

    // Switched off, nothing asks for it any more.
    rig.intent(GuiIntent::SetStayConnected { on: false });
    assert_eq!(service_asks(&rig.intent(GuiIntent::AppReady)), [false; 0]);
    assert_eq!(
        service_asks(&rig.intent(GuiIntent::AppForegrounded)),
        [false; 0]
    );
}

/// A head built after a hub failure starts without the service, which may
/// run on: its `AppReady` asks for it, Kotlin's answer reports it running,
/// and going away keeps the link.
#[test]
fn a_fresh_head_with_stay_connected_on_learns_the_service_runs_before_going_away() {
    let mut rig = Rig::android(ON);
    assert!(!rig.head.awake().holds(Holder::Service));
    assert_eq!(service_asks(&rig.intent(GuiIntent::AppReady)), [true]);
    rig.intent(GuiIntent::BackgroundService { running: true });
    let outs = rig.intent(GuiIntent::AppBackgrounded);
    assert_eq!(core_calls(&outs), Vec::<String>::new());
}

/// The preference is not a holder: until Kotlin reports the service
/// running, going away suspends.
#[test]
fn stay_connected_on_without_a_running_report_suspends_when_backgrounded() {
    let mut rig = Rig::android(ON);
    rig.intent(GuiIntent::AppReady);
    let outs = rig.intent(GuiIntent::AppBackgrounded);
    assert_eq!(core_calls(&outs), ["Suspend"]);
}

#[test]
fn the_settings_screen_shows_the_switch_and_is_sent_again_on_toggle() {
    let mut rig = Rig::android(Prefs::default());
    rig.settings();
    let shown = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Settings { stay_connected, .. } => *stay_connected,
        other => panic!("not settings: {other:?}"),
    };
    assert_eq!(shown(&rig), Some(false));
    // No snapshot in between: the toggle alone sends the screen.
    let outs = rig.intent(GuiIntent::SetStayConnected { on: true });
    assert_eq!(stay_shown(&outs), Some(Some(true)));
    let outs = rig.intent(GuiIntent::SetStayConnected { on: false });
    assert_eq!(stay_shown(&outs), Some(Some(false)));
}

/// Off Android there is no service: no switch, a stray toggle does
/// nothing, and a preferences file saying on asks for nothing.
#[test]
fn off_android_stay_connected_is_not_offered_and_asks_for_nothing() {
    for target in [Target::Mobile, Target::Desktop] {
        let mut rig = Rig::on(Head::with_platform(target, false, ON));
        rig.settings();
        match &rig.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Settings { stay_connected, .. } => assert_eq!(*stay_connected, None),
            other => panic!("not settings: {other:?}"),
        }
        let mut outs = Vec::new();
        for intent in [
            GuiIntent::AppReady,
            GuiIntent::AppBackgrounded,
            GuiIntent::AppForegrounded,
            GuiIntent::SetStayConnected { on: true },
            GuiIntent::SetStayConnected { on: false },
        ] {
            outs.extend(rig.intent(intent));
        }
        assert_eq!(service_asks(&outs), [false; 0], "{target:?}");
        assert!(saved(&outs).is_empty(), "{target:?}");
    }
}

#[test]
fn stay_connected_is_offered_on_the_android_build_alone() {
    let shown = Head::new(ON).stay_connected.shown();
    assert_eq!(shown.is_some(), cfg!(target_os = "android"));
}

#[test]
fn the_notification_switches_are_shown_saved_and_sent_again() {
    let mut rig = Rig::android(Prefs::default());
    rig.settings();
    let shown = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Settings { notifications, .. } => *notifications,
        other => panic!("not settings: {other:?}"),
    };
    assert_eq!(
        shown(&rig),
        NotificationsView {
            all: true,
            approvals: true,
            idle: true,
            shell: true,
        }
    );
    let outs = rig.intent(GuiIntent::SetNotifications {
        all: true,
        approvals: true,
        idle: false,
        shell: true,
    });
    assert_eq!(
        saved(&outs),
        [Prefs {
            stay_connected: false,
            plain_list: false,
            notifications: NotificationPrefs {
                idle: false,
                ..NotificationPrefs::ON
            },
        }]
    );
    let sent = outs.iter().find_map(|out| match out {
        Out::Screen(ScreenUpdate {
            screen: ScreenView::Settings { notifications, .. },
            ..
        }) => Some(*notifications),
        _ => None,
    });
    assert_eq!(
        sent,
        Some(NotificationsView {
            all: true,
            approvals: true,
            idle: false,
            shell: true,
        })
    );
    assert!(!shown(&rig).idle);
}

/// The switches a head booted with are the ones it keeps when Stay
/// connected is toggled.
#[test]
fn stay_connected_keeps_the_notification_switches() {
    let idle_off = NotificationPrefs {
        idle: false,
        ..NotificationPrefs::ON
    };
    let mut rig = Rig::android(Prefs {
        stay_connected: false,
        plain_list: false,
        notifications: idle_off,
    });
    let outs = rig.intent(GuiIntent::SetStayConnected { on: true });
    assert_eq!(
        saved(&outs),
        [Prefs {
            stay_connected: true,
            plain_list: false,
            notifications: idle_off,
        }]
    );
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
fn a_message_over_the_cap_is_refused_here_and_kept() {
    let mut rig = Rig::new();
    let longest = "x".repeat(skywave_proto::limits::MESSAGE_MAX_BYTES);
    rig.edit(&format!("{longest}y"));
    let outs = rig.submit();
    assert!(rig.actions.is_empty(), "{:?}", rig.kinds());
    assert!(outs.iter().any(|o| matches!(
        o,
        Out::Effect(PlatformEffect::Notice { text })
            if text == "message too long: the limit is 256.0 KiB"
    )));
    assert_eq!(rig.text().len(), longest.len() + 1, "the text stays");

    // Positive control: at the cap it goes.
    rig.edit(&longest);
    rig.submit();
    assert_eq!(rig.kinds(), ["send"]);
}

/// A PNG of a 4x4 image whose colour is `shade` in red: each shade is other
/// content, so other bytes and another hash.
fn png(shade: u8) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([shade, 0, 0, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_with_encoder(image::codecs::png::PngEncoder::new(&mut out))
        .unwrap();
    out
}

/// The intent to attach `bytes` to the open session's draft.
fn attach_intent(rig: &mut Rig, bytes: &[u8]) -> Vec<Out> {
    rig.intent(GuiIntent::AttachImageBytes {
        session_id: rig.session.0.to_string(),
        bytes: bytes.to_vec(),
        mime: "image/png".into(),
    })
}

/// How many of `outs` ask for a preparation.
fn prepares(outs: &[Out]) -> usize {
    outs.iter()
        .filter(|out| matches!(out, Out::Prepare { .. }))
        .count()
}

/// Attaches `bytes` as the composer does: the intent, the preparation, then
/// the core's answer and its snapshot. Returns the image's hash.
fn attach(rig: &mut Rig, bytes: &[u8]) -> ContentHash {
    let outs = attach_intent(rig, bytes);
    rig.prepare(outs);
    rig.results();
    rig.view();
    attachment::prepare(bytes).unwrap().sha256
}

#[test]
fn an_image_is_prepared_before_it_is_attached() {
    let mut rig = Rig::new();
    let bytes = png(1);
    let sha = attachment::prepare(&bytes).unwrap().sha256;
    let stored = attachment::attachment_path(rig._dir.path(), sha);
    let outs = attach_intent(&mut rig, &bytes);
    assert_eq!(prepares(&outs), 1);
    assert!(rig.actions.is_empty(), "{:?}", rig.kinds());
    assert_eq!(rig.composer().preparing, 1);
    assert!(!stored.exists(), "stored before it is prepared");

    rig.prepare(outs);
    assert!(stored.is_file(), "the image is stored");
    assert_eq!(rig.kinds().last(), Some(&"attach"));
    rig.results();
    rig.view();
    let composer = rig.composer();
    assert_eq!(composer.images.len(), 1);
    assert_eq!(composer.images[0].sha256, sha.to_string());
    assert_eq!(composer.images[0].path, stored.display().to_string());
    assert_eq!(composer.preparing, 0);
}

#[test]
fn sending_waits_for_an_attachment_being_prepared() {
    let mut rig = Rig::new();
    rig.edit("look at this");
    let attaching = attach_intent(&mut rig, &png(2));
    let outs = rig.submit();
    assert!(rig.actions.is_empty(), "{:?}", rig.kinds());
    assert_eq!(
        notices(&outs),
        ["an attachment is still being prepared"],
        "the message waits"
    );
    assert_eq!(rig.text(), "look at this", "the text stays");

    rig.prepare(attaching);
    rig.submit();
    assert_eq!(rig.kinds().last(), Some(&"send"));
}

/// The intent to attach the file `name`, written with `bytes` into the rig's
/// directory, as the picker or a drop names it.
fn file_intent(rig: &mut Rig, name: &str, bytes: &[u8]) -> Vec<Out> {
    let path = rig._dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    rig.intent(GuiIntent::AttachFile {
        session_id: rig.session.0.to_string(),
        path: path.display().to_string(),
    })
}

#[test]
fn a_non_image_file_is_attached_as_a_file() {
    let mut rig = Rig::new();
    let bytes = b"%PDF-1.4 not a picture";
    let outs = file_intent(&mut rig, "notes.pdf", bytes);
    assert_eq!(rig.composer().preparing, 1);
    let outs = rig.prepare(outs);
    assert!(notices(&outs).is_empty(), "{:?}", notices(&outs));
    assert_eq!(rig.kinds().last(), Some(&"attach file"));
    assert_eq!(rig.composer().preparing, 0);

    rig.results();
    rig.view();
    let composer = rig.composer();
    assert!(composer.images.is_empty(), "{:?}", composer.images);
    assert_eq!(
        composer.files,
        [ComposerFile {
            name: "notes.pdf".into(),
            size_label: format::bytes(u64::try_from(bytes.len()).unwrap()),
            replaces: false,
        }]
    );
    let stored = attachment::attachment_path(rig._dir.path(), ContentHash::of(bytes));
    assert_eq!(std::fs::read(stored).unwrap(), bytes);
}

#[test]
fn pasted_bytes_that_are_not_an_image_are_still_refused() {
    let mut rig = Rig::new();
    let outs = attach_intent(&mut rig, b"%PDF-1.4 not a picture");
    let outs = rig.prepare(outs);
    assert_eq!(notices(&outs), ["not an image"]);
    assert!(!rig.kinds().contains(&"attach"), "{:?}", rig.kinds());
    assert_eq!(rig.composer().preparing, 0);
}

#[test]
fn an_image_file_is_still_attached_as_an_image() {
    let mut rig = Rig::new();
    let bytes = png(5);
    let sha = attachment::prepare(&bytes).unwrap().sha256;
    let outs = file_intent(&mut rig, "photo.png", &bytes);
    rig.prepare(outs);
    assert_eq!(rig.kinds().last(), Some(&"attach"));
    rig.results();
    rig.view();
    let composer = rig.composer();
    assert_eq!(composer.images.len(), 1);
    assert_eq!(composer.images[0].sha256, sha.to_string());
    assert!(composer.files.is_empty(), "{:?}", composer.files);
}

#[test]
fn a_second_file_with_the_same_name_is_refused_with_a_note() {
    let mut rig = Rig::new();
    let outs = file_intent(&mut rig, "notes.pdf", b"%PDF-1.4 first");
    rig.prepare(outs);
    rig.results();
    rig.view();
    assert_eq!(rig.composer().files.len(), 1);

    rig.actions.clear();
    let outs = file_intent(&mut rig, "notes.pdf", b"%PDF-1.4 second");
    let outs = rig.prepare(outs);
    assert_eq!(
        notices(&outs),
        ["a file named notes.pdf is already attached"]
    );
    assert!(!rig.kinds().contains(&"attach file"), "{:?}", rig.kinds());
    rig.results();
    rig.view();
    assert_eq!(rig.composer().files.len(), 1);
}

#[test]
fn detaching_a_file_removes_it() {
    let mut rig = Rig::new();
    let outs = file_intent(&mut rig, "notes.pdf", b"%PDF-1.4 a file");
    rig.prepare(outs);
    rig.results();
    rig.view();
    assert_eq!(rig.composer().files.len(), 1);

    rig.actions.clear();
    rig.intent(GuiIntent::DetachFile {
        session_id: rig.session.0.to_string(),
        name: "notes.pdf".into(),
    });
    assert_eq!(rig.kinds().last(), Some(&"detach file"));
    rig.results();
    rig.view();
    assert!(rig.composer().files.is_empty());
}

#[test]
fn detaching_an_image_removes_it() {
    let mut rig = Rig::new();
    let sha = attach(&mut rig, &png(3));
    assert_eq!(rig.composer().images.len(), 1);

    let outs = rig.intent(GuiIntent::DetachImage {
        session_id: rig.session.0.to_string(),
        sha256: "nope".into(),
    });
    assert_eq!(errors(&outs), ["bad image hash"]);

    rig.actions.clear();
    rig.intent(GuiIntent::DetachImage {
        session_id: rig.session.0.to_string(),
        sha256: sha.to_string(),
    });
    assert_eq!(rig.kinds().last(), Some(&"detach"));
    rig.results();
    rig.view();
    assert!(rig.composer().images.is_empty());
}

#[test]
fn a_ninth_image_is_refused_here() {
    let mut rig = Rig::new();
    for shade in 0..7 {
        attach(&mut rig, &png(shade));
    }
    // Positive control: the eighth is taken, and is being prepared.
    let eighth = attach_intent(&mut rig, &png(7));
    assert_eq!(prepares(&eighth), 1);
    // The one being prepared counts: the ninth is refused here, unprepared.
    let outs = attach_intent(&mut rig, &png(8));
    assert_eq!(prepares(&outs), 0);
    assert_eq!(notices(&outs), ["at most 8 images per message"]);

    rig.prepare(eighth);
    rig.results();
    rig.view();
    assert_eq!(rig.composer().images.len(), 8);
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
fn a_caret_move_keeps_browsing_history() {
    let mut rig = Rig::new();
    for prompt in ["oldest", "123\n123", "newest"] {
        rig.edit(prompt);
        rig.submit();
        rig.results();
        rig.view();
    }
    rig.edit("123\n123\n1");
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().text, "newest");
    assert_eq!(rig.composer().history_label.as_deref(), Some("1 of 3"));
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().text, "123\n123");
    assert_eq!(rig.composer().history_label.as_deref(), Some("2 of 3"));
    // Up inside the recalled multi-line prompt moves the caret, not the text.
    rig.intent(GuiIntent::DraftEdited {
        session_id: rig.session.0.to_string(),
        text: "123\n123".into(),
        cursor_utf16: 3,
    });
    assert_eq!(rig.composer().history_label.as_deref(), Some("2 of 3"));
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().text, "oldest");
    assert_eq!(rig.composer().history_label.as_deref(), Some("3 of 3"));
    for _ in 0..3 {
        rig.intent(GuiIntent::HistoryNewer);
    }
    assert_eq!(rig.composer().text, "123\n123\n1");
    assert_eq!(rig.composer().history_label, None);
}

#[test]
fn a_text_edit_still_ends_browsing() {
    let mut rig = Rig::new();
    rig.edit("first");
    rig.submit();
    rig.results();
    rig.view();
    rig.edit("half");
    rig.intent(GuiIntent::HistoryOlder);
    assert_eq!(rig.composer().history_label.as_deref(), Some("1 of 1"));
    rig.edit("first!");
    assert_eq!(rig.composer().history_label, None);
    rig.intent(GuiIntent::HistoryNewer);
    // Dart's field keeps what Dart typed, so the head's own draft is checked.
    assert_eq!(rig.head.session.as_ref().unwrap().composer.text, "first!");
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
    let mut head = Head::new(Prefs::default());
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
    let mut head = Head::new(Prefs::default());
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
    assert_eq!(shown_kinds(&outs), [NotifyKind::Approval]);
    // A head that starts at that count does not alert.
    let mut fresh = Head::new(Prefs::default());
    let outs = fresh.on_view(rig.head.view.clone().unwrap(), rig.clock);
    assert!(
        !outs
            .iter()
            .any(|o| matches!(o, Out::Effect(PlatformEffect::Notify { .. })))
    );
}

impl Rig {
    /// Logs `kinds` in `session` and shows the head the view that follows.
    fn log_in(&mut self, session: SessionId, kinds: Vec<EventKind>) -> Vec<Out> {
        let from = Seq(self.seam.cursor().unwrap().0 + 1);
        let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
        let events = kinds
            .into_iter()
            .map(|kind| Event {
                session_id: session,
                at,
                command_id: None,
                context: skywave_proto::ContextId::first_of(session),
                kind,
            })
            .collect();
        self.seam
            .apply_events(&EventBatch { from, events })
            .unwrap();
        self.view()
    }
}

/// The notifications `outs` shows, as `(id, session_id)`, and the ids it
/// takes down.
fn notifications(outs: &[Out]) -> (Vec<(i32, Option<String>)>, Vec<i32>) {
    let mut shown = Vec::new();
    let mut dismissed = Vec::new();
    for out in outs {
        match out {
            Out::Effect(PlatformEffect::Notify { id, session_id, .. }) => {
                shown.push((*id, session_id.clone()));
            }
            Out::Effect(PlatformEffect::Dismiss { id }) => dismissed.push(*id),
            _ => {}
        }
    }
    (shown, dismissed)
}

#[test]
fn each_session_has_its_own_notification_taken_down_when_answered_elsewhere() {
    use skywave_proto::{ApprovalId, Decision};
    let mut rig = Rig::new();
    rig.online_on_the_list();
    let (a, b) = (rig.session, SessionId::new());
    rig.log_in(
        b,
        vec![EventKind::SessionCreated {
            workdir: "/srv/other".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        }],
    );
    // Each agent is working when its approval comes in, as a real one does
    // inside a turn: answered elsewhere, it is still working, not idle.
    for session in [a, b] {
        rig.log_in(
            session,
            vec![EventKind::SessionStateChanged(
                skywave_proto::SessionState::AgentWorking,
            )],
        );
    }
    let approval = |id| EventKind::ApprovalRequested {
        id,
        call: None,
        tool: "Bash".into(),
        summary: "ls".into(),
        input: None,
        suggestions: Vec::new(),
        shell: None,
    };
    let (in_a, in_b) = (ApprovalId::new(), ApprovalId::new());
    let (shown_a, none) = notifications(&rig.log_in(a, vec![approval(in_a)]));
    assert!(none.is_empty());
    let (shown_b, none) = notifications(&rig.log_in(b, vec![approval(in_b)]));
    assert!(none.is_empty());
    let [(id_a, Some(session_a))] = shown_a.as_slice() else {
        panic!("one notification for a: {shown_a:?}");
    };
    let [(id_b, Some(session_b))] = shown_b.as_slice() else {
        panic!("one notification for b: {shown_b:?}");
    };
    assert_eq!((session_a, session_b), (&a.0.to_string(), &b.0.to_string()));
    assert_ne!(id_a, id_b, "b's does not replace a's");

    // Another device answers a's: a's notification goes, b's stays.
    let outs = rig.log_in(
        a,
        vec![EventKind::ApprovalResolved {
            id: in_a,
            decision: Decision::Approve,
            by: DeviceId::new(),
            rule: None,
        }],
    );
    assert_eq!(notifications(&outs), (vec![], vec![*id_a]));
    // Once.
    assert_eq!(notifications(&rig.view()), (vec![], vec![]));
    // Cancelled counts as answered too.
    let outs = rig.log_in(
        b,
        vec![EventKind::ApprovalCancelled {
            id: in_b,
            reason: "daemon restarted".into(),
        }],
    );
    assert_eq!(notifications(&outs), (vec![], vec![*id_b]));
}

#[test]
fn the_list_says_which_kind_it_shows() {
    let mut rig = Rig::new();
    let list = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::SessionList {
            kind, title, rows, ..
        } => (*kind, title.clone(), rows.len()),
        other => panic!("not the list: {other:?}"),
    };
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    assert_eq!(list(&rig), (ListKindView::Active, "Sessions".into(), 1));
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::ArchivedSessions,
        },
    });
    rig.results();
    rig.view();
    assert_eq!(list(&rig), (ListKindView::Archived, "Sessions".into(), 0));
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::AllSessions,
        },
    });
    rig.results();
    rig.view();
    assert_eq!(list(&rig), (ListKindView::All, "Sessions".into(), 1));
}

#[test]
fn the_all_list_marks_its_archived_rows() {
    let mut rig = Rig::new();
    let list = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::SessionList { kind, rows, .. } => (
            *kind,
            rows.iter().map(|r| r.archived).collect::<Vec<bool>>(),
        ),
        other => panic!("not the list: {other:?}"),
    };
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 3, 0).unwrap();
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![event_at(
                rig.session,
                at,
                EventKind::SessionArchived {
                    archived: true,
                    by: rig.me,
                },
            )],
        })
        .unwrap();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::AllSessions,
        },
    });
    rig.results();
    rig.view();
    assert_eq!(list(&rig), (ListKindView::All, vec![true]));
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::SessionList,
        },
    });
    rig.results();
    rig.view();
    assert_eq!(list(&rig), (ListKindView::Active, vec![]));
}

#[test]
fn switching_the_plain_list_saves_it_with_the_other_prefs() {
    let mut rig = Rig::android(ON);
    let outs = rig.intent(GuiIntent::SetPlainList { on: true });
    assert_eq!(
        saved(&outs),
        [Prefs {
            stay_connected: true,
            plain_list: true,
            notifications: NotificationPrefs::ON,
        }]
    );
    // Stay connected keeps the layout it was switched beside.
    let outs = rig.intent(GuiIntent::SetStayConnected { on: false });
    assert_eq!(
        saved(&outs),
        [Prefs {
            stay_connected: false,
            plain_list: true,
            notifications: NotificationPrefs::ON,
        }]
    );
}

#[test]
fn the_plain_list_keeps_recency_order_without_groups() {
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
    let created = |workdir: &str| EventKind::SessionCreated {
        workdir: workdir.into(),
        context_name: "start".into(),
        from: None,
        worktree: None,
    };
    // The list sorts by when a session was made, newest first: A, B, C, then
    // the rig's own session in /srv/work.
    for (session, workdir) in [(c, "/srv/app"), (b, "/srv/other"), (a, "/srv/app")] {
        rig.seam
            .apply_events(&EventBatch {
                from: Seq(rig.seam.cursor().unwrap().0 + 1),
                events: vec![event_at(session, at, created(workdir))],
            })
            .unwrap();
    }
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Back,
    });
    rig.results();
    rig.view();
    // Whether the list is plain, and its session ids and group keys in order.
    let shown = |rig: &Rig| match rig.list() {
        ScreenView::SessionList { plain, rows, .. } => (
            plain,
            rows.iter()
                .map(|r| r.session_id.clone())
                .collect::<Vec<_>>(),
            rows.iter().map(|r| r.group.clone()).collect::<Vec<_>>(),
        ),
        _ => unreachable!(),
    };
    let id = |s: SessionId| s.0.to_string();
    // Grouped, the default: C moves up to sit with A, its repo.
    let (plain, ids, _) = shown(&rig);
    assert!(!plain);
    assert_eq!(ids, [id(a), id(c), id(b), id(rig.session)]);

    rig.intent(GuiIntent::SetPlainList { on: true });
    rig.results();
    rig.view();
    let (plain, ids, groups) = shown(&rig);
    assert!(plain);
    assert_eq!(ids, [id(a), id(b), id(c), id(rig.session)]);
    assert!(groups.iter().all(String::is_empty), "{groups:?}");
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
fn the_storage_screen_shows_the_report_and_the_list_storage_show_picks() {
    use crate::signals::{
        StorageGroupingView, StorageRowView, StorageSessionView, StorageTotalView,
    };

    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Storage,
        },
    });
    rig.results();
    rig.view();
    let storage = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Storage {
            grouping,
            status_label,
            totals,
            rows,
            empty_label,
            ..
        } => (
            *grouping,
            status_label.clone(),
            totals.clone(),
            rows.clone(),
            empty_label.clone(),
        ),
        other => panic!("not the storage screen: {other:?}"),
    };
    let (_, status, totals, rows, empty) = storage(&rig);
    assert_eq!(status.as_deref(), Some("Asking the server…"));
    assert!(totals.is_empty() && rows.is_empty() && empty.is_none());

    let req = requests
        .try_iter()
        .find_map(|r| match r {
            skywave_core::Request::Storage { req } => Some(req),
            _ => None,
        })
        .expect("the core asked for the report");
    rig.seam.storage_reported(
        req,
        Ok(skywave_proto::StorageReport {
            database: 1024 * 1024,
            reclaimable: 0,
            event_blobs: 0,
            workdir_blobs: 0,
            shell_running: 0,
            log_file: None,
            free: None,
            min_free: 0,
            sessions: vec![skywave_proto::SessionStorage {
                session: rig.session,
                events: 2,
                event_bytes: 2048,
                blobs: 0,
                blob_bytes: 0,
            }],
            blobs: Vec::new(),
            shell_runs: Vec::new(),
        }),
    );
    rig.view();
    let (grouping, status, totals, rows, empty) = storage(&rig);
    assert_eq!(grouping, StorageGroupingView::Sessions);
    assert_eq!(status, None);
    assert_eq!(
        totals[0],
        StorageTotalView {
            label: "Free".into(),
            value: "unknown".into()
        }
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        StorageRowView {
            label: rows[0].label.clone(),
            detail: "2 events · 2.0 KiB in the log · no blobs".into(),
            size: "2.0 KiB".into(),
            target: None,
            session: Some(StorageSessionView {
                session_id: rig.session.0.to_string(),
                server_size: Some("2.0 KiB".into()),
                device_size: None,
                here: true,
                archived: false,
                deletable: false,
                deletable_everywhere: false,
            }),
        }
    );
    assert_eq!(empty, None);

    rig.intent(GuiIntent::StorageShow {
        grouping: StorageGroupingView::ShellRuns,
    });
    rig.view();
    let (grouping, _, _, rows, empty) = storage(&rig);
    assert_eq!(grouping, StorageGroupingView::ShellRuns);
    assert!(rows.is_empty());
    assert_eq!(empty.as_deref(), Some("No shell runs."));

    rig.intent(GuiIntent::Act {
        action: ActionIntent::RefreshStorage,
    });
    assert_eq!(rig.actions.last(), Some(&Action::RefreshStorage));
}

#[test]
fn the_approvals_screen_lists_every_session_s_waiting_approvals_oldest_first() {
    use crate::signals::DecisionView;
    use skywave_proto::{ApprovalId, CallId, ToolKind};
    let mut rig = Rig::new();
    rig.online_on_the_list();
    let (a, b) = (rig.session, SessionId::new());
    rig.log_in(
        b,
        vec![EventKind::SessionCreated {
            workdir: "/srv/other".into(),
            context_name: "start".into(),
            from: None,
            worktree: None,
        }],
    );
    let mut ids = [ApprovalId::new(), ApprovalId::new()];
    ids.sort();
    let [older, newer] = ids;
    let (agent, step) = (CallId::new(), CallId::new());
    rig.log(vec![
        a_call(None, agent, "Agent", ToolKind::Subagent, "Explore sync"),
        a_call(Some(agent), step, "Bash", ToolKind::Shell, "git status"),
        EventKind::ApprovalRequested {
            id: newer,
            call: Some(step),
            tool: "Bash".into(),
            summary: "git status".into(),
            input: None,
            suggestions: Vec::new(),
            shell: Some("git status".into()),
        },
    ]);
    rig.log_in(
        b,
        vec![EventKind::ApprovalRequested {
            id: older,
            call: None,
            tool: "Write".into(),
            summary: "src/lib.rs".into(),
            input: None,
            suggestions: Vec::new(),
            shell: None,
        }],
    );

    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Approvals,
        },
    });
    rig.results();
    rig.view();
    let ScreenView::Approvals { items } = sent_screen(&rig) else {
        panic!("not the approvals screen");
    };
    let [first, second] = items.as_slice() else {
        panic!("two approvals: {items:?}");
    };
    assert_eq!(first.session_id, b.0.to_string());
    assert_eq!(first.approval_id, older.0.to_string());
    assert_eq!(second.session_id, a.0.to_string());
    assert_eq!(second.approval_id, newer.0.to_string());
    assert!(first.note.is_none() && second.note.is_none());

    let plain = first.panel.as_ref().expect("a panel for b's approval");
    assert!(plain.can_answer);
    assert_eq!(plain.rule_candidates, Vec::<Vec<String>>::new());
    let subagent = second.panel.as_ref().expect("a panel for a's approval");
    assert!(subagent.can_answer);
    assert_eq!(
        subagent.rule_candidates,
        vec![vec!["git".to_owned(), "status".to_owned()]]
    );
    assert!(
        subagent
            .from
            .as_deref()
            .unwrap_or_default()
            .starts_with("from subagent")
    );

    rig.intent(GuiIntent::Act {
        action: ActionIntent::ResolveApproval {
            approval_id: newer.0.to_string(),
            decision: DecisionView::Approve,
            rule: None,
        },
    });
    rig.results();
    rig.view();
    let ScreenView::Approvals { items } = sent_screen(&rig) else {
        panic!("not the approvals screen after the answer");
    };
    let [only] = items.as_slice() else {
        panic!("one approval left: {items:?}");
    };
    assert_eq!(only.approval_id, older.0.to_string());
}

#[test]
fn always_allow_from_the_approvals_screen_answers_its_rule() {
    use crate::signals::DecisionView;
    use skywave_proto::{ApprovalId, RuleScope, RuleSuggestion};
    let mut rig = Rig::new();
    rig.online_on_the_list();
    let approval = ApprovalId::new();
    rig.log(vec![EventKind::ApprovalRequested {
        id: approval,
        call: None,
        tool: "Bash".into(),
        summary: "ls".into(),
        input: None,
        suggestions: vec![RuleSuggestion {
            label: "bash(ls)".into(),
            scope: RuleScope::Project,
        }],
        shell: Some("ls".into()),
    }]);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Approvals,
        },
    });
    rig.results();
    rig.view();
    let ScreenView::Approvals { items } = sent_screen(&rig) else {
        panic!("not the approvals screen");
    };
    let [item] = items.as_slice() else {
        panic!("one approval: {items:?}");
    };
    let panel = item.panel.as_ref().expect("a panel for the approval");
    assert_eq!(panel.suggestions.len(), 1);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::ResolveApproval {
            approval_id: approval.0.to_string(),
            decision: DecisionView::Approve,
            rule: Some(0),
        },
    });
    rig.results();
    rig.view();
    let ScreenView::Approvals { items } = sent_screen(&rig) else {
        panic!("not the approvals screen after the answer");
    };
    assert!(items.is_empty(), "always allow left it: {items:?}");
}

#[test]
fn the_storage_screen_shows_this_devices_usage_and_what_may_be_reclaimed() {
    use crate::signals::StorageKindView;

    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Storage,
        },
    });
    rig.results();
    rig.view();
    let asked: Vec<skywave_core::Request> = requests.try_iter().collect();
    let storage_req = asked
        .iter()
        .find_map(|r| match r {
            skywave_core::Request::Storage { req } => Some(*req),
            _ => None,
        })
        .expect("the core asked for the report");
    let device_req = asked
        .iter()
        .find_map(|r| match r {
            skywave_core::Request::DeviceStorage { req } => Some(*req),
            _ => None,
        })
        .expect("the core asked for this device's storage");
    rig.seam.storage_reported(
        storage_req,
        Ok(skywave_proto::StorageReport {
            database: 8192,
            reclaimable: 4096,
            event_blobs: 0,
            workdir_blobs: 0,
            shell_running: 0,
            log_file: None,
            free: None,
            min_free: 0,
            sessions: vec![skywave_proto::SessionStorage {
                session: rig.session,
                events: 1,
                event_bytes: 2048,
                blobs: 0,
                blob_bytes: 0,
            }],
            blobs: Vec::new(),
            shell_runs: Vec::new(),
        }),
    );
    rig.seam.device_storage_read(
        device_req,
        Ok(skywave_core::DeviceStorage {
            database: 16384,
            reclaimable: 2048,
            blobs: 1024,
            attachments: 0,
            sessions: vec![skywave_core::DeviceSessionStorage {
                session: rig.session,
                event_bytes: 512,
                blob_bytes: 0,
            }],
        }),
    );
    rig.view();
    let screen = |rig: &Rig| rig.head.sent.screen.as_ref().unwrap().screen.clone();
    let ScreenView::Storage {
        server_reclaimable,
        server_reclaiming,
        device_totals,
        device_kinds,
        device_reclaimable,
        device_reclaiming,
        rows,
        ..
    } = screen(&rig)
    else {
        panic!("not the storage screen");
    };
    assert_eq!(server_reclaimable.as_deref(), Some("4.0 KiB"));
    assert_eq!(device_reclaimable.as_deref(), Some("2.0 KiB"));
    assert!(!server_reclaiming && !device_reclaiming);
    assert_eq!(
        device_totals.first().map(|t| t.label.as_str()),
        Some("Used")
    );
    assert_eq!(
        device_kinds,
        vec![
            StorageKindView {
                label: "Database".into(),
                bytes: 16384,
                size: "16.0 KiB".into(),
            },
            StorageKindView {
                label: "Blobs".into(),
                bytes: 1024,
                size: "1.0 KiB".into(),
            },
            StorageKindView {
                label: "Attachments".into(),
                bytes: 0,
                size: "0 B".into(),
            },
        ]
    );
    let session = rows[0].session.clone().expect("a session row");
    assert_eq!(session.device_size.as_deref(), Some("512 B"));

    rig.intent(GuiIntent::Act {
        action: ActionIntent::ReclaimDeviceSpace,
    });
    rig.results();
    rig.view();
    assert_eq!(rig.actions.last(), Some(&Action::ReclaimDeviceSpace));
    let ScreenView::Storage {
        device_reclaimable,
        device_reclaiming,
        ..
    } = screen(&rig)
    else {
        panic!("not the storage screen");
    };
    assert!(device_reclaiming);
    assert_eq!(device_reclaimable, None);
}

#[test]
fn the_storage_intents_reach_the_core() {
    let mut rig = Rig::new();
    for (action, expected) in [
        (ActionIntent::ReclaimServerSpace, Action::ReclaimServerSpace),
        (ActionIntent::ReclaimDeviceSpace, Action::ReclaimDeviceSpace),
        (
            ActionIntent::DeleteSessionEverywhere {
                session_id: rig.session.0.to_string(),
            },
            Action::DeleteSessionEverywhere {
                session_id: rig.session,
            },
        ),
    ] {
        rig.intent(GuiIntent::Act { action });
        assert_eq!(rig.actions.last(), Some(&expected));
    }
}

#[test]
fn the_usage_limits_page_shows_each_limit_with_its_bar() {
    use crate::signals::LimitRowView;

    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::UsageLimits,
        },
    });
    rig.results();
    rig.view();
    let page = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::UsageLimits { limits } => (
            limits.status_label.clone(),
            limits.read_label.clone(),
            limits.rows.clone(),
            limits.empty_label.clone(),
        ),
        other => panic!("not the usage limits page: {other:?}"),
    };
    let (status, read, rows, empty) = page(&rig);
    assert_eq!(status.as_deref(), Some("Asking the agent…"));
    assert_eq!(read, None);
    assert!(rows.is_empty());
    assert_eq!(empty.as_deref(), Some("No usage limits read yet."));

    let req = requests
        .try_iter()
        .find_map(|r| match r {
            skywave_core::Request::UsageLimits { req } => Some(req),
            _ => None,
        })
        .expect("the core asked for the limits");
    rig.seam
        .usage_limits_answered(
            req,
            Ok(skywave_proto::UsageLimitsResult::Read {
                at: Utc::now(),
                limits: vec![skywave_proto::UsageLimit {
                    window: skywave_proto::LimitWindow::Weekly,
                    scope: Some("Fable".into()),
                    percent: Some(22),
                    severity: Some(skywave_proto::LimitSeverity::Normal),
                    resets_at: None,
                }],
            }),
        )
        .unwrap();
    rig.view();
    let (status, read, rows, empty) = page(&rig);
    assert_eq!(status, None);
    assert!(read.is_some_and(|read| read.starts_with("Read ")));
    assert_eq!(
        rows,
        [LimitRowView {
            label: "Weekly · Fable".into(),
            percent_label: Some("22%".into()),
            fill_percent: Some(22),
            warning: false,
            resets_label: None,
        }]
    );
    assert_eq!(empty, None);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::RefreshUsageLimits,
    });
    assert_eq!(rig.actions.last(), Some(&Action::RefreshUsageLimits));
}

#[test]
fn the_login_intents_reach_the_core() {
    let mut rig = Rig::new();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Settings,
        },
    });
    rig.results();
    let act = |rig: &mut Rig, action| {
        rig.intent(GuiIntent::Act { action });
        rig.results();
        rig.view();
        rig.actions.last().cloned()
    };
    assert_eq!(
        act(&mut rig, ActionIntent::StartAgentLogin),
        Some(Action::StartAgentLogin)
    );
    // The core asked the server, and the tile Dart got says so.
    let tile = match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::Settings { login, .. } => login.clone().unwrap(),
        other => panic!("not settings: {other:?}"),
    };
    assert_eq!(tile.state, LoginFlowState::Starting);
    assert_eq!(tile.lines, ["starting the login…"]);
    assert_eq!(
        act(
            &mut rig,
            ActionIntent::FinishAgentLogin {
                code: PastedCode {
                    text: " the-code ".into()
                }
            }
        ),
        Some(Action::FinishAgentLogin {
            code: skywave_core::LoginCode::new(" the-code ")
        })
    );
    assert_eq!(
        act(&mut rig, ActionIntent::RetryAgentLogin),
        Some(Action::RetryAgentLogin)
    );
    assert_eq!(
        act(&mut rig, ActionIntent::DismissAgentLogin),
        Some(Action::DismissAgentLogin)
    );
}

/// The hub logs a dropped intent and every dispatched action with their
/// `Debug`: neither may print a pasted login code.
#[test]
fn a_pasted_login_code_never_shows_in_a_debug() {
    let secret = "sk-secret-code-4711";
    let intent = GuiIntent::Act {
        action: ActionIntent::FinishAgentLogin {
            code: PastedCode {
                text: secret.into(),
            },
        },
    };
    assert!(!format!("{intent:?}").contains(secret), "{intent:?}");
    assert!(format!("{intent:?}").contains("FinishAgentLogin"));
    let GuiIntent::Act { action } = intent else {
        unreachable!()
    };
    let action = crate::intents::action(action).unwrap();
    // The code itself reaches the core whole.
    let Action::FinishAgentLogin { code } = &action else {
        panic!("not a finish: {action:?}")
    };
    assert_eq!(code.as_str(), secret);
    assert!(!format!("{action:?}").contains(secret), "{action:?}");

    let mut rig = Rig::new();
    let outs = rig.intent(GuiIntent::Act {
        action: ActionIntent::FinishAgentLogin {
            code: PastedCode {
                text: secret.into(),
            },
        },
    });
    assert!(rig.actions.contains(&action), "dispatched");
    assert!(!format!("{outs:?}").contains(secret));
}

#[test]
fn the_session_info_page_shows_its_sections_limits_and_subagents() {
    let mut rig = Rig::new();
    rig.seam
        .set_link(skywave_core::PairedLink::Online { latency_ms: 5 });
    rig.intent(GuiIntent::Act {
        action: ActionIntent::OpenSessionInfo {
            session_id: rig.session.0.to_string(),
        },
    });
    rig.results();
    rig.view();
    assert_eq!(
        rig.actions.last(),
        Some(&Action::OpenSessionInfo {
            session_id: rig.session
        })
    );
    match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::SessionInfo {
            session_id,
            sections,
            limits,
            subagents,
            subagents_empty_label,
        } => {
            assert_eq!(*session_id, rig.session.0.to_string());
            let titles: Vec<_> = sections.iter().map(|s| s.title.as_str()).collect();
            assert_eq!(titles, ["Session", "Usage", "Agent"]);
            assert_eq!(sections[0].fields[1].label, "Workdir");
            assert_eq!(limits.status_label.as_deref(), Some("Asking the agent…"));
            assert!(subagents.is_empty());
            assert_eq!(
                subagents_empty_label.as_deref(),
                Some(skywave_present::session_info::NO_SUBAGENTS)
            );
        }
        other => panic!("not the session info page: {other:?}"),
    }
}

#[test]
fn a_tapped_candidate_fills_the_create_prompt() {
    let mut rig = Rig::new();
    let kind = EventKind::SessionCreated {
        workdir: "/srv/other".into(),
        context_name: "start".into(),
        from: None,
        worktree: None,
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
    let listed = shown_prompt(&rig).popup.expect("the candidates are listed");
    let paths: Vec<_> = listed.candidates.iter().map(|c| c.path.clone()).collect();
    assert!(paths.len() >= 2, "both recent workdirs: {paths:?}");

    let outs = rig.intent(GuiIntent::CompletionPick { index: 99 });
    assert!(outs.is_empty(), "a stale tap does nothing");
    let before = shown_prompt(&rig).input_rev;
    rig.intent(GuiIntent::CompletionPick { index: 1 });
    rig.results();
    rig.view();
    let after = shown_prompt(&rig);
    assert_eq!(after.input, format!("{}/", paths[1]));
    assert!(after.input_rev > before);
    let popup = after.popup.expect("taking one keeps the list");
    assert_eq!(popup.selected, None, "with nothing highlighted");
}

/// The new-session prompt as the head last sent it.
fn shown_prompt(rig: &Rig) -> CreatePrompt {
    match &rig.head.sent.screen.as_ref().unwrap().screen {
        ScreenView::SessionList { create, .. } => create.clone().unwrap(),
        other => panic!("not the list: {other:?}"),
    }
}

/// Answers the newest directory listing the core asked for with `names`
/// under `dir`.
fn answer_listing(
    rig: &mut Rig,
    requests: &std::sync::mpsc::Receiver<skywave_core::Request>,
    dir: &str,
    names: &[&str],
) {
    let req = requests
        .try_iter()
        .filter_map(|r| match r {
            skywave_core::Request::ListDirs { req, .. } => Some(req),
            _ => None,
        })
        .last()
        .expect("the core asked for a listing");
    rig.seam.dirs_listed(
        req,
        Ok(DirsResult::Listed {
            dir: dir.into(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            truncated: false,
        }),
    );
    rig.view();
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
    let me = rig.seam.pairing().unwrap().credentials.device_id;
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
    let me = rig.seam.pairing().unwrap().credentials.device_id;
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
                worktree: None,
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

#[test]
fn a_removed_worktree_is_an_archive_style_row() {
    let mut rig = Rig::new();
    let me = rig.seam.pairing().unwrap().credentials.device_id;
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let events = vec![Event {
        session_id: rig.session,
        at,
        command_id: Some(CommandId::new()),
        context: skywave_proto::ContextId::first_of(rig.session),
        kind: EventKind::WorktreeRemoved { by: me },
    }];
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
        .map(|row| (row.kind, row.text.clone()))
        .collect();
    assert_eq!(
        rows,
        [(RowKind::Archived, "worktree removed by you".to_owned())]
    );
}

#[test]
fn a_written_file_is_a_notice() {
    let mut rig = Rig::new();
    let me = rig.seam.pairing().unwrap().credentials.device_id;
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let events = vec![Event {
        session_id: rig.session,
        at,
        command_id: Some(CommandId::new()),
        context: skywave_proto::ContextId::first_of(rig.session),
        kind: EventKind::FileWritten {
            path: "docs/a.txt".into(),
            sha256: ContentHash::of(b"a"),
            size: 1,
            replaced: None,
            by: me,
        },
    }];
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
        .map(|row| (row.kind, row.text.clone()))
        .collect();
    assert_eq!(
        rows,
        [(RowKind::Archived, "docs/a.txt written by you".to_owned())]
    );
}

/// A session created in `/srv/app`, or in its git worktree `branch`.
fn created_session(branch: Option<&str>) -> EventKind {
    EventKind::SessionCreated {
        workdir: match branch {
            Some(branch) => format!("/srv/app.worktrees/{branch}"),
            None => "/srv/app".into(),
        },
        context_name: "start".into(),
        from: None,
        worktree: branch.map(|branch| skywave_proto::Worktree {
            repo: "/srv/app".into(),
            branch: branch.into(),
        }),
    }
}

#[test]
fn a_worktree_row_is_removable_archived_or_not() {
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let me = rig.me;
    let (archived_worktree, archived_plain, active_worktree) =
        (SessionId::new(), SessionId::new(), SessionId::new());
    let archive = EventKind::SessionArchived {
        archived: true,
        by: me,
    };
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![
                event_at(archived_worktree, at, created_session(Some("feat-x"))),
                event_at(archived_worktree, at, archive.clone()),
                event_at(archived_plain, at, created_session(None)),
                event_at(archived_plain, at, archive),
                event_at(active_worktree, at, created_session(Some("feat-y"))),
            ],
        })
        .unwrap();
    let flags = |rig: &Rig, id: SessionId| {
        let row = match rig.list() {
            ScreenView::SessionList { rows, .. } => rows
                .into_iter()
                .find(|r| r.session_id == id.0.to_string())
                .unwrap(),
            _ => unreachable!(),
        };
        (row.removable, row.worktree_removed)
    };
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::ArchivedSessions,
        },
    });
    rig.results();
    rig.view();
    assert_eq!(flags(&rig, archived_worktree), (true, false));
    assert_eq!(flags(&rig, archived_plain), (false, false));

    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![event_at(
                archived_worktree,
                at,
                EventKind::WorktreeRemoved { by: me },
            )],
        })
        .unwrap();
    rig.view();
    assert_eq!(flags(&rig, archived_worktree), (false, true));

    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::SessionList,
        },
    });
    rig.results();
    rig.view();
    // An active worktree row is removable too: archiving never removes.
    assert_eq!(flags(&rig, active_worktree), (true, false));
}

#[test]
fn the_remove_worktree_intent_queues_the_command() {
    let mut rig = Rig::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let session = SessionId::new();
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![
                event_at(session, at, created_session(Some("feat-x"))),
                event_at(
                    session,
                    at,
                    EventKind::SessionArchived {
                        archived: true,
                        by: rig.me,
                    },
                ),
            ],
        })
        .unwrap();
    rig.view();
    rig.intent(GuiIntent::Act {
        action: ActionIntent::RemoveWorktree {
            session_id: session.0.to_string(),
        },
    });
    let entry = rig
        .seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .find(|e| matches!(e.command.kind, CommandKind::RemoveWorktree { .. }))
        .unwrap();
    assert!(matches!(
        entry.command.kind,
        CommandKind::RemoveWorktree { session_id } if session_id == session
    ));
}

/// The screen the head last sent.
fn sent_screen(rig: &Rig) -> ScreenView {
    rig.head.sent.screen.as_ref().unwrap().screen.clone()
}

/// Asks for the worktrees screen from the list, and returns the request.
fn open_worktrees(
    rig: &mut Rig,
    requests: &std::sync::mpsc::Receiver<skywave_core::Request>,
) -> u64 {
    rig.intent(GuiIntent::Act {
        action: ActionIntent::Navigate {
            route: RouteView::Worktrees,
        },
    });
    rig.results();
    rig.view();
    requests
        .try_iter()
        .find_map(|r| match r {
            skywave_core::Request::Worktrees { req } => Some(req),
            _ => None,
        })
        .expect("the core asked for the worktrees")
}

#[test]
fn the_worktrees_screen_lists_each_repo_with_its_sessions() {
    use skywave_proto::{RepoListing, RepoWorktrees, WorktreeEntry, WorktreeList};

    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let in_worktree = SessionId::new();
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![event_at(in_worktree, at, created_session(Some("feat-x")))],
        })
        .unwrap();
    rig.online_on_the_list();
    let req = open_worktrees(&mut rig, &requests);
    let ScreenView::Worktrees { status_label, .. } = sent_screen(&rig) else {
        unreachable!()
    };
    assert_eq!(status_label.as_deref(), Some("Asking the server…"));

    let (repo, path) = ("/srv/app", "/srv/app.worktrees/feat-x");
    rig.seam.worktrees_listed(
        req,
        Ok(WorktreeList {
            at,
            repos: vec![
                RepoWorktrees {
                    repo: repo.into(),
                    listing: RepoListing::Worktrees {
                        worktrees: vec![
                            WorktreeEntry {
                                path: repo.into(),
                                branch: Some("main".into()),
                                head: None,
                                main: true,
                                missing: false,
                                dirty: Some(false),
                                merged: true,
                            },
                            WorktreeEntry {
                                path: path.into(),
                                branch: Some("feat-x".into()),
                                head: None,
                                main: false,
                                missing: false,
                                dirty: Some(true),
                                merged: false,
                            },
                        ],
                        more: false,
                    },
                },
                RepoWorktrees {
                    repo: "/srv/gone".into(),
                    listing: RepoListing::Gone,
                },
            ],
        }),
    );
    rig.view();
    let ScreenView::Worktrees {
        status_label,
        repos,
        empty_label,
    } = sent_screen(&rig)
    else {
        unreachable!()
    };
    assert_eq!(status_label, None);
    assert_eq!(empty_label, None);
    let names: Vec<&str> = repos.iter().map(|r| r.repo.as_str()).collect();
    assert_eq!(names, [repo, "/srv/gone"]);
    assert_eq!(repos[0].note, None);
    assert_eq!(repos[1].note.as_deref(), Some("Not a repository any more."));
    assert!(repos[1].worktrees.is_empty());

    let (feature, main) = (&repos[0].worktrees[0], &repos[0].worktrees[1]);
    assert!(main.main);
    assert!(!feature.main);
    assert_eq!(main.tags, ["main"]);
    assert!(main.sessions.is_empty(), "no session runs in the main tree");
    assert_eq!(feature.branch, "feat-x");
    assert_eq!(feature.tags, ["uncommitted changes", "not merged"]);
    assert_eq!(feature.sessions.len(), 1);
    assert_eq!(feature.sessions[0].session_id, in_worktree.0.to_string());
    assert!(feature.archived.is_empty());
    let remove = feature.remove.as_ref().unwrap();
    assert_eq!(remove.session_id, in_worktree.0.to_string());
    assert_eq!(main.remove, None);
    assert_eq!(feature.removal_label, None);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::RemoveWorktree {
            session_id: in_worktree.0.to_string(),
        },
    });
    rig.results();
    rig.view();
    let ScreenView::Worktrees { repos, .. } = sent_screen(&rig) else {
        unreachable!()
    };
    assert_eq!(
        repos[0].worktrees[0].removal_label.as_deref(),
        Some("queued")
    );
}

#[test]
fn the_worktrees_screen_puts_active_worktrees_first_and_folds_archived_sessions() {
    use skywave_proto::{RepoListing, RepoWorktrees, WorktreeEntry, WorktreeList};

    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    let (active, old) = (SessionId::new(), SessionId::new());
    let archive = EventKind::SessionArchived {
        archived: true,
        by: rig.me,
    };
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![
                event_at(active, at, created_session(Some("feat-x"))),
                event_at(old, at, created_session(Some("old"))),
                event_at(old, at, archive),
            ],
        })
        .unwrap();
    rig.online_on_the_list();
    let req = open_worktrees(&mut rig, &requests);

    let entry = |path: &str, branch: &str, main: bool, merged: bool| WorktreeEntry {
        path: path.into(),
        branch: Some(branch.into()),
        head: None,
        main,
        missing: false,
        dirty: Some(false),
        merged,
    };
    // The server's order: repositories by path, worktrees in git's order.
    rig.seam.worktrees_listed(
        req,
        Ok(WorktreeList {
            at,
            repos: vec![
                RepoWorktrees {
                    repo: "/srv/a-plain".into(),
                    listing: RepoListing::Worktrees {
                        worktrees: vec![entry("/srv/a-plain", "main", true, true)],
                        more: false,
                    },
                },
                RepoWorktrees {
                    repo: "/srv/app".into(),
                    listing: RepoListing::Worktrees {
                        worktrees: vec![
                            entry("/srv/app", "main", true, true),
                            entry("/srv/app.worktrees/old", "old", false, true),
                            entry("/srv/app.worktrees/feat-x", "feat-x", false, false),
                        ],
                        more: false,
                    },
                },
                RepoWorktrees {
                    repo: "/srv/gone".into(),
                    listing: RepoListing::Gone,
                },
            ],
        }),
    );
    rig.view();
    let ScreenView::Worktrees { repos, .. } = sent_screen(&rig) else {
        unreachable!()
    };
    let names: Vec<&str> = repos.iter().map(|r| r.repo.as_str()).collect();
    assert_eq!(names, ["/srv/app", "/srv/a-plain", "/srv/gone"]);
    let paths: Vec<&str> = repos[0].worktrees.iter().map(|w| w.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/srv/app.worktrees/feat-x",
            "/srv/app.worktrees/old",
            "/srv/app"
        ]
    );

    let (feat, old_tree, main) = (
        &repos[0].worktrees[0],
        &repos[0].worktrees[1],
        &repos[0].worktrees[2],
    );
    let ids = |sessions: &[WorktreeSessionView]| -> Vec<String> {
        sessions.iter().map(|s| s.session_id.clone()).collect()
    };
    assert_eq!(ids(&feat.sessions), [active.0.to_string()]);
    assert!(feat.archived.is_empty());
    assert_eq!(
        feat.remove.as_ref().map(|r| r.session_id.clone()),
        Some(active.0.to_string())
    );

    assert!(old_tree.sessions.is_empty());
    assert_eq!(ids(&old_tree.archived), [old.0.to_string()]);
    let remove = old_tree.remove.as_ref().unwrap();
    assert_eq!(remove.session_id, old.0.to_string());
    assert_eq!(remove.title, old_tree.archived[0].title);

    assert!(main.main);
    assert!(main.sessions.is_empty() && main.archived.is_empty());
    assert_eq!(main.remove, None);
}

#[test]
fn refreshing_the_worktrees_screen_asks_the_server_again() {
    use skywave_proto::WorktreeList;

    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    rig.online_on_the_list();
    let req = open_worktrees(&mut rig, &requests);
    rig.seam.worktrees_listed(
        req,
        Ok(WorktreeList {
            at,
            repos: Vec::new(),
        }),
    );
    rig.view();
    let ScreenView::Worktrees {
        status_label,
        empty_label,
        ..
    } = sent_screen(&rig)
    else {
        unreachable!()
    };
    assert_eq!(status_label, None);
    assert_eq!(
        empty_label.as_deref(),
        Some("No session runs in a worktree yet.")
    );

    rig.intent(GuiIntent::Act {
        action: ActionIntent::RefreshWorktrees,
    });
    assert_eq!(rig.actions.last(), Some(&Action::RefreshWorktrees));
    rig.results();
    rig.view();
    let ScreenView::Worktrees { status_label, .. } = sent_screen(&rig) else {
        unreachable!()
    };
    assert_eq!(status_label.as_deref(), Some("Refreshing…"));
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
        attachments: Vec::new(),
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
        attachments: Vec::new(),
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
            full: None,
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
    let me = rig.seam.pairing().unwrap().credentials.device_id;
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
                        worktree: None,
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

/// An edit's call and result: a diff body whole in its preview, the input
/// as a blob, and a result with no preview whose whole text is a 70 B
/// blob (skywaved now carries one that small inline, `an_inline_edit`
/// below; a blob stands for any larger output).
fn an_edit() -> (Vec<EventKind>, skywave_proto::BlobId) {
    use skywave_proto::{BlobId, BlobRef, Body, CallId, Full, ToolCall, ToolDisplay, ToolResult};
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
            input: Some(Full::Blob(BlobRef {
                id: BlobId::new(),
                size: 180,
                sha256: skywave_proto::ContentHash::of(b""),
            })),
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(call),
            is_error: false,
            status: Some("updated +1 -1".into()),
            preview: String::new(),
            lines: 1,
            output: Some(Full::Blob(BlobRef {
                id: output,
                size: 70,
                sha256: skywave_proto::ContentHash::of(b""),
            })),
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
fn expanding_downloads_every_blob_the_item_needs_at_once() {
    use skywave_proto::{BlobId, BlobRef, Full};
    let mut rig = Rig::new();
    let (mut kinds, output) = an_edit();
    let body = BlobId::new();
    let EventKind::ToolCall(call) = &mut kinds[0] else {
        panic!("an edit starts with its call");
    };
    call.display.body.as_mut().unwrap().full = Some(Full::Blob(BlobRef {
        id: body,
        size: 90,
        sha256: skywave_proto::ContentHash::of(b""),
    }));
    rig.log(kinds);
    rig.actions.clear();
    let key = rig.rows().last().unwrap().key.clone();
    // The intent alone, with no view after it: the tap yields both.
    rig.intent(GuiIntent::ExpandItem { key });
    let asked: Vec<_> = rig
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::DownloadBlob(id) => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [body, output], "dispatched {:?}", rig.actions);
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

/// A long message: its head as the row's text, where its whole stands as
/// a section beneath; expanding fetches the whole and shows it, a copy
/// takes it, and its blob may be deleted but not cropped.
#[test]
fn a_long_message_shows_its_head_and_expands_to_its_whole() {
    use crate::signals::{BlobUseView, CopyWhat};
    use skywave_proto::{BlobId, BlobRef, ContentHash, EventKind, Full};
    let mut rig = Rig::new();
    let blob = BlobId::new();
    rig.log(vec![EventKind::AgentMessage {
        text: "# Report\nhead line".into(),
        parent: None,
        full: Some(Full::Blob(BlobRef {
            id: blob,
            size: 40_000,
            sha256: ContentHash::of(b""),
        })),
    }]);
    let row = rig.rows().last().unwrap().clone();
    assert_eq!(row.kind, RowKind::AgentMessage);
    assert_eq!(row.text, "# Report\nhead line");
    assert!(row.expandable && !row.expanded);
    // Collapsed: no lines, only where the whole stands.
    assert_eq!(row.sections.len(), 1, "{row:?}");
    assert!(row.sections[0].lines.is_empty());
    let facts = row.sections[0].blob.as_ref().expect("its blob");
    assert_eq!(facts.label, "not downloaded · 39.1 KiB");
    let target = row.blob_target.as_ref().expect("a target");
    assert!(!target.can_crop && target.delete_question.is_some());
    assert_eq!(target.holds, BlobUseView::Message);

    rig.actions.clear();
    rig.intent(GuiIntent::ExpandItem {
        key: row.key.clone(),
    });
    assert_eq!(rig.actions, [Action::DownloadBlob(blob)]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    std::fs::write(&path, b"# Report\nhead line\nthe deep finding\n").unwrap();
    rig.seam
        .blob_status(blob, skywave_core::BlobStatus::Available { path })
        .unwrap();
    rig.view();
    let row = rig.rows().last().unwrap().clone();
    let lines: Vec<&str> = row.sections[0]
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(lines, ["# Report", "head line", "the deep finding"]);
    let outs = rig.intent(GuiIntent::Copy {
        key: row.key,
        what: CopyWhat::Main,
    });
    assert_eq!(copied(&outs), ["# Report\nhead line\nthe deep finding\n"]);
}

/// A message's image opens its own file, once the image is on this device;
/// before, the open says it is not downloaded.
#[test]
fn opening_a_message_image_opens_its_file() {
    use skywave_proto::{Attachment, BlobId, BlobRef, ImageType};
    let mut rig = Rig::new();
    let blob = BlobId::new();
    // A user message is logged with its command id, as a device's own is.
    let kind = EventKind::UserMessage {
        device_id: DeviceId::new(),
        text: "look".into(),
        attachments: vec![Attachment {
            blob: BlobRef {
                id: blob,
                size: 4,
                sha256: ContentHash::of(b"png1"),
            },
            media_type: ImageType::Png,
        }],
    };
    rig.seam
        .apply_events(&EventBatch {
            from: Seq(rig.seam.cursor().unwrap().0 + 1),
            events: vec![Event {
                session_id: rig.session,
                at: Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap(),
                command_id: kind.logged_by_command().then(CommandId::new),
                context: skywave_proto::ContextId::first_of(rig.session),
                kind,
            }],
        })
        .unwrap();
    rig.view();
    // Control: the message's row carries its image, not yet downloaded.
    let images = &rig.rows().last().unwrap().images;
    assert_eq!(images.len(), 1, "{images:?}");
    assert_eq!(images[0].blob_id, Some(blob.0.to_string()));
    let open = GuiIntent::OpenFull {
        blob_id: blob.0.to_string(),
    };
    let outs = rig.intent(open.clone());
    assert_eq!(notices(&outs), ["not downloaded yet"], "{outs:?}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    std::fs::write(&path, b"png1").unwrap();
    rig.seam
        .blob_status(
            blob,
            skywave_core::BlobStatus::Available { path: path.clone() },
        )
        .unwrap();
    rig.view();
    let outs = rig.intent(open);
    let shown = path.display().to_string();
    assert!(
        outs.iter().any(|out| matches!(
            out,
            Out::Effect(PlatformEffect::OpenFile { path }) if *path == shown
        )),
        "{outs:?}"
    );
}

#[test]
fn an_inline_edit_output_expands_whole_without_a_download() {
    use skywave_proto::{EventKind, Full, ToolResult};
    let mut rig = Rig::new();
    let (mut kinds, _) = an_edit();
    let note = "The file /srv/work/demo.txt has been updated.";
    if let Some(EventKind::ToolResult(ToolResult { output, .. })) = kinds.last_mut() {
        *output = Some(Full::Inline(note.into()));
    }
    rig.log(kinds);
    let key = rig.rows().last().unwrap().key.clone();
    rig.intent(GuiIntent::ExpandItem { key });
    let row = rig.rows().last().unwrap();
    assert!(row.expanded);
    let lines: Vec<&str> = row.sections[1]
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(lines, [note]);
    assert_eq!(row.sections[1].blob, None);
    assert_eq!(row.sections[1].note, None);
    assert!(
        !rig.actions
            .iter()
            .any(|a| matches!(a, Action::DownloadBlob(_))),
        "dispatched {:?}",
        rig.actions
    );
}

#[test]
fn an_inline_output_expanded_is_cut_short_like_a_blob() {
    use skywave_proto::{EventKind, Full, ToolResult};
    let mut rig = Rig::new();
    let (mut kinds, _) = an_edit();
    let whole: String = (0..500).map(|i| format!("l{i:03}\n")).collect();
    if let Some(EventKind::ToolResult(ToolResult { output, .. })) = kinds.last_mut() {
        *output = Some(Full::Inline(whole));
    }
    rig.log(kinds);
    let key = rig.rows().last().unwrap().key.clone();
    rig.intent(GuiIntent::ExpandItem { key });
    let section = &rig.rows().last().unwrap().sections[1];
    assert_eq!(section.lines.len(), crate::timeline::SHOW_LINES);
    assert!(section.cut_short);
    assert_eq!(
        section.note.as_deref(),
        Some("cut short: 400 lines of 2.4 KiB shown")
    );
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

    /// The safety check switch as the head last told Dart.
    fn safety_check(&self) -> crate::signals::SafetyCheckView {
        match &self.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Session { session } => session.safety_check.clone(),
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
    // No bypass until a server allows it; then last, asking once more.
    use crate::signals::PermissionModeView as V;
    let offered = |agent: &crate::signals::AgentSettingsView| -> Vec<V> {
        agent.modes.iter().map(|m| m.mode).collect()
    };
    let safe = [V::Ask, V::AcceptEdits, V::Plan, V::Auto, V::DontAsk];
    assert_eq!(offered(&agent), safe);
    assert!(agent.modes.iter().all(|m| !m.needs_confirmation));
    rig.hello(false, true);
    let agent = rig.agent();
    assert_eq!(offered(&agent)[..5], safe);
    assert_eq!(offered(&agent)[5], V::Bypass);
    assert!(agent.modes[5].needs_confirmation);
    assert!(agent.modes[0].current);
    rig.hello(false, false);
    assert_eq!(offered(&rig.agent()), safe, "a later hello takes it back");
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
fn the_safety_check_switch_shows_off_waiting_then_on() {
    let mut rig = Rig::new();
    let switch = rig.safety_check();
    assert_eq!(switch.label, "safety check off");
    assert!(!switch.on && switch.turn_on && !switch.pending);

    rig.intent(GuiIntent::Act {
        action: ActionIntent::SetSafetyCheck {
            session_id: rig.session.0.to_string(),
            on: true,
        },
    });
    assert_eq!(
        rig.actions,
        [Action::SetSafetyCheck {
            session_id: rig.session,
            on: true,
        }]
    );
    rig.results();
    rig.view();
    let switch = rig.safety_check();
    assert_eq!(switch.label, "safety check off → on");
    assert!(switch.pending && switch.on && !switch.turn_on);

    rig.log(vec![EventKind::SafetyCheckSet {
        on: true,
        by: rig.me,
    }]);
    let switch = rig.safety_check();
    assert_eq!(switch.label, "safety check on");
    assert!(switch.on && !switch.pending && !switch.turn_on);
    assert_eq!(switch.refusal, None);
}

#[test]
fn a_call_the_safety_check_approved_says_so() {
    use skywave_proto::{CallId, ToolKind, ToolResult};
    let approved = "approved by the safety check (read-only: cat)";
    let result = |call| {
        EventKind::ToolResult(ToolResult {
            call: Some(call),
            is_error: false,
            status: None,
            preview: "notes".into(),
            lines: 1,
            output: None,
        })
    };

    // The control: the same call, no approval logged, says nothing of one.
    let mut plain = Rig::new();
    let call = CallId::new();
    plain.log(vec![
        a_call(None, call, "Bash", ToolKind::Shell, "cat notes.txt"),
        result(call),
    ]);
    assert!(!plain.tool_row("Bash").state_label.contains("safety check"));

    let mut rig = Rig::new();
    let call = CallId::new();
    rig.log(vec![
        a_call(None, call, "Bash", ToolKind::Shell, "cat notes.txt"),
        EventKind::SafetyCheckApproved {
            call: Some(call),
            tool: "Bash".into(),
            summary: "cat notes.txt".into(),
            reason: "read-only: cat".into(),
            shell: None,
        },
        result(call),
    ]);
    let row = rig.tool_row("Bash");
    assert!(row.state_label.contains(approved), "{}", row.state_label);
}

/// The result of `call` as the agent logs it, for a call the tests make.
fn a_result(call: skywave_proto::CallId) -> EventKind {
    EventKind::ToolResult(skywave_proto::ToolResult {
        call: Some(call),
        is_error: false,
        status: None,
        preview: "notes".into(),
        lines: 1,
        output: None,
    })
}

#[test]
fn a_call_the_safety_check_approved_offers_should_have_asked() {
    use crate::signals::{SafetyFeedbackFacts, SafetyJudgmentView};
    use skywave_proto::{CallId, SafetyJudgment, ToolKind};
    let mut rig = Rig::new();
    let call = CallId::new();
    rig.log(vec![
        a_call(None, call, "Bash", ToolKind::Shell, "cat a"),
        EventKind::SafetyCheckApproved {
            call: Some(call),
            tool: "Bash".into(),
            summary: "cat a".into(),
            reason: "read-only: cat".into(),
            shell: Some("cat a".into()),
        },
        a_result(call),
    ]);
    let row = rig.tool_row("Bash");
    assert_eq!(
        row.feedback,
        Some(SafetyFeedbackFacts {
            call: call.0.to_string(),
            judgment: SafetyJudgmentView::ShouldAsk,
        })
    );
    assert!(
        !row.state_label.contains("feedback:"),
        "{}",
        row.state_label
    );

    rig.log(vec![EventKind::SafetyCheckFeedback {
        call,
        judgment: SafetyJudgment::ShouldAsk,
        note: None,
        shell: "cat a".into(),
        checked_safe: true,
        checked_reason: "read-only: cat".into(),
        by: rig.me,
    }]);
    let label = rig.tool_row("Bash").state_label;
    assert!(label.ends_with(" · feedback: should have asked"), "{label}");
}

#[test]
fn a_safety_check_feedback_on_its_way_shows_its_outgoing_state() {
    use crate::signals::SafetyJudgmentView;
    use skywave_proto::{CallId, SafetyJudgment, ToolKind};
    let mut rig = Rig::new();
    let call = CallId::new();
    rig.log(vec![
        a_call(None, call, "Bash", ToolKind::Shell, "cat a"),
        EventKind::SafetyCheckApproved {
            call: Some(call),
            tool: "Bash".into(),
            summary: "cat a".into(),
            reason: "read-only: cat".into(),
            shell: Some("cat a".into()),
        },
        a_result(call),
    ]);
    rig.intent(GuiIntent::Act {
        action: ActionIntent::SafetyCheckFeedback {
            session_id: rig.session.0.to_string(),
            call: call.0.to_string(),
            judgment: SafetyJudgmentView::ShouldAsk,
            note: Some("x".into()),
        },
    });
    assert_eq!(
        rig.actions,
        [Action::SafetyCheckFeedback {
            session_id: rig.session,
            call,
            judgment: SafetyJudgment::ShouldAsk,
            note: Some("x".into()),
        }]
    );
    rig.results();
    rig.view();
    let label = rig.tool_row("Bash").state_label;
    assert!(
        label.ends_with(" · feedback: should have asked · queued"),
        "{label}"
    );
}

#[test]
fn a_shell_row_offers_its_commands_as_rule_candidates() {
    use skywave_proto::{CallId, ToolKind};
    let words = |line: &[&str]| line.iter().map(|word| word.to_string()).collect::<Vec<_>>();
    let command = "cargo test -p x | tail -n 5";
    let mut rig = Rig::new();
    let call = CallId::new();
    rig.log(vec![
        a_call(None, call, "Bash", ToolKind::Shell, command),
        EventKind::SafetyCheckApproved {
            call: Some(call),
            tool: "Bash".into(),
            summary: command.into(),
            reason: "read-only: cargo, tail".into(),
            shell: Some(command.into()),
        },
        a_result(call),
    ]);
    assert_eq!(
        rig.tool_row("Bash").rule_candidates,
        vec![
            words(&["cargo", "test", "-p", "x"]),
            words(&["tail", "-n", "5"]),
        ]
    );

    // The control: a call that names no shell command offers none.
    let mut plain = Rig::new();
    let plain_call = CallId::new();
    plain.log(vec![
        a_call(None, plain_call, "Bash", ToolKind::Shell, "rm -rf build"),
        a_result(plain_call),
    ]);
    assert_eq!(
        plain.tool_row("Bash").rule_candidates,
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn trusting_a_command_sends_set_safety_rule() {
    let mut rig = Rig::new();
    let words = vec!["git".to_owned(), "commit".to_owned()];
    rig.intent(GuiIntent::Act {
        action: ActionIntent::SetSafetyRule {
            session_id: rig.session.0.to_string(),
            words: words.clone(),
            on: true,
        },
    });
    assert_eq!(
        rig.actions,
        [Action::SetSafetyRule {
            session_id: rig.session,
            words,
            on: true,
        }]
    );
}

#[test]
fn rule_candidates_wait_for_the_shell_command() {
    use skywave_proto::{ApprovalId, CallId, ToolKind};
    let mut rig = Rig::new();
    let call = CallId::new();
    rig.log(vec![a_call(
        None,
        call,
        "Bash",
        ToolKind::Shell,
        "git status",
    )]);
    // Mapped before its approval names the command: nothing to offer yet,
    // and nothing is remembered for later.
    assert_eq!(
        rig.tool_row("Bash").rule_candidates,
        Vec::<Vec<String>>::new()
    );

    rig.log(vec![EventKind::ApprovalRequested {
        id: ApprovalId::new(),
        call: Some(call),
        tool: "Bash".into(),
        summary: "git status".into(),
        input: None,
        suggestions: Vec::new(),
        shell: Some("git status".into()),
    }]);
    assert_eq!(
        rig.tool_row("Bash").rule_candidates,
        vec![vec!["git".to_owned(), "status".to_owned()]]
    );
}

#[test]
fn a_shell_call_that_asked_offers_should_have_been_approved() {
    use crate::signals::{SafetyFeedbackFacts, SafetyJudgmentView};
    use skywave_proto::{ApprovalId, CallId, ToolKind};
    let approval = |call: CallId, shell: Option<String>| EventKind::ApprovalRequested {
        id: ApprovalId::new(),
        call: Some(call),
        tool: "Bash".into(),
        summary: "rm -rf build".into(),
        input: None,
        suggestions: Vec::new(),
        shell,
    };
    let mut rig = Rig::new();
    let call = CallId::new();
    rig.log(vec![
        a_call(None, call, "Bash", ToolKind::Shell, "rm -rf build"),
        approval(call, Some("rm -rf build".into())),
    ]);
    assert_eq!(
        rig.tool_row("Bash").feedback,
        Some(SafetyFeedbackFacts {
            call: call.0.to_string(),
            judgment: SafetyJudgmentView::ShouldApprove,
        })
    );

    // The control: an approval naming no shell command is offered none.
    let mut plain = Rig::new();
    let plain_call = CallId::new();
    plain.log(vec![
        a_call(None, plain_call, "Bash", ToolKind::Shell, "rm -rf build"),
        approval(plain_call, None),
    ]);
    assert_eq!(plain.tool_row("Bash").feedback, None);
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
            worktree: Some(String::new()),
        })
    );
    assert_eq!(
        queued_creations(&rig),
        [("/srv/app".to_owned(), Some("abc-123".to_owned()))]
    );
}

#[test]
fn accepting_with_a_worktree_branch_asks_for_it() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/app".into(),
    });
    rig.intent(GuiIntent::CreateWorktreeTyped {
        worktree: "  feat/x\u{7}".into(),
    });
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(
        rig.actions.last(),
        Some(&Action::CreateSession {
            workdir: "/srv/app".into(),
            conversation: Some(String::new()),
            worktree: Some("  feat/x".into()),
        })
    );
    // The core trims the branch; queued_creations ignores it, so read it here.
    let worktrees: Vec<Option<String>> = rig
        .seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.command.kind {
            CommandKind::CreateSession { worktree, .. } => Some(worktree),
            _ => None,
        })
        .collect();
    assert_eq!(worktrees, [Some("feat/x".to_owned())]);
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
    // The seeded session's workdir is the one recent candidate: highlight it.
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/".into(),
    });
    rig.results();
    rig.view();
    rig.intent(GuiIntent::CompletionMove { up: false });
    let before = rig.actions.len();
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(
        rig.actions.len(),
        before + 1,
        "taking a candidate descends into it"
    );
    assert!(matches!(
        rig.actions.last(),
        Some(Action::CompleteWorkdir { .. })
    ));
    assert!(queued_creations(&rig).is_empty());
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(queued_creations(&rig).len(), 1);
    assert_eq!(queued_creations(&rig)[0].1.as_deref(), Some("abc-123"));
}

#[test]
fn opening_the_prompt_lists_at_once() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    assert_eq!(
        rig.actions.last(),
        Some(&Action::CompleteWorkdir {
            input: String::new()
        })
    );
    rig.results();
    rig.view();
    let popup = shown_prompt(&rig)
        .popup
        .expect("the candidate list is shown at once");
    assert!(!popup.candidates.is_empty(), "the recent workdir is listed");
    assert_eq!(popup.selected, None, "nothing is highlighted");
}

#[test]
fn enter_on_a_highlighted_row_descends_and_keeps_the_list() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/".into(),
    });
    rig.results();
    rig.view();
    let listed = shown_prompt(&rig).popup.expect("the candidates are listed");
    let paths: Vec<_> = listed.candidates.iter().map(|c| c.path.clone()).collect();
    assert!(!paths.is_empty(), "a recent workdir is listed");
    assert!(listed.candidates.iter().any(|c| c.recent));
    for c in listed.candidates.iter().filter(|c| c.recent) {
        assert_eq!(c.label, c.path, "a recent row shows its full path");
    }

    rig.intent(GuiIntent::CompletionMove { up: false });
    let before = shown_prompt(&rig).input_rev;
    rig.intent(GuiIntent::CompletionAccept);
    assert_eq!(
        rig.actions.last(),
        Some(&Action::CompleteWorkdir {
            input: format!("{}/", paths[0])
        }),
        "Enter descends rather than creating"
    );
    assert!(queued_creations(&rig).is_empty());
    rig.results();
    rig.view();
    let after = shown_prompt(&rig);
    assert_eq!(after.input, format!("{}/", paths[0]));
    assert!(after.input_rev > before);
    let popup = after.popup.expect("the list stays open");
    assert_eq!(popup.selected, None);
}

#[test]
fn a_listing_never_rewrites_the_input() {
    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::CreateOpen);
    // Nothing matches, so Tab fills nothing and asks again.
    rig.intent(GuiIntent::CompletionTyped {
        input: "/nope/".into(),
    });
    rig.intent(GuiIntent::CompletionComplete);
    rig.results();
    rig.view();
    let before = shown_prompt(&rig);
    assert_eq!(before.input, "/nope/");

    answer_listing(&mut rig, &requests, "/nope/", &["x"]);
    let after = shown_prompt(&rig);
    let names: Vec<_> = after
        .popup
        .expect("the list is shown")
        .candidates
        .iter()
        .map(|c| c.path.clone())
        .collect();
    assert_eq!(names, ["/nope/x"], "the reply reached the list");
    assert_eq!(after.input, before.input);
    assert_eq!(after.input_rev, before.input_rev);
}

#[test]
fn stale_rows_are_marked_and_cannot_be_picked() {
    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::CreateOpen);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/".into(),
    });
    rig.results();
    rig.view();
    answer_listing(&mut rig, &requests, "/srv/", &["api", "web"]);
    // A new directory: its listing is pending, the answered one stays.
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/api/".into(),
    });
    rig.results();
    rig.view();
    let shown = shown_prompt(&rig);
    assert_eq!(shown.input, "/srv/api/");
    let popup = shown.popup.expect("the list is shown");
    assert!(popup.stale, "the rows are for the earlier input");
    assert!(popup.loading, "the new directory's listing is pending");
    assert!(
        !popup.candidates.is_empty(),
        "the stale rows are still shown"
    );

    let actions = rig.actions.len();
    rig.intent(GuiIntent::CompletionPick { index: 0 });
    assert_eq!(rig.actions.len(), actions, "a stale row is not taken");
    rig.results();
    rig.view();
    let after = shown_prompt(&rig);
    assert_eq!(after.input, "/srv/api/");
    assert_eq!(after.input_rev, shown.input_rev);

    answer_listing(&mut rig, &requests, "/srv/api/", &["x"]);
    let answered = shown_prompt(&rig).popup.expect("the list is shown");
    assert!(!answered.loading, "the listing has answered");
    assert!(!answered.stale, "the rows are for the input now");
}

#[test]
fn a_listed_row_shows_its_name() {
    let mut rig = Rig::new();
    let requests = rig.seam.take_requests().unwrap();
    rig.online_on_the_list();
    rig.intent(GuiIntent::CreateOpen);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/".into(),
    });
    rig.results();
    rig.view();
    answer_listing(&mut rig, &requests, "/srv/", &["api"]);
    let popup = shown_prompt(&rig).popup.expect("the listing is shown");
    let api = popup
        .candidates
        .iter()
        .find(|c| !c.recent)
        .expect("the server listed a directory");
    assert_eq!(api.path, "/srv/api");
    assert_eq!(api.label, "api", "under the input's directory: its name");
}

#[test]
fn esc_unselects_then_closes() {
    let mut rig = Rig::new();
    open_create_prompt(&mut rig);
    rig.intent(GuiIntent::CompletionTyped {
        input: "/srv/".into(),
    });
    rig.results();
    rig.view();
    rig.intent(GuiIntent::CompletionMove { up: false });
    rig.results();
    rig.view();
    assert_eq!(shown_prompt(&rig).popup.unwrap().selected, Some(0));

    rig.intent(GuiIntent::CompletionCancel);
    rig.results();
    rig.view();
    let kept = shown_prompt(&rig);
    assert_eq!(
        kept.popup.unwrap().selected,
        None,
        "the first Esc unselects"
    );

    rig.intent(GuiIntent::CompletionCancel);
    assert_eq!(rig.actions.last(), Some(&Action::EndCompletion));
    rig.results();
    rig.view();
    let ScreenView::SessionList { create, .. } = rig.list() else {
        unreachable!()
    };
    assert_eq!(create, None, "the second Esc closes the prompt");
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

#[test]
fn conversation_only_leaves_tool_calls_out_of_the_rows_until_switched_off() {
    let mut rig = Rig::new();
    let read = skywave_proto::CallId::new();
    rig.log(vec![
        EventKind::AgentMessage {
            parent: None,
            text: "looking".into(),
            full: None,
        },
        a_call(
            None,
            read,
            "Read",
            skywave_proto::ToolKind::FileRead,
            "a.txt",
        ),
    ]);
    let kinds = |rig: &Rig| rig.rows().iter().map(|row| row.kind).collect::<Vec<_>>();
    assert_eq!(kinds(&rig), [RowKind::AgentMessage, RowKind::Tool]);
    assert!(!rig.open_session().conversation_only);

    rig.intent(GuiIntent::ToggleConversationOnly);
    assert_eq!(kinds(&rig), [RowKind::AgentMessage]);
    assert!(rig.open_session().conversation_only);
    // A call logged while it is on stays out.
    rig.log(vec![a_call(
        None,
        skywave_proto::CallId::new(),
        "Bash",
        skywave_proto::ToolKind::Shell,
        "make",
    )]);
    assert_eq!(kinds(&rig), [RowKind::AgentMessage]);

    rig.intent(GuiIntent::ToggleConversationOnly);
    assert_eq!(
        kinds(&rig),
        [RowKind::AgentMessage, RowKind::Tool, RowKind::Tool]
    );
    assert!(!rig.open_session().conversation_only);
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
fn the_footer_shows_the_context_fill_and_a_finished_subagent_its_own_figures() {
    use skywave_proto::{CallId, SubagentUsage, ToolKind, ToolResult, TurnUsage};
    let mut rig = Rig::new();
    let agent = CallId::new();
    rig.log(vec![
        a_call(None, agent, "Agent", ToolKind::Subagent, "Check flour"),
        a_call(Some(agent), CallId::new(), "Bash", ToolKind::Shell, "ls"),
        EventKind::SubagentUsage(SubagentUsage {
            call: agent,
            duration_ms: Some(4_284),
            tool_uses: Some(1),
            context_tokens: Some(11_850),
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(agent),
            is_error: false,
            status: None,
            preview: "flour ok".into(),
            lines: 1,
            output: None,
        }),
        EventKind::TurnUsage(TurnUsage {
            duration_ms: Some(9_000),
            model: Some("model-a".into()),
            context_tokens: Some(18_976),
            context_window: Some(1_000_000),
            ..TurnUsage::default()
        }),
    ]);
    let status = "finished · 1 step · 4.3s · ctx 12k";
    assert_eq!(rig.tool_row("Agent").status.as_deref(), Some(status));
    // The two numbers stay apart: the footer shows the main thread's only.
    let usage = rig.open_session().usage;
    assert!(usage.contains(&"ctx 19k/1M · 2%".to_owned()), "{usage:?}");
    rig.view_subagent(Some(agent));
    let header = rig.open_session().subagent.expect("a subagent's view");
    assert_eq!(header.status_label, status);
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
            shell: None,
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
fn a_subagents_shell_approval_offers_its_commands_to_trust() {
    use skywave_proto::{ApprovalId, CallId, ToolKind};
    let mut rig = Rig::new();
    let (agent, step) = (CallId::new(), CallId::new());
    rig.log(vec![
        a_call(None, agent, "Agent", ToolKind::Subagent, "Explore sync"),
        a_call(Some(agent), step, "Bash", ToolKind::Shell, "git status"),
        EventKind::ApprovalRequested {
            id: ApprovalId::new(),
            call: Some(step),
            tool: "Bash".into(),
            summary: "git status".into(),
            input: None,
            suggestions: Vec::new(),
            shell: Some("git status".into()),
        },
    ]);
    // The foreground subagent's row is not in the main timeline, yet its
    // approval carries the command to trust.
    let approval = rig
        .open_session()
        .approval
        .expect("the subagent's approval");
    assert_eq!(
        approval.rule_candidates,
        vec![vec!["git".to_owned(), "status".to_owned()]]
    );
    assert!(
        approval
            .from
            .as_deref()
            .unwrap_or_default()
            .starts_with("from subagent")
    );

    // Control: an approval that names no shell command offers nothing.
    let mut rig = Rig::new();
    rig.log(vec![EventKind::ApprovalRequested {
        id: ApprovalId::new(),
        call: None,
        tool: "Write".into(),
        summary: "src/lib.rs".into(),
        input: None,
        suggestions: Vec::new(),
        shell: None,
    }]);
    let approval = rig.open_session().approval.expect("the approval");
    assert_eq!(approval.rule_candidates, Vec::<Vec<String>>::new());
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
        self.hello(shell, false);
    }

    /// The server's `hello`: whether it runs shell commands and whether
    /// it allows bypass mode.
    fn hello(&mut self, shell: bool, bypass: bool) {
        self.seam
            .hello_received(&skywave_proto::HelloResponse {
                protocol_version: skywave_proto::PROTOCOL_VERSION,
                server_id: skywave_proto::ServerId::new(),
                device_id: self.me,
                head: Seq::ZERO,
                log: skywave_proto::LogRun {
                    log: skywave_proto::LogId::new(),
                    run: skywave_proto::RunId::new(),
                    since: None,
                },
                devices: Vec::new(),
                shell,
                bypass,
                login_expires: None,
                agent_login_offered: false,
                agent_logged_out: false,
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

/// A shell command `command` requested by this device, as the log has it.
fn requested(rig: &Rig, command: &str) -> EventKind {
    EventKind::ShellRequested {
        command: command.into(),
        when: RunWhen::Always,
        by: rig.me,
    }
}

/// The shell history picker as the head last sent Dart; `None` while closed.
fn picker(rig: &Rig) -> Option<HistorySearchView> {
    rig.composer().history_search
}

#[test]
fn history_search_spans_sessions() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    let mine = requested(&rig, "git status");
    rig.log(vec![mine]);
    let other = SessionId::new();
    let created = EventKind::SessionCreated {
        workdir: "/srv/work".into(),
        context_name: "start".into(),
        from: None,
        worktree: None,
    };
    let theirs = requested(&rig, "git stash");
    rig.log_in(other, vec![created, theirs]);

    rig.intent(GuiIntent::HistorySearchOpen);
    let open = picker(&rig).expect("the picker is open");
    let commands: Vec<&str> = open.rows.iter().map(|row| row.command.as_str()).collect();
    assert_eq!(commands, ["git stash", "git status"]);
    assert_eq!(open.selected, Some(0));
    assert_eq!(open.count_label, "2/2");
    assert!(
        open.rows.iter().all(|row| row.matched.is_empty()),
        "{:?}",
        open.rows
    );

    rig.edit("gsh");
    let open = picker(&rig).expect("the picker is open");
    assert_eq!(open.rows.len(), 1, "{:?}", open.rows);
    assert_eq!(open.rows[0].command, "git stash");
    assert_eq!(open.count_label, "1/2");
    assert_eq!(
        open.rows[0].matched,
        [
            Utf16Range { start: 0, end: 1 },
            Utf16Range { start: 4, end: 5 },
            Utf16Range { start: 8, end: 9 },
        ]
    );

    rig.edit("git");
    let open = picker(&rig).expect("the picker is open");
    assert_eq!(open.rows.len(), 2, "{:?}", open.rows);
    for row in &open.rows {
        assert_eq!(row.matched, [Utf16Range { start: 0, end: 3 }]);
    }
}

#[test]
fn history_search_accepts_without_running() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    rig.log(vec![requested(&rig, "ls -la")]);
    rig.log(vec![requested(&rig, "make test")]);
    rig.actions.clear();

    rig.intent(GuiIntent::HistorySearchOpen);
    rig.intent(GuiIntent::HistorySearchMove { older: true });
    assert_eq!(picker(&rig).and_then(|p| p.selected), Some(1));
    rig.intent(GuiIntent::HistorySearchMove { older: true });
    assert_eq!(picker(&rig).and_then(|p| p.selected), Some(1), "clamped");
    rig.intent(GuiIntent::HistorySearchAccept);

    assert_eq!(rig.composer().text, "ls -la");
    assert!(picker(&rig).is_none());
    assert!(
        !rig.actions
            .iter()
            .any(|a| matches!(a, Action::RunShell { .. })),
        "{:?}",
        rig.actions
    );
    // Positive control: the same field, sent, does run.
    rig.submit();
    assert!(
        rig.actions
            .iter()
            .any(|a| matches!(a, Action::RunShell { command, .. } if command == "ls -la")),
        "{:?}",
        rig.actions
    );
}

#[test]
fn history_search_pick_and_close() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    rig.log(vec![requested(&rig, "ls -la")]);
    rig.log(vec![requested(&rig, "make test")]);

    rig.intent(GuiIntent::HistorySearchOpen);
    rig.intent(GuiIntent::HistorySearchPick { index: 1 });
    assert_eq!(rig.composer().text, "ls -la");
    assert!(picker(&rig).is_none());

    rig.edit("half");
    rig.intent(GuiIntent::HistorySearchOpen);
    assert!(picker(&rig).is_some());
    rig.intent(GuiIntent::HistorySearchClose);
    assert!(picker(&rig).is_none());
    // Dart's field keeps its text until a `rev`; the typed text is the hub's.
    assert_eq!(rig.text(), "half");
}

#[test]
fn history_search_needs_shell_mode() {
    let mut rig = Rig::new();
    rig.log(vec![requested(&rig, "ls")]);
    rig.intent(GuiIntent::HistorySearchOpen);
    assert!(picker(&rig).is_none(), "no picker in agent mode");

    rig.switch_to_shell();
    rig.intent(GuiIntent::HistorySearchOpen);
    assert!(picker(&rig).is_some());
    let id = rig.session.0.to_string();
    rig.intent(GuiIntent::ToggleShellMode { session_id: id });
    rig.results();
    rig.view();
    assert!(picker(&rig).is_none());
}

#[test]
fn history_search_typing_resets_the_selection() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    rig.log(vec![requested(&rig, "git status")]);
    rig.log(vec![requested(&rig, "git stash")]);

    rig.intent(GuiIntent::HistorySearchOpen);
    rig.intent(GuiIntent::HistorySearchMove { older: true });
    assert_eq!(picker(&rig).and_then(|p| p.selected), Some(1));
    rig.edit("git");
    assert_eq!(picker(&rig).and_then(|p| p.selected), Some(0));
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
                ..
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
    assert_eq!(shown_kinds(&outs), [NotifyKind::Shell]);
    // Once only.
    let outs = rig.head.on_view(view.clone(), rig.clock);
    assert!(
        !outs
            .iter()
            .any(|o| matches!(o, Out::Effect(PlatformEffect::Notify { .. })))
    );
    // A head whose first snapshot already counts it still says so.
    let mut fresh = Head::new(Prefs::default());
    let outs = fresh.on_view(view, rig.clock);
    assert!(
        outs.iter()
            .any(|o| matches!(o, Out::Effect(PlatformEffect::Notify { .. })))
    );
}

/// The notices `outs` shows, as `(id, title, body, session_id, kind)`.
fn shown_notices(outs: &[Out]) -> Vec<(i32, String, String, Option<String>, NotifyKind)> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Effect(PlatformEffect::Notify {
                id,
                title,
                body,
                session_id,
                kind,
            }) => Some((*id, title.clone(), body.clone(), session_id.clone(), *kind)),
            _ => None,
        })
        .collect()
}

/// The kinds of the notices `outs` shows.
fn shown_kinds(outs: &[Out]) -> Vec<NotifyKind> {
    shown_notices(outs)
        .into_iter()
        .map(|notice| notice.4)
        .collect()
}

#[test]
fn a_session_that_went_idle_notifies_once() {
    use skywave_proto::SessionState;
    let mut rig = Rig::new();
    let title = rig.open_session().title;
    rig.online_on_the_list();
    let session = rig.session;
    let working = rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    assert_eq!(notifications(&working), (vec![], vec![]));
    let outs = rig.log(vec![EventKind::SessionStateChanged(SessionState::Idle)]);
    let shown = shown_notices(&outs);
    let [(id, shown_title, body, session_id, kind)] = shown.as_slice() else {
        panic!("one idle notice: {shown:?}");
    };
    assert_eq!(shown_title, &title);
    assert_eq!(body, &format!("agent idle · {title}"));
    assert_eq!(session_id, &Some(session.0.to_string()));
    assert_eq!(kind, &NotifyKind::Idle);
    assert_eq!(*id, rig.head.notifications.id(session, Kind::Idle));
    assert_ne!(*id, rig.head.notifications.id(session, Kind::Approval));
    // Once only.
    assert_eq!(shown_notices(&rig.view()), []);
}

#[test]
fn an_idle_notice_is_taken_down_when_the_session_works_again() {
    use skywave_proto::SessionState;
    let mut rig = Rig::new();
    rig.online_on_the_list();
    rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    let idle = rig.log(vec![EventKind::SessionStateChanged(SessionState::Idle)]);
    let shown = shown_notices(&idle);
    let [(id, ..)] = shown.as_slice() else {
        panic!("one idle notice: {shown:?}");
    };
    let outs = rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    assert_eq!(notifications(&outs), (vec![], vec![*id]));
    // Once.
    assert_eq!(notifications(&rig.view()), (vec![], vec![]));
}

#[test]
fn a_turn_seen_only_in_a_catch_up_does_not_notify() {
    use skywave_proto::SessionState;
    // Not online: the core counts no turn it did not see online.
    let mut rig = Rig::new();
    rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    let outs = rig.log(vec![EventKind::SessionStateChanged(SessionState::Idle)]);
    assert!(rig.core.view().attention.idle.is_empty());
    assert_eq!(notifications(&outs), (vec![], vec![]));
    assert_eq!(shown_notices(&outs), []);
}

#[test]
fn with_idle_off_a_session_going_idle_posts_nothing_but_still_dismisses() {
    use skywave_proto::SessionState;
    let mut rig = Rig::new();
    rig.online_on_the_list();
    let session = rig.session;
    rig.intent(GuiIntent::SetNotifications {
        all: true,
        approvals: true,
        idle: false,
        shell: true,
    });
    rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    let idle = rig.log(vec![EventKind::SessionStateChanged(SessionState::Idle)]);
    assert_eq!(shown_notices(&idle), []);
    // The head never showed a notice, yet the session working again still
    // sends its dismissal: the watcher observed the idle all the same.
    let outs = rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    assert_eq!(
        notifications(&outs),
        (vec![], vec![rig.head.notifications.id(session, Kind::Idle)])
    );
    // Switched back on, the next time the agent goes idle rings once.
    rig.intent(GuiIntent::SetNotifications {
        all: true,
        approvals: true,
        idle: true,
        shell: true,
    });
    let outs = rig.log(vec![EventKind::SessionStateChanged(SessionState::Idle)]);
    let shown = shown_notices(&outs);
    let [(id, _, _, session_id, _)] = shown.as_slice() else {
        panic!("one idle notice: {shown:?}");
    };
    assert_eq!(*id, rig.head.notifications.id(session, Kind::Idle));
    assert_eq!(session_id, &Some(session.0.to_string()));
}

#[test]
fn with_all_off_an_approval_posts_nothing() {
    use skywave_proto::{ApprovalId, SessionState};
    let mut rig = Rig::new();
    rig.online_on_the_list();
    let session = rig.session;
    rig.intent(GuiIntent::SetNotifications {
        all: false,
        approvals: true,
        idle: true,
        shell: true,
    });
    rig.log(vec![EventKind::SessionStateChanged(
        SessionState::AgentWorking,
    )]);
    let approval = |id| EventKind::ApprovalRequested {
        id,
        call: None,
        tool: "Bash".into(),
        summary: "ls".into(),
        input: None,
        suggestions: Vec::new(),
        shell: None,
    };
    let outs = rig.log(vec![approval(ApprovalId::new())]);
    assert_eq!(notifications(&outs), (vec![], vec![]));

    // Switched on again, the next approval rings once.
    rig.intent(GuiIntent::SetNotifications {
        all: true,
        approvals: true,
        idle: true,
        shell: true,
    });
    let (shown, dismissed) = notifications(&rig.log(vec![approval(ApprovalId::new())]));
    assert!(dismissed.is_empty());
    let [(id, Some(session_id))] = shown.as_slice() else {
        panic!("one approval notification: {shown:?}");
    };
    assert_eq!(*id, rig.head.notifications.id(session, Kind::Approval));
    assert_eq!(session_id, &session.0.to_string());
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
    let output = skywave_proto::BlobRef {
        id: blob,
        size: 40,
        sha256: skywave_proto::ContentHash::of(b""),
    };
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

fn notices(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Effect(PlatformEffect::Notice { text }) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A run whose output was deleted on the server copies the tail it kept,
/// and the notice says that is all it is, and why.
#[test]
fn copying_a_run_whose_output_was_deleted_says_so() {
    let mut rig = Rig::new();
    rig.switch_to_shell();
    let blob = skywave_proto::BlobId::new();
    let output = skywave_proto::BlobRef {
        id: blob,
        size: 4000,
        sha256: skywave_proto::ContentHash::of(b""),
    };
    let run = rig.finished_run("make test", "line 39\nline 40", 40, Some(output));
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 3, 0).unwrap();
    let me = rig.me;
    rig.log_shell(
        CommandId::new(),
        at,
        EventKind::BlobDeleted {
            blob,
            holds: skywave_core::BlobUse::ShellOutput,
            by: me,
        },
    );
    let outs = rig.copy_run(run);
    assert_eq!(copied(&outs), ["line 39\nline 40"]);
    assert!(rig.actions.is_empty(), "nothing to download");
    assert_eq!(
        notices(&outs),
        ["copied 15 B · only the kept tail · Deleted by you"]
    );

    // Positive control: a run with all of its output says only how much.
    let whole = rig.finished_run("ls", "a\nb", 2, None);
    assert_eq!(notices(&rig.copy_run(whole)), ["copied 3 B"]);
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

impl Rig {
    /// Logs a screen event as `log_shell` logs a shell one; `command_id` is
    /// the request's id where the event carries it.
    fn log_screen(
        &mut self,
        command_id: Option<CommandId>,
        at: chrono::DateTime<Utc>,
        kind: EventKind,
    ) -> Vec<Out> {
        let from = Seq(self.seam.cursor().unwrap().0 + 1);
        let event = Event {
            session_id: self.session,
            at,
            command_id,
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
}

/// The app screen updates among `outs`.
fn app_screens(outs: &[Out]) -> Vec<AppScreenUpdate> {
    outs.iter()
        .filter_map(|out| match out {
            Out::AppScreen(update) => Some(update.clone()),
            _ => None,
        })
        .collect()
}

/// The screen actions this head dispatched, in order.
fn screen_actions(rig: &Rig) -> Vec<ScreenAction> {
    rig.actions
        .iter()
        .filter_map(|action| match action {
            Action::Screen { action, .. } => Some(action.clone()),
            _ => None,
        })
        .collect()
}

/// This device's device listings still in the outbox.
fn pending_device_listings(rig: &Rig) -> usize {
    rig.seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.command.kind,
                CommandKind::Screen {
                    action: ScreenAction::Devices,
                    ..
                }
            )
        })
        .count()
}

#[test]
fn opening_the_app_screen_lists_the_devices_once() {
    let mut rig = Rig::new();
    let session = rig.session.0.to_string();
    let toggle = || GuiIntent::AppScreenToggle {
        session_id: session.clone(),
    };

    let outs = rig.intent(toggle());
    let updates = app_screens(&outs);
    assert_eq!(updates.len(), 1, "{outs:?}");
    assert!(updates[0].open);
    assert_eq!(updates[0].session_id.as_deref(), Some(session.as_str()));
    rig.results();
    rig.view();
    assert_eq!(pending_device_listings(&rig), 1);

    // Closed: the screen goes out closed, and nothing is asked for.
    let outs = rig.intent(toggle());
    let updates = app_screens(&outs);
    assert_eq!(updates.len(), 1, "{outs:?}");
    assert!(!updates[0].open);
    rig.results();
    rig.view();

    // Open again while the listing still waits: no second one.
    let outs = rig.intent(toggle());
    assert!(app_screens(&outs)[0].open);
    rig.results();
    rig.view();
    assert_eq!(pending_device_listings(&rig), 1);
    assert_eq!(screen_actions(&rig).len(), 1, "{:?}", rig.actions);
}

/// Logs a 1080x2400 capture of `emulator-5554`, asked for and done, and
/// returns its request's id with the outs of its result.
fn log_capture(rig: &mut Rig) -> (CommandId, Vec<Out>) {
    let me = rig.me;
    let capture = CommandId::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 2, 0).unwrap();
    rig.log_screen(
        Some(capture),
        at,
        EventKind::ScreenRequested {
            action: ScreenAction::Capture {
                device: "emulator-5554".into(),
                full: false,
            },
            by: me,
        },
    );
    let outs = rig.log_screen(
        None,
        at + chrono::Duration::seconds(1),
        EventKind::ScreenDone {
            request: capture,
            by: me,
            outcome: ScreenOutcome::Done {
                shot: ScreenShot::Image(ScreenImage {
                    image: skywave_proto::BlobRef {
                        id: skywave_proto::BlobId::new(),
                        size: 1,
                        sha256: ContentHash::of(b""),
                    },
                    format: CaptureFormat::Png,
                    width: 1080,
                    height: 2400,
                    screen_width: 1080,
                    screen_height: 2400,
                    pixels: ContentHash::of(b""),
                }),
            },
            pruned: Vec::new(),
        },
    );
    (capture, outs)
}

#[test]
fn a_tap_aims_at_the_latest_capture() {
    let mut rig = Rig::new();
    let session = rig.session.0.to_string();
    let (capture, outs) = log_capture(&mut rig);
    let update = app_screens(&outs).pop().unwrap();
    let shown = update.capture.as_ref().unwrap();
    assert_eq!(shown.capture_id, capture.0.to_string());
    assert_eq!(update.device.as_deref(), Some("emulator-5554"));

    let tap = GuiIntent::AppScreenTap {
        session_id: session.clone(),
        capture_id: capture.0.to_string(),
        x: 540,
        y: 1200,
    };
    let outs = rig.intent(tap);
    assert!(notices(&outs).is_empty(), "{outs:?}");
    assert_eq!(
        screen_actions(&rig),
        [ScreenAction::Tap {
            device: "emulator-5554".into(),
            aim: capture,
            x: 540,
            y: 1200,
        }]
    );

    // An aim at an older capture is refused here, and says so.
    let outs = rig.intent(GuiIntent::AppScreenTap {
        session_id: session,
        capture_id: CommandId::new().0.to_string(),
        x: 540,
        y: 1200,
    });
    assert_eq!(
        notices(&outs),
        ["the screen changed; aim again on the new capture".to_owned()]
    );
    assert_eq!(screen_actions(&rig).len(), 1);
}

#[test]
fn a_refused_screen_request_is_dismissed() {
    let mut rig = Rig::new();
    let session = rig.session.0.to_string();
    let (capture, _) = log_capture(&mut rig);
    rig.intent(GuiIntent::AppScreenTap {
        session_id: session.clone(),
        capture_id: capture.0.to_string(),
        x: 540,
        y: 1200,
    });
    rig.results();
    let tap = rig
        .seam
        .pending_outbox()
        .unwrap()
        .into_iter()
        .find(|entry| {
            matches!(
                entry.command.kind,
                CommandKind::Screen {
                    action: ScreenAction::Tap { .. },
                    ..
                }
            )
        })
        .unwrap()
        .command
        .id;

    // The screen moved on before the tap reached the server: refused.
    rig.seam.command_sending(tap).unwrap();
    rig.seam
        .command_result(
            tap,
            &CommandResult::Rejected {
                reason: RejectReason::ScreenStale,
            },
        )
        .unwrap();
    let outs = rig.view();
    let row = app_screens(&outs)
        .pop()
        .unwrap()
        .requests
        .into_iter()
        .find(|r| r.key == tap.0.to_string())
        .unwrap();
    assert!(row.refused && row.discardable, "{row:?}");

    rig.intent(GuiIntent::AppScreenDismiss {
        session_id: session,
        request_id: tap.0.to_string(),
    });
    rig.results();
    let outs = rig.view();
    let rows = app_screens(&outs).pop().unwrap().requests;
    assert!(rows.iter().all(|r| r.key != tap.0.to_string()), "{rows:?}");
}

#[test]
fn an_input_before_any_capture_asks_for_one() {
    let mut rig = Rig::new();
    let session = rig.session.0.to_string();
    let me = rig.me;
    let listing = CommandId::new();
    let at = Utc.with_ymd_and_hms(2026, 10, 31, 12, 1, 0).unwrap();
    rig.log_screen(
        Some(listing),
        at,
        EventKind::ScreenRequested {
            action: ScreenAction::Devices,
            by: me,
        },
    );
    rig.log_screen(
        None,
        at + chrono::Duration::seconds(1),
        EventKind::ScreenDone {
            request: listing,
            by: me,
            outcome: ScreenOutcome::Devices {
                devices: vec![ScreenDevice {
                    serial: "emulator-5554".into(),
                    state: "device".into(),
                    model: None,
                }],
            },
            pruned: Vec::new(),
        },
    );

    // The device is shown, with nothing captured yet.
    let outs = rig.intent(GuiIntent::AppScreenTap {
        session_id: session,
        capture_id: CommandId::new().0.to_string(),
        x: 1,
        y: 1,
    });
    assert_eq!(
        notices(&outs),
        ["capture the screen first".to_owned()],
        "{outs:?}"
    );
    assert!(screen_actions(&rig).is_empty(), "{:?}", rig.actions);
}

#[test]
fn the_app_screen_goes_out_only_when_it_changes() {
    let mut rig = Rig::new();
    let session = rig.session.0.to_string();
    let outs = rig.intent(GuiIntent::AppScreenToggle {
        session_id: session,
    });
    assert_eq!(app_screens(&outs).len(), 1, "{outs:?}");
    let outs = rig.intent(GuiIntent::FollowingChanged { following: false });
    assert!(app_screens(&outs).is_empty(), "{outs:?}");
}

mod workdir {
    use std::path::PathBuf;

    use skywave_core::workdir::PushOutcome;
    use skywave_core::{
        BlobId, BlobState, BlobView, ChangeSet, ChangedFile, ChangesResult, DiffBody, DirEntry,
        EntryKind, FileBody, FileChange, FileResult, LinkTarget, ListResult, PushState, Screen,
        WorkdirBody, WorkdirDownload, WorkdirFetch, WorkdirPane, WorkdirPush, WorkdirView,
    };
    use skywave_present::workdir::NOT_FETCHED;
    use skywave_proto::{
        BlobRef, Commit, CommitLog, LogResult, Seq, UploadResult, WriteFileResult,
    };

    use super::*;
    use crate::signals::{
        BlobStateKind, WorkdirChangeKind, WorkdirLine, WorkdirLineKind, WorkdirPaneKind,
        WorkdirScreen, WorkdirTextUpdate,
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
        shown_with(rig, pane, fetch, body, Vec::new(), Vec::new())
    }

    /// [`shown`] with the files asked for download, `downloads`, and the
    /// device files pushed into the workdir, `pushes`.
    fn shown_with(
        rig: &mut Rig,
        pane: WorkdirPane,
        fetch: WorkdirFetch,
        body: WorkdirBody,
        downloads: Vec<WorkdirDownload>,
        pushes: Vec<WorkdirPush>,
    ) -> WorkdirScreen {
        shown_outs(rig, pane, fetch, body, downloads, pushes).0
    }

    /// [`shown_with`], and the outs the view sent besides the screen's
    /// command: what Dart is sent for it.
    fn shown_outs(
        rig: &mut Rig,
        pane: WorkdirPane,
        fetch: WorkdirFetch,
        body: WorkdirBody,
        downloads: Vec<WorkdirDownload>,
        pushes: Vec<WorkdirPush>,
    ) -> (WorkdirScreen, Vec<Out>) {
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
            downloads,
            pushes,
        });
        let outs = rig.head.on_view(view, rig.clock);
        let outs = rig.apply(outs);
        let screen = match &rig.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Workdir { workdir } => workdir.clone(),
            other => panic!("not the workdir screen: {other:?}"),
        };
        (screen, outs)
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
            change: None,
            pending: None,
        }
    }

    fn line(kind: WorkdirLineKind, text: &str) -> WorkdirLine {
        WorkdirLine {
            kind,
            text: text.into(),
        }
    }

    /// The text updates among `outs`.
    fn workdir_text(outs: &[Out]) -> Vec<WorkdirTextUpdate> {
        outs.iter()
            .filter_map(|out| match out {
                Out::WorkdirText(update) => Some(update.clone()),
                _ => None,
            })
            .collect()
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
    fn the_workdir_commits_pane_lists_commits_newest_first() {
        let mut rig = Rig::new();
        let commits = vec![
            Commit {
                sha: "a1b2c3d4e5f6".into(),
                short: "a1b2c3d".into(),
                author: "Ada".into(),
                time: Some(taken() - chrono::Duration::hours(3)),
                subject: "Add the log".into(),
            },
            Commit {
                sha: "e4f5a6b7c8d9".into(),
                short: "e4f5a6b".into(),
                author: "Zoë".into(),
                time: None,
                subject: "First".into(),
            },
        ];
        let screen = shown(
            &mut rig,
            WorkdirPane::Commits,
            WorkdirFetch::Idle,
            WorkdirBody::Log(LogResult::Log(CommitLog {
                at: taken(),
                branch: Some("main".into()),
                commits,
                more: true,
            })),
        );
        assert_eq!(screen.pane, WorkdirPaneKind::Commits);
        assert_eq!(screen.header, "main · taken 12:00:00");
        assert_eq!(screen.wording, None);
        assert_eq!(screen.notes, ["Showing the newest 2 commits."]);
        let rows: Vec<_> = screen
            .commits
            .iter()
            .map(|r| {
                (
                    r.short.as_str(),
                    r.subject.as_str(),
                    r.author.as_str(),
                    r.time_label.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("a1b2c3d", "Add the log", "Ada", Some("4 h ago")),
                ("e4f5a6b", "First", "Zoë", None),
            ]
        );
        assert!(screen.changes.is_empty());
        assert!(screen.entries.is_empty());
        assert_eq!(screen.content, None);
    }

    #[test]
    fn the_workdir_commits_pane_words_an_empty_and_a_failed_log() {
        let mut rig = Rig::new();
        let screen = shown(
            &mut rig,
            WorkdirPane::Commits,
            WorkdirFetch::Idle,
            WorkdirBody::Log(LogResult::Log(CommitLog {
                at: taken(),
                branch: Some("main".into()),
                commits: Vec::new(),
                more: false,
            })),
        );
        assert_eq!(screen.wording.as_deref(), Some("No commits yet."));
        assert!(screen.commits.is_empty());
        let screen = shown(
            &mut rig,
            WorkdirPane::Commits,
            WorkdirFetch::Failed("offline".into()),
            WorkdirBody::Log(LogResult::Failed {
                reason: "boom".into(),
            }),
        );
        assert_eq!(screen.wording.as_deref(), Some("git failed: boom"));
        assert!(screen.commits.is_empty());
        let screen = shown(
            &mut rig,
            WorkdirPane::Commits,
            WorkdirFetch::Loading,
            WorkdirBody::Nothing,
        );
        assert_eq!(screen.header, "Commits");
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
                sha256: skywave_proto::ContentHash::of(b""),
            }),
        );
        let pane = WorkdirPane::Diff {
            path: "a.txt".into(),
            untracked: false,
        };
        let (screen, outs) = shown_outs(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            WorkdirBody::Diff {
                file: Some(changed.clone()),
                blob: Some(blob.clone()),
            },
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(screen.pane, WorkdirPaneKind::Diff);
        assert_eq!(screen.path, "a.txt");
        assert_eq!(screen.header, "M a.txt  +1 \u{2212}1");
        assert_eq!(screen.wording, None);
        let content = screen.content.unwrap();
        assert_eq!(content.blob.state, BlobStateKind::Available);
        assert_eq!(content.blob.blob_id, blob.id.0.to_string());
        assert_eq!(content.cut, None);
        let text = workdir_text(&outs);
        assert_eq!(text.len(), 1, "{outs:?}");
        assert_eq!(text[0].blob_id, content.blob.blob_id);
        assert_eq!(
            text[0].lines,
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

        // Before it is here: its download, and no text.
        let waiting = BlobView {
            id: blob.id,
            state: BlobState::Failed {
                size: 80,
                reason: "offline".into(),
            },
            change: None,
            pending: None,
        };
        let (screen, outs) = shown_outs(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            WorkdirBody::Diff {
                file: Some(changed),
                blob: Some(waiting),
            },
            Vec::new(),
            Vec::new(),
        );
        let content = screen.content.unwrap();
        assert!(content.blob.can_download);
        assert!(workdir_text(&outs).is_empty(), "{outs:?}");

        // A file gone from the changes says so, with no content.
        let (screen, outs) = shown_outs(
            &mut rig,
            pane,
            WorkdirFetch::Idle,
            WorkdirBody::Diff {
                file: None,
                blob: None,
            },
            Vec::new(),
            Vec::new(),
        );
        assert!(screen.wording.unwrap().contains("no longer in the changes"));
        assert_eq!(screen.content, None);
        assert!(workdir_text(&outs).is_empty(), "{outs:?}");
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
        let (screen, outs) = shown_outs(
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
                        sha256: skywave_proto::ContentHash::of(b""),
                    }),
                },
                blob: Some(blob.clone()),
            },
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(screen.header, "src/main.rs  25 B · taken 12:00:00");
        let content = screen.content.unwrap();
        let text = workdir_text(&outs);
        assert_eq!(text.len(), 1, "{outs:?}");
        assert_eq!(text[0].blob_id, content.blob.blob_id);
        assert_eq!(
            text[0].lines,
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
    fn the_workdir_text_is_sent_once_per_blob_not_with_each_screen() {
        let mut rig = Rig::new();
        let blob = available(&rig, "fn main() {}\n");
        let pane = WorkdirPane::File {
            path: "src/main.rs".into(),
        };
        let body = || WorkdirBody::File {
            result: FileResult::File {
                at: taken(),
                path: "src/main.rs".into(),
                size: 13,
                content: FileBody::Blob(BlobRef {
                    id: blob.id,
                    size: 13,
                    sha256: skywave_proto::ContentHash::of(b""),
                }),
            },
            blob: Some(blob.clone()),
        };
        let (_, outs) = shown_outs(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            body(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(workdir_text(&outs).len(), 1, "{outs:?}");

        // A download row changes the screen alone: no text again.
        let downloads = vec![WorkdirDownload {
            path: "a.log".into(),
            fetch: WorkdirFetch::Loading,
            result: None,
            blob: None,
        }];
        let (screen, outs) = shown_outs(
            &mut rig,
            pane.clone(),
            WorkdirFetch::Idle,
            body(),
            downloads,
            Vec::new(),
        );
        assert_eq!(screen.downloads.len(), 1);
        assert!(
            outs.iter().any(|out| matches!(out, Out::Screen(_))),
            "{outs:?}"
        );
        assert!(workdir_text(&outs).is_empty(), "{outs:?}");

        // Another screen in between: coming back to the blob sends its text.
        let away = rig.view();
        assert!(
            matches!(
                rig.head.sent.screen.as_ref().unwrap().screen,
                ScreenView::Session { .. }
            ),
            "{away:?}"
        );
        let (_, outs) = shown_outs(
            &mut rig,
            pane,
            WorkdirFetch::Idle,
            body(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(workdir_text(&outs).len(), 1, "{outs:?}");
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
                pane: WorkdirPaneKind::Commits,
                path: String::new(),
                untracked: false,
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
        rig.intent(GuiIntent::Act {
            action: ActionIntent::DownloadWorkdirFile {
                session_id: rig.session.0.to_string(),
                path: "bin/app".into(),
            },
        });
        rig.intent(GuiIntent::Act {
            action: ActionIntent::PushWorkdirFile {
                session_id: rig.session.0.to_string(),
                source: "/home/me/notes.txt".into(),
                path: "docs/notes.txt".into(),
                replace: None,
            },
        });
        rig.intent(GuiIntent::Act {
            action: ActionIntent::PushWorkdirFile {
                session_id: rig.session.0.to_string(),
                source: "/home/me/notes.txt".into(),
                path: "docs/notes.txt".into(),
                replace: Some(ContentHash::of(b"old").to_string()),
            },
        });
        let workdir: Vec<_> = rig
            .actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::OpenWorkdir { .. }
                        | Action::ShowWorkdir(_)
                        | Action::RefreshWorkdir
                        | Action::DownloadWorkdirFile { .. }
                        | Action::PushWorkdirFile { .. }
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
                Action::ShowWorkdir(WorkdirPane::Commits),
                Action::ShowWorkdir(WorkdirPane::Diff {
                    path: "a.txt".into(),
                    untracked: true
                }),
                Action::ShowWorkdir(WorkdirPane::Dir { path: "src".into() }),
                Action::RefreshWorkdir,
                Action::DownloadWorkdirFile {
                    session_id: rig.session,
                    path: "bin/app".into(),
                },
                Action::PushWorkdirFile {
                    session_id: rig.session,
                    source: "/home/me/notes.txt".into(),
                    path: "docs/notes.txt".into(),
                    replace: None,
                },
                Action::PushWorkdirFile {
                    session_id: rig.session,
                    source: "/home/me/notes.txt".into(),
                    path: "docs/notes.txt".into(),
                    replace: Some(ContentHash::of(b"old")),
                },
            ]
        );
    }

    #[test]
    fn the_workdir_screen_lists_each_download_with_its_state() {
        let mut rig = Rig::new();
        let ready = available(&rig, "x");
        let BlobState::Available { path } = &ready.state else {
            panic!("the blob is not available: {:?}", ready.state)
        };
        let ready_path = path.display().to_string();
        let downloading = BlobId::new();
        let sha256 = skywave_proto::ContentHash::of(b"");
        let downloads = vec![
            WorkdirDownload {
                path: "a.log".into(),
                fetch: WorkdirFetch::Loading,
                result: None,
                blob: None,
            },
            WorkdirDownload {
                path: "b.bin".into(),
                fetch: WorkdirFetch::Failed("timed out".into()),
                result: None,
                blob: None,
            },
            WorkdirDownload {
                path: "c.iso".into(),
                fetch: WorkdirFetch::Idle,
                result: Some(FileResult::File {
                    at: taken(),
                    path: "c.iso".into(),
                    size: 200 * 1024 * 1024,
                    content: FileBody::TooLarge,
                }),
                blob: None,
            },
            WorkdirDownload {
                path: "d.mp4".into(),
                fetch: WorkdirFetch::Idle,
                result: Some(FileResult::File {
                    at: taken(),
                    path: "d.mp4".into(),
                    size: 4096,
                    content: FileBody::Blob(BlobRef {
                        id: downloading,
                        size: 4096,
                        sha256,
                    }),
                }),
                blob: Some(BlobView {
                    id: downloading,
                    state: BlobState::Downloading {
                        got: 1024,
                        size: 4096,
                        paused: None,
                    },
                    change: None,
                    pending: None,
                }),
            },
            WorkdirDownload {
                path: "e.txt".into(),
                fetch: WorkdirFetch::Idle,
                result: Some(FileResult::File {
                    at: taken(),
                    path: "e.txt".into(),
                    size: 1,
                    content: FileBody::Blob(BlobRef {
                        id: ready.id,
                        size: 1,
                        sha256,
                    }),
                }),
                blob: Some(ready.clone()),
            },
        ];
        let screen = shown_with(
            &mut rig,
            WorkdirPane::Changes,
            WorkdirFetch::Idle,
            WorkdirBody::Nothing,
            downloads,
            Vec::new(),
        );
        let rows = &screen.downloads;
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            ["a.log", "b.bin", "c.iso", "d.mp4", "e.txt"]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            [
                "Asking the server\u{2026}",
                "Download failed: timed out",
                "Too large to download: 200.0 MiB",
                "downloading \u{b7} 1.0 KiB of 4.0 KiB (25%)",
                "downloaded",
            ]
        );
        let flags: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.failed,
                    row.blob.as_ref().map(|blob| blob.state),
                    row.local_path.clone(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                (false, None, None),
                (true, None, None),
                (true, None, None),
                (false, Some(BlobStateKind::Downloading), None),
                (false, Some(BlobStateKind::Available), Some(ready_path)),
            ]
        );
    }

    #[test]
    fn the_workdir_screen_lists_each_push_with_its_state() {
        let mut rig = Rig::new();
        let source = |path: &str| PathBuf::from(format!("/home/me/{path}"));
        let pushes = vec![
            WorkdirPush {
                path: "a.bin".into(),
                source: source("a.bin"),
                state: PushState::Pushing,
            },
            WorkdirPush {
                path: "b.bin".into(),
                source: source("b.bin"),
                state: PushState::Failed("timed out".into()),
            },
            WorkdirPush {
                path: "c.bin".into(),
                source: source("c.bin"),
                state: PushState::Done(PushOutcome::Written(WriteFileResult::Written {
                    path: "c.bin".into(),
                    size: 2048,
                    seq: Seq(7),
                })),
            },
            WorkdirPush {
                path: "d.bin".into(),
                source: source("d.bin"),
                state: PushState::Done(PushOutcome::Written(WriteFileResult::Same {
                    path: "d.bin".into(),
                    size: 1,
                })),
            },
            WorkdirPush {
                path: "e.bin".into(),
                source: source("e.bin"),
                state: PushState::Done(PushOutcome::Written(WriteFileResult::Exists {
                    sha256: ContentHash::of(b"old"),
                    size: 3,
                })),
            },
            WorkdirPush {
                path: "f.bin".into(),
                source: source("f.bin"),
                state: PushState::Done(PushOutcome::UploadRefused(UploadResult::TooLarge)),
            },
            WorkdirPush {
                path: "g.bin".into(),
                source: source("g.bin"),
                state: PushState::Done(PushOutcome::Unreadable {
                    reason: "gone".into(),
                }),
            },
        ];
        let screen = shown_with(
            &mut rig,
            WorkdirPane::Changes,
            WorkdirFetch::Idle,
            WorkdirBody::Nothing,
            Vec::new(),
            pushes,
        );
        let rows = &screen.pushes;
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            [
                "a.bin", "b.bin", "c.bin", "d.bin", "e.bin", "f.bin", "g.bin"
            ]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            [
                "Sending\u{2026}",
                "Push failed: timed out",
                "Written \u{b7} 2.0 KiB",
                "Already there",
                "A file is already there (3 B)",
                "Too large to send",
                "Cannot read the file: gone",
            ]
        );
        let flags: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.pushing,
                    row.done,
                    row.existing_sha256.clone(),
                    row.failed,
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                (true, false, None, false),
                (false, false, None, true),
                (false, true, None, false),
                (false, true, None, false),
                (
                    false,
                    false,
                    Some(ContentHash::of(b"old").to_string()),
                    false
                ),
                (false, false, None, true),
                (false, false, None, true),
            ]
        );
    }
}

mod blob_actions {
    use skywave_core::{BlobId, LineSpan};
    use skywave_present::blob_actions::crop_confirmation;

    use super::*;
    use crate::signals::{
        BlobTargetView, BlobUseView, CropContentKind, CropLine, CropLinesUpdate, CropPresetView,
        CropScreen, LineSpanView,
    };

    fn span(first: u32, count: u32) -> LineSpanView {
        LineSpanView { first, count }
    }

    fn core_span(first: u32, count: u32) -> LineSpan {
        LineSpan { first, count }
    }

    fn act(rig: &mut Rig, action: ActionIntent) -> Vec<Out> {
        let mut outs = rig.intent(GuiIntent::Act { action });
        outs.extend(rig.results());
        outs.extend(rig.view());
        outs
    }

    fn crop_screen(rig: &Rig) -> CropScreen {
        match &rig.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Crop { crop } => crop.clone(),
            other => panic!("not the crop page: {other:?}"),
        }
    }

    fn crop_lines(outs: &[Out]) -> Vec<CropLinesUpdate> {
        outs.iter()
            .filter_map(|out| match out {
                Out::CropLines(update) => Some(update.clone()),
                _ => None,
            })
            .collect()
    }

    /// A finished run of `make` by this device whose 60-line output is
    /// downloaded; its id and the blob's.
    fn downloaded_run(rig: &mut Rig) -> (CommandId, BlobId) {
        rig.switch_to_shell();
        let blob = skywave_proto::BlobId::new();
        let output = skywave_proto::BlobRef {
            id: blob,
            size: 400,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        let run = rig.finished_run("make", "line 59\nline 60", 60, Some(output));
        let mut text = String::from("\x1b[1mline 1\x1b[0m\nline\t2\n");
        for n in 3..=60 {
            text.push_str(&format!("line {n}\n"));
        }
        let path = rig._dir.path().join("make-output");
        std::fs::write(&path, text).unwrap();
        rig.seam
            .blob_status(blob, skywave_core::BlobStatus::Available { path })
            .unwrap();
        rig.view();
        (run, blob)
    }

    #[test]
    fn a_runs_output_offers_a_crop_and_a_delete_with_its_question() {
        let mut rig = Rig::new();
        let (_, blob) = downloaded_run(&mut rig);
        assert_eq!(
            rig.runs()[0].target,
            Some(BlobTargetView {
                blob_id: blob.0.to_string(),
                can_crop: true,
                delete_question: Some(
                    "Delete this shell output from the server and from every device? \
                     This can't be undone."
                        .into()
                ),
                pending_note: None,
                discard_id: None,
                session_id: rig.session.0.to_string(),
                holds: BlobUseView::ShellOutput,
                title: "make".into(),
            })
        );
        assert_eq!(rig.runs()[0].output_note, None);
    }

    #[test]
    fn the_crop_page_sends_its_lines_a_window_at_a_time_and_each_intent_its_action() {
        let mut rig = Rig::new();
        let requests = rig.seam.take_requests().unwrap();
        let (_, blob) = downloaded_run(&mut rig);
        let target = rig.runs()[0].target.clone().unwrap();
        rig.actions.clear();

        let outs = act(
            &mut rig,
            ActionIntent::OpenCrop {
                blob_id: target.blob_id.clone(),
                session_id: target.session_id.clone(),
                holds: target.holds,
                title: target.title.clone(),
            },
        );
        assert_eq!(
            rig.actions,
            [Action::OpenCrop {
                blob,
                session: rig.session,
                holds: skywave_core::BlobUse::ShellOutput,
                title: "make".into(),
            }]
        );
        let page = crop_screen(&rig);
        assert_eq!(page.content, CropContentKind::Counting);
        assert_eq!(page.message.as_deref(), Some("Counting its lines…"));
        assert!(crop_lines(&outs).is_empty(), "no window asked for");

        let req = requests
            .try_iter()
            .find_map(|r| match r {
                skywave_core::Request::CountLines { req, .. } => Some(req),
                _ => None,
            })
            .expect("the core asked for a count");
        rig.seam.lines_counted(req, Ok(60));
        rig.view();
        let page = crop_screen(&rig);
        assert_eq!(
            (page.content, page.lines, page.message),
            (CropContentKind::Ready, 60, None)
        );
        assert_eq!(
            page.presets,
            [
                CropPresetView {
                    label: "First 50".into(),
                    keep: vec![span(0, 50)],
                },
                CropPresetView {
                    label: "Last 50".into(),
                    keep: vec![span(10, 50)],
                },
            ]
        );
        assert_eq!(page.keeps_label.as_deref(), Some("keeps 0 of 60 lines"));
        assert!(!page.can_crop && page.confirmation.is_none());

        // A window: those lines alone, stripped and cleaned one by one.
        let outs = rig.intent(GuiIntent::CropLines { from: 0, count: 3 });
        let line = |index, text: &str, kept| CropLine {
            index,
            text: text.into(),
            kept,
        };
        assert_eq!(
            crop_lines(&outs),
            [CropLinesUpdate {
                blob_id: blob.0.to_string(),
                from: 0,
                lines: vec![
                    line(0, "line 1", false),
                    line(1, "line    2", false),
                    line(2, "line 3", false),
                ],
                error: None,
            }]
        );
        // Past the end, cut to the lines there are.
        let outs = rig.intent(GuiIntent::CropLines {
            from: 58,
            count: 10,
        });
        let window = &crop_lines(&outs)[0];
        assert_eq!((window.from, window.lines.len()), (58, 2));
        assert_eq!(window.lines[1], line(59, "line 60", false));
        rig.intent(GuiIntent::CropLines { from: 0, count: 3 });

        // A preset: its spans, normalised, and the window marks them.
        rig.actions.clear();
        let outs = act(
            &mut rig,
            ActionIntent::SetCropKeep {
                keep: vec![span(40, 10), span(0, 45)],
            },
        );
        assert_eq!(rig.actions, [Action::SetCropKeep(vec![core_span(0, 50)])]);
        let page = crop_screen(&rig);
        assert_eq!(page.keep, [span(0, 50)]);
        assert_eq!(page.keeps_label.as_deref(), Some("keeps 50 of 60 lines"));
        assert!(page.can_crop);
        assert_eq!(page.confirmation, Some(crop_confirmation(50, 60)));
        let marked: Vec<bool> = crop_lines(&outs)[0].lines.iter().map(|l| l.kept).collect();
        assert_eq!(marked, [true, true, true]);

        // Two taps before the core's view of the first: the second builds
        // on the first.
        rig.actions.clear();
        rig.intent(GuiIntent::CropToggle { first: 1, last: 1 });
        rig.intent(GuiIntent::CropToggle { first: 2, last: 2 });
        assert_eq!(
            rig.actions,
            [
                Action::SetCropKeep(vec![core_span(0, 1), core_span(2, 48)]),
                Action::SetCropKeep(vec![core_span(0, 1), core_span(3, 47)]),
            ]
        );
        rig.results();
        let outs = rig.view();
        let marked: Vec<bool> = crop_lines(&outs)[0].lines.iter().map(|l| l.kept).collect();
        assert_eq!(marked, [true, false, false]);
        // A range, in either order, all kept: taken out.
        rig.actions.clear();
        rig.intent(GuiIntent::CropToggle { first: 9, last: 3 });
        assert_eq!(
            rig.actions,
            [Action::SetCropKeep(vec![
                core_span(0, 1),
                core_span(10, 40)
            ])]
        );
        rig.results();
        rig.view();

        // The confirmed crop: its spans, and the page leaves.
        let keep = crop_screen(&rig).keep;
        rig.actions.clear();
        act(
            &mut rig,
            ActionIntent::CropBlob {
                blob_id: blob.0.to_string(),
                keep,
            },
        );
        assert_eq!(
            rig.actions,
            [Action::CropBlob {
                blob,
                keep: vec![core_span(0, 1), core_span(10, 40)],
            }]
        );
        assert!(
            matches!(
                rig.head.sent.screen.as_ref().unwrap().screen,
                ScreenView::Session { .. }
            ),
            "back on the session"
        );
        // The run now says its crop is on its way, and offers nothing.
        let target = rig.runs()[0].target.clone().unwrap();
        assert_eq!(target.pending_note.as_deref(), Some("Crop queued"));
        assert!(!target.can_crop && target.delete_question.is_none());
    }

    #[test]
    fn a_storage_blob_row_carries_its_target_and_a_session_row_none() {
        use crate::signals::StorageGroupingView;

        let mut rig = Rig::new();
        let requests = rig.seam.take_requests().unwrap();
        rig.online_on_the_list();
        act(
            &mut rig,
            ActionIntent::Navigate {
                route: RouteView::Storage,
            },
        );
        let req = requests
            .try_iter()
            .find_map(|r| match r {
                skywave_core::Request::Storage { req } => Some(req),
                _ => None,
            })
            .expect("the core asked for the report");
        let blob = skywave_proto::BlobId::new();
        rig.seam.storage_reported(
            req,
            Ok(skywave_proto::StorageReport {
                database: 0,
                reclaimable: 0,
                event_blobs: 0,
                workdir_blobs: 0,
                shell_running: 0,
                log_file: None,
                free: None,
                min_free: 0,
                sessions: vec![skywave_proto::SessionStorage {
                    session: rig.session,
                    events: 2,
                    event_bytes: 2048,
                    blobs: 1,
                    blob_bytes: 4096,
                }],
                blobs: vec![skywave_proto::BlobStorage {
                    id: blob,
                    size: 4096,
                    session: rig.session,
                    seq: Seq(7),
                    holds: skywave_proto::BlobUse::ToolInput,
                }],
                shell_runs: Vec::new(),
            }),
        );
        rig.view();
        let rows = |rig: &Rig| match &rig.head.sent.screen.as_ref().unwrap().screen {
            ScreenView::Storage { rows, .. } => rows.clone(),
            other => panic!("not the storage screen: {other:?}"),
        };
        assert_eq!(rows(&rig)[0].target, None, "a session row");

        rig.intent(GuiIntent::StorageShow {
            grouping: StorageGroupingView::Blobs,
        });
        rig.view();
        let target = rows(&rig)[0].target.clone().expect("a blob row's target");
        assert_eq!(
            target,
            BlobTargetView {
                blob_id: blob.0.to_string(),
                can_crop: false,
                delete_question: Some(
                    "Delete this tool input from the server and from every device? \
                     This can't be undone."
                        .into()
                ),
                pending_note: None,
                discard_id: None,
                session_id: rig.session.0.to_string(),
                holds: BlobUseView::ToolInput,
                title: format!("{} · tool input", rows(&rig)[0].label),
            }
        );
    }

    #[test]
    fn an_output_not_here_offers_its_download_and_no_lines() {
        let mut rig = Rig::new();
        rig.switch_to_shell();
        let blob = skywave_proto::BlobId::new();
        let output = skywave_proto::BlobRef {
            id: blob,
            size: 4096,
            sha256: skywave_proto::ContentHash::of(b""),
        };
        rig.finished_run("make", "a\nb", 90, Some(output));
        rig.intent(GuiIntent::Act {
            action: ActionIntent::SetBlobPolicy {
                policy: crate::signals::BlobPolicyChoice::OnDemand,
            },
        });
        let target = rig.runs()[0].target.clone().unwrap();
        act(
            &mut rig,
            ActionIntent::OpenCrop {
                blob_id: target.blob_id,
                session_id: target.session_id,
                holds: target.holds,
                title: target.title,
            },
        );
        let page = crop_screen(&rig);
        assert_eq!(page.content, CropContentKind::NotHere);
        assert_eq!(
            page.message.as_deref(),
            Some("This output isn't on this device. Download 4.0 KiB to choose lines.")
        );
        let download = page.download.expect("its download");
        assert!(download.can_download, "{download:?}");
        assert_eq!(download.blob_id, blob.0.to_string());
        assert!(page.presets.is_empty() && page.keeps_label.is_none() && !page.can_crop);
        let outs = rig.intent(GuiIntent::CropLines { from: 0, count: 10 });
        assert!(crop_lines(&outs).is_empty(), "nothing to read");
    }

    #[test]
    fn a_tool_outputs_delete_is_its_action_and_its_pending_delete_discards() {
        let mut rig = Rig::new();
        let (kinds, output) = an_edit();
        rig.log(kinds);
        let row = rig
            .rows()
            .iter()
            .find(|r| r.blob_target.is_some())
            .expect("the edit's output is a target")
            .clone();
        let target = row.blob_target.unwrap();
        assert_eq!(target.blob_id, output.0.to_string());
        assert_eq!(target.holds, BlobUseView::ToolOutput);
        assert!(target.can_crop);

        rig.actions.clear();
        act(
            &mut rig,
            ActionIntent::DeleteBlob {
                blob_id: target.blob_id.clone(),
            },
        );
        assert_eq!(rig.actions, [Action::DeleteBlob(output)]);
        let pending = rig
            .rows()
            .iter()
            .find_map(|r| r.blob_target.clone())
            .unwrap();
        assert_eq!(pending.pending_note.as_deref(), Some("Delete queued"));
        assert!(!pending.can_crop && pending.delete_question.is_none());
        let discard = pending.discard_id.expect("discardable while queued");

        rig.actions.clear();
        act(
            &mut rig,
            ActionIntent::Discard {
                command_id: discard.clone(),
            },
        );
        assert_eq!(
            rig.actions,
            [Action::Discard(intents::id("command", &discard).unwrap())]
        );
        let back = rig
            .rows()
            .iter()
            .find_map(|r| r.blob_target.clone())
            .unwrap();
        assert_eq!(back, target, "the offers return");

        // A malformed id is an error, not an action.
        rig.actions.clear();
        let outs = rig.intent(GuiIntent::Act {
            action: ActionIntent::DeleteBlob {
                blob_id: "nope".into(),
            },
        });
        assert_eq!(errors(&outs), ["not a blob id: \"nope\""]);
        assert!(rig.actions.is_empty());
    }
}
