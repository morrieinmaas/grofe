#!/usr/bin/env bash
# footprint.sh: measures the grofe server's memory and prints a table.
#
#   scripts/footprint.sh [path/to/grofe]
#
# The binary is the argument, else $GROFE_BIN, else target/release/grofe.
# Three cases, each on a fresh server:
#   21 idle 80x24 panes
#   after `cat` of a ~46 MB coloured file, at the default history-limit
#   the same at history-limit 50000
#
# Footprint is the server's physical footprint: macOS `vmmap --summary`
# ("Physical footprint"), Linux RssAnon from /proc/<pid>/status.
#
# Each case gets its own HOME, TMPDIR, XDG dirs and -S socket under one
# mktemp -d, so it never touches a default socket, and grofe has no saved
# sessions to restore from an earlier run. GROFE, TMUX and TMUX_PANE are
# unset so a run from inside a pane cannot reach the live server.
set -euo pipefail
cd "$(dirname "$0")/.."

unset GROFE GROFE_PANE TMUX TMUX_PANE
bin="${1:-${GROFE_BIN:-target/release/grofe}}"
case "$bin" in /*) ;; *) bin="$PWD/$bin" ;; esac
[ -x "$bin" ] || { echo "footprint.sh: $bin is not built (cargo build --release)" >&2; exit 1; }

root="$(mktemp -d "${TMPDIR:-/tmp}/grofe-footprint.XXXXXX")"
pid=""
cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$root"
}
trap cleanup EXIT

big="$root/big.txt"
perl -e 'for (1..400000) { print "\e[3", $_%8, "m line $_ ", "x" x 100, "\e[0m\n" }' >"$big"

# Physical footprint of process $1, in MB.
footprint_mb() {
  if [ "$(uname)" = Darwin ]; then
    vmmap --summary "$1" | awk '
      /^Physical footprint:/ {
        v = $3; u = substr(v, length(v)); n = substr(v, 1, length(v) - 1)
        if (u == "K") n /= 1024; else if (u == "G") n *= 1024
        printf "%.1f\n", n; exit
      }'
  else
    awk '/^RssAnon:/ { printf "%.1f\n", $2 / 1024 }' "/proc/$1/status"
  fi
}

# run_case <limit or ""> <idle|cat>: sets $result to the footprint in MB. It runs
# in the main shell, not a subshell, so the EXIT trap sees $pid of a failed case.
run_case() {
  local limit="$1" kind="$2" dir sock
  dir="$(mktemp -d "$root/case.XXXXXX")"
  mkdir -p "$dir/home" "$dir/config/grofe" "$dir/state" "$dir/run" "$dir/tmp"
  [ -n "$limit" ] && echo "history-limit = $limit" >"$dir/config/grofe/config.toml"
  sock="$dir/run/footprint.sock"
  g() {
    HOME="$dir/home" TMPDIR="$dir/tmp" XDG_CONFIG_HOME="$dir/config" \
      XDG_STATE_HOME="$dir/state" XDG_RUNTIME_DIR="$dir/run" SHELL=/bin/sh \
      GROFE_RESTORE_ON_START=0 GROFE_NOTIFY_DESKTOP=off \
      "$bin" -S "$sock" "$@"
  }
  if [ "$kind" = idle ]; then
    g new-session -d -x 80 -y 24 -s fp
    pid="$(g display-message -p '#{pid}')"
    for _ in $(seq 20); do g new-window -d -t fp; done
    sleep 2
  else
    g new-session -d -x 80 -y 24 -s fp "cat '$big'; echo FOOTPRINT-DONE; sleep 600"
    pid="$(g display-message -p '#{pid}')"
    for _ in $(seq 600); do
      [[ "$(g capture-pane -p -t fp)" == *FOOTPRINT-DONE* ]] && break
      sleep 0.2
    done
    sleep 2
  fi
  result="$(footprint_mb "$pid")"
  g kill-server || true
  pid=""
}

run_case "" idle; idle="$result"
run_case "" cat; default="$result"
run_case 50000 cat; big_limit="$result"

printf '%-46s %10s\n' "case" "footprint"
printf '%-46s %7s MB\n' "21 idle 80x24 panes" "$idle"
printf '%-46s %7s MB\n' "after 46 MB, default history-limit" "$default"
printf '%-46s %7s MB\n' "after 46 MB, history-limit 50000" "$big_limit"
