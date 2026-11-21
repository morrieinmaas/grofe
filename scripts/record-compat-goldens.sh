#!/usr/bin/env bash
# Record the tmux compat goldens from real tmux 3.7b (task TC.0b, spec §9).
#
# Usage: scripts/record-compat-goldens.sh [--check] [--exec] [--tmux PATH] [SCENARIO...]
#
#   (default)    record every tests/fixtures/compat/golden/**/*.scenario into
#                its sibling .golden, plus golden/list-commands.txt, and the
#                control-mode cases of tests/control_mode into
#                golden/control_mode/<case>.golden
#   --check      record into a scratch dir and diff against the committed
#                goldens; exits nonzero on any difference (the determinism check)
#   --exec       drive the scenarios from the host, as the control-mode cases
#                are, for a host whose emulated container cannot run rustc
#   --tmux PATH  the tmux to record from, as a path inside the container
#                (default /usr/local/bin/tmux, built by the image)
#   SCENARIO     record only scenarios whose path (relative to golden/)
#                contains one of these strings, e.g. fe_1/ or tc_0b/errors;
#                control_mode records the control-mode cases (alone, it
#                records only them)
#
# Recording happens only inside the container built from
# containers/compat/golden.Containerfile, never against the host's tmux, and
# the test refuses unless the tmux prints `tmux 3.7b`. Crates are fetched in
# a first container run; the recording run has no network, sees the repo
# read-only, and hands the goldens back as a tar on stdout. As a guard the
# script lists the host's real tmux and grofe socket directories before and
# after and fails if they changed.
#
# The control-mode cases are driven from the host (`cargo test --test
# control_mode`, as rustc does not run in the emulated container) against a
# wrapper that runs each tmux command in a container of the same image, so
# the tmux they record is still the container's. `--exec` records the
# scenarios that way too: the wrapper makes each scenario's HOME and
# TMUX_TMPDIR in the container at the host's paths and runs tmux in HOME,
# so keep TMPDIR short (a socket path has a length limit). The container's
# pids are not raised to 5 digits there, so such a scenario should print none.
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
exec_mode=0
tmux_path=/usr/local/bin/tmux
scenarios=()

while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=1 ;;
    --tmux) tmux_path=$2; shift ;;
    --exec) exec_mode=1 ;;
    -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
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

scratch=$(mktemp -d)
mkdir "$scratch/out"
ctr=
trap 'rm -rf "$scratch"; [ -z "$ctr" ] || podman rm -f "$ctr" >/dev/null' EXIT

want_control=0
want_scenarios=0
if [ ${#scenarios[@]} = 0 ]; then
  want_control=1 want_scenarios=1
fi
for s in ${scenarios[@]+"${scenarios[@]}"}; do
  if [ "$s" = control_mode ]; then want_control=1; else want_scenarios=1; fi
done

if [ "$want_control" = 1 ]; then
  ctr=$(podman run -d --rm --platform linux/amd64 --network none --hostname grofe-golden \
    -v "$repo:$repo:ro" "$image" sleep 3600)
  cat > "$scratch/tmux" <<WRAP
#!/bin/sh
tty=
[ -t 0 ] && tty=-t
exec podman exec -i \$tty -e TERM=xterm-256color -e SHELL=/bin/sh -e TMUX_TMPDIR=/tmp \\
  "$ctr" "$tmux_path" "\$@"
WRAP
  chmod +x "$scratch/tmux"
  (cd "$repo" && GROFE_COMPAT_TMUX="$scratch/tmux" GROFE_COMPAT_OUT="$scratch/out" \
    cargo test --locked --quiet --test control_mode -- \
      --ignored --exact record_control_goldens --nocapture >&2)
fi

if [ "$want_scenarios" = 1 ] && [ "$exec_mode" = 1 ]; then
  [ -n "$ctr" ] || ctr=$(podman run -d --rm --platform linux/amd64 --network none \
    --hostname grofe-golden -v "$repo:$repo:ro" "$image" sleep 3600)
  mkdir "$scratch/exec"
  # The recorder clears tmux's environment to fixed_env's, so podman gets
  # back the host's HOME and its own path.
  for name in tmux tmux-sixel; do
    cat > "$scratch/exec/$name" <<WRAP
#!/bin/sh
tty=
[ -t 0 ] && tty=-t
h=\${HOME:-/tmp} t=\${TMUX_TMPDIR:-/tmp}
HOME="$HOME" exec "$(command -v podman)" exec -i \$tty -e TERM=xterm-256color \\
  -e SHELL=/bin/sh -e LANG=C.UTF-8 -e HOME="\$h" -e TMUX_TMPDIR="\$t" "$ctr" \\
  sh -c 'mkdir -p "\$HOME" "\$TMUX_TMPDIR" && cd "\$HOME" && exec "\$0" "\$@"' \\
  "$(dirname "$tmux_path")/$name" "\$@"
WRAP
    chmod +x "$scratch/exec/$name"
  done
  (cd "$repo" && GROFE_COMPAT_TMUX="$scratch/exec/tmux" GROFE_COMPAT_VIA_EXEC=1 \
    GROFE_COMPAT_SCENARIOS="${scenarios[*]:-}" GROFE_COMPAT_OUT="$scratch/out" \
    cargo test --locked --quiet --test compat -- \
      --ignored --exact record::record_goldens --nocapture >&2)
fi

if [ "$want_scenarios" = 1 ] && [ "$exec_mode" = 0 ]; then
  run=(podman run --rm --platform linux/amd64
    -v "$repo:/src:ro"
    -v grofe-compat-golden-target-v2:/target
    -v grofe-compat-golden-cargo-v2:/home/golden/cargo)

  "${run[@]}" "$image" cargo fetch --locked >&2

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
      tar -C /tmp/out -cf - .' | tar -C "$scratch/out" -xf -
fi

after=$(socket_dirs)
if [ "$before" != "$after" ]; then
  echo "error: the host's tmux or grofe socket directories changed during recording" >&2
  diff <(echo "$before") <(echo "$after") >&2 || true
  exit 1
fi

golden=$repo/tests/fixtures/compat/golden
if [ "$check" = 0 ]; then
  cp -R "$scratch/out/." "$golden/"
  exit 0
fi
status=0
while IFS= read -r -d '' f; do
  rel=${f#"$scratch/out"/}
  if ! diff -u "$golden/$rel" "$f"; then
    status=1
  fi
done < <(find "$scratch/out" -type f -print0)
[ "$status" = 0 ] && echo "goldens reproduce byte for byte"
exit "$status"
