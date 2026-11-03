# Claude Code transcript fixtures

Real transcripts (`~/.claude/projects/<dir>/<id>.jsonl`) written by Claude
Code CLI **2.1.310** on 2026-11-03, each in a fresh scratch dir under `/tmp`
with `--model haiku --setting-sources project --strict-mcp-config`. The
format notes are in `docs/agents/claude-code.md` §6c.

Recorded with `tests/support/record-transcript.sh <scenario> <this dir>`,
which also applies the scrubbing below (`--scrub` alone re-applies it).

## Scrubbing

- `api-request*` lines are dropped: they hold whole request bodies, the
  user's own context included.
- Each `attachment` line keeps only `attachment.type` (its content and
  `rendered` are dropped); the line itself stays, as the `parentUuid` chain
  runs through it.
- The scratch dir became `/WORKDIR`, its project dir name `-WORKDIR`, the
  home directory `$HOME`, every `signature` `<scrubbed-signature>`, every
  `gitBranch` empty.
- The script refuses a result that still holds an e-mail address.

Nothing else was changed: uuids, session ids and timestamps are the real
values. Tests write a fixture into a temp projects dir as
`<project_dir(workdir)>/<sessionId>.jsonl`, with `/WORKDIR` replaced by the
test's workdir.

## Files

| file | scenario |
|------|----------|
| `two_turn.jsonl` | `-p "say hi"`, then `-p --resume <id> "what did I just ask?"`. |
| `tools.jsonl` | One `-p` turn asked to run, one call each: Bash `echo hi`, Write `note.txt`, Read it, Edit it, Bash `seq 1 30000`. Has thinking blocks, each tool's `toolUseResult`, and a `<persisted-output>` stub for the big output. |
| `parallel.jsonl` | `-p` asking to Read `a.txt` and `b.txt` in parallel (the script writes both). Two `tool_use` lines in a chain, then two `tool_result` lines, **each parented to the line holding its own call**: the first result is a sibling off the chain that ends the file. |
| `interrupt.jsonl` | stream-json input: a message asking for a 30 s Python sleep, an `interrupt` control request 10 s later (while the Bash call waits), then "say ok". The call's result is an error ("The user doesn't want to proceed…", `toolUseResult: "User rejected tool use"`), followed by the user line `[Request interrupted by user for tool use]`. A first try with `sleep 30` was refused by a hook as a standalone sleep, so the turn ended before the interrupt; the prompt was changed. |
| `subagent.jsonl` | `-p` asking for a foreground `Agent` call whose helper replies PONG. The main file holds only the call and its result. |
| `subagent.subagent-1.jsonl` | That helper's own transcript (`<id>/subagents/agent-*.jsonl`), every line `isSidechain: true`. |
| `compact.jsonl` | `-p "remember the word PELICAN and say ok"`, `-p --resume <id> "/compact"`, `-p --resume <id> "which word did I ask you to remember?"`. Has the `compact_boundary` (`parentUuid: null`, `logicalParentUuid`), the `isCompactSummary` user line, an `isMeta` caveat and the command's own user lines. |

Not recorded, built by the tests from these files instead: a file whose
name is not a UUID (two_turn under `x.jsonl`), sidechain lines inside a
main file, an `ai-title` line (`-p` writes none; the shape is the one seen
in interactive transcripts), a rewound branch, a cut-short last line.
