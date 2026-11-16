# Switching from tmux

## Know the gaps first

The tmux compatibility layer answers what the tools and scripts it was tested
against send, but some things are not there yet or never will be:

- **Plugins.** TPM itself does not run; `grofe pack` replaces it (see
  [Move your plugins to grofe pack](#move-your-plugins-to-grofe-pack)).
  Resurrect and continuum are not needed, since grofe's own persistence
  replaces them. A plugin's `*.tmux` and `*.conf` files are read as tmux
  config only when the request comes through the `tmux` name or the server
  has tmux identity.
- **Copy mode and paste buffers** work, with tmux 3.7b's stock `copy-mode-vi`
  and `copy-mode` (emacs) tables.
- **`tmux -f file` takes a `tmux.conf`** through the `tmux` name, in place
  of the usual config, when the call starts the server. A file named
  `*.toml` stays grofe's own config, and `grofe`'s own server start reads
  TOML only (`grofe import-tmux` converts a `tmux.conf`).
- **Control mode** (`tmux -C` and `-CC`) has a first slice: `attach` and
  `new-session` through a client-side bridge. Several notifications and
  commands are still missing, and iTerm2 has not been checked end to end;
  the [control-mode spec](https://github.com/termage/grofe/blob/main/docs/specs/2026-11-15-control-mode.md)
  lists what is left.
- **Out of scope:** a window linked twice into one session is refused.

Keep real tmux reachable by its full path while you try grofe.

## Convert your config

grofe's own config is TOML, not `tmux.conf`, but it converts one for you,
and never touches the original:

```sh
grofe import-tmux ~/.config/tmux/tmux.conf > ~/.config/grofe/user.toml
```

`just import-tmux` does the same and refuses to overwrite an existing
`user.toml`. Options, key bindings, hooks and status formats come across,
including `#()` segments, which become cached status sources. Anything that
cannot come across is listed with the reason. TPM's `run` line becomes a
`run-shell` of each plugin TPM already installed, in TPM's order, so your
plugins keep loading on the first day; nothing is cloned or updated that
way.

## Move your plugins to grofe pack

`grofe pack` is grofe's plugin manager and replaces TPM. A tmux plugin (a
repository with `*.tmux` files and no `grofe-plugin.toml`) is declared like
any other plugin, and each of its `*.tmux` files runs through `run-shell`
when the config loads. For each `set -g @plugin '<owner>/<repo>'` line, add a
`[[pack]]` table, then remove the TPM `run` line from the converted config:

```toml
[[pack]]
src = "github:tmux-plugins/tmux-yank"

[[pack]]
src = "github:tmux-plugins/tmux-open"
rev = "master"                 # optional: a branch, tag or commit to track
```

```sh
grofe pack sync     # clone what is missing, at the commits in the lock
grofe reload        # load what was installed
```

`grofe pack` pins every plugin to an exact commit in `pack-lock.toml` next to
your config; commit that file to keep machines in step. `grofe pack update`
shows the commits each plugin would gain before it moves the lock, and
`grofe pack list` shows every plugin and its state. See
[Lua scripting and packs](lua.md#grofe-pack) for the details.

## Bring your sessions along

The converted config sets `start-from-tmux = true`. With it, a grofe server
that starts with no sessions of its own takes them from your running tmux
server: names, windows, layouts and directories, with fresh shells, since
the programs are still running in tmux. With no tmux running, it takes them
from tmux-resurrect's newest save in `resurrect-dir` instead, where a pane
saved with a command runs it again.

Only the default server does this. One started with `-L preview` (or any
other socket) starts from its own snapshot, so you can try a new build next
to your main server.

## Scripts and tools

grofe sets `TMUX` and `TMUX_PANE` in its panes and answers the commands,
aliases and formats that scripts and tools send, so things like `fzf --tmux`
and vim-tmux-navigator keep working. A command it does not support fails
with `<command>: not supported (<reason>)`.

## Running grofe as `tmux`

Once your config has moved, you can put grofe on your `PATH` under the name
`tmux`. Use a symlink, since a shell alias does not reach scripts. To try it
first without replacing tmux, link it somewhere off your `PATH` and call it
by its full path:

```sh
mkdir -p ~/grofe-tmux
ln -s ~/.cargo/bin/grofe ~/grofe-tmux/tmux
~/grofe-tmux/tmux -V
```

A server started under that name has **tmux identity**. It reports itself as
tmux (`TERM_PROGRAM=tmux`, and `tmux -V` prints the tmux release grofe is
compatible with), and starts with tmux 3.7b's option defaults and stock key
bindings rather than grofe's own: base index 0, mouse off, numbered session
names, shared pane borders, and `prefix f` as `find-window`. Your config's
values still win; set `pane-frames = true` to get framed panes there too.
`compat-identity = "tmux"` gives a server started as `grofe` the same
identity. The [options reference](reference/options.md) lists every default
that differs.

Under either name, `-L` sockets live in grofe's own directory, never in
tmux's. Run as `tmux`, grofe refuses a `-S` path inside tmux's socket
directory and never replaces a socket it did not create.

Inside grofe, `tmux` is always grofe: the server puts a `tmux` link to itself
first on the `PATH` of its panes and of the commands it runs. Run real tmux
there by its full path, as `env -u TMUX /path/to/tmux`, because `$TMUX`
points at grofe.
