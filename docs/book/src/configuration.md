# Configuration

Configuration is TOML and lives in `~/.config/grofe/` (or
`$XDG_CONFIG_HOME/grofe/`). Files load in this order, each overriding the
ones before:

1. `config.toml`
2. every `conf.d/*.toml`, in name order
3. `user.toml`
4. `settings.toml`

`user.toml` is your own file, so tools and themes can add to your setup
without overriding you. `settings.toml` is the one grofe writes: it loads
last and overrides everything above it, so a value saved there always wins.
Do not expect to edit it by hand; grofe re-reads it before each save and
keeps your comments and other keys, but its layout is its own. Resetting a
setting removes its key, and the value from the layer below applies again.

## Saving from customize-mode

In customize-mode (`prefix C` in the tmux identity), `P` writes the selected
option or key binding to `settings.toml` and reloads the config. `S` is not
used for this: as in tmux, it sets the option globally. An option with no
TOML key (a `@user` option or a list option) answers `not saveable`. If the
reload fails, the file is put back as it was and the error is shown. A
missing `settings.toml` or config folder is created on the first save.

## Another config file

`grofe --config <file>` loads `<file>` in place of `config.toml`, and
`GROFE_CONFIG=<file>` does the same; the flag wins over the variable, which
wins over the default. `conf.d/`, `user.toml` and `layouts/` are then read
from the file's folder, not `~/.config/grofe/`. A relative path is taken from
the current directory and `~/` from `$HOME`. A file that does not exist loads
as empty. `reload` re-reads the same file, and `show-config` honours both. A
folder is refused. Only the command that starts the server picks the file: a
later command with another `--config` prints `grofe: server uses <path>` and
carries on.

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
`desc` shows in `list-keys` and the key menu (`prefix Space`). The default
`history-limit` is 2000 lines, as in tmux.

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

## The look in the screenshots

The screenshots use this config: a pill status bar on top in the Gruvbox
palette, with the agents badge, and frames whose number and controls get the
same rounded caps. It ships as
[`examples/pills.toml`](https://github.com/termage/grofe/blob/main/examples/pills.toml);
copy it into `~/.config/grofe/conf.d/` (it needs a Nerd font), and on a dark
terminal add
[`examples/pills-dark.toml`](https://github.com/termage/grofe/blob/main/examples/pills-dark.toml)
after it. The colours are user options, so changing one repaints the bar.

```toml
{{#include ../../../examples/pills.toml}}
```
