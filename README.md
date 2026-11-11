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

## Framed panes

Under the grofe name a window is a canvas. Each tiled pane is a rounded tile
with its own frame, its title drawn in the frame's top edge, and a cell of
gap between tiles and around the outside:

```
 ╭─ 1: nvim ────── ( z f x ) ─╮ ╭─ 2: zsh ────── ( z f x ) ─╮
 │                            │ │                           │
 │                            │ ╰───────────────────────────╯
 │                            │
 │                            │ ╭─ 3: cargo ──── ( z f x ) ─╮
 │                            │ │                           │
 ╰────────────────────────────╯ ╰───────────────────────────╯
```

The active pane's frame takes `pane-active-border-style`, the others
`pane-border-style`, and the title is `pane-border-format`. With `mouse` on,
each title ends in a small tab of controls drawn as a pill in the frame's own
colour: `z` zooms the pane (`u` unzooms), `f` floats it (`t` puts a floating
pane back in the tiling) and `x` closes it, after asking. They are the mouse
keys `MouseDown1Control8`, `Control7` and `Control9`, so they can be rebound.
The default `pane-border-format`'s `[z][x]` buttons give way to the tab.
The tab and the pane index in the title are outlined in the frame's colour,
with no background. A pane whose title no program has set (tmux gives it
the hostname) shows its command and folder instead, `zsh · grofe`. `pane-frame-tab-ends` sets their two end glyphs: `()` by
default, which any font has; with a Nerd font `""` gives thin rounded
ends, and an empty value blank ones. `#{pane_width}`,
the pty size and `#{window_layout}` are the content, inside the frames. A
window too small to frame every pane falls back to tmux's shared borders.
The keys in `config.toml`:

```toml
pane-frames = true          # false: tmux's shared one-cell borders
pane-gap = 1                # cells between frames and around them, 0 to 8
pane-frame-title = true     # false: a plain top edge
pane-frame-tab-ends = "()"  # the control tab's end glyphs

[pane-border]
lines = "rounded"           # the default with frames; single, double, heavy, ...
```

Under the `tmux` name nothing changes: shared borders, tmux's geometry, and
the frame options are unknown to tmux's `set-option`. Set `pane-frames = true`
(and `pane-gap` if you like) in `config.toml` to get the framed canvas there
too; the titles then keep tmux's own `[z][x]` and draw no tab.

With `mouse` on, panes move with the mouse, as windows do on a desktop. Drag a
tile by its title (or Ctrl-drag anywhere in it, `drag-pane -M`) and it lifts
out of the tiling: it follows the pointer as a floating frame, its content
live, the grabbed cell under the pointer, while its old place stays empty, and
a preview frame shows where it will land. Drop it on another pane's middle
and the two swap; drop it on the outer quarter of a side and it docks there,
splitting that pane; drop it on itself and nothing changes; drop it on a gap,
off every tile, and it stays floating there, at its size. A right click or
Escape before the release puts it back. The program inside learns its new
size once, on the release. Dragging a gap or any other frame edge
resizes, as a border drag does in tmux. Panes can float above the tiles:
`prefix f` (or `F`) floats the active pane at its tiled size, nudged 2 right
and 1 down so the change shows,
or puts a floating one back (`float-pane`, `unfloat-pane`), the right-click
pane menu has Float and Unfloat, and a Ctrl-drag of a floating pane drops it
into the tiling. The Ctrl-drag, `prefix f`/`F` and the menu items are
grofe-name defaults; under the `tmux` name the stock key table (with `f` as
`find-window`) and menu stay. See
`docs/floating-pane-mouse.md`.

## Switching from tmux

> **Know the gaps first.** The tmux compatibility layer answers what the
> tools and scripts it was tested against send, but some things are not there
> yet or never will be:
>
> - **Plugins.** TPM itself does not run. `import-tmux` turns its `run`
>   line into a `run-shell` of each installed plugin's `*.tmux`, in TPM's
>   order, so the plugins you already have load unchanged; nothing is
>   cloned or updated. Resurrect and continuum are left out, since grofe's
>   own persistence replaces them. A plugin's own `*.tmux` and
>   `*.conf` files are read as tmux config only when the request comes
>   through the `tmux` name or the server has tmux identity.
> - **Copy mode and paste buffers** (`copy-mode`, `send-keys -X`, the
>   `*-buffer` commands) work, with tmux 3.7b's stock `copy-mode-vi` and
>   `copy-mode` (emacs) tables. Under the `tmux` name `mode-keys` defaults
>   to `emacs`, as in tmux (`vi` when `$VISUAL` or `$EDITOR` names a vi).
> - **`tmux -f` takes a grofe TOML file**, not a `tmux.conf`. Convert your
>   config with `grofe import-tmux` first.
> - **Out of scope:** tmux's control mode (`-C` and `-CC`). A window linked
>   twice into one session (`link-window` onto a session that has it) is
>   refused.
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

The converted config also says `start-from-tmux = true`: a grofe server that
starts with no sessions takes them from your running tmux server (names,
windows, layouts and directories, with fresh shells, since the programs are
still running in tmux), or, with no tmux running, from tmux-resurrect's
newest save in `resurrect-dir`, where a pane saved with a command runs it
again. Only the default server does this; one started with `-L preview` (or
any other socket) starts from its own snapshot, so you can try a new build
next to your main server. When bare `grofe` starts the server and it comes
up with restored sessions, you are attached to them rather than to a new
empty session on top.

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

## Alias day

Alias day is the day grofe becomes your `tmux` for good: on the release
tag, and only after `scripts/release-gate.sh` is green. These are the
owner's steps, for a machine set up with athome (chezmoi dotfiles that
install grofe and canopy). Every step has a stand-in that the release gate
runs in a container; the commands marked for it below are run there by
`tests/e2e/scenarios/readme.sh`, exactly as written here.

**1. Pin and apply.** In athome, set the `ref=` line (and the `ref:`
comment above it) to the release commit in
`run_onchange_17-install-grofe.sh.tmpl`, and to canopy's release tag in
`run_onchange_18-install-canopy.sh.tmpl`. Then apply athome the way its
`AGENTS.md` says: read the diff, copy anything precious out of the way, and
apply.

```sh
chezmoi diff
chezmoi apply
```

The apply links `~/.local/lib/grofe/bin/tmux` to grofe, puts that directory
first on `PATH`, installs grofe's config and canopy's grofe fragments, and
enables the login unit, `canopy-tmux.service`. Open a new shell so it picks
up the new `PATH`.

**2. Check the login.** At your next login the unit starts grofe as `tmux`.
Check that it did, and that canopy and grofe agree on what a reboot would
bring back:

<!-- alias-day: begin -->
```sh
systemctl --user is-active canopy-tmux.service
command -v tmux
tmux display -p 'grofe #{grofe_version}, as tmux #{version}'
~/.local/share/canopy/bin/canopy doctor
grofe reboot-check
```
<!-- alias-day: end -->

`command -v tmux` names `~/.local/lib/grofe/bin/tmux`, and `display` prints
a grofe version: only grofe knows `#{grofe_version}`. athome does not put
canopy on `PATH`, so these steps call it by its full path.

**3. Reboot for real**, on each machine. Every session comes back with its
panes' `uid`s, and every Claude Code pane is back in its own conversation.

**4. Work under the alias for a week.** Write each difference from tmux in
`docs/dogfood.md`, tagged `compat` or `copy`; each one becomes a scenario
and then gets fixed. Read `compat-miss.log` in grofe's state directory each
day.

**5. Keep real tmux installed** until that week ends with no open entry.
It stays reachable by its full path; inside a grofe pane run it as
`env -u TMUX /usr/bin/tmux`, because `$TMUX` there points at grofe.
Removing it is your call.

### Rolling back

At any point you can go back to real tmux. Stop the login unit, which ends
the grofe server and every session in it, remove the `tmux` symlink (grofe
itself and its state stay where they are), and switch canopy back to its
tmux backend:

<!-- alias-day: begin -->
```sh
systemctl --user stop canopy-tmux.service
rm ~/.local/lib/grofe/bin/tmux
hash -r
tmux -V
~/.local/share/canopy/bin/canopy install --yes
systemctl --user start canopy-tmux.service
tmux display -p 'tmux #{version}'
~/.local/share/canopy/bin/canopy doctor
```
<!-- alias-day: end -->

`tmux -V` now prints the real tmux's version. `canopy install --yes` takes
over `~/.config/tmux/tmux.conf`: athome's copy is moved into
`~/.config/canopy/user.conf`, and the original is kept in a restore point.
Revert the athome change too, so the next machine you apply it to is not
switched. grofe's state lives in its own directory and touches nothing of
tmux's, so nothing else needs cleaning up.

To undo `canopy install` later (before going back to grofe, or to hand
`tmux.conf` back to chezmoi), stop the server and restore. Every file canopy
changed comes back byte for byte:

<!-- alias-day: begin -->
```sh
systemctl --user stop canopy-tmux.service
~/.local/share/canopy/bin/canopy restore --all
```
<!-- alias-day: end -->

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
is not a grofe server. Inside grofe, `tmux` is grofe under either name: the
server puts a `tmux` link to itself first on the `PATH` of its panes and of
the commands it runs, so scripts and plugins that call `tmux` reach it. Run
real tmux there by its full path, as `env -u TMUX /path/to/tmux`.

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

When you are done, stop the server and every session in it, with
`prefix Q` (it asks first) or:

<!-- readme-test: begin -->
```sh
grofe kill-server
```
<!-- readme-test: end -->

## Plugins

grofe runs plugins as separate processes. Declare one in your config and the
server starts it with the server, restarts it when it exits (backing off, and
disabling it after five crashes in a minute), and stops it on `kill-server`:

```toml
[plugins.event-log]
cmd = ["event-log/event-log.sh"]
env = { LC_ALL = "C" }
restart = "always"
```

Only `cmd[0]` is a path: a relative one containing a `/` resolves against the
directory of the config file that sets it, and every later word is passed as
written. A script that needs an interpreter should be `cmd[0]` itself (a
shebang line and the exec bit) or be given by an absolute path. The plugin gets `GROFE` (the socket), `GROFE_PLUGIN` (its name)
and `GROFE_STATE` (the state directory); its output goes to
`<state>/plugins/<name>.log`. `list-plugins` and `plugin-restart <name>` show
and restart them.

A plugin talks to the server with `grofe control`, a stdio bridge that speaks
[protocol v1](docs/protocol-v1.md): one JSON request per line in, replies and
events out, so a plugin in any language needs no socket code.

```sh
printf '{"id":1,"argv":["subscribe"]}\n' | grofe control
```

[`plugins/event-log`](plugins/event-log/README.md) is a working example in
POSIX sh: it subscribes to every event and appends each as a JSON line to
`<state>/events.log`. Copy it next to your config and use the snippet above.

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
