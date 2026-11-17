# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0,
a minor version may break the wire protocol or stored data; an upgrade
migrates the stored data (see the README).

## [Unreleased]

## [0.1.1] - 2026-11-17

The first public release.

### Added

- `skywaved`, the server daemon: runs and supervises Claude Code sessions,
  keeps an append-only event log, its own CA and device registry, and
  serves devices over mutual TLS.
- Pairing with a one-time code and a pinned fingerprint, shown as a QR code
  too; revoking a device from the server.
- `skywave-tui`, the terminal head, and a Flutter app for Android and Linux
  desktop. Each keeps a full local replica, so history opens offline and
  sending never waits on the network.
- Commands sent at most once: every command carries an id, retries are
  deduplicated, and a queued message shows `Queued`, `Sending`, `Accepted`.
- Approvals from any paired device, offline answers queued; approval
  notifications, in the background too on Android with "Stay connected".
- Large tool inputs and outputs as blobs, previewed in the timeline and
  downloaded on demand with resume.
- A session's workdir: its git changes with a diff per file, its files, and
  file download and upload.
- Sessions on a branch in their own git worktree, and a Worktrees page.
- Shell commands on the server from a device (off by default).
- A safety check a session can opt into: the agent's shell commands that are
  provably read-only inside the workdir, or match a rule you trust, are
  approved without asking.
- Continuing a Claude Code conversation started outside Skywave.
- Per-turn and per-session cost and token usage.
- The version and commit in `--version` and on the app's settings page.
- A Licenses page in the app's settings, with the bundled font's license
  and every package's.
