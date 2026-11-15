# Getting started

Running `grofe` on its own starts a session and attaches to it. To see the
pieces one at a time, start a session in the background, check it is there
and attach:

```sh
grofe new-session -d -s demo
grofe list-sessions
grofe attach -t demo
```

A session started without `-s` is named after the git repository it starts
in, or else its folder (`home` for `~`), with `-2`, `-3` for more in the same
place.

## Keys

The keys are tmux's. The prefix is `Ctrl-b`; after it:

| Key | Does |
|---|---|
| `%` | split side by side |
| `"` | split top and bottom |
| `c` | new window |
| arrows | move between panes |
| `f` | float or unfloat the pane |
| `S`, `a`, `A` | [scratchpads](scratchpads.md) |
| `u`, `U` | jump to or pick a [coding agent](agents.md) |
| `:` | command prompt |
| `Space` | the key menu: every binding, grouped |
| `d` | detach |
| `Q` | stop the server (asks first) |

The session keeps running after you detach; `grofe attach -t demo` brings it
back. `grofe list-keys -T prefix` lists every binding and where it came from.

## Commands

Everything a key does, the command line does too, with tmux's command names:

```sh
grofe split-window -h -c ~/src
grofe send-keys -t demo 'make test' Enter
grofe split-window -h ';' select-pane -L
```

A command string can run several commands in a row, separated by `;`.

## Stopping and coming back

`grofe kill-server` (or `prefix Q`) stops the server and every session in it.
That does not forget your work: grofe keeps a snapshot of its sessions, and
the next time the server starts they come back, just as they would after a
reboot. To get rid of a session for good, close it (`kill-session`, or exit
its last pane) before stopping the server. To always start empty, set
`restore-on-start = false` in `config.toml`.

Panes come back in the same layout and directory. A pane that knows how to
resume comes back running its program; the others come back as a shell.
Before a reboot, `grofe reboot-check` lists what will come back and how.
