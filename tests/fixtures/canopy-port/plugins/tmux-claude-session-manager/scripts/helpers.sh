#!/usr/bin/env bash
# Shared helpers for tmux-claude-hatch.

# get_tmux_option <option-name> <default>
# Echoes the global tmux option value, or the default when unset/empty.
get_tmux_option() {
  local value
  value="$(tmux show-option -gqv "$1" 2>/dev/null)"
  if [ -n "$value" ]; then
    printf '%s' "$value"
  else
    printf '%s' "$2"
  fi
}

# session_hash <string>
# Short, stable, portable 8-char hash for deriving a session name from a path.
# Prefers md5sum (Linux), falls back to md5 (macOS) then shasum. The trailing
# newline matches the conventional `echo "$path" | md5sum` scheme, so it stays
# compatible with sessions created that way.
session_hash() {
  local out
  if command -v md5sum >/dev/null 2>&1; then
    out="$(printf '%s\n' "$1" | md5sum)"
  elif command -v md5 >/dev/null 2>&1; then
    out="$(printf '%s\n' "$1" | md5 -q)"
  else
    out="$(printf '%s\n' "$1" | shasum)"
  fi
  out="${out%% *}"
  printf '%s' "${out:0:8}"
}

# file_mtime <path>
# Epoch seconds of a file's last modification. GNU stat (Linux) is tried first,
# then BSD (macOS); each rejects the other's flag, so the fallback is unambiguous.
file_mtime() {
  stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null
}

# claude_transcript_mtime <session-id>
# Epoch seconds of the last write to that Claude session's transcript. Only the
# CLI fallback in agents.sh needs it: `claude agents --json` reports `startedAt`
# but never a last-activity time, so the transcript's mtime stands in for it. A
# rough stand-in — Claude Code also touches the transcripts of idle sessions.
#
# Found by glob so we never have to reproduce Claude's cwd -> project-slug
# encoding. The path is an internal Claude Code detail and may move; an empty
# result just renders the age column as '-'.
claude_transcript_mtime() {
  local base f
  base="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  for f in "$base"/projects/*/"$1".jsonl; do
    [ -f "$f" ] && {
      file_mtime "$f"
      return
    }
  done
}

# copy_to_clipboard <text>
# Puts <text> in a tmux paste buffer and on the system clipboard. A native tool
# is preferred; without one, `set-buffer -w` hands it to the outer terminal via
# OSC 52, which works only when `set-clipboard` is on and the terminal allows it.
copy_to_clipboard() {
  local tool
  for tool in pbcopy wl-copy 'xclip -selection clipboard' 'xsel --clipboard --input'; do
    command -v "${tool%% *}" >/dev/null 2>&1 || continue
    printf '%s' "$1" | $tool 2>/dev/null && {
      tmux set-buffer -- "$1" 2>/dev/null
      return 0
    }
  done
  tmux set-buffer -w -- "$1" 2>/dev/null
}
