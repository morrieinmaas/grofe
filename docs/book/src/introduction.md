# grofe

grofe is a terminal multiplexer for people who like tmux and are tired of
working around it.

It keeps what tmux got right: sessions, windows and panes, a server that
outlives your terminal, a prefix key, and commands like `split-window` and
`send-keys`. If you know tmux, you already know how to drive grofe. What it
changes is the part you keep fighting: sessions that die with the machine, a
config language nobody can debug, and a status line that forks a shell on
every redraw.

> grofe is an early preview (0.1). It is in daily use by its author and
> passes a large tmux compatibility suite, but expect rough edges and keep
> real tmux installed while you try it.

## What's different

- **Your work survives a reboot.** A pane knows what it is running and how
  to bring it back. After a crash or a restart your panes return in the same
  layout and directories; panes that know how to resume come back running
  their program (your editor, a coding agent halfway through a
  conversation), and the rest come back as a shell in their directory.
  `grofe reboot-check` tells you what will come back before you reboot.
- **A config you can debug.** Configuration is TOML with a schema. A typo is
  an error naming the file, the line and the setting you probably meant, and
  `grofe show-config <setting> --explain` tells you which file set a value.
  Reloading checks everything first and applies all of it or none of it.
- **A status line that never slows you down.** It never forks on redraw.
  `#(...)` commands run on a timer, one at a time, and their output is
  cached.
- **Panes like windows on a desktop.** Each pane is a framed tile with its
  title and controls in its border. Drag a pane by its title to swap or dock
  it, or float it above the others. See [Framed panes](framed-panes.md).
- **Coding agents built in.** grofe tracks agents such as Claude Code and
  Codex running in its panes, shows which ones need you, and notifies you.
  See [Agent sessions](agents.md).
- **Modern terminal support.** True colour, the kitty keyboard protocol,
  clipboard over OSC 52, hyperlinks, focus events, tear-free redraws and
  inline images. The defaults are the ones most people configure anyway:
  mouse on, numbering from 1, no half-second escape delay, new panes in the
  current directory.
- **Scriptable.** Everything a key does, the command line does too, and
  `grofe control` speaks a JSON protocol with an event stream, so
  [plugins](plugins.md) react to what happens instead of polling.

## What it isn't

grofe is not a general reimplementation of tmux. It answers to the name
`tmux` for everyday scripts and tools, but it does not read `tmux.conf` as
its config: your setup moves over once (see
[Switching from tmux](switching-from-tmux.md)). It does not run on Windows.
It is also not trying to be Zellij: no modal interface, no WebAssembly
plugins, just tmux's way of working with the rough edges taken off.

## Why Rust

Not for speed; tmux is already fast. Rust gives grofe a single binary with no
runtime dependencies, solid terminal emulation libraries (grofe is built on
the one inside Alacritty), and memory safety for a long-running process that
holds your live work. The attached client is deliberately thin, so if it
crashes your panes don't notice.
