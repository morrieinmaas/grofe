# shellcheck shell=sh

# canopy_require_absolute <name> <value>
# Rejects a relative path, rather than absolutising it. canopy records
# these paths verbatim into a transaction manifest that outlives the
# process, and $tx/restore.sh resolves manifest paths against whatever
# directory it is later run from, so a relative value would produce a
# restore point that names a different file than the one canopy backed
# up. Absolutising would have to pick a cwd on the user's behalf, and
# canopy cannot know which one was meant; the XDG base directory spec
# calls a relative value invalid too. An empty value is not relative, it
# means unset, and falls through to the default below.
canopy_require_absolute() {
  case "$2" in
    '' | /*) ;;
    *) canopy_die "$1 must be an absolute path, got: $2" ;;
  esac
}

canopy_paths() {
  canopy_require_absolute XDG_CONFIG_HOME "${XDG_CONFIG_HOME-}"
  canopy_require_absolute XDG_STATE_HOME "${XDG_STATE_HOME-}"
  canopy_require_absolute XDG_RUNTIME_DIR "${XDG_RUNTIME_DIR-}"
  : "${CANOPY_STORE:=$(cd "$(dirname "$0")/.." && pwd)}"
  : "${CANOPY_CONFIG:=${XDG_CONFIG_HOME:-$HOME/.config}/canopy}"
  : "${CANOPY_STATE:=${XDG_STATE_HOME:-$HOME/.local/state}/canopy}"
  # XDG_RUNTIME_DIR is per-user by spec, so a directory under it can carry
  # canopy's plain name. /tmp is not: it is shared by every account on the
  # machine, and a plain /tmp/canopy belonged to whichever user created it
  # first, mode 755. Every other user's `canopy doctor` then could not
  # write its scratch file there, reported "entry point loads from a bare
  # environment: FAILED" against a perfectly healthy install, and exited
  # 2. The uid in the fallback path is what keeps one account's runtime
  # directory out of another's way.
  if [ -n "${XDG_RUNTIME_DIR-}" ]; then
    : "${CANOPY_RUNTIME:=$XDG_RUNTIME_DIR/canopy}"
  else
    : "${CANOPY_RUNTIME:=/tmp/canopy-$(id -u)}"
  fi
  canopy_require_absolute CANOPY_STORE "$CANOPY_STORE"
  canopy_require_absolute CANOPY_CONFIG "$CANOPY_CONFIG"
  canopy_require_absolute CANOPY_STATE "$CANOPY_STATE"
  canopy_require_absolute CANOPY_RUNTIME "$CANOPY_RUNTIME"
  export CANOPY_STORE CANOPY_CONFIG CANOPY_STATE CANOPY_RUNTIME
}

canopy_have() { command -v "$1" >/dev/null 2>&1; }

# canopy_stat_field <gnu-format> <bsd-format> <path>
# One stat field for <path>, printed without a trailing newline, without
# following a symlink (neither stat dereferences by default). GNU and
# BusyBox stat take -c and reject -f with a format operand; BSD stat is the
# other way round, so the two are tried in that order and the first one
# that both succeeds and prints something wins. Returns non-zero when
# neither does, which every caller treats as "refuse", never as "fine".
canopy_stat_field() {
  if canopy_stat_out="$(stat -c "$1" "$3" 2>/dev/null)" && [ -n "$canopy_stat_out" ]; then
    printf '%s' "$canopy_stat_out"
    return 0
  fi
  if canopy_stat_out="$(stat -f "$2" "$3" 2>/dev/null)" && [ -n "$canopy_stat_out" ]; then
    printf '%s' "$canopy_stat_out"
    return 0
  fi
  return 1
}

# canopy_runtime_ensure
# Makes $CANOPY_RUNTIME usable, or refuses to use it at all.
#
# The fallback runtime path is /tmp/canopy-<uid>, and /tmp is shared and
# world-writable: any local account can work out that path and create it
# first. Whoever creates a directory owns it and sets its mode, so an
# attacker who wins the race owns the directory canopy then writes its
# scratch files into, and can read them, replace them, or point their
# contents somewhere else through a symlink planted inside.
#
# Two halves, and both are needed. Creating it with mode 0700 closes the
# window when canopy gets there first. Refusing a directory that is not
# ours, or that anyone else can write to, closes the window when it does
# not: mkdir -p is silent about a path that already exists, so without the
# check canopy would simply adopt whatever it found.
#
# Called at the point of use rather than from canopy_paths: resolving a
# path should not create a directory, and a command that never writes a
# runtime file should not make one appear.
canopy_runtime_ensure() {
  if [ ! -e "$CANOPY_RUNTIME" ] && [ ! -L "$CANOPY_RUNTIME" ]; then
    (
      umask 077
      mkdir -p "$CANOPY_RUNTIME"
    ) || canopy_die "could not create the runtime directory $CANOPY_RUNTIME"
  fi

  # Checked after the mkdir, never instead of it: mkdir -p succeeds
  # silently on a path that appeared between the test above and the call.
  if [ -L "$CANOPY_RUNTIME" ]; then
    canopy_die "runtime directory $CANOPY_RUNTIME is a symlink, refusing to use it"
  fi
  if [ ! -d "$CANOPY_RUNTIME" ]; then
    canopy_die "runtime directory $CANOPY_RUNTIME exists and is not a directory, refusing to use it"
  fi

  canopy_runtime_uid="$(canopy_stat_field '%u' '%u' "$CANOPY_RUNTIME")" ||
    canopy_die "cannot read the owner of the runtime directory $CANOPY_RUNTIME, refusing to use it"
  if [ "$canopy_runtime_uid" != "$(id -u)" ]; then
    canopy_die "runtime directory $CANOPY_RUNTIME is owned by uid $canopy_runtime_uid, not by you (uid $(id -u)); refusing to use it"
  fi

  canopy_runtime_mode="$(canopy_stat_field '%a' '%Lp' "$CANOPY_RUNTIME")" ||
    canopy_die "cannot read the permissions of the runtime directory $CANOPY_RUNTIME, refusing to use it"
  # A leading 0 makes the shell read the value as the octal it already is.
  if [ $((0$canopy_runtime_mode & 022)) -ne 0 ]; then
    canopy_die "runtime directory $CANOPY_RUNTIME is writable by group or other (mode $canopy_runtime_mode), refusing to use it"
  fi
}

# canopy_sha256 [file]
# With a file argument, hashes that file; with none, hashes stdin. The
# stdin form is why the argument is forwarded as "$@" rather than "$1":
# both tools already read stdin when given no operand, so one spelling
# covers both and neither caller needs to know which tool is installed.
canopy_sha256() {
  if canopy_have sha256sum; then
    sha256sum "$@" | cut -d' ' -f1
  elif canopy_have shasum; then
    shasum -a 256 "$@" | cut -d' ' -f1
  else canopy_die "no sha256 tool found (need sha256sum or shasum)"; fi
}

# canopy_file_state <path>
# The state canopy records for a path, and compares a path against later:
# the literal string "absent", "symlink:<target>", or the sha256 of the
# file's contents.
#
# A symlink is never followed here. The artifact at a symlinked path is the
# link itself, and the bytes at the other end belong to whoever put them
# there, which is the whole reason chezmoi, stow and bare dotfiles repos
# produce one. Hashing through the link recorded the target's bytes under
# the link's path, so canopy could not tell a replaced link from a
# rewritten target, and restore could not put the link back.
canopy_file_state() {
  if [ -L "$1" ]; then
    printf 'symlink:%s' "$(readlink "$1")"
  elif [ -e "$1" ]; then
    canopy_sha256 "$1"
  else
    printf 'absent'
  fi
}

# canopy_entry_point
# The tmux entry point on this machine, by tmux's own precedence:
# ~/.tmux.conf wins when it exists, else the XDG path. Prints nothing when
# neither exists, which is what "canopy is not installed" looks like.
# Every check of a shipped artifact goes through this path, because it is
# the one a user's tmux goes through.
canopy_entry_point() {
  if [ -f "$HOME/.tmux.conf" ]; then
    printf '%s\n' "$HOME/.tmux.conf"
  elif [ -f "${XDG_CONFIG_HOME:-$HOME/.config}/tmux/tmux.conf" ]; then
    printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/tmux/tmux.conf"
  fi
}

canopy_die() {
  printf 'canopy: %s\n' "$1" >&2
  exit 1
}
