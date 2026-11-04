//! Agent adapter seam (T08): the fake agent driven through the event mapper.
//!
//! Each test plays the supervisor's part by hand: commands go through
//! `EventLog::handle`, the agent is told directly, and every adapter event is
//! fed to the mapper.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;

use serde_json::json;
use skywave_proto::limits::{
    AGENT_VERSION_MAX_CHARS, DETAIL_MAX_CHARS, PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES,
    SCHEDULE_WHEN_MAX_CHARS, SCHEDULES_MAX, STATUS_MAX_CHARS, TASK_ID_MAX_CHARS, TASKS_MAX,
    TITLE_MAX_CHARS,
};
use skywave_proto::{
    AgentSettings, AgentWork, ApprovalId, BackgroundTask, BodyFormat, CallId, Command, CommandId,
    CommandKind, CommandResult, ContextId, Decision, DeviceId, EventKind, RejectReason, Schedule,
    ScheduleId, Seq, SessionId, SessionState, TaskId, ToolCall, ToolDisplay, ToolKind, ToolResult,
};
use skywaved::agent::fake::{
    FakeAgent, FakeConversation, FakeInput, FakeScript, Gate, INTERRUPT_REASON, Step,
    subagent_task, task,
};
use skywaved::agent::{
    self, AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentSchedule, AgentSession,
    AgentTask, Conversation, DisplayBody, EventMapper, ResultDisplay, StartSpec, TaskKind,
    TurnOutcome, load_resume_handle,
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
            conversation: None,
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
            context: ContextId::first_of(self.session_id),
            workdir: self.workdir.clone(),
            conversation: match load_resume_handle(
                &self.storage.db,
                ContextId::first_of(self.session_id),
            )
            .unwrap()
            {
                Some(handle) => Conversation::Resume(handle),
                None => Conversation::Fresh,
            },
            grants: None,
            mode: None,
            model: None,
            effort: None,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );

    accepted(f.handle(CommandKind::SendMessage {
        session_id: f.session_id,
        context: ContextId::first_of(f.session_id),
        text: "look around".into(),
    }));
    session.send_message("look around".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, approval_requested).await;

    // The resume handle arrived before the turn finished.
    assert_eq!(
        load_resume_handle(&f.storage.db, ContextId::first_of(f.session_id)).unwrap(),
        Some(FakeAgent::resume_handle(ContextId::first_of(f.session_id)))
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
            parent: None,
            text: "Looking around.".into()
        }
    );
    // An input of one string field is shown whole by the generic display.
    let shell = call_id(&kinds[3]);
    assert_eq!(
        kinds[3],
        EventKind::ToolCall(ToolCall {
            parent: None,
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
            parent: None,
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
            FakeInput::Started {
                conversation: FakeConversation::Fresh,
                context: ContextId::first_of(f.session_id),
                grants: None,
            },
            FakeInput::Message("look around".into()),
            FakeInput::Answer("req-1".into(), Decision::Approve, None),
        ]
    );
    session.shutdown().await.unwrap();
    let rest = f.pump(&mut events, &mut mapper, |_| true).await;
    assert_eq!(
        rest,
        [AdapterEvent::Exited {
            error: None,
            unsent: 0
        }]
    );
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );

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
    let shell = kinds
        .iter()
        .find(|k| matches!(k, EventKind::ToolCall(_)))
        .map(call_id)
        .unwrap();
    assert_eq!(kinds[n - 4], EventKind::TurnInterrupted);
    assert_eq!(
        kinds[n - 3],
        EventKind::ApprovalCancelled {
            id,
            reason: INTERRUPT_REASON.into()
        }
    );
    // The interrupted call never gets its result.
    assert_eq!(
        kinds[n - 2],
        EventKind::CallsUnanswered { calls: vec![shell] }
    );
    assert_eq!(kinds[n - 1], state(SessionState::Idle));
    assert_eq!(f.log.head(), Seq(interrupted.0 + 3));
    assert!(f.log.pending_approvals(&f.storage.db).unwrap().is_empty());
    assert_eq!(mapper.pending_approvals().count(), 0);
    // Too late: a device answering now is told so.
    assert_eq!(
        f.handle(CommandKind::ResolveApproval {
            id,
            decision: Decision::Approve,
            rule: None,
        }),
        CommandResult::Rejected {
            reason: RejectReason::ApprovalCancelled
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
            AdapterEvent::Text {
                text: "Second turn.".into(),
                parent: None,
            },
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );

    session.send_message("first".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, |e| {
        matches!(e, AdapterEvent::Text { .. })
    })
    .await;
    // A message sent mid-turn is queued for the next turn.
    session.send_message("second".into()).await.unwrap();
    session.interrupt().await.unwrap();
    // A repeat reaches the agent after the interrupt ended the turn: the
    // queued message's turn is a later one, and runs on untouched.
    session.escalate_interrupt().await.unwrap();
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
            AdapterEvent::Text {
                text: "released".into(),
                parent: None,
            },
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ]
    );
    let kinds = f.kinds();
    assert_eq!(
        kinds,
        [
            state(SessionState::AgentWorking),
            EventKind::AgentMessage {
                parent: None,
                text: "working".into()
            },
            state(SessionState::Idle),
            state(SessionState::AgentWorking),
            EventKind::AgentMessage {
                parent: None,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
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
            parent: None,
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
    assert_eq!(f.spec().conversation, Conversation::Fresh);
    // A start that got no message leaves no conversation and no handle, so
    // starting fresh again is fine.
    let (session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    session.shutdown().await.unwrap();
    let rest = f.pump(&mut events, &mut mapper, |_| true).await;
    assert_eq!(
        rest,
        [AdapterEvent::Exited {
            error: None,
            unsent: 0
        }]
    );
    assert_eq!(f.spec().conversation, Conversation::Fresh);
    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
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

    // The restart finds the handle in contexts.agent_state.
    let spec = f.spec();
    assert_eq!(
        spec.conversation,
        Conversation::Resume(FakeAgent::resume_handle(ContextId::first_of(f.session_id)))
    );
    let (mut session, mut events) = agent.start(spec).await.unwrap();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    session.send_message("b".into()).await.unwrap();
    let turn = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        turn[1],
        AdapterEvent::Text {
            text: "two".into(),
            parent: None,
        }
    );
    // Past the script, the fake echoes.
    session.send_message("c".into()).await.unwrap();
    let turn = f.pump(&mut events, &mut mapper, turn_ended).await;
    assert_eq!(
        turn[1],
        AdapterEvent::Text {
            text: "echo: c".into(),
            parent: None,
        }
    );

    // A handle for another context is refused.
    let mut wrong = f.spec();
    wrong.conversation = Conversation::Resume(FakeAgent::resume_handle(ContextId::new()));
    assert!(matches!(
        agent.start(wrong).await,
        Err(AgentError::Resume(_))
    ));
}

/// Starting a session fresh again fails, like a reused agent session id.
async fn fresh_start_error(agent: &FakeAgent, f: &Fixture) -> AgentError {
    let mut fresh = f.spec();
    fresh.conversation = Conversation::Fresh;
    match agent.start(fresh).await {
        Err(error) => error,
        Ok(_) => panic!("a second fresh start must fail"),
    }
}

/// A start of `context`, in session `session_id`, forking the conversation
/// `handle` names.
fn fork_spec(
    f: &Fixture,
    session_id: SessionId,
    context: ContextId,
    handle: agent::ResumeHandle,
) -> StartSpec {
    StartSpec {
        session_id,
        context,
        conversation: Conversation::Fork(handle),
        ..f.spec()
    }
}

#[tokio::test]
async fn a_fork_copies_a_conversation_of_another_session() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::text("origin")]));
    let origin = ContextId::first_of(f.session_id);
    let other = SessionId::new();
    let forked = ContextId::first_of(other);
    let origin_handle = FakeAgent::resume_handle(origin);

    // No conversation to copy yet: the origin never got a message.
    let refused = agent
        .start(fork_spec(&f, other, forked, origin_handle.clone()))
        .await;
    assert!(matches!(refused, Err(AgentError::Resume(_))));
    assert_eq!(agent.inputs(other), []);

    let (mut session, mut events) = agent.start(f.spec()).await.unwrap();
    let mut mapper = EventMapper::new(
        f.session_id,
        origin,
        AgentSettings::default(),
        AgentWork::default(),
    );
    session.send_message("a".into()).await.unwrap();
    f.pump(&mut events, &mut mapper, turn_ended).await;
    session.shutdown().await.unwrap();

    // Progress is kept per session, yet the fork finds the origin's
    // conversation in the other session.
    let (mut session, mut events) = agent
        .start(fork_spec(&f, other, forked, origin_handle.clone()))
        .await
        .unwrap();
    assert_eq!(
        agent.inputs(other),
        [FakeInput::Started {
            conversation: FakeConversation::Fork(origin),
            context: forked,
            grants: None,
        }]
    );
    // The copy is the forked context's own conversation: its handle comes
    // with its first message, as for a fresh start, never the origin's.
    session.send_message("b".into()).await.unwrap();
    assert_eq!(
        events.recv().await,
        Some(AdapterEvent::ResumeHandle(FakeAgent::resume_handle(forked)))
    );
    assert_eq!(events.recv().await, Some(AdapterEvent::TurnStarted));
    // Scripted turns run per session: the new session starts at the first.
    assert_eq!(
        events.recv().await,
        Some(AdapterEvent::Text {
            text: "origin".into(),
            parent: None,
        })
    );
    session.shutdown().await.unwrap();

    // Once it has its own conversation, forking into it again is refused.
    let again = agent
        .start(fork_spec(&f, other, forked, origin_handle))
        .await;
    assert!(matches!(again, Err(AgentError::Start(_))));
    // A handle naming no conversation is refused too.
    let unknown = agent
        .start(fork_spec(
            &f,
            other,
            forked,
            FakeAgent::resume_handle(ContextId::new()),
        ))
        .await;
    assert!(matches!(unknown, Err(AgentError::Resume(_))));
}

#[tokio::test]
async fn adopting_copies_an_outside_conversation() {
    let f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::text("hi")]))
        .with_outside_conversation("ext-1");
    let adopt = |id: &str| StartSpec {
        conversation: Conversation::Adopt(id.to_owned()),
        ..f.spec()
    };

    // An id nobody registered is refused, and no start is recorded.
    let refused = agent.start(adopt("ext-2")).await;
    assert!(matches!(refused, Err(AgentError::Resume(_))));
    assert_eq!(agent.inputs(f.session_id), []);

    // Positive control: the registered id starts, on a clone too.
    let (mut session, mut events) = agent.clone().start(adopt("ext-1")).await.unwrap();
    let context = ContextId::first_of(f.session_id);
    assert_eq!(
        agent.inputs(f.session_id),
        [FakeInput::Started {
            conversation: FakeConversation::Adopt("ext-1".into()),
            context,
            grants: None,
        }]
    );
    // The copy is the adopting context's own: its handle comes with its
    // first message, never the outside id.
    session.send_message("a".into()).await.unwrap();
    assert_eq!(
        events.recv().await,
        Some(AdapterEvent::ResumeHandle(FakeAgent::resume_handle(
            context
        )))
    );
    session.shutdown().await.unwrap();

    // Once it has its own conversation, adopting into it again is refused.
    let again = agent.start(adopt("ext-1")).await;
    assert!(matches!(again, Err(AgentError::Start(_))));
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
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
            error: Some("crashed".into()),
            unsent: 0,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
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
            task: None,
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
fn errored_messages_are_cut_like_refusals() {
    use skywave_proto::limits::REFUSAL_MAX_CHARS;

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let huge = "x".repeat(1 << 20);
    for event in [
        AdapterEvent::TurnStarted,
        AdapterEvent::TurnEnded(TurnOutcome::Failed(format!("{huge}\nmore"))),
        AdapterEvent::Exited {
            error: Some(huge.clone()),
            unsent: 0,
        },
    ] {
        mapper.apply(&f.log, &mut f.storage, event).unwrap();
    }
    let messages = errored_messages(&f);
    assert_eq!(messages.len(), 2, "{messages:?}");
    for message in &messages {
        assert_eq!(message.chars().count(), REFUSAL_MAX_CHARS);
        assert!(message.ends_with('…'));
    }
    assert!(messages[1].starts_with("agent exited: xxx"));
}

fn errored_messages(f: &Fixture) -> Vec<String> {
    f.kinds()
        .into_iter()
        .filter_map(|kind| match kind {
            EventKind::SessionStateChanged(SessionState::Errored { message }) => Some(message),
            _ => None,
        })
        .collect()
}

/// The `Errored` messages the mapper logs for `events`, from a fresh
/// mapper.
fn errored_after(events: Vec<AdapterEvent>) -> Vec<String> {
    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    for event in events {
        mapper.apply(&f.log, &mut f.storage, event).unwrap();
    }
    errored_messages(&f)
}

#[test]
fn unsent_messages_are_named_in_the_one_errored_of_the_exit() {
    let exited = |error: Option<&str>, unsent| AdapterEvent::Exited {
        error: error.map(str::to_owned),
        unsent,
    };
    // A stop after the turn ended: only the notice.
    assert_eq!(
        errored_after(vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::TurnEnded(TurnOutcome::Interrupted),
            exited(None, 1),
        ]),
        ["1 queued message was not sent to the agent; send it again"]
    );
    // Cut short mid-turn, or crashed: the exit, then the notice.
    assert_eq!(
        errored_after(vec![AdapterEvent::TurnStarted, exited(None, 2)]),
        [
            "agent exited during a turn; 2 queued messages were not sent to the agent; send them again"
        ]
    );
    assert_eq!(
        errored_after(vec![AdapterEvent::TurnStarted, exited(Some("boom"), 1)]),
        ["agent exited: boom; 1 queued message was not sent to the agent; send it again"]
    );
    // Nothing unsent: as before.
    assert_eq!(
        errored_after(vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::TurnEnded(TurnOutcome::Interrupted),
            exited(None, 0),
        ]),
        Vec::<String>::new()
    );
    assert_eq!(
        errored_after(vec![AdapterEvent::TurnStarted, exited(Some("boom"), 0)]),
        ["agent exited: boom"]
    );
}

#[test]
fn a_long_exit_reason_gives_way_to_the_unsent_notice() {
    use skywave_proto::limits::REFUSAL_MAX_CHARS;

    let huge = "x".repeat(1 << 20);
    let messages = errored_after(vec![
        AdapterEvent::TurnStarted,
        AdapterEvent::Exited {
            error: Some(huge),
            unsent: 3,
        },
    ]);
    assert_eq!(messages.len(), 1, "{messages:?}");
    let message = &messages[0];
    assert_eq!(message.chars().count(), REFUSAL_MAX_CHARS);
    assert!(message.starts_with("agent exited: xxx"), "{message}");
    assert!(
        message.ends_with("…; 3 queued messages were not sent to the agent; send them again"),
        "{message}"
    );
}

#[test]
fn turn_usage_is_logged_before_the_turn_ends_and_only_within_one() {
    use skywaved::agent::TurnUsage;

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    for event in [
        AdapterEvent::TurnStarted,
        AdapterEvent::ToolCall {
            call: None,
            parent: None,
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
            task: None,
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
        AdapterEvent::Exited {
            error: None,
            unsent: 0,
        },
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
    // A call with no id is never answered: no result can name it.
    assert_eq!(
        kinds[5],
        EventKind::CallsUnanswered {
            calls: vec![call_id(&kinds[1])]
        }
    );
    assert_eq!(
        kinds[6],
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let huge = huge_text();
    let input = json!({ "command": huge.clone() });
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                parent: None,
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
                task: None,
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
                parent: None,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let eight_lines: String = (0..PREVIEW_MAX_LINES).map(|i| format!("{i}\n")).collect();
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                parent: None,
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
            parent: None,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let input = json!({ "command": "make", "timeout": 600 });
    let note = "built\n(a note for the model)";
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                parent: None,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let write = json!({ "path": "a.txt", "content": "hi" });
    let edited = json!({ "path": "a.txt", "content": "bye" });
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                parent: None,
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
                task: None,
            },
            AdapterEvent::ApprovalRequested {
                request: "q2".into(),
                call: Some("c1".into()),
                tool: "write".into(),
                input: edited.clone(),
                suggestions: Vec::new(),
                task: None,
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
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    apply_all(
        f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::ToolCall {
                call: Some("c1".into()),
                parent: None,
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

#[test]
fn an_unasked_effort_is_logged_only_when_it_changes() {
    use skywaved::agent::Effort;

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::Effort(Some(Effort::High)),
            AdapterEvent::Effort(Some(Effort::High)),
            AdapterEvent::Effort(None),
            AdapterEvent::Effort(None),
            AdapterEvent::Effort(Some(Effort::Low)),
        ],
    );
    assert_eq!(
        f.kinds(),
        [
            EventKind::EffortChanged {
                effort: Some(Effort::High),
                answers: None,
            },
            EventKind::EffortChanged {
                effort: None,
                answers: None,
            },
            EventKind::EffortChanged {
                effort: Some(Effort::Low),
                answers: None,
            },
        ]
    );

    // A mapper made later starts from what the log has.
    let settings = f.log.agent_settings(&f.storage.db, f.session_id).unwrap();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        settings,
        AgentWork::default(),
    );
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Effort(Some(Effort::Low))],
    );
    assert_eq!(f.kinds().len(), 3);
}

#[test]
fn effort_answers_name_the_requests_in_the_order_asked() {
    use skywaved::agent::Effort;

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let (high, low, max) = (CommandId::new(), CommandId::new(), CommandId::new());
    mapper.asked_effort(Effort::High, Some(high));
    mapper.asked_effort(Effort::Low, Some(low));
    mapper.asked_effort(Effort::Max, Some(max));
    // Asked under no command for what the log already has: nothing to say.
    mapper.asked_effort(Effort::Max, None);
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::EffortSet(Some(Effort::High)),
            AdapterEvent::EffortSet(Some(Effort::Low)),
            AdapterEvent::EffortRefused {
                effort: Effort::Max,
                reason: "not this one".into(),
            },
            AdapterEvent::EffortSet(Some(Effort::Low)),
        ],
    );
    // Under no command, a change is still logged.
    mapper.asked_effort(Effort::Medium, None);
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::EffortSet(Some(Effort::Medium))],
    );
    assert_eq!(
        f.kinds(),
        [
            EventKind::EffortChanged {
                effort: Some(Effort::High),
                answers: Some(high),
            },
            EventKind::EffortChanged {
                effort: Some(Effort::Low),
                answers: Some(low),
            },
            EventKind::EffortRefused {
                effort: Effort::Max,
                reason: "not this one".into(),
                answers: Some(max),
            },
            EventKind::EffortChanged {
                effort: Some(Effort::Medium),
                answers: None,
            },
        ]
    );
}

#[test]
fn exit_refuses_the_efforts_left_unanswered() {
    use skywaved::agent::Effort;

    let mut f = Fixture::new();
    let mut mapper = EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        AgentWork::default(),
    );
    let (high, low) = (CommandId::new(), CommandId::new());
    mapper.asked_effort(Effort::High, Some(high));
    mapper.asked_effort(Effort::Low, Some(low));
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::EffortSet(Some(Effort::High)),
            AdapterEvent::Exited {
                error: None,
                unsent: 0,
            },
        ],
    );
    assert_eq!(
        f.kinds(),
        [
            EventKind::EffortChanged {
                effort: Some(Effort::High),
                answers: Some(high),
            },
            EventKind::EffortRefused {
                effort: Effort::Low,
                reason: "agent exited".into(),
                answers: Some(low),
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// Background tasks

fn agent_task(id: &str, title: &str) -> AgentTask {
    AgentTask {
        id: id.to_owned(),
        kind: TaskKind::Shell,
        title: title.to_owned(),
        parent: None,
        ambient: false,
        call: None,
    }
}

fn logged_task(id: &str, title: &str) -> BackgroundTask {
    BackgroundTask {
        call: None,
        id: TaskId(id.to_owned()),
        kind: TaskKind::Shell,
        title: title.to_owned(),
        parent: None,
        ambient: false,
    }
}

fn task_mapper(f: &Fixture, work: AgentWork) -> EventMapper {
    EventMapper::new(
        f.session_id,
        ContextId::first_of(f.session_id),
        AgentSettings::default(),
        work,
    )
}

fn changed(tasks: Vec<BackgroundTask>) -> EventKind {
    EventKind::BackgroundTasksChanged { tasks }
}

fn schedule(id: &str, outlives_agent: Option<bool>) -> Schedule {
    Schedule {
        id: ScheduleId(id.to_owned()),
        when: "*/5 * * * *".to_owned(),
        recurring: true,
        prompt: "check".to_owned(),
        outlives_agent,
    }
}

#[test]
fn an_equal_set_is_logged_once_and_a_different_one_again() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let one = || AdapterEvent::BackgroundTasks(vec![agent_task("a", "sleep")]);
    apply_all(&mut f, &mut mapper, vec![one(), one()]);
    assert_eq!(f.kinds(), [changed(vec![logged_task("a", "sleep")])]);
    // Positive control: a different set logs.
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::BackgroundTasks(vec![
            agent_task("a", "sleep"),
            agent_task("b", "tail"),
        ])],
    );
    assert_eq!(f.kinds().len(), 2);
}

#[test]
fn tasks_are_cut_to_the_limits() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let long_title = format!("{}\nsecond line", "t".repeat(TITLE_MAX_CHARS + 20));
    let mut tasks = vec![
        agent_task(&"i".repeat(TASK_ID_MAX_CHARS + 1), "dropped"),
        agent_task("long-title", &long_title),
    ];
    let mut child = agent_task("child", "orphaned");
    child.parent = Some("p".repeat(TASK_ID_MAX_CHARS + 1));
    tasks.push(child);
    tasks.extend((0..TASKS_MAX + 1).map(|n| agent_task(&format!("t{n}"), "x")));
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::BackgroundTasks(tasks)],
    );
    let kinds = f.kinds();
    let [EventKind::BackgroundTasksChanged { tasks }] = kinds.as_slice() else {
        panic!("expected one change");
    };
    assert_eq!(tasks.len(), TASKS_MAX);
    // The too-long id is gone; the rest keep the agent's order.
    assert_eq!(tasks[0].id.0, "long-title");
    assert_eq!(chars(&tasks[0].title), TITLE_MAX_CHARS);
    assert!(tasks[0].title.ends_with('…'));
    assert_eq!(tasks[1].id.0, "child");
    assert_eq!(tasks[1].parent, None);
    assert_eq!(tasks[2].id.0, "t0");
    assert_eq!(tasks.last().unwrap().id.0, format!("t{}", TASKS_MAX - 3));
}

#[test]
fn the_seed_decides_whether_the_startup_report_logs() {
    let mut f = Fixture::new();
    let mut empty = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut empty,
        vec![AdapterEvent::BackgroundTasks(vec![])],
    );
    assert_eq!(f.kinds(), []);

    let seeded = AgentWork {
        tasks: vec![logged_task("old", "sleep")],
        schedules: vec![],
    };
    let mut mapper = task_mapper(&f, seeded);
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::BackgroundTasks(vec![])],
    );
    assert_eq!(f.kinds(), [changed(vec![])]);
}

#[test]
fn exit_empties_tasks_and_schedules_that_are_not_empty() {
    let mut f = Fixture::new();
    let work = AgentWork {
        tasks: vec![logged_task("a", "sleep")],
        schedules: vec![schedule("s1", Some(true)), schedule("s2", None)],
    };
    let mut mapper = task_mapper(&f, work);
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Exited {
            error: None,
            unsent: 0,
        }],
    );
    assert_eq!(
        f.kinds(),
        [
            changed(vec![]),
            EventKind::SchedulesChanged { schedules: vec![] },
        ]
    );
    // Both sets are empty now: another exit logs neither.
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Exited {
            error: None,
            unsent: 0,
        }],
    );
    assert_eq!(f.kinds().len(), 2);

    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Exited {
            error: None,
            unsent: 0,
        }],
    );
    assert_eq!(f.kinds(), []);
}

// ---------------------------------------------------------------------------
// Subagent calls

/// A call titled `name` (its generic display), the agent's id `name` too,
/// made under `parent`.
fn named_call(name: &str, parent: Option<&str>) -> AdapterEvent {
    AdapterEvent::ToolCall {
        call: Some(name.into()),
        parent: parent.map(Into::into),
        name: "tool".into(),
        input: json!({ "name": name }),
        display: None,
    }
}

/// The result of call `name`, its preview `name`.
fn answer(name: &str) -> AdapterEvent {
    AdapterEvent::ToolResult {
        call: Some(name.into()),
        result: ok(name),
    }
}

fn sub_text(text: &str, parent: &str) -> AdapterEvent {
    AdapterEvent::Text {
        text: text.into(),
        parent: Some(parent.into()),
    }
}

/// The whole task set: subagent tasks by id and the agent call each runs.
fn subagents(tasks: &[(&str, Option<&str>)]) -> AdapterEvent {
    AdapterEvent::BackgroundTasks(
        tasks
            .iter()
            .map(|(id, call)| {
                let mut task = subagent_task(id, call.unwrap_or_default());
                task.call = call.map(Into::into);
                task
            })
            .collect(),
    )
}

/// Each logged call's id and parent, by its title.
fn logged_calls(kinds: &[EventKind]) -> HashMap<String, (CallId, Option<CallId>)> {
    kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::ToolCall(ToolCall {
                call: Some(call),
                parent,
                display,
                ..
            }) => Some((display.title.clone(), (*call, *parent))),
            _ => None,
        })
        .collect()
}

/// Each logged result's call, by its preview.
fn result_calls(kinds: &[EventKind]) -> Vec<(String, Option<CallId>)> {
    kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::ToolResult(r) => Some((r.preview.clone(), r.call)),
            _ => None,
        })
        .collect()
}

fn message_parents(kinds: &[EventKind]) -> Vec<(String, Option<CallId>)> {
    kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::AgentMessage { text, parent } => Some((text.clone(), *parent)),
            _ => None,
        })
        .collect()
}

fn unanswered(kinds: &[EventKind]) -> Vec<Vec<CallId>> {
    kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::CallsUnanswered { calls } => Some(calls.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_subagent_links_to_its_agent_call_across_turns_and_runs() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let mut shell = task("sh", false);
    shell.parent = Some("t1".into());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            named_call("agent", None),
            // A background subagent's call is answered at once.
            answer("agent"),
            // First reported before its call is known, then again with it.
            subagents(&[("t1", None)]),
            subagents(&[("t1", Some("agent"))]),
            sub_text("first run", "agent"),
            named_call("s1", Some("agent")),
            answer("s1"),
            // It stops inside the turn, its shell still running.
            AdapterEvent::BackgroundTasks(vec![shell.clone()]),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            // The shell wakes it again, outside any turn.
            AdapterEvent::BackgroundTasks(vec![subagent_task("t1", "agent"), shell.clone()]),
            named_call("s2", Some("agent")),
            answer("s2"),
            sub_text("second run", "agent"),
            AdapterEvent::BackgroundTasks(vec![]),
        ],
    );
    let kinds = f.kinds();
    let calls = logged_calls(&kinds);
    let (agent, no_parent) = calls["agent"];
    assert_eq!(no_parent, None);
    assert_eq!(calls["s1"].1, Some(agent));
    // Would be None had the root been dropped when the turn ended.
    assert_eq!(calls["s2"].1, Some(agent));
    assert_eq!(
        result_calls(&kinds),
        [
            ("agent".to_owned(), Some(agent)),
            ("s1".to_owned(), Some(calls["s1"].0)),
            ("s2".to_owned(), Some(calls["s2"].0)),
        ]
    );
    assert_eq!(
        message_parents(&kinds),
        [
            ("first run".to_owned(), Some(agent)),
            ("second run".to_owned(), Some(agent)),
        ]
    );
    let task_calls: Vec<Vec<Option<CallId>>> = kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::BackgroundTasksChanged { tasks } => {
                Some(tasks.iter().map(|t| t.call).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        task_calls,
        [
            vec![None],
            vec![Some(agent)],
            vec![None],
            vec![Some(agent), None],
            vec![],
        ]
    );
    // Everything was answered.
    assert_eq!(unanswered(&kinds), Vec::<Vec<CallId>>::new());
}

#[test]
fn unknown_parents_and_task_calls_map_to_none() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            named_call("agent", None),
            answer("agent"),
            subagents(&[("t1", Some("agent")), ("t2", Some("ghost"))]),
            // Positive controls name the known call.
            sub_text("known", "agent"),
            sub_text("unknown", "ghost"),
            named_call("linked", Some("agent")),
            named_call("orphan", Some("ghost")),
            answer("linked"),
            answer("orphan"),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ],
    );
    let kinds = f.kinds();
    let calls = logged_calls(&kinds);
    let agent = calls["agent"].0;
    assert_eq!(calls["linked"].1, Some(agent));
    assert_eq!(calls["orphan"].1, None);
    assert_eq!(
        message_parents(&kinds),
        [
            ("known".to_owned(), Some(agent)),
            ("unknown".to_owned(), None),
        ]
    );
    let EventKind::BackgroundTasksChanged { tasks } = &kinds[3] else {
        panic!("expected the task set, got {:?}", kinds[3]);
    };
    assert_eq!(
        tasks.iter().map(|t| t.call).collect::<Vec<_>>(),
        [Some(agent), None]
    );
}

#[test]
fn a_turn_names_its_open_calls_and_a_task_its_own_when_it_leaves() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            named_call("agent", None),
            answer("agent"),
            subagents(&[("t1", Some("agent"))]),
            named_call("main-open", None),
            named_call("main-done", None),
            answer("main-done"),
            named_call("sub-open", Some("agent")),
            named_call("sub-done", Some("agent")),
            answer("sub-done"),
            // A foreground subagent: no task, its steps inside the turn.
            named_call("fg", None),
            named_call("fg-open", Some("fg")),
            named_call("fg-done", Some("fg")),
            answer("fg-done"),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ],
    );
    let kinds = f.kinds();
    let calls = logged_calls(&kinds);
    let id = |name: &str| calls[name].0;
    let n = kinds.len();
    // The background subagent's open call is not the turn's.
    assert_eq!(
        kinds[n - 2],
        EventKind::CallsUnanswered {
            calls: vec![id("main-open"), id("fg"), id("fg-open")]
        }
    );
    assert_eq!(kinds[n - 1], state(SessionState::Idle));

    // It is named when its task leaves.
    apply_all(&mut f, &mut mapper, vec![subagents(&[])]);
    let kinds = f.kinds();
    assert_eq!(
        kinds[n..],
        [
            EventKind::CallsUnanswered {
                calls: vec![id("sub-open")]
            },
            changed(vec![]),
        ]
    );

    // With every call answered, nothing is named: not at a task's leaving,
    // not at the turn's end.
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            subagents(&[("t1", Some("agent"))]),
            named_call("again", Some("agent")),
            answer("again"),
            subagents(&[]),
            AdapterEvent::TurnStarted,
            named_call("quiet", None),
            answer("quiet"),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ],
    );
    let kinds = f.kinds();
    assert_eq!(unanswered(&kinds).len(), 2);
    assert_eq!(kinds.last(), Some(&state(SessionState::Idle)));
}

#[test]
fn a_task_leaving_in_a_turn_names_its_calls_once() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            named_call("agent", None),
            answer("agent"),
            subagents(&[("t1", Some("agent"))]),
            named_call("sub-open", Some("agent")),
            named_call("main-open", None),
            subagents(&[]),
            // Late, but the turn still holds the call: linked.
            answer("sub-open"),
            AdapterEvent::TurnEnded(TurnOutcome::Failed("boom".into())),
        ],
    );
    let kinds = f.kinds();
    let calls = logged_calls(&kinds);
    assert_eq!(
        unanswered(&kinds),
        [vec![calls["sub-open"].0], vec![calls["main-open"].0]]
    );
    assert_eq!(
        result_calls(&kinds).last(),
        Some(&("sub-open".to_owned(), Some(calls["sub-open"].0)))
    );
}

#[test]
fn a_backgrounded_foreground_subagent_leaves_the_turn() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            named_call("fg", None),
            named_call("fg-open", Some("fg")),
            named_call("main-open", None),
            // The user backgrounds it: it enters the set under its call.
            subagents(&[("t1", Some("fg"))]),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            subagents(&[]),
        ],
    );
    let kinds = f.kinds();
    let calls = logged_calls(&kinds);
    assert_eq!(
        unanswered(&kinds),
        [
            vec![calls["fg"].0, calls["main-open"].0],
            vec![calls["fg-open"].0],
        ]
    );
}

#[test]
fn exit_names_every_open_call() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            named_call("agent", None),
            answer("agent"),
            subagents(&[("t1", Some("agent"))]),
            named_call("sub-open", Some("agent")),
            named_call("sub-done", Some("agent")),
            answer("sub-done"),
            named_call("main-open", None),
            AdapterEvent::ToolCall {
                call: None,
                parent: None,
                name: "tool".into(),
                input: json!({ "name": "no-id" }),
                display: None,
            },
            AdapterEvent::Exited {
                error: None,
                unsent: 0,
            },
        ],
    );
    let kinds = f.kinds();
    let calls = logged_calls(&kinds);
    let id = |name: &str| calls[name].0;
    let n = kinds.len();
    assert_eq!(
        kinds[n - 3..],
        [
            EventKind::CallsUnanswered {
                calls: vec![id("sub-open"), id("main-open"), id("no-id")]
            },
            changed(vec![]),
            state(SessionState::Errored {
                message: "agent exited during a turn".into()
            }),
        ]
    );

    // Nothing is open after an exit, or with nothing logged.
    let mut fresh = Fixture::new();
    let mut idle = task_mapper(&fresh, AgentWork::default());
    apply_all(
        &mut fresh,
        &mut idle,
        vec![
            AdapterEvent::TurnStarted,
            named_call("done", None),
            answer("done"),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            AdapterEvent::Exited {
                error: None,
                unsent: 0,
            },
        ],
    );
    assert_eq!(unanswered(&fresh.kinds()), Vec::<Vec<CallId>>::new());
}

// ---------------------------------------------------------------------------
// Task stops

fn stop_refused(task: &str, answers: CommandId) -> EventKind {
    EventKind::TaskStopRefused {
        task: TaskId(task.to_owned()),
        reason: "no".into(),
        answers: Some(answers),
    }
}

fn refuse(task: &str) -> AdapterEvent {
    AdapterEvent::TaskStopRefused {
        task: task.to_owned(),
        reason: "no".into(),
    }
}

fn answered(task: &str) -> AdapterEvent {
    AdapterEvent::TaskStopAnswered {
        task: task.to_owned(),
    }
}

#[test]
fn a_refusal_answers_its_own_stop_after_a_success() {
    for (first, second) in [("a", "b"), ("a", "a")] {
        let mut f = Fixture::new();
        let mut mapper = task_mapper(&f, AgentWork::default());
        let (one, two) = (CommandId::new(), CommandId::new());
        mapper.asked_stop(TaskId(first.into()), Some(one));
        mapper.asked_stop(TaskId(second.into()), Some(two));
        // The success logs nothing but retires the first stop, so the
        // refusal names the second.
        apply_all(&mut f, &mut mapper, vec![answered(first), refuse(second)]);
        assert_eq!(
            f.kinds(),
            [stop_refused(second, two)],
            "{first} then {second}"
        );
    }
}

#[test]
fn a_refusal_alone_answers_the_oldest_stop() {
    // Positive control for the test above: with no success first, the
    // refusal names the first command.
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let (one, two) = (CommandId::new(), CommandId::new());
    mapper.asked_stop(TaskId("a".into()), Some(one));
    mapper.asked_stop(TaskId("b".into()), Some(two));
    apply_all(&mut f, &mut mapper, vec![refuse("a"), refuse("b")]);
    assert_eq!(f.kinds(), [stop_refused("a", one), stop_refused("b", two)]);
}

#[test]
fn exit_drops_the_stops_left_unanswered() {
    let mut f = Fixture::new();
    let work = AgentWork {
        tasks: vec![logged_task("a", "sleep")],
        schedules: vec![],
    };
    let mut mapper = task_mapper(&f, work);
    mapper.asked_stop(TaskId("a".into()), Some(CommandId::new()));
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Exited {
            error: None,
            unsent: 0,
        }],
    );
    // The empty set settles the stop; no refusal.
    assert_eq!(f.kinds(), [changed(vec![])]);
    // A stop asked of the next run is not answered by the dropped one.
    let next = CommandId::new();
    mapper.asked_stop(TaskId("b".into()), Some(next));
    apply_all(&mut f, &mut mapper, vec![refuse("b")]);
    assert_eq!(f.kinds()[1..], [stop_refused("b", next)]);
}

// ---------------------------------------------------------------------------
// Agent version

#[test]
fn an_unconfirmed_agent_version_is_logged_cut_to_its_cap() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let long = "9".repeat(AGENT_VERSION_MAX_CHARS + 5);
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::VersionUnconfirmed {
            version: long,
            confirmed: "2.1.310".into(),
        }],
    );
    let [EventKind::AgentVersionUnconfirmed { version, confirmed }] = &f.kinds()[..] else {
        panic!("expected one AgentVersionUnconfirmed: {:?}", f.kinds());
    };
    assert_eq!(confirmed, "2.1.310");
    assert_eq!(version.chars().count(), AGENT_VERSION_MAX_CHARS);
    assert!(version.ends_with('…'), "{version}");
}

// ---------------------------------------------------------------------------
// Schedules

fn agent_schedule(id: &str, outlives_agent: Option<bool>) -> AgentSchedule {
    AgentSchedule {
        id: id.to_owned(),
        when: "*/5 * * * *".to_owned(),
        recurring: true,
        prompt: "check".to_owned(),
        outlives_agent,
    }
}

#[test]
fn an_equal_schedule_set_is_logged_once_and_a_different_one_again() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let one = || AdapterEvent::Schedules(vec![agent_schedule("s1", Some(false))]);
    apply_all(&mut f, &mut mapper, vec![one(), one()]);
    assert_eq!(
        f.kinds(),
        [EventKind::SchedulesChanged {
            schedules: vec![schedule("s1", Some(false))]
        }]
    );
    // Positive control: a different set logs, the empty one included.
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::Schedules(vec![]),
            AdapterEvent::Schedules(vec![]),
        ],
    );
    assert_eq!(f.kinds().len(), 2);
    assert_eq!(
        f.kinds()[1],
        EventKind::SchedulesChanged { schedules: vec![] }
    );
}

#[test]
fn schedules_are_cut_to_the_limits() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    let mut long = agent_schedule("long", None);
    long.when = "w".repeat(SCHEDULE_WHEN_MAX_CHARS + 5);
    long.prompt = format!("{}\nsecond line", "p".repeat(TITLE_MAX_CHARS + 20));
    let mut schedules = vec![
        agent_schedule(&"i".repeat(TASK_ID_MAX_CHARS + 1), None),
        long,
    ];
    schedules.extend((0..SCHEDULES_MAX + 1).map(|n| agent_schedule(&format!("s{n}"), None)));
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Schedules(schedules)],
    );
    let kinds = f.kinds();
    let [EventKind::SchedulesChanged { schedules }] = kinds.as_slice() else {
        panic!("expected one change");
    };
    assert_eq!(schedules.len(), SCHEDULES_MAX);
    // The too-long id is gone; the rest keep the agent's order.
    assert_eq!(schedules[0].id.0, "long");
    assert_eq!(chars(&schedules[0].when), SCHEDULE_WHEN_MAX_CHARS);
    assert!(schedules[0].when.ends_with('…'));
    assert_eq!(chars(&schedules[0].prompt), TITLE_MAX_CHARS);
    assert!(!schedules[0].prompt.contains('\n'));
    assert_eq!(schedules[1].id.0, "s0");
    assert_eq!(
        schedules.last().unwrap().id.0,
        format!("s{}", SCHEDULES_MAX - 2)
    );
}

// ---------------------------------------------------------------------------
// Background approvals

fn asked(request: &str, task: Option<&str>) -> AdapterEvent {
    AdapterEvent::ApprovalRequested {
        request: request.into(),
        call: None,
        tool: "write".into(),
        input: json!(request),
        suggestions: Vec::new(),
        task: task.map(str::to_owned),
    }
}

/// The ids of the approvals logged, by their summary (the request's input).
fn approval_ids(f: &Fixture) -> HashMap<String, ApprovalId> {
    f.kinds()
        .into_iter()
        .filter_map(|k| match k {
            EventKind::ApprovalRequested { id, summary, .. } => Some((summary, id)),
            _ => None,
        })
        .collect()
}

fn cancelled_ids(f: &Fixture) -> Vec<(ApprovalId, String)> {
    f.kinds()
        .into_iter()
        .filter_map(|k| match k {
            EventKind::ApprovalCancelled { id, reason } => Some((id, reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_tasks_approval_outlives_the_turn_and_exit_cancels_every_approval() {
    let mut f = Fixture::new();
    let mut mapper = task_mapper(&f, AgentWork::default());
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            asked("task", Some("sub")),
            asked("turn", None),
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
        ],
    );
    let ids = approval_ids(&f);
    // The turn's end cancels the turn's own leftover only.
    assert_eq!(cancelled_ids(&f), [(ids["turn"], "turn ended".to_owned())]);
    assert_eq!(
        mapper.pending_approvals().collect::<Vec<_>>(),
        [ids["task"]]
    );
    // A later turn's end leaves it too.
    apply_all(
        &mut f,
        &mut mapper,
        vec![
            AdapterEvent::TurnStarted,
            AdapterEvent::TurnEnded(TurnOutcome::Interrupted),
            AdapterEvent::TurnStarted,
            asked("next", None),
        ],
    );
    assert_eq!(cancelled_ids(&f).len(), 1);
    // Exit cancels both, the task's and the open turn's.
    apply_all(
        &mut f,
        &mut mapper,
        vec![AdapterEvent::Exited {
            error: None,
            unsent: 0,
        }],
    );
    let ids = approval_ids(&f);
    assert_eq!(
        cancelled_ids(&f)[1..],
        [
            (ids["task"], "agent exited".to_owned()),
            (ids["next"], "agent exited".to_owned()),
        ]
    );
    assert!(f.log.pending_approvals(&f.storage.db).unwrap().is_empty());
}
