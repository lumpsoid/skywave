//! Skywave wire types, shared by `skywaved` and `skywave-core`.
//!
//! Encoded as JSON for the MVP. Large content is never inlined; it travels as
//! a [`BlobRef`] and is fetched separately. No type here names a concrete
//! agent (D3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire protocol version. Bump on any incompatible change to these types.
pub const PROTOCOL_VERSION: u32 = 1;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A fresh, time-ordered (UUIDv7) id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

uuid_id!(
    /// Identifies a session. Its events live in the server's global log (D12).
    SessionId
);
uuid_id!(
    /// Client-generated command id; the server dedups on it, so resends are safe.
    CommandId
);
uuid_id!(
    /// Identifies a paired device.
    DeviceId
);
uuid_id!(
    /// Identifies a pending approval request.
    ApprovalId
);
uuid_id!(
    /// Identifies a server; reported by `GET /v1/hello`.
    ServerId
);
uuid_id!(
    /// Identifies a blob of large content fetched separately from events.
    BlobId
);

/// Position in the server's global event log (D12).
///
/// One log spans every session. Seqs are contiguous: the first event is
/// `Seq(1)` and each later event is exactly one more. [`Seq::ZERO`] is the
/// position before the first event, so a fresh cursor is `Seq::ZERO`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// Before the first event; the cursor of a device that has seen nothing.
    pub const ZERO: Seq = Seq(0);

    /// The seq directly after this one.
    pub fn next(self) -> Seq {
        Seq(self.0 + 1)
    }
}

/// Reference to large content. Content is always referenced, never inlined.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BlobRef {
    pub id: BlobId,
    pub size: u64,
}

/// One entry in the global event log.
///
/// Carries no seq on the wire: its seq follows from its position in an
/// [`EventBatch`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Event {
    pub session_id: SessionId,
    pub at: DateTime<Utc>,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A session was created in `workdir`, a directory on the server (D14).
    SessionCreated {
        workdir: String,
    },
    UserMessage {
        command_id: CommandId,
        device_id: DeviceId,
        text: String,
    },
    AgentMessage {
        text: String,
    },
    ToolCall {
        name: String,
        summary: String,
        input: Option<BlobRef>,
    },
    ToolResult {
        summary: String,
        output: Option<BlobRef>,
    },
    /// The agent is blocked until a device answers (D8). `input` holds the
    /// full request when it is too large for `summary`.
    ApprovalRequested {
        id: ApprovalId,
        tool: String,
        summary: String,
        input: Option<BlobRef>,
    },
    /// First answer wins (D5).
    ApprovalResolved {
        id: ApprovalId,
        decision: Decision,
        by: DeviceId,
    },
    /// The request can no longer be answered, e.g. the daemon restarted.
    ApprovalCancelled {
        id: ApprovalId,
        reason: String,
    },
    /// The agent's current turn was stopped by an `Interrupt` command.
    TurnInterrupted,
    SessionStateChanged(SessionState),
}

/// Agent state as recorded in the log.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    AgentWorking,
    Errored { message: String },
}

/// Answer to an approval request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Deny,
}

/// Body of `GET /v1/events`: consecutive events starting at seq `from`.
///
/// Seqs are contiguous (D12), so the i-th event has seq `from + i`. An empty
/// batch (the long-poll timed out) has `from = after + 1`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EventBatch {
    pub from: Seq,
    pub events: Vec<Event>,
}

impl EventBatch {
    /// Each event paired with its seq.
    pub fn iter(&self) -> impl Iterator<Item = (Seq, &Event)> {
        (self.from.0..).map(Seq).zip(&self.events)
    }

    /// Seq of the last event, or `None` for an empty batch. A client's
    /// cursor advances to this after applying the batch.
    pub fn last(&self) -> Option<Seq> {
        let n = u64::try_from(self.events.len()).ok()?.checked_sub(1)?;
        Some(Seq(self.from.0 + n))
    }
}

/// A client intent sent to the server (`POST /v1/commands`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Command {
    pub id: CommandId,
    pub kind: CommandKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandKind {
    /// Start a session in `workdir`, which must lie under one of the
    /// server's configured roots (D14).
    CreateSession { workdir: String },
    SendMessage { session_id: SessionId, text: String },
    ResolveApproval { id: ApprovalId, decision: Decision },
    /// Stop the agent's current turn in this session.
    Interrupt { session_id: SessionId },
}

/// Response to `POST /v1/commands`. Resending a [`CommandId`] returns the
/// original result, so it is stable across retries.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CommandResult {
    /// The command's event was placed in the log at `seq`.
    Accepted { seq: Seq },
    /// The command was refused for good; resending gives the same answer.
    Rejected { reason: RejectReason },
}

/// Why the server refused a command.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// `CreateSession.workdir` is outside the configured roots (D14).
    WorkdirNotAllowed,
    /// `CreateSession.workdir` does not exist or is not a directory.
    WorkdirNotFound,
    UnknownSession,
    UnknownApproval,
    /// Another answer already won (D5).
    ApprovalAlreadyResolved,
    /// `Interrupt` arrived while the session had no running turn.
    NoActiveTurn,
}

/// Body of `POST /v1/pair`, the only endpoint reachable without a client
/// certificate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PairRequest {
    /// One-time code printed by `skywaved pair`.
    pub code: String,
    /// PEM-encoded certificate signing request for the device's keypair.
    pub csr_pem: String,
    /// Human-readable device name, shown by `skywaved devices list`.
    pub device_name: String,
}

/// Response to `POST /v1/pair`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PairResponse {
    pub device_id: DeviceId,
    /// PEM-encoded device certificate signed by the server's CA.
    pub device_cert_pem: String,
    /// PEM-encoded CA certificate; the device pins it for later connections.
    pub ca_cert_pem: String,
}

/// Response to `GET /v1/hello`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HelloResponse {
    pub protocol_version: u32,
    pub server_id: ServerId,
    /// The device the caller's certificate belongs to.
    pub device_id: DeviceId,
    /// Seq of the newest event; [`Seq::ZERO`] when the log is empty.
    pub head: Seq,
}

/// Query string of `GET /v1/events`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EventsQuery {
    /// Return events with seq greater than this.
    pub after: Seq,
    /// Seconds to block when no events are available; absent means no wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u32>,
    /// Maximum number of events to return; absent leaves it to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// HTTP paths of the v1 API.
pub mod path {
    use crate::BlobId;

    pub const PAIR: &str = "/v1/pair";
    pub const HELLO: &str = "/v1/hello";
    pub const COMMANDS: &str = "/v1/commands";
    pub const EVENTS: &str = "/v1/events";
    /// Route template for blob downloads; `{id}` is a [`BlobId`].
    pub const BLOB_ROUTE: &str = "/v1/blobs/{id}";

    /// Concrete path for downloading one blob.
    pub fn blob(id: BlobId) -> String {
        format!("/v1/blobs/{}", id.0)
    }
}

