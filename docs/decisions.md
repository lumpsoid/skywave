# Decisions

Each entry: decision, reason, and what is deferred. MVP scope unless noted.

## D1. Rust everywhere except the Android head
`proto`, `skywaved`, `core`, `core-ffi`, `tui` are Rust. The Android head is
Kotlin and talks to the core through UniFFI.
**Why:** one implementation of sync, storage, and protocol logic.

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

## D8. Approvals wait indefinitely
The agent stays blocked until any device answers.
**Deferred:** configurable timeout / auto-deny / pre-approved tool list.

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
- `Instant` → unix-epoch millis (`i64`), converted against `SystemTime::now()`
  at conversion time
- `PathBuf` → `String`

Mirrors are plain data. If a head would need logic to render something
(formatting, counting, deciding visibility), that logic goes into core's
`ViewState`, not into the mirror or Kotlin (D2).
**Why:** keeps the core free of FFI concerns and its types idiomatic Rust; the
conversion layer is the one place the boundary is visible.

## D17. One core per process, pushed to Kotlin as a `StateFlow`
- The Android `Application` opens and owns the single `SkywaveCore` for the
  process lifetime; activities and view models never open their own.
- `ViewState` reaches Kotlin through a Kotlin-implemented callback interface
  passed to `subscribe`. The core has no delivery thread: the listener runs
  on whichever thread delivers — the caller's own inside `subscribe`,
  `dispatch` and `unsubscribe` (on Android, the IO coroutine that consumes
  actions), or an engine thread. So it must be thread-safe and must not
  block: it only hands the snapshot to a `MutableStateFlow`, which is
  thread-safe and conflates (the core may already skip snapshots, so
  conflation loses nothing).
- The device name is passed at open (`EngineConfig::device_name`):
  `Settings.Global.DEVICE_NAME` if set, else `Build.MODEL`. The core's
  host-name default is meaningless on Android.

## D18. Kotlin + Jetpack Compose, Material 3, single Activity
- Navigation is driven by `ViewState.screen`: the core owns the route, the
  Activity renders whatever screen it is told. System back maps to
  `Action::Navigate`; no Jetpack Navigation graph.
- Manual dependency injection (the `Application` holds the core and its
  repository). Hilt/Dagger only if a later task shows a clear need.
**Why:** a second source of navigation truth in the head would violate D2.

## D19. Android build: minSdk 26, arm64-v8a + x86_64, cargo-ndk from Gradle
- `minSdk` 26; `compileSdk` / `targetSdk` the current stable SDK.
- ABIs: `arm64-v8a` (devices) and `x86_64` (emulator). No 32-bit ABIs.
- Gradle runs `cargo-ndk` to build `skywave-core-ffi` into `jniLibs`.
- Kotlin bindings are generated at build time by the crate's own
  `uniffi-bindgen` binary (version-locked to the crate's `uniffi`), never
  checked in.
**Deferred:** armeabi-v7a / x86, if a real device needs them.

## D20. QR scanning without Google Play Services
Pairing scans the `skywaved pair` QR (D9) with `zxing-android-embedded`
(ZXing core plus its own camera handling). No ML Kit / Play Services
dependency.
**Why:** works on de-Googled devices and emulators; one small dependency.
CameraX + ZXing core stays the fallback if the embedded library's camera
handling proves inadequate.

## D21. Background: suspend on stop, resume on start
The head calls `suspend()` when the app goes to the background
(`ProcessLifecycleOwner` `ON_STOP`) and `resume()` on `ON_START`. Actions
taken while suspended still persist and queue (D7).
**Open (A15):** how a new approval reaches the user while the app is in the
background. Options:
- periodic WorkManager catch-up (resume, pull, notify, suspend);
- opt-in foreground service keeping the link up;
- both (WorkManager by default, foreground service as a setting).

## D22. Android device key in app-private `filesDir`
MVP per D9: the core's data dir, including the device key, lives under
`Context.filesDir`, which only the app can read.
**Deferred (A19):** Keystore-backed signer via an FFI callback.
