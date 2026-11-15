# grofe

[![CI](https://img.shields.io/github/actions/workflow/status/termage/grofe/ci.yml?branch=main&label=CI)](https://github.com/termage/grofe/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/termage/grofe)](https://github.com/termage/grofe/releases)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/termage/grofe?style=social)](https://github.com/termage/grofe)

An agent-native terminal multiplexer: run many coding-agent sessions side by
side and let grofe keep track of them.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://termage.github.io/grofe/img/grofe-dark.png">
  <img alt="grofe with framed panes running coding agents, the agent sidebar and the status line" src="https://termage.github.io/grofe/img/grofe-light.png">
</picture>

- **Knows your coding agents.** grofe sees which panes run an agent (Claude
  Code, Codex, opencode, Gemini, pi, aider) and what each one is doing:
  working, blocked on you, done. `prefix u` jumps to the one that needs you,
  a picker, a sidebar and a grid show them all, and notifications reach you
  when you are looking elsewhere.
- **Agents come back after a restart.** Panes return in the same layout and
  folders, and Claude Code, Codex and opencode come back in the same
  conversation once their hooks are set up. No resurrect, no continuum.
- **tmux, without the parts you fight.** tmux's keys, commands and formats,
  so your scripts and plugins keep working, with a TOML config that names
  the line of a typo, a status line that never forks on redraw, and framed
  panes you can drag, dock and float.
- **Scriptable.** Luau scripts and `grofe pack` plugins run inside a time
  and memory budget, and `grofe control` speaks JSON to anything else.

> grofe is an early preview. It is in daily use and passes a large tmux
> compatibility suite, but expect rough edges and keep real tmux installed
> while you try it.

**[Read the docs](https://termage.github.io/grofe/docs/)** for the full
guide and the [options reference](https://termage.github.io/grofe/docs/reference/options.html).

## Install

grofe runs on macOS and Linux. Prebuilt binaries are on the
[releases page](https://github.com/termage/grofe/releases). Or build it
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

Options, key bindings, hooks and status formats come across, and anything
that cannot is listed with the reason. `grofe pack` replaces TPM: declare
each plugin as a `[[pack]]` table and grofe clones it and pins it to a commit
in `pack-lock.toml`. Plugins load after `grofe pack sync`, then
`grofe reload`; until you switch, the plugins TPM already installed keep
loading. The first grofe server then takes its sessions from your running
tmux. Scripts and tools that talk to tmux (`fzf --tmux`,
vim-tmux-navigator) keep working, and grofe can run under the name `tmux`.
See
[Switching from tmux](https://termage.github.io/grofe/docs/switching-from-tmux.html)
for the gaps.

## Scripting and plugins

grofe embeds Luau. An `init.luau` next to your config can bind keys, react to
events, run timers and push values into the status line, each call under a
50 ms and 64 MiB budget so a script can never freeze the server. `grofe pack`
installs plugins from git into a lockfile, Luau plugins and tmux plugins
alike:

```lua
grofe.bind("prefix", "g", function() grofe.cmd("split-window -h") end, { desc = "Split" })
```

See [Lua scripting and packs](https://termage.github.io/grofe/docs/lua.html)
for the API, the pack manager and editor types.

## Contributing

Issues and ideas are welcome; the [roadmap](docs/roadmap.md) lists what is
open. [CONTRIBUTING.md](CONTRIBUTING.md) covers the
build, the checks a change must pass (`just check`), tests, commits and the
docs. When you report a bug, include `grofe -V`, your OS and terminal, and
whether it also happens under the `tmux` name. Security problems go through
[SECURITY.md](SECURITY.md).

If grofe is useful to you, [star it on GitHub](https://github.com/termage/grofe);
it helps others find it.

## License

Apache License 2.0. See [LICENSE](LICENSE).
