#!/usr/bin/env bash
# Cross-builds skywaved (static musl, cargo zigbuild), copies it and the systemd
# user unit over ssh, and runs scripts/deploy/install.sh on the device. With
# --local, the device is this machine: no ssh, plain copies.
#   usage: deploy.sh [--target <triple>] [--now] [--build-only] <ssh-host>|--local
# The device's arch comes from `uname -m` unless --target is given.
set -euo pipefail

usage() {
  echo "usage: deploy.sh [--target <triple>] [--now] [--build-only] <ssh-host>|--local"
}

target=""
now=0
build_only=0
local=0
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
    --local)
      local=1
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

if [ "$local" -eq 1 ] && [ -n "$host" ]; then
  echo "deploy.sh: --local takes no ssh host" >&2
  usage >&2
  exit 2
fi

if [ "$local" -eq 0 ] && [ -z "$host" ] && { [ "$build_only" -eq 0 ] || [ -z "$target" ]; }; then
  echo "deploy.sh: an ssh host or --local is required" >&2
  usage >&2
  exit 2
fi

if [ -z "$target" ]; then
  if [ "$local" -eq 1 ]; then
    arch=$(uname -m)
  else
    arch=$(ssh "$host" uname -m)
  fi
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

if [ "$local" -eq 1 ]; then
  echo "==> copying into $HOME" >&2
  mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user"
  cp "$bin" "$HOME/.local/bin/.skywaved.new"
  cp "$root/contrib/systemd/skywaved.service" "$HOME/.config/systemd/user/.skywaved.service.new"

  echo "==> installing" >&2
  sh "$root/scripts/deploy/install.sh" "$pickup"
  exit 0
fi

echo "==> uploading to $host" >&2
ssh "$host" 'mkdir -p .local/bin .config/systemd/user'
scp "$bin" "$host:.local/bin/.skywaved.new"
scp "$root/contrib/systemd/skywaved.service" "$host:.config/systemd/user/.skywaved.service.new"

echo "==> installing" >&2
ssh "$host" sh -s -- "$pickup" < "$root/scripts/deploy/install.sh"
