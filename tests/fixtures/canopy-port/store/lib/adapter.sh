# shellcheck shell=sh
# shellcheck disable=SC2154  # CANOPY_STORE/CANOPY_CONFIG are exported by lib/env.sh's canopy_paths
# shellcheck disable=SC3043  # `local` is explicitly permitted (plan's Global Constraints): universally supported by dash/ash/bash, used here so callers never see this file's internals

# The agent adapter contract.
#
# An adapter is a directory holding one file, `manifest`, of `key=value`
# lines. It tells canopy how to recognise one agent in a pane and how to
# put that agent back where it was. docs/adapters/contract.md is the
# document; this file is the only thing that reads it.
#
# Adapters are looked up by directory name, and the manifest's own `id`
# must agree with that name. One name, checked, rather than two names that
# can drift: an adapter copied into place under the wrong directory is
# caught the first time anything reads it, not the first time a pane fails
# to resume.
#
# The search order is $CANOPY_CONFIG/adapters before $CANOPY_STORE/adapters,
# so a user's adapter of a given id wins over a shipped one of the same id.

# canopy_adapter_keys
# The six keys, in the order the contract document lists them. Every key
# is required in every manifest; a value may be empty, a key may not be
# absent. Absent-means-default is how two readers of the same file end up
# disagreeing about what the default was.
canopy_adapter_keys() {
  printf 'id command resume_template can_pin_at_launch launch_template detect_draft\n'
}

# canopy_adapter_dir <id>
# The directory holding <id>'s manifest, or 1 if no adapter has that id.
canopy_adapter_dir() {
  local dir
  for dir in "$CANOPY_CONFIG/adapters" "$CANOPY_STORE/adapters"; do
    if [ -f "$dir/$1/manifest" ]; then
      printf '%s\n' "$dir/$1"
      return 0
    fi
  done
  return 1
}

# canopy_adapter_list
# Every installed adapter id, one per line, sorted and deduplicated. An
# adapter shadowed by one of the same id earlier in the search path is
# listed once, because there is one adapter by that id as far as every
# caller is concerned.
canopy_adapter_list() {
  local dir entry name
  {
    for dir in "$CANOPY_CONFIG/adapters" "$CANOPY_STORE/adapters"; do
      [ -d "$dir" ] || continue
      for entry in "$dir"/*/; do
        [ -f "$entry/manifest" ] || continue
        name="${entry%/}"
        printf '%s\n' "${name##*/}"
      done
    done
  } | LC_ALL=C sort -u
}

# canopy_adapter_get <id> <key>
# Prints one key's value. Returns 1 when no adapter has that id, so a
# caller can ask whether an adapter exists. Every other problem is a
# broken contract rather than a missing one, and dies loudly: an unknown
# key in the request, an unknown or duplicated or missing key in the
# manifest, a line that is not key=value, an id that disagrees with the
# directory, a can_pin_at_launch that is neither yes nor no, or a template
# with no {id} to substitute into.
#
# The whole manifest is parsed and validated on every call, not just the
# requested line. A reader that stopped at the key it wanted would happily
# return a correct value out of a manifest that is wrong three lines
# lower, and the consumer that eventually trips over that line would be
# debugging the wrong file.
# canopy_adapter_expand <template> <value> [placeholder]
# The one place a contract placeholder is substituted. Every template key
# in the contract carries one, and more than one caller needs the same
# answer: the save strategy writes a pane's resume command, and
# reboot-check has to work out what that command would have been to tell
# whether the save on disk actually holds it. The same rule written out
# beside each caller is a rule that drifts, and this project has paid for
# that already.
#
# The placeholder defaults to {id} because that is what every template key
# but one carries. detect_draft is the one, and it carries {pane}, so
# `canopy adopt` passes it rather than growing a second copy of this loop.
canopy_adapter_expand() {
  local out rest ph
  ph="${3-}"
  [ -n "$ph" ] || ph='{id}'
  out=""
  rest="$1"
  while :; do
    case "$rest" in
      *"$ph"*)
        out="$out${rest%%"$ph"*}$2"
        rest="${rest#*"$ph"}"
        ;;
      *)
        out="$out$rest"
        break
        ;;
    esac
  done
  printf '%s\n' "$out"
}

# canopy_adapter_id_from_command <adapter-id> <command>
# The session id carried by a command that resumes one, or 1 when the
# command is not that adapter's resume command.
#
# The reverse of canopy_adapter_resume_command, and it exists because a
# restored pane knows its own id while tmux does not. resurrect saves no
# user pane options, so a pane brought back as `agent --resume <id>` comes
# back with @canopy_agent_session empty, and every caller that reads only
# that option concludes the pane has no conversation. reboot-check said
# exactly that about panes it had just restored correctly.
#
# The id is read back out of the command the pane is running, which is the
# one place it still exists, and then held to the same character set the
# reporter enforces: this string comes from a process command line, and it
# goes on to be handed to a shell.
canopy_adapter_id_from_command() {
  local template pre suf rest key
  [ -n "${2-}" ] || return 1
  # Both templates, resume first.
  #
  # A pane carries its id in whichever form started it. One resumed from a
  # save is running the resume command; one an agent launched fresh is
  # running the launch command, and for an agent that takes a caller
  # chosen id that command holds the id just as plainly. Reading only the
  # resume form meant canopy looked at sixteen live panes, each with its
  # uuid visible in its own command line, and reported that not one of
  # them had a session id.
  for key in resume_template launch_template; do
    template="$(canopy_adapter_get "$1" "$key" 2>/dev/null)" || continue
    case "$template" in
      *'{id}'*) ;;
      *) continue ;;
    esac
    if rest="$(canopy_adapter_id_from_template "$template" "$2")"; then
      printf '%s\n' "$rest"
      return 0
    fi
  done
  return 1
}

# canopy_adapter_id_from_template <template> <command>
# The id <command> carries, per one template, or 1 when it carries none.
canopy_adapter_id_from_template() {
  local template pre suf rest
  template="$1"
  pre="${template%%\{id\}*}"
  suf="${template#*\{id\}}"

  case "$2" in
    *"$pre"*) ;;
    *) return 1 ;;
  esac
  rest="${2#*"$pre"}"
  if [ -n "$suf" ]; then
    case "$rest" in
      *"$suf"*) rest="${rest%%"$suf"*}" ;;
      *) return 1 ;;
    esac
  fi
  # An id never contains a space, so anything after one belongs to the
  # command rather than to the id.
  rest="${rest%% *}"
  case "$rest" in
    '' | *[!A-Za-z0-9._:-]*) return 1 ;;
    *) ;;
  esac
  printf '%s\n' "$rest"
}

# canopy_adapter_resume_command <adapter-id> <session-id>
# The command that puts a pane back into the conversation it was in.
# An empty session id fails rather than producing a resume command with
# nothing to resume: that command would start a fresh conversation while
# looking exactly like one that continued the old one.
canopy_adapter_resume_command() {
  local template
  [ -n "${2-}" ] || return 1
  template="$(canopy_adapter_get "$1" resume_template)" || return 1
  [ -n "$template" ] || return 1
  canopy_adapter_expand "$template" "$2"
}

canopy_adapter_get() {
  local id key dir manifest known line k v seen
  local a_id a_command a_resume a_pin a_launch a_draft
  id="$1"
  key="$2"
  known="$(canopy_adapter_keys)"

  case " $known " in
    *" $key "*) ;;
    *) canopy_die "canopy_adapter_get: $key is not a contract key (the contract is: $known)" ;;
  esac

  if ! dir="$(canopy_adapter_dir "$id")"; then
    printf 'canopy_adapter_get: no adapter named %s is installed\n' "$id" >&2
    return 1
  fi
  manifest="$dir/manifest"

  seen=""
  a_id=""
  a_command=""
  a_resume=""
  a_pin=""
  a_launch=""
  a_draft=""

  # `|| [ -n "$line" ]` so a final line with no trailing newline is still
  # read rather than silently dropped, which is how a hand-edited manifest
  # loses its last key.
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      '' | '#'*) continue ;;
      *'='*) ;;
      *) canopy_die "$manifest: not a key=value line: $line" ;;
    esac
    k="${line%%=*}"
    v="${line#*=}"
    case " $known " in
      *" $k "*) ;;
      *) canopy_die "$manifest: unknown key: $k (the contract is: $known)" ;;
    esac
    case " $seen " in
      *" $k "*) canopy_die "$manifest: duplicate key: $k" ;;
      *) ;;
    esac
    seen="$seen $k "
    case "$k" in
      id) a_id="$v" ;;
      command) a_command="$v" ;;
      resume_template) a_resume="$v" ;;
      can_pin_at_launch) a_pin="$v" ;;
      launch_template) a_launch="$v" ;;
      detect_draft) a_draft="$v" ;;
      *) ;;
    esac
  done <"$manifest"

  for k in $known; do
    case " $seen " in
      *" $k "*) ;;
      *) canopy_die "$manifest: missing required key: $k" ;;
    esac
  done

  [ "$a_id" = "$id" ] ||
    canopy_die "$manifest: id is $a_id but the adapter directory is named $id; they must agree"
  [ -n "$a_command" ] ||
    canopy_die "$manifest: command is empty, so no pane could ever be recognised as this agent"
  case "$a_pin" in
    yes | no) ;;
    *) canopy_die "$manifest: can_pin_at_launch must be yes or no, got: $a_pin" ;;
  esac
  case "$a_resume" in
    *'{id}'*) ;;
    *) canopy_die "$manifest: resume_template must contain the {id} placeholder, got: $a_resume" ;;
  esac
  case "$a_draft" in
    '') ;;
    *'{pane}'*) ;;
    *)
      canopy_die "$manifest: detect_draft must contain the {pane} placeholder, got: $a_draft"
      ;;
  esac
  if [ "$a_pin" = yes ]; then
    case "$a_launch" in
      *'{id}'*) ;;
      *) canopy_die "$manifest: can_pin_at_launch is yes, so launch_template must contain the {id} placeholder, got: $a_launch" ;;
    esac
  fi

  case "$key" in
    id) printf '%s\n' "$a_id" ;;
    command) printf '%s\n' "$a_command" ;;
    resume_template) printf '%s\n' "$a_resume" ;;
    can_pin_at_launch) printf '%s\n' "$a_pin" ;;
    launch_template) printf '%s\n' "$a_launch" ;;
    detect_draft) printf '%s\n' "$a_draft" ;;
    *) ;;
  esac
}
