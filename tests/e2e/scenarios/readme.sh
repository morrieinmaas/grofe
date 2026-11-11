#!/usr/bin/env bash
# readme.sh: TE.3's alias-day run, the in-container half. The "Alias
# day" steps, extracted from docs/alias-day.md and run as written, in a clean
# systemd container (tests/e2e/Containerfile.alias-day, a fresh one per
# run). Run by scripts/release-gate.sh as the test user, never on a host.
#
# What runs: every fenced block between `<!-- alias-day: begin -->` and
# `<!-- alias-day: end -->` in docs/alias-day.md, region by region, each with
# `bash -euxo pipefail` in the test user's home, on the PATH a login zsh
# has (athome's ~/.zsh/path.zsh), so any failing command fails its step.
# The steps the doc marks for the owner alone (step 1's athome edit and
# apply, the reboot, the week of work) have no markers and do not run.
# Step 1's apply has its substitute run first by the gate: TA.1's stage 2,
# which applies athome the way bootstrap does on a machine with no package
# manager branch for Debian, as the plan names it.
#
# A region whose block is missing a command it must have (the commands
# below) fails the run, so a step cannot drop out of the doc unseen.
set -uo pipefail

[ -e /run/.containerenv ] || { echo "readme.sh: refusing to run outside a container" >&2; exit 2; }
[ "$(id -un)" = test ] || { echo "readme.sh: run as the test user" >&2; exit 2; }

XDG_RUNTIME_DIR="/run/user/$(id -u)"
export XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
README=/opt/e2e/alias-day.md
OUT="$HOME/e2e-out/readme"
REQUIRED=('canopy/bin/canopy doctor' 'grofe reboot-check' 'rm ~/.local/lib/grofe/bin/tmux'
  'canopy/bin/canopy install --yes' 'canopy/bin/canopy restore --all')
mkdir -p "$OUT"
rm -f "$OUT"/step-*.sh
fails=0

awk -v dir="$OUT" '
  /<!-- alias-day: begin -->/ { n++; on = 1; f = sprintf("%s/step-%02d.sh", dir, n); printf "" > f; next }
  /<!-- alias-day: end -->/ { on = 0; next }
  on && /^```/ { block = !block; next }
  on && block { print > f }
' "$README"

steps=("$OUT"/step-*.sh)
if [ ! -e "${steps[0]}" ]; then
  echo "FAIL no <!-- alias-day: begin --> region in docs/alias-day.md"
  exit 1
fi
all="$(cat "${steps[@]}")"
for cmd in "${REQUIRED[@]}"; do
  if printf '%s\n' "$all" | grep -qF -- "$cmd"; then
    echo "PASS the alias-day steps run '$cmd'"
  else
    echo "FAIL the alias-day steps do not run '$cmd'"
    fails=$((fails + 1))
  fi
done

# The owner's shell is a login zsh; its PATH is the one the steps get.
login_path="$(zsh -lic 'print -r -- "LOGINPATH=$PATH"' </dev/null 2>/dev/null | sed -n 's/^LOGINPATH=//p' | tail -n 1)"
[ -n "$login_path" ] || { echo "FAIL a login zsh printed no PATH"; exit 1; }

for step in "${steps[@]}"; do
  name="$(basename "$step" .sh)"
  echo "### $name"
  cat "$step"
  if (cd "$HOME" && PATH="$login_path" bash -euxo pipefail "$step") >"$OUT/$name.log" 2>&1; then
    echo "PASS $name"
  else
    echo "FAIL $name (exit $?), its output:"
    cat "$OUT/$name.log"
    fails=$((fails + 1))
    break
  fi
  cat "$OUT/$name.log"
done

echo "readme: $fails failure(s)"
[ "$fails" -eq 0 ]
