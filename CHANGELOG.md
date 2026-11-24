# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0,
a minor version may break the wire protocol or stored data; an upgrade
migrates the stored data (see the README).

## [Unreleased]

## [0.2.0] - 2026-11-24

This release changes the wire protocol (v43): upgrade the server and every
device together. The server and the app migrate their stored data on start.

### Added

- App screen: drive an app on an Android device or emulator attached to the
  server, from a session's More menu. List devices, see the screen, tap,
  swipe, type, press keys, install an APK, launch and stop an app. Off by
  default; turn it on with `[screen] enabled` in `skywaved.toml`, which
  gives every paired device the same power as `[shell]`.
- Search across every session's history, or within one session, from the
  app. A hit opens the timeline at its item with the matches marked.
- Cancel a blob download, a workdir download or a file upload from its row.
- Approve and deny with `Ctrl+Shift+A` and `Ctrl+Shift+D` in the app.
- `Escape` goes back on every page of the app.
- The app asks before re-pairing, forgetting a server address, or resetting
  local data.
- A session created from the new session dialog opens right away.
- `scripts/deploy/deploy.sh --local` deploys to the machine that builds the
  server, and `scripts/deploy/setup-build.sh` installs the tools the deploy
  build needs (zig, cargo-zigbuild, the musl Rust targets).

### Changed

- Session activity shows in local time and relative ("5 min ago"), and
  stays current.
- Each pairing failure says what to do next.

### Fixed

- A slow connect no longer eats into a request's deadline.
- An interrupted blob download keeps the progress it made.
- Back from a session returns to the list you came from, also when the open
  session is deleted.
- A held `Escape` goes back only once.

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
