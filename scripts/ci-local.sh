#!/usr/bin/env bash
# Run CI locally: the workflow's Linux steps in a plain Ubuntu container, then
# the macOS leg natively. Stand-in while GitHub Actions is unavailable.
# Usage: scripts/ci-local.sh [linux|macos|e2e]   (default: linux and macos)
#
# The Linux leg runs scripts/ci-linux.sh, the script ci.yml's Linux jobs
# run, in a fresh ubuntu:26.04 container (ubuntu-latest's release, pinned by
# digest) as `runner`, a non-root user with passwordless sudo, as on a
# hosted runner. grofe goes in as a `git archive` of HEAD (or of
# GROFE_CI_REV, the commit the release gate tests), never the working tree. The unit steps run, then the acceptance steps, both always, so one
# red half cannot hide the other. The container is removed at the end.
#
# The e2e leg (T4.6) runs the container scenarios through scripts/e2e.sh:
# today `reboot`, on the Debian image (and Arch with E2E_ARCH=1, never
# gating). It honours e2e.sh's CONTAINER, E2E_PODMAN_NETWORK and
# E2E_PODMAN_OPTS. It is a leg of its own, not part of the default run.
#
# The compat suite (TC.13) runs in both legs when the caller sets
# GROFE_COMPAT_CANOPY and GROFE_COMPAT_ATHOME to canopy and athome checkouts
# at the commits in tests/fixtures/compat/golden/tc_13/PINS. With
# GROFE_COMPAT_REQUIRED=1 a missing checkout fails the run instead of
# skipping the suite. The macOS leg inherits the three; the Linux leg gets
# them with both checkouts mounted read-only.
#
# The Linux leg also runs the plugin cases that need a private /tmp, with
# the release binaries ci-linux.sh provisions for the container's platform
# from the GROFE_COMPAT_CANOPY checkout mounted below. Host binaries are not
# passed in: on macOS they are the wrong platform for the container.
set -euo pipefail
cd "$(dirname "$0")/.."

# The container CLI: docker where it is installed, else podman.
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

LINUX_IMAGE=docker.io/library/ubuntu:26.04@sha256:157dc3d6e4a3e4f2c68d12df9df9ed6e808b59af6d4c3757b432b4f519916725

linux() {
  # Every container this run starts carries this label, so the exit trap
  # removes exactly those, whether the run passed or failed.
  local label name
  label="grofe-ci-local=$$-$(date +%s)"
  name="grofe-ci-linux-$$"
  # shellcheck disable=SC2064 # the label is fixed now; the trap outlives it
  trap "remove_labelled '$label'" EXIT
  # CI, as act and the hosted runner set it: the suites' require_tool,
  # private_tmp, plugin-binary and shellcheck checks fail rather than skip.
  local opts=(--label "$label") envs=(-e CI=true) var dir sub common mounted=0
  if [ -n "${E2E_PODMAN_NETWORK:-}" ]; then
    opts+=("--network=$E2E_PODMAN_NETWORK")
  fi
  if [ -n "${GROFE_COMPAT_REQUIRED:-}" ]; then
    envs+=(-e "GROFE_COMPAT_REQUIRED=$GROFE_COMPAT_REQUIRED")
  fi
  for var in GROFE_COMPAT_CANOPY GROFE_COMPAT_ATHOME; do
    dir="${!var:-}"
    [ -n "$dir" ] || continue
    dir="$(cd "$dir" && pwd)"
    sub="$(echo "${var#GROFE_COMPAT_}" | tr '[:upper:]' '[:lower:]')"
    opts+=(-v "$dir:/compat/$sub:ro")
    # A worktree's .git names its repository's git dir; mounted at the same
    # path, the suite can read the checkout's HEAD for the PINS check.
    common="$(git -C "$dir" rev-parse --path-format=absolute --git-common-dir)"
    if [ "$common" != "$dir/.git" ]; then
      opts+=(-v "$common:$common:ro")
    fi
    envs+=(-e "$var=/compat/$sub")
    mounted=1
  done
  if [ "$mounted" -eq 1 ]; then
    # The mounts belong to another user inside the container.
    envs+=(-e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e 'GIT_CONFIG_VALUE_0=*')
  fi

  engine run -d --name "$name" "${opts[@]}" "$LINUX_IMAGE" sleep infinity >/dev/null
  echo "### the runner user, as on a hosted runner"
  engine exec "$name" bash -c 'set -e
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq sudo curl git ca-certificates build-essential pkg-config procps lsof >/dev/null
    useradd -m -u 1001 -s /bin/bash runner
    printf "runner ALL=(ALL) NOPASSWD:ALL\n" >/etc/sudoers.d/runner
    chmod 440 /etc/sudoers.d/runner
    install -d -o runner -g runner /home/runner/grofe'
  git archive "${GROFE_CI_REV:-HEAD}" | engine exec -i -u runner "$name" tar -x -C /home/runner/grofe
  local as_runner=(engine exec -u runner -w /home/runner/grofe -e HOME=/home/runner "${envs[@]}" "$name")
  echo "### ci-linux.sh toolchain"
  "${as_runner[@]}" scripts/ci-linux.sh toolchain
  local rc=0 path="/home/runner/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
  echo "### ci-linux.sh unit"
  "${as_runner[@]}" env PATH="$path" scripts/ci-linux.sh unit || rc=1
  echo "### ci-linux.sh acceptance"
  "${as_runner[@]}" env PATH="$path" scripts/ci-linux.sh acceptance || rc=1
  return "$rc"
}

# scripts/ci-macos.sh, ci.yml's macOS steps: the unit half, then the
# acceptance half, both always, so one red half cannot hide the other.
macos() {
  local rc=0
  scripts/ci-macos.sh unit || rc=1
  scripts/ci-macos.sh acceptance || rc=1
  return "$rc"
}

e2e() {
  scripts/e2e.sh reboot
}

case "${1:-both}" in
  linux) linux ;;
  macos) macos ;;
  e2e) e2e ;;
  # Not `macos && linux`: bash ignores set -e inside a function on the
  # left of &&, so a failing cargo test there would not stop the run.
  both) macos; linux ;;
  *) echo "usage: $0 [linux|macos|e2e]" >&2; exit 2 ;;
esac
