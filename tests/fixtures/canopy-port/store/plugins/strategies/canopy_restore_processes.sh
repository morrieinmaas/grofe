#!/bin/sh
# Tells resurrect which saved commands it is allowed to replay.
#
# resurrect types a pane's saved command back into the restored pane only
# when that command matches @resurrect-processes. Without this, the resume
# command canopy writes into the save file is never typed: the pane comes
# back as a bare shell and the conversation is gone. Everything else in
# this milestone is the other half of that loop.
#
# Run by tmux at config load, out of 30-plugins.conf and before the two
# plugins are loaded, because continuum's restore fires seconds after the
# server starts and an option set after that would be set too late.
#
# Every failure here is silent and leaves the option empty. This runs
# inside source-file, where a non-zero exit fails the whole configuration
# load, and losing persistence is bad while losing the user's entire tmux
# configuration is worse.
set -u

canopy_restore_processes() {
  store="$(cd "$(dirname "$0")/../.." 2>/dev/null && pwd)" || return 1
  for lib in env.sh adapter.sh resume.sh; do
    [ -r "$store/lib/$lib" ] || return 1
  done
  # shellcheck source=../../lib/env.sh
  # shellcheck disable=SC1091
  . "$store/lib/env.sh"
  # shellcheck source=../../lib/adapter.sh
  # shellcheck disable=SC1091
  . "$store/lib/adapter.sh"
  # shellcheck source=../../lib/resume.sh
  # shellcheck disable=SC1091
  . "$store/lib/resume.sh"
  # Set before canopy_paths rather than left to it: canopy_paths derives an
  # unset store from its caller's own directory, and this caller lives two
  # levels down from one.
  CANOPY_STORE="$store"
  export CANOPY_STORE
  canopy_paths

  canopy_resume_processes_option
}

value="$(canopy_restore_processes 2>/dev/null)" || value=""
tmux set -g @resurrect-processes "$value" >/dev/null 2>&1 || :
exit 0
