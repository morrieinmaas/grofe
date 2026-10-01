#!/bin/sh
# resurrect's save, one at a time per server.
#
# continuum saves by running whatever @resurrect-save-script-path names,
# in the background, every @continuum-save-interval minutes, and nothing in
# continuum or resurrect waits for the previous save to end. continuum's
# own lock covers only its launcher, which exits as soon as the save is
# started. So a save that outlasts the interval is joined by another, and
# on a loaded machine they piled up for hours, each one adding to the load
# that slowed the rest. resurrect has no pre-save hook to refuse one from,
# and the save-command strategy runs only part way through a save, after
# its file is already open. This file is what @resurrect-save-script-path
# names instead (tmux.conf and the grofe plugin fragment set it after the
# loader, because resurrect sets its own path as it loads).
#
# A save that starts while another on the same server is running writes
# one line to $CANOPY_STATE/resurrect-save.log and exits 0 at once. The
# lock is a directory holding its owner's pid; one whose owner is gone (a
# killed save) is taken over.
set -u

here="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || exit 0
# The save script itself; only the test suite names another.
save="${CANOPY_RESURRECT_SAVE:-$here/../tmux-resurrect/scripts/save.sh}"
state="${CANOPY_STATE:-${XDG_STATE_HOME:-$HOME/.local/state}/canopy}"
server="${TMUX:-}"
server="${server#*,}"
server="${server%%,*}"
lock="$state/resurrect-save.${server:-default}.lock"

mkdir -p "$state" 2>/dev/null || exit 0

take_lock() {
  mkdir "$lock" 2>/dev/null || return 1
  printf '%s\n' "$$" >"$lock/pid"
}

if ! take_lock; then
  owner="$(cat "$lock/pid" 2>/dev/null)" || owner=""
  # The owner may have made the directory and not yet written its pid.
  if [ -z "$owner" ]; then
    sleep 1
    owner="$(cat "$lock/pid" 2>/dev/null)" || owner=""
  fi
  case "$owner" in
    '' | *[!0-9]*) owner="" ;;
    *) ;;
  esac
  if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
    printf '%s skipped: a save is already running (pid %s)\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$owner" >>"$state/resurrect-save.log"
    exit 0
  fi
  rm -rf "$lock"
  take_lock || exit 0
fi
trap 'rm -rf "$lock"' EXIT
trap 'exit 1' HUP INT TERM

"$save" "$@"
