#!/usr/bin/env bash
# reboot.sh <phase>: the in-container half of T4.6, the container reboot.
# Run by scripts/e2e.sh as the test user, one phase per call, never on a
# host. The runs e2e.sh makes of it, each in a fresh container from the
# same image (a step `restart` is `podman restart`, which kills every
# process, empties the /run/user tmpfs and keeps the disk):
#
# e2e-run: normal 1 1b restart 2
# e2e-run: mutation! 1 1b mutate restart 2
#
#   1       grofe through the `tmux` symlink: two sessions, three fake-agent
#           panes pinned at launch and bound with `pane-set-resume` from
#           their own process (as an adapter would), a pane whose agent was
#           then closed, a plain shell after `cd`; waits for `reboot-check`
#           to exit 0 naming every pane
#   1b      SIGTERM to every process of the test user, as a reboot or logout
#           does before the kill; state.json must still hold every session
#           and pane
#   mutate  (the mutation run) changes agent b's saved resume to `--resume
#           zz` in state.json, and prints `MUTATED <uid>`
#   2       "boots" with `env -i ... tmux start-server --restore`, as a login
#           unit would, then checks every pane
#
# Every pane check prints `PASS <uid> ...` or `FAIL <uid> ...`, a check not
# tied to one pane `FAIL - ...`; a phase exits nonzero on any FAIL. In the
# mutation run e2e.sh wants phase 2 to fail on the MUTATED uids only.
set -uo pipefail

[ -e /run/.containerenv ] || { echo "reboot.sh: refusing to run outside a container" >&2; exit 2; }
[ "$(id -un)" = test ] || { echo "reboot.sh: run as the test user" >&2; exit 2; }

uid="$(id -u)"
export XDG_RUNTIME_DIR="/run/user/$uid"
export FAKE_AGENT_HOME="$HOME/transcripts"
WORK="$HOME/work"
REC="$HOME/e2e"
STATE_DIR="$HOME/.local/state/grofe"
mkdir -p "$REC"
fails=0

pass() { printf 'PASS %s\n' "$*"; }
fail() {
  printf 'FAIL %s\n' "$*"
  fails=$((fails + 1))
}
die() {
  printf 'FAIL - %s\n' "$*"
  exit 1
}

# wait_for <seconds> <command...>: polls every 0.2 s until the command
# succeeds; nonzero when it never did.
wait_for() {
  local tries=$(($1 * 5))
  shift
  while [ "$tries" -gt 0 ]; do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.2
    tries=$((tries - 1))
  done
  return 1
}

# The one instance's state.json (the default socket under XDG_RUNTIME_DIR).
state_json() {
  local f
  for f in "$STATE_DIR"/*/state.json; do
    [ -e "$f" ] && printf '%s\n' "$f" && return 0
  done
  return 1
}

transcript() { cat "$FAKE_AGENT_HOME/$1.transcript" 2>/dev/null; }
transcript_is() { [ "$(transcript "$1")" = "$2" ]; }
pane_field() { tmux display-message -p -t "$1" "$2"; }
# pane_field_is <pane> <format> <value>, for wait_for: evaluated each try.
pane_field_is() { [ "$(pane_field "$1" "$2")" = "$3" ]; }
pane_field_set() { [ -n "$(pane_field "$1" "$2")" ]; }

phase1() {
  export PATH="/opt/e2e/bin:/usr/bin:/bin"
  mkdir -p "$WORK"/{a,b,c,closed,plain} "$FAKE_AGENT_HOME"
  # What an agent with an adapter does: binds its own resume, then is the
  # process that resume is bound to (the exec keeps the pid). The agent's
  # PATH holds fake-agent (a login shell's profile may reset the pane's);
  # the boot's never does.
  cat >"$HOME/agent-launch" <<'EOF'
#!/bin/sh
PATH="/opt/e2e/bin:$PATH"
grofe pane-set-resume -- fake-agent --resume "$1" || exit 1
exec fake-agent --session-id "$1"
EOF
  chmod 755 "$HOME/agent-launch"
  [ "$(readlink -f "$(command -v tmux)")" = /usr/local/lib/grofe/grofe ] || die "tmux is not grofe"

  tmux new-session -d -s alpha -c "$WORK/a" || die "new-session alpha"
  tmux new-window -d -t alpha:1 -c "$WORK/b" || die "new-window alpha:1"
  tmux split-window -d -t alpha:1 -c "$WORK/closed" || die "split-window alpha:1"
  tmux new-session -d -s beta -c "$WORK/c" || die "new-session beta"
  tmux new-window -d -t beta:1 -c "$HOME" || die "new-window beta:1"

  local a b d c plain
  a="$(pane_field alpha:0.0 '#{pane_id}')"
  b="$(pane_field alpha:1.0 '#{pane_id}')"
  d="$(pane_field alpha:1.1 '#{pane_id}')"
  c="$(pane_field beta:0.0 '#{pane_id}')"
  plain="$(pane_field beta:1.0 '#{pane_id}')"
  tmux send-keys -t "$a" "$HOME/agent-launch a" Enter
  tmux send-keys -t "$b" "$HOME/agent-launch b" Enter
  tmux send-keys -t "$c" "$HOME/agent-launch c" Enter
  tmux send-keys -t "$d" "$HOME/agent-launch d" Enter
  tmux send-keys -t "$plain" "cd $WORK/plain" Enter
  local id
  for id in a b c d; do
    wait_for 10 transcript_is "$id" "launch $id" || die "agent $id never launched: $(transcript "$id")"
  done
  # The closed agent: C-c ends it and its shell stays; its resume goes
  # with its process.
  wait_for 10 pane_field_set "$d" '#{pane_resume}' || die "agent d never bound its resume"
  tmux send-keys -t "$d" C-c
  wait_for 10 pane_field_is "$d" '#{pane_resume}' '' || die "agent d's resume outlived it"
  wait_for 10 pane_field_is "$plain" '#{pane_current_path}' "$WORK/plain" || die "the cd pane never reached $WORK/plain"

  # The record phase 2 compares against: one line per pane.
  local p
  : >"$REC/phase1.tsv"
  for p in "a:$a" "b:$b" "c:$c" "closed:$d" "plain:$plain"; do
    printf '%s\t%s\n' "${p%%:*}" \
      "$(pane_field "${p#*:}" $'#{pane_uid}\t#{session_name}:#{window_index}\t#{pane_current_path}\t#{pane_resume}')" \
      >>"$REC/phase1.tsv"
  done
  cat "$REC/phase1.tsv"
  awk -F'\t' '($1 ~ /^[abc]$/ && $5 == "") || ($1 !~ /^[abc]$/ && $5 != "") { bad = 1 } END { exit bad }' \
    "$REC/phase1.tsv" || die "the resumes are not what phase 1 set"

  local rc=1 out
  for _ in $(seq 1 50); do
    out="$(grofe reboot-check 2>&1)"
    rc=$?
    [ "$rc" -eq 0 ] && break
    sleep 0.2
  done
  printf '%s\n' "$out"
  [ "$rc" -eq 0 ] || die "reboot-check exited $rc"
  pass "- reboot-check exited 0"
  local role puid where
  while IFS=$'\t' read -r role puid where _; do
    case "$role" in
      a | b | c) want="will resume" ;;
      *) want="restores as a shell" ;;
    esac
    if printf '%s\n' "$out" | grep -q "^$where\.[0-9]* $puid $want"; then
      pass "$puid $role reboot-check: $want"
    else
      fail "$puid $role reboot-check does not say '$want' for it"
    fi
  done <"$REC/phase1.tsv"
}

phase1b() {
  [ -s "$REC/phase1.tsv" ] || die "phase 1 left no record"
  # kill(-1) reaches every process this user may signal but the caller.
  kill -TERM -1 2>/dev/null
  # shellcheck disable=SC2016 # the inner shell expands it
  wait_for 10 sh -c '! pgrep -u "$(id -u)" -x grofe' || fail "- grofe outlived SIGTERM"
  local f
  f="$(state_json)" || die "no state.json after SIGTERM"
  printf 'state.json: %s\n' "$f"
  local sessions
  sessions="$(jq -r '[.sessions[].name] | sort | join(" ")' "$f")"
  if [ "$sessions" = "alpha beta" ]; then
    pass "- state.json lists sessions: $sessions"
  else
    fail "- state.json lists sessions '$sessions', not 'alpha beta'"
  fi
  local role puid
  while IFS=$'\t' read -r role puid _; do
    if jq -e --arg u "$puid" '[.sessions[].windows[].panes[].uid] | index($u) != null' "$f" >/dev/null; then
      pass "$puid $role in state.json"
    else
      fail "$puid $role missing from state.json"
    fi
  done <"$REC/phase1.tsv"
}

mutate() {
  local f puid
  [ -s "$REC/phase1.tsv" ] || die "phase 1 left no record"
  f="$(state_json)" || die "no state.json to mutate"
  puid="$(awk -F'\t' '$1 == "b" { print $2 }' "$REC/phase1.tsv")"
  jq --arg u "$puid" '(.sessions[].windows[].panes[] | select(.uid == $u) | .resume) |= (.[0:-1] + ["zz"])' \
    "$f" >"$f.tmp" || die "jq could not edit state.json"
  chmod 600 "$f.tmp" && mv "$f.tmp" "$f"
  jq -c --arg u "$puid" '.sessions[].windows[].panes[] | select(.uid == $u) | .resume' "$f"
  printf 'MUTATED %s\n' "$puid"
}

# The command a pane's own process runs, from /proc.
pane_argv() { tr '\0' ' ' <"/proc/$1/cmdline" 2>/dev/null; }

phase2() {
  [ -s "$REC/phase1.tsv" ] || die "phase 1 left no record"
  # The reboot: nothing of phase 1's runtime is left.
  if [ -z "$(ls -A "$XDG_RUNTIME_DIR" 2>/dev/null)" ]; then
    pass "- $XDG_RUNTIME_DIR is empty after the restart"
  else
    fail "- $XDG_RUNTIME_DIR still holds: $(find "$XDG_RUNTIME_DIR" -mindepth 1 | tr '\n' ' ')"
  fi
  pgrep -u "$uid" -x grofe >/dev/null && fail "- a grofe process survived the restart"
  # The boot, as a login unit would run it: its environment is the unit's
  # (a user unit has XDG_RUNTIME_DIR), not the snapshot's, and its PATH
  # does not hold fake-agent.
  env -i HOME="$HOME" PATH=/usr/bin:/bin XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    FAKE_AGENT_HOME="$FAKE_AGENT_HOME" tmux start-server --restore || die "start-server --restore failed"
  export PATH=/usr/bin:/bin

  # Resumes start within restore-stagger-ms (1000) of the restore.
  local id
  for id in a b c; do
    wait_for 10 transcript_is "$id" "launch $id"$'\n'"resume $id" || true
  done
  sleep 1
  tmux list-panes -a -F $'#{pane_uid}\t#{session_name}:#{window_index}\t#{pane_current_path}\t#{pane_pid}'
  local now
  now="$(tmux list-panes -a -F $'#{pane_uid}\t#{session_name}:#{window_index}\t#{pane_current_path}\t#{pane_pid}')"
  local role puid where cwd line nwhere ncwd npid argv
  while IFS=$'\t' read -r role puid where cwd _; do
    line="$(printf '%s\n' "$now" | awk -F'\t' -v u="$puid" '$1 == u')"
    if [ -z "$line" ]; then
      fail "$puid $role is not in the restored server"
      continue
    fi
    IFS=$'\t' read -r _ nwhere ncwd npid <<<"$line"
    argv="$(pane_argv "$npid")"
    case "$role" in
      a | b | c)
        if [ "$nwhere" != "$where" ]; then
          fail "$puid $role is in $nwhere, not $where"
        elif ! transcript_is "$role" "launch $role"$'\n'"resume $role"; then
          fail "$puid $role transcript is '$(transcript "$role" | tr '\n' ',')' (pane runs: $argv)"
        else
          pass "$puid $role in $nwhere: launch $role, resume $role"
        fi
        ;;
      *)
        if [ "$nwhere" != "$where" ]; then
          fail "$puid $role is in $nwhere, not $where"
        elif [ "$ncwd" != "$cwd" ]; then
          fail "$puid $role is in $ncwd, not $cwd"
        elif [ "$(cat "/proc/$npid/comm" 2>/dev/null)" != bash ]; then
          fail "$puid $role runs '$argv', not a shell"
        elif [ "$role" = closed ] && ! transcript_is d "launch d"; then
          fail "$puid $role resurrected its closed agent: '$(transcript d | tr '\n' ',')'"
        else
          pass "$puid $role in $nwhere: a shell in $ncwd"
        fi
        ;;
    esac
  done <"$REC/phase1.tsv"
  local want got
  want="$(cut -f2 "$REC/phase1.tsv" | sort | tr '\n' ' ')"
  got="$(printf '%s\n' "$now" | cut -f1 | sort | tr '\n' ' ')"
  if [ "$want" = "$got" ]; then
    pass "- the restored uids equal phase 1's"
  else
    fail "- the restored uids are '$got', phase 1 had '$want'"
  fi
}

case "${1:-}" in
  1) phase1 ;;
  1b) phase1b ;;
  mutate) mutate ;;
  2) phase2 ;;
  *) echo "usage: reboot.sh 1|1b|mutate|2" >&2; exit 2 ;;
esac
[ "$fails" -eq 0 ]
