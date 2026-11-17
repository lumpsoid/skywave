#!/usr/bin/env bash
# Tags a release of the version already committed (scripts/version.sh bump,
# then a `chore(release): X.Y.Z` commit), and checks a pushed tag in CI.
#
#   usage: release.sh [--no-gui]   check main and the release, run every
#                                  gate (--no-gui is passed to check.sh),
#                                  then make the annotated tag vX.Y.Z with
#                                  the changelog section as its message.
#                                  Never pushes: it prints the push command.
#          release.sh verify TAG   what .github/workflows/release.yml runs
#                                  on a pushed tag: the same release checks,
#                                  plus that TAG is annotated, names this
#                                  version, is checked out, and is on
#                                  origin/main. Runs no gates (CI runs them).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

die() {
  echo "release.sh: $*" >&2
  exit 1
}

# The changelog section's body for $1: from its heading to the next `## [`,
# without either, and without blank lines at either end.
notes() {
  awk -v h="## [$1]" '
    index($0, h) == 1 { on = 1; next }
    on && /^## \[/ { exit }
    on { lines[++n] = $0 }
    END {
      s = 1; while (s <= n && lines[s] ~ /^[[:space:]]*$/) s++
      e = n; while (e >= s && lines[e] ~ /^[[:space:]]*$/) e--
      for (i = s; i <= e; i++) print lines[i]
    }
  ' CHANGELOG.md
}

# Android's versionCode (the N of X.Y.Z+N) in gui/pubspec.yaml on stdin.
version_code() {
  sed -n 's/^version: *[^+]*+\([0-9]*\).*/\1/p' | head -n1
}

# The checks every release passes, for the version at HEAD, excluding the
# tag $1 itself from the earlier releases.
check_release() {
  local tag=$1 full version code t prev
  scripts/version.sh check
  full=$(scripts/version.sh show)
  version=${full%%+*}
  code=${full##*+}

  [ -n "$(notes "$version")" ] ||
    die "CHANGELOG.md has no [$version] section, or it is empty"

  # A published versionCode can never be reused: Android refuses the update.
  for t in $(git tag -l 'v*.*.*'); do
    [ "$t" = "$tag" ] && continue
    prev=$(git show "$t:gui/pubspec.yaml" | version_code)
    [ -n "$prev" ] || continue
    [ "$code" -gt "$prev" ] ||
      die "versionCode $code is not above $t's $prev"
  done
}

verify() {
  local tag=$1 full version
  full=$(scripts/version.sh show)
  version=${full%%+*}

  [ "$tag" = "v$version" ] || die "tag $tag does not match version $version"
  [ "$(git cat-file -t "refs/tags/$tag")" = tag ] ||
    die "$tag is not an annotated tag; its message is the release notes"
  [ "$(git rev-parse "$tag^{commit}")" = "$(git rev-parse HEAD)" ] ||
    die "$tag is not the checked-out commit"
  git merge-base --is-ancestor HEAD origin/main ||
    die "$tag is not on origin/main"

  check_release "$tag"
  echo "$version"
}

tag() {
  local full version tag
  [ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
  [ -z "$(git status --porcelain)" ] || die "the working tree is not clean"

  scripts/version.sh check
  full=$(scripts/version.sh show)
  version=${full%%+*}
  tag="v$version"

  git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"
  check_release "$tag"

  scripts/check.sh "$@"

  # --cleanup=verbatim: the default would drop `### Added` as a comment.
  git tag -a --cleanup=verbatim -F - "$tag" <<EOF
skywave $version

$(notes "$version")
EOF

  echo "Tagged $tag ($full). Push with:" >&2
  echo "  git push origin main $tag" >&2
}

if [ "${1:-}" = verify ]; then
  [ $# -eq 2 ] || die "usage: release.sh verify TAG"
  verify "$2"
else
  tag "$@"
fi
