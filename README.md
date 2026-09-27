# grofe

A terminal multiplexer for people who like tmux and are tired of working
around it.

grofe keeps what tmux got right: sessions, windows and panes, a server that
outlives your terminal, a prefix key, and commands like `split-window` and
`send-keys`. If you know tmux, you already know how to drive grofe. What it
changes is the part you keep fighting: sessions that die with the machine, a
config language nobody can debug, and a status line that forks a shell on every
redraw.

> grofe is in early development and not usable day to day yet. If you want to
> follow along or help, read on.

## What's different

**Your work survives a reboot.** In grofe a pane knows what it is running and
how to bring it back. After a crash or a restart, your panes return in the same
layout, in the same directories, running the same programs: your editor
session, your REPL, the coding agent halfway through a conversation. No
resurrect, no continuum, no list of process names to whitelist. Before you
reboot, `grofe reboot-check` tells you exactly what will come back.

**A config you can actually debug.** Configuration is TOML with a schema. A
typo is an error that names the file, the line and the setting you probably
meant, not a silently ignored option. Ask where a value came from and grofe
tells you:

```console
$ grofe show-config status.right --explain
status.right = "#{session_name} #{host_short}"
  set by ~/.config/grofe/user.toml
```

Your own file always loads last, so tools and themes can add to your setup
without overriding you. Reloading checks everything first and either applies
all of it or none of it.

```toml
prefix = "C-b"
mouse = true

[keys.prefix]
"|" = "split-window -h -c '#{pane_current_path}'"
"-" = "split-window -v -c '#{pane_current_path}'"
```

**A status line that never slows you down.** grofe does not run commands from
the status line, ever. Anything dynamic (a build result, an agent's state, the
battery) is pushed in by whatever knows the value, so redraws stay instant no
matter how much you put there.

**Modern terminal support out of the box.** True colour, the kitty keyboard
protocol (so keys like a bare `q` or `Ctrl-Enter` reach Neovim, Helix and
coding agents intact), clipboard over OSC 52, hyperlinks, focus events and
tear-free redraws. The defaults are the ones most people end up configuring
anyway: mouse on, numbering from 1, no half-second escape delay, new panes in
the current directory.

**Scriptable.** Everything you can do with a key you can do from the command
line, and `grofe control` speaks a simple JSON protocol with an event stream,
so scripts and plugins react to what happens instead of polling. Plugins are
ordinary programs in whatever language you like, with Lua available for the
quick things.

**Good at running agents.** If you keep several coding agents busy at once,
grofe can show which ones are working, which are waiting for you, and which
have finished, and take you straight to the one that needs attention. After a
reboot, each agent comes back to its own conversation.

## What it isn't

grofe is not a drop-in replacement for tmux. It does not read `tmux.conf` or
run tmux plugins, and it does not run on Windows. It is also not trying to be
Zellij: no modal interface, no WebAssembly plugins, just tmux's way of working
with the rough edges taken off.

## Why Rust

Not for speed; tmux is already fast. Rust gives grofe a single binary with no
runtime dependencies, access to solid terminal emulation libraries (grofe is
built on the one inside Alacritty), and memory safety for a long-running
process that holds your live work. The attached client is deliberately thin,
so if it crashes your panes don't notice.

## Status

The foundation is in place: the crate, the shared types, and a working
single-pane prototype that runs a shell and Neovim through the emulator. You
can try the prototype from a checkout:

```sh
cargo run --example spike      # quit with Ctrl-]
```

The next steps are the headless server, then attaching and drawing multiple
panes, then configuration, at which point grofe becomes usable as a daily
driver. The [roadmap](docs/plans/2026-09-23-milestones.md) has the details and
the [design document](docs/specs/2026-09-23-grofe-design.md) explains the
reasoning behind every choice.

grofe runs on macOS and Linux.

## Contributing

Issues and ideas are welcome. The design document is the best place to start;
if something in it looks wrong, that is exactly the kind of feedback that is
most useful right now. To build and test:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## License

Apache License 2.0. See [LICENSE](LICENSE).
