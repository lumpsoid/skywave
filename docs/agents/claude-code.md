# Claude Code adapter: the stream-json protocol (T07 spike)

**Pinned version: Claude Code CLI `2.1.310`**. The version comes from
`claude --version`. It is also reported as `claude_code_version` in both the
`initialize` response and `system/init`.
Recorded on 2026-10-29 on Linux. The fixtures are in
`crates/skywaved/tests/fixtures/claude-code/`; their `README.md` describes
each file.

This protocol is what the Agent SDKs use internally. It is **not** a
documented stable API (see spec §11). The adapter reads `claude_code_version`
from the `initialize` answer (or the first `system/init`), and once per
process, when it differs from the pinned version, logs a warning and
reports `VersionUnconfirmed`. The session log gets an
`AgentVersionUnconfirmed` event, so every head shows the drift in the
session's timeline.

**No self-updates.** Every CLI process is spawned with
`DISABLE_AUTOUPDATER=1`, so an overnight auto-update cannot change the
protocol under a running server (a changed `result` or `control_response`
shape would be skipped as unknown, and a turn would stay open until
Stop). **[C]** with 2.1.310, from the binary
(`~/.local/share/claude/versions/2.1.310`): the updater's check reads
`process.env.DISABLE_AUTOUPDATER` and reports "set by env:
DISABLE_AUTOUPDATER". It stops only the automatic updater; `claude update`
by hand still works. `DISABLE_UPDATES` would block that too, which is the
operator's call, so the adapter does not set it.

**Checking a new CLI.** After every CLI upgrade, run
`cargo run -p skywave-claude-compat` (`--quick`, the default: no model
call, seconds, no tokens; control requests, plus `/rename`, a local
command, where resume, fork and the transcript need a conversation to
exist), then `-- --full` (real turns on haiku in
a scratch dir, a few minutes, including a `/loop` tick and a background
shell). It drives the real CLI through the adapter's own `Protocol`, so it
writes exactly what skywaved writes, and checks every stdout line against
the fields the adapter reads. Each scenario reports **pass**, **BROKEN**
(a contract check failed: the upgrade breaks skywaved) or **NEWLY
AVAILABLE** (a watch check changed: something we wait for, such as
`send_task_message`, now behaves differently; the report names the decision
to revisit). **inconclusive** means the model did not do what the prompt
asked, so run that scenario again (`--only <name>`). The prompts are a
small play, a user running a bakery in the scratch folder, but each still
names the exact tool and command a check needs. `--list` names the
scenarios. `--record <dir>` writes each scenario's scrubbed recording in
the fixture format. Once every scenario passes, move `PINNED_VERSION` and
the pin above to the new version.

Legend: **[C]** means confirmed, i.e. seen in a fixture or a probe run against
2.1.310. **[U]** means unconfirmed: inferred from SDK behaviour or the help
text, and not seen.

## 0. Summary for T09

| # | capability | status |
|---|------------|--------|
| 1 | Long-lived stream-json process: send user messages, read text, tool_use, tool_result and result | **[C]** |
| 2 | Permission prompt over stdio (`can_use_tool`), allow and deny replies, a long wait | **[C]** (waits ≥150 s seen; a wait with no limit at all is [U], see §3.4) |
| 3 | Interrupt over stdin, while streaming and while an approval is pending; the process stays usable | **[C]** |
| 4 | Session id (`--session-id` and `system/init`), `--resume` in a new process, `--resume --fork-session` with a chosen id | **[C]** |
| 5 | initialize handshake, exit when stdin closes, stderr | **[C]**, with one caveat: stdin EOF does not end the current turn (§6) |

The spec needs no change.

## 1. Process invocation

```
claude -p \
  --input-format stream-json --output-format stream-json --verbose \
  --permission-prompt-tool stdio \
  --settings '{"cleanupPeriodDays":36500}' \
  [--allow-dangerously-skip-permissions] \
  [--session-id <uuid> | --resume <uuid> |
   --resume <parent> --fork-session --session-id <uuid>] \
  [--include-partial-messages] [--model <m>]
```

skywaved starts a fresh context with `--session-id`, a restart with
`--resume`, and a context forked from another's conversation with
`--resume <parent> --fork-session --session-id <new uuid>` (§5).

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
- `--settings '{"cleanupPeriodDays":36500}'` keeps the CLI's retention
  sweep from deleting the transcripts `--resume` needs (§5, Transcript
  retention).
- `--allow-dangerously-skip-permissions` is passed only when the server
  allows bypass (`agent.allow_bypass`, §3.6). **[C]** with 2.1.310: the
  session still starts in `default`, and `set_permission_mode
  bypassPermissions` then succeeds instead of `bypass_not_launched`.
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
keeps the rule at the suggestion's `destination`. **[U] Unverified — pending
capture (2026-10-30).** The shape comes from the SDK's `PermissionResult`; no
fixture holds such a reply yet. §3.5 is the runbook that records one.
Result: the tool runs, `tool_result` follows, and
`tool_result_meta[].permission_decision` is `{"decision":"accept","source":"user_temporary"}`.

A granted update with destination `session` or `cliArg` lives in the CLI
process only: `--resume` restores the conversation, not the permission
mode or rules. The rule was granted for the Skywave session, though, which
outlives the process and spans its contexts. The adapter therefore adds
each one it grants to the session's grants (`{"rules":[<update>, …]}`; a
later `setMode` replaces an earlier one), reports the whole set anew, and
the daemon stores it per Skywave session in `sessions.agent_grants`, apart
from each context's resume handle. Every later process of the session,
fresh (`--session-id`) or resumed (`--resume`), in whichever context, is
started with them as flags, before the session flag:

| update | flags |
|--------|-------|
| `setMode` `acceptEdits` / `bypassPermissions` | `--permission-mode <mode>` (`bypassPermissions` only while `agent.allow_bypass`) |
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

### 3.5 Runbook: capture "always allow" (task d29758)

**Status: unverified — pending capture (2026-10-30).** Nobody has yet
recorded the `updatedPermissions` reply of §3.2 against a real CLI. This
runbook records one real session in which skywaved grants three rules: a
tool rule (`addRules`), a directory (`addDirectories`), and file edits
(`setMode` `acceptEdits`). The recording then becomes the fixture
`tests/fixtures/claude-code/rules_granted.ndjson`. The test
`replay_granted_rules_are_applied_by_the_cli` in `tests/claude_code.rs`
checks two things. (a) Each allow that grants a rule carries the suggestion
unchanged as `updatedPermissions`, byte for byte what the adapter writes.
(b) After each grant, the next call of the same tool gets no `can_use_tool`.
The test is `#[ignore]`d until the fixture exists.

`skywave-claude-compat --only permissions` checks a quicker version
automatically. It grants a `Bash(…)` rule through the adapter, then runs the
same command again. If the call is asked again, the scenario is BROKEN. If
it is not asked, it reports the grant as newly available: the reply shape
works, and this runbook is worth running to record the fixture.

The recorder is `crates/skywaved/tests/support/record-claude.sh`. You set it
as skywaved's agent command. It runs the real CLI and writes every stdin and
stdout line to an NDJSON file in the fixture format (see the fixtures'
`README.md`). So the recording holds what skywaved really sent. Needs `jq`.
Below, `$REPO` is the repository root.

**1. Prepare** a scratch workdir and a directory outside it:

```sh
mkdir -p /tmp/skywave-d29758/work /tmp/skywave-d29758-outside
echo a > /tmp/skywave-d29758-outside/a.txt
echo b > /tmp/skywave-d29758-outside/b.txt
```

**2. Point skywaved at the recorder.** Stop the running daemon. Then edit
`~/.config/skywave/skywaved.toml` and keep a copy of the old file:

```toml
roots = ["/tmp/skywave-d29758", ...]   # add the scratch dir
[agent]
command = "$REPO/crates/skywaved/tests/support/record-claude.sh"   # absolute path
args = ["--model", "haiku", "--setting-sources", "project,local", "--strict-mcp-config"]
```

The `args` are the same as the earlier fixtures, with one change: `local` is
added. The CLI keeps a project rule in the workdir's
`.claude/settings.local.json`, and `local` makes it load that file. Start the
daemon with recording on:

```sh
SKYWAVE_RECORD=/tmp/skywave-d29758/rules_granted.raw.ndjson skywaved serve
# if `claude` is not on the daemon's PATH, also set SKYWAVE_REAL_CLAUDE=/path/to/claude
```

**3. Run the session** in `skywave-tui`. Press `n` and pick workdir
`/tmp/skywave-d29758/work`. Send the six messages below in order. Send each
one only after the previous turn ended, and keep the gaps under 5 minutes
(the idle timeout). Otherwise the CLI restarts, gets the rules as flags
(§3.2), and the recording proves nothing. `A` approves and grants a rule.
With several rules offered, pick one with `↑↓` and `Enter`. Never pick a
`user` scope rule: it would write to your own `~/.claude/settings.json`.

| # | message | expected | do |
|---|---------|----------|----|
| 1 | `Use the Bash tool to run exactly: touch rule-check` | approval for Bash | `A`, pick `Bash(touch rule-check)` (`addRules`) |
| 2 | `Use the Bash tool to run exactly the same command again: touch rule-check` | **no** approval | — |
| 3 | `Use the Read tool to read /tmp/skywave-d29758-outside/a.txt` | approval for Read | `A`, pick `access to /tmp/skywave-d29758-outside` (`addDirectories`) |
| 4 | `Use the Read tool to read /tmp/skywave-d29758-outside/b.txt` | **no** approval | — |
| 5 | `Use the Write tool to create one.txt containing 1` | approval for Write | `A`, pick `file edits` (`setMode acceptEdits`) |
| 6 | `Use the Write tool to create two.txt containing 2` | **no** approval | — |

If the model calls another tool of the same kind in between, or a step
offers a different rule, write down what happened and record again. The
test only looks at the next call of the same tool. If step 3 offers no
`access to …`, record what it did offer, and try the same step with `ls`
through Bash.

Then press `S` on the timeline to stop the agent. This closes its stdin, and
the recorder adds `closing stdin` and `exit 0`. Stop the daemon and put the
old config back.

**4. Scrub and install** the fixture. The scrubbing is the same as in the
fixtures' `README.md`:

```sh
cd $REPO/crates/skywaved
tests/support/record-claude.sh --scrub /tmp/skywave-d29758/rules_granted.raw.ndjson \
  tests/fixtures/claude-code/rules_granted.ndjson
grep -c '"dir":"note","text":"argv' tests/fixtures/claude-code/rules_granted.ndjson   # must be 1
ls /tmp/skywave-d29758/*.stderr 2>/dev/null   # the CLI's stderr, only if there was any
```

Read the file once and check for anything private the scrub missed.

**5. Confirm.** The quick look below lists each grant, then each tool call
with whether the CLI asked for it. Every call after a grant should show
`asked: false`:

```sh
jq -c 'select(.dir=="in") | .msg.response.response.updatedPermissions // empty' \
  tests/fixtures/claude-code/rules_granted.ndjson
jq -sc '[.[] | select(.dir=="out") | .msg] as $m
  | ([$m[] | select(.request.subtype=="can_use_tool") | .request.tool_use_id]) as $asked
  | $m[] | select(.type=="assistant") | .message.content[] | select(.type=="tool_use")
  | {tool: .name, input, asked: (.id as $id | $asked | index($id) != null)}' \
  tests/fixtures/claude-code/rules_granted.ndjson
```

Then remove the `#[ignore]` from `replay_granted_rules_are_applied_by_the_cli`
and run:

```sh
cargo test -p skywaved --test claude_code replay_granted_rules
```

If it passes, mark §3.2 **[C]**, add the file to the fixtures' `README.md`,
and note the CLI version (from `system/init`) and the
`tool_result_meta[].permission_decision.source` of the calls that were not
asked. If the version differs from `PINNED_VERSION`, the other replay tests
still pin 2.1.310. This one accepts any version. If a call is asked again,
the reply shape is wrong. Record what the CLI does instead (does it change
`system/init.permissionMode`? does it write `.claude/settings.local.json`?)
before changing `protocol::allow_always_response`.

### 3.6 Bypass only where the operator allows it (D38)

Decided (D38): bypass, where every tool runs and nothing
asks, is available from a head only when the server's operator set
`[agent] allow_bypass = true` in `skywaved.toml`. It is off by default.

- **Launch.** With the opt-in, every CLI process gets
  `--allow-dangerously-skip-permissions` (§1): it starts in its usual mode
  and may be switched to `bypassPermissions` mid-session (§3.7). Skywave
  never launches with `--dangerously-skip-permissions`, which would start
  every session in bypass. Checked with 2.1.310 (`--help` lists both
  flags; `skywave-claude-compat --only bypass_allowed`).
- **Mode switch.** Without the opt-in the server refuses a
  `SetPermissionMode` to bypass itself, as `RejectReason::BypassDisabled`,
  before the CLI is asked (§3.9). The heads offer bypass only when
  `HelloResponse::bypass` says the server allows it, and ask once more
  before sending it.
- **No back door.** `agent.args` may not hold
  `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions`
  or `--permission-mode bypassPermissions` (two arguments or `=`-joined):
  the config refuses them (`claude_code::bypass_arg`), naming
  `allow_bypass`.
- **Rule suggestions.** `rules::offered` offers a `setMode`
  `bypassPermissions` suggestion, labelled `every tool`, only with the
  opt-in, and only when the CLI itself puts it in a request's
  `permission_suggestions` (Skywave never makes one up; none of the
  recorded requests had one). Without the opt-in it is dropped, so no
  approval can grant it. A session-scoped one granted while the opt-in was
  on is kept in the session's grants, but `rules::cli_args` passes it back
  as `--permission-mode bypassPermissions` only while the opt-in is still
  on: turning it off takes bypass away from every later start. Whether
  that flag holds on resume is **[U]** (§3.2).

### 3.7 Changing the permission mode mid-session [C]

Recorded with 2.1.310 on 2026-10-31 (`permission_modes.ndjson`,
`permission_modes_plan.ndjson`, `permission_mode_errors.ndjson`). This is
what the interactive CLI cycles with shift+tab. skywaved sends it for a
`SetPermissionMode` command and to reapply a session's mode on start
(§3.9).

**Request (stdin → CLI)**, at any time, also before the first user message
and while an approval is pending:

```json
{"type":"control_request","request_id":"<uuid>","request":{"subtype":"set_permission_mode","mode":"acceptEdits"}}
```

`mode` is one of `default` (the UI's "manual"), `acceptEdits`, `plan`,
`auto`, `dontAsk`, `bypassPermissions`.

**Success (CLI → stdout)**: a `control_response` that echoes the mode, then
a status line:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"…","response":{"mode":"plan"}}}
{"type":"system","subtype":"status","status":null,"permissionMode":"plan","uuid":"…","session_id":"…"}
```

The `control_response` is the ack. The status line can come later, in
some runs only after the next stdin line, so don't wait for it. From the next
turn on, `system/init.permissionMode` and `initialize`'s
`current_permission_mode` both report the new mode. The change holds in
the running process; across `--resume` it is lost like any session state
(§3.2).

**Rejection**: an error `control_response` with an `error_code`; the mode
stays as it was.

| `mode` | `error_code` | `error` |
|--------|--------------|---------|
| `bypassPermissions` | `bypass_not_launched` | `Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions` |
| anything unknown | `invalid_mode` | `Cannot set permission mode: must be one of acceptEdits, auto, bypassPermissions, default, dontAsk, plan` |

**Effect**, one tool call per turn, the next turn after the switch:

| mode | Write in workdir | Bash `rm <file in workdir>` |
|------|------------------|-----------------------------|
| `default` | `can_use_tool` | (asks, §3.1) |
| `acceptEdits` | runs, not asked | runs, not asked |
| `plan` | not run; see below | — |
| `auto` | runs, not asked | runs, not asked |
| `dontAsk` | not asked; error `tool_result` "Permission to use Write has been denied because Claude Code is running in don't ask mode…", and a `system/permission_denied` line | — |

`auto` decides on its own, so it may still ask for riskier calls than these;
only these two were recorded.

**Plan mode.** The model writes its plan to `~/.claude/plans/<slug>.md`
without being asked, then calls `ExitPlanMode`. That comes as a normal
`can_use_tool` with `input.plan` (the plan as markdown) and
`permission_suggestions: null`. Allowing it sends a `system/status` that
returns to **the mode before plan**: `default` when plan was entered from
`default`, `acceptEdits` when entered from `acceptEdits`. The model then does
the work under that mode. So approving a plan is an ordinary approval with a
mode change as a side effect, and a client must take the mode from the
status line, not assume one.

**A pending approval is not settled by a mode change.** Switching to
`acceptEdits` while a Write's `can_use_tool` waited sent no
`control_cancel_request`. The CLI went on waiting until it got the answer
(10 s here). The client still has to answer every approval it shows.

### 3.8 Changing the model mid-session [C]

Recorded with 2.1.310 on 2026-10-31 (`model_switch.ndjson`): control
requests only, no user message, so no model call. A turn after a switch
confirmed it takes effect: `system/init.model` and the `result`'s
`model_usage_since_last_result` named the new model.

**Request (stdin → CLI)**, at any time, like `set_permission_mode`:

```json
{"type":"control_request","request_id":"<uuid>","request":{"subtype":"set_model","model":"sonnet"}}
```

`model` is an alias or id: one of the `value`s in the `models` list of the
`initialize` answer (`default`, `opus`, `sonnet`, `haiku`, full ids …) or any
model id the CLI knows. `default` (or no `model` at all) switches to the
account's default.

**Success**: a `control_response` with the model **as the CLI resolved
it**; no status line follows, and `initialize` does not report the current
model (only `system/init` at the next turn does):

```json
{"type":"control_response","response":{"subtype":"success","request_id":"…","response":{"model":"claude-sonnet-5-5"}}}
```

**Rejection**: the model stays as it was.

```json
{"type":"control_response","response":{"subtype":"error","request_id":"…","error":"Model 'bogus-model' not found","error_code":"not_found","model":"claude-opus-4-7"}}
```

**Offered models.** The `initialize` answer lists them: `value` (what
`set_model` takes), `displayName`, `description`, `resolvedModel`, plus
effort and thinking capabilities. The effort ones are read: a model with
`supportsEffort: true` takes the levels in `supportedEffortLevels` (e.g.
`["low","medium","high","xhigh","max"]`; `claude-opus-4-6` has no `xhigh`),
which become `ModelChoice.efforts`; a level skywaved has no name for is
skipped. `claude-haiku-4-5-20251001` has neither field and takes none. The
thinking capabilities are not used.

### 3.9 How skywaved drives mode, model and effort

- A `SetPermissionMode` / `SetModel` / `SetEffort` command logs a
  `…Requested` event; the supervisor sends `set_permission_mode` /
  `set_model` / the effort chain (§3.10) and logs the answer as
  `PermissionModeChanged` / `ModelChanged` / `EffortChanged` or
  `…Refused`, naming the command (`answers`). A session whose agent is not
  running is started for it (no message, so no model call).
- Every agent start asks for the mode, model and effort the log wants (the
  open request, else what the agent last ran with), in that order, with
  these same control requests after `initialize`, so a resumed or restarted
  process keeps all three (§3.2: they die with the process otherwise).
- What the CLI says unasked is logged when it differs from the log:
  `current_permission_mode` from `initialize`, `permissionMode` and `model`
  from each `system/init`, and a `system/status` that is not the echo of a
  switch the adapter asked for (an approved `ExitPlanMode`, §3.7). The
  `models` of `initialize` are logged as `ModelsOffered` when they differ
  from the newest list in the log. The effort is said by nobody unasked,
  so the adapter reads it (`get_settings`, §3.10) after the `initialize`
  answer and after every successful `set_model`, and logs it like the rest.
- Bypass is accepted only where the server allows it (`agent.allow_bypass`,
  §3.6). Without the opt-in the command is rejected `BypassDisabled` and
  nothing is logged or sent to the CLI; with it the CLI is launched with
  `--allow-dangerously-skip-permissions` and the switch goes through like
  any other. A bypass request accepted while the opt-in was on and
  reached after a restart without it is refused by the supervisor itself
  (`PermissionModeRefused`, `supervisor::BYPASS_NOT_ALLOWED`), and a start
  never asks for bypass without the opt-in, even when the log last had it.
  A `bypass_not_launched` refusal can now come only from an
  `agent.command` that ignores the flag; it would be logged as
  `PermissionModeRefused` like any refusal.

### 3.10 Changing effort mid-session [C]

Recorded with 2.1.310 on 2026-10-31 (`effort_switch.ndjson`): control
requests only, no user message, so no model call.

**Request (stdin → CLI)**: the flag-settings layer, at any time:

```json
{"type":"control_request","request_id":"<uuid>","request":{"subtype":"apply_flag_settings","settings":{"effortLevel":"high"}}}
```

`effortLevel` is one of `low`, `medium`, `high`, `xhigh`, `max`.

**The ack proves nothing.** Success is an empty `control_response`, with
no `response` body:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"…"}}
```

and the same comes back for `"bogus"`, which is silently ignored, and for
a level the running model does not take. The binary can also answer an
error (`apply_flag_settings: CLAUDE_CODE_EFFORT_LEVEL overrides effort for
this session`, `…unrecognized effortLevel`); none was recorded.

**The effort in force** comes from `get_settings`:

```json
{"type":"control_request","request_id":"<uuid>","request":{"subtype":"get_settings"}}
{"type":"control_response","response":{"subtype":"success","request_id":"…","response":{"applied":{"model":"claude-haiku-5-5","effort":"high", …},"effective":{…},"sources":[…]}}}
```

`applied.effort` is a level, or `null` when the running model takes none
(`claude-haiku-4-5-20251001`). Don't read `effective.effortLevel`: that is
the flag layer, and it is `null` for `max`, which is session-only. The
answer's `effective` (the merged settings layers) and `sources` can hold the
user's `env`, hooks and permission rules: **never log the body**; the
adapter reads only `applied.effort` and `applied.model`, and the fixture
keeps only `applied`. Neither `initialize` nor `system/init` reports the
effort.

**The level outlasts model switches.** It is a flag, not a property of the
model: `high` on haiku 5.5, then `set_model sonnet`, reads `high`. On a
model that takes none it reads `null`, but it is kept: `high` asked on
haiku 4.5 reads `null`, and switching back to sonnet reads `high`.

**How skywaved settles a request** (prefer the CLI's own read over the ack):

- An `EffortRequested` writes the apply; its success writes a
  `get_settings`, which is the answer: `EffortChanged` when `applied.effort`
  is the level asked, else `EffortRefused` (`kept <level>`, or `<model>
  takes no effort level` for `null`) followed by the level in force,
  logged unasked. An apply error, a read error, or an `applied.effort`
  skywaved has no name for refuses the request; each request gets exactly
  one answer.
- **One chain at a time.** The read goes out only once the apply is
  answered (they are not pipelined), and an effort asked while a chain
  runs waits for it. The supervisor asks every open request as it reaches
  it, so two requests can reach the adapter back to back; without the wait,
  stdin would be `apply A, apply B, read, read`, the first read would see
  B, and A would be refused though the CLI applied it.
- The unasked reads (after `initialize`, after each successful `set_model`)
  report what runs. One answered while a chain runs or waits reports
  nothing: the chain's own read is written later, so it is newer. A chain
  that ends without a read (an apply or read error) with no other waiting
  writes the dropped read again, so what it saw still reaches the log.
- **Start order.** `start` writes `set_model` and the effort's apply back to
  back. The CLI handles stdin in order, and in the recording (step 7:
  `set_model opus` and `xhigh` pipelined) the read after the apply showed
  `xhigh` on opus, so the start effort is not held for the model's answer.
  Since the level outlasts the switch, the order would only matter for the
  reply to a model that takes none, which reads `null` either way.

### 3.11 Background tasks and schedules [C]

Work the agent runs outside a turn: shells started with
`run_in_background`, Monitors, background subagents, and cron jobs
(`CronCreate`, `/loop`). Skywave reports tasks and schedules as two whole
sets (D31). Fixtures: `background_bash`, `subagent`,
`subagent_background_bash`, `stop_task`, `stop_hook`, `loop`, `loop_resume`,
`subagent_approval`, `interrupt_spares_tasks`, `interrupt_kills_tasks`.

**The level signal.** `{"type":"system","subtype":"background_tasks_changed",
"tasks":[…]}` carries the CLI's whole live set each time it changes
(REPLACE semantics: the last event wins). A task has `task_id`, `run_id?`,
`task_type` (`local_bash`, `local_agent`, …), `subagent_type?`,
`description`, `parent_task_id?` (a shell started by a subagent names it)
and `ambient?`. Notes:
- Nothing is sent at process start; the adapter reports an empty set when
  the agent is ready.
- A Monitor is a `local_bash` task. List order is not start order.
- The edge events (`task_updated`, `task_progress`, `task_notification`) are
  not used for state: a missed edge would wedge a set, a missed level event
  cannot (the next one replaces it). `task_started` is used for `local_agent`
  only, to map the task id to its `Agent` call (`tool_use_id`); the set is
  then reported again with `call` filled. The level set stays the only source
  of which tasks are live.
- A foreground Bash gets edge events but never enters the level.

**Subagent messages.** A subagent's `assistant` and `user` messages carry
`parent_tool_use_id` and may arrive after the main `result`. They are
reported with `parent` = `parent_tool_use_id` (the `Agent` call) and shown in
that subagent's own view (D34), not on the main timeline. Tool names are kept
per subagent call across turns. Text in subagent `user` lines (including a
foreground subagent's prompt) and thinking stay dropped. A subagent's
`tool_result` lines carry no `tool_use_result` in any recording.

**Self-started turns.** When a task ends or a cron fires, the CLI starts a
turn with no user message: `system/init`, a whole turn, `result` (a cron's
turn is framed by `command_lifecycle`). Such a turn counts as work for the
idle timer, so the agent is not stopped under it.

**Keep-alive.** A live non-ambient task keeps the agent running; it is
counted from the uncut set. The idle stop (`idle_timeout`) runs once the set
is empty. Schedules never keep the agent alive.

**`stop_task`.**
`{"subtype":"stop_task","task_id":"…"}` (`stop_task`). The success response
is `{}` even for an unknown or already ended id; the only error is a
non-string id (`stop_task: task_id must be a string`), which the adapter
never sends. The CLI sends `task_updated` (`killed`), `task_notification`
(`stopped`) and the level without the task, **then** the success. Stopping a
subagent also stops its own shells, and the CLI then starts a turn. So the
level is the only truth: the adapter answers each stop from the CLI's
success (which follows the level), and `TaskStopRefused` from the CLI is practically
unreachable.

**The Stop hook.** `initialize` registers
`hooks.Stop[{hookCallbackIds:["skywave-stop"]}]`. The CLI then sends a
`hook_callback` control request (`stop_hook`) with `callback_id` and
`input`: `session_id, transcript_path, cwd, prompt_id, permission_mode,
effort{level}, hook_event_name:"Stop", stop_hook_active,
last_assistant_message, background_tasks[…],
session_crons[{id,schedule,recurring,prompt}]`. It comes once per completed
turn end, self-started turns included, after the last `assistant` and
before `result`, and never for an interrupted turn. The adapter answers
success `{}` at once, in the same output as the request; other answers can
block the stop and make the CLI run another turn, so they are never sent. A
callback id other than ours gets an error. `session_crons` is where the
schedules come from.

**Crons are not tasks.** No stdin command can delete one: there is no
control subtype for it, `bash_command` runs in a separate shell, `/loops` is
disabled; only a model turn calling `CronDelete`, or the process ending,
removes one. `CronCreate`'s structured `tool_use_result` is `{id, durable}`;
2.1.310 ignores `durable: true` (always `false`, "Session-only", no
`.claude/scheduled_tasks.json`), but the adapter still reads it. Session-only
jobs come back after `--resume` (`loop_resume`), so a stop **pauses** a loop;
it does not end it. Skywave therefore shows schedules as information only.

**Approvals from a background subagent.** A subagent's `can_use_tool`
arrives after the main `result`, outside a turn, with `agent_id` equal to the
subagent's `task_id` and no `parent_tool_use_id` (`subagent_approval`). Such
an approval survives main-turn ends. It is cancelled when its task leaves
the set (reason `task ended`), by the CLI's `control_cancel_request`, or on
exit. An `agent_id` not in the last live set counts as the turn's.

**`perTaskStopAffordance`.** `initialize` carries
`"perTaskStopAffordance": true`: see §4.4.

**Sending a subagent a message:** see §3.12.

### 3.12 Sending a message to a subagent [C]

In the interactive TUI a user can open a background agent and type to it,
and the main agent can message one with its `SendMessage` tool. This
section records whether a stream-json consumer can do the same (probed
2026-11-02 against 2.1.310; D33 is the decision taken from it). Fixtures:
`subagent_message_ctl`, `subagent_message_ctl_done`, `subagent_message_ptu`,
`subagent_message_relay`, `subagent_message_relay_midrun`, recorded with
the Python probe `tests/support/probe-subagent-message.py` (retired for
`skywave-claude-compat`; git history keeps it). In each, a background subagent is
told to reply with the code word it was sent while it worked, or NONE.

**What was tried.**

| way in | result |
|--------|--------|
| control request `{"subtype":"send_task_message","task_id":…,"message":…}` while the subagent runs (`subagent_message_ctl`) | error `Unsupported control request subtype: send_task_message`; the subagent answers NONE |
| the same after the subagent finished (`subagent_message_ctl_done`) | the same error |
| a `user` line with `parent_tool_use_id` set to the `Agent` call's id (`subagent_message_ptu`) | the **main** agent gets it and runs a turn; the subagent never sees it and answers NONE |
| a main-turn message asking the main agent to `SendMessage` the subagent (`subagent_message_relay`, `subagent_message_relay_midrun`) | delivered: the subagent answers ZEBRA |

So in `-p` mode the only way in is **the relay**: a user message that asks
the main agent to forward the text with `SendMessage`.

**The relay, step by step.**
1. The adapter writes an ordinary user message, which starts a main turn,
   e.g. `Use SendMessage to tell the background agent <task_id> exactly:
   '<text>' Then reply SENT.` The `task_id` is the one from
   `background_tasks_changed` / `task_started`.
2. The main agent loads the tool (`ToolSearch` `select:SendMessage`, since it
   is deferred) and calls `SendMessage {to: <task_id>, message, summary}`.
   Its `tool_result` is JSON: `{"success":true,"message":"Message queued for
   delivery to <id> at its next tool round. If it finishes first, it is
   resumed with the message and notifies again, so wait for that
   notification before resending.","pin":{…}}`. This acks the **queueing**,
   not the delivery.
3. Delivery has two forms:
   - **Mid-run** (`subagent_message_relay_midrun`): the subagent reads the
     message at its next tool round. Nothing on stdout shows it: no line of
     the subagent's carries the text (searched for it); only the subagent's
     later behaviour does.
   - **After it finished** (`subagent_message_relay`): the subagent is
     resumed. A new `task_started` comes with the **same** `task_id` and
     `tool_use_id`, a **new** `run_id`, and `prompt` equal to the text,
     with the task back in the level set. That line is the one definitive
     delivery signal.
4. The subagent sees the text as from the main agent ("the coordinator's
   message"), not from a human. The model passed the text on verbatim in
   every run; nothing guarantees it.

**One `Agent` call, several runs.** A background subagent is resumed not
only by a message: when one of its own background shells ends, it gets a
fresh `task_started` (same `task_id` and `tool_use_id`, new `run_id`) whose
`prompt` is a `<task-notification>` block, works, and sends another
`task_notification` (`completed`). Each run ends with its own
notification, and the main agent runs a turn after each. The level set
drops and re-adds the task, so D31's fold needs nothing new.

**Why `send_task_message` fails (reading the binary).** The CLI is a Bun
binary with its minified JavaScript embedded; `strings -n 6
~/.local/share/claude/versions/2.1.310` gives the source text. The
minified names below hold for 2.1.310 only.
- The print-mode loop (it logs as `[print.ts]`) sends each control request
  whose subtype is in a handler table to that table's dispatcher:
  `if (Ols(j.request)) { … mfr(ne, {...gu, replyNow}) … }`, with
  `function mfr(e,o){ let n = Xe[e.subtype]; … }`.
- The table maps `stop_task`, `background_tasks` and `get_task_output` to
  real handlers, and `send_task_message` to a stub that only throws:
  `zt = (e,o) => { throw new UW(`${xX}: send_task_message`) }`, with
  `xX = "Unsupported control request subtype"`. That is the error above.
- A side branch (`Ils`) treats `send_task_message` differently only when
  the context's `resumeUntrackedAgent` is not `null` and the task is not
  tracked. Every context in this build sets `resumeUntrackedAgent: null`,
  and the branch only picks when the stub runs, not which.
- The name also appears in a list of requests a cloud-hosted session takes
  from clients, in the set of those only the session's owners may send,
  and in the requests refused before a spare process is claimed. So it is
  being plumbed for remote sessions; the local host has only the stub.
- 2.1.280 has no `send_task_message` at all; 2.1.309 and 2.1.310 have the
  same stub.

**How the TUI and `SendMessage` deliver.** Both call the same two
in-process functions; neither goes through a control request.
- The REPL keeps `viewingAgentTaskId` while the user views an agent; a
  submit then goes to `submitToAgent`. For a local agent task it appends
  the text to the agent's own transcript as a human message (origin
  `{kind:"human"}`); then, if the task is `running`, adds it to the task's
  `pendingMessages` (read at the next tool round), and otherwise resumes the
  agent in the background with the text as its prompt (`userInitiated`). If
  it loses the race to resume, it queues instead, or shows "Failed to resume
  agent: it is no longer running; message not delivered".
- `SendMessage` takes the same two paths: queue for an `agent-live`
  recipient, resume for an `agent-stopped` one, with the main agent as the
  origin.

So the relay delivers the way typing in the TUI does; only the origin
differs, and it costs a main turn.

**Relay race (CLI 2.1.310).** The CLI opens main turns on its own when a
background task ends, and a `user` line written then runs as a separate,
later turn. Probe `relay_race` (a quick and a long background subagent; the
relay is written the moment the quick one's `task_notification` arrives)
saw the CLI run its notification turn first (no `SendMessage`, `result`),
then the relay's turn (`ToolSearch`, `SendMessage`, `result`). So "the next
`result` after writing is the line's turn" is false, and a relay's outcome
cannot be settled by turn counting; D34 therefore defers messaging a
subagent.

**`--replay-user-messages`.** With it (`relay_race_echo`) the CLI echoes each
stdin `user` line as `{"type":"user","isReplay":true,"uuid",...}` right after
the `init` of the turn that takes it up, before any `assistant` line: a
definitive marker of which turn is the line's. Unverified: whether an input
`uuid` is echoed back.

**`sleep` is blocked.** This CLI refuses a standalone `sleep 25` in a
subagent ("use Monitor"), so the `relay_live` and `relay_midrun` prompts need
changing before they are re-recorded. The raw `relay_race` recordings
(`/tmp/skywave-race/`) are not fixtures; nothing replays them. The relay
scenarios were not carried into `skywave-claude-compat`: D34 deferred the
relay, so nothing relies on them. Revive them from the retired probe if a
relay is ever reconsidered.

**Re-check on every CLI bump.** `skywave-claude-compat` (quick tier) sends
`send_task_message` and watches the answer. While it is still
`Unsupported control request subtype: send_task_message`, the scenario
passes. Any other answer reports it as newly available: record a fixture
of the request reaching a live subagent and revisit D33 and D34.

### 3.13 Foreground subagents [C]

The `Agent` tool takes `run_in_background`; with `false` the subagent is
**foreground** ("synchronous"): the main agent's `Agent` call blocks until
it ends. The model chooses: asked to wait ("I'll wait for its answer") it
picked foreground, asked for a chore with "no rush" it picked background,
and the same ask went either way across runs. Fixtures:
`subagent_foreground`, `subagent_foreground_backgrounded`,
`subagent_foreground_stop`, recorded 2026-11-02 with the Python probe
`tests/support/probe-foreground-subagent.py` (retired; git history keeps
it), which asks the way a user would over a small folder of recipes and
lets the agent pick its tools. `skywave-claude-compat`'s
`subagent_foreground` re-checks the first of them; backgrounding
(Ctrl+B) and stopping a foreground subagent are deferred (D31, D34), so
it does not check them.

**What a foreground subagent looks like** (`subagent_foreground`):
- `task_started` with a `task_id`, the `Agent` call's `tool_use_id` and
  `is_backgrounded: false`. It never enters `background_tasks_changed`,
  which lists background tasks only.
- Its prompt is streamed as a `user` line whose content is a text block,
  with `parent_tool_use_id`. A background subagent's prompt is not
  streamed (`subagent*` fixtures).
- Its `assistant`/`user` lines carry `parent_tool_use_id` = the `Agent`
  call, as a background subagent's do, and arrive inside the main turn.
- Its `can_use_tool` carries `agent_id` = its task id, which is not in the
  live set, so the approval counts as the turn's (§3.11).
- It ends with `task_updated` (`status: completed`) and `task_notification`;
  only then does the `Agent` call get its `tool_result` (the subagent's
  report) and the main turn go on.

**Moving it to the background** (`subagent_foreground_backgrounded`). The
control request `{"subtype":"background_tasks","tool_use_id":"<Agent
call>"}` is Ctrl+B for one task (without `tool_use_id`, every foreground
task). Its answer is `{"backgrounded": true}` (`false` when nothing was
moved, e.g. the subagent already runs in the background). Before the
answer: `background_tasks_changed` lists the task under the **same**
`task_id`, and `task_updated` patches `is_backgrounded: true`. Then the
blocked `Agent` call gets "Async agent launched successfully…" and the main
turn ends, while the subagent's lines go on with the same
`parent_tool_use_id` and end as a background subagent's do.

**Stopping it** (`subagent_foreground_stop`). `stop_task` with its task id
works though the task was never in the level set: `task_updated`
(`killed`), `task_notification` (`stopped`), then success `{}`. The `Agent`
call's `tool_result` is an error, "[Request interrupted by user for tool
use]", and the main turn ends normally (`result/success`).

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

### 4.4 Background work [C] (`interrupt_spares_tasks.ndjson`, `interrupt_kills_tasks.ndjson`)

What an interrupt does to background tasks depends on `initialize`'s
`"perTaskStopAffordance"`. The CLI's schema: the consumer renders a
per-task stop control wired to `stop_task`; when declared, an interrupt on
an open-input session spares running background agents and workflows (it
aborts the turn only). Absence fails closed.
- **Without it** (`interrupt_kills_tasks`): `task_updated` `killed`,
  `task_notification` `stopped`, the empty level, then the interrupt's
  response.
- **With it** (`interrupt_spares_tasks`): only the main turn's foreground
  work stops. The background subagent stays in the level, its shell
  completes, it ends `completed`, the level empties, and the CLI starts a
  turn of its own.

An interrupted turn gets no Stop hook call. Skywave declares the flag
because both heads render a per-task stop; an approval a spared subagent is
waiting on stays pending.

## 5. Session id and resume

- `--session-id <uuid>` sets the session id. Every `session_id` field and the
  transcript file use it. **[C]** T09 can therefore generate the id up front
  and store it before the first turn finishes, as the context's opaque
  resume handle `{"session_id":"<uuid>"}` in `contexts.agent_state`. It
  reports that handle with the first `TurnStarted`, not at start: the CLI
  keeps no conversation for a session that never got a message, so
  `--resume` of one fails with `No conversation found` (confirmed by the
  user, 2026-11-01). A process stopped before any message (one started for
  a setting, then shut down idle) reports no handle, and the context's next
  start is fresh with a new id. The handle holds no rules: granted session
  rules are the session's grants (§3.2), stored per Skywave session. A
  handle stored when it still carried `rules` resumes all the same; those
  rules are ignored, not granted.
- Without `--session-id`, the CLI picks a UUID. It is first visible in
  `system/init.session_id` and is repeated on every message. **[C]**
- `--resume <uuid>` in a **new** process restores the history (`resume.ndjson`
  quotes the first prompt of `basic`) and keeps the **same** session id.
  **[C]**
- `--resume <A> --fork-session --session-id <B>` in a new process copies A's
  history into a new transcript under the id **B we chose**: `system/init`
  and every message carry B. **[C]** (`fork.ndjson`, 2026-11-01). Without
  `--session-id` the CLI picks the new id (seen in a probe, not recorded).
  A is untouched: a later `--resume <A>` does not know the fork's turns, and
  `--resume <B>` restores A's history plus the fork's. **[C]** Nothing in the
  fork's stream names A, so the parent is known only from what skywaved
  stored. Like a resume, the fork carries A's spend: B's first
  `total_cost_usd` and `modelUsage` include A's, while
  `model_usage_since_last_result` holds only B's turn, so the adapter's
  per-turn cost (§2.5) needs no change. Resuming or forking from a different
  cwd is still **[U]**. Moving a context to a new session starts the new
  session's first context this way: the adapter mints B as for a fresh
  start and reports `{"session_id":"B"}` with the first turn, so a fork
  stopped before any message reports no handle and its next start forks
  again under another new id.
- Adopting a conversation the user had in the CLI outside Skywave
  (`Conversation::Adopt`, D32) is the same fork argv with the outside id as
  parent: `--resume <outside> --fork-session --session-id <new>`; the
  outside transcript is never written. The id must be a session id the CLI
  keeps under the session's workdir (the lookup is per cwd, below): a
  symlinked or different cwd is "No conversation found". The adapter
  rejects an id that is not a UUID before spawning. With stream-json
  stdin open and `initialize` written, an unknown id (resumed or forked)
  still fails before any initialize reply: stderr `No conversation found
  with session ID: <id>`, then the error `result` as the first stdout line,
  exit 1, so the adapter's start reports `ConversationGone`. **[C]**
  (`skywave-claude-compat --only resume_unknown`, 2.1.310, 2026-11-04.)
  Manual checks for the user, with the real CLI (none confirmed yet):
  1. In a dir under the roots, run `claude`, say "remember the word
     PELICAN", quit; note the id (`/status`, or the newest
     `~/.claude/projects/<dir>/*.jsonl`).
  2. Adopt it (TUI `c`, pick it or `p` to paste its id; or the GUI
     dialog's id field). The window shows the `Adopted` divider, the
     imported history, then the fork-point divider. Ask "what word did I ask you to
     remember?": expect PELICAN.
  3. The outside transcript's line count is unchanged afterwards.
  4. Restart skywaved and send another message: the log shows `resumed`,
     not `adopted`.
  5. Adopt a random UUID: the first message is `Errored` with "No
     conversation found", and no new `.jsonl` appears. Note whether it
     failed at start (`agent not started`) or as an exit after the
     handshake, and replace the **[U]** above.
  6. Adopt with the terminal `claude` still open on that session: it
     should work, and the terminal continues independently.
- Forking an unknown parent fails like resuming one. **[C]** (probe,
  CLI 2.1.310, 2026-11-01: `--resume 00000000-0000-0000-0000-000000000000
  --fork-session --session-id <B>`, stdin closed, in an empty scratch dir.)
  stderr is `No conversation found with session ID:
  00000000-0000-0000-0000-000000000000` (the parent's id), stdout one
  `result` with `subtype:"error_during_execution"`, `num_turns:0`,
  `total_cost_usd:0`, `errors:["No conversation found with session ID:
  00000000-…"]` and `session_id` **B**, exit 1, before any model call. No
  `~/.claude/projects/` directory was created for the scratch cwd, so a
  failed fork leaves no B transcript behind.
- Reusing an existing id with `--session-id` fails:
  `Error: Session ID <id> is already in use.`, exit 1. **[C]** So the first
  start uses `--session-id`, and every restart uses `--resume`.
- Resuming an unknown id fails. The CLI prints
  `No conversation found with session ID: <id>`, then a `result` with
  `subtype:"error_during_execution"`, `num_turns:0` and `errors:[...]`, and
  exits 1. **[C]** Fallback for T09: surface a session error. Silently
  starting a fresh session would lose history.
- Sessions persist under `~/.claude/projects/<dir>/<id>.jsonl`, `<dir>`
  being the cwd made a file name (exact rule in §6c).
  The resume must run with the **same cwd**: lookup is per project directory.
  The per-cwd layout is **[C]**; resuming from a different cwd was not tested
  **[U]**. D14 already fixes the workdir per session, so this holds.
  `--no-session-persistence` would make resume impossible, so it must not be
  in `agent.args`.
- **Transcript retention.** Every CLI process sweeps transcripts (assumed
  of every project, not just its own cwd: **[U]**) last written more than `cleanupPeriodDays`
  days ago; a context idle that long can then not be resumed (`No
  conversation found`). **[C]** with 2.1.310, from the binary
  (`~/.local/share/claude/versions/2.1.310`, 2026-11-04): the default is
  30 (`m1t=30`; the schema says "default: 30. Minimum 1"); the value is read
  as the first set of policy (managed) settings, flag settings
  (`--settings <file-or-json>`), then user settings
  (`~/.claude/settings.json`), so a `--settings` value outranks the user's
  file and only managed policy outranks it; `0` is rejected by validation.
  A probe in a scratch dir (`claude -p --no-session-persistence --settings
  '{"cleanupPeriodDays":36500}' "Reply with just: ok"`) ran normally; that
  the sweep then keeps a 31-day-old transcript is **[U]** (not probed: a
  probe would risk the user's real transcripts). skywaved therefore passes
  `--settings '{"cleanupPeriodDays":36500}'` to every process
  (`KEEP_TRANSCRIPTS_SETTINGS`, in `REQUIRED_ARGS`) and never touches the
  user's settings files. Two gaps remain: a `claude` the user runs directly
  as the same OS user still sweeps with the user's own value (30 unless set),
  deleting skywaved's old transcripts too, and a `--settings` in
  `agent.args` replaces skywaved's (the last one wins, as for any repeated
  commander option: **[U]**, not probed). The README's server
  setup tells the operator to set `cleanupPeriodDays` in
  `~/.claude/settings.json` in that case. A resume that fails anyway is
  `AgentError::ConversationGone`, whose text tells the user to start a new
  context (§6a).

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
  waits for `result`; it sends one per turn, and a repeated interrupt from
  the user before that `result` kills the process instead (a CLI that never
  answers would otherwise hold the turn). To stop the process, it closes stdin when idle, or
  kills the process if it does not exit within a grace period.
- **stderr [C].** Empty in every stream-json run. Startup errors (bad flags,
  an unknown resume id, a session id already in use) are printed as one line,
  `Error: ...` or `No conversation found ...`, and exit 1. T09 should capture
  stderr into the daemon log.
- **No CLI timeout while idle [C].** In the interrupt run the process sat idle
  for about 240 s between turns with no output, and then served the next turn.
- **Signals [C].** Probed 2026-11-01 on 2.1.310 with skywaved's default argv
  (§1 flags, `--session-id`, no `agent.args`) and its `initialize`; the
  signal went to the CLI's process group, then the session was resumed.
  Not kept as fixtures: the runs carry a real user's settings and hooks.
  - **Bash commands run outside the CLI's process group.** Every Bash
    command, foreground or `run_in_background`, runs under a shell that is
    its own session leader (`pgid = sid = ` its pid). A signal to the CLI's
    group never reaches it.
  - **SIGTERM: the CLI cleans up and exits** with 143 within about 0.6 s in
    every case. A running foreground command is killed and reported:
    `task_notification` (`stopped`), then the call's `tool_result`
    `"Exit code 137"` (`is_error`), which the CLI also keeps in its
    conversation. A background task is killed: `task_notification`
    (`stopped`), `task_updated` (`killed`), `background_tasks_changed` with
    `[]`. No `result` comes, and nothing else.
  - **SIGKILL: the CLI's commands keep running**, reparented away from it:
    foreground and background alike. The conversation ends at the
    `tool_use` with no result. On resume the CLI reports the background
    task as `task_notification` `stopped` ("Background shell command didn't
    finish before the previous session ended") while it in fact still runs.
  - **A text answer cut off** (signal while it is generated, before its
    `assistant` line): either signal, nothing more is printed and nothing
    of the answer is kept. A `thinking` block already printed is not kept
    either. On resume the CLI answers the unanswered message with the
    placeholder `No response requested.`; the model sees no answer.
  - **A pending approval** (`can_use_tool` unanswered) on SIGTERM: nothing
    is printed, the tool does not run, and the conversation ends at the
    `tool_use`; on resume the CLI adds an interruption marker as its
    result.

## 6a. How T09 implements it

`skywaved::agent::claude_code` (`protocol.rs` is the pure translator,
replay-tested on the fixtures in `tests/claude_code.rs`). Choices beyond the
notes above:

- **Mid-turn messages are queued by the adapter**, not sent: the next user
  message is written only after the turn's `result`. So each written message
  is exactly one turn, and the [U] CLI-side queueing (§7) is never relied on.
- **`start` waits for the initialize reply.** A CLI that exits or reports an
  error `result` before it is a start error; with `--resume` (a restart
  or a fork) and `No conversation found` in the error text,
  `AgentError::ConversationGone` (§5): the session's `Errored` then says the
  context's history is gone from the server and to start a new context
  (`CONVERSATION_GONE`), and each later message tries the resume again. If a CLI
  ever reports the unknown id only after the handshake, it surfaces as
  `Exited { error }` instead, which still marks the session errored.
- **An error `tool_result` for a call whose approval is pending** (stdin
  closed, or a future CLI-side timeout, §3.4) reports `ApprovalCancelled`
  with the result's first line as the reason.
- **Approvals still pending at `result`** are cancelled (`interrupted` or
  `turn ended`) before `TurnEnded`, except a background subagent's: a
  `can_use_tool` whose `agent_id` is a live task's id (from
  `background_tasks_changed`; `subagent_approval.ndjson`) can come outside
  any turn and is that task's. It stays pending across main turns and is
  cancelled (`task ended`) when a `background_tasks_changed` no longer lists
  the task, or withdrawn by the CLI's own cancel. An `agent_id` no task set
  listed (a foreground subagent) is the turn's.
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
- **Killing is SIGTERM, then SIGKILL.** Each CLI leads its own process
  group; a kill sends SIGTERM to the group, then SIGKILL to whatever is
  left after 5 s (the term grace). The SIGTERM is what stops the CLI's
  Bash commands (§6, Signals): they are out of the group's reach, so a CLI
  that outlives the grace leaves them running. The group still takes
  children that stay in it. On Linux the CLI gets `PR_SET_PDEATHSIG`
  SIGTERM, so a crashed daemon's CLI stops its commands and exits too.
- **A kill reads stdout until it ends** (bounded by the term grace) and
  reports what comes, so the log gets the killed call's `tool_result` and
  the stopped tasks, as the CLI's conversation does. The CLI reports no
  `result` and cancels no approval on SIGTERM, so an abandoned turn and
  its approvals stay open for the next start to settle. stdin is closed
  after the signal, so the CLI handles the signal rather than an EOF.
- **A write the CLI does not read within 30 s** kills it, reported as
  `Exited { error }`: a wedged CLI cannot hold up its session's driver.
- **Once stdout ends the session refuses requests** (`AgentError::Closed`),
  while the driver still waits for the exit: a message sent then fails, so
  the supervisor restarts the agent and delivers it there instead of it
  being dropped. A repeated interrupt that kills the CLI refuses requests
  the same way from the kill on.
- **Messages that never ran are counted, not dropped silently.** The ones
  still queued behind a turn when the session stops (a stop sets no new turn
  going, even once the interrupted turn's `result` comes), when a repeated
  interrupt kills the CLI, or when it crashes, plus the ones accepted but not
  read before stdout ended, are `Exited { unsent }`. The exit reason stays
  the exit's own (a clean exit is still `error: None`); the mapper logs one
  `Errored` for the exit naming both, e.g. "agent exited during a turn; 1
  queued message was not sent to the agent; send it again". They are not
  redelivered on the next start. One race remains: a `result` read before
  the stop's `Shutdown` request starts the queued turn, which the stop then
  abandons (logged as "agent exited during a turn").

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
output when a field is missing. The transcript fixtures (§6c) also show
Read's `{type, file}` and Edit's `structuredPatch` shapes. **[C]**

## 6c. Stored transcripts (outside conversations)

`claude_code/transcripts.rs` reads the CLI's own transcripts to list the
conversations a session can adopt and to replay one's history. Probed and
recorded with 2.1.310 on 2026-11-03; the fixtures are
`tests/fixtures/claude-code/transcripts/` (recorded with
`tests/support/record-transcript.sh`, scenarios in its README).

**Where.** `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects` **[U]**
(only the default was seen), then one dir per cwd holding `<id>.jsonl` per
conversation, `<id>/subagents/agent-*.jsonl` per subagent and
`<id>/tool-results/*.txt` for big tool outputs. **[C]** Every conversation
is there, Skywave's own (fresh, resumed, forked) as much as the terminal's.

**Dir name.** **[C]** (probe: cwds `/tmp/skywave-sanitise-probe/a.b_c d+é`
and one of 269 chars.) Each UTF-16 unit that is not an ASCII letter or digit
becomes `-` (`a.b_c d+é` → `a-b-c-d--`). A name longer than 200 is cut to
200, then `-` and a hash of the whole cwd: the JavaScript string hash
(`h = (h << 5) - h + unit | 0`, absolute value, base 36). The rule is
lossy (`/a/b-c` and `/a/b/c` share `-a-b-c`), so the reader never takes a
workdir from the dir name: a transcript's workdir is the `cwd` of its first
non-sidechain `user` line, and it counts only when that cwd's dir name is
the dir it sits in.

**Lines.** One JSON object per line, appended as the conversation goes; a
line being written may be cut short, so a line that does not parse is
skipped. **[C]** Types seen: `user`, `assistant` (the API `message`, one
content block per line), `attachment` (context the CLI adds: environment,
skills, date, …), `system` (`compact_boundary`, `local_command`, …),
`queue-operation`, `last-prompt`, `mode`, `atis-latch`, `cost-state`,
`api-request*` (whole request bodies, the user's context included),
`ai-title` (`aiTitle`, interactive sessions only: `-p` writes none),
`custom-title` and `agent-name` (`/rename`, also under `-p`; not read).
Every
`user`/`assistant`/`attachment`/`system` line carries `uuid`,
`parentUuid`, `isSidechain`, `cwd`, `timestamp`, `sessionId`, `version`.
A tool result's structured form is the line's `toolUseResult` (the stream's
`tool_use_result`), and it is translated with the same helpers as the
stream, so a call and its result look the same imported or live.
- `isMeta` user lines are the CLI's own context (`<local-command-caveat>`).
- The `/compact` summary is a `user` line with `isCompactSummary` (and
  `isVisibleInTranscriptOnly`): left out, since the walk reads the history
  it summarises. The command itself leaves `<command-name>/compact…` and
  `<local-command-stdout>Compacted…` user lines, kept verbatim, like the
  interrupt marker `[Request interrupted by user for tool use]`.
- A subagent's lines (`isSidechain: true`) live in its own file under
  `subagents/` in this build; the main file has only the `Agent` call and its
  result. Sidechain lines are dropped wherever they are.
- A big tool output is a `<persisted-output>` stub naming the
  `tool-results/` file and a preview; the stub is kept as the output.

**Active branch.** **[C]** for the fixtures (straight chains, one
`/compact`). Lines chain through `parentUuid`, attachment and system lines
included. The reader starts at the last non-sidechain `user`/`assistant`
line and walks parents back, then reverses: what `--resume` continues. A
`compact_boundary` has `parentUuid: null` and the pre-compaction leaf as
`logicalParentUuid`, which the walk follows, so a compacted conversation is
read whole. Parallel calls do not chain: each `tool_result` line is parented
to the assistant line holding its own `tool_use`, so every result but the
last is a sibling off the walked chain (`parallel.jsonl`). The reader keeps
a results-only `user` line whose parent is on the chain, in file order,
which is the stream's order. Where a parent is missing (or a cycle), the reader falls back to
file order minus sidechain lines. Rewound or edited branches were not
recorded **[U]**; a test builds one.

**Listing.** Title: the last `ai-title`. First prompt: the first line of the
first user text on the branch. Messages: user texts plus agent texts on the
branch. Last active: the last line's `timestamp`, else the file's mtime.
A project dir or transcript that is a symlink is skipped, by listing and
history alike. Only a canonical cwd under a root is listed (a cwd through a symlink cannot
be resumed, §5).

## 7. Not covered (out of MVP scope or not needed by T09)

- Images and attachments in `user.content`
- removing rules granted with "always allow" (they live in the CLI's
  settings, out of Skywave's sight)
- hooks over the control channel
- MCP over stdio
- `--replay-user-messages`
- sending a second user message while a turn runs (`still_queued` suggests the
  CLI queues it)
