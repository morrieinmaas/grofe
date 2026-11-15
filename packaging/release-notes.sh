#!/usr/bin/env bash
# release-notes.sh <version>: print CHANGELOG.md's section for <version>,
# the body of that tag's GitHub release. Fails when there is none.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: release-notes.sh <version>}"
version="${version#v}"
notes="$(awk -v v="$version" '
  index($0, "## [" v "]") == 1 { on = 1; next }
  on && /^## \[/ { exit }
  on
' CHANGELOG.md)"
[ -n "$(echo "$notes" | tr -d '[:space:]')" ] || { echo "release-notes.sh: no CHANGELOG.md section for $version" >&2; exit 1; }
printf '%s\n' "$notes"
