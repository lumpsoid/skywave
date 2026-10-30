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
