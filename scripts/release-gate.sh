#!/usr/bin/env bash
# release-gate.sh: TE.3, the release gate. Every suite, from a clean export
# of HEAD, one leg after another (never two builds at once):
#
#   macos       scripts/ci-local.sh macos (see "The macos leg" below)
#   linux       scripts/ci-local.sh linux: scripts/ci-linux.sh, CI's Linux
#               steps, in a plain ubuntu:26.04 container as a sudo user
#   acceptance  scripts/e2e.sh acceptance (TE.1)
#   compat      scripts/e2e.sh compat (TE.2)
#   reboot      scripts/e2e.sh reboot (T4.6)
#   alias-day   scripts/e2e.sh alias-day (TA.1, standing in for T7.8)
#   rollback    tests/e2e/scenarios/rollback.sh in the systemd image
#   readme      tests/e2e/scenarios/readme.sh in the systemd image: the
#               README's "Alias day" steps, as written
#
#   GROFE_E2E_CANOPY_REPO=<canopy clone> GROFE_E2E_ATHOME_REPO=<athome clone> \
#     scripts/release-gate.sh
#
# It tests the commit at HEAD, and refuses to start with uncommitted
# changes: every leg runs from a detached checkout of that commit under
# target/release-gate/<run-id>/src, removed at the end.
#
# stdout is exactly one line per leg, in the order above:
#
#   PASS|FAIL|SKIP <leg> [(<why>)] <log>
#
# Everything a leg prints goes to its log, target/release-gate/<run-id>/
# <leg>.log; the verdict goes to stderr. Exits 0 only when every required
# leg passed, and otherwise 1, naming the legs that did not.
#
# Inputs: canopy and athome are taken from the two clones read only, at
# the commits tests/fixtures/compat/golden/tc_13/PINS records, through
# throwaway clones under a scratch directory. The ci-local legs get those
# clones as GROFE_COMPAT_CANOPY and GROFE_COMPAT_ATHOME, with
# GROFE_COMPAT_REQUIRED=1, so the compat suite runs instead of skipping.
# The alias-day leg reads athome's ref from tests/e2e/alias-day/pins.env
# (ATHOME_ALIAS_REF, a branch name), so it gets a throwaway athome clone
# whose branch of that name is PINS's athome commit: athome's own branch
# may have moved past what the goldens and the alias-day test were
# recorded against.
#
# The macos leg: on a macOS host it is ci-local.sh macos, required. On any
# other host it cannot be a macOS run, so it is reported SKIP and does not
# gate, but it still runs what this host can check: ci-local.sh macos
# natively here (fmt, clippy, the tests, the bats suite), and `cargo clippy
# --all-targets --target aarch64-apple-darwin -D warnings`, which compiles
# the macOS-only code paths (the aarch64-apple-darwin std must be
# installed: `rustup target add aarch64-apple-darwin`). If either fails the
# leg is FAIL and gates. A green gate from a Linux host therefore says
# nothing about macOS at run time; the release tag needs one run on macOS.
#
# The systemd image (rollback, readme) is tests/e2e/Containerfile.alias-day:
# TA.1's Debian image with systemd as PID 1, a lingering test user and real
# tmux 3.7b built at /usr/local/bin/tmux, which is the systemd image the
# plan's T7.8 would have built. Both legs share one build, which the gate
# removes at the end with the layers its build log names. Its inputs are
# canopy and athome at PINS, through scripts/e2e.sh's secrets gate.
#
# Environment:
#   GROFE_E2E_CANOPY_REPO, GROFE_E2E_ATHOME_REPO   required, as above
#   GROFE_GATE_FAIL=<leg>...   force these legs red without running them,
#                              to prove the gate fails and names them
#   GROFE_GATE_LEGS=<leg>...   run only these legs; the others print SKIP
#                              (not selected) and the run is no release gate
#   CONTAINER, E2E_PODMAN_NETWORK, E2E_PODMAN_OPTS   as scripts/e2e.sh
#
# E2E_STRICT=1 is exported to every leg: a scenario case that failed and
# passed on its rerun (FLAKY) fails the leg.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEGS=(macos linux acceptance compat reboot alias-day rollback readme)
export E2E_STRICT=1

# Wall-clock limits, in seconds; a leg that hits one is FAIL.
leg_timeout() {
  case "$1" in
    macos | reboot) echo 3600 ;;
    *) echo 7200 ;;
  esac
}

die() {
  echo "release-gate: $*" >&2
  exit 2
}

is_leg() {
  local l
  for l in "${LEGS[@]}"; do [ "$l" = "$1" ] && return 0; done
  return 1
}

in_list() {
  local want="$1" w
  shift
  for w in "$@"; do [ "$w" = "$want" ] && return 0; done
  return 1
}

# pin_of <name>: the commit PINS records for canopy or athome.
pin_of() {
  awk -v n="$1" '$1 == n { print $2 }' "$repo/tests/fixtures/compat/golden/tc_13/PINS"
}

# clone_at <src> <dest> <sha>: a throwaway clone of <src>, detached at <sha>.
clone_at() {
  git clone --quiet --no-checkout "$1" "$2" &&
    git -C "$2" checkout --quiet --detach "$3"
}

# --- the legs (each runs in its own process: release-gate.sh --leg <leg>) ---
# A leg exits 0 for PASS, 1 for FAIL, 3 for SKIP; its first line on fd 3
# (the reason file) is printed after the verdict.

leg_macos() {
  local rc=0
  scripts/ci-local.sh macos || rc=$?
  if [ "$(uname -s)" = Darwin ]; then
    return "$rc"
  fi
  if [ "$rc" -ne 0 ]; then
    echo "ci-local.sh macos exited $rc on this $(uname -s) host" >&3
    return 1
  fi
  if ! rustup target list --installed 2>/dev/null | grep -qx aarch64-apple-darwin; then
    echo "not a macOS host, and no aarch64-apple-darwin std for the clippy cross-check" >&3
    return 1
  fi
  echo "### cargo clippy --all-targets --target aarch64-apple-darwin"
  if ! cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings; then
    echo "clippy for aarch64-apple-darwin failed" >&3
    return 1
  fi
  echo "not a macOS host: ci-local.sh macos natively and clippy for aarch64-apple-darwin passed, nothing ran on macOS" >&3
  return 3
}

# Any failure is 1, so that no runner's own exit status reads as SKIP.
leg_linux() {
  scripts/ci-local.sh linux || return 1
}

leg_e2e() {
  scripts/e2e.sh "$1" || return 1
}

leg_alias_day() {
  local ref clone
  ref="$(sed -n 's/^ATHOME_ALIAS_REF=//p' tests/e2e/alias-day/pins.env)"
  clone="$GATE_WORK/alias-athome"
  rm -rf "$clone"
  git clone --quiet --no-checkout "$GROFE_E2E_ATHOME_REPO" "$clone"
  git -C "$clone" branch --quiet --force "$ref" "$(pin_of athome)"
  echo "### athome: a throwaway clone with $ref at $(pin_of athome) (PINS)"
  GROFE_E2E_ATHOME_REPO="$clone" scripts/e2e.sh alias-day || return 1
}

# systemd_image: builds the systemd image once per gate run, from grofe at
# the gate's commit and canopy and athome at PINS, as TA.1's runner lays its context out.
systemd_image() {
  if engine image exists "$GATE_IMAGE" 2>/dev/null; then
    echo "### systemd image $GATE_IMAGE: already built by an earlier leg"
    return 0
  fi
  local w="$GATE_WORK/systemd" in
  rm -rf "$w"
  mkdir -p "$w/ctx/grofe"
  git -C "$repo" archive "$GATE_SHA" | tar -x -C "$w/ctx/grofe"
  pinned_inputs "$w" canopy athome || return 1
  # The image wants athome as a tar and canopy's bundle on branch alias-day.
  in="$w/ctx/inputs"
  tar -C "$in/athome" -cf "$w/athome.tar" . && rm -rf "$in/athome" && mv "$w/athome.tar" "$in/athome.tar" || return 1
  git -C "$w/clone-canopy" branch --quiet --force alias-day "$(pin_of canopy)" &&
    git -C "$w/clone-canopy" bundle create --quiet "$in/canopy.bundle" alias-day || return 1
  rm -rf "$in/canopy" "$w/gate-"*
  echo "### building $GATE_IMAGE (log: $GATE_OUT/systemd-build.log)"
  timeout 3600 "${engine_argv[@]}" build ${net_args[@]+"${net_args[@]}"} \
    -f "$w/ctx/grofe/tests/e2e/Containerfile.alias-day" -t "$GATE_IMAGE" "$w/ctx" \
    >"$GATE_OUT/systemd-build.log" 2>&1
}

# leg_systemd <scenario>: a fresh container of the systemd image, booted;
# TA.1's stage 2 (athome applied: canopy on grofe, grofe as `tmux`, the
# login unit started), then the scenario. The host-state check of
# scripts/e2e.sh must find the owner's state unchanged.
leg_systemd() {
  local scenario="$1" name rc=0
  # shellcheck source-path=SCRIPTDIR source=e2e.sh
  . "$repo/scripts/e2e.sh"
  host_state >"$GATE_WORK/$scenario.host.before"
  systemd_image || { echo "the systemd image did not build" >&3; return 1; }
  name="grofe-gate-$scenario-$$"
  # shellcheck disable=SC2064 # the name is fixed now
  trap "engine rm -f -t 0 '$name' >/dev/null 2>&1 || true" EXIT
  engine run -d --name "$name" ${net_args[@]+"${net_args[@]}"} --hostname e2e --systemd=always "$GATE_IMAGE" >/dev/null
  engine exec "$name" timeout 120 systemctl is-system-running --wait || true
  local i
  for i in $(seq 1 60); do
    engine exec "$name" test -S /run/user/1000/bus && break
    [ "$i" -lt 60 ] || { echo "the test user's bus never came up" >&3; return 1; }
    sleep 1
  done
  local as_test=(engine exec -u test -w /home/test -e HOME=/home/test -e USER=test -e LOGNAME=test
    -e E2E_STRICT="$E2E_STRICT" "$name")
  echo "### TA.1 stage 2: athome applied, canopy on grofe"
  if ! "${as_test[@]}" bash /opt/e2e/alias-day.sh apply; then
    rc=1
    echo "TA.1's apply failed, so the scenario did not run" >&3
  else
    echo "### $scenario.sh"
    "${as_test[@]}" bash "/opt/e2e/scenarios/$scenario.sh" || rc=$?
    [ "$rc" -eq 0 ] || echo "$scenario.sh exited $rc" >&3
  fi
  engine cp "$name:/home/test/e2e-out" "$GATE_OUT/$scenario-out" >/dev/null 2>&1 || true
  engine exec "$name" journalctl --no-pager -b >"$GATE_OUT/$scenario-journal.log" 2>&1 || true
  engine rm -f -t 0 "$name" >/dev/null 2>&1 || true
  host_state >"$GATE_WORK/$scenario.host.after"
  if ! cmp -s "$GATE_WORK/$scenario.host.before" "$GATE_WORK/$scenario.host.after"; then
    diff "$GATE_WORK/$scenario.host.before" "$GATE_WORK/$scenario.host.after" || true
    echo "the host-state check found a difference" >&3
    rc=1
  fi
  [ "$rc" -eq 0 ]
}

run_one_leg() {
  local leg="$1"
  cd "$repo"
  export GROFE_COMPAT_CANOPY="$GATE_WORK/pins/canopy" GROFE_COMPAT_ATHOME="$GATE_WORK/pins/athome"
  export GROFE_COMPAT_REQUIRED=1
  case "$leg" in
    macos) leg_macos ;;
    linux) leg_linux ;;
    acceptance | compat | reboot) leg_e2e "$leg" ;;
    alias-day) leg_alias_day ;;
    rollback | readme) leg_systemd "$leg" ;;
  esac
}

# --- the gate -----------------------------------------------------------------

main() {
  local forced=() selected=() leg
  # shellcheck disable=SC2206 # lists of words
  forced=(${GROFE_GATE_FAIL:-})
  # shellcheck disable=SC2206
  selected=(${GROFE_GATE_LEGS:-})
  for leg in ${forced[@]+"${forced[@]}"} ${selected[@]+"${selected[@]}"}; do
    is_leg "$leg" || die "no leg named '$leg' (legs: ${LEGS[*]})"
  done
  [ -n "${GROFE_E2E_CANOPY_REPO:-}" ] || die "set GROFE_E2E_CANOPY_REPO to a local canopy clone"
  [ -n "${GROFE_E2E_ATHOME_REPO:-}" ] || die "set GROFE_E2E_ATHOME_REPO to a local athome clone"
  GROFE_E2E_CANOPY_REPO="$(cd "$GROFE_E2E_CANOPY_REPO" && pwd)"
  GROFE_E2E_ATHOME_REPO="$(cd "$GROFE_E2E_ATHOME_REPO" && pwd)"
  export GROFE_E2E_CANOPY_REPO GROFE_E2E_ATHOME_REPO

  # The gate tests one commit: a change not committed would be tested by
  # no leg, or by some legs and not others.
  [ -z "$(git -C "$repo" status --porcelain)" ] ||
    die "the working tree has changes; commit or stash them first (git status)"
  local sha run_id
  sha="$(git -C "$repo" rev-parse HEAD)"
  run_id="$(date -u +%Y%m%dT%H%M%SZ)-${sha:0:7}"
  GATE_OUT="$repo/target/release-gate/$run_id"
  GATE_WORK="$(mktemp -d "${TMPDIR:-/tmp}/grofe-gate.XXXXXX")"
  GATE_IMAGE="localhost/grofe-gate-systemd:$(printf '%s' "$run_id" | tr '[:upper:]' '[:lower:]')"
  # Every leg runs from this detached checkout of the commit, so a commit
  # or an edit made while the gate runs changes nothing it tests. It lives
  # on disk with the logs (its builds are large), and goes at the end.
  GATE_SRC="$GATE_OUT/src"
  GATE_SHA="$sha"
  GROFE_CI_REV="$sha"
  export GATE_OUT GATE_WORK GATE_IMAGE GATE_SHA GROFE_CI_REV
  mkdir -p "$GATE_OUT"
  git -C "$repo" worktree add --quiet --detach "$GATE_SRC" "$sha" ||
    die "could not check out $sha at $GATE_SRC"
  # The e2e runners write their reports under target/e2e: the repo's, so
  # they outlive the checkout.
  mkdir -p "$repo/target/e2e" "$GATE_SRC/target"
  ln -s "$repo/target/e2e" "$GATE_SRC/target/e2e"
  # shellcheck disable=SC2329 # invoked by the EXIT trap
  cleanup() {
    local engine_cmd=("${CONTAINER:-podman}")
    # shellcheck disable=SC2206 # E2E_PODMAN_OPTS is a list of words
    engine_cmd+=(${E2E_PODMAN_OPTS:-})
    if "${engine_cmd[@]}" image exists "$GATE_IMAGE" 2>/dev/null; then
      "${engine_cmd[@]}" rmi -f "$GATE_IMAGE" >/dev/null 2>&1 || true
    fi
    # The layers the systemd build made or reused, as its log names them;
    # the bases, pulled by digest, are shared and stay.
    grep -oE '^(--> |Using cache )[0-9a-f]{12,}' "$GATE_OUT/systemd-build.log" 2>/dev/null | awk '{ print $NF }' |
      sort -u | xargs -r "${engine_cmd[@]}" rmi -f >/dev/null 2>&1 || true
    git -C "$repo" worktree remove --force "$GATE_SRC" >/dev/null 2>&1 || true
    rm -rf "$GATE_WORK"
  }
  trap cleanup EXIT

  echo "release-gate: grofe $sha, logs in $GATE_OUT" >&2
  local pins_ok=1 c a
  c="$(pin_of canopy)"
  a="$(pin_of athome)"
  if ! { clone_at "$GROFE_E2E_CANOPY_REPO" "$GATE_WORK/pins/canopy" "$c" &&
    clone_at "$GROFE_E2E_ATHOME_REPO" "$GATE_WORK/pins/athome" "$a"; } >"$GATE_OUT/pins.log" 2>&1; then
    pins_ok=0
    echo "release-gate: could not check out canopy $c and athome $a (PINS): $GATE_OUT/pins.log" >&2
  fi

  local failed=() verdict why log rc start
  for leg in "${LEGS[@]}"; do
    log="$GATE_OUT/$leg.log"
    why=""
    if in_list "$leg" ${forced[@]+"${forced[@]}"}; then
      verdict=FAIL why="forced by GROFE_GATE_FAIL"
      echo "forced red by GROFE_GATE_FAIL; not run" >"$log"
    elif [ "${#selected[@]}" -gt 0 ] && ! in_list "$leg" "${selected[@]}"; then
      verdict=SKIP why="not selected"
      echo "not in GROFE_GATE_LEGS; not run" >"$log"
    elif [ "$pins_ok" -eq 0 ]; then
      verdict=FAIL why="no checkout at PINS"
      cp "$GATE_OUT/pins.log" "$log"
    else
      rc=0
      start="$(date +%s)"
      : >"$GATE_WORK/why"
      timeout --kill-after=60 "$(leg_timeout "$leg")" bash "$GATE_SRC/scripts/release-gate.sh" --leg "$leg" \
        </dev/null >"$log" 2>&1 3>"$GATE_WORK/why" || rc=$?
      why="$(head -n 1 "$GATE_WORK/why")"
      case "$rc" in
        0) verdict=PASS ;;
        3) verdict=SKIP ;;
        124 | 137) verdict=FAIL why="timed out after $(leg_timeout "$leg")s" ;;
        *) verdict=FAIL why="${why:-exit $rc}" ;;
      esac
      echo "release-gate: $leg $verdict after $(($(date +%s) - start))s" >&2
    fi
    case "$verdict" in
      FAIL) failed+=("$leg") ;;
      SKIP) [ "$why" = "not selected" ] || [ "$leg" = macos ] || failed+=("$leg") ;;
    esac
    if [ -n "$why" ]; then
      printf '%s %s (%s) %s\n' "$verdict" "$leg" "$why" "${log#"$repo"/}"
    else
      printf '%s %s %s\n' "$verdict" "$leg" "${log#"$repo"/}"
    fi
  done

  if [ "${#failed[@]}" -gt 0 ]; then
    echo "release-gate: FAIL: ${failed[*]}" >&2
    return 1
  fi
  if [ "${#selected[@]}" -gt 0 ]; then
    echo "release-gate: the selected legs passed (a partial run, not a release gate)" >&2
  else
    echo "release-gate: PASS" >&2
  fi
}

if [ "${1:-}" = --leg ]; then
  is_leg "${2:-}" || die "no leg named '${2:-}'"
  run_one_leg "$2"
  exit
fi
main "$@"
