#!/usr/bin/env bash
# ci-macos.sh: CI's macOS steps, in one place. .github/workflows/ci.yml's
# macOS jobs run them on macos-latest, and scripts/ci-local.sh macos (the
# release gate's macos leg) runs them natively.
#
#   scripts/ci-macos.sh unit         fmt, clippy, the lib, bin and doc tests
#   scripts/ci-macos.sh acceptance   the plugin release binaries, every
#                                    integration test binary (no fail-fast),
#                                    and the bats suite
#
# The tools (neovim, fzf, htop, tmux, bats, shellcheck) are installed by
# ci.yml's Homebrew step, or are the developer's own.
#
# Under CI (a hosted runner, a VM of the job's own) /tmp is the job's, so
# the plugin cases that write fixed /tmp paths run (GROFE_TEST_PRIVATE_TMP),
# and a missing tool fails its test. On a developer's Mac neither holds:
# /tmp is shared with whatever else runs there, so those cases skip and say
# so, as do the tests whose tool is missing. Only a hosted run covers them.
#
# Environment:
#   CI                   set by the hosted runner
#   RUNNER_TEMP          scratch space (GitHub sets it; otherwise a temp dir)
#   GROFE_COMPAT_CANOPY  a canopy checkout for the plugin binaries; without
#                        it canopy is cloned at the commit tc_13's PINS names
set -euo pipefail
cd "$(dirname "$0")/.."

tmp="${RUNNER_TEMP:-}"
if [ -z "$tmp" ]; then
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/ci-macos.XXXXXX")"
fi

unit() {
  # rust-toolchain.toml's channel, with the components the checks need.
  if command -v rustup >/dev/null; then
    rustup component add rustfmt clippy
  fi
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test --lib --bins
  cargo test --doc
}

acceptance() {
  local t
  for t in nvim fzf htop tmux bats less shellcheck; do
    command -v "$t" >/dev/null || echo "ci-macos.sh: $t is not on PATH; its tests skip (fail under CI)" >&2
  done

  if [ -n "${CI:-}" ]; then
    export GROFE_TEST_PRIVATE_TMP=1
  fi
  local canopy="${GROFE_COMPAT_CANOPY:-}" sha line provisioned
  if [ -z "$canopy" ]; then
    canopy="$tmp/canopy"
    sha="$(awk '$1 == "canopy" { print $2 }' tests/fixtures/compat/golden/tc_13/PINS)"
    git clone --quiet https://github.com/morrieinmaas/canopy "$canopy"
    git -C "$canopy" checkout --quiet "$sha"
  fi
  # Captured first, so a failed provisioning stops the run.
  provisioned="$(scripts/provision-plugin-binaries.sh "$canopy" "$tmp/plugin-bin")"
  while IFS= read -r line; do
    [ -n "$line" ] && export "${line?}"
  done <<<"$provisioned"

  # Built first: self_test otherwise runs `cargo build --example spike` in
  # the middle of the suite, and that compile starves the timing cases
  # (m2_robust's resize storm).
  cargo build --example spike
  cargo test --test '*' --no-fail-fast

  cargo build
  bats tests/cli.bats
}

case "${1:-}" in
  unit) unit ;;
  acceptance) acceptance ;;
  *) echo "usage: $0 unit|acceptance" >&2; exit 2 ;;
esac
