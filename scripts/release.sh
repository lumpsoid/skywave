#!/usr/bin/env bash
# Tags a release of the version already committed (scripts/version.sh bump,
# then a `chore(release): X.Y.Z` commit). Checks that main is clean, the
# versions agree and the changelog has the section, runs every gate, then
# makes the annotated tag vX.Y.Z with that section as its message. Never
# pushes: it prints the push command.
#   usage: release.sh [--no-gui]   (--no-gui is passed to check.sh)
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

die() {
  echo "release.sh: $*" >&2
  exit 1
}

[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "the working tree is not clean"

scripts/version.sh check
full=$(scripts/version.sh show)
version=${full%%+*}
tag="v$version"

git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"

# The section's body: from its heading to the next `## [`, without either,
# and without blank lines at either end.
notes=$(awk -v h="## [$version]" '
  index($0, h) == 1 { on = 1; next }
  on && /^## \[/ { exit }
  on { lines[++n] = $0 }
  END {
    s = 1; while (s <= n && lines[s] ~ /^[[:space:]]*$/) s++
    e = n; while (e >= s && lines[e] ~ /^[[:space:]]*$/) e--
    for (i = s; i <= e; i++) print lines[i]
  }
' CHANGELOG.md)
[ -n "$notes" ] || die "CHANGELOG.md has no [$version] section, or it is empty"

scripts/check.sh "$@"

# --cleanup=verbatim: the default would drop `### Added` as a comment.
git tag -a --cleanup=verbatim -F - "$tag" <<EOF
skywave $version

$notes
EOF

echo "Tagged $tag ($full). Push with:" >&2
echo "  git push origin main $tag" >&2
