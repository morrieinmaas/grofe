#!/usr/bin/env bash
# ssh-e2e-release.sh: a local release directory, laid out as the release
# workflow (.github/workflows/release.yml) lays out a release's assets, so
# the bootstrap install can be tested with `remote-release-url` pointed at a
# local http server.
#
#   scripts/ssh-e2e-release.sh <outdir> <grofe-binary> <target> [tag]
#
# writes <outdir>/grofe-<tag>-<target>.tar.gz (a directory of that name
# holding grofe, README.md and LICENSE), a .sha256 per asset and SHA256SUMS
# listing every asset, as the publish job does. <tag> defaults to v plus
# Cargo.toml's version. The URL a client uses is the server's root, with the
# assets directly below it.
set -euo pipefail
cd "$(dirname "$0")/.."

[ $# -ge 3 ] || { echo "usage: $0 <outdir> <grofe-binary> <target> [tag]" >&2; exit 2; }
out="$1" bin="$2" target="$3"
tag="${4:-v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
[ -f "$bin" ] || { echo "ssh-e2e-release: no binary at $bin" >&2; exit 1; }

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

name="grofe-$tag-$target"
work="$(mktemp -d "${TMPDIR:-/tmp}/ssh-e2e-release.XXXXXX")"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out" "$work/$name"
cp "$bin" "$work/$name/grofe"
cp README.md LICENSE "$work/$name/"
tar -C "$work" -czf "$out/$name.tar.gz" "$name"

(
  cd "$out"
  for f in *.tar.gz; do sha256 "$f" > "$f.sha256"; done
  assets=()
  for f in *; do
    case "$f" in *.sha256 | SHA256SUMS) ;; *) assets+=("$f") ;; esac
  done
  sha256 "${assets[@]}" > SHA256SUMS
)
echo "$out/$name.tar.gz"
