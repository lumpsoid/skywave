//! Agent supervisor (T10) with the fake agent: commands go through
//! `EventLog::handle` as the HTTPS API would send them, and the supervisor
//! does the rest. Waits are driven by the log's notifier with a bound, never
//! by fixed sleeps.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use skywave_proto::{
    ApprovalId, Command, CommandId, CommandKind, CommandResult, Decision, DeviceId, EventKind,
    RejectReason, RuleScope, RuleSuggestion, Seq, SessionId, SessionState, ToolResult,
};
use skywaved::agent::fake::{FakeAgent, FakeInput, FakeScript, INTERRUPT_REASON, Step};
use skywaved::supervisor::{DAEMON_RESTARTED, TURN_LOST};
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle};
use tempfile::TempDir;

/// Upper bound for any wait; a passing test never gets near it.
const WAIT: Duration = Duration::from_secs(10);

struct Fixture {
    _tmp: TempDir,
    data: PathBuf,
    root: PathBuf,
    storage: Storage,
    log: EventLog,
    session_id: SessionId,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let workdir = root.join("project");
        std::fs::create_dir_all(&workdir).unwrap();
        let data = tmp.path().join("data");
        let storage = Storage::open(&data).unwrap();
        let log = EventLog::open(&storage.db, vec![root.clone()]).unwrap();
        let mut f = Fixture {
            _tmp: tmp,
            data,
            root,
            storage,
            log,
            session_id: SessionId::new(),
        };
        let seq = accepted(f.handle(
            DeviceId::new(),
            CommandKind::CreateSession {
                workdir: workdir.to_str().unwrap().to_owned(),
            },
        ));
        f.session_id = f.log.read(&f.storage.db, Seq(seq.0 - 1), 1).unwrap().events[0].session_id;
        f
    }

    async fn start(&self, agent: &FakeAgent) -> SupervisorHandle {
        self.start_with(agent, SupervisorConfig::default()).await
    }

    async fn start_with(&self, agent: &FakeAgent, config: SupervisorConfig) -> SupervisorHandle {
        Supervisor::start(agent.clone(), &self.data, self.log.clone(), config)
            .await
            .unwrap()
    }

    /// Simulates a daemon restart: a new process has a new `EventLog`.
    fn reopen_log(&mut self) {
        self.log = EventLog::open(&self.storage.db, vec![self.root.clone()]).unwrap();
    }

    fn handle(&mut self, device: DeviceId, kind: CommandKind) -> CommandResult {
        let command = Command {
            id: CommandId::new(),
            kind,
        };
        self.log
            .handle(&mut self.storage.db, device, &command)
            .unwrap()
    }

    fn send(&mut self, text: &str) -> Seq {
        accepted(self.handle(
            DeviceId::new(),
            CommandKind::SendMessage {
                session_id: self.session_id,
                text: text.into(),
            },
        ))
    }

    fn resolve(&mut self, device: DeviceId, id: ApprovalId, decision: Decision) -> CommandResult {
        self.resolve_rule(device, id, decision, None)
    }

    fn resolve_rule(
        &mut self,
        device: DeviceId,
        id: ApprovalId,
        decision: Decision,
        rule: Option<u8>,
    ) -> CommandResult {
        self.handle(device, CommandKind::ResolveApproval { id, decision, rule })
    }

    fn interrupt(&mut self) -> CommandResult {
        self.handle(
            DeviceId::new(),
            CommandKind::Interrupt {
                session_id: self.session_id,
            },
        )
    }

    fn stop(&mut self) -> CommandResult {
        self.handle(
            DeviceId::new(),
            CommandKind::StopSession {
                session_id: self.session_id,
            },
        )
    }

    /// The session's log after `SessionCreated`.
    fn kinds(&self) -> Vec<EventKind> {
        self.log
            .read(&self.storage.db, Seq::ZERO, 5000)
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.session_id == self.session_id)
            .map(|e| e.kind)
            .skip(1)
            .collect()
    }

    /// Waits until the session's log satisfies `done`, and returns it.
    async fn wait_for(&self, what: &str, done: impl Fn(&[EventKind]) -> bool) -> Vec<EventKind> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let head = self.log.head();
            let kinds = self.kinds();
            if done(&kinds) {
                return kinds;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; log: {kinds:#?}"
            );
            self.log.wait_after(head, WAIT).await;
        }
    }

    /// Waits until the log has `n` events matching `kind` after `SessionCreated`.
    async fn wait_count(
        &self,
        what: &str,
        n: usize,
        kind: impl Fn(&EventKind) -> bool,
    ) -> Vec<EventKind> {
        self.wait_for(what, |k| k.iter().filter(|e| kind(e)).count() >= n)
            .await
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

fn state(state: SessionState) -> EventKind {
    EventKind::SessionStateChanged(state)
}

fn is_idle(kind: &EventKind) -> bool {
    *kind == state(SessionState::Idle)
}

fn is_approval_requested(kind: &EventKind) -> bool {
    matches!(kind, EventKind::ApprovalRequested { .. })
}

fn is_errored(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::SessionStateChanged(SessionState::Errored { .. })
    )
}

fn approval_script() -> FakeScript {
    FakeScript::new().turn([
        Step::text("Writing a file."),
        Step::approval("req-1", "write", json!({ "path": "notes.txt" }))
            .on_approve([Step::tool_result("written"), Step::text("Done.")])
            .on_deny([Step::text("Not writing it.")]),
    ])
}

fn message(text: &str) -> FakeInput {
    FakeInput::Message(text.into())
}

#[tokio::test]
async fn full_turn_through_commands() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;

    f.send("write notes");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();
    accepted(f.resolve(DeviceId::new(), id, Decision::Approve));
    let kinds = f.wait_count("idle", 1, is_idle).await;

    assert!(matches!(&kinds[0], EventKind::UserMessage { text, .. } if text == "write notes"));
    assert_eq!(kinds[1], state(SessionState::AgentWorking));
    assert_eq!(
        kinds[2],
        EventKind::AgentMessage {
            text: "Writing a file.".into()
        }
    );
    assert!(matches!(&kinds[3], EventKind::ApprovalRequested { tool, .. } if tool == "write"));
    assert!(matches!(
        &kinds[4],
        EventKind::ApprovalResolved {
            decision: Decision::Approve,
            ..
        }
    ));
    assert!(
        matches!(&kinds[5], EventKind::ToolResult(ToolResult { preview, .. }) if preview == "written")
    );
    assert_eq!(
        kinds[6],
        EventKind::AgentMessage {
            text: "Done.".into()
        }
    );
    assert_eq!(kinds[7], state(SessionState::Idle));
    assert_eq!(kinds.len(), 8);
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("write notes"),
            FakeInput::Answer("req-1".into(), Decision::Approve, None),
        ]
    );
    // The resume handle was stored for later restarts.
    assert_eq!(
        skywaved::agent::load_resume_handle(&f.storage.db, f.session_id).unwrap(),
        Some(FakeAgent::resume_handle(f.session_id))
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_granted_rule_is_logged_and_reaches_the_agent() {
    let mut f = Fixture::new();
    let script =
        FakeScript::new().turn([
            Step::approval("req-1", "write", json!({ "path": "notes.txt" }))
                .suggest("write(notes.txt)", RuleScope::Project)
                .suggest("all edits", RuleScope::Session)
                .on_approve([Step::tool_result("written")]),
        ]);
    let agent = FakeAgent::new(script);
    let supervisor = f.start(&agent).await;

    f.send("write notes");
    let kinds = f.wait_count("approval", 1, is_approval_requested).await;
    let offered: Vec<_> = kinds
        .iter()
        .find_map(|k| match k {
            EventKind::ApprovalRequested { suggestions, .. } => Some(suggestions.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        offered,
        [
            RuleSuggestion {
                label: "write(notes.txt)".into(),
                scope: RuleScope::Project,
            },
            RuleSuggestion {
                label: "all edits".into(),
                scope: RuleScope::Session,
            },
        ]
    );
    let id = f.approval_id();
    let device = DeviceId::new();
    accepted(f.resolve_rule(device, id, Decision::Approve, Some(1)));
    let kinds = f.wait_count("idle", 1, is_idle).await;
    assert!(kinds.contains(&EventKind::ApprovalResolved {
        id,
        decision: Decision::Approve,
        by: device,
        rule: Some(1),
    }));
    let answers: Vec<_> = agent
        .inputs(f.session_id)
        .into_iter()
        .filter(|i| matches!(i, FakeInput::Answer(..)))
        .collect();
    assert_eq!(
        answers,
        [FakeInput::Answer(
            "req-1".into(),
            Decision::Approve,
            Some(1)
        )]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_of_two_devices_answering_wins() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;
    let (phone, laptop) = (DeviceId::new(), DeviceId::new());

    f.send("write notes");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();
    accepted(f.resolve(phone, id, Decision::Deny));
    assert_eq!(
        f.resolve(laptop, id, Decision::Approve),
        CommandResult::Rejected {
            reason: RejectReason::ApprovalAlreadyResolved
        }
    );
    let kinds = f.wait_count("idle", 1, is_idle).await;

    let resolved: Vec<_> = kinds
        .iter()
        .filter(|k| matches!(k, EventKind::ApprovalResolved { .. }))
        .collect();
    assert_eq!(
        resolved,
        [&EventKind::ApprovalResolved {
            id,
            decision: Decision::Deny,
            by: phone,
            rule: None,
        }]
    );
    assert!(kinds.contains(&EventKind::AgentMessage {
        text: "Not writing it.".into()
    }));
    let answers: Vec<_> = agent
        .inputs(f.session_id)
        .into_iter()
        .filter(|i| matches!(i, FakeInput::Answer(..)))
        .collect();
    assert_eq!(
        answers,
        [FakeInput::Answer("req-1".into(), Decision::Deny, None)]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupt_with_pending_approval_returns_to_idle() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;

    f.send("write notes");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();
    accepted(f.interrupt());
    let kinds = f.wait_count("idle", 1, is_idle).await;

    let tail = &kinds[kinds.len() - 3..];
    assert_eq!(
        tail,
        [
            EventKind::TurnInterrupted,
            EventKind::ApprovalCancelled {
                id,
                reason: INTERRUPT_REASON.into()
            },
            state(SessionState::Idle),
        ]
    );
    // The cancelled approval can no longer be answered.
    assert_eq!(
        f.resolve(DeviceId::new(), id, Decision::Approve),
        CommandResult::Rejected {
            reason: RejectReason::ApprovalCancelled
        }
    );
    assert_eq!(
        f.interrupt(),
        CommandResult::Rejected {
            reason: RejectReason::NoActiveTurn
        }
    );

    // The agent is still usable for the next turn.
    f.send("again");
    let kinds = f.wait_count("second idle", 2, is_idle).await;
    assert!(kinds.contains(&EventKind::AgentMessage {
        text: "echo: again".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("write notes"),
            FakeInput::Interrupt,
            message("again"),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_mid_turn_then_next_message_resumes() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new().turn([Step::text("Starting."), Step::Exit(Some("boom".into()))]),
    );
    let supervisor = f.start(&agent).await;

    f.send("first");
    let kinds = f.wait_count("errored", 1, is_errored).await;
    assert_eq!(
        kinds.last(),
        Some(&state(SessionState::Errored {
            message: "agent exited: boom".into()
        }))
    );

    f.send("second");
    let kinds = f.wait_count("idle", 1, is_idle).await;
    assert!(kinds.contains(&EventKind::AgentMessage {
        text: "echo: second".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("first"),
            FakeInput::Started { resumed: true },
            message("second"),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_turn_keeps_the_agent_running() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Fail("tool broke".into())]));
    let supervisor = f.start(&agent).await;

    f.send("first");
    f.wait_count("errored", 1, is_errored).await;
    f.send("second");
    f.wait_count("idle", 1, is_idle).await;
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("first"),
            message("second"),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_restart_cancels_pending_approvals_and_resumes() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;

    f.send("write notes");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();
    supervisor.shutdown().await.unwrap();
    // Nothing is settled until the next start.
    assert_eq!(f.log.pending_approvals(&f.storage.db).unwrap().len(), 1);

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    // Settled by the time `start` returns.
    let kinds = f.kinds();
    assert_eq!(
        kinds[kinds.len() - 2..],
        [
            EventKind::ApprovalCancelled {
                id,
                reason: DAEMON_RESTARTED.into()
            },
            state(SessionState::Errored {
                message: TURN_LOST.into()
            }),
        ]
    );
    assert!(f.log.pending_approvals(&f.storage.db).unwrap().is_empty());
    assert!(
        !f.log
            .session(&f.storage.db, f.session_id)
            .unwrap()
            .unwrap()
            .turn_active
    );
    assert_eq!(
        f.resolve(DeviceId::new(), id, Decision::Approve),
        CommandResult::Rejected {
            reason: RejectReason::ApprovalCancelled
        }
    );

    f.send("again");
    let kinds = f.wait_count("idle", 1, is_idle).await;
    assert!(kinds.contains(&EventKind::AgentMessage {
        text: "echo: again".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("write notes"),
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
            message("again"),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_restart_delivers_pending_messages_once() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;

    f.send("one");
    f.wait_count("first reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();

    // Accepted while no supervisor runs.
    f.send("two");
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("second reply", 2, is_idle).await;
    assert!(kinds.contains(&EventKind::AgentMessage {
        text: "echo: two".into()
    }));
    supervisor.shutdown().await.unwrap();

    // A third run redelivers nothing: only the new message reaches the agent.
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    f.send("three");
    f.wait_count("third reply", 3, is_idle).await;
    supervisor.shutdown().await.unwrap();

    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("one"),
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
            message("two"),
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
            message("three"),
            FakeInput::Shutdown,
        ]
    );
    // No restart left a turn to settle.
    assert!(!f.kinds().iter().any(is_errored));
}

#[tokio::test]
async fn interrupt_accepted_while_down_ends_idle_on_restart() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;

    f.send("hang");
    f.wait_for("working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    supervisor.shutdown().await.unwrap();
    accepted(f.interrupt());

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    let kinds = f.kinds();
    assert_eq!(
        kinds[kinds.len() - 2..],
        [EventKind::TurnInterrupted, state(SessionState::Idle)]
    );
    assert!(!kinds.iter().any(is_errored));
    supervisor.shutdown().await.unwrap();
    // The replayed interrupt reached no agent.
    assert!(!agent.inputs(f.session_id).contains(&FakeInput::Interrupt));
}

#[tokio::test]
async fn idle_agent_is_shut_down_and_resumed_on_next_message() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.send("one");
    f.wait_count("first reply", 1, is_idle).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !agent.inputs(f.session_id).contains(&FakeInput::Shutdown) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "idle agent not shut down"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    f.send("two");
    f.wait_count("second reply", 2, is_idle).await;
    // Whether the idle timer or the shutdown stops it, it stops once.
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("one"),
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
            message("two"),
            FakeInput::Shutdown,
        ]
    );
}

#[tokio::test]
async fn stop_mid_turn_interrupts_shuts_down_and_resumes() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;

    f.send("write notes");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();
    accepted(f.stop());
    let kinds = f.wait_count("idle", 1, is_idle).await;

    // Interrupted, not lost: no `Errored`.
    let tail = &kinds[kinds.len() - 3..];
    assert_eq!(
        tail,
        [
            EventKind::SessionStopped,
            EventKind::ApprovalCancelled {
                id,
                reason: INTERRUPT_REASON.into()
            },
            state(SessionState::Idle),
        ]
    );
    assert!(!kinds.iter().any(is_errored), "{kinds:#?}");

    f.send("again");
    let kinds = f.wait_count("second idle", 2, is_idle).await;
    assert!(kinds.contains(&EventKind::AgentMessage {
        text: "echo: again".into()
    }));
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("write notes"),
            FakeInput::Interrupt,
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
            message("again"),
            FakeInput::Shutdown,
        ]
    );
}

#[tokio::test]
async fn stop_without_a_running_agent_does_nothing() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;

    accepted(f.stop());
    f.send("one");
    f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            message("one"),
            FakeInput::Shutdown,
        ]
    );
    assert_eq!(
        f.handle(
            DeviceId::new(),
            CommandKind::StopSession {
                session_id: SessionId::new()
            }
        ),
        CommandResult::Rejected {
            reason: RejectReason::UnknownSession
        }
    );
}

#[tokio::test]
async fn agent_that_cannot_start_errors_the_session() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    // A session the fake already knows cannot be started fresh again.
    use skywaved::agent::{AgentAdapter, StartSpec};
    let workdir = f
        .log
        .session(&f.storage.db, f.session_id)
        .unwrap()
        .unwrap()
        .workdir;
    let _ = agent
        .start(StartSpec {
            session_id: f.session_id,
            workdir: workdir.into(),
            resume: None,
        })
        .await
        .unwrap();
    let supervisor = f.start(&agent).await;

    f.send("hello");
    let kinds = f.wait_count("errored", 1, is_errored).await;
    assert!(matches!(
        kinds.last(),
        Some(EventKind::SessionStateChanged(SessionState::Errored { message }))
            if message.starts_with("cannot start agent")
    ));
    supervisor.shutdown().await.unwrap();
}
