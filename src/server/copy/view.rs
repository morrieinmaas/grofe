//! View mode (tmux 3.7b's `window_view_mode`): what a command run from a
//! key binding, a menu or a prompt prints, and what `run-shell` prints, is
//! shown over the pane in a copy mode of its own, whose keys are copy
//! mode's (`q` leaves it). `server_client_print` enters it and
//! `window_copy_add` adds each line to a screen as wide as the pane, which
//! scrolls into a history when it is full.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use unicode_width::UnicodeWidthChar;

use super::{Grid, Mode, Row, active_pane, blank, vi_keys};
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId};

/// How a line of output is read: tmux runs it through its input parser, so
/// colours work and other control characters do nothing.
#[derive(Debug, Clone)]
pub(super) struct Pen {
    fg: Color,
    bg: Color,
    flags: CellFlags,
    underline_color: Option<Color>,
}

impl Pen {
    pub(super) fn new() -> Pen {
        Pen {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: CellFlags::empty(),
            underline_color: None,
        }
    }

    fn cell(&self, c: char) -> SnapCell {
        SnapCell {
            c,
            fg: self.fg,
            bg: self.bg,
            flags: self.flags,
            zerowidth: Vec::new(),
            underline_color: self.underline_color,
            hyperlink: None,
        }
    }

    fn sgr(&mut self, params: &[u16]) {
        let mut it = params.iter().copied().peekable();
        if params.is_empty() {
            *self = Pen::new();
        }
        while let Some(p) = it.next() {
            match p {
                0 => *self = Pen::new(),
                1 => self.flags.insert(CellFlags::BOLD),
                2 => self.flags.insert(CellFlags::DIM),
                3 => self.flags.insert(CellFlags::ITALIC),
                4 => self.flags.insert(CellFlags::UNDERLINE),
                7 => self.flags.insert(CellFlags::INVERSE),
                8 => self.flags.insert(CellFlags::HIDDEN),
                9 => self.flags.insert(CellFlags::STRIKEOUT),
                22 => self.flags.remove(CellFlags::BOLD | CellFlags::DIM),
                23 => self.flags.remove(CellFlags::ITALIC),
                24 => self.flags.remove(CellFlags::ALL_UNDERLINES),
                27 => self.flags.remove(CellFlags::INVERSE),
                28 => self.flags.remove(CellFlags::HIDDEN),
                29 => self.flags.remove(CellFlags::STRIKEOUT),
                30..=37 => self.fg = Color::Indexed((p - 30) as u8),
                90..=97 => self.fg = Color::Indexed((p - 90 + 8) as u8),
                40..=47 => self.bg = Color::Indexed((p - 40) as u8),
                100..=107 => self.bg = Color::Indexed((p - 100 + 8) as u8),
                39 => self.fg = Color::Named(NamedColor::Foreground),
                49 => self.bg = Color::Named(NamedColor::Background),
                38 | 48 => {
                    let colour = match it.next() {
                        Some(5) => it.next().map(|n| Color::Indexed(n as u8)),
                        Some(2) => {
                            let (r, g, b) = (it.next(), it.next(), it.next());
                            match (r, g, b) {
                                (Some(r), Some(g), Some(b)) => Some(Color::Spec(Rgb {
                                    r: r as u8,
                                    g: g as u8,
                                    b: b as u8,
                                })),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = colour {
                        if p == 38 {
                            self.fg = c;
                        } else {
                            self.bg = c;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The cells of one logical line, laid out in rows of `sx` columns.
struct Writer<'a> {
    grid: &'a mut Grid,
    cells: Vec<SnapCell>,
    x: usize,
    now: u64,
}

impl Writer<'_> {
    fn fresh(&self) -> Vec<SnapCell> {
        vec![blank(); self.grid.sx]
    }

    /// Ends the row; `wrapped` when the text goes on in the next.
    fn end_row(&mut self, wrapped: bool) {
        let fresh = self.fresh();
        let mut cells = std::mem::replace(&mut self.cells, fresh);
        if wrapped && let Some(last) = cells.last_mut() {
            last.flags.insert(CellFlags::WRAPLINE);
        }
        let tabs = vec![false; cells.len()];
        self.grid.push_row(&mut cells, &tabs);
        self.grid.scroll_into_history(self.now);
        self.x = 0;
    }

    fn put(&mut self, c: char, pen: &Pen) {
        let sx = self.grid.sx;
        match c.width() {
            None => {}
            Some(0) => {
                if self.x > 0
                    && let Some(cell) = self.cells.get_mut(self.x - 1)
                {
                    cell.zerowidth.push(c);
                }
            }
            Some(w) => {
                if w > sx {
                    return;
                }
                if self.x + w > sx {
                    if let Some(last) = self.cells.last_mut() {
                        last.flags.insert(CellFlags::LEADING_WIDE_CHAR_SPACER);
                    }
                    self.end_row(true);
                }
                let mut cell = pen.cell(c);
                if w == 2 {
                    cell.flags.insert(CellFlags::WIDE_CHAR);
                }
                self.cells[self.x] = cell;
                if w == 2 {
                    let mut spacer = pen.cell(' ');
                    spacer.flags.insert(CellFlags::WIDE_CHAR_SPACER);
                    self.cells[self.x + 1] = spacer;
                }
                self.x += w;
            }
        }
    }
}

impl Grid {
    /// An empty view-mode screen `sx` wide and `sy` high.
    pub fn view(sx: usize, sy: usize) -> Grid {
        Grid::empty(sx, 0, sy)
    }

    /// Moves the rows past the screen's height into the history, stamping
    /// each with `now` (`grid_scroll_history`); how many moved.
    fn scroll_into_history(&mut self, now: u64) -> usize {
        let mut moved = 0;
        while self.rows.len() > self.hsize + self.sy {
            self.rows[self.hsize].time = now;
            self.hsize += 1;
            moved += 1;
        }
        moved
    }

    /// One more line of text (`window_copy_add` with `parse`); how much
    /// the history grew.
    pub fn add_line(&mut self, text: &str, now: u64) -> usize {
        let before = self.hsize;
        let mut pen = std::mem::replace(&mut self.pen, Pen::new());
        let mut w = Writer {
            cells: vec![blank(); self.sx],
            grid: self,
            x: 0,
            now,
        };
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.peek() {
                    Some('[') => {
                        chars.next();
                        let mut params = String::new();
                        let mut last = None;
                        for n in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&n) {
                                last = Some(n);
                                break;
                            }
                            params.push(n);
                        }
                        if last == Some('m') {
                            let nums: Vec<u16> = params
                                .split([';', ':'])
                                .map(|p| p.parse().unwrap_or(0))
                                .collect();
                            pen.sgr(if params.is_empty() { &[] } else { &nums });
                        }
                    }
                    Some(']') => {
                        chars.next();
                        // An operating system command ends at BEL or ST.
                        while let Some(n) = chars.next() {
                            if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                },
                '\t' => w.x = ((w.x / 8 + 1) * 8).min(w.grid.sx.saturating_sub(1)),
                '\r' => w.x = 0,
                c if c.is_control() => {}
                c => w.put(c, &pen),
            }
        }
        w.end_row(false);
        self.pen = pen;
        self.hsize - before
    }

    /// When the row at the top of the view went into the history, or 0
    /// (`#{top_line_time}`).
    pub fn row_time(&self, y: usize) -> u64 {
        self.rows.get(y).map_or(0, |r: &Row| r.time)
    }
}

impl Mode {
    /// A view mode over a screen `sx` by `sy` (`window_copy_view_init`).
    fn view(sx: usize, sy: usize, vi: bool) -> Mode {
        let mut mode = Mode::new(Grid::view(sx, sy), (0, 0), vi);
        mode.view = true;
        mode
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Adds `text` to `pane`'s view mode, entering it first unless the pane is
/// in it. Each line of `text` is one `window_copy_add`; with `keep_blank`
/// unset the empty ones are dropped, as tmux drops a `cmdq_print` of "".
pub fn print(state: &mut State, pane: PaneId, text: &str, keep_blank: bool) {
    let Some((sx, sy)) = state
        .model
        .panes
        .get(&pane)
        .map(|p| (usize::from(p.cols), usize::from(p.rows)))
    else {
        return;
    };
    if sx == 0 || sy == 0 {
        return;
    }
    if !state.copy.get(pane).is_some_and(|m| m.view) {
        let mut mode = Mode::view(sx, sy, vi_keys(state, pane));
        mode.under = state.copy.modes.remove(&pane).map(Box::new);
        mode.over_clock = state.clocks.is_active(pane);
        state.copy.insert(pane, mode);
    }
    // A lone newline is `cmdq_print` of "": the mode, and no line.
    if text == "\n" {
        clients::mark_all_dirty(state);
        return;
    }
    let lines: Vec<&str> = match text.strip_suffix('\n') {
        Some(rest) => rest.split('\n').collect(),
        None if text.is_empty() => Vec::new(),
        None => text.split('\n').collect(),
    };
    let when = now();
    if let Some(mode) = state.copy.get_mut(pane) {
        for line in lines {
            if line.is_empty() && !keep_blank {
                continue;
            }
            let grid = std::sync::Arc::make_mut(&mut mode.grid);
            let grew = grid.add_line(line, when);
            mode.oy += grew;
        }
    }
    clients::mark_all_dirty(state);
}

/// What a command printed for an attached client: in the view mode of the
/// client's pane (`server_client_print`).
pub fn print_to_client(state: &mut State, client: ClientId, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(pane) = active_pane(state, client) {
        print(state, pane, text, true);
    }
}

/// The pane a command with nothing to say where it runs finds
/// (`cmd_find_from_nothing`): the active pane of the most recently used
/// session.
fn pane_from_nothing(state: &State) -> Option<PaneId> {
    let model = &state.model;
    let session = model.sessions.values().max_by_key(|s| s.activity)?;
    model.windows.get(&session.active).map(|w| w.active)
}

/// What `run-shell` printed, in the view mode of `pane`, else of the
/// client's pane, else of the pane [`pane_from_nothing`] finds. Through a
/// client's `cmdq_print` an empty line adds nothing; added to a pane
/// (`-b`, `-t`) it is a blank line (`keep_blank`).
pub fn print_shell(
    state: &mut State,
    pane: Option<PaneId>,
    client: Option<ClientId>,
    text: &str,
    keep_blank: bool,
) {
    if text.is_empty() {
        return;
    }
    let pane = pane
        .filter(|p| state.model.panes.contains_key(p))
        .or_else(|| client.and_then(|c| active_pane(state, c)))
        .or_else(|| pane_from_nothing(state));
    if let Some(pane) = pane {
        print(state, pane, text, keep_blank);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_fill_the_screen_then_scroll_into_the_history() {
        let mut g = Grid::view(10, 3);
        for i in 0..5 {
            g.add_line(&format!("line {i}"), 7);
        }
        assert_eq!((g.hsize, g.sy), (2, 3));
        assert_eq!(g.line_text(0), "line 0");
        assert_eq!(g.row_time(0), 7);
        assert_eq!(g.row_time(2), 0, "screen rows have no time");
    }

    #[test]
    fn a_long_line_wraps_and_colours_are_read() {
        let mut g = Grid::view(4, 5);
        let grew = g.add_line("\x1b[31mabcdefg\x1b[0m", 0);
        assert_eq!(grew, 0);
        assert!(g.wrapped(0));
        assert_eq!(g.line_text(1), "efg");
        assert_ne!(g.snap(0, 0).fg, g.snap(0, 1).bg);
    }

    /// `window_copy_vadd` parses every line with the mode's one input
    /// context, so a colour set on one line goes on into the next.
    #[test]
    fn a_colour_carries_over_to_the_next_line() {
        let mut g = Grid::view(4, 3);
        g.add_line("\x1b[31ma", 0);
        g.add_line("b", 0);
        assert_eq!(g.snap(0, 1).fg, g.snap(0, 0).fg);
        assert_ne!(g.snap(0, 1).fg, Color::Named(NamedColor::Foreground));
    }

    #[test]
    fn the_history_growth_is_reported() {
        let mut g = Grid::view(10, 1);
        g.add_line("a", 0);
        assert_eq!(g.add_line("b", 0), 1);
    }
}
