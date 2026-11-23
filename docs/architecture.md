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
| `skywave-present`  | Head-side presentation every head shares: timeline sections, neutral wording, UTF-16 cursors, the display filter for agent content (control characters dropped, bidi controls escaped visibly); pure, on `skywave-core` only |
| `skywave-safety`   | The safety check (D59): proves a shell command read-only and inside a workdir, on a parsed AST. No side effects (reads only file metadata); used by `skywaved` |
| `skywave-version`  | The build's version with its commit, `0.1.0 (2391c1d)`, from a `build.rs`; a leaf, so a new commit rebuilds only it and what prints it (`skywaved`, `skywave-tui`, `hub`) |
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
  contents, logs, long agent messages) is a blob referenced by id and
  fetched separately. The
  reference (`BlobRef`) carries the content's size and SHA-256, which the
  server computes as it stores the blob; a device accepts no bytes that do
  not hash to it.
- **Transport** sits behind a `Transport` trait: HTTPS today, QUIC later.
- **Compression** (D45, `skywave_proto::encoding`): an event batch or a
  blob body of at least 1 KiB (`COMPRESS_MIN_BYTES`) travels as one zstd
  frame (level 3) to a request that sends `Accept-Encoding: zstd`, which
  the device's every request does; a shorter body, or a request that does
  not ask (curl, the server's own tests), gets the bytes as they are. The
  transport decodes it, so everything above it (sizes, hashes, download
  progress, `.part` offsets) counts decoded bytes. A blob's `Range` and
  `Content-Range` count decoded bytes too, unlike plain HTTP: a resumed
  download asks for the rest from its decoded offset and the server
  compresses that range as a fresh frame, so nothing compressed is stored.
  The decoder emits only bytes of the content, in order (a compressed
  block once all of it arrived, a raw one possibly as it streams in), so
  what a cut download got is a correct start of the content and resumes
  from there. A blob frame ends a block every 8 KiB of content
  (`FLUSH_EVERY`), so a cut download loses at most 8 KiB of content from
  what arrived. A compressed blob response is streamed with no
  `Content-Length` and names its decoded length in
  `skywave-decoded-length`; a compressed batch is sent whole, with the
  frame's `Content-Length`.
- **Slowest link:** an event batch or blob body is timed per chunk, not as
  a whole, so a slow link that keeps delivering bytes finishes. A chunk is
  one TLS record (up to 16 KB) and must arrive within `request_timeout`
  (30 s), so below about 550 B/s the link counts as dead. Accepted limit:
  revisit only if a real network that slow turns up. A command's upload
  is timed the same way, by progress: it fails once the socket has taken
  none of it for `request_timeout` (30 s), which, after its last byte,
  is also how long the response head may take. So a message up to
  `MESSAGE_MAX_BYTES` gets through a slow uplink that keeps moving. On
  Linux and Android the client socket sets `TCP_NOTSENT_LOWAT` to 16 KiB,
  so it takes a write only while less than that is unsent: a taken write
  means bytes left the host, and after the last one only about 16 KiB
  plus what is in flight remains for the head wait (some 8 s at 2 KB/s).
  Elsewhere (desktop macOS and Windows) the send buffer, which can
  autotune to megabytes, counts as sent, so on an uplink too slow to
  drain it within 30 s a large command times out there and is resent.
- **Dead connections:** the HTTPS transport keeps up to `max_idle` (4)
  connections for reuse. A NAT or network that drops a connection
  silently leaves it looking ready. So a request that times out or whose
  connection breaks drops every idle connection with its own: they share
  its path, and the reconnect's `hello` then opens a fresh one instead of
  waiting out a timeout on each stale one in turn. On Linux and Android
  the client socket also sets `TCP_USER_TIMEOUT` to 20 s: once sent bytes
  go unacknowledged that long, the kernel closes the connection and its
  request fails at once. A server holding a long-poll has acknowledged
  the request, so the long-poll is not cut; a long-poll whose path dies
  after that is still found only by its own timeout (wait + 30 s). This
  also bounds an upload that stops being acknowledged at 20 s, under its
  30 s stall bound. Elsewhere only the request timeouts apply.

## Protocol

`skywave-proto`: JSON over HTTPS, every endpoint under `/v1`, every request
but pairing authenticated by the device's client certificate. The types'
doc comments are the reference; `PROTOCOL_VERSION`'s lists what each
version changed. Client and server must match it exactly (checked at
`hello`); there is no compatibility layer on the wire. Stored data does
carry over a bump: both the server's database and a device's replica
migrate from any version since `MIGRATED_SINCE` (D43). The device reads the hello's
`protocol_version` alone before decoding the rest, so a server of another
version, whatever its hello's other fields, is a protocol mismatch rather
than an unreadable reply; that one field must keep its name and type in
every version.

| Endpoint | Purpose |
|----------|---------|
| `POST /v1/pair` | No client certificate. A one-time code, a CSR and a device name; returns the device certificate, the CA certificate and the `DeviceId`. |
| `GET /v1/hello` | Protocol version, server id, the caller's `DeviceId`, the committed log head, the log's identity, this run of the server and, for `?since=<RunId>`, what became of that run (`LogRun`, see "A log that went backwards"), every paired device with its name, whether the server runs shell commands, whether it drives screens (`[screen] enabled`, D65), whether it allows bypass mode (`agent.allow_bypass`, D38), when the agent's login expires (`login_expires`), whether a device may log the agent in (`agent_login_offered`) and whether the agent is logged out (`agent_logged_out`, true only for a definite "not logged in"); see "The agent's login". Since v29. |
| `POST /v1/commands` | One `Command`; returns `Accepted { seq }` or `Rejected { reason }` (a closed `RejectReason`). Resending a `CommandId` returns the stored result. |
| `GET /v1/events?after=&wait=&limit=` | Events with `seq > after` as an `EventBatch { from, events }`; blocks up to `wait` when there are none. `wait` ≤ 60 s, `limit` ≤ 5000 (default 500). A batch also stops before the event that would take it past 4 MiB of stored events (`BATCH_MAX_BYTES`), but always holds the first, so an event larger than that comes alone; a short batch is not the end of the log, and the client just pulls again from its last seq. An `after` above `i64::MAX` (the highest seq the log can hold) is a 400. Compressed when asked (D45, Sync model). |
| `GET /v1/blobs/{id}` | Blob content; one `bytes=` range (of decoded bytes) as 206, so downloads resume. Compressed when asked (D45, Sync model). |
| `HEAD /v1/attachments/{sha256}` | 204 if a file with that sha256 is staged, else 404; lets a device skip a re-upload across reconnects. A 404 carries `skywave-upload-offset: N` when the server holds N bytes of an interrupted upload (since v37), so the device resumes at N. Since v30. |
| `PUT /v1/attachments/{sha256}` | Stages a file of any type (raw bytes, at most the server's `[files] upload_max`, default 64 MiB, counted over the whole file; streamed to the upload's partial, `attachments/.part-<sha256 hex>`, never held in memory) for a later `SendMessage` to name, answering an `UploadResult` with 200: `Stored { size }`, `TooLarge`, `HashMismatch`, `DiskLow` or `OffsetMismatch { have }`. An optional `skywave-upload-offset: N` header makes the body the file from byte N on: N must be the partial's length (the body appends), or 0 (the partial restarts); a PUT of a file already staged answers `Stored` at any offset with nothing written; any other N is `OffsetMismatch { have }` with nothing written. A body that ends early (a lost connection, a stall) keeps its partial for the next `PUT`. One `PUT` per sha256 writes at a time; the others wait for its lock. `DiskLow` is checked before the body and again before each write. A `content-length` that takes the file (offset included) past `upload_max` is `TooLarge` before the body is read, and the partial goes. The file is `attachments/<sha256 hex>`, with no table, kept only while it hashes to its name; staging is a cache, capped at 256 MiB (the oldest evicted on each upload) and swept of files older than 7 days at startup. Partials survive a restart, are never evicted and are swept with the same age limit. A lost upload surfaces as `UnknownAttachment`. Since v30; the offset header since v37. |
| `GET /v1/dirs?dir=` | Workdir completion: subdirectories of `dir` under a root (canonicalised; `..` and symlinks cannot escape), else the roots. |
| `GET /v1/conversations` | The agent's own conversations, those had outside Skywave included (D37), as a `ConversationsResult`: those whose workdir is under a root, newest first, at most `CONVERSATIONS_MAX`, `truncated` when the cap or the scan's deadline (8 s; 503 at 10 s) cut it short. Each has the agent's id (what an adopting `CreateSession` names), workdir, title and first prompt cut to one line, last activity, message count, and the session already running it, if any. `Unavailable` when the agent keeps none Skywave can read. |
| `GET /v1/storage` | What the server's data takes on its disk, as a `StorageReport` read when asked: the database with its WAL, event and workdir blobs, running shell output, the log file, the free space and the `min_free` floor; every session (its events' stored JSON, its events' blobs), the largest event blobs and the shell runs taking the most (`STORAGE_TOP_MAX` each), largest first. |
| `POST /v1/storage/reclaim` | Compacts the server's database (`VACUUM`, then the WAL truncated into it, no body), answering the fresh `StorageReport`, whose `reclaimable` is then 0. Explicit only. Refused with 507 when the free space is below the database's size plus `min_free`, nothing run; 503 when the connection is still held at the report's 8 s deadline, or past the 10-minute reply timeout. Since v41. |
| `GET /v1/usage-limits` | The usage limits of the account the agent runs as, as a `UsageLimitsResult` read from the agent when asked: `Read { at, limits }` (each limit's window, `Session` or `Weekly`, its scope such as one model, percent used, severity and reset time, a figure the agent did not report left out), `Unavailable` for an agent that reports none, or `Failed { reason }`. Nothing is stored on the server. |
| `POST /v1/agent-login/start` | No body. Starts logging the agent in from a device (D49), as a `LoginStartResult`: `Open { flow, url }` (the user signs in at `url`, at most `LOGIN_URL_MAX_BYTES`; a longer one is `Failed`, never cut), the same flow and URL to a start while one is starting or open, `Busy` while a finish runs, `Unavailable` when `agent_login_offered` is false, or `Failed { reason }`. The agent gets 15 s; 503 at 20 s. See "The agent's login". Since v29. |
| `POST /v1/agent-login/finish` | A `LoginFinish { flow, code }` (the code the sign-in page showed; over `LOGIN_CODE_MAX_BYTES` a 400). Answers `LoggedIn { expires }`, `NoFlow` (no such flow, or given up on) or `Failed { reason }`. The first finish of a flow is the only one the agent sees: every later one gets its outcome, whatever code it carries. 503 after 20 s of waiting for the agent, while the finish goes on; a retry collects it. Since v29. |
| `GET /v1/workdir/changes?session=` | A session's git changes (D36) as a `ChangesResult`: every file changed against `HEAD` (staged and unstaged together) plus untracked ones, sorted by path and capped, each with its own diff blob (or `Binary` / `TooLarge` / `Omitted`). `NotGit` outside a work tree; `Failed { reason }` when git is missing, refuses, or runs past the deadline. |
| `GET /v1/workdir/log?session=` | A session's commit log (the repo's `HEAD` history, not narrowed to the workdir) as a `LogResult`: `Log(CommitLog)` with at most `limits::WORKDIR_LOG_MAX` commits, newest first, and `more` past them; `branch` is `None` when detached and `commits` empty on an unborn branch. `NotGit` outside a work tree, `Failed { reason }` as for changes. Since v31. |
| `GET /v1/worktrees` | The git worktrees of every repository a session's worktree was made from (D52, read from `SessionCreated`), as a `WorktreeList` read when asked: per repo a `RepoListing`, `Worktrees { worktrees, more }` with the main working tree first and at most `limits::WORKTREES_MAX` entries (`more` past them), each with its branch (`None` when detached), head, `main`, `missing`, `dirty` (`None` when its status was not read: missing, or outside the roots) and `merged`; `Gone` when the repo's directory is gone or is not its own top level, `Failed { reason }` when git fails for that repo alone. Always 200. Since v34. |
| `GET /v1/workdir/list?session=&path=` | One directory of a session's workdir (D36) as a `ListResult`: entries sorted by name, capped, with each symlink's target class. `path` is relative to the workdir; the answer names the canonical one. |
| `GET /v1/workdir/file?session=&path=` | One file of a session's workdir (D36) as a `FileResult`: size and a content blob, or `Binary` / `TooLarge` without one. Unchanged content keeps its `BlobId`. |
| `GET /v1/workdir/download?session=&path=` | One file of a session's workdir to save on the device (D54), text or binary, as a `FileResult` whose content is a blob up to the server's download cap (`[files] download_max`, 64 MiB by default), else `TooLarge`. Recorded apart from `file`, so neither frees the other's blob. Since v36. |
| `POST /v1/workdir/write` | Writes a staged upload (`PUT /v1/attachments/{sha256}`) to `path` in a session's workdir, a `WriteFileRequest { session, path, content, replace }` answered with a `WriteFileResult`, always 200. The parent directory must exist. Refused as `NotAllowed` before the file system is touched: an absolute path, one climbing out of the workdir, any `.git` component. A symlink at the target is `NotAllowed` too, found when the file is opened and never followed. `Same` when the file already holds the content, and nothing is written. `Exists { sha256, size }` when another file is there and `replace` does not name its sha256; naming it replaces it, and a new file is never created over one made meanwhile. A replaced file keeps its mode, a new one is 0644. The staged upload is kept. A write that lands is logged as `FileWritten` by the calling device and answered `Written` with that event's seq. `DiskLow` is checked first; a write past 120 s is a 503. Since v36. |

- **Seqs** are global and contiguous: the first event is 1, 0 is "before
  everything". Events carry no seq on the wire; the batch carries `from`.
- **Every accepted command logs exactly one event naming it**
  (`Event.command_id`), the last it appends, at the seq of its `Accepted`;
  a rejected one logs nothing. So the log acknowledges a command as its
  reply does (`MoveContext` also logs the new session's `SessionCreated`
  first; an adopting `CreateSession` logs its `SessionCreated` and the
  imported history first, and names itself on the `ImportFinished` after
  them, D37). A `DeleteBlob`'s `BlobDeleted` and a `CropBlob`'s
  `BlobCropped` are stamped with the session and context of the event
  that referenced the blob, as an `ApprovalResolved` takes its approval's
  (D42).
- **Every event carries its context** (`Event.context`), stamped by the
  server, never derived from position (D29). Session-level state (name,
  archived, mode, model, effort) folds per session regardless.
- **Agent-agnostic** (D3): tool events carry a bounded display (`ToolKind`,
  title, detail, a preview body) and clients branch on `kind`, never on the
  agent's tool name. `CallId`s are minted by the server; the agent's own ids
  and resume handles never leave `skywaved`. Size caps live in
  `skywave_proto::limits`; content past them is a blob.
- **Command text is capped, never cut.** A `SendMessage` over
  `MESSAGE_MAX_BYTES` (256 KiB) is `MessageTooLong`, a `RunShell` over
  `SHELL_COMMAND_MAX_BYTES` `BadShellCommand`, so a client minting fresh
  command ids cannot write more than that to the permanent log per request.
  The heads refuse an over-cap message before dispatching, keeping it in the
  composer; the server's refusal is the backstop.
- **Every request names the run it is meant for** once the device knows
  one: a `skywave-run: <RunId>` header (`RUN_HEADER`), on every request
  but the `hello` that asks about a run with `?since=` (that is how the
  device learns the current run). The server checks it right after
  authentication and answers 412 when it names another run than its own,
  before any handler runs: a command is not handled, a batch not served.
  Without the header nothing is checked; a malformed one is a 400. A
  header rather than a field because it applies to every endpoint alike,
  and a command's body stays the command the server dedups.
- **Errors are plain HTTP statuses** with no typed body. Pairing: 400
  malformed, 401 wrong, used or expired code (indistinguishable), 429 too
  many failures (`Retry-After`). Elsewhere: 401 no client certificate, 403 a
  device unknown or revoked (checked on every request, so a revocation
  applies to an open connection's next request), 400 malformed (including an
  attachment path that is not a sha256), 404 unknown blob or attachment not
  staged, 412 another run expected, 416 bad range, 500 server fault. `/v1/commands` answers 200 for
  `Accepted` and `Rejected` alike, and `/v1/dirs`, `/v1/conversations`,
  `/v1/usage-limits`, `/v1/agent-login/*`, `/v1/attachments/{sha256}` (`PUT`) and `/v1/workdir/*` 200 for every result (never 401/403 beyond
  authentication, which clients read as revoked). `/v1/dirs`,
  `/v1/conversations`, `/v1/storage`, `/v1/storage/reclaim` and `/v1/workdir/*` answer 503 when
  they run out of time, or when 8 of their file-system reads are already
  running (a wait of at most 0.5 s first). Each read holds its permit until its blocking
  thread returns, even after its request gave up, so reads stuck on a hung
  mount stay capped at 8 and never exhaust the blocking threads that
  authentication, commands and events need.

## Server

`skywaved`. Its config keys are in the README; its data directory holds
`skywave.db` (SQLite, WAL), `blobs/`, `ca/` and `server/` (ECDSA P-256 keys,
mode 0600), `server_id`, `shell/` (running shell output until it becomes
a blob), `serve.lock` (held locked by the one running `serve`) and
`control.sock` (where that `serve` takes `stop`, `restart` and `status`, D41).
`serve` creates the CA, certificate and id on first run and reissues the certificate (same CA) when its names change, warning when a
name was dropped (devices that connect by it fail with a name mismatch). A missing
`ca/` is recreated by `serve` or `pair`; when unrevoked devices are registered,
the command warns on stderr (and in the log) that every one must re-pair. Both
warnings go to the command's stderr as well as the log. Admin commands
(`pair`, `devices list`, `devices revoke`, `safety-feedback`) work on the
database directly, beside a running `serve`.

`stop` and `restart` (D41) instead ask the running `serve` over
`control.sock` in its data dir (JSON lines, `crates/skywaved/src/control.rs`)
and never open the database: a binary deployed since would migrate it under
the running `serve`. With work running (a session with `turn_active` or a
message owed to its agent, a shell run not finished) a stop now is refused
with that list, and is taken when asked again confirming it (or forced),
unless more started since. `--after-work` sets the log's *stopping* flag:
the commands that start new work are refused `ServerStopping` (checked
inside the command's write transaction, like `DiskLow`, and first). `serve`
then stops once no shell run is unfinished and the supervisor's published
activity has no turn running or on its way (owed, held for a start or
stop, open) and has read the log up to where the refusals began, which it
records under the write lock. It checks again at every append and every
supervisor step, never on a timer. `--cancel` clears the flag. Any decided
stop is SIGTERM's graceful stop; a restart then executes `serve`'s binary
path, read at start, again with the same arguments and pid. The
connection stays open until the old `serve` is gone. `status` asks the same
socket how `serve` is (protocol version, a waiting stop, a low disk, the
running work) and fails when no `serve` answers: the health check, local
only, so nothing about the daemon is served without a client certificate.

`serve` runs `PRAGMA quick_check` once at startup, after taking the serve
lock and before anything reads the log, and refuses a damaged file, telling
the operator to restore a backup. Backups follow from the layout: the
database is copied with SQLite's online backup (`sqlite3 .backup`), never
as a file under a running WAL; then `blobs/`, `ca/`, `server/` and
`server_id`. Blob files are written before their rows, so blobs copied
after the database cover every event blob it names. Only a workdir blob
collected between the two copies can be missing; fetching it fails until a
refresh of its read replaces it. Copying with `serve` stopped is exact
(README, "Running it as a service").

Opening the database migrates its schema (`PRAGMA user_version`); no
schema migration rewrites the wire JSON its columns hold. So the database
records the `PROTOCOL_VERSION` its JSON is in (`protocol_stamp`), and
opening it then runs the protocol steps (`PROTOCOL_STEPS` in `db.rs`, one
per version since `MIGRATED_SINCE`) from that version on, in the same
transaction as the schema migrations and after them, and restamps it
(D43). `serve` and the admin commands refuse a database stamped before
`MIGRATED_SINCE` or after this build's version, before reading it, with an
error naming the versions it reads. A database from before the stamp takes
the current version.
The database also holds its log's identity (`log_identity`, a `LogId`
minted with it), and every `serve` is a new run (`RunId`), recorded in
`log_runs` with the head it started from, in one statement. The record is
append-only, so a restored backup holds only the runs before it. It runs with `synchronous = FULL`: under WAL, `NORMAL` may
drop the last commits on power loss, and the log is the only record of what
users did; the cost is one fsync of the WAL per commit. A writer waits up
to 60 s (`BUSY_TIMEOUT`) for another connection's write lock: an admin
command, an import or a workdir answer holding it for seconds only delays
the daemon's appends, which would otherwise stop `serve` (below).

- **Connections** (`ConnectionLimits`): at most 512 open at once and 32 per
  source, an IPv4 address or an IPv6 /64 (an IPv4-mapped address counts as
  its IPv4 one). One past either cap is closed as soon as it is accepted,
  before TLS. Every time limit is on *no progress*, never on total
  duration, so a 60 s long-poll and a slow blob download stay up:
  - 10 s for the TLS handshake.
  - 30 s idle: no request in flight (from its parsed head until hyper
    drops its response body), no write waiting on the socket, and (checked
    when the clock runs out, on Linux and Android) nothing left
    unacknowledged in the kernel's send queue. This bounds a slow request
    head and a keep-alive connection between requests, and never runs out
    while a response is still on its way.
    Hyper's own header timeout is off because it starts as soon as a
    response is encoded and cut slow downloads on kept connections.
  - 30 s without a byte of a request body: 408 (which devices retry; the
    400 the JSON extractor would make of it they take as final), and the
    connection closes after the reply.
  - 60 s in which the peer acknowledges none of what was sent
    (`TCP_USER_TIMEOUT`, so a slow link that keeps acknowledging is never
    cut, while one gone or stuck at a zero window is). Linux and Android
    only; elsewhere only the OS's own retransmission limit applies, and a
    peer advertising a zero window can hold its connection.

  Accepted sockets use TCP keepalive (probes after 60 s of silence, every
  15 s, 4 times). `serve` raises its soft open-files limit to the hard
  limit at start.
- **Commands** are deduplicated, validated and appended in one transaction;
  rejected ones are stored too, so their answer stays stable. Validation
  reads projections of the log (`sessions`, `contexts`, `approvals`,
  `shell_runs`, `event_blobs`, `screen_captures`) updated in the transaction
  that appends the event.
- **Deleting an event blob** (D42): `DeleteBlob { blob }` is `UnknownBlob`
  when no event referenced the blob (no `event_blobs` row: a workdir blob,
  an id never minted) and `BlobGone` when one did but its `blobs` row is
  gone. Otherwise its transaction deletes the `blobs` row and appends
  `BlobDeleted { blob, holds, by }`; the file is removed after the commit
  (a crash in between leaves a file without a row, which the startup sweep
  removes, Blob GC below). The `event_blobs` row stays, which is what tells
  `BlobGone` from `UnknownBlob`; the referencing event and its preview are
  untouched. It starts no work, so it is taken while the disk is low and
  while `serve` stops: freeing space is what a low disk needs.
- **Cropping an event blob** (D42): `CropBlob { blob, keep }` is refused
  like a delete (`UnknownBlob`, `BlobGone`), then `NotCroppable` unless
  the blob holds a tool output or a shell run's output, then `BadCrop`
  for spans of the wrong shape (`skywave_proto::crop::check_spans`:
  none, unsorted, overlapping, empty, more than `CROP_SPANS_MAX`). The
  new blob is written before the transaction, like an adopting create's
  history: the API checks all that briefly on its connection and opens
  the blob, then streams it through `skywave_proto::crop::crop` into a
  new blob file on a blocking thread, off the connection, within
  `CROP_DEADLINE` (20 s, under the device's 30 s reply bound). The open
  file survives a concurrent delete's unlink. Spans past the last line or
  keeping every line are `BadCrop`, a write failing for space (`ENOSPC`,
  `EDQUOT`) is `DiskLow`; both are stored like any refusal and leave no
  file. Past the deadline the reply is a 503 and nothing is handled;
  another I/O failure is a 500. A resend while the crop is written waits
  for its reply, as for an import. The transaction checks again that
  `blob` is stored (`BlobGone` if a delete or crop got there first),
  records the new blob, deletes the old `blobs` row and appends
  `BlobCropped { blob, holds, into, kept, lines, by }` (`kept` as asked:
  spans that touch are not merged); the old file goes after the commit,
  and a crop that does not commit removes its new one. `into`'s
  `event_blobs` row takes the old row's session, seq and holds, not the
  `BlobCropped`'s: the new blob is the content of the event that
  referenced the old one, so the storage report counts it there (a shell
  run finds its output by its `ShellFinished` seq) and a later crop or
  delete is stamped like the first. Taken while the disk is low and while
  `serve` stops, like a delete.
- **Images on a message** (v30, D50): `SendMessage { attachments }` names staged
  images (`PUT /v1/attachments/{sha256}`) by hash, and is prepared like a
  crop, before its transaction. Each staged image is hard-linked into a
  fresh blob (`BlobStore::link`, a copy across file systems), not adopted,
  so a refused message keeps its staged files for a resend and the same
  image named twice is two blobs (D44). The transaction records those
  blobs' rows, each `event_blobs` row holding `attachment`, and appends the
  `UserMessage` with its attachments; the staged files go after the commit.
  Refusals, in order: `ServerStopping`, `DiskLow` (nothing is linked),
  `UnknownSession`, `ContextChanged`, `MessageTooLong`, then
  `TooManyAttachments` (more than `ATTACHMENTS_MAX` names), then
  `AttachmentsTooLarge` (an image over `ATTACHMENT_MAX_BYTES`, or the images
  total over `ATTACHMENTS_MAX_TOTAL_BYTES`), then `UnknownAttachment` (a name
  not staged, a staged file that is not an image, or one no longer the hash it
  names; the image check is here, not at staging). A staged file no longer the
  hash it names is removed at that point, so the device's next `HEAD` misses
  and it uploads the file again. A refused command's linked files go as a crop's do. A
  crash between a link and the commit leaves blob files without rows, which
  the startup sweep removes; one between the commit and the staged file's
  removal leaves that file, which eviction removes. `DeleteBlob` takes an
  attachment's blob; `CropBlob` answers `NotCroppable`. Delivery (the
  supervisor's hand-over, `user_input`) opens each image's blob before the
  agent gets the message: one that is gone or cannot be opened is left out,
  `[image removed before delivery]` is appended to the text, and the message
  is still delivered. An open file keeps its bytes if the blob is deleted
  after, so the agent's copy outlives a delete that lands in the hand-over.
- **Restart cost**: nothing prunes the log yet, so what a daemon start
  reads must not grow with it. Each lookup it makes, per session or over
  the log, reads a projection (`open_calls`: a `ToolCall` adds its call,
  a `ToolResult` or `CallsUnanswered` removes it), a partial index over
  what is not over (pending approvals, unfinished shell runs), or a
  session's newest event of one type through the `events_session_type`
  expression index (one `MAX(seq)` per type; an `IN` over types would read
  every match). Safety rules, which apply in every session, are read per
  approval through the `events_type` expression index (newest event per rule
  across the log). The supervisor's held and owed events are re-read from
  their stored seqs on. A start thus costs the sessions and what is open,
  not the history: 100k events recovered in about 150 ms before, 3 ms
  after, and 3 ms at 1M (`recovery_on_a_large_log`, by hand). The
  per-append cost is the index and projection writes in the same
  transaction.
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
- **Worktrees** (D52): `CreateSession { workdir, conversation, worktree }`
  with a branch name in `worktree` makes a git worktree of the workdir's
  repository on that new branch, at `<parent>/<repo>.worktrees/<dir>` beside
  the repository, where `<dir>` is the branch with each `/` as `-` (`feat/x`
  is `feat-x`), starting at the workdir's `HEAD`. The session runs at the
  worktree's root. The repository is the parent of
  `git rev-parse --git-common-dir`, so a workdir inside a linked worktree
  resolves to the main repository; a bare one is `NotARepository`. The
  parent must lie under a root, else `WorkdirNotAllowed`. The branch name
  goes through `git check-ref-format`; an existing branch is `BranchExists`,
  any other git failure `WorktreeFailed` (git's stderr is logged). A branch
  whose `<dir>` already exists, such as `feat-a/b` once `feat/a-b` has one,
  is also `WorktreeFailed`. With a `conversation` too, the command is
  `BadConversation` and no worktree is made. As for an import (D37), the git
  work runs in the command's preparation, on a blocking thread and outside
  the transaction, under `WORKTREE_DEADLINE` (20 s). The git runner kills
  git past it; a tokio timeout would answer while git still ran. Disk-low
  and server-stopping are checked before git runs. Create and remove share
  one process-wide mutex (`crates/skywaved/src/worktree.rs`). A worktree
  whose command is refused, or whose transaction does not commit, is
  discarded (`worktree remove --force`, `branch -D`); only a worktree just
  made is ever discarded. `RemoveWorktree { session_id }` is the only
  command that removes one: archiving never does. It answers
  `WorktreeRemoved { by }` after these checks, in order: the session has a
  worktree (else `NotAWorktree`); no session in the same workdir has a
  running turn, a message handed to its agent whose turn has not started, or
  a message the supervisor has not handled yet (`TurnRunning`), nor a shell
  command running or waiting (`ShellRunning`); the branch and the worktree's
  `HEAD` are merged into the main repository's `HEAD` (`merge-base
  --is-ancestor`, else `BranchUnmerged`); the worktree is clean (`status
  --porcelain`, untracked files included, else `WorktreeDirty`). Then
  `git worktree remove` (pruned instead when the directory was deleted by
  hand) and `branch -D`, after the branch's own merge check. Nothing is
  forced. The server reads a session's worktree from its `SessionCreated`,
  the first event it logs; there is no server column for it. A session that
  already has `WorktreeRemoved` skips git, so a resend after a failed
  transaction is accepted. `WorktreeRemoved` makes the supervisor stop the
  agents of every session in that workdir. Afterwards work (messages,
  settings, `ClearContext`, `MoveContext`, `RunShell`) is refused
  `WorkdirNotFound`, decided from the log, for the removed session and for
  every other session in that directory, until a new worktree is made at
  that path (D52). A crash between the `git worktree add` and the session's
  commit is undone at startup. `create` records the worktree's intent (repo,
  branch, target, start commit, command) in `worktree_intents` before git
  runs, and the row is dropped with the commit or the discard. `serve`
  sweeps the rows after the serve lock and before the API serves: a row
  whose command was accepted is only dropped; any other goes to
  `worktree::sweep`, which removes its worktree and branch only while git
  still matches it.
- **Agent supervisor** (D3, D4, D8): at most one agent process per session,
  started lazily for the context a message or setting request is stamped
  with: resumed from that context's opaque handle, else forked from the
  context it continues (D30) or the outside conversation it adopts (D32),
  else fresh. A handle whose conversation the agent no longer has (its
  history deleted on the server) fails the start, never falls back to a
  fresh one: the message is `Errored` saying the history is gone and to
  start a new context. A clear or switch stops a running agent like `StopSession`.
  The session's grants pass to every start of its contexts, never to
  another session. An approval waits with no timeout but ends with the agent
  process. When the session's safety check is on (its newest
  `SafetyCheckSet`), an approval that carries a `shell` command is first
  given to `skywave_safety::check`: a `Safe` verdict answers the agent at
  once and logs `SafetyCheckApproved` instead of `ApprovalRequested`, so
  nothing is pending; `Ask` logs the approval as usual (D59).

  Beside the read-only proof, the check takes the person's safety rules
  (D61): every `SafetyRuleSet` across sessions, newest per `words` winning.
  A simple command that starts with a rule's words is trusted, though its
  redirections are still checked; a line passes only when every simple
  command is covered. The approval's `reason` names the rules used. Devices
  keep the rules in their replica (core's `safety_rules` table) for the
  settings list, and a shell row offers its commands' literal words
  (`skywave_safety::simple_commands`) as rule candidates. The approval panel
  offers the same candidates for the approval's `shell` command (so a
  subagent's too) as **Trust…**. **Trust and approve** sends one
  `SetSafetyRule` per kept prefix, then approves: a rule is checked when a
  call arrives, never against an approval already pending (D61).

  Both events carry the `shell` command. A device can then send
  `SafetyCheckFeedback` on such a call: `ShouldApprove` where a person was
  asked, `ShouldAsk` where the check approved. The server checks that from
  the call's newest `ApprovalRequested` or `SafetyCheckApproved` in the log
  (else `NotAShellCall`), re-runs the check and logs `SafetyCheckFeedback`
  with today's verdict, the judgment and the trimmed note. Core puts it on
  the call's tool row (`ToolView.safety_feedback`): which judgment the row
  may send and the newest one sent. `skywaved safety-feedback` exports them
  all as JSON lines (D60).

  A question the agent asks with options (`QuestionAsked`, D47)
  is an approval in all but its answer: the same ids, pending list,
  cancels and first answer wins. `AnswerQuestion` answers it with one
  `QuestionAnswer` per question, checked against the questions in the
  `approvals` row (`answers_fit`, else `BadAnswer`), and logs
  `QuestionAnswered`, which the supervisor hands the agent as it does an
  `ApprovalResolved`. A denial declines it, and approving one without
  answers is `BadAnswer`. An `Interrupt` repeated before the agent ends the interrupted
  turn is accepted and stops an agent that has not answered the first
  (no timer: the turn's end is the answer); the turn ends `Errored`
  ("agent did not answer the interrupt; stopped it") and the next
  message resumes the agent. On daemon start every pending approval gets
  `ApprovalCancelled { reason: "daemon restarted" }` and every call left
  without a result is named in a `CallsUnanswered`. Agent starts (a
  handshake of up to a minute) and stops (tens of seconds of grace) run
  beside the supervisor's loop, so one session's slow agent never delays
  another session's answers, interrupts or events. While one runs, that
  session's own events wait in log order and are handled once it
  completes, decided by the completion (started or failed, stopped), never
  by a timer; a start that forks a moved context waits until the source
  session has settled. Those waiting events are persisted with the cursor,
  so a restart handles them rather than losing them, and a daemon shutdown
  drops a start under way (killing its agent) and leaves its message for
  the next run. Messages are delivered at most once: the supervisor stores
  each message as owed (handed over, its turn not started) before the agent
  gets it, and drops it once the agent's `AgentWorking` for it, or an
  `Errored` reporting it, is logged. The adapter says which turns are a
  message's (`TurnStarted { message }`): a turn the agent starts on its
  own (a wakeup, a background job's report) answers no message, so a
  message queued behind it stays owed. The agent may instead take that
  message up inside its own turn (`TurnStarted { message: true }` while the
  turn is open): no `AgentWorking` follows, so the message is settled as
  that is read. Each settling event is tied to its
  message by position: the message is stored as *starting* (or
  *settling*, for an `Errored`) just before the event is logged, and a
  restart counts only such an event, logged after the stored rows, as
  settling it. Every message handed to an agent thus ends with its turn
  or exactly one `Errored` in its context, and one the agent answered is
  never errored. Messages an agent accepted but never
  began (queued behind a turn when it is stopped, killed or crashes) are
  named in the one `Errored` its exit logs, ending "N queued messages were
  not sent to the agent; send them again" (counted by the adapter's
  `Exited`, or by the supervisor when that is more). The messages still
  owed when the daemon stopped, cleanly or not, in any session, are
  reported on the next start, once: "message lost when the server stopped;
  send it again", or, queued behind a turn lost with the daemon, in that
  turn's `Errored`. They are reported, not redelivered: the user stopped
  the work, or a crash leaves delivery at most once. If the supervisor
  fails, it stops its agents as a shutdown does (an interrupted turn's
  grace, then SIGTERM, so the CLI stops the commands it started; SIGKILL
  only for what outlives the term grace), logs nothing more, and `serve`
  stops rather than accept messages no agent would hear.
  It fails only when it cannot read or append to the log (an append it
  skipped would leave the log without an event the mapper counts as
  logged); a session's own trouble (an agent that will not start, exits
  or refuses) is logged in that session, and a blob it cannot store is
  marked lost in its event (Event mapper). Recovery stores no blobs, so a
  restart on a full blob disk does not fail the same way again.
- **A low disk** (D40): the free space of the data dir's file system
  (`statvfs`) is checked against `[disk] min_free` (default 1 GiB; 0
  turns it off) only where something is about to take space: at `serve`'s
  start, by the API when a command that starts work arrives, by the
  supervisor as it logs agent events, and by the shell runner as it stores
  and logs output. A reading up to 5 s old is reused, so a burst costs
  one, and an idle server reads nothing and runs no timer. Below the floor
  the disk is *low*; the flip starts the one poll left, every 5 s until
  the space is back, which flips it back and stops. While low:
  - Commands that start new work are `DiskLow` before anything else is
    checked: `CreateSession`, `SendMessage`, `SetPermissionMode`,
    `SetModel`, `SetEffort`, `ClearContext`, `MoveContext` and `RunShell`.
    Control is taken as usual (`ResolveApproval`, `AnswerQuestion`, `Interrupt`,
    `StopSession`, `StopTask`, `StopShell`), and so is what logs one small
    event and starts nothing (`RenameSession`, `SetSessionArchived`,
    `DeleteSession`, `RemoveWorktree`, `SwitchContext`, `DeleteBlob`,
    `CropBlob`). Reads and long-polls are untouched. The refusal is
    stored like any other; the heads show its reason, which is how a
    device learns the server is paused.
  - The supervisor stops reading its agents' events. An adapter reports on
    a bounded channel, so the agent then waits to report (the CLI blocks on
    its stdout): it stays alive and supervised, nothing is lost, and its
    events are logged in order once the disk has room. The adapter's stdin
    write timeout runs only while a write is in progress, never in that
    wait, so a paused CLI is not killed as wedged. An interrupt or task
    stop still goes to the agent, and that session alone is read until the
    turn ends or the stop is answered; a stop drains and stops the agent as
    always. No agent is stopped as idle while paused. Commands accepted
    before the pause still reach their agent.
  - The shell runner stops reading a running run's output: it waits in
    the pipe, and the command blocks once the pipe is full, paused like an
    agent with nothing lost. A stop still ends the run.
  - A start below the floor comes up paused.
  The floor is what normally keeps the log from filling. A disk that fills
  between checks anyway fails an append, which ends the supervisor (above)
  and `serve`; before its first write the next `serve` waits in the same
  5 s loop, logging that it does, until 64 MiB (at most the floor) is free,
  so a full disk never makes it fail on every start.
  An agent process that reports a version other than the one its adapter
  was checked against logs one `AgentVersionUnconfirmed` (neither notable
  nor activity) in the context it was started for, and every head shows it
  as a warning in the timeline. The Claude Code adapter also starts the CLI
  with its auto-updater off (docs/agents/claude-code.md), so the version
  changes only when the operator updates it.
- **Storage report** (`GET /v1/storage`, `crates/skywaved/src/storage.rs`):
  sizes, never content, for an operator deciding what to remove. The
  file sizes come from the file system (database and WAL, `shell/`, the
  configured `log_file`), blob sizes from the `blobs` rows (workdir blobs
  are those `workdir_blobs` names, event blobs the rest), and the free
  space from `statvfs` as read now, recorded nowhere. A session's share is
  its events' stored JSON plus the blobs its events reference, which the
  `event_blobs` projection records with each append (`EventKind::blobs`:
  blob, session, seq and what it holds). A shell run's is its
  `ShellOutput` events (read per session through `events_session_type`)
  plus its output: the finished run's blob or the running one's file;
  `output_blob` names the finished run's blob, the one a device deletes
  or crops. A deleted blob has no `blobs` row, so it counts nowhere; a
  cropped one's replacement counts where the original did. Summing every session's events reads the whole log, so a report costs
  the log's length: it runs on a connection of its own, one at a time,
  in one read transaction (appends go on beside it under WAL), and SQLite
  stops it at its 8 s deadline (a progress handler), a 503; the reply
  gives up at 10 s. The report itself removes nothing: deleting a blob
  is a command (`DeleteBlob`, above). A deleted session (`DeleteSession`,
  D62) is in no report from the moment its delete is logged. The supervisor
  then erases its rows and has the agent delete its transcripts of the
  session's conversations (D62). The erased rows stay in the file as free
  pages until it is compacted: `reclaimable` is their size (`freelist_count`
  times `page_size`), and `POST /v1/storage/reclaim` compacts it. Reclaim is
  a `VACUUM` then `PRAGMA wal_checkpoint(TRUNCATE)`, on the storage reader's
  connection, answering the fresh report. It is explicit only, never
  automatic: `VACUUM` blocks other writers (they wait out their busy
  timeout) and writes a copy of the database, so it is refused with 507 when
  the free space is below the database's size plus `min_free`, and nothing
  runs. Only the wait for the connection has a deadline; the `VACUUM` itself
  is not cut short, and its reply gives up at 10 minutes.
- **Usage limits** (`GET /v1/usage-limits`): the account's limits belong to
  no session, so the API asks a `UsageLimitsSource` (agent-agnostic;
  `NoUsageLimits` answers `Unavailable`). The Claude Code one
  (`agent/claude_code/limits.rs`) starts a process of its own for each
  read, as a session's but under a new session id and in the first root
  (the data dir with none), writes one `get_usage` with no handshake,
  reads until its answer and kills it (docs/agents/claude-code.md §3.15).
  A process sent no message keeps no conversation, so the read leaves
  nothing behind; reads run one at a time. The source gets 15 s, past
  which it answers `Failed`; the reply gives up with a 503 at 20 s. A
  scope is cut to one line of `TITLE_MAX_CHARS`, a reason of
  `REFUSAL_MAX_CHARS`.
- **The agent's login** (`login_expires` on `hello`): a headless agent
  never says its login is about to expire, and past it no session starts
  until someone logs the agent in again on the server. Each `hello` asks
  an `AgentLogin` (agent-agnostic; `NoAgentLogin` says nothing) when it
  expires, so a login renewed on the server shows at a device's next
  connection. The Claude Code one (`agent/claude_code/login.rs`) reads the
  refresh token's expiry from the CLI's `.credentials.json` each time,
  and says nothing when the environment logs the CLI in another way
  (docs/agents/claude-code.md §3.16). Nothing is stored on the server.
  Whether the agent is logged in at all is kept in memory, in one
  `LoginState` (`Unknown`, `LoggedIn` or `LoggedOut`) the supervisor
  fills from what its session processes say (`AdapterEvent::LoggedIn`):
  a process's handshake, and a turn it refused for want of a login. It
  starts `Unknown`, and no process is started just to find out. The cell
  counts login successes too; the supervisor notes the count when it
  begins a process's start and when it hands the process each message
  (`LoginSeen`). A "logged out" from the start is dropped once a login
  has succeeded since the start began; one from a refused turn, once a
  login has succeeded since that turn's message was handed over (a turn
  the agent started on its own counts from the latest message, else the
  start). Either may have been answered before the process saw the
  login; a refusal of a message handed over after it is fresh. For
  Claude Code the rule is in docs/agents/claude-code.md §3.17. Each
  `hello` reports the cell as `agent_logged_out` (true only for
  `LoggedOut`), and `AgentLogin::offered` as `agent_login_offered`: false
  for `NoAgentLogin`, and for Claude Code when the environment logs the
  CLI in another way, which a login made from a device would not
  override. `agent_logged_out` is reported either way. `AgentLogin::begin`
  starts a login attempt (`LoginAttempt`): a URL to sign in at, a shape
  check of the pasted code, and `finish`, which hands the code to the agent
  and waits, with no deadline, for its answer. Dropping the attempt stops
  it. The Claude Code one runs the CLI's manual-code login in a process of
  its own (docs/agents/claude-code.md §3.17). A device drives it over
  `POST /v1/agent-login/start` and `/finish` (D49) through `LoginFlows`
  (`login.rs`), one in-memory slot: idle, starting, open, finishing or
  done. A start mints a random `LoginFlowId` and calls `begin` on a task
  of its own; a start while it runs waits for the same answer, and one
  while the flow is open answers the same flow and URL, so racing starts
  run one process. A start while a finish runs is `Busy`. A finish naming
  another flow is `NoFlow`. The first finish of the open flow checks the
  code's shape (a refusal keeps the flow open and reaches no agent), then
  hands the code to the attempt on a task of its own; that outcome,
  logged in or the agent's refusal, is the flow's, and every later finish
  of it gets it, whatever code it carries. A success tells the
  `LoginState` (`login_succeeded`), and the answer's `expires` is read
  right after. An open flow 15 minutes old (`LOGIN_FLOW_MAX`) is dropped,
  its process with it, at the next start, finish or `hello`: there is no
  timer and no cancel. The slot's lock is never held across an await,
  since `hello` looks at it on its blocking thread. A restart forgets the
  flow. Neither the URL nor the code is logged or stored, and the flow
  logs no event.
- **Event mapper** (agent-agnostic): gives each tool call a fresh `CallId`,
  cuts every text to the limits on char boundaries, and makes a blob only for
  content that was cut, so a large payload costs a bounded event plus a blob
  fetched on demand (D11). A call's input is a blob only when its display
  does not carry all of it (the adapter's display left a field out, or the
  detail, or a title beside a body, was cut); a cut body's blob already
  holds it whole, so a big Write, Edit or command is stored once and
  deleting that blob frees its bytes. Each such field (`ToolCall::input`,
  `Body::full`, `ToolResult::output`, `ApprovalRequested::input`) is a
  `Full`: `None` when the preview holds it all, `Blob` with the reference,
  `Inline` with the content itself, or `Lost` when the blob could not be
  stored (a full disk, its row refused). A tool result's whole output of
  at most `limits::INLINE_MAX_BYTES` (4 KiB) is `Inline` rather than a
  blob: an adapter's preview nearly always differs from the output (an
  edit's note to the model, a read's line numbers, a grep's header), and
  a blob that small costs a file, two fsyncs, a row and, under the eager
  policy, a request per device for less than its own overhead. Heads
  treat it as content already here: an expanded section shows it, a copy
  takes it and a search looks through it; it has no blob actions. A lost one is logged at error; the event keeps its cut
  preview and whole `lines` count, the turn goes on, and every head says
  the rest is gone (`skywave_present::sections`), up front in an approval
  prompt, since the user decides on the summary.
  Agent text travels whole in its `AgentMessage` up to
  `limits::MESSAGE_WHOLE_MAX_BYTES` (16 KiB, several screens: nearly
  every reply); a longer one (a report, a dump) is its first
  `limits::MESSAGE_PREVIEW_MAX_BYTES` (2 KiB, cut after its last line
  break) with `full`, a `BlobUse::Message` blob of the whole (or `Lost`),
  so it does not reach every device as event data. An import bounds its
  user and agent messages the same way. A typed `UserMessage` stays whole:
  `SendMessage` caps it at `limits::MESSAGE_MAX_BYTES`, and the supervisor
  hands the event's text to the agent. Heads show a long message's head
  as the message and its whole as a section beneath it
  (`Section::message`): collapsed it shows only where the blob stands
  (download state, deleted, not stored); expanded it fetches the blob and
  shows its head (capped like any blob's), so the head shows twice. A
  copy takes the whole once here, a search looks through it, and its blob
  may be deleted like any other but not cropped (a message cut to some of
  its lines says something else).
- **Shell runner** (D35): a sibling of the supervisor that follows the log on
  its own thread and connection. One FIFO per session, one run at a time,
  sessions in parallel. A run is `<program> -c <wrapper>` in the session's
  last cwd (a `cd` sticks), with no controlling terminal, stdin
  `/dev/null`, and `TERM=dumb`, `NO_COLOR=1`, `PAGER=cat`, `CI=1`. Its
  `when` is matched against the session's last end (`Exited { code: 0 }`
  is success). A run ends when the shell exits; its process group is then
  killed, and on Linux a subreaper host reaps orphans. Output streams to a
  file (capped by `output_max`), reported as a tail at 2, 5, 10, 30 s,
  then every minute up to 10 min, every 5 min up to 1 h and every 15 min
  after (D39), only when the tail changed, and becomes a blob unless the
  preview holds all of it: the file is hashed in place and renamed into
  `blobs/`, not copied (copied only if `shell/` is on another file system);
  output that cannot become a blob is logged `truncated` without one.
  On start, runs left running are finished `Lost` and queued ones run; if
  the runner fails, `serve` stops rather than accept runs nothing would
  start.
- **Screen runner** (D65): a sibling of the shell runner in
  `crates/skywaved/src/screen/`: `mod.rs` the config and its validation,
  `backend.rs` the `ScreenBackend` trait, `android.rs` the adb backend (the
  only file that names adb), `capture.rs` the decoding, scaling and encoding,
  `runner.rs` the runner thread. A `Screen` command is validated in
  `commands.rs` and logged as `ScreenRequested`, its `command_id` the
  request. The runner follows the log on its own thread, runs one request at
  a time from one FIFO for the whole server, and logs `ScreenDone` through
  `EventLog::screen_event`. Every action but `Devices` ends with a capture,
  scaled to a longest side of 1280 px and JPEG-encoded (or a full PNG when
  asked); a capture equal to the latest is logged `Unchanged` and keeps the
  latest's id. Two server tables back it: `screen_requests` holds the open
  requests, for recovery, and `screen_captures` the latest capture per
  session and device, for the stale check and the coordinate scaling. After
  each `ScreenDone` only the newest 20 capture blobs of its session remain,
  and never a device's latest; the older ones are deleted in that transaction
  and listed in its `pruned`.
  On start every request without a `ScreenDone` is settled `Lost`, never run
  again, or `NotStarted` while the runner is disabled. Config:
  `[screen] enabled` (off by default) and `adb` (the adb binary). See D65.
- **Workdir reads** (D36): list and file resolve the session's stored
  workdir, which must still canonicalise to itself under the roots, then the
  requested path, which must be relative, must not climb out with `..`, and
  must canonicalise under the canonical workdir. A file must be a regular
  file before it is opened (a FIFO would block). Reads run on their own
  connection, one snapshot at a time behind a mutex, so they never hold the
  API's connection and a refresh storm costs one snapshot at a time. Each
  request's deadline (15 s) starts with its handler: a request that has not
  got the lock by then (it polls the lock, so a snapshot stuck on a hung
  mount does not keep waiters forever) does nothing (a 503), and work in
  progress stops at it (a
  503 for list and file; changes answers `Failed { "timed out" }`, its git
  children killed). The outer timeout is 20 s. Content blobs are
  deduplicated by `(sha256, size)` in `workdir_blobs`, so an unchanged file
  keeps its blob across refreshes; event blobs are never reused for them,
  nor deduplicated among themselves (D44). `download` shares `file`'s path
  rules and differs only in serving binary content and in its cap. Its copy
  runs outside the workdir lock under its own 120 s deadline
  (`DOWNLOAD_DEADLINE`; its outer timeout is 125 s): the file is streamed into
  the blob store with no lock held, so a large file neither blocks other
  reads nor outlasts the 15 s snapshot deadline.
  **Blob GC**: a workdir blob lives while the latest answer to some read
  names it, any session: `workdir_answer_blobs` keeps the blobs of each
  read's latest answer, keyed as the core keys its stored answers
  (session, `changes`, `file` or `download`, the path as requested), and every
  changes, file or download answer but `ChangesResult::Failed` replaces its read's
  rows. In that transaction every workdir blob no answer names loses its
  rows; its file goes after the commit. All of it runs on the workdir
  connection under the snapshot mutex, so no pass falls between a store
  reusing a blob and the answer naming it. A device holding a replaced
  answer (lost reply, another device's refresh) gets a 404 for its blob
  and shows the download failed until a refresh. An event blob lives until
  a device deletes or crops it (`DeleteBlob`, `CropBlob`, above); nothing
collects one. `serve`,
  once its address is bound
  and before the supervisor, shell runner or API can write blobs, runs the
  same pass, then removes every `.tmp-*` file in `blobs/` and every blob
  file without a row (an interrupted write, a crash between a file and
  its row, or one between a deletion's commit and its unlink). One `serve` per data dir is enforced: `serve` first takes an
  exclusive OS lock (`flock`) on `serve.lock` in the data dir, held for
  the process lifetime, so a second `serve` on the dir, on any address,
  fails at once naming the dir. `pair` and `devices` do not take it and
  run beside the daemon.
  Changes run the `git` CLI in the workdir (no git library): `rev-parse`
  decides `NotGit` from its output, then one `git diff <the branch's full
  ref, bare HEAD when detached, or the empty tree> --relative -M -z --raw
  --numstat` gives the entries (so a workdir below a repo's top shows its
  own subtree, in its own terms),
  `ls-files --unmerged` marks `Conflicted`, and `ls-files --others
  --exclude-standard` the untracked ones. One streamed `git diff` gives the
  patches, split at `diff --git ` lines and paired with entries by their
  exact expected header, never by position; an entry with a quoted path, a
  header shared with another kept entry, more sections than its record
  gives (an entry past the cap may share its header), or no section gets
  its own `git diff -- :(literal)<path>`, of which it keeps only sections
  with its own header. Both listings stream into a heap of the first 500
  by path, so memory is bounded by the cap rather than the workdir, and
  only those untracked paths are `lstat`ed.
  An unmerged path whose worktree equals `HEAD` is still a `Conflicted`
  entry. Untracked diffs are synthesised from `lstat` (a
  symlink's diff is its link text). At most 500 entries (`total` counts
  all), 1 MiB per diff and 8 MiB per snapshot are kept. Every child runs
  with fixed flags (`core.fsmonitor=false`, no external diff, no textconv,
  no pager, `--submodule=short`), the repository's own filter drivers emptied, inherited `GIT_*`
  variables removed, stdin closed, and leads a process group killed at the
  deadline. The commit log (`GET /v1/workdir/log`) runs `git log` on the
  commit the branch names, by its full ref (never the bare `HEAD`, which a
  ref named `HEAD` could shadow on an unborn branch), with
  `--no-show-signature`, since a repo's `log.showSignature` with its
  `gpg.program` would run a program; it lists at most `WORKDIR_LOG_MAX` (50)
  commits plus `more`, an unborn branch is an empty log, a bad author date
  is no time, and nothing is recorded, as the answer names no blob.
  The worktree list (`GET /v1/worktrees`) covers the repositories that
  sessions' `SessionCreated` worktrees name (their `worktree.repo`, distinct
  and sorted; there is no repo column, D52). A repo whose directory is gone,
  or no longer canonicalises under a root to itself, is `Gone` and runs no
  git. Otherwise `git rev-parse --show-toplevel` runs in it first: a non-zero
  exit, or a top level other than the repo itself (its `.git` is gone, so git
  finds a parent repository), is `Gone` too, with no further git. Then `git
  worktree list --porcelain -z` runs in it: NUL-separated, so a path with a
  newline cannot forge a block; a path that is not UTF-8, or a `HEAD` that is
  not a 40 or 64 digit lowercase sha, fails the repo (`Failed`). A bare block
  is skipped, an all-zeros `HEAD` is unborn, and a block is `missing` when it
  has a `prunable` line or its directory is not a directory (git prints no
  `prunable` for a locked worktree whose directory is gone). A missing
  worktree, or one outside the roots (not canonical under them to itself),
  runs no status and its `dirty` is `None`. Otherwise `dirty` is `git status
  --porcelain --untracked-files=normal --ignore-submodules=dirty` run in its
  directory, after the repo's filter drivers are neutralised as in Changes
  (`status` runs a `clean` filter to compare a changed file): the check
  `RemoveWorktree` makes, except that a submodule's own dirty content is
  neither counted nor read, so its filters do not run. `merged` is `merge-base --is-ancestor` of the worktree's `HEAD`
  against the main working tree's, run in the repo (exit 1 is not merged,
  any other exit `Failed` for that repo); the main working tree is merged by
  definition. Entries past `WORKTREES_MAX` get no git call. Nothing is
  recorded.
- **Workdir writes** (D56): `POST /v1/workdir/write` runs as one workdir
  snapshot (`workdir_task`), under `WRITE_DEADLINE` (120 s, a 503 past it),
  and checks `DiskLow` first. The target is refused `NotAllowed` for an
  absolute path, one climbing out with `..`, any `.git` component, or a last
  component that is not a normal name. The parent must exist as a directory
  (`NotFound`) and canonicalise under the canonical workdir (`NotAllowed` for
  a link out; a link inside it is followed). The file is opened with
  `O_NOFOLLOW`: a symlink is `NotAllowed`, a directory `IsADirectory`, another
  non-regular file `NotAFile`. A regular file already holding the staged bytes
  is `Same`, and nothing is written. Otherwise it is replaced only when
  `replace` names its sha256, else the answer is `Exists { sha256, size }`. A
  new file is placed without clobbering, so a file that appears meanwhile is
  read and answered by the same rule. The staged upload is copied into a temp
  file in the target's directory and hashed as it goes; one that no longer
  hashes to its name is `NotStaged`, and the staged file is kept (staging is a
  cache). A replaced file keeps its mode, a new one is `0644`, and the
  directory is synced after the rename. A write that lands is logged as
  `FileWritten` by the calling device and answered `Written`
  (`workdir_write::write`, `api.rs`).

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
   A handshake the device's pinned CA refuses is final, decided from
   rustls's verdict, not a retry count: a server certificate from another
   CA (the server's `ca/` was recreated, or something else answers) is
   `Rejected(ServerChanged)`, one that does not cover the address the
   device connects by is `Rejected(NameMismatch)`, and a stored identity
   that cannot be loaded into TLS is `Rejected(BadIdentity)`. Heads show
   the re-pair notice for these as for a revoke. A reissue that drops a
   name from the server certificate (`server_names` or the host name
   changed) warns on the reissuing command's output.

- **Codes:** 10 Crockford base32 symbols (50 bits), shown `XXXXX-XXXXX`;
  input ignores case, `-` and spaces, and reads `O` as `0`, `I`/`L` as `1`.
- **Rate limit:** at most 5 failed attempts per sliding minute per source,
  and 100 from all remote sources together, in memory; a full window
  answers 429 even to the right code. A source is the peer's IP (an IPv6
  address by its /64; all of loopback is one source). Loopback is outside
  the global cap, so a remote flood can't lock out pairing on the server
  itself; 100 a minute still leaves guessing a 50-bit code hopeless. So a
  few dozen remote addresses can't keep remote pairing blocked, issuing a
  code clears both windows: `skywaved pair` gets the owner unstuck. It may
  run as a separate process, so the running server reads, at each attempt,
  which code was issued last (`pair_codes`, highest rowid) and clears the
  windows when that changed. The pairing
  endpoint is the only one without a client certificate and exposes
  nothing else.
- **Pairing URI** (the QR code):
  `skywave://pair?addr=<host:port>&code=<code>&fp=<fingerprint>`, `addr`
  percent-encoded, `code` without `-`, `fp` as 64 lowercase hex digits.
  The core parses it (`parse_pair_uri`) so heads don't. An address without
  a port means 7443.
- **Addresses:** a paired device can reach its server at another address
  (`SetServerAddress`) without re-pairing: the pinned CA still decides
  whom it trusts. The address it leaves, or the one a re-pair with the
  same CA replaces, is kept among the server's other addresses
  (`ServerAddresses`, part of the pairing), to switch back to in one step
  or forget. Pairing with another server drops them.
  The core parses an address once, where it comes in (`Action::Pair`,
  `SetServerAddress`, the pairing URI), into a `ServerAddress`, and
  carries that type from there on: the pairing, the pairing request, the
  transport. Equality is on its canonical form (lowercase name, port
  filled in, IPv6 bracketed), so `MyHost` and `myhost:7443` are one
  address, and that form is what the core stores. Schema 29
  canonicalised the addresses stored before, dropping (with a logged
  reason) any that did not parse.
- **The pairing record** (`Pairing`): the device key, its certificate, the
  pinned CA and the server's addresses are one value, stored as one file
  in the core's data directory (`pairing.json`, mode 0600). A save writes
  `pairing.json.new`, flushes it, renames it over the record and flushes
  the directory, so a crash leaves the old pairing or the new one, never
  a mix and never none where there was one; a leftover `pairing.json.new`
  is ignored. A device is paired exactly when the record exists:
  `Link::Unpaired` is derived from it, and the sync engine reports a
  `PairedLink`, which has no `Unpaired`. A record that does not read (an
  address that does not parse, say) fails the open rather than leave an
  address the link retries forever. Pairing with another server resets
  the replica first and saves the record last: if the save fails, the
  device keeps its old pairing whole over an empty replica, and connects
  with it again.
- **Older data dirs:** a build before the record kept the identity in
  `identity/` (four files, replaced through `identity.new/`) and the
  addresses among the replica's settings. Opening the core moves both into
  the record, then removes them; a crash before that redoes the move, or
  finds the record and only clears up. A complete `identity.new/` without
  `identity/` (a crash between the old build's remove and rename) is
  recovered; an incomplete one alone was an interrupted first pairing. An
  identity without an address that parses (one schema 29 dropped) has no
  server to reach: the device is unpaired and pairs again, rather than
  showing `Connecting` with nothing trying.
- The device key and certificate are read behind the `Identity` trait, so
  a platform keystore can replace it.

## Client core

`skywave-core`. `Core::open(data_dir)`, `dispatch(Action)`,
`subscribe(ViewState)`, on its own tokio runtime. The state machine
(`Model`) does no I/O: the network tasks reach it through `core.seam()`,
receiving `Request`s and reporting results, and every report is persisted,
then pushed as a new `ViewState`.

The core runs its engine (and every task it spawns: link, blob
downloader, listings, pairing) on its runtime, and subscriber callbacks on
its delivery thread, `skywave-deliver`. A panic in a callback is logged
and skipped. A panic anywhere else on those threads would stop sync or
snapshots for good with nothing to show for it, so it is caught and
*fails the core*: `Core::on_failure(watcher)` calls the watcher once, with
what panicked. A task cancelled by `Core::shutdown` or a drop never
fails it. The GUI hub answers `HubFailed` and reopens the core on Restart
(or by itself, once, off screen under the Stay connected service);
the TUI exits with "the client core failed: ...".

The runtime has two workers, and the store's SQLite (and the disk work done under the state lock) blocks the thread it runs on. Engine tasks reach the state only through `Seam`, which runs each access in `block_in_place`, so the runtime moves the worker's other tasks (the link, downloads) to another thread meanwhile. Heads call `dispatch` from their own threads, never a core worker. Whole-buffer CPU work (hashing a message or workdir-push file before upload, decompressing a zstd response body) runs in `spawn_blocking`, so it never holds a worker either.

One core per data dir: an open core holds an exclusive OS lock (`flock`)
on `core.lock` in it, released when its last handle goes (`Core::shutdown`
for a deterministic close) or the process exits. A second core on the same
dir, in this process or another, fails with `LockError::InUse` ("another
skywave client is using <dir>"), which the heads show; otherwise both would
hit SQLite busy errors and each model would go stale against the other's
writes.

The replica, like the server's database, records the `PROTOCOL_VERSION`
its JSON is in (`protocol_stamp`; one from before the stamp takes the
current one), and opening it runs its own protocol steps (`PROTOCOL_STEPS`
in `store.rs`) from that version on, the outbox's unsent commands included
(D43). `Core::open` fails with `CoreError::ReplicaUnreadable` when the
stamp is before `MIGRATED_SINCE` or after this build's version, the schema
is newer, the file is damaged, or a row it loads no longer decodes
(`StoreError::unreadable`), instead of reading wire JSON it cannot migrate.
`Core::reset_replica(data_dir)` needs no open core, only the data dir's
lock: it drops what the server's log produced (events and their
projections, cursor, outbox, blob rows and files, workdir answers, the
stored usage limits, offered models, safety rules, device names) and restamps, in one transaction, keeping the
identity and everything that is this device's (settings, drafts, composer
modes, snippets, read marks, local deletes, alerted approvals), so the
next open pulls the whole log again from seq 0. A settings row this build
cannot read goes in the same transaction. If even that fails as
unreadable, it builds a new database as `core.db.new` (schema, stamp and
the old settings this build reads, in one transaction), checks that it
loads, removes the old WAL files and renames it over `core.db`, so a crash
leaves either the old database or the whole new one. The TUI names `--reset-replica` and exits; the GUI's
boot failure (`BootFailed.unreadable`) offers "Reset local data", which
asks first (unsent messages are lost) and then sends `Boot` with
`reset_replica`. Deleting a session locally skips (and logs) an event that
no longer decodes rather than failing.

- **Replica** (SQLite, `core.db`): the replicated `events`, projections
  folded in the transaction that stores them (`sessions`, and `turns`, each
  `TurnUsage` by seq for the usage screen), the `outbox`, the `cursor`,
  `blobs`, `settings`, and what is this device's alone (D5: drafts per
  session and composer mode, composer modes, snippets, read marks, local
  deletes, alerted approvals). A schema migration backfills a new
  projection from the stored events. `draft_attachments` keeps a session's
  agent draft's images in order, and `attachments` each image once by
  sha256 (its bytes are the file under `<data>/attachments/`); a blank
  text leaves them, sending the message or deleting the session drops
  them, and editing a queued message puts its images back before the
  draft's. An image's row and file go once no draft holds it (D50), no queued or
  failed (still editable) message names it, and no accepted message the
  replica has not applied names it; this runs when the store opens (which
  also clears files without a row), and not after each batch: a detached
  image keeps its row and file until the next open, so attaching it again
  in the meantime finds both. `draft_files` and `outbox_files` keep the
  non-image files a draft or a queued message carries, in order, each as a
  copy under `<data>/attachments/` (`store_file`, streamed, never in memory).
  A draft holds one file per name; queuing a message moves its files onto
  the outbox entry, and withdrawing it puts them back. A copy is kept while
  a draft, or a message the server has not accepted (queued, or refused and
  editable), names it; an accepted message's copy goes at the next open,
  since the server holds it. `workdir_snapshots` keeps the latest
  stored answer per workdir read (session, kind, requested path), so a
  workdir already browsed reads offline; `workdir_blob_refs` names the
  blobs each answer holds. A refresh replaces the answer, and a blob row
  goes once no answer of any session names it (the server deduplicates
  workdir content across sessions). An answer for a session not in the
  replica (unknown, deleted here, or from before a re-pair) is dropped.
  `usage_limits` keeps the latest answer to the usage limits read.
  `server_facts` keeps what the last `hello` said of the server
  (`ServerFacts`, `Settings::server`): whether it runs shell commands,
  whether it drives screens, whether it allows bypass mode, when its agent's login expires
  (`login_expires`), whether a device may log the agent in
  (`agent_login_offered`) and whether the agent is logged out
  (`agent_logged_out`). One row, replaced whole by each `hello` and
  cleared with the replica, so a re-pair to another server (or any reset)
  drops all of it until the next `hello`. It shows offline too.
- **Session list order**: newest first by `SessionSummary::last_message_seq`,
  the seq of the session's newest `UserMessage` (its creation until it has
  one). Only a message someone writes moves a session up. The agent's
  replies, tool calls, approvals and state changes do not, so a row does
  not jump while it is being read. `last_seq` and `last_activity` still
  follow every activity event, for read marks and the row's time. The row
  shows that time as `5 min ago` for the last day, then as the local date
  and minute (`skywave_present::format::activity_label`); the GUI hub wakes
  once a minute while the list shows, so the ages advance. The GUI
  groups the list by repo (the key is under **Worktrees** below) unless the
  user picked the plain layout, a per-device preference
  (`GuiIntent::SetPlainList`): a group sits where its newest row does, and
  its rows keep this order within it. The list has three scopes,
  `ListKind::Active`, `Archived` and `All`; the all list marks its archived
  rows, and back from a session returns to the list last shown, so
  archiving or unarchiving the open session doesn't move the user to another
  list; that list also shows when the open session is deleted.
- **Worktrees** (D52): the replica's `sessions` table has a `worktree`
  column (the JSON `{repo, branch}` of the session's `SessionCreated`, NULL
  when it has none) and a `worktree_removed` column (set by
  `WorktreeRemoved`). Both are in `SessionSummary` and `SessionRow`. A
  schema migration backfills them from the stored events. The worktree stays
  recorded after removal, so grouping holds. `Action::CreateSession` takes a
  `worktree`; `Action::RemoveWorktree` enqueues only for a session that has
  one; heads offer it for any such session that is not removed, archived or
  not. The timeline shows `WorktreeRemoved`. Heads group the list by repo
  (`skywave_present::session_list::by_repo`) with this key: the session's
  own worktree's repo; else the repo of a worktree session in the same
  workdir (a plain session started in a worktree joins its repo); else the
  workdir. A plain create makes no git call. The worktrees screen (below)
  lists them; the TUI has no worktree UI yet.
- **Sync engine** (D7): the pull loop long-polls from the cursor and applies
  each batch and the new cursor in one transaction. The outbox sends oldest
  first, one command in flight, so the server sees commands in the order
  they were made. A pulled event naming an unanswered command settles it as
  `Accepted` at its seq; a reply that stays unreadable is settled from the
  log, or failed as undeliverable once the replica has passed the log head
  read after it, so it is never resent into a duplicate. A refusal (4xx),
  an unreadable reply or a server error (5xx) is an answer about one
  command and keeps the connection, so the pull goes on. A 5xx is resent
  after a backoff; after `MAX_SERVER_ERRORS` (5) in a row the command is
  settled from the log the same way, so one the server keeps failing ends
  undeliverable (shown failed, discardable, never resent) instead of
  holding back every later command. A command whose own request failed
  (timed out, lost its connection, or a 408) is no answer, but each
  request has a connection of its own, so it does not end the link
  either: the pull goes on, and the command is resent, same id, after a
  backoff for the failures in a row, as often as it takes. A link that
  is really down fails the pull, which ends the connection. Before a message
  with images is posted (D50), the outbox stages them: each image is checked with a
  HEAD on every attempt and PUT from its local file when the server does not
  hold it (the PUT streams the file from disk, a 64 KiB chunk at a time, and
  never reads it whole), since the server unlinks the staged files a message
  used, so nothing is cached. When the HEAD reports bytes held of an
  interrupted upload (`skywave-upload-offset`, v37), the PUT resumes there:
  an `OffsetMismatch { have }` answer re-PUTs from `have`, a `HashMismatch` to
  a resumed PUT restarts once from 0, and an offset that keeps moving after 3
  PUTs is a transport error, retried from the HEAD (D58). A missing,
  unreadable or too-large local file, or an upload the server refuses as too
  large or a hash mismatch, fails the
  message as
  `AttachmentRefused`: final, discardable, never posted. (An outbox entry
  stored before v36 may carry `NotAnImage`, which the server no longer
  answers.) A low disk
  stops the uploads and posts the message anyway, so the server's stored
  `DiskLow` refusal shows it and the outbox keeps moving. An upload sends
  `Expect: 100-continue` and holds its body until the server's `100 Continue`:
  a refusal the server answers before it asks for the body (`DiskLow`, or
  `TooLarge` from the `content-length`) then finds no body byte in flight, so
  a connection reset can't drop it. A HEAD or PUT that fails in transport
  backs off like the command's own request. After its
  images, the sender writes each file of the message into the workdir root as
  `<name>`: the file is uploaded as an attachment, then written with the
  workdir write route, with the file's `replace`, and only then is the message
  posted. The message text already names `./<name>`. A file whose device copy
  is missing, or no longer hashes to its name, fails the message as `Missing`,
  without a request. A workdir that already holds other content at the name
  fails it as `AttachmentRefused(FileExists)`, and the hash found there is kept
  on that file. The user can then resend it replacing that file
  (`ResendReplacing`): the same command is requeued with `replace` set to the
  hash found, and a file changed again meanwhile is refused again. An `Exists`
  or `Same` answer with our content counts as written, so a resend after a lost
  answer is safe. A `NotStaged` answer uploads the file once more; any other
  refusal fails the message as `FileNotWritten`, or as `ServerDiskLow` on a low
  disk. A third loop,
  the draft stager, runs beside the pull and the sender: it uploads a draft's
  images as soon as they are attached (woken when a draft gains images), HEAD
  then PUT, each image once per connection (a draft's files are not staged
  early: the sender uploads them at send). It only saves time, since the
  outbox still checks every image before posting; a transport error makes it
  wait a backoff and never ends the connection, and a refused image is left
  for the outbox to report when the message is sent. The user may
  take back any message, shell run, session creation, context move, or
  blob delete or crop the
  server has not accepted, sent or not (`OutboxEntry::discardable`): the
  row goes, the wait to resend it ends and the next command goes out. One
  already sent may have reached the server anyway; then its event shows it
  in the log like any other, and an answer that still comes settles
  nothing. Reconnects back off
  exponentially with jitter to 30 s. The attempt count starts over after a
  connection that made progress (a pull returned or the server settled a
  command); `hello` alone is not progress, so a server that answers it and
  then fails keeps backing off. `offline_since` starts over once `hello`
  succeeds. The wait ends by whichever of the
  monotonic and wall clocks says first, so a device that slept retries when
  the head says. `Reconnecting` carries why the last attempt failed
  (`LinkFailure`: unreachable, timed out, connection lost, handshake,
  server restarted, a server status, an unreadable reply, local storage),
  worded by `skywave-present::link`. A revoked device, a protocol
  mismatch, or an identity fault (see Pairing) is `Rejected`, and retries
  stop until the user acts: re-pairing, or a retry where a server-side fix
  may clear it (`LinkRejection::can_retry`: a protocol mismatch, a
  changed CA or name). `suspend()` / `resume()` pause the network
  for heads that background.
- **A log that went backwards.** The server's log can lose events the
  device holds: a crash dropping its last commits, the data dir restored
  from an older backup, or the database replaced. Pulling on would wait
  past a head that never comes (a head below the cursor) or, once the log
  grows again, stack another history's events onto the replica. The
  replica keeps the `LogId` and `RunId` of the last `hello` it took
  (`server_log`). Each connection's `hello` asks about that run
  (`?since=`) and is checked before anything is pulled or sent, against
  what the device has *seen*: its cursor, or the seq of any of its
  commands the server accepted beyond it. The server answers from its
  record of runs: `Unknown`, or `Known { next_start }`, the lowest head
  any later run started from. With something seen, the log lost history
  when `head` < seen; when its `LogId` differs (another database: the id
  is minted with it); when the run is `Unknown` (or unanswered); or when
  `next_start` < seen (equal is a clean restart). This is sound because
  the log is append-only: within one `LogId` only its end can be lost,
  and only while the server is down, so a run after the loss starts below
  it, and a restore also takes away the record of the runs after the
  backup. It holds however many restarts there were since, and however
  far the log has grown past the cursor again. An `Unknown` run is
  conservative: the record went back past it, and nothing says how far.
  A first `hello` (fresh or reset replica) compares only the head.
  `hello`'s head is the committed one (read from the database, not the
  in-memory watch, which is published after the commit), so it is never
  below an event a pull already got. The server id says nothing here: it
  lives outside the database, and a restored data dir keeps it.

  The check holds past the `hello`: once it passes, the link sets that run
  in its `ExpectedRun`, and every request names it (the `skywave-run`
  header). A server rolled back without closing the
  device's sockets (a VM snapshot restored, a hard reboot) leaves the
  long-poll hanging on a dead connection while the pool opens fresh ones
  for the next command; that command reaches the rolled-back run, which
  refuses it with 412 (`TransportError::RunChanged`) without handling it.
  The connection ends like any failure, the command stays unanswered
  (`Sending`, not given up on), and the next connection's `hello` decides:
  a lost log stops the link as above, a clean restart carries on and the
  command is sent again. The blob downloader and the side transport
  (listings, workdir reads) build transports of their own, but every
  transport the engine builds for one link (`Connector`) reads the run from
  that link's one `ExpectedRun`, as each request leaves. So a rolled-back
  run refuses their requests too, and one the link carried on from after a
  clean restart is what they name from then on, never the run before it.
  A refused blob is retried after a backoff like any retryable failure (by
  then the link has met the new run, or stopped on the lost log, which
  stops the downloader); a refused listing or workdir read fails with
  `RunChanged`. Each link start makes a new `ExpectedRun`, empty until its
  first `hello` passes, so a link to a newly paired server never names the
  old server's run.

  The link then stops with `Rejected(HistoryLost { have, server })`,
  writes nothing, and `RetryNow` does not get past it. The replica stays
  readable, as it is the only copy of what the server lost (messages the
  device saw `Accepted` included), so nothing is reset on its own and
  nothing is resent: the server's dedup record of those commands went with
  the log, so a resend would run them again, after side effects the agent
  may already have had on the workdir. `Action::ResetReplica` (only in
  that state) clears what the server's log produced, as
  `Core::reset_replica` does, in place: events, projections, cursor,
  outbox (queued commands included: they may name sessions the server no
  longer has), blobs, workdir answers, devices, offered models and the
  kept log and run; settings, drafts, composer modes, snippets, read marks,
  local deletes and alerted approvals stay. It then connects again and
  pulls the log from seq 0; the link shows `Connecting` only once the
  reset is stored and reloaded, so a reset that fails leaves it
  `Rejected(HistoryLost)` to be tried again. The GUI's banner offers "Reset local data"
  after a confirmation; the TUI says to restart with `--reset-replica`.
- **Blobs** (D6): `OnDemand` by default, or `Eager`. Downloaded beside the
  link by three workers (two take any blob, the third only blobs the user
  asked for, so a request never waits behind eager downloads), into
  `blobs/<id>.part`, resumed with `Range` from
  the recorded progress (decoded bytes, D45; each progress report follows an fsync, so a
  power loss never resumes past durable bytes), verified by size and by
  the `BlobRef`'s SHA-256 (the replica keeps it in `blob_hashes`), and
  renamed when complete. A blob already whole on disk (an empty one always
  is) is checked the same way without a request. A newly registered blob
  whose hash is an attachment this device holds (the image of its own
  message, D50) is not downloaded, under any policy: before the batch applies,
  outside the state lock, it is hard-linked (copied, and synced, where linking
  fails) from `attachments/` to `blobs/<id>.own`, but only for a blob the
  replica does not know yet (a short in-memory check under the state lock,
  taken only when some referenced blob is an own attachment); in the locked
  change that applies the batch it is renamed onto `blobs/<id>` and marked
  available, so a replica rebuilt from the log while those files exist reads
  them too. A
  `.own` file the batch did not use is removed afterwards. A hard link outlives
  the attachment's file, which only the next store open removes.
  A wrong hash deletes the file; the first
  one a worker sees while it holds the blob refetches it from zero at once (the
  bytes may be the device's own, kept from an earlier try), a second is
  the server's (every byte came from it since) and leaves the blob
  `Failed` until it is asked for again, as does a size mismatch, an
  unknown blob or a local file error. The replica keeps the failure and
  its reason (`blob_failures`), so a restart neither resumes it nor, under
  `Eager`, queues it; only asking for it tries again. A file write that
  finds the device's disk full (`ENOSPC`, or a quota), or a progress
  report the replica refuses with `SQLITE_FULL`, is no local file error:
  deleting the `.part` would only free room for the next eager blob to
  fill and lose in turn. The blob keeps its `.part` and recorded progress
  (still `Downloading`, or `NotDownloaded` if no report made it; a failed
  write records only what the synced file holds), waits at the front of
  the queue, and
  every download pauses (no worker takes a blob) until a download is
  asked for or the app restarts; nothing measures free space, and a disk
  still full pauses again at the first write. The queue reports the
  pause to the views under its lock: `BlobState::Downloading { paused:
  Some(Pause::DiskFull) }` (over `Offline`) and
  `SettingsView::blobs_disk_full`. `Link` ignores them
  (synced metadata counts as synced). Switching to `OnDemand` drops every
  download nobody asked for: waiting ones leave the queue, one in progress
  stops at its next chunk (or while its request waits), one backing off
  at once. Bytes over a bad network are dear, so a dropped blob keeps its
  `.part` and recorded progress as `BlobStatus::Stopped` (stored as
  `not_downloaded` with `got` and `path` set), which the views show as
  `NotDownloaded`; asking for it, or `Eager` again, resumes from there. The
  user stops one by hand with `Action::StopBlobDownload`, asked for or not and
  whatever the policy: it ends the same way, `Stopped` with its `.part`, and
  the device asks for it again on its own only when the policy is set to
  `Eager` (a restart under `Eager`, or switching back to it); captures and
  the shown workdir blob do not, until the user does (`DownloadBlob`, or a new workdir download of it). A
  restart queues `Downloading` rows, and `Stopped` ones only under
  `Eager`, so it never resumes under `OnDemand` what nobody asked for;
  only a kill between the
  switch and the stop leaves a row `Downloading`. The store keeps no
  "asked for" mark, so the restored policy decides how a `Downloading`
  row resumes: as asked for under `OnDemand` (only a request starts one
  there), as a background download under `Eager` (workdir blobs always as
  asked for), which a later switch to `OnDemand` stops; a user who asked
  before the restart asks again. A download the user asked for keeps
  going. Workdir blobs (diffs and file contents a
  stored workdir answer names) are never fetched eagerly, whatever the
  policy: only asking for one downloads it. Their ids recur (the server
  names content by hash), so a refresh can purge a blob a download holds
  and the next one register it again: each try reads the blob with a lease
  (a number for that registration, beside the replica under the same
  lock, never reused), and a report under a lease the blob no longer has
  fails as for an unknown blob. Purging a blob drops its lease, so leases
  are kept only for blobs in the replica that a download read, however
  many refreshes come and go. A request for a blob a
  download holds is kept and queued once it lets go. The core hands
  requests to the engine under the state lock, in the order of the changes
  that made them, so a purge never overtakes a later request for the same
  id. A change that fails part way still hands over the requests of the
  steps it committed before (`Shared::update`): an action whose later
  effect fails still reports the blobs an earlier local delete purged.

  A server delete or crop reaches every replica as the log's `BlobDeleted`
  or `BlobCropped`, whoever decided it. `apply_batch` keeps the blob's
  fate (`blob_fates`, a `BlobFate` by blob id: who deleted it, or the
  crop's `into`, kept spans and the old line count) and, in the same
  transaction, purges the old blob's row: its content is gone from the
  server, so a download of it could not finish, and the fate says what to
  show. Purging a blob also purges any blob a pending crop would have made
  it from. A crop's `into` registers like any referenced blob; a blob
  with a fate never registers again (so its lease is never needed to tell
  a new row apart). `Applied::purged` names the purged blobs, and the
  seam handles them as a workdir refresh does: leases dropped, files
  (complete and `.part`) removed, `Request::BlobsPurged` sent; a download
  holding one stops at its next report. `Applied::blobs`, the eager
  queue's input, leaves out what the same batch purged: a crop of a crop
  pulled in one batch by a device not holding the original registers and
  purges the middle blob there.

  Every purge (a batch's fates, a workdir refresh, a local session
  delete) commits the rows first and removes the files after, so a crash
  in between leaves files without a row. As the server does (Blob GC),
  the device sweeps them on startup, before the downloader runs
  (`BlobQueue::restore`, from the one read of every blob row, event and
  workdir alike): a row owns `blobs/<id>` when `Available` and
  `blobs/<id>.part` when `Downloading` or `Stopped`; every other `<id>`
  or `<id>.part` goes, a `.part` beside a `NotDownloaded` or `Failed`
  row included (no recorded progress points into it, so a try would
  truncate it anyway), and so does a `<id>.own` (the device's own image,
  which only a crash before its batch applied leaves). Any other name in
  `blobs/` is left alone and logged.

  **Local crop.** A crop of a blob this device *holds* (its row
  `available`, or itself the `into` of a pending crop: a crop of a crop)
  keeps the old row instead, and the device makes `into` from those bytes
  rather than fetching it, under any policy and offline too. The batch
  returns it in `Applied::local_crops` (a `PendingCrop { into, from }`)
  and leaves `into` out of `Applied::blobs`. The engine queues `into` at
  the front as a blob-queue job with the source `LocalCrop { from }`; a
  worker claims it as it would a download (so no eager download or
  request writes `blobs/<into>.part` meanwhile; and a worker claiming
  `into` however it was queued asks the replica for a pending crop and
  makes that first), crops `from`'s file into
  that `.part` on a blocking thread (`skywave_proto::crop`, the server's
  own function, so the bytes hash to `into.sha256`), verifies size and
  hash, renames, and marks `into` `Available` under its lease. Made or
  not, `Seam::local_crop_ended` then purges the old blob in one update,
  as above. A missing file, a mismatch or an I/O error deletes the `.part`
  and the same claim falls through to a fetch of `into` if it is wanted
  (asked for, or `Eager`) and has no fate of its own (the server no
  longer has a crop's middle blob); otherwise `into` stays
  `NotDownloaded`. An `into` purged meanwhile fails the lease check like
  any download (and its purge took the old blob too). A crop job waits
  while the blob it reads is queued or held, so a crop of a crop runs in
  log order. An eager worker backing off from a blob lets it go when a
  crop job is ready (it is queued again behind the crops), so a crop
  never waits on a link that is down. Pending
  crops are not stored on their own: a `Cropped` fate whose blob still
  has a row is one (`Store::pending_crops`, also in `Snapshot`), and the
  queue is restored with them at the front on startup, so an exit before
  the job ran neither leaves the old blob behind nor fetches `into`. The
  views never show the held blob's content: they resolve through the
  fate to `into`, `NotDownloaded` until the crop is made. The model folds the same fates from the
  batch, so it agrees with the store (and with the store after a
  resync). Fates go with the events (a reset) and with their session's
  local delete. The views resolve an event's blob through the fates, a
  crop of a crop step by step: deleted at any step is
  `FullView::Deleted { by }` (no blob, nothing to download); cropped is
  the last crop's blob with `BlobView::change = Cropped { kept, of, by }`,
  `kept` the lines the last crop kept, `of` the original's line count
  (the first crop's `lines`; every crop keeps lines of the original) and
  `by` the last crop's device. A shell run's `output` resolves the same
  (`ShellRun::output: Option<FullView>`).
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
- **Shell history** (D53): `ViewState::shell_history` holds this device's
  shell commands from every session, newest first, each once, at most
  `SHELL_HISTORY_MAX` (1000). `Store::load` rebuilds it from the log
  (`Store::shell_history`, in `Snapshot`). `apply_batch` keeps it current:
  each `ShellRequested` moves its command to the front, and events of a
  session deleted here are skipped. Only logged commands are in it, never
  the outbox's. Nothing is stored for it.
- **App screen** (D65): `Action::Screen` queues a `CommandKind::Screen`
  through the outbox, discardable while it is refused or unsent. A capture is
  not queued while a capture of the same device (a full one, for a full
  capture) is still unsent or unanswered in this device's outbox, so repeated
  presses offline run once. Only the server's `ScreenStale` at accept decides
  a stale input, so it never expires. `SessionView::app_screen`
  (`AppScreenView`) folds
  `ScreenRequested` and `ScreenDone`: the devices are the newest `Devices`
  listing, and each device's latest capture is its last `Done { Image }`
  (`Unchanged` keeps it). `ScreenDone::pruned` becomes `Deleted` blob fates,
  purged as `BlobDeleted` is. Each device's latest capture in the open
  session is requested for download whatever the blob policy
  (`Model::want_captures`). `skywave_present::app_screen` words the requests
  and builds an input only when it is aimed at the latest capture this device
  knows.
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
  `CreateSession { workdir, conversation, worktree }` (no worktree), which ends the picker, as do `EndConversations` and leaving the list.
  Nothing is stored: the listing goes stale at once.
- **Workdir snapshots** (`Screen::Workdir`): `OpenWorkdir` shows a session's
  changes; `ShowWorkdir` switches between the changes, one file's diff (a
  path and whether it is the untracked entry), a directory and a file; a
  commits pane shows the stored commit log, asked and refreshed like the
  changes. `RefreshWorkdir` reads the pane again (a diff: the changes). The fetch
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
  neither the replica nor the model. A transport error,
  `ChangesResult::Failed` or `LogResult::Failed` stays in memory and never
  replaces a stored answer; any other answer is stored, and the model learns the blobs it
  registered and purged in the same update. Back walks file → its
  directory, diff → changes, directory → its parent, and the changes, the
  commits or the workdir root → the session; a session deleted here falls back
  to the list last shown, the one back from the session returns to.
- **Workdir downloads and pushes** (D54, D56). `Action::DownloadWorkdirFile`
  asks for one file as a `WorkdirAsk::Download`, stored under its own kind so
  it never replaces a file view's answer; the stored `WorkdirAnswer::File`
  keeps its content as a workdir blob, which the model wants at once, off
  screen (`Model::want_blob`). An action while that download is in flight
  asks nothing. `WorkdirView.downloads` lists the session's downloads that are
  stored, in flight or failed, each with its `fetch`, `result` and `blob`.
  `Action::CancelWorkdirDownload` drops a download still asking the server (it shows as failed, `cancelled`; its late answer is stale) or stops its blob as `StopBlobDownload` does, keeping what it got.
  `Action::PushWorkdirFile { session_id, source, path, replace }` makes a
  `Request::PushWorkdirFile`, which the engine's `push_workdir_file` carries
  out: it hashes the device file where it is (not copied into the data dir)
  by a streaming read, HEADs `/v1/attachments/{sha256}` and, unless the server
  holds them, PUTs the bytes hashed, streamed from the file and resuming at
  the bytes the server holds, as the outbox does, so a push of up to 256 MiB
  holds one chunk in memory; a file changed between the hash and the PUT is
  the server's `HashMismatch`. Then it posts the write route.
  `WorkdirView.pushes`
  lists the session's pushes with a `PushState` (`Pushing`, `Failed`, or
  `Done(PushOutcome)`); a push the server answers `Exists` is
  `Done(Written(Exists { sha256, size }))`, the hash the GUI's Replace… resends
  with. A download's answer is stored like a read's; a push is held in
  memory only. `Action::CancelWorkdirPush` stops a push until its write is
  sent: the engine then answers `Done(Cancelled)`, nothing written, and the
  server keeps what was uploaded, so pushing again resumes. Once the write is
  sent the push ends as the server answers; the model marks nothing cancelled
  before the engine says so.
- **Usage screen**: every `TurnUsage` in the replica as a `TurnRecord` with
  the sessions they ran in; heads total them with `skywave_present::usage`
  by local day, session, workdir or model. A turn with no cost is unpriced,
  never $0. Archived sessions count; a session deleted from this device took
  its turns with it, and the screen says so.
- **Context fill and subagent figures** (D46): two separate numbers, never
  added. The session's `SessionUsage::context` is the newest `TurnUsage`'s
  `context_tokens` of `context_window`, set at each turn's end; a turn that
  reports none keeps it, and one marked `context_compacted` makes it
  unknown. `SessionUsage::fills` keeps each context's last fill, which a
  `ContextSwitched` restores; a `ContextStarted` context is unknown until
  its first turn reports one. Heads put it in the
  session footer as `skywave_present::format::context_label` (`ctx
  19k/1M · 2%`, `ctx ?`), among `session_usage_fields`. A
  `SubagentUsage` event folds into its call's `SubagentStatus::usage`, which
  `subagent_status` shows once the subagent finished (`finished · 1 step ·
  4.3s · ctx 12k`): its own context at its end, not what it spent. A
  stopped subagent has none.

- **Storage screen** (`Route::Storage`): opening it asks
  `GET /v1/storage` on the side transport within `STORAGE_TIMEOUT` (12 s,
  above the server's 10 s), and `RefreshStorage` asks again, as does this
  device's own `DeleteBlob` or `CropBlob` settling accepted while the
  screen is open (by its reply or, the reply lost, by the log).
  `StorageView` shows `Loading`, `Ready` or `Unavailable(Offline |
  Failed)`, the latest report (a failed refresh keeps the one before) and
  what the replica knows of each session it names (`StorageView::sessions`:
  its title, whether it is archived, and the two delete gates); a session
  missing there is not on this device. Only the latest request's answer is kept;
  offline nothing is asked. Nothing is stored: opening the screen drops
  the last report and asks again. The same asks also read this device's
  own report (`StorageView::device`, `store::read_device_storage`: the
  replica with its WAL, blob bytes on disk, attachment files, and each
  session's event and blob bytes), off the state lock on a read-only
  connection, and work offline too.
  `ReclaimServerSpace` (paired, online) asks `POST /v1/storage/reclaim`
  within `RECLAIM_TIMEOUT` (610 s, above the server's 600 s `VACUUM`) and
  shows `Reclaiming` until its answer; a refresh or a second reclaim asks
  nothing meanwhile. The server's `507` (no free disk for a reclaim) is
  `Unavailable(NoRoomToReclaim)`, the report before kept.
  `ReclaimDeviceSpace` (on this screen, no server needed) runs `VACUUM` and
  a WAL truncate on the `Store`'s own connection under the state lock, on a
  blocking thread, so dispatch waits for it rather than failing on the 5 s
  busy timeout; then this device's report is read again and replaces
  `StorageView::device`. `device_reclaiming` is true meanwhile: a refresh
  asks no device report and a second reclaim nothing. Opening the screen
  forgets a reclaim in flight; a failure keeps the report before it.
  `skywave_present::storage` words the
  totals and the three lists (by session, largest blobs, shell runs) for
  both heads. The by-session list joins the server's sessions with this
  device's (largest by both sizes), so it shows offline too; each row
  carries its `SessionFacts` for selecting it. A blob or run row may do
  (`RowActions`: delete
  any blob, crop tool or shell output of a session in this replica; a
  run acts on its `output_blob`). `skywave_present::blob_actions` holds
  the same rule for a timeline item's content and the confirmation
  texts. Either offers nothing while this device's own delete or crop of
  the blob is pending, refused included: `BlobView.pending` and
  `StorageView.pending` carry it as a `BlobPending` (command id, kind,
  delivery, `discardable`), so a head can take it back with `Discard`,
  after which the offers return. A storage row also offers nothing once
  the replica's log holds a fate for its blob (`StorageView.gone`: a
  delete or crop by any device), since the report may predate it: another
  device's delete pulled while the screen is open, or this device's own
  settled in the log before the refresh it asked for answers. The fate
  decides the row; nothing asks the server again for it.
- **Worktrees screen** (`Route::Worktrees`, D52): opening it asks
  `GET /v1/worktrees` within `WORKTREES_TIMEOUT` (20 s), and
  `RefreshWorktrees` asks again, as does this device's own `RemoveWorktree`
  settling accepted while the screen is open (by its reply or, the reply
  lost, by the log). Offline nothing is asked. `WorktreesView.status` is
  `Loading`, `Ready` or `Unavailable(Offline | Failed)`. The list stays
  during a refresh and after a failed one; `list` is the latest answer.
  `sessions` maps each listed worktree's path to the sessions whose workdir
  is that path, latest message first, each with its title, `archived`,
  `worktree` (made in a skywave worktree), `worktree_removed` and the
  delivery state of its latest `RemoveWorktree` (`removal`). A listed
  worktree no session runs in has no key. Nothing is stored: opening the
  screen drops the last list and asks again. `skywave_present::worktrees`
  words it for both heads. The GUI's hub (`gui/native/hub/src/screen.rs`)
  orders the page: a repository with a linked (non-main) worktree first, then
  one with only its main working tree, then gone or failed ones, each group in
  the server's path order. Inside a repository, linked worktrees with a
  non-archived session come first, then the other linked worktrees, and the
  main working tree last; git's order holds within each group. The hub splits
  each worktree's sessions into `WorktreeView.sessions` and `archived`, latest
  message first in both. The GUI folds the archived ones behind an "N archived"
  toggle, collapsed by default. Removal is per worktree card:
  `WorktreeView.remove` names the first removable session (a skywave worktree
  not yet removed, non-archived before archived), and `RemoveWorktree` for it
  is sent after a confirm. `removal_label` is the first removal on its way or
  failed among the worktree's sessions. The main working tree has none, and a
  worktree no session runs in can't be removed from the page. Tapping a session
  line sends `OpenSession`. The card leads with the branch, with the path under
  it.
- **Server addresses page** (`Route::ServerAddresses`,
  `Screen::ServerAddresses(ServerAddressesView)`): the address in use and
  the server's other addresses (see Pairing), from the replica's settings.
  `SetServerAddress` connects to a typed or listed one and
  `ForgetServerAddress` drops one (the GUI asks first). Back returns to the
  settings. The GUI opens it from the settings' server tile; the TUI changes
  the address from its settings (`a`) and never opens it.
- **Usage limits page** (`Route::UsageLimits`): the replica keeps the
  latest answer (`usage_limits`, one row, cleared with the replica), so
  the page reads offline with the time the agent reported it. Opening it
  asks `GET /v1/usage-limits` on the side transport within
  `USAGE_LIMITS_TIMEOUT` (25 s, above the server's 20 s) only when no
  answer is stored and none is on its way; `RefreshUsageLimits` asks
  again. `Seam::usage_limits_answered` takes only the latest request's
  answer: `Read` or `Unavailable` is stored, while a transport error or a
  `Failed` stays in memory as `UsageLimitsStatus::Unavailable(Offline |
  Failed)` beside the stored answer. Offline nothing is asked. Back
  returns to the session the page was opened from, else the session
  list. `skywave_present::limits` words a row per limit (label with its
  scope, percent, bar fill, warning, `resets in 2 h 13 min`), when the
  limits were read and why there is nothing to list; a figure the agent
  did not report is left out. The TUI opens it with `L` (session list,
  usage screen); the GUI from the usage page. From a session the limits
  show on its info page instead.
- **Approvals screen** (`Route::Approvals`, D63): every approval this device
  can answer, from every session, oldest first (`Model::waiting_approvals`).
  Each item's `Asks` is an `Approval` prompt, a `Question` (answered in its
  session, not here) or `Unknown`. Only the open session's events are in
  memory, so the model caches each waiting approval's facts
  (`waiting_facts`). `Effect::LoadApprovals(SessionId)` reads a session's
  events from the store only while this screen is shown, and only for a
  session with a pending approval it has no facts for; the seam asks again
  after each batch it applies and after a resync. A request missing from
  those events is cached `Unknown`, so it is not read again. Facts are cached
  raw: the input's blob state and this device's answer are read at view time.
  An answered approval leaves the list at once (`can_answer`). The TUI's
  screen only counts them.
- **Agent login** (`ViewState::login_expires`, on every screen): Settings
  always say when the agent's login expires, once paired
  (`skywave_present::login::expiry_label`: `expires in 12 days ·
  2026-11-21`, `expired · …`, `no expiry reported`; the TUI's `Agent
  login:` line, the GUI's `agent_login` tile). Within three days of the
  expiry, and once it has passed, the banner warns too
  (`login::expiry_warning`: `agent login expires in 2 days · log in again
  on the server`). Both count from the head's clock, a part of a day
  counting as a day, as the agent's own terminal counts. The TUI adds the
  warning to the link banner; the GUI's `LinkUpdate` carries it as
  `login_label`, shown in the banner even while online with nothing
  else to say, with a close button that hides it until Rust words it
  differently (the next day's count). The GUI says where to log in again
  (`login::expiry_warning_for`: `from settings` where the server offers a
  login from a device, `on the server` where not), and while the agent
  is logged out its `logged_out_label` (`login::logged_out_warning`:
  `agent not logged in · log in from settings`, or `… on the server`)
  shows the same way, in place of `login_label` and with no close (no expiry warning for a
  logged-out agent). The GUI's settings carry
  the login from a device as `login` (`AgentLoginView`: `can_log_in`
  whenever offered, so a login can be renewed early, the flow's state,
  its URL, `login::flow_lines` and what each login intent would do).
  The `agent_login` tile shows `Log in` (`Start again` once a login
  ended other than logged in) whenever `can_log_in`, and below it the
  GUI's `AgentLoginPanel` shows the flow on the Settings screen itself,
  so the core stays on Settings, the only place it acts on the login
  intents: the lines as Rust words them, the URL with copy and open in
  the browser, a field for the code with `Send code` while open,
  `Retry` for a code that got no answer, and `Close`
  (`DismissAgentLogin`). The pasted code lives in that widget's state
  only (no restoration id, no draft, no keyboard suggestions or IME
  learning). It is cleared once the login takes no code (sent, started
  again, ended) or shows another URL, but stays after a local refusal
  (blank, too long), and goes with the widget when Settings is left; leaving
  does not dismiss the login, so back on Settings it is still open. The
  TUI has no login UI yet.
  `FinishAgentLogin`'s pasted code travels in a `PastedCode` whose
  `Debug` is redacted, as core's `LoginCode` is, since the hub logs
  intents and actions with `Debug`.
- **Logging the agent in from a device** (`SettingsView::login`, a
  `LoginView`: `offered`, `logged_out` and `expires` as the last `hello`
  said, plus the `flow`): on the settings screen, `StartAgentLogin` posts
  `/v1/agent-login/start` and `FinishAgentLogin { code }` posts the
  pasted code with the flow the start opened, each on the side transport
  within `LOGIN_TIMEOUT` (25 s, above the server's 20 s). `login.rs` holds
  the flow, shaped like the usage limits: only the latest request's
  answer is taken. The flow id, its URL and the code live in model memory
  only, never in the store, a draft or the outbox, and the code never in
  a `Debug` (`LoginCode`, `LoginFinish`); a restart, `DismissAgentLogin`
  or pairing with another server forgets them. `LoginFlowView` is `Idle`,
  `Starting`, `Open { url, refused }`, `Finishing { url }`,
  `NoAnswer { url, why }` or `Done(LoggedIn | Busy | Unavailable | NoFlow
  | Offline | Failed)`. A finish that gets no answer (offline, or a
  retryable transport error: the server's 503, the timeout, a broken
  connection) is `NoAnswer` and keeps the flow and code: `RetryAgentLogin`
  resends both unchanged, and the server answers a repeated finish with
  the first one's outcome. A finish refused for good (a 4xx, an
  unreadable answer) is `Done(Failed)`. So is a `Failed` answer: the
  server sends it both for a mis-shaped code (its flow stays open) and
  for a code the agent refused (its flow is done, and every later finish
  gets the same `Failed`), and the wire does not tell them apart; the way
  on is `StartAgentLogin`, which joins a flow still open or opens a new
  one. `FinishAgentLogin` with no open flow sends nothing; a blank code
  or one over `LOGIN_CODE_MAX_BYTES` once trimmed is not sent and the
  flow stays `Open { refused: Some(LoginCodeRefused) }`. A start that gets no
  answer is `Done(Failed)`; starting again joins whatever the server
  opened. `Seam::login_finished` stores a `LoggedIn` as a `hello` would
  (`agent_logged_out` false, `login_expires` its `expires`) and shows it
  at once.
- **Session info page** (`Action::OpenSessionInfo { session_id }`,
  `Screen::SessionInfo(SessionInfoView)`): one session's particulars, all
  from the replica but the limits: its title and full workdir, the usage
  limits (`UsageLimitsView`, asked for on opening and refreshed exactly as
  on the usage limits page; `RefreshUsageLimits` works on either page),
  its `SessionUsage` (tokens in and out summed, the context fill, cost,
  time), its `TurnRecord`s (summed by model), the agent's model, effort
  and mode (`AgentView`), and every subagent it ran, in any context and
  nested ones too, in launch order, as `SubagentHeader`s folded from the
  timeline. The page keeps the session's events loaded as the session
  view does (`Model::loaded_session`: opened over the session it reuses
  them, else it loads them), so a subagent launched while it is on screen
  joins the list. Back returns to the session. `skywave_present::session_info`
  words the sections (Session, Usage, Agent, Models) and a row per
  subagent with its timeline status (`finished · 14 steps · 4.3s · ctx
  12k`). The GUI also shows the session id under the Session section, with
  a button that copies it. The TUI opens it with `I` on a session; the GUI
  from the usage summary on the status line and the session's More menu.
- **Crop screen** (`Action::OpenCrop { blob, session, holds, title }`,
  `Screen::Crop(CropView)`): chooses which lines of a tool's or shell
  run's output to keep. The head opening it names the blob as `CropBlob`
  will (a view's current blob) with its session, what it holds and the
  title to show; back returns to the session on screen, or to the storage
  screen (asked again) if it was opened there. Opening downloads nothing:
  `CropContent::NotHere(BlobState)` offers `DownloadBlob` and shows its
  progress. Once the blob is held the core asks `Request::CountLines`
  once (checked after every core update, so a download, a local crop or
  opening the page all lead to it); the engine runs `crop::count` on a
  blocking task and answers `Seam::lines_counted`, a stale `req` dropped.
  Meanwhile the page is `Counting`, then `Ready { path, lines }` (or
  `Unreadable` if the file could not be read). A fate for the blob, or
  its purge, makes it `Gone`. The selection is `SetCropKeep`, stored as
  given; `skywave_present::crop` normalises it, offers presets (first,
  last, first and last 50/200/1000) and says when the crop may be sent.
  A `CropBlob` of the page's blob, once queued, leaves the page as back
  does, so the user who cropped never sees it turn `Gone`.
  `skywave_present::blob_actions` turns a timeline entry, a shell run or
  a storage row into the blob to act on and the `OpenCrop` fields, so
  every head offers the same.
  The view holds no content: heads read the lines they show from `path`
  through `LineIndex`, numbered as a crop numbers them, and strip each
  line by itself (an OSC string can swallow a `\n`).

## Testing

- Tests are deterministic: a scripted fake agent (`skywaved/src/agent/fake.rs`,
  `tests/support/fake-claude.sh`) and recorded CLI fixtures. Only fixture
  recording and `skywave-claude-compat` run the real CLI. The Claude Code
  fixtures are recordings of compat scenarios (`--record`), so their stdin is
  what skywaved writes, `uuid`s included. A replay feeds the recorded stdout
  to the adapter and maps only the recorded `uuid`s to the ones it wrote.
- **Link harness** (`skywave-testkit`, D11): an in-process TCP proxy that
  injects latency, jitter, bandwidth caps and drops between real cores and a
  real `skywaved`. It asserts no lost commands, no duplicated events, cursor
  convergence, and two devices converging on the same state.
- `tracing` logs on both sides; the daemon logs to stderr or `log_file`,
  which `serve` reopens at its path on SIGHUP (for logrotate). An agent's
  stderr and stray stdout lines are all logged at warn, unthrottled: one
  dropped could be the one that explains a failure, and rotation bounds
  the file.

## Render states (owned by core, drawn by heads)

```rust
enum Link {
    Unpaired,                        // not paired (no pairing record): nothing to connect to
    Connecting,
    Online { latency_ms: u32 },
    CatchingUp { have: Seq, server: Seq },
    // wall-clock instants fixed at the failure; heads count down to retry_at
    // reason: why the attempt that started this wait failed
    Reconnecting { attempt: u32, retry_at: DateTime<Utc>, offline_since: DateTime<Utc>, reason: LinkFailure },
    // revoked cert, server CA changed, name mismatch, bad identity,
    // protocol mismatch, history lost
    Rejected(LinkRejection),
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
    BackgroundWork,                  // idle turn, live tasks or schedules (D51)
    AwaitingApproval(ApprovalId),
    Errored(ErrorInfo),
    Stopped,
}

enum BlobState {
    NotDownloaded { size: u64 },
    Requested { size: u64 },                         // asked for, waiting for a slot
    Downloading { got: u64, size: u64, paused: Option<Pause> }, // Offline | DiskFull
    Failed { size: u64, reason: String },            // given up; asking again retries
    Available { path: PathBuf },
}

enum CropContent {
    NotHere(BlobState),                // not on this device; opening downloads nothing
    Counting,                          // held; its lines are being counted
    Ready { path: PathBuf, lines: u32 },
    Unreadable { reason: String },     // held, but its file could not be read
    Gone,                              // deleted or cropped meanwhile, or purged
}
```

| Situation | Presentation |
|-----------|--------------|
| `Unpaired` | No banner; the pairing screen. |
| `Connecting`, `CatchingUp` | Render local replica immediately. TUI: banner. GUI: the app bar's status line (`LinkUpdate::status_label`: `Connecting…`, `Syncing · 4/9 · N queued`) in place of the page's subtitle, and a 2 dp progress line along the app bar's bottom edge; neither moves the page. |
| `Reconnecting` with queued items | TUI: banner: offline · why · N queued · retry in Xs. GUI: the status line (`Offline · retry in 07s · why · N queued`, countdown first so a narrow app bar cuts the reason), whose tap opens a sheet with the line in full and Retry now. Input stays enabled. |
| `Online` | Nothing, whatever is queued: a queued send says so on its own row. Settings shows the round trip. |
| `AwaitingApproval` while offline | Show request; the answer is queued like any command. |
| Approval answered elsewhere | "Approved on <device>", controls removed. |
| Question pending (`SessionView::pending_question`) | A question panel instead of the approval prompt: each question with its options, their descriptions and previews, and the user's own answer. The picks stay in the head (TUI: `skywave_present::question::QuestionDraft` behind `a`; GUI: the panel's state) until every question has an answer, then go as one `Action::AnswerQuestion`; `x` / Decline is a denial. Its call's row reads "needs an answer", then "answered here / on <device>" (`ApprovalStatus::Answered`), with what was picked in its result. |
| New approval while `Online` (`attention.alerts` grows) | Bell or desktop notification naming the session, once; never at start, after a catch-up that resolved it, or for one answered here. The GUI keeps one notification per session (ids from the hub, `PlatformEffect::Notify { id }`) and takes it down (`Dismiss`) once that session has no approval pending, however it went (`AlertUpdate::cleared`); a terminal bell has nothing to take down. |
| Long shell run of this device's finished, at least 10 s (`attention.shell_alerts` grows; `last_shell`), not while its session is open in shell mode | Notification naming the command, how it ended and the session, once per run, on the `shell` switch and the `activity` channel (D64). Read from the `ShellFinished` alone, so a catch-up's finish alerts too; heads count from 0. A terminal bell or OSC, as for approvals. |
| An agent went from working to idle (`attention.idle_alerts` grows; `attention.idle` lists the sessions) | Notification `agent idle · <title>`, once per session, on the `idle` switch and the `activity` channel (D64). Only work this device saw, online or busy at start; stopped, errored and deleted sessions never ring. Taken down (`Dismiss`) once the session works again or is gone. In memory only. The TUI has none yet (D64). |
| Approvals pending in other sessions (`attention.elsewhere`) | TUI: banner: N approvals elsewhere. GUI: a bell badged with N among every page's app bar actions. A tap opens the Approvals page, which answers them all; Ctrl+G opens the next one instead. |
| `Rejected(Revoked \| ServerChanged \| NameMismatch \| BadIdentity)` (`needs_repair`) | Full-screen state saying which, with re-pair action (and retry where `can_retry`); local data stays readable. |
| `Rejected(HistoryLost)` | Banner: server lost history (had N, server at M) · sync stopped; local data stays readable. GUI: "Reset local data" after a confirmation; TUI: restart with `--reset-replica`. In the GUI the banner shows only a rejected link and the agent login warnings (`LinkUpdate::label` is empty otherwise). |
| Blob `NotDownloaded` | Placeholder with size and a download action. |
| Blob `Downloading` with `paused` | Its progress, then `format::pause_note`: "paused, offline" or "paused, device disk full". |
| `FullView::Deleted { by }` | The preview, and under it "Deleted by <device>" (`sections::deleted_note`); no download action. |
| Blob with `change: Cropped` | Its state as any blob's, and beside it "Cropped by <device>: kept K of M lines" (`sections::change_note`). Every count of lines left out ("+N lines", "N earlier lines") counts the K kept lines, not the original's M (`sections::whole_lines`, `output::output_lines`). |
| Crop page `NotHere` / `Counting` / `Ready` / `Gone` | Not here: says the output isn't on this device, with its size and one Download action, then the blob's progress or failure. Counting: a counting note. Ready: the lines from `LineIndex`, presets, "keeps K of M lines", and Crop only while `crop::crop_enabled`, after `crop_confirmation`. Gone: nothing to crop, back only. |

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

- **Signals** are per feature: `LinkUpdate`, `ScreenUpdate`, `AppScreenUpdate`, `SearchUpdate`
  replace whole; the timeline and the shell runs (`ShellUpdate`, keyed by run
  id) are a `Snapshot` then keyed `Upserted`/`Removed` diffs; `PlatformEffect` is a command Dart carries out and forgets
  (notification, clipboard, opening a file, a snackbar). The contract is
  `gui/native/hub/src/signals/*.rs`; the Dart side is generated by `rinf gen`.
- **App screen** (D65): opened from a session's More menu (`App screen`). The
  GUI keeps the timeline unmounted while it is open. The hub sends
  `AppScreenUpdate` only when it differs from the last one sent, and again on
  `AppReady`.
- **Workdir text** travels in its own signal, `WorkdirTextUpdate`, and only
  for the diff or file pane shown: the hub reads the head of its downloaded
  blob once per blob (1 MiB or 20,000 lines, lower than the TUI's 4 MiB
  because every line crosses to Dart), with a diff's line kinds from
  `diff_lines`. It is sent once per blob, not with each `ScreenUpdate`,
  which is re-sent on every transfer tick; the screen carries only the
  blob's facts and the cut note. "Open" (`OpenFull`) opens the whole file.
- **Crop lines** never travel whole. The crop page asks for a window
  (`CropLines`, at most 1,000 lines) as its virtualised list scrolls; the
  hub opens the content's `LineIndex` once per page and file, reads the
  window once, and sends it (`CropLinesUpdate`, each line stripped and
  marked kept) again when the window or the selection changes. A line
  toggle (`CropToggle`) is turned into `SetCropKeep` by the hub (present's
  `crop::toggle`, the rule the TUI's `v` range uses too), from the
  selection it sent last, so two taps faster than a snapshot do not lose
  one. Timeline rows, shell runs and storage rows carry a
  `BlobTargetView` (present's `BlobTarget`, worded), so Dart's delete and
  crop menu decides nothing; every delete or crop goes through an
  `AlertDialog` with present's question first. On the storage page the
  sessions' ticks are Dart-local state; each bulk action (archive, remove
  from this device, delete everywhere) acts only on the ticked rows whose
  flags (`here`/`archived`, `deletable`, `deletableEverywhere`) allow it,
  and the two deletes ask first.
- **Agent messages are markdown** (D48). The hub parses an agent message's
  text, live or imported, into flat blocks (present's `markdown::blocks`)
  sent as `TimelineRow.markdown` beside the raw `text`; an expanded long
  message's whole comes the same way on its section
  (`SectionView.markdown`). Dart lays the blocks out and decides nothing:
  a row or section with search marks shows its raw text, where the marks'
  ranges point. Copy takes the raw text. Only `http`, `https` and `mailto`
  links are tappable, and open (`url_launcher`) after a dialog showing the
  URL; images are never fetched.
- **Images are prepared off the hub task.** An attach intent makes the head
  emit `Out::Prepare`; `run` prepares the picked or pasted image on a
  blocking thread (`attach::prepare_image`: read, `attachment::prepare`,
  `attachment::store`). Its result returns to the loop as
  `Head::on_prepared`, which dispatches `Action::Attach` with the metadata
  only. The head counts each session's preparations in flight: a send waits
  for them, and the image cap counts the draft's images and those still
  being prepared.
- **Attachments** (D50, D57). The composer's "Attach file" menu entry picks
  any file: `file_selector`'s open dialog on desktop, and on Android and iOS
  a sheet with the `image_picker` gallery, the camera, or the phone's `File`
  option. Each picked path is sent as `GuiIntent::AttachFile`, and the hub
  sniffs its first bytes: an image goes through the preparation above; any
  other file is copied as it is and attached under its name. A second file of
  a name the draft holds is refused with a notice ("a file named … is already
  attached"), since the core keeps one per name. Desktop drop (`desktop_drop`)
  sends one `AttachFile` per dropped path, of any kind. Desktop paste
  (Ctrl/Cmd+V) reads a clipboard image with `pasteboard`, which wins over text
  (text alone is a normal paste), and mobile keyboard insertion (the
  TextField's `contentInsertionConfiguration`) sends bytes
  (`AttachImageBytes`), as paste does; each image goes through the preparation
  above. The draft's files show as chips with a ✕ above the field. A queued
  message lists its files; one already in the workdir is marked "already in the
  workdir", and "Replace & resend" for it asks first. The timeline shows an
  image as a thumbnail (160 px high) above the text; while it loads, or until
  it is downloaded, a placeholder shows its size. A tap downloads it
  (`DownloadBlob`) or, once loaded, opens it (`OpenFull`, or the local file
  for a message still queued).
- **Workdir download and push** (D54, D56). A file row in a directory listing
  and the file pane's app bar carry Download
  (`ActionIntentDownloadWorkdirFile`).
  The downloads panel shows each download: its progress, Cancel while it is
  under way, Download to resume a stopped one, Retry when it failed, and Save…
  once the file is on the device. Save… is the system save sheet on
  Android and iOS (`flutter_file_dialog`), and `file_selector`'s
  `getSaveLocation` on desktop, which copies the file
  (`gui/lib/workdir/saving.dart`). The directory pane's push action picks a
  device file (`pickPushFile`), proposes `<shown dir>/<file name>` in a target
  dialog, and sends `ActionIntentPushWorkdirFile`. The pushes panel shows each
  push's one line of progress or outcome, Cancel while it pushes, Retry when it
  failed or was cancelled, and Replace…
  when the path holds another file: asked first, then pushed again with the
  hash found. A written file's timeline row reads `<path> written by <who>`,
  or `replaced` when it replaced one.
- **One hub task** owns the head's state and runs one `select!` loop over the
  intents, the core's snapshots, dispatch results and the policies' next
  wake-up. `dispatch` runs on one dedicated thread, in the order sent.
  A panic in the hub task is caught around its future (`catch_unwind` per
  poll): the core is closed (queued actions applied, no stop-time draft
  save), Dart gets `HubFailed` with the panic's message and shows the boot
  failure page with Restart, which sends `Boot` and opens the core afresh
  with a new head. A panic in `dispatch` is caught on the dispatch thread,
  which reports it and ends (nothing queued after it is applied); the hub
  fails the same way. A panic part way through opening the core closes
  what was built before the slot is released, so Restart does not find the
  data dir in use. `HubFailed` carries an epoch, and Dart answers it at
  once with `HubFailedSeen` on the intent channel: the hub drops every
  intent queued before that marker, as meant for the head that failed.
  A panic on the core's own threads (any task on its engine runtime,
  `skywave-deliver` outside a callback) reaches the hub through
  `Core::on_failure`, and the hub fails the same way. A failure off screen
  while the Android service holds the link would otherwise go unseen
  under a notification claiming the link is open: the hub reopens the
  core itself with the last `Boot`, answering `Booted` unasked, and the
  new head starts with the failed head's holders (off screen, service
  held). Only once per absence: a second failure before the app is back
  on screen stops the service and posts a "Skywave stopped" notification
  (taken down by the next successful boot), and the Restart page waits.
- **Boot:** Dart sends its application support directory and device name;
  the hub opens the core in `<dir>/skywave`. The GUI is its own device with
  its own data (D25). A hot restart reruns Rust's `main` and reopens the
  core behind `CORE_SLOT` (D27).
- **Lifecycle:** Dart reports `AppBackgrounded`/`AppForegrounded`, and
  Kotlin `BackgroundService { running }` once the foreground service started
  or stopped. Two things hold the link: the app on screen and the running
  service. On Android and iOS the hub suspends the core when nothing holds
  it, and resumes it then retries the link when something holds it again
  (D21's rule); a report that changes no holder does nothing. Coming on
  screen while the service held the link only retries, to cut short a
  backoff wait. With Stay connected on (Android only; the hub keeps its
  preferences, this one, the notification switches (D64) and the session
  list layout, in
  `<app support>/skywave-gui.json`), the hub asks Dart to
  start the service on the toggle, on `AppForegrounded` and on `AppReady`,
  and only Kotlin's `running` report sets the service holder; switched
  off, the service holds until its `stopped` report. Desktop never
  suspends. Every platform saves the
  open session's unsaved draft and marks it read at once on leaving, and
  again whenever the hub task stops, before the core closes. On Android the
  app owns the Flutter engine (`MainActivity.engine`), so Dart and the core
  outlive the Activity: a detach does not close the core, which only
  desktop's exit request does. When a new Activity attaches to the live
  engine, Dart reports `AppForegrounded` and checks whether a notification
  tap launched it.
- **Notifications** (D64). The hub's `PlatformEffect::Notify` carries a
  `NotifyKind` (`Approval`, `Shell`, `Idle`, `Stopped`), and Dart picks its
  channel in `gui/lib/effects/notifications.dart`: `Approval` and `Stopped` on
  `approvals` (high importance), `Shell` and `Idle` on `activity` ("Agent
  activity", default importance). Ids are `slot * 4 + kind` in `notify.rs`
  (approval 0, shell 1, idle 2), one per kind per session, so a `Dismiss`
  takes down only its kind. A notice of kind K posts only if `all && K` in
  `Prefs.notifications` (`NotificationPrefs`, all on by default); dismissals,
  the watchers and the hub-stopped notice go on regardless.
  `GuiIntent::SetNotifications` carries the whole set, and `Head::prefs()`
  saves every preference with it.
- **Settings → Notifications** (`gui/lib/settings/notifications_page.dart`):
  four switches, Notifications, Approvals, Agent idle and Long shell runs; the
  three kinds are disabled while the first is off. No core route: the page is
  Flutter-local, a page in `ScreenNavigator`'s pages list, opened by the
  Settings tile through `Holders.notificationsOpen` and closed when the screen
  leaves Settings. Back closes it without leaving Settings.
- **Escape** (GUI): Escape is system back (`Back`, or closing the
  notifications page) wherever a page can go back; it never leaves the app,
  and the session page's own Esc (closing search, leaving the composer)
  comes first.
- **Approve and deny** (GUI): on the session page Ctrl+Shift+A approves and
  Ctrl+Shift+D denies the approval on screen (with none, Ctrl+Shift+D
  declines the question); the buttons' tooltips name them. A held key
  answers once; the Approvals page binds no key, as it shows several.
- **Platforms:** Linux and Android verified; macOS, iOS and Windows
  scaffolded only; no web (D26).
- **Shell history picker** (D53). The hub owns its state. In shell mode
  `HistorySearchOpen` opens it (Ctrl-R toggles it; the input menu's "Search
  history" opens it too), and the composer text is the query. The hub
  re-filters on every `DraftEdited` with
  `skywave_present::policy::search_history`, and the selection goes back to
  the newest row. `HistorySearchMove`, `HistorySearchAccept` (fills the
  field, caret at the end, never runs it), `HistorySearchPick` (a row tap)
  and `HistorySearchClose` (Esc, keeps the text) are the other intents;
  `ToggleShellMode` and `ComposerSubmit` close it.
  `ComposerView::history_search` carries at most 100 rows, the selected row,
  a `matching/total` count and an empty label, each row's matched chars as
  `Utf16Range`s. Dart only
  draws it: an inline list at the top of the composer, above its shell bar
  (the field keeps focus), newest row at the bottom as in fzf, matched chars
  highlighted. Up/Down and Ctrl-P/N move while it is open, Enter and Tab
  accept, Esc closes.
