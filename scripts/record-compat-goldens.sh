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
# the test refuses unless the tmux prints `tmux 3.7b`. As a guard the script
# lists the host's real tmux and grofe socket directories before and after
# and fails if they changed.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
image=localhost/grofe-compat-golden:3.7b
check=0
tmux_path=/usr/local/bin/tmux
scenarios=()

while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=1 ;;
    --tmux) tmux_path=$2; shift ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) scenarios+=("$1") ;;
  esac
  shift
done

# DOCKER_HOST may name a socket that does not exist; podman is used directly.
unset DOCKER_HOST

socket_dirs() {
  local uid
  uid=$(id -u)
  for d in "${TMUX_TMPDIR:-/tmp}/tmux-$uid" \
           "${XDG_RUNTIME_DIR:-/nonexistent}/grofe" \
           "${TMPDIR:-/tmp}/grofe-$uid"; do
    [ -d "$d" ] && ls -1ia "$d"
  done
  return 0
}
before=$(socket_dirs)

podman build -q -t "$image" -f "$repo/containers/compat/golden.Containerfile" \
  "$repo/containers/compat" >/dev/null

out_mount=()
out_env=()
if [ "$check" = 1 ]; then
  scratch=$(mktemp -d)
  trap 'rm -rf "$scratch"' EXIT
  out_mount=(-v "$scratch:/out:Z")
  out_env=(-e GROFE_COMPAT_OUT=/out)
fi

# A fixed hostname keeps pane titles (and so the status line) stable; the
# normalizer still replaces it, for the grofe side.
podman run --rm --hostname grofe-golden \
  -v "$repo:/src:Z" \
  -v grofe-compat-golden-target:/target \
  -v grofe-compat-golden-cargo:/usr/local/cargo/registry \
  ${out_mount[@]+"${out_mount[@]}"} \
  -e CARGO_TARGET_DIR=/target \
  -e GROFE_COMPAT_TMUX="$tmux_path" \
  -e GROFE_COMPAT_SCENARIOS="${scenarios[*]:-}" \
  ${out_env[@]+"${out_env[@]}"} \
  "$image" \
  cargo test --quiet --test compat -- --ignored --exact record::record_goldens --nocapture

after=$(socket_dirs)
if [ "$before" != "$after" ]; then
  echo "error: the host's tmux or grofe socket directories changed during recording" >&2
  diff <(echo "$before") <(echo "$after") >&2 || true
  exit 1
fi

if [ "$check" = 1 ]; then
  golden=$repo/tests/fixtures/compat/golden
  status=0
  while IFS= read -r -d '' f; do
    rel=${f#"$scratch"/}
    if ! diff -u "$golden/$rel" "$f"; then
      status=1
    fi
  done < <(find "$scratch" -type f -print0)
  [ "$status" = 0 ] && echo "goldens reproduce byte for byte"
  exit "$status"
fi
