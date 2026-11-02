# Skywave

A remote client for coding agents that works over bad networks. `skywaved`
runs the agent on the server; heads (TUI, Flutter GUI) keep a full local
replica through `skywave-core`. Read `README.md` for the product and the docs
below before designing anything.

## Layout

- `crates/skywave-proto`: the wire protocol. `PROTOCOL_VERSION` must match exactly.
- `crates/skywaved`: server: event log, supervisor, agent adapters
  (`src/agent/claude_code/`), pairing and the CA.
- `crates/skywave-core`: device side: replica, outbox, sync, link state.
- `crates/skywave-present`: presentation decisions shared by every head.
- `crates/skywave-tui`: terminal head. `crates/skywave-testkit`: test helpers.
- `gui/`: Flutter head. `gui/native/hub` is the rinf bridge (crate name `hub`)
  and a member of the Cargo workspace.
- `docs/`: `mvp-spec.md` (the spec), `decisions.md` (D-numbered and settled),
  `architecture.md`, `agents/claude-code.md` (the stream-json protocol notes).

## Gates

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd gui && flutter analyze && flutter test)   # only when gui/ changed
```

- Run `rinf gen` in `gui/` after changing `gui/native/hub/src/signals`. The
  Dart bindings it generates are not committed.
- Wire snapshots: `UPDATE_SNAPSHOTS=1 cargo test -p skywave-proto --test wire`
  rewrites them. Only do that for a deliberate wire change, and bump
  `PROTOCOL_VERSION` once per batch of wire changes.
- While iterating, run the narrowest test (`cargo test -p <crate> --test <file> <name>`).
  In multi-phase work, run only the tests you changed or added. Run the full
  gates once, at the end.

## Build setup

- `.cargo/config.toml` sets `rustc-wrapper = "sccache"`. Each worktree keeps
  its own `target/`. Don't point several worktrees at one target dir: cargo
  then reuses another branch's test binaries as fresh.
- `/tmp` is a 16 GB tmpfs that other agents share. Never put a
  `CARGO_TARGET_DIR` or a build there.

## Tests

- Tests are deterministic. Use the fake agent (`crates/skywaved/tests/support/fake-claude.sh`)
  and the recorded fixtures in `crates/skywaved/tests/fixtures/claude-code/`.
- Only fixture recording (`tests/support/record-claude.sh`) and probes run the
  real `claude` CLI, in a scratch directory with trivial prompts. Write down
  the CLI version.

## Rules

- Alpha, with no release. Nothing earlier is supported: no old clients, data or
  protocol versions. Don't add compatibility layers.
- If code has to depart from the spec, update `docs/mvp-spec.md` (or the
  relevant doc) in the same commit and say so in the report.
- Agent-agnostic naming outside `src/agent/claude_code/` (D3).
- Decide outcomes from definitive signals (command ids, log position), not
  timeouts or guesses.
- Plans, handoffs and prompts for another agent never go in the repo. Use
  `/handoff` and `/pickup` (or `agent-plan`), which keep them outside the
  repo and archive them when done. The repo holds only what stays true.
- Out-of-scope work becomes a task: `git task add "<title>" -b "<why>" -t followup`.
  Tasks live in `refs/tasks/index` (see the git-task skill).

## Commits

- Imperative, sentence-case subject with no type prefix ("Retry the link after
  a protocol mismatch"). Put the why and any gotchas in the body.
- No `Co-Authored-By` or other AI attribution trailer.
- Stage by explicit path. Never `git add -A` or `git add .`.

## Working as an agent here

- **Edit files with Edit/Write.** In a worktree-isolated agent, any command
  the harness can't prove stays inside the worktree is refused as "too complex
  to verify". That covers `python3 - <<EOF` rewrites, `sed -i`, `perl -pi`,
  `for` loops, shell variables, `$(...)` and paths outside the worktree. It
  accounted for 216 refused calls. Read with Read, search with Grep, and run
  any multi-step script from a file inside the worktree.
- **Map Rust before reading it.** Your first call in a Rust module is
  `rust_index <dir>` (the `rust-index` skill), before any grep or Read. It
  lists every item and signature with line numbers in milliseconds. Don't
  grep for definitions (`fn foo`, `enum Bar`); find them in the index, then
  Read just those lines instead of paging with `sed -n` or offset reads.
- **Keep scratch files in `$WORKTREE/.scratch/`** (it is git-ignored), not
  `/tmp`.
- **Run one plain command per Bash call** from the worktree root. Don't `cd`
  into the main checkout.
- **Never wait with `sleep`.** Start long work with `run_in_background` (you
  get notified when it exits) or watch for a condition with Monitor.
- Recursive `rm` works only on literal paths under `/tmp/` or
  `.claude/worktrees/`. Ask the user about anything else.
- When an orchestrator lands a worktree, it removes the worktree and its
  branch right away. Each `target/` is 5–13 GB.
