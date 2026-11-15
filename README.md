# grofe

A terminal multiplexer for people who like tmux and are tired of working
around it.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://morrieinmaas.github.io/grofe/img/grofe-dark.png">
  <img alt="grofe with three framed panes: vim, a file listing and a shell session" src="https://morrieinmaas.github.io/grofe/img/grofe-light.png">
</picture>

grofe keeps what tmux got right (sessions, windows and panes, a server that
outlives your terminal, a prefix key, `split-window` and `send-keys`) and
fixes the parts you keep fighting:

- **Your work survives a reboot.** Panes come back in the same layout and
  folders, and programs that know how to resume (your editor, a coding agent
  mid-conversation) come back running. No resurrect, no continuum.
- **A config you can debug.** TOML with a schema: a typo names the file and
  line, and `show-config --explain` says where a value came from.
- **A status line that never forks on redraw.** `#()` commands run on a timer
  and are cached.
- **Framed panes you can drag, dock and float**, coding agents tracked in the
  status line, and modern terminal support out of the box.

> grofe is an early preview (0.1). It is in daily use and passes a large tmux
> compatibility suite, but expect rough edges and keep real tmux installed
> while you try it.

**[Read the docs](https://morrieinmaas.github.io/grofe/docs/)** for the full
guide and the [options reference](https://morrieinmaas.github.io/grofe/docs/reference/options.html).

## Install

grofe runs on macOS and Linux. Prebuilt binaries are on the
[releases page](https://github.com/morrieinmaas/grofe/releases). Or build it
with Rust 1.88 or newer from a checkout:

<!-- readme-test: begin -->
```sh
cargo install --locked --path .
```
<!-- readme-test: end -->

## Quickstart

Running `grofe` on its own starts a session and attaches to it. Or start one
in the background and check that it is there:

<!-- readme-test: begin -->
```sh
grofe new-session -d -s demo
grofe list-sessions
```
<!-- readme-test: end -->

`grofe attach -t demo` attaches. The keys are tmux's: `Ctrl-b` then `%` or
`"` splits, `c` opens a window, `f` floats a pane, `Space` lists every key,
`d` detaches.

Configuration is TOML in `~/.config/grofe/`. Write a small config, check what
grofe makes of it, and apply it to the running server:

<!-- readme-test: begin -->
```sh
mkdir -p ~/.config/grofe
cat > ~/.config/grofe/config.toml <<'TOML'
history-limit = 100000

[keys.prefix]
"|" = "split-window -h -c '#{pane_current_path}'"
"-" = "split-window -v -c '#{pane_current_path}'"
"r" = { cmd = "reload", desc = "reload the config" }
TOML
grofe show-config --file ~/.config/grofe/config.toml
grofe reload
grofe list-keys -T prefix
```
<!-- readme-test: end -->

Stop the server with `prefix Q` or:

<!-- readme-test: begin -->
```sh
grofe kill-server
```
<!-- readme-test: end -->

Your sessions come back the next time it starts.

## Switching from tmux

```sh
grofe import-tmux ~/.config/tmux/tmux.conf > ~/.config/grofe/user.toml
```

Options, key bindings, hooks, status formats and your TPM plugins come
across, and anything that cannot is listed with the reason. The first grofe
server then takes its sessions from your running tmux. Scripts and tools that
talk to tmux (`fzf --tmux`, vim-tmux-navigator) keep working, and grofe can
run under the name `tmux`. See
[Switching from tmux](https://morrieinmaas.github.io/grofe/docs/switching-from-tmux.html)
for the gaps.

## Contributing

Issues and ideas are welcome; the
[design document](docs/specs/2026-09-23-grofe-design.md) is the best place to
start. When you report a bug, include `grofe -V`, your OS and terminal, and
whether it also happens under the `tmux` name.

A change must pass `just check` (`cargo fmt --check`, clippy with
`-D warnings`, `cargo test` and `bats tests/cli.bats`). Integration tests
start their own servers on private sockets, so they never touch a grofe or
tmux you have running. `just docs` regenerates the options reference and
builds the book; `just site-serve` builds the whole site and serves it on
localhost.

## License

Apache License 2.0. See [LICENSE](LICENSE).
