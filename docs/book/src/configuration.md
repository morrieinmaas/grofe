# Configuration

Configuration is TOML and lives in `~/.config/grofe/` (or
`$XDG_CONFIG_HOME/grofe/`). Files load in this order, each overriding the
ones before:

1. `config.toml`
2. every `conf.d/*.toml`, in name order
3. `user.toml`

Your own file always loads last, so tools and themes can add to your setup
without overriding you.

## A small config

```toml
prefix = "C-a"
history-limit = 100000
mouse = true

[status]
right = "#{session_name} %H:%M"
interval = 5

[keys.prefix]
"|" = "split-window -h -c '#{pane_current_path}'"
"-" = "split-window -v -c '#{pane_current_path}'"
"r" = { cmd = "reload", desc = "reload the config" }
```

Every option has a TOML key, listed in the
[options reference](reference/options.md). Most are the tmux name; some live
in a table (`status.right` is `right` under `[status]`) and durations say
their unit (`escape-time-ms`). Booleans are `true` and `false`. A binding's
`desc` shows in `list-keys` and the key menu (`prefix Space`).

## Check, explain, reload

`show-config` needs no server. It prints every setting (`--file <path>`
checks one file); with a setting's name and `--explain` it says which file
set it:

```console
$ grofe show-config status.right --explain
status.right = "#{session_name} %H:%M"
  set by ~/.config/grofe/config.toml
```

A typo is an error naming the file, the line and the setting you probably
meant, never a silently ignored option. `grofe reload` checks every file
first; if any has an error it changes nothing and lists every error with its
file and line.

`list-keys` shows where each binding came from: `default`, a file and line,
or `runtime` for one made with `bind-key` since the server started.

## At runtime

`set-option` and `show-options` work as in tmux, by tmux name:

```sh
grofe set-option -g status-position top
grofe show-options -g status-position
```

Once an option is set at runtime the config no longer changes it, so a
`reload` does not undo it.

## The status line

The status line never forks on redraw. `#(...)` commands in it run on a
timer (`status.interval`, in seconds), one at a time, and their output is
cached. Anything else dynamic, such as a build result, is pushed in by
whatever knows the value, so redraws stay instant.
