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
**Superseded by D28:** the Flutter head has no QR scanning yet (D24); the
no-Play-Services constraint still applies when it gets one.

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
catch-up), notifications while suspended, deep links and QR scanning.

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
notifications while suspended, `skywave://pair` links and QR scanning
(D20). Git history keeps the Kotlin implementation as a reference.
