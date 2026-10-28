#!/usr/bin/env bash
# mac-trial.sh: try grofe as tmux, with your own setup, in one terminal
# window, for a few days, without touching your live tmux, canopy or config.
# docs/mac-trial.md is the guide.
#
#   mac-trial.sh setup  [options]   build or copy grofe, make the trial root
#   mac-trial.sh start  [options]   setup if needed, then start or attach
#   mac-trial.sh status [--root DIR]
#   mac-trial.sh report [--root DIR]
#   mac-trial.sh reset  [--root DIR] deletes the trial root, after asking
#
# Options:
#   --root DIR        the trial root (default ~/.local/share/grofe-trial)
#   --grofe BIN       use this grofe binary instead of a release build
#   --canopy DIR      the canopy to copy (default ~/.local/share/canopy);
#                     it needs canopy's grofe backend (bin/canopy-grofe)
#   --user-toml FILE  the grofe user.toml to use; without it, setup imports
#                     your canopy user.conf with `grofe import-tmux`
#   --offline         do not download canopy's plugin binaries
#
# Everything the trial writes lives under the root. The live directories
# are read, never written: the tmux config dir, canopy's store, state and
# config dirs, grofe's own state dir, ~/Library and the shared temp dirs.
set -euo pipefail
umask 077

SOCKET=grofe-trial
SESSION=trial
MARKER=.grofe-trial
repo="$(cd "$(dirname "$0")/.." && pwd -P)"

die() {
  printf 'mac-trial: %s\n' "$*" >&2
  exit 1
}
say() { printf 'mac-trial: %s\n' "$*"; }
usage() {
  sed -n '6,19s/^# \{0,1\}//p' "$0" >&2
  exit 2
}

# --- the live paths, which the trial reads and never writes ----------------

home="${HOME:?HOME is not set}"
if [ -n "${GROFE_TRIAL_ROOT:-}" ]; then
  # Inside the trial the XDG variables point at the trial itself.
  live_cfg="$home/.config"
  live_data="$home/.local/share"
  live_state="$home/.local/state"
else
  live_cfg="${XDG_CONFIG_HOME:-$home/.config}"
  live_data="${XDG_DATA_HOME:-$home/.local/share}"
  live_state="${XDG_STATE_HOME:-$home/.local/state}"
fi

# --- arguments --------------------------------------------------------------

[ "$#" -gt 0 ] || usage
cmd="$1"
shift
root="${GROFE_TRIAL_ROOT:-$live_data/grofe-trial}"
grofe_bin=""
canopy_src="$live_data/canopy"
user_toml=""
offline=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --root | --grofe | --canopy | --user-toml)
      [ "$#" -ge 2 ] || usage
      case "$1" in
        --root) root="${2%/}" ;;
        --grofe) grofe_bin="$2" ;;
        --canopy) canopy_src="${2%/}" ;;
        --user-toml) user_toml="$2" ;;
      esac
      shift 2
      ;;
    --offline)
      offline=1
      shift
      ;;
    -h | --help) usage ;;
    *) die "unknown argument: $1" ;;
  esac
done

# --- path guards ------------------------------------------------------------

# resolve <abs path>: the path with its longest existing prefix resolved
# through symlinks, so a guard compares where a write would really land.
resolve() {
  local p="$1" rest=""
  while [ ! -d "$p" ]; do
    rest="/${p##*/}$rest"
    p="${p%/*}"
    [ -n "$p" ] || p=/
  done
  p="$(cd -P "$p" && pwd -P)"
  [ "$p" = / ] && p=""
  printf '%s%s\n' "$p" "$rest"
}

# within <a> <b>: a is b, or inside b.
within() {
  case "$1/" in
    "$2"/*) return 0 ;;
  esac
  return 1
}

guard_root() {
  case "$root" in
    /*) ;;
    *) die "--root must be an absolute path, got: $root" ;;
  esac
  case "$root/" in
    */../* | */./*) die "--root must not contain . or .. components: $root" ;;
  esac
  local r p rp
  r="$(resolve "$root")"
  [ -n "$r" ] || die "--root must not be /"
  # The root must not be, or hold, a directory the trial must not touch.
  for p in "$home" "$live_cfg" "$live_data" "$live_state" /tmp /private/tmp /var/tmp; do
    rp="$(resolve "$p")"
    if within "$rp" "$r"; then die "refusing: the trial root $root would contain $p"; fi
  done
  # Nor sit inside one.
  for p in "$live_cfg" "$live_data/canopy" "$live_state/canopy" "$live_state/grofe" \
    "$home/Library" /tmp /private/tmp /var/tmp ${TMPDIR:+"$TMPDIR"}; do
    rp="$(resolve "$p")"
    if within "$r" "$rp"; then die "refusing: the trial root $root is inside $p, which the trial never writes to"; fi
  done
  root="$r"
}

# Each directory the trial writes, checked once it exists: a real directory
# under the root, never a symlink planted to lead out of it.
guard_dir() {
  [ ! -L "$1" ] || die "refusing: $1 is a symlink"
  within "$(resolve "$1")" "$root" || die "refusing: $1 resolves outside $root"
}

set_paths() {
  bin="$root/bin"
  cfg="$root/config"
  data="$root/data"
  state="$root/state"
  run="$root/run"
  ttmp="$root/tmux-tmp"
  stmp="$root/tmp"
  zdot="$root/zdotdir"
  info="$root/info"
  sock="$run/grofe/$SOCKET.sock"
  [ "${#sock}" -lt 100 ] || die "the socket path would be ${#sock} bytes, macOS allows 103: pick a shorter --root"
}

# trial_env <cmd...>: runs cmd in the trial's environment, so grofe, canopy
# and every plugin resolve their paths inside the root.
trial_env() {
  env -u TMUX -u TMUX_PANE -u GROFE -u GROFE_PANE \
    XDG_CONFIG_HOME="$cfg" XDG_DATA_HOME="$data" XDG_STATE_HOME="$state" \
    XDG_RUNTIME_DIR="$run" TMUX_TMPDIR="$ttmp" \
    CANOPY_STORE="$data/canopy" CANOPY_STATE="$state/canopy" CANOPY_CONFIG="$cfg/canopy" \
    GROFE_TRIAL_ROOT="$root" GROFE_TRIAL_BIN="$bin" PATH="$bin:$PATH" "$@"
}

server_running() {
  [ -x "$bin/grofe" ] && trial_env "$bin/grofe" -L "$SOCKET" list-sessions >/dev/null 2>&1
}

# --- setup ------------------------------------------------------------------

# copy_tree <src> <dest> [exclude...]: a copy, never a link, without .git.
copy_tree() {
  local src="$1" dest="$2" x
  shift 2
  local args=(--exclude .git)
  for x in "$@"; do args+=(--exclude "$x"); done
  if [ -e "$dest" ] || [ -L "$dest" ]; then guard_dir "$dest"; fi
  rm -rf "$dest"
  mkdir -p "$dest"
  guard_dir "$dest"
  (cd "$src" && tar -cf - "${args[@]}" .) | (cd "$dest" && tar -xf -)
}

# BRE escapes for sed: one for a pattern, one for a replacement.
esc_pat() { printf '%s' "$1" | sed 's/[][\.*^$#]/\\&/g'; }
esc_rep() { printf '%s' "$1" | sed 's/[\&#]/\\&/g'; }

# patch_paths <file>: points the live directories the trial copied at the
# copies, and puts the trial's bin/ first on a PATH a script sets itself.
# shellcheck disable=SC2016  # the spellings and replacements are literal text
patch_paths() {
  local f="$1" script="" spelling dir var
  for spelling in "$home" '$HOME' '${HOME}' '~'; do
    for dir in .config/tmux:'$XDG_CONFIG_HOME/tmux' .config/canopy:'$CANOPY_CONFIG' \
      .local/share/canopy:'$CANOPY_STORE' .local/state/canopy:'$CANOPY_STATE'; do
      var="${dir#*:}"
      script="${script}s#$(esc_pat "$spelling/${dir%%:*}")#$(esc_rep "$var")#g
"
    done
  done
  script="${script}"'s#export PATH="#export PATH="${GROFE_TRIAL_BIN:+$GROFE_TRIAL_BIN:}#g'
  sed "$script" "$f" >"$f.trial"
  cat "$f.trial" >"$f"
  rm -f "$f.trial"
}

# link_farm <live dir> <trial dir> <skip...>: every other entry of a live
# XDG directory as a symlink, so the rest of your setup (nvim, gh, mise...)
# still finds its files from inside the trial.
link_farm() {
  local src="$1" dest="$2" e name s skip
  shift 2
  mkdir -p "$dest"
  guard_dir "$dest"
  for e in "$dest"/* "$dest"/.[!.]*; do
    if [ -L "$e" ]; then rm -f "$e"; fi
  done
  [ -d "$src" ] || return 0
  for e in "$src"/* "$src"/.[!.]*; do
    [ -e "$e" ] || [ -L "$e" ] || continue
    name="${e##*/}"
    skip=0
    for s in "$@"; do [ "$name" = "$s" ] && skip=1; done
    if within "$(resolve "$e")" "$root"; then skip=1; fi
    [ "$skip" -eq 1 ] || [ -e "$dest/$name" ] || ln -s "$e" "$dest/$name"
  done
}

write_zdotdir() {
  mkdir -p "$zdot"
  guard_dir "$zdot"
  local f
  for f in .zshenv .zprofile .zshrc .zlogin; do
    cat >"$zdot/$f" <<ZSH
# Generated by grofe's scripts/mac-trial.sh. Sources your own $f, then puts
# the trial's bin/ first on PATH again, so \`tmux\` stays grofe in every pane.
_grofe_trial_zdotdir=\$ZDOTDIR
ZDOTDIR=\${GROFE_TRIAL_USER_ZDOTDIR:-\$HOME}
[[ -r \$ZDOTDIR/$f ]] && source \$ZDOTDIR/$f
GROFE_TRIAL_USER_ZDOTDIR=\$ZDOTDIR
ZDOTDIR=\$_grofe_trial_zdotdir
unset _grofe_trial_zdotdir
typeset -U path
path=("\$GROFE_TRIAL_BIN" \$path)
ZSH
  done
}

do_setup() {
  [ -z "${GROFE_TRIAL_ROOT:-}" ] || die "run setup from outside the trial"
  if [ -e "$root" ] && [ ! -f "$root/$MARKER" ]; then
    if [ -n "$(ls -A "$root" 2>/dev/null)" ]; then
      die "$root exists, is not empty and is not a trial root: pick another --root"
    fi
  fi
  if server_running; then
    die "the trial server is running: detach, run \`$bin/tmux -L $SOCKET kill-server\` from the trial, then setup again"
  fi
  [ -d "$canopy_src" ] || die "no canopy at $canopy_src: pass --canopy <dir>"
  [ -x "$canopy_src/bin/canopy-grofe" ] ||
    die "the canopy at $canopy_src has no grofe backend (bin/canopy-grofe): pass --canopy <a canopy checkout that has it>"
  if [ -n "$user_toml" ]; then [ -f "$user_toml" ] || die "no such file: $user_toml"; fi

  # The binary first, so a failed build leaves nothing behind.
  local src_bin built_from
  if [ -n "$grofe_bin" ]; then
    [ -x "$grofe_bin" ] || die "not an executable: $grofe_bin"
    src_bin="$grofe_bin"
    built_from="given binary"
  else
    say "building grofe (release, CARGO_BUILD_JOBS=4)"
    (cd "$repo" && CARGO_BUILD_JOBS=4 cargo build --release --locked --bin grofe)
    src_bin="$repo/target/release/grofe"
    built_from="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    if [ -n "$(git -C "$repo" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
      built_from="$built_from with local changes"
    fi
  fi

  mkdir -p "$root"
  guard_dir "$root"
  printf 'grofe trial root, made by scripts/mac-trial.sh; mac-trial.sh reset deletes it\n' >"$root/$MARKER"
  local d
  for d in "$bin" "$cfg" "$data" "$state" "$info" "$stmp" "$root/reports"; do
    mkdir -p "$d"
    guard_dir "$d"
  done
  for d in "$run" "$ttmp"; do
    mkdir -p "$d"
    guard_dir "$d"
    chmod 700 "$d"
  done
  export TMPDIR="$stmp"

  # A copy, not a link: a rebuild never swaps the binary under the server.
  cp "$src_bin" "$bin/grofe.new"
  chmod 755 "$bin/grofe.new"
  mv -f "$bin/grofe.new" "$bin/grofe"
  ln -sfn grofe "$bin/tmux"
  {
    printf 'grofe\t%s\t%s\n' "$("$bin/grofe" -V)" "$built_from"
    printf 'tmux name\t%s\n' "$("$bin/tmux" -V)"
  } >"$info/grofe-source.tsv"

  # Your tmux scripts and canopy's user.conf, copied and pointed at the copies.
  say "copying your tmux dir, canopy config and canopy store"
  local f
  if [ -d "$live_cfg/tmux" ]; then
    copy_tree "$live_cfg/tmux" "$cfg/tmux" ./plugins
    for f in "$cfg/tmux"/*.sh "$cfg/tmux"/*.conf; do
      if [ -f "$f" ]; then patch_paths "$f"; fi
    done
  fi
  if [ -d "$live_cfg/canopy" ]; then
    copy_tree "$live_cfg/canopy" "$cfg/canopy"
    for f in "$cfg/canopy"/*; do
      if [ -f "$f" ]; then patch_paths "$f"; fi
    done
  else
    mkdir -p "$cfg/canopy"
  fi
  copy_tree "$canopy_src" "$data/canopy"
  # continuum adds or removes a login agent (~/Library/LaunchAgents, or a
  # systemd unit) every time it loads, by $HOME, which the trial keeps. In
  # the copy those four scripts do nothing, so launchd is never touched.
  # canopy doctor run in the trial reports the copy's continuum tree as
  # changed because of it.
  local auto="$data/canopy/plugins/tmux-continuum/scripts/handle_tmux_automatic_start"
  for f in "$auto"/osx_enable.sh "$auto"/osx_disable.sh "$auto"/systemd_enable.sh "$auto"/systemd_disable.sh; do
    if [ -f "$f" ]; then
      printf '#!/bin/sh\n# Disabled by grofe'"'"'s mac-trial.sh: the trial never touches login agents.\nexit 0\n' >"$f"
    fi
  done
  {
    printf 'canopy\t%s\t%s\n' "$(cat "$data/canopy/VERSION" 2>/dev/null || echo '?')" \
      "$(git -C "$canopy_src" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  } >"$info/canopy-source.tsv"
  # grofe refuses a state dir that group or other can read.
  mkdir -p "$state/canopy" "$state/grofe"
  chmod 700 "$state/grofe"

  link_farm "$live_cfg" "$cfg" tmux canopy grofe
  link_farm "$live_data" "$data" canopy grofe
  link_farm "$live_state" "$state" canopy grofe

  # grofe's config: the three layers, all inside the trial.
  local g="$cfg/grofe"
  mkdir -p "$g/conf.d"
  guard_dir "$g"
  cat >"$g/config.toml" <<TOML
# Generated by grofe's scripts/mac-trial.sh; setup rewrites it. Your own
# trial-only settings go in conf.d/99-trial.toml, which setup never touches.
[environment]
CANOPY_STORE = "$data/canopy"
CANOPY_STATE = "$state/canopy"
CANOPY_CONFIG = "$cfg/canopy"
TOML
  if [ ! -f "$g/conf.d/99-trial.toml" ]; then
    cat >"$g/conf.d/99-trial.toml" <<'TOML'
# Trial-only settings, kept across `mac-trial.sh setup`. For example:
# graphics = true
TOML
  fi

  say "generating canopy's grofe fragments"
  trial_env CANOPY_GROFE="$bin/grofe" "$data/canopy/bin/canopy-grofe" generate --out "$g/conf.d"
  local c
  for c in caps index keys; do
    if [ -x "$data/canopy/bin/canopy-$c" ]; then
      trial_env "$data/canopy/bin/canopy-$c" >/dev/null 2>&1 || say "canopy $c failed (not fatal)"
    fi
  done
  if [ "$offline" -eq 0 ]; then
    # shellcheck disable=SC2016  # expanded by the inner bash, in the trial's env
    trial_env bash -c '. "$CANOPY_STORE/lib/env.sh" && . "$CANOPY_STORE/lib/plugins.sh" && canopy_provision_artifacts' ||
      say "some plugin binaries could not be provided; canopy doctor lists them"
  fi

  if [ -n "$user_toml" ]; then
    cp "$user_toml" "$g/user.toml"
    printf 'user.toml copied from --user-toml\n' >"$info/user-toml.txt"
  elif [ -f "$cfg/canopy/user.conf" ]; then
    say "importing your canopy user.conf with grofe import-tmux"
    rm -f "$g/user.toml"
    if ! trial_env "$bin/grofe" import-tmux "$cfg/canopy/user.conf" -o "$g/user.toml" 2>"$info/user-toml.txt"; then
      say "import-tmux failed, so there is no user.toml; see $info/user-toml.txt"
      rm -f "$g/user.toml"
    fi
  fi
  if [ -f "$g/user.toml" ]; then patch_paths "$g/user.toml"; fi

  if [ "${SHELL##*/}" = zsh ]; then write_zdotdir; fi

  if ! trial_env "$bin/grofe" show-config >/dev/null 2>"$info/show-config.err" || [ -s "$info/show-config.err" ]; then
    cat "$info/show-config.err" >&2
    die "grofe refuses the trial config (above); fix it under $g and run setup again"
  fi
  say "ready: $root"
  say "start it in a fresh terminal window: $0 start --root $root"
}

# --- start ------------------------------------------------------------------

do_start() {
  [ -z "${TMUX:-}" ] || die "you are inside tmux: open a fresh terminal window and run this there"
  [ -z "${GROFE_TRIAL_ROOT:-}" ] || die "you are already inside the trial"
  if [ ! -f "$root/$MARKER" ] || [ ! -x "$bin/tmux" ]; then do_setup; fi
  printf '%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${TERM_PROGRAM:-unknown}" "${TERM_PROGRAM_VERSION:-}" \
    >>"$info/terminals.tsv"
  local shell="${SHELL:-/bin/zsh}"
  local extra=(GROFE_TRIAL_SOCKET="$SOCKET")
  if [ "${shell##*/}" = zsh ] && [ -d "$zdot" ]; then
    extra+=(ZDOTDIR="$zdot" GROFE_TRIAL_USER_ZDOTDIR="${ZDOTDIR:-$home}")
  fi
  # shellcheck disable=SC2016  # expanded by the login shell, in the trial's env
  exec env -u TMUX -u TMUX_PANE -u GROFE -u GROFE_PANE \
    XDG_CONFIG_HOME="$cfg" XDG_DATA_HOME="$data" XDG_STATE_HOME="$state" \
    XDG_RUNTIME_DIR="$run" TMUX_TMPDIR="$ttmp" \
    CANOPY_STORE="$data/canopy" CANOPY_STATE="$state/canopy" CANOPY_CONFIG="$cfg/canopy" \
    GROFE_TRIAL_ROOT="$root" GROFE_TRIAL_BIN="$bin" PATH="$bin:$PATH" "${extra[@]}" \
    "$shell" -l -c 'exec "$GROFE_TRIAL_BIN/tmux" -L "$GROFE_TRIAL_SOCKET" new-session -A -s '"$SESSION"
}

# --- status -----------------------------------------------------------------

do_status() {
  if [ ! -f "$root/$MARKER" ]; then
    say "no trial at $root (run: $0 setup)"
    return 0
  fi
  printf 'root      %s\n' "$root"
  printf 'socket    %s\n' "$sock"
  if [ -f "$info/grofe-source.tsv" ]; then
    awk -F'\t' '{ printf "%-9s %s%s\n", $1, $2, ($3 != "" ? " (" $3 ")" : "") }' "$info/grofe-source.tsv"
  fi
  if [ -f "$info/canopy-source.tsv" ]; then
    awk -F'\t' '{ printf "%-9s %s, %s\n", $1, $2, $3 }' "$info/canopy-source.tsv"
  fi
  if server_running; then
    printf 'server    running, %s session(s)\n' \
      "$(trial_env "$bin/grofe" -L "$SOCKET" list-sessions 2>/dev/null | wc -l | tr -d ' ')"
  else
    printf 'server    not running\n'
  fi
  local miss="$state/grofe/compat-miss.log"
  if [ -f "$miss" ]; then
    printf 'compat    %s line(s) in %s\n' "$(wc -l <"$miss" | tr -d ' ')" "$miss"
  else
    printf 'compat    no compat-miss.log yet\n'
  fi
  local panics
  panics="$(find "$state/grofe" -maxdepth 1 -name '*.panic.*.log' 2>/dev/null | wc -l | tr -d ' ')"
  printf 'panics    %s\n' "$panics"
}

# --- report -----------------------------------------------------------------

# redact: home to ~, the host name out, and anything shaped like a secret.
redact() {
  local host_l host_s
  host_l="$(hostname 2>/dev/null || true)"
  host_s="${host_l%%.*}"
  local script
  script="s#$(esc_pat "$home")#~#g"
  [ -n "$host_l" ] && script="$script
s#$(esc_pat "$host_l")#<host>#g"
  [ -n "$host_s" ] && script="$script
s#$(esc_pat "$host_s")#<host>#g"
  sed -e "$script" |
    sed -E \
      -e 's/(gh[pousr]_|github_pat_)[A-Za-z0-9_]{8,}/\1<redacted>/g' \
      -e 's/(sk-|xox[abpr]-)[A-Za-z0-9_-]{8,}/\1<redacted>/g' \
      -e 's/AKIA[0-9A-Z]{16}/AKIA<redacted>/g' \
      -e 's#(https?://)[^/@ ]+@#\1<redacted>@#g' \
      -e 's/([A-Za-z0-9_]*([Tt][Oo][Kk][Ee][Nn]|[Ss][Ee][Cc][Rr][Ee][Tt]|[Pp][Aa][Ss][Ss][Ww][Oo]?[Rr][Dd]|[Aa][Pp][Ii]_?[Kk][Ee][Yy])[A-Za-z0-9_]*)([=: ]+)[^ ,;"'\'']+/\1\3<redacted>/g'
}

section() { printf '\n== %s\n' "$1"; }

do_report() {
  [ -f "$root/$MARKER" ] || die "no trial at $root"
  local out
  out="$root/reports/report-$(date -u +%Y%m%dT%H%M%SZ).txt"
  mkdir -p "$root/reports"
  guard_dir "$root/reports"
  {
    printf 'grofe mac trial report, %s\n' "$(date -u '+%Y-%m-%d %H:%M UTC')"
    section versions
    cat "$info/grofe-source.tsv" "$info/canopy-source.tsv" 2>/dev/null | tr '\t' ' '
    printf 'macOS %s (%s)\n' "$(sw_vers -productVersion 2>/dev/null || echo '?')" "$(uname -m)"
    section "terminals the trial was started in (date, TERM_PROGRAM, version)"
    cat "$info/terminals.tsv" 2>/dev/null || echo "(never started)"
    section server
    if server_running; then
      printf 'running, %s session(s)\n' "$(trial_env "$bin/grofe" -L "$SOCKET" list-sessions 2>/dev/null | wc -l | tr -d ' ')"
    else
      echo "not running"
    fi
    section "config check (grofe show-config errors)"
    trial_env "$bin/grofe" show-config 2>&1 >/dev/null || true
    section "user.toml import report"
    cat "$info/user-toml.txt" 2>/dev/null || echo "(none)"
    section compat-miss.log
    cat "$state/grofe/compat-miss.log" 2>/dev/null || echo "(empty)"
    local f
    for f in "$state/grofe"/*.log "$state/grofe/plugins"/*.log "$state/canopy"/*.log; do
      [ -f "$f" ] || continue
      case "$f" in */compat-miss.log) continue ;; esac
      case "$f" in
        *.panic.*) section "${f#"$root"/} (whole)" && cat "$f" ;;
        *) section "${f#"$root"/} (last 300 lines)" && tail -n 300 "$f" ;;
      esac
    done
  } 2>&1 | redact >"$out"
  say "report: $out"
  say "read it before you share it: secrets are masked by pattern, not by knowledge"
}

# --- reset ------------------------------------------------------------------

do_reset() {
  [ -z "${GROFE_TRIAL_ROOT:-}" ] || die "you are inside the trial: detach first, then reset from another window"
  [ -e "$root" ] || {
    say "nothing at $root"
    return 0
  }
  [ -f "$root/$MARKER" ] || die "$root has no $MARKER marker, so it is not a trial root; not deleting it"
  printf 'This deletes %s and everything in it (the trial server, if running, is stopped first).\nType yes to go on: ' "$root"
  local answer=""
  read -r answer || true
  [ "$answer" = yes ] || die "not deleted"
  if server_running; then
    trial_env "$bin/grofe" -L "$SOCKET" kill-server || true
  fi
  rm -rf -- "$root"
  say "deleted $root"
}

guard_root
set_paths
case "$cmd" in
  setup) do_setup ;;
  start) do_start ;;
  status) do_status ;;
  report) do_report ;;
  reset) do_reset ;;
  *) usage ;;
esac
