# Troubleshooting

## A setting does not take effect

- `grofe show-config <setting> --explain` says which file set the value. A
  later file wins: `config.toml`, then `conf.d/*.toml`, then `user.toml`.
- `grofe reload` changes nothing when any file has an error, and lists each
  error with its file and line.
- A value set at runtime with `set-option` is no longer touched by the
  config. Stop and start the server to start from the config again (a value
  set with `--persist` is kept in the snapshot).
- Some tmux options are accepted but have no effect in grofe; the
  [options reference](reference/options.md) marks them.

## The server is on an old build

A running server keeps the build it started with. Stop it with
`grofe kill-server` (or `prefix Q`) and start it again; your sessions come
back.

## Keys arrive wrong in Neovim or an agent

Your terminal is not using the kitty keyboard protocol. In WezTerm, add
`config.enable_kitty_keyboard = true` to `wezterm.lua`. See
[Terminals](install.md#terminals).

## A tmux command or script fails

A command grofe does not support fails with
`<command>: not supported (<reason>)`. The
[gaps](switching-from-tmux.md#know-the-gaps-first) are listed in Switching
from tmux. Inside grofe, `tmux` is grofe; run real tmux by its full path with
`env -u TMUX`.

## Before a reboot

`grofe reboot-check` tells you which panes will come back running their
program and which come back as a shell.

## Logs and reports

The server logs to `~/.local/state/grofe/<instance>.log` (or under
`$XDG_STATE_HOME`), `default.log` for the default server. Panic reports and
the log of tmux commands grofe could not answer (`compat-miss.log`) live in
the same directory. With a checkout and `just`:

```sh
just log        # follow the default server's log
just reports    # list panic reports and recent compat misses
```

## Reporting a bug

Open an issue on [GitHub](https://github.com/termage/grofe/issues) with
`grofe -V`, your OS and terminal, and whether it also happens under the
`tmux` name.
