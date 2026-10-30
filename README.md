# Skywave

**Drive your coding agent from anywhere, even over a terrible connection.**

Skywave is a remote client for coding agents (Claude Code first). The agent
runs on your server, next to your code. You talk to it from a terminal on any
device: a laptop on a train, a phone tethered through one bar of signal, a
hotel Wi-Fi that drops every five minutes. Typing, reading history and queuing
work never wait on the network, and nothing you send is ever lost or sent
twice.

```
 ┌──────────── your server ───────────┐            ┌──────── your device ────────┐
 │  claude (the agent)                │            │  skywave-tui                │
 │     ▲                              │  mTLS,     │     ▲                       │
 │  skywaved                          │◄──────────►│  skywave-core               │
 │   · runs and supervises the agent  │  small,    │   · full local replica      │
 │   · append-only event log          │  resumable │   · outbox for your sends   │
 │   · private CA + device registry   │  payloads  │   · sync, blobs, link state │
 └────────────────────────────────────┘            └─────────────────────────────┘
```

---

## Why Skywave exists

Coding agents work best on a real machine: your repos, your toolchain, your
GPU, your secrets. *You* don't always sit at that machine. The usual answer
is SSH plus `tmux`, which works fine until the link gets bad:

- **Every keystroke is a round trip.** At 800 ms of jitter, typing a prompt
  becomes painful.
- **A dropped connection loses context.** You reconnect and scroll back to
  work out what happened, if the session survived at all.
- **The agent stalls on you.** It asks for permission, you're offline, and
  nothing happens until you're back on that exact terminal.
- **Big outputs eat your bandwidth.** A 2 MB test log scrolls through your
  connection whether you wanted to read it or not.

Skywave treats a bad network as normal, not as a failure case. The server
keeps the truth, the device keeps a full copy, and the two sync small,
resumable pieces whenever the link allows.

## How it helps you

### ✍️ You never wait for the network
Everything you do is written locally first. Compose a message in a tunnel and
hit Enter: it shows `Queued`, then `Sending`, then `Accepted` once the link
comes back. Exactly one copy reaches the agent, because every command carries
an id and the server deduplicates retries.

### 🔁 Drops cost you nothing
The server keeps one append-only event log with a contiguous sequence number.
Your device remembers how far it has read. After a drop it picks up from that
point, with no gaps and no duplicates, however long you were gone.

### 🧊 Cold start shows everything at once
Quit the TUI, open it offline days later, and the full history of every
session is there right away, read from the local replica. Syncing catches up
quietly in the background.

### 🛂 Approve from any device
When the agent asks permission to run a tool, it waits for you with no
timeout. Approve or deny from whichever paired device you have. Other devices
see "resolved on \<device\>". Answering while offline works too: the answer is
queued like any other command.

### 📦 Bandwidth for what you read
Large tool inputs and outputs become **blobs**: you get a short preview in the timeline
and fetch the full content only when you open it. Downloads resume where they
stopped. Prefer everything up front on a good link? Switch the blob policy
from `OnDemand` to `Eager`.

### 📡 You always know where you stand
A link banner is always on screen: `Online · 120 ms`, `Catching up`, or
`Offline · 3 queued · retry in 08s`. Input stays enabled in every state.

### 🔐 Private by default
`skywaved` runs its own certificate authority. Devices pair with a one-time
code and a pinned fingerprint (shown as a QR code too), then use mutual TLS
for every request. No public CA, no third-party relay, no account. Revoke a
lost laptop with one command and its next request is refused.

### 🖥️ Many devices, one view
Pair as many devices as you like with one server. Sessions created on one
show up on all of them, in the same order.

---

## Quick start

> **Status:** pre-release. The server, sync core and TUI are in place and
> pass end-to-end tests under a simulated bad link; the full acceptance pass
> against real Claude Code is still in progress.

Requires Rust 1.89+ and, on the server, the [`claude`](https://docs.anthropic.com/en/docs/claude-code)
CLI on `PATH`.

```sh
cargo build --release
```

### 1. Start the server

```toml
# ~/.config/skywave/skywaved.toml (every key is optional)
listen = "0.0.0.0:7443"
roots = ["/home/me/code"]            # where sessions may run
server_names = ["devbox.example.net"] # names in the server certificate
```

```sh
skywaved serve
```

The first run creates the CA, the server certificate and the database.

### 2. Pair a device

On the server:

```sh
skywaved pair            # prints code, address, fingerprint and a QR code
```

On your device:

```sh
skywave-tui
```

Enter the address, code and fingerprint, or paste the `skywave://pair?...`
URI to fill all three. Done: the device holds its own certificate and trusts
only your server.

### 3. Work

Press `n` to create a session in a directory under one of your `roots`
(`Tab` completes the path from the server, or from recent workdirs while
offline), then start talking to the agent. Each session is titled by the
first line of its first message until you name it: `r` on the session list
(or `R` on the timeline) renames it on every device, and an empty name brings
the default title back.

`a` on the session list archives the selected session on every device, and
`A` switches between the active and the archived sessions (`a` there brings
one back). An archived session keeps running, opens and works as usual, and
a message to it from any device brings it back; its pending approvals still
alert and still count for `Ctrl-G`.

`D` on the archived list deletes the selected session from this device only,
after a y/N question: its events, blobs, draft and read mark go, freeing
their space. It stays on the server and on other devices, and never comes
back here, not even when it is used elsewhere. It cannot be deleted while it
has a pending approval, a running turn or a command from this device still
unanswered.

When the agent reports them, each turn ends with its time and cost, the
session's status line shows the last turn's time, tokens and cost, the
session's cost and the model, and the session list shows each session's cost.

| Key | Session view |
|-----|--------------|
| `i` / `c` | Focus the composer |
| `Enter` | Send (`Shift`/`Alt`+`Enter` or `Ctrl-J` for a newline) |
| `Tab` | Switch between composer and timeline |
| `j` / `k`, `PgUp` / `PgDn` | Move through the timeline |
| `Enter` / `Space` | Expand a tool output |
| `d` | Download a blob |
| `y` / `Y` | Copy the selected output, diff, file or message / its command |
| `a` / `x` | Approve / deny a pending request |
| `A` | Approve and always allow a rule the agent offers (timeline); with several, pick one with `↑↓` `Enter` |
| `Ctrl-X` | Interrupt the agent's turn |
| `S` | Stop the agent (timeline); the next message restarts it |
| `R` | Rename the session (timeline); empty restores the default title |
| `Ctrl-R` | Retry the connection now while offline (any screen) |
| `Ctrl-G` | Open the next session waiting for an approval (any screen) |
| `?` / `F1` | Help |

Copies go to your local clipboard through the terminal (OSC 52), so they work
over SSH. Inside tmux, enable `set -g set-clipboard on`. Content over 74 KiB is
not copied (the notice names the blob's file); `SKYWAVE_OSC52_MAX` sets another
limit in bytes.

When an agent asks for an approval the TUI rings the terminal bell, once per
new batch of requests. `--notify` (or `SKYWAVE_NOTIFY`) picks how: `off`, or
any of `bell`, `osc9`, `osc777` separated by commas; `osc9` / `osc777` send a
desktop notification naming the session (never the tool's input). Inside tmux
those two need `set -g allow-passthrough on`; the bell always works.

### Managing devices

```sh
skywaved devices list
skywaved devices revoke <id>
```

Admin commands work on the database directly, so `serve` can keep running.

### Approval alerts on Android

The Android app posts one notification per approval (tap it to open the
session), including when it is in the background or not running:

- **By default**, the app stops its connection when you leave it and checks
  in about every 15 minutes while a network is up (WorkManager, so Android
  batches the wake-up with other apps'). Each check takes seconds: it
  connects, catches up, posts what is new and disconnects. In Doze Android
  may stretch the interval.
- **Settings → Stay connected** keeps the connection open while the app is in
  the background, so alerts arrive within seconds while agents work. It shows
  an ongoing "Staying connected" notification (on its own low-importance
  "Connection" channel, which you can silence) and costs more battery: the
  phone holds one long-poll to your server open. It keeps running when you
  swipe the app away; after a reboot it comes back the next time you open the
  app. Turn it off in the same place.

---

## Under the hood

| Crate | Role |
|-------|------|
| `skywaved` | Server daemon: agent supervisor, event log, blob store, CA, mTLS HTTPS API, admin CLI |
| `skywave-core` | All client logic: local replica, outbox, sync engine, blob downloads, view state |
| `skywave-tui` | Terminal head built on ratatui; renders the core's view state and nothing else |
| `skywave-proto` | Versioned, agent-agnostic wire types |
| `skywave-core-ffi` | UniFFI bindings the Android head (`android/`) builds against |
| `skywave-testkit` | Link harness: a proxy that injects latency, jitter, bandwidth caps and drops |

A few design choices that make it hold up:

- **The server log is the source of truth.** Devices pull with a long-poll
  from their cursor and apply each batch in one transaction.
- **Commands are idempotent.** Client-generated ids mean a resend after
  reconnect is always safe.
- **The agent is behind an adapter.** Nothing outside one module in
  `skywaved` names Claude Code, so adding other agents doesn't touch the
  protocol or the clients.
- **It's tested on a bad link.** The sync tests run the real core against the
  real daemon through the link harness and check for lost commands,
  duplicated events, cursor convergence and two devices converging on the
  same state.

More detail in [`docs/architecture.md`](docs/architecture.md),
[`docs/mvp-spec.md`](docs/mvp-spec.md) and
[`docs/decisions.md`](docs/decisions.md).

### Android build

The Android head lives in [`android/`](android), a Gradle project (Kotlin,
Jetpack Compose, Material 3) that builds the Rust core as part of the app
build.

Prerequisites:

- JDK 17 or newer.
- The Android SDK with the platform the app compiles against (API 37). Gradle
  finds it through `sdk.dir` in `android/local.properties` or `ANDROID_HOME`.
- Rust (`cargo` on `PATH`, or `skywave.cargo=/path/to/cargo` in
  `android/local.properties`) with the Android targets:
  `rustup target add aarch64-linux-android x86_64-linux-android`.
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk):
  `cargo install cargo-ndk`.
- An Android NDK. `cargo-ndk` builds the `.so`, and Gradle tells it which NDK
  to use (as `ANDROID_NDK_HOME`). The first of these wins:
  1. `ANDROID_NDK_HOME` in the environment;
  2. `skywave.ndk=/path/to/ndk` in `android/local.properties`;
  3. the NDK pinned by `ndkVersion` in `android/app/build.gradle.kts`
     (currently `28.2.13676358`), looked up under the SDK's `ndk/` directory.

  No NDK environment variable is needed, so a build started from Android
  Studio's desktop entry works. Install the pinned NDK with
  `sdkmanager "ndk;28.2.13676358"`, or override it with option 1 or 2. Any
  recent NDK (r26+) works. The `cargoBuildAndroid<Variant>` log prints the
  NDK it used (`Using NDK ...`). Don't use `ndk.dir` for this: AGP reads that
  key itself and rejects an NDK that doesn't match `ndkVersion`.

Build and install:

```sh
cd android
./gradlew :app:assembleDebug    # app/build/outputs/apk/debug/app-debug.apk
./gradlew :app:installDebug     # onto the connected device or emulator
```

Each variant runs two tasks before compiling:

- `cargoBuildAndroid<Variant>` runs `crates/skywave-core-ffi/build-android.sh`,
  which builds `libskywave_core_ffi.so` for `arm64-v8a` and `x86_64` (API 26).
  Debug uses cargo's `dev` profile; release uses the size-optimised
  `release-android` profile.
- `generateUniffiBindings<Variant>` runs
  `crates/skywave-core-ffi/gen-kotlin.sh`, which generates the Kotlin
  bindings (package `dev.skywave.core`) with the crate's own `uniffi-bindgen`.
  They are build output and never checked in.

Both tasks are skipped when nothing under `crates/`, `Cargo.toml` or
`Cargo.lock` has changed. The scripts also work on their own, e.g.
`crates/skywave-core-ffi/build-android.sh <dir>/jniLibs`.

## License

Licensed under either of [MIT](https://opensource.org/licenses/MIT) or
[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0), at your option.
