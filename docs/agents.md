# Coding agents

grofe knows which panes run a coding agent (Claude Code, Codex, Gemini CLI,
opencode, pi, aider, or anything you teach it), what each one is doing, and
which ones need you. It shows that in the status line, a picker, a sidebar and
a grid, and tells you through a notification tray, toasts and desktop
notifications. All of it is built in: no plugin, no session manager and no
helper script is needed.

Everything here is on under grofe's own identity (running `grofe`). Under the
tmux identity (grofe installed as `tmux`) the agent tracking and commands still
work, but the default keys, glyphs and badge are left out so the stock tmux
setup is unchanged. The commands are grofe's own, so call them as `grofe`:
under the `tmux` name they are unknown commands, as they are in tmux.

## Quick start

1. Install the hooks for the agents you use. This edits the agent's own
   config, shows the change and asks first:

   ```sh
   grofe agent setup claude      # also: codex, gemini, opencode
   grofe agent setup --status    # per kind: installed, not installed or agent not found
   ```

2. Start an agent in a grofe pane as usual. To let grofe start it with
   `grofe agent new claude` (see [Starting agents](#starting-agents)).
3. Press `prefix u` to jump to the agent that needs you most.

Agents with no hooks still show up: grofe recognises them by program name and
treats a bell or a desktop notification escape from them as "needs a look".

After upgrading grofe, restart the server (`grofe kill-server`, or `prefix Q`)
so it runs the new build; a running server keeps the build it started with.

## States

Each agent pane has one state:

| State | Glyph | Meaning |
|---|---|---|
| `blocked` | `⏸` | waiting for you: a permission prompt, a question, a login |
| `error` | `✗` | it stopped on an error |
| `working` | `●` | busy |
| `idle` | | started, nothing going on |
| `done` | | finished its turn |

A pane is also **unseen** (`✓`) when it finished or rang a bell while you were
looking elsewhere. Looking at it (the active pane of a focused client) clears
that. Lists are sorted by urgency: blocked, error, unseen, working, idle,
done, oldest first within each.

## Keys

| Key | Command | What it does |
|---|---|---|
| `prefix u` | `agent next` | jump to the most urgent agent |
| `prefix U` | `choose-agent` | the agent picker |
| `prefix b` | `agent-sidebar toggle` | show or hide the sidebar |
| `prefix O` | `agent-grid` | the grid overview |
| `prefix y` | `choose-notification` | the notification tray |
| `prefix Space` | `key-menu -T prefix` | the key menu |
| `prefix M-Space` | `next-layout` | moved here from `prefix Space` |

Rebind any of them like any other key, in TOML or with `bind-key`.

## The key menu

`prefix Space` lists the bindings of the prefix table by group (agents,
notifications, pane, window and so on), each with its key and description.
Pressing a key that is bound in the table runs that binding at once and
closes the menu. The menu keeps `C-n` and `C-p` (move), `C-f` (start
filtering, then `Enter` runs the selected row), `Escape` and `C-c` (close)
for itself; a binding on one of those keys is still listed and can be reached
by filtering. A binding that runs `switch-client -T <table>` opens that
table's menu.

`key-menu -T <table>` opens the menu for any key table. The group comes from
`bind-key -G <group>` or a `group` in a TOML binding, and the description from
`bind-key -N` or `desc`. Both are formats, so a group such as
`Agents (#{agents_attention})` works.

## The picker

`prefix U` opens a list of every agent with its state, time in that state,
`session:window.pane`, kind, last message and directory, and a live preview of
the selected pane below it. Type to filter. `Enter` jumps, `C-x` kills the pane
(it asks first), `C-y` copies the pane's target, `C-s` marks it seen, `Escape`
closes.

Jumping keeps you in your session when the pane's window is in it, and
switches session otherwise.

## The sidebar

`prefix b` shows a strip at the right of your view listing the agents, grouped
by session, two lines each. When panes are framed it sits in a frame like theirs, styled by
`pane-border-style` (`pane-active-border-style` while it has the keys), and the
selected row takes `mode-style`. It is drawn by grofe, not a pane, so it runs
no process. It is per client and hidden until you toggle it (or set
`show-on-attach`), and it hides itself while the terminal is narrower than
`min-client-width`. Click a row to jump; `agent-sidebar focus` gives it the
keys until `Escape`.

## The grid

`prefix O` shows every agent at once, one tile each with the bottom lines of
its pane. It is read only: arrows move, typing filters, `Enter` jumps,
`Escape` closes.

## Notifications

A notification is raised when an agent enters a state listed in
`notify.on` (by default `blocked`, `error` and `done`), unless you are already
looking at it. A `done` after less than `done-min-work` of work is not worth
a notification and is skipped.

- **Tray:** the last `history` notifications (default 10). `prefix y` opens
  it: `Enter` jumps, `C-x` dismisses one, `C-l` clears all. From the shell:
  `grofe notify list`, `grofe notify dismiss <id>`, `grofe notify clear`.
- **Status badge:** the default status line shows a badge such as `⏸2 ✓1`
  when something needs you; click it to open the tray. Window tabs get the
  agent glyph, and a blocked pane's border turns yellow.
- **Toasts:** a short message in a corner, by default for `blocked` only.
- **Desktop:** by default for `blocked`, `error` and `done`. With
  `delivery = "auto"` grofe sends OSC 9 to your terminal when it knows the
  terminal is not focused (Ghostty, WezTerm, iTerm2 and kitty show it as a
  desktop notification, over ssh too), and runs `notify-send` (Linux) or
  `osascript` (macOS) when no client is attached.
- **Mute:** `grofe notify mute [duration]` sends everything to the tray only
  until `grofe notify unmute` or the duration ends; the badge shows `🔕`.
  `notify.enabled = false` turns notifications off altogether.

"Not focused" needs `focus-events` on; with it off grofe assumes you are
looking.

## Starting agents

```sh
grofe agent new claude                 # new window, current directory
grofe agent new codex -p               # split the current pane instead
grofe agent new claude -w -b fix/api   # in a new git worktree on branch fix/api
```

Each built-in kind starts the program of its own name. To run something else,
set `command` for the kind:

```toml
[agents.kinds.claude]
command = "claude"
```

Options: `-c dir` (default: the current pane's directory), `-n name` (the
window name), `-d` (do not switch to it), `-b branch`, `-w` always use a new
worktree, `-W` never. Without `-w` or `-W`, the kind's own `worktree` key or
else `agents.new.worktree` decides (`never` by default, or `always`, or `ask`,
which prompts for a branch; an empty answer means no worktree). A worktree
needs a branch: from the shell, pass `-b`.

Worktrees go to `<repo>/../worktrees/<branch>`, with any `/` in the branch
turned into `-`. Set `agents.new.worktree-command` to use another tool (for
example worktrunk); when its last line of output is an existing directory,
that directory is used.

## How grofe learns an agent's state

- **Hooks** (`grofe agent setup <kind>`): the agent calls
  `grofe agent report <state> --hook <kind>` on its own events. Hook mode
  reads the hook payload from stdin, never blocks the agent, and always exits 0.
  For `claude`, `codex` and `gemini` the setup merges hooks into the agent's
  JSON file (`$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`,
  `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`,
  `~/.gemini/settings.json`). It edits the file in place (a symlinked
  dotfile stays a symlink) and keeps a one-time backup next to it
  (`<file>.grofe-bak`). For `opencode` it writes a plugin,
  `~/.config/opencode/plugins/grofe.js`. `pi` needs no hooks; grofe
  recognises it by name. `--dry-run` shows the change and writes nothing,
  `--yes` skips the question, and `--remove` takes out exactly what it
  added.
- **Reports from anything:** a script can call
  `grofe agent report <state> [--kind k] [--msg text] [--blocked permission|question|auth]`
  from inside a grofe pane. Outside one, or with no server, it does nothing.
- **OSC 7501**, for programs that speak it:
  `ESC ] 7501 ; state=blocked:kind=permission:msg=<base64> ST`.
- **By program name:** the foreground program is matched against each kind's
  `match` list (looking through `node`, `npx`, `uv` and similar launchers). A
  bell or an OSC 9/777 notification from an agent that sends no reports marks
  it unseen.

Agent text is shown as text: control characters are removed, and it can never
run a command or start a status line style.

## Configuration

All of this is set in your TOML config. The defaults:

```toml
[agents]
enabled = true            # false: no records, no surfaces
scan-interval = "1s"      # how often a busy pane's program is checked

[agents.kinds.claude]     # merges over the built-in kind; enabled = false removes it
match = ["claude"]
resume = "claude --resume {id}"
# command = "claude"      # default: the kind's own name
# worktree = "ask"        # overrides agents.new.worktree for this kind

[agents.new]
worktree = "never"        # ask | always | never
worktree-command = ["git", "-C", "{dir}", "worktree", "add", "-b", "{branch}", "{path}"]

[agents.picker]
width = "80%"
height = "70%"

[agents.sidebar]
width = 32
show-on-attach = false
min-client-width = 120

[agents.grid]
tile-width = 60
preview-lines = 12

[notify]
enabled = true
on = ["blocked", "error", "done"]
toast = ["blocked"]
desktop = ["blocked", "error", "done"]
history = 10
skip-when-seen = true
done-min-work = "30s"
min-interval = "10s"
toast-time = "5s"
delivery = "auto"         # auto | terminal | command | off
sound = false
# command = ["my-notifier", "--", "{title}", "{message}"]
```

A custom `notify.command` is an argv, never run through a shell; `{title}`,
`{message}` and `{pane}` are each replaced inside one element. Put `--` before
them so agent text is never read as an option.

Built-in kinds: `claude`, `codex`, `opencode`, `pi`, `gemini` and `aider`.
Each matches its own name. Three come with a resume command:
`claude --resume {id}`, `codex resume {id}` and `opencode --session {id}`.
Add your own kind with an `[agents.kinds.<name>]` table. `agents.picker`
sizes are percentages written as strings (`"80%"`); the sidebar and grid
sizes are plain numbers of cells.

The look comes from options (see the [config reference](config-reference.md)):
`agent-glyph-blocked`, `agent-glyph-error`, `agent-glyph-unseen`,
`agent-glyph-working`, `notify-muted-glyph` and `agent-blocked-border-style`.

## Formats

For your own status line or scripts:

| Variable | Value |
|---|---|
| `agent_kind`, `agent_state`, `agent_msg`, `agent_blocked_kind` | the pane's agent; empty when none |
| `agent_since` | seconds in the current state |
| `agent_unseen`, `agent_attention` | `1` or `0` |
| `window_agent_state`, `session_agent_state` | the most urgent state in the window or session |
| `agents_total`, `agents_blocked`, `agents_working`, `agents_attention` | counts |
| `notifications_unread` | count |
| `agents_badge` | for example `⏸2 ✓1`; empty when nothing needs you |

`grofe agent list -F '#{agent_kind} #{agent_state} #{agent_msg}'` prints one
line per agent, most urgent first.
