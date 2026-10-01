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
# its file is already open. So this file is what continuum's saves and
# canopy's save key (M-s) both run: tmux.conf and the grofe plugin fragment
# point @resurrect-save-script-path and M-s at it after the loader, because
# resurrect sets its own path and key as it loads.
#
# A save that starts while another on the same server is running writes
# one line to $CANOPY_STATE/resurrect-save.log, says so on the status line
# unless it was asked to be quiet, and exits 0 at once.
#
# The lock is a directory holding its owner's pid. It is taken over, never
# waited on, when it is stale: its owner is gone, or its pid now belongs to
# something that is not this script (a reused pid), or it is older than
# three save intervals (ten minutes with no interval), or it has had no pid
# for longer than a save takes to write one. A takeover renames the stale
# lock out of the way first, so of two savers that both judge it stale
# only one gets it.
set -u

here="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || exit 0
# The save script itself; only the test suite names another.
save="${CANOPY_RESURRECT_SAVE:-$here/../tmux-resurrect/scripts/save.sh}"
state="${CANOPY_STATE:-${XDG_STATE_HOME:-$HOME/.local/state}/canopy}"
server="${TMUX:-}"
server="${server#*,}"
server="${server%%,*}"
lock="$state/resurrect-save.${server:-default}.lock"
quiet=0
owner=""
[ "${1-}" = quiet ] && quiet=1

mkdir -p "$state" 2>/dev/null || exit 0

take_lock() {
  mkdir "$lock" 2>/dev/null || return 1
  printf '%s\n' "$$" >"$lock/pid"
}

# Seconds since <path> was last modified, or nothing.
age_of() {
  local_m="$(stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null)" || return 1
  case "$local_m" in
    '' | *[!0-9]*) return 1 ;;
    *) ;;
  esac
  echo $(($(date +%s) - local_m))
}

# Most seconds a save may hold the lock: three save intervals, or ten
# minutes when no interval is set (or saving is off).
max_age() {
  minutes="$(tmux show -gqv @continuum-save-interval 2>/dev/null)" || minutes=""
  case "$minutes" in
    '' | *[!0-9]* | 0) echo 600 ;;
    *) echo $((minutes * 180)) ;;
  esac
}

# True when the lock is held by a save that is still running.
held() {
  age="$(age_of "$lock")" || return 1
  owner="$(cat "$lock/pid" 2>/dev/null)" || owner=""
  case "$owner" in
    '' | *[!0-9]*)
      # Made and not yet given its pid: a save writes it straight after
      # the mkdir, so only a lock this new is still being taken.
      [ "$age" -le 5 ]
      return
      ;;
    *) ;;
  esac
  [ "$age" -le "$(max_age)" ] || return 1
  kill -0 "$owner" 2>/dev/null || return 1
  # A machine without ps (a slim container) cannot tell a reused pid; the
  # age bound still frees the lock.
  command -v ps >/dev/null 2>&1 || return 0
  case "$(ps -o command= -p "$owner" 2>/dev/null)" in
    *canopy_save*) return 0 ;;
    *) return 1 ;;
  esac
}

skip() {
  printf '%s skipped: a save is already running (pid %s)\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${owner:-?}" >>"$state/resurrect-save.log"
  if [ "$quiet" -eq 0 ]; then
    tmux display-message "canopy: a save is already running, this one was skipped" >/dev/null 2>&1 || :
  fi
  exit 0
}

if ! take_lock; then
  held && skip
  # Stale: move it aside, so that of two savers judging it stale at once
  # only the one whose rename lands can take the lock. What was moved is
  # checked to be the lock judged stale: a saver that renames a lock taken
  # fresh in between puts it back and stands down.
  stale="$owner"
  aside="$lock.stale.$$"
  if mv "$lock" "$aside" 2>/dev/null; then
    moved="$(cat "$aside/pid" 2>/dev/null)" || moved=""
    if [ "$moved" != "$stale" ]; then
      mv "$aside" "$lock" 2>/dev/null || rm -rf "$aside"
      owner="$moved"
      skip
    fi
    rm -rf "$aside"
  fi
  take_lock || {
    owner="$(cat "$lock/pid" 2>/dev/null)" || owner=""
    skip
  }
fi
release() {
  [ "$(cat "$lock/pid" 2>/dev/null)" = "$$" ] && rm -rf "$lock"
  return 0
}
trap release EXIT
trap 'exit 1' HUP INT TERM

"$save" "$@"
