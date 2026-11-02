# Decisions

Each entry: decision, reason, and what is deferred. MVP scope unless noted.

## D1. Rust everywhere except the Android head
`proto`, `skywaved`, `core`, `core-ffi`, `tui` are Rust. The Android head is
Kotlin and talks to the core through UniFFI.
**Why:** one implementation of sync, storage, and protocol logic.
**Superseded by D28:** the Kotlin head is gone; every head is Rust or a
Flutter head over a Rust hub.

## D2. Heads only render
All logic lives in `core`. The TUI is a head like any other and gets no
shortcuts around `dispatch` / `subscribe`.

## D3. Agent-agnostic
No type, message, or API outside the agent adapter names a concrete agent
(`AgentWorking`, not `ClaudeWorking`). Claude Code is the first adapter,
driven via its stream-json CLI mode.
**Why:** the adapter isolates the agent's protocol; swapping or adding agents
touches only `skywaved`.

## D4. Own wire protocol
Clients never see the agent's native protocol. `skywaved` translates it into
`proto` events.

## D5. One server, one account, many devices
Every device is a full view of the same data.
- Approvals: first answer wins; others receive `ApprovalResolved { by: DeviceId }`.
- Drafts are per-device.
- Read marks (what this device has seen of each session, behind the unread
  counts and the "new" divider) are per-device too and never synced.
**Deferred:** multiple servers per client.

## D6. Full local replica on every client
Not platform-specific.
- **Synced** means metadata synced. Blobs (tool outputs, file contents, logs)
  are fetched separately.
- Blob policy knob: `Eager` (download alongside metadata) or `OnDemand`
  (download on explicit user intent).
- Retention: forever; user can delete sessions locally (they stay on the server).
**Deferred:** retention policies, storage limits.

## D7. Reconnect and resend
On link loss: reconnect with backoff, resume pull from last `seq`, resend
queued commands. Client-generated command ids make resend idempotent.

## D8. Approvals wait indefinitely while the agent session lives
The agent stays blocked until any device answers. A daemon restart cancels
the approvals still pending (`ApprovalCancelled { reason: "daemon
restarted" }`): the agent's request lives only in its process, over stdio,
and `--resume` restores the history, not an in-flight turn
(docs/agents/claude-code.md §5).
**Deferred:** configurable timeout / auto-deny / pre-approved tool list.
**Deferred:** an agent host process that survives `skywaved` restarts, the
only way to keep a pending approval across one.

## D9. Identity: mTLS with a server-owned CA
- `skywaved` creates its CA and server certificate on first run.
- Pairing: `skywaved pair` prints a one-time code and the server cert
  fingerprint (text + QR). The device generates its keypair locally, sends a
  CSR with the code, receives a signed device certificate, and pins the
  server certificate.
- Every connection is checked against the CA **and** the device registry,
  so revocation works.
- Android MVP: device key in app-private storage.
  **Deferred:** Keystore-backed signer via an FFI callback; the core's
  `Identity` abstraction must allow it.

## D10. Admin via daemon subcommands
Pairing and device management are `skywaved` subcommands, not head features:
`skywaved pair`, `skywaved devices list`, `skywaved devices revoke <id>`.

## D11. Risk focus is the link
The agent side on the server is not a major risk. Engineering effort goes
into the client–server link: jitter, low bandwidth, drops.

## D12. One global event log per server
A single monotonic `seq` across all sessions; each event carries its
`session_id`. A device keeps one cursor and one long-poll.
**Why:** one account per server (D5), so a global log costs nothing, and
sessions created on another device arrive through the same stream.
Seqs are contiguous (single writer), so an event batch carries its first seq
once (`from`) and events carry no seq on the wire.
**Status:** provisional — chosen until real usage shows we need something else.

## D13. MVP = `skywaved` + `skywave-core` + `skywave-tui`
The Android head is the first post-MVP milestone. The core is designed for it
from day one (D2), but no Kotlin ships in the MVP.

## D14. Sessions run in a server directory under configured roots
`CreateSession` names a working directory on the server. `skywaved` rejects
paths outside the `roots` list in its config.

## D15. Long-poll over HTTPS for the MVP
`GET /v1/events?after=&wait=` returns a batch or times out empty. Survives
proxies and the worst links; SSE/QUIC can replace it behind `Transport`.

## D16. UniFFI in proc-macro mode; mirror types live in `skywave-core-ffi`
No UDL file. `skywave-core-ffi` declares its own `#[uniffi::Record]` /
`#[uniffi::Enum]` / `#[uniffi::Object]` mirrors of `Action`, `ViewState` and
friends, with `From` / `TryFrom` conversions to and from the core types.
`skywave-core` never depends on `uniffi`.
Type mapping across the boundary:
- ids (`SessionId`, `ApprovalId`, command ids, ...) → `String`
- `Seq` → `u64`
- `Duration` → millis (`u64`)
- `DateTime<Utc>` → unix-epoch millis (`i64`)
- `PathBuf` → `String`
- draft cursor (code points in core) → `cursor_utf16` (`u64`, UTF-16 code
  units, what Kotlin `TextRange` indexes), converted against the draft
  text both ways; a unit offset inside a surrogate pair rounds down, past
  the end clamps. The core and the TUI keep counting code points.

No `Instant` crosses the boundary: a monotonic instant can only be shown as
wall time by reading a clock at conversion, which makes every snapshot of
the same value differ, and `CLOCK_MONOTONIC` stops while Android sleeps. A
view field that is a moment holds a `DateTime<Utc>` fixed when the core
learned of it (e.g. `Link::Reconnecting { retry_at, offline_since }`), and a
head counts against its own wall clock from there.

Mirrors are plain data. If a head would need logic to render something
(formatting, counting, deciding visibility), that logic goes into core's
`ViewState`, not into the mirror or Kotlin (D2).
**Why:** keeps the core free of FFI concerns and its types idiomatic Rust; the
conversion layer is the one place the boundary is visible.
**Superseded by D28:** `skywave-core-ffi` is removed. The rules about
`Instant` and plain-data mirrors still describe how the hub maps for Dart.

## D17. One core per process, pushed to Kotlin as a `StateFlow`
- The Android `Application` opens and owns the single `SkywaveCore` for the
  process lifetime; activities and view models never open their own.
- `ViewState` reaches Kotlin through a Kotlin-implemented callback interface
  passed to `subscribe`. The core owns one delivery thread
  (`skywave-deliver`) that renders the latest snapshot and runs every
  listener, one at a time; `dispatch` only persists the action and returns,
  so a busy catch-up never holds the IO coroutine that consumes actions
  behind snapshot conversions. The listener must not block: it only hands
  the snapshot to a `MutableStateFlow`, which is thread-safe and conflates
  (the core may already skip snapshots, so conflation loses nothing).
- The device name is passed at open (`EngineConfig::device_name`):
  `Settings.Global.DEVICE_NAME` if set, else `Build.MODEL`. The core's
  host-name default is meaningless on Android.
**Superseded by D28** for the Kotlin parts. One core per process still
holds (D25, D27).

## D18. Kotlin + Jetpack Compose, Material 3, single Activity
- Navigation is driven by `ViewState.screen`: the core owns the route, the
  Activity renders whatever screen it is told. System back maps to
  `Action::Navigate`; no Jetpack Navigation graph.
- Manual dependency injection (the `Application` holds the core and its
  repository). Hilt/Dagger only if a later task shows a clear need.
**Why:** a second source of navigation truth in the head would violate D2.
**Superseded by D28.**

## D19. Android build: minSdk 26, arm64-v8a + x86_64, cargo-ndk from Gradle
- `minSdk` 26; `compileSdk` / `targetSdk` the current stable SDK.
- ABIs: `arm64-v8a` (devices) and `x86_64` (emulator). No 32-bit ABIs.
- Gradle runs `cargo-ndk` to build `skywave-core-ffi` into `jniLibs`.
- Kotlin bindings are generated at build time by the crate's own
  `uniffi-bindgen` binary (version-locked to the crate's `uniffi`), never
  checked in.
**Deferred:** armeabi-v7a / x86, if a real device needs them.
**Superseded by D28:** the Flutter head builds through cargokit (D26).

## D20. QR scanning without Google Play Services
Pairing scans the `skywaved pair` QR (D9) with `zxing-android-embedded`
(ZXing core plus its own camera handling). No ML Kit / Play Services
dependency.
**Why:** works on de-Googled devices and emulators; one small dependency.
CameraX + ZXing core stays the fallback if the embedded library's camera
handling proves inadequate.
**Superseded by D28, then restored:** the Flutter head scans with the same
library, launched from `MainActivity` over the `dev.skywave.gui/scan`
method channel; the scanned text goes to the hub as `PairScanned`.

## D21. Background: suspend on stop, resume on start
The head calls `suspend()` when the app goes to the background
(`ProcessLifecycleOwner` `ON_STOP`) and `resume()` on `ON_START`. Actions
taken while suspended still persist and queue (D7).
**Resolved (A15, 2026-10-30): both.** One owner (`CoreAwake`) decides:
the link runs while any holder needs it (the visible app, a catch-up, the
service) and the core is suspended once none does.
- By default, a periodic WorkManager catch-up every 15 minutes (the
  minimum) with a network constraint, scheduled once paired: it resumes
  the core, waits up to 60 s for `Online` reached after the resume (or
  `Unpaired`/`Rejected`), gives the approval notifier 2 s to post, and lets
  go. A timeout is not retried: the next period is the retry.
- Opt-in "Stay connected" in Settings, a head-local preference (no rule in
  it, so not in `SettingsView`; D2): a foreground service of type
  `specialUse` (API 34+; `dataSync` is capped at 6 h a day on API 35+ and
  does not describe a standing connection) that holds the link up so
  alerts arrive within seconds, with an ongoing low-importance
  "Connection" notification. Started only from the foreground (the toggle,
  or the app coming to the foreground with it on); not restarted on boot.
- No wakelocks beyond WorkManager's and no polling in Kotlin: the core's
  long-poll is the only traffic. The core still decides which approvals
  are new; the notifier posts one notification per approval.
**Superseded by D28** for the Kotlin implementation. The hub's
suspend-on-leave follows the first paragraph; the A15 design above is the
reference for the Flutter head's deferred background work (D24).

## D22. Android device key in app-private `filesDir`
MVP per D9: the core's data dir, including the device key, lives under
`Context.filesDir`, which only the app can read.
**Deferred (A19):** Keystore-backed signer via an FFI callback.
**Superseded by D28:** the Flutter head keeps the key in its application
support dir, also app-private (D25); A19 still applies to it.

## D23. Head-side presentation and policies live in `skywave-present`
What a head decides beyond `ViewState` (timeline sections and which item is
selectable or discardable, neutral wording for bytes, durations, failures
and rejections, UTF-16 cursor conversion, and the clock-free policies:
draft debounce, read marks, approval alerts, prompt history, completion,
timeline search) lives in one crate above `skywave-core`. The TUI and the
Flutter hub both import it; each keeps only what is purely UI (key maps,
key hints, geometry, colours).
- Policies never read the clock: they take `Instant` and return the actions
  due plus their next wake-up, so they test without timers.
- It has no rinf and no UI framework. The TUI is the proof: if the GUI
  needed something the TUI could not use, it would have leaked.
- D16's rule ("logic goes into core's `ViewState`, not into the mirror")
  is about the FFI mirror gaining logic, and still holds there. Present is
  a separate crate, so the spirit holds: one Rust owner per rule.
**Why:** reimplementing in Dart would make another copy after the TUI, and
break "heads only render" (D2). The hub must stay pure mapping,
so it cannot own the logic either. Core's `ViewState` is the wrong home for
head wording ("press d").

## D24. Flutter head over rinf; Flutter only renders
The GUI head (`gui/`) is Flutter, bridged to Rust by rinf 8.11. Its Rust
side, `hub`, owns the head's state and does the mapping; Dart holds one
holder per signal and performs platform effects with one plugin call each.
- **Intents:** one `GuiIntent` DartSignal enum. rinf orders signals only
  within a type, and order matters: an edit before the `Back` that saves
  it, a typed query before the step through it.
- **Timeline:** sent as a `Snapshot`, then `Upserted`/`Removed` diffs keyed
  by item (`s:<seq>`, `p:<command id>`). The core pushes the whole
  `ViewState` on every change; resending a long timeline per keystroke
  would rebuild all of it in Dart.
- **Dispatch:** one dedicated thread takes actions from a FIFO queue.
  `dispatch` blocks on SQLite, so it cannot run on the hub task, and
  `spawn_blocking` would not keep the order the policies rely on (a draft
  saved before the send that follows it).
- Leaving the app is reported, not decided: Dart sends
  `AppBackgrounded`/`AppForegrounded`, and the hub suspends and resumes the
  core on Android and iOS only.
**Why:** one owner per rule (D2, D23) and a bridge thin enough to read;
rinf's message passing keeps Rust's state off the Dart side.
**Deferred:** Android background work (A15's service and periodic
catch-up), notifications while suspended and deep links. QR scanning
returned on Android (D20).

## D25. The GUI has its own data dir and pairs as its own device
The hub opens the core in `<application support dir>/skywave`, which Dart
resolves with `path_provider`, and pairs under the host name (desktop) or
platform name (mobile). On desktop too it never shares the TUI's data.
**Why:** two processes driving one replica is outside D17 (one core per
process): two engines would race on the cursor and the outbox. Separate
devices is what the server already models (D5). On Android the app id is
`dev.skywave.gui`.

## D26. GUI platforms: Linux and Android verified, Apple and Windows scaffolded, no web
- **Built and run:** Linux desktop and Android.
- **Scaffolded, never built:** macOS, iOS, Windows. Their platform folders,
  cargokit wiring, entitlements, local-network usage string and
  notification setup are in place, but no toolchain was at hand, so nothing
  says they build.
- **Web: not supported.** `skywave-core` cannot target wasm32: it needs
  tokio's multi-threaded runtime, `std::thread`, TCP sockets, bundled
  SQLite (C), `std::time::Instant` (panics on wasm32), and ring and
  getrandom built without their wasm backends. A web head would need a fetch- or WebSocket-based `Transport`,
  IndexedDB or OPFS storage, and no threads: its own project. `gui/` has no
  `web/` folder, so nothing suggests support that is not there.

## D27. A hot restart reopens the core, one at a time
rinf 8.11 reruns the whole Rust `main` on a Flutter hot restart, while the
old runtime is still shutting down. The hub closes the core on
`dart_shutdown`, and a process-wide `CORE_SLOT` lock makes the new
runtime's open wait until the old core has closed.
**Why:** D17 allows one core per process; two engines on one data dir
would race. Reopening costs a cold start from the replica, which is the
normal path anyway. Within one runtime, `AppReady` asks the hub to resend
everything, so a rebuilt widget tree starts whole.

## D28. Flutter is the only Android head; the Kotlin head is removed
`android/` (Kotlin, Compose) and `skywave-core-ffi` (its UniFFI bindings)
are deleted, with the `release-android` profile and the `uniffi`
dependency. The Flutter head (`gui/`, `dev.skywave.gui`) is the Android
app. The acceptance rig in `scripts/android-acceptance/` targets it.
**Why:** two Android heads meant every head-facing change landed twice, and
the Kotlin one kept its own presentation outside `skywave-present` (D23).
One head over the shared hub keeps one owner per rule (D2).
**Lost until the Flutter head gains them (D24's deferred list):** periodic
catch-up and "Stay connected" in the background (D21), approval
notifications while suspended and `skywave://pair` links. QR scanning
(D20) is back. Git history keeps the Kotlin implementation as a reference.

## D29. Session contexts: sequential, stamped by the server
A Skywave session owns an ordered list of contexts, each its own agent
conversation with its own resume handle. Contexts are sequential: one is
current and at most one agent process runs. `ClearContext` starts a new,
empty context; `SwitchContext` makes an earlier one current again; the
supervisor treats either as a restart of the agent in that context. The
server stamps every event with the context it belongs to (`Event.context`)
when it logs it, so clients draw dividers, list contexts and filter the
timeline from the stamp, never from position. A session's first context has
the session's id, for new sessions and migrated ones alike. The id of a
later context is chosen by the client that clears, so an offline device can
address it (a message queued after the clear names it) before the server
answers. Names are two random words carried in the log: the server names the
first context in `SessionCreated`, the clearing client names the rest; a
blank name is replaced, never refused, because a refusal would cascade into a
`ContextChanged` refusal of the message the same device queued next.
"New context" always creates one, even when the current context is empty;
clients disable it while the current context has no message, and the picker
hides empty contexts other than the current one.
Grants (rules the agent was granted) are session-scoped and kept apart from
the per-context resume handle: rules are documented as granted for the
Skywave session, and clients list them session-wide.
**Why:** sequential contexts keep one agent per session and one ordering
owner (the log); stamping at append time keeps the fold trivial and correct
across switches (an `Interrupt` accepted after a switch but before the old
turn ended is stamped with the old turn's context).
**Lost on upgrade:** rules granted before the upgrade, since an opaque
handle cannot be split by a migration (alpha stance). Known consequence:
`SessionView::rules` folds from the log's `ApprovalResolved { rule }`, so
clients keep listing pre-upgrade rules the agent no longer has until they
are granted again. Pre-upgrade sessions have one context named "start".
**Rejected:** server-assigned context numbers (an offline device could not
address the new context); names derived from the id (locks the word lists
forever); reusing an empty current context (would need a fake event or a
cascading rejection).

## D30. Moving a context: one command, two events, only the last names it
`MoveContext { session_id, context, name }` continues a context of the
session that has a conversation and is not current as a new session in the
source's stored workdir. The server mints the new session's id and logs, in
one transaction, `SessionCreated { workdir, context_name: name, from:
Some(ContextRef { session, context }) }` in the new session (stamped with
its first context, naming no command), then `ContextMoved { to, by }` in the
source (stamped with the moved context, naming the command, at its
`Accepted { seq }`). The moved context stays in the source, never current
or started again (`SwitchContext` to it is `ContextMoved`); the new
session's first context records it as its `origin` and forks its
conversation on first start.
**Why two events:** clients fold each session from its own events only, so
both sessions need one. **Why only the last names the command:** the
settling rule is "the event naming the command is at its `Accepted` seq".
With exactly one such event and it the last the command appends, a device
that settles from the log already has the earlier `SessionCreated`, since
batches are contiguous. So "every accepted command logs exactly one event"
becomes "exactly one event naming it, the last it appends";
`logged_by_command` is per event (`SessionCreated` with a `from` is the
daemon's).
**Rejected:** both events naming the command (a pull split between them
would settle at the first seq while the source does not yet show the
move); the command id on `SessionCreated` with `ContextMoved` logged by the
daemon after (settles before the source shows the move); one event with
derivation across sessions (clients would fold a session from another's
events); `CreateSession { workdir, from }` (another command's validation
and events).
**Moving the current context** (decided 2026-11-01): the server refuses it
(`ContextCurrent`). A client sends `ClearContext` then `MoveContext`, so the
source goes on in a fresh empty context as after "New context"; switching
back to the previous context instead was rejected.
**Not carried:** the new session starts with the agent's defaults for
mode, model and effort (carrying them is deferred), and with no grants
(D29: grants belong to one session, never shared).
**Blocked on a client** while that device has unsent or unpulled
`SendMessage`s to the context: the clear right behind them would interrupt
the turn they start, and the fork would carry an interrupted answer.
**Deferred:** copying a context into a new session (it would reuse
`SessionCreated { from }`, `origin` and the fork, add a sibling
`ContextCopied { to, by }`, and must fork eagerly or snapshot: a claude
handle is a session id, not a point in time, so a lazily forked copy would
include the source's later turns); carrying grants, mode, model or effort;
showing the origin's history inline in the new session; naming the new
session after the source; a client-chosen id for the new session, so an
offline device could open it before the server answers; an agent
capability that lets clients hide the move for agents that cannot fork
(today such an adapter fails `start` with `AgentError::Start`); moving
into an existing session or merging; moving a new session's first context
again before it has had a turn (`NoConversation`); a different workdir for
the new session (blocked: resuming from another cwd is untested); and a
reason in the core for why a context cannot move, so heads can tell "the
agent never ran in it" from "a message of yours is on its way".

## D31. Background work: level sets, per-task stop, best-effort schedules
The agent's work outside a turn reaches the log as two whole-set events per
session: `BackgroundTasksChanged { tasks }` and `SchedulesChanged
{ schedules }`. Both are agent-agnostic (D3: `TaskKind` is `Shell`,
`Subagent`, `Other`; a schedule's `when` is the agent's own words, never
parsed). The last event wins, server and clients fold them with the one rule
`AgentWork::apply`. Neither is notable or activity. `Exited` and `recover()`
empty both sets, durable schedules too.
**Why whole sets:** a missed event cannot wedge a set, since the next one
replaces it and process exit empties it. **Rejected:** edge events
(`TaskStarted` / `TaskEnded`: a lost end leaves a task live forever, and the
keep-alive with it), and one combined event (a schedule change would restate
the tasks, and the two come from different CLI signals at different times).
**Bounds:** `TASKS_MAX` 32, `SCHEDULES_MAX` 16, the first N kept; an id over
`TASK_ID_MAX_CHARS` is dropped, not cut (a cut id names another task). The
keep-alive counts the uncut set, so a bound never lets the agent idle out
under live work. **Rejected:** keeping durable schedules across `Exited`
(nothing would clear them).
**Stopping a task.** `StopTask { session_id, task }` is validated against the
server's fold (`UnknownTask`) and logs `TaskStopRequested`. It never starts
an agent: with none running it is refused ("agent not running",
`TaskStopRefused`). Success is settled only by the level (the task leaves the
set); **rejected:** a `TaskStopped` ack, which would be a second truth. Every
stop the agent is asked gets exactly one answer, so the supervisor's FIFO of
asks stays aligned with the adapter's answers; stops left over at exit are
dropped. A refusal is `TaskStopRefused { answers }`.
**Schedules are information only** (user decision, 2026-11-01): they do not
keep the agent alive, have no limit, and Skywave cannot cancel one; the agent
ends one when asked. The CLI offers no way, and a stop only **pauses** a
loop: session-only jobs come back after `--resume`. So stopping the agent
(Stop agent, new context, switch, move of the current context) asks for
confirmation in both heads whenever non-ambient tasks run or a schedule
exists, with the shared `skywave-present::format::stop_warning`, which says
background tasks end and loops are paused, not ended. There is no always-on
indicator (user decision); the work shows in a place the user opens.
**Subagents.** A subagent's own messages are dropped from the main timeline.
A background subagent's approval stays open past the main turn, tied to its
task, and is cancelled when the task leaves the set.
**`perTaskStopAffordance`.** The adapter declares it in `initialize`: an
interrupt then aborts the turn and spares background agents, instead of the
CLI killing them. It is a promise that the consumer can stop a task on its
own, true because both heads render a per-task stop.
**Deferred:**
- cancelling a schedule from Skywave by any means (revisit only if a CLI adds
  a control request);
- an always-on indicator;
- a subagent transcript view (needs `parent` on `ToolCall` / `AgentMessage`
  and `forwardSubagentText`);
- task output (`output_file`, `summary`, `usage`);
- backgrounding a foreground task (`background_tasks` control request,
  Ctrl+B);
- durable schedules across sessions (listing them while no agent runs;
  reading `.claude/scheduled_tasks.json` would break D3), moot while the CLI
  ignores `durable`;
- push/desktop notifications when background work finishes;
- a per-task stop for tasks inside a subagent beyond what `stop_task` allows;
- waking a stopped agent for a pending durable schedule;
- showing a stop refusal's reason in the heads (core `task_stops` drops a
  refused stop silently; `TaskStopRefused` has no timeline entry);
- GUI sheet polish: the Stop button is not disabled while a stop is pending,
  and the contexts dialog's confirm uses the warning as of when it opened.

## D32. Adopting an outside agent conversation
`CreateSession { workdir, conversation: Some(id) }` creates a session that
continues a conversation the agent had outside Skywave, in `workdir`. The
server logs one `SessionCreated { workdir, from: Some(Agent { conversation
}) }` (named by the command, settled at its own `Accepted { seq }`). The
first context stores `from` as its `origin`: one JSON `SessionOrigin`
column for a move and an adoption alike, so a context cannot continue both.
On that context's first start the supervisor passes
`Conversation::Adopt(id)`; the Claude adapter forks it (`--resume <id>
--fork-session --session-id <new>`), so the outside transcript is never
written. Precedence at a start: the context's own handle (`Resume`), else
its origin (`Fork` for a moved context, `Adopt` for an outside
conversation), else `Fresh`. The own handle stays first, or every restart
would re-fork and lose Skywave's turns; until the copy reports a handle (its first message)
every start adopts again under a new id.
**Why fork, not resume** (confirmed by the user 2026-11-02): a terminal
`claude` may still be running the session, two CLI writers on one
transcript is untested, and nothing tells skywaved whether it is alive. A
fork keeps every stored handle one skywaved minted and reuses the move
path. The cost: the terminal and Skywave diverge, and handing back needs the
fork's id, which nothing shows yet. The fork reads the outside conversation
as saved at fork time.
**Why no existence check:** the log's transaction is synchronous, the
EventLog has no adapter, and the transcript is the CLI's internal storage.
The agent settles it at first start: an unknown id errors each message
(`Errored`) and leaves the session in place for the user to archive. The
server checks only the shape (`BadConversation`: blank, over 128 chars,
whitespace or control characters) and never cuts or rewrites the id. Core
does not check it either: `dispatch` has no failure channel and the GUI
closes its dialog on any accepted dispatch, so a silent drop would lose the
input; a refused id is a failed, discardable pending creation.
**Rejected:** adopt by resume (a nicer hand-back, but concurrent writers and
a server-written, adapter-shaped handle breaking D3: only the adapter knows
a handle's shape); a new `AdoptSession` command (extending `CreateSession`
fits instead: D30 rejected `CreateSession { workdir, from }` for moves
because a move has another command's validation and two events, and
adoption has neither); a second optional field next to `from`, since a
session continues at most one thing, so one `SessionOrigin` enum
(`Context` | `Agent`; `logged_by_command()` is true for `None` and `Agent`).
Both heads adopt by pasting the id (decided by the user 2026-11-02): TUI
`c`, the GUI dialog's second field. An adopted session's first timeline
item is a divider, `Entry::Adopted`.
**Not carried:** mode, model, effort and grants (as D30). The outside
conversation's cumulative spend shows in the fork's `total_cost_usd`;
per-turn cost is unaffected. An adopted first context that never ran
(cleared before its first message) cannot be moved (`NoConversation`).
**Deferred:** showing the outside history (a best-effort parse of
`~/.claude/projects/*.jsonl`); listing adoptable conversations on the server
or picking from a list in a head; handing a conversation back to the
terminal (showing the fork's CLI id); adopt by resume; checking existence at
command time; adopting into an existing session as a new context; carrying
mode, model, effort or grants; an agent capability that lets clients hide
adoption for agents that cannot fork (today such an adapter must fail
`Adopt` with `AgentError::Start`); migrating old stored `from` events
(alpha: such databases are deleted).
