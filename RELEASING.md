# Releasing grofe

A release is a `v*` tag. Pushing one runs `.github/workflows/release.yml`,
which builds everything and publishes it; nothing is uploaded by hand.

| Artifact | Built by | Published to |
| --- | --- | --- |
| Tarballs for macOS and Linux (musl), x86_64 and arm64 | `build` | the GitHub release |
| `.deb` and `.rpm` for Linux x86_64 and arm64 | `build` (`just deb`, `just rpm`) | the GitHub release |
| Release notes | `packaging/release-notes.sh` (the tag's CHANGELOG.md section) | the GitHub release |
| Homebrew formula | `packaging/render.sh` (`just pkg-render`) | `Formula/grofe.rb` in `<owner>/homebrew-tap` |
| AUR `grofe-bin` | `packaging/render.sh` plus `makepkg --printsrcinfo` | `aur.archlinux.org/grofe-bin.git` |
| Crate | `cargo publish` | crates.io |

Every file on the release has a `.sha256` next to it. The Homebrew, AUR and
crates.io jobs run only when their credential is configured, and are
skipped (not failed) otherwise.

## Each release

1. On an up-to-date `main` with a clean tree, run `just release 0.2.0`.
   It sets the version in `Cargo.toml` and `Cargo.lock`, regenerates
   `CHANGELOG.md` with git-cliff, commits `chore(release): v0.2.0` and
   creates the tag `v0.2.0`. It does not push.
2. Read the new CHANGELOG.md section (`git show HEAD`). To fix it, edit the
   commit, then move the tag: `git tag -fa v0.2.0 -m "grofe v0.2.0"`.
3. Push the commit and the tag: `git push origin HEAD v0.2.0`.
4. Watch the Release workflow. When it is green, check the release page,
   `brew upgrade grofe` (or `brew install morrieinmaas/tap/grofe`) and the
   AUR page.

To redo a release's build for a tag that already exists, run the Release
workflow by hand (Actions, Release, Run workflow) with that tag. It adds the
files to the existing release, replacing those of the same name.

To check the formula and PKGBUILD for a published release locally, run
`just pkg-render 0.1.0`; they are written to `target/pkg/`.

Tools for the local recipes: `git-cliff` (`just changelog`, `just release`),
`cargo-deb` and `cargo-generate-rpm` (`just deb`, `just rpm`), installed with
`cargo install --locked git-cliff cargo-deb cargo-generate-rpm`.

## One-time setup

Done once by the maintainer, in this order.

### crates.io

1. Log in to crates.io with GitHub and create an API token with the
   `publish-new` scope.
2. From the tag of the first release, publish by hand:
   `cargo login`, then `cargo publish --locked`. The token can be revoked
   afterwards.
3. On the crate's Settings page, add a trusted publisher: GitHub, owner
   `morrieinmaas`, repository `grofe`, workflow `release.yml`, no
   environment.
4. In the GitHub repository (Settings, Secrets and variables, Actions,
   Variables), add the variable `CRATES_IO_TRUSTED` with the value `true`.
   From then on the `crates` job publishes each tag.

### Homebrew tap

1. Create the public repository `morrieinmaas/homebrew-tap` with a
   `Formula/` directory (a README is enough to start).
2. Create a fine-grained personal access token limited to that repository,
   with Contents read and write.
3. In this repository add the secret `HOMEBREW_TAP_TOKEN` with that token.

Users then install with `brew install morrieinmaas/tap/grofe`.

### AUR

1. Create an account on aur.archlinux.org and add an SSH public key to it.
   Use a key made for this purpose only (`ssh-keygen -t ed25519`).
2. In this repository add the secret `AUR_SSH_PRIVATE_KEY` with the private
   key.

The AUR creates `grofe-bin` on its first push, so the next release's `aur`
job makes the package; no manual push is needed. The template has no
`# Maintainer:` line; add one to `packaging/aur/PKGBUILD.tmpl` if you want
it.

### Names in one place

| Kind | Name | Used by |
| --- | --- | --- |
| Secret | `HOMEBREW_TAP_TOKEN` | `homebrew` job |
| Secret | `AUR_SSH_PRIVATE_KEY` | `aur` job |
| Variable | `CRATES_IO_TRUSTED` (`true`) | `crates` job |
