//! Agent supervisor (T10) with the fake agent: commands go through
//! `EventLog::handle` as the HTTPS API would send them, and the supervisor
//! does the rest; a few run the Claude Code adapter against
//! `tests/support/fake-claude.sh` instead. Waits are driven by the log's notifier with a bound, never
//! by fixed sleeps.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use skywave_proto::limits::TASKS_MAX;
use skywave_proto::{
    AgentWork, ApprovalId, CallId, Command, CommandId, CommandKind, CommandResult, ContextId,
    Decision, DeviceId, Effort, Event, EventKind, Full, ModelChoice, PermissionMode, RejectReason,
    RuleScope, RuleSuggestion, Schedule, ScheduleId, Seq, SessionId, SessionOrigin, SessionState,
    TaskId, ToolCall, ToolDisplay, ToolKind, ToolResult,
};
use skywaved::agent::claude_code::ClaudeCodeAdapter;
use skywaved::agent::fake::{
    FakeAgent, FakeConversation, FakeInput, FakeOutside, FakeScript, Gate, INTERRUPT_REASON,
    REFUSE_REASON, Step, schedule, task,
};
use skywaved::agent::import::{self, IMPORT_DEADLINE};
use skywaved::agent::{AdapterEvent, AgentCallId, HistoryItem, HistoryRead, INTERRUPT_UNANSWERED};
use skywaved::config::AgentConfig;
use skywaved::control::{Confirm, Control, Reply, Stop, Work};
use skywaved::disk::{Disk, FreeSpace};
use skywaved::supervisor::{
    AGENT_NOT_RUNNING, BYPASS_NOT_ALLOWED, DAEMON_RESTARTED, MESSAGE_LOST, TURN_LOST,
};
use skywaved::{EventLog, Storage, Supervisor, SupervisorConfig, SupervisorHandle};
use tempfile::TempDir;

#[path = "support/control.rs"]
mod control;
use control::{TestClient, stop_after_work, stop_now};

/// Upper bound for any wait; a passing test never gets near it.
const WAIT: Duration = Duration::from_secs(10);

struct Fixture {
    _tmp: TempDir,
    data: PathBuf,
    root: PathBuf,
    storage: Storage,
    log: EventLog,
    session_id: SessionId,
    /// The disk the log watches, and the free space it is checked against.
    disk: Disk,
    space: Arc<FakeSpace>,
}

/// The free space a test sets, as [`FreeSpace`] reports it, counting its
/// reads.
#[derive(Debug, Default)]
struct FakeSpace {
    free: AtomicU64,
    reads: AtomicUsize,
}

impl FreeSpace for FakeSpace {
    fn free(&self) -> std::io::Result<u64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.free.load(Ordering::SeqCst))
    }
}

/// The fixture's `[disk] min_free`.
const FLOOR: u64 = 1 << 30;

impl Fixture {
    fn new() -> Fixture {
        Fixture::created(None)
    }

    /// A fixture whose session adopts the outside agent conversation `id`.
    fn adopting(id: &str) -> Fixture {
        Fixture::created(Some(id.into()))
    }

    /// A fixture whose session adopts the outside agent conversation `id`,
    /// importing `history` as the API does.
    fn importing(id: &str, history: Vec<HistoryItem>) -> Fixture {
        Fixture::created_with(Some(id.into()), Some(history))
    }

    /// A fixture with one session, adopting `conversation` if any.
    fn created(conversation: Option<String>) -> Fixture {
        Fixture::created_with(conversation, None)
    }

    fn created_with(conversation: Option<String>, history: Option<Vec<HistoryItem>>) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let workdir = root.join("project");
        std::fs::create_dir_all(&workdir).unwrap();
        let data = tmp.path().join("data");
        let storage = Storage::open(&data).unwrap();
        let space = Arc::new(FakeSpace::default());
        space.free.store(FLOOR, Ordering::SeqCst);
        let disk = Disk::new(space.clone(), FLOOR);
        let log = EventLog::open(&storage.db, vec![root.clone()])
            .unwrap()
            .with_disk(disk.clone());
        let mut f = Fixture {
            _tmp: tmp,
            data,
            root,
            storage,
            log,
            session_id: SessionId::new(),
            disk,
            space,
        };
        let workdir = workdir.to_str().unwrap().to_owned();
        let prepared = match (&conversation, history) {
            (Some(id), Some(items)) => {
                let source = FakeOutside::new();
                source.conversation(Path::new(&workdir), id, HistoryRead::Read { items });
                let deadline = std::time::Instant::now() + IMPORT_DEADLINE;
                let blobs = f.storage.blobs.clone();
                Some(import::prepare(&source, blobs, &workdir, id, deadline))
            }
            _ => None,
        };
        let command = Command {
            id: CommandId::new(),
            kind: CommandKind::CreateSession {
                workdir,
                conversation,
            },
        };
        let seq = accepted(
            f.log
                .handle_with(
                    &mut f.storage.db,
                    DeviceId::new(),
                    &command,
                    prepared.map(Into::into),
                )
                .unwrap(),
        );
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
        self.log = EventLog::open(&self.storage.db, vec![self.root.clone()])
            .unwrap()
            .with_disk(self.disk.clone());
    }

    /// Leaves `free` bytes on the disk and reads it now, rather than at the
    /// next write or poll.
    fn disk_free(&self, free: u64) {
        self.space.free.store(free, Ordering::SeqCst);
        self.disk.check();
    }

    /// How often the free space was read.
    fn disk_reads(&self) -> usize {
        self.space.reads.load(Ordering::SeqCst)
    }

    /// The disk drops below the floor.
    fn disk_low(&self) {
        self.disk_free(FLOOR - 1);
        assert!(self.disk.is_low());
    }

    /// The disk is back at the floor.
    fn disk_freed(&self) {
        self.disk_free(FLOOR);
        assert!(!self.disk.is_low());
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

    /// Sends `text` in the session's current context.
    fn send(&mut self, text: &str) -> Seq {
        let context = self.current();
        accepted(self.handle(
            DeviceId::new(),
            CommandKind::SendMessage {
                session_id: self.session_id,
                context,
                text: text.into(),
            },
        ))
    }

    /// The session's current context.
    fn current(&self) -> ContextId {
        self.log
            .session(&self.storage.db, self.session_id)
            .unwrap()
            .unwrap()
            .context
    }

    /// Clears the session into a new context `context`; the marker's seq.
    fn clear(&mut self, context: ContextId) -> Seq {
        accepted(self.handle(
            DeviceId::new(),
            CommandKind::ClearContext {
                session_id: self.session_id,
                context,
                name: "next".into(),
            },
        ))
    }

    /// Switches the session to its context `context`; the marker's seq.
    fn switch(&mut self, context: ContextId) -> Seq {
        accepted(self.handle(
            DeviceId::new(),
            CommandKind::SwitchContext {
                session_id: self.session_id,
                context,
            },
        ))
    }

    /// The session's events after `seq`, with their seqs.
    fn events_after(&self, seq: Seq) -> Vec<(Seq, Event)> {
        let batch = self.log.read(&self.storage.db, seq, 5000).unwrap();
        (batch.from.0..)
            .map(Seq)
            .zip(batch.events)
            .filter(|(_, e)| e.session_id == self.session_id)
            .collect()
    }

    /// The session's events after `seq`, each kind with its stamp.
    fn stamped_after(&self, seq: Seq) -> Vec<(EventKind, ContextId)> {
        self.events_after(seq)
            .into_iter()
            .map(|(_, e)| (e.kind, e.context))
            .collect()
    }

    fn turn_context(&self) -> (bool, Option<ContextId>) {
        let info = self
            .log
            .session(&self.storage.db, self.session_id)
            .unwrap()
            .unwrap();
        (info.turn_active, info.turn_context)
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

    /// Creates another session, with a workdir of its own; its id.
    fn create_session(&mut self) -> SessionId {
        let workdir = self.root.join("other");
        std::fs::create_dir_all(&workdir).unwrap();
        let seq = accepted(self.handle(
            DeviceId::new(),
            CommandKind::CreateSession {
                workdir: workdir.to_str().unwrap().to_owned(),
                conversation: None,
            },
        ));
        self.log
            .read(&self.storage.db, Seq(seq.0 - 1), 1)
            .unwrap()
            .events[0]
            .session_id
    }

    /// The session's first context, the one it was created with.
    /// Makes every write of the supervisor's cursor fail as a full disk
    /// would, for every connection to the database, until
    /// [`Fixture::disk_has_room`].
    fn cursor_disk_full(&self) {
        self.storage
            .db
            .conn()
            .execute_batch(
                "CREATE TRIGGER disk_full BEFORE UPDATE ON supervisor_cursor
                 BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
            )
            .unwrap();
    }

    fn disk_has_room(&self) {
        self.storage
            .db
            .conn()
            .execute_batch("DROP TRIGGER disk_full;")
            .unwrap();
    }

    /// Makes every blob write fail, as a full disk would, by putting a file
    /// where `blobs/` was (refused even to root), until
    /// [`Fixture::blobs_writable`].
    fn blobs_unwritable(&self) {
        let blobs = self.data.join("blobs");
        std::fs::remove_dir(&blobs).unwrap();
        std::fs::write(&blobs, b"").unwrap();
    }

    /// Makes recording any blob fail as a full disk would, for every
    /// connection to the database; `blobs/` stays a directory, so the
    /// daemon's storage still opens.
    fn blob_rows_refused(&self) {
        self.storage
            .db
            .conn()
            .execute_batch(
                "CREATE TRIGGER blobs_full BEFORE INSERT ON blobs
                 BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
            )
            .unwrap();
    }

    fn blobs_writable(&self) {
        let blobs = self.data.join("blobs");
        std::fs::remove_file(&blobs).unwrap();
        std::fs::create_dir(&blobs).unwrap();
    }

    fn first(&self) -> ContextId {
        ContextId::first_of(self.session_id)
    }

    /// The fake's record of a start in the session's first context.
    fn started(&self, resumed: bool) -> FakeInput {
        FakeInput::Started {
            conversation: fake_conversation(resumed),
            context: self.first(),
            grants: None,
        }
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

/// What the fake records for a start resumed or (`false`) fresh.
fn fake_conversation(resumed: bool) -> FakeConversation {
    if resumed {
        FakeConversation::Resume
    } else {
        FakeConversation::Fresh
    }
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
            parent: None,
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
            parent: None,
            text: "Done.".into()
        }
    );
    assert_eq!(kinds[7], state(SessionState::Idle));
    assert_eq!(kinds.len(), 8);
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("write notes"),
            FakeInput::Answer("req-1".into(), Decision::Approve, None),
        ]
    );
    // The resume handle was stored for later restarts.
    assert_eq!(
        skywaved::agent::load_resume_handle(&f.storage.db, ContextId::first_of(f.session_id))
            .unwrap(),
        Some(FakeAgent::resume_handle(f.first()))
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
        parent: None,
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
        parent: None,
        text: "echo: again".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
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
        parent: None,
        text: "echo: second".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("first"),
            f.started(true),
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
        [f.started(false), message("first"), message("second"),]
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
        parent: None,
        text: "echo: again".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("write notes"),
            FakeInput::Shutdown,
            f.started(true),
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
        parent: None,
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
            f.started(false),
            message("one"),
            FakeInput::Shutdown,
            f.started(true),
            message("two"),
            FakeInput::Shutdown,
            f.started(true),
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

/// Stores `seq` as the supervisor's cursor and the message there as handed
/// over with its turn not started, as a daemon that handed it over and
/// stopped before it heard more would have left them.
fn set_cursor(f: &Fixture, seq: Seq) {
    let conn = f.storage.db.conn();
    conn.execute(
        "UPDATE supervisor_cursor SET seq = ?1, accounted = ?1 WHERE id = 1",
        [i64::try_from(seq.0).unwrap()],
    )
    .unwrap();
    set_owed(f, &[seq], "waiting");
}

/// Stores the messages at `seqs` as handed over with their turn not
/// started, in `state`: `waiting`; `starting`: their turn's `AgentWorking`
/// was about to be logged; `settling`: an `Errored` reporting them was
/// about to be logged. Replaces what was stored.
fn set_owed(f: &Fixture, seqs: &[Seq], state: &str) {
    let conn = f.storage.db.conn();
    conn.execute("DELETE FROM supervisor_owed", []).unwrap();
    for &seq in seqs {
        conn.execute(
            "INSERT INTO supervisor_owed (seq, session_id, context, state) \
             SELECT seq, session_id, context, ?2 FROM events WHERE seq = ?1",
            rusqlite::params![i64::try_from(seq.0).unwrap(), state],
        )
        .unwrap();
    }
}

/// Stores the log's head as what the stored messages owed reflect.
fn set_accounted(f: &Fixture) {
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE supervisor_cursor SET accounted = (SELECT MAX(seq) FROM events) WHERE id = 1",
            [],
        )
        .unwrap();
}

/// The messages stored as owed, in seq order.
fn owed_rows(f: &Fixture) -> Vec<i64> {
    let conn = f.storage.db.conn();
    let mut stmt = conn
        .prepare("SELECT seq FROM supervisor_owed ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn message_lost() -> EventKind {
    state(SessionState::Errored {
        message: MESSAGE_LOST.into(),
    })
}

#[tokio::test]
async fn a_message_the_daemon_stopped_before_delivering_errors_once_on_restart() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    // The daemon stored the cursor on the message and died before the agent
    // got it.
    let seq = f.send("lost");
    set_cursor(&f, seq);

    let supervisor = f.start(&agent).await;
    // Settled by the time `start` returns, in the message's context.
    assert_eq!(f.stamped_after(seq), [(message_lost(), f.first())]);
    supervisor.shutdown().await.unwrap();
    // Never redelivered.
    assert!(agent.inputs(f.session_id).is_empty());

    // A second restart finds it settled.
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(f.stamped_after(seq), [(message_lost(), f.first())]);
    assert!(agent.inputs(f.session_id).is_empty());
}

#[tokio::test]
async fn a_message_whose_turn_ran_is_not_errored_on_restart() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    let seq = f.send("answered");
    f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    // As if the daemon died right after the turn, before the cursor moved:
    // the message was marked starting, and stored, before its turn's
    // `AgentWorking` was logged.
    set_cursor(&f, seq);
    set_owed(&f, &[seq], "starting");

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    let kinds = f.kinds();
    assert!(kinds.contains(&state(SessionState::AgentWorking)));
    assert!(!kinds.iter().any(is_errored), "log: {kinds:#?}");
}

#[tokio::test]
async fn a_message_whose_turn_was_lost_errors_once_as_a_lost_turn() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    let seq = f.send("hang");
    f.wait_for("working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    supervisor.shutdown().await.unwrap();
    set_cursor(&f, seq);
    set_owed(&f, &[seq], "starting");

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    let errored: Vec<EventKind> = f.kinds().into_iter().filter(is_errored).collect();
    assert_eq!(
        errored,
        [state(SessionState::Errored {
            message: TURN_LOST.into()
        })]
    );
}

/// The session's `Errored` events after `seq`, with their stamps.
fn errored_after(f: &Fixture, seq: Seq) -> Vec<(EventKind, ContextId)> {
    f.stamped_after(seq)
        .into_iter()
        .filter(|(kind, _)| is_errored(kind))
        .collect()
}

fn errored(message: &str) -> EventKind {
    state(SessionState::Errored {
        message: message.into(),
    })
}

/// Waits until the fake got the message `text` in session `session`.
async fn wait_message(agent: &FakeAgent, session: SessionId, text: &str) {
    let got = message(text);
    wait_inputs(agent, session, text, |inputs| inputs.contains(&got)).await;
}

/// A clean shutdown whose agent ends its turn before it began the messages
/// queued behind it: the next start reports them, once, and never
/// redelivers them.
#[tokio::test]
async fn a_clean_shutdown_before_queued_messages_ran_errors_them_once_on_restart() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]).end_turns_on_shutdown());
    let supervisor = f.start(&agent).await;
    f.send("hang");
    f.wait_for("working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    let one = f.send("one");
    f.send("two");
    wait_message(&agent, f.session_id, "two").await;
    supervisor.shutdown().await.unwrap();
    // The turn ended at the shutdown; nothing was reported yet.
    assert!(errored_after(&f, one).is_empty());
    assert_eq!(owed_rows(&f).len(), 2);

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    let lost = errored("2 messages lost when the server stopped; send them again");
    assert_eq!(errored_after(&f, one), [(lost.clone(), f.first())]);
    assert!(owed_rows(&f).is_empty());
    supervisor.shutdown().await.unwrap();

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, one), [(lost, f.first())]);
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("hang"),
            message("one"),
            message("two"),
            FakeInput::Shutdown,
        ]
    );
}

/// A message queued behind a turn the daemon abandoned is named in the
/// lost turn's one `Errored`.
#[tokio::test]
async fn a_message_queued_behind_a_lost_turn_is_named_in_its_errored() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    let hang = f.send("hang");
    f.wait_for("working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    f.send("queued");
    wait_message(&agent, f.session_id, "queued").await;
    supervisor.shutdown().await.unwrap();

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        errored_after(&f, hang),
        [(errored(&format!("{TURN_LOST}; {ONE_UNSENT}")), f.first())]
    );
}

/// Runs a turn the agent starts on its own (a wakeup) with a message
/// handed while the turn before it ran, so before the wakeup's turn
/// began: the agent hangs in the wakeup's turn, the message queued behind
/// it. Shuts the daemon down cleanly there. The first message's seq and
/// the queued one's.
async fn hand_a_message_before_a_wakeup(f: &mut Fixture) -> (FakeAgent, Seq, Seq) {
    let first_ends = Gate::new();
    // The wakeup is due before the first turn ends, so it runs before the
    // message queued meanwhile, as a CLI's would.
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::wake(Duration::ZERO, [Step::Hang]),
        Step::Wait(first_ends.clone()),
    ]));
    let supervisor = f.start(&agent).await;
    let first = f.send("first");
    let working = |k: &EventKind| *k == state(SessionState::AgentWorking);
    f.wait_count("the first turn", 1, working).await;
    let queued = f.send("queued");
    wait_message(&agent, f.session_id, "queued").await;
    first_ends.open();
    f.wait_count("the wakeup's turn", 2, working).await;
    supervisor.shutdown().await.unwrap();
    assert!(errored_after(f, first).is_empty(), "{:#?}", f.kinds());
    (agent, first, queued)
}

/// A turn the agent starts on its own answers no message: one handed
/// before it began stays owed through it, and a clean shutdown during that
/// turn names the message in the lost turn's one `Errored` on restart.
#[tokio::test]
async fn a_message_queued_behind_a_turn_the_agent_started_errors_once_after_a_shutdown() {
    let mut f = Fixture::new();
    let (agent, first, queued) = hand_a_message_before_a_wakeup(&mut f).await;
    // The wakeup's turn did not take the message.
    assert_eq!(owed_rows(&f), [i64::try_from(queued.0).unwrap()]);

    let lost = errored(&format!("{TURN_LOST}; {ONE_UNSENT}"));
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, first), [(lost.clone(), f.first())]);
    assert!(owed_rows(&f).is_empty());

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, first), [(lost, f.first())]);
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("first"),
            message("queued"),
            FakeInput::Shutdown,
        ]
    );
}

/// The same after a crash: the owed rows were last stored when the message
/// was handed, so the wakeup's `AgentWorking`, logged after them, is read
/// back on restart. It answers no message, so the message is still named
/// in the lost turn's one `Errored`.
#[tokio::test]
async fn a_message_queued_behind_a_turn_the_agent_started_errors_once_after_a_crash() {
    let mut f = Fixture::new();
    let (agent, first, queued) = hand_a_message_before_a_wakeup(&mut f).await;
    // As the run stored them when it handed the message over.
    set_owed(&f, &[queued], "waiting");
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE supervisor_cursor SET accounted = ?1 WHERE id = 1",
            [i64::try_from(queued.0).unwrap()],
        )
        .unwrap();
    let working = |k: &(EventKind, ContextId)| k.0 == state(SessionState::AgentWorking);
    assert_eq!(
        f.stamped_after(queued)
            .iter()
            .filter(|k| working(k))
            .count(),
        1
    );

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        errored_after(&f, first),
        [(errored(&format!("{TURN_LOST}; {ONE_UNSENT}")), f.first())]
    );
}

/// An agent may take a message handed before its own turn began up inside
/// that turn (a CLI does at a tool round): `TurnStarted { message: true }`
/// in the open turn, with no `AgentWorking` of its own. That settles the
/// message at once, so a shutdown during the turn names only the lost turn.
#[tokio::test]
async fn a_message_taken_up_inside_a_turn_the_agent_started_is_settled() {
    let mut f = Fixture::new();
    let first_ends = Gate::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::wake(Duration::ZERO, [Step::TakeUp, Step::Hang]),
        Step::Wait(first_ends.clone()),
    ]));
    let supervisor = f.start(&agent).await;
    let first = f.send("first");
    let working = |k: &EventKind| *k == state(SessionState::AgentWorking);
    f.wait_count("the first turn", 1, working).await;
    let queued = f.send("queued");
    wait_message(&agent, f.session_id, "queued").await;
    assert_eq!(owed_rows(&f), [i64::try_from(queued.0).unwrap()]);
    first_ends.open();
    let answer = |k: &EventKind| {
        *k == EventKind::AgentMessage {
            parent: None,
            text: "echo: queued".into(),
        }
    };
    let kinds = f.wait_count("the message's answer", 1, answer).await;
    // One turn the agent started, which the message joined.
    assert_eq!(kinds.iter().filter(|k| working(k)).count(), 2);
    assert!(owed_rows(&f).is_empty());
    supervisor.shutdown().await.unwrap();

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, first), [(errored(TURN_LOST), f.first())]);
    assert!(owed_rows(&f).is_empty());
}

/// Every session's messages are checked, not only the one handed over
/// last: another session's message handed later does not hide one whose
/// turn never began.
#[tokio::test]
async fn a_restart_errors_a_lost_message_of_a_session_handed_before_another() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]).end_turns_on_shutdown());
    let supervisor = f.start(&agent).await;
    let a = f.session_id;
    f.send("hang a");
    f.wait_for("a working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    let lost = f.send("queued in a");
    wait_message(&agent, a, "queued in a").await;
    let b = f.create_session();
    f.session_id = b;
    let b_seq = f.send("hang b");
    f.wait_for("b working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    supervisor.shutdown().await.unwrap();

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    // B's message ran its turn, which the shutdown ended.
    assert!(errored_after(&f, b_seq).is_empty(), "{:#?}", f.kinds());
    f.session_id = a;
    assert_eq!(errored_after(&f, lost), [(message_lost(), f.first())]);
}

/// Starts complete out of order: a message handed over after another
/// session's later one (a slow start) does not hide that one either.
#[tokio::test]
async fn a_restart_errors_a_lost_message_handed_before_an_earlier_one() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]).end_turns_on_shutdown());
    let supervisor = f.start(&agent).await;
    let a = f.session_id;
    let b = f.create_session();
    let started = Gate::new();
    agent.hold_start(a, started.clone());
    let a_seq = send_in(&mut f, a, "slow start");
    wait_inputs(&agent, a, "a's start", |i| !i.is_empty()).await;
    f.session_id = b;
    f.send("hang b");
    f.wait_for("b working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    let lost = f.send("queued in b");
    wait_message(&agent, b, "queued in b").await;
    assert!(lost > a_seq);
    // A's message, the earlier one, is handed over last.
    started.open();
    wait_message(&agent, a, "slow start").await;
    f.session_id = a;
    f.wait_for("a working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    supervisor.shutdown().await.unwrap();

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert!(errored_after(&f, a_seq).is_empty(), "{:#?}", f.kinds());
    f.session_id = b;
    assert_eq!(
        errored_after(&f, lost),
        [(message_lost(), ContextId::first_of(b))]
    );
}

/// An `Errored` logged before the message was handed over (a stopped
/// agent's exit, logged after the message only because the message waited
/// for the stop) does not answer it: a crash before the agent got the
/// message still reports it.
#[tokio::test]
async fn a_stopped_agents_late_errored_does_not_hide_a_message_handed_after_it() {
    let mut f = Fixture::new();
    // The interrupt ends the first turn; the queued message's turn is
    // still running when the stop ends it, which the exit reports.
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]).turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    f.send("hang");
    f.wait_for("working", |k| {
        k.contains(&state(SessionState::AgentWorking))
    })
    .await;
    f.send("queued");
    wait_message(&agent, f.session_id, "queued").await;
    let stopped = Gate::new();
    agent.hold_shutdown(f.session_id, stopped.clone());
    accepted(f.stop());
    wait_shutdown(&agent, &f, 1).await;
    let during = f.send("during the stop");
    // The stop completes; the start for the message never does.
    agent.hold_start(f.session_id, Gate::new());
    stopped.open();
    f.wait_count("the stopped agent's exit", 1, is_errored)
        .await;
    wait_inputs(&agent, f.session_id, "the second start", |i| {
        i.iter()
            .filter(|i| matches!(i, FakeInput::Started { .. }))
            .count()
            == 2
    })
    .await;
    supervisor.shutdown().await.unwrap();
    // The exit's `Errored` follows the message in the log.
    assert_eq!(errored_after(&f, during).len(), 1);

    // As if the run had handed the message over after logging that exit,
    // and died before the agent got it.
    f.storage
        .db
        .conn()
        .execute("DELETE FROM supervisor_held", [])
        .unwrap();
    set_owed(&f, &[during], "waiting");
    set_accounted(&f);

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    let after = errored_after(&f, during);
    assert_eq!(after.len(), 2, "{after:#?}");
    assert_eq!(after[1], (message_lost(), f.first()));
    assert!(
        !agent
            .inputs(f.session_id)
            .contains(&message("during the stop"))
    );
}

/// A message marked as reported by an `Errored` about to be logged is
/// settled once that `Errored` is in the log after the stored rows, and
/// reported by the restart when it is not.
#[tokio::test]
async fn a_restart_reports_a_settling_message_only_if_its_errored_is_missing() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let seq = f.send("lost");
    let conn = f.storage.db.conn();
    conn.execute(
        "UPDATE supervisor_cursor SET seq = ?1 WHERE id = 1",
        [i64::try_from(seq.0).unwrap()],
    )
    .unwrap();
    // The `Errored` was not logged: the restart reports the message.
    set_owed(&f, &[seq], "settling");
    set_accounted(&f);
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, seq), [(message_lost(), f.first())]);

    // It was logged after the rows were stored: nothing more.
    set_owed(&f, &[seq], "settling");
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE supervisor_cursor SET accounted = ?1 WHERE id = 1",
            [i64::try_from(seq.0).unwrap()],
        )
        .unwrap();
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, seq), [(message_lost(), f.first())]);
    assert!(agent.inputs(f.session_id).is_empty());
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
            f.started(false),
            message("one"),
            FakeInput::Shutdown,
            f.started(true),
            message("two"),
            FakeInput::Shutdown,
        ]
    );
}

/// Daemon shutdown stops every warm agent at once, so it takes one agent's
/// grace period rather than the sum of them. The fake holds each shutdown
/// until both are under way: stopped one after another, the first never
/// returns and the bounded wait fails.
#[tokio::test]
async fn shutdown_stops_the_warm_agents_concurrently() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().shutdowns_wait_for_each_other(2));
    let supervisor = f.start(&agent).await;

    let first = f.session_id;
    f.send("one");
    f.wait_count("the first session's reply", 1, is_idle).await;
    let second = f.create_session();
    f.session_id = second;
    f.send("two");
    f.wait_count("the second session's reply", 1, is_idle).await;
    let logged = |f: &mut Fixture, id: SessionId| {
        f.session_id = id;
        f.kinds()
    };
    let before = [logged(&mut f, first), logged(&mut f, second)];

    tokio::time::timeout(WAIT, supervisor.shutdown())
        .await
        .expect("the agents stopped one after another")
        .unwrap();
    for (id, before) in [first, second].into_iter().zip(before) {
        let inputs = agent.inputs(id);
        assert_eq!(inputs.last(), Some(&FakeInput::Shutdown), "{inputs:?}");
        // Their exit is still left unrecorded for the next start.
        assert_eq!(logged(&mut f, id), before);
    }
}

/// A turn the agent starts on its own (a scheduled wakeup, like `/loop`)
/// keeps it running past the idle deadline of the turn before, and the idle
/// timer starts again once it ends.
#[tokio::test]
async fn a_turn_the_agent_starts_itself_is_not_cut_by_the_idle_timer() {
    const IDLE: Duration = Duration::from_millis(600);
    let mut f = Fixture::new();
    let tick = Gate::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::wake(
            Duration::from_millis(50),
            [Step::Wait(tick.clone()), Step::text("tick")],
        ),
        Step::text("scheduled"),
    ]));
    let config = SupervisorConfig { idle_timeout: IDLE };
    let supervisor = f.start_with(&agent, config).await;

    f.send("loop");
    let working = |k: &EventKind| *k == state(SessionState::AgentWorking);
    f.wait_count("the wakeup's turn", 2, working).await;
    // Proving the agent is not stopped takes waiting past the deadline the
    // first turn armed; a stop there logs the open turn as `Errored`.
    tokio::time::sleep(IDLE + IDLE / 2).await;
    assert!(
        !agent.inputs(f.session_id).contains(&FakeInput::Shutdown),
        "agent stopped mid-turn"
    );
    assert!(!f.kinds().iter().any(is_errored), "log: {:#?}", f.kinds());

    tick.open();
    f.wait_count("the wakeup's end", 2, is_idle).await;
    wait_for_shutdowns(&agent, &f, 1).await;
    supervisor.shutdown().await.unwrap();
    let kinds = f.kinds();
    assert!(!kinds.iter().any(is_errored), "log: {kinds:#?}");
    assert!(kinds.contains(&EventKind::AgentMessage {
        parent: None,
        text: "tick".into()
    }));
    assert_eq!(
        agent.inputs(f.session_id),
        [f.started(false), message("loop"), FakeInput::Shutdown]
    );
}

// ---------------------------------------------------------------------------
// Background tasks keep the agent

/// The idle timeout of the background-task tests: long enough to tell a
/// stop at the deadline from none.
const TASK_IDLE: Duration = Duration::from_millis(600);

fn tasks_changed(f: &Fixture) -> Vec<EventKind> {
    f.kinds()
        .into_iter()
        .filter(|k| matches!(k, EventKind::BackgroundTasksChanged { .. }))
        .collect()
}

fn no_tasks() -> EventKind {
    EventKind::BackgroundTasksChanged { tasks: Vec::new() }
}

fn assert_running(agent: &FakeAgent, f: &Fixture, why: &str) {
    assert!(
        !agent.inputs(f.session_id).contains(&FakeInput::Shutdown),
        "agent stopped {why}; log: {:#?}",
        f.kinds()
    );
}

/// A background job the agent reports at the end of its turn keeps it past
/// the idle timeout, and the timeout counts from when the job ends.
/// `ambient_tasks_do_not_keep_the_agent` is the same script with an ambient
/// task, stopped at the deadline.
#[tokio::test]
async fn a_live_background_task_keeps_the_agent_until_it_ends() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::tasks([task("job", false)]),
        Step::later(
            TASK_IDLE * 5 / 2,
            [AdapterEvent::BackgroundTasks(Vec::new())],
        ),
    ]));
    let config = SupervisorConfig {
        idle_timeout: TASK_IDLE,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.send("run it in the background");
    f.wait_count("the turn's end", 1, is_idle).await;
    tokio::time::sleep(TASK_IDLE * 3 / 2).await;
    assert_running(&agent, &f, "with a task live");

    f.wait_for("the job's end", |k| k.last() == Some(&no_tasks()))
        .await;
    // The clock starts now, not at the turn's end long ago.
    tokio::time::sleep(TASK_IDLE / 2).await;
    assert_running(&agent, &f, "right as its task ended");
    wait_for_shutdowns(&agent, &f, 1).await;
    supervisor.shutdown().await.unwrap();

    assert!(!f.kinds().iter().any(is_errored), "log: {:#?}", f.kinds());
    assert_eq!(tasks_changed(&f).len(), 2, "log: {:#?}", f.kinds());
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("run it in the background"),
            FakeInput::Shutdown
        ]
    );
}

#[tokio::test]
async fn ambient_tasks_do_not_keep_the_agent() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::tasks([task("watch", true)]),
        Step::later(
            TASK_IDLE * 5 / 2,
            [AdapterEvent::BackgroundTasks(Vec::new())],
        ),
    ]));
    let config = SupervisorConfig {
        idle_timeout: TASK_IDLE,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.send("watch it");
    f.wait_count("the turn's end", 1, is_idle).await;
    let idle = tokio::time::Instant::now();
    wait_for_shutdowns(&agent, &f, 1).await;
    // Stopped at the deadline, long before the task's end was due.
    assert!(idle.elapsed() < TASK_IDLE * 2, "{:?}", idle.elapsed());
    supervisor.shutdown().await.unwrap();
    // Its exit ended the ambient task in the log.
    assert_eq!(tasks_changed(&f).last(), Some(&no_tasks()));
}

/// Schedules are information only: an idle agent with a wakeup scheduled
/// stops at its deadline all the same. The positive control is
/// `a_live_background_task_keeps_the_agent_until_it_ends`, the same script
/// with a task.
#[tokio::test]
async fn schedules_do_not_keep_the_agent() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::schedules([schedule("tick")]),
        Step::later(TASK_IDLE * 5 / 2, [AdapterEvent::Schedules(Vec::new())]),
    ]));
    let config = SupervisorConfig {
        idle_timeout: TASK_IDLE,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.send("loop it");
    f.wait_count("the turn's end", 1, is_idle).await;
    let idle = tokio::time::Instant::now();
    wait_for_shutdowns(&agent, &f, 1).await;
    // Stopped at the deadline, long before the schedule's end was due.
    assert!(idle.elapsed() < TASK_IDLE * 2, "{:?}", idle.elapsed());
    supervisor.shutdown().await.unwrap();
    let schedules: Vec<_> = f
        .kinds()
        .into_iter()
        .filter_map(|k| match k {
            EventKind::SchedulesChanged { schedules } => Some(schedules),
            _ => None,
        })
        .collect();
    // The schedule was logged, and its exit emptied the set.
    assert_eq!(schedules.len(), 2, "log: {:#?}", f.kinds());
    assert_eq!(schedules[0][0].id.0, "tick");
    assert_eq!(schedules[1], []);
}

/// An idle agent reporting again that it runs nothing keeps its deadline:
/// the clock started at the turn's end is not restarted.
#[tokio::test]
async fn a_repeated_report_of_no_tasks_does_not_push_the_deadline_back() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::later(
        TASK_IDLE * 2 / 3,
        [AdapterEvent::BackgroundTasks(Vec::new())],
    )]));
    let config = SupervisorConfig {
        idle_timeout: TASK_IDLE,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.send("nothing to run");
    f.wait_count("the turn's end", 1, is_idle).await;
    let idle = tokio::time::Instant::now();
    wait_for_shutdowns(&agent, &f, 1).await;
    // A restarted clock would stop it at 5/3 of the timeout.
    assert!(idle.elapsed() < TASK_IDLE * 3 / 2, "{:?}", idle.elapsed());
    supervisor.shutdown().await.unwrap();
}

/// The log keeps the first `TASKS_MAX` tasks; one past them is still
/// counted. Here the ones the log keeps are all ambient.
#[tokio::test]
async fn a_busy_task_past_the_log_cap_still_keeps_the_agent() {
    let mut f = Fixture::new();
    let mut tasks: Vec<_> = (0..TASKS_MAX)
        .map(|i| task(&format!("watch-{i}"), true))
        .collect();
    tasks.push(task("job", false));
    let agent = FakeAgent::new(FakeScript::new().turn([Step::tasks(tasks)]));
    let config = SupervisorConfig {
        idle_timeout: TASK_IDLE,
    };
    let supervisor = f.start_with(&agent, config).await;

    f.send("run them");
    f.wait_count("the turn's end", 1, is_idle).await;
    let work = f.log.agent_work(&f.storage.db, f.session_id).unwrap();
    assert_eq!(work.tasks.len(), TASKS_MAX);
    assert!(work.tasks.iter().all(|t| t.ambient));
    tokio::time::sleep(TASK_IDLE * 3 / 2).await;
    assert_running(&agent, &f, "with a task past the cap live");
    supervisor.shutdown().await.unwrap();
}

/// Tasks and schedules left in the log by a daemon that stopped are ended
/// on the next start, in the context of the agent that reported them.
#[tokio::test]
async fn a_daemon_restart_ends_the_work_its_agent_left() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::tasks([task("job", false)])]));
    let supervisor = f.start(&agent).await;
    f.send("run it");
    f.wait_count("the turn's end", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();

    // While the daemon is down: a schedule of the same agent, then a clear.
    let schedule = Schedule {
        id: ScheduleId("s1".into()),
        when: "*/5 * * * *".into(),
        recurring: true,
        prompt: "check the build".into(),
        outlives_agent: Some(false),
    };
    f.log
        .append(
            &mut f.storage.db,
            f.session_id,
            a,
            EventKind::SchedulesChanged {
                schedules: vec![schedule],
            },
        )
        .unwrap();
    let marker = f.clear(b);
    let work = f.log.agent_work(&f.storage.db, f.session_id).unwrap();
    assert_eq!((work.tasks.len(), work.schedules.len()), (1, 1));

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    assert_eq!(
        f.stamped_after(marker),
        [
            (no_tasks(), a),
            (
                EventKind::SchedulesChanged {
                    schedules: Vec::new()
                },
                a
            ),
        ]
    );
    assert_eq!(
        f.log.agent_work(&f.storage.db, f.session_id).unwrap(),
        AgentWork::default()
    );
    supervisor.shutdown().await.unwrap();

    // Settled once: a second restart finds nothing to end.
    f.reopen_log();
    let head = f.log.head();
    let supervisor = f.start(&agent).await;
    assert_eq!(f.stamped_after(head), []);
    supervisor.shutdown().await.unwrap();
}

/// A tool call logged in `context`, by `parent`'s subagent if any; its id.
fn log_call(f: &mut Fixture, context: ContextId, parent: Option<CallId>) -> CallId {
    let call = CallId::new();
    f.log
        .append(
            &mut f.storage.db,
            f.session_id,
            context,
            EventKind::ToolCall(ToolCall {
                call: Some(call),
                parent,
                name: "exec".into(),
                display: ToolDisplay {
                    kind: ToolKind::Shell,
                    title: "$ true".into(),
                    detail: None,
                    body: None,
                },
                input: None,
            }),
        )
        .unwrap();
    call
}

/// Logs `call`'s result in `context`.
fn log_result(f: &mut Fixture, context: ContextId, call: CallId) {
    f.log
        .append(
            &mut f.storage.db,
            f.session_id,
            context,
            EventKind::ToolResult(ToolResult {
                call: Some(call),
                is_error: false,
                status: None,
                preview: String::new(),
                lines: 0,
                output: None,
            }),
        )
        .unwrap();
}

fn log_agent(f: &mut Fixture, context: ContextId, kind: EventKind) {
    f.log
        .append(&mut f.storage.db, f.session_id, context, kind)
        .unwrap();
}

/// A turn the daemon died in: its open calls are named, and only those,
/// before the state event that ends the turn.
#[tokio::test]
async fn a_daemon_restart_names_the_calls_its_turn_left_open() {
    let mut f = Fixture::new();
    let a = f.first();
    log_agent(&mut f, a, state(SessionState::AgentWorking));
    let answered = log_call(&mut f, a, None);
    log_result(&mut f, a, answered);
    let first = log_call(&mut f, a, None);
    let named = log_call(&mut f, a, None);
    log_agent(&mut f, a, EventKind::CallsUnanswered { calls: vec![named] });
    let late = log_call(&mut f, a, None);
    log_agent(&mut f, a, EventKind::CallsUnanswered { calls: vec![late] });
    log_result(&mut f, a, late);
    let second = log_call(&mut f, a, None);
    let head = f.log.head();

    f.reopen_log();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    assert_eq!(
        f.stamped_after(head),
        [
            (
                EventKind::CallsUnanswered {
                    calls: vec![first, second]
                },
                a
            ),
            (
                state(SessionState::Errored {
                    message: TURN_LOST.into()
                }),
                a
            ),
        ]
    );
    supervisor.shutdown().await.unwrap();

    // Settled once: a second restart names nothing.
    f.reopen_log();
    let head = f.log.head();
    let supervisor = f.start(&agent).await;
    assert_eq!(f.stamped_after(head), []);
    supervisor.shutdown().await.unwrap();
}

/// An idle session whose background subagent had a call in flight: it is
/// named, though there is no turn to settle.
#[tokio::test]
async fn a_daemon_restart_names_an_idle_sessions_open_subagent_call() {
    let mut f = Fixture::new();
    let a = f.first();
    log_agent(&mut f, a, state(SessionState::AgentWorking));
    let launch = log_call(&mut f, a, None);
    log_result(&mut f, a, launch);
    log_agent(&mut f, a, state(SessionState::Idle));
    let done = log_call(&mut f, a, Some(launch));
    log_result(&mut f, a, done);
    let open = log_call(&mut f, a, Some(launch));
    let head = f.log.head();

    f.reopen_log();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    assert_eq!(
        f.stamped_after(head),
        [(EventKind::CallsUnanswered { calls: vec![open] }, a)]
    );
    supervisor.shutdown().await.unwrap();
}

/// Calls open in two contexts are named in two events, each stamped with
/// the context its calls were logged in, in log order.
#[tokio::test]
async fn a_daemon_restart_names_open_calls_per_context() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let in_a = log_call(&mut f, a, None);
    f.clear(b);
    let in_b = log_call(&mut f, b, None);
    let answered = log_call(&mut f, b, None);
    log_result(&mut f, b, answered);
    let also_in_a = log_call(&mut f, a, Some(in_a));
    let head = f.log.head();

    f.reopen_log();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    assert_eq!(
        f.stamped_after(head),
        [
            (
                EventKind::CallsUnanswered {
                    calls: vec![in_a, also_in_a]
                },
                a
            ),
            (EventKind::CallsUnanswered { calls: vec![in_b] }, b),
        ]
    );
    supervisor.shutdown().await.unwrap();
}

/// Times a daemon restart's recovery over a large log; run by hand
/// (`cargo test --release -p skywaved --test supervisor -- --ignored --nocapture
/// recovery_on_a_large_log`). Each session's turn started at the beginning
/// of its history and the daemon died in it with one call open, so every
/// per-session lookup recovery makes has the whole history behind it. What
/// keeps that cost off the history is checked by the query plans
/// (`event_log::tests::recovery_queries_use_an_index`); this only reports.
#[tokio::test]
#[ignore = "timing, not a check: run by hand"]
async fn recovery_on_a_large_log() {
    const SESSIONS: usize = 10;
    const PAIRS: usize = 5_000;
    let mut f = Fixture::new();
    let mut sessions = vec![f.session_id];
    sessions.extend((1..SESSIONS).map(|_| f.create_session()));
    let seeding = std::time::Instant::now();
    let mut open = Vec::new();
    for &session in &sessions {
        let context = f
            .log
            .session(&f.storage.db, session)
            .unwrap()
            .unwrap()
            .context;
        let mut log = |kind| {
            f.log
                .append(&mut f.storage.db, session, context, kind)
                .unwrap();
        };
        log(state(SessionState::AgentWorking));
        for i in 0..=PAIRS {
            let call = CallId::new();
            log(EventKind::ToolCall(ToolCall {
                call: Some(call),
                parent: None,
                name: "exec".into(),
                display: ToolDisplay {
                    kind: ToolKind::Shell,
                    title: "$ true".into(),
                    detail: None,
                    body: None,
                },
                input: None,
            }));
            if i == PAIRS {
                open.push(call);
                break;
            }
            log(EventKind::ToolResult(ToolResult {
                call: Some(call),
                is_error: false,
                status: None,
                preview: "ok".into(),
                lines: 1,
                output: None,
            }));
        }
    }
    let head = f.log.head();
    eprintln!("seeded {} events in {:?}", head.0, seeding.elapsed());

    f.reopen_log();
    let agent = FakeAgent::new(FakeScript::new());
    let timing = std::time::Instant::now();
    let supervisor = f.start(&agent).await;
    let elapsed = timing.elapsed();
    eprintln!(
        "recovery over {} events in {SESSIONS} sessions: {elapsed:?}",
        head.0
    );
    let after = f.log.read(&f.storage.db, head, 5000).unwrap().events;
    let named: Vec<CallId> = (after.into_iter())
        .filter_map(|event| match event.kind {
            EventKind::CallsUnanswered { calls } => Some(calls),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(named, open);
    supervisor.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Stopping one task

/// Sends a `StopTask` for `task` and returns its command id.
fn stop_task(f: &mut Fixture, task: &str) -> CommandId {
    f.command(CommandKind::StopTask {
        session_id: f.session_id,
        task: TaskId(task.into()),
    })
}

fn is_stop_refused(kind: &EventKind) -> bool {
    matches!(kind, EventKind::TaskStopRefused { .. })
}

/// Runs a turn that leaves tasks `job` and `keep` live, and waits for it.
async fn two_live_tasks(f: &mut Fixture) {
    f.send("run two jobs");
    f.wait_count("the turn's end", 1, is_idle).await;
    assert_eq!(tasks_changed(f).len(), 1, "log: {:#?}", f.kinds());
}

fn two_tasks_script() -> FakeScript {
    FakeScript::new().turn([Step::tasks([task("job", false), task("keep", false)])])
}

#[tokio::test]
async fn a_stop_reaches_the_running_agent_and_the_set_shrinks() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(two_tasks_script());
    let supervisor = f.start(&agent).await;
    two_live_tasks(&mut f).await;

    stop_task(&mut f, "job");
    f.wait_count("the shrunk set", 2, |k| {
        matches!(k, EventKind::BackgroundTasksChanged { .. })
    })
    .await;
    let EventKind::BackgroundTasksChanged { tasks } = tasks_changed(&f).pop().unwrap() else {
        unreachable!()
    };
    let ids: Vec<&str> = tasks.iter().map(|t| t.id.0.as_str()).collect();
    assert_eq!(ids, ["keep"]);
    assert_running(&agent, &f, "with a task still live");
    supervisor.shutdown().await.unwrap();

    // `a_refused_stop_is_logged_answering_its_command` is the positive
    // control: there the same check sees the refusal.
    assert!(
        !f.kinds().iter().any(is_stop_refused),
        "log: {:#?}",
        f.kinds()
    );
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("run two jobs"),
            FakeInput::StopTask("job".into()),
            FakeInput::Shutdown
        ]
    );
}

#[tokio::test]
async fn a_refused_stop_is_logged_answering_its_command() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(two_tasks_script().refuse_stops());
    let supervisor = f.start(&agent).await;
    two_live_tasks(&mut f).await;

    let command = stop_task(&mut f, "job");
    f.wait_count("the refusal", 1, is_stop_refused).await;
    supervisor.shutdown().await.unwrap();
    assert!(f.kinds().contains(&EventKind::TaskStopRefused {
        task: TaskId("job".into()),
        reason: REFUSE_REASON.into(),
        answers: Some(command),
    }));
    assert_eq!(tasks_changed(&f).len(), 1, "the set stays as it was");
}

#[tokio::test]
async fn a_stop_with_no_agent_running_is_refused_without_starting_one() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    // A set the log has with no agent running, as a device last saw it.
    let (session, first) = (f.session_id, f.first());
    f.log
        .append(
            &mut f.storage.db,
            session,
            first,
            EventKind::BackgroundTasksChanged {
                tasks: vec![skywave_proto::BackgroundTask {
                    call: None,
                    id: TaskId("job".into()),
                    kind: skywave_proto::TaskKind::Shell,
                    title: "sleep 60".into(),
                    parent: None,
                    ambient: false,
                }],
            },
        )
        .unwrap();

    let before = f.log.head();
    let command = stop_task(&mut f, "job");
    f.wait_count("the refusal", 1, is_stop_refused).await;
    supervisor.shutdown().await.unwrap();
    let refusal = EventKind::TaskStopRefused {
        task: TaskId("job".into()),
        reason: AGENT_NOT_RUNNING.into(),
        answers: Some(command),
    };
    let after: Vec<_> = f
        .events_after(before)
        .into_iter()
        .map(|(_, e)| (e.kind, e.context, e.command_id))
        .collect();
    let [(requested, _, asked), refused] = after.as_slice() else {
        panic!("expected the request and its refusal: {after:#?}");
    };
    assert!(matches!(requested, EventKind::TaskStopRequested { .. }));
    assert_eq!(*asked, Some(command));
    assert_eq!(*refused, (refusal, first, None));
    assert_eq!(agent.inputs(f.session_id), [], "no agent was started");
}

/// Stopping the agent ends its tasks; the next start reports none and logs
/// nothing for it.
#[tokio::test]
async fn a_stop_ends_the_live_tasks_and_the_next_start_has_none() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([Step::tasks([task("job", false)])])
            .turn([Step::tasks([]), Step::text("fresh")]),
    );
    let supervisor = f.start(&agent).await;
    f.send("run it");
    f.wait_count("the turn's end", 1, is_idle).await;

    // The seq of `SessionStopped`; what follows is the agent's exit.
    let stop = accepted(f.stop());
    f.wait_for("the tasks' end", |k| k.last() == Some(&no_tasks()))
        .await;
    assert_eq!(f.stamped_after(stop), [(no_tasks(), f.first())]);

    f.send("again");
    f.wait_count("second idle", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert!(f.kinds().contains(&EventKind::AgentMessage {
        parent: None,
        text: "fresh".into()
    }));
    // The set, its end at the stop, and nothing for the empty report.
    assert_eq!(tasks_changed(&f).len(), 2, "log: {:#?}", f.kinds());
    assert_eq!(tasks_changed(&f).last(), Some(&no_tasks()));
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
        parent: None,
        text: "echo: again".into()
    }));
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("write notes"),
            FakeInput::Interrupt,
            FakeInput::Shutdown,
            f.started(true),
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
        [f.started(false), message("one"), FakeInput::Shutdown,]
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

/// Has `agent` hold a conversation for `context` that the log knows
/// nothing of: started outside the supervisor and given a message, so no
/// handle is stored and the supervisor's fresh start of `context` fails.
async fn converse_elsewhere(agent: &FakeAgent, f: &Fixture, context: ContextId) {
    use skywaved::agent::{AgentAdapter, AgentSession, Conversation, StartSpec};
    let workdir = f
        .log
        .session(&f.storage.db, f.session_id)
        .unwrap()
        .unwrap()
        .workdir;
    let (mut elsewhere, mut events) = agent
        .start(StartSpec {
            session_id: f.session_id,
            context,
            workdir: workdir.into(),
            conversation: Conversation::Fresh,
            grants: None,
            mode: None,
            model: None,
            effort: None,
        })
        .await
        .unwrap();
    elsewhere.send_message("elsewhere".into()).await.unwrap();
    while !matches!(events.recv().await, Some(AdapterEvent::ResumeHandle(_))) {}
    elsewhere.shutdown().await.unwrap();
}

#[tokio::test]
async fn agent_that_cannot_start_errors_the_session() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    // A context whose conversation the fake already has, with no handle
    // stored, cannot be started fresh again.
    converse_elsewhere(&agent, &f, f.first()).await;
    let supervisor = f.start(&agent).await;

    f.send("hello");
    let kinds = f.wait_count("errored", 1, is_errored).await;
    assert!(matches!(
        kinds.last(),
        Some(EventKind::SessionStateChanged(SessionState::Errored { message }))
            if message.starts_with("cannot start agent")
                // Control: only a conversation that is gone tells the user
                // to start over.
                && !message.contains(skywaved::agent::CONVERSATION_GONE)
    ));
    supervisor.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// The Claude Code adapter, against tests/support/fake-claude.sh

fn claude_adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(AgentConfig {
        command: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-claude.sh"),
        args: Vec::new(),
        allow_bypass: false,
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
async fn a_context_whose_conversation_is_gone_says_to_start_a_new_one() {
    use skywaved::agent::CONVERSATION_GONE;
    use skywaved::agent::claude_code::protocol::resume_handle;
    let mut f = Fixture::new();
    // The context ran a conversation whose transcript the CLI has since
    // deleted: the fake CLI knows no session with the nil id.
    skywaved::agent::store_resume_handle(
        &f.storage.db,
        f.first(),
        &resume_handle(uuid::Uuid::nil()),
    )
    .unwrap();
    let supervisor = start_claude(&f).await;
    let gone = |kind: &EventKind| {
        matches!(
            kind,
            EventKind::SessionStateChanged(SessionState::Errored { message })
                if message.starts_with(CONVERSATION_GONE)
                    && message.contains("No conversation found")
        )
    };

    f.send("hello");
    let kinds = f.wait_count("errored", 1, is_errored).await;
    assert!(gone(kinds.last().unwrap()), "{:?}", kinds.last());
    // Never started fresh behind the user's back.
    assert!(argv_has_resume(&f), "the agent was not resumed");
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, EventKind::AgentMessage { .. }))
    );

    // Each message tries again and is told the same.
    f.send("again");
    let kinds = f.wait_count("errored", 2, is_errored).await;
    assert!(gone(kinds.last().unwrap()), "{:?}", kinds.last());

    // The way out the message names works: a new context starts fresh.
    f.clear(ContextId::new());
    f.send("hello again");
    let kinds = f.wait_count("idle", 1, is_idle).await;
    assert!(!argv_has_resume(&f), "the new context resumed");
    assert_eq!(kinds.iter().filter(|k| is_errored(k)).count(), 2);
    supervisor.shutdown().await.unwrap();
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
    // The call the approval gated never got its result either.
    let gated = kinds
        .iter()
        .find_map(|k| match k {
            EventKind::ApprovalRequested { call, .. } => *call,
            _ => None,
        })
        .unwrap();
    assert_eq!(
        kinds[kinds.len() - 3..],
        [
            EventKind::ApprovalCancelled {
                id,
                reason: DAEMON_RESTARTED.into()
            },
            EventKind::CallsUnanswered { calls: vec![gated] },
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

#[tokio::test]
async fn a_repeated_interrupt_stops_an_agent_that_ignores_the_first() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;
    let said = |text: &'static str| move |k: &EventKind| matches!(k, EventKind::AgentMessage { text: t, .. } if t == text);

    f.send("stuck");
    f.wait_count("working", 1, said("working")).await;
    let pid = fake_cli_pid(&f);
    accepted(f.interrupt());
    // The CLI got the interrupt and ignores it: the turn stays open.
    f.wait_count("the interrupt ignored", 1, said("ignored"))
        .await;
    accepted(f.interrupt());
    let kinds = f.wait_count("errored", 1, is_errored).await;
    assert_eq!(
        kinds.last(),
        Some(&state(SessionState::Errored {
            message: format!("agent exited: {INTERRUPT_UNANSWERED}")
        }))
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::TurnInterrupted)
            .count(),
        2
    );
    assert!(!process_exists(pid), "agent process {pid} still exists");
    // The turn is over: nothing left to interrupt.
    assert_eq!(
        f.interrupt(),
        CommandResult::Rejected {
            reason: RejectReason::NoActiveTurn
        }
    );

    // The next message resumes the agent.
    f.send("again");
    f.wait_count("idle", 1, is_idle).await;
    assert!(argv_has_resume(&f), "the agent was not resumed");
    let pid = fake_cli_pid(&f);
    supervisor.shutdown().await.unwrap();
    assert!(!process_exists(pid), "agent process {pid} still exists");
}

#[tokio::test]
async fn a_message_sent_after_the_agent_left_between_turns_is_answered_once_by_a_new_one() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;
    let echoed =
        |k: &&EventKind| matches!(k, EventKind::AgentMessage { text, .. } if text == "echo");

    // The CLI answers, its result's line ending only with its stdout, and
    // lingers until cued: once the turn's end is logged its agent has seen
    // stdout end, and the message below reaches the supervisor while that
    // agent still runs but will never read it.
    f.send("linger");
    f.wait_count("idle", 1, is_idle).await;
    let left = fake_cli_pid(&f);
    let hello = f.send("hello");
    // The supervisor stores its cursor at the message right before it
    // hands it over, in the same step: only then may the CLI exit.
    wait_read(&f, hello).await;
    std::fs::write(f.root.join("project/linger.fifo"), "go\n").unwrap();
    let kinds = f.wait_count("idle again", 2, is_idle).await;
    assert_ne!(
        fake_cli_pid(&f),
        left,
        "the message went to the agent that left"
    );
    assert!(argv_has_resume(&f), "the agent was not resumed");
    assert_eq!(kinds.iter().filter(echoed).count(), 2);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == state(SessionState::AgentWorking))
            .count(),
        2,
        "the message was not answered exactly once: {kinds:?}"
    );
    assert!(!kinds.iter().any(is_errored), "{kinds:?}");
    let pid = fake_cli_pid(&f);
    supervisor.shutdown().await.unwrap();
    assert!(!process_exists(pid), "agent process {pid} still exists");
}

/// The `Errored` notice for one queued message that never ran.
const ONE_UNSENT: &str = "1 queued message was not sent to the agent; send it again";

/// Sends "again" after whatever ended the agent and waits for its turn:
/// the supervisor handles the message only after it logged that agent's
/// exit, so the returned log holds everything the exit logged.
async fn resume_after_exit(f: &mut Fixture, idle_before: usize) -> Vec<EventKind> {
    f.send("again");
    let kinds = f.wait_count("idle again", idle_before + 1, is_idle).await;
    assert!(argv_has_resume(f), "the agent was not resumed");
    kinds
}

#[tokio::test]
async fn a_stop_reports_a_message_queued_behind_the_turn_once() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;

    f.send("hang");
    f.send("hello");
    accepted(f.stop());
    let kinds = f.wait_count("errored", 1, is_errored).await;
    // The turn ends as interrupted, then the exit names the message it
    // never began.
    assert_eq!(
        kinds[kinds.len() - 2..],
        [
            state(SessionState::Idle),
            state(SessionState::Errored {
                message: ONE_UNSENT.into()
            }),
        ]
    );
    let kinds = resume_after_exit(&mut f, 1).await;
    assert_eq!(kinds.iter().filter(|k| is_errored(k)).count(), 1);
    // "hang" and "again" each ran a turn; "hello" never did.
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == state(SessionState::AgentWorking))
            .count(),
        2,
        "{kinds:#?}"
    );
    supervisor.shutdown().await.unwrap();
}

/// The exit of an agent stopped on request already reported its queued
/// message: a restart does not report it again.
#[tokio::test]
async fn a_restart_does_not_report_a_stopped_agents_queued_message_again() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;

    f.send("hang");
    let hello = f.send("hello");
    accepted(f.stop());
    f.wait_count("errored", 1, is_errored).await;
    supervisor.shutdown().await.unwrap();
    assert!(owed_rows(&f).is_empty());

    f.reopen_log();
    let supervisor = start_claude(&f).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(errored_after(&f, hello), [(errored(ONE_UNSENT), f.first())]);
}

#[tokio::test]
async fn a_stop_with_nothing_queued_reports_nothing_more() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;

    f.send("hang");
    accepted(f.stop());
    f.wait_count("idle", 1, is_idle).await;
    let kinds = resume_after_exit(&mut f, 1).await;
    assert!(!kinds.iter().any(is_errored), "{kinds:#?}");
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_crash_reports_a_message_queued_behind_the_turn_with_the_exit() {
    let mut f = Fixture::new();
    let supervisor = start_claude(&f).await;
    let said = |text: &'static str| move |k: &EventKind| matches!(k, EventKind::AgentMessage { text: t, .. } if t == text);

    f.send("stuck");
    f.wait_count("working", 1, said("working")).await;
    f.send("hello");
    // The interrupt reaches the CLI after the message reached the agent,
    // so once the CLI says "ignored" the message is queued.
    accepted(f.interrupt());
    f.wait_count("the interrupt ignored", 1, said("ignored"))
        .await;
    let pid = fake_cli_pid(&f);
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let kinds = f.wait_count("errored", 1, is_errored).await;
    let Some(EventKind::SessionStateChanged(SessionState::Errored { message })) = kinds.last()
    else {
        panic!("expected Errored last: {kinds:#?}");
    };
    assert!(message.starts_with("agent exited: "), "{message}");
    assert!(message.contains("SIGKILL"), "{message}");
    assert!(message.ends_with(&format!("; {ONE_UNSENT}")), "{message}");
    let kinds = resume_after_exit(&mut f, 0).await;
    assert_eq!(kinds.iter().filter(|k| is_errored(k)).count(), 1);
    supervisor.shutdown().await.unwrap();
}

/// Whether the fake CLI last ran with `--resume`, from `argv.txt`.
fn argv_has_resume(f: &Fixture) -> bool {
    std::fs::read_to_string(f.root.join("project/argv.txt"))
        .unwrap()
        .lines()
        .any(|arg| arg == "--resume")
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
            f.started(false),
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
            .refuse_mode(PermissionMode::DontAsk)
            .refuse_model("bogus"),
    );
    let supervisor = f.start(&agent).await;

    let edits = f.set_mode(PermissionMode::AcceptEdits);
    f.wait_count("first answer", 1, is_setting_answer).await;
    let dont_ask = f.set_mode(PermissionMode::DontAsk);
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
                mode: PermissionMode::DontAsk,
                reason: REFUSE_REASON.into(),
                answers: Some(dont_ask),
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

/// A `SetPermissionMode` to bypass, accepted while the log allowed it; the
/// log is then reopened without the opt-in, as a restart that turned it off.
fn bypass_accepted_then_disallowed(f: &mut Fixture) -> CommandId {
    f.log = f.log.clone().with_bypass(true);
    let bypass = f.set_mode(PermissionMode::Bypass);
    f.reopen_log();
    assert!(!f.log.bypass_allowed());
    bypass
}

fn bypass_refused(command: CommandId) -> EventKind {
    EventKind::PermissionModeRefused {
        mode: PermissionMode::Bypass,
        reason: BYPASS_NOT_ALLOWED.into(),
        answers: Some(command),
    }
}

#[tokio::test]
async fn a_bypass_request_from_before_the_opt_in_was_turned_off_is_refused_by_the_server() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let bypass = bypass_accepted_then_disallowed(&mut f);
    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("answer", 1, is_setting_answer).await;
    assert_eq!(setting_answers(&kinds), [bypass_refused(bypass)]);
    // Positive control: a later request goes to an agent started for it.
    let edits = f.set_mode(PermissionMode::AcceptEdits);
    let kinds = f.wait_count("answers", 2, is_setting_answer).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        setting_answers(&kinds)[1],
        EventKind::PermissionModeChanged {
            mode: PermissionMode::AcceptEdits,
            answers: Some(edits),
        }
    );
    let inputs = agent.inputs(f.session_id);
    assert_eq!(
        inputs[..2],
        [
            f.started(false),
            FakeInput::SetPermissionMode(PermissionMode::AcceptEdits),
        ],
        "no agent started for the bypass, and none asked for it"
    );
    let settings = f.log.agent_settings(&f.storage.db, f.session_id).unwrap();
    assert_eq!(settings.mode.current, Some(PermissionMode::AcceptEdits));
}

#[tokio::test]
async fn an_agent_started_before_an_open_bypass_request_is_reached_is_not_asked_for_it() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    // The message comes first, so its start sees the bypass request open.
    f.send("hello");
    let bypass = bypass_accepted_then_disallowed(&mut f);
    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(setting_answers(&kinds), [bypass_refused(bypass)]);
    let inputs = agent.inputs(f.session_id);
    assert_eq!(inputs[..2], [f.started(false), message("hello")]);
    assert!(
        !inputs.contains(&FakeInput::SetPermissionMode(PermissionMode::Bypass)),
        "{inputs:?}"
    );
}

#[tokio::test]
async fn a_restart_without_the_opt_in_does_not_ask_for_the_bypass_the_agent_had() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    f.log = f.log.clone().with_bypass(true);
    let supervisor = f.start(&agent).await;
    let bypass = f.set_mode(PermissionMode::Bypass);
    let kinds = f.wait_count("answer", 1, is_setting_answer).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        setting_answers(&kinds),
        [EventKind::PermissionModeChanged {
            mode: PermissionMode::Bypass,
            answers: Some(bypass),
        }],
        "with the opt-in, bypass goes to the agent"
    );

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    f.send("hello");
    f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    let inputs = agent.inputs(f.session_id);
    let restarted = inputs
        .iter()
        .rposition(|i| matches!(i, FakeInput::Started { .. }))
        .unwrap();
    assert_ne!(restarted, 0, "started twice");
    assert_eq!(inputs[restarted + 1], message("hello"), "{inputs:?}");
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
            f.started(false),
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
    // The first agent got no message, so it left no conversation to resume:
    // the restart is fresh, and still gets the settings back.
    let restarted = inputs
        .iter()
        .rposition(|i| matches!(i, FakeInput::Started { .. }))
        .expect("restarted");
    assert_ne!(restarted, 0, "started twice");
    assert_eq!(
        inputs[restarted..restarted + 4],
        [
            f.started(false),
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
            f.started(false),
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
        [f.started(false), FakeInput::SetEffort(Effort::High),]
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
        [f.started(false), FakeInput::SetEffort(Effort::Low),]
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

    // Restarted (fresh: the first agent got no message, so there is no
    // conversation to resume): asked for what it last ran with, which logs
    // nothing new.
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
            f.started(false),
            FakeInput::SetEffort(Effort::High),
            FakeInput::Shutdown,
            f.started(false),
            FakeInput::SetEffort(Effort::High),
            message("one"),
            FakeInput::Shutdown,
            f.started(true),
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

// ---------------------------------------------------------------------------
// Session contexts: one context's agent at a time

fn started_in(context: ContextId, resumed: bool) -> FakeInput {
    FakeInput::Started {
        conversation: fake_conversation(resumed),
        context,
        grants: None,
    }
}

/// Every start the fake recorded for the session, in order.
fn starts(agent: &FakeAgent, f: &Fixture) -> Vec<FakeInput> {
    agent
        .inputs(f.session_id)
        .into_iter()
        .filter(|i| matches!(i, FakeInput::Started { .. }))
        .collect()
}

fn handle_of(f: &Fixture, context: ContextId) -> Option<skywaved::agent::ResumeHandle> {
    skywaved::agent::load_resume_handle(&f.storage.db, context).unwrap()
}

fn reply(text: &str) -> EventKind {
    EventKind::AgentMessage {
        parent: None,
        text: format!("echo: {text}"),
    }
}

#[tokio::test]
async fn a_message_then_a_switch_logged_before_the_supervisor_runs_stay_in_their_context() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::text("partial"), Step::Hang]));
    // Both contexts exist and A is current; the supervisor is not running.
    f.clear(b);
    f.switch(a);
    let sent = f.send("for a");
    let marker = f.switch(b);
    assert_eq!(
        marker,
        sent.next(),
        "both logged before the supervisor runs"
    );

    let supervisor = f.start(&agent).await;
    f.wait_count("A's turn ended", 1, is_idle).await;
    wait_for_shutdowns(&agent, &f, 1).await;
    // The message went to A's agent, which was then interrupted and stopped.
    assert_eq!(
        agent.inputs(f.session_id),
        [
            started_in(a, false),
            message("for a"),
            FakeInput::Interrupt,
            FakeInput::Shutdown,
        ]
    );
    // What A reported after the switch is A's. (The fake sees the
    // interrupt before its first step, so its turn has no text.)
    assert_eq!(
        f.stamped_after(marker),
        [
            (state(SessionState::AgentWorking), a),
            (state(SessionState::Idle), a),
        ]
    );
    assert_eq!(f.turn_context(), (false, None));
    assert_eq!(handle_of(&f, a), Some(FakeAgent::resume_handle(a)));
    assert_eq!(handle_of(&f, b), None);
    assert_eq!(
        starts(&agent, &f),
        [started_in(a, false)],
        "B did not start"
    );

    // Positive control: B starts once it gets a message, fresh.
    let to_b = f.send("for b");
    f.wait_count("B's reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts(&agent, &f),
        [started_in(a, false), started_in(b, false)]
    );
    assert_eq!(
        f.stamped_after(to_b),
        [
            (state(SessionState::AgentWorking), b),
            (reply("for b"), b),
            (state(SessionState::Idle), b),
        ]
    );
    assert_eq!(handle_of(&f, b), Some(FakeAgent::resume_handle(b)));
}

#[tokio::test]
async fn a_switch_while_idle_stops_the_agent_and_one_to_the_current_context_keeps_it() {
    let mut f = Fixture::new();
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("reply", 1, is_idle).await;

    // A switch to the context already running keeps its agent.
    f.switch(a);
    f.send("two");
    f.wait_count("reply", 2, is_idle).await;
    assert_eq!(
        agent.inputs(f.session_id),
        [started_in(a, false), message("one"), message("two")]
    );

    // A clear stops the idle agent, without an interrupt, and logs nothing.
    let b = ContextId::new();
    let marker = f.clear(b);
    wait_for_shutdowns(&agent, &f, 1).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            started_in(a, false),
            message("one"),
            message("two"),
            FakeInput::Shutdown,
        ]
    );
    assert_eq!(f.stamped_after(marker), []);
    // Positive control: the same query sees the marker itself.
    let around = f.stamped_after(Seq(marker.0 - 1));
    assert!(matches!(
        around.as_slice(),
        [(EventKind::ContextStarted { .. }, context)] if *context == b
    ));
}

#[tokio::test]
async fn contexts_start_fresh_and_resume_their_own_conversation() {
    // End to end through commands: create, send, clear, send, switch back,
    // send.
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    let one = f.send("one");
    f.wait_count("reply", 1, is_idle).await;
    f.clear(b);
    let two = f.send("two");
    f.wait_count("reply", 2, is_idle).await;
    f.switch(a);
    let three = f.send("three");
    f.wait_count("reply", 3, is_idle).await;
    supervisor.shutdown().await.unwrap();

    assert_eq!(
        agent.inputs(f.session_id),
        [
            started_in(a, false),
            message("one"),
            FakeInput::Shutdown,
            started_in(b, false),
            message("two"),
            FakeInput::Shutdown,
            started_in(a, true),
            message("three"),
            FakeInput::Shutdown,
        ]
    );
    assert_eq!(handle_of(&f, a), Some(FakeAgent::resume_handle(a)));
    assert_eq!(handle_of(&f, b), Some(FakeAgent::resume_handle(b)));
    for (sent, text, context) in [(one, "one", a), (two, "two", b), (three, "three", a)] {
        let after = f.stamped_after(sent);
        assert_eq!(
            after[..3],
            [
                (state(SessionState::AgentWorking), context),
                (reply(text), context),
                (state(SessionState::Idle), context),
            ]
        );
    }
}

#[tokio::test]
async fn mode_model_and_effort_asked_in_one_context_are_asked_again_in_the_next() {
    let mut f = Fixture::new();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    f.set_mode(PermissionMode::AcceptEdits);
    f.set_model("model-b");
    f.set_effort(Effort::High);
    f.wait_count("answers", 3, is_setting_answer).await;
    f.send("in a");
    f.wait_count("reply", 1, is_idle).await;

    f.clear(b);
    f.send("in b");
    let kinds = f.wait_count("reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    let inputs = agent.inputs(f.session_id);
    let in_b = inputs
        .iter()
        .position(|i| *i == started_in(b, false))
        .expect("B started");
    assert_eq!(
        inputs[in_b..],
        [
            started_in(b, false),
            FakeInput::SetPermissionMode(PermissionMode::AcceptEdits),
            FakeInput::SetModel("model-b".into()),
            FakeInput::SetEffort(Effort::High),
            message("in b"),
            FakeInput::Shutdown,
        ]
    );
    // Restoring what the log already has logs nothing new.
    assert_eq!(setting_answers(&kinds).len(), 3);
}

#[tokio::test]
async fn an_approval_pending_at_a_clear_is_cancelled_in_its_own_context() {
    let mut f = Fixture::new();
    let a = f.first();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;
    f.send("write notes");
    f.wait_count("approval", 1, is_approval_requested).await;
    let id = f.approval_id();

    let marker = f.clear(ContextId::new());
    f.wait_count("A's turn ended", 1, is_idle).await;
    wait_for_shutdowns(&agent, &f, 1).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        f.stamped_after(marker),
        [
            (
                EventKind::ApprovalCancelled {
                    id,
                    reason: INTERRUPT_REASON.into(),
                },
                a,
            ),
            (state(SessionState::Idle), a),
        ]
    );
    assert_eq!(f.turn_context(), (false, None));
    assert_eq!(
        agent.inputs(f.session_id)[2..],
        [FakeInput::Interrupt, FakeInput::Shutdown]
    );
}

#[tokio::test]
async fn a_turn_lost_with_the_daemon_after_a_clear_is_settled_in_its_own_context() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    f.send("hang");
    f.wait_count("turn", 1, |k| *k == state(SessionState::AgentWorking))
        .await;
    supervisor.shutdown().await.unwrap();

    // Accepted while the daemon is down, with A's turn still open.
    let marker = f.clear(b);
    assert_eq!(f.turn_context(), (true, Some(a)));
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    assert_eq!(
        f.stamped_after(marker),
        [(
            state(SessionState::Errored {
                message: TURN_LOST.into()
            }),
            a
        )]
    );
    assert_eq!(f.turn_context(), (false, None));

    // The next message starts B, fresh.
    let to_b = f.send("in b");
    f.wait_count("reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts(&agent, &f),
        [started_in(a, false), started_in(b, false)]
    );
    assert_eq!(
        f.stamped_after(to_b),
        [
            (state(SessionState::AgentWorking), b),
            (reply("in b"), b),
            (state(SessionState::Idle), b),
        ]
    );
}

/// What a supervisor appends with no agent process for a message or a
/// setting request whose agent did not start: `[(kind, context)]` of an
/// `Errored` then a `PermissionModeRefused`, checked against `context`.
fn assert_start_failures(after: &[(EventKind, ContextId)], context: ContextId) {
    assert!(
        matches!(
            after,
            [
                (EventKind::SessionStateChanged(SessionState::Errored { message }), errored),
                (EventKind::PermissionModeRefused { reason, .. }, refused),
            ] if message.starts_with("cannot start agent")
                && reason.starts_with("agent did not start")
                && *errored == context
                && *refused == context
        ),
        "{after:#?}"
    );
}

#[tokio::test]
async fn a_context_whose_agent_cannot_start_answers_in_that_context() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new());
    converse_elsewhere(&agent, &f, a).await;
    converse_elsewhere(&agent, &f, b).await;
    // A message and a request in A, then a clear to B, all before the
    // supervisor runs: the answers are A's though B is current by then.
    f.send("in a");
    f.set_mode(PermissionMode::Plan);
    let marker = f.clear(b);
    let supervisor = f.start(&agent).await;
    f.wait_count("refusal", 1, is_setting_answer).await;
    assert_start_failures(&f.stamped_after(marker), a);

    // The same in B, now current.
    let head = f.log.head();
    f.send("in b");
    let edits = f.set_mode(PermissionMode::AcceptEdits);
    let kinds = f.wait_count("refusal", 2, is_setting_answer).await;
    supervisor.shutdown().await.unwrap();
    let after: Vec<_> = f
        .stamped_after(head)
        .into_iter()
        .filter(|(kind, _)| {
            !matches!(
                kind,
                EventKind::UserMessage { .. } | EventKind::PermissionModeRequested { .. }
            )
        })
        .collect();
    assert_start_failures(&after, b);
    assert!(matches!(
        setting_answers(&kinds).last(),
        Some(EventKind::PermissionModeRefused { answers, .. }) if *answers == Some(edits)
    ));
}

#[tokio::test]
async fn a_context_started_for_a_setting_leaves_no_handle_and_starts_fresh_for_its_message() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let agent = FakeAgent::new(FakeScript::new());
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config).await;
    f.send("in a");
    f.wait_count("reply", 1, is_idle).await;
    wait_for_shutdowns(&agent, &f, 1).await;

    let marker = f.clear(b);
    let edits = f.set_mode(PermissionMode::AcceptEdits);
    f.wait_count("answer", 1, is_setting_answer).await;
    wait_for_shutdowns(&agent, &f, 2).await;
    assert_eq!(
        f.stamped_after(marker)[1..],
        [(
            EventKind::PermissionModeChanged {
                mode: PermissionMode::AcceptEdits,
                answers: Some(edits),
            },
            b
        )]
    );
    assert_eq!(handle_of(&f, b), None, "no handle before B's first message");

    let to_b = f.send("in b");
    f.wait_count("reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts(&agent, &f),
        [
            started_in(a, false),
            started_in(b, false),
            started_in(b, false)
        ]
    );
    assert_eq!(
        f.stamped_after(to_b)[..3],
        [
            (state(SessionState::AgentWorking), b),
            (reply("in b"), b),
            (state(SessionState::Idle), b),
        ]
    );
    assert_eq!(handle_of(&f, b), Some(FakeAgent::resume_handle(b)));
}

// ---------------------------------------------------------------------------
// Moved contexts: the new session forks the moved context's conversation

/// Moves the session's `context` into a new session; the `ContextMoved`'s
/// seq and the new session.
fn move_context(f: &mut Fixture, context: ContextId) -> (Seq, SessionId) {
    let seq = accepted(f.handle(
        DeviceId::new(),
        CommandKind::MoveContext {
            session_id: f.session_id,
            context,
            name: "moved".into(),
        },
    ));
    let event = f.log.read(&f.storage.db, Seq(seq.0 - 1), 1).unwrap().events[0].clone();
    match event.kind {
        EventKind::ContextMoved { to, .. } => (seq, to),
        other => panic!("expected ContextMoved, got {other:?}"),
    }
}

/// Sends `text` in session `session`'s first context.
fn send_in(f: &mut Fixture, session: SessionId, text: &str) -> Seq {
    accepted(f.handle(
        DeviceId::new(),
        CommandKind::SendMessage {
            session_id: session,
            context: ContextId::first_of(session),
            text: text.into(),
        },
    ))
}

/// Session `session`'s whole log, with seqs.
fn events_in(f: &Fixture, session: SessionId) -> Vec<(Seq, Event)> {
    let batch = f.log.read(&f.storage.db, Seq::ZERO, 5000).unwrap();
    (batch.from.0..)
        .map(Seq)
        .zip(batch.events)
        .filter(|(_, e)| e.session_id == session)
        .collect()
}

/// Waits until session `session` has `n` `Idle`s.
async fn wait_idle_in(f: &Fixture, session: SessionId, n: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let head = f.log.head();
        let events = events_in(f, session);
        if events.iter().filter(|(_, e)| is_idle(&e.kind)).count() >= n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {n} idle in {session:?}; log: {events:#?}"
        );
        f.log.wait_after(head, WAIT).await;
    }
}

/// Every start the fake recorded for session `session`, in order.
fn starts_in(agent: &FakeAgent, session: SessionId) -> Vec<FakeInput> {
    agent
        .inputs(session)
        .into_iter()
        .filter(|i| matches!(i, FakeInput::Started { .. }))
        .collect()
}

/// The fake's record of a start of `context` in `conversation`.
fn started_as(context: ContextId, conversation: FakeConversation) -> FakeInput {
    FakeInput::Started {
        conversation,
        context,
        grants: None,
    }
}

#[tokio::test]
async fn a_context_moved_mid_turn_is_stopped_and_forked_by_its_new_session() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let gate = Gate::new();
    // Turns run per session: each session's first message gets the first.
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([Step::text("first")])
            .turn([Step::Wait(gate.clone())]),
    );
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("A's reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();

    // Logged before the supervisor runs again, so it reaches the clear with
    // A's turn owed and the move already logged: the stopped agent's
    // events come after the move. The new session's message is logged too,
    // so the supervisor alone orders its start after the source's stop.
    f.send("two");
    let marker = f.clear(b);
    let (moved, new) = move_context(&mut f, a);
    let forked = ContextId::first_of(new);
    let to_new = send_in(&mut f, new, "in new");
    assert_eq!(to_new, moved.next(), "nothing ran in between");
    let supervisor = f.start(&agent).await;
    wait_idle_in(&f, new, 1).await;

    // A's agent was resumed for its message, interrupted and stopped.
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("one"),
            FakeInput::Shutdown,
            f.started(true),
            message("two"),
            FakeInput::Interrupt,
            FakeInput::Shutdown,
        ]
    );
    // Its late turn events land in the source after the move, stamped with
    // the moved context, and leave the source idle.
    let after = f.stamped_after(marker);
    assert!(
        matches!(
            &after[..],
            [
                (EventKind::ContextMoved { to, .. }, at_move),
                (EventKind::SessionStateChanged(SessionState::AgentWorking), working),
                (EventKind::SessionStateChanged(SessionState::Idle), idle),
            ] if *to == new && [*at_move, *working, *idle] == [a, a, a]
        ),
        "{after:#?}"
    );
    assert_eq!(f.turn_context(), (false, None));

    // The new session forked A's conversation, and only after A's agent had
    // stopped: its first agent event comes after A's last.
    assert_eq!(
        agent.inputs(new),
        [
            started_as(forked, FakeConversation::Fork(a)),
            message("in new")
        ]
    );
    let source_idle = f.events_after(marker).last().unwrap().0;
    let new_events = events_in(&f, new);
    let kinds: Vec<_> = new_events
        .iter()
        .map(|(_, e)| (e.kind.clone(), e.context))
        .collect();
    assert!(
        matches!(
            &kinds[..],
            [
                (EventKind::SessionCreated { from: Some(SessionOrigin::Context(from)), .. }, _),
                (EventKind::UserMessage { .. }, _),
                (EventKind::SessionStateChanged(SessionState::AgentWorking), _),
                (EventKind::AgentMessage { text, .. }, _),
                (EventKind::SessionStateChanged(SessionState::Idle), _),
            ] if from.session == f.session_id && from.context == a && text == "first"
        ),
        "{kinds:#?}"
    );
    assert!(kinds.iter().all(|(_, context)| *context == forked));
    assert!(
        source_idle < new_events[2].0,
        "A stopped before the fork ran"
    );
    // The copy is the new context's own conversation.
    assert_eq!(
        handle_of(&f, forked),
        Some(FakeAgent::resume_handle(forked))
    );
    assert_eq!(handle_of(&f, a), Some(FakeAgent::resume_handle(a)));

    // A restart of the new session resumes its own conversation, not the
    // origin's again.
    supervisor.shutdown().await.unwrap();
    gate.open();
    let supervisor = f.start(&agent).await;
    send_in(&mut f, new, "again");
    wait_idle_in(&f, new, 2).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts_in(&agent, new),
        [
            started_as(forked, FakeConversation::Fork(a)),
            started_as(forked, FakeConversation::Resume),
        ]
    );
}

#[tokio::test]
async fn a_new_session_started_for_a_setting_forks_again_for_its_message() {
    let mut f = Fixture::new();
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new());
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config).await;
    f.send("one");
    f.wait_count("A's reply", 1, is_idle).await;
    f.clear(ContextId::new());
    let (_, new) = move_context(&mut f, a);
    let forked = ContextId::first_of(new);

    // A setting request starts the new session with no message; the idle
    // agent is stopped at once, before its copy has a handle.
    f.command(CommandKind::SetPermissionMode {
        session_id: new,
        mode: PermissionMode::Plan,
    });
    let deadline = tokio::time::Instant::now() + WAIT;
    while !agent.inputs(new).contains(&FakeInput::Shutdown) {
        assert!(tokio::time::Instant::now() < deadline, "not shut down");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(handle_of(&f, forked), None, "no handle before a message");

    send_in(&mut f, new, "in new");
    wait_idle_in(&f, new, 1).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts_in(&agent, new),
        [
            started_as(forked, FakeConversation::Fork(a)),
            started_as(forked, FakeConversation::Fork(a)),
        ]
    );
    assert_eq!(
        handle_of(&f, forked),
        Some(FakeAgent::resume_handle(forked))
    );
}

#[tokio::test]
async fn a_new_session_whose_origin_lost_its_handle_does_not_start_fresh() {
    let mut f = Fixture::new();
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("A's reply", 1, is_idle).await;
    f.clear(ContextId::new());
    let (_, new) = move_context(&mut f, a);
    let forked = ContextId::first_of(new);
    // Validation moves only a context with a handle; force it away.
    f.storage
        .db
        .conn()
        .execute(
            "UPDATE contexts SET agent_state = NULL WHERE id = ?1",
            [a.0.to_string()],
        )
        .unwrap();

    let sent = send_in(&mut f, new, "in new");
    let deadline = tokio::time::Instant::now() + WAIT;
    let errored = loop {
        let head = f.log.head();
        let after: Vec<_> = events_in(&f, new)
            .into_iter()
            .filter(|(seq, _)| *seq > sent)
            .collect();
        if let Some((_, event)) = after.first() {
            break event.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "no answer");
        f.log.wait_after(head, WAIT).await;
    };
    assert!(
        matches!(
            &errored.kind,
            EventKind::SessionStateChanged(SessionState::Errored { message })
                if message.contains("no conversation to continue")
        ),
        "{errored:#?}"
    );
    assert_eq!(errored.context, forked);
    assert_eq!(agent.inputs(new), [], "no start at all, fresh or not");

    // Positive control: with the origin's handle back, the same message
    // forks it.
    skywaved::agent::store_resume_handle(&f.storage.db, a, &FakeAgent::resume_handle(a)).unwrap();
    send_in(&mut f, new, "again");
    wait_idle_in(&f, new, 1).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts_in(&agent, new),
        [started_as(forked, FakeConversation::Fork(a))]
    );
}

// ---------------------------------------------------------------------------
// Grants: per session, across its contexts

fn grants(rules: serde_json::Value) -> skywaved::agent::Grants {
    skywaved::agent::Grants(json!({ "rules": rules }))
}

fn grants_of(f: &Fixture, session: SessionId) -> Option<skywaved::agent::Grants> {
    skywaved::agent::load_grants(&f.storage.db, session).unwrap()
}

fn started_with(
    context: ContextId,
    resumed: bool,
    grants: Option<skywaved::agent::Grants>,
) -> FakeInput {
    FakeInput::Started {
        conversation: fake_conversation(resumed),
        context,
        grants,
    }
}

#[tokio::test]
async fn grants_reported_in_one_context_go_to_every_later_start_of_the_session() {
    let mut f = Fixture::new();
    let a = f.first();
    let b = ContextId::new();
    let c = ContextId::new();
    let granted = grants(json!(["edits", "Bash(cargo test)"]));
    // The second turn, which runs in B, reports a first set and then the
    // whole new one, which replaces it.
    let script = FakeScript::new().turn([Step::text("in a")]).turn([
        Step::grants(json!({ "rules": ["edits"] })),
        Step::grants(granted.0.clone()),
        Step::text("in b"),
    ]);
    let agent = FakeAgent::new(script);
    let supervisor = f.start(&agent).await;

    f.send("one");
    f.wait_count("A's reply", 1, is_idle).await;
    f.clear(b);
    f.send("two");
    f.wait_count("B's reply", 2, is_idle).await;
    assert_eq!(grants_of(&f, f.session_id), Some(granted.clone()));

    // Switching back to A resumes it with what B was granted.
    f.switch(a);
    f.send("three");
    f.wait_count("A's second reply", 3, is_idle).await;
    // The next message after a clear starts fresh, with them too.
    f.clear(c);
    f.send("four");
    f.wait_count("C's reply", 4, is_idle).await;
    supervisor.shutdown().await.unwrap();

    assert_eq!(
        starts(&agent, &f),
        [
            started_with(a, false, None),
            started_with(b, false, None),
            started_with(a, true, Some(granted.clone())),
            started_with(c, false, Some(granted.clone())),
        ]
    );
    // Stored for the session, not for the context that reported them.
    assert_eq!(handle_of(&f, b), Some(FakeAgent::resume_handle(b)));
    assert_eq!(grants_of(&f, f.session_id), Some(granted));
}

#[tokio::test]
async fn grants_never_cross_sessions() {
    let mut f = Fixture::new();
    let first = f.session_id;
    let granted = grants(json!(["edits"]));
    // Each session's turns run in order: only a session's second turn
    // reports grants.
    let script = FakeScript::new()
        .turn([Step::text("first turn")])
        .turn([Step::grants(granted.0.clone())]);
    let agent = FakeAgent::new(script);
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.send("two");
    f.wait_count("both replies", 2, is_idle).await;
    assert_eq!(grants_of(&f, first), Some(granted.clone()));

    // Another session starts without them.
    let second = f.create_session();
    f.session_id = second;
    f.send("elsewhere");
    f.wait_count("the other session's reply", 1, is_idle).await;
    assert_eq!(
        starts(&agent, &f),
        [started_with(ContextId::first_of(second), false, None)]
    );
    assert_eq!(grants_of(&f, second), None);

    // Positive control: the first session's next start gets them.
    f.session_id = first;
    let next = ContextId::new();
    f.clear(next);
    f.send("three");
    f.wait_count("the first session's third reply", 3, is_idle)
        .await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts(&agent, &f),
        [
            started_with(ContextId::first_of(first), false, None),
            started_with(next, false, Some(granted.clone())),
        ]
    );
    assert_eq!(grants_of(&f, first), Some(granted));
    assert_eq!(grants_of(&f, second), None);
}

// ---------------------------------------------------------------------------
// Adopted sessions: the first context forks the outside conversation

/// Waits until the fake has recorded a shutdown for the session.
async fn wait_shutdown(agent: &FakeAgent, f: &Fixture, n: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while agent
        .inputs(f.session_id)
        .iter()
        .filter(|i| **i == FakeInput::Shutdown)
        .count()
        < n
    {
        assert!(tokio::time::Instant::now() < deadline, "not shut down");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn adopt(id: &str) -> FakeConversation {
    FakeConversation::Adopt(id.into())
}

#[tokio::test]
async fn an_adopted_session_adopts_for_its_first_message_and_resumes_after() {
    let mut f = Fixture::adopting("ext-1");
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new()).with_outside_conversation("ext-1");
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("the reply", 1, is_idle).await;
    assert_eq!(
        agent.inputs(f.session_id),
        [started_as(a, adopt("ext-1")), message("one")]
    );
    // The copy is the context's own conversation.
    assert_eq!(handle_of(&f, a), Some(FakeAgent::resume_handle(a)));

    // Once it has a handle, a restart resumes the copy, not the outside
    // conversation again.
    accepted(f.stop());
    wait_shutdown(&agent, &f, 1).await;
    f.send("two");
    f.wait_count("the second reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts(&agent, &f),
        [
            started_as(a, adopt("ext-1")),
            started_as(a, FakeConversation::Resume),
        ]
    );
}

#[tokio::test]
async fn an_adopted_session_started_for_a_setting_adopts_again_for_its_message() {
    let mut f = Fixture::adopting("ext-1");
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new()).with_outside_conversation("ext-1");
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config).await;

    // A setting request starts the agent with no message; the idle agent is
    // stopped at once, before its copy has a handle.
    f.set_mode(PermissionMode::Plan);
    wait_shutdown(&agent, &f, 1).await;
    assert_eq!(handle_of(&f, a), None, "no handle before a message");

    f.send("one");
    f.wait_count("the reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        starts(&agent, &f),
        [started_as(a, adopt("ext-1")), started_as(a, adopt("ext-1"))]
    );
    assert_eq!(handle_of(&f, a), Some(FakeAgent::resume_handle(a)));
}

#[tokio::test]
async fn an_adoption_the_agent_cannot_make_errors_each_message() {
    let mut f = Fixture::adopting("ext-unknown");
    let agent = FakeAgent::new(FakeScript::new()).with_outside_conversation("ext-1");
    let supervisor = f.start(&agent).await;
    f.send("one");
    let kinds = f.wait_count("errored", 1, is_errored).await;
    assert_eq!(
        kinds.last(),
        Some(&state(SessionState::Errored {
            message: "cannot resume agent session: ext-unknown names no outside conversation"
                .into()
        }))
    );
    assert_eq!(handle_of(&f, f.first()), None);

    // The session stays; the next message tries again and fails again.
    f.send("two");
    f.wait_count("errored again", 2, is_errored).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(starts(&agent, &f), [], "no start, adopted or fresh");
    assert_eq!(handle_of(&f, f.first()), None);
}

#[tokio::test]
async fn a_context_moved_out_of_an_adopted_session_forks_its_copy() {
    let mut f = Fixture::adopting("ext-1");
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new()).with_outside_conversation("ext-1");
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("the reply", 1, is_idle).await;
    f.clear(ContextId::new());
    let (_, new) = move_context(&mut f, a);
    let forked = ContextId::first_of(new);
    send_in(&mut f, new, "in new");
    wait_idle_in(&f, new, 1).await;
    supervisor.shutdown().await.unwrap();

    assert_eq!(starts(&agent, &f), [started_as(a, adopt("ext-1"))]);
    assert_eq!(
        starts_in(&agent, new),
        [started_as(forked, FakeConversation::Fork(a))]
    );
}

/// An imported history is a record: its user message never reaches the
/// agent, which adopts the conversation it already holds, and its call,
/// unanswered, is not one a restart names.
#[tokio::test]
async fn an_imported_history_never_reaches_the_agent_nor_opens_a_call() {
    let history = vec![
        HistoryItem::User {
            at: None,
            text: "imported prompt".into(),
        },
        HistoryItem::Agent {
            at: None,
            event: AdapterEvent::ToolCall {
                call: Some(AgentCallId::from("toolu_1")),
                parent: None,
                name: "Bash".into(),
                input: json!({ "command": "sleep 100" }),
                display: None,
            },
        },
    ];
    let mut f = Fixture::importing("ext-1", history);
    let a = f.first();
    let imported = f
        .kinds()
        .iter()
        .filter(|k| matches!(k, EventKind::Imported { .. }))
        .count();
    assert_eq!(imported, 2, "the history is in the log");
    assert_eq!(f.log.open_calls(&f.storage.db, f.session_id).unwrap(), []);

    // The daemon starts with the import in the log: nothing to settle.
    let head = f.log.head();
    let agent = FakeAgent::new(FakeScript::new()).with_outside_conversation("ext-1");
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("the reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            started_as(a, adopt("ext-1")),
            message("one"),
            FakeInput::Shutdown
        ]
    );
    let after = f.stamped_after(head);
    assert!(
        !after
            .iter()
            .any(|(k, _)| matches!(k, EventKind::CallsUnanswered { .. })),
        "{after:?}"
    );

    // Nor does a restart find an open call.
    f.reopen_log();
    let head = f.log.head();
    let supervisor = f.start(&agent).await;
    assert_eq!(f.stamped_after(head), []);
    supervisor.shutdown().await.unwrap();
}

/// Shell runs are the shell runner's: the agent never sees one, and a turn
/// running alongside goes on as if there were none.
#[tokio::test]
async fn shell_events_never_reach_the_agent() {
    let mut f = Fixture::new();
    f.log = f.log.clone().with_shell(true);
    let gate = Gate::new();
    let agent =
        FakeAgent::new(FakeScript::new().turn([Step::Wait(gate.clone()), Step::text("done")]));
    let supervisor = f.start(&agent).await;
    f.send("work");
    let working = |k: &EventKind| *k == state(SessionState::AgentWorking);
    f.wait_count("the turn", 1, working).await;

    // Every shell event, logged mid-turn as the API and the runner would.
    let by = DeviceId::new();
    let shell = |f: &mut Fixture, kind| {
        f.log.shell_event(&mut f.storage.db, kind).unwrap();
    };
    let run = CommandId::new();
    let session_id = f.session_id;
    accepted(
        f.log
            .handle(
                &mut f.storage.db,
                by,
                &Command {
                    id: run,
                    kind: CommandKind::RunShell {
                        session_id,
                        command: "make".into(),
                        when: Default::default(),
                    },
                },
            )
            .unwrap(),
    );
    shell(
        &mut f,
        EventKind::ShellStarted {
            run,
            cwd: "/w".into(),
        },
    );
    shell(
        &mut f,
        EventKind::ShellOutput {
            run,
            tail: "building".into(),
            lines: 1,
            bytes: 9,
        },
    );
    accepted(f.handle(by, CommandKind::StopShell { session_id, run }));
    shell(
        &mut f,
        EventKind::ShellFinished {
            run,
            by,
            started_at: None,
            command_line: "make".into(),
            end: skywave_proto::ShellEnd::Stopped,
            cwd: None,
            preview: String::new(),
            lines: 0,
            output: None,
            truncated: false,
        },
    );

    gate.open();
    let kinds = f.wait_count("the turn's end", 1, is_idle).await;
    assert_eq!(
        agent.inputs(f.session_id),
        [f.started(false), message("work")]
    );
    supervisor.shutdown().await.unwrap();
    let is_shell = |k: &EventKind| {
        matches!(
            k,
            EventKind::ShellRequested { .. }
                | EventKind::ShellStarted { .. }
                | EventKind::ShellOutput { .. }
                | EventKind::ShellStopRequested { .. }
                | EventKind::ShellFinished { .. }
        )
    };
    assert_eq!(
        kinds.iter().filter(|k| is_shell(k)).count(),
        5,
        "{kinds:#?}"
    );
    let rest: Vec<_> = kinds.iter().filter(|k| !is_shell(k)).collect();
    assert!(
        matches!(
            &rest[..],
            [
                EventKind::UserMessage { text, .. },
                EventKind::SessionStateChanged(SessionState::AgentWorking),
                EventKind::AgentMessage { text: done, .. },
                EventKind::SessionStateChanged(SessionState::Idle),
            ] if text == "work" && done == "done"
        ),
        "{rest:#?}"
    );
}

// --- failure ------------------------------------------------------------

#[tokio::test]
async fn a_supervisor_error_surfaces_and_the_next_start_delivers() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let mut supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("first reply", 1, is_idle).await;

    // The cursor passes a message before the agent gets it: that write
    // fails, so the supervisor stops on it.
    f.cursor_disk_full();
    f.send("two");
    tokio::time::timeout(WAIT, supervisor.failed())
        .await
        .expect("the supervisor's failure surfaces");
    let error = supervisor.shutdown().await.unwrap_err();
    assert!(error.to_string().contains("disk is full"), "{error}");
    assert_eq!(
        agent.inputs(f.session_id),
        [f.started(false), message("one"), FakeInput::Shutdown],
        "the failed supervisor never handed the message out, and stopped its agent"
    );

    // Its cursor never passed "two": the next start delivers it.
    f.disk_has_room();
    f.reopen_log();
    let supervisor = f.start(&agent).await;
    f.wait_count("second reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("one"),
            FakeInput::Shutdown,
            f.started(true),
            message("two"),
            FakeInput::Shutdown,
        ]
    );
}

/// A failed supervisor stops its agents as a shutdown does: the CLI gets
/// SIGTERM (and stops its commands) rather than a bare SIGKILL of its group.
#[tokio::test]
async fn a_failed_supervisor_stops_its_agent_with_sigterm() {
    let mut f = Fixture::new();
    let mut supervisor = start_claude(&f).await;
    f.send("run");
    f.wait_for("the running call", |k| {
        k.iter().any(|k| matches!(k, EventKind::ToolCall(_)))
    })
    .await;
    let pid = fake_cli_pid(&f);

    f.cursor_disk_full();
    f.send("two");
    tokio::time::timeout(WAIT, supervisor.failed())
        .await
        .expect("the supervisor's failure surfaces");
    assert!(
        f.root.join("project/term.txt").exists(),
        "the agent got SIGTERM"
    );
    assert!(!process_exists(pid), "agent process {pid} still exists");
    let error = supervisor.shutdown().await.unwrap_err();
    assert!(error.to_string().contains("disk is full"), "{error}");
    f.disk_has_room();
}

// --- a low disk -----------------------------------------------------------

fn said(text: &str) -> EventKind {
    EventKind::AgentMessage {
        parent: None,
        text: text.into(),
    }
}

fn rejected(result: CommandResult) -> RejectReason {
    match result {
        CommandResult::Rejected { reason } => reason,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

/// Bounds a check that something does not happen. The pause itself is
/// decided by the disk state the supervisor reads at every poll; this only
/// gives an agent that was wrongly read the time to show it.
const QUIET: Duration = Duration::from_millis(300);

/// Asserts the session's log does not get `kind` within [`QUIET`].
async fn stays_unlogged(f: &Fixture, kind: &EventKind) {
    let logged = tokio::time::timeout(QUIET, f.wait_for("nothing", |k| k.contains(kind))).await;
    assert!(logged.is_err(), "{kind:?} was logged while paused");
}

/// The free space is read only where something is about to take space,
/// and a recent reading is reused: commands and the burst of events a turn
/// logs within the interval cost one reading, and an idle server, over a
/// day, none. The
/// disk's own timers would run on this test's runtime, whose clock the
/// day is skipped on.
#[tokio::test]
async fn work_reads_the_free_space_once_per_interval_and_idle_reads_none() {
    let mut f = Fixture::new();
    let lines: Vec<Step> = (0..50).map(|i| Step::text(format!("line {i}"))).collect();
    let agent = FakeAgent::new(FakeScript::new().turn(lines));
    let supervisor = f.start(&agent).await;
    f.send("work");
    f.wait_count("the turn's end", 1, is_idle).await;
    assert_eq!(
        f.disk_reads(),
        1,
        "the session's creation, the message and its burst share one reading"
    );

    let reads = f.disk_reads();
    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    tokio::time::resume();
    assert_eq!(f.disk_reads(), reads, "an idle server reads nothing");
    supervisor.shutdown().await.unwrap();
}

/// Below the floor the agent's events stay unread, so it waits with
/// nothing lost; new work is refused, and an interrupt and a stop still
/// reach the agent and are logged.
#[tokio::test]
async fn a_low_disk_pauses_the_agent_refuses_new_work_and_still_interrupts_and_stops() {
    let mut f = Fixture::new();
    let gate = Gate::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::text("before"),
        Step::Wait(gate.clone()),
        Step::text("after"),
        Step::Hang,
    ]));
    let supervisor = f.start(&agent).await;
    f.send("work");
    f.wait_for("the turn to block", |k| k.contains(&said("before")))
        .await;

    f.disk_low();
    gate.open();
    stays_unlogged(&f, &said("after")).await;

    let context = f.current();
    let message = CommandKind::SendMessage {
        session_id: f.session_id,
        context,
        text: "more".into(),
    };
    assert_eq!(
        rejected(f.handle(DeviceId::new(), message)),
        RejectReason::DiskLow
    );
    let create = CommandKind::CreateSession {
        workdir: f.root.join("project").to_str().unwrap().to_owned(),
        conversation: None,
    };
    assert_eq!(
        rejected(f.handle(DeviceId::new(), create)),
        RejectReason::DiskLow
    );

    // The interrupt reaches the paused agent, and its session is read
    // until the turn ends: what it said meanwhile first.
    accepted(f.interrupt());
    let kinds = f.wait_count("the interrupted turn's end", 1, is_idle).await;
    let after = kinds.iter().position(|k| *k == said("after"));
    let idle = kinds.iter().position(is_idle);
    assert!(after.is_some() && after < idle, "{kinds:#?}");
    assert!(agent.inputs(f.session_id).contains(&FakeInput::Interrupt));

    // A stop still stops it, as always.
    accepted(f.stop());
    wait_inputs(&agent, f.session_id, "the stop", |inputs| {
        inputs.contains(&FakeInput::Shutdown)
    })
    .await;
    f.disk_freed();
    supervisor.shutdown().await.unwrap();
}

/// Once the disk has room again the paused agent is read on where it
/// stopped: everything it said while paused is logged once, in order, and
/// it was never stopped.
#[tokio::test]
async fn a_paused_agent_resumes_when_space_returns_and_loses_nothing() {
    // More than a fake session's event channel holds, so the agent blocks.
    let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
    let mut f = Fixture::new();
    let gate = Gate::new();
    let mut steps = vec![Step::text("before"), Step::Wait(gate.clone())];
    steps.extend(lines.iter().map(|line| Step::text(line.clone())));
    let agent = FakeAgent::new(FakeScript::new().turn(steps));
    let supervisor = f.start(&agent).await;
    f.send("work");
    f.wait_for("the turn to block", |k| k.contains(&said("before")))
        .await;

    f.disk_low();
    gate.open();
    stays_unlogged(&f, &said("line 0")).await;

    f.disk_freed();
    let kinds = f.wait_count("the turn's end", 1, is_idle).await;
    let logged: Vec<&EventKind> = kinds
        .iter()
        .skip_while(|k| **k != said("before"))
        .skip(1)
        .filter(|k| matches!(k, EventKind::AgentMessage { .. }))
        .collect();
    let expected: Vec<EventKind> = lines.iter().map(|line| said(line)).collect();
    assert_eq!(logged, expected.iter().collect::<Vec<_>>());
    assert_eq!(
        agent.inputs(f.session_id),
        [f.started(false), message("work")],
        "the agent was neither interrupted nor stopped"
    );
    supervisor.shutdown().await.unwrap();
}

/// A daemon that starts below the floor comes up paused: it recovers and
/// hands a message accepted before to its agent, logs nothing the agent
/// says until there is room, and refuses new work meanwhile.
#[tokio::test]
async fn a_start_below_the_floor_comes_up_paused() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    f.send("one");
    f.disk_low();
    let supervisor = f.start(&agent).await;
    wait_inputs(&agent, f.session_id, "the message", |inputs| {
        inputs.contains(&message("one"))
    })
    .await;
    stays_unlogged(&f, &state(SessionState::AgentWorking)).await;
    let context = f.current();
    let message = CommandKind::SendMessage {
        session_id: f.session_id,
        context,
        text: "two".into(),
    };
    assert_eq!(
        rejected(f.handle(DeviceId::new(), message)),
        RejectReason::DiskLow
    );

    f.disk_freed();
    let kinds = f.wait_count("the reply", 1, is_idle).await;
    assert!(kinds.contains(&said("echo: one")), "{kinds:#?}");
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_running_supervisor_reports_no_failure() {
    let f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let mut supervisor = f.start(&agent).await;
    let failed = tokio::time::timeout(Duration::from_millis(200), supervisor.failed()).await;
    assert!(failed.is_err(), "a running supervisor has not failed");
    supervisor.shutdown().await.unwrap();
}

/// The tool events of the session's log, in order.
fn tool_events(kinds: &[EventKind]) -> (Vec<ToolCall>, Vec<ToolResult>) {
    let calls = kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    let results = kinds
        .iter()
        .filter_map(|k| match k {
            EventKind::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect();
    (calls, results)
}

#[tokio::test]
async fn a_blob_that_cannot_be_written_is_marked_lost_and_the_turn_goes_on() {
    let mut f = Fixture::new();
    let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
    let turn = || {
        [
            Step::tool_call("shell", json!({ "command": long.clone() })),
            Step::tool_result(long.clone()),
            Step::text("done"),
        ]
    };
    let agent = FakeAgent::new(FakeScript::new().turn(turn()).turn(turn()));
    let mut supervisor = f.start(&agent).await;

    f.blobs_unwritable();
    f.send("one");
    let kinds = f.wait_count("first turn", 1, is_idle).await;
    let (calls, results) = tool_events(&kinds);
    let body = calls[0].display.body.as_ref().expect("a cut title's body");
    assert_eq!(
        body.full,
        Some(Full::Lost),
        "the body's blob could not be written"
    );
    assert_eq!(
        calls[0].input,
        Some(Full::Lost),
        "the input's blob could not be written"
    );
    assert_eq!(
        results[0].output,
        Some(Full::Lost),
        "the output's blob could not be written"
    );
    // The event is cut like any long one, and counts all of the output.
    assert_eq!(results[0].lines, 100);
    assert_eq!(
        results[0].preview.lines().count(),
        skywave_proto::limits::PREVIEW_MAX_LINES
    );
    assert!(kinds.contains(&EventKind::AgentMessage {
        parent: None,
        text: "done".into()
    }));
    let failed = tokio::time::timeout(Duration::from_millis(200), supervisor.failed()).await;
    assert!(failed.is_err(), "a blob write is no supervisor failure");

    // Control: with room again the same turn keeps its blobs.
    f.blobs_writable();
    f.send("two");
    let kinds = f.wait_count("second turn", 2, is_idle).await;
    let (calls, results) = tool_events(&kinds);
    let blob = |full: &Option<Full>| full.as_ref().and_then(Full::blob).cloned();
    assert!(blob(&calls[1].display.body.as_ref().unwrap().full).is_some());
    assert!(blob(&calls[1].input).is_some());
    let output = blob(&results[1].output).expect("the output's blob");
    assert_eq!(output.size, long.len() as u64);
    supervisor.shutdown().await.unwrap();
}

/// The user decides an approval on its summary: one whose input could not
/// be kept says so, so the summary is never taken for all of it.
#[tokio::test]
async fn an_approval_whose_input_cannot_be_written_is_marked_lost() {
    let mut f = Fixture::new();
    let ask = |path: &str| {
        Step::approval(
            "req-1",
            "write",
            // The summary is the first string field, in either key order.
            json!({ "file": path, "text": "x\n".repeat(100) }),
        )
        .on_approve([Step::text("Done.")])
    };
    let script = FakeScript::new().turn([ask("a.txt")]).turn([ask("b.txt")]);
    let agent = FakeAgent::new(script);
    let mut supervisor = f.start(&agent).await;
    let input = |kinds: &[EventKind]| {
        kinds
            .iter()
            .rev()
            .find_map(|k| match k {
                EventKind::ApprovalRequested { summary, input, .. } => {
                    Some((summary.clone(), input.clone()))
                }
                _ => None,
            })
            .expect("an approval")
    };

    f.blobs_unwritable();
    f.send("one");
    let kinds = f.wait_count("approval", 1, is_approval_requested).await;
    assert_eq!(input(&kinds), ("a.txt".into(), Some(Full::Lost)));
    let failed = tokio::time::timeout(Duration::from_millis(200), supervisor.failed()).await;
    assert!(failed.is_err(), "a blob write is no supervisor failure");
    accepted(f.resolve(DeviceId::new(), f.approval_id(), Decision::Approve));
    f.wait_count("first turn", 1, is_idle).await;

    // Control: with room again the same request keeps its input.
    f.blobs_writable();
    f.send("two");
    let kinds = f.wait_count("approval", 2, is_approval_requested).await;
    let (summary, full) = input(&kinds);
    assert_eq!(summary, "b.txt");
    assert!(matches!(full, Some(Full::Blob(_))), "{full:?}");
    supervisor.shutdown().await.unwrap();
}

/// A write lock another connection holds past the old 5 s busy timeout (an
/// admin command, an import) delays the supervisor's appends; it no longer
/// ends it.
#[tokio::test]
async fn a_write_lock_held_for_seconds_only_delays_the_supervisor() {
    const HELD: Duration = Duration::from_secs(6);
    let mut f = Fixture::new();
    let gate = Gate::new();
    let agent = FakeAgent::new(FakeScript::new().turn([
        Step::text("before"),
        Step::Wait(gate.clone()),
        Step::text("after"),
    ]));
    let mut supervisor = f.start(&agent).await;
    f.send("work");
    f.wait_for("the turn to block", |k| {
        k.contains(&EventKind::AgentMessage {
            parent: None,
            text: "before".into(),
        })
    })
    .await;

    let (locked, lock_taken) = std::sync::mpsc::channel();
    let data = f.data.clone();
    let holder = std::thread::spawn(move || {
        let mut other = Storage::open(&data).unwrap();
        let tx = other
            .db
            .conn_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(HELD);
        drop(tx);
    });
    lock_taken.recv().unwrap();
    let blocked = std::time::Instant::now();
    gate.open();

    let kinds = f.wait_count("the turn's end", 1, is_idle).await;
    assert!(
        blocked.elapsed() >= Duration::from_secs(5),
        "the append waited out the lock"
    );
    assert!(kinds.contains(&EventKind::AgentMessage {
        parent: None,
        text: "after".into()
    }));
    holder.join().unwrap();
    let failed = tokio::time::timeout(Duration::from_millis(200), supervisor.failed()).await;
    assert!(failed.is_err(), "a held lock is no supervisor failure");
    supervisor.shutdown().await.unwrap();
}

/// A daemon restarted while blobs still cannot be stored settles what its
/// last run left and runs the next turn, rather than failing on the same
/// error again (a service manager would restart it in a loop).
#[tokio::test]
async fn a_restart_while_blobs_cannot_be_stored_recovers_and_runs() {
    let mut f = Fixture::new();
    let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
    let gate = Gate::new();
    let agent = FakeAgent::new(
        FakeScript::new()
            .turn([
                Step::tool_call("shell", json!({ "command": long.clone() })),
                Step::Wait(gate.clone()),
            ])
            .turn([Step::tool_result(long.clone())]),
    );
    let supervisor = f.start(&agent).await;
    f.blob_rows_refused();
    f.send("one");
    f.wait_count("the call", 1, |k| matches!(k, EventKind::ToolCall(_)))
        .await;
    // The daemon stops mid-turn, its call open.
    supervisor.shutdown().await.unwrap();

    f.reopen_log();
    let mut supervisor = f.start(&agent).await;
    let kinds = f.wait_count("the lost turn", 1, is_errored).await;
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EventKind::CallsUnanswered { calls } if calls.len() == 1)),
        "{kinds:#?}"
    );
    f.send("two");
    let kinds = f.wait_count("the next turn", 1, is_idle).await;
    let (_, results) = tool_events(&kinds);
    assert_eq!(results.len(), 1, "{kinds:#?}");
    assert_eq!(
        results[0].output,
        Some(Full::Lost),
        "the blob still could not be stored"
    );
    let failed = tokio::time::timeout(Duration::from_millis(200), supervisor.failed()).await;
    assert!(failed.is_err());
    supervisor.shutdown().await.unwrap();
}

/// The daemon stops, non-zero and with the error logged, when its agent
/// supervisor fails, instead of accepting messages no agent would hear.
#[tokio::test]
async fn the_daemon_stops_when_its_supervisor_fails() {
    let f = Fixture::new();
    // The session's creation is past the cursor: following it writes the
    // cursor, which fails.
    f.cursor_disk_full();
    let dir = f.data.parent().unwrap();
    let log_file = dir.join("skywaved.log");
    let config = dir.join("skywaved.toml");
    std::fs::write(
        &config,
        format!(
            // No floor: the test machine's free space is no part of it.
            "listen = \"127.0.0.1:0\"\ndata_dir = {:?}\nroots = [{:?}]\nlog_file = {:?}\n\
             [disk]\nmin_free = 0\n",
            f.data, f.root, log_file
        ),
    )
    .unwrap();
    let daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_skywaved"))
        .arg("--config")
        .arg(&config)
        .arg("serve")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(WAIT, daemon)
        .await
        .expect("the daemon stops on its own")
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("disk is full"), "{stderr}");
    let logged = std::fs::read_to_string(&log_file).unwrap();
    assert!(logged.contains("listening"), "{logged}");
    assert!(
        logged.contains("agent supervisor failed; stopping the daemon"),
        "{logged}"
    );
    f.disk_has_room();
}

// Starts and stops run off the supervisor's loop: one session's slow agent
// holds up only that session's events, which wait in order.

/// Waits until session `session`'s fake inputs satisfy `done`.
async fn wait_inputs(
    agent: &FakeAgent,
    session: SessionId,
    what: &str,
    done: impl Fn(&[FakeInput]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let inputs = agent.inputs(session);
        if done(&inputs) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; inputs: {inputs:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until the supervisor has stored a cursor at or past `seq`: it has
/// read the event there, and handled it or held it for its session.
async fn wait_read(f: &Fixture, seq: Seq) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let stored: i64 = f
            .storage
            .db
            .conn()
            .query_row("SELECT seq FROM supervisor_cursor WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        if stored >= i64::try_from(seq.0).unwrap() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the supervisor did not read {seq:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The fake's record of a fresh start of session `session`'s first context.
fn started_fresh(session: SessionId) -> FakeInput {
    started_as(ContextId::first_of(session), FakeConversation::Fresh)
}

#[tokio::test]
async fn a_slow_start_holds_up_no_other_sessions_answer_or_interrupt() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    let slow = f.session_id;
    let other = f.create_session();
    f.session_id = other;
    f.send("write");
    f.wait_count("the other session's approval", 1, is_approval_requested)
        .await;

    let started = Gate::new();
    agent.hold_start(slow, started.clone());
    send_in(&mut f, slow, "slow");
    wait_inputs(&agent, slow, "the slow start", |i| !i.is_empty()).await;

    // Logged after the slow session's message, and carried out while its
    // agent still starts.
    let id = f.approval_id();
    accepted(f.resolve(DeviceId::new(), id, Decision::Approve));
    f.wait_count("the answered turn's end", 1, is_idle).await;
    f.send("hang");
    f.wait_count("the hanging turn", 2, |k| {
        *k == state(SessionState::AgentWorking)
    })
    .await;
    accepted(f.interrupt());
    f.wait_count("the interrupted turn's end", 2, is_idle).await;
    assert_eq!(agent.inputs(slow), [started_fresh(slow)]);

    // The slow session's message goes out once its agent has started.
    started.open();
    f.session_id = slow;
    f.wait_count("the slow session's approval", 1, is_approval_requested)
        .await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(slow),
        [started_fresh(slow), message("slow"), FakeInput::Shutdown]
    );
}

#[tokio::test]
async fn a_slow_stop_holds_up_no_other_session_and_a_message_meanwhile_starts_the_agent_after_it() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(approval_script());
    let supervisor = f.start(&agent).await;
    let slow = f.session_id;
    f.send("one");
    f.wait_count("the first approval", 1, is_approval_requested)
        .await;
    let first = f.approval_id();
    accepted(f.resolve(DeviceId::new(), first, Decision::Approve));
    f.wait_count("the first turn's end", 1, is_idle).await;
    let other = f.create_session();
    f.session_id = other;
    f.send("write");
    f.wait_count("the other session's approval", 1, is_approval_requested)
        .await;

    let stopped = Gate::new();
    agent.hold_shutdown(slow, stopped.clone());
    f.session_id = slow;
    accepted(f.stop());
    wait_inputs(&agent, slow, "the slow stop", |i| {
        i.last() == Some(&FakeInput::Shutdown)
    })
    .await;
    f.send("two");

    // The other session's answer reaches its agent while the stop runs; the
    // message waits for the stop.
    f.session_id = other;
    let id = f.approval_id();
    accepted(f.resolve(DeviceId::new(), id, Decision::Approve));
    f.wait_count("the other turn's end", 1, is_idle).await;
    let answered = FakeInput::Answer("req-1".into(), Decision::Approve, None);
    assert_eq!(
        agent.inputs(slow),
        [
            started_fresh(slow),
            message("one"),
            answered.clone(),
            FakeInput::Shutdown,
        ]
    );

    stopped.open();
    f.session_id = slow;
    f.wait_count("the message after the stop", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(slow),
        [
            started_fresh(slow),
            message("one"),
            answered,
            FakeInput::Shutdown,
            f.started(true),
            message("two"),
            FakeInput::Shutdown,
        ]
    );
    let kinds = f.kinds();
    assert!(!kinds.iter().any(is_errored), "{kinds:#?}");
}

#[tokio::test]
async fn a_stop_while_the_agent_starts_stops_it_once_it_has_started() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    let started = Gate::new();
    agent.hold_start(f.session_id, started.clone());
    f.send("hang");
    wait_inputs(&agent, f.session_id, "the start", |i| !i.is_empty()).await;
    let stop = accepted(f.stop());
    wait_read(&f, stop).await;

    started.open();
    wait_shutdown(&agent, &f, 1).await;
    let kinds = f.wait_count("the interrupted turn's end", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    // Given the message it started for, then stopped: no second start.
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("hang"),
            FakeInput::Interrupt,
            FakeInput::Shutdown,
        ]
    );
    assert!(!kinds.iter().any(is_errored), "{kinds:#?}");
}

#[tokio::test]
async fn a_clear_while_the_agent_starts_stops_it_and_the_new_context_starts_fresh() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    let started = Gate::new();
    agent.hold_start(f.session_id, started.clone());
    f.send("hang");
    wait_inputs(&agent, f.session_id, "the start", |i| !i.is_empty()).await;
    let next = ContextId::new();
    let marker = f.clear(next);
    wait_read(&f, marker).await;

    started.open();
    f.wait_count("the interrupted turn's end", 1, is_idle).await;
    f.send("next");
    f.wait_count("the new context's reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            message("hang"),
            FakeInput::Interrupt,
            FakeInput::Shutdown,
            started_as(next, FakeConversation::Fresh),
            message("next"),
            FakeInput::Shutdown,
        ]
    );
    // The first context's turn ran and ended in it, after the clear.
    assert_eq!(
        f.stamped_after(marker)[..2],
        [
            (state(SessionState::AgentWorking), f.first()),
            (state(SessionState::Idle), f.first()),
        ]
    );
}

#[tokio::test]
async fn a_message_during_an_idle_shutdown_resumes_the_agent_after_it() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let config = SupervisorConfig {
        idle_timeout: Duration::ZERO,
    };
    let supervisor = f.start_with(&agent, config).await;
    let stopped = Gate::new();
    agent.hold_shutdown(f.session_id, stopped.clone());
    f.send("one");
    f.wait_count("the first reply", 1, is_idle).await;
    wait_shutdown(&agent, &f, 1).await;
    let two = f.send("two");
    wait_read(&f, two).await;
    assert_eq!(
        agent.inputs(f.session_id),
        [f.started(false), message("one"), FakeInput::Shutdown]
    );

    stopped.open();
    f.wait_count("the second reply", 2, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id)[..5],
        [
            f.started(false),
            message("one"),
            FakeInput::Shutdown,
            f.started(true),
            message("two"),
        ]
    );
}

/// A daemon shutdown drops a start under way (which kills its agent) rather
/// than wait for it; the message it was for was not handed over, so the
/// next run delivers it.
#[tokio::test]
async fn a_daemon_shutdown_drops_a_start_under_way_and_the_next_run_delivers_its_message() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    // Never opened.
    agent.hold_start(f.session_id, Gate::new());
    f.send("held");
    wait_inputs(&agent, f.session_id, "the start", |i| !i.is_empty()).await;
    tokio::time::timeout(WAIT, supervisor.shutdown())
        .await
        .expect("the shutdown waited for the start")
        .unwrap();
    assert!(held_row(&f, f.session_id).is_some());

    f.reopen_log();
    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("the reply", 1, is_idle).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(f.session_id),
        [
            f.started(false),
            f.started(false),
            message("held"),
            FakeInput::Shutdown,
        ]
    );
    assert!(!kinds.iter().any(is_errored), "{kinds:#?}");
    assert_eq!(held_row(&f, f.session_id), None);
}

/// A stop under way when the daemon shuts down runs alongside the warm
/// agents' stops: the fake holds each shutdown until both are under way.
#[tokio::test]
async fn a_daemon_shutdown_stops_the_warm_agents_alongside_a_stop_under_way() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().shutdowns_wait_for_each_other(2));
    let supervisor = f.start(&agent).await;
    let stopping = f.session_id;
    f.send("one");
    f.wait_count("the first session's reply", 1, is_idle).await;
    let warm = f.create_session();
    f.session_id = warm;
    f.send("two");
    f.wait_count("the second session's reply", 1, is_idle).await;
    let before = f.kinds();

    f.session_id = stopping;
    accepted(f.stop());
    wait_shutdown(&agent, &f, 1).await;
    tokio::time::timeout(WAIT, supervisor.shutdown())
        .await
        .expect("the agents stopped one after another")
        .unwrap();
    assert_eq!(agent.inputs(warm).last(), Some(&FakeInput::Shutdown));
    // The warm agent's exit is left unrecorded for the next start.
    f.session_id = warm;
    assert_eq!(f.kinds(), before);
}

/// Session `session`'s row in `supervisor_held`, if it has one.
fn held_row(f: &Fixture, session: SessionId) -> Option<i64> {
    let row = f.storage.db.conn().query_row(
        "SELECT seq FROM supervisor_held WHERE session_id = ?1",
        [session.0.to_string()],
        |r| r.get(0),
    );
    match row {
        Ok(seq) => Some(seq),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => panic!("{error}"),
    }
}

/// Events the previous run read but held for their session's agent are
/// handled by the next run; a message the cursor passed with no hold is
/// not.
#[tokio::test]
async fn a_message_held_when_the_daemon_stopped_is_delivered_by_the_next_run() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new());
    let held = f.session_id;
    let held_seq = f.send("held");
    let other = f.create_session();
    let passed = send_in(&mut f, other, "passed");
    let conn = f.storage.db.conn();
    conn.execute(
        "UPDATE supervisor_cursor SET seq = ?1 WHERE id = 1",
        [i64::try_from(passed.0).unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO supervisor_held (session_id, seq) VALUES (?1, ?2)",
        rusqlite::params![held.0.to_string(), i64::try_from(held_seq.0).unwrap() - 1],
    )
    .unwrap();

    let supervisor = f.start(&agent).await;
    let kinds = f.wait_count("the held message's reply", 1, is_idle).await;
    assert!(!kinds.iter().any(is_errored), "{kinds:#?}");
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(held),
        [f.started(false), message("held"), FakeInput::Shutdown]
    );
    assert_eq!(held_row(&f, held), None);
    // The other session's message was past the cursor with no hold.
    assert!(agent.inputs(other).is_empty());
}

/// A new session's start forks a moved context only once its origin
/// session settles: the clear before the move stops the origin's agent,
/// which may still be writing the conversation.
#[tokio::test]
async fn a_fork_waits_for_its_origin_sessions_stop() {
    let mut f = Fixture::new();
    let a = f.first();
    let agent = FakeAgent::new(FakeScript::new());
    let supervisor = f.start(&agent).await;
    f.send("one");
    f.wait_count("the reply", 1, is_idle).await;

    let stopped = Gate::new();
    agent.hold_shutdown(f.session_id, stopped.clone());
    f.clear(ContextId::new());
    wait_shutdown(&agent, &f, 1).await;
    let (_, new) = move_context(&mut f, a);
    let to_new = send_in(&mut f, new, "in new");
    wait_read(&f, to_new).await;
    assert_eq!(starts_in(&agent, new), []);
    assert!(held_row(&f, new).is_some());

    stopped.open();
    wait_idle_in(&f, new, 1).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        agent.inputs(new),
        [
            started_as(ContextId::first_of(new), FakeConversation::Fork(a)),
            message("in new"),
            FakeInput::Shutdown,
        ]
    );
}

// Stopping serve over its control socket (`skywaved::control`) while a
// turn runs or is on its way.

impl Fixture {
    fn control(&self, supervisor: &SupervisorHandle) -> Control {
        Control::bind(
            self.storage.dir.control_socket(),
            &self.storage.dir.db_path(),
            self.log.clone(),
            supervisor.activity(),
        )
        .unwrap()
    }

    async fn client(&self) -> TestClient {
        TestClient::connect(&self.storage.dir.control_socket()).await
    }

    /// The session's turn as the control socket lists it.
    fn listed_turn(&self) -> Work {
        Work::Turn {
            session: self.session_id,
            workdir: (self.log.session(&self.storage.db, self.session_id))
                .unwrap()
                .unwrap()
                .workdir,
        }
    }

    fn try_send(&mut self, text: &str) -> CommandResult {
        let context = self.current();
        self.handle(
            DeviceId::new(),
            CommandKind::SendMessage {
                session_id: self.session_id,
                context,
                text: text.into(),
            },
        )
    }
}

#[tokio::test]
async fn a_stop_now_needs_the_running_turn_confirmed() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    f.send("work");
    f.wait_count("the turn", 1, |k| *k == state(SessionState::AgentWorking))
        .await;
    let mut control = f.control(&supervisor);
    let mut client = f.client().await;
    let turn = f.listed_turn();
    assert_eq!(
        client.ask(stop_now(Confirm::Nothing)).await,
        Reply::Busy {
            work: vec![turn.clone()]
        }
    );
    assert_eq!(
        client.ask(stop_now(Confirm::Seen(vec![turn]))).await,
        Reply::Stopping { restart: false }
    );
    assert_eq!(control.stopped().await, Stop { restart: false });
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stop_after_work_waits_for_the_running_turn_and_refuses_messages() {
    let mut f = Fixture::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Hang]));
    let supervisor = f.start(&agent).await;
    f.send("work");
    f.wait_count("the turn", 1, |k| *k == state(SessionState::AgentWorking))
        .await;
    let mut control = f.control(&supervisor);
    let mut client = f.client().await;
    assert_eq!(
        client.ask(stop_after_work(false)).await,
        Reply::Waiting {
            work: vec![f.listed_turn()]
        }
    );
    assert_eq!(rejected(f.try_send("more")), RejectReason::ServerStopping);
    // Control is still taken.
    accepted(f.interrupt());
    f.wait_count("the interrupted turn's end", 1, is_idle).await;
    assert!(!client.stopping().await);
    assert_eq!(control.stopped().await, Stop { restart: false });
    supervisor.shutdown().await.unwrap();
}

/// A message accepted before the wait began, whose agent still starts: no
/// turn is in the log yet, but the supervisor has it on its way.
#[tokio::test]
async fn a_stop_after_work_waits_for_a_message_whose_agent_still_starts() {
    let mut f = Fixture::new();
    let turn = Gate::new();
    let agent = FakeAgent::new(FakeScript::new().turn([Step::Wait(turn.clone())]));
    let started = Gate::new();
    agent.hold_start(f.session_id, started.clone());
    let supervisor = f.start(&agent).await;
    f.send("work");
    wait_inputs(&agent, f.session_id, "the held start", |i| !i.is_empty()).await;

    let mut control = f.control(&supervisor);
    let mut client = f.client().await;
    assert_eq!(
        client.ask(stop_after_work(true)).await,
        Reply::Waiting { work: vec![] }
    );
    started.open();
    assert_eq!(
        client.next().await,
        Reply::Waiting {
            work: vec![f.listed_turn()]
        }
    );
    turn.open();
    f.wait_count("the turn's end", 1, is_idle).await;
    assert!(client.stopping().await);
    assert_eq!(control.stopped().await, Stop { restart: true });
    supervisor.shutdown().await.unwrap();
}
