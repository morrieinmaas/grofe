#!/usr/bin/env bash
# key-parity.sh: every key binding resolves on grofe as it does on real
# tmux 3.7b, copy mode included, with the owner's whole config and every
# plugin loaded (plan M3.7, TP.9a and TP.9b).
#
#   scripts/key-parity.sh [--keep <dir>] <canopy-checkout> <athome-checkout>
#
# Three servers load the owner's config, each in a scratch world of its own
# (home, XDG, TMUX_TMPDIR, TMPDIR, CANOPY_* and a copy of canopy's store,
# installed where `canopy install` puts it):
#
#   tmux          real tmux 3.7b ($KEY_PARITY_TMUX, an absolute path) on
#                 -L grofe-tp9b-tmux -f /dev/null, sourcing canopy's
#                 tmux.conf as canopy's installed stub does: its layers,
#                 athome's user.conf, then the plugin loader. canopy's
#                 conf.d/40-nav.conf binds vim-tmux-navigator's keys.
#   grofe-tmux    grofe answering to the tmux name, reading its native
#                 config: athome's config.toml, canopy's grofe/conf.d
#                 fragments, athome's user.toml, whose load-commands run
#                 the same loader.
#   grofe         the same, the server started under grofe's own name.
#
# Part 1, the tables. Each grofe's `list-keys -T <table>` is compared with
# tmux's, line for line once the world's own paths are replaced, for every
# table either side has, and tmux's copy-mode-vi table must be TM.0's
# golden. A key bound on one side only, or to another command, fails,
# unless docs/compat-coverage.md's "Key parity" table names the table and
# key (or `(all)`, every other key of the table), for that grofe server,
# with a class and a reason:
#
#   oos         out of scope in the spec: reported, excused
#   mouse       a stock mouse binding only a server with tmux identity binds
#   native      grofe binds the key in its own form, by design
#   gap         a known gap outside grofe
#
# A row of any class fails when its key matches anyway, when neither side
# binds its key, or when neither side has its table, so the table cannot go
# stale; an `(all)` row fails when no key of its table differs.
#
# Part 2, copy mode, buffers and the mouse, on a client attached on a pty:
# prefix [ PPage = ] and tmux-yank's y are pressed, and the root wheel,
# drag, double, triple and middle click rows are sent as SGR reports. After
# each step the pane's mode and position, the newest buffer, what the
# clipboard stubs got and what was pasted must read back as on tmux.
#
# Part 3, the daily bindings: prefix s K ? u g G P T W a A are pressed once
# each, on the same client, and what each one ran (its script's or stub's
# argv, directory and popup size) must be what it ran on tmux. The owner's
# own tools (tsess, gh-dash, pet-pick.sh and the rest) are stubs that log a
# line, as are fzf, the claude command and every clipboard command; every
# plugin script runs unchanged.
#
# Every child runs under `env -i` with the world's variables only: none of
# the caller's tmux, grofe or canopy environment reaches it, nothing is
# fetched (opensessions' binaries and fzf-url's xre are stubs, as canopy's
# own loader tests make them), and every server started is killed on exit.
#
# $GROFE_BIN names the grofe binary (default: target/debug/grofe).
set -euo pipefail

keep=""
if [ "${1:-}" = "--keep" ]; then
  keep="$2"
  shift 2
fi
if [ "$#" -ne 2 ]; then
  echo "usage: $0 [--keep <dir>] <canopy-checkout> <athome-checkout>" >&2
  exit 2
fi
canopy="$(cd "$1" && pwd)"
athome="$(cd "$2" && pwd)"
dots="$athome/home"
repo="$(cd "$(dirname "$0")/.." && pwd)"
grofe="${GROFE_BIN:-$repo/target/debug/grofe}"
tmux37="${KEY_PARITY_TMUX:-}"
reasons_doc="$repo/docs/compat-coverage.md"
[ -x "$grofe" ] || { echo "no grofe binary at $grofe (cargo build first)" >&2; exit 2; }
case "$tmux37" in
  /*) ;;
  *) echo "KEY_PARITY_TMUX must be the absolute path of a tmux 3.7b" >&2; exit 2 ;;
esac
[ "$(env -i "$tmux37" -V)" = "tmux 3.7b" ] || { echo "$tmux37 is not tmux 3.7b" >&2; exit 2; }

tmp="$(mktemp -d "${TMPDIR:-/tmp}/key-parity.XXXXXX")"
tmp="$(cd "$tmp" && pwd -P)"
# A socket path must fit sun_path (about 100 bytes), so each world's
# TMUX_TMPDIR and XDG_RUNTIME_DIR is a private directory under a fresh
# mktemp directory in /tmp, never /tmp itself.
runs="$(mktemp -d /tmp/kp.XXXXXX)"
servers=()
pty_pids=()
# shellcheck disable=SC2329  # run by the EXIT trap
cleanup() {
  local s p
  for p in "${pty_pids[@]}"; do
    kill "$p" 2>/dev/null || true
  done
  for s in "${servers[@]}"; do
    run_in "${s%%|*}" "${s#*|}" kill-server >/dev/null 2>&1 || true
  done
  # Anything left: a grofe server names its socket under $runs in its argv,
  # and every server was started from a binary under $tmp.
  pkill -f "$runs/" 2>/dev/null || true
  pkill -f "$tmp/" 2>/dev/null || true
  # A server that spins (tmux 3.7b can) ignores both kill-server and
  # SIGTERM: whatever is left a moment later gets SIGKILL.
  sleep 1
  pkill -9 -f "$runs/" 2>/dev/null || true
  pkill -9 -f "$tmp/" 2>/dev/null || true
  if [ -n "$keep" ]; then
    mkdir -p "$keep"
    cp -R "$tmp/out/." "$keep/" 2>/dev/null || true
  fi
  rm -rf "$tmp" "$runs"
}
trap cleanup EXIT

status=0
fail() { echo "FAIL: $*" >&2; status=1; }

# The first `name` on the caller's PATH, a version manager's shim skipped.
host_tool() {
  local d
  local IFS=:
  for d in $PATH; do
    case "$d" in */shims) continue ;; esac
    [ -f "$d/$1" ] && [ -x "$d/$1" ] && { printf '%s\n' "$d/$1"; return 0; }
  done
  return 1
}

render() {
  perl -0pe 's/\{\{ with lookPath "zsh" -\}\}\n(.*?)"\{\{ \. \}\}"\n\{\{ end -\}\}\n/$1"\/bin\/zsh"\n/s;
             s/\{\{ \.chezmoi\.homeDir \}\}/~/g' "$1"
}

# Every pane of every world starts here, so a path a binding hashes (the
# claude session's name) is the same on each.
work="$tmp/work"
mkdir -p "$work" "$tmp/out"
real_fzf="$(host_tool fzf || true)"
real_fzf_tmux="$(host_tool fzf-tmux || true)"
[ -n "$real_fzf" ] && [ -n "$real_fzf_tmux" ] ||
  { echo "the daily bindings need fzf and fzf-tmux on PATH" >&2; exit 2; }

# A stub that logs what it was run with: its name, argv, directory, the
# size of the terminal it runs in and THEME_POPUP, then exits 0. Each of
# the owner's tools a daily binding runs is one.
stub() {
  local log="$1" path="$2"
  mkdir -p "$(dirname "$path")"
  cat >"$path" <<EOF
#!/bin/sh
{
  printf '%s' "\${0##*/}"
  for a in "\$@"; do printf ' [%s]' "\$a"; done
  printf ' cwd=%s' "\$PWD"
  [ -n "\${THEME_POPUP:-}" ] && printf ' THEME_POPUP=%s' "\$THEME_POPUP"
  size=\$(stty size </dev/tty 2>/dev/null) && printf ' tty=%s' "\$size"
  printf '\n'
} >>"$log"
EOF
  chmod +x "$path"
}

# world <name> <binary>: a scratch home laid out as the owner's machine
# lays it out, canopy installed at ~/.local/share/canopy.
world() {
  local w="$tmp/$1" home store b f log
  home="$w/hd"
  store="$home/.local/share/canopy"
  log="$w/daily.log"
  mkdir -p "$w/bin" "$w/tmp" "$home/.config/canopy" "$home/.local/state/canopy" \
    "$home/.config/tmux/plugins/tpm" "$store"
  (umask 077 && mkdir -p "$runs/$1")
  : >"$home/.zshrc"
  : >"$log"
  ln -s "$2" "$w/bin/tmux"
  [ "$1" = grofe ] && ln -s "$2" "$w/bin/grofe"
  f="$(host_tool bash)" && ln -s "$f" "$w/bin/bash"
  for b in bin lib plugins tmux grofe; do
    cp -R "$canopy/$b" "$store/$b"
  done
  # What canopy install provisions, as stubs: nothing is fetched.
  rm -f "$store/plugins/opensessions/bin/"*.tar.gz
  for b in opensessions-sidebar opensessions-server lazydiff; do
    printf '#!/bin/sh\nexit 0\n' >"$store/plugins/opensessions/bin/$b"
    chmod +x "$store/plugins/opensessions/bin/$b"
  done
  sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$store/plugins/opensessions/package.json" |
    head -1 >"$store/plugins/opensessions/bin/.opensessions-version"
  # xre answers its version, and otherwise lists the http(s) URLs it reads.
  mkdir -p "$store/plugins/tmux-fzf-url/bin"
  cat >"$store/plugins/tmux-fzf-url/bin/xre" <<'EOF'
#!/bin/sh
[ "$1" = --version ] && { echo "xre 0.1.1"; exit 0; }
grep -oE 'https?://[A-Za-z0-9./_-]+'
exit 0
EOF
  chmod +x "$store/plugins/tmux-fzf-url/bin/xre"
  : >"$w/artifacts"
  for f in opensessions/bin/opensessions-sidebar opensessions/bin/opensessions-server \
    opensessions/bin/lazydiff opensessions/bin/.opensessions-version tmux-fzf-url/bin/xre; do
    printf '%s any https://fixture.invalid/none %s none %s %s\n' "${f%%/*}" \
      "$(printf '0%.0s' $(seq 64))" "${f#*/}" \
      "$(shasum -a 256 "$store/plugins/$f" | awk '{ print $1 }')" >>"$w/artifacts"
  done
  # A TPM that loads nothing, should a user.conf still run one: canopy
  # vendors the plugins, and the clone TPM would run must not happen here.
  printf '#!/bin/sh\nexit 0\n' >"$home/.config/tmux/plugins/tpm/tpm"
  chmod +x "$home/.config/tmux/plugins/tpm/tpm"
  # vim-tmux-navigator's tmux side is canopy's tmux/conf.d/40-nav.conf on
  # tmux, and canopy's 40-canopy-nav.toml binds the same keys on grofe.
  # The one athome script that changes a key table as it loads.
  cp "$dots/dot_config/tmux/executable_wk-keys-fix.sh" "$home/.config/tmux/wk-keys-fix.sh"
  chmod +x "$home/.config/tmux/wk-keys-fix.sh"
  # The owner's tools the daily bindings run.
  for f in .local/bin/tsess .local/bin/theme .local/bin/cockpit \
    .config/tmux/keys-popup.sh .config/tmux/enhance-pr.sh .config/tmux/pet-pick.sh \
    .config/tmux/net-popup.sh .config/themes/apply-ghdash.sh .local/share/mise/shims/gh-dash; do
    stub "$log" "$home/$f"
  done
  # claude-session-manager's agent: logs which session it runs in, then
  # gives the popup a moment before the session ends and closes it.
  cat >"$w/bin/claude" <<EOF
#!/bin/sh
printf 'claude session=%s cwd=%s tty=%s\n' "\$(tmux display-message -p '#S')" "\$PWD" \
  "\$(stty size </dev/tty 2>/dev/null)" >>"$log"
sleep 1
EOF
  chmod +x "$w/bin/claude"
  # Every clipboard command a plugin or grofe could reach is a stub that
  # logs what it was given, so nothing reaches the machine's clipboard.
  for b in pbcopy pbpaste wl-copy wl-paste xclip xsel clip.exe; do
    cat >"$w/bin/$b" <<EOF
#!/bin/sh
{ printf '%s:' "\${0##*/}"; cat; printf '\n'; } >>"$w/clip.log"
EOF
    chmod +x "$w/bin/$b"
  done
  : >"$w/clip.log"
  # fzf and fzf-tmux are the host's, so each plugin takes the path it takes
  # on this machine and its picker opens where it opens there. fzf first
  # logs its argv, the size of the terminal it runs in and its input; the
  # picker then logs the size it got, from its start event, and aborts
  # (FZF_DEFAULT_OPTS, which daily() sets on the server).
  cat >"$w/bin/fzf" <<EOF
#!/bin/sh
[ "\$1" = --version ] && exec "$real_fzf" --version
in=\$(mktemp "$w/tmp/fzf-in.XXXXXX")
cat >"\$in"
{
  printf '%s' "\${0##*/}"
  for a in "\$@"; do printf ' [%s]' "\$a"; done
  size=\$(stty size </dev/tty 2>/dev/null) && printf ' tty=%s' "\$size"
  printf '\n'
  sed 's/^/  < /' "\$in"
} >>"$log"
exec "$real_fzf" "\$@" <"\$in"
EOF
  chmod +x "$w/bin/fzf"
  ln -s "$real_fzf_tmux" "$w/bin/fzf-tmux"
}

# run_in <world> <socket> <tmux args>: the world's tmux, from its home,
# with the world's environment and nothing else.
run_in() {
  local w="$tmp/$1" r="$runs/$1" sock="$2" bin=tmux
  shift 2
  [ "${1:-}" = --grofe ] && { bin=grofe; shift; }
  (
    cd "${run_cwd:-$w/hd}"
    # Bounded: a command to a server that stopped answering gets SIGALRM.
    exec perl -e 'alarm shift; exec @ARGV or die "$ARGV[0]: $!\n"' 60 env -i \
      HOME="$w/hd" XDG_CONFIG_HOME="$w/hd/.config" XDG_DATA_HOME="$w/hd/.local/share" \
      XDG_STATE_HOME="$w/hd/.local/state" XDG_RUNTIME_DIR="$r" TMUX_TMPDIR="$r" \
      CANOPY_SCRATCH_TMPDIR="$r" TMPDIR="$w/tmp" \
      PATH="$w/bin:/usr/bin:/bin:/usr/sbin:/sbin" SHELL=/bin/sh TERM=xterm-256color LANG=en_US.UTF-8 \
      GROFE_RESTORE_ON_START=0 OPENSESSIONS_SKIP_BINARY_DOWNLOAD=1 \
      OPENSESSIONS_PID_FILE="$w/tmp/opensessions.pid" CANOPY_ARTIFACTS="$w/artifacts" \
      "$w/bin/$bin" -L "$sock" "$@"
  )
}

canopy_env() {
  local home="$tmp/$1/hd"
  printf '%s\n' "CANOPY_STORE=$home/.local/share/canopy" \
    "CANOPY_STATE=$home/.local/state/canopy" "CANOPY_CONFIG=$home/.config/canopy"
}

# canopy's generated state layers and the owner's user.conf, as `canopy
# install` and chezmoi leave them.
canopy_layers() {
  local w="$tmp/$1"
  render "$dots/dot_config/canopy/user.conf.tmpl" >"$w/hd/.config/canopy/user.conf"
  # shellcheck disable=SC2046
  env -i HOME="$w/hd" PATH="$w/bin:/usr/bin:/bin" $(canopy_env "$1") \
    sh "$w/hd/.local/share/canopy/bin/canopy-keys" >/dev/null
  # shellcheck disable=SC2046
  env -i HOME="$w/hd" PATH="$w/bin:/usr/bin:/bin" $(canopy_env "$1") \
    sh "$w/hd/.local/share/canopy/bin/canopy-caps" >/dev/null
}

# The first pane shows a URL for fzf-url to find, and holds still.
pane_cmd="printf 'see https://example.invalid/tp9a\\n'; exec cat"

# On tmux, the stub canopy install writes: the three paths, then tmux.conf.
start_tmux() {
  local kv
  run_in "$1" "$2" -f /dev/null new-session -d -s main -x 120 -y 40 -c "$work" "$pane_cmd"
  servers+=("$1|$2")
  while IFS= read -r kv; do
    run_in "$1" "$2" set-environment -g "${kv%%=*}" "${kv#*=}"
  done < <(canopy_env "$1")
  # From the directory the session starts in, as `tmux new` run there
  # loads the config: a plugin reads $PWD as it loads (sessionx).
  run_cwd="$work" run_in "$1" "$2" source-file "$tmp/$1/hd/.local/share/canopy/tmux/tmux.conf" ||
    fail "$1: source-file tmux.conf exited nonzero"
  # The window the session started with, where base-index puts it once
  # the config is read, as it is when tmux reads the config first.
  run_in "$1" "$2" move-window -s main:0 -t main:1
}

# On grofe, the native tree in ~/.config/grofe, as chezmoi and `canopy
# grofe generate` leave it.
start_grofe() {
  local conf="$tmp/$1/hd/.config/grofe" how=()
  mkdir -p "$conf/conf.d"
  cp "$dots/dot_config/grofe/config.toml" "$conf/config.toml"
  cp "$canopy"/grofe/conf.d/*.toml "$conf/conf.d/"
  render "$dots/dot_config/grofe/user.toml.tmpl" >"$conf/user.toml"
  [ "$1" = grofe ] && how=(--grofe)
  run_in "$1" "$2" "${how[@]}" new-session -d -s main -x 120 -y 40 -c "$work" "$pane_cmd"
  servers+=("$1|$2")
}

# The load is done when claude-session-manager, the last plugin, has bound
# its key and prefix C-s is send-prefix again. Then wk-keys-fix.sh runs
# once more: user.conf starts it before the loader on tmux, and it gives up
# after 10 s if which-key has not built its menu by then, so without this
# the Space binding would depend on how long the load took.
wait_loaded() {
  local keys
  for _ in $(seq 1 160); do
    keys="$(run_in "$1" "$2" list-keys -T prefix 2>/dev/null)" || keys=""
    if printf '%s\n' "$keys" | grep -q 'tmux-claude-session-manager/scripts/launch.sh' &&
      printf '%s\n' "$keys" | grep -Eq ' C-s +send-prefix$'; then
      run_in "$1" "$2" run-shell "$tmp/$1/hd/.config/tmux/wk-keys-fix.sh"
      return 0
    fi
    sleep 0.25
  done
  fail "$1: the plugin load never finished; plugins.log:
$(cat "$tmp/$1/hd/.local/state/canopy/plugins.log" 2>/dev/null)"
  return 1
}

# normalize <world>: the world's own paths, ttys and the padding list-keys
# puts after the key (as wide as the table's widest key).
normalize() {
  sed -e "s|$tmp/$1/hd|~|g" -e "s|$work|<work>|g" -e "s|$runs/$1|<run>|g" \
    -e 's|/dev/ttys*[0-9][0-9]*|<tty>|g' -e 's|/dev/pts/[0-9][0-9]*|<tty>|g'
}

# Every table's `list-keys`, one file per table, as `<key><TAB><line>`.
dump() {
  local w="$1" sock="$2" out="$tmp/out/$1" t
  mkdir -p "$out"
  run_in "$w" "$sock" list-keys | awk '{ print ($2 == "-r") ? $4 : $3 }' | sort -u >"$out/tables"
  while IFS= read -r t; do
    run_in "$w" "$sock" list-keys -T "$t" | normalize "$w" |
      awk '{ k = ($2 == "-r") ? 5 : 4; key = $k
             line = $0; sub("^bind-key +(-r +)?-T +[^ ]+ +[^ ]+ +", "", line)
             print key "\t" (($2 == "-r") ? "-r " : "") line }' |
      LC_ALL=C sort -t "$(printf '\t')" -k1,1 >"$out/$t"
  done <"$out/tables"
}

# The reasons table: `| table | key | class | servers | reason |` rows
# under the "## Key parity" heading of docs/compat-coverage.md, as
# tab-separated table, key, class, servers and reason.
reasons() {
  awk -F'|' '
    /^## Key parity/ { on = 1; next }
    on && /^## / { on = 0 }
    on && /^\| `/ {
      for (i = 2; i <= 6; i++) { gsub(/^ +| +$/, "", $i) }
      t = $2; k = $3; gsub(/^`|`$/, "", t); gsub(/^`|`$/, "", k)
      if ($4 ~ /^(mouse|oos|native|gap)$/) print t "\t" k "\t" $4 "\t" $5 "\t" $6
    }' "$reasons_doc"
}

# compare <grofe-world>: per table, every key bound on either side.
compare() {
  local g="$1" ref="$tmp/out/tmux" got="$tmp/out/$1" t key class on
  while IFS= read -r t; do
    touch "$ref/$t" "$got/$t"
    awk -F'\t' -v t="$t" -v g="$g" '
      FILENAME == ARGV[1] {
        if ($1 != t || ($4 != "both" && $4 != g)) next
        # A row for "(all)" excuses every key of the table without a row
        # of its own; it is stale when no key of the table differs.
        if ($2 == "(all)") { allwhy = $3 ": " $5; allclass = $3; next }
        why[$2] = $3 ": " $5; class[$2] = $3; next
      }
      FILENAME == ARGV[2] { want[$1] = $2; keys[$1] = 1; next }
      { have[$1] = $2; keys[$1] = 1 }
      END {
        for (k in keys) {
          w = (k in want) ? want[k] : ""; h = (k in have) ? have[k] : ""
          c = (k in class) ? class[k] : ""
          if (w == h && c == "") { same++; continue }
          if (c == "" && allclass != "") { n[allclass]++; alln++; print "EXCUSED\t" k "\t" allwhy; continue }
          # A row whose key matches is stale, whatever its class.
          if (w == h) { print "FAIL\t" k "\tmatches tmux, but the reasons table excuses it (" why[k] ")"; bad++; continue }
          if (c != "") { n[c]++; print "EXCUSED\t" k "\t" why[k]; continue }
          print "FAIL\t" k "\ttmux: " (w == "" ? "(unbound)" : w) " || grofe: " (h == "" ? "(unbound)" : h)
          bad++
        }
        # A row for a key neither side binds is stale too.
        for (k in why) if (!(k in keys)) {
          print "FAIL\t" k "\tneither side binds it, but the reasons table names it (" why[k] ")"; bad++
        }
        if (allclass != "" && alln == 0) {
          print "FAIL\t(all)\tno key differs, but the reasons table excuses the table (" allwhy ")"; bad++
        }
        printf "SUMMARY\t%-14s %-11s %3d match, %d mouse, %d oos, %d native, %d gap, %d differ\n",
          t, g, same, n["mouse"], n["oos"], n["native"], n["gap"], bad
      }' <(reasons) "$ref/$t" "$got/$t" | LC_ALL=C sort >"$tmp/out/$g.$t.result"
    while IFS="$(printf '\t')" read -r kind key rest; do
      case "$kind" in
        FAIL) fail "$g $t $key: $rest" ;;
        EXCUSED) printf '%s %s %s: %s\n' "$g" "$t" "$key" "$rest" >>"$tmp/out/excused" ;;
        SUMMARY) printf '%s %s\n' "$key" "$rest" >>"$tmp/out/summary" ;;
      esac
    done <"$tmp/out/$g.$t.result"
  done < <(sort -u "$ref/tables" "$got/tables")
  # And a row for a table neither side has.
  while IFS="$(printf '\t')" read -r t key class on _; do
    case "$on" in both | "$g") ;; *) continue ;; esac
    sort -u "$ref/tables" "$got/tables" | grep -qx -- "$t" ||
      fail "$g $t $key: neither side has table $t, but the reasons table names it ($class)"
  done < <(reasons)
}

# --- the attached client, and part 3: the daily bindings --------------------

daily_keys="s K ? u g G P T W a A"

# attach <world> <socket>: a client on main through a pty, 120x40, fed
# from a fifo the script holds open on fd 9 for that world.
attach() {
  local w="$1" sock="$2" fifo="$tmp/$1/keys" inner pty
  mkfifo "$fifo"
  inner="stty cols 120 rows 40; exec \"$tmp/$w/bin/tmux\" -L $sock attach -t main"
  if script -q /dev/null true </dev/null >/dev/null 2>&1; then
    # BSD script: script [-q] file command...
    pty=(script -q /dev/null sh -c "$inner")
  else
    # util-linux script: script -q -c command file
    pty=(script -q -c "$inner" /dev/null)
  fi
  (
    cd "$tmp/$w/hd"
    exec env -i HOME="$tmp/$w/hd" XDG_CONFIG_HOME="$tmp/$w/hd/.config" \
      XDG_STATE_HOME="$tmp/$w/hd/.local/state" XDG_RUNTIME_DIR="$runs/$w" \
      TMUX_TMPDIR="$runs/$w" PATH="$tmp/$w/bin:/usr/bin:/bin" TERM=xterm-256color \
      LANG=en_US.UTF-8 "${pty[@]}"
  ) < <(cat "$fifo") >/dev/null 2>&1 &
  pty_pids+=("$!")
  exec 9>"$fifo"
  for _ in $(seq 1 80); do
    run_in "$w" "$sock" list-clients -F '#{client_session}' 2>/dev/null | grep -qx main && return 0
    sleep 0.25
  done
  fail "$w: no client attached"
  return 1
}

# press <world> <socket> <key>: prefix, then the key; then wait until the
# binding is done: its first log line written, then no new line for 1.5 s
# and only the one client left (an agent's nested client gone, its popup
# closed). The key's section is the lines up to that point, so nothing a
# binding logs late can land in the next key's.
press() {
  local w="$1" sock="$2" key="$3" log="$tmp/$1/daily.log" before n last=-1 still=0
  before="$(wc -l <"$log")"
  printf '%s%s' "$prefix_byte" "$key" >&9
  for _ in $(seq 1 80); do
    sleep 0.25
    n="$(wc -l <"$log")"
    [ "$n" -gt "$before" ] || continue
    if [ "$n" -eq "$last" ] &&
      [ "$(run_in "$w" "$sock" list-clients -F x 2>/dev/null | wc -l)" -eq 1 ]; then
      still=$((still + 1))
    else
      still=0
    fi
    last="$n"
    [ "$still" -ge 6 ] && break
  done
  [ "$still" -ge 6 ] || fail "$w prefix $key: never settled"
  printf '== %s\n' "$key" >>"$tmp/out/$w.daily"
  head -n "$last" "$log" | tail -n +"$((before + 1))" | normalize "$w" >>"$tmp/out/$w.daily"
  sleep 0.5
}

daily() {
  local w="$1" sock="$2" key
  local pfx n
  pfx="$(run_in "$w" "$sock" show-options -gv prefix)"
  case "$pfx" in
    C-?) ;;
    *) fail "$w: prefix $pfx is not a C- key"; return 0 ;;
  esac
  n=$(( $(printf '%d' "'${pfx#C-}") & 31 ))
  # shellcheck disable=SC2059  # the format is the octal escape itself
  prefix_byte="$(printf "\\$(printf '%03o' "$n")")"
  # Each picker reports the size it got and closes at once.
  run_in "$w" "$sock" set-environment -g FZF_DEFAULT_OPTS \
    "--bind 'start:execute-silent(echo picker \$FZF_COLUMNS x \$FZF_LINES >>$tmp/$w/daily.log)+abort'"
  : >"$tmp/out/$w.daily"
  attach "$w" "$sock" || return 0
  # Copy mode first: a daily binding may leave a picker open on the client.
  copy_checks "$w" "$sock"
  for key in $daily_keys; do
    press "$w" "$sock" "$key"
  done
  exec 9>&-
}

# --- part 2: copy mode, buffers and the mouse ---------------------------------

# copy_checks <world> <socket>: on the attached client, in a window of its
# own, each copy-mode and buffer key of M5 is pressed once (prefix [ PPage
# = ], tmux-yank's y), and each root mouse row is clicked, wheeled or
# dragged as an outer terminal reports it (SGR 1006). After each step the
# pane's mode, position, newest buffer, what the clipboard stubs got and
# whether a pasted text reached the pane are read back; every grofe server
# must read back what tmux did.
copy_checks() {
  local w="$1" sock="$2" out="$tmp/out/$1.copy" pane row x y left top off pos lines
  : >"$out"
  pane="$(run_in "$w" "$sock" new-window -P -F '#{pane_id}' -n cm -c "$work" \
    "for n in \$(seq 1 60); do printf 'line %02d alpha beta-%02d gamma\\n' \$n \$n; done; exec cat")"
  for _ in $(seq 1 40); do
    run_in "$w" "$sock" capture-pane -p -t "$pane" | grep -q '^line 60 ' && break
    sleep 0.25
  done
  # Client cells of line 50's text: the pane's place, below a top status.
  row="$(run_in "$w" "$sock" capture-pane -p -t "$pane" | grep -n '^line 50 ' | cut -d: -f1)"
  left="$(run_in "$w" "$sock" display-message -p -t "$pane" '#{pane_left}')"
  top="$(run_in "$w" "$sock" display-message -p -t "$pane" '#{pane_top}')"
  pos="$(run_in "$w" "$sock" show-options -gv status-position)"
  lines="$(run_in "$w" "$sock" show-options -gv status)"
  case "$lines" in on) lines=1 ;; off) lines=0 ;; esac
  off=0
  [ "$pos" = top ] && off="$lines"
  y=$((off + top + row))
  x=$((left + 1))
  printf 'layout row=%s left=%s top=%s status=%s/%s\n' "$row" "$left" "$top" "$pos" "$lines" >>"$out"

  clip_seen=0
  readout() {
    local clip
    clip="$(tail -n +"$((clip_seen + 1))" "$tmp/$w/clip.log")"
    clip_seen="$(wc -l <"$tmp/$w/clip.log")"
    {
      printf '== %s\n' "$1"
      run_in "$w" "$sock" display-message -p -t "$pane" \
        'mode=#{pane_in_mode} #{pane_mode} pos=#{scroll_position} zoom=#{window_zoomed_flag}'
      printf 'buffer=%s\n' "$(run_in "$w" "$sock" show-buffer 2>&1 | head -3)"
      printf 'clipboard=%s\n' "$clip"
      printf 'pasted=%s\n' "$(run_in "$w" "$sock" capture-pane -p -t "$pane" |
        grep -oE 'tp9b-(middle|bracket)' | tr '\n' ' ')"
    } >>"$out"
  }
  # keys <seconds> <bytes>: typed on the client, then a settle.
  keys() {
    printf '%b' "$2" >&9
    sleep "$1"
  }
  # sgr <button> <col> <row> [m]: one SGR 1006 report at client cell col,
  # row of line 50 (1-based), a press or motion, or with m a release.
  sgr() { printf '\033[<%s;%s;%s%s' "$1" "$(($2 + x))" "$y" "${4:-M}"; }

  readout start
  keys 1 "${prefix_byte}["
  readout 'prefix ['
  keys 1 q
  keys 1 "$prefix_byte\033[5~"
  readout 'prefix PPage'
  keys 1 q
  # tmux-yank's y, after a selection of line 60's first word.
  keys 1 "${prefix_byte}["
  keys 1 'k0 e'
  keys 1 y
  readout 'copy-mode-vi y (tmux-yank)'
  # WheelUpPane: the first tick enters copy mode, the next scrolls.
  keys 1 "$(sgr 64 10 0)"
  readout 'WheelUpPane'
  keys 1 "$(sgr 64 10 0)"
  readout 'WheelUpPane, again'
  keys 1 q
  # MouseDrag1Pane from "alpha" to the end of "beta-50", then the release
  # (tmux-yank's MouseDragEnd1Pane).
  keys 0.3 "$(sgr 0 8 0)"
  keys 0.3 "$(sgr 32 10 0)"
  keys 0.3 "$(sgr 32 20 0)"
  keys 1.5 "$(sgr 0 20 0 m)"
  readout 'MouseDrag1Pane, MouseDragEnd1Pane'
  # DoubleClick1Pane and TripleClick1Pane on "gamma": two and three
  # presses within 300 ms, then the copy after run-shell -d 0.3.
  keys 2 "$(sgr 0 23 0)$(sgr 0 23 0 m)$(sgr 0 23 0)$(sgr 0 23 0 m)"
  readout 'DoubleClick1Pane'
  keys 2 "$(sgr 0 9 0)$(sgr 0 9 0 m)$(sgr 0 9 0)$(sgr 0 9 0 m)$(sgr 0 9 0)$(sgr 0 9 0 m)"
  readout 'TripleClick1Pane'
  # prefix = opens the buffer chooser zoomed; q closes it.
  keys 1 "$prefix_byte="
  readout 'prefix ='
  keys 1 q
  readout 'prefix =, q'
  # MouseDown2Pane pastes the newest buffer; prefix ] does too.
  run_in "$w" "$sock" set-buffer tp9b-middle
  keys 1 "$(sgr 1 30 0)$(sgr 1 30 0 m)"
  readout 'MouseDown2Pane'
  run_in "$w" "$sock" set-buffer tp9b-bracket
  keys 1 "$prefix_byte]"
  readout 'prefix ]'
  run_in "$w" "$sock" kill-window -t "$pane"
}

# --- run ---------------------------------------------------------------------

world tmux "$tmux37"
canopy_layers tmux
start_tmux tmux grofe-tp9b-tmux
wait_loaded tmux grofe-tp9b-tmux || true
dump tmux grofe-tp9b-tmux
# With every plugin loaded, tmux's copy-mode-vi table is TM.0's golden, row
# for row, so the per-key comparison below covers exactly its 95 rows.
golden="$repo/tests/fixtures/compat/golden/tm_0/list-keys-copy-mode-vi.txt"
run_in tmux grofe-tp9b-tmux list-keys -T copy-mode-vi | normalize tmux >"$tmp/out/tmux.copy-mode-vi.raw"
diff -u "$golden" "$tmp/out/tmux.copy-mode-vi.raw" >"$tmp/out/golden.diff" ||
  fail "tmux's copy-mode-vi table is not TM.0's golden:
$(cat "$tmp/out/golden.diff")"
printf 'copy-mode-vi   tmux        %3d rows, TM.0 golden %d rows\n' \
  "$(wc -l <"$tmp/out/tmux.copy-mode-vi.raw")" "$(wc -l <"$golden")" >"$tmp/out/summary"

for g in grofe-tmux grofe; do
  world "$g" "$grofe"
  canopy_layers "$g"
  start_grofe "$g" "grofe-tp9b-$g"
  wait_loaded "$g" "grofe-tp9b-$g" || true
  dump "$g" "grofe-tp9b-$g"
  compare "$g"
done

: >"$tmp/out/daily.summary"
for w in tmux grofe-tmux grofe; do
  sock="grofe-tp9b-$w"
  daily "$w" "$sock"
done
for g in grofe-tmux grofe; do
  for key in $daily_keys; do
    want="$(awk -v k="== $key" '$0 == k { on = 1; next } /^== / { on = 0 } on' "$tmp/out/tmux.daily")"
    have="$(awk -v k="== $key" '$0 == k { on = 1; next } /^== / { on = 0 } on' "$tmp/out/$g.daily")"
    if [ -z "$want" ]; then
      fail "daily $key: nothing ran on tmux"
    elif [ "$want" != "$have" ]; then
      fail "daily $g prefix $key:
    tmux:  $want
    grofe: ${have:-(nothing ran)}"
    else
      printf '%s prefix %s: %s\n' "$g" "$key" "$(printf '%s' "$want" | head -1)" >>"$tmp/out/daily.summary"
    fi
  done
done

: >"$tmp/out/copy.summary"
for g in grofe-tmux grofe; do
  while IFS= read -r step; do
    want="$(awk -v k="$step" '$0 == k { on = 1; next } /^== / { on = 0 } on' "$tmp/out/tmux.copy")"
    have="$(awk -v k="$step" '$0 == k { on = 1; next } /^== / { on = 0 } on' "$tmp/out/$g.copy")"
    if [ "$want" != "$have" ]; then
      fail "copy $g ${step#== }:
    tmux:
$(printf '%s\n' "$want" | sed 's/^/      /')
    grofe:
$(printf '%s\n' "${have:-(nothing)}" | sed 's/^/      /')"
    else
      printf '%s %s: %s\n' "$g" "${step#== }" "$(printf '%s' "$want" | head -1)" >>"$tmp/out/copy.summary"
    fi
  done < <(grep '^== ' "$tmp/out/tmux.copy")
  [ "$(head -1 "$tmp/out/tmux.copy")" = "$(head -1 "$tmp/out/$g.copy")" ] ||
    fail "copy $g: the pane's layout differs: $(head -1 "$tmp/out/$g.copy")"
done

echo "== tables"
cat "$tmp/out/summary"
echo "== excused"
cat "$tmp/out/excused" 2>/dev/null || true
echo "== daily bindings, as on tmux"
cat "$tmp/out/daily.summary"
echo "== copy mode, buffers and the mouse, as on tmux"
cat "$tmp/out/copy.summary"
if [ "$status" -eq 0 ]; then
  echo "key-parity: ok"
fi
exit "$status"
