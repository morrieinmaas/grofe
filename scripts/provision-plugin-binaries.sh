#!/usr/bin/env bash
# provision-plugin-binaries.sh: the release binaries TP.8 and TP.9 run
# against, provisioned by canopy's own checksum-verified function.
#
#   scripts/provision-plugin-binaries.sh <canopy-checkout> <out-dir>
#
# Copies canopy's committed lib/ and plugins/ into <out-dir>/store (the
# checkout may be read-only, and whatever an install left untracked in it,
# such as another platform's binaries, stays out) and runs canopy_provision_artifacts there, which fetches each
# archive in plugins/ARTIFACTS for this machine and refuses any archive or
# file whose sha256 differs from the recorded one. Then prints the two
# settings the tests read, as KEY=VALUE lines (for $GITHUB_ENV):
#
#   GROFE_TEST_OPENSESSIONS  opensessions' bin/ (server, sidebar, lazydiff)
#   GROFE_TEST_XRE           tmux-fzf-url's bin/xre
#
# Exits nonzero if canopy could not provide every artifact.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <canopy-checkout> <out-dir>" >&2
  exit 2
fi
canopy="$(cd "$1" && pwd)"
mkdir -p "$2"
out="$(cd "$2" && pwd)"
store="$out/store"

rm -rf "$store"
mkdir -p "$store"
git -C "$canopy" archive HEAD lib plugins | tar -x -C "$store"

# shellcheck disable=SC2016 # expanded by the inner bash
env -u CANOPY_ARTIFACTS CANOPY_STORE="$store" bash -c '
  . "$CANOPY_STORE/lib/env.sh"
  . "$CANOPY_STORE/lib/plugins.sh"
  canopy_provision_artifacts
' >&2

echo "GROFE_TEST_OPENSESSIONS=$store/plugins/opensessions/bin"
echo "GROFE_TEST_XRE=$store/plugins/tmux-fzf-url/bin/xre"
