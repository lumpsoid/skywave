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
- Every `get_settings` answer keeps only `response.applied`: its `effective` and `sources` hold the merged settings layers (a real user's `env`, hooks and permission rules).

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
| `effort_switch.ndjson` | 2026-10-31, a Python driver, control requests only, no user message, `step:` notes. It writes exactly what the adapter writes, in its order: each `get_settings` goes out only when the answer it follows comes. (1) `initialize`, then an unasked `get_settings` (`medium`). (2, 3) `apply_flag_settings` `high`, `max`, each acked with an empty success and read back. (4) `low` and `medium` as two queued requests: `medium`'s apply waits for `low`'s read. (5) `set_model claude-haiku-4-5-20251001` reads `effort: null`; `high` is acked and still reads `null`. (6) `set_model sonnet` reads `high`: the level outlasted the model that took none. (7) A start: `set_model opus` and the apply `xhigh` pipelined, a read after each answer; both read `xhigh` on opus. |
| `resume.ndjson` | A new process with `--resume <session id from basic.ndjson>`. The model quotes the first prompt from `basic`, and every message carries the **same** session id. |
| `fork.ndjson` | 2026-11-01, made by driving `record-claude.sh` directly (skywaved cannot fork yet), with the stdin skywaved writes (`initialize`, then one `user` message). Four processes: (1) `--session-id A`, the model is told PELICAN; (2) `--resume A --fork-session --session-id B`, told OTTER: every message carries **B**; (3) `--resume A` lists only PELICAN, so the fork left A untouched; (4) `--resume B` lists PELICAN and OTTER. B's first `total_cost_usd` includes A's; its `model_usage_since_last_result` is its own turn. |
| `background_bash.ndjson` | 2026-11-01, a Python driver writing what skywaved writes (`initialize`, one `user` message), allowing every `can_use_tool`, and keeping stdin open after each `result` until 200 s pass with no output; notes carry `t+Ns`. A Bash `sleep 120` with `run_in_background`: `system/background_tasks_changed` lists the `local_bash` task and `task_started` names it, the tool result says it runs in the background, and the turn ends at t+4s. At t+123s, with no message written, the CLI sends `task_updated`, `task_notification`, `background_tasks_changed` with `tasks: []`, then a fresh `system/init` and a whole turn of its own. |
| `subagent.ndjson` | 2026-11-01, same driver. An `Agent` call runs in the background by default: `background_tasks_changed` and `task_started` (`local_agent`, `is_backgrounded: true`), "Async agent launched", and the turn ends. The subagent's own `assistant` message then comes on stdout **after** that `result` and outside any turn, marked by `parent_tool_use_id`; its `task_notification` (`summary` is its answer) and the empty task list are followed by a turn the CLI starts on its own. |
| `subagent_background_bash.ndjson` | 2026-11-01, same driver. A background subagent starts a background Bash `sleep 120` and a `Monitor` on it: their `task_started` carry `owned_by_subagent: true`, and every `background_tasks_changed` holds the whole list of pending tasks, the subagent's own included. The subagent finishes first (its tasks still listed), so the CLI starts a turn; at t+127s the sleep ends, the subagent is woken again (a new `run_id` for the same `task_id`), and the CLI starts two more turns by itself. The list is empty only after the last of them. |
| `loop.ndjson` | 2026-11-01, same driver, stopped at 330 s. `/loop 1m reply only TICK` becomes a `CronCreate` (recurring, session-only) and the turn ends. Every minute the CLI starts a turn on its own, framed by `command_lifecycle` `started` (`wakeup_source: "schedule_wakeup"`) and `completed` around `system/init` … `result`. A cron job is **not** a background task: no `background_tasks_changed` names it, so between ticks nothing says it is pending. |
| `stop_task.ndjson` | 2026-11-01, a scripted Python driver: `initialize` with `hooks: {Stop: [{hookCallbackIds: ["skywave-stop"]}]}` and `perTaskStopAffordance: true`, every `hook_callback` answered at once with an empty success, every `can_use_tool` allowed, `step:` notes with `t+Ns`. The model starts a background Bash `sleep 120` and a background subagent that runs a foreground 90 s Python sleep; the turn ends at t+3s. At t+8s `stop_task` the Bash: `task_updated` `killed`, `task_notification` `stopped`, a level without it, **then** the success `control_response` (`response: {}`). At t+16s `stop_task` the subagent: the same order, the level goes empty, then the subagent's own shell gets `stopped` and the CLI starts a turn by itself. At t+24s `stop_task` an unknown id and the already stopped Bash: **both answer success** with `{}`, no other output. A second process (no user message) sends `stop_task` with `task_id: 123` and with none: the only error seen, `"stop_task: task_id must be a string"`. |
| `interrupt_spares_tasks.ndjson` | 2026-11-01, same driver and `initialize`. A background subagent runs a foreground 45 s Python sleep while the main turn runs a foreground 30 s one; `interrupt` at t+10s. Only the main turn's shell is `stopped`; the turn ends `aborted_tools` with **no** `hook_callback`. The subagent stays in the level, its shell finishes (`completed`), it answers, and at t+53s `task_notification` `completed`, the empty level and a turn the CLI starts by itself follow. |
| `interrupt_kills_tasks.ndjson` | 2026-11-01, same driver, but `initialize` with `hooks: null` and no `perTaskStopAffordance` (what skywaved sent before). Same scenario, `interrupt` at t+10s: the subagent is `killed`/`stopped` and the level goes empty **before** the interrupt's `control_response`, then its shell and the main shell are `stopped` and the turn ends `aborted_tools`. Nothing more comes. The CLI exited 1 when stdin was closed. |
| `stop_hook.ndjson` | 2026-11-01, same driver and `initialize`. `/loop 1m reply only TICK`, one tick, then a message asking for `CronDelete`, then one asking for a `CronCreate` with `durable: true` (`0 4 * * *`), then one deleting it. Every turn end, the self-started tick included, brings exactly one `hook_callback` (`callback_id: "skywave-stop"`, `input.hook_event_name: "Stop"`) after the turn's last `assistant` and **before** its `result`; the empty success lets the turn end normally. `session_crons` lists `{id, schedule, recurring, prompt}` after the create and the tick, `[]` after the delete. `durable: true` is **ignored** in this build: `tool_use_result.durable` is `false`, the text says "Session-only", `session_crons` shows it like any other job, and no `.claude/scheduled_tasks.json` appears (notes at t+65s, t+73s). |
| `loop_resume.ndjson` | 2026-11-01, same driver and `initialize`. `/loop 1m reply only TICK`, one tick (t+60s), stdin closed; a second process with `--resume <same id>` (instead of `--session-id`) gets one message. Its first Stop hook (t+66s) already lists the same job id in `session_crons`, and the ticks come back on their own at t+121s and t+180s, framed by `command_lifecycle` as before. |
| `subagent_approval.ndjson` | 2026-11-01, same driver and `initialize`; every `can_use_tool` for Write is held 20 s. A background subagent is told to Write a file. The main turn's `result` comes at t+4s; the subagent's `can_use_tool` comes at t+5s, **outside any turn**, carrying `agent_id` (the subagent's `task_id`) and the subagent's own `tool_use_id`. Allowed at t+25s, the Write runs, the subagent ends and the CLI starts a turn by itself. |
