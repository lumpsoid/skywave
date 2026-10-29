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
        EventKind::ToolCall {
            name: "bash".into(),
            summary: "cargo test".into(),
            input: None,
        },
        EventKind::ToolCall {
            name: "write".into(),
            summary: "src/lib.rs".into(),
            input: Some(blob()),
        },
        EventKind::ToolResult {
            summary: "exit 0".into(),
            output: Some(blob()),
        },
        EventKind::ToolResult {
            summary: "no output".into(),
            output: None,
        },
        EventKind::ApprovalRequested {
            id: approval(),
            tool: "bash".into(),
            summary: "rm -rf target".into(),
            input: None,
        },
        EventKind::ApprovalRequested {
            id: approval(),
            tool: "write".into(),
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
