//! Event log and command handling (T03), driven through the public API.

use std::path::{Path, PathBuf};
use std::time::Duration;

use skywave_proto::{
    ApprovalId, Command, CommandId, CommandKind, CommandResult, Decision, DeviceId, EventKind,
    RejectReason, RuleScope, RuleSuggestion, Seq, SessionId, SessionState,
};
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

    fn agent(&mut self, session: SessionId, kind: EventKind) -> Seq {
        self.log
            .append(&mut self.storage.db, session, kind)
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
        let batch = self.log.read(&self.storage.db, Seq::ZERO, 5000).unwrap();
        batch.events.into_iter().map(|e| e.kind).collect()
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
    assert_eq!(
        f.all_kinds().last().unwrap(),
        &EventKind::UserMessage {
            command_id: command.id,
            device_id: device,
            text: "hello".into(),
        }
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
                                text,
                            },
                        };
                        accepted(log.handle(&mut conn.db, device, &command).unwrap())
                    } else {
                        log.append(&mut conn.db, session, EventKind::AgentMessage { text })
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
    assert_eq!(
        batch.events[0].kind,
        EventKind::SessionCreated {
            workdir: canonical.clone()
        }
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

#[test]
fn agent_appends_are_checked() {
    let mut f = Fixture::new();
    let unknown = f.log.append(
        &mut f.storage.db,
        SessionId::new(),
        EventKind::AgentMessage { text: "x".into() },
    );
    assert!(matches!(unknown, Err(Error::InvalidAppend(_))));

    let session = f.create_session();
    for kind in [
        EventKind::TurnInterrupted,
        EventKind::SessionCreated {
            workdir: "/".into(),
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
    ] {
        assert!(matches!(
            f.log.append(&mut f.storage.db, session, kind),
            Err(Error::InvalidAppend(_))
        ));
    }
    let approval = f.request_approval(session);
    let duplicate = f.log.append(
        &mut f.storage.db,
        session,
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
    assert_eq!(f.log.head(), Seq(2), "refused appends leave no trace");
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
        text: "x".into(),
    });
    assert!(!rx.has_changed().unwrap());
}

#[test]
fn log_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<EventLog>();
}
