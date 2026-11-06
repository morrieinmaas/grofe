#!/usr/bin/env bash
# The login shell commands are zsh, expanded there.
# shellcheck disable=SC2016
# rollback.sh: TE.3's rollback drill, the in-container half. Run by
# scripts/release-gate.sh as the test user in the systemd image
# (tests/e2e/Containerfile.alias-day), after TA.1's stage 2 has applied
# athome: canopy on grofe, grofe first on PATH as `tmux`, the login unit
# (canopy-tmux.service) running a grofe server. Never run on a host.
#
# The drill is the README's rollback, step by step, each checked:
#
#   1. canopy is on grofe: a login shell's `tmux` is the grofe symlink, and
#      the login unit's server answers #{grofe_version}
#   2. the login unit stops, and the `tmux` symlink goes; grofe itself
#      stays. A login shell now finds real tmux 3.7b (/usr/local/bin/tmux)
#   3. canopy is switched back to its tmux backend: `canopy install --yes`
#      takes the tmux entry point over (athome's stub, migrated into
#      user.conf) and records a restore point
#   4. real tmux and canopy work as before: the login unit starts a real
#      tmux 3.7b server with canopy's config loaded (@continuum-restore on,
#      user.conf sourced), on tmux's own socket, and `canopy doctor` passes;
#      grofe's state directory is byte-identical to step 2's, and grofe
#      still runs by its full path
#   5. `canopy restore --all` (the server stopped first) returns the home
#      byte-identical to step 2's, outside canopy's own state tree (which
#      canopy documents as left behind: its restore points live there);
#      canopy's store is compared as git sees it (see snapshot below)
#
# Every check prints `PASS <what>` or `FAIL <what>`; the drill exits
# nonzero on any FAIL. Snapshots and logs go to ~/e2e-out/rollback.
set -uo pipefail

[ -e /run/.containerenv ] || { echo "rollback.sh: refusing to run outside a container" >&2; exit 2; }
[ "$(id -un)" = test ] || { echo "rollback.sh: run as the test user" >&2; exit 2; }

XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
OUT="$HOME/e2e-out/rollback"
GROFE_DIR="$HOME/.local/lib/grofe"
GROFE_STATE="$HOME/.local/state/grofe"
STORE="$HOME/.local/share/canopy"
CANOPY="$STORE/bin/canopy"
REAL_TMUX=/usr/local/bin/tmux
mkdir -p "$OUT"
fails=0

pass() { printf 'PASS %s\n' "$*"; }
fail() {
  printf 'FAIL %s\n' "$*"
  fails=$((fails + 1))
}
expect() {
  if [ "$1" = "$2" ]; then pass "$3 is '$1'"; else fail "$3 is '$1', not '$2'"; fi
}
check() {
  local what="$1"
  shift
  if "$@" >>"$OUT/checks.log" 2>&1; then pass "$what"; else fail "$what (see checks.log)"; fi
}

# login <command>: the command's last line of output in a login zsh, the
# owner's shell, which sources athome's PATH.
login() {
  zsh -lic "$1" </dev/null 2>/dev/null | tail -n 1
}

# The home as bytes: every entry's path, kind, mode and size, a symlink's
# target, and every file's sha256. Left out: this drill's own output,
# canopy's own state tree (its restore points, which canopy documents as
# left behind by a restore), ~/.cache, and what a login zsh rewrites each
# time it starts (history, completion dumps). canopy's store, a git
# checkout, is compared as git sees it: HEAD, and every tracked or
# untracked change. The files its .gitignore names are what the vendored
# plugins generate when a tmux server loads them (tmux-which-key's menu
# files among them), not anything canopy install writes.
snapshot() {
  local prune=(\( -path ./e2e-out -o -path ./.local/state/canopy -o -path ./.cache
    -o -path ./.local/share/canopy -o -name '.zcompdump*' -o -name .zsh_history \) -prune -o)
  (cd "$HOME" && find . "${prune[@]}" -printf '%p %y %m %s %l\n' | LC_ALL=C sort &&
    find . "${prune[@]}" -type f -print0 | xargs -0 -r sha256sum | LC_ALL=C sort -k2)
  printf '== canopy store\n'
  git -C "$STORE" rev-parse HEAD
  git -C "$STORE" status --porcelain
}

tree_of() {
  (cd "$1" 2>/dev/null && find . -type f -exec sha256sum {} + | LC_ALL=C sort -k2) || echo "(absent)"
}

# --- 1. canopy on grofe -------------------------------------------------------
t="$(login 'print -r -- $(command -v tmux)')"
if [ "$t" = "$GROFE_DIR/bin/tmux" ] && [ "$(readlink -f "$t")" = "$GROFE_DIR/grofe" ]; then
  pass "before: a login shell's tmux is grofe ($t)"
else
  fail "before: a login shell's tmux is '$t', not $GROFE_DIR/bin/tmux"
fi
v="$(PATH="$GROFE_DIR/bin:$PATH" tmux display -p '#{grofe_version}' 2>&1)"
if [ -n "$v" ] && systemctl --user is-active --quiet canopy-tmux.service; then
  pass "before: canopy-tmux.service runs a grofe server (grofe_version=$v)"
else
  fail "before: no grofe server from canopy-tmux.service ('$v')"
fi

# --- 2. the login unit stops, the symlink goes --------------------------------
check "the login unit stops" systemctl --user stop canopy-tmux.service
check "no grofe server is left" bash -c "! '$GROFE_DIR/bin/grofe' list-sessions"
rm "$GROFE_DIR/bin/tmux"
t="$(login 'print -r -- $(command -v tmux)')"
if [ "$t" = "$REAL_TMUX" ] && ! [ "$t" -ef "$GROFE_DIR/grofe" ] && ldd "$t" | grep -q libevent; then
  pass "after the symlink goes, a login shell's tmux is real tmux ($t)"
else
  fail "after the symlink goes, a login shell's tmux is '$t', not $REAL_TMUX"
fi
v="$(login 'tmux -V')"
expect "$v" "tmux 3.7b" "a login shell's tmux -V"
check "grofe still runs by its full path" "$GROFE_DIR/bin/grofe" -V
snapshot >"$OUT/home.before"
tree_of "$GROFE_STATE" >"$OUT/grofe-state.before"
check "no real tmux server runs yet" bash -c "! '$REAL_TMUX' list-sessions"

# --- 3. canopy back on its tmux backend ---------------------------------------
if (cd "$HOME" && PATH="$(dirname "$CANOPY"):$PATH" "$CANOPY" install --yes) >"$OUT/canopy-install.log" 2>&1; then
  pass "canopy install --yes, on real tmux"
else
  fail "canopy install --yes exited nonzero (see canopy-install.log)"
fi
check "the tmux entry point is canopy's stub" grep -qF '# canopy:entry-point' "$HOME/.config/tmux/tmux.conf"
check "canopy recorded a restore point" bash -c "'$CANOPY' restore --list | grep -q install"

# --- 4. real tmux and canopy work as before -----------------------------------
check "the login unit starts again" systemctl --user start canopy-tmux.service
v="$(tmux display -p '#{version} [#{grofe_version}]' 2>&1)"
expect "$v" "3.7b []" "the login unit's server is real tmux: #{version} [#{grofe_version}]"
check "the server is on tmux's own socket" test -S "/tmp/tmux-$(id -u)/default"
v="$(tmux show -gv @continuum-restore 2>&1)"
expect "$v" on "canopy's config is loaded: @continuum-restore"
check "canopy doctor passes" bash -c "cd ~ && PATH='$(dirname "$CANOPY")':\$PATH '$CANOPY' doctor"
tree_of "$GROFE_STATE" >"$OUT/grofe-state.after"
if cmp -s "$OUT/grofe-state.before" "$OUT/grofe-state.after"; then
  pass "grofe's state directory is untouched by real tmux"
else
  fail "grofe's state directory changed under real tmux"
  diff "$OUT/grofe-state.before" "$OUT/grofe-state.after"
fi

# --- 5. canopy restore --all ----------------------------------------------------
check "the login unit stops" systemctl --user stop canopy-tmux.service
if (cd "$HOME" && "$CANOPY" restore --all) >"$OUT/canopy-restore.log" 2>&1; then
  pass "canopy restore --all: $(tail -n 1 "$OUT/canopy-restore.log")"
else
  fail "canopy restore --all exited nonzero (see canopy-restore.log)"
fi
snapshot >"$OUT/home.after"
if cmp -s "$OUT/home.before" "$OUT/home.after"; then
  pass "the home is byte-identical to before canopy install ($(wc -l <"$OUT/home.after") entries)"
else
  fail "the home differs from before canopy install:"
  diff "$OUT/home.before" "$OUT/home.after"
fi

echo "rollback: $fails failure(s)"
[ "$fails" -eq 0 ]
