#!/usr/bin/env bash
# Android acceptance rig: skywaved with a stub agent behind the link harness,
# reachable from an emulator at 10.0.2.2 (or a phone on the LAN, see below).
#
#   scripts/android-acceptance/run.sh start                # build, start skywaved + proxy
#   scripts/android-acceptance/run.sh pair                # pairing URI through the proxy
#   scripts/android-acceptance/run.sh link 'profile latency=800 jitter=1200 bw=8000'
#   scripts/android-acceptance/run.sh link cut|down|blackhole|up|clean|stats
#   scripts/android-acceptance/run.sh restart-daemon      # stop and start skywaved only
#   scripts/android-acceptance/run.sh skywaved devices list   # any admin command
#   scripts/android-acceptance/run.sh stop
#
# Layout under $STATE_DIR (default /tmp/skywave-acceptance):
#   serve.toml  skywaved listens on 127.0.0.1:$SERVER_PORT (direct, unimpaired)
#   pair.toml   same data dir, listen = the proxy port, so `pair` prints the
#               proxy's address and a device paired with it always goes
#               through the harness
#   data/ root/ skywaved.log linkproxy.log stub.log link.ctl (fifo) *.pid
#
# Env: SERVER_PORT (27444), PROXY_PORT (27443), HOST_NAME (10.0.2.2: how the
# emulator reaches the host's loopback; for a phone on the LAN set it to the
# host's LAN address and PROXY_BIND=0.0.0.0), STUB_DELAY (0.5 s).
# The stub agent (stub-agent.py) never calls a real model.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
STATE="${STATE_DIR:-/tmp/skywave-acceptance}"
SERVER_PORT="${SERVER_PORT:-27444}"
PROXY_PORT="${PROXY_PORT:-27443}"
PROXY_BIND="${PROXY_BIND:-127.0.0.1}"
HOST_NAME="${HOST_NAME:-10.0.2.2}"
SKYWAVED="$REPO/target/debug/skywaved"
PROXY="${CARGO_TARGET_DIR_PROXY:-$STATE/linkproxy-target}/debug/skywave-linkproxy"

write_configs() {
  mkdir -p "$STATE/data" "$STATE/root/demo"
  for which in serve pair; do
    local port=$SERVER_PORT
    [[ $which == pair ]] && port=$PROXY_PORT
    cat >"$STATE/$which.toml" <<EOF
listen = "127.0.0.1:$port"
data_dir = "$STATE/data"
roots = ["$STATE/root"]
server_names = ["$HOST_NAME"]
log_file = "$STATE/skywaved.log"

[agent]
command = "$HERE/stub-agent.py"
EOF
  done
}

start_daemon() {
  STUB_LOG="$STATE/stub.log" STUB_SAFE_PREFIX="$STATE/" STUB_DELAY="${STUB_DELAY:-0.5}" \
    nohup "$SKYWAVED" --config "$STATE/serve.toml" serve >>"$STATE/skywaved.stdout" 2>&1 &
  echo $! >"$STATE/skywaved.pid"
  echo "skywaved pid $(cat "$STATE/skywaved.pid") on 127.0.0.1:$SERVER_PORT"
}

stop_pid() {
  local f="$STATE/$1.pid"
  [[ -f $f ]] || return 0
  kill "$(cat "$f")" 2>/dev/null || true
  rm -f "$f"
}

case "${1:-}" in
start)
  mkdir -p "$STATE"
  (cd "$REPO" && cargo build -q -p skywaved -p skywave-tui)
  (cd "$HERE/linkproxy" && CARGO_TARGET_DIR="${CARGO_TARGET_DIR_PROXY:-$STATE/linkproxy-target}" cargo build -q)
  write_configs
  start_daemon
  [[ -p $STATE/link.ctl ]] || mkfifo "$STATE/link.ctl"
  nohup "$PROXY" "$PROXY_BIND:$PROXY_PORT" "127.0.0.1:$SERVER_PORT" "$STATE/link.ctl" \
    >>"$STATE/linkproxy.log" 2>&1 &
  echo $! >"$STATE/linkproxy.pid"
  echo "link proxy pid $(cat "$STATE/linkproxy.pid") on $PROXY_BIND:$PROXY_PORT"
  ;;
pair)
  "$SKYWAVED" --config "$STATE/pair.toml" pair "${@:2}"
  ;;
link)
  shift
  echo "$*" >"$STATE/link.ctl"
  sleep 0.2
  tail -n 1 "$STATE/linkproxy.log"
  ;;
restart-daemon)
  stop_pid skywaved
  sleep 1
  start_daemon
  ;;
skywaved)
  shift
  "$SKYWAVED" --config "$STATE/serve.toml" "$@"
  ;;
stop)
  stop_pid linkproxy
  stop_pid skywaved
  ;;
*)
  sed -n '2,23p' "$0"
  exit 2
  ;;
esac
