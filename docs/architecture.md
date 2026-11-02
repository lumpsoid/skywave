# Skywave — Architecture

Remote client for coding agents (Claude Code first) built for bad links:
high jitter, low bandwidth, frequent drops. The agent runs on a server next to
the code; devices compose locally and sync small payloads.

The design is agent-agnostic. Agent-specific code lives in one adapter on the
server; nothing else names a concrete agent.

## Topology

```
 ┌──────────── server ────────────┐          ┌─────────────── device ───────────────┐
 │ agent process (e.g. claude -p) │          │ head: TUI     head: GUI              │
 │      ▲ agent adapter           │          │ (ratatui)     (Flutter: desktop,     │
 │ skywaved                       │          │    ▲           Android)              │
 │  - agent supervisor            │◄────────►│    │            ▲ rinf               │
 │  - shell runner (sh -c)        │ mTLS +   │    │          hub                    │
 │  - event log (SQLite)          │ own proto│    │            ▲                    │
 │  - device registry / CA        │          │    │            │                    │
 └────────────────────────────────┘          │ present: sections, wording, policies │
                                             │    ▲                                 │
                                             │ core: state, sync, outbox, replica   │
                                             └──────────────────────────────────────┘
```

One server = one account. Many devices = many views of the same data.

The shell runner sits beside the agent supervisor, not under it: each has
its own thread and database connection and follows the event log. The
supervisor drives the agent; the runner runs the one-shot shell commands
devices send (`RunShell`), one at a time per session, and logs their start,
progress and end (D35). Neither sees the other's events.

Each head is its own device. The head-side logic the TUI and the GUI share
(timeline sections, neutral wording, draft, read-mark and alert policies)
lives in `skywave-present`, above the core (D23). The Flutter head is the
Android app; the Kotlin head it replaced is gone (D28).

## Workspace

| Crate              | Role |
|--------------------|------|
| `skywave-proto`    | Versioned wire types shared by daemon and core |
| `skywaved`         | Server daemon: agent supervisor, event log, HTTPS API, admin CLI |
| `skywave-core`     | All client logic: state machine, sync, outbox, local replica, transport |
| `skywave-present`  | Head-side presentation every head shares: timeline sections, neutral wording, UTF-16 cursors; pure, on `skywave-core` only |
| `skywave-tui`      | First head |
| `skywave-testkit`  | Test-only: the link harness proxy (latency, jitter, bandwidth, drops); a dev-dependency only |
| `skywave-claude-compat` | Manual tool: runs the real Claude Code CLI through the adapter's `Protocol` and reports broken contracts and newly available features (docs/agents/claude-code.md) |
| `gui/`             | Flutter head; its rinf bridge crate `hub` lives at `gui/native/hub` |

Crates live under `crates/`. Every crate carries the `skywave-` prefix (the
daemon binary is `skywaved`); a crate named `core` would shadow Rust's
built-in `core`. Heads depend on `skywave-core` (and `skywave-present`),
never on `skywave-proto`.

One exception to the prefix: the Flutter head's bridge crate is named `hub`
and lives at `gui/native/hub`, because rinf's build tooling (cargokit) finds
the library by its package name and its Android, Linux and Apple builds link
`libhub`. It is still a workspace member.

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

- **Server log is the source of truth.** One append-only event log per server
  with a single monotonic `seq`; every event carries its `session_id` (D12).
- **Pull:** `GET /v1/events?after={seq}&wait={secs}` (long-poll). After a
  drop the client resumes from its last `seq` — no gaps, no duplicates. One
  cursor per device covers all sessions, including ones created elsewhere.
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
    Unpaired,                        // never paired: nothing to connect to
    Connecting,
    Online { latency_ms: u32 },
    CatchingUp { have: Seq, server: Seq },
    // wall-clock instants fixed at the failure; heads count down to retry_at
    Reconnecting { attempt: u32, retry_at: DateTime<Utc>, offline_since: DateTime<Utc> },
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
    Requested { size: u64 },                         // asked for, waiting for a slot
    Downloading { got: u64, size: u64, paused: bool }, // paused: the link is down
    Failed { size: u64, reason: String },            // given up; asking again retries
    Available { path: PathBuf },
}
```

| Situation | Presentation |
|-----------|--------------|
| `Unpaired` | No banner; the pairing screen. |
| `Reconnecting` with queued items | Banner: offline · N queued · retry in Xs. Input stays enabled. |
| `CatchingUp` | Render local replica immediately, subtle syncing indicator. |
| `AwaitingApproval` while offline | Show request; the answer is queued like any command. |
| Approval answered elsewhere | "Approved on <device>", controls removed. |
| New approval while `Online` (`attention.alerts` grows) | Bell or desktop notification naming the session, once; never at start, after a catch-up that resolved it, or for one answered here. |
| Approvals pending in other sessions (`attention.elsewhere`) | Banner: N approvals elsewhere; one key opens the next. |
| `Rejected(Revoked)` | Full-screen state with re-pair action; local data stays readable. |
| Blob `NotDownloaded` | Placeholder with size and a download action. |

## Flutter head

`gui/`. Decisions D23–D28. Flutter only renders and performs platform
effects; what the head decides is Rust's.

```
Flutter (gui/lib)            one holder per signal; widgets send GuiIntent
   ▲ Rust signals           │ GuiIntent (one ordered enum)
hub (gui/native/hub, rinf)   maps snapshots to GUI-shaped signals, intents to Actions
   ▲                         │
skywave-present              sections, wording, policies (shared with the TUI)
   ▲                         │ dispatcher thread (FIFO)
skywave-core                 all logic, sync, replica
```

- **Signals** are per feature: `LinkUpdate`, `ScreenUpdate`, `SearchUpdate`
  replace whole; the timeline and the shell runs (`ShellUpdate`, keyed by run
  id) are a `Snapshot` then keyed `Upserted`/`Removed` diffs; `PlatformEffect` is a command Dart carries out and forgets
  (notification, clipboard, opening a file, a snackbar). The contract is
  `gui/native/hub/src/signals/*.rs`; the Dart side is generated by `rinf gen`.
- **One hub task** owns the head's state and runs one `select!` loop over the
  intents, the core's snapshots, dispatch results and the policies' next
  wake-up. `dispatch` runs on one dedicated thread, in the order sent.
- **Boot:** Dart sends its application support directory and device name;
  the hub opens the core in `<dir>/skywave`. The GUI is its own device with
  its own data (D25). A hot restart reruns Rust's `main` and reopens the
  core behind `CORE_SLOT` (D27).
- **Lifecycle:** Dart reports `AppBackgrounded`/`AppForegrounded`; on
  Android and iOS the hub suspends the core, and resumes it then retries the
  link (D21's rule, without A15's background work). Desktop ignores both.
- **Platforms:** Linux and Android verified; macOS, iOS and Windows
  scaffolded only; no web (D26).
