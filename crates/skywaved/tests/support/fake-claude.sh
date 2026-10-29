#!/bin/sh
# A stand-in for the agent CLI in tests/claude_code.rs. It speaks just
# enough of the stream-json protocol (docs/agents/claude-code.md), with
# shapes copied from the T07 fixtures, and replies only to what it reads, so
# every run is the same.
#
# It writes its argv to ./argv.txt (the session workdir). A user message
# containing "approve" asks for a Write permission; one containing "hang"
# streams until interrupted; one containing "crash" exits 3 with a stderr
# line; any other message gets the text "echo".
# `--resume 00000000-0000-0000-0000-000000000000` fails like an unknown id.

printf '%s\n' "$@" > argv.txt

sid=""
prev=""
for arg in "$@"; do
  case "$prev" in --session-id|--resume) sid="$arg" ;; esac
  prev="$arg"
done

if [ "$sid" = "00000000-0000-0000-0000-000000000000" ]; then
  echo "No conversation found with session ID: $sid" >&2
  printf '{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,"result":null,"errors":["No conversation found with session ID: %s"],"session_id":"%s"}\n' "$sid" "$sid"
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
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"claude_code_version":"2.1.310","account":{"email":"user@example.com"},"session_state":"idle"}}}\n' "$(reqid "$line")"
      ;;
    *'"subtype":"interrupt"'*)
      ack "$line"
      ;;
    *'"type":"user"'*)
      printf '{"type":"system","subtype":"init","cwd":"%s","session_id":"%s","claude_code_version":"2.1.310"}\n' "$PWD" "$sid"
      case "$line" in
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
        *hang*)
          text "working"
          IFS= read -r req
          ack "$req"
          printf '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"session_id":"%s"}\n' "$sid"
          result_aborted aborted_streaming
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
