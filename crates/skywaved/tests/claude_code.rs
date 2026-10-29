//! Claude Code adapter (T09).
//!
//! Replay tests feed the T07 recordings (`tests/fixtures/claude-code/`)
//! through the protocol state machine: every recorded stdout line goes in,
//! and every recorded stdin line must be what the adapter itself would have
//! written at that point. Process tests run the adapter against
//! `tests/support/fake-claude.sh`, a stand-in that replies only to what it
//! reads, so they are deterministic.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use skywave_proto::{Decision, SessionId};
use skywaved::agent::claude_code::protocol::{self, INTERRUPT_REASON, PINNED_VERSION, Protocol};
use skywaved::agent::claude_code::{ClaudeCodeAdapter, REQUIRED_ARGS};
use skywaved::agent::{
    AdapterEvent, AdapterEvents, AgentAdapter, AgentError, AgentRequestId, AgentSession,
    ResumeHandle, StartSpec, TurnOutcome,
};
use skywaved::config::AgentConfig;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Replay

fn fixture(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code")
        .join(format!("{name}.ndjson"));
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Our own control requests carry ids we choose; compare them without.
fn normalized(message: &Value) -> Value {
    let mut message = message.clone();
    if message["type"] == "control_request" {
        message.as_object_mut().unwrap().remove("request_id");
    }
    message
}

/// Replays fixture `name` through a fresh protocol for CLI session
/// `session_id` and returns the events it reported. The `n`th recorded stdin
/// line (0-based) must be what the protocol writes for the matching request,
/// except those listed in `dropped`, for which it must write nothing.
fn replay(name: &str, session_id: Uuid, resumed: bool, dropped: &[usize]) -> Vec<AdapterEvent> {
    let mut protocol = Protocol::new(session_id, resumed);
    let start = protocol.start();
    let mut events = start.events;
    let mut written: VecDeque<Value> = start.writes.into();
    let mut n_in = 0;
    for record in fixture(name) {
        let msg = &record["msg"];
        match record["dir"].as_str().unwrap() {
            "in" => {
                let out = match (
                    msg["type"].as_str().unwrap(),
                    msg["request"]["subtype"].as_str(),
                ) {
                    // Written by `start`.
                    ("control_request", Some("initialize")) => Default::default(),
                    ("control_request", Some("interrupt")) => protocol.interrupt(),
                    ("user", _) => protocol
                        .send_message(msg["message"]["content"].as_str().unwrap().to_owned()),
                    ("control_response", _) => {
                        let response = &msg["response"];
                        let decision = match response["response"]["behavior"].as_str() {
                            Some("allow") => Decision::Approve,
                            _ => Decision::Deny,
                        };
                        let request = response["request_id"].as_str().unwrap();
                        protocol.answer(&AgentRequestId::from(request), decision)
                    }
                    other => panic!("{name}: unexpected stdin record {other:?}"),
                };
                events.extend(out.events);
                written.extend(out.writes);
                if dropped.contains(&n_in) {
                    assert!(
                        written.is_empty(),
                        "{name}: stdin #{n_in} should write nothing, wrote {written:?}"
                    );
                } else {
                    let ours = written
                        .pop_front()
                        .unwrap_or_else(|| panic!("{name}: nothing written for stdin #{n_in}"));
                    assert_eq!(normalized(&ours), normalized(msg), "{name}: stdin #{n_in}");
                }
                n_in += 1;
            }
            "out" => {
                let line = serde_json::to_string(msg).unwrap();
                let out = protocol.on_line(&line);
                assert!(
                    out.writes.is_empty(),
                    "{name}: unexpected writes {:?}",
                    out.writes
                );
                events.extend(out.events);
            }
            _ => {}
        }
    }
    assert!(written.is_empty(), "{name}: left over writes {written:?}");
    assert!(!protocol.turn_open(), "{name}: a turn is still open");
    assert_eq!(protocol.version(), Some(PINNED_VERSION));
    events
}

/// One line per event, cut short, for readable expectations.
fn describe(events: &[AdapterEvent]) -> Vec<String> {
    fn cut(text: &str) -> String {
        let first = text.lines().next().unwrap_or("");
        first.chars().take(48).collect()
    }
    events
        .iter()
        .map(|event| match event {
            AdapterEvent::TurnStarted => "turn started".to_owned(),
            AdapterEvent::Text(text) => format!("text: {}", cut(text)),
            AdapterEvent::ToolCall { name, .. } => format!("tool call: {name}"),
            AdapterEvent::ToolResult { output, is_error } => {
                let kind = if *is_error { "error" } else { "ok" };
                format!("tool result {kind}: {}", cut(output))
            }
            AdapterEvent::ApprovalRequested { request, tool, .. } => {
                format!("approval {}: {tool}", request.0)
            }
            AdapterEvent::ApprovalCancelled { request, reason } => {
                format!("cancelled {}: {}", request.0, cut(reason))
            }
            AdapterEvent::TurnEnded(outcome) => format!("turn ended: {outcome:?}"),
            AdapterEvent::ResumeHandle(handle) => {
                format!(
                    "resume handle: {}",
                    handle.0["session_id"].as_str().unwrap()
                )
            }
            AdapterEvent::Exited { error } => format!("exited: {error:?}"),
        })
        .collect()
}

fn uuid(id: &str) -> Uuid {
    Uuid::parse_str(id).unwrap()
}

const BASIC_SESSION: &str = "ce771f8e-e3a8-428f-b5d7-e2f40233a2b6";

#[test]
fn replay_basic_two_turns() {
    let events = replay("basic", uuid(BASIC_SESSION), false, &[]);
    assert_eq!(
        describe(&events),
        [
            "resume handle: ce771f8e-e3a8-428f-b5d7-e2f40233a2b6",
            "turn started",
            "text: Hi there!",
            "turn ended: Completed",
            "turn started",
            "text: You asked me to say hi in exactly two words.",
            "turn ended: Completed",
        ]
    );
}

#[test]
fn replay_resume_reports_no_new_handle() {
    let events = replay("resume", uuid(BASIC_SESSION), true, &[]);
    assert_eq!(
        describe(&events),
        [
            "turn started",
            "text: \"Say hi in exactly two words.\"",
            "turn ended: Completed",
        ]
    );
}

#[test]
fn replay_permissions_allow_then_deny() {
    let events = replay(
        "permissions",
        uuid("7d7945cf-3afb-478c-b0e7-a1e6b4dff356"),
        false,
        &[],
    );
    assert_eq!(
        describe(&events),
        [
            "resume handle: 7d7945cf-3afb-478c-b0e7-a1e6b4dff356",
            "turn started",
            "tool call: Write",
            "approval 6587ec1f-4dd9-4cd1-b704-51fcb4f5ec34: Write",
            "tool result ok: File created successfully at: /tmp/skywave-t07-s",
            "text: I created `/tmp/skywave-t07-scratch/hello.txt` c",
            "turn ended: Completed",
            "turn started",
            "tool call: Bash",
            "approval 5446b928-31cd-4fb3-b499-1f4fccf50f2a: Bash",
            "tool result error: The user denied this action.",
            "text: The `rm` was denied, so I haven't deleted `hello",
            "turn ended: Completed",
        ]
    );
    // The approval carries the tool's input, the same as its tool call.
    let write = json!({ "file_path": "/tmp/skywave-t07-scratch/hello.txt", "content": "hi" });
    assert!(matches!(&events[2], AdapterEvent::ToolCall { input, .. } if *input == write));
    assert!(matches!(&events[3], AdapterEvent::ApprovalRequested { input, .. } if *input == write));
}

#[test]
fn replay_interrupt_while_streaming_and_while_idle() {
    // This run had no --session-id: the CLI's own id is adopted.
    let ours = uuid("11111111-1111-4111-8111-111111111111");
    // stdin #4 is an interrupt sent while idle: the adapter writes nothing.
    let events = replay("interrupt", ours, false, &[4]);
    let cli = fixture("interrupt")
        .into_iter()
        .find(|r| r["msg"]["subtype"] == "init")
        .unwrap()["msg"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        describe(&events),
        [
            format!("resume handle: {ours}"),
            "turn started".to_owned(),
            format!("resume handle: {cli}"),
            "text: 1".to_owned(),
            "turn ended: Interrupted".to_owned(),
            "turn started".to_owned(),
            "tool call: Bash".to_owned(),
            "tool result ok: pending-test".to_owned(),
            "text: pending-test".to_owned(),
            "turn ended: Completed".to_owned(),
            "turn started".to_owned(),
            "text: ok".to_owned(),
            "turn ended: Completed".to_owned(),
        ]
    );
}

#[test]
fn replay_interrupt_with_a_pending_approval() {
    let cli = "5be4083d-8321-408c-a950-44c6e8a7f067";
    // stdin #3 is the late allow for the cancelled request: dropped.
    let events = replay("interrupt_pending", uuid(cli), false, &[3]);
    assert_eq!(
        describe(&events),
        [
            format!("resume handle: {cli}"),
            "turn started".to_owned(),
            "tool call: Write".to_owned(),
            "approval aa219e26-965b-44bc-ad3e-0f57fede7ee4: Write".to_owned(),
            format!("cancelled aa219e26-965b-44bc-ad3e-0f57fede7ee4: {INTERRUPT_REASON}"),
            "tool result error: The user doesn't want to proceed with this tool ".to_owned(),
            "turn ended: Interrupted".to_owned(),
            // Then stdin closes while a second approval waits: the CLI fails
            // the approval itself and carries on.
            "turn started".to_owned(),
            "tool call: Write".to_owned(),
            "approval 071d7172-762d-450e-8cca-676210e9b4c4: Write".to_owned(),
            "cancelled 071d7172-762d-450e-8cca-676210e9b4c4: Tool permission request failed: AbortError: Tool"
                .to_owned(),
            "tool result error: Tool permission request failed: AbortError: Tool".to_owned(),
            "tool call: Write".to_owned(),
            "tool result error: Tool permission request failed: AbortError: Stre".to_owned(),
            "text: I couldn't create `pending2.txt`. The Write call".to_owned(),
            "turn ended: Completed".to_owned(),
        ]
    );
}

#[test]
fn a_message_during_a_turn_waits_for_its_result() {
    let mut protocol = Protocol::new(Uuid::new_v4(), true);
    protocol.start();
    protocol.on_line(r#"{"type":"control_response","response":{"subtype":"success","request_id":"skywave_1","response":{}}}"#);
    assert!(protocol.ready());
    let first = protocol.send_message("one".to_owned());
    assert_eq!(first.events, [AdapterEvent::TurnStarted]);
    let queued = protocol.send_message("two".to_owned());
    assert_eq!(queued, Default::default());
    let end =
        protocol.on_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#);
    assert_eq!(
        end.events,
        [
            AdapterEvent::TurnEnded(TurnOutcome::Completed),
            AdapterEvent::TurnStarted
        ]
    );
    assert_eq!(end.writes, [protocol::user_message("two")]);
}

#[test]
fn a_failed_turn_and_unknown_messages() {
    let mut protocol = Protocol::new(Uuid::new_v4(), true);
    protocol.start();
    protocol.send_message("go".to_owned());
    for line in [
        r#"{"type":"something_new","x":1}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        "",
    ] {
        assert_eq!(protocol.on_line(line), Default::default());
    }
    // A control request the adapter does not handle is refused, not ignored.
    let hook = protocol.on_line(
        r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback"}}"#,
    );
    assert_eq!(hook.writes[0]["response"]["subtype"], "error");
    assert_eq!(hook.writes[0]["response"]["request_id"], "h1");
    let end = protocol.on_line(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"errors":["API down"]}"#,
    );
    assert_eq!(
        end.events,
        [AdapterEvent::TurnEnded(TurnOutcome::Failed(
            "API down".to_owned()
        ))]
    );
}

#[test]
fn resume_handles_round_trip() {
    let id = Uuid::new_v4();
    let handle = protocol::resume_handle(id);
    assert_eq!(handle.0, json!({ "session_id": id.to_string() }));
    assert_eq!(protocol::parse_resume_handle(&handle), Ok(id));
    assert!(protocol::parse_resume_handle(&ResumeHandle(json!({ "other": 1 }))).is_err());
}

// ---------------------------------------------------------------------------
// Process

const WAIT: Duration = Duration::from_secs(10);

fn fake_cli() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-claude.sh")
}

fn adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(AgentConfig {
        command: fake_cli(),
        args: vec!["--model".to_owned(), "haiku".to_owned()],
    })
    .with_grace_period(Duration::from_secs(5))
}

fn spec(workdir: &Path, resume: Option<ResumeHandle>) -> StartSpec {
    StartSpec {
        session_id: SessionId::new(),
        workdir: workdir.to_owned(),
        resume,
    }
}

async fn next(events: &mut AdapterEvents) -> AdapterEvent {
    tokio::time::timeout(WAIT, events.recv())
        .await
        .expect("no adapter event in time")
        .expect("adapter event stream ended")
}

async fn take(events: &mut AdapterEvents, n: usize) -> Vec<AdapterEvent> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(next(events).await);
    }
    out
}

fn argv(workdir: &Path) -> Vec<String> {
    std::fs::read_to_string(workdir.join("argv.txt"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn text(text: &str) -> AdapterEvent {
    AdapterEvent::Text(text.to_owned())
}

fn tool_result(output: &str, is_error: bool) -> AdapterEvent {
    AdapterEvent::ToolResult {
        output: output.to_owned(),
        is_error,
    }
}

fn write_call() -> AdapterEvent {
    AdapterEvent::ToolCall {
        name: "Write".to_owned(),
        input: json!({ "file_path": "a.txt", "content": "hi" }),
    }
}

fn write_approval() -> AdapterEvent {
    AdapterEvent::ApprovalRequested {
        request: "perm-1".into(),
        tool: "Write".to_owned(),
        input: json!({ "file_path": "a.txt", "content": "hi" }),
    }
}

const COMPLETED: AdapterEvent = AdapterEvent::TurnEnded(TurnOutcome::Completed);
const INTERRUPTED: AdapterEvent = AdapterEvent::TurnEnded(TurnOutcome::Interrupted);

#[tokio::test]
async fn a_session_runs_turns_approvals_and_interrupts() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();

    // The handle is known before any turn, and names the --session-id.
    let AdapterEvent::ResumeHandle(handle) = next(&mut events).await else {
        panic!("expected the resume handle first");
    };
    let id = protocol::parse_resume_handle(&handle).unwrap();
    let mut expected: Vec<String> = REQUIRED_ARGS.iter().map(|a| (*a).to_owned()).collect();
    expected.extend(["--session-id".to_owned(), id.to_string()]);
    expected.extend(["--model".to_owned(), "haiku".to_owned()]);
    assert_eq!(argv(tmp.path()), expected);

    session.send_message("hello".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, text("echo"), COMPLETED]
    );

    // Allow, with a message queued behind the approval and a stale answer.
    session
        .send_message("please approve".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    session.send_message("queued".to_owned()).await.unwrap();
    session
        .answer_approval("not-pending".into(), Decision::Approve)
        .await
        .unwrap();
    session
        .answer_approval("perm-1".into(), Decision::Approve)
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 6).await,
        [
            tool_result("written", false),
            text("done writing"),
            COMPLETED,
            AdapterEvent::TurnStarted,
            text("echo"),
            COMPLETED,
        ]
    );

    // Deny.
    session
        .send_message("approve again".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    session
        .answer_approval("perm-1".into(), Decision::Deny)
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [
            tool_result("The user denied this action.", true),
            text("not writing"),
            COMPLETED,
        ]
    );

    // Interrupt while the approval waits: cancelled, then interrupted.
    session
        .send_message("approve, then stop".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, write_call(), write_approval()]
    );
    session.interrupt().await.unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [
            AdapterEvent::ApprovalCancelled {
                request: "perm-1".into(),
                reason: INTERRUPT_REASON.to_owned(),
            },
            tool_result("rejected", true),
            INTERRUPTED,
        ]
    );

    // Interrupt while text streams; the session stays usable.
    session.send_message("hang".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [AdapterEvent::TurnStarted, text("working")]
    );
    session.interrupt().await.unwrap();
    session.interrupt().await.unwrap();
    assert_eq!(next(&mut events).await, INTERRUPTED);

    // Idle: an interrupt does nothing, a message runs a normal turn.
    session.interrupt().await.unwrap();
    session
        .send_message("still there?".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, text("echo"), COMPLETED]
    );

    session.shutdown().await.unwrap();
    assert_eq!(
        next(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn a_restart_resumes_with_the_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let handle = protocol::resume_handle(id);
    let (mut session, mut events) = adapter()
        .start(spec(tmp.path(), Some(handle)))
        .await
        .unwrap();
    let args = argv(tmp.path());
    let at = args.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(args[at + 1], id.to_string());
    assert!(!args.contains(&"--session-id".to_owned()));

    // No new handle: the first event is the turn.
    session
        .send_message("hello again".to_owned())
        .await
        .unwrap();
    assert_eq!(
        take(&mut events, 3).await,
        [AdapterEvent::TurnStarted, text("echo"), COMPLETED]
    );
    session.shutdown().await.unwrap();
    assert_eq!(
        next(&mut events).await,
        AdapterEvent::Exited { error: None }
    );
}

#[tokio::test]
async fn resuming_an_unknown_session_is_a_resume_error() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = protocol::resume_handle(Uuid::nil());
    let error = adapter()
        .start(spec(tmp.path(), Some(handle)))
        .await
        .unwrap_err();
    match error {
        AgentError::Resume(message) => {
            assert!(message.contains("No conversation found"), "{message}")
        }
        other => panic!("expected a resume error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_foreign_resume_handle_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let handle = ResumeHandle(json!({ "fake_session": "x" }));
    let error = adapter()
        .start(spec(tmp.path(), Some(handle)))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::Resume(_)), "{error:?}");
}

#[tokio::test]
async fn a_missing_command_is_a_start_error() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = ClaudeCodeAdapter::new(AgentConfig {
        command: tmp.path().join("no-such-agent"),
        args: Vec::new(),
    });
    let error = adapter.start(spec(tmp.path(), None)).await.unwrap_err();
    assert!(matches!(error, AgentError::Start(_)), "{error:?}");
}

#[tokio::test]
async fn shutdown_mid_turn_interrupts_and_waits_for_the_result() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("hang".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [AdapterEvent::TurnStarted, text("working")]
    );
    // A message queued behind the running turn is dropped by the shutdown.
    session.send_message("never sent".to_owned()).await.unwrap();
    session.shutdown().await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [INTERRUPTED, AdapterEvent::Exited { error: None }]
    );
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn a_crash_is_reported_with_stderr_and_closes_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("crash".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [
            AdapterEvent::TurnStarted,
            AdapterEvent::Exited {
                error: Some("boom: the agent fell over".to_owned())
            },
        ]
    );
    assert!(events.recv().await.is_none());
    assert!(matches!(
        session.send_message("anyone?".to_owned()).await,
        Err(AgentError::Closed)
    ));
}

#[tokio::test]
async fn dropping_the_session_stops_the_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, mut events) = adapter().start(spec(tmp.path(), None)).await.unwrap();
    assert!(matches!(
        next(&mut events).await,
        AdapterEvent::ResumeHandle(_)
    ));
    session.send_message("hang".to_owned()).await.unwrap();
    assert_eq!(
        take(&mut events, 2).await,
        [AdapterEvent::TurnStarted, text("working")]
    );
    drop(session);
    assert_eq!(
        take(&mut events, 2).await,
        [INTERRUPTED, AdapterEvent::Exited { error: None }]
    );
}
