#!/usr/bin/env bash
# e2e-alias-day.sh: TA.1, the alias-day container test. The owner's setup,
# athome with canopy and grofe as `tmux`, end to end in a disposable podman
# container, with a report for the owner.
#
#   GROFE_E2E_ATHOME_REPO=<athome clone> GROFE_E2E_CANOPY_REPO=<canopy clone> \
#     scripts/e2e-alias-day.sh [--keep-image]
#
# Stage 1 (here, read-only): grofe HEAD and athome's ATHOME_ALIAS_REF as `git
# archive` exports, canopy as a bundle of CANOPY_SHA from a throwaway clone;
# the secrets gate and gitleaks over all three before anything is copied.
# Stages 2 and 3 run in the container (tests/e2e/scenarios/alias-day.sh).
# The report lands in target/e2e/<run-id>/alias-day/report.md.
#
# Nothing of the host's is mounted or copied in: no ~/.secrets, no
# ~/.config/chezmoi, no ~/.ssh, no rbw state, no .env, no credential in the
# container's environment. The host-state check compares the owner's tmux
# socket directory, grofe's socket and state directories, ~/.config/canopy,
# canopy's store and the athome repo's status before and after.
#
# Every container and image the run creates is removed at the end, the build
# stage's included; --keep-image keeps the images, for a rerun that reuses
# the build cache. Exits 0 only when the report's verdict is PASS.
# Folded into scripts/e2e.sh by T4.6.
set -euo pipefail

keep_image=0
[ "${1:-}" = --keep-image ] && keep_image=1

repo="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=../tests/e2e/alias-day/pins.env disable=SC1091
. "$repo/tests/e2e/alias-day/pins.env"
: "${GROFE_E2E_ATHOME_REPO:?set GROFE_E2E_ATHOME_REPO to a local athome clone}"
: "${GROFE_E2E_CANOPY_REPO:?set GROFE_E2E_CANOPY_REPO to a local canopy clone holding CANOPY_SHA}"
command -v podman >/dev/null || { echo "e2e-alias-day: podman not found" >&2; exit 2; }
command -v gitleaks >/dev/null || { echo "e2e-alias-day: gitleaks not found" >&2; exit 2; }

grofe_sha="$(git -C "$repo" rev-parse HEAD)"
run_id="$(date -u +%Y%m%dT%H%M%SZ)-${grofe_sha:0:7}"
out="$repo/target/e2e/$run_id/alias-day"
mkdir -p "$out"
work="$(mktemp -d "${TMPDIR:-/tmp}/grofe-alias-day.XXXXXX")"
name="grofe-alias-day-$(printf '%s' "$run_id" | tr '[:upper:]' '[:lower:]')"
image="localhost/grofe-alias-day:$(printf '%s' "$run_id" | tr '[:upper:]' '[:lower:]')"
podman images -a -q --no-trunc | sort -u >"$work/images.before"

cleanup() {
  podman rm -f "$name" >/dev/null 2>&1 || true
  if [ "$keep_image" -eq 0 ]; then
    podman rmi -f "$image" >/dev/null 2>&1 || true
    # The build stage and any layer this run made: every image id that was
    # not there before it started.
    podman images -a -q --no-trunc | sort -u | comm -13 "$work/images.before" - |
      xargs -r podman rmi -f >/dev/null 2>&1 || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

log() { printf '[alias-day] %s\n' "$*" | tee -a "$out/run.log" >&2; }

# --- the host-state check ---------------------------------------------------
# Names, kinds and sizes everywhere, plus content hashes of regular files;
# sockets have no content, so their names stand for them.
host_state() {
  local uid dir
  uid="$(id -u)"
  for dir in "${TMUX_TMPDIR:-/tmp}/tmux-$uid" "/tmp/tmux-$uid" "${TMPDIR:-/tmp}/grofe-$uid" \
    "${XDG_RUNTIME_DIR:-/nonexistent}/grofe" "${XDG_STATE_HOME:-$HOME/.local/state}/grofe" \
    "$HOME/.config/canopy" "$HOME/.local/share/canopy"; do
    printf '== %s\n' "$dir"
    [ -e "$dir" ] || { echo "(absent)"; continue; }
    (cd "$dir" && find . -path ./.git -prune -o -print | LC_ALL=C sort | while read -r p; do
      if [ -f "$p" ] && [ ! -L "$p" ]; then
        printf '%s f %s\n' "$p" "$(shasum -a 256 "$p" | cut -d' ' -f1)"
      elif [ -S "$p" ]; then
        printf '%s socket\n' "$p"
      elif [ -L "$p" ]; then
        printf '%s link %s\n' "$p" "$(readlink "$p")"
      else
        printf '%s other\n' "$p"
      fi
    done)
  done
  if [ -d "$HOME/.local/share/canopy/.git" ]; then
    printf '== canopy store HEAD\n%s\n' "$(git -C "$HOME/.local/share/canopy" rev-parse HEAD)"
  fi
  printf '== athome repo status\n%s\n' "$(git -C "$GROFE_E2E_ATHOME_REPO" rev-parse HEAD)"
  git -C "$GROFE_E2E_ATHOME_REPO" status --porcelain
}
host_state >"$work/host.before"

# --- stage 1: inputs ------------------------------------------------------------
ctx="$work/ctx"
mkdir -p "$ctx/grofe" "$ctx/inputs" "$work/athome" "$work/canopy-tree"
: >"$out/stage1.tsv"
gate=pass
gate_fail() {
  printf '%s\tfail\t%s\n' "$1" "$2" >>"$out/stage1.tsv"
  gate=fail
}
gate_pass() { printf '%s\tpass\t%s\n' "$1" "$2" >>"$out/stage1.tsv"; }

log "stage 1: inputs (grofe $grofe_sha)"
git -C "$repo" archive HEAD | tar -x -C "$ctx/grofe"

athome_sha="$(git -C "$GROFE_E2E_ATHOME_REPO" rev-parse --verify "$ATHOME_ALIAS_REF^{commit}")"
git -C "$GROFE_E2E_ATHOME_REPO" archive "$athome_sha" >"$ctx/inputs/athome.tar"
tar -x -C "$work/athome" -f "$ctx/inputs/athome.tar"

git clone --quiet --no-checkout "$GROFE_E2E_CANOPY_REPO" "$work/canopy"
if git -C "$work/canopy" cat-file -e "$CANOPY_SHA^{commit}" 2>/dev/null; then
  git -C "$work/canopy" branch --quiet alias-day "$CANOPY_SHA"
  git -C "$work/canopy" bundle create --quiet "$ctx/inputs/canopy.bundle" alias-day
  git -C "$work/canopy" archive "$CANOPY_SHA" | tar -x -C "$work/canopy-tree"
  gate_pass "canopy" "bundle of $CANOPY_SHA, from a throwaway clone"
else
  gate_fail "canopy" "$CANOPY_SHA is not in $GROFE_E2E_CANOPY_REPO"
fi

pin="$(sed -n 's/^ref="\(.*\)"$/\1/p' "$work/athome/home/.chezmoiscripts/run_onchange_18-install-canopy.sh.tmpl")"
if [ "$pin" = "$CANOPY_TAG" ]; then
  gate_pass "canopy pin" "athome's run_onchange_18 ref= is CANOPY_TAG"
else
  gate_fail "canopy pin" "athome's run_onchange_18 ref=$pin, pins.env CANOPY_TAG=$CANOPY_TAG"
fi
grofe_pin="$(sed -n 's/^ref="\(.*\)"$/\1/p' "$work/athome/home/.chezmoiscripts/run_onchange_17-install-grofe.sh.tmpl" 2>/dev/null || true)"

# The secrets gate, on athome's tree: nothing under a .secrets directory, no
# .env file, no .envrc but athome's shipped examples.
bad="$(cd "$work/athome" && find . \( -path '*/.secrets' -o -path '*/.secrets/*' -o -name '.env' -o -name '*.env' \
  -o \( -name '.envrc' -o -name 'dot_envrc*' \) \) -print | LC_ALL=C sort)"
if [ -z "$bad" ]; then
  gate_pass "secrets gate" "no .secrets/, .env or .envrc in the athome archive"
else
  gate_fail "secrets gate" "athome archive holds: $(printf '%s' "$bad" | tr '\n' ' ')"
fi

leaks=0
for tree in grofe:"$ctx/grofe" athome:"$work/athome" canopy:"$work/canopy-tree"; do
  label="${tree%%:*}"
  dir="${tree#*:}"
  rc=0
  gitleaks dir --no-banner --redact --exit-code 1 --report-format json \
    --report-path "$work/gitleaks-$label.json" "$dir" >"$work/gitleaks-$label.log" 2>&1 || rc=$?
  n="$(jq length "$work/gitleaks-$label.json" 2>/dev/null || echo "?")"
  if [ "$rc" -eq 0 ]; then
    gate_pass "gitleaks $label" "0 findings"
  else
    gate_fail "gitleaks $label" "exit $rc, $n finding(s)"
    leaks=$((leaks + ${n//[!0-9]/0}))
  fi
done
rc=0
gitleaks git --no-banner --redact --exit-code 1 --log-opts=alias-day --report-format json \
  --report-path "$work/gitleaks-canopy-history.json" "$work/canopy" >"$work/gitleaks-canopy-history.log" 2>&1 || rc=$?
if [ "$rc" -eq 0 ]; then
  gate_pass "gitleaks canopy history" "0 findings in the bundle's commits"
else
  gate_fail "gitleaks canopy history" "exit $rc"
  leaks=$((leaks + 1))
fi
log "stage 1: secrets gate $gate, gitleaks findings $leaks"

# --- stages 2 and 3: the container ----------------------------------------------
stage2=fail
stage3=fail
image_id="(not built)"
if [ "$gate" = pass ]; then
  log "building the image"
  if timeout 3000 podman build --quiet -f "$ctx/grofe/tests/e2e/Containerfile.alias-day" -t "$image" "$ctx" \
    >"$out/build.log" 2>&1; then
    image_id="$(podman image inspect --format '{{.Id}}' "$image")"
    log "image $image_id; starting the container"
    podman run -d --name "$name" --hostname e2e --systemd=always "$image" >/dev/null
    podman exec "$name" timeout 120 systemctl is-system-running --wait >"$out/boot.log" 2>&1 || true
    for _ in $(seq 1 60); do
      podman exec "$name" test -S /run/user/1000/bus && break
      sleep 1
    done
    as_test=(podman exec -u test -w /home/test -e HOME=/home/test -e USER=test -e LOGNAME=test "$name")
    log "stage 2: apply"
    if "${as_test[@]}" bash /opt/e2e/alias-day.sh apply >"$out/stage2.log" 2>&1; then
      stage2=pass
    fi
    log "stage 2: $stage2; stage 3: workflow"
    if "${as_test[@]}" bash /opt/e2e/alias-day.sh workflow >"$out/stage3.log" 2>&1; then
      stage3=pass
    fi
    log "stage 3: $stage3"
    podman exec "$name" env >"$work/container.env" 2>&1 || true
    podman cp "$name:/home/test/e2e-out" "$out/out" >/dev/null 2>&1 || true
    podman exec "$name" journalctl --no-pager -b >"$out/journal.log" 2>&1 || true
  else
    log "the image did not build: see build.log"
  fi
fi
creds="$(grep -E '^(GITHUB_TOKEN|GH_TOKEN|BW_[A-Z_]*|RBW_[A-Z_]*)=' "$work/container.env" 2>/dev/null | cut -d= -f1 | tr '\n' ' ' || true)"

# The run is over: the container goes now, so the host check sees no podman
# process of ours.
podman rm -f "$name" >/dev/null 2>&1 || true
host_state >"$work/host.after"
if cmp -s "$work/host.before" "$work/host.after"; then
  host="byte-identical: yes, for every directory below"
  host_ok=1
else
  host="byte-identical: NO; the diff:"$'\n\n```\n'"$(diff "$work/host.before" "$work/host.after" | head -40)"$'\n```'
  host_ok=0
fi
host="$host"$'\n\n'"$(grep '^== ' "$work/host.before" | sed 's/^== /- /')"

# --- the report ---------------------------------------------------------------
o="$out/out"
blocks="$work/blocks"
mkdir -p "$blocks"
awk -F'\t' '{ printf "- %s: %s, %s\n", $1, $2, $3 }' "$out/stage1.tsv" >"$blocks/stage1"
skip="$repo/tests/e2e/alias-day/skip.tsv"
: >"$blocks/scripts"
while IFS=$'\t' read -r script action how why; do
  case "$script" in '#'* | '') continue ;; esac
  row="$(awk -F'\t' -v s="$script" '$1 == s' "$o/scripts.tsv" 2>/dev/null || true)"
  if [ -n "$row" ]; then
    IFS=$'\t' read -r _ ran how_ran exit_code <<<"$row"
  elif [ "$action" = absent ]; then
    if [ -e "$work/athome/home/.chezmoiscripts/$script" ] || [ -e "$work/athome/home/.chezmoiscripts/$script.tmpl" ]; then
      ran="PRESENT" how_ran="$how" exit_code="-"
    else
      ran="absent" how_ran="$how" exit_code="-"
    fi
  else
    ran="not reached" how_ran="$how" exit_code="-"
  fi
  printf '| %s | %s | %s | %s | %s |\n' "$script" "$ran" "$how_ran" "$exit_code" "$why" >>"$blocks/scripts"
done <"$skip"
awk -F'\t' '{ printf "- %s: %s\n", $1, $2 }' "$o/checks.tsv" >"$blocks/checks" 2>/dev/null || echo "- (stage 2 checks did not run)" >"$blocks/checks"
[ -s "$blocks/checks" ] || echo "- (stage 2 checks did not run)" >"$blocks/checks"
awk -F'\t' '{ printf "| %s | %s | %s | %s | %s |\n", $1, $2, $3, $4, $5 }' "$o/steps.tsv" >"$blocks/steps" 2>/dev/null || true
[ -s "$blocks/steps" ] || echo "| - | the workflow did not run | fail | | |" >"$blocks/steps"
if [ -s "$o/compat-miss.log" ]; then
  { echo '```'; cat "$o/compat-miss.log"; echo '```'; } >"$blocks/compat"
elif [ -e "$o/steps.tsv" ] && grep -q '^W14	.*	pass	' "$o/steps.tsv"; then
  echo "empty" >"$blocks/compat"
else
  echo "not read (W14 did not pass)" >"$blocks/compat"
fi
printf '%s\n' "$host" >"$blocks/host"

steps_ok=0
if [ -s "$o/steps.tsv" ] && ! awk -F'\t' '$3 != "pass"' "$o/steps.tsv" | grep -q .; then
  [ "$(wc -l <"$o/steps.tsv" | tr -d ' ')" -eq 15 ] && steps_ok=1
fi
if [ "$gate" != pass ]; then
  verdict="FAIL: the secrets gate (stage 1)"
elif [ "$stage2" != pass ]; then
  verdict="FAIL: stage 2 (the apply or its checks)"
elif [ "$stage3" != pass ] || [ "$steps_ok" -ne 1 ]; then
  verdict="FAIL: stage 3, $(awk -F'\t' '$3 != "pass" { print $1; exit }' "$o/steps.tsv" 2>/dev/null || echo "the workflow")"
elif [ -n "$creds" ]; then
  verdict="FAIL: credentials in the container env"
elif [ "$host_ok" -ne 1 ]; then
  verdict="FAIL: the host-state check"
else
  verdict="PASS"
fi
init_line="$(awk -F'\t' '{ printf "`%s` exited %s.", $1, $2 }' "$o/init.tsv" 2>/dev/null || echo "chezmoi init did not run.")"
chezmoi_v="$(sed -n 's/^ARG CHEZMOI_VERSION=//p' "$repo/tests/e2e/Containerfile.alias-day")"
tmux_v="$(sed -n 's/^ARG TMUX_VERSION=//p' "$repo/tests/e2e/Containerfile.alias-day")"

fill() {
  local line key
  while IFS= read -r line; do
    if [[ "$line" =~ ^@@BLOCK:([a-z0-9]+)@@$ ]]; then
      cat "$blocks/${BASH_REMATCH[1]}"
      continue
    fi
    for key in RUN_ID DATE VERDICT GROFE_SHA ATHOME_REF ATHOME_SHA CANOPY_TAG CANOPY_SHA IMAGE_ID \
      CHEZMOI_VERSION REAL_TMUX ATHOME_GROFE_PIN SECRETS_GATE GITLEAKS CREDENTIALS INIT_LINE SKIP_PASSES; do
      line="${line//@@$key@@/${vals[$key]}}"
    done
    printf '%s\n' "$line"
  done <"$repo/tests/e2e/alias-day/report.md.tmpl"
}
declare -A vals=(
  [RUN_ID]="$run_id" [DATE]="$(date -u '+%Y-%m-%d %H:%M UTC')" [VERDICT]="$verdict"
  [GROFE_SHA]="$grofe_sha" [ATHOME_REF]="$ATHOME_ALIAS_REF" [ATHOME_SHA]="$athome_sha"
  [CANOPY_TAG]="$CANOPY_TAG" [CANOPY_SHA]="$CANOPY_SHA" [IMAGE_ID]="$image_id"
  [CHEZMOI_VERSION]="$chezmoi_v" [REAL_TMUX]="$tmux_v" [ATHOME_GROFE_PIN]="${grofe_pin:-none}"
  [SECRETS_GATE]="$gate" [GITLEAKS]="$leaks findings" [CREDENTIALS]="${creds:-none}"
  [INIT_LINE]="$init_line"
  [SKIP_PASSES]="none: every skipped script is one no workflow step depends on, and each is listed in the stage 2 table"
)
fill >"$out/report.md"
log "report: $out/report.md"
log "verdict: $verdict"
[ "$verdict" = PASS ]
