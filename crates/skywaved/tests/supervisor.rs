//! Agent supervisor (T10) with the fake agent: commands go through
//! `EventLog::handle` as the HTTPS API would send them, and the supervisor
//! does the rest; a few run the Claude Code adapter against
//! `tests/support/fake-claude.sh` instead. Waits are driven by the log's notifier with a bound, never
//! by fixed sleeps.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use skywave_proto::{
    ApprovalId, Command, CommandId, CommandKind, CommandResult, ContextId, Decision, DeviceId,
    Effort, EventKind, ModelChoice, PermissionMode, RejectReason, RuleScope, RuleSuggestion, Seq,
    SessionId, SessionState, ToolResult,
};
use skywaved::agent::AdapterEvent;
use skywaved::agent::claude_code::ClaudeCodeAdapter;
use skywaved::agent::fake::{
    FakeAgent, FakeInput, FakeScript, INTERRUPT_REASON, REFUSE_REASON, Step,
};
use skywaved::config::AgentConfig;
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
                context: ContextId::first_of(self.session_id),
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

    /// Sends a `SetPermissionMode` and returns its command id.
    fn set_mode(&mut self, mode: PermissionMode) -> CommandId {
        self.command(CommandKind::SetPermissionMode {
            session_id: self.session_id,
            mode,
        })
    }

    /// Sends a `SetModel` and returns its command id.
    fn set_model(&mut self, model: &str) -> CommandId {
        self.command(CommandKind::SetModel {
            session_id: self.session_id,
            model: model.into(),
        })
    }

    /// Sends a `SetEffort` and returns its command id.
    fn set_effort(&mut self, effort: Effort) -> CommandId {
        self.command(CommandKind::SetEffort {
            session_id: self.session_id,
            effort,
        })
    }

    fn command(&mut self, kind: CommandKind) -> CommandId {
        let command = Command {
            id: CommandId::new(),
            kind,
        };
        accepted(
            self.log
                .handle(&mut self.storage.db, DeviceId::new(), &command)
                .unwrap(),
        );
        command.id
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
        skywaved::agent::load_resume_handle(&f.storage.db, ContextId::first_of(f.session_id))
            .unwrap(),
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
            mode: None,
            model: None,
            effort: None,
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

// ---------------------------------------------------------------------------
// The Claude Code adapter, against tests/support/fake-claude.sh

fn claude_adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(AgentConfig {
        command: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-claude.sh"),
        args: Vec::new(),
    })
    .with_grace_period(Duration::from_secs(5))
}

/// The fake CLI's process id, from `pid.txt` in the session workdir.
fn fake_cli_pid(f: &Fixture) -> u32 {
    std::fs::read_to_string(f.root.join("project/pid.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether process `pid` exists, a zombie included (`kill -0`).
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn start_claude(f: &Fixture) -> SupervisorHandle {
    Supervisor::start(
        claude_adapter(),
        &f.data,
        f.log.clone(),
        SupervisorConfig::default(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn daemon_restart_cancels_a_claude_approval_left_pending_at_shutdown() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;

    f.send("please approve");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();
    let pid = fake_cli_pid(&f);
    supervisor.shutdown().await.unwrap();
    // The agent is gone, and was not interrupted: nothing was cancelled.
    assert!(!process_exists(pid), "agent process {pid} still exists");
    assert_eq!(f.log.pending_approvals(&f.storage.db).unwrap().len(), 1);
    assert!(
        !f.kinds()
            .iter()
            .any(|k| matches!(k, EventKind::ApprovalCancelled { .. }))
    );

    f.reopen_log();
    let supervisor = start_claude(&f).await;
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
    assert_eq!(
        f.resolve(DeviceId::new(), id, Decision::Approve),
        CommandResult::Rejected {
            reason: RejectReason::ApprovalCancelled
        }
    );

    // The next message resumes the agent.
    f.send("again");
    f.wait_count("idle", 1, is_idle).await;
    let pid = fake_cli_pid(&f);
    supervisor.shutdown().await.unwrap();
    assert!(!process_exists(pid), "agent process {pid} still exists");
}

fn is_setting_answer(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::PermissionModeChanged { .. }
            | EventKind::PermissionModeRefused { .. }
            | EventKind::ModelChanged { .. }
            | EventKind::ModelRefused { .. }
            | EventKind::EffortChanged { .. }
            | EventKind::EffortRefused { .. }
    )
}

/// The answers about mode, model and effort in `kinds`, in order.
fn setting_answers(kinds: &[EventKind]) -> Vec<EventKind> {
    kinds
        .iter()
        .filter(|k| is_setting_answer(k))
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_setting_starts_an_idle_session_and_logs_the_answer() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;

    let plan = f.set_mode(PermissionMode::Plan);
    let kinds = f.wait_count("mode answer", 1, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds),
        [EventKind::PermissionModeChanged {
            mode: PermissionMode::Plan,
            answers: Some(plan),
        }]
    );
    // Started for it, no turn ran.
    assert!(
        !kinds
            .iter()
            .any(|k| *k == state(SessionState::AgentWorking))
    );

    // While it runs, a request goes straight to it.
    let model = f.set_model("model-b");
    let kinds = f.wait_count("model answer", 2, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds)[1],
        EventKind::ModelChanged {
            model: "model-b".into(),
            answers: Some(model),
        }
    );
    let settings = f.log.agent_settings(&f.storage.db, f.session_id).unwrap();
    assert_eq!(settings.mode.current, Some(PermissionMode::Plan));
    assert_eq!(settings.model.current.as_deref(), Some("model-b"));
    assert_eq!(
        (settings.mode.requested, settings.model.requested),
        (None, None)
    );

    f.send("hello");
    f.wait_count("reply", 1, is_idle).await;
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            FakeInput::SetPermissionMode(PermissionMode::Plan),
            FakeInput::SetModel("model-b".into()),
            message("hello"),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_refused_setting_keeps_what_the_agent_had() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .refuse_mode(PermissionMode::Bypass)
            .refuse_model("bogus"),
    );
    let supervisor = f.start(&agent).await;

    let edits = f.set_mode(PermissionMode::AcceptEdits);
    f.wait_count("first answer", 1, is_setting_answer).await;
    let bypass = f.set_mode(PermissionMode::Bypass);
    let bogus = f.set_model("bogus");
    let kinds = f.wait_count("answers", 3, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds),
        [
            EventKind::PermissionModeChanged {
                mode: PermissionMode::AcceptEdits,
                answers: Some(edits),
            },
            EventKind::PermissionModeRefused {
                mode: PermissionMode::Bypass,
                reason: REFUSE_REASON.into(),
                answers: Some(bypass),
            },
            EventKind::ModelRefused {
                model: "bogus".into(),
                reason: REFUSE_REASON.into(),
                answers: Some(bogus),
            },
        ]
    );
    let settings = f.log.agent_settings(&f.storage.db, f.session_id).unwrap();
    assert_eq!(settings.mode.current, Some(PermissionMode::AcceptEdits));
    assert_eq!(settings.mode.wanted(), Some(&PermissionMode::AcceptEdits));
    assert_eq!(settings.model.current, None);
    assert_eq!(
        settings.model.refused.map(|r| r.value).as_deref(),
        Some("bogus")
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_the_newest_of_queued_requests_goes_out() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    // All logged before the supervisor reads any: a message, then two modes.
    f.send("hello");
    f.set_mode(PermissionMode::Plan);
    let edits = f.set_mode(PermissionMode::AcceptEdits);
    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        setting_answers(&kinds),
        [EventKind::PermissionModeChanged {
            mode: PermissionMode::AcceptEdits,
            answers: Some(edits),
        }]
    );
    assert_eq!(
        agent.inputs(f.session_id)[..3],
        [
            FakeInput::Started { resumed: false },
            FakeInput::SetPermissionMode(PermissionMode::AcceptEdits),
            message("hello"),
        ]
    );
}

#[tokio::test]
async fn a_resumed_agent_gets_its_mode_and_model_back() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.set_mode(PermissionMode::AcceptEdits);
    f.set_model("model-b");
    f.wait_count("answers", 2, is_setting_answer).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !agent.inputs(f.session_id).contains(&FakeInput::Shutdown) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "idle agent not shut down"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    f.send("again");
    let kinds = f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    let inputs = agent.inputs(f.session_id);
    let resumed = inputs
        .iter()
        .position(|i| *i == FakeInput::Started { resumed: true })
        .expect("resumed");
    assert_eq!(
        inputs[resumed..resumed + 4],
        [
            FakeInput::Started { resumed: true },
            FakeInput::SetPermissionMode(PermissionMode::AcceptEdits),
            FakeInput::SetModel("model-b".into()),
            message("again"),
        ]
    );
    // Restoring what the log already has logs nothing new.
    assert_eq!(setting_answers(&kinds).len(), 2);
}

#[tokio::test]
async fn a_setting_accepted_while_down_is_carried_out_on_restart() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let plan = f.set_mode(PermissionMode::Plan);

    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("answer", 1, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds),
        [EventKind::PermissionModeChanged {
            mode: PermissionMode::Plan,
            answers: Some(plan),
        }]
    );
    supervisor.shutdown().await.unwrap();
}

/// Waits until the session's agent has been shut down `n` times.
async fn wait_for_shutdowns(agent: &FakeAgent, f: &Fixture, n: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    let shutdowns = || {
        agent
            .inputs(f.session_id)
            .iter()
            .filter(|i| **i == FakeInput::Shutdown)
            .count()
    };
    while shutdowns() < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "idle agent not shut down"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn an_effort_starts_an_idle_session_and_requests_are_answered_in_order() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().refuse_effort(Effort::Max));
    let supervisor = f.start(&agent).await;

    let high = f.set_effort(Effort::High);
    f.wait_count("first answer", 1, is_setting_answer).await;
    // Each goes out once the one before it was asked, so each is answered.
    let low = f.set_effort(Effort::Low);
    f.wait_count("second answer", 2, is_setting_answer).await;
    let max = f.set_effort(Effort::Max);
    let kinds = f.wait_count("third answer", 3, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds),
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
                reason: REFUSE_REASON.into(),
                answers: Some(max),
            },
        ]
    );
    // Started for it, no turn ran.
    assert!(
        !kinds
            .iter()
            .any(|k| *k == state(SessionState::AgentWorking))
    );
    let settings = f.log.agent_settings(&f.storage.db, f.session_id).unwrap();
    assert_eq!(settings.effort.current, Some(Some(Effort::Low)));
    assert_eq!(settings.effort.requested, None);
    assert_eq!(
        agent.inputs(f.session_id),
        [
            FakeInput::Started { resumed: false },
            FakeInput::SetEffort(Effort::High),
            FakeInput::SetEffort(Effort::Low),
            FakeInput::SetEffort(Effort::Max),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_the_newest_of_queued_efforts_goes_out() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    // Both logged before the supervisor reads either.
    f.set_effort(Effort::Low);
    let high = f.set_effort(Effort::High);
    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("answer", 1, is_setting_answer).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        setting_answers(&kinds),
        [EventKind::EffortChanged {
            effort: Some(Effort::High),
            answers: Some(high),
        }]
    );
    assert_eq!(
        agent.inputs(f.session_id)[..2],
        [
            FakeInput::Started { resumed: false },
            FakeInput::SetEffort(Effort::High),
        ]
    );
}

#[tokio::test]
async fn a_restart_refuses_an_effort_request_it_had_handed_out() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let lost = f.set_effort(Effort::High);
    // The daemon handled the request and stopped before the agent answered.
    let seq = f.log.command_seq(&f.storage.db, lost).unwrap().unwrap();
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE supervisor_cursor SET seq = ?1 WHERE id = 1",
            [i64::try_from(seq.0).unwrap()],
        )
        .unwrap();

    let supervisor = f.start(&agent).await;
    // Settled by the time `start` returns.
    assert_eq!(
        setting_answers(&f.kinds()),
        [EventKind::EffortRefused {
            effort: Effort::High,
            reason: DAEMON_RESTARTED.into(),
            answers: Some(lost),
        }]
    );
    // One the supervisor had not reached yet is carried out as usual.
    let low = f.set_effort(Effort::Low);
    let kinds = f.wait_count("answer", 2, is_setting_answer).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        setting_answers(&kinds)[1],
        EventKind::EffortChanged {
            effort: Some(Effort::Low),
            answers: Some(low),
        }
    );
    assert_eq!(
        agent.inputs(f.session_id)[..2],
        [
            FakeInput::Started { resumed: false },
            FakeInput::SetEffort(Effort::Low),
        ]
    );
}

#[tokio::test]
async fn a_restarted_agent_is_asked_for_the_effort_the_log_wants() {
    let mut f = Fixture::new();
    // The first turn's model takes no effort level.
    let agent = FakeAgent::new(
        FakeScript::new().turn([Step::Emit(AdapterEvent::Effort(None)), Step::text("done")]),
    );
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    // Accepted while down: the start carries the open request out.
    let high = f.set_effort(Effort::High);
    let supervisor = f.start_with(&agent, config).await;
    f.wait_count("answer", 1, is_setting_answer).await;
    wait_for_shutdowns(&agent, &f, 1).await;

    // Resumed: asked for what it last ran with, which logs nothing new.
    f.send("one");
    f.wait_count("reply", 1, is_idle).await;
    wait_for_shutdowns(&agent, &f, 2).await;

    // Resumed after reporting no effort level: asked for none.
    f.send("two");
    let kinds = f.wait_count("reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id)[..8],
        [
            FakeInput::Started { resumed: false },
            FakeInput::SetEffort(Effort::High),
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
            FakeInput::SetEffort(Effort::High),
            message("one"),
            FakeInput::Shutdown,
            FakeInput::Started { resumed: true },
        ]
    );
    assert_eq!(agent.inputs(f.session_id)[8], message("two"));
    assert_eq!(
        setting_answers(&kinds),
        [
            EventKind::EffortChanged {
                effort: Some(Effort::High),
                answers: Some(high),
            },
            EventKind::EffortChanged {
                effort: None,
                answers: None,
            },
        ]
    );
}

#[tokio::test]
async fn a_claude_session_switches_its_effort_and_settles_each_request() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;
    let high = f.set_effort(Effort::High);
    let kinds = f.wait_count("answer", 1, is_setting_answer).await;
    // The start asked for it, so the answer names the command; the look the
    // handshake wrote came back mid-chain and logged nothing before it.
    assert_eq!(
        setting_answers(&kinds),
        [EventKind::EffortChanged {
            effort: Some(Effort::High),
            answers: Some(high),
        }]
    );
    // While it runs, a request goes straight to it.
    let low = f.set_effort(Effort::Low);
    let kinds = f.wait_count("answer", 2, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds)[1],
        EventKind::EffortChanged {
            effort: Some(Effort::Low),
            answers: Some(low),
        }
    );
    // On a model that takes none, the look after the switch is logged
    // unasked, and an effort is refused.
    let model = f.set_model("no-effort");
    let kinds = f.wait_count("answer", 4, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds)[2..],
        [
            EventKind::ModelChanged {
                model: "resolved-no-effort".into(),
                answers: Some(model),
            },
            EventKind::EffortChanged {
                effort: None,
                answers: None,
            },
        ]
    );
    let max = f.set_effort(Effort::Max);
    let kinds = f.wait_count("answer", 5, is_setting_answer).await;
    assert_eq!(
        setting_answers(&kinds)[4],
        EventKind::EffortRefused {
            effort: Effort::Max,
            reason: "resolved-no-effort takes no effort level".into(),
            answers: Some(max),
        }
    );
    let pid = fake_cli_pid(&f);
    supervisor.shutdown().await.unwrap();
    assert!(!process_exists(pid), "agent process {pid} still exists");
    // What the refusal's read showed was already in the log.
    assert_eq!(setting_answers(&f.kinds()).len(), 5);
}

#[tokio::test]
async fn offered_models_are_logged_only_when_they_change() {
    let mut f = Fixture::new();
    let models = [ModelChoice {
        id: "fast".into(),
        label: "Fast".into(),
        detail: None,
        resolved: Some("model-f".into()),
        efforts: Vec::new(),
    }];
    let agent = FakeAgent::new(FakeScript::new().offer_models(models.clone()));
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config.clone()).await;
    f.send("one");
    f.wait_count("reply", 1, is_idle).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !agent.inputs(f.session_id).contains(&FakeInput::Shutdown) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "idle agent not shut down"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f.send("two");
    f.wait_count("reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();

    // A daemon restart knows the list too.
    f.reopen_log();
    let supervisor = f.start_with(&agent, config).await;
    f.send("three");
    let kinds = f.wait_count("reply", 3, is_idle).await;
    supervisor.shutdown().await.unwrap();
    let offered: Vec<_> = kinds
        .iter()
        .filter(|k| matches!(k, EventKind::ModelsOffered { .. }))
        .collect();
    assert_eq!(
        offered,
        [&EventKind::ModelsOffered {
            models: models.to_vec()
        }]
    );
}

#[tokio::test]
async fn a_blank_model_is_refused() {
    let mut f = Fixture::new();
    let session_id = f.session_id;
    assert_eq!(
        f.handle(
            DeviceId::new(),
            CommandKind::SetModel {
                session_id,
                model: " \n ".into(),
            },
        ),
        CommandResult::Rejected {
            reason: RejectReason::UnknownModel
        }
    );
    assert_eq!(
        f.handle(
            DeviceId::new(),
            CommandKind::SetPermissionMode {
                session_id: SessionId::new(),
                mode: PermissionMode::Plan,
            },
        ),
        CommandResult::Rejected {
            reason: RejectReason::UnknownSession
        }
    );
}
