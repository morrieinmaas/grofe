#!/usr/bin/env bash
# Run CI locally: the real workflow's Linux jobs in a container via act,
# then the macOS leg natively. Stand-in while GitHub Actions is unavailable.
# Usage: scripts/ci-local.sh [linux|macos]   (default: both)
set -euo pipefail
cd "$(dirname "$0")/.."

linux() {
  # A DOCKER_HOST pointing at a socket that does not exist is as good as unset.
  if [ -n "${DOCKER_HOST:-}" ] && [ ! -S "${DOCKER_HOST#unix://}" ]; then
    unset DOCKER_HOST
  fi
  if [ -z "${DOCKER_HOST:-}" ] && command -v podman >/dev/null; then
    DOCKER_HOST="unix://$(podman machine inspect --format '{{.ConnectionInfo.PodmanSocket.Path}}')"
    export DOCKER_HOST
  fi
  act workflow_dispatch --matrix os:ubuntu-latest \
    -P ubuntu-latest=catthehacker/ubuntu:act-24.04 --container-daemon-socket -
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
