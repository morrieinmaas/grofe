#!/usr/bin/env bash
# seed.sh <athome-tree>: chezmoi.toml with a [data] answer for every prompt
# in the archive's .chezmoi.toml.tmpl, on stdout.
#
# TE.2b's stage 2, step 1: the keys bootstrap seeds, read from the archive
# itself so a new athome prompt shows up as a key here rather than a prompt
# at init. Values come from the archive's own dummy answers
# (test/e2e/bootstrap.toml), with three fixed for the container:
# hostname = "e2e", meshProvider = "none", includeAgents = false. A key with
# no answer is seeded empty, as bootstrap seeds an unset ATHOME_* variable.
set -euo pipefail

src="${1:?usage: seed.sh <athome-tree>}"
tmpl="$src/home/.chezmoi.toml.tmpl"
answers="$src/test/e2e/bootstrap.toml"
[ -f "$tmpl" ] || { echo "seed.sh: no $tmpl" >&2; exit 1; }
[ -f "$answers" ] || { echo "seed.sh: no $answers" >&2; exit 1; }

answer() {
  sed -n "s/^$1[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*\$/\1/p" "$answers" | head -n 1
}

keys="$(grep -oE 'prompt(String|Bool|Choice|Int)Once[[:space:]]+\.[[:space:]]+"[A-Za-z_]+"' "$tmpl" |
  sed -E 's/^(prompt[A-Za-z]+Once)[[:space:]]+\.[[:space:]]+"([A-Za-z_]+)"$/\1 \2/')"
[ -n "$keys" ] || { echo "seed.sh: no prompts found in $tmpl" >&2; exit 1; }

printf '# Seeded by tests/e2e/alias-day/seed.sh from the athome archive.\n[data]\n'
while read -r kind key; do
  case "$key" in
    hostname) value=e2e ;;
    meshProvider) value=none ;;
    includeAgents) value=false ;;
    machineType) value="$(answer machine)" ;;
    *) value="$(answer "$key")" ;;
  esac
  case "$kind" in
    promptBoolOnce) printf '    %s = %s\n' "$key" "${value:-false}" ;;
    promptIntOnce) printf '    %s = %s\n' "$key" "${value:-0}" ;;
    *) printf '    %s = "%s"\n' "$key" "$value" ;;
  esac
done <<<"$keys"
