#!/usr/bin/env bash
# record-container-corpus.sh: record the owner's tmux argv inside a container
# and write tests/compat/corpus/container-*.argv (plan M3.6, task TC.0).
#
# The owner's live tmux is never touched (plan addendum, owner constraint of
# 2026-09-28). This script only READS the owner's sources, copies what the
# container needs into a staging directory, refuses to go on if the copy
# holds the home path, hostname or git email of this machine, and runs
# containers/compat/Containerfile's image with the copy mounted read-only.
# Inside, tmux 3.7b runs from source with the argv recorder first on PATH
# (containers/compat/scenario.sh).
#
# Environment:
#   GROFE_COMPAT_CANOPY   canopy checkout (required)
#   GROFE_COMPAT_ATHOME   athome checkout (required)
#   GROFE_COMPAT_PLUGINS  TPM plugin directory (default ~/.config/tmux/plugins)
#   TC0_KEEP=1            keep the staging and output directories
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
canopy=${GROFE_COMPAT_CANOPY:?set GROFE_COMPAT_CANOPY to the canopy checkout}
athome=${GROFE_COMPAT_ATHOME:?set GROFE_COMPAT_ATHOME to the athome checkout}
plugins=${GROFE_COMPAT_PLUGINS:-$HOME/.config/tmux/plugins}
image=grofe-compat-tc0

# the podman machine shares the home directory, not $TMPDIR, so stage there
work="$repo/target/compat-record"
rm -rf "$work"
mkdir -p "$work/src/canopy" "$work/src/athome" "$work/src/plugins" "$work/src/scripts" "$work/src/compat" "$work/out"
[ "${TC0_KEEP:-}" = 1 ] || trap 'rm -rf "$work"' EXIT

# a git working tree's own files (tracked, plus untracked that are not
# ignored), so local build output and state stay behind
copy_tree() {
  src=$1 dst=$2
  mkdir -p "$dst"
  if git -C "$src" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    (cd "$src" && git ls-files -z -co --exclude-standard |
      while IFS= read -r -d '' f; do
        [ -f "$f" ] && printf '%s\0' "$f"
      done | tar --null -T - -cf -) | tar -xf - -C "$dst"
  else
    (cd "$src" && tar --exclude .git --exclude bin -cf - .) | tar -xf - -C "$dst"
  fi
}

copy_tree "$canopy" "$work/src/canopy"
for rel in \
  home/dot_config/tmux \
  home/dot_config/canopy/user.conf.tmpl \
  home/dot_config/themes \
  home/dot_local/bin/executable_tsess \
  home/dot_local/bin/executable_cockpit \
  home/dot_local/bin/executable_theme; do
  mkdir -p "$work/src/athome/$(dirname "$rel")"
  cp -R "$athome/$rel" "$work/src/athome/$rel"
done
for p in tpm tmux-sensible tmux-yank tmux-fzf vim-tmux-navigator tmux-sessionx \
  tmux-which-key opensessions tmux-fzf-url tmux-claude-session-manager; do
  copy_tree "$plugins/$p" "$work/src/plugins/$p"
done
rm -rf "$work/src/plugins/opensessions/bin"  # this machine's binaries; the image has Linux ones
cp "$repo/scripts/tmux-argv-recorder.sh" "$work/src/scripts/"
cp "$repo/containers/compat/scenario.sh" "$repo/containers/compat/drive.py" "$repo/containers/compat/smug.yaml" "$work/src/compat/"

# nothing of this machine goes into the container
deny=("$HOME" "$(hostname)" "$(hostname -s)")
email=$(git config user.email 2>/dev/null || true)
[ -n "$email" ] && deny+=("$email")
hits=
for d in "${deny[@]}"; do
  [ -n "$d" ] || continue
  if found=$(grep -rIlF -- "$d" "$work/src" 2>/dev/null); then
    hits+="$found"$'\n'
  fi
done
if [ -n "$hits" ]; then
  printf 'record: the staged copy holds a string of this machine (home, hostname or email); scrub these first:\n%s' "$hits" >&2
  exit 1
fi

chmod -R a+rX "$work/src"
chmod 777 "$work/out"
podman build -q -t "$image" -f "$repo/containers/compat/Containerfile" "$repo/containers/compat" >/dev/null
podman run --rm \
  -v "$work/src:/src:ro" \
  -v "$work/out:/out" \
  "$image" bash /src/compat/scenario.sh

first=$(head -1 "$work/out/list-commands.txt")
case $first in
  attach-session*) cp "$work/out/list-commands.txt" "$repo/tests/compat/corpus/tmux-list-commands.txt" ;;
  *) echo "record: list-commands.txt looks wrong: $first" >&2; exit 1 ;;
esac
uv run "$repo/scripts/corpus.py" normalize \
  --log "$work/out/argv.log" --home /home/rec \
  --list-keys "$work/out/list-keys.txt" --hooks "$work/out/hooks.txt"
