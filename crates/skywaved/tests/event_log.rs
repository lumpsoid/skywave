//! Event log and command handling (T03), driven through the public API.

use std::path::{Path, PathBuf};
use std::time::Duration;

use skywave_proto::{
    AgentWork, ApprovalId, BackgroundTask, Command, CommandId, CommandKind, CommandResult,
    ContextId, ContextRef, Decision, DeviceId, Effort, Event, EventKind, RejectReason, RuleScope,
    RuleSuggestion, Schedule, ScheduleId, Seq, SessionId, SessionState, TaskId, TaskKind,
};
use skywaved::agent::{ResumeHandle, store_resume_handle};
use skywaved::event_log::{DEFAULT_BATCH_LIMIT, PendingApproval};
use skywaved::{Error, EventLog, Storage};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
    storage: Storage,
    log: EventLog,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("project")).unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let log = EventLog::open(&storage.db, vec![root.clone()]).unwrap();
        Fixture {
            tmp,
            storage,
            log,
            root,
        }
    }

    /// Another connection to the same database, as T06 and T10 each hold.
    fn connection(&self) -> Storage {
        Storage::open(self.tmp.path().join("data")).unwrap()
    }

    fn handle(&mut self, kind: CommandKind) -> CommandResult {
        let command = Command {
            id: CommandId::new(),
            kind,
        };
        self.log
            .handle(&mut self.storage.db, DeviceId::new(), &command)
            .unwrap()
    }

    fn create_session(&mut self) -> SessionId {
        let workdir = self.root.join("project").to_str().unwrap().to_owned();
        let seq = accepted(self.handle(CommandKind::CreateSession { workdir }));
        let batch = self.log.read(&self.storage.db, Seq(seq.0 - 1), 1).unwrap();
        assert!(matches!(
            batch.events[0].kind,
            EventKind::SessionCreated { .. }
        ));
        batch.events[0].session_id
    }

    /// Appends `kind` as the agent of the session's current context.
    fn agent(&mut self, session: SessionId, kind: EventKind) -> Seq {
        let context = self
            .log
            .session(&self.storage.db, session)
            .unwrap()
            .unwrap()
            .context;
        self.agent_in(session, context, kind)
    }

    fn agent_in(&mut self, session: SessionId, context: ContextId, kind: EventKind) -> Seq {
        self.log
            .append(&mut self.storage.db, session, context, kind)
            .unwrap()
    }

    fn start_turn(&mut self, session: SessionId) {
        self.agent(
            session,
            EventKind::SessionStateChanged(SessionState::AgentWorking),
        );
    }

    fn request_approval(&mut self, session: SessionId) -> ApprovalId {
        self.request_approval_offering(session, Vec::new())
    }

    fn request_approval_offering(
        &mut self,
        session: SessionId,
        suggestions: Vec<RuleSuggestion>,
    ) -> ApprovalId {
        let id = ApprovalId::new();
        self.agent(
            session,
            EventKind::ApprovalRequested {
                id,
                call: None,
                tool: "Bash".into(),
                summary: "ls".into(),
                input: None,
                suggestions,
            },
        );
        id
    }

    fn all_kinds(&self) -> Vec<EventKind> {
        self.all_events().into_iter().map(|e| e.kind).collect()
    }

    fn all_events(&self) -> Vec<Event> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
    }
}

/// Every test ends by checking that every row names a context of its own
/// session.
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let violations = self.storage.db.context_stamp_violations().unwrap();
            assert!(violations.is_empty(), "context stamps: {violations:#?}");
        }
    }
}

fn accepted(result: CommandResult) -> Seq {
    match result {
        CommandResult::Accepted { seq } => seq,
        other => panic!("expected Accepted, got {other:?}"),
    }
}

fn rejected(reason: RejectReason) -> CommandResult {
    CommandResult::Rejected { reason }
}

/// A `ClearContext` of `session` into a fresh context.
fn clear_kind(session: SessionId, name: &str) -> CommandKind {
    CommandKind::ClearContext {
        session_id: session,
        context: ContextId::new(),
        name: name.into(),
    }
}

fn move_kind(session: SessionId, context: ContextId, name: &str) -> CommandKind {
    CommandKind::MoveContext {
        session_id: session,
        context,
        name: name.into(),
    }
}

fn workdir(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

// --- seq and reads ---------------------------------------------------------

#[test]
fn seqs_start_at_one_and_reads_page_by_cursor() {
    let mut f = Fixture::new();
    assert_eq!(f.log.head(), Seq::ZERO);
    let session = f.create_session();
    assert_eq!(f.log.head(), Seq(1));
    for i in 0..4 {
        let seq = f.agent(
            session,
            EventKind::AgentMessage {
                text: i.to_string(),
            },
        );
        assert_eq!(seq, Seq(2 + i));
    }
    assert_eq!(f.log.head(), Seq(5));

    let page = f.log.read(&f.storage.db, Seq(1), 2).unwrap();
    assert_eq!(page.from, Seq(2));
    assert_eq!(page.last(), Some(Seq(3)));
    assert_eq!(
        page.events[1].kind,
        EventKind::AgentMessage { text: "1".into() }
    );
    let rest = f
        .log
        .read(&f.storage.db, page.last().unwrap(), 100)
        .unwrap();
    assert_eq!(rest.from, Seq(4));
    assert_eq!(rest.events.len(), 2);

    let empty = f
        .log
        .read(&f.storage.db, Seq(5), DEFAULT_BATCH_LIMIT)
        .unwrap();
    assert_eq!(empty.from, Seq(6));
    assert!(empty.events.is_empty());
    assert_eq!(empty.last(), None);
}

#[test]
fn events_round_trip_unchanged() {
    let mut f = Fixture::new();
    let session = f.create_session();
    f.agent(session, EventKind::AgentMessage { text: "hi".into() });
    let first = f.log.read(&f.storage.db, Seq::ZERO, 10).unwrap();
    let again = f.log.read(&f.connection().db, Seq::ZERO, 10).unwrap();
    assert_eq!(first, again);
    assert!(first.events.iter().all(|e| e.session_id == session));
}

#[test]
fn head_survives_reopen() {
    let mut f = Fixture::new();
    f.create_session();
    let reopened = EventLog::open(&f.connection().db, vec![]).unwrap();
    assert_eq!(reopened.head(), Seq(1));
}

// --- dedup -----------------------------------------------------------------

#[test]
fn resending_a_command_returns_the_stored_result_without_appending() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id: session,
            context: ContextId::first_of(session),
            text: "hello".into(),
        },
    };
    let first = f.log.handle(&mut f.storage.db, device, &command).unwrap();
    let seq = accepted(first.clone());
    // A resend, even over another connection, gets the same answer.
    let mut other = f.connection();
    for _ in 0..3 {
        assert_eq!(
            f.log.handle(&mut other.db, device, &command).unwrap(),
            first
        );
    }
    assert_eq!(f.log.head(), seq);
    let logged = f.all_events().pop().unwrap();
    assert_eq!(logged.command_id, Some(command.id));
    assert_eq!(
        logged.kind,
        EventKind::UserMessage {
            device_id: device,
            text: "hello".into(),
        }
    );
}

/// Every command logs exactly one event naming it, carrying the command's
/// id at the seq its `Accepted` names, so the log alone acknowledges it;
/// agent and daemon events carry none (the new session's `SessionCreated`
/// a move logs before its own event included), and a rejected command logs
/// nothing.
#[test]
fn each_accepted_command_logs_one_event_naming_it() {
    let mut f = Fixture::new();
    let device = DeviceId::new();
    let send = |f: &mut Fixture, kind: CommandKind| {
        let command = Command {
            id: CommandId::new(),
            kind,
        };
        let before = f.log.head();
        let result = f.log.handle(&mut f.storage.db, device, &command).unwrap();
        (command.id, result, before)
    };
    let workdir = workdir(&f.root.join("project"));
    let (id, result, before) = send(&mut f, CommandKind::CreateSession { workdir });
    let seq = accepted(result);
    assert_eq!(seq, before.next(), "one event, nothing else");
    let session = f.all_events()[(seq.0 - 1) as usize].session_id;
    let mut logged = vec![(id, seq)];

    f.start_turn(session);
    let approval = f.request_approval(session);
    let commands = [
        CommandKind::SendMessage {
            session_id: session,
            context: ContextId::first_of(session),
            text: "hi".into(),
        },
        CommandKind::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        },
        CommandKind::Interrupt {
            session_id: session,
        },
        CommandKind::StopSession {
            session_id: session,
        },
        CommandKind::RenameSession {
            session_id: session,
            name: Some("name".into()),
        },
        CommandKind::SetSessionArchived {
            session_id: session,
            archived: true,
        },
    ];
    for kind in commands {
        let (id, result, before) = send(&mut f, kind);
        let seq = accepted(result);
        assert_eq!(seq, before.next(), "one event, nothing else");
        logged.push((id, seq));
    }
    // A move: its first context had a turn and is no longer current.
    let first = ContextId::first_of(session);
    f.converse(first);
    let (id, result, before) = send(&mut f, clear_kind(session, "second"));
    let seq = accepted(result);
    assert_eq!(seq, before.next(), "one event, nothing else");
    logged.push((id, seq));
    let (id, result, before) = send(&mut f, move_kind(session, first, "moved"));
    let seq = accepted(result);
    assert_eq!(
        seq,
        before.next().next(),
        "the new session's SessionCreated, then the move's own event"
    );
    logged.push((id, seq));
    let head = f.log.head();
    let (_, refused, _) = send(
        &mut f,
        CommandKind::Interrupt {
            session_id: session,
        },
    );
    assert_eq!(refused, rejected(RejectReason::NoActiveTurn));
    assert_eq!(f.log.head(), head, "a rejected command logs nothing");

    let events = f.all_events();
    for (id, seq) in &logged {
        let event = &events[(seq.0 - 1) as usize];
        assert_eq!(event.command_id, Some(*id), "{event:?}");
        assert!(event.kind.logged_by_command(), "{event:?}");
    }
    let named: Vec<_> = events.iter().filter_map(|e| e.command_id).collect();
    assert_eq!(named.len(), logged.len(), "agent events name no command");
    assert!(
        events
            .iter()
            .filter(|e| e.command_id.is_none())
            .all(|e| !e.kind.logged_by_command())
    );
}

#[test]
fn rejected_results_are_stored_and_stay_stable() {
    let mut f = Fixture::new();
    let device = DeviceId::new();
    let missing = SessionId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::SendMessage {
            session_id: missing,
            context: ContextId::first_of(missing),
            text: "x".into(),
        },
    };
    let first = f.log.handle(&mut f.storage.db, device, &command).unwrap();
    assert_eq!(first, rejected(RejectReason::UnknownSession));
    assert_eq!(f.log.head(), Seq::ZERO);

    // Even once the reason no longer holds, the stored answer wins: here a
    // resend after the approval it targets exists.
    let session = f.create_session();
    let approval = f.request_approval(session);
    let resolve = Command {
        id: CommandId::new(),
        kind: CommandKind::ResolveApproval {
            id: approval,
            decision: Decision::Approve,
            rule: None,
        },
    };
    accepted(f.log.handle(&mut f.storage.db, device, &resolve).unwrap());
    let late = Command {
        id: CommandId::new(),
        kind: CommandKind::ResolveApproval {
            id: approval,
            decision: Decision::Deny,
            rule: None,
        },
    };
    let answer = f.log.handle(&mut f.storage.db, device, &late).unwrap();
    assert_eq!(answer, rejected(RejectReason::ApprovalAlreadyResolved));
    assert_eq!(
        f.log.handle(&mut f.storage.db, device, &late).unwrap(),
        answer
    );
    assert_eq!(
        f.log.handle(&mut f.storage.db, device, &command).unwrap(),
        first
    );
}

#[test]
fn concurrent_resends_of_one_command_append_once() {
    let f = Fixture::new();
    let mut setup = f.connection();
    let workdir = workdir(&f.root.join("project"));
    let create = Command {
        id: CommandId::new(),
        kind: CommandKind::CreateSession { workdir },
    };
    let device = DeviceId::new();
    let results: Vec<CommandResult> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (log, create) = (f.log.clone(), &create);
                let mut conn = f.connection();
                s.spawn(move || log.handle(&mut conn.db, device, create).unwrap())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(results.iter().all(|r| *r == results[0]));
    assert_eq!(accepted(results[0].clone()), Seq(1));
    assert_eq!(
        f.log.read(&setup.db, Seq::ZERO, 100).unwrap().events.len(),
        1
    );
    assert_eq!(f.log.sessions(&setup.db).unwrap().len(), 1);
    // The handle still works on a connection opened before the race.
    assert_eq!(
        f.log.handle(&mut setup.db, device, &create).unwrap(),
        results[0]
    );
}

// --- ordering across concurrent writers ------------------------------------

#[test]
fn concurrent_writers_get_contiguous_seqs_in_commit_order() {
    const WRITERS: usize = 6;
    const EACH: usize = 25;
    let mut f = Fixture::new();
    let session = f.create_session();

    std::thread::scope(|s| {
        for writer in 0..WRITERS {
            let log = f.log.clone();
            let mut conn = f.connection();
            s.spawn(move || {
                let device = DeviceId::new();
                let mut last = Seq::ZERO;
                for i in 0..EACH {
                    let text = format!("{writer}:{i}");
                    // Half the writers are clients, half act as the supervisor.
                    let seq = if writer % 2 == 0 {
                        let command = Command {
                            id: CommandId::new(),
                            kind: CommandKind::SendMessage {
                                session_id: session,
                                context: ContextId::first_of(session),
                                text,
                            },
                        };
                        accepted(log.handle(&mut conn.db, device, &command).unwrap())
                    } else {
                        let context = ContextId::first_of(session);
                        log.append(
                            &mut conn.db,
                            session,
                            context,
                            EventKind::AgentMessage { text },
                        )
                        .unwrap()
                    };
                    assert!(seq > last, "a writer's own events are ordered");
                    last = seq;
                }
            });
        }
    });

    let total = 1 + WRITERS * EACH;
    assert_eq!(f.log.head(), Seq(total as u64));
    let batch = f.log.read(&f.storage.db, Seq::ZERO, 5000).unwrap();
    assert_eq!(batch.from, Seq(1));
    assert_eq!(batch.events.len(), total, "no gaps, no duplicates");
    // Each writer's events appear in the order it wrote them.
    let mut next = [0usize; WRITERS];
    for event in &batch.events[1..] {
        let text = match &event.kind {
            EventKind::UserMessage { text, .. } | EventKind::AgentMessage { text } => text,
            other => panic!("unexpected {other:?}"),
        };
        let (writer, i) = text.split_once(':').unwrap();
        let (writer, i): (usize, usize) = (writer.parse().unwrap(), i.parse().unwrap());
        assert_eq!(i, next[writer]);
        next[writer] += 1;
    }
    assert_eq!(next, [EACH; WRITERS]);
}

// --- CreateSession and roots -----------------------------------------------

#[test]
fn create_session_records_the_canonical_workdir() {
    let mut f = Fixture::new();
    let dotted = f.root.join("project").join("..").join("project");
    let seq = accepted(f.handle(CommandKind::CreateSession {
        workdir: workdir(&dotted),
    }));
    let batch = f.log.read(&f.storage.db, Seq::ZERO, 10).unwrap();
    let canonical = workdir(&std::fs::canonicalize(f.root.join("project")).unwrap());
    let EventKind::SessionCreated {
        workdir: logged,
        context_name,
        from: None,
    } = &batch.events[0].kind
    else {
        panic!("not SessionCreated: {:?}", batch.events[0].kind);
    };
    assert_eq!(logged, &canonical);
    // The server names the first context.
    assert_eq!(
        skywave_proto::normalise_context_name(context_name).as_ref(),
        Some(context_name)
    );
    assert_eq!(batch.last(), Some(seq));
    let info = f
        .log
        .session(&f.storage.db, batch.events[0].session_id)
        .unwrap()
        .unwrap();
    assert_eq!(info.workdir, canonical);
    assert!(!info.turn_active);
    // The root itself is allowed too.
    accepted(f.handle(CommandKind::CreateSession {
        workdir: workdir(&f.root),
    }));
}

#[test]
fn create_session_rejects_workdirs_outside_roots() {
    let mut f = Fixture::new();
    let outside = f.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    // A sibling whose name extends the root's is not under it.
    let sibling = f.tmp.path().join("root2");
    std::fs::create_dir(&sibling).unwrap();
    let file = f.root.join("file.txt");
    std::fs::write(&file, "x").unwrap();

    let cases = [
        (workdir(&outside), RejectReason::WorkdirNotAllowed),
        (workdir(&sibling), RejectReason::WorkdirNotAllowed),
        (
            workdir(&f.root.join("..").join("outside")),
            RejectReason::WorkdirNotAllowed,
        ),
        ("root/project".to_owned(), RejectReason::WorkdirNotAllowed),
        (
            workdir(&f.root.join("missing")),
            RejectReason::WorkdirNotFound,
        ),
        (workdir(&file), RejectReason::WorkdirNotFound),
    ];
    for (dir, reason) in cases {
        assert_eq!(
            f.handle(CommandKind::CreateSession {
                workdir: dir.clone()
            }),
            rejected(reason),
            "{dir}"
        );
    }
    assert_eq!(f.log.head(), Seq::ZERO);
    assert!(f.log.sessions(&f.storage.db).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn create_session_follows_symlinks_before_checking_roots() {
    let mut f = Fixture::new();
    let outside = f.tmp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("escape")).unwrap();
    assert_eq!(
        f.handle(CommandKind::CreateSession {
            workdir: workdir(&f.root.join("escape"))
        }),
        rejected(RejectReason::WorkdirNotAllowed)
    );

    // A link from outside into a root is fine: the target is what counts.
    let link_in = f.tmp.path().join("link-in");
    std::os::unix::fs::symlink(f.root.join("project"), &link_in).unwrap();
    accepted(f.handle(CommandKind::CreateSession {
        workdir: workdir(&link_in),
    }));
}

#[test]
fn no_roots_allow_nothing() {
    let mut f = Fixture::new();
    f.log = EventLog::open(&f.storage.db, vec![]).unwrap();
    assert_eq!(
        f.handle(CommandKind::CreateSession {
            workdir: workdir(&f.root.join("project"))
        }),
        rejected(RejectReason::WorkdirNotAllowed)
    );
}

// --- SendMessage, ResolveApproval, Interrupt -------------------------------

#[test]
fn approvals_first_answer_wins_and_cancelled_ones_refuse_answers() {
    let mut f = Fixture::new();
    let session = f.create_session();
    assert_eq!(
        f.handle(CommandKind::ResolveApproval {
            id: ApprovalId::new(),
            decision: Decision::Approve,
            rule: None,
        }),
        rejected(RejectReason::UnknownApproval)
    );

    let approval = f.request_approval(session);
    assert_eq!(
        f.log.pending_approvals(&f.storage.db).unwrap(),
        [PendingApproval {
            id: approval,
            session_id: session
        }]
    );
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::ResolveApproval {
            id: approval,
            decision: Decision::Deny,
            rule: None,
        },
    };
    let seq = accepted(f.log.handle(&mut f.storage.db, device, &command).unwrap());
    let event = &f.log.read(&f.storage.db, Seq(seq.0 - 1), 1).unwrap().events[0];
    assert_eq!(event.session_id, session);
    assert_eq!(
        event.kind,
        EventKind::ApprovalResolved {
            id: approval,
            decision: Decision::Deny,
            by: device,
            rule: None,
        }
    );
    assert!(f.log.pending_approvals(&f.storage.db).unwrap().is_empty());
    assert_eq!(
        f.log
            .cancel_approval(&mut f.storage.db, approval, "late")
            .unwrap(),
        None
    );

    let cancelled = f.request_approval(session);
    let seq = f
        .log
        .cancel_approval(&mut f.storage.db, cancelled, "daemon restarted")
        .unwrap()
        .unwrap();
    assert_eq!(seq, f.log.head());
    assert_eq!(
        f.handle(CommandKind::ResolveApproval {
            id: cancelled,
            decision: Decision::Approve,
            rule: None,
        }),
        rejected(RejectReason::ApprovalCancelled)
    );
}

#[test]
fn an_approval_grants_an_offered_rule_and_the_log_echoes_it() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let offered = vec![
        RuleSuggestion {
            label: "Bash(ls)".into(),
            scope: RuleScope::Project,
        },
        RuleSuggestion {
            label: "all edits".into(),
            scope: RuleScope::Session,
        },
    ];
    let approval = f.request_approval_offering(session, offered);
    let resolve = |decision, rule| CommandKind::ResolveApproval {
        id: approval,
        decision,
        rule,
    };
    // A denial grants nothing; an index past the offer names nothing.
    for (decision, rule) in [(Decision::Deny, Some(0)), (Decision::Approve, Some(2))] {
        assert_eq!(
            f.handle(resolve(decision, rule)),
            rejected(RejectReason::UnknownRule),
            "{decision:?} {rule:?}"
        );
    }
    assert_eq!(f.log.pending_approvals(&f.storage.db).unwrap().len(), 1);

    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: resolve(Decision::Approve, Some(1)),
    };
    let seq = accepted(f.log.handle(&mut f.storage.db, device, &command).unwrap());
    let event = &f.log.read(&f.storage.db, Seq(seq.0 - 1), 1).unwrap().events[0];
    assert_eq!(
        event.kind,
        EventKind::ApprovalResolved {
            id: approval,
            decision: Decision::Approve,
            by: device,
            rule: Some(1),
        }
    );

    // A request offering nothing takes no rule.
    let bare = f.request_approval(session);
    assert_eq!(
        f.handle(CommandKind::ResolveApproval {
            id: bare,
            decision: Decision::Approve,
            rule: Some(0),
        }),
        rejected(RejectReason::UnknownRule)
    );
}

#[test]
fn interrupt_needs_an_active_turn_and_ends_it() {
    let mut f = Fixture::new();
    assert_eq!(
        f.handle(CommandKind::Interrupt {
            session_id: SessionId::new()
        }),
        rejected(RejectReason::UnknownSession)
    );
    let session = f.create_session();
    assert_eq!(
        f.handle(CommandKind::Interrupt {
            session_id: session
        }),
        rejected(RejectReason::NoActiveTurn)
    );

    f.start_turn(session);
    assert!(
        f.log
            .session(&f.storage.db, session)
            .unwrap()
            .unwrap()
            .turn_active
    );
    let seq = accepted(f.handle(CommandKind::Interrupt {
        session_id: session,
    }));
    assert_eq!(
        f.all_kinds()[seq.0 as usize - 1],
        EventKind::TurnInterrupted
    );
    assert!(
        !f.log
            .session(&f.storage.db, session)
            .unwrap()
            .unwrap()
            .turn_active
    );
    // The turn is already interrupted, so a second interrupt has nothing to stop.
    assert_eq!(
        f.handle(CommandKind::Interrupt {
            session_id: session
        }),
        rejected(RejectReason::NoActiveTurn)
    );

    // Idle and Errored end a turn too.
    for end in [
        SessionState::Idle,
        SessionState::Errored {
            message: "boom".into(),
        },
    ] {
        f.start_turn(session);
        f.agent(session, EventKind::SessionStateChanged(end));
        assert_eq!(
            f.handle(CommandKind::Interrupt {
                session_id: session
            }),
            rejected(RejectReason::NoActiveTurn)
        );
    }
}

// --- RenameSession -----------------------------------------------------------

#[test]
fn rename_normalises_the_name_and_records_who() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let device = DeviceId::new();
    let long = "é".repeat(100);
    let cases = [
        (
            Some("  fix\nthe\ttests \u{7}  "),
            Some("fixthetests".to_owned()),
        ),
        (Some(long.as_str()), Some("é".repeat(80))),
        (Some(" \n\t "), None),
        (Some(""), None),
        (None, None),
    ];
    for (name, expected) in cases {
        let command = Command {
            id: CommandId::new(),
            kind: CommandKind::RenameSession {
                session_id: session,
                name: name.map(str::to_owned),
            },
        };
        accepted(f.log.handle(&mut f.storage.db, device, &command).unwrap());
        assert_eq!(
            f.all_kinds().last().unwrap(),
            &EventKind::SessionRenamed {
                name: expected,
                by: device
            },
            "{name:?}"
        );
    }
    // A cut that lands after a space leaves no trailing whitespace.
    let spaced = format!("{} tail", "a".repeat(79));
    accepted(f.handle(CommandKind::RenameSession {
        session_id: session,
        name: Some(spaced),
    }));
    assert!(matches!(
        f.all_kinds().last().unwrap(),
        EventKind::SessionRenamed { name: Some(name), .. } if *name == "a".repeat(79)
    ));
}

#[test]
fn rename_needs_a_known_session_and_dedups_resends() {
    let mut f = Fixture::new();
    assert_eq!(
        f.handle(CommandKind::RenameSession {
            session_id: SessionId::new(),
            name: Some("x".into()),
        }),
        rejected(RejectReason::UnknownSession)
    );
    let session = f.create_session();
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::RenameSession {
            session_id: session,
            name: Some("x".into()),
        },
    };
    let first = f.log.handle(&mut f.storage.db, device, &command).unwrap();
    let seq = accepted(first.clone());
    assert_eq!(
        f.log.handle(&mut f.storage.db, device, &command).unwrap(),
        first
    );
    assert_eq!(f.log.head(), seq, "a resend appends nothing");
    // The same name again is a new command: harmless, still appended.
    let again = accepted(f.handle(CommandKind::RenameSession {
        session_id: session,
        name: Some("x".into()),
    }));
    assert_eq!(again, seq.next());
}

// --- SetSessionArchived -----------------------------------------------------

/// The archived flag of the fixture's one session, folded from the log as
/// clients fold it.
fn archived(f: &Fixture) -> bool {
    f.all_kinds().iter().fold(false, |archived, kind| {
        kind.archived_effect().unwrap_or(archived)
    })
}

#[test]
fn archive_logs_the_absolute_value_and_a_message_brings_it_back() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let device = DeviceId::new();
    let set = |archived| Command {
        id: CommandId::new(),
        kind: CommandKind::SetSessionArchived {
            session_id: session,
            archived,
        },
    };
    // Twice the same value: both logged, neither flips it back.
    for _ in 0..2 {
        accepted(f.log.handle(&mut f.storage.db, device, &set(true)).unwrap());
        assert_eq!(
            f.all_kinds().last().unwrap(),
            &EventKind::SessionArchived {
                archived: true,
                by: device
            }
        );
        assert!(archived(&f));
    }
    // A message from any device brings it back, by the shared rule.
    accepted(f.handle(CommandKind::SendMessage {
        session_id: session,
        context: ContextId::first_of(session),
        text: "again".into(),
    }));
    assert!(!archived(&f));
    accepted(f.log.handle(&mut f.storage.db, device, &set(true)).unwrap());
    accepted(
        f.log
            .handle(&mut f.storage.db, device, &set(false))
            .unwrap(),
    );
    assert!(!archived(&f));
}

#[test]
fn archive_needs_a_known_session_and_dedups_resends() {
    let mut f = Fixture::new();
    assert_eq!(
        f.handle(CommandKind::SetSessionArchived {
            session_id: SessionId::new(),
            archived: true,
        }),
        rejected(RejectReason::UnknownSession)
    );
    let session = f.create_session();
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::SetSessionArchived {
            session_id: session,
            archived: true,
        },
    };
    let first = f.log.handle(&mut f.storage.db, device, &command).unwrap();
    let seq = accepted(first.clone());
    assert_eq!(
        f.log.handle(&mut f.storage.db, device, &command).unwrap(),
        first
    );
    assert_eq!(f.log.head(), seq, "a resend appends nothing");
}

// --- SetEffort ---------------------------------------------------------------

#[test]
fn set_effort_needs_a_known_session() {
    let mut f = Fixture::new();
    f.create_session();
    let head = f.log.head();
    assert_eq!(
        f.handle(CommandKind::SetEffort {
            session_id: SessionId::new(),
            effort: Effort::High,
        }),
        rejected(RejectReason::UnknownSession)
    );
    assert_eq!(f.log.head(), head, "a rejected command logs nothing");
}

#[test]
fn set_effort_logs_one_request_the_agent_answers() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::SetEffort {
            session_id: session,
            effort: Effort::XHigh,
        },
    };
    let before = f.log.head();
    let seq = accepted(f.log.handle(&mut f.storage.db, device, &command).unwrap());
    assert_eq!(seq, before.next(), "one event, nothing else");
    let event = f.all_events().pop().unwrap();
    assert_eq!(event.command_id, Some(command.id));
    assert_eq!(
        event.kind,
        EventKind::EffortRequested {
            effort: Effort::XHigh,
            by: device,
        }
    );
    let settings = f.log.agent_settings(&f.storage.db, session).unwrap();
    let open = settings.effort.requested_effort().unwrap();
    assert_eq!((open.value, open.command), (Effort::XHigh, command.id));

    // The agent's answer is an agent event, and settles the request.
    f.agent(
        session,
        EventKind::EffortChanged {
            effort: Some(Effort::XHigh),
            answers: Some(command.id),
        },
    );
    let settings = f.log.agent_settings(&f.storage.db, session).unwrap();
    assert_eq!(settings.effort.requested, None);
    assert_eq!(settings.effort.current, Some(Some(Effort::XHigh)));
}

#[test]
fn background_work_folds_each_set_on_its_own() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let other = f.create_session();
    let work = |f: &Fixture, id| f.log.agent_work(&f.storage.db, id).unwrap();
    assert_eq!(work(&f, session), AgentWork::default());

    let tasks = vec![
        BackgroundTask {
            id: TaskId("a1".into()),
            kind: TaskKind::Subagent,
            title: "Explore the repo".into(),
            parent: None,
            ambient: false,
        },
        BackgroundTask {
            id: TaskId("b2".into()),
            kind: TaskKind::Shell,
            title: "cargo test".into(),
            parent: Some(TaskId("a1".into())),
            ambient: true,
        },
    ];
    let schedules = vec![Schedule {
        id: ScheduleId("s1".into()),
        when: "*/5 * * * *".into(),
        recurring: true,
        prompt: "check the deploy".into(),
        outlives_agent: Some(false),
    }];
    f.agent(
        session,
        EventKind::BackgroundTasksChanged {
            tasks: tasks.clone(),
        },
    );
    f.agent(
        session,
        EventKind::SchedulesChanged {
            schedules: schedules.clone(),
        },
    );
    assert_eq!(
        work(&f, session),
        AgentWork {
            tasks,
            schedules: schedules.clone(),
        }
    );
    assert_eq!(work(&f, other), AgentWork::default(), "per session");

    // A new set replaces the old one whole and leaves the other set alone.
    f.agent(session, EventKind::BackgroundTasksChanged { tasks: vec![] });
    assert_eq!(
        work(&f, session),
        AgentWork {
            tasks: vec![],
            schedules,
        }
    );
    // Another connection reads the same fold.
    let other_db = f.connection();
    assert_eq!(
        f.log.agent_work(&other_db.db, session).unwrap(),
        work(&f, session)
    );
}

#[test]
fn agent_appends_are_checked() {
    let mut f = Fixture::new();
    let nobody = SessionId::new();
    let unknown = f.log.append(
        &mut f.storage.db,
        nobody,
        ContextId::first_of(nobody),
        EventKind::AgentMessage { text: "x".into() },
    );
    assert!(matches!(unknown, Err(Error::InvalidAppend(_))));

    let session = f.create_session();
    let context = ContextId::first_of(session);
    // A context that is not the session's: unknown, or another session's.
    let other = f.create_session();
    for wrong in [ContextId::new(), ContextId::first_of(other)] {
        let appended = f.log.append(
            &mut f.storage.db,
            session,
            wrong,
            EventKind::AgentMessage { text: "x".into() },
        );
        assert!(matches!(appended, Err(Error::InvalidAppend(_))));
    }
    for kind in [
        EventKind::TurnInterrupted,
        EventKind::SessionCreated {
            workdir: "/".into(),
            context_name: "start".into(),
            from: None,
        },
        EventKind::ContextStarted {
            by: DeviceId::new(),
            name: "x".into(),
        },
        EventKind::ContextSwitched {
            by: DeviceId::new(),
        },
        EventKind::ContextMoved {
            to: other,
            by: DeviceId::new(),
        },
        EventKind::SessionCreated {
            workdir: "/".into(),
            context_name: "moved".into(),
            from: Some(ContextRef { session, context }),
        },
        EventKind::ApprovalCancelled {
            id: ApprovalId::new(),
            reason: "x".into(),
        },
        EventKind::SessionRenamed {
            name: Some("x".into()),
            by: DeviceId::new(),
        },
        EventKind::SessionArchived {
            archived: true,
            by: DeviceId::new(),
        },
        EventKind::EffortRequested {
            effort: Effort::High,
            by: DeviceId::new(),
        },
    ] {
        assert!(matches!(
            f.log.append(&mut f.storage.db, session, context, kind),
            Err(Error::InvalidAppend(_))
        ));
    }
    let approval = f.request_approval(session);
    let duplicate = f.log.append(
        &mut f.storage.db,
        session,
        context,
        EventKind::ApprovalRequested {
            id: approval,
            call: None,
            tool: "Bash".into(),
            summary: "again".into(),
            input: None,
            suggestions: Vec::new(),
        },
    );
    assert!(matches!(duplicate, Err(Error::InvalidAppend(_))));
    assert_eq!(f.log.head(), Seq(3), "refused appends leave no trace");
    // Positive control: the session's own context is accepted.
    f.agent_in(
        session,
        context,
        EventKind::AgentMessage { text: "x".into() },
    );
    assert_eq!(f.log.head(), Seq(4));
}

// --- contexts ----------------------------------------------------------------

impl Fixture {
    /// The session's newest event.
    fn last_event(&self) -> Event {
        self.all_events().pop().unwrap()
    }

    fn current_context(&self, session: SessionId) -> ContextId {
        self.log
            .session(&self.storage.db, session)
            .unwrap()
            .unwrap()
            .context
    }

    fn clear(&mut self, session: SessionId, context: ContextId, name: &str) -> CommandResult {
        self.handle(CommandKind::ClearContext {
            session_id: session,
            context,
            name: name.into(),
        })
    }

    /// Gives `context` a conversation: stores a resume handle for it, as
    /// the agent does before its first turn is logged.
    fn converse(&self, context: ContextId) {
        let handle = ResumeHandle(serde_json::json!({ "session": context.0.to_string() }));
        store_resume_handle(&self.storage.db, context, &handle).unwrap();
    }

    /// `contexts.origin` and `contexts.moved_to` of `context`.
    fn moves(&self, context: ContextId) -> (Option<String>, Option<String>) {
        self.storage
            .db
            .conn()
            .query_row(
                "SELECT origin, moved_to FROM contexts WHERE id = ?1",
                [context.0.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }
}

#[test]
fn a_new_session_opens_its_first_context_with_its_own_id() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let first = ContextId::first_of(session);
    assert_eq!(first.0, session.0);
    assert_eq!(f.current_context(session), first);
    let created = f.last_event();
    assert!(matches!(created.kind, EventKind::SessionCreated { .. }));
    assert_eq!(created.context, first);
    // Its own name, from the shared word lists.
    let EventKind::SessionCreated { context_name, .. } = created.kind else {
        unreachable!()
    };
    let (adjective, noun) = context_name.split_once(' ').unwrap();
    assert!(skywave_proto::context_name::ADJECTIVES.contains(&adjective));
    assert!(skywave_proto::context_name::NOUNS.contains(&noun));
}

#[test]
fn clear_context_starts_the_given_context_and_makes_it_current() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let fresh = ContextId::new();
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: CommandKind::ClearContext {
            session_id: session,
            context: fresh,
            name: "  fresh start ".into(),
        },
    };
    let seq = accepted(f.log.handle(&mut f.storage.db, device, &command).unwrap());
    let started = f.last_event();
    assert_eq!(
        started.kind,
        EventKind::ContextStarted {
            by: device,
            name: "fresh start".into()
        }
    );
    assert_eq!(started.context, fresh);
    assert_eq!(started.command_id, Some(command.id));
    assert_eq!(f.current_context(session), fresh);

    // A resend is the same command: same seq, nothing logged.
    let head = f.log.head();
    let again = f.log.handle(&mut f.storage.db, device, &command).unwrap();
    assert_eq!(again, CommandResult::Accepted { seq });
    assert_eq!(f.log.head(), head);

    // Another command naming an existing context is refused, whichever
    // session it belongs to, and logs nothing.
    let other = f.create_session();
    let head = f.log.head();
    assert_eq!(
        f.clear(session, fresh, "x"),
        rejected(RejectReason::ContextExists)
    );
    assert_eq!(
        f.clear(other, fresh, "x"),
        rejected(RejectReason::ContextExists)
    );
    assert_eq!(
        f.clear(session, ContextId::first_of(other), "x"),
        rejected(RejectReason::ContextExists)
    );
    assert_eq!(
        f.clear(SessionId::new(), ContextId::new(), "x"),
        rejected(RejectReason::UnknownSession)
    );
    assert_eq!(f.log.head(), head);
    assert_eq!(f.current_context(other), ContextId::first_of(other));
}

#[test]
fn a_blank_context_name_is_replaced_and_a_long_one_cut() {
    let mut f = Fixture::new();
    let session = f.create_session();
    accepted(f.clear(session, ContextId::new(), " \n\t "));
    let EventKind::ContextStarted { name, .. } = f.last_event().kind else {
        panic!("not ContextStarted");
    };
    let (adjective, noun) = name.split_once(' ').expect("a generated name");
    assert!(skywave_proto::context_name::ADJECTIVES.contains(&adjective));
    assert!(skywave_proto::context_name::NOUNS.contains(&noun));

    accepted(f.clear(session, ContextId::new(), &"long ".repeat(30)));
    let EventKind::ContextStarted { name, .. } = f.last_event().kind else {
        panic!("not ContextStarted");
    };
    assert!(name.starts_with("long long"), "{name:?}");
    assert!(name.chars().count() <= skywave_proto::limits::CONTEXT_NAME_MAX_CHARS);
    assert!(!name.ends_with(' '));
}

#[test]
fn switch_context_accepts_only_a_context_of_the_session() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let other = f.create_session();
    let first = ContextId::first_of(session);
    let second = ContextId::new();
    accepted(f.clear(session, second, "second"));
    let switch = |context| CommandKind::SwitchContext {
        session_id: session,
        context,
    };

    let head = f.log.head();
    assert_eq!(
        f.handle(switch(ContextId::new())),
        rejected(RejectReason::UnknownContext)
    );
    assert_eq!(
        f.handle(switch(ContextId::first_of(other))),
        rejected(RejectReason::UnknownContext)
    );
    assert_eq!(
        f.handle(CommandKind::SwitchContext {
            session_id: SessionId::new(),
            context: first,
        }),
        rejected(RejectReason::UnknownSession)
    );
    assert_eq!(f.log.head(), head);

    // Switching to the current context is accepted and logged once.
    let seq = accepted(f.handle(switch(second)));
    assert_eq!(seq, head.next());
    assert_eq!(f.log.head(), seq);
    let switched = f.last_event();
    assert!(matches!(switched.kind, EventKind::ContextSwitched { .. }));
    assert_eq!(switched.context, second);
    assert_eq!(f.current_context(session), second);

    // Back to the first.
    accepted(f.handle(switch(first)));
    assert_eq!(f.last_event().context, first);
    assert_eq!(f.current_context(session), first);
}

#[test]
fn a_message_for_a_context_left_behind_is_refused() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let first = ContextId::first_of(session);
    let second = ContextId::new();
    let send = |context| CommandKind::SendMessage {
        session_id: session,
        context,
        text: "hi".into(),
    };
    accepted(f.handle(send(first)));
    assert_eq!(f.last_event().context, first);

    accepted(f.clear(session, second, "second"));
    let head = f.log.head();
    assert_eq!(
        f.handle(send(first)),
        rejected(RejectReason::ContextChanged)
    );
    assert_eq!(f.log.head(), head);
    // Unknown session is checked first.
    assert_eq!(
        f.handle(CommandKind::SendMessage {
            session_id: SessionId::new(),
            context: second,
            text: "x".into(),
        }),
        rejected(RejectReason::UnknownSession)
    );

    accepted(f.handle(send(second)));
    let message = f.last_event();
    assert!(matches!(message.kind, EventKind::UserMessage { .. }));
    assert_eq!(message.context, second);
}

#[test]
fn session_wide_events_are_stamped_with_the_current_context() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let second = ContextId::new();
    accepted(f.clear(session, second, "second"));
    for kind in [
        CommandKind::RenameSession {
            session_id: session,
            name: Some("x".into()),
        },
        CommandKind::SetSessionArchived {
            session_id: session,
            archived: true,
        },
        CommandKind::SetEffort {
            session_id: session,
            effort: Effort::High,
        },
        CommandKind::StopSession {
            session_id: session,
        },
    ] {
        accepted(f.handle(kind));
        assert_eq!(f.last_event().context, second);
    }
    // What the agent logs carries the context it is given, the context of
    // the process that produced it, current or not.
    let first = ContextId::first_of(session);
    f.agent_in(session, first, EventKind::AgentMessage { text: "x".into() });
    assert_eq!(f.last_event().context, first);
    f.agent_in(
        session,
        second,
        EventKind::AgentMessage { text: "y".into() },
    );
    assert_eq!(f.last_event().context, second);
}

#[test]
fn an_answer_is_stamped_with_the_context_its_approval_was_asked_in() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let first = ContextId::first_of(session);
    f.start_turn(session);
    let answered = f.request_approval(session);
    let cancelled = f.request_approval(session);
    let second = ContextId::new();
    accepted(f.clear(session, second, "second"));

    accepted(f.handle(CommandKind::ResolveApproval {
        id: answered,
        decision: Decision::Approve,
        rule: None,
    }));
    let resolved = f.last_event();
    assert!(matches!(resolved.kind, EventKind::ApprovalResolved { .. }));
    assert_eq!(resolved.context, first);

    f.log
        .cancel_approval(&mut f.storage.db, cancelled, "gone")
        .unwrap()
        .unwrap();
    let cancel = f.last_event();
    assert!(matches!(cancel.kind, EventKind::ApprovalCancelled { .. }));
    assert_eq!(cancel.context, first);

    // Positive control: an approval asked after the clear is the new one's.
    let later = f.request_approval(session);
    accepted(f.handle(CommandKind::ResolveApproval {
        id: later,
        decision: Decision::Deny,
        rule: None,
    }));
    assert_eq!(f.last_event().context, second);
}

#[test]
fn an_interrupt_is_stamped_with_the_context_of_the_running_turn() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let first = ContextId::first_of(session);
    f.start_turn(session);
    let info = f.log.session(&f.storage.db, session).unwrap().unwrap();
    assert_eq!(info.turn_context, Some(first));
    let second = ContextId::new();
    accepted(f.clear(session, second, "second"));
    // The old turn still runs until its agent says otherwise.
    let interrupt = CommandKind::Interrupt {
        session_id: session,
    };
    accepted(f.handle(interrupt.clone()));
    let interrupted = f.last_event();
    assert_eq!(interrupted.kind, EventKind::TurnInterrupted);
    assert_eq!(interrupted.context, first);
    let info = f.log.session(&f.storage.db, session).unwrap().unwrap();
    assert_eq!((info.turn_active, info.turn_context), (false, None));

    // A turn started in the new context is that context's.
    f.start_turn(session);
    accepted(f.handle(interrupt.clone()));
    assert_eq!(f.last_event().context, second);

    // An ending turn clears its context with it.
    f.start_turn(session);
    f.agent(session, EventKind::SessionStateChanged(SessionState::Idle));
    let info = f.log.session(&f.storage.db, session).unwrap().unwrap();
    assert_eq!((info.turn_active, info.turn_context), (false, None));
}

// --- wake-up ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiters_wake_on_append() {
    let f = Fixture::new();
    let mut rx = f.log.subscribe();
    let waiter = {
        let log = f.log.clone();
        tokio::spawn(async move { log.wait_after(Seq::ZERO, Duration::from_secs(30)).await })
    };
    let writer = {
        let log = f.log.clone();
        let mut conn = f.connection();
        let workdir = workdir(&f.root.join("project"));
        tokio::task::spawn_blocking(move || {
            let command = Command {
                id: CommandId::new(),
                kind: CommandKind::CreateSession { workdir },
            };
            log.handle(&mut conn.db, DeviceId::new(), &command).unwrap()
        })
    };
    let head = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("waiter woke")
        .unwrap();
    assert_eq!(head, Seq(1));
    assert_eq!(accepted(writer.await.unwrap()), Seq(1));
    rx.changed().await.unwrap();
    assert_eq!(*rx.borrow(), Seq(1));
    // Readers woken by the notifier see the committed event.
    let batch = f.log.read(&f.storage.db, Seq::ZERO, 10).unwrap();
    assert_eq!(batch.events.len(), 1);
}

#[tokio::test]
async fn waiting_times_out_with_the_old_head() {
    let mut f = Fixture::new();
    f.create_session();
    let head = f.log.wait_after(Seq(1), Duration::from_millis(50)).await;
    assert_eq!(head, Seq(1));
}

#[tokio::test]
async fn waiting_returns_at_once_when_events_exist() {
    let mut f = Fixture::new();
    f.create_session();
    let started = std::time::Instant::now();
    let head = f.log.wait_after(Seq::ZERO, Duration::from_secs(30)).await;
    assert_eq!(head, Seq(1));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn rejected_commands_do_not_wake_waiters() {
    let mut f = Fixture::new();
    let rx = f.log.subscribe();
    f.handle(CommandKind::SendMessage {
        session_id: SessionId::new(),
        context: ContextId::first_of(SessionId::new()),
        text: "x".into(),
    });
    assert!(!rx.has_changed().unwrap());
}

#[test]
fn log_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<EventLog>();
}

// --- MoveContext -------------------------------------------------------------

impl Fixture {
    /// A session in `dir` whose first context had a turn and is no longer
    /// current: the context a move takes.
    fn movable_in(&mut self, dir: &Path) -> (SessionId, ContextId) {
        let seq = accepted(self.handle(CommandKind::CreateSession {
            workdir: workdir(dir),
        }));
        let session = self.all_events()[(seq.0 - 1) as usize].session_id;
        let first = ContextId::first_of(session);
        self.converse(first);
        accepted(self.handle(clear_kind(session, "next")));
        (session, first)
    }

    fn movable(&mut self) -> (SessionId, ContextId) {
        let project = self.root.join("project");
        self.movable_in(&project)
    }

    fn workdir_of(&self, session: SessionId) -> String {
        self.log
            .session(&self.storage.db, session)
            .unwrap()
            .unwrap()
            .workdir
    }
}

#[test]
fn a_move_logs_the_new_session_then_the_move_naming_the_command() {
    let mut f = Fixture::new();
    let (session, first) = f.movable();
    let current = f.current_context(session);
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: move_kind(session, first, "  amber\n otter "),
    };
    let before = f.log.head();
    let seq = accepted(f.log.handle(&mut f.storage.db, device, &command).unwrap());
    assert_eq!(seq, Seq(before.0 + 2), "exactly two new events");
    assert_eq!(f.log.head(), seq);

    let events = f.all_events();
    let created = &events[(seq.0 - 2) as usize];
    let moved = &events[(seq.0 - 1) as usize];
    let to = created.session_id;
    assert_ne!(to, session);
    // The new session first, naming no command, in its first context.
    assert_eq!(created.command_id, None);
    assert_eq!(created.context, ContextId::first_of(to));
    assert_eq!(
        created.kind,
        EventKind::SessionCreated {
            workdir: f.workdir_of(session),
            context_name: "amber otter".into(),
            from: Some(ContextRef {
                session,
                context: first,
            }),
        }
    );
    // Then the move itself, at the command's seq, stamped with the moved
    // context.
    assert_eq!(moved.session_id, session);
    assert_eq!(moved.command_id, Some(command.id));
    assert_eq!(moved.context, first);
    assert_eq!(moved.kind, EventKind::ContextMoved { to, by: device });

    // Projections: the new session runs in the source's stored workdir, in
    // its first context, which continues the moved one.
    assert_eq!(f.workdir_of(to), f.workdir_of(session));
    assert_eq!(f.current_context(to), ContextId::first_of(to));
    assert_eq!(f.current_context(session), current, "the source goes on");
    let id = |context: ContextId| Some(context.0.to_string());
    let to_id = Some(to.0.to_string());
    assert_eq!(f.moves(first), (None, to_id));
    assert_eq!(f.moves(ContextId::first_of(to)), (id(first), None));
    assert_eq!(f.moves(current), (None, None));
    assert_eq!(
        f.storage.db.context_stamp_violations().unwrap(),
        Vec::<String>::new()
    );

    // A name blank once normalised is generated, as for `ClearContext`.
    f.converse(current);
    accepted(f.handle(clear_kind(session, "third")));
    let seq = accepted(f.handle(move_kind(session, current, " \n ")));
    let EventKind::SessionCreated { context_name, .. } = &f.all_events()[(seq.0 - 2) as usize].kind
    else {
        panic!("no SessionCreated before the move");
    };
    let (adjective, noun) = context_name.split_once(' ').unwrap();
    assert!(skywave_proto::context_name::ADJECTIVES.contains(&adjective));
    assert!(skywave_proto::context_name::NOUNS.contains(&noun));
}

#[test]
fn a_refused_move_appends_nothing() {
    let mut f = Fixture::new();
    let session = f.create_session();
    let other = f.create_session();
    f.converse(ContextId::first_of(other));
    // first: had a turn; second: never did; third: current, never did.
    let first = ContextId::first_of(session);
    f.converse(first);
    let second = ContextId::new();
    accepted(f.clear(session, second, "second"));
    let third = ContextId::new();
    accepted(f.clear(session, third, "third"));

    let head = f.log.head();
    let mut refuse = |session: SessionId, context: ContextId, reason: RejectReason| {
        assert_eq!(
            f.handle(move_kind(session, context, "x")),
            rejected(reason),
            "{context:?}"
        );
        assert_eq!(f.log.head(), head, "a refused move logs nothing");
    };
    refuse(SessionId::new(), first, RejectReason::UnknownSession);
    refuse(session, ContextId::new(), RejectReason::UnknownContext);
    refuse(
        session,
        ContextId::first_of(other),
        RejectReason::UnknownContext,
    );
    // Current is checked before the conversation.
    refuse(session, third, RejectReason::ContextCurrent);
    refuse(session, second, RejectReason::NoConversation);

    // Positive control: the context that had a turn moves, once.
    let seq = accepted(f.handle(move_kind(session, first, "x")));
    assert_eq!(seq, Seq(head.0 + 2));
    let head = f.log.head();
    assert_eq!(
        f.handle(move_kind(session, first, "again")),
        rejected(RejectReason::ContextMoved)
    );
    assert_eq!(f.log.head(), head);
}

#[test]
fn a_move_needs_the_workdir_as_the_source_stored_it() {
    let mut f = Fixture::new();
    // Removed after the session was created.
    let gone = f.root.join("gone");
    std::fs::create_dir(&gone).unwrap();
    let (removed, removed_context) = f.movable_in(&gone);
    std::fs::remove_dir(&gone).unwrap();
    let head = f.log.head();
    assert_eq!(
        f.handle(move_kind(removed, removed_context, "x")),
        rejected(RejectReason::WorkdirNotFound)
    );
    assert_eq!(f.log.head(), head);

    // Its root dropped from the config.
    let (session, first) = f.movable();
    let elsewhere = f.tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let configured = std::mem::replace(
        &mut f.log,
        EventLog::open(&f.storage.db, vec![elsewhere]).unwrap(),
    );
    let head = f.log.head();
    assert_eq!(
        f.handle(move_kind(session, first, "x")),
        rejected(RejectReason::WorkdirNotAllowed)
    );
    assert_eq!(f.log.head(), head);
    // Positive control: with the root back the same move is accepted.
    f.log = configured;
    accepted(f.handle(move_kind(session, first, "x")));
}

/// The new session runs in the source's stored path verbatim, where the
/// agent finds the conversation it forks: one that canonicalises elsewhere
/// now (a directory on it became a symlink) is not that directory.
#[cfg(unix)]
#[test]
fn a_move_refuses_a_workdir_that_now_resolves_elsewhere() {
    let mut f = Fixture::new();
    let parent = f.root.join("p");
    std::fs::create_dir_all(parent.join("w")).unwrap();
    std::fs::create_dir_all(f.root.join("q").join("w")).unwrap();
    let (session, first) = f.movable_in(&parent.join("w"));
    assert_eq!(f.workdir_of(session), workdir(&parent.join("w")));

    let aside = f.root.join("p-aside");
    std::fs::rename(&parent, &aside).unwrap();
    std::os::unix::fs::symlink(f.root.join("q"), &parent).unwrap();
    let head = f.log.head();
    assert_eq!(
        f.handle(move_kind(session, first, "x")),
        rejected(RejectReason::WorkdirNotFound)
    );
    assert_eq!(f.log.head(), head);

    // Positive control: with the directory back, the same move goes ahead.
    std::fs::remove_file(&parent).unwrap();
    std::fs::rename(&aside, &parent).unwrap();
    accepted(f.handle(move_kind(session, first, "x")));
}

#[test]
fn a_resent_move_gets_the_same_answer_and_one_new_session() {
    let mut f = Fixture::new();
    let (session, first) = f.movable();
    let device = DeviceId::new();
    let command = Command {
        id: CommandId::new(),
        kind: move_kind(session, first, "moved"),
    };
    let result = f.log.handle(&mut f.storage.db, device, &command).unwrap();
    let seq = accepted(result.clone());
    let mut other = f.connection();
    for _ in 0..3 {
        assert_eq!(
            f.log.handle(&mut other.db, device, &command).unwrap(),
            result
        );
    }
    assert_eq!(f.log.head(), seq);
    let moved_sessions = f
        .all_kinds()
        .into_iter()
        .filter(|kind| matches!(kind, EventKind::SessionCreated { from: Some(_), .. }))
        .count();
    assert_eq!(moved_sessions, 1);
}

#[test]
fn switching_to_a_moved_context_is_refused() {
    let mut f = Fixture::new();
    let (session, first) = f.movable();
    let current = f.current_context(session);
    accepted(f.handle(move_kind(session, first, "moved")));
    let switch = |context| CommandKind::SwitchContext {
        session_id: session,
        context,
    };
    let head = f.log.head();
    assert_eq!(
        f.handle(switch(first)),
        rejected(RejectReason::ContextMoved)
    );
    assert_eq!(f.log.head(), head);
    assert_eq!(f.current_context(session), current);
    // Positive control: a context that stayed is switched to as before.
    accepted(f.handle(switch(current)));
}
