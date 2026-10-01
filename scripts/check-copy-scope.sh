#!/usr/bin/env bash
# check-copy-scope.sh: docs/copy-mode.md's keymap matches the recorded
# copy-mode-vi golden (plan M5, task TM.0). Exits nonzero when:
#
#   - list-keys-copy-mode-vi.txt is not exactly the list-keys output held in
#     list-keys-copy-mode-vi.golden (the recorder's file);
#   - a golden line has no row in the doc's "## Keymap" table, a row has no
#     golden line, or either side holds the same binding twice;
#   - a row's task is not exactly one of TM.1 to TM.6, or its scenario id is
#     empty or used by another row;
#   - the golden does not hold the inventory's 93 bindings and the doc has
#     fewer "- Difference `<key>`" notes than the gap, or a note names a key
#     the golden does not bind.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
dir=$repo/tests/fixtures/compat/golden/tm_0
golden=$dir/list-keys-copy-mode-vi.golden
txt=$dir/list-keys-copy-mode-vi.txt
doc=$repo/docs/copy-mode.md
inventory=93

fail=0
err() {
  echo "check-copy-scope: $*" >&2
  fail=1
}

for f in "$golden" "$txt" "$doc"; do
  [ -s "$f" ] || { err "missing or empty: ${f#"$repo"/}"; exit 1; }
done

body=$(awk '/^\$ list-keys -T copy-mode-vi$/ { on = 1; next } /^\? / { on = 0 } on' "$golden")
if [ "$body" != "$(cat "$txt")" ]; then
  err "list-keys-copy-mode-vi.txt differs from the list-keys output in its .golden"
fi

# One "key<TAB>command" per golden line. list-keys pads the key column.
golden_pairs=$(awk '
  /^bind-key  -T copy-mode-vi / {
    rest = substr($0, length("bind-key  -T copy-mode-vi ") + 1)
    key = rest; sub(/ .*/, "", key)
    cmd = substr(rest, length(key) + 1); sub(/^ +/, "", cmd)
    print key "\t" cmd
    next
  }
  { print "BAD\t" $0 }' "$txt")
if grep -q '^BAD' <<<"$golden_pairs"; then
  err "golden lines that are not copy-mode-vi bindings:"
  grep '^BAD' <<<"$golden_pairs" | cut -f2- >&2
fi
count=$(grep -vc '^BAD' <<<"$golden_pairs" || true)

# The keymap table's rows: | `key` | `command` | grofe | task | `scenario` |
# A literal | inside a cell is written \|. Fields are split on \037, not a
# tab, so an empty cell stays a field.
rows=$(awk '
  /^## / { on = ($0 == "## Keymap"); next }
  on && /^\| `/ {
    line = $0
    gsub(/\\\|/, "\001", line)
    n = split(line, c, "|")
    for (i = 1; i <= n; i++) {
      gsub(/^ +| +$/, "", c[i]); gsub(/\001/, "|", c[i])
    }
    key = c[2]; cmd = c[3]; task = c[5]; sc = c[6]
    sub(/^`/, "", key); sub(/`$/, "", key)
    sub(/^`/, "", cmd); sub(/`$/, "", cmd)
    sub(/^`/, "", sc); sub(/`$/, "", sc)
    print key "\037" cmd "\037" task "\037" sc "\037" n
  }' "$doc")
[ -n "$rows" ] || err "no rows under \"## Keymap\" in docs/copy-mode.md"

while IFS=$'\037' read -r key _ task sc n; do
  [ -n "$key" ] || continue
  [ "$n" = 7 ] || err "row for $key has $((n - 2)) cells, want 5"
  [[ "$task" =~ ^TM\.[1-6]$ ]] || err "row for $key names task '$task', want one of TM.1 to TM.6"
  [ -n "$sc" ] || err "row for $key has no scenario id"
done <<<"$rows"

doc_pairs=$(awk -F'\037' '{ print $1 "\t" $2 }' <<<"$rows")
dups=$( { sort <<<"$doc_pairs" | uniq -d; sort <<<"$golden_pairs" | uniq -d; } )
[ -z "$dups" ] || err "bindings listed twice: $dups"
dup_ids=$(awk -F'\037' '$4 != "" { print $4 }' <<<"$rows" | sort | uniq -d)
[ -z "$dup_ids" ] || err "scenario ids used twice: $dup_ids"

missing=$(comm -23 <(sort -u <<<"$golden_pairs") <(sort -u <<<"$doc_pairs"))
extra=$(comm -13 <(sort -u <<<"$golden_pairs") <(sort -u <<<"$doc_pairs"))
[ -z "$missing" ] || { err "golden lines with no doc row:"; echo "$missing" >&2; }
[ -z "$extra" ] || { err "doc rows with no golden line:"; echo "$extra" >&2; }

if [ "$count" != "$inventory" ]; then
  gap=$((count - inventory))
  gap=${gap#-}
  # shellcheck disable=SC2016 # the backticks are literal markdown
  notes=$(sed -n 's/^- Difference `\([^`]*\)`.*/\1/p' "$doc")
  have=$(grep -c . <<<"$notes" || true)
  if [ "$have" -lt "$gap" ]; then
    err "golden has $count bindings, the inventory $inventory; the doc explains $have of the $gap"
  fi
  keys=$(cut -f1 <<<"$golden_pairs")
  while IFS= read -r k; do
    [ -n "$k" ] || continue
    grep -qxF -- "$k" <<<"$keys" || err "difference note names $k, which the golden does not bind"
  done <<<"$notes"
  dup_notes=$(sort <<<"$notes" | uniq -d)
  [ -z "$dup_notes" ] || err "difference notes repeated: $dup_notes"
fi

if [ "$fail" = 0 ]; then
  echo "check-copy-scope: $count bindings, one doc row each (inventory $inventory)"
fi
exit "$fail"
