#!/usr/bin/env bash
# tmux-argv-recorder.sh: a `tmux` that appends its argv to a log, then runs
# the real tmux (plan M3.6, task TC.0).
#
# Install:
#
#   scripts/tmux-argv-recorder.sh install <bindir> <real-tmux> <log>
#
# writes <bindir>/tmux, a copy of this script with the real tmux's absolute
# path and the log path fixed in it. The copy execs the real tmux by that
# path, never through PATH, so it cannot find itself and recurse.
#
# Then put <bindir> first on PATH and START the tmux server from a shell that
# has that PATH (kill the old server first, or use a fresh -L socket). The
# server's environment is what run-shell, hooks, if-shell, popups and #()
# jobs inherit, so only a server started this way sends their `tmux` calls
# through the recorder. A log with no `job` line means the restart did not
# take.
#
# The owner's live tmux is never recorded this way (plan addendum, owner
# constraint of 2026-09-28): scripts/record-container-corpus.sh installs the
# recorder inside a container with its own tmux 3.7b.
#
# Each call appends one line: an origin, a tab, then the argv as bash's
# `printf %q` writes it. Origins:
#
#   job   the tmux server is an ancestor and the call has no controlling
#         terminal: a run-shell, hook, if-shell or #() job
#   pane  the tmux server is an ancestor and there is a terminal: a process
#         in a pane or a popup
#   cli   anything else: a script or shell outside tmux, or a daemon that a
#         job started and that detached from the server (opensessions'
#         server), whose parent chain no longer reaches it
#
# scripts/normalize-corpus.sh turns the log into corpus lines.

set -u

REAL_TMUX=@REAL_TMUX@
ARGV_LOG=@ARGV_LOG@

install_recorder() {
  if [ "$#" -ne 3 ]; then
    printf 'usage: %s install <bindir> <real-tmux> <log>\n' "$0" >&2
    return 2
  fi
  bindir=$1 real=$2 log=$3
  case $real in
    /*) ;;
    *) printf 'install: the real tmux must be an absolute path: %s\n' "$real" >&2; return 1 ;;
  esac
  case $log in
    /*) ;;
    *) printf 'install: the log must be an absolute path: %s\n' "$log" >&2; return 1 ;;
  esac
  if [ ! -x "$real" ] || [ -d "$real" ]; then
    printf 'install: not an executable: %s\n' "$real" >&2
    return 1
  fi
  if grep -q '^# tmux-argv-recorder.sh:' "$real" 2>/dev/null; then
    printf 'install: %s is a recorder itself, not the real tmux\n' "$real" >&2
    return 1
  fi
  mkdir -p "$bindir" || return 1
  target="$bindir/tmux"
  if [ "$(cd "$(dirname "$real")" && pwd -P)/$(basename "$real")" = "$(cd "$bindir" && pwd -P)/tmux" ]; then
    printf 'install: the recorder would replace the real tmux: %s\n' "$real" >&2
    return 1
  fi
  : >>"$log" || return 1
  q_real=$(printf '%q' "$real")
  q_log=$(printf '%q' "$log")
  tmp="$target.tmp.$$"
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      'REAL_TMUX=@REAL_TMUX@') line="REAL_TMUX=$q_real" ;;
      'ARGV_LOG=@ARGV_LOG@') line="ARGV_LOG=$q_log" ;;
    esac
    printf '%s\n' "$line"
  done <"${BASH_SOURCE[0]}" >"$tmp" || return 1
  chmod 755 "$tmp" && mv -f "$tmp" "$target" || return 1
  printf 'installed %s -> %s, logging to %s\n' "$target" "$real" "$log"
}

case $REAL_TMUX in
  @*@)
    # the template itself: only `install` is meaningful
    if [ "${1:-}" = install ]; then
      shift
      install_recorder "$@"
      exit
    fi
    printf 'tmux-argv-recorder.sh: not installed; run: %s install <bindir> <real-tmux> <log>\n' "$0" >&2
    exit 2
    ;;
esac

# Is the tmux server an ancestor? Its pid is the second field of $TMUX, which
# the server sets for everything it starts; walking the parents checks the
# value is not just inherited from somewhere else.
origin=cli
server_pid=${TMUX:-}
server_pid=${server_pid#*,}
server_pid=${server_pid%%,*}
case $server_pid in
  ''|*[!0-9]*) server_pid= ;;
esac
pid=$PPID
while [ -n "$server_pid" ] && [ "$pid" -gt 1 ] 2>/dev/null; do
  if [ "$pid" = "$server_pid" ]; then
    tty=$(ps -o tty= -p $$ 2>/dev/null | tr -d ' ')
    case $tty in
      ''|'?'|'??') origin=job ;;
      *) origin=pane ;;
    esac
    break
  fi
  pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
done

args=
if [ "$#" -gt 0 ]; then
  args=$(printf '%q ' "$@")
fi
# one write per call, so concurrent calls do not interleave within a line
printf '%s\t%s\n' "$origin" "$args" >>"$ARGV_LOG" 2>/dev/null
exec "$REAL_TMUX" "$@"
