#!/usr/bin/env bash
# The project's version lives in two files that must agree:
#   Cargo.toml   [workspace.package] version = "X.Y.Z"
#   gui/pubspec.yaml   version: X.Y.Z+N   (N: Android versionCode)
#
#   usage: version.sh show           print X.Y.Z+N
#          version.sh check          fail unless both files agree
#          version.sh bump X.Y.Z     set X.Y.Z in both, N += 1, date the
#                                    changelog's [Unreleased] section and
#                                    refresh Cargo.lock
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cargo_toml="$root/Cargo.toml"
pubspec="$root/gui/pubspec.yaml"
changelog="$root/CHANGELOG.md"

die() {
  echo "version.sh: $*" >&2
  exit 1
}

# The version in [workspace.package], the first `version = ` after it.
cargo_version() {
  awk '
    /^\[workspace\.package\]/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && /^version *= *"/ { gsub(/^version *= *"|".*$/, ""); print; exit }
  ' "$cargo_toml"
}

pubspec_version() {
  sed -n 's/^version: *\([^ ]*\).*/\1/p' "$pubspec" | head -n1
}

check() {
  local cargo pub
  cargo=$(cargo_version)
  pub=$(pubspec_version)
  [ -n "$cargo" ] || die "no version in $cargo_toml [workspace.package]"
  [[ "$pub" =~ ^[0-9]+\.[0-9]+\.[0-9]+\+[0-9]+$ ]] ||
    die "gui/pubspec.yaml version '$pub' is not X.Y.Z+N"
  [ "${pub%%+*}" = "$cargo" ] ||
    die "Cargo.toml says $cargo, gui/pubspec.yaml says ${pub%%+*}"
}

bump() {
  local new=$1 old code today
  [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "'$new' is not X.Y.Z"
  check
  old=$(cargo_version)
  code=$(pubspec_version)
  code=$((${code##*+} + 1))
  today=$(date +%F)

  grep -q '^## \[Unreleased\]' "$changelog" ||
    die "CHANGELOG.md has no '## [Unreleased]' section"
  if grep -q "^## \[$new\]" "$changelog"; then
    die "CHANGELOG.md already has a [$new] section"
  fi

  # Only the [workspace.package] version line.
  awk -v new="$new" '
    /^\[workspace\.package\]/ { in_pkg = 1 }
    /^\[/ && !/^\[workspace\.package\]/ { in_pkg = 0 }
    in_pkg && !done && /^version *= *"/ { print "version = \"" new "\""; done = 1; next }
    { print }
  ' "$cargo_toml" >"$cargo_toml.tmp"
  mv "$cargo_toml.tmp" "$cargo_toml"

  sed -i "s/^version: .*/version: $new+$code/" "$pubspec"

  # A fresh [Unreleased] above the newly dated section.
  sed -i "s/^## \[Unreleased\]$/## [Unreleased]\n\n## [$new] - $today/" "$changelog"

  (cd "$root" && cargo update --workspace --quiet)
  check
  echo "$old -> $new+$code" >&2
}

case "${1:-}" in
  show)
    check
    pubspec_version
    ;;
  check)
    check
    ;;
  bump)
    [ $# -eq 2 ] || die "usage: version.sh bump X.Y.Z"
    bump "$2"
    ;;
  *)
    echo "usage: version.sh show | check | bump X.Y.Z" >&2
    exit 2
    ;;
esac
