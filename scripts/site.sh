#!/usr/bin/env bash
# Assemble the project site in <out> (default _site/), as the Pages
# workflow does: site/ at the root, the book under docs/ and the
# screenshots under img/. Needs mdbook, plus what scripts/screenshots.sh
# needs (vhs, ttyd, ffmpeg and a built grofe).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/_site}
rm -rf "$out"
mkdir -p "$out"
out=$(cd "$out" && pwd)

# Everything under site/ (the landing page and its assets) goes to the root.
cp -R "$root/site/." "$out/"
touch "$out/.nojekyll"
mdbook build "$root/docs/book" -d "$out/docs"
"$root/scripts/screenshots.sh" "$out/img"

for f in index.html docs/index.html docs/reference/options.html img/grofe-dark.png img/grofe-light.png; do
    test -s "$out/$f" || { echo "site: $f missing" >&2; exit 1; }
done
echo "site: $out"
