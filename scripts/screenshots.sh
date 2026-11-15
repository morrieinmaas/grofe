#!/usr/bin/env bash
# Render docs/tapes/grofe.tape with vhs, once in a dark and once in a light
# theme, to <out>/grofe-dark.png and <out>/grofe-light.png. Needs vhs (and
# its ttyd and ffmpeg) and a built grofe: GROFE_BIN, or target/release/grofe.
# The grofe it drives runs on a private socket with its own home, config
# and state, and is stopped afterwards.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=$(mkdir -p "${1:-$root/target/site/img}" && cd "${1:-$root/target/site/img}" && pwd)
bin=${GROFE_BIN:-$root/target/release/grofe}
font=${GROFE_SHOT_FONT:-JetBrainsMono Nerd Font}
[ -x "$bin" ] || { echo "screenshots: no grofe at $bin; run cargo build --release" >&2; exit 1; }

work=$(mktemp -d)
export GROFE_DEMO="$work" GROFE_REPO="$root" GROFE_SOCK="$work/sock"
cleanup() {
    "$bin" -S "$GROFE_SOCK" kill-server 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
unset TMUX TMUX_PANE GROFE GROFE_PANE
mkdir -p "$work/home" "$work/config/grofe" "$work/state" "$work/bin"
cp "$root/docs/tapes/config.toml" "$work/config/grofe/config.toml"
ln -s "$bin" "$work/bin/grofe"
export PATH="$work/bin:$PATH"

cd "$root"
for theme in dark light; do
    case $theme in
        dark) name="Catppuccin Mocha" ;;
        light) name="Catppuccin Latte" ;;
    esac
    sed -e "s|@THEME@|$name|" -e "s|@FONT@|$font|" \
        -e "s|@GIF@|$work/$theme.gif|" -e "s|@PNG@|$out/grofe-$theme.png|" \
        docs/tapes/grofe.tape > "$work/$theme.tape"
    vhs "$work/$theme.tape"
    "$bin" -S "$GROFE_SOCK" kill-server 2>/dev/null || true
    test -s "$out/grofe-$theme.png"
done
echo "screenshots: $out/grofe-dark.png $out/grofe-light.png"
