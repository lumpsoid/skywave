#!/bin/sh
# Records a real Claude Code transcript (`~/.claude/projects/<dir>/<id>.jsonl`)
# into tests/fixtures/claude-code/transcripts/, for docs/agents/claude-code.md
# §6c. See that directory's README for the scenarios and the scrubbing.
#
# Record: `record-transcript.sh <scenario> <out dir>`. Runs the real CLI
# (SKYWAVE_REAL_CLAUDE, default `claude`) with trivial prompts in a fresh
# scratch dir under /tmp, then copies the transcript to
# `<out dir>/<scenario>.jsonl`, scrubbed, and each of its subagent
# transcripts to `<out dir>/<scenario>.subagent-<n>.jsonl`. Scenarios:
# two_turn, tools, parallel, interrupt, subagent, compact.
#
# Scrub: `record-transcript.sh --scrub <in.jsonl> <scratch dir> <out.jsonl>`
# applies the README's scrubbing and changes nothing else.
#
# Needs jq. The CLI writes its transcript under ~/.claude/projects (or
# $CLAUDE_CONFIG_DIR/projects) as usual; the scratch dir and that copy are
# left in place.

set -eu

scrub() {
  # $1 in, $2 scratch dir (canonical), $3 out
  sanitised=$(printf '%s' "$2" | sed 's/[^A-Za-z0-9]/-/g')
  jq -c --arg home "$HOME" --arg cwd "$2" --arg dir "$sanitised" '
    select((.type // "" | startswith("api-request")) | not)
    | (if .type == "attachment" then
        .attachment |= {type: .type} | del(.rendered, .renderedRole)
      else . end)
    | walk(
      if type == "string" then
        split($cwd) | join("/WORKDIR")
        | split($dir) | join("-WORKDIR")
        | split($home) | join("$HOME")
      elif type == "object" then
        (if has("signature") then .signature = "<scrubbed-signature>" else . end)
        | (if has("gitBranch") then .gitBranch = "" else . end)
      else . end)' "$1" > "$3"
  # The user's identity rides in context attachments and request lines,
  # both dropped above; refuse a fixture that still names an address.
  if grep -Eq '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}' "$3"; then
    grep -Eo '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}' "$3" | sort -u | grep -v '^noreply@anthropic.com$' &&
      { echo "$3 still holds an e-mail address; scrub it" >&2; exit 1; }
  fi
  return 0
}

if [ "${1:-}" = "--scrub" ]; then
  [ $# -eq 4 ] || { echo "usage: $0 --scrub <in.jsonl> <scratch dir> <out.jsonl>" >&2; exit 2; }
  scrub "$2" "$3" "$4"
  exit 0
fi

[ $# -eq 2 ] || { echo "usage: $0 <scenario> <out dir>" >&2; exit 2; }
scenario=$1
out=$(cd "$2" && pwd)
real="${SKYWAVE_REAL_CLAUDE:-claude}"
projects="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects"
scratch=$(cd "$(mktemp -d /tmp/skywave-transcript-XXXXXX)" && pwd -P)
id=$(cat /proc/sys/kernel/random/uuid)
cd "$scratch"

base="--model haiku --setting-sources project --strict-mcp-config"

# One message as stream-json input.
user_line() {
  jq -nc --arg t "$1" '{type: "user", message: {role: "user", content: $t},
    parent_tool_use_id: null, session_id: ""}'
}

case "$scenario" in
  two_turn)
    "$real" -p $base --session-id "$id" "say hi" > /dev/null
    "$real" -p $base --resume "$id" "what did I just ask?" > /dev/null
    ;;
  tools)
    # --allowedTools takes every argument after it: the prompt goes first.
    "$real" -p "Do these one at a time, one tool call each: run the Bash command \`echo hi\`; \
Write the file note.txt containing the line one; Read note.txt; Edit note.txt to \
replace one with two; run the Bash command \`seq 1 30000\`. Then say done." \
      $base --session-id "$id" --allowedTools "Bash Write Read Edit" > /dev/null
    ;;
  parallel)
    printf 'alpha\n' > a.txt
    printf 'beta\n' > b.txt
    "$real" -p "Read a.txt and b.txt in parallel: two Read calls in one message. \
Then say what they hold." $base --session-id "$id" --allowedTools Read > /dev/null
    ;;
  interrupt)
    # The interrupt arrives while the Bash sleep runs; a second message
    # follows in the same process.
    {
      jq -nc '{type: "control_request", request_id: "init", request: {subtype: "initialize"}}'
      # (A plain `sleep 30` may be refused by a hook as a standalone sleep.)
      user_line "Run the Bash command \`python3 -c 'import time; time.sleep(30)'\`, then say done."
      sleep 10
      jq -nc '{type: "control_request", request_id: "int", request: {subtype: "interrupt"}}'
      sleep 5
      user_line "say ok"
      sleep 15
    } | "$real" -p $base --session-id "$id" --allowedTools Bash \
      --input-format stream-json --output-format stream-json --verbose > /dev/null
    ;;
  subagent)
    "$real" -p "Use the Agent tool (subagent_type general-purpose, not in the background) to \
ask a helper to reply with only the word PONG. Wait for it, then tell me what it said." \
      $base --session-id "$id" --allowedTools Agent > /dev/null
    ;;
  compact)
    "$real" -p $base --session-id "$id" "remember the word PELICAN and say ok" > /dev/null
    "$real" -p $base --resume "$id" "/compact" > /dev/null
    "$real" -p $base --resume "$id" "which word did I ask you to remember?" > /dev/null
    ;;
  *)
    echo "unknown scenario: $scenario" >&2
    exit 2
    ;;
esac

file=$(find "$projects" -name "$id.jsonl" -not -path '*/subagents/*' | head -n 1)
[ -n "$file" ] || { echo "no transcript for $id under $projects" >&2; exit 1; }
scrub "$file" "$scratch" "$out/$scenario.jsonl"
n=0
for sub in "${file%.jsonl}"/subagents/*.jsonl; do
  [ -e "$sub" ] || continue
  n=$((n + 1))
  scrub "$sub" "$scratch" "$out/$scenario.subagent-$n.jsonl"
done
echo "$scenario: $file ($("$real" --version))"
