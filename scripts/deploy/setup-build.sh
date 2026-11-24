#!/usr/bin/env bash
# Installs what deploy.sh builds with: zig, cargo-zigbuild and the musl Rust
# targets. Everything goes under your home directory; no sudo.
#   usage: setup-build.sh [--zig <version>] [--target <triple>]...
# Without --zig, an installed zig is kept, else the latest stable release is
# installed. Without --target, adds the musl target for this machine's CPU.
# Safe to run again: each step skips what is already there.
set -euo pipefail

usage() {
  echo "usage: setup-build.sh [--zig <version>] [--target <triple>]..."
}

zig_version=""
targets=()
while [ $# -gt 0 ]; do
  case "$1" in
    --zig | --target)
      if [ $# -lt 2 ]; then
        echo "setup-build.sh: $1 needs a value" >&2
        usage >&2
        exit 2
      fi
      if [ "$1" = --zig ]; then
        zig_version="$2"
      else
        targets+=("$2")
      fi
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "setup-build.sh: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

arch=$(uname -m)
case "$arch" in
  x86_64)
    zig_arch=x86_64-linux
    host_target=x86_64-unknown-linux-musl
    ;;
  aarch64 | arm64)
    zig_arch=aarch64-linux
    host_target=aarch64-unknown-linux-musl
    ;;
  *)
    echo "setup-build.sh: unknown arch '$arch'" >&2
    exit 1
    ;;
esac
if [ ${#targets[@]} -eq 0 ]; then
  targets=("$host_target")
fi

opt="$HOME/.local/opt"
bin="$HOME/.local/bin"

if [ -z "$zig_version" ] && command -v zig >/dev/null; then
  echo "==> zig $(zig version) already installed at $(command -v zig)" >&2
else
  echo "==> installing zig ${zig_version:-(latest stable)} for $zig_arch" >&2
  cache="${XDG_CACHE_HOME:-$HOME/.cache}"
  mkdir -p "$cache"
  work=$(mktemp -d "$cache/skywave-zig.XXXXXX")
  trap 'rm -rf "$work"' EXIT

  curl -fsSL https://ziglang.org/download/index.json -o "$work/index.json"
  # Prints "<tarball url> <sha256>" for the requested (or newest stable) release.
  # Captured first, so a failure stops the script under set -e.
  release=$(python3 -I - "$work/index.json" "$zig_version" "$zig_arch" <<'EOF'
import json, sys

index_path, version, arch = sys.argv[1:]
index = json.load(open(index_path))
if not version:
    stable = [k for k in index if k.replace(".", "").isdigit()]
    version = max(stable, key=lambda k: tuple(int(p) for p in k.split(".")))
release = index.get(version)
if release is None:
    sys.exit(f"setup-build.sh: no zig release '{version}'")
build = release.get(arch)
if build is None:
    sys.exit(f"setup-build.sh: zig {version} has no {arch} build")
print(build["tarball"], build["shasum"])
EOF
  )
  read -r url shasum <<<"$release"

  tarball="$work/${url##*/}"
  curl -fL --progress-bar "$url" -o "$tarball"
  echo "$shasum  $tarball" | sha256sum -c --quiet -

  name="${url##*/}"
  name="${name%.tar.xz}"
  mkdir -p "$opt" "$bin"
  rm -rf "${opt:?}/$name"
  tar -xJf "$tarball" -C "$opt"
  ln -sfn "$opt/$name/zig" "$bin/zig"
  echo "==> zig $("$bin/zig" version) at $bin/zig" >&2

  case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "note: $bin is not on PATH; add it so cargo zigbuild finds zig" >&2 ;;
  esac
fi

if command -v cargo-zigbuild >/dev/null; then
  echo "==> $(cargo-zigbuild --version) already installed" >&2
else
  echo "==> installing cargo-zigbuild (compiles from source)" >&2
  cargo install --locked cargo-zigbuild
fi

echo "==> adding Rust targets: ${targets[*]}" >&2
rustup target add "${targets[@]}"

echo "==> done. Try: scripts/deploy/deploy.sh --build-only --local" >&2
