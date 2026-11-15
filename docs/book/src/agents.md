# Agent sessions

grofe knows when a pane is running a coding agent, what state it is in, and
whether it needs you. It recognises `claude`, `codex`, `opencode`, `pi`,
`gemini` and `aider` by the program a pane runs, and learns more from the
agent's own hooks.

## Set up the hooks

Recognition by name alone tells grofe an agent is there. To learn when it is
working, waiting on you or done, install grofe's hooks into the agent's
config:

```sh
grofe agent setup claude --dry-run   # show what would change
grofe agent setup claude             # back up the file, then add the hooks
grofe agent setup --status           # which agents have grofe's hooks
grofe agent setup claude --remove
```

`setup` works for `claude`, `codex`, `gemini` and `opencode`, needs no
server, and keeps a backup of the file it edits as `<file>.grofe-bak`. `pi`
and `aider` have no hooks and are recognised by name only.

Anything else can report its state too:

```sh
grofe agent report blocked --msg "needs approval"
```

`report` targets `$GROFE_PANE` unless given `-t`, and does nothing (exiting
0) outside grofe.

## States

| State | Means |
|---|---|
| `working` | busy |
| `blocked` | waiting on you: a permission, a question or a login |
| `error` | stopped with an error |
| `done` | finished its turn |
| `idle` | started, nothing asked yet |

An agent that becomes blocked, done or errors while you are not looking at
its pane is also **unseen** until you look. Blocked, error and unseen agents
need your attention.

## Finding the agent that needs you

| Key | Command | Does |
|---|---|---|
| `prefix u` | `agent next` | jump to the most urgent agent that needs you |
| `prefix U` | `choose-agent` | the picker: every agent, with a live preview |
| `prefix b` | `agent-sidebar toggle` | a sidebar listing agents beside your panes |
| `prefix O` | `agent-grid` | every agent's latest output as tiles |
| `prefix y` | `choose-notification` | the notification tray |

In the picker, type to filter, `Enter` jumps, `C-s` marks an agent seen,
`C-y` copies its target and `C-x` kills its pane (after asking). In the grid,
the arrows move and `Enter` jumps. These keys are grofe's own; under tmux
identity the prefix table is tmux's.

The status line shows each window's agent glyph in the window list and, at
the right, a badge counting the agents that need you (blocked, error,
unseen). Click the badge to open the tray. A blocked agent's pane frame takes
`agent-blocked-border-style`. The glyphs are options: `agent-glyph-blocked`,
`agent-glyph-error`, `agent-glyph-unseen`, `agent-glyph-working` and
`notify-muted-glyph`.

`grofe agent list` prints every agent; `grofe agent seen -a` marks them all
seen.

## Notifications

When an agent becomes blocked, errors or finishes a long turn, grofe adds a
notification to the tray, shows a toast for blocked agents, and sends a
desktop notification (`terminal-notifier` or `osascript` on macOS,
`notify-send` on Linux). Nothing is sent for a pane you are looking at.

```sh
grofe notify mute 30m    # tray only, no toasts or desktop notifications
grofe notify unmute
grofe notify list
grofe notify clear
```

## Starting an agent

`grofe agent new claude` opens a new window running that kind's command
(`-p` splits the current pane instead). `-c <dir>` picks the folder,
`-n <name>` names it, `-d` leaves it in the background, and `-w` starts it in
a new git worktree (`-b <branch>` names the branch; `-W` says no worktree when
the config would make one).

## Coming back after a restart

When an agent reports its session id and its kind has a resume command,
grofe remembers how to resume it, so after a restart or reboot the pane
comes back in the same conversation: `claude --resume <id>`,
`codex resume <id>`, `opencode --session <id>`.

## Configuration

```toml
[agents]
enabled = true
scan-interval = "1s"

[agents.kinds.claude]
command = "claude"
resume = "claude --resume {id}"

[agents.sidebar]
width = 32
show-on-attach = false
min-client-width = 120      # hide the sidebar on narrower clients

[notify]
on = ["blocked", "error", "done"]
toast = ["blocked"]
desktop = ["blocked", "error", "done"]
done-min-work = "30s"       # only notify "done" after this much work
delivery = "auto"           # terminal, command or off
```

A `[agents.kinds.<name>]` table adds a kind or changes a built-in one key by
key: `match` (program names), `command`, `resume` and `enabled`.
