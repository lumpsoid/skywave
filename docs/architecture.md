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
- **Transport** sits behind a `Transport` trait: HTTPS today, QUIC later.

## Protocol

`skywave-proto`: JSON over HTTPS, every endpoint under `/v1`, every request
but pairing authenticated by the device's client certificate. The types'
doc comments are the reference; `PROTOCOL_VERSION`'s lists what each
version changed. Client and server must match it exactly (checked at
`hello`); there is no compatibility layer.

| Endpoint | Purpose |
|----------|---------|
| `POST /v1/pair` | No client certificate. A one-time code, a CSR and a device name; returns the device certificate, the CA certificate and the `DeviceId`. |
| `GET /v1/hello` | Protocol version, server id, the caller's `DeviceId`, the log head, every paired device with its name, and whether the server runs shell commands. |
| `POST /v1/commands` | One `Command`; returns `Accepted { seq }` or `Rejected { reason }` (a closed `RejectReason`). Resending a `CommandId` returns the stored result. |
| `GET /v1/events?after=&wait=&limit=` | Events with `seq > after` as an `EventBatch { from, events }`; blocks up to `wait` when there are none. `wait` ≤ 60 s, `limit` ≤ 5000 (default 500). |
| `GET /v1/blobs/{id}` | Blob content; one `bytes=` range as 206, so downloads resume. |
| `GET /v1/dirs?dir=` | Workdir completion: subdirectories of `dir` under a root (canonicalised; `..` and symlinks cannot escape), else the roots. |
| `GET /v1/conversations` | The agent's own conversations, those had outside Skywave included (D37), as a `ConversationsResult`: those whose workdir is under a root, newest first, at most `CONVERSATIONS_MAX`, `truncated` when the cap or the scan's deadline (8 s; 503 at 10 s) cut it short. Each has the agent's id (what an adopting `CreateSession` names), workdir, title and first prompt cut to one line, last activity, message count, and the session already running it, if any. `Unavailable` when the agent keeps none Skywave can read. |
| `GET /v1/workdir/changes?session=` | A session's git changes (D36) as a `ChangesResult`: every file changed against `HEAD` (staged and unstaged together) plus untracked ones, sorted by path and capped, each with its own diff blob (or `Binary` / `TooLarge` / `Omitted`). `NotGit` outside a work tree; `Failed { reason }` when git is missing, refuses, or runs past the deadline. |
| `GET /v1/workdir/list?session=&path=` | One directory of a session's workdir (D36) as a `ListResult`: entries sorted by name, capped, with each symlink's target class. `path` is relative to the workdir; the answer names the canonical one. |
| `GET /v1/workdir/file?session=&path=` | One file of a session's workdir (D36) as a `FileResult`: size and a content blob, or `Binary` / `TooLarge` without one. Unchanged content keeps its `BlobId`. |

- **Seqs** are global and contiguous: the first event is 1, 0 is "before
  everything". Events carry no seq on the wire; the batch carries `from`.
- **Every accepted command logs exactly one event naming it**
  (`Event.command_id`), the last it appends, at the seq of its `Accepted`;
  a rejected one logs nothing. So the log acknowledges a command as its
  reply does (`MoveContext` also logs the new session's `SessionCreated`
  first; an adopting `CreateSession` logs its `SessionCreated` and the
  imported history first, and names itself on the `ImportFinished` after
  them, D37).
- **Every event carries its context** (`Event.context`), stamped by the
  server, never derived from position (D29). Session-level state (name,
  archived, mode, model, effort) folds per session regardless.
- **Agent-agnostic** (D3): tool events carry a bounded display (`ToolKind`,
  title, detail, a preview body) and clients branch on `kind`, never on the
  agent's tool name. `CallId`s are minted by the server; the agent's own ids
  and resume handles never leave `skywaved`. Size caps live in
  `skywave_proto::limits`; content past them is a blob.
- **Errors are plain HTTP statuses** with no typed body. Pairing: 400
  malformed, 401 wrong, used or expired code (indistinguishable), 429 too
  many failures (`Retry-After`). Elsewhere: 401 no client certificate, 403 a
  device unknown or revoked (checked on every request, so a revocation
  applies to an open connection's next request), 400 malformed, 404 unknown
  blob, 416 bad range, 500 server fault. `/v1/commands` answers 200 for
  `Accepted` and `Rejected` alike, and `/v1/dirs`, `/v1/conversations` and
  `/v1/workdir/*` 200 for every result (never 401/403 beyond
  authentication, which clients read as revoked). `/v1/dirs`,
  `/v1/conversations` and `/v1/workdir/*` answer 503 when they run out of
  time.

## Server

`skywaved`. Its config keys are in the README; its data directory holds
`skywave.db` (SQLite, WAL), `blobs/`, `ca/` and `server/` (ECDSA P-256 keys,
mode 0600), `server_id`, and `shell/` (running shell output until it becomes
a blob). `serve` creates the CA, certificate and id on first run and
reissues the certificate (same CA) when its names change. Admin commands
(`pair`, `devices list`, `devices revoke`) work on the database directly,
beside a running `serve`.

- **Commands** are deduplicated, validated and appended in one transaction;
  rejected ones are stored too, so their answer stays stable. Validation
  reads projections of the log (`sessions`, `contexts`, `approvals`,
  `shell_runs`) updated in the transaction that appends the event.
- **Importing an outside conversation** (D37): for an adopting
  `CreateSession` the API first reads the conversation's history from the
  agent's source (built next to the adapter) on a blocking thread, off its
  database connection, within a 20 s deadline (under the device's 30 s
  reply timeout; past it `Failed("timed out")`; a workdir that is still
  canonicalising then gets a 503 and nothing is handled, so the device
  resends), and bounds it with the
  event mapper's helpers into blobs written but not yet recorded. The
  command's transaction then records them and appends `SessionCreated`,
  one `Imported` per item and the command's `ImportFinished`; any path
  that does not commit them removes their files. A resend while the first
  request prepares waits for it; one after it is answered from the stored
  result. Imported items are records: never delivered to the agent, never
  open calls.
- **Agent supervisor** (D3, D4, D8): at most one agent process per session,
  started lazily for the context a message or setting request is stamped
  with: resumed from that context's opaque handle, else forked from the
  context it continues (D30) or the outside conversation it adopts (D32),
  else fresh. A clear or switch stops a running agent like `StopSession`.
  The session's grants pass to every start of its contexts, never to
  another session. An approval waits with no timeout but ends with the agent
  process. On daemon start every pending approval gets
  `ApprovalCancelled { reason: "daemon restarted" }` and every call left
  without a result is named in a `CallsUnanswered`.
- **Event mapper** (agent-agnostic): gives each tool call a fresh `CallId`,
  cuts every text to the limits on char boundaries, and makes a blob only for
  content that was cut, so a large payload costs a bounded event plus a blob
  fetched on demand (D11).
- **Shell runner** (D35): a sibling of the supervisor that follows the log on
  its own thread and connection. One FIFO per session, one run at a time,
  sessions in parallel. A run is `<program> -c <wrapper>` in the session's
  last cwd (a `cd` sticks), with no controlling terminal, stdin
  `/dev/null`, and `TERM=dumb`, `NO_COLOR=1`, `PAGER=cat`, `CI=1`. Its
  `when` is matched against the session's last end (`Exited { code: 0 }`
  is success). A run ends when the shell exits; its process group is then
  killed, and on Linux a subreaper host reaps orphans. Output streams to a
  file (capped by `output_max`), reported as a tail at 2, 5, 10, 30 s and
  then every 60 s, and becomes a blob unless the preview holds all of it.
  On start, runs left running are finished `Lost` and queued ones run; if
  the runner fails, `serve` stops rather than accept runs nothing would
  start.
- **Workdir reads** (D36): list and file resolve the session's stored
  workdir, which must still canonicalise to itself under the roots, then the
  requested path, which must be relative, must not climb out with `..`, and
  must canonicalise under the canonical workdir. A file must be a regular
  file before it is opened (a FIFO would block). Reads run on their own
  connection, one snapshot at a time behind a mutex, so they never hold the
  API's connection and a refresh storm costs one snapshot at a time. Each
  request's deadline (15 s) starts with its handler: a request that gets the
  lock past it does nothing (a 503), and work in progress stops at it (a
  503 for list and file; changes answers `Failed { "timed out" }`, its git
  children killed). The outer timeout is 20 s. Content blobs are
  deduplicated by `(sha256, size)` in `workdir_blobs`, so an unchanged file
  keeps its blob across refreshes; event blobs are never reused for them.
  Nothing deletes blobs yet (no GC).
  Changes run the `git` CLI in the workdir (no git library): `rev-parse`
  decides `NotGit` from its output, then one `git diff <HEAD or the empty
  tree> --relative -M -z --raw --numstat` gives the entries (so a workdir
  below a repo's top shows its own subtree, in its own terms),
  `ls-files --unmerged` marks `Conflicted`, and `ls-files --others
  --exclude-standard` the untracked ones. One streamed `git diff` gives the
  patches, split at `diff --git ` lines and paired with entries by their
  exact expected header, never by position; an entry with a quoted path, a
  header shared with another entry, or no section gets its own `git diff --
  :(literal)<path>`, of which it keeps only sections with its own header.
  An unmerged path whose worktree equals `HEAD` is still a `Conflicted`
  entry. Untracked diffs are synthesised from `lstat` (a
  symlink's diff is its link text). At most 500 entries (`total` counts
  all), 1 MiB per diff and 8 MiB per snapshot are kept. Every child runs
  with fixed flags (`core.fsmonitor=false`, no external diff, no textconv,
  no pager, `--submodule=short`), the repository's own filter drivers emptied, inherited `GIT_*`
  variables removed, stdin closed, and leads a process group killed at the
  deadline.

## Pairing

D9. The server owns a CA; every device holds a certificate it signed.

1. `skywaved pair` stores the SHA-256 of a one-time code and prints the
   code, the server address and the CA fingerprint (SHA-256 of the CA
   certificate's DER, colon-separated hex), also as a QR code.
2. The device makes a keypair and CSR and calls `POST /v1/pair`, accepting
   the server only if the last certificate of the chain it presents has the
   entered fingerprint and the leaf is valid for the typed address under
   that CA. A mismatch fails the handshake, so the code is never sent.
3. The server checks the code (single use, `--ttl` default 10 min, at most
   24 h), signs the CSR, records the device and returns its certificate and
   the CA.
4. Every later connection is mTLS: chain **and** registry (not revoked).

- **Codes:** 10 Crockford base32 symbols (50 bits), shown `XXXXX-XXXXX`;
  input ignores case, `-` and spaces, and reads `O` as `0`, `I`/`L` as `1`.
- **Rate limit:** at most 5 failed attempts per sliding minute, server-wide,
  in memory; a full window answers 429 even to the right code. The pairing
  endpoint is the only one without a client certificate and exposes
  nothing else.
- **Pairing URI** (the QR code):
  `skywave://pair?addr=<host:port>&code=<code>&fp=<fingerprint>`, `addr`
  percent-encoded, `code` without `-`, `fp` as 64 lowercase hex digits.
  The core parses it (`parse_pair_uri`) so heads don't. An address without
  a port means 7443.
- The device key and certificate live in the core's data directory behind
  the `Identity` trait, so a platform keystore can replace it.

## Client core

`skywave-core`. `Core::open(data_dir)`, `dispatch(Action)`,
`subscribe(ViewState)`, on its own tokio runtime. The state machine
(`Model`) does no I/O: the network tasks reach it through `core.seam()`,
receiving `Request`s and reporting results, and every report is persisted,
then pushed as a new `ViewState`.

- **Replica** (SQLite, `core.db`): the replicated `events`, projections
  folded in the transaction that stores them (`sessions`, and `turns`, each
  `TurnUsage` by seq for the usage screen), the `outbox`, the `cursor`,
  `blobs`, `settings`, and what is this device's alone (D5: drafts per
  session and composer mode, composer modes, snippets, read marks, local
  deletes, alerted approvals). A schema migration backfills a new
  projection from the stored events. `workdir_snapshots` keeps the latest
  stored answer per workdir read (session, kind, requested path), so a
  workdir already browsed reads offline; `workdir_blob_refs` names the
  blobs each answer holds. A refresh replaces the answer, and a blob row
  goes once no answer of any session names it (the server deduplicates
  workdir content across sessions). An answer for a session not in the
  replica (unknown, deleted here, or from before a re-pair) is dropped.
- **Sync engine** (D7): the pull loop long-polls from the cursor and applies
  each batch and the new cursor in one transaction. The outbox sends oldest
  first, one command in flight, so the server sees commands in the order
  they were made. A pulled event naming an unanswered command settles it as
  `Accepted` at its seq; a reply that stays unreadable is settled from the
  log, or failed as undeliverable once the replica has passed the log head
  read after it, so it is never resent into a duplicate. Reconnects back off
  exponentially with jitter to 30 s, and the wait ends by whichever of the
  monotonic and wall clocks says first, so a device that slept retries when
  the head says. A revoked device or protocol mismatch is `Rejected`, and
  retries stop until re-pairing. `suspend()` / `resume()` pause the network
  for heads that background.
- **Blobs** (D6): `OnDemand` by default, or `Eager`. Downloaded beside the
  link, two at a time, into `blobs/<id>.part`, resumed with `Range`,
  verified by size and renamed when complete; `Link` ignores them (synced
  metadata counts as synced). Workdir blobs (diffs and file contents a
  stored workdir answer names) are never fetched eagerly, whatever the
  policy: only asking for one downloads it. Their ids recur (the server
  names content by hash), so a refresh can purge a blob a download holds
  and the next one register it again: each try reads the blob with a lease
  (its purge count, beside the replica under the same lock), and a report
  under an older lease fails as for an unknown blob. A request for a blob a
  download holds is kept and queued once it lets go. The core hands
  requests to the engine under the state lock, in the order of the changes
  that made them, so a purge never overtakes a later request for the same
  id.
- **Imported history** (D37): an adopted session's `Imported` events are
  records. They are not activity (the session's `last_seq` and
  `last_activity` stay at its creation, so a read mark stops before them),
  never notable, and the "new" divider never lands at or inside them (the
  creation and its history are one unit, read or not). The first
  imported prompt is the session's `first_prompt` (its title) and its
  first context's, and imported prompts count in that context's messages.
  Their blobs are blobs like any other, so `Eager` fetches them. The
  timeline shows each as `Entry::Imported` with the agent's own time (the
  log's when it gave none), a result folded into its call (its `took` zero
  unless the agent timed both: the log is another clock), a call never
  answered as `NoResult` (never running), then `Entry::ImportFinished`;
  narrowing to a context keeps the imported entries and drops that
  divider. `SessionRow::importing` (the `sessions.importing` column) holds
  from an adopting `SessionCreated` until its `ImportFinished` is stored:
  an import longer than one pull shows the session beside its still
  pending creation (`PendingSession::conversation`) until then.
- **Conversation picker** (D37): `ListConversations` on the active session
  list asks `GET /v1/conversations` on the side transport within
  `CONVERSATIONS_TIMEOUT` (12 s, above the server's 10 s) and shows
  `SessionListView::conversations` as `Loading`, then `Ready { truncated }`
  or `Unavailable(why)`: `Offline`, `AgentCannotList` or `Failed(reason)`,
  typed so heads word it without matching text. Each item carries
  `continued_in`, the replica's sessions whose `SessionCreated` adopted it (`sessions.adopted`, the
  conversation an `Agent` origin names; adopting twice is allowed), and the
  server's `session` only while that session is in the replica (not
  deleted here, already pulled). Only the latest request's answer is kept;
  offline (or with the server unreachable) it is `Unavailable(Offline)`,
  at once when the link is down, and nothing is asked later. Continuing is
  `CreateSession { workdir, conversation }`, which ends the picker, as do `EndConversations` and leaving the list.
  Nothing is stored: the listing goes stale at once.
- **Workdir snapshots** (`Screen::Workdir`): `OpenWorkdir` shows a session's
  changes; `ShowWorkdir` switches between the changes, one file's diff (a
  path and whether it is the untracked entry), a directory and a file;
  `RefreshWorkdir` reads the pane again (a diff: the changes). The fetch
  rule is plain: showing a pane whose read has no stored answer asks once,
  a stored one shows with its time until a refresh, and showing a diff or
  file whose content is not downloaded asks for that blob. Offline nothing
  is asked or queued: the read fails at once with "not connected" and the
  stored answer stays. The engine runs each read on its own task within
  `WORKDIR_TIMEOUT`, on the side transport it keeps for listings, failing
  at once when unpaired, suspended or rejected. The model keeps the latest
  answer per read (as stored), the request id each read waits on, and the
  last failure, in memory. `Seam::workdir_answered` lets the model decide
  first: an answer to an older request than the read's latest touches
  neither the replica nor the model. A transport error or
  `ChangesResult::Failed` stays in memory and never replaces a stored
  answer; any other answer is stored, and the model learns the blobs it
  registered and purged in the same update. Back walks file → its
  directory, diff → changes, directory → its parent, and the changes or
  the workdir root → the session; a session deleted here falls back to the
  archived list.
- **Usage screen**: every `TurnUsage` in the replica as a `TurnRecord` with
  the sessions they ran in; heads total them with `skywave_present::usage`
  by local day, session, workdir or model. A turn with no cost is unpriced,
  never $0. Archived sessions count; a session deleted from this device took
  its turns with it, and the screen says so.

## Testing

- Tests are deterministic: a scripted fake agent (`skywaved/src/agent/fake.rs`,
  `tests/support/fake-claude.sh`) and recorded CLI fixtures. Only fixture
  recording and `skywave-claude-compat` run the real CLI.
- **Link harness** (`skywave-testkit`, D11): an in-process TCP proxy that
  injects latency, jitter, bandwidth caps and drops between real cores and a
  real `skywaved`. It asserts no lost commands, no duplicated events, cursor
  convergence, and two devices converging on the same state.
- `tracing` logs on both sides; the daemon logs to stderr or `log_file`.

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
- **Workdir text** travels in `ScreenUpdate`, and only for the diff or file
  pane shown: the hub reads the head of its downloaded blob once per blob
  (1 MiB or 20,000 lines, lower than the TUI's 4 MiB because every line
  crosses to Dart), with a diff's line kinds from `diff_lines`. "Open"
  (`OpenFull`) opens the whole file.
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
