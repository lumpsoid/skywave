#!/bin/sh
# Records a real Claude Code session into a fixture in the format of
# tests/fixtures/claude-code/README.md, for docs/agents/claude-code.md §3.5.
#
# Record: set it as skywaved's agent command (`[agent] command = ".../record-claude.sh"`)
# and start the daemon with SKYWAVE_RECORD=<absolute path of the .ndjson to
# write>. Each run appends: a `note` with the argv, an `in` record per line
# skywaved writes to the CLI's stdin, an `out` record per line the CLI writes
# to stdout, a `note` when stdin closes and one with the exit code. Each
# record is written before its line is passed on, so the file keeps the
# order in which the lines happened. stderr is passed on and also appended
# to <file>.stderr. SKYWAVE_REAL_CLAUDE names the real CLI (default
# `claude`). Without SKYWAVE_RECORD the real CLI just runs.
#
# Scrub: `record-claude.sh --scrub <in.ndjson> <out.ndjson>` applies the
# fixture README's scrubbing (account email and organization, pid, $HOME,
# thinking signatures) and changes nothing else.
#
# Needs jq. If the CLI exits on its own, the wrapper exits once skywaved
# closes its stdin: fine for a recording, not for everyday use.

set -u

if [ "${1:-}" = "--scrub" ]; then
  [ $# -eq 3 ] || { echo "usage: $0 --scrub <in.ndjson> <out.ndjson>" >&2; exit 2; }
  exec jq -c --arg home "$HOME" '
    walk(
      if type == "string" then split($home) | join("$HOME")
      elif type == "object" then
        (if has("signature") then .signature = "<scrubbed-signature>" else . end)
        | (if has("pid") then .pid = 12345 else . end)
        | (if (.account | type) == "object" then
            .account |= (
              (if has("email") then .email = "user@example.com" else . end)
              | (if has("organization") then .organization = "<scrubbed-organization>" else . end))
          else . end)
      else . end)' "$2" > "$3"
fi

real="${SKYWAVE_REAL_CLAUDE:-claude}"
out="${SKYWAVE_RECORD:-}"
[ -n "$out" ] || exec "$real" "$@"

note() {
  jq -nc --arg t "$1" '{dir: "note", text: $t}' >> "$out"
}

# Appends `line` as a `dir` record; a line that is not JSON becomes a note.
record() {
  printf '%s\n' "$2" | jq -c --arg d "$1" '{dir: $d, msg: .}' >> "$out" 2>/dev/null ||
    note "non-JSON $1 line: $2"
}

forward() {
  while IFS= read -r line || [ -n "$line" ]; do
    record "$1" "$line"
    printf '%s\n' "$line"
  done
}

status=$(mktemp)
trap 'rm -f "$status"' EXIT
note "argv: claude $*"
{ forward in; note "closing stdin"; } |
  { { "$real" "$@"; echo $? > "$status"; } 2>&1 1>&3 | tee -a "$out.stderr" >&2; } 3>&1 |
  forward out
code=$(cat "$status")
note "exit $code"
[ -s "$out.stderr" ] || rm -f "$out.stderr"
exit "${code:-1}"
