# Framed panes

Each tiled pane is a rounded tile with its own frame, its title drawn in the
frame's top edge, and a cell of gap between tiles and around the outside:

```text
 ╭─ ( 1 ) nvim ──── ( z f x ) ─╮ ╭─ ( 2 ) zsh · ui ─ ( z f x ) ─╮
 │                             │ │                              │
 │                             │ ╰──────────────────────────────╯
 │                             │
 │                             │ ╭─ ( 3 ) cargo ──── ( z f x ) ─╮
 │                             │ │                              │
 ╰─────────────────────────────╯ ╰──────────────────────────────╯
```

The active pane's frame takes `pane-active-border-style` and the others
`pane-border-style`. The title shows the pane's number and the title its
program set; a pane with no title of its own shows its command and folder,
such as `zsh · ui`.

With `mouse` on, the end of the title holds three controls: `z` zooms the
pane (`u` while zoomed), `f` floats it (`t` puts a floating pane back) and
`x` closes it, after asking. They are the mouse keys `MouseDown1Control8`,
`Control7` and `Control9`, so they can be rebound.

A window too small to frame every pane falls back to tmux's shared borders.
`#{pane_width}`, the pty size and `#{window_layout}` describe the content
inside the frames.

## Settings

```toml
pane-frames = true          # false: tmux's shared one-cell borders
pane-gap = 1                # cells between frames and around them, 0 to 8
pane-frame-title = true     # false: a plain top edge
pane-frame-tab-ends = "()"  # the end glyphs around the number and controls

[pane-border]
lines = "rounded"           # single, double, heavy, simple, number, spaces
```

`()` works in every font. With a Nerd font, `"\ue0b7\ue0b5"` gives thin
rounded ends, and `""` gives none.

## Moving panes

With `mouse` on, panes move as windows do on a desktop. Drag a tile by its
title (or Ctrl-drag anywhere in it) and it lifts out of the tiling and
follows the pointer, its content still live, while a preview frame shows
where it will land:

- drop it on another pane's middle and the two swap;
- drop it on the thin band along a side and it docks there, splitting that
  pane;
- drop it on a gap, the status line or off the window and it snaps to the
  nearest pane, so a tiled pane never ends up floating by accident;
- right-click or press Escape before you let go to put it back.

The program inside learns its new size once, when you let go. Dragging a gap
or any other frame edge resizes, as a border drag does in tmux.

## Floating panes

`prefix f` (or `F`) floats the active pane above the tiles at the size it
had, shifted a little down and to the right; pressed on a floating pane it
puts the pane back. The same actions are `float-pane` and `unfloat-pane`,
Float and Unfloat in the right-click menu, and Ctrl-dragging a floating pane
onto a tile docks it.
