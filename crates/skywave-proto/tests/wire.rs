use chrono::{DateTime, Utc};
use skywave_proto::*;
use uuid::Uuid;

fn session() -> SessionId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0001).into()
}
fn command() -> CommandId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0002).into()
}
fn device() -> DeviceId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0003).into()
}
fn approval() -> ApprovalId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0004).into()
}
fn blob_id() -> BlobId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0005).into()
}
fn server() -> ServerId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0006).into()
}
fn call() -> CallId {
    Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0007).into()
}
fn blob() -> BlobRef {
    BlobRef {
        id: blob_id(),
        size: 48_213,
    }
}
fn at() -> DateTime<Utc> {
    "2026-10-29T12:00:00Z".parse().unwrap()
}

fn event(kind: EventKind) -> Event {
    Event {
        session_id: session(),
        at: at(),
        kind,
    }
}

fn all_event_kinds() -> Vec<EventKind> {
    vec![
        EventKind::SessionCreated {
            workdir: "/srv/code/skywave".into(),
        },
        EventKind::UserMessage {
            command_id: command(),
            device_id: device(),
            text: "run the tests".into(),
        },
        EventKind::AgentMessage {
            text: "Running cargo test.".into(),
        },
        EventKind::ToolCall(ToolCall {
            call: Some(call()),
            name: "exec".into(),
            display: ToolDisplay {
                kind: ToolKind::Shell,
                title: "cargo test".into(),
                detail: Some("run the test suite".into()),
                body: None,
            },
            input: None,
        }),
        EventKind::ToolCall(ToolCall {
            call: Some(call()),
            name: "patch".into(),
            display: ToolDisplay {
                kind: ToolKind::FileEdit,
                title: "src/lib.rs".into(),
                detail: None,
                body: Some(Body {
                    format: BodyFormat::Diff,
                    preview: "@@ -1 +1 @@\n-old\n+new".into(),
                    lines: 120,
                    full: Some(blob()),
                }),
            },
            input: Some(blob()),
        }),
        EventKind::ToolCall(ToolCall {
            call: None,
            name: "put".into(),
            display: ToolDisplay {
                kind: ToolKind::FileWrite,
                title: "notes.txt".into(),
                detail: None,
                body: Some(Body {
                    format: BodyFormat::Text,
                    preview: "hello".into(),
                    lines: 1,
                    full: None,
                }),
            },
            input: None,
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(call()),
            is_error: false,
            status: Some("exit 0".into()),
            preview: "running 3 tests\ntest a ... ok".into(),
            lines: 40,
            output: Some(blob()),
        }),
        EventKind::ToolResult(ToolResult {
            call: Some(call()),
            is_error: true,
            status: Some("exit 1".into()),
            preview: "error: no such file".into(),
            lines: 1,
            output: None,
        }),
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: String::new(),
            lines: 0,
            output: None,
        }),
        EventKind::ApprovalRequested {
            id: approval(),
            call: Some(call()),
            tool: "exec".into(),
            summary: "rm -rf target".into(),
            input: None,
        },
        EventKind::ApprovalRequested {
            id: approval(),
            call: None,
            tool: "put".into(),
            summary: "src/main.rs".into(),
            input: Some(blob()),
        },
        EventKind::ApprovalResolved {
            id: approval(),
            decision: Decision::Approve,
            by: device(),
        },
        EventKind::ApprovalResolved {
            id: approval(),
            decision: Decision::Deny,
            by: device(),
        },
        EventKind::ApprovalCancelled {
            id: approval(),
            reason: "daemon restarted".into(),
        },
        EventKind::TurnInterrupted,
        EventKind::SessionStateChanged(SessionState::Idle),
        EventKind::SessionStateChanged(SessionState::AgentWorking),
        EventKind::SessionStateChanged(SessionState::Errored {
            message: "agent exited".into(),
        }),
    ]
}

fn all_command_kinds() -> Vec<CommandKind> {
    vec![
        CommandKind::CreateSession {
            workdir: "/srv/code/skywave".into(),
        },
        CommandKind::SendMessage {
            session_id: session(),
            text: "hello".into(),
        },
        CommandKind::ResolveApproval {
            id: approval(),
            decision: Decision::Approve,
        },
        CommandKind::ResolveApproval {
            id: approval(),
            decision: Decision::Deny,
        },
        CommandKind::Interrupt {
            session_id: session(),
        },
    ]
}

fn all_command_results() -> Vec<CommandResult> {
    let mut results = vec![CommandResult::Accepted { seq: Seq(42) }];
    results.extend(
        [
            RejectReason::WorkdirNotAllowed,
            RejectReason::WorkdirNotFound,
            RejectReason::UnknownSession,
            RejectReason::UnknownApproval,
            RejectReason::ApprovalAlreadyResolved,
            RejectReason::NoActiveTurn,
        ]
        .map(|reason| CommandResult::Rejected { reason }),
    );
    results
}

fn batch() -> EventBatch {
    EventBatch {
        from: Seq(1),
        events: all_event_kinds().into_iter().map(event).collect(),
    }
}

fn pair_request() -> PairRequest {
    PairRequest {
        code: "K7Q4-9M2X".into(),
        csr_pem: "-----BEGIN CERTIFICATE REQUEST-----\nMIIB\n-----END CERTIFICATE REQUEST-----\n"
            .into(),
        device_name: "laptop".into(),
    }
}

fn pair_response() -> PairResponse {
    PairResponse {
        device_id: device(),
        device_cert_pem: "-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n".into(),
        ca_cert_pem: "-----BEGIN CERTIFICATE-----\nMIID\n-----END CERTIFICATE-----\n".into(),
    }
}

fn hello() -> HelloResponse {
    HelloResponse {
        protocol_version: PROTOCOL_VERSION,
        server_id: server(),
        device_id: device(),
        head: Seq(16),
    }
}

fn round_trip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "round trip changed value; json: {json}");
}

#[test]
fn every_event_kind_round_trips() {
    for kind in all_event_kinds() {
        round_trip(&event(kind));
    }
}

#[test]
fn every_tool_kind_and_body_format_round_trips() {
    let kinds = [
        ToolKind::FileWrite,
        ToolKind::FileEdit,
        ToolKind::FileRead,
        ToolKind::Shell,
        ToolKind::Search,
        ToolKind::Fetch,
        ToolKind::Subagent,
        ToolKind::Other,
    ];
    for kind in kinds {
        round_trip(&kind);
    }
    let names: Vec<String> = kinds
        .iter()
        .map(|k| serde_json::to_string(k).unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "\"file_write\"",
            "\"file_edit\"",
            "\"file_read\"",
            "\"shell\"",
            "\"search\"",
            "\"fetch\"",
            "\"subagent\"",
            "\"other\""
        ]
    );
    round_trip(&BodyFormat::Text);
    round_trip(&BodyFormat::Diff);
}

#[test]
fn unknown_tool_kind_decodes_as_other() {
    let kind: ToolKind = serde_json::from_str("\"teleport\"").unwrap();
    assert_eq!(kind, ToolKind::Other);
}

fn decode(json: &str) -> EventKind {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {json}"))
}

/// Literal events as a v1 server wrote them still decode.
#[test]
fn v1_tool_call_decodes_with_an_other_display() {
    let kind = decode(
        r#"{"type":"tool_call","name":"exec","summary":"cargo test","input":{"id":"01920000-0000-7000-8000-000000000005","size":48213}}"#,
    );
    assert_eq!(
        kind,
        EventKind::ToolCall(ToolCall {
            call: None,
            name: "exec".into(),
            display: ToolDisplay {
                kind: ToolKind::Other,
                title: "cargo test".into(),
                detail: None,
                body: None,
            },
            input: Some(blob()),
        })
    );
    let kind = decode(r#"{"type":"tool_call","name":"exec","summary":"ls","input":null}"#);
    let EventKind::ToolCall(call) = kind else {
        panic!("{kind:?}")
    };
    assert_eq!(call.display.title, "ls");
    assert_eq!(call.input, None);
}

#[test]
fn v1_tool_result_decodes_with_the_summary_as_preview() {
    let kind =
        decode(r#"{"type":"tool_result","summary":"[error] line one\nline two","output":null}"#);
    assert_eq!(
        kind,
        EventKind::ToolResult(ToolResult {
            call: None,
            is_error: false,
            status: None,
            preview: "[error] line one\nline two".into(),
            lines: 2,
            output: None,
        })
    );
    let kind = decode(
        r#"{"type":"tool_result","summary":"(no output)","output":{"id":"01920000-0000-7000-8000-000000000005","size":48213}}"#,
    );
    let EventKind::ToolResult(result) = kind else {
        panic!("{kind:?}")
    };
    assert_eq!((result.lines, result.output), (1, Some(blob())));
}

#[test]
fn v1_approval_requested_decodes_without_a_call() {
    let kind = decode(
        r#"{"type":"approval_requested","id":"01920000-0000-7000-8000-000000000004","tool":"exec","summary":"rm -rf target","input":null}"#,
    );
    assert_eq!(
        kind,
        EventKind::ApprovalRequested {
            id: approval(),
            call: None,
            tool: "exec".into(),
            summary: "rm -rf target".into(),
            input: None,
        }
    );
}

#[test]
fn tool_events_without_display_or_summary_are_rejected() {
    for json in [
        r#"{"type":"tool_call","name":"exec","input":null}"#,
        r#"{"type":"tool_result","output":null}"#,
        r#"{"type":"tool_result","preview":"half","output":null}"#,
    ] {
        assert!(serde_json::from_str::<EventKind>(json).is_err(), "{json}");
    }
}

#[test]
fn display_caps() {
    assert_eq!(limits::TITLE_MAX_CHARS, 120);
    assert_eq!(limits::DETAIL_MAX_CHARS, 120);
    assert_eq!(limits::STATUS_MAX_CHARS, 60);
    assert_eq!(limits::PREVIEW_MAX_LINES, 8);
    assert_eq!(limits::PREVIEW_MAX_BYTES, 512);
}

#[test]
fn every_command_kind_round_trips() {
    for kind in all_command_kinds() {
        round_trip(&Command {
            id: command(),
            kind,
        });
    }
}

#[test]
fn every_command_result_round_trips() {
    for result in all_command_results() {
        round_trip(&result);
    }
}

#[test]
fn api_bodies_round_trip() {
    round_trip(&batch());
    round_trip(&EventBatch {
        from: Seq(17),
        events: vec![],
    });
    round_trip(&pair_request());
    round_trip(&pair_response());
    round_trip(&hello());
    round_trip(&EventsQuery {
        after: Seq(16),
        wait: Some(30),
        limit: Some(500),
    });
    round_trip(&EventsQuery {
        after: Seq::ZERO,
        wait: None,
        limit: None,
    });
}

#[test]
fn events_carry_no_seq_on_the_wire() {
    let json = serde_json::to_value(event(EventKind::TurnInterrupted)).unwrap();
    assert!(json.get("seq").is_none(), "{json}");
}

#[test]
fn batch_assigns_contiguous_seqs() {
    let batch = EventBatch {
        from: Seq(7),
        events: vec![
            event(EventKind::TurnInterrupted),
            event(EventKind::SessionStateChanged(SessionState::Idle)),
            event(EventKind::AgentMessage { text: "ok".into() }),
        ],
    };
    let seqs: Vec<Seq> = batch.iter().map(|(seq, _)| seq).collect();
    assert_eq!(seqs, [Seq(7), Seq(8), Seq(9)]);
    assert_eq!(batch.last(), Some(Seq(9)));

    let empty = EventBatch {
        from: Seq(10),
        events: vec![],
    };
    assert_eq!(empty.iter().count(), 0);
    assert_eq!(empty.last(), None);
    assert_eq!(Seq::ZERO.next(), Seq(1));
}

#[test]
fn api_paths() {
    assert_eq!(path::PAIR, "/v1/pair");
    assert_eq!(path::HELLO, "/v1/hello");
    assert_eq!(path::COMMANDS, "/v1/commands");
    assert_eq!(path::EVENTS, "/v1/events");
    assert_eq!(path::BLOB_ROUTE, "/v1/blobs/{id}");
    assert_eq!(
        path::blob(blob_id()),
        "/v1/blobs/01920000-0000-7000-8000-000000000005"
    );
}

#[test]
fn fresh_ids_are_uuid_v7() {
    assert_eq!(SessionId::new().0.get_version_num(), 7);
    assert_eq!(CommandId::new().0.get_version_num(), 7);
}

/// Pins the JSON wire format. If this fails, the format changed: update the
/// expectation deliberately (`UPDATE_SNAPSHOTS=1 cargo test`) and consider
/// bumping `PROTOCOL_VERSION`.
#[test]
fn wire_format_snapshot() {
    let commands: Vec<Command> = all_command_kinds()
        .into_iter()
        .map(|kind| Command {
            id: command(),
            kind,
        })
        .collect();
    // A struct, not `json!`, so keys keep their real wire order.
    #[derive(serde::Serialize)]
    struct Snapshot {
        protocol_version: u32,
        hello: HelloResponse,
        pair_request: PairRequest,
        pair_response: PairResponse,
        events_query: EventsQuery,
        event_batch: EventBatch,
        commands: Vec<Command>,
        command_results: Vec<CommandResult>,
    }
    let actual = serde_json::to_string_pretty(&Snapshot {
        protocol_version: PROTOCOL_VERSION,
        hello: hello(),
        pair_request: pair_request(),
        pair_response: pair_response(),
        events_query: EventsQuery {
            after: Seq(16),
            wait: Some(30),
            limit: Some(500),
        },
        event_batch: batch(),
        commands,
        command_results: all_command_results(),
    })
    .unwrap();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/wire.json");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(path, format!("{actual}\n")).unwrap();
    }
    let expected = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "\nactual:\n{actual}\n"
    );
}
