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
from `OnDemand` to `Eager`. Workdir diffs and files still download only when
you open them.

### 📂 See the workdir
Check what the agent actually changed without a shell on the server. Open a
session's workdir to see its net git changes, with a diff per file, or browse
its files. It is read-only and fetched only when you ask, and what you read
stays on the device, so you can still read it offline.

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
# ~/.config/skywave/skywaved.toml, or --config (every key is optional;
# relative paths resolve against this file's directory)
listen = "0.0.0.0:7443"              # default 127.0.0.1:7443
roots = ["/home/me/code"]            # where sessions may run
server_names = ["devbox.example.net"] # names in the server certificate
# data_dir = "/srv/skywave"          # default $XDG_DATA_HOME/skywave
# log_file = "/var/log/skywaved.log" # default stderr

[agent]
command = "claude"                   # default: claude on PATH
args = []
# allow_bypass = true                # let devices switch a session to bypass
                                     # mode (every tool runs unasked); off by default

[shell]                 # shell commands from devices; off by default
enabled = true
program = "/bin/sh"     # any POSIX sh
# rc = "/home/me/.config/skywave/shellrc" # sourced before every command
output_max = "64MiB"    # output kept per command; the rest is dropped
```

```sh
skywaved serve
```

The first run creates the CA, the server certificate and the database.
In a container, run it under an init (`docker run --init`, or `tini`):
`skywaved` reaps its own agents, and on Linux each shell command runs under
a small host process that reaps that command's orphans, but nothing reaps
the other orphans reparented to PID 1.

> **Shell commands run as the daemon's user.** With `[shell] enabled`, any
> paired device can run arbitrary commands on the server, and the `roots`
> do not confine `cd`. That is no more than an agent in bypass mode can do,
> but it is why the shell is off by default. Each command is a one-shot
> `sh -c` with no terminal, so a prompt for a password fails rather than
> waits. When the shell exits, everything still in its process group
> (`cmd &` included) is killed; a process that escaped the group (`setsid`,
> a daemonizing tool) keeps running and is not tracked. With `program` set
> to bash, aliases from `rc` need `shopt -s expand_aliases` in it.

> **Bypass mode is the operator's call.** Without `[agent] allow_bypass`,
> the heads don't offer bypass and the server refuses a switch to it. With
> it, any paired device can turn off every prompt of a session after one
> extra confirmation; sessions still start in their usual mode. It is the
> only switch: `agent.args` that would enable bypass (such as
> `--dangerously-skip-permissions`) are refused at startup.

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

`c` on the session list continues a Claude Code conversation you started
outside Skywave: it lists the conversations the server's `claude` keeps under
your `roots`, newest first, and Enter continues the highlighted one in a new
session (`o` opens the session already running one, `p` pastes an id
instead: the directory `claude` ran in, then the session id). The GUI's New
session dialog has the id as an optional field. The session starts with the
earlier history the server could read, imported once on every device, dimmed
and marked "imported", and never unread; Skywave works on a copy (a fork), so
the terminal's session is untouched and the two diverge.

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
| `Ctrl-X` | Interrupt the agent's turn; in shell mode, stop the running shell command (else the newest waiting one) |
| `Ctrl-T` | Switch the composer between the agent and shell commands (only when the server runs them) |
| `Ctrl-O` | Shell mode: run the command only if the previous run succeeded, then only if it failed, then always |
| `S` | Stop the agent (timeline); the next message restarts it |
| `R` | Rename the session (timeline); empty restores the default title |
| `Ctrl-R` | Retry the connection now while offline (any screen) |
| `Ctrl-G` | Open the next session waiting for an approval (any screen) |
| `?` / `F1` | Help |

In shell mode (`Ctrl-T`, when the server's `[shell] enabled` is on) the
composer writes shell commands for the server, prompted `<dir> $` in its own
colour, and the session's runs stand in for the timeline. `Enter` runs the
command in the session's workdir; `↑` / `Ctrl-P` recall earlier commands.
`Ctrl-O`'s condition shows beside the prompt (`work [if ok ✓] $`), never in
the text: `&&` is just text, passed to the shell. On the runs, `j` / `k`
select, `Enter` shows a run's whole output, `d` downloads it, `y` copies it
with colours stripped, and `e` / `Del` edit or take back a command not sent
yet. A run of yours that took ten seconds or more alerts when it finishes
while you are not watching it.

Copies go to your local clipboard through the terminal (OSC 52), so they work
over SSH. Inside tmux, enable `set -g set-clipboard on`. Content over 74 KiB is
not copied (the notice names the blob's file); `SKYWAVE_OSC52_MAX` sets another
limit in bytes.

When an agent asks for an approval the TUI rings the terminal bell, once per
new batch of requests. `--notify` (or `SKYWAVE_NOTIFY`) picks how: `off`, or
any of `bell`, `osc9`, `osc777` separated by commas; `osc9` / `osc777` send a
desktop notification naming the session (never the tool's input). Inside tmux
those two need `set -g allow-passthrough on`; the bell always works.

If the local data cannot be read (written by another skywave version, or
damaged), the TUI says so and exits. `skywave-tui --reset-replica` drops the
local copy of the sessions, queued commands and downloaded blobs and syncs
everything again from the server, keeping the pairing, settings, drafts and
snippets. The GUI offers the same as "Reset local data".

### Managing devices

```sh
skywaved devices list
skywaved devices revoke <id>
```

Admin commands work on the database directly, so `serve` can keep running.

### Pair a phone

1. **Install the app.** Build the Flutter head for Android as in
   [`gui/README.md`](gui/README.md#android-verified) and install it with the
   phone connected over USB (USB debugging on):

   ```sh
   cd gui && flutter run -d <device>
   # or: flutter build apk --debug && adb install -r build/app/outputs/flutter-apk/app-debug.apk
   ```

2. **Make the server reachable.** The phone must reach `listen` and find the
   address it dials in the server certificate. Listen on all interfaces and
   put the server's LAN address (or a name the phone resolves) first in
   `server_names`; `skywaved pair` advertises that first entry:

   ```toml
   listen = "0.0.0.0:7443"
   server_names = ["192.168.1.20"]
   ```

   Restart `skywaved serve` after changing `server_names`: it reissues the
   server certificate for the new names and keeps the CA, so devices paired
   already keep working. Open TCP port 7443 in the server's firewall.

3. **Pair.** Run `skywaved pair` on the server. In the app, tap **Scan QR
   code** and point the camera at the QR code it printed, or type the
   address, code and fingerprint, then tap **Pair**. A wrong fingerprint is
   refused before the code is sent, so the code stays usable.
   `skywaved devices list` now shows the phone. Opening
   `skywave://pair` links is not supported yet.

4. **Answer an approval.** Tap **New session**, enter a folder under one of
   your `roots` and tap **Create** (to continue a Claude Code session started outside Skywave, paste its id in the optional field first); open the session and send a message.
   When the agent asks, the session shows an **Approval needed** panel with
   what it wants to do: tap **Approve** or **Deny**, or **Always allow**.
   Answers given offline are queued and go out when the phone is back
   online.

5. **Get alerts.** Allow notifications when asked. The app posts a
   notification for new approvals while it is open, one per session, and
   removes it once that session's approvals are answered, from any
   device. In the background it
   suspends its connection, so alerts wait until you return to it.

6. **Revoke.** `skywaved devices revoke <id>` on the server. Within about a
   minute (when its open request ends) the app shows **This device was
   revoked**; the sessions on the phone stay readable.

`scripts/android-acceptance/` holds the rig used to check all of this on an
emulator over a bad link: a stub agent, the link harness as a standalone
proxy, and adb helpers. `run.sh` without arguments prints how to use it.

---

## Under the hood

| Crate | Role |
|-------|------|
| `skywaved` | Server daemon: agent supervisor, shell runner, event log, blob store, CA, mTLS HTTPS API, admin CLI |
| `skywave-core` | All client logic: local replica, outbox, sync engine, blob downloads, view state |
| `skywave-tui` | Terminal head built on ratatui; renders the core's view state and nothing else |
| `skywave-proto` | Versioned, agent-agnostic wire types |
| `skywave-present` | Head-side presentation the TUI and the GUI share: timeline sections, wording, draft, read-mark and alert policies |
| `gui/` | Flutter GUI head (desktop and Android) over rinf; its Rust bridge `hub` is in `gui/native/hub` |
| `skywave-testkit` | Link harness: a proxy that injects latency, jitter, bandwidth caps and drops |
| `skywave-claude-compat` | Run after a Claude Code CLI upgrade: checks the new CLI against what the adapter relies on |

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

More detail in [`docs/architecture.md`](docs/architecture.md) and
[`docs/decisions.md`](docs/decisions.md).

### GUI build

A Flutter GUI head lives in [`gui/`](gui): a desktop and mobile app that
pairs as its own device and renders what the core and `skywave-present`
decide, with system notifications for approvals. Linux and Android are
built and run; macOS, iOS and Windows are scaffolded but not yet built; web
is not supported. Build steps per platform, including `rinf gen` after a
fresh checkout, are in [`gui/README.md`](gui/README.md).

## Roadmap

- Background approval alerts and pairing links in the Android app
- More agents beyond Claude Code
- QUIC transport and compact encoding
- Several servers per client
- Approval policies and session retention options

## License

Licensed under either of [MIT](https://opensource.org/licenses/MIT) or
[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0), at your option.
