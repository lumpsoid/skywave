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
    /// Identifies a session (one server-side event log).
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
    /// Identifies a blob of large content fetched separately from events.
    BlobId
);

/// Position in a session's append-only event log. Monotonic per session.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Seq(pub u64);

/// Reference to large content. Content is always referenced, never inlined.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BlobRef {
    pub id: BlobId,
    pub size: u64,
}

/// One entry in a session's event log.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Event {
    pub seq: Seq,
    pub session_id: SessionId,
    pub at: DateTime<Utc>,
    pub kind: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    SessionCreated,
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
    ApprovalRequested {
        id: ApprovalId,
        tool: String,
        summary: String,
    },
    ApprovalResolved {
        id: ApprovalId,
        decision: Decision,
        by: DeviceId,
    },
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

/// A client intent sent to the server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Command {
    pub id: CommandId,
    pub kind: CommandKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandKind {
    CreateSession,
    SendMessage { session_id: SessionId, text: String },
    ResolveApproval { id: ApprovalId, decision: Decision },
}
