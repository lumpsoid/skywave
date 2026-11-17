# Contributing to Skywave

Thanks for wanting to help. Skywave is a small project with settled design
decisions, so a short conversation before code saves everyone time.

## Start with an issue

For anything beyond a small, obvious fix, I'd like you to
[open a proposal](https://github.com/lumpsoid/skywave/issues/new?template=proposal.yml)
first: what problem you hit, what you'd change, and whether you'd implement
it. Once we agree on the shape, open a pull request that links the issue
(`Closes #123`).

This is a wish, not a rule. Typos, a broken link, a clear bug with a
one-line fix: send the pull request straight away.

Bugs go in a [bug report](https://github.com/lumpsoid/skywave/issues/new?template=bug_report.yml).
Include the version from `skywaved --version`, `skywave-tui --version` or
the app's **Settings → About**, on each side involved.

## Before you design anything

Read [`docs/architecture.md`](docs/architecture.md) for how the system works
and [`docs/decisions.md`](docs/decisions.md) for what is settled and why
(the D-numbers). A change that goes against a decision is a proposal to
amend that decision; say so in the issue.

## Setup

- Rust: the version in `rust-version` in [`Cargo.toml`](Cargo.toml), via
  rustup.
- For the GUI: Flutter (stable), and the rinf CLI pinned to the rinf crate's
  version: `cargo install rinf_cli --version 8.11.0 --locked`. Run
  `rinf gen` in `gui/` after a fresh checkout and after changing
  `gui/native/hub/src/signals`; the generated Dart bindings are not
  committed. [`gui/README.md`](gui/README.md) has the per-platform steps.
- The server's integration tests use a fake agent; you don't need the
  `claude` CLI or an account to run them.

## Where a change goes

| Change | Crate |
|--------|-------|
| Wire types | `crates/skywave-proto` (bump `PROTOCOL_VERSION` and add a migration step, see below) |
| Server, event log, agent adapters, pairing | `crates/skywaved` (Claude Code lives only in `src/agent/claude_code/`) |
| Device logic: replica, outbox, sync | `crates/skywave-core` |
| Wording and presentation shared by the heads | `crates/skywave-present` |
| The terminal head | `crates/skywave-tui` |
| The Flutter head | `gui/`, with its Rust bridge in `gui/native/hub` |

Outside the Claude Code adapter, names stay agent-agnostic. A
`PROTOCOL_VERSION` bump adds a step to `PROTOCOL_STEPS` in both
`crates/skywaved/src/db.rs` and `crates/skywave-core/src/store.rs`,
tested on data the previous version wrote. Wire snapshots are rewritten with
`UPDATE_SNAPSHOTS=1 cargo test -p skywave-proto --test wire`, only for a
deliberate wire change.

## The gates

```sh
scripts/check.sh            # everything CI runs, in its order
scripts/check.sh --no-gui   # Rust only
```

That is `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, then `rinf gen`, `flutter analyze`
and `flutter test` in `gui/`. Tests are deterministic: use the fake agent
(`crates/skywaved/tests/support/fake-claude.sh`) and the recorded fixtures,
never the real CLI or a timeout.

If your change makes a doc wrong (`docs/architecture.md`, a README), fix the
doc in the same commit. Add a line to [`CHANGELOG.md`](CHANGELOG.md) under
`[Unreleased]` for anything a user would notice.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): subject`, imperative and lower-case, for example
`fix(skywave-core): retry the link after a protocol mismatch`. Types:
`feat`, `fix`, `docs`, `refactor`, `test`, `chore`; the scope is the crate
or area (`skywaved`, `skywave-core`, `gui`, `claude-code`, ...). Put the why
and any gotchas in the body. Keep commits focused; rebase rather than merge
`main` into your branch.

## License

Skywave is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE). Unless you say otherwise, any contribution you
submit is licensed the same way, without additional terms or conditions.
