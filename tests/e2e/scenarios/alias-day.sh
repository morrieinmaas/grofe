#!/usr/bin/env bash
# The checks name paths as the report prints them, and run their own shells.
# shellcheck disable=SC2016,SC2088
# alias-day.sh <stage>: the in-container half of TA.1, the alias-day test.
# Run by scripts/e2e-alias-day.sh as the test user, never on a host.
#
#   apply      stage 2: athome applied from its archive, the way bootstrap
#              does it (chezmoi init --apply --force --exclude=scripts, then
#              the allowlisted scripts rendered and run in chezmoi's order),
#              then the stage 2 checks, then the login unit started
#   workflow   stage 3: tests/e2e_alias_day.rs, the owner's workflow through
#              a pty, against the server the login unit started
#
# Everything a report needs is written under $OUT (~/e2e-out), as TSV rows
# and logs the host runner fills the report from.
set -uo pipefail

[ -e /run/.containerenv ] || { echo "alias-day.sh: refusing to run outside a container" >&2; exit 2; }
[ "$(id -un)" = test ] || { echo "alias-day.sh: run as the test user" >&2; exit 2; }

E2E=/opt/e2e
OUT="$HOME/e2e-out"
SRC="$HOME/athome-src"
STORE="$HOME/.local/share/canopy"
GROFE_IMAGE_BIN=/usr/local/lib/grofe/grofe
# shellcheck source=../alias-day/pins.env disable=SC1091
. "$E2E/alias-day/pins.env"
XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
mkdir -p "$OUT"

# check <name> <command...>: one stage 2 check, its result as a row.
check() {
  local name="$1"
  shift
  if "$@" >>"$OUT/checks.log" 2>&1; then
    printf '%s\tpass\n' "$name" >>"$OUT/checks.tsv"
  else
    printf '%s\tfail\n' "$name" >>"$OUT/checks.tsv"
    fails=$((fails + 1))
  fi
}

# The order chezmoi runs scripts in: before_ first, then plain, then
# after_, each by name with its attributes stripped.
script_order() {
  local f base phase name
  for f in "$SRC"/home/.chezmoiscripts/*; do
    base="${f##*/}"
    base="${base%.tmpl}"
    case "$base" in
      *_before_*) phase=0 ;;
      *_after_*) phase=2 ;;
      *) phase=1 ;;
    esac
    name="$(printf '%s' "$base" | sed -E 's/^run_((once|onchange)_)?((before|after)_)?//')"
    printf '%s\t%s\t%s\n' "$phase" "$name" "$f"
  done | sort -t "$(printf '\t')" -k1,1n -k2,2 | cut -f3
}

stage_apply() {
  fails=0
  : >"$OUT/checks.tsv"
  : >"$OUT/scripts.tsv"
  mkdir -p "$OUT/scripts"

  mkdir -p "$SRC"
  tar -x -C "$SRC" -f "$E2E/inputs/athome.tar"
  local table="$E2E/alias-day/skip.tsv"

  # Every athome script has a row, and no absent row is present.
  local f base row missing=0
  for f in "$SRC"/home/.chezmoiscripts/*; do
    base="${f##*/}"
    base="${base%.tmpl}"
    row="$(awk -F'\t' -v s="$base" '$1 == s' "$table")"
    if [ -z "$row" ]; then
      echo "skip.tsv has no row for $base" >>"$OUT/stage2.err"
      missing=1
    elif [ "$(printf '%s' "$row" | cut -f2)" = absent ]; then
      echo "$base must be absent from the alias-day ref" >>"$OUT/stage2.err"
      missing=1
    fi
  done
  [ "$missing" -eq 0 ] || { echo "stage 2: the script table does not match the archive" >&2; return 1; }

  # 1. Seed chezmoi's config, generated from the archive.
  mkdir -p "$HOME/.config/chezmoi"
  bash "$E2E/alias-day/seed.sh" "$SRC" >"$HOME/.config/chezmoi/chezmoi.toml" || return 1

  # 2. canopy's store, a clone of the bundle, so run_onchange_18 takes its
  # update path offline. The bundle carries one branch and no HEAD, so the
  # clone names it; script 18 then detaches at the pinned commit.
  mkdir -p "$(dirname "$STORE")"
  git -c init.defaultBranch=alias-day clone --quiet --branch alias-day "$E2E/inputs/canopy.bundle" "$STORE" || return 1

  # 3. The baseline apply, bootstrap's own call. stdin is /dev/null, so a
  # prompt is an error, never a hang.
  local rc=0
  timeout 300 chezmoi init --apply --force --exclude=scripts --source="$SRC/home" \
    </dev/null >"$OUT/chezmoi-init.log" 2>&1 || rc=$?
  printf 'chezmoi init --apply --force --exclude=scripts\t%s\n' "$rc" >"$OUT/init.tsv"
  [ "$rc" -eq 0 ] || { echo "stage 2: chezmoi init exited $rc" >&2; return 1; }

  # 4. The script table, in chezmoi's order.
  local action how env_args log
  while read -r f; do
    base="${f##*/}"
    base="${base%.tmpl}"
    row="$(awk -F'\t' -v s="$base" '$1 == s' "$table")"
    action="$(printf '%s' "$row" | cut -f2)"
    how="$(printf '%s' "$row" | cut -f3)"
    case "$action" in
      run) env_args=(-u ATHOME_CI) ;;
      ci) env_args=(ATHOME_CI=1) ;;
      flag) env_args=(-u ATHOME_CI "$how") ;;
      *)
        printf '%s\tskipped\t%s\t-\n' "$base" "$how" >>"$OUT/scripts.tsv"
        continue
        ;;
    esac
    log="$OUT/scripts/$base.log"
    rc=0
    chezmoi execute-template --source="$SRC/home" <"$f" >"$OUT/scripts/$base" 2>"$log" || rc=$?
    if [ "$rc" -eq 0 ]; then
      (cd "$HOME" && timeout 600 env "${env_args[@]}" bash "$OUT/scripts/$base" </dev/null) >>"$log" 2>&1 || rc=$?
    fi
    case "$action" in
      run) how="ran, ATHOME_CI unset" ;;
      ci) how="ran with ATHOME_CI=1" ;;
      flag) how="ran with $how" ;;
    esac
    printf '%s\tran\t%s\t%s\n' "$base" "$how" "$rc" >>"$OUT/scripts.tsv"
    [ "$rc" -eq 0 ] || fails=$((fails + 1))
  done < <(script_order)

  # Prewarm what a first interactive shell fetches (zinit and its plugins),
  # so the login-zsh checks and stage 3 start from a machine that has been
  # used once, and zinit's install output is not mistaken for theirs.
  zsh -lic 'exit' </dev/null >"$OUT/zsh-first-run.log" 2>&1 || true
  # The same for nvim: its first start clones vim.pack's 38 plugins and
  # builds the treesitter parsers, all over the network. Left to W11, that
  # raced the step's timeouts, and a slow clone failed it.
  local nvim_rc=0
  (cd /tmp && timeout 400 nvim --headless -c "luafile $E2E/alias-day/nvim-first-run.lua") \
    </dev/null >"$OUT/nvim-first-run.log" 2>&1 || nvim_rc=$?

  # 5. The stage 2 checks.
  local home_bin="$HOME/.local/lib/grofe"
  check "the dummy handle is in the rendered chezmoi.toml" \
    grep -q 'githubHandle = "athome-ci"' "$HOME/.config/chezmoi/chezmoi.toml"
  check "~/.zshrc, tmux.conf and canopy's user.conf exist" \
    test -f "$HOME/.zshrc" -a -f "$HOME/.config/tmux/tmux.conf" -a -f "$HOME/.config/canopy/user.conf"
  check "grofe's config tree exists: config.toml, user.toml, six canopy fragments" \
    bash -c 'test -f ~/.config/grofe/config.toml -a -f ~/.config/grofe/user.toml &&
      test "$(ls ~/.config/grofe/conf.d/[0-9][0-9]-canopy-*.toml | wc -l)" -eq 6'
  check "canopy's store is detached at CANOPY_SHA" \
    bash -c "test \"\$(git -C '$STORE' rev-parse HEAD)\" = '$CANOPY_SHA' && ! git -C '$STORE' symbolic-ref -q HEAD"
  check "in a login zsh, tmux is a symlink to the grofe binary" \
    bash -c "p=\"\$(zsh -lic 'print -r -- TMUXPATH=\$(command -v tmux)' </dev/null 2>/dev/null | sed -n 's/.*TMUXPATH=//p' | tail -n 1)\";
      echo \"tmux: \$p\"; test -L \"\$p\" && test \"\$(readlink -f \"\$p\")\" = '$home_bin/grofe' &&
      cmp -s '$home_bin/grofe' '$GROFE_IMAGE_BIN'"
  check "in a login zsh, tmux -V prints the compat level (tmux 3.7b)" \
    bash -c "v=\"\$(zsh -lic 'tmux -V' </dev/null 2>/dev/null | tail -n 1)\"; echo \"\$v\"; test \"\$v\" = 'tmux 3.7b'"
  check "/usr/local/bin/tmux is real tmux 3.7b, and not first on PATH" \
    bash -c "test \"\$(/usr/local/bin/tmux -V)\" = 'tmux 3.7b' &&
      p=\"\$(zsh -lic 'print -r -- TMUXPATH=\$(command -v tmux)' </dev/null 2>/dev/null | sed -n 's/.*TMUXPATH=//p' | tail -n 1)\" &&
      test \"\$p\" != /usr/local/bin/tmux"
  check "nvim's first run installed its plugins (vim-tmux-navigator among them) and treesitter parsers" \
    bash -c "echo 'first run exit $nvim_rc'; test $nvim_rc -eq 0 &&
      test -d ~/.local/share/nvim/site/pack/core/opt/vim-tmux-navigator &&
      ls ~/.local/share/nvim/site/parser/lua.so ~/.local/share/nvim/site/parser/markdown.so &&
      ! tr '\\r' '\\n' <'$OUT/nvim-first-run.log' | grep -E ' error: |^E[0-9]+:'"
  check "no ~/.secrets, ~/.local/share/rbw or ~/.config/rbw" \
    bash -c 'test ! -e ~/.secrets -a ! -e ~/.local/share/rbw -a ! -e ~/.config/rbw'
  check "grofe show-config loads the installed tree with no errors" \
    bash -c "'$home_bin/bin/grofe' show-config >'$OUT/show-config.txt' 2>'$OUT/show-config.err' && test ! -s '$OUT/show-config.err'"
  check "no credential in the environment (GITHUB_TOKEN, GH_TOKEN, BW_*, RBW_*)" \
    bash -c "! env | grep -E '^(GITHUB_TOKEN|GH_TOKEN|BW_[A-Z_]*|RBW_[A-Z_]*)=' && ! sudo cat /proc/1/environ | tr '\\0' '\\n' | grep -E '^(GITHUB_TOKEN|GH_TOKEN|BW_|RBW_)'"

  # The login: athome's tmux.service, which script 24 enabled. Its
  # graphical-session.target never comes up in a headless container, so the
  # unit is started by hand, as the target would start it.
  check "tmux.service is enabled (script 24)" systemctl --user is-enabled tmux.service
  check "tmux.service starts a grofe server, which answers #{grofe_version}" \
    bash -c "systemctl --user start tmux.service && systemctl --user is-active tmux.service &&
      v=\"\$(PATH='$home_bin/bin':\$PATH tmux display -p '#{grofe_version}')\" && echo \"grofe_version=\$v\" && test -n \"\$v\""
  systemctl --user status tmux.service --no-pager >"$OUT/tmux.service.status" 2>&1 || true


  echo "stage 2: $fails failure(s)"
  awk -F'\t' '$2 == "ran" && $4 != 0 { print "  script " $1 " exited " $4 }' "$OUT/scripts.tsv"
  awk -F'\t' '$2 == "fail" { print "  check failed: " $1 }' "$OUT/checks.tsv"
  [ "$fails" -eq 0 ]
}

stage_workflow() {
  local rc=0
  GROFE_E2E_ALIAS_DAY=1 GROFE_E2E_OUT="$OUT" GROFE_E2E_GOLDEN="$E2E/golden" \
    timeout 1500 /usr/local/lib/grofe-e2e/e2e_alias_day --ignored --test-threads=1 --nocapture \
    >"$OUT/workflow.log" 2>&1 || rc=$?
  echo "stage 3: the workflow test exited $rc"
  return "$rc"
}

case "${1:-}" in
  apply) stage_apply ;;
  workflow) stage_workflow ;;
  *)
    echo "usage: alias-day.sh apply|workflow" >&2
    exit 2
    ;;
esac
