#!/bin/sh
# A stand-in for the agent CLI in tests/claude_code.rs. It speaks just
# enough of the stream-json protocol (docs/agents/claude-code.md), with
# shapes copied from the T07 fixtures, and replies only to what it reads, so
# every run is the same.
#
# It writes its argv to ./argv.txt and its pid to ./pid.txt (the session
# workdir). It reports version 2.1.310, or the one `--fake-version <v>`
# names. A user message containing "approve" asks for a Write permission;
# one containing "hang" streams until interrupted; one containing "crash"
# exits 3 with a stderr line; one containing "deaf" gets the text "deaf",
# then the CLI stops reading stdin (a wedged CLI); any other message gets the text "echo". One containing
# "orphan" also starts a background `sleep`, its pid in ./child.txt, before
# it is handled as above; with "stubborn" too, the `sleep` ignores SIGTERM.
# One containing "litter" first writes a stray line that is not JSON; one
# containing "quit" then exits 0, mid-turn and without a word.
# One containing "leave" gets the text "echo" and its result, then the CLI
# closes stdout and exits 0 a second later, between turns.
# One containing "run" starts the Write call and runs it until interrupted.
# One containing "stuck" gets the text "working", then the CLI answers
# nothing more, interrupts included (each one gets the text "ignored").
# A SIGTERM makes the CLI write ./term.txt and exit, reporting a running
# call as killed first, as the real CLI does (§6).
# `--resume 00000000-0000-0000-0000-000000000000` fails like an unknown id,
# with `--fork-session` too (the parent is unknown; §5). A fork otherwise
# runs as its `--session-id`, as the real CLI does.
# `set_permission_mode` and `set_model` are answered like the real CLI's
# (§3.7): bypassPermissions is refused unless the CLI was launched with
# --allow-dangerously-skip-permissions (§3.6), and the model "bogus" is.
# `apply_flag_settings` and `get_settings` are answered like the real CLI's
# (§3.10): the apply is acked with an empty success and sets `effortLevel`
# only when it is a level the CLI knows, and `get_settings` reads back
# `applied.effort`, `medium` at first and `null` while the model is
# "no-effort" (a model that takes none, whose level is kept for the next).

printf '%s\n' "$@" > argv.txt
echo $$ > pid.txt
running=""
trap 'echo term > term.txt; if [ -n "$running" ]; then tool_result "Exit code 137" true; fi; exit 143' TERM

sid=""
mode="default"
# No model until one is set, so a turn reports no usage by default.
model=""
effort="medium"
resume=""
fork=""
allow_bypass=""
version="2.1.310"
prev=""
for arg in "$@"; do
  case "$prev" in
    --session-id) sid="$arg" ;;
    --resume) resume="$arg" ;;
    --fake-version) version="$arg" ;;
  esac
  case "$arg" in
    --fork-session) fork=1 ;;
    --allow-dangerously-skip-permissions) allow_bypass=1 ;;
  esac
  prev="$arg"
done
# Without --fork-session the process runs as the session it resumes.
if [ -z "$fork" ] && [ -n "$resume" ]; then
  sid="$resume"
fi

if [ "$resume" = "00000000-0000-0000-0000-000000000000" ]; then
  # The real CLI names the unknown parent, under the id it would run as.
  echo "No conversation found with session ID: $resume" >&2
  printf '{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"result":null,"errors":["No conversation found with session ID: %s"],"session_id":"%s"}\n' "$resume" "$sid"
  exit 1
fi

reqid() {
  printf '%s' "$1" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p'
}
ack() {
  printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"still_queued":[]}}}\n' "$(reqid "$1")"
}
text() {
  printf '{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"%s"}]},"parent_tool_use_id":null,"session_id":"%s"}\n' "$1" "$sid"
}
result_ok() {
  printf '{"type":"result","subtype":"success","is_error":false,"result":"done","stop_reason":"end_turn","terminal_reason":"completed","session_id":"%s"}\n' "$sid"
}
result_aborted() {
  printf '{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"terminal_reason":"%s","session_id":"%s"}\n' "$1" "$sid"
}
tool_result() {
  printf '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"%s","is_error":%s}]},"session_id":"%s"}\n' "$1" "$2" "$sid"
}

while IFS= read -r line; do
  case "$line" in
    *'"subtype":"initialize"'*)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"claude_code_version":"%s","account":{"email":"user@example.com"},"session_state":"idle"}}}\n' "$(reqid "$line")" "$version"
      ;;
    *'"subtype":"interrupt"'*)
      ack "$line"
      ;;
    *'"subtype":"set_permission_mode"'*)
      asked=$(printf '%s' "$line" | sed -n 's/.*"mode":"\([^"]*\)".*/\1/p')
      if [ "$asked" = "bypassPermissions" ] && [ -z "$allow_bypass" ]; then
        printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions","error_code":"bypass_not_launched"}}\n' "$(reqid "$line")"
      else
        mode="$asked"
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"mode":"%s"}}}\n' "$(reqid "$line")" "$mode"
        printf '{"type":"system","subtype":"status","status":null,"permissionMode":"%s","session_id":"%s"}\n' "$mode" "$sid"
      fi
      ;;
    *'"subtype":"set_model"'*)
      asked=$(printf '%s' "$line" | sed -n 's/.*"model":"\([^"]*\)".*/\1/p')
      if [ "$asked" = "bogus" ]; then
        printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"Model '"'"'bogus'"'"' not found","error_code":"not_found","model":"%s"}}\n' "$(reqid "$line")" "$model"
      else
        model="resolved-$asked"
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"model":"%s"}}}\n' "$(reqid "$line")" "$model"
      fi
      ;;
    *'"subtype":"apply_flag_settings"'*)
      asked=$(printf '%s' "$line" | sed -n 's/.*"effortLevel":"\([^"]*\)".*/\1/p')
      case "$asked" in low|medium|high|xhigh|max) effort="$asked" ;; esac
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s"}}\n' "$(reqid "$line")"
      ;;
    *'"subtype":"get_settings"'*)
      if [ "$model" = "resolved-no-effort" ]; then applied="null"; else applied="\"$effort\""; fi
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"applied":{"model":"%s","effort":%s}}}}\n' "$(reqid "$line")" "$model" "$applied"
      ;;
    *'"type":"user"'*)
      if [ -n "$model" ]; then
        printf '{"type":"system","subtype":"init","cwd":"%s","session_id":"%s","claude_code_version":"%s","model":"%s","permissionMode":"%s"}\n' "$PWD" "$sid" "$version" "$model" "$mode"
      else
        printf '{"type":"system","subtype":"init","cwd":"%s","session_id":"%s","claude_code_version":"%s","permissionMode":"%s"}\n' "$PWD" "$sid" "$version" "$mode"
      fi
      case "$line" in
        *stubborn*)
          (trap '' TERM; exec sleep 300) </dev/null >/dev/null 2>&1 &
          echo $! > child.txt
          ;;
        *orphan*)
          sleep 300 </dev/null >/dev/null 2>&1 &
          echo $! > child.txt
          ;;
      esac
      case "$line" in
        *litter*) printf 'not json\n' ;;
      esac
      case "$line" in
        *quit*)
          exit 0
          ;;
        *approve*)
          printf '{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"a.txt","content":"hi"}}]},"session_id":"%s"}\n' "$sid"
          printf '{"type":"control_request","request_id":"perm-1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"a.txt","content":"hi"},"tool_use_id":"toolu_1"}}\n'
          IFS= read -r answer
          case "$answer" in
            *'"behavior":"allow"'*'"updatedInput":{"content":"hi","file_path":"a.txt"}'* | *'"behavior":"allow"'*'"updatedInput":{"file_path":"a.txt","content":"hi"}'*)
              tool_result "written" false
              text "done writing"
              result_ok
              ;;
            *'"behavior":"deny"'*)
              tool_result "The user denied this action." true
              text "not writing"
              result_ok
              ;;
            *'"subtype":"interrupt"'*)
              printf '{"type":"control_cancel_request","request_id":"perm-1"}\n'
              ack "$answer"
              tool_result "rejected" true
              result_aborted aborted_tools
              ;;
            *)
              echo "unexpected answer: $answer" >&2
              exit 2
              ;;
          esac
          ;;
        *crash*)
          echo "boom: the agent fell over" >&2
          exit 3
          ;;
        *run*)
          running=1
          printf '{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"a.txt","content":"hi"}}]},"session_id":"%s"}\n' "$sid"
          IFS= read -r req
          running=""
          ack "$req"
          tool_result "rejected" true
          result_aborted aborted_tools
          ;;
        *deaf*)
          text "deaf"
          result_ok
          sleep 300
          ;;
        *stuck*)
          # Hung: answers nothing it reads, until stdin ends or a SIGTERM
          # comes. Each interrupt it ignores gets the text "ignored", so a
          # test sees it arrive; the turn stays open.
          text "working"
          while IFS= read -r req; do
            case "$req" in
              *'"subtype":"interrupt"'*) text "ignored" ;;
            esac
          done
          exit 0
          ;;
        *hang*)
          text "working"
          IFS= read -r req
          ack "$req"
          printf '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"session_id":"%s"}\n' "$sid"
          result_aborted aborted_streaming
          ;;
        *leave*)
          # Ends the turn, then exits 0 between turns: stdout closes at
          # once and the process lingers a moment, so whatever is sent
          # meanwhile meets a CLI that will never read it.
          text "echo"
          result_ok
          exec >&-
          sleep 1
          exit 0
          ;;
        *garbage*)
          # Bytes that are not UTF-8, inside a message and on a stray line,
          # then a line over the driver's cap (32 MiB).
          printf '{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"caf\351"}]},"parent_tool_use_id":null,"session_id":"%s"}\n' "$sid"
          printf 'stray \377\376\n'
          head -c 33554433 /dev/zero | tr '\0' x
          printf '\n'
          text "still here"
          result_ok
          ;;
        *)
          text "echo"
          result_ok
          ;;
      esac
      ;;
  esac
done
exit 0
