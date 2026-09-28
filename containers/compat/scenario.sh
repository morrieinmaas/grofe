#!/usr/bin/env bash
# Runs inside the grofe-compat-tc0 image (containers/compat/Containerfile).
#
# Lays the owner's setup out under $HOME the way chezmoi and canopy do on a
# real machine, from the scrubbed read-only copy at /src, installs the argv
# recorder first on PATH, starts a tmux 3.7b server through it (so the
# server's jobs resolve `tmux` through the recorder too), drives the setup
# (drive.py), and writes to /out:
#
#   argv.log            the recorder's log
#   list-keys.txt       every binding the server holds at the end
#   hooks.txt           every global hook
#   list-commands.txt   tmux 3.7b's command table
#   scenario.log        what the driver did
set -euo pipefail

# Only ever inside the recording container, never next to a live tmux: this
# script starts, drives and kills a tmux server.
if [ ! -e /run/.containerenv ] && [ ! -e /.dockerenv ]; then
  echo "scenario: refusing to run outside a container" >&2
  exit 1
fi
if [ -n "${TMUX:-}" ]; then
  echo "scenario: refusing to run inside tmux (TMUX is set)" >&2
  exit 1
fi
export TMUX_TMPDIR="$HOME/.tmux-tmp"
mkdir -p "$TMUX_TMPDIR"

real=/opt/tmux/bin/tmux
[ "$("$real" -V)" = "tmux 3.7b" ] || { echo "scenario: need tmux 3.7b, have $("$real" -V)" >&2; exit 1; }
out=/out
: >"$out/argv.log"

# --- the owner's layout ----------------------------------------------------
store="$HOME/.local/share/canopy"
mkdir -p "$HOME/.local/share" "$HOME/.local/state/canopy" "$HOME/.config/tmux" \
  "$HOME/.config/canopy" "$HOME/.config/themes" "$HOME/.local/bin" "$HOME/bin"
cp -R /src/canopy "$store"

render() { # chezmoi's two constructs these templates use; zsh is absent here
  awk -v home="$HOME" '
    /\{\{ *with lookPath/ { skip = 1; next }
    skip && /\{\{ *end/ { skip = 0; next }
    skip { next }
    { gsub(/\{\{ *\.chezmoi\.homeDir *\}\}/, home); print }
  ' "$1"
}
a=/src/athome/home
render "$a/dot_config/tmux/tmux.conf.tmpl" >"$HOME/.config/tmux/tmux.conf"
render "$a/dot_config/canopy/user.conf.tmpl" >"$HOME/.config/canopy/user.conf"
for f in "$a"/dot_config/tmux/executable_*; do
  install -m 755 "$f" "$HOME/.config/tmux/$(basename "$f" | cut -c12-)"
done
for f in "$a"/dot_config/themes/*; do
  name=$(basename "$f")
  install -m 755 "$f" "$HOME/.config/themes/${name#executable_}"
done
for f in tsess cockpit theme; do
  install -m 755 "$a/dot_local/bin/executable_$f" "$HOME/.local/bin/$f"
done
cp -R /src/plugins "$HOME/.config/tmux/plugins"
cp -R /opt/opensessions-bin "$HOME/.config/tmux/plugins/opensessions/bin"
cp /src/compat/smug.yaml "$HOME/smug.yaml"

# --- the recorder, first on PATH; no real tmux anywhere on PATH -------------
bash /src/scripts/tmux-argv-recorder.sh install "$HOME/bin" "$real" "$out/argv.log"
export PATH="$HOME/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin"
[ "$(command -v tmux)" = "$HOME/bin/tmux" ] || { echo "scenario: tmux does not resolve to the recorder" >&2; exit 1; }

# canopy's generated layers, as run_onchange does on the owner's machine
export CANOPY_STORE="$store" CANOPY_STATE="$HOME/.local/state/canopy" CANOPY_CONFIG="$HOME/.config/canopy"
"$store/bin/canopy" caps >/dev/null
"$store/bin/canopy" keys >/dev/null
unset CANOPY_STORE CANOPY_STATE CANOPY_CONFIG

# --- drive it ---------------------------------------------------------------
python3 /src/compat/drive.py --out "$out" --real "$real" 2>&1 | tee "$out/scenario.log"

# --- the server's final tables (through the real tmux: not recorded) --------
if "$real" has-session 2>/dev/null; then
  "$real" list-keys >"$out/list-keys.txt"
  "$real" show-hooks -g >"$out/hooks.txt"
  "$real" list-commands >"$out/list-commands.txt"
  "$real" kill-server
fi
echo "scenario: $(wc -l <"$out/argv.log") recorded calls"
