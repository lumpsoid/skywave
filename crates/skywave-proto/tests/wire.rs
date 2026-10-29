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
fn blob() -> BlobRef {
    BlobRef {
        id: Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0005).into(),
        size: 48_213,
    }
}
fn at() -> DateTime<Utc> {
    "2026-10-29T12:00:00Z".parse().unwrap()
}

fn event(seq: u64, kind: EventKind) -> Event {
    Event {
        seq: Seq(seq),
        session_id: session(),
        at: at(),
        kind,
    }
}

fn all_event_kinds() -> Vec<EventKind> {
    vec![
        EventKind::SessionCreated,
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
        EventKind::SessionStateChanged(SessionState::Idle),
        EventKind::SessionStateChanged(SessionState::AgentWorking),
        EventKind::SessionStateChanged(SessionState::Errored {
            message: "agent exited".into(),
        }),
    ]
}

fn all_command_kinds() -> Vec<CommandKind> {
    vec![
        CommandKind::CreateSession,
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
    ]
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
    for (i, kind) in all_event_kinds().into_iter().enumerate() {
        round_trip(&event(i as u64 + 1, kind));
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
fn fresh_ids_are_uuid_v7() {
    assert_eq!(SessionId::new().0.get_version_num(), 7);
    assert_eq!(CommandId::new().0.get_version_num(), 7);
}

/// Pins the JSON wire format. If this fails, the format changed: update the
/// expectation deliberately (`UPDATE_SNAPSHOTS=1 cargo test`) and consider
/// bumping `PROTOCOL_VERSION`.
#[test]
fn wire_format_snapshot() {
    let events: Vec<Event> = all_event_kinds()
        .into_iter()
        .enumerate()
        .map(|(i, kind)| event(i as u64 + 1, kind))
        .collect();
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
        events: Vec<Event>,
        commands: Vec<Command>,
    }
    let actual = serde_json::to_string_pretty(&Snapshot {
        protocol_version: PROTOCOL_VERSION,
        events,
        commands,
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
