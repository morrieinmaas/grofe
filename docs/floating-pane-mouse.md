# Floating panes with the mouse

tmux 3.7b has floating panes (`new-pane`) but no mouse key on their border: a click or
drag there reaches nothing. grofe adds, by default, a few things on top. None of them
changes what tmux 3.7b does; each only acts where tmux does nothing.

## Dragging (needs `mouse on`)

Press the left button on a floating pane's border and drag:

| Where | What |
|---|---|
| top edge, left edge | move the pane |
| right edge, bottom edge | resize (width, height) |
| any corner | resize from that corner |

The pane is focused and raised to the top on the press. The whole pane, border
included, stays inside the window, and may shrink to 1x1. The pointer's movement is
measured from the press, so a pane held back by the window edge follows again when
the pointer returns. The release ends the drag.

`floating-pane-mouse` (window option, `on` or `off`, default `on`) turns this off; off,
a floating pane's border belongs to no pane exactly as in tmux.

## Which binding wins

A press or drag on a floating border is the key `MouseDown1Border` or
`MouseDrag1Border`, as on a tiled border. tmux never fires a key there, so any root
binding of those keys is one tmux would not have run at that spot. grofe runs it
anyway, and then does not drag: a binding of your own wins. The exception is tmux's
stock rows, `resize-pane -M` (which the tmux front end turns into no command) and
`select-pane -M`, which a config copied from tmux's defaults carries and which mean
nothing on a floating pane; those do not shadow the drag.

## Commands

tmux 3.7b has neither verb, so both are free of tmux names, and both are unknown
commands under the `tmux` name. Flags are `new-pane`'s: `-x`/`-y` the size, `-X`/`-Y`
the window cell of the top-left content cell (cells or `N%`), `-t` the pane.

- `float-pane [-t pane] [-x w] [-y h] [-X x] [-Y y]`: a tiled pane becomes floating,
  on top, keeping its process and history. Defaults are `new-pane`'s. A window's last
  tiled pane cannot float. To put it back, use `join-pane` or `select-layout` as
  before.
- `place-pane [-t pane] [-X x] [-Y y]`: moves a floating pane, held inside the window
  as a drag holds it. An axis not given stays.

`#{pane_floating_flag}` (tmux's own format) tells a floating pane.
