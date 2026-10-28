# TOML layouts

A layout file describes one or several sessions: their windows, panes and
floating panes, the commands to start in them, and hooks. `grofe layout`
builds them, saves a running session back to a file, lists them and picks one
from a menu. It is a grofe verb only: the tmux identity (`tmux layout ...`)
does not know it, and no stock key binds it.

## Where files live

`$XDG_CONFIG_HOME/grofe/layouts/<name>.toml`, else `~/.config/grofe/layouts/`
(the same directory rule as `config.toml`).

An argument to `layout up` or `layout save` is a **path** when it starts with
`/`, `./` or `~/`; otherwise it is a **name**. A name is
`[A-Za-z0-9][A-Za-z0-9._-]*` with no `..`, with an optional `.toml` suffix,
and resolves only inside the layouts directory. Anything else (`../x`,
`a/b`, an empty name) is refused with `invalid layout name`.

## Schema

A file is one session (the keys at the top level) or several (`[[session]]`
tables, with nothing else at the top level). Unknown keys, wrong types and
bad values are errors reported as `<file>:<line>: <message>`. The whole file
is validated before anything is created.

```toml
name     = "dev"                # default: the file name without .toml
root     = "~/code/app"         # session directory; ~ expanded; default: the CLI's cwd
env      = { RUST_LOG = "debug" }
before   = "docker compose up -d"   # host shell, before the session is created
on_start = "echo started"           # host shell, right after the session exists
after    = "echo ready"             # host shell, once every pane is up
on_stop  = "docker compose down"    # host shell, after the session has ended
attach   = true                     # default true

[[window]]
name   = "edit"
cwd    = "src"                  # relative to root (absolute and ~ also fine)
layout = "main-vertical"        # a preset name, or a tmux layout string
focus  = true                   # the window selected when everything is up
env    = { A = "1" }
split  = "v"                    # without layout: "v" stacks panes, "h" puts them side by side

[[window.pane]]
cwd  = "."                      # relative to the window's cwd
cmd  = "nvim"                   # typed into the pane's shell, which stays
env  = { B = "2" }
focus = true                    # the pane selected in this window
wait = { regex = "ready", timeout = 10 }   # or { path = "/tmp/x.sock" }

[[window.pane]]
cmd  = "make watch"
exec = true                     # run cmd as the pane's command instead of typing it
size = "30%"                    # or 20 (cells); see below

[[window.float]]                # a floating pane (grofe's new-pane)
cmd    = "htop"
exec   = true
cwd    = "."
env    = {}
x      = "10%"                  # cells (may be negative) or N%
y      = 2
width  = "60%"                  # cells or N%
height = 12
border = "heavy"                # pane-border-lines for this pane
title  = "monitor"              # the pane's title
focus  = false
```

With several sessions, each is a `[[session]]` table with `name` required and
its windows `[[session.window]]`, `[[session.window.pane]]` and
`[[session.window.float]]`. Session names are unique in the file.

### Sessions

`name` may not contain `:`, `.` or a newline. `env` values are passed to the
panes as they are; nothing is shell-interpolated. Keys are
`[A-Za-z_][A-Za-z0-9_]*`. Session `env` reaches every pane, window `env`
the window's panes and floats, pane `env` that pane; later levels win.

Hooks run with the user's shell the same way `run-shell` does (the server's
`default-shell`, else `$SHELL`, as `<shell> -c <command>`), with the session
`root` as the working directory and `GROFE_LAYOUT_SESSION` set to the session
name. A failing `before` stops `up` before anything is created; a failing
`on_start`, `after` or `on_stop` is reported and does not undo the session.
`on_stop` is run by a small background `grofe layout __watch` process that
`up` starts as soon as the session exists. It subscribes to the server's
`session-closed` event and runs the hook when the session ends (killed, last
window closed, removed by a failed `up`, or the server gone: the subscription
ends with the server, so the helper never outlives it). A session already
running gets no second helper.

### Windows and panes

A window with no `[[window.pane]]` gets one shell pane. `layout` is applied
after all tiled panes exist, through the same codec as `select-layout`;
a preset name is one of `even-horizontal`, `even-vertical`,
`main-horizontal`, `main-horizontal-mirrored`, `main-vertical`,
`main-vertical-mirrored`, `tiled`. A layout string must be valid and have no
more cells than the window has panes (checked before anything is created).

Without `layout`, panes are made by successive splits of the previous pane in
the direction `split` says. `size` is the `split-window -l` of the split that
creates the pane: `"30%"` (1 to 99) or cells (a positive integer, or a
string of digits). It is an error on the window's first pane (no split makes
it) and in a window that has a `layout` (the layout fixes the sizes).

`cmd` is typed into the pane's shell followed by Enter, so the pane keeps its
shell when the command ends. With `exec = true` it is the pane's command and
the pane ends with it (`exec` needs `cmd`).

`wait` pauses the build after that pane has been started and before the next
pane (or window) is made. It holds exactly one of `regex` (matched against the
pane's visible screen, polled) or `path` (a file that must exist; relative
paths are relative to the pane's cwd), and `timeout` in seconds (default 30, at most 86400).
The screen includes the command line you typed, so a regex that the `cmd`
text itself matches is satisfied at once. A timeout is an error: `up` stops, removes the session it created and exits
non-zero.

At most one window has `focus = true`, and at most one pane or float per
window. Without any, the first window and its first tiled pane are selected.

### Floating panes

A `[[window.float]]` is created with `new-pane`, so the placement rules and
limits are new-pane's (tmux 3.7b). Percentages above 100 are refused when the
file is validated. Whether a size or position fits the window is only known
when the pane is made; new-pane then refuses with `size or position too
large`, and `up` fails and removes the session. A float that merely overlaps
the edge is placed as new-pane places it.
Floats are made after the layout is applied. `border` is one of `single`,
`double`, `heavy`, `simple`, `number`, `spaces`. Hidden floats are not
supported yet.

## Commands

```
grofe layout up <name|path> [-d] [--session NAME]
grofe layout save <session> [name|path] [--force]
grofe layout list
grofe layout pick
```

**up.** For each session in the file: if a session of that name is already
running, nothing is built (idempotent). Otherwise it is built, in order:
`before`, create (the session directory is `root`), the `on_stop` watcher,
the session `env`, `on_start`, windows and panes, layouts, floats, focus,
then `after`. Every window is made by `new-window`, so each pane starts once,
with its environment in place: `new-session`'s own window only holds a
placeholder process until the real ones exist, and is then removed and the
windows renumbered from `base-index`. Then the first session whose `attach` is
not false is attached, unless `-d` is given. Run inside a pane of this
server's client, `up` switches that client (`switch-client`) instead of
nesting. With no terminal and no client, `up` builds and does not attach.
`--session NAME` renames the session of a single-session file; in a
multi-session file it selects the one session to build. On any build error the
sessions created by this call are removed.

**save.** Writes the running session as TOML: the session `name` and `root`
(its directory), each window's `name`, `layout` (the layout string of its
tiled panes) and `focus` for the active one, each pane's `cwd` (absolute),
`cmd` when the foreground process is not a shell (its name only, not its
arguments, and only a plain name of letters, digits and `._+-`: a process
names itself, and `up` types `cmd` into a shell), `focus` for the active pane, and each floating pane's geometry
(`x`, `y`, `width`, `height` in cells), `cwd`, `cmd`, `title`, `border` and
`focus`. The target defaults to `<session>` in the layouts directory (as a name
only: a session called `~/x` is not a path, and needs a target given). An
existing file is only replaced with `--force`; the file is written beside the
target and renamed over it, so a symlink there is replaced, not written
through. Saving then running `up` in a
fresh server gives the same window names, pane count, layout string and cwds.
Env, hooks, `wait` and `size` are not recoverable from a running session and
are not written.

**list.** One line per file, tab separated: name, path, then the session
names comma separated (or `! <error>` for a file that does not parse).

**pick.** Opens a `display-menu` of the layouts on the current client; the
choice runs `grofe layout up`. It needs a client (run it from a key binding).
The menu keys are `1`-`9` then `a`-`z`.

## Binding example

grofe ships no stock binding. `g` is unbound in tmux 3.7b's stock prefix
table and in canopy's prefix bindings and plugin defaults, so in
`~/.config/grofe/user.toml`:

```toml
[keys.prefix]
g = "run-shell 'grofe layout pick'"
```

`layout pick` finds the client through the server, so it works from
`run-shell` as bound here; `layout up` then switches that client.
