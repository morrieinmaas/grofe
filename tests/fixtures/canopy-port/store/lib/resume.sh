# shellcheck shell=sh
# shellcheck disable=SC3043  # `local` is explicitly permitted (plan's Global Constraints): universally supported by dash/ash/bash, used here so callers never see this file's internals

# The stagger wrapper that canopy puts in front of a saved resume command.
#
# Ten agent panes restored at once launch ten agents at once, and an agent
# is not a cheap process. The command resurrect saves therefore carries a
# small wait in front of it, so the launches spread out instead of landing
# together on a machine that has just finished booting.
#
# The wrapper ends up in the command line a human may later read in the
# save file, or type again by hand. That is why it is a plain `sleep` and
# an `&&`: whoever reads it should need no explanation and no lookup.
#
# The functions here are written together because they have to agree about
# that shape, and there are three of them, in three languages: a printf
# format that emits the wrapper, a shell glob that takes it back off, and an
# extended regex that recognises it inside @resurrect-processes. Each has
# exactly one caller, so none of them checks the others.
#
# The regex is the one that can fail silently. Let it drift from the format
# and canopy keeps writing perfect resume commands while resurrect never
# replays them: the pane comes back a bare shell, the conversation is gone,
# and nothing reports anything. What stands between those literals is a
# test, "the wrapper a save writes matches the pattern the restore list
# carries" in test/stagger.bats, which matches one against the other with
# bash's own `=~`, the same operator resurrect matches with. Change any of
# the three and that test is what tells you the other two still agree.

# canopy_resume_stagger_prefix <pane pid> <@canopy_resume_stagger_ms>
# Prints the wrapper for that pane, or nothing at all when the option is
# exactly 0, which is how the feature is turned off rather than left
# emitting a wrapper that waits for nothing. An unset option, or one
# holding something that is not a number, takes the shipped default: there
# is nobody to report a bad value to from inside a save.
canopy_resume_stagger_prefix() {
  local bound delay
  # Long enough that ten panes come up about a tenth of a second apart,
  # short enough that a single restored pane is not noticeably late.
  bound=1000
  # A leading zero makes shell arithmetic read the value as octal, so 0500
  # would be 320 and 0900 is not a number at all: "value too great for
  # base", which fails the function and silently drops the stagger. Both
  # fall back to the default instead. Plain 0 still means off, because
  # that is the documented way to disable the wrapper.
  case "${2-}" in
    '' | *[!0-9]*) ;;
    0[0-9]*) ;;
    *) bound="$2" ;;
  esac
  [ "$bound" -gt 0 ] || return 0
  case "${1-}" in
    '' | *[!0-9]*) return 0 ;;
    *) ;;
  esac

  # The pid is multiplied by a prime before the modulo because panes opened
  # together get consecutive pids: taking the pid modulo the bound directly
  # would hand ten panes ten waits within a few milliseconds of each other,
  # which is the herd this exists to break up. The pid is reduced first so
  # the product cannot overflow a 32 bit shell's arithmetic.
  delay=$(((($1 % 100000) * 7919) % bound + 1))
  printf 'sleep %d.%03d && ' "$((delay / 1000))" "$((delay % 1000))"
}

# canopy_resume_stagger_strip <command>
# The command without its wrapper, for a caller comparing a saved command
# against the resume command it should hold.
canopy_resume_stagger_strip() {
  case "$1" in
    'sleep '*' && '*) printf '%s\n' "${1#*' && '}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

# canopy_resume_stagger_pattern
# The wrapper as the extended regular expression resurrect matches saved
# commands against. The third statement of the same shape, and the reason
# all three live in this file: a pattern that stops agreeing with the
# prefix is a pane that silently comes back as a bare shell.
canopy_resume_stagger_pattern() { printf '^sleep [0-9.]* && \n'; }

# canopy_resume_processes_option
# The value for @resurrect-processes: the list of saved commands resurrect
# is allowed to type back into a restored pane.
#
# Generated from the adapters canopy has rather than written out anywhere,
# so installing an adapter is what makes its panes restorable, and a
# machine with no adapters gets an empty list and leaves resurrect's own
# defaults exactly as they were.
#
# Two entries per command. The plain one is resurrect's own form, anchored
# by resurrect at the start of the saved command. The second is a regular
# expression for the same command behind the stagger wrapper, which the
# first would no longer match.
#
# Quoting is not decoration: resurrect splits this value with `eval set`,
# so the regular expression has to arrive as one word and anything a shell
# would act on has to stay inert. A command that is not a plain word is
# dropped rather than quoted harder, because the safe answer for a pane is
# to come back without its conversation and the unsafe one is to run an
# adapter's manifest as code.
canopy_resume_processes_option() {
  local id command seen out
  seen=""
  out=""
  for id in $(canopy_adapter_list); do
    command="$(canopy_adapter_get "$id" command 2>/dev/null)" || continue
    case "$command" in
      '' | *[!A-Za-z0-9._-]*) continue ;;
      *) ;;
    esac
    case " $seen " in
      *" $command "*) continue ;;
      *) ;;
    esac
    seen="$seen $command"
    out="$out $command \"~$(canopy_resume_stagger_pattern)$command\""
  done
  printf '%s\n' "${out# }"
}
