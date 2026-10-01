# shellcheck shell=sh
# shellcheck disable=SC3043  # `local` is not in POSIX, but dash, ash, BusyBox and every other /bin/sh canopy runs on implement it, and every other lib/ file here already relies on it

# Which pane is running which agent.
#
# One lookup, here, rather than one per command: `reboot-check` and `adopt`
# both have to answer it, and two implementations of "is this an agent pane"
# is two chances to answer differently about the same pane.
#
# **Never `pane_current_command`.** tmux reports the interpreter for an
# agent that is a shell script, and on a real machine a resurrect save of
# this project's owner recorded `pane_current_command` for its claude panes
# as "2.1.245", the agent's own version string. The pane's command line is
# what canopy matches, which is also what tmux-resurrect saves and restores.

# canopy_pane_start_command <pane-id>
# The command tmux itself was told to run in the pane, as tmux renders it
# (quoting included). Empty for a pane that got the default shell, which
# after a resurrect restore is every pane: resurrect re-creates a pane with
# the shell and then types the saved command into it.
canopy_pane_start_command() {
  tmux display-message -p -t "$1" '#{pane_start_command}' 2>/dev/null
}

# canopy_pane_running_command <pane-id>
# The command line of the program running in front of the pane's shell,
# read the way tmux-resurrect's own `ps` save-command strategy reads it.
#
# The ppid test is `==`, not resurrect's `grep "^$pid"`: a prefix match lets
# pane pid 123 claim the children of pid 1234. Every failure here is
# reported as "no command", because a userland whose ps cannot answer is a
# reason to recognise nothing, never a reason to abort the caller.
canopy_pane_running_command() {
  local pid
  pid="$(tmux display-message -p -t "$1" '#{pane_pid}' 2>/dev/null)" || return 1
  [ -n "$pid" ] || return 1
  ps -ao ppid,args 2>/dev/null |
    awk -v p="$pid" '$1 == p { $1 = ""; sub(/^ */, ""); print; exit }'
}

# canopy_pane_command_name <command-line>
# The program a command line names, with its directory and the quoting tmux
# adds both removed. Returns 1 when the line names no program at all.
#
# Leading interpreters are skipped, because that is how an agent written as
# a shell script arrives: exec'ing a script makes argv[0] the interpreter,
# so the fixture agent shows up in ps as `/bin/sh /path/to/fake-agent ...`
# and a plain first-word match would call every such agent "sh". Options and
# VAR=value assignments are skipped for the same reason, so that
# `env FOO=1 claude --resume x` still names claude.
canopy_pane_command_name() {
  local line word
  line="$(printf '%s' "$1" | tr -d "\"'")"
  while [ -n "$line" ]; do
    word="${line%% *}"
    case "$line" in
      *' '*) line="${line#* }" ;;
      *) line="" ;;
    esac
    word="${word##*/}"
    case "$word" in
      '' | -* | *=*) continue ;;
      sh | bash | dash | ksh | zsh | env) continue ;;
      *)
        printf '%s\n' "$word"
        return 0
        ;;
    esac
  done
  return 1
}

# canopy_pane_command <pane-id>
# The pane's command line: what tmux started the pane with when that names
# a program, else what is running in front of the pane's shell.
#
# The order matters and is not interchangeable. A pane started AS the agent
# (`new-window 'claude --resume x'`) has the agent as its own process, so
# the ps lookup finds the agent's children instead of the agent, and for the
# fixture agent that is a bare `sleep 1`. A pane the agent was typed into,
# which after a resurrect restore is every pane, has no start command at all
# and only the ps lookup can see it. Each source covers the other's blind
# spot, so both are consulted, start command first.
canopy_pane_command() {
  local cmd
  cmd="$(canopy_pane_start_command "$1")" || cmd=""
  if [ -n "$cmd" ] && [ -n "$(canopy_pane_command_name "$cmd")" ]; then
    printf '%s\n' "$cmd"
    return 0
  fi
  cmd="$(canopy_pane_running_command "$1")" || return 1
  [ -n "$cmd" ] || return 1
  printf '%s\n' "$cmd"
}

# canopy_pane_adapter <pane-id>
# The id of the installed adapter whose `command` this pane is running.
# Returns 1, printing nothing, when the pane is running no agent canopy
# knows about.
#
# Deliberately says nothing about @canopy_agent_source. That option records
# what a pane last reported and outlives the process that reported it, so a
# pane whose agent exited an hour ago still carries it. The question this
# answers is what is running now, which is the question `reboot-check` lists
# on and the one `adopt` is forbidden to act without.
canopy_pane_adapter() {
  local cmd name id
  cmd="$(canopy_pane_command "$1")" || return 1
  [ -n "$cmd" ] || return 1
  name="$(canopy_pane_command_name "$cmd")" || return 1
  [ -n "$name" ] || return 1
  for id in $(canopy_adapter_list); do
    if [ "$(canopy_adapter_get "$id" command)" = "$name" ]; then
      printf '%s\n' "$id"
      return 0
    fi
  done
  return 1
}

# canopy_pane_agent_panes
# Every pane on this server running a known agent, one per line, as
# <pane-id><TAB><adapter-id>. Panes running anything else are absent rather
# than present-and-blank, so a caller iterating the output acts only on
# panes canopy actually recognises.
canopy_pane_agent_panes() {
  local pane id
  tmux list-panes -a -F '#{pane_id}' 2>/dev/null | while IFS= read -r pane; do
    if id="$(canopy_pane_adapter "$pane")"; then
      printf '%s\t%s\n' "$pane" "$id"
    fi
  done
}
