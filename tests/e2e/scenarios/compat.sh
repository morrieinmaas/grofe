#!/usr/bin/env bash
# compat.sh <phase>: the in-container half of TE.2, grofe's compat suites
# in a clean image. Run by scripts/e2e.sh as the test user, one phase per
# call, never on a host, in the Containerfile's `compat` stage (the
# acceptance stage plus real tmux 3.7b and its sixel build on
# /usr/local/bin, canopy and athome at their pinned commits, the compat
# tests prebuilt):
#
# e2e-target: compat
# e2e-inputs: canopy athome
# e2e-hostname: grofe-golden
# e2e-run: suites tools tc13 copymode
# e2e-run: rerecord rerecord
#
#   tools      what the phases below rely on: real tmux 3.7b (and
#              tmux-sixel) first on the recorder's PATH and not grofe, fzf
#              at the owner's 0.74.4,
#              /usr/bin/tmux grofe answering as 3.7b, canopy's checkout at
#              its PINS commit with a clean tree, athome's tree and the
#              recording trees present
#   tc13       TC.13's suite (`scenarios::`, copy mode aside) with canopy
#              and athome required: its scenarios against their goldens,
#              the live fzf and Neovim checks, and the corpus replay, whose
#              compat-miss.log must hold exactly the documented
#              MISS_ALLOWED lines
#   copymode   TM.7's parity suite (`scenarios::copy_mode::`): every
#              keymap row's scenario byte for byte, and each one's
#              compat-miss.log as its USAGE_ERRORS entry says (empty for
#              all but two)
#   rerecord   TC.0b's recorder, record::record_goldens, re-records every
#              golden from real tmux 3.7b into a scratch directory, and
#              each must equal the committed one byte for byte
#
# The hostname is TC.0b's recording hostname (status lines pad around
# it), and the recorder runs under the user name `golden`, as in its own
# image, so its `user` normalization meets the same name: a user
# namespace maps the test user's uid onto golden's (uid 1001), so the
# test user's files are still its own.
#
# A failed suite case gets one serial rerun, as in acceptance.sh: passing
# there is reported as FLAKY and counted (`FLAKY-COUNT <n>`), and with
# E2E_STRICT=1 a FLAKY case fails the phase. A skipped scenario fails.
# Each check prints `PASS <what>` or `FAIL <what>`; a phase exits nonzero
# on any FAIL.
set -uo pipefail

[ -e /run/.containerenv ] || { echo "compat.sh: refusing to run outside a container" >&2; exit 2; }
[ "$(id -un)" = test ] || { echo "compat.sh: run as the test user" >&2; exit 2; }

XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR
SRC="$HOME/grofe"
GROFE=/usr/local/lib/grofe/grofe
GOLDEN="$SRC/tests/fixtures/compat/golden"
export GROFE_COMPAT_CANOPY="$HOME/compat/canopy"
export GROFE_COMPAT_ATHOME="$HOME/compat/athome"
export GROFE_COMPAT_REQUIRED=1
fails=0

pass() { printf 'PASS %s\n' "$*"; }
fail() {
  printf 'FAIL %s\n' "$*"
  fails=$((fails + 1))
}

pin() { awk -v n="$1" '$1 == n { print $2 }' "$GOLDEN/tc_13/PINS"; }

tools() {
  local t real head
  for t in /usr/local/bin/tmux /usr/local/bin/tmux-sixel; do
    real="$(readlink -f "$t")"
    if [ ! -x "$real" ]; then
      fail "real tmux: no $t"
    elif [ "$real" -ef "$GROFE" ]; then
      fail "real tmux: $t is grofe"
    elif [ "$(env -u TMUX "$t" -V)" != "tmux 3.7b" ]; then
      fail "real tmux: $t -V prints '$(env -u TMUX "$t" -V 2>&1)', not 'tmux 3.7b'"
    else
      pass "real tmux: $t ($real) -V prints tmux 3.7b"
    fi
  done
  if [ "$(env -i PATH=/usr/local/bin:/usr/bin:/bin sh -c 'command -v tmux')" = /usr/local/bin/tmux ]; then
    pass "the recorder's PATH finds real tmux first"
  else
    fail "the recorder's PATH does not find /usr/local/bin/tmux first"
  fi
  if [ "$(command -v fzf)" = /usr/local/bin/fzf ] && fzf --version | grep -q '^0\.74\.4 '; then
    pass "fzf: /usr/local/bin/fzf is 0.74.4, the owner's version"
  else
    fail "fzf: PATH finds '$(command -v fzf)', $(fzf --version 2>&1), not /usr/local/bin/fzf 0.74.4"
  fi
  if [ /usr/bin/tmux -ef "$GROFE" ] && [ "$(env -u TMUX /usr/bin/tmux -V)" = "tmux 3.7b" ]; then
    pass "grofe as tmux: /usr/bin/tmux is grofe and prints tmux 3.7b"
  else
    fail "grofe as tmux: /usr/bin/tmux is not grofe answering as tmux 3.7b"
  fi

  head="$(git -C "$GROFE_COMPAT_CANOPY" rev-parse HEAD 2>&1)"
  if [ "$head" != "$(pin canopy)" ]; then
    fail "canopy: $GROFE_COMPAT_CANOPY is at '$head', PINS says $(pin canopy)"
  elif [ -n "$(git -C "$GROFE_COMPAT_CANOPY" status --porcelain)" ]; then
    fail "canopy: $GROFE_COMPAT_CANOPY has changes"
  else
    pass "canopy: $GROFE_COMPAT_CANOPY at the PINS commit $head, clean"
  fi
  if [ -f "$GROFE_COMPAT_ATHOME/home/dot_config/grofe/config.toml" ] && [ ! -e "$GROFE_COMPAT_ATHOME/.git" ]; then
    pass "athome: the archive of the PINS commit $(pin athome) at $GROFE_COMPAT_ATHOME"
  else
    fail "athome: no exported tree at $GROFE_COMPAT_ATHOME"
  fi
  for t in canopy athome; do
    if [ -d "$SRC/tests/compat/scenarios/trees/$t/.git" ] || [ -z "$(ls -A "$SRC/tests/compat/scenarios/trees/$t" 2>/dev/null)" ]; then
      fail "recording tree: tests/compat/scenarios/trees/$t is not an exported tree"
    else
      pass "recording tree: tests/compat/scenarios/trees/$t"
    fi
  done
  if [ "$(as_golden id -un)" = golden ]; then
    pass "the recorder's user: the test user maps onto golden"
  else
    fail "the recorder's user: a user namespace onto golden (uid 1001) does not open"
  fi
}

# as_golden <cmd>...: <cmd> as uid 1001 (`golden`) in a user namespace
# whose 1001 is the test user's uid outside, so its files stay writable.
as_golden() { unshare --user --map-user=1001 --map-group=1001 "$@"; }

# failed_tests <log>: the names libtest lists under its last `failures:`,
# none when it printed no such list (a crashed run).
failed_tests() {
  awk '/^failures:$/ { f = 1; n = 0; delete t; next } /^$/ && n { f = 0 }
    f && /^    [A-Za-z0-9_:]+$/ { t[n++] = $1 }
    END { for (i = 0; i < n; i++) print t[i] }' "$1"
}

# suite <label> <libtest args>...: `cargo test --test compat` with those
# arguments, a failed case rerun alone and serially (FLAKY when it passes
# there), a skip a FAIL.
suite() {
  local label="$1" log="$HOME/$1.log" relog="$HOME/$1-retry.log" rc=0 failed=()
  shift
  cd "$SRC" || { fail "no source at $SRC"; return; }
  cargo test --locked --offline --test compat -- --show-output "$@" 2>&1 | tee "$log" || rc=$?
  grep -E '^test result:' "$log" | sed 's/^/SUMMARY /'
  if grep -E '^SKIPP(ED|ING)' "$log" | sort -u | sed 's/^/FAIL skip: /' | grep .; then
    fails=$((fails + 1))
  fi
  if ! grep -qE '^test result: .* [1-9][0-9]* passed' "$log"; then
    fail "$label: no case ran"
    return
  fi
  if [ "$rc" -eq 0 ]; then
    echo "FLAKY-COUNT 0"
    pass "$label"
    return
  fi
  mapfile -t failed < <(failed_tests "$log")
  if [ "${#failed[@]}" -eq 0 ]; then
    fail "$label: cargo test exited $rc with no failed case listed"
    return
  fi
  for t in "${failed[@]}"; do printf 'FAILED-TEST %s\n' "$t"; done
  rc=0
  cargo test --locked --offline --test compat -- --show-output --exact --test-threads=1 "${failed[@]}" 2>&1 |
    tee "$relog" || rc=$?
  grep -E '^test result:' "$relog" | sed 's/^/RETRY-SUMMARY /'
  if [ "$rc" -ne 0 ]; then
    mapfile -t failed < <(failed_tests "$relog")
    for t in "${failed[@]}"; do fail "$label: $t failed in the full run and again alone"; done
    [ "${#failed[@]}" -gt 0 ] || fail "$label: the serial rerun exited $rc with no failed case listed"
    return
  fi
  for t in "${failed[@]}"; do printf 'FLAKY %s (failed in the full run, passed alone)\n' "$t"; done
  echo "FLAKY-COUNT ${#failed[@]}"
  if [ "${E2E_STRICT:-0}" = 1 ]; then
    fail "$label: ${#failed[@]} FLAKY case(s), and E2E_STRICT=1"
  else
    pass "$label (${#failed[@]} FLAKY case(s) passed on the serial rerun)"
  fi
}

rerecord() {
  local out="$HOME/rerecord" rc=0 f rel n=0 differ=0 scenarios
  cd "$SRC" || { fail "no source at $SRC"; return; }
  rm -rf "$out"
  # As scripts/record-compat-goldens.sh runs it, minus podman: the
  # container is this one.
  as_golden env CARGO_NET_OFFLINE=true GROFE_COMPAT_TMUX=/usr/local/bin/tmux GROFE_COMPAT_OUT="$out" \
    cargo test --locked --offline --quiet --test compat -- --ignored --exact record::record_goldens --nocapture \
    >"$HOME/rerecord.log" 2>&1 || rc=$?
  tail -n 5 "$HOME/rerecord.log"
  if [ "$rc" -ne 0 ]; then
    fail "rerecord: the recorder exited $rc (see the lines above)"
    return
  fi
  while IFS= read -r -d '' f; do
    rel="${f#"$out"/}"
    n=$((n + 1))
    if ! diff -u "$GOLDEN/$rel" "$f"; then
      differ=$((differ + 1))
    fi
  done < <(find "$out" -type f -print0)
  scenarios="$(find "$GOLDEN" -name '*.scenario' | wc -l)"
  if [ "$n" -ne "$((scenarios + 1))" ]; then
    fail "rerecord: $n files recorded, want $scenarios goldens and list-commands.txt"
  elif [ "$differ" -ne 0 ]; then
    fail "rerecord: $differ of $n recorded files differ from the committed goldens"
  else
    pass "rerecord: all $n recorded files ($scenarios goldens and list-commands.txt) are byte-identical"
  fi
}

case "${1:-}" in
  tools) tools ;;
  tc13) suite tc13 scenarios:: --skip scenarios::copy_mode:: ;;
  copymode) suite copymode scenarios::copy_mode:: ;;
  rerecord) rerecord ;;
  *) echo "usage: compat.sh tools|tc13|copymode|rerecord" >&2; exit 2 ;;
esac
[ "$fails" -eq 0 ]
