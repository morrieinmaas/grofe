# Install

grofe runs on macOS and Linux.

## Prebuilt binaries

Each tagged release has binaries for x86_64 and arm64 (static on Linux) on
the [releases page](https://github.com/morrieinmaas/grofe/releases). Unpack
one and put `grofe` on your `PATH`.

## From source

With Rust 1.88 or newer (from [rustup](https://rustup.rs)), in a checkout of
the repository:

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
