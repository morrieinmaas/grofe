# Options

<!-- Generated from the option table in src/server/dispatch/options/table.rs and lists.rs.
     Do not edit by hand: run `just docs` to regenerate. -->

Every built-in option, by its tmux name. Set one in `config.toml` by its
TOML key (a dotted key such as `status.right` is the `right` key in the
`[status]` table), or at runtime by its tmux name:

```sh
grofe set-option -g status-interval 5
grofe show-options -g status-interval
```

The scope is the one `set-option` and `show-options` use when no scope flag
is given. Under tmux identity (grofe run as `tmux`, or
`compat-identity = "tmux"`) some defaults are tmux's instead; those are
listed with each option. Options marked as having no effect are accepted so
that tmux configs and plugins load, but grofe never acts on them.

## activity-action

Which windows' activity alerts are acted on: any, none, the current one or the others.

- **TOML key**: `activity-action` (string)
- **Scope**: session
- **Values**: `none`, `any`, `current`, `other`

Default: `other`

## agent-blocked-border-style

Border style of a pane whose agent is waiting on you, active or not.

- **TOML key**: `agent-blocked-border-style` (string)
- **Scope**: session
- **Values**: text

Default: `fg=yellow`

grofe only: tmux does not have this option.

## agent-glyph-blocked

Glyph for an agent waiting on you, in the window list and the agents badge.

- **TOML key**: `agent-glyph-blocked` (string)
- **Scope**: session
- **Values**: text

Default: `⏸`

grofe only: tmux does not have this option.

## agent-glyph-error

Glyph for an agent that stopped with an error, in the window list and the agents badge.

- **TOML key**: `agent-glyph-error` (string)
- **Scope**: session
- **Values**: text

Default: `✗`

grofe only: tmux does not have this option.

## agent-glyph-unseen

Glyph for an agent that finished and has not been looked at, in the window list and the agents badge.

- **TOML key**: `agent-glyph-unseen` (string)
- **Scope**: session
- **Values**: text

Default: `✓`

grofe only: tmux does not have this option.

## agent-glyph-working

Glyph for a working agent, in the window list.

- **TOML key**: `agent-glyph-working` (string)
- **Scope**: session
- **Values**: text

Default: `●`

grofe only: tmux does not have this option.

## aggressive-resize

Resize a window to the clients that show it rather than to every client attached to the session.

- **TOML key**: `aggressive-resize` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

Accepted for compatibility; it has no effect in grofe.

## allow-passthrough

Let programs pass escape sequences through to the outer terminal: off, on (visible panes only) or all.

- **TOML key**: `allow-passthrough` (string)
- **Scope**: window
- **Values**: `off`, `on`, `all`

Default: `off`

## allow-rename

Let a program rename its window with an escape sequence.

- **TOML key**: `allow-rename` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## allow-set-title

Let a program set its pane's title with an escape sequence.

- **TOML key**: `allow-set-title` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

## alternate-screen

Let programs use the alternate screen.

- **TOML key**: `alternate-screen` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Accepted for compatibility; it has no effect in grofe.

## assume-paste-time

Keys arriving faster than this many milliseconds apart are taken as a paste, not bindings.

- **TOML key**: `assume-paste-time` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `1`

Accepted for compatibility; it has no effect in grofe.

## automatic-rename

Rename a window after automatic-rename-format as its pane changes.

- **TOML key**: `automatic-rename` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

## automatic-rename-format

Format for a window's name while automatic-rename is on; grofe's default is the current folder.

- **TOML key**: `automatic-rename-format` (string)
- **Scope**: window
- **Values**: text

Default: `#{b:pane_current_path}`

Default under tmux identity:

```text
#{?pane_in_mode,[tmux],#{pane_current_command}}#{?pane_dead,[dead],}
```

## backspace

The key sent for Backspace.

- **TOML key**: `backspace` (string)
- **Scope**: global
- **Values**: a key name, or `None`

Default: `C-?`

Accepted for compatibility; it has no effect in grofe.

## base-index

The number the first window of a new session gets.

- **TOML key**: `base-index` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `1`

Default under tmux identity: `0`

## bell-action

Which windows' bells are acted on: any, none, the current one or the others.

- **TOML key**: `bell-action` (string)
- **Scope**: session
- **Values**: `none`, `any`, `current`, `other`

Default: `any`

## buffer-limit

How many automatically named paste buffers to keep; the oldest is dropped past this.

- **TOML key**: `buffer-limit` (integer)
- **Scope**: global
- **Values**: a whole number, at most 2147483647

Default: `50`

## clock-mode-colour

Colour of the clock in clock mode.

- **TOML key**: `clock-mode-colour` (string)
- **Scope**: window
- **Values**: text

Default: `blue`

## clock-mode-style

Whether clock mode shows a 12 or 24 hour clock, with or without seconds.

- **TOML key**: `clock-mode-style` (string)
- **Scope**: window
- **Values**: `12`, `24`, `12-with-seconds`, `24-with-seconds`

Default: `24`

## codepoint-widths

Width overrides for Unicode code points, each codepoint=width.

- **TOML key**: `codepoint-widths` (array of strings)
- **Scope**: global
- **Values**: a list of text, element N at index N

Default: empty

Accepted for compatibility; it has no effect in grofe.

## command-alias

Command aliases, each name=command; tmux's six defaults apply under tmux identity.

- **TOML key**: `command-alias` (array of strings)
- **Scope**: global
- **Values**: a list of text, element N at index N

Default: empty

Default under tmux identity:

```text
[0] split-pane=split-window
[1] splitp=split-window
[2] server-info=show-messages -JT
[3] info=show-messages -JT
[4] choose-window=choose-tree -w
[5] choose-session=choose-tree -s
```

## compat-identity

Whether the server behaves as grofe or as tmux; auto decides by the name it was started under. Fixed when the server starts.

- **TOML key**: `compat-identity` (string)
- **Scope**: global
- **Values**: `auto`, `tmux`, `grofe`

Default: `auto`

## copy-command

A shell command that copy mode's copy-pipe commands send the copied text to when they are given none.

- **TOML key**: `copy-command` (string)
- **Scope**: global
- **Values**: text

Default: empty

## copy-hint-patterns

Regular expressions copy-mode -h labels on screen, tried in order.

- **TOML key**: `copy-hint-patterns` (array of strings)
- **Scope**: global
- **Values**: a list of text, element N at index N

Default:

```text
[0] (?:https?|ftp|file|ssh|git)://[^\s<>"'`()\[\]{}]*[^\s<>"'`()\[\]{}.,;:!?]
[1] (?:^|[\s"'(=:])((?:~|\.{1,2})?(?:/[\w.@%+~-]+)+/?)
[2] (?:^|[\s"'(=])((?:[\w.@%+~-]+/)+[\w.@%+~-]+/?)
[3] (?:^|[\s\[(])((?:(?:[0-9a-fA-F]{1,4}:){1,6}|:):[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4}){0,5}|(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4})\b
[4] \b\d{1,3}(?:\.\d{1,3}){3}\b
[5] \b[0-9a-f]{7,40}\b
[6] \b\d{4,}\b
```

grofe only: tmux does not have this option.

## copy-mode-current-line-number-style

Style of the current line's number in copy mode.

- **TOML key**: `copy-mode-current-line-number-style` (string)
- **Scope**: window
- **Values**: text

Default: `fg=yellow`

Accepted for compatibility; it has no effect in grofe.

## copy-mode-current-match-style

Style of the current search match in copy mode.

- **TOML key**: `copy-mode.current-match-style` (string)
- **Scope**: window
- **Values**: text

Default: `bg=magenta,fg=black`

## copy-mode-line-number-style

Style of line numbers in copy mode.

- **TOML key**: `copy-mode-line-number-style` (string)
- **Scope**: window
- **Values**: text

Default: `fg=white,dim`

Accepted for compatibility; it has no effect in grofe.

## copy-mode-line-numbers

Show line numbers in copy mode, and which kind.

- **TOML key**: `copy-mode-line-numbers` (string)
- **Scope**: window
- **Values**: `off`, `default`, `absolute`, `relative`, `hybrid`

Default: `off`

Accepted for compatibility; it has no effect in grofe.

## copy-mode-mark-style

Style of the marked line in copy mode.

- **TOML key**: `copy-mode-mark-style` (string)
- **Scope**: window
- **Values**: text

Default: `bg=red,fg=black`

Accepted for compatibility; it has no effect in grofe.

## copy-mode-match-style

Style of search matches in copy mode.

- **TOML key**: `copy-mode.match-style` (string)
- **Scope**: window
- **Values**: text

Default: `bg=cyan,fg=black`

## copy-mode-position-format

Format of the position indicator in copy mode.

- **TOML key**: `copy-mode-position-format` (string)
- **Scope**: window
- **Values**: text

Default:

```text
#[align=right]#{t/p:top_line_time}#{?#{e|>:#{top_line_time},0}, ,}[#{copy_position}/#{copy_position_limit}]#{?search_timed_out, (timed out),#{?search_count, (#{search_count}#{?search_count_partial,+,} results),}}
```

## copy-mode-position-style

Style of the position indicator in copy mode.

- **TOML key**: `copy-mode-position-style` (string)
- **Scope**: window
- **Values**: text

Default: `#{E:mode-style}`

## copy-mode-selection-style

Style of the selection in copy mode.

- **TOML key**: `copy-mode-selection-style` (string)
- **Scope**: window
- **Values**: text

Default: `#{E:mode-style}`

Accepted for compatibility; it has no effect in grofe.

## cursor-colour

Cursor colour in panes.

- **TOML key**: `cursor-colour` (string)
- **Scope**: window
- **Values**: a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`

Default: `none`

Accepted for compatibility; it has no effect in grofe.

## cursor-style

Cursor shape in panes.

- **TOML key**: `cursor-style` (string)
- **Scope**: window
- **Values**: `default`, `blinking-block`, `block`, `blinking-underline`, `underline`, `blinking-bar`, `bar`

Default: `default`

Accepted for compatibility; it has no effect in grofe.

## default-client-command

Command run when grofe is started with no command.

- **TOML key**: `default-client-command` (string)
- **Scope**: global
- **Values**: text

Default: `new-session`

Accepted for compatibility; it has no effect in grofe.

## default-command

Command new panes run; empty runs default-shell.

- **TOML key**: `default-command` (string)
- **Scope**: session
- **Values**: text

Default: empty

## default-shell

Shell new panes run when default-command is empty; set from $SHELL when the server starts.

- **TOML key**: `default-shell` (string)
- **Scope**: session
- **Values**: text

Default: `/bin/sh`

## default-size

Size of new windows when a session starts detached or window-size is manual, as columns x rows.

- **TOML key**: `default-size` (string)
- **Scope**: session
- **Values**: `<columns>x<rows>`

Default: `80x24`

## default-terminal

The TERM value programs in panes see.

- **TOML key**: `default-terminal` (string)
- **Scope**: global
- **Values**: text

Default: `tmux-256color`

## destroy-unattached

Close a session when its last client detaches: off, on, keep-last or keep-group.

- **TOML key**: `destroy-unattached` (string)
- **Scope**: session
- **Values**: `off`, `on`, `keep-last`, `keep-group`

Default: `off`

## detach-on-destroy

What an attached client does when its session closes: detach (on), switch to the most recent other session (off), switch unless none is detached (no-detached), or go to the previous or next session.

- **TOML key**: `detach-on-destroy` (string)
- **Scope**: session
- **Values**: `off`, `on`, `no-detached`, `previous`, `next`

Default: `on`

## display-panes-active-colour

Colour of the active pane's number in display-panes.

- **TOML key**: `display-panes-active-colour` (string)
- **Scope**: session
- **Values**: a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`

Default: `red`

## display-panes-colour

Colour of the other panes' numbers in display-panes.

- **TOML key**: `display-panes-colour` (string)
- **Scope**: session
- **Values**: a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`

Default: `blue`

## display-panes-time

Milliseconds display-panes shows the pane numbers for.

- **TOML key**: `display-panes-time` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `1000`

## display-time

How long a message stays, in milliseconds.

- **TOML key**: `message.display-time-ms` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `750`

## editor

Editor run by commands that open one.

- **TOML key**: `editor` (string)
- **Scope**: global
- **Values**: text

Default: `/usr/bin/vi`

Accepted for compatibility; it has no effect in grofe.

## escape-time

Milliseconds to wait after an Escape for the rest of a key sequence.

- **TOML key**: `escape-time-ms` (integer)
- **Scope**: global
- **Values**: a whole number, at most 2147483647

Default: `25`

Default under tmux identity: `10`

## exit-empty

Stop the server when its last session closes.

- **TOML key**: `exit-empty` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

## exit-unattached

Stop the server when no client is attached.

- **TOML key**: `exit-unattached` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## extended-keys

Whether keys are sent to programs in the extended (CSI u) encoding: off, on (when a program asks) or always.

- **TOML key**: `extended-keys` (string)
- **Scope**: global
- **Values**: `off`, `on`, `always`

Default: `on`

Default under tmux identity: `off`

Accepted for compatibility; it has no effect in grofe.

## extended-keys-format

Encoding for extended keys sent to programs: xterm or csi-u.

- **TOML key**: `extended-keys-format` (string)
- **Scope**: global
- **Values**: `csi-u`, `xterm`

Default: `xterm`

Accepted for compatibility; it has no effect in grofe.

## fill-character

Character to fill unused parts of a window with; empty is a space.

- **TOML key**: `fill-character` (string)
- **Scope**: window
- **Values**: text

Default: empty

## focus-events

Pass focus in and out events to programs that ask for them.

- **TOML key**: `focus-events` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Default under tmux identity: `off`

## focus-follows-mouse

Make the pane under the mouse the active pane as the pointer moves.

- **TOML key**: `focus-follows-mouse` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## forward-browser

Set BROWSER to grofe open in new panes, so a remote client opens the URLs a pane asks for.

- **TOML key**: `forward-browser` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

grofe only: tmux does not have this option.

## get-clipboard

Whether a program's OSC 52 clipboard query is answered. Only buffer answers it, from the newest paste buffer, and only when set-clipboard is on; off, request and both are never answered, and the terminal's clipboard is never asked.

- **TOML key**: `get-clipboard` (string)
- **Scope**: global
- **Values**: `off`, `buffer`, `request`, `both`

Default: `buffer`

## graphics

Enable kitty graphics direct placements, animation and sixel (Unicode placeholders work without it).

- **TOML key**: `graphics` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## history-file

File the command prompt's history is saved to; empty saves none.

- **TOML key**: `history-file` (string)
- **Scope**: global
- **Values**: text

Default: empty

Accepted for compatibility; it has no effect in grofe.

## history-limit

How many lines of scrollback each new pane keeps.

- **TOML key**: `history-limit` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `50000`

Default under tmux identity: `2000`

## initial-repeat-time

Repeat window, in milliseconds, after the first press of a repeatable key; 0 uses repeat-time.

- **TOML key**: `initial-repeat-time` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2000000

Default: `0`

## input-buffer-size

Most bytes one escape sequence from a program may hold.

- **TOML key**: `input-buffer-size` (integer)
- **Scope**: global
- **Values**: a whole number, at most 4294967295

Default: `1048576`

Accepted for compatibility; it has no effect in grofe.

## key-table

The key table a client starts in.

- **TOML key**: `key-table` (string)
- **Scope**: session
- **Values**: text

Default: `root`

Accepted for compatibility; it has no effect in grofe.

## lock-after-time

Lock a session after this many seconds without input; 0 never locks.

- **TOML key**: `lock-after-time` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `0`

Accepted for compatibility; it has no effect in grofe.

## lock-command

Command run to lock a client.

- **TOML key**: `lock-command` (string)
- **Scope**: session
- **Values**: text

Default: `lock -np`

Accepted for compatibility; it has no effect in grofe.

## main-pane-height

Height of the main pane in the main-horizontal layout, in cells or as a percentage.

- **TOML key**: `main-pane-height` (string)
- **Scope**: window
- **Values**: text

Default: `24`

## main-pane-width

Width of the main pane in the main-vertical layout, in cells or as a percentage.

- **TOML key**: `main-pane-width` (string)
- **Scope**: window
- **Values**: text

Default: `80`

## menu-border-lines

Characters a menu's border is drawn with.

- **TOML key**: `menu-border-lines` (string)
- **Scope**: window
- **Values**: `single`, `double`, `heavy`, `simple`, `rounded`, `padded`, `none`

Default: `single`

## menu-border-style

Style of a menu's border.

- **TOML key**: `menu-border-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## menu-selected-style

Style of a menu's selected item.

- **TOML key**: `menu-selected-style` (string)
- **Scope**: window
- **Values**: text

Default: `bg=yellow,fg=black`

## menu-style

Style of menus.

- **TOML key**: `menu-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## message-command-style

Style of the command prompt while it is in vi command mode.

- **TOML key**: `message-command-style` (string)
- **Scope**: session
- **Values**: text

Default: `bg=black,fg=yellow,fill=black`

## message-format

Format of messages and the prompt line.

- **TOML key**: `message-format` (string)
- **Scope**: session
- **Values**: text

Default:

```text
#[#{?#{command_prompt},#{E:message-command-style},#{E:message-style}}]#{message}
```

## message-limit

How many messages to keep in the message log.

- **TOML key**: `message-limit` (integer)
- **Scope**: global
- **Values**: a whole number, at most 2147483647

Default: `1000`

## message-line

Which status row messages and the prompt use.

- **TOML key**: `message-line` (string)
- **Scope**: session
- **Values**: `0`, `1`, `2`, `3`, `4`

Default: `0`

## message-pill

Draw messages and the status-line prompt as a pill in the middle of the status line; off gives tmux's full-width bar.

- **TOML key**: `message.pill` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Default under tmux identity: `off`

grofe only: tmux does not have this option.

## message-pill-ends

Two glyphs drawn at the ends of the message and prompt pill, in its colour; empty takes the theme's @pill_l and @pill_r, else Nerd Font round caps.

- **TOML key**: `message.pill-ends` (string)
- **Scope**: session
- **Values**: text

Default: empty

grofe only: tmux does not have this option.

## message-style

Style of messages and of the command prompt.

- **TOML key**: `message.style` (string)
- **Scope**: session
- **Values**: text

Default: `bg=yellow,fg=black,fill=yellow`

## mode-keys

Key table copy mode and the choose modes use: vi or emacs.

- **TOML key**: `mode-keys` (string)
- **Scope**: window
- **Values**: `emacs`, `vi`

Default: `vi`

Default under tmux identity: `emacs`, or `vi` when `$VISUAL` or `$EDITOR` names a vi

## mode-style

Style of copy mode and of the choose and tree modes, including the selection.

- **TOML key**: `mode-style` (string)
- **Scope**: window
- **Values**: text

Default: `noattr,bg=yellow,fg=black`

## monitor-activity

Flag a window that has output while you are not looking at it.

- **TOML key**: `monitor-activity` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## monitor-bell

Flag a window that rings the bell.

- **TOML key**: `monitor-bell` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

## monitor-silence

Flag a window that has had no output for this many seconds; 0 turns it off.

- **TOML key**: `monitor-silence` (integer)
- **Scope**: window
- **Values**: a whole number, at most 2147483647

Default: `0`

## mouse

Mouse support: select, resize, drag and scroll panes and click the status line.

- **TOML key**: `mouse` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Default under tmux identity: `off`

## notify-muted-glyph

Glyph the agents badge shows while notifications are muted.

- **TOML key**: `notify-muted-glyph` (string)
- **Scope**: session
- **Values**: text

Default: `🔕`

grofe only: tmux does not have this option.

## other-pane-height

Height of the other panes in the main-horizontal layout; 0 shares the space.

- **TOML key**: `other-pane-height` (string)
- **Scope**: window
- **Values**: text

Default: `0`

## other-pane-width

Width of the other panes in the main-vertical layout; 0 shares the space.

- **TOML key**: `other-pane-width` (string)
- **Scope**: window
- **Values**: text

Default: `0`

## pane-active-border-style

Style of the active pane's border or frame.

- **TOML key**: `pane-border.active-style` (string)
- **Scope**: window
- **Values**: text

Default:

```text
#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}
```

## pane-base-index

The number the first pane of a window gets.

- **TOML key**: `pane-base-index` (integer)
- **Scope**: window
- **Values**: a whole number, at most 65535

Default: `1`

Default under tmux identity: `0`

## pane-border-format

Format of the pane status line shown in a border, and of a frame's title when it is changed from the default.

- **TOML key**: `pane-border.format` (string)
- **Scope**: window
- **Values**: text

Default:

```text
#{?pane_active,#[reverse],}#{pane_index}#[default] "#{pane_title}"#{?#{mouse},#[align=right]#[range=control|8][#{?#{window_zoomed_flag},u,z}]#[norange]#[range=control|9][x]#[norange],}
```

## pane-border-indicators

How shared borders mark the active pane: by colour, with arrows, both or not at all.

- **TOML key**: `pane-border.indicators` (string)
- **Scope**: window
- **Values**: `off`, `colour`, `arrows`, `both`

Default: `colour`

## pane-border-lines

Characters borders and frames are drawn with.

- **TOML key**: `pane-border.lines` (string)
- **Scope**: window
- **Values**: `single`, `double`, `heavy`, `simple`, `number`, `spaces`, `rounded`

Default: `rounded`

Default under tmux identity: `single`

## pane-border-status

Show a status line in the top or bottom border of each pane.

- **TOML key**: `pane-border.status` (string)
- **Scope**: window
- **Values**: `off`, `top`, `bottom`

Default: `off`

## pane-border-style

Style of the borders or frames of inactive panes.

- **TOML key**: `pane-border.style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## pane-colours

The colour palette programs in a pane see.

- **TOML key**: `pane-colours` (array of strings)
- **Scope**: window
- **Values**: a list of text, element N at index N

Default: empty

Accepted for compatibility; it has no effect in grofe.

## pane-frame-tab-ends

Two glyphs drawn around the frame's number and controls; empty draws none.

- **TOML key**: `pane-frame-tab-ends` (string)
- **Scope**: window
- **Values**: text

Default: `()`

grofe only: tmux does not have this option.

## pane-frame-title

Draw the pane's number, title and controls in its frame's top edge.

- **TOML key**: `pane-frame-title` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

grofe only: tmux does not have this option.

## pane-frames

Give each tiled pane its own rounded frame with its title, gaps between them; off gives tmux's shared borders.

- **TOML key**: `pane-frames` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Default under tmux identity: `off`

grofe only: tmux does not have this option.

## pane-gap

Cells of gap between pane frames and around the outside.

- **TOML key**: `pane-gap` (integer)
- **Scope**: window
- **Values**: a whole number, at most 8

Default: `1`

Default under tmux identity: `0`

grofe only: tmux does not have this option.

## pane-scrollbars

Show scrollbars in panes: off, in copy mode only (modal) or always.

- **TOML key**: `pane-scrollbars` (string)
- **Scope**: window
- **Values**: `off`, `modal`, `on`

Default: `off`

Accepted for compatibility; it has no effect in grofe.

## pane-scrollbars-position

Which side of the pane its scrollbar is on.

- **TOML key**: `pane-scrollbars-position` (string)
- **Scope**: window
- **Values**: `right`, `left`

Default: `right`

Accepted for compatibility; it has no effect in grofe.

## pane-scrollbars-style

Style and width of pane scrollbars.

- **TOML key**: `pane-scrollbars-style` (string)
- **Scope**: window
- **Values**: text

Default: `bg=black,fg=white,width=1,pad=0`

Accepted for compatibility; it has no effect in grofe.

## pane-status-current-style

Style of the current pane's entry in a status-format pane list.

- **TOML key**: `pane-status-current-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## pane-status-style

Style of a pane's entry in a status-format pane list.

- **TOML key**: `pane-status-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## popup-border-lines

Characters a popup's border is drawn with.

- **TOML key**: `popup-border-lines` (string)
- **Scope**: window
- **Values**: `single`, `double`, `heavy`, `simple`, `rounded`, `padded`, `none`

Default: `single`

## popup-border-style

Style of a popup's border.

- **TOML key**: `popup-border-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## popup-style

Style of popups.

- **TOML key**: `popup-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## prefix

The prefix key.

- **TOML key**: `prefix` (string)
- **Scope**: session
- **Values**: a key name, or `None`

Default: `C-b`

## prefix-timeout

Milliseconds after the prefix before it is cancelled; 0 waits forever.

- **TOML key**: `prefix-timeout` (integer)
- **Scope**: global
- **Values**: a whole number, at most 2147483647

Default: `0`

## prefix2

A second prefix key; None for none.

- **TOML key**: `prefix2` (string)
- **Scope**: session
- **Values**: a key name, or `None`

Default: `None`

## prompt-command-cursor-style

Cursor shape in the command prompt's vi command mode.

- **TOML key**: `prompt-command-cursor-style` (string)
- **Scope**: session
- **Values**: `default`, `blinking-block`, `block`, `blinking-underline`, `underline`, `blinking-bar`, `bar`

Default: `default`

Accepted for compatibility; it has no effect in grofe.

## prompt-cursor-colour

Cursor colour in the command prompt.

- **TOML key**: `prompt-cursor-colour` (string)
- **Scope**: session
- **Values**: a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`

Default: `none`

Accepted for compatibility; it has no effect in grofe.

## prompt-cursor-style

Cursor shape in the command prompt.

- **TOML key**: `prompt-cursor-style` (string)
- **Scope**: session
- **Values**: `default`, `blinking-block`, `block`, `blinking-underline`, `underline`, `blinking-bar`, `bar`

Default: `default`

Accepted for compatibility; it has no effect in grofe.

## prompt-history-limit

How many command prompt entries to keep in its history.

- **TOML key**: `prompt-history-limit` (integer)
- **Scope**: global
- **Values**: a whole number, at most 2147483647

Default: `100`

Accepted for compatibility; it has no effect in grofe.

## prompt-position

Where the command prompt opens: on the status line (status) or as a box a third down the window (centre).

- **TOML key**: `prompt-position` (string)
- **Scope**: session
- **Values**: `status`, `centre`, `center`

Default: `status`

grofe only: tmux does not have this option.

## remain-on-exit

Keep a pane open after its program exits: always (on), only when it failed (failed), or until a key is pressed (key).

- **TOML key**: `remain-on-exit` (boolean)
- **Scope**: window
- **Values**: `off`, `on`, `failed`, `key`

Default: `off`

## remain-on-exit-format

Text shown at the bottom of a pane kept open by remain-on-exit.

- **TOML key**: `remain-on-exit-format` (string)
- **Scope**: window
- **Values**: text

Default:

```text
Pane is dead (#{?#{!=:#{pane_dead_status},},status #{pane_dead_status},}#{?#{!=:#{pane_dead_signal},},signal #{pane_dead_signal},}, #{t:pane_dead_time})
```

## remote-forward-ports

What a remote client does when a pane starts listening on a port: ask forwards it to localhost (the same port, or the next free one) after a prompt, auto forwards without asking, off never forwards.

- **TOML key**: `remote-forward-ports` (string)
- **Scope**: session
- **Values**: `ask`, `auto`, `off`

Default: `ask`

grofe only: tmux does not have this option.

## remote-upload-max

The largest total size, in MiB, of the files one paste uploads to a remote session; a bigger paste is refused.

- **TOML key**: `remote-upload-max` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `100`

grofe only: tmux does not have this option.

## remote-upload-paste

What a remote client does with a bracketed paste of local file paths: ask uploads them to the pane's directory after a prompt, on uploads without asking, off pastes the paths as text.

- **TOML key**: `remote-upload-paste` (string)
- **Scope**: session
- **Values**: `ask`, `on`, `off`

Default: `ask`

grofe only: tmux does not have this option.

## renumber-windows

Renumber a session's windows when one is closed, so there are no gaps.

- **TOML key**: `renumber-windows` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Default under tmux identity: `off`

## repeat-time

Milliseconds during which a repeatable key (bound with -r) can be pressed again without the prefix.

- **TOML key**: `repeat-time-ms` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2000000

Default: `500`

## restore-on-start

Bring back the last snapshot's sessions when the server starts.

- **TOML key**: `restore-on-start` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

## restore-stagger-ms

Spread restored panes' resume commands over up to this many milliseconds; 0 starts them all at once.

- **TOML key**: `restore-stagger-ms` (integer)
- **Scope**: global
- **Values**: a whole number, at most 2147483647

Default: `1000`

## resurrect-dir

Where to look for tmux-resurrect saves for start-from-tmux; empty uses resurrect's own default.

- **TOML key**: `resurrect-dir` (string)
- **Scope**: global
- **Values**: text

Default: empty

## scratchpad-history

How many hidden scratchpads to keep; the oldest closes past this, 0 closes one as soon as it is hidden.

- **TOML key**: `scratchpad-history` (integer)
- **Scope**: global
- **Values**: a whole number, at most 100

Default: `10`

grofe only: tmux does not have this option.

## scroll-on-clear

Push the screen into the history when a program clears it.

- **TOML key**: `scroll-on-clear` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Accepted for compatibility; it has no effect in grofe.

## session-status-current-style

Style of the current session's entry in a status-format session list.

- **TOML key**: `session-status-current-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## session-status-style

Style of a session's entry in a status-format session list.

- **TOML key**: `session-status-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## set-clipboard

Where clipboard text goes: on makes a program's OSC 52 a paste buffer and passes it to the terminal, external sends only copy-mode copies to the terminal and ignores programs' OSC 52, off does neither.

- **TOML key**: `set-clipboard` (string)
- **Scope**: global
- **Values**: `off`, `external`, `on`

Default: `external`

## set-titles

Set the outer terminal's title from set-titles-string.

- **TOML key**: `set-titles` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## set-titles-string

Format for the outer terminal's title when set-titles is on.

- **TOML key**: `set-titles-string` (string)
- **Scope**: session
- **Values**: text

Default:

```text
#{session_name}:#{window_index}:#{window_name} - "#{pane_title}" #{session_alerts}
```

## silence-action

Which windows' silence alerts are acted on: any, none, the current one or the others.

- **TOML key**: `silence-action` (string)
- **Scope**: session
- **Values**: `none`, `any`, `current`, `other`

Default: `other`

## stable-ssh-agent

Give a session a stable SSH_AUTH_SOCK link that each attach repoints at the attaching client's agent, so old panes keep a working agent.

- **TOML key**: `stable-ssh-agent` (boolean)
- **Scope**: session
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

grofe only: tmux does not have this option.

## start-from-tmux

When the default server starts with no sessions, take them from a running tmux server or tmux-resurrect's newest save.

- **TOML key**: `start-from-tmux` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## status

Show the status line, and how many rows it has (on is one row).

- **TOML key**: `status.lines` (integer)
- **Scope**: session
- **Values**: `off`, `on`, `2`, `3`, `4`, `5`

Default: `on`

## status-bg

Background colour of the status line (an older spelling of status-style's bg).

- **TOML key**: `status-bg` (string)
- **Scope**: session
- **Values**: a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`

Default: `default`

## status-fg

Foreground colour of the status line (an older spelling of status-style's fg).

- **TOML key**: `status-fg` (string)
- **Scope**: session
- **Values**: a colour: `default`, `terminal`, a name such as `red`, `colourN` or `#rrggbb`

Default: `default`

## status-format

Format of each status line row; an empty row 0 draws the built-in row (left part, window list, right part).

- **TOML key**: `status.rows` (array of strings)
- **Scope**: session
- **Values**: a list of text, element N at index N

Default:

```text
[0] 
[1] #[align=left]#{R: ,#{n:#{session_name}}}P: #[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{P:#[range=pane|#{pane_id} #{E:pane-status-style}]#[push-default]#{T:window-pane-status-format}#[pop-default]#[norange list=on default]  ,#[range=pane|#{pane_id} list=focus #{?#{!=:#{E:pane-status-current-style},default},#{E:pane-status-current-style},#{E:pane-status-style}}]#[push-default]#{T:window-pane-current-status-format}#[pop-default]#[norange list=on default] }
[2] #[align=left]#{R: ,#{n:#{session_name}}}S: #[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{S:#[range=session|#{session_id} #{E:session-status-style}]#[push-default]#{session_name}#{session_alert}#[pop-default]#[norange list=on default]  ,#[range=session|#{session_id} list=focus #{?#{!=:#{E:session-status-current-style},default},#{E:session-status-current-style},#{E:session-status-style}}]#[push-default]#{session_name}*#{session_alert}#[pop-default]#[norange list=on default] }
```

## status-interval

Seconds between status line refreshes and runs of its #() commands; 0 turns the timer off.

- **TOML key**: `status.interval` (integer)
- **Scope**: session
- **Values**: a whole number, at most 2147483647

Default: `15`

## status-justify

Where the window list sits in the status line.

- **TOML key**: `status.justify` (string)
- **Scope**: session
- **Values**: `left`, `centre`, `right`, `absolute-centre`

Default: `left`

## status-keys

Key table the command prompt uses: emacs or vi.

- **TOML key**: `status.keys` (string)
- **Scope**: session
- **Values**: `emacs`, `vi`

Default: `emacs`

Default under tmux identity: `emacs`, or `vi` when `$VISUAL` or `$EDITOR` names a vi

## status-left

Format for the left part of the status line.

- **TOML key**: `status.left` (string)
- **Scope**: session
- **Values**: text

Default: `[#{session_name}] `

## status-left-length

The widest the left part of the status line may be, in cells.

- **TOML key**: `status.left-length` (integer)
- **Scope**: session
- **Values**: a whole number, at most 32767

Default: `10`

## status-left-style

Style of the left part of the status line.

- **TOML key**: `status.left-style` (string)
- **Scope**: session
- **Values**: text

Default: `default`

## status-position

Whether the status line is at the top or bottom of the window.

- **TOML key**: `status.position` (string)
- **Scope**: session
- **Values**: `top`, `bottom`

Default: `bottom`

## status-right

Format for the right part of the status line; grofe's default ends with the agents badge.

- **TOML key**: `status.right` (string)
- **Scope**: session
- **Values**: text

Default:

```text
"#{pane_title}" %H:%M %d-%b-%y#[range=user|agents]#{agents_badge}#[norange]
```

Default under tmux identity: `"#{pane_title}" %H:%M %d-%b-%y`

## status-right-length

The widest the right part of the status line may be, in cells.

- **TOML key**: `status.right-length` (integer)
- **Scope**: session
- **Values**: a whole number, at most 32767

Default: `40`

## status-right-style

Style of the right part of the status line.

- **TOML key**: `status.right-style` (string)
- **Scope**: session
- **Values**: text

Default: `default`

## status-style

Style of the status line.

- **TOML key**: `status.style` (string)
- **Scope**: session
- **Values**: text

Default: `bg=green,fg=black`

## synchronize-panes

Send input typed into one pane to every pane in the window.

- **TOML key**: `synchronize-panes` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `off`

## terminal-features

Features of outer terminals by TERM pattern; grofe asks the terminal what it supports instead.

- **TOML key**: `terminal-features` (array of strings)
- **Scope**: global
- **Values**: a list of text, element N at index N

Default:

```text
[0] xterm*:clipboard:ccolour:cstyle:focus:title
[1] screen*:title
[2] rxvt*:ignorefkeys
```

Accepted for compatibility; it has no effect in grofe.

## terminal-overrides

terminfo overrides for outer terminals by TERM pattern; grofe asks the terminal what it supports instead.

- **TOML key**: `terminal-overrides` (array of strings)
- **Scope**: global
- **Values**: a list of text, element N at index N

Default: `[0] linux*:AX@`

Accepted for compatibility; it has no effect in grofe.

## tiled-layout-max-columns

The most columns the tiled layout uses; 0 has no limit.

- **TOML key**: `tiled-layout-max-columns` (integer)
- **Scope**: window
- **Values**: a whole number, at most 65535

Default: `0`

## tree-mode-preview-format

Format of the labels in choose-tree's preview.

- **TOML key**: `tree-mode-preview-format` (string)
- **Scope**: window
- **Values**: text

Default:

```text
#{?pane_format,#{pane_index}:#{pane_title},#{window_index}:#{window_name}}
```

## tree-mode-preview-style

Style of the labels in choose-tree's preview.

- **TOML key**: `tree-mode-preview-style` (string)
- **Scope**: window
- **Values**: text

Default:

```text
fg=#{?#{||:#{&&:#{pane_format},#{pane_active}},#{&&:#{window_format},#{window_active}}},#{display-panes-active-colour},#{display-panes-colour}}
```

## update-environment

Environment variables copied from the client into a session when it is created or a client attaches.

- **TOML key**: `update-environment` (array of strings)
- **Scope**: session
- **Values**: a list of text, element N at index N

Default:

```text
[0] DISPLAY
[1] KRB5CCNAME
[2] MSYSTEM
[3] SSH_ASKPASS
[4] SSH_AUTH_SOCK
[5] SSH_AGENT_PID
[6] SSH_CONNECTION
[7] WAYLAND_DISPLAY
[8] WINDOWID
[9] XAUTHORITY
[10] XDG_CURRENT_DESKTOP
[11] XDG_SESSION_DESKTOP
[12] XDG_SESSION_TYPE
```

## user-keys

Escape sequences that become the keys User0, User1 and so on.

- **TOML key**: `user-keys` (array of strings)
- **Scope**: global
- **Values**: a list of text, element N at index N

Default: empty

## variation-selector-always-wide

Treat a character followed by variation selector 16 as two cells wide.

- **TOML key**: `variation-selector-always-wide` (boolean)
- **Scope**: global
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Accepted for compatibility; it has no effect in grofe.

## visual-activity

Show a message for an activity alert (on), ring the bell (off), or both.

- **TOML key**: `visual-activity` (string)
- **Scope**: session
- **Values**: `off`, `on`, `both`

Default: `off`

## visual-bell

Show a message for a bell (on), pass the bell to the terminal (off), or both.

- **TOML key**: `visual-bell` (string)
- **Scope**: session
- **Values**: `off`, `on`, `both`

Default: `off`

## visual-silence

Show a message for a silence alert (on), ring the bell (off), or both.

- **TOML key**: `visual-silence` (string)
- **Scope**: session
- **Values**: `off`, `on`, `both`

Default: `off`

## window-active-style

Default style of the active pane's content.

- **TOML key**: `window-active-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## window-pane-current-status-format

Format of the current pane's entry in a status-format pane list.

- **TOML key**: `window-pane-current-status-format` (string)
- **Scope**: window
- **Values**: text

Default: `#P:[#T]#{?pane_flags,#{pane_flags}, }`

## window-pane-status-format

Format of a pane's entry in a status-format pane list.

- **TOML key**: `window-pane-status-format` (string)
- **Scope**: window
- **Values**: text

Default: `#P:[#T]#{?pane_flags,#{pane_flags}, }`

## window-size

How a window's size follows the clients attached to it: the largest, the smallest, the latest used, or manual.

- **TOML key**: `window-size` (string)
- **Scope**: window
- **Values**: `largest`, `smallest`, `manual`, `latest`

Default: `latest`

## window-status-activity-style

Style of a window's entry in the window list when it has an activity or silence alert.

- **TOML key**: `window-status-activity-style` (string)
- **Scope**: window
- **Values**: text

Default: `reverse`

## window-status-bell-style

Style of a window's entry in the window list when it has a bell alert.

- **TOML key**: `status.window.bell-style` (string)
- **Scope**: window
- **Values**: text

Default: `reverse`

## window-status-current-format

Format of the current window's entry in the window list; grofe's default adds the window's agent glyph.

- **TOML key**: `status.window.current-format` (string)
- **Scope**: window
- **Values**: text

Default:

```text
#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}
```

Default under tmux identity:

```text
#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }
```

## window-status-current-style

Style of the current window's entry in the window list.

- **TOML key**: `status.window.current-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## window-status-format

Format of a window's entry in the status line's window list; grofe's default adds the window's agent glyph.

- **TOML key**: `status.window.format` (string)
- **Scope**: window
- **Values**: text

Default:

```text
#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}
```

Default under tmux identity:

```text
#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }
```

## window-status-last-style

Style of the previously selected window's entry in the window list.

- **TOML key**: `status.window.last-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## window-status-separator

Text drawn between windows in the window list.

- **TOML key**: `status.window.separator` (string)
- **Scope**: window
- **Values**: text

Default: ` `

## window-status-style

Style of a window's entry in the window list.

- **TOML key**: `status.window.style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## window-style

Default style of inactive panes' content.

- **TOML key**: `window-style` (string)
- **Scope**: window
- **Values**: text

Default: `default`

## word-separators

Characters that end a word for copy mode's word motions and double-click selection.

- **TOML key**: `word-separators` (string)
- **Scope**: session
- **Values**: text

Default: `` !"#$%&'()*+,-./:;<=>?@[\]^`{|}~ ``

## wrap-search

Whether a copy-mode search wraps around at the top or bottom of the history.

- **TOML key**: `wrap-search` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

## xterm-keys

Send function keys with modifiers in xterm's encoding.

- **TOML key**: `xterm-keys` (boolean)
- **Scope**: window
- **Values**: `on` or `off` (`true` or `false` in TOML)

Default: `on`

Accepted for compatibility; it has no effect in grofe.
