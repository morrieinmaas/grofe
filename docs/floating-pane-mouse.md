# Floating panes with the mouse

tmux 3.7b's floating panes (`new-pane`) take the mouse on their border through
tmux's stock rows, and grofe does what tmux does there (recorded on tmux 3.7b:
`tests/fixtures/compat/golden/tc_6/float-border-mouse`). grofe adds a window
option and two verbs of its own on top; none of them changes what tmux 3.7b does.

## The border (needs `mouse on`)

A press or drag on a floating pane's border is the key `MouseDown1Border` or
`MouseDrag1Border`, as on a tiled border, aimed at the floating pane.

- The press is tmux's `select-pane -M`: the marked pane is cleared, and the floating
  pane is focused and raised.
- A left drag is tmux's `resize-pane -M`: the pane is focused and raised, then each
  report moves the part of the border the previous report was on to the pointer.

| Where | What |
|---|---|
| top edge | move the pane |
| left, right or bottom edge | resize from that side |
| any corner (and the cell next to it on the top or bottom edge) | resize from that corner |

The pane may leave the window, and is never smaller than 1x1; a report that would
make it smaller changes nothing, and the drag then waits until the pointer is back
on the border. A pointer on the status line drags along the window's last row.
The program inside learns its new size at most every 250 ms during a drag, as in
tmux, and always its final size.

The release fires `MouseDragEnd1Border`, or `MouseDragEnd1Pane` when it lands inside
a pane, as in tmux. A plain click fires `MouseUp1Border` too.

A pane that asked for the mouse gets the clicks inside it, in its own cells, and
nothing for a click on its border.

## Which binding wins

As in tmux, a root binding of `MouseDown1Border` or `MouseDrag1Border` of your own
runs instead, with the floating pane as its target, and the pane does not move. A
binding that is tmux's stock row itself (`select-pane -M` for the press,
`resize-pane -M` for the drag, or one with no command) is the built-in behaviour
above, on tiled and floating borders alike, so a config copied from tmux's defaults
behaves as tmux does.

Known difference: unbinding `MouseDown1Border` or `MouseDrag1Border` does not stop
the built-in behaviour, where in tmux the border then does nothing. The same holds
for tiled borders.

## `floating-pane-mouse`

Window option, `on` or `off`, default `on` (grofe's own; tmux 3.7b has no such
option). Off, a floating pane's border runs bindings of your own only: no focus,
move or resize from the built-in behaviour.

## Commands

tmux 3.7b has neither verb, so both are free of tmux names, and both are unknown
commands under the `tmux` name. Flags are `new-pane`'s: `-x`/`-y` the size, `-X`/`-Y`
the window cell of the top-left content cell (cells or `N%`), `-t` the pane.

- `float-pane [-t pane] [-x w] [-y h] [-X x] [-Y y]`: a tiled pane becomes floating,
  on top, keeping its process and history; the other tiled panes take its place, and
  a zoomed window is unzoomed. Defaults are `new-pane`'s. A window's last tiled pane
  cannot float. To put it back, use `join-pane` or `select-layout` as before.
- `place-pane [-t pane] [-X x] [-Y y]`: moves a floating pane, held so that the whole
  pane, border included, stays inside the window (a pane larger than the window goes
  to its top left). An axis not given stays.

`#{pane_floating_flag}` (tmux's own format) tells a floating pane.
