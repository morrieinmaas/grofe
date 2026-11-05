#!/usr/bin/env bash
# acceptance.sh <phase>: the in-container half of TE.1, the spec 8
# acceptance suite in a clean image. Run by scripts/e2e.sh as the test
# user, one phase per call, never on a host, in the Containerfile's
# `acceptance` stage (the toolchain, grofe's source at HEAD in ~/grofe with
# the suite prebuilt, the spec 8 tools and real tmux 3.7b):
#
# e2e-target: acceptance
# e2e-run: cargo tools cargo
# e2e-run: bats tools bats
#
#   tools   every tool the suite drives is on PATH (a missing one is a FAIL
#           naming it, never a skip); `tmux` on the suite's PATH is real
#           tmux 3.7b and not grofe, and /usr/bin/tmux is grofe answering
#           as tmux 3.7b
#   cargo   `cargo test --test acceptance`; a test that skipped for want of
#           a spec 8 tool is a FAIL naming the tool; a failed case gets one
#           serial rerun, and passing there is reported as FLAKY
#   bats    `bats tests/cli.bats` against the debug binary
#
# Each check prints `PASS <what>` or `FAIL <what>`; a phase exits nonzero
# on any FAIL.
set -uo pipefail

[ -e /run/.containerenv ] || { echo "acceptance.sh: refusing to run outside a container" >&2; exit 2; }
[ "$(id -un)" = test ] || { echo "acceptance.sh: run as the test user" >&2; exit 2; }

XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR
SRC="$HOME/grofe"
GROFE=/usr/local/lib/grofe/grofe
# Real tmux, first on the suite's PATH, so the nested-tmux item runs it
# (`command -v tmux` in a pane, `env -u TMUX`); /usr/bin/tmux is grofe.
REAL_TMUX_DIR=/usr/local/lib/tmux-3.7b/bin
export PATH="$REAL_TMUX_DIR:$PATH"
# The container's /tmp is its own: the plugin cases writing fixed /tmp
# paths may run.
export GROFE_TEST_PRIVATE_TMP=1
# The tools spec 8 names (kitten is packaged on both images, so required),
# and the ones the suite's cases also drive.
TOOLS=(nvim fzf less htop git kitten tmux bats cargo bash python3 lsof ps pgrep)
fails=0

pass() { printf 'PASS %s\n' "$*"; }
fail() {
  printf 'FAIL %s\n' "$*"
  fails=$((fails + 1))
}

tools() {
  local t p real
  for t in "${TOOLS[@]}"; do
    if p="$(command -v "$t")"; then
      pass "tool $t: $p"
    else
      fail "tool $t: not found on PATH ($PATH)"
    fi
  done

  # Real tmux: the binary on PATH is the 3.7b build, a different file from
  # grofe, and links libevent, which grofe never does.
  real="$(command -v tmux)"
  if [ "$real" != "$REAL_TMUX_DIR/tmux" ]; then
    fail "real tmux: PATH resolves tmux to '$real', not $REAL_TMUX_DIR/tmux"
  elif [ "$real" -ef "$GROFE" ]; then
    fail "real tmux: $real is grofe"
  elif ! ldd "$real" 2>/dev/null | grep -q libevent; then
    fail "real tmux: $real does not link libevent, so it is not tmux"
  elif [ "$(env -u TMUX "$real" -V)" != "tmux 3.7b" ]; then
    fail "real tmux: $real -V prints '$(env -u TMUX "$real" -V 2>&1)', not 'tmux 3.7b'"
  else
    pass "real tmux: $real -V prints tmux 3.7b"
  fi

  # grofe as tmux: the symlink the other images have, under tmux identity.
  if ! [ /usr/bin/tmux -ef "$GROFE" ]; then
    fail "grofe as tmux: /usr/bin/tmux is not $GROFE"
  elif [ "$(env -u TMUX /usr/bin/tmux -V)" != "tmux 3.7b" ]; then
    fail "grofe as tmux: /usr/bin/tmux -V prints '$(env -u TMUX /usr/bin/tmux -V 2>&1)', not 'tmux 3.7b'"
  else
    pass "grofe as tmux: /usr/bin/tmux -V prints tmux 3.7b"
  fi
}

# failed_tests <log>: the names libtest lists under its last `failures:`.
failed_tests() {
  awk '/^failures:$/ { n = 0; delete t; next } /^    [A-Za-z0-9_:]+$/ { t[n++] = $1 }
    END { for (i = 0; i < n; i++) print t[i] }' "$1"
}

cargo_suite() {
  local log="$HOME/cargo-test.log" relog="$HOME/cargo-retry.log" rc=0 t failed=()
  cd "$SRC" || { fail "no source at $SRC"; return; }
  # --show-output prints what passing cases wrote too, so a skip shows.
  cargo test --locked --offline --test acceptance -- --show-output 2>&1 | tee "$log" || rc=$?
  grep -E '^test result:' "$log" | sed 's/^/SUMMARY /'
  for t in "${TOOLS[@]}"; do
    if grep -q "skipping: $t not found" "$log"; then
      fail "cargo: a case skipped because $t is not found"
    fi
  done
  # Skips for anything else (the plugin cases' provisioned release
  # binaries, which TE.1 does not install) are listed, not failed.
  grep -iE '^skipping' "$log" | sort | uniq -c | sed 's/^ */SKIP /'
  if [ "$rc" -eq 0 ]; then
    pass "cargo test --test acceptance"
    return
  fi
  mapfile -t failed < <(failed_tests "$log")
  if [ "${#failed[@]}" -eq 0 ]; then
    fail "cargo test --test acceptance exited $rc with no failed case listed"
    return
  fi
  # The timing cases are bounded in wall time, and a loaded host (or the
  # suite's own parallel cases) can push one past its bound. Each failed
  # case gets one rerun, alone and serially: one that passes there is
  # printed as FLAKY, one that fails again fails the phase.
  for t in "${failed[@]}"; do printf 'FAILED-TEST %s\n' "$t"; done
  rc=0
  cargo test --locked --offline --test acceptance -- --exact --test-threads=1 "${failed[@]}" 2>&1 |
    tee "$relog" || rc=$?
  grep -E '^test result:' "$relog" | sed 's/^/RETRY-SUMMARY /'
  if [ "$rc" -eq 0 ]; then
    for t in "${failed[@]}"; do printf 'FLAKY %s (failed in the full run, passed alone)\n' "$t"; done
    pass "cargo test --test acceptance (${#failed[@]} case(s) passed on the serial rerun)"
  else
    for t in $(failed_tests "$relog"); do fail "cargo: $t failed in the full run and again alone"; done
  fi
}

bats_suite() {
  local rc=0
  cd "$SRC" || { fail "no source at $SRC"; return; }
  [ -x target/debug/grofe ] || { fail "no target/debug/grofe"; return; }
  bats tests/cli.bats || rc=$?
  if [ "$rc" -eq 0 ]; then
    pass "bats tests/cli.bats"
  else
    fail "bats tests/cli.bats exited $rc"
  fi
}

case "${1:-}" in
  tools) tools ;;
  cargo) cargo_suite ;;
  bats) bats_suite ;;
  *) echo "usage: acceptance.sh tools|cargo|bats" >&2; exit 2 ;;
esac
[ "$fails" -eq 0 ]
