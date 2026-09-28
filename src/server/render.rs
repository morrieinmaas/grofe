//! compose, diff, colour downgrade, synchronized output.
//!
//! T2.0 lands the contract (types and signatures); T2.2 fills in
//! [`compose`] and [`diff`] (spec §4.4).
//!
//! Coordinates: pane rects, borders and [`FrameInputs::cursor`] are in
//! window coordinates, the space the layout was computed in
//! ([`FrameInputs::area`]). A [`Screen`] is in client coordinates: the
//! client's terminal, `client.0` columns by `client.1` rows.

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::protocol::Caps;
use crate::server::layout::BorderSegment;
use crate::server::pane::{CellFlags, CursorState, RowsSnapshot, SnapCell};
use crate::server::status::StyledLine;
use crate::types::{PaneId, Rect};

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
        };
        Screen {
            cols,
            rows,
            cells: vec![blank; cols as usize * rows as usize],
            cursor: None,
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
    pub borders: Vec<BorderSegment>,
    /// The pane whose borders are drawn active.
    pub active: PaneId,
    /// The status row, drawn on the client's last row; the window then has
    /// the rows above it. `None` when `status` is off.
    pub status: Option<StyledLine>,
    /// Drawn last, in order, over everything else.
    pub overlays: &'a [&'a dyn Overlay],
    /// The active pane's cursor in window coordinates (its rect origin
    /// added); `None` hides the cursor.
    pub cursor: Option<CursorState>,
}

/// Something drawn over the composed frame (M3: popups, menus, prompts).
pub trait Overlay {
    fn draw(&self, screen: &mut Screen);
}

/// Composes one client's frame.
// T2.2: stub until the renderer lands.
pub fn compose(inputs: &FrameInputs) -> Screen {
    Screen::new(inputs.client.0, inputs.client.1)
}

/// The escape stream that turns `prev` into `next` on a terminal with
/// `caps`: colours downgraded to what it supports, wrapped in synchronized
/// output when it has it. `prev = None` is a full redraw (first frame, or
/// after a dropped frame).
// T2.2: stub until the renderer lands.
pub fn diff(prev: Option<&Screen>, next: &Screen, caps: &Caps) -> Vec<u8> {
    let _ = (prev, next, caps);
    Vec::new()
}
