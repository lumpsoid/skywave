# Claude Code stream-json fixtures

(Stored transcripts, a different format, are in `transcripts/` with their
own README.)

Real recordings of Claude Code CLI **2.1.310** on haiku, except the three
`context` rows at the end (2026-11-08, opus with sonnet subagents). All but
three were made by `skywave-claude-compat --only <name> --record <dir>`, on
2026-11-05 unless their row says otherwise:
the scenario of the same name drives the CLI through the adapter's
`Protocol`, so every stdin line is what skywaved writes, and every `user`
line carries the `uuid` that the CLI's `command_lifecycle` lines name. The
scenarios are in `crates/skywave-claude-compat/src/scenarios/`, and their
doc comments say what each one checks. The protocol notes that go with the
recordings are in `docs/agents/claude-code.md`.

## Format

Each file is NDJSON. Each line is one record, in the order it happened:

| `dir`  | meaning                                                          |
|--------|------------------------------------------------------------------|
| `in`   | `msg` is a line the driver wrote to the CLI's **stdin**          |
| `out`  | `msg` is a line the CLI wrote to **stdout**                      |
| `note` | `text` is a driver remark: argv, `t+Ns` timings, steps, "closing stdin", exit code |

To get the raw stream back:
`jq -c 'select(.dir=="out") | .msg' basic.ndjson` (use `"in"` for stdin).
Timings are left out of the records. Each `note` carries the seconds since
its process started (`t+Ns`). The first note names the scenario and the CLI
version. A file with several processes has one `argv` note for each.

Every run used this argv, in a scratch directory under `/tmp`:

```
claude -p --input-format stream-json --output-format stream-json --verbose \
  --permission-prompt-tool stdio --settings '{"cleanupPeriodDays":36500}' \
  --model haiku --setting-sources project --strict-mcp-config \
  --session-id <uuid> | --resume <uuid>  [scenario flags]
```

Every process opens with skywaved's `initialize` (the Stop hook
`skywave-stop`, `perTaskStopAffordance: true`) and a `get_settings`, unless
its row says otherwise. Every Stop `hook_callback` gets an empty success.

To re-record one file, run `skywave-claude-compat --only <name> --record <dir>`
and copy the result here. `tests/support/record-claude.sh` writes the same
format from between skywaved and the real CLI; `--scrub` applies the
scrubbing below.

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
| `basic.ndjson` | Two processes. (1) `--session-id`: two turns (PONG, then PING), then stdin is closed and the CLI exits 0. Shows `command_lifecycle` around each message, `system/init`, `assistant` text, the Stop hook before each `result/success`. (2) `--resume` the same id: one turn whose answer names both bells, so the history came back; every message carries the **same** session id. |
| `permissions.ndjson` | Four turns. A Write gets `can_use_tool` and is allowed; a Bash `rm` is denied, and the CLI sends back an error `tool_result` and a final text; a Bash `touch oven-timer` is allowed with its offered rule `Bash(touch oven-timer *)` granted; the same `touch` in the next turn gets no `can_use_tool`. |
| `interrupt.ndjson` | `--include-partial-messages`. (1) An `interrupt` while text streams: its success (`still_queued: []`), a partial `assistant`, the user message `[Request interrupted by user]`, and `result/error_during_execution` with `terminal_reason:"aborted_streaming"`. (2) `echo` is auto-allowed as read-only, so this turn has no approval; it shows a full tool turn as `stream_event`s. (3) An `interrupt` while idle, written raw (the adapter sends none then), still gets a success. (4) A later turn works normally. |
| `interrupt_pending.ndjson` | An `interrupt` while a Write's `can_use_tool` waits: the CLI sends a `control_cancel_request` for it, then the interrupt's success, a rejected `tool_result` and `result/error_during_execution` (`aborted_streaming`). The next turn works (PONG). Then stdin is closed while a second Write approval waits: the permission fails, the model retries, the retry fails too, the turn ends with `result/success`, and the CLI exits 0. |
| `permission_modes.ndjson` | A `set_permission_mode` before turns walks `default` → `acceptEdits` → `plan` → `auto` → `dontAsk` → `default`, with one Write per mode (and a Bash `rm` in `acceptEdits` and `auto`); `step:` notes mark each turn and every `can_use_tool` is allowed. Only `default` asks. In `plan` the model writes its plan file unasked and asks for `ExitPlanMode`; allowing it returns to `acceptEdits`, so the Write that follows is not asked. In `dontAsk` the Write is refused with `system/permission_denied` and no `can_use_tool`. The last turn switches to `acceptEdits` while a Write approval waits: no cancel follows, and the approval is then answered. |
| `permission_modes_plan.ndjson` | `default` → `plan`, then one Write: the approved `ExitPlanMode` returns to `default`, so the Write and the next turn's Write are both asked. |
| `rules_granted.ndjson` | `--setting-sources project,local` (`docs/agents/claude-code.md` §3.5). One process, three approvals that each grant an update through `updatedPermissions`: `addRules` `Bash(touch oven-timer *)` (`localSettings`), `addDirectories` for a dir outside the workdir (`session`, offered for a `touch` there), `setMode` `acceptEdits` (`session`, offered for a Write). The next call of the same tool after each (the same `touch`, a `cat` in the added dir, another Write) gets no `can_use_tool`. |
| `ask_user_question.ndjson` | 2026-11-08 (`docs/agents/claude-code.md` §3.14). Four turns, each an `AskUserQuestion` `can_use_tool` (`requires_user_interaction: true`, no suggestions). (1) single-select, answered with one label: `updatedInput.answers` maps the question's text to `Rye`; (2) multi-select, two labels joined `Seeds, Oats`; (3) text of the user's own, `Marmalade, homemade`, and the `tool_result` reads "The user answered: …"; (4) denied: an error `tool_result` with the deny message. Every turn ends with `result/success`. |
| `fork_turns.ndjson` | Four processes (`docs/agents/claude-code.md` §5). (1) `--session-id A`, the model is told PELICAN; (2) `--resume A --fork-session --session-id B`, told OTTER, answers PELICAN OTTER: every message carries **B**; (3) `--resume A` lists only PELICAN, so the fork left A untouched; (4) `--resume B` lists PELICAN and OTTER. |
| `background_bash.ndjson` | A Bash `sleep 20 && echo DING` with `run_in_background`: `system/background_tasks_changed` lists the `local_bash` task and `task_started` names it, the tool result says it runs in the background, and the turn ends at t+3s. When the sleep ends, with no message written, the CLI sends `task_updated`, `task_notification`, `background_tasks_changed` with `tasks: []`, then a fresh `system/init` and a whole turn of its own, framed by `command_lifecycle`. |
| `subagent.ndjson` | A background `Agent` call: `background_tasks_changed` and `task_started` (`local_agent`, `is_backgrounded: true`), "Async agent launched", LAUNCHED, and the turn ends. The subagent's steps then come **after** that `result`, outside any turn, marked by `parent_tool_use_id`: a Write of `chalkboard.txt` whose `can_use_tool` carries `agent_id`, and its answer WRITTEN. Its `task_notification` and the empty task list are followed by a turn the CLI starts on its own (NOTED). |
| `subagent_background_bash.ndjson` | A background subagent starts a background Bash `sleep 60` and a `Monitor` on it: every `background_tasks_changed` holds the whole list of pending tasks, the subagent's own included. The subagent finishes first (its tasks still listed), so the CLI starts a turn; when the sleep ends the subagent is woken again (a new `task_started` for the same `task_id`), answers SLEPT, and the list goes empty before the CLI's last turn of its own. |
| `message_meets_own_turn.ndjson` | Task b26ecf, `docs/agents/claude-code.md` §3.11. A background Bash `sleep 6 && echo BOILED`; the turn ends. The moment its `task_notification` arrives, a second message is written: the CLI reads it (`queued`), then starts a turn of its own (`system/init`), runs `ls`, and only after that tool result takes the message up (`started`), answers TEA and ends both with one `result`. |
| `stop_task.ndjson` | A background Bash `sleep 120` and a background subagent that runs a foreground 90 s Python sleep; the turn ends at t+5s. `stop_task` the Bash: `task_updated` `killed`, `task_notification` `stopped`, a list without it, **then** the success `control_response` (`response: {}`). `stop_task` the subagent: the same order, the list goes empty, then the subagent's own shell gets `stopped` and the CLI starts a turn by itself. `stop_task` an unknown id and the already stopped Bash: **both answer success** with `{}`, no other output. A second process (no user message) sends `stop_task` with `task_id: 123` and with none: the only error seen, `"stop_task: task_id must be a string"`. |
| `interrupt_spares_tasks.ndjson` | A background subagent runs a foreground 45 s Python sleep while the main turn runs a foreground 30 s one; `interrupt` at t+6s. Only the main turn's shell is `stopped`; the turn ends `aborted_tools` with **no** `hook_callback`. The subagent stays in the list, its shell finishes (`completed`), it answers RISEN, and `task_notification` `completed`, the empty list and a turn the CLI starts by itself follow. |
| `interrupt_kills_tasks.ndjson` | The same, but with `initialize` holding no hooks and no `perTaskStopAffordance` (what skywaved sent before), written raw. `interrupt` at t+6s: the subagent is `killed`/`stopped` and the list goes empty **before** the interrupt's `control_response`, then the main shell is `stopped` and the turn ends `aborted_tools`. Nothing more comes. The CLI exits 1 when stdin is closed. |
| `loop.ndjson` | `/loop 1m peek at the oven and say only TICK` becomes a `CronCreate` (recurring, session-only) and the turn ends. A minute later the CLI starts a turn on its own (TICK), framed by `command_lifecycle` `started` (`wakeup_source: "schedule_wakeup"`) and `completed` around `system/init` … `result`. Then a message asks for a `CronCreate` with `durable: true`; the CLI makes it session-only. A cron job is **not** a background task: no `background_tasks_changed` names it, so between ticks nothing but the Stop hook's `session_crons` says it is pending. |
| `stop_hook.ndjson` | `/loop 1m …`, one tick, then a message asking for `CronDelete`, then one asking for a `CronCreate` with `durable: true` (`0 4 * * *`), then one deleting it. Every turn end, the self-started tick included, brings exactly one `hook_callback` (`callback_id: "skywave-stop"`, `input.hook_event_name: "Stop"`) after the turn's last `assistant` and **before** its `result`; the empty success lets the turn end normally. `session_crons` lists `{id, schedule, recurring, prompt}` after the create and the tick, `[]` after the delete. `durable: true` is **ignored** in this build: `tool_use_result.durable` is `false`, the text says "Session-only", and `session_crons` shows it like any other job. |
| `loop_resume.ndjson` | `/loop 1m …`, one tick, stdin closed; a second process with `--resume <same id>` gets one message. Its first Stop hook already lists the same job id in `session_crons`, and the next tick comes back on its own, framed by `command_lifecycle` as before. |
| `subagent_approval.ndjson` | A background subagent is told to Write a file. The main turn's `result` comes first; the subagent's `can_use_tool` comes **outside any turn**, carrying `agent_id` (the subagent's `task_id`) and the subagent's own `tool_use_id`. Once answered, the Write runs, the subagent ends and the CLI starts a turn by itself. |
| `subagent_foreground.ndjson` | A scratch dir holding `recipes/`; the main agent is asked for a foreground `Agent` call that finds the garlic recipes and writes them to a file: `task_started` with `is_backgrounded: false`, **no** `background_tasks_changed` at any point, the subagent's prompt streamed as a `user` text line with `parent_tool_use_id`, its steps tagged likewise, a `can_use_tool` for its Write with `agent_id` = the task id (the approval is the turn's), then `task_updated` (`completed`) and `task_notification`; the `Agent` call's `tool_result` comes only then, and the turn ends. |
| `subagent_foreground_backgrounded.ndjson` | A foreground helper writing tasting notes. Once it took two steps, `{"subtype":"background_tasks","tool_use_id":<Agent call>}` (Ctrl+B): `background_tasks_changed` gains the task id, `task_updated` patches `is_backgrounded: true`, the answer is `{"backgrounded": true}`, the blocked `Agent` call gets "Async agent launched" at once and the main turn ends; the helper's steps go on with the same `parent_tool_use_id` and end with `task_notification` and a turn the CLI starts on its own. |
| `subagent_foreground_stop.ndjson` | The same helper, stopped after two steps with `stop_task` on its task id (never in the list): `task_updated` (`killed`), `task_notification` (`stopped`), then success `{}`; the `Agent` call's `tool_result` is an error, "[Request interrupted by user for tool use]", and the main turn ends with `result/success`. |
| `subagent_message_ctl.ndjson` | A background subagent is told to run two foreground 20 s Python sleeps and reply with the code word it was sent, or NONE. Once the main turn ends, a `send_task_message` control request (`task_id`, `message`), written raw, gets the error `Unsupported control request subtype: send_task_message`; the subagent answers NONE. |
| `subagent_message_ctl_done.ndjson` | The subagent replies PONG and finishes; `send_task_message` to the finished task gets the same `Unsupported` error. |
| `subagent_message_ptu.ndjson` | A `user` line with `parent_tool_use_id` set to the `Agent` call's id, written raw while the subagent runs, starts a **main** turn ("Noted: ZEBRA"); the subagent never sees it and answers NONE. |
| `subagent_message_relay.ndjson` | A main-turn message asks the main agent to `SendMessage` the sleeping subagent the code word: it loads the tool with `ToolSearch`, and the result says "queued for delivery … at its next tool round". The subagent, still in its sleeps, gets it with no line on stdout showing the delivery, and answers ZEBRA. |
| `subagent_message_relay_midrun.ndjson` | The same relay to a subagent busy with ten `date +%N` calls: the message lands mid-run, with no line on stdout showing the delivery, and the subagent answers ZEBRA. |
| `context_fill.ndjson` | 2026-11-08, `--model claude-opus-5-5` (`docs/agents/claude-code.md` §2.6). After every turn a `get_context_usage` request, written raw, reads the CLI's own count. (1) PONG: `usage.iterations` holds one request, whose fill (input + cache read + cache creation + output) is within tens of tokens of `totalTokens`; `model_usage_since_last_result.claude-opus-5-5.contextWindow` is 1000000, and `maxTokens` says the same. (2) Two `echo` calls in separate requests (`num_turns: 3`): `usage` sums the three requests (56055), and `iterations` still holds only the last one. (3) `/compact`: `system/status` `compacting`, `system/compact_boundary` (`trigger: "manual"`, `pre_tokens` 18723 = the last fill, `post_tokens` 3010, which leaves out the system prompt and tools), then a `result` with `num_turns: 0` and no `iterations`. (4) The next turn recalls the bell from the summary; its fill is 19.4k. |
| `context_autocompact.ndjson` | 2026-11-08, opus, with `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=3` (about 30k tokens of the 1M window). Three `seq` outputs of 2,500 lines each, then a recall. The third turn's `system/init` is followed by `status` `compacting` and `compact_boundary` (`trigger: "auto"`, `pre_tokens` 32756, 30 over the last fill of 32726) **before** its first request; the turn then goes on, and its `result` reports the fill after the compaction (26.9k). |
| `subagent_context.ndjson` | 2026-11-08, opus, the subagents on sonnet (the Agent call's `model: "sonnet"`). (1) A foreground helper: its `task_notification` carries `usage: {total_tokens: 11850, tool_uses: 1, duration_ms: 4284}`; the Agent call's `tool_use_result` carries `resolvedModel: "claude-sonnet-5-5"`, `totalTokens` 11832 (= the fill of its own `usage.iterations`), `totalDurationMs`, `totalToolUseCount`. The turn's `model_usage_since_last_result` lists both models. (2) Two background helpers from one message: each Agent result is `async_launched` with `resolvedModel` and no figures; each `task_notification` carries its own `usage`. The CLI then starts a turn on its own, and a second one after stdin is closed: the first's `model_usage_since_last_result` holds a sonnet entry, as the helpers' spend comes in the next `result`. |
| `permission_mode_errors.ndjson` | 2026-10-31, control requests only, no user message. `set_permission_mode` `plan` succeeds, then `bypassPermissions` (`bypass_not_launched`) and `bogus` (`invalid_mode`) are rejected; an `initialize` after each reads `current_permission_mode` back. |
| `model_switch.ndjson` | 2026-10-31, task 34f66d, a Python driver, control requests only, no user message. `set_model` `sonnet` and `claude-opus-4-7` succeed with the resolved model, `bogus-model` is rejected (`not_found`), `default` and a request with no `model` both switch to the account default; an `initialize` between some steps shows it does not report the current model. |
| `effort_switch.ndjson` | 2026-10-31, a Python driver, control requests only, no user message, `step:` notes. It writes exactly what the adapter writes, in its order: each `get_settings` goes out only when the answer it follows comes. (1) `initialize`, then an unasked `get_settings` (`medium`). (2, 3) `apply_flag_settings` `high`, `max`, each acked with an empty success and read back. (4) `low` and `medium` as two queued requests: `medium`'s apply waits for `low`'s read. (5) `set_model claude-haiku-4-5-20251001` reads `effort: null`; `high` is acked and still reads `null`. (6) `set_model sonnet` reads `high`: the level outlasted the model that took none. (7) A start: `set_model opus` and the apply `xhigh` pipelined, a read after each answer; both read `xhigh` on opus. |
