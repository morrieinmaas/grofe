#!/usr/bin/env bash
# ci-linux.sh: CI's Linux steps, in one place. .github/workflows/ci.yml runs
# them on ubuntu-latest, and scripts/ci-local.sh linux (the release gate's
# linux leg) runs them in a plain Ubuntu container, as a non-root user with
# sudo, the way the hosted runner's user is set up. One script, so the two
# cannot drift apart.
#
#   scripts/ci-linux.sh toolchain    Rust at RUST_TOOLCHAIN through rustup,
#                                    when cargo is not already that version
#   scripts/ci-linux.sh unit         fmt, clippy, the lib, bin and doc tests
#   scripts/ci-linux.sh acceptance   the tools, fzf at FZF_VERSION, the plugin
#                                    release binaries, every integration test
#                                    binary (no fail-fast), and the bats suite
#
# The macOS steps stay in ci.yml.
#
# Environment:
#   RUNNER_TEMP          scratch space (GitHub sets it; otherwise a temp dir)
#   GITHUB_PATH          where a step's PATH additions go, when under Actions
#   GROFE_COMPAT_CANOPY  a canopy checkout for the plugin binaries; without
#                        it canopy is cloned at the commit tc_13's PINS names
set -euo pipefail
cd "$(dirname "$0")/.."

RUST_TOOLCHAIN=1.98.1
FZF_VERSION=0.74.4
FZF_SHA256=05e6813a337cc722c3ed07e54a764b75cc5d671e2e60459db0ba696ee5fa7504
# The tools the suites run; under CI each one missing fails its test
# (require_tool) instead of skipping it.
TOOLS=(neovim htop tmux bats less shellcheck python3)

tmp="${RUNNER_TEMP:-}"
if [ -z "$tmp" ]; then
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/ci-linux.XXXXXX")"
fi

# add_path <dir>: on PATH for this script, and for later steps under Actions.
add_path() {
  export PATH="$1:$PATH"
  if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$1" >>"$GITHUB_PATH"
  fi
}

toolchain() {
  if command -v cargo >/dev/null && cargo --version | grep -q "^cargo $RUST_TOOLCHAIN "; then
    return 0
  fi
  if ! command -v rustup >/dev/null; then
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
      --default-toolchain "$RUST_TOOLCHAIN"
    add_path "$HOME/.cargo/bin"
  fi
  rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --component rustfmt,clippy
  rustup default "$RUST_TOOLCHAIN"
  cargo --version
}

unit() {
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test --lib --bins
  cargo test --doc
}

acceptance() {
  sudo apt-get update
  sudo apt-get install -y "${TOOLS[@]}"

  # The owner's fzf pin, as compat.yml. fe_4's `fzf-tmux` case waits for a
  # floating pane, and Ubuntu's older fzf opens a popup instead.
  mkdir -p "$tmp/fzf"
  curl -fsSLo "$tmp/fzf.tar.gz" \
    "https://github.com/junegunn/fzf/releases/download/v${FZF_VERSION}/fzf-${FZF_VERSION}-linux_amd64.tar.gz"
  echo "${FZF_SHA256}  $tmp/fzf.tar.gz" | sha256sum -c -
  tar -xzf "$tmp/fzf.tar.gz" -C "$tmp/fzf" fzf
  add_path "$tmp/fzf"
  fzf --version | grep -q "^${FZF_VERSION} "

  nvim --version | head -1
  fzf --version
  htop --version
  tmux -V
  bats --version
  less --version | head -1
  shellcheck --version | head -2

  # The machine is the job's own (a hosted runner VM, or the gate's
  # container), so /tmp is too: the plugin cases that write fixed /tmp
  # paths run (TP.8, TP.9 cases 2 and 3, TP.12 case 2), with the release
  # binaries canopy's checksum-verified provisioning fetches.
  export GROFE_TEST_PRIVATE_TMP=1
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
  # (m2_robust's resize storm) on the runner's few cores.
  cargo build --example spike
  cargo test --test '*' --no-fail-fast

  cargo build
  bats tests/cli.bats
}

case "${1:-}" in
  toolchain) toolchain ;;
  unit) unit ;;
  acceptance) acceptance ;;
  *) echo "usage: $0 toolchain|unit|acceptance" >&2; exit 2 ;;
esac
