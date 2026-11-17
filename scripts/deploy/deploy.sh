#!/usr/bin/env bash
# Cross-builds skywaved (static musl, cargo zigbuild), copies it and the systemd
# user unit over ssh, and runs scripts/deploy/install.sh on the device.
#   usage: deploy.sh [--target <triple>] [--now] [--build-only] <ssh-host>
# The device's arch comes from `uname -m` unless --target is given.
set -euo pipefail

usage() {
  echo "usage: deploy.sh [--target <triple>] [--now] [--build-only] <ssh-host>"
}

target=""
now=0
build_only=0
host=""
while [ $# -gt 0 ]; do
  case "$1" in
    --target)
      if [ $# -lt 2 ]; then
        echo "deploy.sh: --target needs a value" >&2
        usage >&2
        exit 2
      fi
      target="$2"
      shift 2
      ;;
    --now)
      now=1
      shift
      ;;
    --build-only)
      build_only=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    -*)
      echo "deploy.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [ -n "$host" ]; then
        echo "deploy.sh: more than one ssh host given" >&2
        usage >&2
        exit 2
      fi
      host="$1"
      shift
      ;;
  esac
done

if [ -z "$host" ] && { [ "$build_only" -eq 0 ] || [ -z "$target" ]; }; then
  echo "deploy.sh: an ssh host is required" >&2
  usage >&2
  exit 2
fi

if [ -z "$target" ]; then
  arch=$(ssh "$host" uname -m)
  case "$arch" in
    x86_64)
      target=x86_64-unknown-linux-musl
      ;;
    aarch64 | arm64)
      target=aarch64-unknown-linux-musl
      ;;
    *)
      echo "deploy.sh: unknown device arch '$arch'; pass --target <triple>" >&2
      exit 1
      ;;
  esac
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"

echo "==> building $target" >&2
"${CARGO:-cargo}" zigbuild --release --locked -p skywaved --target "$target"
bin="${CARGO_TARGET_DIR:-$root/target}/$target/release/skywaved"

if [ "$build_only" -eq 1 ]; then
  printf '%s\n' "$bin"
  exit 0
fi

if [ "$now" -eq 1 ]; then
  pickup=now
else
  pickup=after-work
fi

echo "==> uploading to $host" >&2
ssh "$host" 'mkdir -p .local/bin .config/systemd/user'
scp "$bin" "$host:.local/bin/.skywaved.new"
scp "$root/contrib/systemd/skywaved.service" "$host:.config/systemd/user/.skywaved.service.new"

echo "==> installing" >&2
ssh "$host" sh -s -- "$pickup" < "$root/scripts/deploy/install.sh"
