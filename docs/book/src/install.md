# Install

grofe runs on macOS and Linux.

## Homebrew

On macOS and Linux:

```sh
brew install termage/tap/grofe
```

`brew upgrade grofe` picks up new releases.

## crates.io

With Rust 1.88 or newer (from [rustup](https://rustup.rs)):

```sh
cargo install --locked grofe
```

## Debian, Ubuntu and Fedora

Each release on the [releases page](https://github.com/termage/grofe/releases)
carries a `.deb` and an `.rpm` for amd64 and arm64:

```sh
sudo apt install ./grofe_0.2.0-1_amd64.deb      # Debian, Ubuntu
sudo dnf install ./grofe-0.2.0-1.x86_64.rpm     # Fedora, RHEL
```

## Prebuilt binaries

The same release has static binaries for x86_64 and arm64 on Linux, and
binaries for both on macOS. Unpack one and put `grofe` on your `PATH`.

## Verify a download

Every release file is listed in `SHA256SUMS` and has a build attestation
from GitHub Actions. In the folder you downloaded to:

```sh
sha256sum -c --ignore-missing SHA256SUMS        # Linux
shasum -a 256 -c --ignore-missing SHA256SUMS    # macOS
gh attestation verify grofe-v0.2.0-x86_64-unknown-linux-musl.tar.gz --repo termage/grofe
```

The first proves the file is intact; the second proves it was built from
this repository by its release workflow.

## From source

In a checkout of the repository:

```sh
cargo install --locked --path .
```

That puts `grofe` in `~/.cargo/bin`, which needs to be on your `PATH`. With
[just](https://github.com/casey/just) installed, `just install` does the
same.

## Upgrading

A running server keeps the build it started with. After an upgrade, a client
that attaches is told the server is still on the old build; stop it with
`grofe kill-server` (or `prefix Q`) and start again. Your sessions come back
with the new server.

## Terminals

grofe works in any modern terminal and is built and tested against
[Ghostty](https://ghostty.org) and [WezTerm](https://wezterm.org).

- **Ghostty** needs no setup.
- **WezTerm** speaks the kitty keyboard protocol only when asked. Without
  this line in `wezterm.lua`, keys like `Ctrl-Enter` and a bare `q` reach
  your programs in the older, lossy encoding:

  ```lua
  config.enable_kitty_keyboard = true
  ```

Other terminals get the same grofe with the older key encoding and without
the features they lack (inline images above all). grofe asks the terminal
what it supports when you attach, so nothing breaks.
