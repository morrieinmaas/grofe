# shellcheck shell=sh
# shellcheck disable=SC2154  # CANOPY_STORE is exported by lib/env.sh's canopy_paths
# shellcheck disable=SC3043  # `local` is explicitly permitted (plan's Global Constraints): universally supported by dash/ash/bash, used here so callers never see this file's internals

# The vendored plugin store.
#
# Two files describe it, and they answer different questions:
#
#   plugins/VERSIONS   <name> <upstream-url> <commit>
#     Where each tree came from. This is the provenance record, and the
#     input to plugins/vendor.sh when a tree has to be produced again.
#
#   plugins/CHECKSUMS  <name> <digest>
#     What each tree looked like when it was vendored. This is what lets
#     doctor say "modified" rather than only "present", and it is a
#     separate file deliberately: VERSIONS is a three-column contract that
#     other tasks in this milestone read, and widening it to carry a
#     fourth, generated column is how a format drifts away from the thing
#     that documents it.
#
#   plugins/LOCAL      <name> <path>
#     Files written into a vendored tree after vendoring, by canopy install
#     or by the plugin itself on load. The digest leaves them out, so a
#     plugin that has run is still a tree that matches.
#
#   plugins/ARTIFACTS  <name> <platform> <url> <sha256> <member> <dest> <sha256>
#     Release binaries a plugin would otherwise fetch from the network when
#     it loads or runs. canopy install downloads each one for this machine,
#     checks it, and puts it where the plugin looks; doctor checks it again.
#
# Neither VERSIONS nor CHECKSUMS is consulted at tmux render time.

# canopy_plugin_names
# Every plugin named in VERSIONS, one per line, in file order.
canopy_plugin_names() {
  local file
  file="$CANOPY_STORE/plugins/VERSIONS"
  [ -f "$file" ] || return 1
  awk 'NF > 0 && substr($1, 1, 1) != "#" { print $1 }' "$file"
}

# canopy_plugin_pin <name>
# Prints "<url> <commit>" for one plugin, or returns 1 if it is not pinned.
canopy_plugin_pin() {
  local file
  file="$CANOPY_STORE/plugins/VERSIONS"
  [ -f "$file" ] || return 1
  awk -v n="$1" '
    NF > 0 && substr($1, 1, 1) != "#" && $1 == n { print $2, $3; found = 1; exit }
    END { exit found ? 0 : 1 }
  ' "$file"
}

# canopy_plugin_recorded_digest <name>
# The digest CHECKSUMS holds for one plugin, or 1 if it holds none.
canopy_plugin_recorded_digest() {
  local file
  file="$CANOPY_STORE/plugins/CHECKSUMS"
  [ -f "$file" ] || return 1
  awk -v n="$1" '
    NF > 0 && substr($1, 1, 1) != "#" && $1 == n { print $2; found = 1; exit }
    END { exit found ? 0 : 1 }
  ' "$file"
}

# canopy_plugin_local <name>
# The paths plugins/LOCAL lists for one plugin, one per line, relative to
# its tree.
canopy_plugin_local() {
  local file
  file="$CANOPY_STORE/plugins/LOCAL"
  [ -f "$file" ] || return 0
  awk -v n="$1" 'NF > 0 && substr($1, 1, 1) != "#" && $1 == n { print $2 }' "$file"
}

# canopy_plugin_local_copies
# The plugins/LOCAL rows with a third column, the file in the same tree
# that the plugin copies to make it, as "<name> <path> <source>".
canopy_plugin_local_copies() {
  local file
  file="$CANOPY_STORE/plugins/LOCAL"
  [ -f "$file" ] || return 0
  awk 'NF >= 3 && substr($1, 1, 1) != "#" { print $1, $2, $3 }' "$file"
}

# canopy_plugin_digest <dir> [<name>]
# One sha256 over the whole tree: every regular file, in C-collated path
# order, contributing its own hash, its path, and whether it is
# executable. The executable bit is in there because a vendored .tmux
# entry point that lost its +x is a tree that no longer works, and a
# digest that only covered content would call that tree unmodified.
#
# The files plugins/LOCAL lists for <name> (by default the directory's own
# name) are left out: they are written after vendoring, by canopy install
# or by the plugin, and are not what upstream shipped.
#
# Nothing about the enclosing directory's name is hashed, so a tree stays
# comparable after it is copied somewhere else, which is what lets a test
# assert on a copy and doctor assert on the store.
canopy_plugin_digest() {
  local dir name skip
  dir="$1"
  name="${2:-${1##*/}}"
  [ -d "$dir" ] || return 1
  skip="$(canopy_plugin_local "$name")"
  (
    cd "$dir" || exit 1
    find . \( -type f -o -type l \) | LC_ALL=C sort | while IFS= read -r f; do
      if [ -n "$skip" ] && printf '%s\n' "$skip" | grep -qxF "${f#./}"; then
        continue
      fi
      # A symlink counts by where it points, so retargeting one is a
      # modified tree even when the target's content is unchanged.
      if [ -L "$f" ]; then
        printf 'l %s %s\n' "$(readlink "$f" | canopy_sha256)" "${f#./}"
        continue
      fi
      # The mode flag goes through %s rather than being written into the
      # format string: a format string of "- " is a leading dash, and
      # bash's printf reads that as an option and fails.
      if [ -x "$f" ]; then
        mode=x
      else
        mode=-
      fi
      printf '%s %s %s\n' "$mode" "$(canopy_sha256 "$f")" "${f#./}"
    done
  ) | canopy_sha256
}

# --- release artifacts ---------------------------------------------------------
#
# Two plugins fetch release binaries from GitHub on their own, which canopy
# never lets happen at load or use time: opensessions downloads its sidebar
# binaries when its entry point runs, and tmux-fzf-url pipes an installer
# into bash the first time its key is pressed. Both look for the binaries
# in their own tree first, so canopy install puts them there, from
# plugins/ARTIFACTS, each archive and each file checked against the sha256
# recorded with the pins.

# canopy_platform
# This machine as ARTIFACTS names it: darwin-arm64, darwin-x86_64,
# linux-arm64 or linux-x86_64. Prints nothing elsewhere.
canopy_platform() {
  local os arch
  case "$(uname -s)" in
    Darwin) os=darwin ;;
    Linux) os=linux ;;
    *) return 1 ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch=arm64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) return 1 ;;
  esac
  printf '%s-%s\n' "$os" "$arch"
}

# The ARTIFACTS file. CANOPY_ARTIFACTS names another only for the test
# suite, whose harness sets it explicitly (test/helper.bash) so that no
# test provisions from the network into this checkout. Whatever file is
# read, a row is held to the store's own VERSIONS and LOCAL.
canopy_artifacts_file() {
  printf '%s\n' "${CANOPY_ARTIFACTS:-$CANOPY_STORE/plugins/ARTIFACTS}"
}

# canopy_artifact_row_ok <name> <dest>
# True when a row may write <dest> into plugins/<name>: <name> is pinned in
# VERSIONS, and <dest> is a plain relative path (only [A-Za-z0-9._/-], no
# leading /, no .. segment) that plugins/LOCAL lists for it.
canopy_artifact_row_ok() {
  canopy_plugin_pin "$1" >/dev/null 2>&1 || return 1
  case "$2" in
    '' | /* | *[!A-Za-z0-9._/-]*) return 1 ;;
    .. | ../* | */.. | */../*) return 1 ;;
    *) ;;
  esac
  canopy_plugin_local "$1" | grep -qxF "$2"
}

# canopy_plugin_artifacts [<platform>]
# The ARTIFACTS rows for this machine (or <platform>), plus every `any`
# row, as "<ok|bad> <name> <url> <sha256> <member> <dest> <sha256>": bad
# is a row canopy_artifact_row_ok refuses, which nothing acts on.
canopy_plugin_artifacts() {
  local file platform name url sum member dest want verdict
  file="$(canopy_artifacts_file)"
  [ -f "$file" ] || return 0
  platform="${1:-$(canopy_platform || true)}"
  awk -v p="$platform" '
    NF >= 7 && substr($1, 1, 1) != "#" && ($2 == p || $2 == "any") { print $1, $3, $4, $5, $6, $7 }
  ' "$file" | while read -r name url sum member dest want; do
    verdict=bad
    canopy_artifact_row_ok "$name" "$dest" && verdict=ok
    printf '%s %s %s %s %s %s %s\n' "$verdict" "$name" "$url" "$sum" "$member" "$dest" "$want"
  done
}

# canopy_plugin_artifacts_ok <name>
# True when every ARTIFACTS row for <name> on this machine is a row
# canopy_artifact_row_ok accepts and its file is in place with the
# recorded sha256. A plugin for which this fails is one that would fetch
# what is missing from the network, so the loader does not run it.
#
# Returns 2 when <name> has rows for other platforms and none for this one:
# nothing can be provided here, and the plugin would download its own.
canopy_plugin_artifacts_ok() {
  local verdict name url sum member dest want path file
  file="$(canopy_artifacts_file)"
  if [ -f "$file" ] && awk -v n="$1" -v p="$(canopy_platform || true)" '
    NF >= 7 && substr($1, 1, 1) != "#" && $1 == n && $2 != "any" { other = 1; if ($2 == p) here = 1 }
    END { exit !(other && !here) }
  ' "$file"; then
    return 2
  fi
  canopy_plugin_artifacts | while read -r verdict name url sum member dest want; do
    [ "$name" = "$1" ] || continue
    [ "$verdict" = ok ] || exit 1
    path="$CANOPY_STORE/plugins/$name/$dest"
    [ -f "$path" ] && [ ! -L "$path" ] || exit 1
    [ "$(canopy_sha256 "$path")" = "$want" ] || exit 1
  done
}

# canopy_fetch <url> <file>
# https only, redirects included.
canopy_fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto =https --proto-redir =https "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    return 1
  fi
}

# canopy_provision_artifacts
# Puts every artifact for this machine in place. A file already there with
# the recorded digest is left alone, so a second run fetches nothing. An
# archive or a file whose digest does not match is refused and nothing is
# written for it. Prints one line per artifact it could not provide and
# returns 1 if there was one; the rest are still provided.
#
# A url of the form text:<value> is a file holding <value> and a newline,
# for a plugin that also reads a version stamp next to its binaries.
canopy_provision_artifacts() {
  local work rows status verdict name url sum member dest want path got archive
  rows="$(canopy_plugin_artifacts)"
  [ -n "$rows" ] || return 0
  work="$(mktemp -d "${TMPDIR:-/tmp}/canopy-artifacts.XXXXXX")" || return 1
  status=0
  while read -r verdict name url sum member dest want; do
    if [ "$verdict" != ok ]; then
      printf 'canopy: %s: refused an ARTIFACTS row for %s: not a pinned plugin, or not a plain path plugins/LOCAL lists\n' "$name" "$dest"
      status=1
      continue
    fi
    path="$CANOPY_STORE/plugins/$name/$dest"
    if [ -f "$path" ] && [ "$(canopy_sha256 "$path")" = "$want" ]; then
      continue
    fi
    rm -f "$work/file"
    case "$url" in
      text:*)
        printf '%s\n' "${url#text:}" >"$work/file"
        ;;
      *)
        archive="$work/$(printf '%s' "$url" | canopy_sha256)"
        if [ ! -f "$archive" ]; then
          if ! canopy_fetch "$url" "$archive.part"; then
            printf 'canopy: %s: could not download %s\n' "$name" "$url"
            rm -f "$archive.part"
            status=1
            continue
          fi
          mv "$archive.part" "$archive"
        fi
        got="$(canopy_sha256 "$archive")"
        if [ "$got" != "$sum" ]; then
          printf 'canopy: %s: %s has sha256 %s, not the recorded %s; refused\n' "$name" "$url" "$got" "$sum"
          status=1
          continue
        fi
        rm -rf "$work/x"
        mkdir -p "$work/x"
        if ! tar -xzf "$archive" -C "$work/x" -- "$member" 2>/dev/null || [ ! -f "$work/x/$member" ]; then
          printf 'canopy: %s: %s holds no %s\n' "$name" "$url" "$member"
          status=1
          continue
        fi
        mv "$work/x/$member" "$work/file"
        ;;
    esac
    got="$(canopy_sha256 "$work/file")"
    if [ "$got" != "$want" ]; then
      printf 'canopy: %s: %s has sha256 %s, not the recorded %s; refused\n' "$name" "$dest" "$got" "$want"
      status=1
      continue
    fi
    mkdir -p "${path%/*}"
    case "$url" in
      text:*) chmod 644 "$work/file" ;;
      *) chmod 755 "$work/file" ;;
    esac
    mv "$work/file" "$path"
  done <<ROWS
$rows
ROWS
  rm -rf "$work"
  return "$status"
}

# --- the loader ----------------------------------------------------------------
#
# What TPM's tpm script does for a plugin, without the clone, the update,
# or anything else that reaches the network. canopy's tmux.conf (after
# user.conf, where TPM ran) and the grofe plugin fragment call
# canopy_load_plugins once, with the fixed list in load order, at server
# start and again on every reload.
#
# It runs as the body of a run-shell, and talks only to the server that
# ran it: the socket in $TMUX, named with -S, never whatever server a bare
# `tmux` would find. The plugins' own scripts call `tmux` the same way, so
# nothing here is tmux specific: grofe answering to the tmux name runs it
# unchanged.

# Where a load pass records the plugins it could not load.
canopy_plugin_log() {
  printf '%s/plugins.log\n' "${CANOPY_STATE:-${XDG_STATE_HOME:-$HOME/.local/state}/canopy}"
}

canopy_plugin_note() {
  printf '%s %s: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2" \
    >>"$(canopy_plugin_log)" 2>/dev/null || true
}

# canopy_shell_quote <word>
# <word> as one single-quoted shell word, a ' inside it included.
canopy_shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# canopy_load_plugin <name> [<entry-glob>]
#
# TPM's convention for the entry point: every top-level *.tmux file in the
# plugin's root, run in glob order. Each runs through `run-shell` on this
# server, with its output discarded as TPM discards it. An entry point
# whose first line names bash needs bash; without it, or with no
# executable entry point, the plugin is dormant: nothing on the status
# line, one log line. So is a plugin whose entry point is a symlink, and one
# whose release binaries (plugins/ARTIFACTS) are not all in place with their
# recorded sha256: it would fetch them itself, and canopy never lets a
# plugin reach the network. A plugin whose entry point fails is logged too, and
# does not stop the plugins after it.
canopy_load_plugin() {
  local name glob dir entry found shebang artifacts
  name="$1"
  glob="${2:-*.tmux}"
  dir="$CANOPY_STORE/plugins/$name"
  found=0
  # The glob is the point here: it is expanded, never quoted.
  # shellcheck disable=SC2086
  for entry in "$dir"/$glob; do
    if [ -L "$entry" ]; then
      found=1
      canopy_plugin_note "$name" "dormant, entry point ${entry##*/} is a symlink, refused"
      continue
    fi
    [ -f "$entry" ] && [ -x "$entry" ] || continue
    found=1
    artifacts=0
    canopy_plugin_artifacts_ok "$name" || artifacts=$?
    if [ "$artifacts" -eq 2 ]; then
      canopy_plugin_note "$name" "dormant, plugins/ARTIFACTS has no release binaries for this platform"
      return 0
    elif [ "$artifacts" -ne 0 ]; then
      canopy_plugin_note "$name" "dormant, a release binary from plugins/ARTIFACTS is missing or fails its sha256 (canopy install provides it)"
      return 0
    fi
    IFS= read -r shebang <"$entry" || shebang=""
    case "$shebang" in
      '#!'*bash*)
        if ! command -v bash >/dev/null 2>&1; then
          canopy_plugin_note "$name" "dormant, bash not found"
          continue
        fi
        ;;
      *) ;;
    esac
    if ! tmux -S "$canopy_load_socket" run-shell "$(canopy_shell_quote "$entry")" >/dev/null 2>&1; then
      canopy_plugin_note "$name" "entry point ${entry##*/} failed"
    fi
  done
  if [ "$found" -eq 0 ]; then
    canopy_plugin_note "$name" "dormant, no executable $glob in $dir"
  fi
  return 0
}

# canopy_load_plugins <name>...
#
# One load pass, in the order given. The log is started afresh, so it
# describes the last load and nothing older.
#
# Outside a server ($TMUX unset) it loads nothing and says so in the log.
# A server whose @canopy_plugins is off, or a run with CANOPY_PLUGINS=off
# in its environment, loads none and leaves the log alone. canopy's own
# scratch servers set the option (lib/tmux.sh), and the test suite sets the
# variable: they exist to check that a config loads, and a plugin's load
# writes outside the server, into $HOME and /tmp.
canopy_load_plugins() {
  local log name
  log="$(canopy_plugin_log)"
  if [ -z "${TMUX:-}" ]; then
    mkdir -p "${log%/*}" 2>/dev/null
    canopy_plugin_note loader "loaded nothing, TMUX is unset so there is no server to load into"
    return 0
  fi
  canopy_load_socket="${TMUX%%,*}"
  if [ "${CANOPY_PLUGINS:-}" = off ] ||
    [ "$(tmux -S "$canopy_load_socket" show-option -gqv @canopy_plugins 2>/dev/null)" = off ]; then
    return 0
  fi
  mkdir -p "${log%/*}" 2>/dev/null && : >"$log" 2>/dev/null
  for name in "$@"; do
    canopy_load_plugin "$name"
  done
  return 0
}
