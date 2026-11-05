#!/usr/bin/env bash
# e2e.sh: the container harness every e2e scenario runs through (plan T4.6,
# "Rules for M4 onward").
#
#   scripts/e2e.sh <scenario> [image...] [--keep-image]
#
# <scenario> is tests/e2e/scenarios/<scenario>.sh; [image...] names
# tests/e2e/Containerfile.<image>, default `debian`, plus `arch` when
# E2E_ARCH=1. Arch is reported and never gates: only the other images
# decide the exit status. `alias-day` is TA.1's runner,
# scripts/e2e-alias-day.sh, which has its own inputs and report and takes
# its host-state check and podman options from here.
#
# For each image: grofe at HEAD goes into a build context as a `git
# archive` export (never the working tree), the image is built from it,
# and each run the scenario declares (header lines `# e2e-run: <name>
# <step>...`) gets a fresh container. A step is a phase, run as the test
# user with `podman exec ... bash /opt/e2e/scenarios/<scenario>.sh <step>`,
# or `restart`, which is `podman restart`: every process is killed, the
# /run/user tmpfs comes back empty, the container's disk stays. Before each
# phase /run/user/<uid> is created as logind would. A run whose name ends in
# `!` is a mutation run: it passes only when its last phase fails and the
# uids on its `FAIL` lines are exactly those on its `MUTATED` lines. A
# header line `# e2e-target: <stage>` builds that stage of the
# Containerfile (`--target`) instead of its last one.
#
# Nothing of the host's is mounted into a container: no home, no sockets,
# no runtime dir. Around the whole thing the host-state check (host_state
# below) must find the owner's state unchanged, or the run fails.
#
# Environment:
#   CONTAINER            the engine, default podman (rootless)
#   E2E_PODMAN_NETWORK   a network for `build` and `run` (--network=...),
#                        default the engine's own; `host` where the default
#                        (pasta) cannot open /dev/net/tun. Sockets and
#                        runtime dirs stay the container's either way: grofe
#                        uses path sockets, and host networking shares only
#                        the network namespace.
#   E2E_PODMAN_OPTS      global engine options, word-split, put before every
#                        subcommand, e.g. `--storage-opt
#                        overlay.mount_program=/usr/bin/fuse-overlayfs` on a
#                        kernel without overlayfs
#   E2E_ARCH=1           also run on Arch
#
# Output: target/e2e/<run-id>/<scenario>/<image>/{build,<run>}.log, and one
# summary line per image and run. Exits 0 only when every gating run passed
# and the host-state check found no difference.
set -euo pipefail

# --- shared with scripts/e2e-alias-day.sh ------------------------------------

# podman with the global options from E2E_PODMAN_OPTS, as a command line
# (for timeout and xargs) and as a function.
# shellcheck disable=SC2206 # E2E_PODMAN_OPTS is a list of words
engine_argv=("${CONTAINER:-podman}" ${E2E_PODMAN_OPTS:-})
engine() { "${engine_argv[@]}" "$@"; }

# The --network option for build and run, when E2E_PODMAN_NETWORK is set.
net_args=()
if [ -n "${E2E_PODMAN_NETWORK:-}" ]; then
  net_args=("--network=$E2E_PODMAN_NETWORK")
fi

# The host-state check of "Rules for M4 onward", printed so that two runs
# compare with cmp. The socket directories (tmux's, grofe's) hold the
# owner's live servers, so they give each entry's name and kind only: a
# live server rewrites its socket's metadata and never its name. The state
# and config directories give names, kinds and, for regular files, sizes
# and sha256s: byte-identical. canopy's store is a git checkout: its tree
# is hashed without .git, and HEAD and `git status` stand for .git.
hash_file() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}
list_dir() {
  local dir="$1" mode="$2" p
  printf '== %s (%s)\n' "$dir" "$mode"
  [ -e "$dir" ] || { echo "(absent)"; return 0; }
  (cd "$dir" && find . -path ./.git -prune -o -print | LC_ALL=C sort | while IFS= read -r p; do
    if [ -L "$p" ]; then
      printf '%s link %s\n' "$p" "$(readlink "$p")"
    elif [ -S "$p" ]; then
      printf '%s socket\n' "$p"
    elif [ -d "$p" ]; then
      printf '%s dir\n' "$p"
    elif [ -f "$p" ] && [ "$mode" = names ]; then
      printf '%s file\n' "$p"
    elif [ -f "$p" ]; then
      printf '%s file %s %s\n' "$p" "$(wc -c <"$p" | tr -d ' ')" "$(hash_file "$p")"
    else
      printf '%s other\n' "$p"
    fi
  done)
}
host_state() {
  local uid store
  uid="$(id -u)"
  list_dir "${TMUX_TMPDIR:-/tmp}/tmux-$uid" names
  [ "${TMUX_TMPDIR:-/tmp}" = /tmp ] || list_dir "/tmp/tmux-$uid" names
  list_dir "${XDG_RUNTIME_DIR:-/nonexistent}/grofe" names
  list_dir "${TMPDIR:-/tmp}/grofe-$uid" names
  list_dir "${XDG_STATE_HOME:-$HOME/.local/state}/grofe" contents
  list_dir "${XDG_CONFIG_HOME:-$HOME/.config}/canopy" contents
  store="${XDG_DATA_HOME:-$HOME/.local/share}/canopy"
  list_dir "$store" contents
  if [ -d "$store/.git" ]; then
    printf '== canopy store git\n%s\n' "$(git -C "$store" rev-parse HEAD 2>&1)"
    git -C "$store" status --porcelain 2>&1
  fi
}

# --- the harness -------------------------------------------------------------

log() { printf '[e2e] %s\n' "$*" >&2; }

# run_steps <container> <scenario> <log> <step>...: each phase as the test
# user, `restart` as podman restart; stops at the first failing phase and
# returns its status.
run_steps() {
  local name="$1" scenario="$2" out="$3" step rc
  shift 3
  for step in "$@"; do
    if [ "$step" = restart ]; then
      printf '### restart\n' >>"$out"
      engine restart -t 5 "$name" >>"$out" 2>&1 || return 1
      continue
    fi
    # What logind does at login: the user's runtime dir, on the tmpfs.
    engine exec -u 0 "$name" install -d -o test -g test -m 0700 /run/user/1000 >>"$out" 2>&1 || return 1
    printf '### phase %s\n' "$step" >>"$out"
    rc=0
    engine exec -u test -w /home/test -e HOME=/home/test -e USER=test -e LOGNAME=test \
      "$name" bash "/opt/e2e/scenarios/$scenario.sh" "$step" >>"$out" 2>&1 || rc=$?
    printf '### phase %s exited %s\n' "$step" "$rc" >>"$out"
    [ "$rc" -eq 0 ] || return "$rc"
  done
}

# mutation_caught <log>: the last phase's FAIL uids are exactly the MUTATED
# ones, and something else passed.
mutation_caught() {
  local mutated failed
  mutated="$(sed -n 's/^MUTATED \([^ ]*\).*/\1/p' "$1" | LC_ALL=C sort -u)"
  failed="$(awk '/^### phase [^ ]*$/ { buf = "" } /^FAIL / { buf = buf $2 "\n" } END { printf "%s", buf }' "$1" |
    LC_ALL=C sort -u)"
  [ -n "$mutated" ] && [ "$mutated" = "$failed" ] && grep -q '^PASS ' "$1"
}

# run_image <scenario> <image> <dir>: builds the image and makes every run
# the scenario declares; 0 when all of them came out as declared.
run_image() {
  local scenario="$1" image="$2" dir="$3" tag runs run_name steps name status=0 rc
  local ctx="$work/ctx"
  tag="localhost/grofe-e2e-$image:$(printf '%s' "$run_id" | tr '[:upper:]' '[:lower:]')"
  [ -f "$ctx/grofe/tests/e2e/Containerfile.$image" ] || { log "$image: no tests/e2e/Containerfile.$image at HEAD"; return 1; }
  mkdir -p "$dir"
  # Arch's base is amd64 only: on an arm64 host the build (and so every run
  # of the image) is emulated.
  local plat_args=()
  [ "$image" = arch ] && plat_args=(--platform linux/amd64)
  local target target_args=()
  target="$(sed -n 's/^# e2e-target: *//p' "$ctx/grofe/tests/e2e/scenarios/$scenario.sh" | head -n 1)"
  [ -n "$target" ] && target_args=(--target "$target")
  log "$image: building $tag"
  if ! engine build ${plat_args[@]+"${plat_args[@]}"} ${target_args[@]+"${target_args[@]}"} ${net_args[@]+"${net_args[@]}"} -f "$ctx/grofe/tests/e2e/Containerfile.$image" \
    -t "$tag" "$ctx" >"$dir/build.log" 2>&1; then
    log "$image: the image did not build: $dir/build.log"
    printf '%s\tbuild\tFAIL\n' "$image" >>"$summary"
    return 1
  fi
  images+=("$tag")
  runs="$(sed -n 's/^# e2e-run: //p' "$ctx/grofe/tests/e2e/scenarios/$scenario.sh")"
  [ -n "$runs" ] || { log "$scenario declares no '# e2e-run:' line"; return 1; }
  while read -r run_name steps; do
    name="grofe-e2e-$image-${run_name%!}-$$"
    containers+=("$name")
    # shellcheck disable=SC2086 # steps is a list of words
    if ! engine run -d --init --name "$name" --hostname e2e ${net_args[@]+"${net_args[@]}"} \
      --tmpfs /run/user:rw,mode=0755 "$tag" >"$dir/${run_name%!}.log" 2>&1; then
      rc=1
    else
      rc=0
      # shellcheck disable=SC2086 # steps is a list of words
      run_steps "$name" "$scenario" "$dir/${run_name%!}.log" $steps || rc=$?
    fi
    engine rm -f -t 0 "$name" >/dev/null 2>&1 || true
    local verdict
    case "$run_name" in
      *!)
        if [ "$rc" -ne 0 ] && mutation_caught "$dir/${run_name%!}.log"; then
          verdict="PASS (the mutation failed on the mutated pane only)"
        else
          verdict="FAIL (the mutation was not caught on the mutated pane alone)"
        fi
        ;;
      *) if [ "$rc" -eq 0 ]; then verdict=PASS; else verdict="FAIL (exit $rc)"; fi ;;
    esac
    case "$verdict" in FAIL*) status=1 ;; esac
    printf '%s\t%s\t%s\n' "$image" "${run_name%!}" "$verdict" >>"$summary"
    log "$image: run ${run_name%!}: $verdict ($dir/${run_name%!}.log)"
  done <<<"$runs"
  return "$status"
}

main() {
  local scenario="" arg gating=()
  # Globals: the exit trap reads them after main returns.
  keep_image=0
  local repo
  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  wanted=()
  for arg in "$@"; do
    case "$arg" in
      --keep-image) keep_image=1 ;;
      -*) echo "usage: $0 <scenario> [image...] [--keep-image]" >&2; exit 2 ;;
      *) if [ -z "$scenario" ]; then scenario="$arg"; else wanted+=("$arg"); fi ;;
    esac
  done
  [ -n "$scenario" ] || { echo "usage: $0 <scenario> [image...] [--keep-image]" >&2; exit 2; }
  if [ "$scenario" = alias-day ]; then
    local pass_on=()
    [ "$keep_image" -eq 1 ] && pass_on=(--keep-image)
    exec "$repo/scripts/e2e-alias-day.sh" ${pass_on[@]+"${pass_on[@]}"}
  fi
  [ -f "$repo/tests/e2e/scenarios/$scenario.sh" ] || { echo "e2e: no scenario tests/e2e/scenarios/$scenario.sh" >&2; exit 2; }
  command -v "${CONTAINER:-podman}" >/dev/null || { echo "e2e: ${CONTAINER:-podman} not found" >&2; exit 2; }
  if [ "${#wanted[@]}" -eq 0 ]; then
    wanted=(debian)
    [ "${E2E_ARCH:-0}" = 1 ] && wanted+=(arch)
  fi

  local sha
  sha="$(git -C "$repo" rev-parse HEAD)"
  run_id="$(date -u +%Y%m%dT%H%M%SZ)-${sha:0:7}"
  out="$repo/target/e2e/$run_id/$scenario"
  mkdir -p "$out"
  summary="$out/summary.tsv"
  : >"$summary"
  work="$(mktemp -d "${TMPDIR:-/tmp}/grofe-e2e.XXXXXX")"
  images=()
  containers=()
  cleanup() {
    local c i
    for c in ${containers[@]+"${containers[@]}"}; do engine rm -f -t 0 "$c" >/dev/null 2>&1 || true; done
    if [ "$keep_image" -eq 0 ]; then
      for i in ${images[@]+"${images[@]}"}; do engine rmi -f "$i" >/dev/null 2>&1 || true; done
      # The build stage and every layer these builds made or reused, as
      # their logs name them, so an image another process built is never
      # touched. The bases, pulled by digest, are shared and stay.
      cat "$out"/*/build.log 2>/dev/null | grep -oE '^(--> |Using cache )[0-9a-f]{12,}' | awk '{ print $NF }' |
        sort -u | xargs -r "${engine_argv[@]}" rmi -f >/dev/null 2>&1 || true
    fi
    rm -rf "$work"
  }
  trap cleanup EXIT

  host_state >"$work/host.before"
  mkdir -p "$work/ctx/grofe"
  git -C "$repo" archive HEAD | tar -x -C "$work/ctx/grofe"
  log "$scenario at $sha, images: ${wanted[*]}, out: $out"

  local image status=0
  for image in "${wanted[@]}"; do
    if run_image "$scenario" "$image" "$out/$image"; then
      :
    elif [ "$image" = arch ]; then
      log "arch: failed, reported and not gating"
    else
      status=1
      gating+=("$image")
    fi
  done

  # The containers are gone before the second look, so it sees none of ours.
  for c in ${containers[@]+"${containers[@]}"}; do engine rm -f -t 0 "$c" >/dev/null 2>&1 || true; done
  host_state >"$work/host.after"
  if cmp -s "$work/host.before" "$work/host.after"; then
    printf 'host\tstate\tPASS (unchanged)\n' >>"$summary"
  else
    diff "$work/host.before" "$work/host.after" >"$out/host-state.diff" || true
    printf 'host\tstate\tFAIL (see host-state.diff)\n' >>"$summary"
    log "the host-state check found a difference: $out/host-state.diff"
    status=1
  fi
  column -t -s $'\t' "$summary" >&2 2>/dev/null || cat "$summary" >&2
  return "$status"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
