#!/bin/sh
# canopy's save-command strategy for tmux-resurrect.
#
# resurrect asks a save-command strategy one question, once per pane, every
# time it saves: "what command should I record for this pane?" It hands over
# the pane's pid and reads one line from stdout. Upstream's own answer, the
# `ps` strategy, is whatever child the pane's shell has, recorded verbatim.
#
# Verbatim is wrong for exactly one kind of pane. `claude --session-id
# <uuid>` replayed after a reboot does not continue that conversation: it
# asks for a NEW one under an id that is already taken. The pane comes back
# and the work in it does not, which is the single failure this milestone
# exists to prevent. So for a pane canopy knows is holding an agent session,
# this strategy answers with that adapter's resume command instead, built
# from the session id the agent itself reported.
#
# The id comes off the pane's own options rather than out of the command
# line the agent was launched with. That is the whole reason this design was
# chosen over pinning an id at launch: an agent that cannot be handed an id
# when it starts is still resumable, because canopy learns the id from the
# agent's report and never needs it to have been on the command line.
#
# EVERYTHING ELSE MUST COME OUT UNCHANGED. This runs inside resurrect's save
# path, on every pane of every session, every time continuum fires. A line
# this script gets wrong is a pane that comes back as a bare shell, and a
# line it fails to print at all is the same thing. So every path that is not
# a recognised agent pane, including every error path, ends in
# canopy_save_default: resurrect's own strategy, run on the same pid, its
# output passed through untouched. A missing adapter, a malformed manifest,
# a tmux that will not answer, a store that has moved: all of them are a
# fallback, never a partial command and never an empty one.
set -u

pane_pid="${1-}"

# Where this file is, not where the environment says canopy is. A strategy
# runs as a plain child of resurrect's save script, which the tmux server
# runs, and the server's environment is whatever it was started with:
# $CANOPY_STORE may be in it, may be stale, may be missing. This file's own
# location is none of those things.
store="$(cd "$(dirname "$0")/../.." 2>/dev/null && pwd)" || store=""

canopy_save_default() {
  [ -n "$store" ] || exit 0
  default="$store/plugins/tmux-resurrect/save_command_strategies/ps.sh"
  [ -x "$default" ] || exit 0
  exec "$default" "$pane_pid"
}

# canopy_save_children <pid> <tty>
# What resurrect's own `ps` strategy prints for <pid>: the command line of
# each child it has, one per line, nothing for an idle shell.
#
# Asked of the pane's own terminal, never of the whole process table.
# resurrect runs a strategy once per pane, and its own answer reads every
# process on the machine each time, which is cheap on an idle laptop and is
# not on a busy one: a save over a few dozen panes then outlasted
# continuum's interval, continuum started the next save regardless, and
# they piled up for hours, each one adding to the load slowing the rest.
# A process on the pane's terminal is what `ps -a` lists for it anyway,
# and the parent test is `==` where resurrect's is a prefix match, the
# same as lib/pane.sh's.
canopy_save_children() {
  [ -n "$2" ] || return 1
  ps -o ppid=,args= -t "${2#/dev/}" 2>/dev/null |
    awk -v p="$1" '$1 == p { sub(/^ *[0-9]+ /, ""); print }'
}

# Prints the line to record for the pane holding $pane_pid: its resume
# command for an agent pane, else what resurrect's own strategy would have
# printed (nothing, for an idle shell). Fails when neither can be worked
# out, and resurrect's own strategy answers instead. Run inside a command
# substitution, so a canopy_die in any library it loads, or an unset
# variable under set -u, ends this lookup and not the save.
canopy_save_answer() {
  [ -n "$pane_pid" ] || return 1
  [ -n "$store" ] || return 1

  for lib in env.sh adapter.sh pane.sh resume.sh; do
    [ -r "$store/lib/$lib" ] || return 1
  done
  # shellcheck source=../../lib/env.sh
  # shellcheck disable=SC1091
  . "$store/lib/env.sh"
  # shellcheck source=../../lib/adapter.sh
  # shellcheck disable=SC1091
  . "$store/lib/adapter.sh"
  # shellcheck source=../../lib/pane.sh
  # shellcheck disable=SC1091
  . "$store/lib/pane.sh"
  # shellcheck source=../../lib/resume.sh
  # shellcheck disable=SC1091
  . "$store/lib/resume.sh"
  # Set before canopy_paths rather than left to it: canopy_paths derives an
  # unset store from its caller's own directory, and this caller lives two
  # levels down from one.
  CANOPY_STORE="$store"
  export CANOPY_STORE
  canopy_paths

  # One tmux call for the whole server: the pid resurrect named, the pane it
  # belongs to, and what that pane has reported about itself. The separator
  # is the one `canopy agent report` already refuses to accept inside either
  # value, and the session id is last so that a value carrying one anyway
  # cannot shift the fields before it.
  panes="$(tmux list-panes -a -F \
    '#{pane_pid}|#{pane_id}|#{pane_tty}|#{@canopy_agent_source}|#{@canopy_agent_session}' \
    2>/dev/null)" || return 1

  pane=""
  tty=""
  adapter=""
  session=""
  while IFS='|' read -r line_pid line_pane line_tty line_source line_session; do
    [ "$line_pid" = "$pane_pid" ] || continue
    pane="$line_pane"
    tty="$line_tty"
    adapter="$line_source"
    session="$line_session"
    break
  done <<EOF
$panes
EOF
  [ -n "$pane" ] || return 1
  default="$(canopy_save_children "$pane_pid" "$tty")" || return 1

  # lib/pane.sh's lookup of the running command reads the whole process
  # table. This pane's children are already known, so it is answered from
  # them instead.
  # shellcheck disable=SC2329  # called by canopy_pane_command in lib/pane.sh
  canopy_pane_running_command() {
    printf '%s\n' "$default" | head -n 1
  }

  # A session id with no source beside it: the three options are written by
  # more than one caller, and a hook that reports only a state leaves the
  # source empty. The pane is still resumable, so ask the same lookup every
  # other canopy command uses which agent it is running. It reads the pane's
  # start command and falls back to the running process, because a restored
  # pane has the second and not the first.
  if [ -z "$adapter" ]; then
    adapter="$(canopy_pane_adapter "$pane" 2>/dev/null)" || adapter=""
  fi

  # A pane restored from a save carries no pane options, because resurrect
  # saves none, so the id has to be read back out of the command the pane
  # is running. Without this the lookup gave up here and resurrect's own
  # strategy fell in behind it, saving the running command verbatim:
  # right by luck, and stripped of the stagger wrapper, so the reboot
  # after that started every agent in the same second.
  if [ -n "$adapter" ] && [ -z "$session" ]; then
    session="$(canopy_adapter_id_from_command "$adapter" \
      "$(canopy_pane_command "$pane" 2>/dev/null)" 2>/dev/null)" || session=""
  fi

  # No session id anywhere means no conversation to resume, whatever else
  # the pane may be running. That is most panes, and they all end up here.
  if [ -z "$adapter" ] || [ -z "$session" ] ||
    ! out="$(canopy_adapter_resume_command "$adapter" "$session" 2>/dev/null)"; then
    printf '%s\n' "$default"
    return 0
  fi

  # The wait goes in front of the command that is about to be saved, not
  # around the restore, because the saved line is the only thing resurrect
  # replays.
  stagger="$(tmux show -gqv @canopy_resume_stagger_ms 2>/dev/null)" || stagger=""
  out="$(canopy_resume_stagger_prefix "$pane_pid" "$stagger")$out"

  # resurrect's save file is tab delimited and one line per pane. A command
  # carrying a control character would not come back as itself; it would
  # take the rest of the file's shape with it.
  case "$out" in
    '' | *[[:cntrl:]]*) out="$default" ;;
    *) ;;
  esac

  printf '%s\n' "$out"
}

if answer="$(canopy_save_answer 2>/dev/null)"; then
  [ -z "$answer" ] || printf '%s\n' "$answer"
else
  canopy_save_default
fi
