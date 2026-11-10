//! compose, diff, colour downgrade, synchronized output.
//!
//! [`compose`] builds a client's [`Screen`] from snapshotted pane rows,
//! borders and the status line; the caller then draws [`Overlay`]s over it
//! and runs [`fix_wide_pairs`]; [`diff`] turns the last screen
//! into the next as a minimal escape stream (spec §4.4); [`write_sgr`] is
//! the one SGR writer, shared with `capture-pane -e`.
//!
//! Coordinates: pane rects, borders and [`FrameInputs::cursor`] are in
//! window coordinates, the space the layout was computed in
//! ([`FrameInputs::area`]). A [`Screen`] is in client coordinates: the
//! client's terminal, `client.0` columns by `client.1` rows.

use std::collections::HashMap;

use alacritty_terminal::vte::ansi::{Color, NamedColor};
use unicode_width::UnicodeWidthChar;

use crate::protocol::Caps;
use crate::server::State;
use crate::server::borders::{self, Geometry, Indicators, PaneBox, PaneLines, PaneStatus};
use crate::server::dispatch::options;
use crate::server::graphics;
use crate::server::layout::BorderSegment;
use crate::server::model::{Model, Window};
use crate::server::pane::{CellFlags, CursorShape, CursorState, Hyperlink, RowsSnapshot, SnapCell};
use crate::server::screenimg::{self, ImageCaps, Place, ScreenImage};
use crate::server::status::{StatusRow, Style, StyledLine};
use crate::types::{ClientId, PaneId, Rect};

/// One client's composed frame: a cell buffer the size of its terminal.
/// The server keeps the last one per client and diffs the next against it.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    /// Row-major, `cols * rows` cells. A wide character is its cell plus a
    /// `WIDE_CHAR_SPACER` cell, as in the emulator.
    pub cells: Vec<SnapCell>,
    /// Where the outer cursor goes, in client coordinates; `None` hides it.
    pub cursor: Option<CursorState>,
    /// The images the terminal shows over the cells (G3).
    pub images: Vec<ScreenImage>,
}

impl Screen {
    /// A blank screen: spaces in the terminal's default colours, no cursor.
    pub fn new(cols: u16, rows: u16) -> Screen {
        let blank = SnapCell {
            c: ' ',
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: CellFlags::empty(),
            zerowidth: Vec::new(),
            underline_color: None,
            hyperlink: None,
        };
        Screen {
            cols,
            rows,
            cells: vec![blank; cols as usize * rows as usize],
            cursor: None,
            images: Vec::new(),
        }
    }
}

/// Everything [`compose`] needs for one client's frame, snapshotted by the
/// caller with every pane lock already released.
pub struct FrameInputs<'a> {
    /// The client's terminal size, `(cols, rows)`: the size of the composed
    /// [`Screen`].
    pub client: (u16, u16),
    /// The window cell `(x, y)` drawn at the client's top-left. `(0, 0)`
    /// unless the client is smaller than the window (crop), where it
    /// follows the active pane's cursor (spec D6). A window smaller than
    /// the client is drawn at the top-left and the rest letterboxed.
    pub viewport: (u16, u16),
    /// The window's area: what the layout was computed against, at
    /// `x = 0, y = 0`, sized to the window.
    pub area: Rect,
    /// Each visible pane's id, rect (window coordinates, borders excluded)
    /// and rows, top row first.
    pub panes: Vec<(PaneId, Rect, RowsSnapshot)>,
    /// The layout's border segments. The borders drawn come from the pane
    /// boxes, as tmux works them out; a segment no pane owns there is
    /// drawn from these.
    pub borders: Vec<BorderSegment>,
    /// Border cells away from the active pane (`pane-border-style`).
    pub border_style: Style,
    /// Border cells around the active pane (`pane-active-border-style`).
    pub active_border_style: Style,
    /// The pane whose borders are drawn active.
    pub active: PaneId,
    /// The marked pane when it is in this window: its border cells are
    /// drawn with reverse toggled, as tmux's `screen_redraw_draw_borders`.
    pub marked: Option<PaneId>,
    /// The status row, drawn on the client's last row; the window then has
    /// the rows above it. `None` when `status` is off.
    pub status: Option<StyledLine>,
    /// The active pane's cursor in window coordinates (its rect origin
    /// added); `None` hides the cursor.
    pub cursor: Option<CursorState>,
    /// Kitty graphics placeholder ids to rewrite (T3.8); `None` while no
    /// pane has an image.
    pub graphics: Option<&'a graphics::Remap>,
    /// What the client can show of the panes' images (G3); `None` shows
    /// none.
    pub images: Option<ImageCaps>,
    /// How the panes and their borders look beyond the plain defaults:
    /// `window-style`, the `pane-border-*` options.
    pub look: PaneLooks,
}

/// One pane's look: the colours its default-colour cells take and the
/// styles of its borders.
#[derive(Debug, Clone)]
pub struct PaneLook {
    /// What a cell in the default foreground shows (`window-style`,
    /// `window-active-style`); the terminal's default when unset.
    pub fg: Color,
    /// As `fg`, for the background.
    pub bg: Color,
    /// The pane's `pane-border-style` and `pane-active-border-style`.
    pub border: Style,
    pub active_border: Style,
    /// The pane's `pane-border-format`, expanded; drawn on its border
    /// line while `pane-border-status` is on.
    pub title: String,
    /// grofe identity's control tab ([`frame_tab`]), drawn at the right end
    /// of the frame's top edge; `None` draws none.
    pub tab: Option<String>,
    /// The pane's index (`pane-border-lines number` shows it).
    pub index: usize,
}

/// How many cells the control tab ([`frame_tab`]) covers.
pub const TAB_WIDTH: i32 = 7;

/// The control tab of a frame's title (grofe identity, spec
/// 2026-11-05-frames.md "Drawing"): zoom (`u` to unzoom), float (`t` to
/// tile a floating pane) and close, as the click ranges `control|8`,
/// `control|7` and `control|9`, between `ends`' two end glyphs. The ends
/// are drawn in the frame's style and the controls in it reversed, a pill
/// of the frame's colour on any theme; `ends` other than two one-cell
/// glyphs gives a pill with plain ends. Always [`TAB_WIDTH`] cells.
pub fn frame_tab(ends: &str, zoomed: bool, floating: bool) -> String {
    let one = |c: &char| UnicodeWidthChar::width(*c) == Some(1);
    let glyphs: Vec<char> = ends.chars().collect();
    let (left, right) = match glyphs[..] {
        [l, r] if one(&l) && one(&r) => (format!("{l}#[reverse]"), format!("#[noreverse]{r}")),
        _ => ("#[reverse] ".to_string(), " #[noreverse]".to_string()),
    };
    let zoom = if zoomed { 'u' } else { 'z' };
    let float = if floating { 't' } else { 'f' };
    // A range runs one cell past its `#[norange]`, as in tmux, so each
    // control is targeted on two or three cells: zoom on the left end and
    // `z`, float on ` f `, close on `x` and the right end.
    format!(
        "#[range=control|8]{left}#[norange]{zoom}#[range=control|7] {float}#[norange] \
         #[range=control|9]x#[norange]{right}"
    )
}

/// The window's border options and each pane's [`PaneLook`].
#[derive(Debug, Clone, Default)]
pub struct PaneLooks {
    pub status: PaneStatus,
    pub lines: PaneLines,
    pub indicators: Indicators,
    /// `fill-character`, for the part of the client the window does not
    /// reach; the middle dot without one.
    pub fill: Option<char>,
    pub panes: HashMap<PaneId, PaneLook>,
    /// Framed tiles (spec 2026-11-05-frames.md): each visible tiled pane's
    /// frame rect, its content grown by one cell on every side, in window
    /// coordinates ([`Window::frame_rects`]). Empty draws tmux's shared
    /// borders. While it is not, floating panes take the same frames.
    pub frames: Vec<(PaneId, Rect)>,
    /// `pane-frame-title`: a frame's top edge carries the pane's
    /// `pane-border-format`.
    pub frame_title: bool,
    /// Where a dragged pane would land, a content rect
    /// ([`Window::drop_preview`]): framed in `pane-active-border-style`
    /// above the tiles and below the floating panes.
    pub drop_preview: Option<Rect>,
    /// A pane being moved with the mouse: its tile's frame is not drawn
    /// (its place shows empty), as the pane is drawn under the pointer.
    pub lifted: Option<PaneId>,
}

/// Fills in `looks`' frame part for window `w` (spec
/// 2026-11-05-frames.md): its frame rects and `pane-frame-title`, its drop
/// preview, and `rounded` lines where `pane-frames` is on and
/// `pane-border-lines` is left at its default. A framed window has no
/// `pane-border-status` (the title is in the frame).
pub fn apply_frames(model: &Model, w: &Window, looks: &mut PaneLooks) {
    looks.drop_preview = w.drop_preview;
    if !w.frames {
        return;
    }
    let chain = options::chain(model, w.active);
    // grofe's built-in default is `rounded` already; tmux identity's is
    // `single`, which gives way to `rounded` with frames on unless the
    // user set the lines: on the pane, window or session, or globally
    // (`set -g`, or the config).
    let name = "pane-border-lines";
    let set = model.options.set_globally(name)
        || chain.iter().any(|o| model.options.get(*o, name).is_some());
    if looks.lines == PaneLines::Single && model.identity == crate::compat::Identity::Tmux && !set {
        looks.lines = PaneLines::Rounded;
    }
    looks.frame_title = model.options.lookup("pane-frame-title", &chain) != Some("off");
    looks.frames = w.frame_rects();
    if !looks.frames.is_empty() {
        looks.status = PaneStatus::Off;
    }
}

/// Where things are on one client's screen, for an [`Overlay`] (and for
/// input hooks, through `clients::overlay_ctx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayCtx {
    pub client: ClientId,
    /// The window cell drawn at the client's top-left (D6 crop).
    pub viewport: (u16, u16),
    /// The client row a message or prompt goes on: the first status row
    /// (tmux's `message-line 0`); `None` when `status` is off.
    pub status_row: Option<u16>,
    /// The client row the window's top row is drawn on: the number of
    /// status rows with `status-position top`, else 0 (TC.11).
    pub window_top: u16,
    /// Each visible pane and its rect in client coordinates, clipped to
    /// the part the client shows. With a viewport, a pane's cell `(x, y)`
    /// is at client cell `(x + pane_left - viewport.0, ...)`: the clipped
    /// rect alone does not say where the pane's own origin is.
    pub panes: Vec<(PaneId, Rect)>,
}

/// Something drawn over a client's composed frame (M3: the prompt, the
/// scrollback view; later popups and menus). Overlays are stateless: what
/// they draw comes from `state`, for the client `client`.
///
/// An overlay may overwrite half of a wide pair; after every overlay has
/// drawn, the caller runs [`fix_wide_pairs`].
pub trait Overlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen);
}

/// Attribute flags that change how a cell looks. Everything else alacritty
/// keeps on a cell (wrap markers, the wide-char layout flags) is layout.
const STYLE_FLAGS: CellFlags = CellFlags::BOLD
    .union(CellFlags::DIM)
    .union(CellFlags::ITALIC)
    .union(CellFlags::ALL_UNDERLINES)
    .union(CellFlags::INVERSE)
    .union(CellFlags::HIDDEN)
    .union(CellFlags::STRIKEOUT);

/// What a composed cell may carry: style plus the wide-char pair flags.
const SCREEN_FLAGS: CellFlags = STYLE_FLAGS
    .union(CellFlags::WIDE_CHAR)
    .union(CellFlags::WIDE_CHAR_SPACER);

/// Letterbox fill for client cells outside a smaller window (spec D6).
const LETTERBOX: char = '·';

fn blank_like(cell: &SnapCell) -> SnapCell {
    SnapCell {
        c: ' ',
        fg: cell.fg,
        bg: cell.bg,
        flags: cell.flags & STYLE_FLAGS,
        zerowidth: Vec::new(),
        underline_color: cell.underline_color,
        hyperlink: None,
    }
}

fn styled(c: char, style: &Style) -> SnapCell {
    SnapCell {
        c,
        fg: style.fg,
        bg: style.bg,
        flags: style.attrs & STYLE_FLAGS,
        zerowidth: Vec::new(),
        underline_color: style.underline_color,
        hyperlink: None,
    }
}

fn cell_style(cell: &SnapCell) -> Style {
    Style {
        fg: cell.fg,
        bg: cell.bg,
        attrs: cell.flags & STYLE_FLAGS,
        underline_color: cell.underline_color,
    }
}

/// Composes one client's frame: the window (panes clipped into their rects,
/// then borders) seen through the viewport, the letterbox around a smaller
/// window, the status row and the cursor. Overlays are the caller's.
pub fn compose(inputs: &FrameInputs) -> Screen {
    let (cols, rows) = inputs.client;
    let mut screen = Screen::new(cols, rows);
    let (window, border) = compose_window(inputs);
    let (ww, wh) = (i32::from(window.cols), i32::from(window.rows));
    let is_border = |x: i32, y: i32| {
        (0..ww).contains(&x) && (0..wh).contains(&y) && border[(y * ww + x) as usize]
    };
    let win_rows = if inputs.status.is_some() {
        rows.saturating_sub(1)
    } else {
        rows
    };
    let (vx, vy) = inputs.viewport;
    let letterbox = SnapCell {
        c: inputs.look.fill.unwrap_or(LETTERBOX),
        flags: CellFlags::DIM,
        ..Screen::new(1, 1).cells[0].clone()
    };
    for cy in 0..win_rows {
        for cx in 0..cols {
            let (wx, wy) = (cx as u32 + vx as u32, cy as u32 + vy as u32);
            let inside = wx < window.cols as u32 && wy < window.rows as u32;
            let (ex, ey) = (wx as i32, wy as i32);
            let edge = (ex == ww && ey <= wh) || (ey == wh && ex <= ww);
            let mut cell = if inside {
                window.cells[wy as usize * window.cols as usize + wx as usize].clone()
            } else if edge {
                // tmux 3.7b (recorded) draws the edge of a window smaller
                // than its client as a border, joined to the pane borders
                // that meet it: `│` down its right, `─` along its foot,
                // `┘` at the corner, `┴` or `┤` where a border runs in.
                let up = if ex == ww {
                    ey > 0
                } else {
                    is_border(ex, ey - 1)
                };
                let down = ex == ww && ey < wh;
                let left = if ey == wh {
                    ex > 0
                } else {
                    is_border(ex - 1, ey)
                };
                let right = ey == wh && ex < ww;
                styled(glyph(up, down, left, right), &inputs.border_style)
            } else {
                letterbox.clone()
            };
            if cx == 0 && vx > 0 && inside {
                // The placeholder's left neighbour is cropped away: its
                // inherited values must be spelled out.
                let row = wy as usize * window.cols as usize;
                let row = &window.cells[row..row + window.cols as usize];
                if let Some(explicit) = graphics::detach_left(row, wx as usize) {
                    cell = explicit;
                }
            }
            // A wide char cut by the crop edge is drawn as a space.
            let cut_left = cx == 0 && cell.flags.contains(CellFlags::WIDE_CHAR_SPACER);
            let cut_right = cx + 1 == cols && cell.flags.contains(CellFlags::WIDE_CHAR);
            if cut_left || cut_right {
                cell = blank_like(&cell);
            }
            screen.cells[cy as usize * cols as usize + cx as usize] = cell;
        }
    }
    if let Some(caps) = inputs.images {
        let mut fallbacks = Vec::new();
        for (id, rect, snap) in &inputs.panes {
            let Some(view) = &snap.images else {
                continue;
            };
            let at = Place {
                origin: (
                    i32::from(rect.x) - i32::from(vx),
                    i32::from(rect.y) - i32::from(vy),
                ),
                size: (rect.cols, rect.rows),
                area: (cols, win_rows),
            };
            screenimg::collect(
                &mut screen.images,
                &mut fallbacks,
                *id,
                view,
                at,
                inputs.graphics,
                caps,
            );
        }
        for fallback in fallbacks {
            screenimg::paint_fallback(&mut screen, fallback);
        }
    }
    if let (Some(status), true) = (&inputs.status, rows > 0) {
        draw_line(&mut screen, rows - 1, status);
    }
    screen.cursor = inputs.cursor.filter(|c| c.visible).and_then(|c| {
        let col = c.col.checked_sub(vx)?;
        let row = c.row.checked_sub(vy)?;
        (col < cols && row < win_rows).then_some(CursorState { row, col, ..c })
    });
    screen
}

/// Makes every wide pair whole again: a `WIDE_CHAR` without its spacer, or
/// a spacer without its wide char, becomes a space keeping its colours.
/// Run after overlays have drawn; a no-op on a screen [`compose`] made.
pub fn fix_wide_pairs(screen: &mut Screen) {
    let cols = screen.cols as usize;
    if cols == 0 {
        return;
    }
    for row in screen.cells.chunks_mut(cols) {
        let mut x = 0;
        while x < cols {
            let wide = row[x].flags.contains(CellFlags::WIDE_CHAR);
            let spacer_next =
                x + 1 < cols && row[x + 1].flags.contains(CellFlags::WIDE_CHAR_SPACER);
            if wide && spacer_next {
                x += 2;
                continue;
            }
            if wide || row[x].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                row[x] = blank_like(&row[x]);
            }
            x += 1;
        }
    }
}

/// The whole window, in window coordinates, sized to `inputs.area`.
/// The window's cells and which of them are border.
fn compose_window(inputs: &FrameInputs) -> (Screen, Vec<bool>) {
    let area = inputs.area;
    let mut win = Screen::new(area.cols, area.rows);
    let width = area.cols as usize;
    for (id, rect, snap) in &inputs.panes {
        let look = inputs.look.panes.get(id);
        if let Some(look) = look {
            let blank = SnapCell {
                fg: look.fg,
                bg: look.bg,
                ..Screen::new(1, 1).cells[0].clone()
            };
            let left = (rect.x as usize).min(width);
            let right = (rect.x as usize + rect.cols as usize).min(width);
            let bottom = (rect.y as usize + rect.rows as usize).min(area.rows as usize);
            for wy in rect.y as usize..bottom {
                win.cells[wy * width + left..wy * width + right].fill(blank.clone());
            }
        }
        for (ry, row) in snap.rows.iter().take(rect.rows as usize).enumerate() {
            let wy = rect.y as usize + ry;
            if wy >= area.rows as usize {
                break;
            }
            let right = (rect.x as usize + rect.cols as usize).min(width);
            for (rx, src) in row.iter().enumerate() {
                let wx = rect.x as usize + rx;
                if wx >= right {
                    break;
                }
                let mut cell = src.clone();
                if let Some(look) = look {
                    // Only cells in the default colours take the style's.
                    if cell.fg == Color::Named(NamedColor::Foreground) {
                        cell.fg = look.fg;
                    }
                    if cell.bg == Color::Named(NamedColor::Background) {
                        cell.bg = look.bg;
                    }
                }
                if cell.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER) {
                    cell = blank_like(&cell);
                }
                cell.flags &= SCREEN_FLAGS;
                // A wide char whose right half falls outside the rect, or a
                // right half without its left, is a space.
                let wide_cut = cell.flags.contains(CellFlags::WIDE_CHAR) && wx + 1 >= right;
                let orphan = cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) && rx == 0;
                if wide_cut || orphan {
                    cell = blank_like(&cell);
                }
                win.cells[wy * width + wx] = cell;
            }
            let left = wy * width + rect.x as usize;
            let right = wy * width + right;
            if left < right {
                graphics::rewrite_row(&mut win.cells[left..right], *id, inputs.graphics);
            }
        }
    }
    let border = if inputs.look.frames.is_empty() {
        draw_borders(&mut win, inputs)
    } else {
        // Frames stand apart from the window's edge and from each other:
        // no cell of theirs joins the edge a smaller window gets.
        draw_frames(&mut win, inputs);
        vec![false; win.cells.len()]
    };
    if let Some(r) = inputs.look.drop_preview {
        let preview = PaneBox {
            id: inputs.active,
            x: i32::from(r.x),
            y: i32::from(r.y),
            w: i32::from(r.cols),
            h: i32::from(r.rows),
            index: 0,
        };
        let frame = Frame {
            lines: inputs.look.lines,
            style: inputs.active_border_style,
            title: None,
            tab: None,
            marked: false,
        };
        draw_frame(&mut win, &preview, &frame);
    }
    (win, border)
}

/// How one frame is drawn.
struct Frame<'a> {
    lines: PaneLines,
    style: Style,
    /// The title drawn in the top edge, if any.
    title: Option<&'a str>,
    /// The control tab at the right end of the top edge, if any.
    tab: Option<&'a str>,
    /// The marked pane's frame is drawn with reverse toggled.
    marked: bool,
}

/// Each framed tile's frame (spec 2026-11-05-frames.md), in its pane's
/// `pane-active-border-style` for the active pane, else its
/// `pane-border-style`, with the title in the top edge while
/// `pane-frame-title` is on. Gap cells keep the window's blank.
fn draw_frames(win: &mut Screen, inputs: &FrameInputs) {
    let looks = &inputs.look;
    for (i, (id, r)) in looks.frames.iter().enumerate() {
        if looks.lifted == Some(*id) {
            continue;
        }
        let look = looks.panes.get(id);
        let style = match (*id == inputs.active, look) {
            (true, Some(l)) => l.active_border,
            (false, Some(l)) => l.border,
            (true, None) => inputs.active_border_style,
            (false, None) => inputs.border_style,
        };
        let content = PaneBox {
            id: *id,
            x: i32::from(r.x) + 1,
            y: i32::from(r.y) + 1,
            w: i32::from(r.cols) - 2,
            h: i32::from(r.rows) - 2,
            index: look.map_or(i, |l| l.index),
        };
        let frame = Frame {
            lines: looks.lines,
            style,
            title: look.map(|l| l.title.as_str()).filter(|_| looks.frame_title),
            tab: look.and_then(|l| l.tab.as_deref()),
            marked: inputs.marked == Some(*id),
        };
        draw_frame(win, &content, &frame);
    }
}

/// The frame round `pane`'s content (`pane` in `screen`'s coordinates):
/// corners and edges of `frame.lines`, clipped to the screen, and the
/// title from two cells in, between a space each side, cut to leave the
/// last edge cell before the corner: `╭─ 1: nvim ─────╮`.
fn draw_frame(screen: &mut Screen, pane: &PaneBox, frame: &Frame) {
    let (cols, rows) = (i32::from(screen.cols), i32::from(screen.rows));
    let (x0, y0) = (pane.x - 1, pane.y - 1);
    let (x1, y1) = (pane.x + pane.w, pane.y + pane.h);
    if pane.w < 0 || pane.h < 0 {
        return;
    }
    let g = |cell| borders::frame_glyph(frame.lines, cell, Some(pane));
    let mut edge = Vec::new();
    for x in x0..=x1 {
        let (top, bottom) = match x {
            x if x == x0 => (borders::CELL_TOPLEFT, borders::CELL_BOTTOMLEFT),
            x if x == x1 => (borders::CELL_TOPRIGHT, borders::CELL_BOTTOMRIGHT),
            _ => (borders::CELL_LEFTRIGHT, borders::CELL_LEFTRIGHT),
        };
        edge.push((x, y0, g(top)));
        edge.push((x, y1, g(bottom)));
    }
    for y in pane.y..y1 {
        edge.push((x0, y, g(borders::CELL_TOPBOTTOM)));
        edge.push((x1, y, g(borders::CELL_TOPBOTTOM)));
    }
    let on_screen = |x: i32, y: i32| (0..cols).contains(&x) && (0..rows).contains(&y);
    for (x, y, ch) in &edge {
        if on_screen(*x, *y) {
            screen.cells[(y * cols + x) as usize] = styled(*ch, &frame.style);
        }
    }
    if frame.title.is_some() || frame.tab.is_some() {
        let edge = g(borders::CELL_LEFTRIGHT);
        let title = frame.title.unwrap_or("");
        let drawn = frame_title((x0, x1), cols, title, frame.tab, frame.style, edge);
        for (tx, row) in [drawn.title, drawn.tab].into_iter().flatten() {
            if on_screen(tx, y0) {
                draw_line_at(screen, y0 as u16, tx as usize, &row.line);
            }
        }
    }
    if frame.marked {
        for (x, y, _) in &edge {
            if on_screen(*x, *y) {
                screen.cells[(y * cols + x) as usize].flags ^= CellFlags::INVERSE;
            }
        }
    }
}

/// What a frame's top edge carries: its title and its control tab, each
/// with the column it starts at and what it draws.
#[derive(Debug, Default)]
pub struct TitleEdge {
    pub title: Option<(i32, StatusRow)>,
    pub tab: Option<(i32, StatusRow)>,
}

impl TitleEdge {
    /// The `#[range=...]` under column `x`: the tab's first, then the
    /// title's.
    pub fn range_at(&self, x: i32) -> Option<&crate::server::status::StatusRange> {
        [&self.tab, &self.title]
            .into_iter()
            .flatten()
            .find_map(|(tx, row)| row.range_at(u16::try_from(x - tx).ok()?))
    }
}

/// A frame's title and control tab on its top edge from column `x0` to
/// `x1` (the corners) in a screen `cols` wide. The tab ([`frame_tab`]),
/// when given and there is room, ends one edge cell before the right
/// corner. The title runs from two cells in, between a space each side,
/// over `edge` glyphs, and stops one edge cell before the tab (or before
/// the right corner), so corners and the cells next to them stay the
/// frame's. Each part keeps its `#[range=...]` parts (columns from its
/// start) for the mouse; one with no room, or no text, is `None`.
pub fn frame_title(
    (x0, x1): (i32, i32),
    cols: i32,
    title: &str,
    tab: Option<&str>,
    style: Style,
    edge: char,
) -> TitleEdge {
    let draw = |text: &str, width: i32| {
        let under = vec![(edge, style); width as usize];
        crate::server::status::draw_over(text, style, &under)
    };
    let mut out = TitleEdge::default();
    let tab_x = x1 - 1 - TAB_WIDTH;
    let mut right = x1 - 1;
    if let Some(tab) = tab
        && tab_x >= x0 + 2
        && tab_x >= 0
        && tab_x + TAB_WIDTH <= cols
    {
        out.tab = Some((tab_x, draw(tab, TAB_WIDTH)));
        right = tab_x - 1;
    }
    let tx = x0 + 2;
    let width = (right - tx).min(cols - tx);
    if !title.is_empty() && width >= 3 && tx >= 0 {
        out.title = Some((tx, draw(&format!(" {title} "), width)));
    }
    out
}

/// A border glyph by which neighbours are border too.
fn glyph(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (true, true, true, true) => '┼',
        (true, true, false, true) => '├',
        (true, true, true, false) => '┤',
        (false, true, true, true) => '┬',
        (true, false, true, true) => '┴',
        (false, true, false, true) => '┌',
        (false, true, true, false) => '┐',
        (true, false, false, true) => '└',
        (true, false, true, false) => '┘',
        (_, _, false, false) => '│',
        _ => '─',
    }
}

/// The pane borders and their titles, drawn as tmux 3.7b's
/// `screen-redraw.c` does ([`crate::server::borders`]): each cell that is
/// not a pane's content gets the glyph its neighbours call for in
/// `pane-border-lines`, in the style of the pane it belongs to: its
/// `pane-active-border-style` where it is on the active pane's border,
/// else `pane-border-style`. Returns which cells are border.
fn draw_borders(win: &mut Screen, inputs: &FrameInputs) -> Vec<bool> {
    let (w, h) = (win.cols as i32, win.rows as i32);
    let mut mask = vec![false; win.cells.len()];
    let looks = &inputs.look;
    let rects: Vec<(PaneId, Rect)> = inputs.panes.iter().map(|(id, r, _)| (*id, *r)).collect();
    let boxes = pane_boxes(looks, &rects);
    let active = boxes.iter().position(|b| b.id == inputs.active);
    let marked = inputs
        .marked
        .and_then(|m| boxes.iter().position(|b| b.id == m));
    let geom = Geometry::new((w, h), looks.status, looks.indicators, &boxes, active);
    let style_of = |id: PaneId, is_active: bool| {
        let look = looks.panes.get(&id);
        match (is_active, look) {
            (true, Some(l)) => l.active_border,
            (false, Some(l)) => l.border,
            (true, None) => inputs.active_border_style,
            (false, None) => inputs.border_style,
        }
    };
    for y in 0..h {
        for x in 0..w {
            if boxes
                .iter()
                .any(|b| x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h)
            {
                continue;
            }
            let (cell, owner) = geom.check_cell(x, y);
            let Some(owner) = owner.map(|i| &boxes[i]) else {
                continue;
            };
            if cell == borders::CELL_INSIDE || cell == borders::CELL_OUTSIDE {
                continue;
            }
            let on_active = active.is_some_and(|a| geom.is_border_of(&boxes[a], x, y));
            let ch = geom
                .arrow(x, y, cell, owner)
                .unwrap_or_else(|| borders::glyph(looks.lines, cell, Some(owner)));
            let i = (y * w + x) as usize;
            let mut drawn = styled(ch, &style_of(owner.id, on_active));
            // `screen_redraw_draw_borders_cell`: the marked pane's border
            // is drawn in reverse.
            if marked.is_some_and(|m| geom.is_border_of(&boxes[m], x, y)) {
                drawn.flags ^= CellFlags::INVERSE;
            }
            win.cells[i] = drawn;
            mask[i] = true;
        }
    }
    draw_unowned_segments(win, inputs, &mut mask, &boxes);
    let titles = border_titles(
        looks,
        (w, h),
        &rects,
        inputs.active,
        (inputs.border_style, inputs.active_border_style),
    );
    for t in titles {
        draw_line_at(win, t.row as u16, t.x as usize, &t.drawn.line);
        for i in 0..t.width {
            mask[(t.row * w + t.x + i) as usize] = true;
        }
    }
    mask
}

/// `rects` as the boxes the border geometry works on.
fn pane_boxes(looks: &PaneLooks, rects: &[(PaneId, Rect)]) -> Vec<PaneBox> {
    rects
        .iter()
        .enumerate()
        .map(|(i, (id, r))| PaneBox {
            id: *id,
            x: i32::from(r.x),
            y: i32::from(r.y),
            w: i32::from(r.cols),
            h: i32::from(r.rows),
            index: looks.panes.get(id).map_or(i, |l| l.index),
        })
        .collect()
}

/// One pane's border title, drawn.
pub struct Title {
    pub pane: PaneId,
    /// The window row it is on and the column it starts at.
    pub row: i32,
    pub x: i32,
    /// How many columns it covers.
    pub width: i32,
    /// The title over the border glyphs it replaces, with the click ranges
    /// of its `#[range=...]` parts (columns from `x`).
    pub drawn: StatusRow,
}

/// The titles `pane-border-status` draws on `rects`' panes' border lines in
/// a window `size` cells big (`fallback` styles for a pane `looks` has none
/// for): the pane's `pane-border-format` over its border, in its border
/// style (the active one for `active`).
pub fn border_titles(
    looks: &PaneLooks,
    size: (i32, i32),
    rects: &[(PaneId, Rect)],
    active: PaneId,
    fallback: (Style, Style),
) -> Vec<Title> {
    if looks.status == PaneStatus::Off {
        return Vec::new();
    }
    let boxes = pane_boxes(looks, rects);
    let active_index = boxes.iter().position(|b| b.id == active);
    let geom = Geometry::new(size, looks.status, looks.indicators, &boxes, active_index);
    let mut out = Vec::new();
    for (n, b) in boxes.iter().enumerate() {
        let width = geom.title_width(b);
        let row = geom.title_row(b);
        if width <= 0 || !(0..size.1).contains(&row) {
            continue;
        }
        let look = looks.panes.get(&b.id);
        let style = match (active_index == Some(n), look) {
            (true, Some(l)) => l.active_border,
            (false, Some(l)) => l.border,
            (true, None) => fallback.1,
            (false, None) => fallback.0,
        };
        let under: Vec<(char, Style)> = (0..width)
            .map(|i| {
                let cell = geom.type_of_cell(b.x + 2 + i, row);
                (borders::glyph(looks.lines, cell, Some(b)), style)
            })
            .collect();
        let title = look.map_or("", |l| l.title.as_str());
        out.push(Title {
            pane: b.id,
            row,
            x: b.x + 2,
            width,
            drawn: crate::server::status::draw_over(title, style, &under),
        });
    }
    out
}

/// Layout border segments no pane owns in tmux's geometry (a window too
/// small for its panes, a hand-built layout) still get a line, its glyph
/// picked from which neighbours are border, in `pane-border-style`.
fn draw_unowned_segments(
    win: &mut Screen,
    inputs: &FrameInputs,
    mask: &mut [bool],
    boxes: &[PaneBox],
) {
    let (w, h) = (win.cols as i32, win.rows as i32);
    let mut seg = vec![false; mask.len()];
    let mut mark = |x: i32, y: i32| {
        if (0..w).contains(&x) && (0..h).contains(&y) {
            seg[(y * w + x) as usize] = true;
        }
    };
    for s in &inputs.borders {
        match *s {
            BorderSegment::Vertical { col, row, len } => {
                (0..len).for_each(|i| mark(i32::from(col), i32::from(row) + i32::from(i)))
            }
            BorderSegment::Horizontal { row, col, len } => {
                (0..len).for_each(|i| mark(i32::from(col) + i32::from(i), i32::from(row)))
            }
        }
    }
    let joined: Vec<bool> = mask.iter().zip(&seg).map(|(a, b)| *a || *b).collect();
    let at =
        |x: i32, y: i32| (0..w).contains(&x) && (0..h).contains(&y) && joined[(y * w + x) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            let inside = boxes
                .iter()
                .any(|b| x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h);
            if !seg[i] || mask[i] || inside {
                continue;
            }
            let ch = glyph(at(x, y - 1), at(x, y + 1), at(x - 1, y), at(x + 1, y));
            win.cells[i] = styled(ch, &inputs.border_style);
            mask[i] = true;
        }
    }
}

/// A floating pane to draw over a window (tmux 3.7's `new-pane`, TC.6):
/// its top-left content cell in window coordinates, its size and rows,
/// and whether it is the window's active pane.
pub struct FloatingPane {
    pub id: PaneId,
    pub at: (i32, i32),
    pub size: (u16, u16),
    pub snap: RowsSnapshot,
    pub active: bool,
}

/// Draws `panes` (lowest first) over a window's composed `screen`, seen
/// through `viewport`, as tmux 3.7b draws floating panes (recorded): each
/// is a box of `pane-border-lines` around its content, in the pane's
/// active border style for the active pane and its border style for the
/// others (`style` and `active_style` where `looks` has none for it), over
/// the tiled panes and any lower floating pane, with the pane's border
/// title on its top (or bottom) line while `pane-border-status` is on. The
/// cursor is the active floating pane's; the active tiled pane's is hidden
/// when a floating pane covers it.
pub fn draw_floating(
    screen: &mut Screen,
    panes: &[FloatingPane],
    viewport: (u16, u16),
    style: &Style,
    active_style: &Style,
    looks: &PaneLooks,
) {
    let (cols, rows) = (i32::from(screen.cols), i32::from(screen.rows));
    let put = |screen: &mut Screen, x: i32, y: i32, cell: SnapCell| {
        if (0..cols).contains(&x) && (0..rows).contains(&y) {
            screen.cells[(y * cols + x) as usize] = cell;
        }
    };
    for pane in panes {
        let x = pane.at.0 - i32::from(viewport.0);
        let y = pane.at.1 - i32::from(viewport.1);
        let (w, h) = (i32::from(pane.size.0), i32::from(pane.size.1));
        let look = looks.panes.get(&pane.id);
        let border = match (pane.active, look) {
            (true, Some(l)) => l.active_border,
            (false, Some(l)) => l.border,
            (true, None) => *active_style,
            (false, None) => *style,
        };
        let index = look.map(|l| l.index);
        let shown = PaneBox {
            id: pane.id,
            x: pane.at.0,
            y: pane.at.1,
            w,
            h,
            index: index.unwrap_or(0),
        };
        let g = |cell| borders::glyph(looks.lines, cell, Some(&shown));
        let framed = !looks.frames.is_empty();
        if framed {
            // The tiles' frame style, title included (spec
            // 2026-11-05-frames.md).
            let on_screen = PaneBox { x, y, ..shown };
            let frame = Frame {
                lines: looks.lines,
                style: border,
                title: look.map(|l| l.title.as_str()).filter(|_| looks.frame_title),
                tab: look.and_then(|l| l.tab.as_deref()),
                marked: false,
            };
            draw_frame(screen, &on_screen, &frame);
        }
        for i in (-1..=w).filter(|_| !framed) {
            let (top, bottom) = match i {
                -1 => (borders::CELL_TOPLEFT, borders::CELL_BOTTOMLEFT),
                i if i == w => (borders::CELL_TOPRIGHT, borders::CELL_BOTTOMRIGHT),
                _ => (borders::CELL_LEFTRIGHT, borders::CELL_LEFTRIGHT),
            };
            put(screen, x + i, y - 1, styled(g(top), &border));
            put(screen, x + i, y + h, styled(g(bottom), &border));
        }
        for j in (0..h).filter(|_| !framed) {
            put(
                screen,
                x - 1,
                y + j,
                styled(g(borders::CELL_TOPBOTTOM), &border),
            );
            put(
                screen,
                x + w,
                y + j,
                styled(g(borders::CELL_TOPBOTTOM), &border),
            );
        }
        for (r, row) in pane.snap.rows.iter().enumerate() {
            for (c, cell) in row.iter().take(pane.size.0 as usize).enumerate() {
                let mut cell = cell.clone();
                if let Some(l) = look {
                    if cell.fg == Color::Named(NamedColor::Foreground) {
                        cell.fg = l.fg;
                    }
                    if cell.bg == Color::Named(NamedColor::Background) {
                        cell.bg = l.bg;
                    }
                }
                put(screen, x + c as i32, y + r as i32, cell);
            }
        }
        if let (Some(l), true) = (look, looks.status != PaneStatus::Off) {
            // The title runs from two cells in, the pane's width less two,
            // and not past the window's right edge.
            let width = if w < 4 { 0 } else { w - 2 };
            let width = width.min((cols - (x + 2)).max(0)).min(cols);
            let row = if looks.status == PaneStatus::Top {
                y - 1
            } else {
                y + h
            };
            if width > 0 && x + 2 >= 0 && (0..rows).contains(&row) {
                let under: Vec<(char, Style)> = (0..width)
                    .map(|_| (g(borders::CELL_LEFTRIGHT), border))
                    .collect();
                let line = crate::server::status::draw_over(&l.title, border, &under).line;
                draw_line_at(screen, row as u16, (x + 2) as usize, &line);
            }
        }
        let covers =
            |cx: i32, cy: i32| (x - 1..=x + w).contains(&cx) && (y - 1..=y + h).contains(&cy);
        if pane.active {
            let c = pane.snap.cursor;
            let (cx, cy) = (x + i32::from(c.col), y + i32::from(c.row));
            let shown = c.visible && (0..cols).contains(&cx) && (0..rows).contains(&cy);
            screen.cursor = shown.then_some(CursorState {
                col: cx as u16,
                row: cy as u16,
                ..c
            });
        } else if screen
            .cursor
            .is_some_and(|c| covers(i32::from(c.col), i32::from(c.row)))
        {
            screen.cursor = None;
        }
    }
}

/// A window's composed screen with the status rows stacked above it
/// (`top`) or below it, as one client screen: the window's cells and
/// cursor move down by the rows above them (TC.11, `status-position`).
pub fn with_status(window: Screen, status: &[StyledLine], top: bool) -> Screen {
    let lines = status.len() as u16;
    let mut screen = Screen::new(window.cols, window.rows + lines);
    let above = if top { lines } else { 0 };
    let cols = usize::from(window.cols);
    let start = usize::from(above) * cols;
    screen.cells[start..start + window.cells.len()].clone_from_slice(&window.cells);
    screen.images = window.images;
    screenimg::shift_down(&mut screen.images, above);
    let first = if top { 0 } else { window.rows };
    for (i, line) in status.iter().enumerate() {
        draw_line(&mut screen, first + i as u16, line);
    }
    screen.cursor = window.cursor.map(|c| CursorState {
        row: c.row + above,
        ..c
    });
    screen
}

/// Draws `line` over row `row` of `screen`, from column 0.
pub fn draw_row(screen: &mut Screen, row: u16, line: &StyledLine) {
    if row < screen.rows {
        draw_line(screen, row, line);
    }
}

/// Draws `line` on `row` from column 0, clipped at the screen width. Zero
/// width characters join the cell before them; a wide char that does not
/// fit is a space.
fn draw_line(screen: &mut Screen, row: u16, line: &StyledLine) {
    draw_line_at(screen, row, 0, line);
}

/// [`draw_line`] from column `x0`.
fn draw_line_at(screen: &mut Screen, row: u16, x0: usize, line: &StyledLine) {
    let cols = screen.cols as usize;
    let base = row as usize * cols;
    let mut x = x0;
    let mut last: Option<usize> = None;
    for span in &line.spans {
        for ch in span.text.chars() {
            let Some(width) = UnicodeWidthChar::width(ch) else {
                continue;
            };
            if width == 0 {
                if let Some(i) = last {
                    screen.cells[base + i].zerowidth.push(ch);
                }
                continue;
            }
            if x + width > cols {
                if x < cols {
                    screen.cells[base + x] = styled(' ', &span.style);
                }
                return;
            }
            let mut cell = styled(ch, &span.style);
            if width == 2 {
                cell.flags |= CellFlags::WIDE_CHAR;
                let mut spacer = styled(' ', &span.style);
                spacer.flags |= CellFlags::WIDE_CHAR_SPACER;
                screen.cells[base + x + 1] = spacer;
            }
            screen.cells[base + x] = cell;
            last = Some(x);
            x += width;
        }
    }
}

/// The escape stream that turns `prev` into `next` on a terminal with
/// `caps`: colours downgraded to what it supports, wrapped in synchronized
/// output when it has it. `prev = None` is a full redraw (first frame, or
/// after a dropped frame), as is a size change. An unchanged frame is an
/// empty stream.
///
/// Every run of changed cells starts with an absolute cursor move, and one
/// follows every wide or zero-width cell (spec §4.4: the outer terminal's
/// width table may disagree with ours).
pub fn diff(prev: Option<&Screen>, next: &Screen, caps: &Caps) -> Vec<u8> {
    diff_placed(prev, next, caps, prev.map_or(&[], |p| &p.images))
}

/// [`diff`] for a terminal that shows the images `placed`, which are not
/// always those of `prev` (a dropped frame forgets `prev`, not the
/// terminal's images).
pub fn diff_placed(
    prev: Option<&Screen>,
    next: &Screen,
    caps: &Caps,
    placed: &[ScreenImage],
) -> Vec<u8> {
    let blank;
    let (base, full) = match prev {
        Some(p) if p.cols == next.cols && p.rows == next.rows => (p, false),
        _ => {
            blank = Screen::new(next.cols, next.rows);
            (&blank, true)
        }
    };
    let mut w = Writer {
        out: Vec::new(),
        pos: None,
        pen: None,
        link: None,
        cols: next.cols,
        caps,
    };
    if full {
        // Reset the scroll region, origin mode and pen, then clear to
        // default blanks: exactly `blank`, whatever state the terminal was in.
        w.out
            .extend_from_slice(b"\x1b[r\x1b[?6l\x1b[0m\x1b[H\x1b[2J");
    }
    let cols = next.cols as usize;
    let mut changed = vec![false; cols];
    // A sixel image the terminal wipes only when its cells are written.
    let wipe = if full {
        Vec::new()
    } else {
        screenimg::wipe_rects(placed, &next.images)
    };
    for y in 0..next.rows as usize {
        let row = &next.cells[y * cols..(y + 1) * cols];
        let old = &base.cells[y * cols..(y + 1) * cols];
        for x in 0..cols {
            changed[x] = row[x] != old[x]
                || wipe.iter().any(|&(wx, wy, ww, wh)| {
                    (wx as usize..(wx + ww) as usize).contains(&x)
                        && (wy as usize..(wy + wh) as usize).contains(&y)
                });
        }
        // A changed right half redraws its wide char.
        for x in 1..cols {
            if changed[x] && row[x].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                changed[x - 1] = true;
            }
        }
        for x in 0..cols {
            if changed[x] && !row[x].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                w.cell(x as u16, y as u16, &row[x]);
            }
        }
        w.link_to(None);
    }
    if !(placed.is_empty() && next.images.is_empty()) {
        let mut images = Vec::new();
        screenimg::emit(&mut images, next, placed, full, |x, y| {
            let i = y as usize * cols + x as usize;
            full || next.cells[i] != base.cells[i]
        });
        w.out.extend_from_slice(&images);
    }
    let drew = !w.out.is_empty();
    let was = if full { None } else { base.cursor };
    match next.cursor {
        Some(c) => {
            if full || drew || was.is_none_or(|p| (p.row, p.col) != (c.row, c.col)) {
                cup(&mut w.out, c.col, c.row);
            }
            if full || was.is_none() {
                w.out.extend_from_slice(b"\x1b[?25h");
            }
            if full || was.is_none_or(|p| (p.shape, p.blink) != (c.shape, c.blink)) {
                decscusr(&mut w.out, &c);
            }
        }
        None => {
            if full || was.is_some() {
                w.out.extend_from_slice(b"\x1b[?25l");
            }
        }
    }
    if w.out.is_empty() {
        return w.out;
    }
    if caps.sync {
        let mut out = Vec::with_capacity(w.out.len() + 16);
        out.extend_from_slice(b"\x1b[?2026h");
        out.extend_from_slice(&w.out);
        out.extend_from_slice(b"\x1b[?2026l");
        out
    } else {
        w.out
    }
}

/// DECSCUSR for `c`. A steady block is sent as `0`, the terminal's own
/// default: alacritty cannot tell "the app never set a style" from "the app
/// asked for a steady block", and the first is far more common.
fn decscusr(out: &mut Vec<u8>, c: &CursorState) {
    let n = match (c.shape, c.blink) {
        (CursorShape::Block, false) => 0,
        (CursorShape::Block, true) => 1,
        (CursorShape::Underline, true) => 3,
        (CursorShape::Underline, false) => 4,
        (CursorShape::Bar, true) => 5,
        (CursorShape::Bar, false) => 6,
    };
    out.extend_from_slice(format!("\x1b[{n} q").as_bytes());
}

fn cup(out: &mut Vec<u8>, col: u16, row: u16) {
    out.extend_from_slice(format!("\x1b[{};{}H", row as u32 + 1, col as u32 + 1).as_bytes());
}

/// Tracks the outer terminal's cursor and pen while a diff is written.
struct Writer<'a> {
    out: Vec<u8>,
    /// Where the outer cursor is, when known.
    pos: Option<(u16, u16)>,
    /// The SGR state last written in this diff, and the caps it was
    /// written for; `None` until the first.
    pen: Option<(Style, Caps)>,
    /// The OSC 8 hyperlink open on the outer terminal (R20).
    link: Option<Hyperlink>,
    cols: u16,
    caps: &'a Caps,
}

impl Writer<'_> {
    fn cell(&mut self, x: u16, y: u16, cell: &SnapCell) {
        if self.pos != Some((x, y)) {
            // A link must not cover the cells the move skips.
            self.link_to(None);
            cup(&mut self.out, x, y);
        }
        self.link_to(cell.hyperlink.as_ref());
        let style = cell_style(cell);
        // A placeholder's colour is its image id and its underline colour
        // the placement id: never downgraded or dropped for a terminal
        // that shows kitty graphics.
        let caps = if cell.c == graphics::PLACEHOLDER {
            Caps {
                truecolor: true,
                styled_underline: self.caps.styled_underline || self.caps.kitty_graphics,
                ..*self.caps
            }
        } else {
            *self.caps
        };
        let pen = (style, caps);
        if self.pen != Some(pen) {
            write_sgr(&mut self.out, &style, &caps);
            self.pen = Some(pen);
        }
        let c = if cell.c < ' ' || cell.c == '\u{7f}' {
            ' '
        } else {
            cell.c
        };
        let mut buf = [0u8; 4];
        self.out
            .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        for zw in &cell.zerowidth {
            self.out
                .extend_from_slice(zw.encode_utf8(&mut buf).as_bytes());
        }
        let wide = cell.flags.contains(CellFlags::WIDE_CHAR);
        let odd = wide || !cell.zerowidth.is_empty() || UnicodeWidthChar::width(c) != Some(1);
        let next = x + if wide { 2 } else { 1 };
        if odd {
            // The outer width table may disagree: put the cursor where ours
            // says the next cell is.
            let col = next.min(self.cols - 1);
            cup(&mut self.out, col, y);
            self.pos = Some((col, y));
        } else {
            // Past the last column the cursor sits in a pending wrap.
            self.pos = (next < self.cols).then_some((next, y));
        }
    }

    /// Opens `link` (OSC 8 with its id, so a link cut by a move or a row
    /// end stays one link), or closes the open one for `None`.
    // ponytail: sent to every client; tmux sends OSC 8 only to terminals
    // with its `hyperlinks` feature. A `Caps` flag for it is the M3.6
    // upgrade.
    fn link_to(&mut self, link: Option<&Hyperlink>) {
        if self.link.as_ref() == link {
            return;
        }
        let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
        let seq = match link {
            Some(l) => format!("\x1b]8;id={};{}\x1b\\", clean(l.id()), clean(l.uri())),
            None => "\x1b]8;;\x1b\\".to_string(),
        };
        self.out.extend_from_slice(seq.as_bytes());
        self.link = link.cloned();
    }
}

/// Writes one complete SGR sequence, `ESC [ 0 ; ... m`, that sets the pen
/// to `style` on a terminal with `caps`: truecolor downgraded to the
/// nearest 256-colour index without `caps.truecolor`; styled underlines
/// degraded to a plain one and the underline colour dropped without
/// `caps.styled_underline`. Layout flags in `style.attrs` are ignored.
///
/// The renderer calls it on every pen change; `capture-pane -e` reuses it.
pub fn write_sgr(out: &mut Vec<u8>, style: &Style, caps: &Caps) {
    let mut s = String::from("\x1b[0");
    let f = style.attrs;
    for (flag, code) in [
        (CellFlags::BOLD, "1"),
        (CellFlags::DIM, "2"),
        (CellFlags::ITALIC, "3"),
    ] {
        if f.contains(flag) {
            s.push(';');
            s.push_str(code);
        }
    }
    if f.intersects(CellFlags::ALL_UNDERLINES) {
        let code = if !caps.styled_underline {
            "4"
        } else if f.contains(CellFlags::UNDERCURL) {
            "4:3"
        } else if f.contains(CellFlags::DOUBLE_UNDERLINE) {
            "4:2"
        } else if f.contains(CellFlags::DOTTED_UNDERLINE) {
            "4:4"
        } else if f.contains(CellFlags::DASHED_UNDERLINE) {
            "4:5"
        } else {
            "4"
        };
        s.push(';');
        s.push_str(code);
    }
    for (flag, code) in [
        (CellFlags::INVERSE, "7"),
        (CellFlags::HIDDEN, "8"),
        (CellFlags::STRIKEOUT, "9"),
    ] {
        if f.contains(flag) {
            s.push(';');
            s.push_str(code);
        }
    }
    push_color(&mut s, style.fg, Layer::Fg, caps);
    push_color(&mut s, style.bg, Layer::Bg, caps);
    if caps.styled_underline
        && let Some(u) = style.underline_color
    {
        push_color(&mut s, u, Layer::Underline, caps);
    }
    s.push('m');
    out.extend_from_slice(s.as_bytes());
}

#[derive(Clone, Copy)]
enum Layer {
    Fg,
    Bg,
    Underline,
}

fn push_color(s: &mut String, color: Color, layer: Layer, caps: &Caps) {
    let (base, bright, extended) = match layer {
        Layer::Fg => (30, 90, 38),
        Layer::Bg => (40, 100, 48),
        Layer::Underline => (0, 0, 58),
    };
    let code = match color {
        Color::Named(n) => {
            let i = n as usize;
            match (layer, i) {
                // SGR 58 has no short forms: use the palette index.
                (Layer::Underline, 0..16) => format!("{extended};5;{i}"),
                (_, 0..8) => format!("{}", base + i),
                (_, 8..16) => format!("{}", bright + i - 8),
                // Dim variants are the base colour; the cell carries DIM.
                (Layer::Underline, 259..=266) => format!("{extended};5;{}", i - 259),
                (_, 259..=266) => format!("{}", base + i - 259),
                // Foreground, Background, Cursor and the rest: the default,
                // which the leading reset already set.
                _ => return,
            }
        }
        Color::Indexed(i) => format!("{extended};5;{i}"),
        Color::Spec(rgb) if caps.truecolor => {
            format!("{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b)
        }
        Color::Spec(rgb) => format!("{extended};5;{}", rgb_to_256(rgb.r, rgb.g, rgb.b)),
    };
    s.push(';');
    s.push_str(&code);
}

/// The nearest xterm 256-colour index to an RGB colour: the closer of the
/// 6x6x6 cube and the grey ramp (tmux's `colour_find_rgb`).
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let cube = |v: u8| -> usize {
        match v {
            0..48 => 0,
            48..114 => 1,
            _ => (v as usize - 35) / 40,
        }
    };
    let (qr, qg, qb) = (cube(r), cube(g), cube(b));
    let (cr, cg, cb) = (LEVELS[qr], LEVELS[qg], LEVELS[qb]);
    let (r, g, b) = (r as i32, g as i32, b as i32);
    if (cr, cg, cb) == (r, g, b) {
        return (16 + 36 * qr + 6 * qg + qb) as u8;
    }
    let avg = (r + g + b) / 3;
    let grey_idx = if avg > 238 { 23 } else { (avg - 3).max(0) / 10 };
    let grey = 8 + 10 * grey_idx;
    let dist = |x: i32, y: i32, z: i32| (x - r).pow(2) + (y - g).pow(2) + (z - b).pow(2);
    if dist(grey, grey, grey) < dist(cr, cg, cb) {
        (232 + grey_idx) as u8
    } else {
        (16 + 36 * qr + 6 * qg + qb) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_downgrade_matches_xterm_palette() {
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(128, 128, 128), 244);
        assert_eq!(rgb_to_256(0x87, 0xaf, 0xd7), 16 + 36 * 2 + 6 * 3 + 4);
        assert_eq!(rgb_to_256(1, 2, 3), 16);
    }

    #[test]
    fn overlays_that_split_a_wide_pair_leave_spaces() {
        let red = Color::Named(NamedColor::Red);
        let mut snap = RowsSnapshot::default();
        let mut row = Screen::new(6, 1).cells;
        for x in [0, 2] {
            row[x].c = '\u{6F22}';
            row[x].flags = CellFlags::WIDE_CHAR;
            row[x].bg = red;
            row[x + 1].flags = CellFlags::WIDE_CHAR_SPACER;
            row[x + 1].bg = red;
        }
        snap.rows.push(row);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 6,
            rows: 1,
        };
        let mut screen = compose(&FrameInputs {
            client: (6, 1),
            viewport: (0, 0),
            area,
            panes: vec![(PaneId(1), area, snap)],
            borders: Vec::new(),
            border_style: Style::default(),
            active_border_style: Style::default(),
            active: PaneId(1),
            marked: None,
            status: None,
            cursor: None,
            graphics: None,
            images: None,
            look: Default::default(),
        });
        // What an overlay might do: overwrite the spacer of the first pair
        // and the wide char of the second.
        screen.cells[1].c = 'x';
        screen.cells[1].flags = CellFlags::empty();
        screen.cells[2].c = 'y';
        screen.cells[2].flags = CellFlags::empty();
        fix_wide_pairs(&mut screen);
        let cells = &screen.cells;
        assert_eq!(
            (cells[0].c, cells[0].flags, cells[0].bg),
            (' ', CellFlags::empty(), red)
        );
        assert_eq!((cells[1].c, cells[2].c), ('x', 'y'));
        assert_eq!(
            (cells[3].c, cells[3].flags, cells[3].bg),
            (' ', CellFlags::empty(), red)
        );
    }

    fn find(hay: &[u8], needle: &[u8]) -> Vec<usize> {
        (0..hay.len().saturating_sub(needle.len() - 1))
            .filter(|&i| hay[i..].starts_with(needle))
            .collect()
    }

    /// R20: a linked cell is drawn inside OSC 8 open and close; the link
    /// closes before a move that skips cells and at the row end.
    #[test]
    fn hyperlinks_are_reemitted_around_their_cells() {
        let link = Hyperlink::new(Some("k"), "https://example.com".to_string());
        let open: &[u8] = b"\x1b]8;id=k;https://example.com\x1b\\";
        let close: &[u8] = b"\x1b]8;;\x1b\\";

        let mut next = Screen::new(6, 2);
        next.cells[1].c = 'X';
        next.cells[1].hyperlink = Some(link.clone());
        next.cells[2].c = 'y';
        let out = diff(None, &next, &Caps::default());
        let o = find(&out, open);
        assert_eq!(o.len(), 1, "{}", String::from_utf8_lossy(&out));
        // The pen may be set between the open and the `X`.
        let x = find(&out, b"X")[0];
        assert!(o[0] < x, "{}", String::from_utf8_lossy(&out));
        assert!(
            out[x + 1..].starts_with(close),
            "{}",
            String::from_utf8_lossy(&out)
        );

        // Only the linked cells change: each run opens after its move and
        // closes before the next move and at the row end.
        let prev = Screen::new(6, 2);
        let mut next = Screen::new(6, 2);
        for x in [0, 3, 5] {
            next.cells[x].c = 'L';
            next.cells[x].hyperlink = Some(link.clone());
        }
        next.cells[6].c = 'z';
        let out = diff(Some(&prev), &next, &Caps::default());
        let text = String::from_utf8_lossy(&out);
        assert_eq!(find(&out, open).len(), 3, "{text}");
        assert_eq!(find(&out, close).len(), 3, "{text}");
        let z = find(&out, b"z")[0];
        assert!(find(&out, close).iter().all(|&c| c < z), "{text}");
    }

    #[test]
    fn default_style_is_a_bare_reset() {
        let mut out = Vec::new();
        write_sgr(&mut out, &Style::default(), &Caps::default());
        assert_eq!(out, b"\x1b[0m");
    }
}
