#!/usr/bin/env bash
# Run CI locally: the real workflow's Linux jobs in a container via act,
# then the macOS leg natively. Stand-in while GitHub Actions is unavailable.
# Usage: scripts/ci-local.sh [linux|macos]   (default: both)
#
# The compat suite (TC.13) runs in both legs when the caller sets
# GROFE_COMPAT_CANOPY and GROFE_COMPAT_ATHOME to canopy and athome checkouts
# at the commits in tests/fixtures/compat/golden/tc_13/PINS. With
# GROFE_COMPAT_REQUIRED=1 a missing checkout fails the run instead of
# skipping the suite. The macOS leg inherits the three; the Linux leg gets
# them through act, with both checkouts mounted read-only.
#
# The Linux leg also runs the plugin cases that need a private /tmp (the
# workflow sets GROFE_TEST_PRIVATE_TMP=1 there) with their release
# binaries, GROFE_TEST_OPENSESSIONS and GROFE_TEST_XRE, which the job
# provisions for its own platform (scripts/provision-plugin-binaries.sh)
# from the GROFE_COMPAT_CANOPY checkout mounted below. Host binaries are not
# passed in: on macOS they are the wrong platform for the container.
set -euo pipefail
cd "$(dirname "$0")/.."

# The container CLI act's daemon answers to (docker honours DOCKER_HOST).
engine() {
  if command -v docker >/dev/null; then
    docker "$@"
  else
    podman "$@"
  fi
}

# Removes the containers carrying label `$1`: the ones this run started.
remove_labelled() {
  local ids
  ids="$(engine ps -aq --filter "label=$1" 2>/dev/null || true)"
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086 # one id per word
    engine rm -f $ids >/dev/null 2>&1 || true
  fi
}

linux() {
  # A DOCKER_HOST pointing at a socket that does not exist is as good as unset.
  if [ -n "${DOCKER_HOST:-}" ] && [ ! -S "${DOCKER_HOST#unix://}" ]; then
    unset DOCKER_HOST
  fi
  if [ -z "${DOCKER_HOST:-}" ] && command -v podman >/dev/null; then
    DOCKER_HOST="unix://$(podman machine inspect --format '{{.ConnectionInfo.PodmanSocket.Path}}')"
    export DOCKER_HOST
  fi

  # Every job container this run starts carries this label, so the exit
  # trap removes exactly those, whether the job passed or failed (act keeps
  # a failed job's container running), and nothing else.
  local label
  label="grofe-ci-local=$$-$(date +%s)"
  # shellcheck disable=SC2064 # the label is fixed now; the trap outlives it
  trap "remove_labelled '$label'" EXIT
  local opts="--label $label" args=() var dir name common mounted=0
  if [ -n "${GROFE_COMPAT_REQUIRED:-}" ]; then
    args+=(--env "GROFE_COMPAT_REQUIRED=$GROFE_COMPAT_REQUIRED")
  fi
  for var in GROFE_COMPAT_CANOPY GROFE_COMPAT_ATHOME; do
    dir="${!var:-}"
    [ -n "$dir" ] || continue
    dir="$(cd "$dir" && pwd)"
    case "$dir" in
      *[[:space:]]*) echo "ci-local: $var has whitespace in it: $dir" >&2; exit 2 ;;
    esac
    name="$(echo "${var#GROFE_COMPAT_}" | tr '[:upper:]' '[:lower:]')"
    opts+=" -v $dir:/compat/$name:ro"
    # A worktree's .git names its repository's git dir; mounted at the same
    # path, the suite can read the checkout's HEAD for the PINS check.
    common="$(git -C "$dir" rev-parse --path-format=absolute --git-common-dir)"
    if [ "$common" != "$dir/.git" ]; then
      opts+=" -v $common:$common:ro"
    fi
    args+=(--env "$var=/compat/$name")
    mounted=1
  done
  if [ "$mounted" -eq 1 ]; then
    # The mounts belong to another user inside the container.
    args+=(--env GIT_CONFIG_COUNT=1 --env GIT_CONFIG_KEY_0=safe.directory
      --env 'GIT_CONFIG_VALUE_0=*')
  fi

  act workflow_dispatch --matrix os:ubuntu-latest \
    -P ubuntu-latest=catthehacker/ubuntu:act-24.04 --container-daemon-socket - \
    --container-options "$opts" ${args[@]+"${args[@]}"}
}

macos() {
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test
  cargo build
  bats tests/cli.bats
}

case "${1:-both}" in
  linux) linux ;;
  macos) macos ;;
  both) macos && linux ;;
  *) echo "usage: $0 [linux|macos]" >&2; exit 2 ;;
esac
