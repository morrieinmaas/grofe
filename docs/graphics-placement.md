# Image placements: kitty direct mode and sixel (G3)

Opt-in. With `graphics` off (the default) nothing below runs and the bytes
sent to clients are what T3.8 sent: Unicode placeholders only, direct
placements and sixel dropped. `docs/graphics-notes.md` has the survey of what
tools send; this file is the design of the rest.

## Switch

Built-in option `graphics` (`off`|`on`, global, default `off`), TOML key
`graphics = true`. It is grofe's own name, not a tmux one, like
`compat-identity`: tmux configs never set it, and `set -g @x` style user
options stay free for them. `on` enables kitty direct placements, animation
and sixel together. A client still needs the capability to receive each.

## Where the state lives

The pane reader already parses the stream (scanner, then emulator). Two
things only it can know are the cursor at the byte where an image command
arrived, and every scroll. So the **placement store** lives in the pane's
`Emu` next to the `Term`, mutated by the reader and read by snapshots:

- `Store { main, alt }`, one layer per screen (like tmux's saved images),
  each with `v: u64`, kitty placements and sixel images.
- `v` is the absolute id of the viewport's first row. A placement stores
  `(abs_row, col)` with `abs_row = v + cursor_line` at the time it was made.
  A scroll up by `k` full-screen lines adds `k` to `v`; nothing else moves.
  Scrolled-off rows (`abs_row + rows <= v - history`) are dropped lazily.
- Scroll counting is done in `Written` (the parser's handler wrapper): each
  op that can scroll (`linefeed`, `newline`, `scroll_up`, `delete_lines`, a
  wrapping `input`, `clear_screen`) compares the history size before and
  after; with history full (or off) the count is computed from the op and
  the cursor instead. Alt screen has no history, so rows above `v` just go.
- Resize: kitty placements keep their cells (no reflow of images) and `v`
  moves by the history delta; with the history full and the height changed
  they are dropped, as the shift cannot be told. Sixel is dropped on any
  resize, as tmux does.
- `RIS`, ED 2 (screen rows) and ED 3 (history rows) drop what they cover;
  entering the alt screen starts an empty alt layer, leaving it drops it.

The **actor** keeps `graphics.rs` as today (id remap, transmit cache, `a=q`
answers) and gains: direct transmits (forwarded as `a=t`, never displayed by
the outer terminal on its own), frames and animation control (`a=f/a/c`,
forwarded with the remapped id and cached after the transmit), and the
extra delete forms. The reader rewrites anonymous `a=T` (no `i=`) to carry a
reserved id so both sides name the image alike.

## Commands the reader applies to the store

`a=T`/`a=p` without `U=1`: anchor at the cursor, size from `c`,`r` or the
image's pixels over the pane's cell size (PNG header, `s`,`v`, or the first
bytes of a `t=f` file), then move the cursor right by the columns and down
by rows-1 unless `C=1` (injected as `IND` x (rows-1) and `CUF`, so the
emulator scrolls like it would for text). Relative placements (`P`,`Q`,`H`,
`V`) are resolved to the parent's current cell when made and are deleted
with it; the cursor does not move. `z` is kept for stacking.

`a=d`, every `d=` key, both cases: `a`, `c`, `p`, `q`, `x`, `y`, `z` select
placements on screen (1-based `x`,`y`, as the cursor); `i` by image (and `p`),
`r` by id range; `n`, `f` are the actor's. Uppercase also frees an image
that no placement of the store still uses, by sending the actor an explicit
`d=I`; selectors the actor can answer alone (`i`, `r`, `a`) go through
unchanged.

## What a client is sent

Each frame, `compose_client` turns the visible panes' store views into
`Screen::images`: for every placement the cell rectangle on the client,
clipped to its pane rect, the client viewport and the window. Then:

1. **Hidden when covered.** The screen cells before the overlays are kept;
   after popups, menus, prompts, choosers and floating panes have drawn, an
   image whose visible rectangle contains a changed cell is left out whole.
   (Hiding whole beats cutting a hole: a kitty placement is a rectangle.)
2. **Kitty.** `Outbox` remembers what it placed. A new or changed placement
   is `CUP` + `a=p,i=<outer>,p=<serial>,c,r,x,y,w,h,X,Y,z,C=1,q=2`; a gone one
   is `a=d,d=i,i=<outer>,p=<serial>,q=2` (lowercase: data stays). The same
   `i`,`p` replaces in place, so a move does not flicker. Clipping is by
   source rectangle: cropped columns/rows `[c0,c1)x[r0,r1)` become
   `x = sx + c0*sw/cols`, `w = (c1-c0)*sw/cols` (and for rows) with `c`,`r`
   set to the visible counts. Sub-cell `X`,`Y` offsets survive only when the
   top-left is not cropped. A full redraw deletes every placed id, then
   places again (its `ESC [ 2 J` may have cleared them).
3. **Sixel.** The store keeps the parsed image (the port of tmux's
   `image-sixel.c`: pixels as palette indexes). Per client it is rescaled
   to that client's cell pixels, cropped to the visible cells, and printed
   again as tmux's `sixel_print` does. The terminal draws sixel into cells,
   so unlike kitty a sixel must be re-sent whenever any cell under it was
   rewritten in the diff, and the cells of a removed or moved sixel are
   rewritten to wipe it.
4. **No capability, nothing sent.** Kitty needs `caps.kitty_graphics`,
   sixel `caps.sixel` (DA1 attribute 4 in the attach probe, or a matching
   `terminal-features` `sixel`). A client without sixel gets tmux's text
   fallback (`SIXEL IMAGE (WxH)` over `+` rows) painted into the cells, as
   `tty_cmd_sixelimage` does when `TERM_SIXEL` is unset or the pixel size is
   unknown.

Attach, zoom, resize, window switch and redraw all go through the same
compose, so they only need the diff against what the client really has:
`Outbox.placed` survives dropped frames, and the transmit cache is re-sent
on a client's first full redraw as before.

## What tmux 3.7b does for sixel (source read, `--enable-sixel`)

- `DCS q` with no intermediate, `input_dcs_dispatch`, `sixel_parse` with the
  window's cell pixels (default 16x32); bytes past `input-buffer-size`
  (1 MiB) are discarded. Images are at most 10000x10000 pixels.
- `screen_write_sixelimage`: clipped to the pane width and height-1, the
  screen scrolls if it does not fit below the cursor, the image is stored at
  the cursor and the cursor goes to column 0, row `cy + image rows`.
- Freed by: writing text over any of its cells, line ops over its rows,
  ED 2, RIS, alt screen swap, resize; cropped (not freed) when it scrolls
  off the top; at most 20 images in the server, oldest first (grofe: per
  pane).
- Drawn only for a terminal with the `sixel` feature and known cell pixels;
  else the text fallback. tmux draws it over popups; grofe hides it.

The golden recorder's tmux is built without `--enable-sixel` (see
`containers/compat/golden.Containerfile`), so it treats `DCS q` as an
unknown DCS. Sixel goldens use a second image built with the flag.

## Limits

Per pane: 512 kitty placements, 19 sixel images, 16 MiB of decoded sixel.
Transmit cache stays 12 MiB (T3.8). Animation frames count against it.
Not done: copy-mode view of images, scroll regions that do not start at the
top moving images, image numbers (`I=`), sub-cell offsets under a crop.
