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
keeps your comments and other keys, but its layout is its own. Resetting an
option or a key binding removes its entry, and the value or binding from
the layer below applies again.

## Saving from customize-mode

In customize-mode (`prefix C` in the tmux identity), `P` writes the selected
option or key binding to `settings.toml` and reloads the config. `S` is not
used for this: as in tmux, it sets the option globally. A key binding is
saved with its command, its note (`desc`), its repeat flag, its `group` and
its `name`; saving it again replaces the whole entry, so a field the binding
no longer has (a note it was rebound without) leaves the file. Once saved, `settings.toml` owns it: a hand edit of the file and a
`reload` apply, even for a value or binding first set at runtime.

Only global values are saveable: a value set on one session, window or pane
answers `only global values are saveable`. A binding that is not grofe-dialect (a stock key of the
tmux identity, or a `bind-key` from a tmux client) answers `only grofe-dialect
bindings are saveable`. An option with no TOML key (a
`@user` option or a list option) answers `not saveable`, and its preview in
customize-mode says so. If the reload fails, the file is put back as it was
and the error is shown. A missing `settings.toml` or config folder is created
on the first save.

`d` and `D` (reset) on a global option or a key binding take its entry (an
option's key, a binding's `[keys.<table>]` entry) out of `settings.toml` and
reload: the value or binding from the layer below, such as `user.toml`, or
else the built-in default, applies again, and the config owns it from then on.
If the reload fails, the file, the value and the binding stay as they were
before the reset and the error is shown. On a value set on one session,
window or pane, `d` unsets it as tmux does and leaves `settings.toml` alone.

A server started with `tmux -f <file>` reads only the files you name, never
`settings.toml`, so `P` there answers that nothing was saved.

## Another config file

`grofe --config <file>` loads `<file>` in place of `config.toml`, and
`GROFE_CONFIG=<file>` does the same; the flag wins over the variable, which
wins over the default. The file's folder then becomes the config dir:
`conf.d/`, `user.toml`, `settings.toml`, `layouts/`, packs (`pack-lock.toml`),
`init.luau` and `machines.toml` are read from there, not `~/.config/grofe/`. A relative path is taken from
the current directory and `~/` from `$HOME`. A file that does not exist loads
as empty. `reload` re-reads the same file, and `show-config` honours both. A
folder is refused by `--config` (exit 1); a `GROFE_CONFIG` naming a folder is
ignored with a notice on stderr. Only the command that starts the server picks the file: a
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
