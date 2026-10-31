# Claude Code stream-json fixtures

Real recordings of Claude Code CLI **2.1.310**, made for T07 on 2026-10-29.
The protocol notes that go with them are in `docs/agents/claude-code.md`.

## Format

Each file is NDJSON. Each line is one record, in the order it happened:

| `dir`  | meaning                                                          |
|--------|------------------------------------------------------------------|
| `in`   | `msg` is a line the driver wrote to the CLI's **stdin**          |
| `out`  | `msg` is a line the CLI wrote to **stdout**                      |
| `note` | `text` is a driver remark: argv, delays, "closing stdin", exit code |

To get the raw stream back:
`jq -c 'select(.dir=="out") | .msg' basic.ndjson` (use `"in"` for stdin).
Timings are left out. Where a delay matters, a `note` line records it.
stderr was empty in every run, so there are no stderr files.

Every run used this argv, run in a scratch directory under `/tmp`:

```
claude -p --input-format stream-json --output-format stream-json --verbose \
  --permission-prompt-tool stdio --model haiku \
  --setting-sources project --strict-mcp-config  [scenario flags]
```

New recordings are made with `tests/support/record-claude.sh`, which sits
between skywaved and the real CLI and writes this format; `--scrub` applies
the scrubbing below. `rules_granted.ndjson` (rule grants, task d29758) is
not recorded yet; the runbook is in `docs/agents/claude-code.md` §3.5.

## Scrubbing

- `account.email` became `user@example.com`, and `account.organization` became `<scrubbed-organization>`. Both are in the `initialize` control_response.
- `pid` became `12345`.
- The home directory path became `$HOME`.
- Every thinking-block `signature` (an opaque base64 string) became `<scrubbed-signature>`.

Nothing else was changed. Session ids, message ids and request ids are the real values.

## Files

| file | scenario |
|------|----------|
| `basic.ndjson` | `--session-id <uuid>`. An `initialize` handshake, then two user turns in one process ("say hi", then "what did I just ask?"), then stdin is closed. The CLI exits 0. Shows `system/init`, `assistant` text, `rate_limit_event` and `result/success`. |
| `permissions.ndjson` | A Write tool call gets a `can_use_tool` control_request. The driver holds it for **150 s** and then sends allow, and the tool runs. In the second turn a Bash call gets `can_use_tool`, the driver sends deny, and the CLI sends back an error `tool_result` and a final text. |
| `interrupt.ndjson` | `--include-partial-messages`. (1) An `interrupt` is sent while text streams: you see the control_response, a partial `assistant`, the user message `[Request interrupted by user]`, and `result/error_during_execution` with `terminal_reason:"aborted_streaming"`. (2) `echo` was auto-allowed as read-only, so this turn has no prompt; it shows a full tool turn as `stream_event`s. (3) An `interrupt` sent while idle still gets a success response. (4) A later turn works normally. |
| `interrupt_pending.ndjson` | An `interrupt` is sent while a `can_use_tool` is waiting. The CLI sends a `control_cancel_request` for it, a rejected `tool_result`, and `result` with `terminal_reason:"aborted_tools"`. A late allow for the cancelled request is ignored, and the file is not written. Then stdin is closed while a second approval waits. The permission fails with an AbortError, and the model keeps going: it retries, and the retry fails too. The turn ends with `result/success`, and then the CLI exits 0. |
| `permission_modes.ndjson` | 2026-10-31, task d29758. A Python driver, not skywaved, so no `--session-id`, and `step:` notes mark each turn. A `set_permission_mode` before each turn walks `default` → `acceptEdits` → `plan` → `auto` → `dontAsk` → `default`, with one Write (and, in `acceptEdits` and `auto`, a Bash `rm`) per mode. Every `can_use_tool` is allowed. In plan, the model writes its plan file and asks for `ExitPlanMode`; allowing it returns to `acceptEdits`. The last turn switches to `acceptEdits` while a Write approval waits: no cancel follows, and the approval is answered 10 s later. |
| `permission_modes_plan.ndjson` | 2026-10-31, same driver. `default` → `plan`, then one Write: the approved `ExitPlanMode` returns to `default`, so the Write and the next turn's Write are both asked. |
| `permission_mode_errors.ndjson` | 2026-10-31, control requests only, no user message. `set_permission_mode` `plan` succeeds, then `bypassPermissions` (`bypass_not_launched`) and `bogus` (`invalid_mode`) are rejected; an `initialize` after each reads `current_permission_mode` back. |
| `model_switch.ndjson` | 2026-10-31, task 34f66d, a Python driver, control requests only, no user message. `set_model` `sonnet` and `claude-opus-4-7` succeed with the resolved model, `bogus-model` is rejected (`not_found`), `default` and a request with no `model` both switch to the account default; an `initialize` between some steps shows it does not report the current model. |
| `resume.ndjson` | A new process with `--resume <session id from basic.ndjson>`. The model quotes the first prompt from `basic`, and every message carries the **same** session id. |
