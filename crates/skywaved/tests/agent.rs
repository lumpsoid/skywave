//! Agent adapter seam (T08): the fake agent driven through the event mapper.
//!
//! Each test plays the supervisor's part by hand: commands go through
//! `EventLog::handle`, the agent is told directly, and every adapter event is
//! fed to the mapper.

use std::io::Read;
use std::path::PathBuf;

use serde_json::json;
use skywave_proto::limits::{
    DETAIL_MAX_CHARS, PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES, STATUS_MAX_CHARS, TITLE_MAX_CHARS,
};
use skywave_proto::{
    ApprovalId, BodyFormat, CallId, Command, CommandId, CommandKind, CommandResult, Decision,
    DeviceId, EventKind, RejectReason, Seq, SessionId, SessionState, ToolCall, ToolDisplay,
    ToolKind, ToolResult,
};
use skywaved::agent::fake::{FakeAgent, FakeInput, FakeScript, Gate, INTERRUPT_REASON, Step};
use skywaved::agent::{
    self, AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentSession, DisplayBody,
    EventMapper, ResultDisplay, StartSpec, TurnOutcome, load_resume_handle,
};
use skywaved::{EventLog, Storage};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    storage: Storage,
    log: EventLog,
    session_id: SessionId,
    workdir: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let workdir = root.join("project");
        std::fs::create_dir_all(&workdir).unwrap();
        let storage = Storage::open(tmp.path().join("data")).unwrap();
        let log = EventLog::open(&storage.db, vec![root]).unwrap();
        let mut fixture = Fixture {
            _tmp: tmp,
            storage,
            log,
            session_id: SessionId::new(),
            workdir: workdir.clone(),
        };
        let seq = accepted(fixture.handle(CommandKind::CreateSession {
            workdir: workdir.to_str().unwrap().to_owned(),
        }));
        let batch = fixture
            .log
            .read(&fixture.storage.db, Seq(seq.0 - 1), 1)
            .unwrap();
        fixture.session_id = batch.events[0].session_id;
        fixture
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

    fn spec(&self) -> StartSpec {
        StartSpec {
            session_id: self.session_id,
            workdir: self.workdir.clone(),
            resume: load_resume_handle(&self.storage.db, self.session_id).unwrap(),
        }
    }

    /// Feeds adapter events to `mapper` until one matches `until`, and
    /// returns them all.
    async fn pump(
        &mut self,
        events: &mut AdapterEvents,
        mapper: &mut EventMapper,
        until: impl Fn(&AdapterEvent) -> bool,
    ) -> Vec<AdapterEvent> {
        let mut seen = Vec::new();
        loop {
            let event = events.recv().await.expect("agent event stream ended");
            mapper
                .apply(&self.log, &mut self.storage, event.clone())
                .unwrap();
            let done = until(&event);
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    /// The session's log after `SessionCreated`.
    fn kinds(&self) -> Vec<EventKind> {
        let batch = self.log.read(&self.storage.db, Seq::ZERO, 1000).unwrap();
        batch
            .events
            .into_iter()
            .filter(|e| e.session_id == self.session_id)
            .map(|e| e.kind)
            .skip(1)
            .collect()
    }

    fn blob(&self, kind: &EventKind) -> String {
        let blob = match kind {
            EventKind::ToolCall(ToolCall { input, .. })
            | EventKind::ApprovalRequested { input, .. } => input,
            EventKind::ToolResult(ToolResult { output, .. }) => output,
            other => panic!("no blob in {other:?}"),
        }
        .clone()
        .expect("payload should be a blob");
        let mut content = String::new();
        self.storage
            .open_blob(blob.id)
            .unwrap()
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content.len() as u64, blob.size);
        content
    }

    fn approval_id(&self) -> ApprovalId {
        self.kinds()
            .iter()
            .rev()
            .find_map(|k| match k {
                EventKind::ApprovalRequested { id, .. } => Some(*id),
                _ => None,
            })
            .expect("an approval was requested")
    }
}

fn accepted(result: CommandResult) -> Seq {
    match result {
        CommandResult::Accepted { seq } => seq,
        other => panic!("expected Accepted, got {other:?}"),
    }
}

fn turn_ended(event: &AdapterEvent) -> bool {
    matches!(event, AdapterEvent::TurnEnded(_))
}

fn approval_requested(event: &AdapterEvent) -> bool {
    matches!(event, AdapterEvent::ApprovalRequested { .. })
}

fn state(state: SessionState) -> EventKind {
    EventKind::SessionStateChanged(state)
}

fn call_id(kind: &EventKind) -> CallId {
    match kind {
        EventKind::ToolCall(ToolCall {
            call: Some(call), ..
        }) => *call,
        other => panic!("expected a ToolCall with an id, got {other:?}"),
    }
}

fn big_output() -> String {
    (0..400)
        .map(|i| format!("line {i:04} of the listing\n"))
        .collect()
}

#[tokio::test]
async fn full_turn_with_blob_output_and_approved_approval() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new().turn([
            Step::text("Looking around."),
            Step::tool_call("shell", json!({ "command": "ls -la" })).call("c1"),
            Step::tool_result(big_output()).call("c1"),
            Step::tool_call("write", json!({ "path": "notes.txt", "content": "hi" })).call("c2"),
            Step::approval(
                "req-1",
                "write",
                json!({ "path": "notes.txt", "content": "hi" }),
            )
            .call("c2")
            .on_approve([Step::tool_result("written").call("c2"), Step::text("Done.")])
            .on_deny([Step::text("Not writing it.")]),
        ]),
    );
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);

    accepted(f.handle(CommandKind::SendMessage {
        session_id: f.session_id,
        text: "look around".into(),
    }));
    session.send_message("look around".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, approval_requested).await;

    // The resume handle arrived before the turn finished.
    assert_eq!(
        load_resume_handle(&f.storage.db, f.session_id).unwrap(),
        Some(FakeAgent::resume_handle(f.session_id))
    );
    let id = f.approval_id();
    assert_eq!(mapper.pending_approvals().collect::<Vec<_>>(), [id]);
    assert!(
        f.log
            .session(&f.storage.db, f.session_id)
            .unwrap()
            .unwrap()
            .turn_active
    );

    // A device approves; the supervisor relays the answer.
    accepted(f.handle(CommandKind::ResolveApproval {
        id,
        decision: Decision::Approve,
        rule: None,
    }));
    let request = mapper
        .take_approval(id)
        .expect("the mapper knows the request");
    assert_eq!(request.0, "req-1");
    assert_eq!(mapper.take_approval(id), None);
    session
        .answer_approval(request, Decision::Approve, None)
        .await
        .unwrap();
    let tail = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        tail.last(),
        Some(&AdapterEvent::TurnEnded(TurnOutcome::Completed))
    );

    let kinds = f.kinds();
    assert!(matches!(&kinds[0], EventKind::UserMessage { text, .. } if text == "look around"));
    assert_eq!(kinds[1], state(SessionState::AgentWorking));
    assert_eq!(
        kinds[2],
        EventKind::AgentMessage {
            text: "Looking around.".into()
        }
    );
    // An input of one string field is shown whole by the generic display.
    let shell = call_id(&kinds[3]);
    assert_eq!(
        kinds[3],
        EventKind::ToolCall(ToolCall {
            call: Some(shell),
            name: "shell".into(),
            display: ToolDisplay {
                kind: ToolKind::Other,
                title: "ls -la".into(),
                detail: None,
                body: None
            },
            input: None,
        })
    );
    // A large output: a bounded preview, its line count, the whole as a blob.
    let EventKind::ToolResult(ToolResult {
        call,
        preview,
        lines,
        output,
        ..
    }) = &kinds[4]
    else {
        panic!("expected ToolResult, got {:?}", kinds[4]);
    };
    assert_eq!(*call, Some(shell));
    let big = big_output();
    let head: Vec<&str> = big.lines().take(PREVIEW_MAX_LINES).collect();
    assert_eq!(*preview, head.join("\n"));
    assert_eq!(*lines, 400);
    assert_eq!(output.as_ref().unwrap().size, big_output().len() as u64);
    assert_eq!(f.blob(&kinds[4]), big_output());
    // Other fields left out of the generic title: the input is a blob.
    let write = call_id(&kinds[5]);
    assert_ne!(write, shell);
    let EventKind::ToolCall(ToolCall { display, .. }) = &kinds[5] else {
        unreachable!()
    };
    assert_eq!(display.title, "hi");
    let input: serde_json::Value = serde_json::from_str(&f.blob(&kinds[5])).unwrap();
    assert_eq!(input, json!({ "path": "notes.txt", "content": "hi" }));
    // The approval names its call and drops the input it shares with it.
    assert!(matches!(
        &kinds[6],
        EventKind::ApprovalRequested { id: a, call: Some(c), tool, summary, input: None, suggestions, }
            if *a == id && *c == write && tool == "write" && summary == "hi" && suggestions.is_empty()
    ));
    assert!(matches!(
        kinds[7],
        EventKind::ApprovalResolved {
            decision: Decision::Approve,
            ..
        }
    ));
    assert_eq!(
        kinds[8],
        EventKind::ToolResult(ToolResult {
            call: Some(write),
            is_error: false,
            status: None,
            preview: "written".into(),
            lines: 1,
            output: None,
        })
    );
    assert_eq!(
        kinds[9],
        EventKind::AgentMessage {
            text: "Done.".into()
        }
    );
    assert_eq!(kinds[10], state(SessionState::Idle));
    assert_eq!(kinds.len(), 11);
    assert!(
        !f.log
            .session(&f.storage.db, f.session_id)
            .unwrap()
            .unwrap()
            .turn_active
    );

    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            FakeInput::Message("look around".into()),
            FakeInput::Answer("req-1".into(), Decision::Approve, None),
        ]
    );
    session.shutdown().await.unwrap();
    let rest = f.pump(&mut events, &mut mapper, |_| true).await;
    assert_eq!(rest, [AdapterEvent::Exited { error: None }]);
    assert_eq!(f.kinds().len(), 11, "a clean exit while idle logs nothing");
}

#[tokio::test]
async fn interrupt_cancels_the_pending_approval_and_the_session_stays_usable() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([
                Step::tool_call("shell", json!({ "command": "rm -rf build" })),
                Step::approval("req-7", "shell", json!({ "command": "rm -rf build" }))
                    .on_approve([Step::tool_result("removed")]),
            ])
            .turn([Step::text("Second turn.")]),
    );
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);

    session.send_message("clean up".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, approval_requested).await;
    let id = f.approval_id();

    // The Interrupt command is accepted at once; the agent is told after.
    let interrupted = accepted(f.handle(CommandKind::Interrupt {
        session_id: f.session_id,
    }));
    session.interrupt().await.unwrap();
    let tail = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        tail,
        [
            AdapterEvent::ApprovalCancelled {
                request: "req-7".into(),
                reason: INTERRUPT_REASON.into(),
            },
            AdapterEvent::TurnEnded(TurnOutcome::Interrupted),
        ]
    );

    let kinds = f.kinds();
    let n = kinds.len();
    assert_eq!(kinds[n - 3], EventKind::TurnInterrupted);
    assert_eq!(
        kinds[n - 2],
        EventKind::ApprovalCancelled {
            id,
            reason: INTERRUPT_REASON.into()
        }
    );
    assert_eq!(kinds[n - 1], state(SessionState::Idle));
    assert_eq!(f.log.head(), Seq(interrupted.0 + 2));
    assert!(f.log.pending_approvals(&f.storage.db).unwrap().is_empty());
    assert_eq!(mapper.pending_approvals().count(), 0);
    // Too late: a device answering now loses.
    assert_eq!(
        f.handle(CommandKind::ResolveApproval {
            id,
            decision: Decision::Approve,
            rule: None,
        }),
        CommandResult::Rejected {
            reason: RejectReason::ApprovalAlreadyResolved
        }
    );

    // A stale answer is ignored and the next message runs the next turn.
    session
        .answer_approval("req-7".into(), Decision::Approve, None)
        .await
        .unwrap();
    session.send_message("go on".into()).await.unwrap();
    let next = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        next,
        [
            AdapterEvent::TurnStarted,
            AdapterEvent::Text("Second turn.".into()),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ]
    );
}

#[tokio::test]
async fn interrupt_stops_a_hanging_turn_and_a_gate_releases_a_waiting_one() {
    let mut f = Fixture::new();
    let gate = Gate::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([Step::text("working"), Step::Hang, Step::text("never")])
            .turn([Step::Wait(gate.clone()), Step::text("released")]),
    );
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);

    session.send_message("first".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, |e| {
        matches!(e, AdapterEvent::Text(_))
    })
    .await;
    // A message sent mid-turn is queued for the next turn.
    session.send_message("second".into()).await.unwrap();
    session.interrupt().await.unwrap();
    let tail = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(tail, [AdapterEvent::TurnEnded(TurnOutcome::Interrupted)]);

    f.pump(&mut events, &mut mapper, |e| {
        *e == AdapterEvent::TurnStarted
    })
    .await;
    gate.open();
    let tail = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        tail,
        [
            AdapterEvent::Text("released".into()),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ]
    );
    let kinds = f.kinds();
    assert_eq!(
        kinds,
        [
            state(SessionState::AgentWorking),
            EventKind::AgentMessage {
                text: "working".into()
            },
            state(SessionState::Idle),
            state(SessionState::AgentWorking),
            EventKind::AgentMessage {
                text: "released".into()
            },
            state(SessionState::Idle),
        ]
    );
}

#[tokio::test]
async fn denied_approval_runs_the_deny_branch() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new().turn([Step::approval("r", "write", json!({ "path": "x" }))
            .on_approve([Step::text("approved")])
            .on_deny([Step::tool_error("denied by user"), Step::text("Skipping.")])]),
    );
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);
    session.send_message("write x".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, approval_requested).await;
    let id = f.approval_id();
    accepted(f.handle(CommandKind::ResolveApproval {
        id,
        decision: Decision::Deny,
        rule: None,
    }));
    let request = mapper.take_approval(id).unwrap();
    session
        .answer_approval(request, Decision::Deny, None)
        .await
        .unwrap();
    f.pump(&mut events, &mut mapper, turn_ended).await;
    let kinds = f.kinds();
    let n = kinds.len();
    assert_eq!(
        kinds[n - 3],
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: true,
            status: None,
            preview: "denied by user".into(),
            lines: 1,
            output: None,
        })
    );
    assert_eq!(
        kinds[n - 2],
        EventKind::AgentMessage {
            text: "Skipping.".into()
        }
    );
    assert_eq!(kinds[n - 1], state(SessionState::Idle));
}

#[tokio::test]
async fn restart_resumes_with_the_stored_handle() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([Step::text("one")])
            .turn([Step::text("two")]),
    );
    assert_eq!(f.spec().resume, None);
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);
    session.send_message("a".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, turn_ended).await;
    session.shutdown().await.unwrap();
    f.pump(&mut events, &mut mapper, |e| {
        matches!(e, AdapterEvent::Exited { .. })
    })
    .await;
    assert!(matches!(
        fresh_start_error(&agent, &f).await,
        AgentError::Start(_)
    ));

    // The restart finds the handle in sessions.agent_state.
    let spec = f.spec();
    assert_eq!(spec.resume, Some(FakeAgent::resume_handle(f.session_id)));
    let (mut session, mut events) = agent.start(spec).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);
    session.send_message("b".into()).await.unwrap();
    let turn = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(turn[1], AdapterEvent::Text("two".into()));
    // Past the script, the fake echoes.
    session.send_message("c".into()).await.unwrap();
    let turn = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(turn[1], AdapterEvent::Text("echo: c".into()));

    // A handle for another session is refused.
    let mut wrong = f.spec();
    wrong.resume = Some(FakeAgent::resume_handle(SessionId::new()));
    assert!(matches!(
        agent.start(wrong).await,
        Err(AgentError::Resume(_))
    ));
}

/// Starting a session fresh again fails, like a reused agent session id.
async fn fresh_start_error(agent: &FakeAgent, f: &Fixture) -> AgentError {
    let mut fresh = f.spec();
    fresh.resume = None;
    match agent.start(fresh).await {
        Err(error) => error,
        Ok(_) => panic!("a second fresh start must fail"),
    }
}

#[tokio::test]
async fn failures_and_exits_mark_the_session_errored() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([Step::Fail("model overloaded".into())])
            .turn([
                Step::approval("r", "shell", json!({ "command": "make" })),
                Step::Exit(Some("crashed".into())),
            ]),
    );
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(f.session_id);
    session.send_message("1".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        f.kinds().last().unwrap(),
        &state(SessionState::Errored {
            message: "model overloaded".into()
        })
    );

    session.send_message("2".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, approval_requested).await;
    let id = f.approval_id();
    accepted(f.handle(CommandKind::ResolveApproval {
        id,
        decision: Decision::Approve,
        rule: None,
    }));
    let request = mapper.take_approval(id).unwrap();
    session
        .answer_approval(request, Decision::Approve, None)
        .await
        .unwrap();
    let tail = f
        .pump(&mut events, &mut mapper, |e| {
            matches!(e, AdapterEvent::Exited { .. })
        })
        .await;
    assert_eq!(
        tail,
        [AdapterEvent::Exited {
            error: Some("crashed".into())
        }]
    );
    assert_eq!(
        f.kinds().last().unwrap(),
        &state(SessionState::Errored {
            message: "agent exited: crashed".into()
        })
    );
    assert!(!mapper.turn_open());
    assert!(matches!(
        session.send_message("3".into()).await,
        Err(AgentError::Closed)
    ));
}

#[test]
fn rule_suggestions_are_cut_to_the_protocol_caps() {
    use skywave_proto::limits::{RULE_LABEL_MAX_CHARS, RULE_SUGGESTIONS_MAX};
    use skywaved::agent::{RuleScope, RuleSuggestion};

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    let long = "x".repeat(RULE_LABEL_MAX_CHARS + 10);
    let mut suggestions = vec![
        RuleSuggestion {
            label: format!("{long}\nsecond line"),
            scope: RuleScope::Project,
        },
        RuleSuggestion {
            label: "two\nlines".into(),
            scope: RuleScope::User,
        },
    ];
    suggestions.extend((0..RULE_SUGGESTIONS_MAX).map(|n| RuleSuggestion {
        label: format!("rule {n}"),
        scope: RuleScope::Session,
    }));
    for event in [
        AdapterEvent::TurnStarted,
        AdapterEvent::ApprovalRequested {
            request: "q".into(),
            call: None,
            tool: "write".into(),
            input: json!("short"),
            suggestions,
        },
    ] {
        mapper.apply(&f.log, &mut f.storage, event).unwrap();
    }
    let kinds = f.kinds();
    let EventKind::ApprovalRequested { suggestions, .. } = &kinds[1] else {
        panic!("expected ApprovalRequested, got {:?}", kinds[1]);
    };
    // The first ones kept, in order, so the adapter's indices still hold.
    assert_eq!(suggestions.len(), RULE_SUGGESTIONS_MAX);
    assert_eq!(suggestions[0].label.chars().count(), RULE_LABEL_MAX_CHARS);
    assert!(suggestions[0].label.ends_with('…'));
    assert_eq!(suggestions[0].scope, RuleScope::Project);
    assert_eq!(suggestions[1].label, "two…");
    assert_eq!(suggestions[2].label, "rule 0");
    assert_eq!(suggestions[7].label, "rule 5");
}

#[test]
fn turn_usage_is_logged_before_the_turn_ends_and_only_within_one() {
    use skywaved::agent::TurnUsage;

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    let usage = TurnUsage {
        duration_ms: Some(12_400),
        cost_usd: Some(0.04),
        ..TurnUsage::default()
    };
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::TurnUsage(TurnUsage::default()),
            AdapterEvent::TurnUsage(usage.clone()),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            AdapterEvent::TurnUsage(usage.clone()),
        ],
    );
    assert_eq!(
        f.kinds(),
        [
            state(SessionState::AgentWorking),
            EventKind::TurnUsage(usage),
            state(SessionState::Idle),
        ]
    );
}

#[tokio::test]
async fn exit_with_a_pending_approval_cancels_it() {
    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    for event in [
        AdapterEvent::TurnStarted,
        AdapterEvent::ToolCall {
            call: None,
            name: "write".into(),
            input: json!({ "path": "a.txt", "content": "a long enough body" }),
            display: None,
        },
        AdapterEvent::ApprovalRequested {
            request: "q".into(),
            call: None,
            tool: "write".into(),
            input: json!("short"),
            suggestions: Vec::new(),
        },
        AdapterEvent::ToolResult {
            call: None,
            result: ResultDisplay {
                is_error: false,
                status: None,
                preview: None,
                output: String::new(),
            },
        },
        AdapterEvent::Exited { error: None },
    ] {
        mapper.apply(&f.log, &mut f.storage, event).unwrap();
    }
    let kinds = f.kinds();
    // The generic title is the first string field; the rest of the input is
    // only in its blob, pretty-printed JSON.
    let EventKind::ToolCall(ToolCall { display, .. }) = &kinds[1] else {
        panic!("expected ToolCall, got {:?}", kinds[1]);
    };
    assert_eq!(display.title, "a long enough body");
    let blob: serde_json::Value = serde_json::from_str(&f.blob(&kinds[1])).unwrap();
    assert_eq!(blob["path"], "a.txt");
    assert!(f.blob(&kinds[1]).contains('\n'));
    let id = f.approval_id();
    assert!(
        matches!(&kinds[2], EventKind::ApprovalRequested { call: None, summary, input: None, .. } if summary == "short")
    );
    assert_eq!(
        kinds[3],
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: String::new(),
            lines: 0,
            output: None,
        })
    );
    assert_eq!(
        kinds[4],
        EventKind::ApprovalCancelled {
            id,
            reason: "agent exited".into()
        }
    );
    assert_eq!(
        kinds[5],
        state(SessionState::Errored {
            message: "agent exited during a turn".into()
        })
    );
    assert!(f.log.pending_approvals(&f.storage.db).unwrap().is_empty());
}

fn apply_all(f: &mut Fixture, mapper: &mut EventMapper, events: Vec<AdapterEvent>) {
    for event in events {
        mapper.apply(&f.log, &mut f.storage, event).unwrap();
    }
}

fn ok(output: &str) -> ResultDisplay {
    ResultDisplay {
        is_error: false,
        status: None,
        preview: None,
        output: output.to_owned(),
    }
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Over 100 KiB: a first line of 300 two-byte chars, then numbered lines.
fn huge_text() -> String {
    let mut text = "é".repeat(300);
    text.push('\n');
    text.extend((0..4000).map(|i| format!("{i:05} and some more text here\n")));
    assert!(text.len() > 100 * 1024);
    text
}

#[test]
fn large_content_gives_small_events_and_blobs() {
    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    let huge = huge_text();
    let input = json!({ "command": huge.clone() });
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                name: "shell".into(),
                input: input.clone(),
                display: None,
            },
            AdapterEvent::ApprovalRequested {
                request: "q".into(),
                call: None,
                tool: "shell".into(),
                input: input.clone(),
                suggestions: Vec::new(),
            },
            AdapterEvent::ToolResult {
                call: Some("c1".into()),
                result: ResultDisplay {
                    is_error: true,
                    status: Some(huge.clone()),
                    preview: None,
                    output: huge.clone(),
                },
            },
            AdapterEvent::ToolCall {
                call: Some("c2".into()),
                name: "edit".into(),
                input: json!({ "path": "a.txt" }),
                display: Some(agent::ToolDisplay {
                    kind: ToolKind::FileEdit,
                    title: huge.clone(),
                    detail: Some(huge.clone()),
                    body: Some(DisplayBody {
                        format: BodyFormat::Diff,
                        text: huge.clone(),
                    }),
                    shows_all_input: true,
                }),
            },
        ],
    );
    let kinds = f.kinds();
    for (i, kind) in kinds[1..=3].iter().enumerate() {
        let wire = serde_json::to_string(kind).unwrap();
        // The call carries a body preview beside its title.
        let max = if i == 0 {
            1024 + PREVIEW_MAX_BYTES
        } else {
            1024
        };
        assert!(wire.len() < max, "{} bytes: {wire}", wire.len());
        assert!(!wire.contains("[error]"));
    }
    let title = format!("{}…", "é".repeat(TITLE_MAX_CHARS - 1));

    // The generic display: the cut first line, the whole text as a body;
    // the whole input as a blob.
    let EventKind::ToolCall(ToolCall { display, .. }) = &kinds[1] else {
        panic!("expected ToolCall, got {:?}", kinds[1]);
    };
    assert_eq!(display.title, title);
    let body = display.body.as_ref().expect("a cut title has a body");
    assert_eq!((body.format, body.lines), (BodyFormat::Text, 4001));
    assert!(body.full.is_some());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.blob(&kinds[1])).unwrap(),
        input
    );

    // An approval for no logged call: generic summary, the input as a blob.
    let EventKind::ApprovalRequested { call, summary, .. } = &kinds[2] else {
        panic!("expected ApprovalRequested, got {:?}", kinds[2]);
    };
    assert_eq!((call, summary), (&None, &title));
    assert!(f.blob(&kinds[2]).contains("00042 and some more text"));

    // A failure is `is_error`, with a bounded status and preview.
    let EventKind::ToolResult(result) = &kinds[3] else {
        panic!("expected ToolResult, got {:?}", kinds[3]);
    };
    assert!(result.is_error);
    assert_eq!(result.call, Some(call_id(&kinds[1])));
    let status = result.status.as_deref().unwrap();
    assert_eq!(chars(status), STATUS_MAX_CHARS);
    assert!(status.ends_with('…'));
    assert_eq!(result.preview, "é".repeat(PREVIEW_MAX_BYTES / 2));
    assert_eq!(result.lines, 4001);
    assert_eq!(f.blob(&kinds[3]), huge);

    // An adapter's display is cut to the caps too; what was cut is a blob.
    let EventKind::ToolCall(ToolCall { display, input, .. }) = &kinds[4] else {
        panic!("expected ToolCall, got {:?}", kinds[4]);
    };
    assert_eq!(display.kind, ToolKind::FileEdit);
    assert_eq!(display.title, title);
    assert_eq!(chars(display.detail.as_deref().unwrap()), DETAIL_MAX_CHARS);
    let body = display.body.as_ref().unwrap();
    assert_eq!(body.format, BodyFormat::Diff);
    assert!(body.preview.len() <= PREVIEW_MAX_BYTES);
    assert_eq!(body.lines, 4001);
    let full = body
        .full
        .as_ref()
        .expect("a cut body has its text as a blob");
    assert_eq!(full.size, huge.len() as u64);
    assert!(input.is_some(), "a cut display blobs the input");
}

#[test]
fn content_shown_whole_makes_no_blob() {
    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    let eight_lines: String = (0..PREVIEW_MAX_LINES).map(|i| format!("{i}\n")).collect();
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                name: "shell".into(),
                input: json!({ "command": "ls", "description": "List files" }),
                display: Some(agent::ToolDisplay {
                    kind: ToolKind::Shell,
                    title: "ls".into(),
                    detail: Some("List files".into()),
                    body: Some(DisplayBody {
                        format: BodyFormat::Text,
                        text: eight_lines.clone(),
                    }),
                    shows_all_input: true,
                }),
            },
            AdapterEvent::ToolResult {
                call: Some("c1".into()),
                result: ResultDisplay {
                    is_error: false,
                    status: Some("exit 0".into()),
                    preview: None,
                    output: eight_lines.clone(),
                },
            },
        ],
    );
    let kinds = f.kinds();
    let call = call_id(&kinds[1]);
    assert_eq!(
        kinds[1],
        EventKind::ToolCall(ToolCall {
            call: Some(call),
            name: "shell".into(),
            display: ToolDisplay {
                kind: ToolKind::Shell,
                title: "ls".into(),
                detail: Some("List files".into()),
                body: Some(skywave_proto::Body {
                    format: BodyFormat::Text,
                    preview: eight_lines.clone(),
                    lines: 8,
                    full: None,
                }),
            },
            input: None,
        })
    );
    assert_eq!(
        kinds[2],
        EventKind::ToolResult(ToolResult {
            call: Some(call),
            is_error: false,
            status: Some("exit 0".into()),
            preview: eight_lines,
            lines: 8,
            output: None,
        })
    );
}

#[test]
fn omitted_input_and_a_separate_preview_are_blobs() {
    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    let input = json!({ "command": "make", "timeout": 600 });
    let note = "built\n(a note for the model)";
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                name: "shell".into(),
                input: input.clone(),
                display: Some(agent::ToolDisplay {
                    kind: ToolKind::Shell,
                    title: "make".into(),
                    detail: None,
                    body: None,
                    shows_all_input: false,
                }),
            },
            AdapterEvent::ToolResult {
                call: Some("c1".into()),
                result: ResultDisplay {
                    is_error: false,
                    status: Some("created".into()),
                    preview: Some("built".into()),
                    output: note.into(),
                },
            },
            AdapterEvent::ToolResult {
                call: Some("c1".into()),
                result: ResultDisplay {
                    is_error: false,
                    status: None,
                    preview: Some(note.into()),
                    output: note.into(),
                },
            },
        ],
    );
    let kinds = f.kinds();
    // The display left a field out: the input is a blob, uncut title or not.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.blob(&kinds[1])).unwrap(),
        input
    );
    // The preview is the adapter's text; the output keeps the note.
    let EventKind::ToolResult(result) = &kinds[2] else {
        panic!("expected ToolResult, got {:?}", kinds[2]);
    };
    assert_eq!((result.preview.as_str(), result.lines), ("built", 2));
    assert_eq!(f.blob(&kinds[2]), note);
    // A preview equal to the output is no reason for a blob.
    let EventKind::ToolResult(result) = &kinds[3] else {
        panic!("expected ToolResult, got {:?}", kinds[3]);
    };
    assert_eq!((result.preview.as_str(), &result.output), (note, &None));
}

#[test]
fn call_approval_and_result_share_one_call_id_within_a_turn() {
    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(f.session_id);
    let write = json!({ "path": "a.txt", "content": "hi" });
    let edited = json!({ "path": "a.txt", "content": "bye" });
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                name: "write".into(),
                input: write.clone(),
                display: None,
            },
            AdapterEvent::ApprovalRequested {
                request: "q1".into(),
                call: Some("c1".into()),
                tool: "write".into(),
                input: write.clone(),
                suggestions: Vec::new(),
            },
            AdapterEvent::ApprovalRequested {
                request: "q2".into(),
                call: Some("c1".into()),
                tool: "write".into(),
                input: edited.clone(),
                suggestions: Vec::new(),
            },
            AdapterEvent::ToolResult {
                call: Some("c1".into()),
                result: ok("written"),
            },
            AdapterEvent::ToolResult {
                call: Some("unknown".into()),
                result: ok("stray"),
            },
            AdapterEvent::ToolResult {
                call: None,
                result: ok("anonymous"),
            },
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            AdapterEvent::TurnStarted,
            // Ids are only linked within their turn.
            AdapterEvent::ToolResult {
                call: Some("c1".into()),
                result: ok("late"),
            },
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ],
    );
    let kinds = f.kinds();
    let call = call_id(&kinds[1]);
    // Same input as the call: summarized by its title, no input.
    assert!(matches!(
        &kinds[2],
        EventKind::ApprovalRequested { call: Some(c), summary, input: None, .. }
            if *c == call && summary == "hi"
    ));
    // A different input from the call's is kept as a blob.
    assert!(matches!(
        &kinds[3],
        EventKind::ApprovalRequested { call: Some(c), summary, input: Some(_), .. }
            if *c == call && summary == "hi"
    ));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.blob(&kinds[3])).unwrap(),
        edited
    );
    let results: Vec<(Option<CallId>, &str)> = kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::ToolResult(r) => Some((r.call, r.preview.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            (Some(call), "written"),
            (None, "stray"),
            (None, "anonymous"),
            (None, "late"),
        ]
    );
}

/// A shell call shown by the adapter with `title` and no body; the input is
/// a blob exactly when the body is cut.
fn shell_call(f: &mut Fixture, title: &str) -> ToolDisplay {
    let mut mapper = EventMapper::new(f.session_id);
    apply_all(
        f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                name: "shell".into(),
                input: json!({ "command": title }),
                display: Some(agent::ToolDisplay {
                    kind: ToolKind::Shell,
                    title: title.into(),
                    detail: None,
                    body: None,
                    shows_all_input: true,
                }),
            },
        ],
    );
    let EventKind::ToolCall(call) = f.kinds().remove(1) else {
        panic!("expected ToolCall");
    };
    let body_cut = call.display.body.as_ref().is_some_and(|b| b.full.is_some());
    assert_eq!(call.input.is_some(), body_cut, "{call:?}");
    call.display
}

#[test]
fn a_cut_title_gets_a_text_body() {
    let mut f = Fixture::new();
    let command = "cargo build \\\n  --release\n";
    let display = shell_call(&mut f, command);
    assert_eq!(display.title, "cargo build \\…");
    assert_eq!(
        display.body,
        Some(skywave_proto::Body {
            format: BodyFormat::Text,
            preview: command.trim().to_owned(),
            lines: 2,
            full: None,
        })
    );
}

#[test]
fn a_short_title_gets_none() {
    let mut f = Fixture::new();
    let display = shell_call(&mut f, "ls -la");
    assert_eq!((display.title.as_str(), display.body), ("ls -la", None));
}

#[test]
fn a_huge_command_blobs_the_body() {
    let mut f = Fixture::new();
    let huge = huge_text();
    let display = shell_call(&mut f, &huge);
    let body = display.body.expect("a cut title has a body");
    assert_eq!(body.format, BodyFormat::Text);
    assert!(body.preview.len() <= PREVIEW_MAX_BYTES);
    assert_eq!(body.lines, 4001);
    let full = body.full.expect("the body is a blob");
    assert_eq!(full.size, huge.trim().len() as u64);
}
