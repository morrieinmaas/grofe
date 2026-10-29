//! Copy mode (spec §4.11, TM.1): a per-pane mode over a copy of the pane's
//! history and screen, taken on entry as tmux 3.7b's `window-copy.c` takes
//! one, so output arriving meanwhile moves neither the view nor the text
//! (`refresh-from-pane` takes a new copy). Its keys go through the
//! `copy-mode-vi` table ([`CopyHook`]), whose commands are `send-keys -X
//! <command>` ([`run`]), and it is drawn over the pane with the
//! `[offset/size]` indicator in `mode-style` ([`CopyOverlay`]).
//!
//! The motions, scrolling and paging are ports of tmux 3.7b's
//! `window-copy.c` and `grid-reader.c`, quirks included (the column a
//! vertical move remembers, `-e` leaving at the bottom, the paging maths),
//! checked against the `tm_1` goldens. Coordinates are tmux's: `cx`, `cy`
//! on the view, `oy` lines scrolled up, and a row `py` of the copy counted
//! from the oldest history line ([`Mode::py`]).
//!
//! Selection is TM.2's ([`select`]), search and `#{copy_cursor_word}`
//! TM.3's ([`search`]), copy targets TM.5's ([`pipe`]): the motions call
//! their hooks where tmux calls its own.
// ponytail: tabs are one cell each in the copy (tmux keeps a tab as one
// wide cell), and a resize takes a fresh copy instead of reflowing the old
// one; `copy-mode-position-format`'s line time is not drawn.

pub mod pipe;
pub mod search;
pub mod select;
pub mod view;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::cmd;
use crate::protocol::Dialect;
use crate::server::dispatch::options;
use crate::server::imgstore::PaneImages;
use crate::server::input::{Input, InputHook, ROOT_TABLE, Routed};
use crate::server::model::Window;
use crate::server::pane::{CellFlags, CursorShape, CursorState, PaneHandle, SnapCell, marks};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::screenimg::{self, ImageCaps, Place};
use crate::server::status::{self, Style};
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId, Rect};

/// `#{pane_mode}` while a pane is in copy mode.
pub const MODE_NAME: &str = "copy-mode";
/// `#{pane_mode}` while it shows command output.
pub const VIEW_MODE_NAME: &str = "view-mode";
/// The key table copy mode uses with `mode-keys vi`.
pub const VI_TABLE: &str = "copy-mode-vi";
/// The key table with `mode-keys emacs` (not shipped, spec §4.11).
pub const EMACS_TABLE: &str = "copy-mode";

/// tmux's `WHITESPACE`.
const WHITESPACE: &str = "\t ";
/// `copy-mode-mark-style`'s default.
const MARK_STYLE: &str = "bg=red,fg=black";
/// `mode-style`'s default, if the option is missing.
const MODE_STYLE: &str = "noattr,bg=yellow,fg=black";

/// The most repeats a count runs (`send-keys -N`, a mode's prefix).
// ponytail: tmux 3.7b runs every one, and for `-N 4294967295` stops
// answering (recorded); grofe runs this many.
pub const MAX_REPEAT: u32 = 100_000;

/// The commands whose string `send-keys -F` expands as a format (tmux's
/// `window_copy_expand_search_string`).
pub const FORMAT_COMMANDS: &[&str] = &[
    "search-backward",
    "search-backward-text",
    "search-forward",
    "search-forward-text",
];

/// The panes in copy mode. Lives on `State.copy`.
#[derive(Default)]
pub struct Copies {
    modes: BTreeMap<PaneId, Mode>,
    /// Each pane's last search, for its next copy mode (TM.3).
    searches: BTreeMap<PaneId, search::Remembered>,
}

impl Copies {
    pub fn get(&self, pane: PaneId) -> Option<&Mode> {
        self.modes.get(&pane)
    }

    pub fn get_mut(&mut self, pane: PaneId) -> Option<&mut Mode> {
        self.modes.get_mut(&pane)
    }

    pub fn is_active(&self, pane: PaneId) -> bool {
        self.modes.contains_key(&pane)
    }

    pub fn insert(&mut self, pane: PaneId, mode: Mode) {
        self.modes.insert(pane, mode);
    }

    /// Leaves copy mode in `pane`; whether it was in it.
    pub fn remove(&mut self, pane: PaneId) -> bool {
        let Some(mut mode) = self.modes.remove(&pane) else {
            return false;
        };
        if let Some(last) = search::remember(&mode) {
            self.searches.insert(pane, last);
        }
        // A view mode opened over copy mode gives the pane back to it.
        if let Some(under) = mode.under.take() {
            self.modes.insert(pane, *under);
        }
        true
    }

    /// Drops the modes of panes that are gone.
    pub fn retain(&mut self, live: impl Fn(PaneId) -> bool) {
        self.modes.retain(|p, _| live(*p));
        self.searches.retain(|p, _| live(*p));
    }
}

// ---- The copy of the pane ------------------------------------------------

/// One cell of the copy: its character and how it looks, an index into
/// [`Grid::looks`]. A tab is `\t`, and each cell after it up to its stop
/// is [`TAB_PAD`] (tmux's `GRID_FLAG_TAB` cell and its padding).
#[derive(Debug, Clone, Copy)]
struct Cell {
    c: char,
    look: u32,
}

/// A cell an intact tab covers.
const TAB_PAD: char = '\0';

#[derive(Debug, Default, Clone)]
struct Row {
    /// Trailing default blanks are dropped.
    cells: Vec<Cell>,
    /// The row runs on into the next (tmux's `GRID_LINE_WRAPPED`).
    wrapped: bool,
    /// The OSC 133 marks of the row ([`marks`]: `GRID_LINE_START_PROMPT`
    /// and `GRID_LINE_START_OUTPUT`).
    mark: u8,
    /// When the row went into the history (tmux's `grid_line.time`), in
    /// seconds; only a view mode's rows have one.
    time: u64,
}

/// A copy of a pane's history and screen (tmux's backing screen): `hsize`
/// history rows, then `sy` screen rows, `sx` columns.
#[derive(Debug, Clone)]
pub struct Grid {
    pub sx: usize,
    pub hsize: usize,
    pub sy: usize,
    rows: Vec<Row>,
    /// Each distinct look, with `c` unused; 0 is the default blank.
    looks: Vec<SnapCell>,
    /// Interns [`Grid::looks`].
    look_ids: HashMap<LookKey, u32>,
    /// The pane's kitty placements in the copy's rows (G3).
    images: Option<PaneImages>,
}

/// A look as a hashable key (the colours as numbers).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LookKey {
    fg: u32,
    bg: u32,
    flags: u16,
    underline: Option<u32>,
    zerowidth: Vec<char>,
    hyperlink: Option<crate::server::pane::Hyperlink>,
}

fn colour_key(c: Color) -> u32 {
    match c {
        Color::Named(n) => 0x0100_0000 | n as u32,
        Color::Indexed(i) => 0x0200_0000 | u32::from(i),
        Color::Spec(rgb) => {
            0x0300_0000 | u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
        }
    }
}

fn look_key(cell: &SnapCell) -> LookKey {
    LookKey {
        fg: colour_key(cell.fg),
        bg: colour_key(cell.bg),
        flags: cell.flags.bits(),
        underline: cell.underline_color.map(colour_key),
        zerowidth: cell.zerowidth.clone(),
        hyperlink: cell.hyperlink.clone(),
    }
}

/// What tmux's `grid_get_cell` tells a motion about one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridCell {
    pub c: char,
    /// The second half of a wide character (`GRID_FLAG_PADDING`).
    pub padding: bool,
    /// One byte of UTF-8 (`gc.data.size == 1`).
    pub single: bool,
    /// A tab's width (tmux's `GRID_FLAG_TAB`), 0 for any other cell.
    pub tab: usize,
}

fn blank() -> SnapCell {
    SnapCell {
        c: ' ',
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: CellFlags::empty(),
        zerowidth: Vec::new(),
        underline_color: None,
        hyperlink: None,
    }
}

impl Grid {
    fn empty(sx: usize, hsize: usize, sy: usize) -> Grid {
        let mut look_ids = HashMap::new();
        look_ids.insert(look_key(&blank()), 0);
        Grid {
            sx,
            hsize,
            sy,
            rows: Vec::new(),
            looks: vec![blank()],
            look_ids,
            images: None,
        }
    }

    /// Copies `handle`'s grid; also returns the cursor (column, row on the
    /// screen).
    pub fn from_pane(handle: &PaneHandle) -> (Grid, (usize, usize)) {
        let mut grid = Grid::empty(0, 0, 0);
        let (hsize, cursor) = handle.copy_grid(|cells, tabs, mark| {
            grid.push_row(cells, tabs);
            if let Some(row) = grid.rows.last_mut() {
                row.mark = mark;
            }
        });
        grid.rows.shrink_to_fit();
        grid.looks.shrink_to_fit();
        grid.hsize = hsize.min(grid.rows.len());
        grid.sy = grid.rows.len() - grid.hsize;
        grid.images = handle.copy_images();
        (grid, (usize::from(cursor.col), usize::from(cursor.row)))
    }

    /// This copy as another pane's mode shows it (`copy-mode -s`, tmux's
    /// `window_copy_clone_screen` with `trim`): the screen's trailing blank
    /// lines go, then it is made `rows` high, blank lines added below or
    /// the top ones pushed into the history.
    fn fit_to(&mut self, rows: usize) {
        while self.rows.len() > self.hsize && self.line_length(self.rows.len() - 1) == 0 {
            self.rows.pop();
        }
        self.sy = self.rows.len() - self.hsize;
        while self.sy < rows {
            let mut blanks = vec![blank(); self.sx];
            self.push_row(&mut blanks, &vec![false; self.sx]);
            self.sy += 1;
        }
        if self.sy > rows {
            self.hsize += self.sy - rows;
            self.sy = rows;
        }
    }

    /// A grid of plain text rows, for tests: `hsize` of them history.
    #[cfg(test)]
    pub fn from_text(sx: usize, hsize: usize, rows: &[&str]) -> Grid {
        let mut grid = Grid::empty(sx, hsize, rows.len() - hsize);
        for text in rows {
            // A tab runs to the next stop of 8, as a terminal moves it.
            let mut cells: Vec<SnapCell> = Vec::new();
            let mut tabs = Vec::new();
            for c in text.chars() {
                cells.push(SnapCell { c, ..blank() });
                tabs.push(false);
                if c == '\t' {
                    while !cells.len().is_multiple_of(8) {
                        cells.push(blank());
                        tabs.push(true);
                    }
                }
            }
            cells.resize(sx, blank());
            tabs.resize(sx, false);
            grid.push_row(&mut cells, &tabs);
        }
        grid
    }

    /// The copy's heap size, roughly: its cells, rows and looks.
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let cells: usize = self.rows.iter().map(|r| r.cells.capacity()).sum();
        cells * size_of::<Cell>()
            + self.rows.capacity() * size_of::<Row>()
            + self.looks.capacity() * size_of::<SnapCell>()
            + self.look_ids.capacity() * (size_of::<LookKey>() + size_of::<u32>())
    }

    fn intern(&mut self, cell: &SnapCell) -> u32 {
        let key = look_key(cell);
        if let Some(&id) = self.look_ids.get(&key) {
            return id;
        }
        let id = self.looks.len() as u32;
        self.looks.push(cell.clone());
        self.look_ids.insert(key, id);
        id
    }

    /// Takes row `cells` (their characters and wrap flags are cleared).
    fn push_row(&mut self, cells: &mut [SnapCell], tabs: &[bool]) {
        self.sx = self.sx.max(cells.len());
        let wrapped = cells
            .last()
            .is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE));
        let mut row = Row {
            cells: Vec::with_capacity(cells.len()),
            wrapped,
            mark: 0,
            time: 0,
        };
        // The cell with the latest interned look, and that look.
        let mut last: Option<(usize, u32)> = None;
        for i in 0..cells.len() {
            let covered = tabs.get(i).copied().unwrap_or(false);
            let c = match cells[i].c {
                _ if covered => TAB_PAD,
                // A tab written over before its stop is a blank, as
                // `capture-pane` reads it; one a cell wide covers nothing.
                '\t' if !tabs.get(i + 1).copied().unwrap_or(false)
                    && i + 1 < tabs.len()
                    && !(i + 1).is_multiple_of(8) =>
                {
                    ' '
                }
                c => c,
            };
            cells[i].flags.remove(CellFlags::WRAPLINE);
            cells[i].c = ' ';
            let look = match last {
                Some((prev, id)) if cells[prev] == cells[i] => id,
                _ => {
                    let id = self.intern(&cells[i]);
                    last = Some((i, id));
                    id
                }
            };
            row.cells.push(Cell { c, look });
        }
        while row.cells.last().is_some_and(|c| c.c == ' ' && c.look == 0) {
            row.cells.pop();
        }
        row.cells.shrink_to_fit();
        self.rows.push(row);
    }

    /// Every row: history then screen.
    pub fn total(&self) -> usize {
        self.hsize + self.sy
    }

    fn look(&self, x: usize, y: usize) -> Option<(char, &SnapCell)> {
        let cell = self.rows.get(y)?.cells.get(x)?;
        Some((cell.c, &self.looks[cell.look as usize]))
    }

    /// A tab's width: itself and the padding after it.
    fn tab_width(&self, x: usize, y: usize) -> usize {
        let cells = self
            .rows
            .get(y)
            .map(|r| r.cells.as_slice())
            .unwrap_or_default();
        1 + cells
            .iter()
            .skip(x + 1)
            .take_while(|c| c.c == TAB_PAD)
            .count()
    }

    /// tmux's `grid_get_cell`: past the end of a row, a blank.
    pub fn get(&self, x: usize, y: usize) -> GridCell {
        match self.look(x, y) {
            Some((TAB_PAD, _)) => GridCell {
                c: ' ',
                padding: true,
                single: true,
                tab: 0,
            },
            Some(('\t', _)) => {
                let tab = self.tab_width(x, y);
                GridCell {
                    c: if tab == 1 { ' ' } else { '\t' },
                    padding: false,
                    single: tab == 1,
                    tab,
                }
            }
            Some((c, look)) => {
                let padding = look.flags.contains(CellFlags::WIDE_CHAR_SPACER);
                let c = if look.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER) {
                    ' '
                } else {
                    c
                };
                GridCell {
                    c,
                    padding,
                    single: c.is_ascii() && look.zerowidth.is_empty(),
                    tab: 0,
                }
            }
            None => GridCell {
                c: ' ',
                padding: false,
                single: true,
                tab: 0,
            },
        }
    }

    /// The cell as drawn: a tab and its padding are blanks.
    pub fn snap(&self, x: usize, y: usize) -> SnapCell {
        match self.look(x, y) {
            Some((TAB_PAD | '\t', look)) => SnapCell {
                c: ' ',
                ..look.clone()
            },
            Some((c, look)) => SnapCell { c, ..look.clone() },
            None => blank(),
        }
    }

    /// tmux's `grid_line_length`: the row's width without trailing blanks.
    pub fn line_length(&self, y: usize) -> usize {
        let Some(row) = self.rows.get(y) else {
            return 0;
        };
        let mut px = row.cells.len().min(self.sx);
        while px > 0 {
            let gc = self.get(px - 1, y);
            if gc.padding || !gc.single || gc.c != ' ' {
                break;
            }
            px -= 1;
        }
        px
    }

    pub fn wrapped(&self, y: usize) -> bool {
        self.rows.get(y).is_some_and(|r| r.wrapped)
    }

    /// tmux's `grid_in_set`: 1 when the cell's character is in `set`, 0
    /// for padding or a character outside it. With a tab in `set`, a tab
    /// counts as its width, and its padding as the width left.
    pub fn in_set(&self, x: usize, y: usize, set: &str) -> usize {
        let gc = self.get(x, y);
        if set.contains('\t') {
            if gc.padding && self.look(x, y).is_some_and(|(c, _)| c == TAB_PAD) {
                let mut start = x;
                while start > 0 && self.get(start, y).padding {
                    start -= 1;
                }
                let tab = self.get(start, y).tab;
                if tab > 0 {
                    return tab - (x - start);
                }
            } else if gc.tab > 0 {
                return gc.tab;
            }
        }
        if gc.padding {
            return 0;
        }
        usize::from(set.contains(gc.c))
    }

    /// tmux's `format_grid_word`: the word around `(x, y)`, between
    /// `separators`, blanks and tabs, joined across wraps.
    pub fn word(&self, mut x: usize, mut y: usize, separators: &str) -> String {
        let is_sep = |gc: &GridCell| separators.contains(gc.c) || gc.tab > 0 || gc.c == ' ';
        let mut found = false;
        loop {
            let gc = self.get(x, y);
            if !gc.padding && is_sep(&gc) {
                found = true;
                break;
            }
            if x == 0 {
                if y == 0 || !self.wrapped(y - 1) {
                    break;
                }
                y -= 1;
                x = self.line_length(y);
                if x == 0 {
                    break;
                }
            }
            x -= 1;
        }
        let mut out = String::new();
        loop {
            if found {
                let end = self.line_length(y);
                if end == 0 || x == end - 1 {
                    if y == self.total() - 1 || !self.wrapped(y) {
                        break;
                    }
                    y += 1;
                    x = 0;
                } else {
                    x += 1;
                }
            }
            found = true;
            let gc = self.get(x, y);
            if gc.padding {
                continue;
            }
            if is_sep(&gc) {
                break;
            }
            out.push(gc.c);
            if let Some((_, look)) = self.look(x, y) {
                out.extend(&look.zerowidth);
            }
        }
        out
    }
}

impl Grid {
    /// tmux's `format_grid_line`: row `y` up to its used width, a tab as
    /// itself.
    pub fn line_text(&self, y: usize) -> String {
        let mut out = String::new();
        for x in 0..self.line_length(y) {
            let gc = self.get(x, y);
            if gc.padding {
                continue;
            }
            if gc.tab > 0 {
                out.push('\t');
                continue;
            }
            out.push(gc.c);
            if let Some((_, look)) = self.look(x, y) {
                out.extend(&look.zerowidth);
            }
        }
        out
    }

    /// tmux's `format_grid_hyperlink`: the target of the link at `(x, y)`,
    /// looking left over the second half of a wide character.
    pub fn hyperlink_at(&self, mut x: usize, y: usize) -> Option<String> {
        while self.get(x, y).padding {
            x = x.checked_sub(1)?;
        }
        let (_, look) = self.look(x, y)?;
        look.hyperlink.as_ref().map(|h| h.uri().to_string())
    }
}

/// `#{mouse_word}`, `#{mouse_line}` and `#{mouse_hyperlink}` for the cell
/// `(x, y)` of `pane` (tmux's `format_cb_mouse_*`): from the pane's screen,
/// or from the copy view while it is in copy mode. `None` in any other mode,
/// as tmux gives nothing then.
pub fn mouse_text(state: &State, pane: PaneId, x: u16, y: u16, name: &str) -> Option<String> {
    let (x, y) = (usize::from(x), usize::from(y));
    let separators = state
        .model
        .options
        .lookup("word-separators", &[])
        .unwrap_or_default();
    let read = |grid: &Grid, row: usize| match name {
        "mouse_word" => Some(grid.word(x, row, separators)),
        "mouse_line" => Some(grid.line_text(row)),
        _ => grid.hyperlink_at(x, row),
    };
    if let Some(mode) = state.copy.get(pane) {
        return read(&mode.grid, (mode.grid.hsize + y).checked_sub(mode.oy)?);
    }
    if crate::server::tree::format_var(state, pane, "pane_in_mode").is_some() {
        return None;
    }
    let (grid, _) = Grid::from_pane(&state.model.panes.get(&pane)?.handle);
    read(&grid, grid.hsize + y)
}

// ---- tmux's grid reader --------------------------------------------------

/// A cursor moving over the copy (tmux's `struct grid_reader`).
struct Reader<'a> {
    gd: &'a Grid,
    cx: usize,
    cy: usize,
}

impl Reader<'_> {
    fn line_length(&self) -> usize {
        self.gd.line_length(self.cy)
    }

    fn last_row(&self) -> usize {
        self.gd.total() - 1
    }

    fn padding(&self) -> bool {
        self.gd.get(self.cx, self.cy).padding
    }

    fn in_set(&self, set: &str) -> usize {
        self.gd.in_set(self.cx, self.cy, set)
    }

    fn cursor_right(&mut self, wrap: bool, all: bool, onemore: bool) {
        let px = if all {
            self.gd.sx
        } else if onemore {
            self.line_length()
        } else {
            self.line_length().saturating_sub(1)
        };
        if wrap && self.cx >= px && self.cy < self.last_row() {
            self.cursor_start_of_line(false);
            self.cursor_down();
        } else if self.cx < px {
            self.cx += 1;
            while self.cx < px && self.padding() {
                self.cx += 1;
            }
        }
    }

    fn cursor_left(&mut self, wrap: bool) {
        while self.cx > 0 && self.padding() {
            self.cx -= 1;
        }
        if self.cx == 0 && self.cy > 0 && (wrap || self.gd.wrapped(self.cy - 1)) {
            self.cursor_up();
            self.cursor_end_of_line(false, false);
        } else if self.cx > 0 {
            self.cx -= 1;
        }
    }

    fn cursor_down(&mut self) {
        if self.cy < self.last_row() {
            self.cy += 1;
        }
        while self.cx > 0 && self.padding() {
            self.cx -= 1;
        }
    }

    fn cursor_up(&mut self) {
        if self.cy > 0 {
            self.cy -= 1;
        }
        while self.cx > 0 && self.padding() {
            self.cx -= 1;
        }
    }

    fn cursor_start_of_line(&mut self, wrap: bool) {
        if wrap {
            while self.cy > 0 && self.gd.wrapped(self.cy - 1) {
                self.cy -= 1;
            }
        }
        self.cx = 0;
    }

    fn cursor_end_of_line(&mut self, wrap: bool, all: bool) {
        if wrap {
            let yy = self.last_row();
            while self.cy < yy && self.gd.wrapped(self.cy) {
                self.cy += 1;
            }
        }
        self.cx = if all { self.gd.sx } else { self.line_length() };
    }

    /// tmux's `grid_reader_handle_wrap`: false when the cursor would wrap
    /// past the bottom.
    fn handle_wrap(&mut self, xx: &mut usize, yy: usize) -> bool {
        while self.cx > *xx {
            if self.cy == yy {
                return false;
            }
            self.cursor_start_of_line(false);
            self.cursor_down();
            *xx = if self.gd.wrapped(self.cy) {
                self.gd.sx - 1
            } else {
                self.line_length()
            };
        }
        true
    }

    fn wrap_limit(&self) -> usize {
        if self.gd.wrapped(self.cy) {
            self.gd.sx - 1
        } else {
            self.line_length()
        }
    }

    fn cursor_next_word(&mut self, separators: &str) {
        let mut xx = self.wrap_limit();
        let yy = self.last_row();
        if !self.handle_wrap(&mut xx, yy) {
            return;
        }
        if self.in_set(WHITESPACE) == 0 {
            if self.in_set(separators) != 0 {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx, yy)
                        && self.in_set(separators) != 0
                        && self.in_set(WHITESPACE) == 0)
                    {
                        break;
                    }
                }
            } else {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx, yy)
                        && !(self.in_set(separators) != 0 || self.in_set(WHITESPACE) != 0))
                    {
                        break;
                    }
                }
            }
        }
        while self.handle_wrap(&mut xx, yy) {
            let width = self.in_set(WHITESPACE);
            if width == 0 {
                break;
            }
            self.cx += width;
        }
    }

    fn cursor_next_word_end(&mut self, separators: &str) {
        let mut xx = self.wrap_limit();
        let yy = self.last_row();
        while self.handle_wrap(&mut xx, yy) {
            if self.in_set(WHITESPACE) != 0 {
                self.cx += 1;
            } else if self.in_set(separators) != 0 {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx, yy)
                        && self.in_set(separators) != 0
                        && self.in_set(WHITESPACE) == 0)
                    {
                        break;
                    }
                }
                return;
            } else {
                loop {
                    self.cx += 1;
                    if !(self.handle_wrap(&mut xx, yy)
                        && !(self.in_set(WHITESPACE) != 0 || self.in_set(separators) != 0))
                    {
                        break;
                    }
                }
                return;
            }
        }
    }

    fn cursor_previous_word(&mut self, separators: &str, already: bool, stop_at_eol: bool) {
        let word_is_letters;
        if already || self.in_set(WHITESPACE) != 0 {
            loop {
                if self.cx > 0 {
                    self.cx -= 1;
                    if self.in_set(WHITESPACE) == 0 {
                        word_is_letters = self.in_set(separators) == 0;
                        break;
                    }
                } else {
                    if self.cy == 0 {
                        return;
                    }
                    self.cursor_up();
                    self.cursor_end_of_line(false, false);
                    if stop_at_eol && self.cx > 0 {
                        let oldx = self.cx;
                        self.cx -= 1;
                        let at_eol = self.in_set(WHITESPACE) != 0;
                        self.cx = oldx;
                        if at_eol {
                            word_is_letters = false;
                            break;
                        }
                    }
                }
            }
        } else {
            word_is_letters = self.in_set(separators) == 0;
        }
        let (mut oldx, mut oldy);
        loop {
            oldx = self.cx;
            oldy = self.cy;
            if self.cx == 0 {
                if self.cy == 0 || !self.gd.wrapped(self.cy - 1) {
                    break;
                }
                self.cursor_up();
                self.cursor_end_of_line(false, true);
            }
            if self.cx > 0 {
                self.cx -= 1;
            }
            if !(self.in_set(WHITESPACE) == 0 && word_is_letters != (self.in_set(separators) != 0))
            {
                break;
            }
        }
        self.cx = oldx;
        self.cy = oldy;
    }

    fn cursor_back_to_indentation(&mut self) {
        let yy = self.last_row();
        let (oldx, oldy) = (self.cx, self.cy);
        self.cursor_start_of_line(true);
        for py in self.cy..=yy {
            let xx = self.gd.line_length(py);
            for px in 0..xx {
                let gc = self.gd.get(px, py);
                if !(gc.single && gc.c == ' ') && gc.tab == 0 && !gc.padding {
                    self.cx = px;
                    self.cy = py;
                    return;
                }
            }
            if !self.gd.wrapped(py) {
                break;
            }
        }
        self.cx = oldx;
        self.cy = oldy;
    }
}

// ---- The mode ------------------------------------------------------------

/// `lineflag`: which way a line selection (TM.2's `select-line`) runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineSel {
    #[default]
    None,
    LeftRight,
    RightLeft,
}

/// One pane's copy mode (tmux's `window_copy_mode_data`).
#[derive(Debug)]
pub struct Mode {
    /// The copy of the pane, taken on entry (`backing`); shared with a
    /// search running off the actor.
    pub grid: Arc<Grid>,
    /// The cursor on the view.
    pub cx: usize,
    pub cy: usize,
    /// Lines scrolled up from the bottom (`#{scroll_position}`).
    pub oy: usize,
    /// The column a vertical move keeps, and the length of the line it
    /// was on (`lastcx`, `lastsx`).
    lastcx: usize,
    lastsx: usize,
    /// The mark (`set-mark`), as a column and a row of the copy.
    pub mx: usize,
    pub my: usize,
    pub showmark: bool,
    /// `-e`: scrolling to the bottom leaves the mode.
    pub scroll_exit: bool,
    /// `-s`: the pane whose screen this copy was taken from, when it is not
    /// the pane in the mode; a refresh reads that one again.
    pub source: Option<PaneId>,
    /// `toggle-position`: the indicator is hidden.
    pub hide_position: bool,
    /// `recentre-top-bottom`'s next stop, and the row of the copy it was
    /// last used on (`recentre_state`, `recentre_line`).
    recentre: (Recentre, usize),
    /// The count the next command runs with (`send-keys -N`, `wme->prefix`).
    pub prefix: u32,
    /// `mode-keys` is `vi`, read before each command.
    pub vi: bool,
    /// A selection exists (`screen.sel`, TM.2).
    pub selecting: bool,
    /// Rectangle selection (`rectflag`, TM.2).
    pub rect: bool,
    /// `lineflag` (TM.2).
    pub line_sel: LineSel,
    /// The selection's start and end, as a column and a row of the copy
    /// (`selx`, `sely`, `endselx`, `endsely`, TM.2).
    pub sel_start: (usize, usize),
    pub sel_end: (usize, usize),
    /// The rest of the selection's state (TM.2).
    pub sel: select::Sel,
    /// The last search's string (tmux's `searchstr`), as `send-keys -F`
    /// expanded it; TM.3 searches for it.
    pub search_str: Option<String>,
    /// The search's and the jumps' own state (TM.3).
    pub search: search::Search,
    /// A view mode (tmux's `window_view_mode`): the text is a command's
    /// output, not the pane's.
    pub view: bool,
    /// The copy mode a view mode was opened over, which `q` gives back.
    pub under: Option<Box<Mode>>,
    /// The pane was showing the clock when this view mode opened over it,
    /// so the mode is on top and the clock gets the keys after it.
    pub over_clock: bool,
}

/// Where `recentre-top-bottom` puts the cursor's line next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recentre {
    Middle,
    Top,
    Bottom,
}

/// What a command asks of the mode afterwards (tmux's
/// `window_copy_cmd_action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Nothing,
    Redraw,
    /// Leave copy mode.
    Cancel,
}

/// What a command reads from outside the mode.
pub struct Ctx<'a> {
    /// The session's `word-separators`.
    pub separators: &'a str,
}

impl Mode {
    /// Copy mode over `grid` with the cursor at screen cell `cursor`, as
    /// tmux's `window_copy_init` starts it.
    pub fn new(grid: Grid, cursor: (usize, usize), vi: bool) -> Mode {
        let (cx, row) = cursor;
        let cy = row.min(grid.sy.saturating_sub(1));
        let my = grid.hsize + cy;
        Mode {
            grid: Arc::new(grid),
            cx,
            cy,
            oy: 0,
            lastcx: 0,
            lastsx: 0,
            mx: cx,
            my,
            showmark: false,
            scroll_exit: false,
            source: None,
            hide_position: false,
            recentre: (Recentre::Middle, 0),
            prefix: 1,
            vi,
            selecting: false,
            rect: false,
            line_sel: LineSel::None,
            sel_start: (0, 0),
            sel_end: (0, 0),
            sel: select::Sel::default(),
            search_str: None,
            search: search::Search::default(),
            view: false,
            under: None,
            over_clock: false,
        }
    }

    /// The copy's row under the cursor.
    pub fn py(&self) -> usize {
        self.grid.hsize + self.cy - self.oy
    }

    /// The view's rows (`screen_size_y(&data->screen)`).
    fn sy(&self) -> usize {
        self.grid.sy
    }

    fn norectsel(&self) -> bool {
        !self.selecting || !self.rect
    }

    fn find_length(&self, py: usize) -> usize {
        self.grid.line_length(py)
    }

    fn cursor_limit(&self, py: usize, allow_onemore: bool) -> usize {
        let len = self.find_length(py);
        if allow_onemore || !self.vi {
            return len;
        }
        len.saturating_sub(1)
    }

    fn update_selection(&mut self) {
        select::update_selection(self);
    }

    fn reader(&self, cx: usize, cy: usize) -> Reader<'_> {
        Reader {
            gd: &self.grid,
            cx,
            cy,
        }
    }

    fn update_cursor(&mut self, mut cx: usize, cy: usize) {
        if !self.rect && cy < self.sy() {
            let allow_onemore = self.selecting && self.rect;
            let py = self.grid.hsize + cy - self.oy;
            let maxx = self.cursor_limit(py, allow_onemore);
            if cx > maxx {
                cx = maxx;
            }
        }
        self.cx = cx;
        self.cy = cy;
    }

    /// Moves the view down `ny` lines (tmux's `window_copy_scroll_up`).
    fn scroll_up(&mut self, ny: usize) {
        let ny = ny.min(self.oy);
        if ny == 0 {
            return;
        }
        self.oy -= ny;
        self.update_selection();
    }

    /// Moves the view up `ny` lines (tmux's `window_copy_scroll_down`).
    fn scroll_down(&mut self, ny: usize) {
        let hsize = self.grid.hsize;
        if ny > hsize {
            return;
        }
        let ny = if self.oy > hsize - ny {
            hsize - self.oy
        } else {
            ny
        };
        if ny == 0 {
            return;
        }
        self.oy += ny;
        self.update_selection();
    }

    /// The vertical moves' column: remembered unless the cursor is at the
    /// end of its line.
    fn remember_column(&mut self) {
        let ox = self.find_length(self.py());
        if self.norectsel() && self.cx != ox {
            self.lastcx = self.cx;
            self.lastsx = ox;
        }
    }

    /// After a vertical move, to the end of the new line if the column
    /// came from one, or is past it.
    fn stick_to_end(&mut self) {
        if self.norectsel() {
            let px = self.find_length(self.py());
            if (self.cx >= self.lastsx && self.cx != px) || self.cx > px {
                self.update_cursor(px, self.cy);
                self.update_selection();
            }
        }
    }

    fn follow_line_selection(&mut self) {
        match self.line_sel {
            LineSel::LeftRight => {
                let py = self.py();
                let px = if self.rect {
                    self.grid.sx
                } else {
                    self.find_length(py)
                };
                self.update_cursor(px, self.cy);
                self.update_selection();
            }
            LineSel::RightLeft => {
                self.update_cursor(0, self.cy);
                self.update_selection();
            }
            LineSel::None => {}
        }
    }

    fn cursor_up(&mut self, scroll_only: bool) {
        let norectsel = self.norectsel();
        let oy = self.py();
        self.remember_column();
        if self.line_sel == LineSel::LeftRight && oy == self.sel_start.1 {
            select::other_end(self);
        }
        if scroll_only || self.cy == 0 {
            if norectsel {
                self.cx = self.lastcx;
            }
            self.scroll_down(1);
        } else {
            let cx = if norectsel { self.lastcx } else { self.cx };
            self.update_cursor(cx, self.cy - 1);
            self.update_selection();
        }
        self.stick_to_end();
        self.follow_line_selection();
    }

    fn cursor_down(&mut self, scroll_only: bool) {
        let norectsel = self.norectsel();
        let oy = self.py();
        self.remember_column();
        if self.line_sel == LineSel::RightLeft && oy == self.sel_end.1 {
            select::other_end(self);
        }
        if scroll_only || self.cy == self.sy() - 1 {
            if norectsel {
                self.cx = self.lastcx;
            }
            self.scroll_up(1);
        } else {
            let cx = if norectsel { self.lastcx } else { self.cx };
            self.update_cursor(cx, self.cy + 1);
            self.update_selection();
        }
        self.stick_to_end();
        self.follow_line_selection();
    }

    /// Scrolls up if the cursor went off the top of the view.
    fn acquire_cursor_up(&mut self, hsize: usize, oy: usize, px: usize, py: usize) {
        let yy = hsize - oy;
        let (ny, cy) = if py < yy { (yy - py, 0) } else { (0, py - yy) };
        for _ in 0..ny {
            self.cursor_up(true);
        }
        self.update_cursor(px, cy);
        self.update_selection();
    }

    /// Scrolls down if the cursor went off the bottom of the view.
    fn acquire_cursor_down(&mut self, hsize: usize, oy: usize, px: usize, py: usize) {
        let cy = py + oy - hsize;
        let yy = self.sy() - 1;
        let ny = cy.saturating_sub(yy);
        for _ in 0..ny {
            self.cursor_down(true);
        }
        self.update_cursor(px, cy.min(yy));
        self.update_selection();
    }

    /// Runs `f` on a reader at the cursor, then moves the cursor where it
    /// ended, scrolling up or down to it.
    fn read_up(&mut self, f: impl FnOnce(&mut Reader<'_>)) {
        let (hsize, oy) = (self.grid.hsize, self.oy);
        let mut r = self.reader(self.cx, self.py());
        f(&mut r);
        let (px, py) = (r.cx, r.cy);
        self.acquire_cursor_up(hsize, oy, px, py);
    }

    fn read_down(&mut self, f: impl FnOnce(&mut Reader<'_>)) {
        let (hsize, oy) = (self.grid.hsize, self.oy);
        let mut r = self.reader(self.cx, self.py());
        f(&mut r);
        let (px, py) = (r.cx, r.cy);
        self.acquire_cursor_down(hsize, oy, px, py);
    }

    fn cursor_left(&mut self) {
        self.read_up(|r| r.cursor_left(true));
    }

    fn cursor_right(&mut self, all: bool) {
        let onemore = !self.vi;
        self.read_down(|r| r.cursor_right(true, all, onemore));
    }

    fn cursor_start_of_line(&mut self) {
        self.read_up(|r| r.cursor_start_of_line(true));
    }

    fn cursor_back_to_indentation(&mut self) {
        self.read_up(|r| r.cursor_back_to_indentation());
    }

    fn cursor_end_of_line(&mut self) {
        let (hsize, oy) = (self.grid.hsize, self.oy);
        let all = self.selecting && self.rect;
        let mut r = self.reader(self.cx, self.py());
        r.cursor_end_of_line(true, all);
        let (mut px, py) = (r.cx, r.cy);
        if self.norectsel() {
            px = self.cursor_limit(py, false);
        }
        self.acquire_cursor_down(hsize, oy, px, py);
    }

    fn cursor_next_word(&mut self, separators: &str) {
        self.read_down(|r| r.cursor_next_word(separators));
    }

    fn cursor_next_word_end(&mut self, separators: &str) {
        let vi = self.vi;
        self.read_down(|r| {
            if vi {
                if r.in_set(WHITESPACE) == 0 {
                    r.cursor_right(false, false, false);
                }
                r.cursor_next_word_end(separators);
                r.cursor_left(true);
            } else {
                r.cursor_next_word_end(separators);
            }
        });
    }

    fn cursor_previous_word(&mut self, separators: &str, already: bool) {
        let stop_at_eol = !self.vi;
        self.read_up(|r| r.cursor_previous_word(separators, already, stop_at_eol));
    }

    fn pageup(&mut self, half: bool) {
        self.remember_column_always();
        let n = self.page(half);
        let hsize = self.grid.hsize;
        if self.oy + n > hsize {
            self.oy = hsize;
            self.cy = self.cy.saturating_sub(n);
        } else {
            self.oy += n;
        }
        self.end_after_page();
        self.update_selection();
    }

    /// Whether `-e` leaves the mode.
    fn pagedown(&mut self, half: bool, scroll_exit: bool) -> bool {
        self.remember_column_always();
        let n = self.page(half);
        if self.oy < n {
            self.oy = 0;
            if self.cy + n >= self.grid.sy {
                self.cy = self.grid.sy - 1;
            } else {
                self.cy += n;
            }
        } else {
            self.oy -= n;
        }
        self.end_after_page();
        if scroll_exit && self.oy == 0 {
            return true;
        }
        self.update_selection();
        false
    }

    /// Paging's column: remembered even with a rectangle, then used.
    fn remember_column_always(&mut self) {
        let ox = self.find_length(self.py());
        if self.cx != ox {
            self.lastcx = self.cx;
            self.lastsx = ox;
        }
        self.cx = self.lastcx;
    }

    fn page(&self, half: bool) -> usize {
        let sy = self.sy();
        match (sy > 2, half) {
            (false, _) => 1,
            (true, true) => sy / 2,
            (true, false) => sy - 2,
        }
    }

    fn end_after_page(&mut self) {
        if self.norectsel() {
            let px = self.find_length(self.py());
            if (self.cx >= self.lastsx && self.cx != px) || self.cx > px {
                self.cursor_end_of_line();
            }
        }
    }

    /// Puts the cursor on copy row `py`, scrolling to it if it is off the
    /// view (tmux's `window_copy_scroll_to`).
    fn scroll_to(&mut self, px: usize, py: usize) {
        let gd = &self.grid;
        let (hsize, sy) = (gd.hsize, gd.sy);
        self.cx = px;
        if py >= hsize - self.oy && py < hsize - self.oy + sy {
            self.cy = py - (hsize - self.oy);
        } else {
            let gap = sy / 4;
            let offset;
            if py < sy {
                offset = 0;
                self.cy = py;
            } else if py > hsize + sy - gap {
                offset = hsize;
                self.cy = py - hsize;
            } else {
                offset = py + gap - sy;
                self.cy = py - offset;
            }
            self.oy = hsize - offset;
        }
        self.update_selection();
    }

    /// tmux's `window_copy_cmd_recentre_top_bottom`: the cursor's line goes
    /// to the middle, then the top, then the bottom of the view.
    fn recentre_top_bottom(&mut self) {
        let cy = self.cy;
        let sy = self.sy() - 1;
        let sm = sy / 2;
        let backing_row = self.grid.hsize + cy - self.oy;
        if self.recentre.1 != backing_row {
            self.recentre = (Recentre::Middle, backing_row);
        }
        let (next, target) = match self.recentre.0 {
            Recentre::Middle => (Recentre::Top, Recentre::Middle),
            Recentre::Top => (Recentre::Bottom, Recentre::Top),
            Recentre::Bottom => (Recentre::Middle, Recentre::Bottom),
        };
        self.recentre.0 = next;
        let oy = self.oy;
        let moved = |now: usize| (cy + now).wrapping_sub(oy);
        match target {
            Recentre::Middle => {
                if cy < sm {
                    self.scroll_down(sm - cy);
                } else if cy > sm {
                    self.scroll_up(cy - sm);
                }
                if self.oy != oy {
                    self.cy = moved(self.oy);
                }
            }
            Recentre::Top => {
                self.scroll_up(cy);
                self.cy = cy - (oy - self.oy);
            }
            Recentre::Bottom => {
                self.scroll_down(sy - cy);
                self.cy = moved(self.oy);
            }
        }
        self.update_selection();
    }

    fn scroll_cursor_to(&mut self, to: usize) {
        let up = self.cy as isize - to as isize;
        let delta = up.unsigned_abs();
        let below = self.grid.hsize - self.oy;
        if up > 0 && self.oy >= delta {
            self.scroll_up(delta);
            self.cy -= delta;
        } else if up < 0 && below >= delta {
            self.scroll_down(delta);
            self.cy += delta;
        }
        self.update_selection();
    }

    fn previous_paragraph(&mut self) {
        let mut oy = self.py();
        while oy > 0 && self.find_length(oy) == 0 {
            oy -= 1;
        }
        while oy > 0 && self.find_length(oy) > 0 {
            oy -= 1;
        }
        self.scroll_to(0, oy);
    }

    fn next_paragraph(&mut self) {
        let mut oy = self.py();
        let maxy = self.grid.hsize + self.sy() - 1;
        while oy < maxy && self.find_length(oy) == 0 {
            oy += 1;
        }
        while oy < maxy && self.find_length(oy) > 0 {
            oy += 1;
        }
        let ox = self.find_length(oy);
        self.scroll_to(ox, oy);
    }

    /// `next-prompt` and `previous-prompt` (`window_copy_cursor_prompt`):
    /// to column 0 of the next or previous row marked as a prompt start, or
    /// as an output start with `output`. Nothing moves when there is none.
    fn cursor_prompt(&mut self, down: bool, output: bool) {
        let bit = if output { marks::OUTPUT } else { marks::PROMPT };
        let end = if down { self.grid.total() - 1 } else { 0 };
        let mut line = self.py();
        loop {
            if line == end {
                return;
            }
            line = if down { line + 1 } else { line - 1 };
            if self.grid.rows[line].mark & bit != 0 {
                break;
            }
        }
        self.cx = 0;
        let hsize = self.grid.hsize;
        if line > hsize {
            self.cy = line - hsize;
            self.oy = 0;
        } else {
            self.cy = 0;
            self.oy = hsize - line;
        }
        self.update_selection();
    }

    fn history_top(&mut self) {
        let oy = self.py();
        if self.line_sel == LineSel::LeftRight && oy == self.sel_start.1 {
            select::other_end(self);
        }
        self.cy = 0;
        self.cx = 0;
        self.oy = self.grid.hsize;
        self.update_selection();
    }

    fn history_bottom(&mut self) {
        let oy = self.py();
        if self.line_sel == LineSel::RightLeft && oy == self.sel_end.1 {
            select::other_end(self);
        }
        self.cy = self.sy() - 1;
        self.cx = self.cursor_limit(self.grid.hsize + self.cy, false);
        self.oy = 0;
        self.update_selection();
    }

    fn goto_line(&mut self, line: &str) {
        let Ok(lineno) = strtonum(line, -1, i64::from(i32::MAX)) else {
            return;
        };
        let hsize = self.grid.hsize;
        self.oy = match usize::try_from(lineno) {
            Ok(n) if n <= hsize => n,
            _ => hsize,
        };
        self.update_selection();
    }

    fn set_mark(&mut self) {
        self.mx = self.cx;
        self.my = self.py();
        self.showmark = true;
    }

    fn jump_to_mark(&mut self) {
        let (tmx, tmy) = (self.cx, self.py());
        let hsize = self.grid.hsize;
        self.cx = self.mx;
        if self.my < hsize {
            self.cy = 0;
            self.oy = hsize - self.my;
        } else {
            self.cy = self.my - hsize;
            self.oy = 0;
        }
        self.mx = tmx;
        self.my = tmy;
        self.showmark = true;
        self.update_selection();
    }

    fn previous_matching_bracket(&mut self, np: u32, separators: &str) {
        const OPEN: &[u8] = b"{[(";
        const CLOSE: &[u8] = b"}])";
        for _ in 0..np {
            let (mut px, mut py) = (self.cx, self.py());
            let mut xx = self.find_length(py);
            if xx == 0 {
                break;
            }
            let mut tried = false;
            let (found, start) = loop {
                let gc = self.grid.get(px, py);
                let cp = (gc.single && !gc.padding)
                    .then(|| CLOSE.iter().position(|&b| char::from(b) == gc.c))
                    .flatten();
                match cp {
                    Some(i) => break (gc.c, char::from(OPEN[i])),
                    None if !self.vi && !tried && px > 0 => {
                        px -= 1;
                        tried = true;
                    }
                    None => {
                        if !self.vi {
                            let close = std::str::from_utf8(CLOSE).unwrap_or_default();
                            self.cursor_previous_word(close, true);
                        }
                        let _ = separators;
                        break ('\0', '\0');
                    }
                }
            };
            if found == '\0' {
                continue;
            }
            let mut n = 1;
            let mut failed = false;
            loop {
                if px == 0 {
                    if py == 0 {
                        failed = true;
                        break;
                    }
                    loop {
                        py -= 1;
                        xx = self.find_length(py);
                        if !(xx == 0 && py > 0) {
                            break;
                        }
                    }
                    if xx == 0 && py == 0 {
                        failed = true;
                        break;
                    }
                    px = xx - 1;
                } else {
                    px -= 1;
                }
                let gc = self.grid.get(px, py);
                if gc.single && !gc.padding {
                    if gc.c == found {
                        n += 1;
                    } else if gc.c == start {
                        n -= 1;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            if !failed {
                self.scroll_to(px, py);
            }
        }
    }

    fn next_matching_bracket(&mut self, np: u32, separators: &str) {
        const OPEN: &[u8] = b"{[(";
        const CLOSE: &[u8] = b"}])";
        let is = |set: &[u8], c: char| set.iter().position(|&b| char::from(b) == c);
        'count: for _ in 0..np {
            let (mut px, mut py) = (self.cx, self.py());
            let mut xx = self.find_length(py);
            let yy = self.grid.total() - 1;
            if xx == 0 {
                break;
            }
            let mut tried = false;
            let (found, end) = loop {
                let gc = self.grid.get(px, py);
                let mut cp = None;
                if gc.single && !gc.padding {
                    if is(CLOSE, gc.c).is_some() && self.vi {
                        // As tmux in vi: on a closing bracket, go back to
                        // its opening one, or stay.
                        let (sx, sy) = (self.cx, self.py());
                        self.scroll_to(px, py);
                        self.previous_matching_bracket(np, separators);
                        let gc = self.grid.get(self.cx, self.py());
                        if gc.single && !gc.padding && is(CLOSE, gc.c).is_some() {
                            self.scroll_to(sx, sy);
                        }
                        break 'count;
                    }
                    cp = is(OPEN, gc.c);
                }
                if let Some(i) = cp {
                    break (gc.c, char::from(CLOSE[i]));
                }
                if !self.vi {
                    if !tried && px <= xx {
                        px += 1;
                        tried = true;
                        continue;
                    }
                    let open = std::str::from_utf8(OPEN).unwrap_or_default();
                    self.cursor_next_word_end(open);
                    continue 'count;
                }
                // In vi, look on along the line (and its wraps).
                if px > xx {
                    if py == yy || !self.grid.wrapped(py) {
                        continue 'count;
                    }
                    px = 0;
                    py += 1;
                    xx = self.find_length(py);
                } else {
                    px += 1;
                }
            };
            let mut n = 1;
            let mut failed = false;
            loop {
                if px > xx {
                    if py == yy {
                        failed = true;
                        break;
                    }
                    px = 0;
                    py += 1;
                    xx = self.find_length(py);
                } else {
                    px += 1;
                }
                let gc = self.grid.get(px, py);
                if gc.single && !gc.padding {
                    if gc.c == found {
                        n += 1;
                    } else if gc.c == end {
                        n -= 1;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            if !failed {
                self.scroll_to(px, py);
            }
        }
    }

    /// `refresh-from-pane`: a new copy, keeping the view's distance from
    /// the top of history.
    pub fn refresh(&mut self, grid: Grid) {
        let hsize = self.grid.hsize;
        self.oy = self.oy.min(hsize);
        let from_top = hsize - self.oy;
        self.grid = Arc::new(grid);
        let hsize = self.grid.hsize;
        if from_top <= hsize {
            self.oy = hsize - from_top;
        } else {
            self.cy = 0;
            self.oy = hsize;
        }
        self.cy = self.cy.min(self.grid.sy.saturating_sub(1));
        self.size_changed();
    }

    /// tmux's `window_copy_size_changed`: the selection and search marks
    /// go.
    fn size_changed(&mut self) {
        select::clear_selection(self);
        search::clear_marks(self);
        self.search.start = Some((self.cx, self.cy, self.oy));
    }

    /// `#{copy_cursor_word}`: the word under the cursor, by `separators`
    /// (the global `word-separators`, as tmux reads it).
    pub fn cursor_word(&self, separators: &str) -> String {
        self.grid.word(self.cx, self.py(), separators)
    }
}

/// tmux's `strtonum`: a decimal in `min..=max`, else its reason.
pub fn strtonum(s: &str, min: i64, max: i64) -> Result<i64, &'static str> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid");
    }
    match t.parse::<i64>() {
        Ok(n) if n < min => Err("too small"),
        Ok(n) if n > max => Err("too large"),
        Ok(n) => Ok(n),
        Err(_) if t.starts_with('-') => Err("too small"),
        Err(_) => Err("too large"),
    }
}

/// A copy-mode command's own arguments, as tmux's `args_parse` reads them
/// with no flags: `--` ends the flags; any other word starting with `-`
/// is an unknown flag, which tmux drops the command for. `None` too when
/// there are fewer than `min` or more than `max`.
fn positionals(args: &[String], min: usize, max: usize) -> Option<&[String]> {
    let args = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        Some(a) if a.starts_with('-') && a.len() > 1 => return None,
        _ => args,
    };
    (min..=max).contains(&args.len()).then_some(args)
}

/// Most distinct names [`note_ignored`] remembers; past it nothing new is
/// noted, so a loop of made-up names grows neither the set nor the log.
const NOTED_MAX: usize = 64;

/// Whether `name` is new to `seen`, which it joins unless it is full.
fn remember(seen: &mut BTreeSet<String>, name: &str) -> bool {
    seen.len() < NOTED_MAX && seen.insert(name.to_string())
}

/// Notes in `compat-miss.log`, once per name, a command that did nothing
/// because grofe does not know it or its arguments do not fit (tmux drops
/// both silently too). Only a name made of lowercase letters and dashes is
/// written: nothing a command carries but its name ever is.
fn note_ignored(name: &str) {
    static SEEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let plain = !name.is_empty()
        && name.len() <= 48
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-');
    let shown = if plain { name } else { "(not a name)" };
    let first = remember(&mut SEEN.lock().unwrap_or_else(|e| e.into_inner()), shown);
    if first {
        crate::compat::note_miss(&format!("send-keys -X {shown}: ignored"));
    }
}

/// Runs one `send-keys -X` command (`args[0]` is its name) with the
/// mode's count, then resets the count, as tmux's `window_copy_command`.
/// A command it does not know, or whose arguments do not fit, does
/// nothing, as in tmux.
pub fn run(mode: &mut Mode, args: &[String], ctx: &Ctx<'_>) -> Action {
    let Some((name, rest)) = args.split_first() else {
        return Action::Nothing;
    };
    let action = command(mode, name, rest, ctx);
    if action.is_none() {
        note_ignored(name);
    }
    search::after_command(mode, name, action.is_some());
    mode.prefix = 1;
    action.unwrap_or(Action::Nothing)
}

fn command(mode: &mut Mode, name: &str, rest: &[String], ctx: &Ctx<'_>) -> Option<Action> {
    use Action::{Cancel, Nothing, Redraw};
    let np = mode.prefix;
    let seps = ctx.separators;
    let none = |rest| positionals(rest, 0, 0).map(|_| ());
    let action = match name {
        "cursor-up" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_up(false));
            Nothing
        }
        "cursor-down" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_down(false));
            Nothing
        }
        "cursor-left" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_left());
            Nothing
        }
        "cursor-right" => {
            none(rest)?;
            let all = mode.selecting && mode.rect;
            (0..np).for_each(|_| mode.cursor_right(all));
            Nothing
        }
        "start-of-line" => {
            none(rest)?;
            mode.cursor_start_of_line();
            Nothing
        }
        "end-of-line" => {
            none(rest)?;
            mode.cursor_end_of_line();
            Nothing
        }
        "back-to-indentation" => {
            none(rest)?;
            mode.cursor_back_to_indentation();
            Nothing
        }
        "next-word" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_next_word(seps));
            Nothing
        }
        "next-word-end" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_next_word_end(seps));
            Nothing
        }
        "previous-word" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_previous_word(seps, true));
            Nothing
        }
        "next-space" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_next_word(""));
            Nothing
        }
        "next-space-end" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_next_word_end(""));
            Nothing
        }
        "previous-space" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_previous_word("", true));
            Nothing
        }
        "top-line" => {
            none(rest)?;
            mode.cx = 0;
            mode.cy = 0;
            mode.update_selection();
            Redraw
        }
        "middle-line" => {
            none(rest)?;
            mode.cx = 0;
            mode.cy = (mode.sy() - 1) / 2;
            mode.update_selection();
            Redraw
        }
        "bottom-line" => {
            none(rest)?;
            mode.cx = 0;
            mode.cy = mode.sy() - 1;
            mode.update_selection();
            Redraw
        }
        "history-top" => {
            none(rest)?;
            mode.history_top();
            Redraw
        }
        "history-bottom" => {
            none(rest)?;
            mode.history_bottom();
            Redraw
        }
        "scroll-up" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_up(true));
            Nothing
        }
        "scroll-down" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_down(true));
            if mode.scroll_exit && mode.oy == 0 {
                Cancel
            } else {
                Nothing
            }
        }
        "scroll-down-and-cancel" => {
            none(rest)?;
            (0..np).for_each(|_| mode.cursor_down(true));
            if mode.oy == 0 { Cancel } else { Nothing }
        }
        "scroll-exit-on" | "scroll-exit-off" | "scroll-exit-toggle" => {
            none(rest)?;
            mode.scroll_exit = match name {
                "scroll-exit-on" => true,
                "scroll-exit-off" => false,
                _ => !mode.scroll_exit,
            };
            Nothing
        }
        "cursor-down-and-cancel" => {
            none(rest)?;
            let cy = mode.cy;
            (0..np).for_each(|_| mode.cursor_down(false));
            if cy == mode.cy && mode.oy == 0 {
                Cancel
            } else {
                Nothing
            }
        }
        "cursor-centre-vertical" => {
            none(rest)?;
            mode.update_cursor(mode.cx, mode.sy() / 2);
            mode.update_selection();
            Redraw
        }
        "cursor-centre-horizontal" => {
            none(rest)?;
            mode.update_cursor(mode.grid.sx / 2, mode.cy);
            mode.update_selection();
            Redraw
        }
        "recentre-top-bottom" => {
            none(rest)?;
            mode.recentre_top_bottom();
            Redraw
        }
        "scroll-middle" => {
            none(rest)?;
            mode.scroll_cursor_to((mode.sy() - 1) / 2);
            Redraw
        }
        "scroll-top" => {
            none(rest)?;
            mode.scroll_cursor_to(0);
            Redraw
        }
        "scroll-bottom" => {
            none(rest)?;
            mode.scroll_cursor_to(mode.sy() - 1);
            Redraw
        }
        "page-up" | "halfpage-up" => {
            none(rest)?;
            let half = name == "halfpage-up";
            (0..np).for_each(|_| mode.pageup(half));
            Nothing
        }
        "page-down" | "halfpage-down" | "page-down-and-cancel" | "halfpage-down-and-cancel" => {
            none(rest)?;
            let half = name.starts_with("halfpage");
            let exit = mode.scroll_exit || name.ends_with("-and-cancel");
            if (0..np).any(|_| mode.pagedown(half, exit)) {
                Cancel
            } else {
                Nothing
            }
        }
        "next-paragraph" => {
            none(rest)?;
            (0..np).for_each(|_| mode.next_paragraph());
            Nothing
        }
        "previous-paragraph" => {
            none(rest)?;
            (0..np).for_each(|_| mode.previous_paragraph());
            Nothing
        }
        "next-matching-bracket" => {
            none(rest)?;
            mode.next_matching_bracket(np, seps);
            Nothing
        }
        "previous-matching-bracket" => {
            none(rest)?;
            mode.previous_matching_bracket(np, seps);
            Nothing
        }
        "next-prompt" | "previous-prompt" => {
            let (flags, _) = select::parse(rest, "o", 0)?;
            mode.cursor_prompt(name == "next-prompt", flags.contains('o'));
            Nothing
        }
        "goto-line" => {
            let line = positionals(rest, 1, 1)?;
            if !line[0].is_empty() {
                mode.goto_line(&line[0]);
            }
            Redraw
        }
        "set-mark" => {
            none(rest)?;
            mode.set_mark();
            Redraw
        }
        "jump-to-mark" => {
            none(rest)?;
            mode.jump_to_mark();
            Redraw
        }
        "toggle-position" => {
            none(rest)?;
            mode.hide_position = !mode.hide_position;
            Redraw
        }
        "cancel" => {
            none(rest)?;
            Cancel
        }
        // As tmux's `window_copy_expand_search_string`: an empty string
        // searches for nothing; any other is kept for the search (TM.3).
        _ if FORMAT_COMMANDS.contains(&name) => {
            let text = positionals(rest, 0, 1)?;
            match text.first() {
                Some(t) if !t.is_empty() => mode.search_str = Some(t.clone()),
                _ => return Some(Nothing),
            }
            return search::command(mode, name, rest, ctx).or(Some(Nothing));
        }
        // The other commands are TM.2's, TM.3's and TM.5's.
        _ => {
            return select::command(mode, name, rest, ctx)
                .or_else(|| search::command(mode, name, rest, ctx))
                .or_else(|| pipe::command(mode, name, rest, ctx));
        }
    };
    Some(action)
}

// ---- Entering, leaving, formats --------------------------------------------

/// The copy-mode key table for `pane`'s window (`mode-keys`).
pub fn key_table(state: &State, pane: PaneId) -> &'static str {
    if vi_keys(state, pane) {
        VI_TABLE
    } else {
        EMACS_TABLE
    }
}

fn vi_keys(state: &State, pane: PaneId) -> bool {
    let model = &state.model;
    model.panes.contains_key(&pane)
        && model
            .options
            .lookup("mode-keys", &options::chain(model, pane))
            != Some("emacs")
}

/// `word-separators` for `pane`'s session.
pub fn separators(state: &State, pane: PaneId) -> String {
    let model = &state.model;
    model
        .options
        .lookup("word-separators", &options::chain(model, pane))
        .unwrap_or_default()
        .to_string()
}

/// Enters copy mode in `pane` with a fresh copy of it, unless it is
/// already in it; whether it entered.
pub fn enter(state: &mut State, pane: PaneId, source: PaneId, scroll_exit: bool) -> bool {
    if state.copy.is_active(pane) {
        return false;
    }
    let vi = vi_keys(state, pane);
    let Some(p) = state.model.panes.get(&source) else {
        return false;
    };
    let (mut grid, cursor) = Grid::from_pane(&p.handle);
    if source != pane {
        let rows = state
            .model
            .panes
            .get(&pane)
            .map_or(grid.sy, |t| usize::from(t.rows));
        grid.fit_to(rows);
    }
    if grid.sy == 0 {
        return false;
    }
    let mut mode = Mode::new(grid, cursor, vi);
    mode.scroll_exit = scroll_exit;
    mode.source = (source != pane).then_some(source);
    if let Some(last) = state.copy.searches.get(&pane) {
        search::restore(&mut mode, last);
    }
    state.copy.insert(pane, mode);
    true
}

/// `copy-mode -u`: a page up, as tmux's `window_copy_pageup`.
pub fn page_up(state: &mut State, pane: PaneId) {
    if let Some(mode) = state.copy.get_mut(pane) {
        mode.pageup(false);
    }
}

/// `copy-mode -d`: a page down; with `scroll_exit` reaching the bottom
/// leaves the mode, as tmux's `window_copy_pagedown`.
pub fn page_down(state: &mut State, pane: PaneId, scroll_exit: bool) {
    if state
        .copy
        .get_mut(pane)
        .is_some_and(|mode| mode.pagedown(false, scroll_exit))
    {
        state.copy.remove(pane);
    }
}

/// A fresh copy of `pane` for its mode (`refresh-from-pane`).
pub fn refresh(state: &mut State, pane: PaneId) {
    let source = state.copy.get(pane).and_then(|m| m.source).unwrap_or(pane);
    let Some(p) = state.model.panes.get(&source) else {
        return;
    };
    let (mut grid, _) = Grid::from_pane(&p.handle);
    if source != pane {
        let rows = state
            .model
            .panes
            .get(&pane)
            .map_or(grid.sy, |t| usize::from(t.rows));
        grid.fit_to(rows);
    }
    if grid.sy == 0 {
        return;
    }
    if let Some(mode) = state.copy.get_mut(pane) {
        mode.refresh(grid);
    }
}

/// Runs `send-keys -X` `args` in `pane`'s mode; the pane leaves the mode
/// if the command says so. `Err` when the pane is in no mode, with tmux's
/// text.
pub fn command_in(state: &mut State, pane: PaneId, args: &[String]) -> Result<(), String> {
    if !state.copy.is_active(pane) {
        return Err("not in a mode".to_string());
    }
    // A search in flight (TM.3): the command waits its turn.
    if search::busy(state, pane) {
        search::hold(state, pane, args);
        return Ok(());
    }
    command_now(state, pane, args)?;
    search::launch(state, pane, true);
    Ok(())
}

/// [`command_in`] for a mode with no search in flight.
fn command_now(state: &mut State, pane: PaneId, args: &[String]) -> Result<(), String> {
    if !state.copy.is_active(pane) {
        return Err("not in a mode".to_string());
    }
    let vi = vi_keys(state, pane);
    let seps = separators(state, pane);
    resize_to_pane(state, pane);
    let args = &pipe::expand(state, pane, args);
    if args.first().map(String::as_str) == Some("refresh-from-pane") {
        if positionals(&args[1..], 0, 0).is_some() {
            refresh(state, pane);
        }
        if let Some(mode) = state.copy.get_mut(pane) {
            mode.prefix = 1;
        }
        return Ok(());
    }
    let Some(mode) = state.copy.get_mut(pane) else {
        return Err("not in a mode".to_string());
    };
    mode.vi = vi;
    let ctx = Ctx { separators: &seps };
    let action = run(mode, args, &ctx);
    let copied = std::mem::take(&mut mode.sel.copied);
    let error = mode.sel.error.take();
    if action == Action::Cancel {
        state.copy.remove(pane);
    }
    pipe::run_pipes(state, pane, &copied);
    crate::server::passthrough::on_copy(state, pane, &copied);
    select::store(state, copied)?;
    error.map_or(Ok(()), Err)
}

/// Runs `f` on `pane`'s mode now, with its `word-separators`, as a mouse
/// event's own change to it (TM.6): after a pending resize, as a command
/// would see it. `None` outside the mode or while a search is in flight.
// ponytail: a mouse change waiting behind a search is dropped rather than
// queued after it.
pub fn edit_now<R>(
    state: &mut State,
    pane: PaneId,
    f: impl FnOnce(&mut Mode, &str) -> R,
) -> Option<R> {
    if !state.copy.is_active(pane) || search::busy(state, pane) {
        return None;
    }
    let vi = vi_keys(state, pane);
    let seps = separators(state, pane);
    resize_to_pane(state, pane);
    let mode = state.copy.get_mut(pane)?;
    mode.vi = vi;
    Some(f(mode, &seps))
}

/// Sets the count the next command in `pane`'s mode runs with (`send-keys
/// -N` with no keys); `Err` when the pane is in no mode.
pub fn set_prefix(state: &mut State, pane: PaneId, count: u32) -> Result<(), String> {
    let mode = state
        .copy
        .get_mut(pane)
        .ok_or_else(|| "not in a mode".to_string())?;
    mode.prefix = count.min(MAX_REPEAT);
    Ok(())
}

/// The commands that only move the cursor, the view or the selection's
/// moving end, as a function of where they start: one that leaves
/// [`position`] as it found it would do the same if run again.
const MOTIONS: &[&str] = &[
    "back-to-indentation",
    "bottom-line",
    "cursor-down",
    "cursor-left",
    "cursor-right",
    "cursor-up",
    "end-of-line",
    "halfpage-down",
    "halfpage-up",
    "history-bottom",
    "history-top",
    "middle-line",
    "next-paragraph",
    "next-space",
    "next-space-end",
    "next-word",
    "next-word-end",
    "page-down",
    "page-up",
    "previous-paragraph",
    "previous-space",
    "previous-word",
    "scroll-bottom",
    "scroll-down",
    "scroll-middle",
    "scroll-top",
    "scroll-up",
    "start-of-line",
    "top-line",
];

/// Whether a bound command is `send-keys -X` with a motion and nothing else.
pub fn is_motion(argv: &[String]) -> bool {
    match argv {
        [send, x, name] => {
            matches!(send.as_str(), "send-keys" | "send")
                && x == "-X"
                && MOTIONS.contains(&name.as_str())
        }
        _ => false,
    }
}

/// Everything a motion reads or writes in `pane`'s mode, beyond the copy
/// itself and the options; `None` outside the mode, or while its next
/// command would wait for a search or take a fresh copy first.
#[derive(Debug, PartialEq, Eq)]
pub struct Position {
    /// Which copy (a fresh one is another).
    grid: usize,
    cursor: (usize, usize, usize),
    last: (usize, usize),
    mark: (usize, usize, bool),
    selection: (bool, (usize, usize), (usize, usize)),
    prefix: u32,
}

pub fn position(state: &State, pane: PaneId) -> Option<Position> {
    if search::busy(state, pane) || resize_pending(state, pane) {
        return None;
    }
    let m = state.copy.get(pane)?;
    Some(Position {
        grid: Arc::as_ptr(&m.grid) as usize,
        cursor: (m.cx, m.cy, m.oy),
        last: (m.lastcx, m.lastsx),
        mark: (m.mx, m.my, m.showmark),
        selection: (m.selecting, m.sel_start, m.sel_end),
        prefix: m.prefix,
    })
}

/// Whether `pane` has been resized since its mode's copy was taken.
fn resize_pending(state: &State, pane: PaneId) -> bool {
    let (Some(p), Some(m)) = (state.model.panes.get(&pane), state.copy.get(pane)) else {
        return false;
    };
    // The text of a view mode is not the pane's to read again.
    !m.view && (m.grid.sy != usize::from(p.rows) || m.grid.sx != usize::from(p.cols))
}

// ponytail: a pane resized in copy mode gets a fresh copy at its new size
// (as `refresh-from-pane`) instead of tmux's reflow of the old copy.
fn resize_to_pane(state: &mut State, pane: PaneId) {
    if resize_pending(state, pane) {
        refresh(state, pane);
        if let Some(mode) = state.copy.get_mut(pane) {
            let limit = mode.cursor_limit(mode.py(), true);
            mode.cx = mode.cx.min(limit);
        }
    }
}

/// The format variables copy mode answers for `pane`: `pane_in_mode` and
/// `pane_mode` while it is in the mode, and the mode's own (tmux's
/// `window_copy_formats`). `None` lets the caller's own variables answer.
pub fn format_var(state: &State, pane: PaneId, name: &str) -> Option<String> {
    // The pane's last search, in the mode or after it (`wp->searchstr`).
    if name == "pane_search_string" {
        let last = match state.copy.get(pane) {
            Some(mode) => search::remember(mode),
            None => state.copy.searches.get(&pane).cloned(),
        };
        return Some(last.map(|l| l.text().to_string()).unwrap_or_default());
    }
    let mode = state.copy.get(pane)?;
    Some(match name {
        "pane_in_mode" => {
            (1 + usize::from(mode.under.is_some()) + usize::from(mode.over_clock)).to_string()
        }
        "pane_mode" => if mode.view { VIEW_MODE_NAME } else { MODE_NAME }.to_string(),
        "top_line_time" => mode.grid.row_time(mode.grid.hsize - mode.oy).to_string(),
        "copy_cursor_x" => mode.cx.to_string(),
        "copy_cursor_y" => mode.cy.to_string(),
        "scroll_position" => mode.oy.to_string(),
        "copy_position" => mode.oy.to_string(),
        "copy_position_limit" => mode.grid.hsize.to_string(),
        "copy_cursor_word" => {
            let seps = state.model.options.lookup("word-separators", &[]);
            mode.cursor_word(seps.unwrap_or_default())
        }
        _ => {
            // A resize not yet applied clears the selection (tmux's
            // `window_copy_size_changed`).
            return select::format_var(mode, name, resize_pending(state, pane))
                .or_else(|| search::format_var(mode, name));
        }
    })
}

// ---- Keys --------------------------------------------------------------------

/// Routes a client's keys while its active pane is in copy mode, as tmux
/// 3.7b's `server_client_key_callback` does when the client is in its
/// root table: the prefix and any other table the client is in route as
/// usual; otherwise a key bound in the mode's table runs, else one bound
/// in `root`, else it is dropped (the pane never gets it). A paste is
/// dropped too.
pub struct CopyHook;

impl InputHook for CopyHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        let model = &state.model;
        state.copy.retain(|p| model.panes.contains_key(&p));
        let pane = active_pane(state, client)?;
        if !state.copy.is_active(pane) {
            return None;
        }
        let c = state.clients.get(&client)?;
        if c.keys.table() != ROOT_TABLE {
            return None;
        }
        match input {
            Input::Key(key) => {
                let settings = clients::key_settings(&state.model, c.session);
                if settings.is_prefix(key) {
                    return None;
                }
                let tables = &state.registry.key_tables;
                let b = tables
                    .get(key_table(state, pane), key)
                    .or_else(|| tables.get(ROOT_TABLE, key));
                Some(match b {
                    Some(b) => Routed::Bound(b.cmds.clone(), b.dialect),
                    None => Routed::Consumed,
                })
            }
            Input::Paste { .. } => Some(Routed::Consumed),
            Input::Mouse(_) | Input::Focus(_) => None,
        }
    }
}

/// The binding `send-keys` runs for `key` typed into `pane`'s mode: the
/// mode table's only, as tmux's `cmd_send_keys_inject_key`.
pub fn binding_for(
    state: &State,
    pane: PaneId,
    key: &crate::server::input::Key,
) -> Option<(Vec<Vec<String>>, Dialect)> {
    let b = state.registry.key_tables.get(key_table(state, pane), key)?;
    Some((b.cmds.clone(), b.dialect))
}

fn client_window(state: &State, id: ClientId) -> Option<&Window> {
    let c = state.clients.get(&id)?;
    let s = state.model.sessions.get(&c.session)?;
    state.model.windows.get(&s.active)
}

fn active_pane(state: &State, id: ClientId) -> Option<PaneId> {
    client_window(state, id).map(|w| w.active)
}

// ---- Drawing -------------------------------------------------------------------

/// Draws each visible pane in copy mode: the copy's rows at the view's
/// offset, the mark's row in `copy-mode-mark-style`, the `[offset/size]`
/// indicator at the top right in `mode-style` (unless
/// `toggle-position` hid it), and the cursor where the mode's is when the
/// pane is the client's active one.
pub struct CopyOverlay;

impl Overlay for CopyOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        let Some(w) = client_window(state, client) else {
            return;
        };
        let rects: Vec<(PaneId, Rect)> = w.pane_rects();
        for (pane, rect) in rects {
            let Some(mode) = state.copy.get(pane) else {
                continue;
            };
            let Some(&(_, clip)) = cx.panes.iter().find(|(id, _)| *id == pane) else {
                continue;
            };
            let at = |x: usize, y: usize| -> Option<(u16, u16)> {
                let col = i64::from(rect.x) + x as i64 - i64::from(cx.viewport.0);
                let row = i64::from(rect.y) + y as i64 - i64::from(cx.viewport.1)
                    + i64::from(cx.window_top);
                let inside = |v: i64, lo: u16, len: u16| {
                    v >= i64::from(lo) && v < i64::from(lo) + i64::from(len)
                };
                (inside(col, clip.x, clip.cols) && inside(row, clip.y, clip.rows))
                    .then_some((col as u16, row as u16))
            };
            let mut put = |x: usize, y: usize, cell: SnapCell| {
                if let Some((col, row)) = at(x, y) {
                    let i = usize::from(row) * usize::from(screen.cols) + usize::from(col);
                    if let Some(slot) = screen.cells.get_mut(i) {
                        *slot = cell;
                    }
                }
            };
            let chain = options::chain(&state.model, pane);
            let style = |name: &str, default: &str| {
                status::parse_style(state.model.options.lookup(name, &chain).unwrap_or(default))
            };
            let mark = status::parse_style(MARK_STYLE);
            let found = search::view_marks(mode);
            let all = style("copy-mode-match-style", search::MATCH_STYLE);
            let current = style("copy-mode-current-match-style", search::CURRENT_MATCH_STYLE);
            let selected = style("mode-style", MODE_STYLE);
            let (cols, rows) = (usize::from(rect.cols), usize::from(rect.rows));
            let top = mode.grid.hsize - mode.oy;
            let (default_fg, default_bg) = clients::pane_defaults(&state.model, pane);
            for y in 0..rows.min(mode.grid.sy) {
                let fy = top + y;
                for x in 0..cols {
                    let mut cell = mode.grid.snap(x, fy);
                    // The pane's `window-style` shows in the default colours.
                    if cell.fg == Color::Named(NamedColor::Foreground) {
                        cell.fg = default_fg;
                    }
                    if cell.bg == Color::Named(NamedColor::Background) {
                        cell.bg = default_bg;
                    }
                    if mode.showmark && fy == mode.my {
                        let (fg, bg) = if x == mode.mx {
                            (mark.bg, mark.fg)
                        } else {
                            (mark.fg, mark.bg)
                        };
                        cell.fg = fg;
                        cell.bg = bg;
                        cell.flags = (cell.flags & !STYLE_FLAGS) | mark.attrs;
                    }
                    if let Some(found) = &found {
                        let inv = mode.showmark && fy == mode.my && x == mode.mx;
                        found.restyle(x, y, inv, &all, &current, &mut cell);
                    }
                    // As tmux's `screen_write_cell`: the selection draws over
                    // the mark and the search's styles.
                    if select::highlighted(mode, x, fy) {
                        cell.fg = selected.fg;
                        cell.bg = selected.bg;
                        cell.flags = (cell.flags & !STYLE_FLAGS) | selected.attrs;
                    }
                    put(x, y, cell);
                }
            }
            if !mode.hide_position && mode.grid.sy > 1 {
                position_label(state, pane, mode, cols, &mut |x, cell| put(x, 0, cell));
            }
            if mode.cx >= cols && mode.cy < rows {
                put(
                    cols.saturating_sub(1),
                    mode.cy,
                    SnapCell { c: '$', ..blank() },
                );
            }
            if pane == w.active
                && let Some((col, row)) = at(mode.cx.min(cols.saturating_sub(1)), mode.cy)
            {
                screen.cursor = Some(CursorState {
                    row,
                    col,
                    visible: true,
                    shape: CursorShape::Block,
                    blink: false,
                });
            }
            // Images the text had scrolled away with it, shown again as the
            // view goes back to them.
            if let (Some(images), Some(c)) = (&mode.grid.images, state.clients.get(&client)) {
                let from = screen.images.len();
                let place = Place {
                    origin: (
                        i32::from(rect.x) - i32::from(cx.viewport.0),
                        i32::from(rect.y) - i32::from(cx.viewport.1) + i32::from(cx.window_top),
                    ),
                    size: (rect.cols, rect.rows),
                    area: (clip.x + clip.cols, clip.y + clip.rows),
                };
                let view = images.shifted(-(top as i32));
                let mut none = Vec::new();
                screenimg::collect(
                    &mut screen.images,
                    &mut none,
                    pane,
                    &view,
                    place,
                    state.graphics.remap(),
                    ImageCaps::of(&c.caps),
                );
                screenimg::mark_covered(screen, from);
            }
        }
    }
}

/// `copy-mode-position-format` on the view's first row (tmux's
/// `window_copy_write_line`): its columns are drawn over the row's text,
/// in `copy-mode-position-style`.
fn position_label(
    state: &State,
    pane: PaneId,
    mode: &Mode,
    cols: usize,
    put: &mut dyn FnMut(usize, SnapCell),
) {
    use crate::server::dispatch::options::{FormatCtx, ServerVars};
    use unicode_width::UnicodeWidthChar;
    let chain = options::chain(&state.model, pane);
    let fmt = state
        .model
        .options
        .lookup("copy-mode-position-format", &chain)
        .unwrap_or_default();
    if fmt.is_empty() {
        return;
    }
    let vars = FormatCtx::new(&state.model, pane);
    let vars = ServerVars::new(state, pane, &vars);
    let expanded = cmd::expand(fmt, &vars);
    if expanded.is_empty() {
        return;
    }
    let style = state
        .model
        .options
        .lookup("copy-mode-position-style", &chain)
        .unwrap_or(MODE_STYLE);
    let base = status::base_style(style, &vars);
    let top = mode.grid.hsize - mode.oy;
    let under: Vec<(char, Style)> = (0..cols)
        .map(|x| {
            let cell = mode.grid.snap(x, top);
            let look = Style {
                fg: cell.fg,
                bg: cell.bg,
                attrs: cell.flags & STYLE_FLAGS,
                underline_color: cell.underline_color,
            };
            (cell.c, look)
        })
        .collect();
    let drawn = status::draw_over(&expanded, base, &under);
    let mut x = 0;
    for span in &drawn.line.spans {
        for c in span.text.chars() {
            let w = c.width().unwrap_or(0);
            if w == 0 {
                continue;
            }
            if x + w > cols {
                return;
            }
            if under.get(x) != Some(&(c, span.style)) {
                let mut cell = styled(c, &span.style);
                if w == 2 {
                    cell.flags.insert(CellFlags::WIDE_CHAR);
                }
                put(x, cell);
                if w == 2 {
                    let mut spacer = styled(' ', &span.style);
                    spacer.flags.insert(CellFlags::WIDE_CHAR_SPACER);
                    put(x + 1, spacer);
                }
            }
            x += w;
        }
    }
}

const STYLE_FLAGS: CellFlags = CellFlags::BOLD
    .union(CellFlags::DIM)
    .union(CellFlags::ITALIC)
    .union(CellFlags::ALL_UNDERLINES)
    .union(CellFlags::INVERSE)
    .union(CellFlags::HIDDEN)
    .union(CellFlags::STRIKEOUT);

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

/// Registers copy mode's input hook and overlay (`server::register_builtins`).
pub fn register(state: &mut State) {
    crate::server::register_input_hook(state, Arc::new(CopyHook));
    crate::server::register_overlay(state, Arc::new(CopyOverlay));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(rows: &[&str], hsize: usize, cursor: (usize, usize)) -> Mode {
        Mode::new(Grid::from_text(20, hsize, rows), cursor, true)
    }

    fn ctx() -> Ctx<'static> {
        Ctx {
            separators: "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~",
        }
    }

    fn x(m: &mut Mode, words: &[&str]) -> Action {
        let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        run(m, &args, &ctx())
    }

    fn at(m: &Mode) -> (usize, usize, usize) {
        (m.cx, m.cy, m.oy)
    }

    /// A copy of `rows` (the first `hsize` of them history) with the marks
    /// `marked` gives to rows by index.
    fn marked(rows: &[&str], hsize: usize, marked: &[(usize, u8)], cursor: (usize, usize)) -> Mode {
        let mut g = Grid::from_text(20, hsize, rows);
        for &(row, bits) in marked {
            g.rows[row].mark = bits;
        }
        Mode::new(g, cursor, true)
    }

    #[test]
    fn ignored_command_names_are_noted_once_and_only_so_many() {
        let mut seen = BTreeSet::new();
        assert!(remember(&mut seen, "nope"));
        assert!(!remember(&mut seen, "nope"));
        for i in 0..NOTED_MAX * 2 {
            remember(&mut seen, &format!("n{i}"));
        }
        assert_eq!(seen.len(), NOTED_MAX);
        assert!(!remember(&mut seen, "fresh"));
    }

    #[test]
    fn the_and_cancel_variants_cancel_only_at_the_bottom() {
        let rows: Vec<String> = (0..8).map(|i| format!("row {i}")).collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        // Four rows of history over a four-row view.
        let mut m = mode(&rows, 4, (0, 3));
        m.oy = 4;
        m.cy = 0;
        assert_eq!(x(&mut m, &["cursor-down-and-cancel"]), Action::Nothing);
        assert_eq!(x(&mut m, &["scroll-down-and-cancel"]), Action::Nothing);
        assert_eq!(m.oy, 3);
        assert_eq!(x(&mut m, &["page-down-and-cancel"]), Action::Nothing);
        assert!(m.oy > 0, "{:?}", at(&m));
        m.oy = 0;
        assert_eq!(x(&mut m, &["halfpage-down-and-cancel"]), Action::Cancel);
        assert_eq!(x(&mut m, &["page-down-and-cancel"]), Action::Cancel);
        m.cy = m.sy() - 1;
        assert_eq!(x(&mut m, &["cursor-down-and-cancel"]), Action::Cancel);
        assert_eq!(x(&mut m, &["scroll-down-and-cancel"]), Action::Cancel);
        assert_eq!(x(&mut m, &["scroll-down-and-cancel", "x"]), Action::Nothing);
    }

    #[test]
    fn scroll_exit_commands_set_and_flip_the_flag() {
        let mut m = mode(&["a", "b"], 0, (0, 1));
        x(&mut m, &["scroll-exit-on"]);
        assert!(m.scroll_exit);
        x(&mut m, &["scroll-exit-toggle"]);
        assert!(!m.scroll_exit);
        x(&mut m, &["scroll-exit-toggle"]);
        x(&mut m, &["scroll-exit-off"]);
        assert!(!m.scroll_exit);
        x(&mut m, &["scroll-exit-on", "extra"]);
        assert!(!m.scroll_exit, "an argument it does not take: nothing");
    }

    #[test]
    fn cursor_centring_goes_to_the_middle_of_the_view() {
        let rows: Vec<String> = (0..12).map(|i| format!("row number {i}")).collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        let mut m = Mode::new(Grid::from_text(20, 4, &rows), (0, 0), true);
        x(&mut m, &["cursor-centre-vertical"]);
        assert_eq!(m.cy, 4);
        x(&mut m, &["cursor-centre-horizontal"]);
        assert_eq!(m.cx, 10, "half the pane's width");
        let mut short = mode(&["ab", "c", "d", "e"], 0, (0, 0));
        x(&mut short, &["cursor-centre-horizontal"]);
        assert_eq!(short.cx, 1, "no further than the line goes (vi: last cell)");
    }

    #[test]
    fn recentre_top_bottom_cycles_and_starts_over_on_another_line() {
        let rows: Vec<String> = (0..20).map(|i| format!("row {i}")).collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        // Twelve rows of history over an eight-row view, viewed from the
        // top of the history with the cursor on its fourth row.
        let mut m = Mode::new(Grid::from_text(20, 12, &rows), (0, 7), false);
        m.oy = 12;
        m.cy = 5;
        let row = |m: &Mode| m.py();
        x(&mut m, &["recentre-top-bottom"]);
        assert_eq!((row(&m), m.cy), (5, 3), "first the middle");
        x(&mut m, &["recentre-top-bottom"]);
        assert_eq!((row(&m), m.cy), (5, 0), "then the top");
        x(&mut m, &["recentre-top-bottom"]);
        // The history ends there: the view cannot go further.
        assert_eq!((row(&m), m.cy), (5, 5), "then the bottom");
        x(&mut m, &["cursor-down"]);
        x(&mut m, &["recentre-top-bottom"]);
        assert_eq!(m.cy, 3, "another line begins with the middle again");
    }

    #[test]
    fn selection_mode_reads_a_name_in_any_case() {
        let mut m = mode(&["a b"], 0, (0, 0));
        for (arg, want) in [
            ("word", select::Unit::Word),
            ("L", select::Unit::Line),
            ("c", select::Unit::Char),
            ("W", select::Unit::Word),
            ("bogus", select::Unit::Word),
            ("Char", select::Unit::Char),
        ] {
            x(&mut m, &["selection-mode", arg]);
            assert_eq!(m.sel.unit, want, "{arg}");
        }
        x(&mut m, &["selection-mode", "line"]);
        x(&mut m, &["selection-mode"]);
        assert_eq!(m.sel.unit, select::Unit::Char, "no argument: char");
        x(&mut m, &["selection-mode", "line", "extra"]);
        assert_eq!(m.sel.unit, select::Unit::Char, "two words: nothing");
    }

    #[test]
    fn prompt_jumps_land_in_column_0_of_the_next_or_previous_mark() {
        let rows = ["a", "b", "c", "d", "e", "f"];
        let marks = [(1, marks::PROMPT), (3, marks::OUTPUT), (4, marks::PROMPT)];
        // Two rows of history; the cursor on the last row.
        let mut m = marked(&rows, 2, &marks, (0, 3));
        x(&mut m, &["previous-prompt"]);
        assert_eq!(at(&m), (0, 2, 0), "row 4");
        x(&mut m, &["previous-prompt"]);
        assert_eq!(
            at(&m),
            (0, 0, 1),
            "row 1 is in the history: the view scrolls"
        );
        x(&mut m, &["previous-prompt"]);
        assert_eq!(at(&m), (0, 0, 1), "no more");
        x(&mut m, &["next-prompt"]);
        assert_eq!(at(&m), (0, 2, 0));
        x(&mut m, &["next-prompt"]);
        assert_eq!(at(&m), (0, 2, 0), "no more");
        // -o looks for output starts.
        x(&mut m, &["previous-prompt", "-o"]);
        assert_eq!(at(&m), (0, 1, 0), "row 3");
        x(&mut m, &["previous-prompt", "-o"]);
        assert_eq!(at(&m), (0, 1, 0));
        x(&mut m, &["next-prompt", "-o"]);
        assert_eq!(at(&m), (0, 1, 0));
    }

    #[test]
    fn prompt_jumps_take_the_selection_along_and_ignore_the_count() {
        let rows = ["a", "b", "c", "d"];
        let marks = [(2, marks::PROMPT)];
        let mut m = marked(&rows, 0, &marks, (0, 0));
        x(&mut m, &["begin-selection"]);
        m.prefix = 9;
        x(&mut m, &["next-prompt"]);
        assert_eq!(at(&m), (0, 2, 0));
        assert_eq!(m.sel_end.1, 2, "the selection follows the cursor");
        assert_eq!(m.prefix, 1);
    }

    #[test]
    fn prompt_jumps_refuse_what_tmux_refuses() {
        let rows = ["a", "b", "c"];
        let marks = [(2, marks::PROMPT)];
        let mut m = marked(&rows, 0, &marks, (0, 0));
        for args in [&["-x"][..], &["extra"], &["-o", "extra"]] {
            let mut a = vec!["next-prompt"];
            a.extend(args);
            x(&mut m, &a);
            assert_eq!(at(&m), (0, 0, 0), "{args:?}");
        }
        x(&mut m, &["next-prompt", "-oo"]);
        assert_eq!(at(&m), (0, 0, 0), "-o finds no output start");
        x(&mut m, &["next-prompt", "--"]);
        assert_eq!(at(&m), (0, 2, 0));
    }

    #[test]
    fn line_length_trims_blanks() {
        let g = Grid::from_text(10, 0, &["ab  ", "", "  c"]);
        assert_eq!(
            (g.line_length(0), g.line_length(1), g.line_length(2)),
            (2, 0, 3)
        );
    }

    #[test]
    fn vertical_moves_keep_the_column_and_stick_to_ends() {
        let mut m = mode(&["long line here", "ab", "another long one", ""], 0, (0, 3));
        x(&mut m, &["cursor-up"]);
        // From an empty line (lastsx 0) the cursor sticks to the end.
        assert_eq!(at(&m), (15, 2, 0));
        x(&mut m, &["cursor-up"]);
        assert_eq!(at(&m), (1, 1, 0));
        // tmux remembers the column the cursor was clamped to.
        x(&mut m, &["cursor-up"]);
        assert_eq!(at(&m), (1, 0, 0));
    }

    #[test]
    fn scrolling_stops_at_the_ends_and_counts_repeat() {
        let mut m = mode(&["h0", "h1", "s0", "s1"], 2, (0, 1));
        m.prefix = 5;
        x(&mut m, &["scroll-up"]);
        assert_eq!(m.oy, 2);
        assert_eq!(m.prefix, 1);
        x(&mut m, &["scroll-down"]);
        assert_eq!(m.oy, 1);
        m.scroll_exit = true;
        assert_eq!(x(&mut m, &["scroll-down"]), Action::Cancel);
    }

    #[test]
    fn words_move_as_tmux_reads_separators() {
        let mut m = mode(&["foo.bar baz", ""], 0, (0, 0));
        x(&mut m, &["next-word"]);
        assert_eq!(m.cx, 3);
        x(&mut m, &["next-word"]);
        assert_eq!(m.cx, 4);
        x(&mut m, &["next-word"]);
        assert_eq!(m.cx, 8);
        x(&mut m, &["previous-word"]);
        assert_eq!(m.cx, 4);
        x(&mut m, &["next-space-end"]);
        assert_eq!(m.cx, 6);
    }

    #[test]
    fn brackets_match_across_lines() {
        let mut m = mode(&["a (b", "c) d", ""], 0, (2, 0));
        x(&mut m, &["next-matching-bracket"]);
        assert_eq!(at(&m), (1, 1, 0));
        x(&mut m, &["next-matching-bracket"]);
        assert_eq!(at(&m), (2, 0, 0));
    }

    #[test]
    fn commands_with_wrong_arguments_do_nothing() {
        let mut m = mode(&["abc", ""], 0, (0, 1));
        assert_eq!(x(&mut m, &["cursor-up", "x"]), Action::Nothing);
        assert_eq!(at(&m), (0, 1, 0));
        x(&mut m, &["goto-line", "-1"]);
        assert_eq!(at(&m), (0, 1, 0));
        x(&mut m, &["no-such-command"]);
        assert_eq!(at(&m), (0, 1, 0));
    }

    /// The default table holds tmux 3.7b's 88 stock rows, every one
    /// parsed.
    #[test]
    fn the_default_table_has_the_88_stock_rows() {
        let t = crate::server::input::default_bindings();
        assert_eq!(t.keys(VI_TABLE).len(), 88);
        assert_eq!(crate::server::input::COPY_MODE_VI.len(), 88);
    }

    /// Recorded on tmux 3.7b: `l` over `a<Tab>b<Tab>c` steps 0, 1, 8, 9,
    /// 16; a tab is whitespace to `w`, and a separator to the word.
    #[test]
    fn a_tab_is_one_wide_cell() {
        let mut m = mode(&["a\tb\tc", ""], 0, (0, 0));
        let mut seen = vec![m.cx];
        for _ in 0..4 {
            x(&mut m, &["cursor-right"]);
            seen.push(m.cx);
        }
        assert_eq!(seen, [0, 1, 8, 9, 16]);
        x(&mut m, &["start-of-line"]);
        x(&mut m, &["next-word"]);
        assert_eq!(m.cx, 8);
        assert_eq!(m.grid.word(8, 0, ""), "b");
        // As tmux (recorded): left from after a tab lands in its padding.
        x(&mut m, &["cursor-left"]);
        assert_eq!(m.cx, 7);
    }

    #[test]
    fn the_cursor_word_stops_at_separators() {
        let m = mode(&["foo.bar baz", ""], 0, (5, 0));
        assert_eq!(m.cursor_word(ctx().separators), "bar");
        assert_eq!(m.cursor_word(""), "foo.bar");
    }

    /// `#{mouse_line}` (tmux's `format_grid_line`) is the row up to its used
    /// width, a tab as a tab, and `#{mouse_word}` the word between blanks
    /// and tabs.
    #[test]
    fn a_row_is_its_text_up_to_its_used_width() {
        let g = Grid::from_text(20, 0, &["ab\tcd  ", "", "  x"]);
        assert_eq!(g.line_text(0), "ab\tcd");
        assert_eq!(g.line_text(1), "");
        assert_eq!(g.line_text(2), "  x");
        assert_eq!(g.word(9, 0, ""), "cd");
        assert_eq!(g.word(8, 0, ""), "cd");
        // On a blank, the word after it.
        assert_eq!(g.word(10, 0, ""), "");
        assert_eq!(g.hyperlink_at(0, 0), None);
    }

    /// The copy costs about 8 bytes a written cell: 100 000 rows of 80
    /// cells in alternating colours (`history-limit 100000`) stay under
    /// 100 000 * (80 * 8 + 64) bytes, with a handful of looks.
    #[test]
    fn a_full_history_copy_is_bounded() {
        let red = SnapCell {
            fg: Color::Named(NamedColor::Red),
            ..blank()
        };
        let mut g = Grid::empty(80, 0, 0);
        for y in 0..100_000usize {
            let mut cells: Vec<SnapCell> = (0..80)
                .map(|x| {
                    let look = if (x / 4 + y) % 2 == 0 {
                        red.clone()
                    } else {
                        blank()
                    };
                    SnapCell { c: 'x', ..look }
                })
                .collect();
            g.push_row(&mut cells, &[]);
        }
        g.hsize = 100_000 - 24;
        g.sy = 24;
        assert_eq!(g.looks.len(), 2);
        assert!(
            g.heap_bytes() < 100_000 * (80 * 8 + 64),
            "{} bytes",
            g.heap_bytes()
        );
        // Blank rows cost no cells.
        let mut blank_rows = Grid::empty(80, 0, 0);
        for _ in 0..1000 {
            blank_rows.push_row(&mut vec![blank(); 80], &[]);
        }
        assert!(blank_rows.rows.iter().all(|r| r.cells.capacity() == 0));
    }

    /// What `#` and `*` hand the search: the word `send-keys -F` expanded.
    #[test]
    fn a_search_keeps_its_string() {
        let mut m = mode(&["abc", ""], 0, (0, 1));
        x(&mut m, &["search-backward", "--", "foo"]);
        assert_eq!(m.search_str.as_deref(), Some("foo"));
        x(&mut m, &["search-forward", ""]);
        assert_eq!(m.search_str.as_deref(), Some("foo"));
    }

    #[test]
    fn strtonum_reasons() {
        assert_eq!(strtonum("12", 1, 20), Ok(12));
        assert_eq!(strtonum("abc", 1, 20), Err("invalid"));
        assert_eq!(strtonum("", 1, 20), Err("invalid"));
        assert_eq!(strtonum("0", 1, 20), Err("too small"));
        assert_eq!(strtonum("21", 1, 20), Err("too large"));
        assert_eq!(strtonum("99999999999999999999", 1, 20), Err("too large"));
    }
}
