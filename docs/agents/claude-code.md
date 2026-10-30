# Claude Code adapter: the stream-json protocol (T07 spike)

**Pinned version: Claude Code CLI `2.1.310`**. The version comes from
`claude --version`. It is also reported as `claude_code_version` in both the
`initialize` response and `system/init`.
Recorded on 2026-10-29 on Linux. The fixtures are in
`crates/skywaved/tests/fixtures/claude-code/`; their `README.md` describes
each file.

This protocol is what the Agent SDKs use internally. It is **not** a
documented stable API (see spec §11). T09 must check `claude_code_version`
at startup and log a warning when it differs from the pinned version.

Legend: **[C]** means confirmed, i.e. seen in a fixture or a probe run against
2.1.310. **[U]** means unconfirmed: inferred from SDK behaviour or the help
text, and not seen.

## 0. Summary for T09

| # | capability | status |
|---|------------|--------|
| 1 | Long-lived stream-json process: send user messages, read text, tool_use, tool_result and result | **[C]** |
| 2 | Permission prompt over stdio (`can_use_tool`), allow and deny replies, a long wait | **[C]** (waits ≥150 s seen; a wait with no limit at all is [U], see §3.4) |
| 3 | Interrupt over stdin, while streaming and while an approval is pending; the process stays usable | **[C]** |
| 4 | Session id (`--session-id` and `system/init`), `--resume` in a new process | **[C]** |
| 5 | initialize handshake, exit when stdin closes, stderr | **[C]**, with one caveat: stdin EOF does not end the current turn (§6) |

The spec needs no change.

## 1. Process invocation

```
claude -p \
  --input-format stream-json --output-format stream-json --verbose \
  --permission-prompt-tool stdio \
  [--session-id <uuid> | --resume <uuid>] \
  [--include-partial-messages] [--model <m>]
```

- `--verbose` is **required**. Without it the CLI exits 1 with
  `Error: When using --print, --output-format=stream-json requires --verbose`. **[C]**
- `--permission-prompt-tool stdio` is a hidden flag that the SDK passes.
  Every recording used it, and permission prompts arrived as `can_use_tool`
  control requests. **[C]** `--help` lists `--permission-prompts host|none`
  (default `host`) for the same purpose. Whether prompts still come over stdio
  without `--permission-prompt-tool stdio` is **[U]**. T09 should always pass
  the flag.
- The fixture runs also used `--setting-sources project --strict-mcp-config`,
  so the user's plugins, hooks and MCP servers stayed out of the transcript.
  Whether T09 does the same is a config question (`agent.args`). Without these
  flags, user hooks and MCP servers can add messages and prompts.
- `--include-partial-messages` adds `stream_event` lines that carry
  Anthropic-API stream events (§2.4). **[C]**
- The working directory is the process cwd (D14). `system/init.cwd` echoes it. **[C]**
- Fallback for an unsupported flag or version: the CLI exits 1 with a
  one-line `Error: ...`. T09 should surface that as a session error.

## 2. Messages

All messages are one JSON object per line. Every stdout message has a `type`.
The fixtures show other fields not listed here, such as `uuid`,
`timestamp`, `usage`, `modelUsage` and `capabilities`. T09 must ignore
unknown fields and unknown `type`/`subtype` values.

### 2.1 stdin → CLI

User message **[C]**. The CLI echoed back `session_id` in its own messages
whatever value was sent here (`""` in every recording):

```json
{"type":"user","message":{"role":"user","content":"Say hi in exactly two words."},"parent_tool_use_id":null,"session_id":""}
```

`content` can also be an array of Anthropic content blocks, which is how the
SDK sends images and blobs. **[U]**

Control request. `request_id` is any unique string that the host chooses. **[C]**

```json
{"type":"control_request","request_id":"req_1_0968b2c1","request":{"subtype":"initialize","hooks":null}}
{"type":"control_request","request_id":"req_2_64946128","request":{"subtype":"interrupt"}}
```

Control response, the reply to a CLI-initiated request (§3). **[C]**

### 2.2 CLI → stdout, per turn

A turn begins when a `user` message is written to stdin. The CLI then emits
the following, in order:

1. `{"type":"system","subtype":"init", "session_id", "cwd", "model", "tools":[...], "permissionMode", "claude_code_version", ...}`.
   This is re-emitted **at the start of every turn**, not only once. **[C]**
2. Zero or more of the following:
   - `{"type":"assistant","message":{"id":"msg_...","role":"assistant","content":[<block>],...},"parent_tool_use_id":null,"session_id":...}`.
     Each `assistant` line carries one content block: `text`, `thinking` or
     `tool_use`. One API message can span several lines that share the same
     `message.id`. **[C]**
     - text: `{"type":"text","text":"Hi there!"}`
     - tool_use: `{"type":"tool_use","id":"toolu_...","name":"Write","input":{"file_path":"...","content":"hi"}}`
     - thinking: `{"type":"thinking","thinking":"","signature":"..."}`. The
       text was empty in the recordings, so treat it as opaque.
   - `{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_...","content":"...","is_error":true?}]},"tool_use_result":...,"tool_result_meta":[...]}`.
     This is the tool output. `tool_use_result` is a structured value whose
     shape depends on the tool, for example `{stdout,stderr,interrupted}` for
     Bash or `{type:"create",filePath,...}` for Write. It is a string
     (`"Error: ..."`) when the tool failed or was denied. **[C]**
   - `control_request` / `can_use_tool` (§3). **[C]**
   - `{"type":"rate_limit_event","rate_limit_info":{...}}`. Informational only;
     ignore it. **[C]**
   - `{"type":"system","subtype":"thinking_tokens",...}` and
     `{"type":"system","subtype":"status","status":"requesting"}`. Progress
     signals only; ignore them. **[C]**
3. `{"type":"result", ...}`. **This ends the turn.** **[C]** Important fields:
   - `subtype`: `"success"` or `"error_during_execution"`
   - `is_error`
   - `result`: the final text, or `null`
   - `stop_reason`: `"end_turn"`, `"tool_use"` or `null`
   - `terminal_reason`: `"completed"`, `"aborted_streaming"` or `"aborted_tools"`
   - `session_id`
   - `permission_denials`: an array of `{tool_name, tool_use_id, tool_input}`
   - `total_cost_usd`, `usage`, `model_usage_since_last_result`,
     `num_turns`, `duration_ms`: which are per turn and which run on is in
     §2.5

   The startup-failure `result` also has `errors:[...]` (§5).

A second `user` message sent after the `result` starts a new turn in the same
process, and the context is kept. **[C]** (`basic.ndjson`: "What did I just
ask you?" was answered correctly.) T09 can therefore keep one process per
session across turns.

### 2.3 Mapping notes for T09

`skywaved` maps these messages to `EventKind`. This is guidance only; T09 owns
the final mapping.

- `assistant` text → agent text.
- `assistant` `tool_use` → tool call (`id` as the agent call id, `name`, a
  display from the input, §6b; the mapper cuts the display and blobs what was
  cut).
- `user` `tool_result` → tool output, correlated by `tool_use_id`; status and
  preview from `tool_use_result` (§6b).
- `can_use_tool` → `ApprovalRequested`.
- `control_cancel_request` → `ApprovalCancelled`.
- `result` → turn ended (`is_error` / `terminal_reason` give the outcome).
- `system/init` → session id and a check of the version and model.

### 2.4 Partial messages (`--include-partial-messages`)

`{"type":"stream_event","event":{...},"session_id":...,"parent_tool_use_id":null}`.
`event` is a raw Anthropic streaming event: `message_start`,
`content_block_start`, `content_block_delta` (`text_delta`, `thinking_delta`,
`signature_delta`, `input_json_delta`), `content_block_stop`,
`message_delta` (with `delta.stop_reason`) or `message_stop`. **[C]**

The full `assistant` message for each block is still emitted, so T09 can
ignore `stream_event` entirely and get correct output, only less live.

### 2.5 Cost, tokens and time in `result` (read off the fixtures, 2026-10-30)

Which figures in `result` are the turn's own and which run on, checked on
every `result` in the fixtures: `basic.ndjson` (two turns in one process),
`resume.ndjson` (a new process resuming `basic`'s session),
`permissions.ndjson`, `interrupt.ndjson` and `interrupt_pending.ndjson`
(several turns each). No new capture was needed. All runs are on one model.

| field | scope | evidence |
|-------|-------|----------|
| `duration_ms` | **the turn**, wall clock, waiting for approvals included | `permissions` turn 1: 153312 ms with the approval held 150 s |
| `usage` | **the turn**, summed over its requests (`usage.iterations`) | `permissions` turn 1 (`num_turns: 2`): `cache_read_input_tokens` 34669 = 15367 + 19302 of its two `assistant` messages |
| `model_usage_since_last_result` | **the turn**, per model, with `costUSD` | `basic` turn 2: `costUSD` 0.00086724 = 0.00229693 − 0.00142969 |
| `total_cost_usd` | **the whole CLI session, across processes** | `basic`: 0.00142969, then 0.00229693; `resume`'s first turn: 0.00251523 = 0.00229693 + 0.0002183 |
| `modelUsage` | the whole CLI session, across processes, per model | `resume`: `inputTokens` 6 = 2 + 2 (`basic`) + 2 |

So `total_cost_usd` is cumulative, and **a resume does not reset it**: the
CLI restores the session's spend from its transcript. A delta of
`total_cost_usd` within one process would be wrong for the first turn after
every restart. The adapter therefore takes the turn's figures directly and
never differences totals (`usage.rs`):

- time: `duration_ms`;
- tokens read: `usage.input_tokens + cache_read_input_tokens +
  cache_creation_input_tokens` (cached input is most of what a turn reads;
  `input_tokens` alone is 2 or 4 in every fixture); tokens written:
  `usage.output_tokens`;
- cost: the sum of `costUSD` over `model_usage_since_last_result` (one
  entry per model the turn used, e.g. a subagent on another model);
- model: the `model` of the turn's `system/init`.

A turn interrupted while text streamed (`interrupt.ndjson` turn 1,
`terminal_reason:"aborted_streaming"`) reports zero `usage`, an empty
`model_usage_since_last_result` and `total_cost_usd: 0`, though its
`assistant` message did consume tokens; the spend is not counted in any later
`result` either (turn 2's `total_cost_usd` is turn 2's own cost). Only its
time is reported for it. A turn interrupted at an approval (`interrupt_pending`,
`aborted_tools`) reports its figures normally.

Not observed: a turn on two models. Summing `costUSD` per model is the
reading of the field's shape, not a capture.

## 3. Permission prompts

### 3.1 The request (CLI → stdout) [C]

```json
{"type":"control_request","request_id":"6587ec1f-4dd9-4cd1-b704-51fcb4f5ec34",
 "request":{"subtype":"can_use_tool","tool_name":"Write","display_name":"Write",
  "input":{"file_path":"/tmp/skywave-t07-scratch/hello.txt","content":"hi"},
  "description":"hello.txt",
  "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],
  "tool_use_id":"toolu_01KUggBJCAEkGBaGqvziMF7r"}}
```

- The `request_id` is a UUID chosen by the CLI. The reply must echo it.
- The `assistant` `tool_use` with the same `tool_use_id` comes **before** the
  request.
- The `description` for Bash is the tool's own description string.
- `permission_suggestions` holds `addRules`, `addDirectories` and `setMode`
  entries, which the SDK uses to offer "always allow". **[C]** seen, e.g.
  `{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"…"}],"behavior":"allow","destination":"localSettings"}`
  and `{"type":"addDirectories","directories":["/tmp/x"],"destination":"session"}`.
  The adapter (`claude_code::rules`) offers the ones that allow more as
  agent-agnostic rule suggestions (label, and scope from `destination`:
  `session`/`cliArg` → session, `localSettings`/`projectSettings` →
  project, `userSettings` → user); see §3.2 for granting one.
- Not every tool call asks. Read-only commands such as `echo` were allowed
  without a prompt (`tool_result_meta.permission_decision.source:"config"`).
  **[C]** Only calls that would need a prompt produce `can_use_tool`.

### 3.2 Allow (stdin → CLI) [C]

```json
{"type":"control_response","response":{"subtype":"success","request_id":"6587ec1f-4dd9-4cd1-b704-51fcb4f5ec34",
 "response":{"behavior":"allow","updatedInput":{"file_path":"/tmp/skywave-t07-scratch/hello.txt","content":"hi"}}}}
```

Pass the original `input` back as `updatedInput`. Omitting `updatedInput` was
not tested, so send it every time. **[U]**

To grant a suggestion with the approval ("always allow"), the response also
carries it, verbatim, as `"updatedPermissions":[<suggestion>]`; the CLI then
keeps the rule at the suggestion's `destination`. **[U]** shape from the SDK's
`PermissionResult`; not recorded.
Result: the tool runs, `tool_result` follows, and
`tool_result_meta[].permission_decision` is `{"decision":"accept","source":"user_temporary"}`.

A granted update with destination `session` or `cliArg` lives in the CLI
process only: `--resume` restores the conversation, not the permission
mode or rules. The adapter therefore adds each one it grants to its resume
handle (`{"session_id":…, "rules":[<update>, …]}`; a later `setMode`
replaces an earlier one) and starts a resumed process with them as flags,
before `--resume`:

| update | flags |
|--------|-------|
| `setMode` `acceptEdits` / `bypassPermissions` | `--permission-mode <mode>` |
| `addRules` (`allow`) | `--allowedTools <Tool(content)>` per rule |
| `addDirectories` | `--add-dir <dir>` per directory |

**[C]** with 2.1.310: after `--resume` with `--permission-mode acceptEdits`,
`system/init` reports `permissionMode:"acceptEdits"` and a Write runs
without `can_use_tool`; `--add-dir` shows in `additional_directories`;
two `--allowedTools 'Bash(touch x)'` flags both apply (the same command
without one asks). `bypassPermissions` on resume is **[U]**.

### 3.3 Deny (stdin → CLI) [C]

```json
{"type":"control_response","response":{"subtype":"success","request_id":"5446b928-31cd-4fb3-b499-1f4fccf50f2a",
 "response":{"behavior":"deny","message":"The user denied this action."}}}
```

Result: the CLI emits a `tool_result` with `is_error:true` whose `content` is
the `message`, and the model **continues the turn**. In the recording it
replied with text and then `result/success`. The denial is listed in
`result.permission_denials`.

The SDK also supports `"interrupt": true` on a deny, to end the turn
immediately. **[U]** Not needed: for that, T09 can deny and then send an
interrupt (§4).

### 3.4 Indefinite wait (D8)

An approval held for **150 s** and then allowed worked normally, with no
keep-alive or error messages during the wait. **[C]**
Waits longer than that were not tested. No CLI-side permission timeout is
documented. **[U]**

Fallback if a CLI-side timeout ever appears: it would surface as a
`control_cancel_request` or an error `tool_result` for that `tool_use_id`.
T09 must handle both anyway (§4.2, §6). Map either one to
`ApprovalCancelled` with a reason, so the log never shows an approval that
hangs forever.

## 4. Interrupt

Request **[C]**:

```json
{"type":"control_request","request_id":"req_2_64946128","request":{"subtype":"interrupt"}}
```

The response always arrives quickly:
`{"type":"control_response","response":{"subtype":"success","request_id":"req_2_64946128","response":{"still_queued":[]}}}`.
**[C]** The same success response came back when the CLI was idle with no
turn running.

### 4.1 While text streams [C] (`interrupt.ndjson`)

The CLI emits, in order:

1. the control_response
2. a partial `assistant` message (the text so far)
3. `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}`
4. `stream_event` `message_stop` (with partial messages on)
5. `result` with `subtype:"error_during_execution"`, `is_error:true`,
   `terminal_reason:"aborted_streaming"`, `result:null`

### 4.2 While an approval is pending [C] (`interrupt_pending.ndjson`)

The CLI emits, in order:

1. `{"type":"control_cancel_request","request_id":"<the can_use_tool request_id>"}`
   → T09 maps this to `ApprovalCancelled`.
2. the interrupt's control_response
3. a `tool_result` with `is_error:true`: "The user doesn't want to proceed with this tool use..."
4. the user message `[Request interrupted by user for tool use]`
5. `result` with `error_during_execution`, `terminal_reason:"aborted_tools"`,
   and the tool listed in `permission_denials`

A **late** `control_response` for the cancelled request, sent 3 s later, was
silently ignored. There was no error, and the file was not written. **[C]**
T09 should still drop answers to approvals that are no longer pending, rather
than rely on that.

### 4.3 Process still usable [C]

After both interrupt cases, the next `user` message ran a normal turn in the
same process, with the same session id.

## 5. Session id and resume

- `--session-id <uuid>` sets the session id. Every `session_id` field and the
  transcript file use it. **[C]** T09 can therefore generate the id up front
  and store it as the opaque `agent_state` before the first turn finishes.
- Without `--session-id`, the CLI picks a UUID. It is first visible in
  `system/init.session_id` and is repeated on every message. **[C]**
- `--resume <uuid>` in a **new** process restores the history (`resume.ndjson`
  quotes the first prompt of `basic`) and keeps the **same** session id.
  **[C]** `--fork-session` would create a new id instead. **[U]** Not needed.
- Reusing an existing id with `--session-id` fails:
  `Error: Session ID <id> is already in use.`, exit 1. **[C]** So the first
  start uses `--session-id`, and every restart uses `--resume`.
- Resuming an unknown id fails. The CLI prints
  `No conversation found with session ID: <id>`, then a `result` with
  `subtype:"error_during_execution"`, `num_turns:0` and `errors:[...]`, and
  exits 1. **[C]** Fallback for T09: surface a session error. Silently
  starting a fresh session would lose history.
- Sessions persist under `~/.claude/projects/<cwd with / replaced by ->/<id>.jsonl`.
  The resume must run with the **same cwd**: lookup is per project directory.
  The per-cwd layout is **[C]**; resuming from a different cwd was not tested
  **[U]**. D14 already fixes the workdir per session, so this holds.
  `--no-session-persistence` would make resume impossible, so it must not be
  in `agent.args`.

## 6. Lifecycle details

- **initialize handshake [C].** The host sends
  `control_request {subtype:"initialize","hooks":null}`. The CLI answers with
  a `control_response` whose `response` holds `commands`, `agents`, `models`,
  `account` (personal data: email and org), `pid`, `claude_code_version`,
  `current_permission_mode`, `session_state:"idle"` and `capabilities`.
  T09 should send it, read `claude_code_version` from it, and not log
  `account`.
- **Is the handshake required? [U].** All recordings sent it, and `system/init`
  only appeared after the first user message. Sending it costs nothing.
- **stdin EOF while idle [C].** The CLI exits 0 about 1 s later.
- **stdin EOF during a turn [C].** A pending permission fails right away
  (`tool_result`: "Tool permission request failed: AbortError: Tool permission
  stream closed before response received"). The model **keeps running**: it
  retried the tool, which failed with "Stream closed", then wrote text and
  `result/success`. The CLI exited only after that. So closing stdin is
  **not** a way to stop work. To stop a turn, T09 sends `interrupt` and
  waits for `result`. To stop the process, it closes stdin when idle, or
  kills the process if it does not exit within a grace period.
- **stderr [C].** Empty in every stream-json run. Startup errors (bad flags,
  an unknown resume id, a session id already in use) are printed as one line,
  `Error: ...` or `No conversation found ...`, and exit 1. T09 should capture
  stderr into the daemon log.
- **No CLI timeout while idle [C].** In the interrupt run the process sat idle
  for about 240 s between turns with no output, and then served the next turn.

## 6a. How T09 implements it

`skywaved::agent::claude_code` (`protocol.rs` is the pure translator,
replay-tested on the fixtures in `tests/claude_code.rs`). Choices beyond the
notes above:

- **Mid-turn messages are queued by the adapter**, not sent: the next user
  message is written only after the turn's `result`. So each written message
  is exactly one turn, and the [U] CLI-side queueing (§7) is never relied on.
- **`start` waits for the initialize reply.** A CLI that exits or reports an
  error `result` before it is a start error; with `--resume` and
  `No conversation found` in the error text, a resume error (§5). If a CLI
  ever reports the unknown id only after the handshake, it surfaces as
  `Exited { error }` instead, which still marks the session errored.
- **An error `tool_result` for a call whose approval is pending** (stdin
  closed, or a future CLI-side timeout, §3.4) reports `ApprovalCancelled`
  with the result's first line as the reason.
- **Approvals still pending at `result`** are cancelled (`interrupted` or
  `turn ended`) before `TurnEnded`.
- **The turn's usage** (§2.5) is reported as `TurnUsage` just before
  `TurnEnded`, per-turn figures only; nothing is carried between turns or
  processes, so a restart needs no reset. Core sums them per session.
- **Turn outcome:** `success` and not `is_error` → completed; otherwise
  interrupted if an interrupt was sent this turn or `terminal_reason` starts
  with `aborted`; otherwise failed with `errors` (or `result`) as message.
- **Other CLI control requests** (hooks, MCP) get an `error`
  control_response so the CLI does not wait on them.
- The deny message is `The user denied this action.`; the version check
  warns when `claude_code_version` is not `2.1.310`.

## 6b. Tool displays (Tc)

`claude_code/display.rs` maps each `tool_use` onto an agent-agnostic
`ToolDisplay` from its **input alone**, so an approval for the call (which the
mapper summarises by the call's title) shows the same thing, an edit's diff
included, before the tool runs. Content is handed over uncut; the mapper cuts
it and blobs what was cut. `shows_all_input` is false when the input has a
non-null field the display does not show (e.g. Bash `timeout`), so the mapper
keeps the input as a blob.

| tool | kind | title | detail | body |
|------|------|-------|--------|------|
| `Write` | `FileWrite` | `file_path` | - | `content` (text) |
| `Edit` | `FileEdit` | `file_path` | `all occurrences` if `replace_all` | diff of `old_string` → `new_string` |
| `MultiEdit` | `FileEdit` | `file_path` | `N edits` | the edits' diffs, each after a `@@` line |
| `Bash` | `Shell` | `$ <command>` | `description` | - |
| `Read` | `FileRead` | `file_path` | `lines a–b` / `from line a` / `first N lines` | - |
| `Grep` | `Search` | `pattern` | `path · glob · type` (those given) | - |
| `Glob` | `Search` | `pattern` | `path` | - |
| `WebFetch` | `Fetch` | `url` | `prompt` | - |
| `WebSearch` | `Fetch` | `query` | - | - |
| `Task` / `Agent` | `Subagent` | `description` | `subagent_type` | `prompt` (text) |
| anything else, or a known tool missing its title field | generic (`Other`, the mapper's display) | | | |

The diff is a line diff (LCS after trimming common head and tail lines), one
line each prefixed `-`, `+` or ` `, no hunk headers; past 2^20 line pairs it
shows all old lines removed and all new ones added.

**Results** are read from the `user` message's structured `tool_use_result`
(when the message holds one `tool_result`, as the CLI sends them), by the
tool its `tool_use_id` named. The model-facing `tool_result` content is
always the whole `output`; the display sets a separate `preview` (and the
mapper then blobs the output) so notes such as "file state is current in your
context" or "STOP what you are doing" stay out of the preview.

| `tool_use_result` | status | preview |
|-------------------|--------|---------|
| string `"Error: The user denied this action."` (our deny message, §3.3) | `denied` | the message |
| string `"User rejected tool use"` (interrupt, §4.2) or containing `AbortError` (stdin closed, §6) | `cancelled` | the message |
| string `"Error: Exit code N\n…"` | `exit N` | the rest |
| any other string | - | the string without `Error: ` |
| Bash `{stdout, stderr, interrupted}` | `interrupted` if so | stdout, then stderr |
| Write/Edit/MultiEdit `type:"create"` | `created` | empty (the call shows the content) |
| Write/Edit/MultiEdit with `structuredPatch` | `updated +a -b` (lines added/removed in the patch) | empty (the call shows the diff) |
| Read `{file:{content,numLines}}` | `N lines` | `file.content` (no line numbers) |
| Grep | `N files` / `N lines` (mode `content`) / `N matches` (mode `count`) | `content`, else `filenames` |
| Glob | `N files` | `filenames` |
| WebFetch | `<code> <codeText>` | `result` |
| other, or no `tool_use_result` | - (an error's text is still read as a string above) | the output |

`is_error` is always the `tool_result`'s own flag. The core shows a denied or
cancelled approval as the call's outcome, so `denied` / `cancelled` match it.
Only the Write, Bash and error shapes are confirmed by the fixtures; the
others follow the CLI's documented result types and fall back to the plain
output when a field is missing.

## 7. Not covered (out of MVP scope or not needed by T09)

- Images and attachments in `user.content`
- removing rules granted with "always allow" (they live in the CLI's
  settings, out of Skywave's sight)
- hooks over the control channel
- `set_permission_mode`
- MCP over stdio
- `--replay-user-messages`
- sending a second user message while a turn runs (`still_queued` suggests the
  CLI queues it)
