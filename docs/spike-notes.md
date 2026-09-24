# M0 spike notes (T0.3)

Single pane, no server: `examples/spike.rs` spawns a program on a `nix` pty, feeds
its output into `alacritty_terminal`, and redraws the whole grid to the outer
terminal (crossterm raw mode + alt screen). Acceptance is `tests/spike.rs`, which
runs the spike inside an expectrl pty and rebuilds the outer screen with a second
`Term`.

Run it: `cargo run --example spike` (your `$SHELL`) or
`cargo run --example spike -- nvim`. Quit with `Ctrl-]`. Set
`GROFE_SPIKE_LOG=/tmp/spike.log` to see title, bell and clipboard events.

## What works (agent-verified on macOS)

- Spawning per spec §4.8: `openpty`, `FD_CLOEXEC` on master and slave, `pre_exec`
  with only `setsid`, `ioctl(TIOCSCTTY)`, `dup2` to 0/1/2. The child is a
  session leader with the pty as controlling tty.
- `/bin/sh` echo round trip (`echo_roundtrip`).
- `PtyWrite` replies: a shell that sends CPR (`CSI 6n`) and blocks in `read` gets
  its answer (`cpr_reply`). Negative control done: with the `PtyWrite` send
  removed the test times out, so it really proves the reply path.
- nvim (`-u NONE -i NONE`): alt screen, insert, Esc, `:q!`, spike exits on child
  exit (`nvim_edit`).
- Resize path compiles and is wired (SIGWINCH flag, `term.resize`,
  `TIOCSWINSZ`), but no agent test drives it. See the owner checklist.

## Gaps

| Gap | Tag | Note |
|---|---|---|
| Named colours rendered as outer SGR 30-37/90-97/39/49, not truecolor | ours | `Term::colors()` is all `None` unless the app sets OSC 4/10/11, so there is no RGB to send. Passing named/indexed through keeps the user's outer theme; only `Color::Spec` goes out as `38;2`. The real renderer downgrades per client caps (§4.4). |
| `ColorRequest` (OSC 10/11/4 queries) answered only for fg/bg/cursor from a fixed palette | ours | Needs the configured palette. |
| `ClipboardLoad` (OSC 52 paste) ignored; `ClipboardStore`, `Title`, `ResetTitle`, `Bell` only logged | ours | Become `PaneEvent`s in M1. Default `Config::osc52` is `OnlyCopy`, so loads rarely arrive anyway. |
| Underline styles collapse to plain `4`; underline colour (`cell.underline_color()`) and OSC 8 hyperlinks (`cell.hyperlink()`) not emitted | ours | Data is in the cell, the renderer just ignores it. |
| No mouse, no focus events | ours | Spike never enables outer mouse/focus reporting; raw stdin bytes are forwarded with no decoding. |
| Kitty keyboard protocol off | ours | `Config::kitty_keyboard` defaults to false. |
| No synchronized output on the outer side, full redraw every frame | ours | Out of scope; M1 compositor diffs. Flicker possible on large redraws. |
| Blocking reader/writer/stdin threads, not tokio `AsyncFd` | ours | Spike choice. M1 uses `AsyncFd` per §8. |
| Each `Processor::new()` pre-allocates a 2 MiB sync-update buffer | lib | `vte` 0.15 `SYNC_BUFFER_SIZE = 0x20_0000`, `Vec::with_capacity`. That is 2 MiB per pane before any output; fine for tens of panes, worth knowing. |
| XTVERSION (`CSI > q`) gets no reply | lib | vte 0.15 has no dispatch for it. Apps that probe it (nvim) just time out; answer it ourselves if one misbehaves. |
| `Term` never emits `Wakeup` or `ChildExit` | lib | Those come from alacritty's own `event_loop`/`tty`, which we do not use. Our reader signals redraws; the child handle signals exit. |
| Two drainers (reader, sync-timeout on main) share one event buffer | ours | Order is kept by enqueueing to the writer while holding the event lock. With one reader task per pane in M1 this goes away. |

## API notes for implementers (alacritty_terminal 0.26.0, vte 0.15.0)

Crate layout: `alacritty_terminal::{Term, Grid, vte}`; modules `event`, `grid`,
`index`, `term`, `term::cell`, `term::color`. `vte` is re-exported, so use
`alacritty_terminal::vte::ansi::{Processor, Color, NamedColor, Rgb, CursorShape, CursorStyle}`
and do not add a separate `vte` dependency.

**Construction.** `Term::new(config: term::Config, dims: &impl Dimensions, listener: T) -> Term<T>`.
`Config::default()` gives 10 000 lines of scrollback, block cursor, no kitty
keyboard, OSC 52 copy-only. `Dimensions` (in `grid`) needs `total_lines`,
`screen_lines`, `columns`; implement it on a small size struct.
`term::test::TermSize::new(cols, lines)` is public (not `cfg(test)`) and handy in
tests.

**Parsing.** `let mut p: Processor = Processor::new();` (default type param
`StdSyncHandler`). Feed with `p.advance(&mut term, &bytes)`; `Term<T: EventListener>`
implements `vte::ansi::Handler`. Keep `Processor` next to `Term` under the same lock.
Gotcha: during a synchronized update (DEC mode 2026) `advance` only buffers. You
must poll `p.sync_timeout().sync_timeout() -> Option<Instant>` and call
`p.stop_sync(&mut term)` when it has passed (timeout is 150 ms), or a pane that
begins an update and dies freezes.

**Listener.** `trait EventListener { fn send_event(&self, event: Event) {} }`, called
synchronously from inside `advance` while you hold the Term lock, with `&self`.
Buffer events (the spike uses `Arc<Mutex<Vec<Event>>>`) and handle them after
unlocking. `Event` is `Clone + Debug`; variants: `MouseCursorDirty`, `Title(String)`,
`ResetTitle`, `ClipboardStore(ClipboardType, String)`,
`ClipboardLoad(ClipboardType, Arc<dyn Fn(&str) -> String>)`,
`ColorRequest(usize, Arc<dyn Fn(Rgb) -> String>)` (index into `term::color::Colors`:
0..256 palette, 256 fg, 257 bg, 258 cursor), `PtyWrite(String)`,
`TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String>)`, `CursorBlinkingChange`,
`Wakeup`, `Bell`, `Exit`, `ChildExit(ExitStatus)`. The `Arc<dyn Fn>` payloads are
formatters: call them with the answer and write the returned string to the pty.
Replies that arrive as plain `PtyWrite`: DA1/DA2 (`identify_terminal`), DSR/CPR
(`device_status`), DECRQM (`report_mode`, `report_private_mode`), kitty keyboard
query (`report_keyboard_mode`), `CSI 18 t` (`text_area_size_chars`). XTVERSION
(`CSI > q`) is not answered by `Term`. `WindowSize { num_lines, num_cols, cell_width, cell_height }` (all `u16`).

**Reading the screen.** `term.renderable_content()` returns `RenderableContent {
display_iter, selection, cursor, display_offset, colors, mode }`. `display_iter`
yields `grid::Indexed<&Cell> { point, cell }` (derefs to the cell) over the visible
rows. `point.line` is a `Line(i32)` relative to the live screen (negative when
scrolled back), so the screen row is `point.line.0 + display_offset as i32`;
`point.column` is `Column(usize)`. Random access: `term.grid()[Line(i)][Column(j)]`.
`Cell { c: char, fg: Color, bg: Color, flags: Flags, extra }` plus
`cell.zerowidth() -> Option<&[char]>`, `underline_color()`, `hyperlink()`.
`Color` is `Named(NamedColor) | Spec(Rgb) | Indexed(u8)`; `NamedColor as usize`
gives 0..16 for ANSI, 256 `Foreground`, 257 `Background`, 258 `Cursor`, 259..=266
`DimBlack..DimWhite`, then `BrightForeground`, `DimForeground`.

**Wide characters.** A wide char is a cell with `Flags::WIDE_CHAR` followed by a
cell with `Flags::WIDE_CHAR_SPACER` (skip it when rendering). When a wide char
does not fit at the end of a line, the last cell gets
`Flags::LEADING_WIDE_CHAR_SPACER` (render it as a blank) and the char wraps.
Style flags: `BOLD`, `DIM`, `ITALIC`, `UNDERLINE`, `DOUBLE_UNDERLINE`, `UNDERCURL`,
`DOTTED_UNDERLINE`, `DASHED_UNDERLINE` (`ALL_UNDERLINES`), `INVERSE`, `HIDDEN`,
`STRIKEOUT`; layout flag `WRAPLINE`.

**Cursor.** `content.cursor: RenderableCursor { shape: CursorShape, point }`. The
point is already moved off a wide-char spacer. `shape` is `CursorShape::Hidden`
when `SHOW_CURSOR` is off, else `Block | Underline | Beam | HollowBlock`.
Blinking lives on `term.cursor_style() -> CursorStyle { shape, blinking }`.
DECSCUSR out: block 2, underline 4, beam 6, minus one when blinking.

**Modes.** `*term.mode()` (or `content.mode`) is a `TermMode` bitflag:
`SHOW_CURSOR`, `APP_CURSOR`, `APP_KEYPAD`, `MOUSE_REPORT_CLICK`, `MOUSE_DRAG`,
`MOUSE_MOTION` (`MOUSE_MODE` = any), `SGR_MOUSE`, `UTF8_MOUSE`, `BRACKETED_PASTE`,
`FOCUS_IN_OUT`, `ALT_SCREEN`, `ALTERNATE_SCROLL`, `LINE_WRAP`, `INSERT`, `ORIGIN`,
kitty flags (`KITTY_KEYBOARD_PROTOCOL` = any). Default is
`SHOW_CURSOR | LINE_WRAP | ALTERNATE_SCROLL | URGENCY_HINTS`.

**Resize.** `term.resize(size)` takes any `Dimensions` by value; a no-op if
unchanged. It resizes both screens with `grid.resize(!is_alt, ...)` and
`inactive_grid.resize(is_alt, ...)` (the bool is `reflow`). `swap_alt` swaps the
two grids, so while the alt screen is active the primary sits in `inactive_grid`.
Net effect: the primary screen is always reflowed (also while an alt-screen app
runs), the alt screen is never reflowed (the app redraws it after SIGWINCH).
It also clears the selection on column change and resets the scroll region. Separately `ioctl(TIOCSWINSZ)` on the master
(`nix::ioctl_write_ptr_bad!(set_winsize, libc::TIOCSWINSZ, nix::pty::Winsize)`);
the kernel then sends SIGWINCH to the child's foreground process group.

**Damage (for the M1 diff).** `term.damage()` returns
`TermDamage::Full | TermDamage::Partial(iter of LineDamageBounds { line, left, right })`;
call `term.reset_damage()` after. Not used by the spike.

**nix 0.31.** `openpty(&Winsize, None) -> OpenptyResult { master, slave }` as
`OwnedFd`s without `CLOEXEC`; set it yourself with
`fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))`. Use `nix::libc` for the raw
calls in `pre_exec` (no extra dependency). Install SIGWINCH with `SA_RESTART` so
blocking reads elsewhere are not interrupted. On macOS the master read fails
with `EIO` once every slave holder has exited.

**expectrl 0.9 (tests).** `Session` is generic; name it as
`expectrl::session::OsSession`. `ptyprocess` spawns at 80x24 with echo off, and
resizing afterwards races the child's startup, so the tests start the spike via
`/bin/sh -c 'stty cols 100 rows 30 && exec "$0" -- "$@"'`. `try_read` flips
`O_NONBLOCK` on and off per call; it returns `EIO` after the child exits.
`session.get_process().is_alive()` checks exit; dropping a `Session` force-kills a
live child. The tests build the example with `$CARGO build --example spike` once
and locate it next to the test binary (`target/<profile>/examples/spike`).

## Owner live run (pending)

Run `cargo run --example spike` in Ghostty and tick each item, noting anything odd.

- [ ] Ghostty true colour: `printf '\e[38;2;255;100;0mTRUECOLOR\e[0m\n'` is orange, and a 24-bit gradient script looks smooth
- [ ] Cursor shape in nvim: beam in insert mode, block in normal mode, restored on quit
- [ ] Resize reflow: resize the Ghostty window with a long wrapped line on screen (shell) and inside nvim; no garbage, prompt stays in place
- [ ] less: `less /etc/services`, scroll, `/search`, `q` restores the screen
- [ ] fzf: `ls | fzf`, arrow keys, typing, Enter
- [ ] htop: colours, live updates, F10 quits
- [ ] Ctrl-] quits and leaves the outer terminal usable (echo on, cursor visible, normal screen)

## TERM decision (T0.4)

D7 asked whether the inner `TERM` should move from `screen-256color` to
`tmux-256color`, conditional on every extra capability `tmux-256color`'s
terminfo entry advertises actually working end to end through the M0
pipeline. It does not, so the default stays `screen-256color` plus
`COLORTERM=truecolor`.

**Capability comparison.** `infocmp -1x -d screen-256color tmux-256color` on
this machine (Homebrew ncurses 6.6; `-x` is required, the extended
capabilities like `Smulx` and `smxx` are invisible without it). The extra
string capabilities that affect what an app running in the pane can emit,
grouped by what they do:

| Capability | Escape | Category | alacritty_terminal 0.26 | Spike's outer render |
|---|---|---|---|---|
| `sitm`/`ritm` | `\E[3m` / `\E[23m` (italic) | SGR attribute | `Attr::Italic` → `Flags::ITALIC` (`term/mod.rs`) | Emitted (`STYLE` mask, `sgr()` code `3`) |
| `smxx`/`rmxx` | `\E[9m` / `\E[29m` (strikeout) | SGR attribute | `Attr::Strike` → `Flags::STRIKEOUT` | Emitted (code `9`) |
| `invis` | `\E[8m` (hidden) | SGR attribute | `Attr::Hidden` → `Flags::HIDDEN` | Emitted (code `8`) |
| `smso`/`rmso` | `\E[7m`/`\E[27m` (reverse, tmux's standout) | SGR attribute | `Attr::Reverse` → `Flags::INVERSE` | Emitted (code `7`); already reachable under `screen-256color` too, since its own `sgr` cap uses the same reverse parameter |
| `Smulx` | `\E[4:%p1%dm` (extended underline: double/curly/dotted/dashed) | SGR attribute | Parsed correctly: `vte`'s `attrs_from_sgr_parameters` maps `[4,2]`/`[4,3]`/`[4,4]`/`[4,5]` to `DoubleUnderline`/`Undercurl`/`DottedUnderline`/`DashedUnderline` (`ansi.rs`) | **Not emitted distinctly.** `examples/spike.rs`'s `STYLE` mask includes `Flags::ALL_UNDERLINES` as one group, and `sgr()` writes a bare code `4` for any of them; the style byte is dropped |
| `dsl`/`fsl`/`tsl` | OSC 0 (window title via the status-line mechanism) | Non-SGR (title) | `Event::Title`/`Event::ResetTitle` fire | **Not forwarded.** `drain_events()` only logs `Event::Title`/`Event::ResetTitle` |
| `Ms` | OSC 52 (clipboard) | Non-SGR (clipboard) | `Event::ClipboardStore`/`Event::ClipboardLoad` fire | **Not forwarded.** `ClipboardStore` only logged; `ClipboardLoad` ignored entirely |
| `Cs`/`Cr` | OSC 12 / OSC 112 (cursor colour set/reset) | Non-SGR (cursor colour) | Internal colour state updates | **Not emitted.** `render()` writes DECSCUSR (shape) and visibility but no cursor-colour escape |
| `fd`/`fe` | `\E[?1004h`/`l` (focus reporting) | Non-SGR (focus events) | `TermMode::FOCUS_IN_OUT` tracked | **Not forwarded.** Spike never enables outer focus reporting or synthesizes `FocusIn`/`FocusOut` (already listed as a Gap above) |
| `Se`/`Ss` | `\E[2 q` / `\E[%p1%d q` (DECSCUSR, cursor style) | Non-SGR (cursor shape) | `cursor_style()` tracked | Emitted unconditionally every frame (not gated on TERM either way); still on the pending owner live-run checklist, unverified in a real terminal |
| `hs`, `XF`, `E3`, `RV`/`XR`/`rv`/`xr`, `kf13`-`kf63`, `kDC`/`kEND`/`kHOM`/`kIC`/`kLFT`/`kNXT`/`kPRV`/`kRIT` (+numbered variants), `kUP`/`kDN`, `kxIN`/`kxOUT` | various | Not per-pane rendering (`hs`/`XF`: outer status line; `E3`: erase scrollback, invisible to a single full-redraw frame; `RV`/`XR`/`rv`/`xr`: version reporting, unhandled by `vte` either way; the `k*` set: modified-key *input* encoding, decided by the client's key encoder in M1, not by the inner emulator) | n/a | n/a |

**Evidence.** `tests/term.rs::smulx_underline_style_does_not_survive_the_spike`
drives the spike through `expectrl`, writes `\e[3mBB\e[23m \e[9mCC\e[29m
\e[4:3mDD\e[24m` (italic, strikeout, curly underline) into it, and
reconstructs the outer screen with a second `Term`. Italic and strikeout
land as `Flags::ITALIC` / `Flags::STRIKEOUT` on the reconstructed cells,
confirming those two extras genuinely work end to end. The curly-underline
cell lands as plain `Flags::UNDERLINE`, `Flags::UNDERCURL` absent, confirming
the `Smulx` distinction is lost by the time it reaches the outer terminal.
The non-SGR extras (title, clipboard, cursor colour, focus) are not
exercised live; the source citations above (`drain_events()` in
`examples/spike.rs`) are the evidence, matching the Gaps table earlier in
this document, which already records them as deferred to M1.

**Decision.** Since not every capability `tmux-256color` adds over
`screen-256color` survives the M0 spike's render path, D7's bar for
switching is not met. `src/term.rs::resolve_term()` keeps `screen-256color`
as the default (falling back to `xterm-256color` if even that entry is
missing on the host), with `COLORTERM=truecolor` unconditionally. Revisit
once M1's compositor forwards title, clipboard, cursor colour and focus
events, and once the renderer stops collapsing underline styles.
