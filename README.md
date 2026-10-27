# grofe

A terminal multiplexer for people who like tmux and are tired of working
around it.

grofe keeps what tmux got right: sessions, windows and panes, a server that
outlives your terminal, a prefix key, and commands like `split-window` and
`send-keys`. If you know tmux, you already know how to drive grofe. What it
changes is the part you keep fighting: sessions that die with the machine, a
config language nobody can debug, and a status line that forks a shell on every
redraw.

> grofe is in early development and not ready for daily use yet. If you want
> to try it, follow along or help, read on.

## What's different

**Your work survives a reboot.** In grofe a pane knows what it is running and
how to bring it back. After a crash or a restart, your panes return in the same
layout and the same directories. Panes that know how to resume come back
running their program: your editor session, the coding agent halfway through
a conversation. Every other pane comes back as a shell in its directory. No
resurrect, no continuum, no list of process names to whitelist. Before you
reboot, `grofe reboot-check` tells you exactly what will come back.

**Works with the agent tools you already use.** grofe does not build its own
agent dashboard. The tmux plugins that track coding agents, such as
tmux-claude-session-manager and opensessions, run on grofe unchanged, and a
pane told how to resume its agent comes back to the same conversation.

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

**A status line that never slows you down.** grofe's status line never forks
on redraw. Commands you configure for it (the classic `#(...)` segments) run on
a timer, one at a time, and their output is cached; anything else dynamic (a
build result, an agent's state) is pushed in by whatever knows the value. So
redraws stay instant no matter how much you put there.

**Modern terminal support out of the box.** True colour, the kitty keyboard
protocol (so keys like a bare `q` or `Ctrl-Enter` reach Neovim, Helix and
coding agents intact), clipboard over OSC 52, hyperlinks, focus events,
tear-free redraws, and inline images through kitty's graphics protocol
(Unicode placeholders by default; `graphics = true` in `config.toml` adds
direct placements, animation and sixel, see `docs/graphics-placement.md`). The
defaults are the ones most people end up configuring anyway: mouse on,
numbering from 1, no half-second escape delay, new panes in the current
directory.

**Scriptable.** Everything you can do with a key you can do from the command
line, and `grofe control` speaks a simple JSON protocol with an event stream,
so scripts and plugins react to what happens instead of polling. Plugins are
ordinary programs in whatever language you like.

## Switching from tmux

> **Know the gaps first.** The tmux compatibility layer answers what the
> tools and scripts it was tested against send, but some things are not there
> yet or never will be:
>
> - **Plugins.** TPM itself does not run. The plugins you load with it run
>   unchanged through canopy's plugin loader, which vendors each one and
>   starts it with `run-shell`, as TPM does. A plugin's own `*.tmux` and
>   `*.conf` files are read as tmux config only when the request comes
>   through the `tmux` name or the server has tmux identity.
> - **Copy mode and paste buffers** (`copy-mode`, `send-keys -X`, the
>   `*-buffer` commands) work, with tmux 3.7b's `copy-mode-vi` table. Only
>   the vi keys are there; `mode-keys emacs` has no table.
> - **`tmux -f` takes a grofe TOML file**, not a `tmux.conf`. Convert your
>   config with `grofe import-tmux` first.
> - **Out of scope:** tmux's control mode (`-C` and `-CC`) and session groups
>   (`new-session -t`, `link-window`).
>
> Keep real tmux reachable by its full path while you try grofe.

grofe does not read `tmux.conf`, but it will convert one for you:

```sh
grofe import-tmux ~/.config/tmux/tmux.conf > ~/.config/grofe/user.toml
```

Options, key bindings, hooks and status formats come across, including `#()`
segments, which become cached status sources; anything that cannot (plugin
lines above all, which canopy's loader takes over) is listed with the reason,
so you know exactly what is left to do.
Your `tmux.conf` is never touched, and tmux keeps working alongside grofe while
you try it.

grofe sets `TMUX` and `TMUX_PANE` in its panes and answers the commands,
aliases and formats that scripts and tools send, so things like `fzf --tmux`
and vim-tmux-navigator keep working. Once your config has moved, you can put
grofe on your `PATH` under the name `tmux` (a symlink, since a shell alias does
not reach scripts); a server started under that name also reports itself as
tmux (`TERM_PROGRAM`, `tmux -V`) and starts with tmux 3.7b's option defaults
and stock key bindings rather than grofe's own (base index 0, mouse off and so
on), so nothing you rely on in tmux moves; your config's values still win.
tmux compatibility is a translation layer; grofe works differently underneath.
Real tmux stays available by its full path. Inside a grofe pane, run it as
`env -u TMUX /path/to/tmux`, because `$TMUX` there points at grofe.

## Terminals

grofe works in any modern terminal and is built and tested against two:
[Ghostty](https://ghostty.org) and [WezTerm](https://wezterm.org). Every
feature above is available in both.

- **Ghostty** needs no setup.
- **WezTerm** speaks the kitty keyboard protocol only when asked. Add this
  line to your `wezterm.lua`, or keys like `Ctrl-Enter` and a bare `q` reach
  your programs in the older, lossy encoding:

  ```lua
  config.enable_kitty_keyboard = true
  ```

Other terminals get the same grofe with the older key encoding and without
the features they don't support (inline images above all). Nothing breaks:
grofe asks the terminal what it supports when you attach.

## Install

grofe runs on macOS and Linux and builds with Rust 1.88 or newer (get Rust
from [rustup](https://rustup.rs)). There are no releases yet, so build it from
a checkout of this repository:

<!-- readme-test: begin -->
```sh
cargo install --locked --path .
```
<!-- readme-test: end -->

That puts `grofe` in `~/.cargo/bin`, which needs to be on your `PATH`.

grofe also answers to the name `tmux` (see [Switching from tmux](#switching-from-tmux)
for what that covers and what it does not). A command it does not support
fails with `<command>: not supported (<reason>)`. To try the
name without replacing tmux, link it somewhere off your `PATH` and call it by
its full path:

```sh
mkdir -p ~/grofe-tmux
ln -s ~/.cargo/bin/grofe ~/grofe-tmux/tmux
~/grofe-tmux/tmux -V
```

A server started through that link has tmux identity: its panes get
`TERM_PROGRAM=tmux`, and `tmux -V` prints the tmux release grofe is compatible
with. `compat-identity = "tmux"` in `config.toml` does the same for a server
started as `grofe`. Under either name, `-L` sockets live in grofe's own
directory, never in tmux's. Run as `tmux`, grofe refuses a `-S` path inside
tmux's socket directory (`$TMUX_TMPDIR` or `/tmp`, then `tmux-<uid>`) and
never replaces a socket it did not create. A `-S` path elsewhere that a real
tmux server listens on is still connected to, and grofe then reports that it
is not a grofe server. Inside a grofe pane, `$TMUX` points at grofe, so run
real tmux there as `env -u TMUX /path/to/tmux`.

## First steps

Start a session in the background and check that it is there:

<!-- readme-test: begin -->
```sh
grofe new-session -d -s demo
grofe list-sessions
```
<!-- readme-test: end -->

Then attach to it:

```sh
grofe attach -t demo
```

Inside, the keys are tmux's. The prefix is `Ctrl-b`; after it, `%` splits
side by side, `"` splits top and bottom, `c` opens a window, the arrows move
between panes, `:` opens the command prompt and `d` detaches. The session
keeps running after you detach, and `grofe attach -t demo` brings it back.

Configuration lives in `~/.config/grofe/` (or `$XDG_CONFIG_HOME/grofe/`).
`config.toml` loads first, then every `conf.d/*.toml` in name order, then
`user.toml`, and each one overrides the ones before. Write a small config,
check what grofe makes of it, and apply it to the running server:

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

`show-config` needs no server. It prints every setting, and with a setting's
name and `--explain` it tells you which file set it. `reload` checks every
file first. If any has an error it changes nothing and lists every error with
its file and line. `list-keys` shows where each binding came from: `default`,
a file and line, or `runtime` for one made with `bind-key` since the server
started. A command string can run several commands in a row, separated by
`;`, and so can the command line: `grofe split-window -h ';' select-pane -L`.

When you are done, stop the server and every session in it:

<!-- readme-test: begin -->
```sh
grofe kill-server
```
<!-- readme-test: end -->

## What it isn't

grofe is not a general reimplementation of tmux. It answers to the name `tmux`
for everyday scripts and tools, but it does not read `tmux.conf` as its
config: your setup moves over once, to grofe's own config, and your tmux
plugins run unchanged through canopy's loader rather than TPM. It also does not
run on Windows. It is also not trying to be Zellij: no modal
interface, no WebAssembly plugins, just tmux's way of working with the rough
edges taken off.

## Why Rust

Not for speed; tmux is already fast. Rust gives grofe a single binary with no
runtime dependencies, access to solid terminal emulation libraries (grofe is
built on the one inside Alacritty), and memory safety for a long-running
process that holds your live work. The attached client is deliberately thin,
so if it crashes your panes don't notice.

## Status

Sessions, windows and panes, the status line with cached `#()` sources, the
mouse, the command prompt, popups and menus, hooks, clipboard and title
passthrough, copy mode and paste buffers, the config described above,
`import-tmux` and the tmux compatibility layer work today, and tmux plugins
run unchanged through canopy's loader. Restoring sessions after a reboot (M4)
and grofe's own plugin API (M6) are still to come. Agent features stay with
tmux plugins and canopy rather than grofe. The
[roadmap](docs/plans/2026-09-23-milestones.md) has the details and the
[design document](docs/specs/2026-09-23-grofe-design.md) explains the
reasoning behind every choice.

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
