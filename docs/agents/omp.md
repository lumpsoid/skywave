# omp (oh-my-pi) as an agent: the RPC protocol against the adapter traits

Research only: no omp adapter exists yet. This records what omp offers a
headless host, how it maps onto `AgentAdapter` / `AgentSession` /
`AdapterEvent` (`crates/skywaved/src/agent/mod.rs`), and what it lacks.

Checked against omp 18.18.6 (source `oh-my-pi` at `235357cf94`,
2026-11-10). Nothing here was recorded from a live process yet: every
`[S]` claim was read in source, `[D]` only in omp's docs. Record fixtures
before relying on any of it, as for Claude Code (`claude-code.md`).

Sources:

- Bundled docs: `omp://rpc.md`, `omp://approval-mode.md` (match the
  installed binary). The public page <https://omp.sh/docs/rpc> is an older,
  shorter reference: it lacks `open_session`, `fork`, `session_settled`,
  and says `prompt_result` is only emitted for local-only completion. Do not
  build against it.
- Source: `packages/coding-agent/src/modes/rpc/` (`rpc-mode.ts`,
  `rpc-types.ts`, `rpc-prompt-results.ts`), `packages/agent/src/agent-loop.ts`,
  `packages/coding-agent/src/tools/approval.ts`,
  `packages/coding-agent/src/extensibility/extensions/wrapper.ts`,
  `packages/coding-agent/src/session/agent-session.ts`.

## 1. Verdict

Feasible; medium effort. Turn accounting, tool calls, model and effort,
resume and fork, subagents and usage all have definitive signals. Two gaps:

1. **Approvals carry no tool call id on the wire** (§4). Blocking: needs a
   small upstream change, or ACP.
2. **Permission mode cannot change at runtime** (§5).

Nothing outside `skywaved` changes: no wire change, no `PROTOCOL_VERSION`
bump.

## 2. Process and transport

- `omp --mode rpc-ui --no-ui [--approval-mode <m>] [--model p/id]`, cwd =
  the session workdir. `rpc-ui` is needed so tool dialogs (`ask`, approvals)
  reach the host; `--no-ui` keeps extensions headless. Plain `--mode rpc`
  fails approvals closed ("no interactive UI available"). [S]
- JSONL both ways; stdout is protocol only, stderr is logs. First frame is
  `ready { protocolVersion: 1, supportedProtocolVersions: [1,2],
  maxFrameBytes, maxReassembledFrameBytes }`. [D]
- Send `negotiate_protocol { protocolVersion: 2 }` at once: otherwise
  frames over 1 MiB are truncated or fail. v2 wraps oversized frames in
  `rpc_chunk` sequences the adapter must reassemble and validate (rules in
  `rpc.md` "Transport and Framing"). [D]
- Commands carry `id`; the `response` echoes it. Responses can arrive out of
  order (`bash`, `prompt` admission run concurrently): match on `id` only.
- Close stdin to stop: accepted commands drain, then exit 0. Keep reading
  stdout until EOF. [D]
- `ready` carries no omp version. `VersionUnconfirmed` needs `omp --version`
  before spawning, or a field upstream.

## 3. Turns: how messages become turns [S]

The command `id` acknowledges **admission** (turn started, or message
queued), and `prompt_result` reports the **end** of that prompt's work.
Neither marks when a turn takes a message up. The event stream does:

- `agent-loop.ts` `emitInputMessages` writes `message_start` /
  `message_end` for every input message at the point it enters context: the
  run's first prompt, a steer at a tool boundary, a follow-up drained at the
  stop boundary.
- Steers and follow-ups drained at the stop boundary stay **in the same
  run** (`agent-loop.ts` ~1891: `pendingMessages = [...lateSteering,
  ...asides, ...followUps]; continue`): no new `agent_start`, one
  `agent_end` closes all of them. This is Skywave's "taken up inside the
  open turn" case of `TurnStarted { message: true }`.
- Messages a host sends are `{ role: "user", attribution: "user" }`
  (`rpc-mode.ts`). omp's own injections are `attribution: "agent"` or
  `synthetic: true` (`UserMessage`, `packages/ai/src/types.ts`). Hidden
  companions (keyword notices, image descriptions) are queued as separate
  messages just before the user message (`#pushQueuedUserMessage`).
- `agent_end` has `yielded`; `yielded: false` (retry, compaction, stop-time
  reminder) is not the end of the turn.
- `prompt_result { id, agentInvoked, status: completed|aborted|error,
  error?, sessionSettled }` (`RpcPromptResults`) reports per prompt at the
  yield. `session_settled` follows once background work can no longer wake
  the agent.

Mapping:

| omp | Skywave |
|---|---|
| `agent_start` | open a turn; hold `TurnStarted` until its first input message |
| `message_start` user, `attribution: "user"`, not `synthetic` | `TurnStarted { message: true }` (also inside an open turn) |
| run whose first input is not a host message (async job result, goal continuation, IRC) | `TurnStarted { message: false }` |
| `agent_end` with `yielded` (or `isTerminal !== false` on older builds) | `TurnEnded`; outcome from `prompt_result.status` / last assistant `stopReason` |
| `response.data.agentInvoked: false` (local slash command) | no turn: emit `TurnStarted` + `TurnEnded(Completed)`, or refuse `/…` before sending |

Sending: `prompt` while idle; `prompt` with `streamingBehavior: "followUp"`
while a turn runs (without it a busy `prompt` fails). Queue modes default
to `one-at-a-time`.

To verify with fixtures: companions' exact role and filtering; that every
host message gets exactly one `message_start`; `abort` behaviour below.

## 4. Approvals: the blocking gap [S]

How omp asks:

- `tools.approvalMode`: `always-ask`, `always-ask-cached`, `write`, `yolo`
  (default `yolo`), plus per-tool `tools.approval.<tool>` and
  `tools.restrictedFolders`. Subagents always run `yolo`.
- The gate runs inside the tool's `execute`, after `tool_execution_start`
  (`agent-loop.ts` `runTool`), in `ExtensionToolWrapper.#execute`.
- `requestApproval` (`tools/approval.ts`) uses `ui.approve(prompt)` when the
  host's UI context has one. The RPC UI context does not, so it falls back
  to `ui.select(fullText, labels)`, sent as
  `extension_ui_request { method: "select", title, options }`. Labels are
  grant choices (`Approve File`, `Always Approve Directory: …`),
  `Approve`/`Approve Once`, `Approve <tool> for this session`, `Deny`.
- The wrapper knows `toolCallId` and emits `tool_approval_requested
  { toolCallId, toolName, approvalMode }`, but only to in-process
  extensions, never on the wire.
- Tools in a batch run concurrently unless `exclusive`, so two approval
  selects can be pending at once and their order does not identify the call.

So `ApprovalRequested { call, tool, input, suggestions }` cannot be filled
from definitive signals. Options:

1. **Upstream (preferred):** implement `approve` in the RPC UI context and
   thread `toolCallId`, tool name and args into `ExtensionApprovalPrompt`
   (`extensions/types.ts`). Its `session` and `grants` choices map onto
   `suggestions`; `ExtensionApprovalAnswer` onto `Decision` + `rule`.
2. **Bridge extension** (`-e`) relaying `tool_approval_requested` as a
   `setStatus` frame before the select: relies on frame order, which
   concurrent tools break. Rejected by the "definitive signals" rule.
3. **ACP** (`--mode acp`): structured `session/request_permission`, but
   loses omp-specific frames (subagents, queue, goal).

Grants: omp stores its own (`approval-cache.db`, session approvals in
memory); `Grants` would mirror them or stay unused.

Questions (`ask` tool) arrive as `select` (options) and `editor` with
`promptStyle: true` (free text), and `cancel` withdraws a dialog. Mapping to
`QuestionAsked` / `ApprovalCancelled` is plausible but has the same
missing-call-id problem.

## 5. Mode, model, effort

| Skywave | omp | Notes |
|---|---|---|
| `set_permission_mode` | none at runtime | `rpc-types.ts` has no approval-mode or plan-mode command. Only `--approval-mode` at start: restart the process, or refuse. Rough map: `Ask`→`always-ask`, `AcceptEdits`→`write`, `Bypass`→`yolo` (gate on D38), `Plan`/`Auto`/`DontAsk` → refuse. |
| `set_model` / `ModelSet` / `ModelRefused` | `set_model { provider, modelId }` → `Model` or `Model not found` | model string must carry the provider |
| `Model` (unasked) | `model_changed` (no payload) → `get_state.model` | |
| `ModelsOffered` | `get_available_models` | waits for model discovery |
| `set_effort` / `EffortSet` | `set_thinking_level` (`off`…`max`) | `Effort` maps 1:1 for low…max |
| `Effort` (unasked) | `thinking_level_changed { thinkingLevel?, resolved? }` | |

omp's default `yolo` means the adapter must pin `--approval-mode` from the
start, or bypass gating (D38) is lost.

## 6. Interrupt and stop

- `interrupt` → `abort`; the response comes after the abort completes. A
  plain `abort` re-queues stranded steers and drains them into a fresh
  turn; `abort_and_restore_queue` instead withdraws all user-queued input and
  returns it. Which one fits `Exited.unsent` and owed-message accounting is
  open; record both.
- `escalate_interrupt` → kill the process.
- `shutdown` → close stdin, read to EOF.

## 7. Conversations: resume, fork, adopt

- Sessions are JSONL files under `~/.omp/agent/sessions/<cwd-slug>/`
  (`session/session-paths.ts`, `sessionDirForCwd`). `--session-dir`
  overrides the root.
- `Resume`: `open_session { sessionDir }` continues the newest session in a
  directory, or `switch_session { sessionPath }` / `--resume`. Fails with
  `Could not restore model …` when the saved model is gone (`AgentError::Resume`
  candidate). Handle = session file path + id from `get_state`.
- `Fork`: `fork` (whole session, or up to `entryId`); new file and id via
  `get_state`. Refused with `code: "session_busy"` while anything runs.
- `Adopt` / `OutsideConversations`: a new reader for omp's JSONL, like
  `claude_code/transcripts.rs`. Format not yet studied.
- `get_session_lease` says whether another omp process owns a session.

## 8. Everything else

| Skywave | omp | Status |
|---|---|---|
| `Text` | `message_update.assistantMessageEvent.text_delta`; final in `message_end` | good; `set_event_filter … messageUpdates: "delta"` cuts bandwidth |
| `ToolCall` / `ToolResult` | `tool_execution_start { toolCallId, toolName, args, intent? }` / `tool_execution_end { result, isError? }` | good |
| `TurnUsage` | assistant `usage { input, output, cacheRead, cacheWrite, cost }` | good |
| subagent `parent`, `SubagentUsage` | `set_subagent_subscription "events"`; `subagent_lifecycle.payload.parentToolCallId`, `subagent_event`, `subagent_progress` (tokens, cost) | likely good |
| `BackgroundTasks` / `stop_task` | `get_subagents`, `cancel_subagent`; async bash/eval jobs are not listed over RPC | partial |
| `Schedules` | none | absent |
| `UsageLimitsSource` | `get_state.usageLimit { stage, resetsAtSec, allowanceLeftPercent? }`, Claude subscriptions only | mostly `Unavailable` |
| `AgentLogin` | `login { providerId }` → `open_url`, then `input` for the code; multi-provider; secret prompts refused | flow fits `begin`/`finish`; no expiry exposed |
| `LoggedIn` | `get_login_providers[].authenticated` | per provider, not per account |

## 9. Changes in skywaved when this is built

- `main.rs` hard-wires the `Claude*` types. `AgentAdapter` returns
  `impl Future`, so it is not object-safe: select by an enum adapter or a
  generic `run::<A>()`.
- `config.rs` calls `claude_code::bypass_arg`: route it through the chosen
  agent. Add `[agent] kind`.
- A fake omp script beside `tests/support/fake-claude.sh`, recorded
  fixtures, and a compat check like `skywave-claude-compat`.
- Update `docs/architecture.md` and add a decision for the second adapter.
