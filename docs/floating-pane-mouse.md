# Floating panes with the mouse

tmux 3.7b's floating panes (`new-pane`) take the mouse on their border through
tmux's stock rows, and grofe does what tmux does there (recorded on tmux 3.7b:
`tests/fixtures/compat/golden/tc_6/float-border-mouse`). grofe adds three verbs of its own on top; none changes what tmux 3.7b does.

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

## Framed tiles (needs `mouse on`)

With `pane-frames on` (spec `docs/specs/2026-11-05-frames.md`) every cell of a
tile's frame, and every gap cell, is a border too: a press or drag there is
`MouseDown1Border` or `MouseDrag1Border`, aimed at the pane the cell belongs to.
A gap cell belongs to the nearest frame, a tie going to the pane on the left or
above, as tmux gives a border to the pane on its left or above.

The built-in `resize-pane -M` drag there:

| Where the drag starts | What it does |
|---|---|
| a tile's top edge (its title), corners aside | moves the pane (below) |
| any other frame edge, or a gap cell between tiles | resizes, as a tmux border drag: the pane on the left of (or above) the separator grows by the cells moved, the other shrinks |
| a floating pane's border | as above |

Under grofe identity `C-MouseDrag1Pane` (Ctrl and drag anywhere in a pane) runs
`drag-pane -M`, which starts the same move for the pane under the pointer,
tiled or floating. Under tmux identity the key is unbound and `drag-pane` is an
unknown command, as in tmux 3.7b. Outside a mouse key `drag-pane -M` fails with
`no mouse target`, as `send-keys -M` does.

While the button is down the window's drop preview shows where the pane will
land. On release over a tiled pane `T`:

| Released over | Result |
|---|---|
| the middle of `T` | the two swap places, as `swap-pane` |
| the outer quarter of one of `T`'s sides (its frame included) | `T` is split, and the pane takes the half on that side; the space it left is filled as `kill-pane` would fill it |
| the pane itself, a gap, or no pane | nothing changes |

- An edge drop that would leave the layout's shape as it is changes nothing,
  so a moved divider stays put.
- A side of `T` too small to split along its axis is no edge: the drop there
  is the middle's swap.
- A floating pane dropped by `drag-pane -M` goes into the tiling by the same
  table, which is unfloating it with the mouse. On `T`'s middle it does not
  swap: `T` is split along its longer axis and the pane takes the right or
  bottom half, as `unfloat-pane -s T` does, so no other pane starts floating.
  With no tiled pane left it takes the whole window wherever it is dropped.
- The moved pane becomes the active pane. It is the same pane: its process,
  history, uid and resume command stay.
- A zoomed window does not start a move.
- The program inside learns its new size once, on release; the preview goes
  on release, or when the client goes.

## Which binding wins

As in tmux, the border does what the root table says and nothing else. The default
table binds tmux's stock rows, `MouseDown1Border` `select-pane -M` and
`MouseDrag1Border` `resize-pane -M` (`list-keys -T root` shows them), and they run
the built-in behaviour above, on tiled and floating borders alike.

- A root binding of one of those keys of your own runs instead, with the floating pane
  as its target, and the pane does not move.
- A binding that is tmux's stock row itself (`select-pane -M` for the press,
  `resize-pane -M` for the drag, or one with no command) is the built-in behaviour,
  so a config copied from tmux's defaults behaves as tmux does.
- `unbind -n MouseDown1Border` or `unbind -n MouseDrag1Border` removes the row: that
  press or drag then does nothing, as in tmux.
- Modifier variants (`M-MouseDrag1Border`, `C-`, `S-`) act only if you bind them.

## Commands

tmux 3.7b has none of these verbs, so they are free of tmux names, and all three are
unknown commands under the `tmux` name. Flags are `new-pane`'s: `-x`/`-y` the size,
`-X`/`-Y` the window cell of the top-left content cell (cells or `N%`), `-t` the pane.

- `float-pane [-t pane] [-x w] [-y h] [-X x] [-Y y]`: a tiled pane becomes floating,
  on top, keeping its process and history; the other tiled panes take its place, and
  a zoomed window is unzoomed. Defaults are `new-pane`'s. A window's last tiled pane
  cannot float. `unfloat-pane` puts it back.
- `unfloat-pane [-t pane] [-s target-pane] [-h|-v]`: a floating pane goes back into
  the tiling, as the same pane: its process, history, uid and resume command stay, and
  the snapshot saves it again. It splits `-s target-pane` if given (a tiled pane of
  the same window), else the tiled pane under its centre, else the window's last
  active tiled pane; a window with no tiled pane left takes it as
  its only one. The split is side by side with `-h`, one above the other with `-v`
  (`-h` wins, as in `join-pane`), else along the longer side of the pane split. The
  pane goes after the one it splits and becomes the active pane; a zoomed window is
  unzoomed.
- `place-pane [-t pane] [-X x] [-Y y]`: moves a floating pane, held so that the whole
  pane, border included, stays inside the window (a pane larger than the window goes
  to its top left). An axis not given stays.

`#{pane_floating_flag}` (tmux's own format) tells a floating pane.

## Keys and the pane menu

Under grofe identity:

- `prefix F` floats the active pane, or unfloats it if it is floating:
  `if-shell -F '#{pane_floating_flag}' unfloat-pane float-pane`.
- A right click on a pane (`MouseDown3Pane`) opens the pane's menu at the pointer,
  acting on the pane clicked: Float on a tiled pane, Unfloat on a floating one (key
  `f`), Horizontal and Vertical Split on a tiled pane, Kill, and Zoom. A pane that
  asked for the mouse gets the click instead, as with tmux's stock menu.

Under tmux identity neither is bound: `F` is unbound in the prefix table and
`MouseDown3Pane` is tmux 3.7b's stock menu, unchanged.
