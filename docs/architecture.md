# Skywave — Architecture

Remote client for coding agents (Claude Code first) built for bad links:
high jitter, low bandwidth, frequent drops. The agent runs on a server next to
the code; devices compose locally and sync small payloads.

The design is agent-agnostic. Agent-specific code lives in one adapter on the
server; nothing else names a concrete agent.

## Topology

```
 ┌──────────── server ────────────┐          ┌─────────── device ────────────┐
 │ agent process (e.g. claude -p) │          │ head: TUI (ratatui)           │
 │      ▲ agent adapter           │          │ head: Android (Kotlin/Compose)│
 │ skywaved                       │◄────────►│        ▲ UniFFI               │
 │  - agent supervisor            │ mTLS +   │ core: state, sync, outbox,    │
 │  - event log (SQLite)          │ own proto│       full local replica      │
 │  - device registry / CA        │          │                               │
 └────────────────────────────────┘          └───────────────────────────────┘
```

One server = one account. Many devices = many views of the same data.

## Workspace

| Crate              | Role |
|--------------------|------|
| `skywave-proto`    | Versioned wire types shared by daemon and core |
| `skywaved`         | Server daemon: agent supervisor, event log, HTTPS API, admin CLI |
| `skywave-core`     | All client logic: state machine, sync, outbox, local replica, transport |
| `skywave-core-ffi` | UniFFI wrapper over `skywave-core`; `skywave-core` has no FFI dependency |
| `skywave-tui`      | First head |
| `android/`         | Gradle project, Kotlin head, builds `skywave-core-ffi` via `cargo-ndk` |

Crates live under `crates/`. Every crate carries the `skywave-` prefix (the
daemon binary is `skywaved`); a crate named `core` would shadow Rust's
built-in `core`. Heads depend on `skywave-core` only, never on
`skywave-proto`.

## Core / head contract

```rust
core.dispatch(Action)          // intents: SendMessage, Approve(id), OpenSession, ...
core.subscribe(|ViewState| ..) // immutable per-screen snapshots
```

- The core owns all logic and I/O (network, SQLite, internal tokio runtime).
- Heads render `ViewState` and own platform concerns only (notifications,
  key storage, clipboard).
- Startup always hydrates from the local replica, then reconnects. A cold
  start (incl. Android killing a backgrounded process) is the normal path.
- Every user action is persisted locally first, then synced. The UI never
  waits on the network.

## Sync model

- **Server log is the source of truth.** Each session is an append-only event
  log with a monotonic `seq`.
- **Pull:** `GET /sessions/{id}/events?after={seq}` (long-poll / SSE). After a
  drop the client resumes from its last `seq` — no gaps, no duplicates.
- **Push:** commands carry a client-generated id (UUIDv7); the server dedups,
  so resend after reconnect is always safe.
- **Ordering:** concurrent sends from several devices are ordered by server
  acceptance.
- **Payloads:** event metadata is small; large content (tool outputs, file
  contents, logs) is a blob referenced by id and fetched separately.
- **Transport** sits behind a `Transport` trait: HTTPS for MVP, QUIC later.

## Render states (owned by core, drawn by heads)

```rust
enum Link {
    Connecting,
    Online { latency_ms: u32 },
    CatchingUp { have: Seq, server: Seq },
    Reconnecting { attempt: u32, next_in: Duration, offline_since: Instant },
    Rejected(RejectReason),          // revoked cert, protocol mismatch
}

enum Outgoing {
    Queued,                          // persisted locally
    Sending,
    Accepted(Seq),                   // server acked, placed in the log
    Failed(FailReason),              // permanent
}

enum SessionState {
    Idle,
    AgentWorking,
    AwaitingApproval(ApprovalId),
    Errored(ErrorInfo),
}

enum BlobState {
    NotDownloaded { size: u64 },
    Downloading { got: u64, size: u64 },
    Available,
}
```

| Situation | Presentation |
|-----------|--------------|
| `Reconnecting` with queued items | Banner: offline · N queued · retry in Xs. Input stays enabled. |
| `CatchingUp` | Render local replica immediately, subtle syncing indicator. |
| `AwaitingApproval` while offline | Show request; the answer is queued like any command. |
| Approval answered elsewhere | "Approved on <device>", controls removed. |
| `Rejected(Revoked)` | Full-screen state with re-pair action; local data stays readable. |
| Blob `NotDownloaded` | Placeholder with size and a download action. |
