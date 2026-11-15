#!/usr/bin/env bash
# render.sh <version> [out-dir]: render the Homebrew formula and the AUR
# PKGBUILD for an existing GitHub release, with the sha256 sums its
# .sha256 assets publish. Writes <out-dir>/grofe.rb and <out-dir>/PKGBUILD
# (out-dir defaults to target/pkg).
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: render.sh <version> [out-dir]}"
version="${version#v}"
out="${2:-target/pkg}"
base="https://github.com/morrieinmaas/grofe/releases/download/v$version"
targets=(aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl)

subst=(-e "s/@VERSION@/$version/g")
for target in "${targets[@]}"; do
  asset="grofe-v$version-$target.tar.gz"
  sum="$(curl -fsSL "$base/$asset.sha256" | awk '{print $1}')"
  [[ "$sum" =~ ^[0-9a-f]{64}$ ]] || { echo "render.sh: no sha256 for $asset" >&2; exit 1; }
  key="$(echo "$target" | tr 'a-z-' 'A-Z_')"
  subst+=(-e "s/@SHA256_${key}@/$sum/g")
done

mkdir -p "$out"
sed "${subst[@]}" packaging/homebrew/grofe.rb.tmpl >"$out/grofe.rb"
sed "${subst[@]}" packaging/aur/PKGBUILD.tmpl >"$out/PKGBUILD"
if grep -n '@[A-Z0-9_]*@' "$out/grofe.rb" "$out/PKGBUILD"; then
  echo "render.sh: placeholders left unrendered" >&2
  exit 1
fi
echo "rendered $out/grofe.rb and $out/PKGBUILD for v$version"
