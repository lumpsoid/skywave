#!/usr/bin/env bash
# Runs every gate in CI's order and stops at the first failure.
#   usage: check.sh [--no-gui]
# --no-gui skips the Flutter head (rinf gen, analyze, test).
set -euo pipefail

gui=1
case "${1:-}" in
  --no-gui) gui=0 ;;
  "") ;;
  *)
    echo "usage: check.sh [--no-gui]" >&2
    exit 2
    ;;
esac

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

step() { echo "==> $*" >&2; }

step "versions agree"
scripts/version.sh check
step "cargo fmt"
cargo fmt --all --check
step "cargo clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
step "cargo test"
cargo test --workspace --locked

if [ "$gui" -eq 1 ]; then
  cd gui
  step "rinf gen"
  rinf gen
  step "flutter analyze"
  flutter analyze
  step "flutter test"
  flutter test
fi

step "all gates passed"
