#!/usr/bin/env bash
# Record the tmux compat goldens from real tmux 3.7b (task TC.0b, spec §9).
#
# Usage: scripts/record-compat-goldens.sh [--check] [--tmux PATH] [SCENARIO...]
#
#   (default)    record every tests/fixtures/compat/golden/**/*.scenario into
#                its sibling .golden, plus golden/list-commands.txt
#   --check      record into a scratch dir and diff against the committed
#                goldens; exits nonzero on any difference (the determinism check)
#   --tmux PATH  the tmux to record from, as a path inside the container
#                (default /usr/local/bin/tmux, built by the image)
#   SCENARIO     record only scenarios whose path (relative to golden/)
#                contains one of these strings, e.g. fe_1/ or tc_0b/errors
#
# Recording happens only inside the container built from
# containers/compat/golden.Containerfile, never against the host's tmux, and
# the test refuses unless the tmux prints `tmux 3.7b`. Crates are fetched in
# a first container run; the recording run has no network, sees the repo
# read-only, and hands the goldens back as a tar on stdout. As a guard the
# script lists the host's real tmux and grofe socket directories before and
# after and fails if they changed.
#
# The image is always linux/amd64, emulated on another host: tmux's output
# depends on the platform, as copy-mode search keeps the case of a pattern
# outside ASCII only where `char` is signed (tm_3/unicode).
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
# The image has a second tmux, built with sixel, for golden/g3/ (G3); its own
# tag keeps a checkout without it from replacing this one.
image=localhost/grofe-compat-golden:3.7b-sixel
check=0
tmux_path=/usr/local/bin/tmux
scenarios=()

while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=1 ;;
    --tmux) tmux_path=$2; shift ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    *) scenarios+=("$1") ;;
  esac
  shift
done

# DOCKER_HOST may name a socket that does not exist; podman is used directly.
unset DOCKER_HOST

# Inode and name of every socket in the user's tmux and grofe directories.
# `grofe-*` entries are skipped: other agents' private scratch servers
# (`tmux -L grofe-<task>`) come and go on their own.
socket_dirs() {
  local uid d
  uid=$(id -u)
  for d in "${TMUX_TMPDIR:-/tmp}/tmux-$uid" \
           "${XDG_RUNTIME_DIR:-/nonexistent}/grofe" \
           "${TMPDIR:-/tmp}/grofe-$uid"; do
    [ -d "$d" ] || continue
    echo "$d"
    ls -1ia "$d" | awk '$2 !~ /^grofe-/'
  done
}
before=$(socket_dirs)

podman build -q --platform linux/amd64 -t "$image" -f "$repo/containers/compat/golden.Containerfile" \
  "$repo/containers/compat" >/dev/null

run=(podman run --rm --platform linux/amd64
  -v "$repo:/src:ro"
  -v grofe-compat-golden-target-v2:/target
  -v grofe-compat-golden-cargo-v2:/home/golden/cargo)

"${run[@]}" "$image" cargo fetch --locked >&2

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# A fixed hostname keeps pane titles (and so the status line) stable; the
# normalizer still replaces it, for the grofe side.
"${run[@]}" --network none --hostname grofe-golden \
  -e CARGO_NET_OFFLINE=true \
  -e GROFE_COMPAT_TMUX="$tmux_path" \
  -e GROFE_COMPAT_SCENARIOS="${scenarios[*]:-}" \
  -e GROFE_COMPAT_OUT=/tmp/out \
  "$image" sh -c '
    cargo test --locked --quiet --test compat -- \
      --ignored --exact record::record_goldens --nocapture >&2 &&
    tar -C /tmp/out -cf - .' | tar -C "$scratch" -xf -

after=$(socket_dirs)
if [ "$before" != "$after" ]; then
  echo "error: the host's tmux or grofe socket directories changed during recording" >&2
  diff <(echo "$before") <(echo "$after") >&2 || true
  exit 1
fi

golden=$repo/tests/fixtures/compat/golden
if [ "$check" = 0 ]; then
  cp -R "$scratch/." "$golden/"
  exit 0
fi
status=0
while IFS= read -r -d '' f; do
  rel=${f#"$scratch"/}
  if ! diff -u "$golden/$rel" "$f"; then
    status=1
  fi
done < <(find "$scratch" -type f -print0)
[ "$status" = 0 ] && echo "goldens reproduce byte for byte"
exit "$status"
