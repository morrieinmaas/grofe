//! Compact history rows.
//!
//! A row is its text (one char per cell) plus runs of equal look, with the
//! looks interned once for the whole store. Default blanks past the used
//! width are not stored.

use std::collections::VecDeque;
use std::mem::size_of;

use alacritty_terminal::term::cell::{Cell, CellExtra};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::SnapCell;
use super::looks::Looks;
use super::marks::RowShape;
use super::reflow::{ColdMeta, reflow_rows};

/// A cell `encode` can store: a [`SnapCell`] or the emulator's own.
trait Look {
    fn ch(&self) -> char;
    /// Never true for two different looks (it may miss equal ones).
    fn same_look(&self, other: &Self) -> bool;
    /// The end of the run of cells in `from..to` that look like the first.
    fn run_end(cells: &[Self], from: usize, to: usize) -> usize
    where
        Self: Sized,
    {
        cells[from + 1..to]
            .iter()
            .position(|c| !c.same_look(&cells[from]))
            .map_or(to, |n| from + 1 + n)
    }
    /// A space in the default look (id 0), which a row need not store.
    fn default_blank(&self) -> bool;
    fn look(&self) -> SnapCell;
}

impl Look for SnapCell {
    fn ch(&self) -> char {
        self.c
    }
    fn same_look(&self, o: &Self) -> bool {
        self.fg == o.fg
            && self.bg == o.bg
            && self.flags == o.flags
            && self.underline_color == o.underline_color
            && self.zerowidth == o.zerowidth
            && self.hyperlink == o.hyperlink
    }
    fn default_blank(&self) -> bool {
        self.c == ' '
            && self.fg == Color::Named(NamedColor::Foreground)
            && self.bg == Color::Named(NamedColor::Background)
            && self.flags.is_empty()
            && self.underline_color.is_none()
            && self.zerowidth.is_empty()
            && self.hyperlink.is_none()
    }
    fn look(&self) -> SnapCell {
        self.clone()
    }
}

impl Look for Cell {
    #[inline]
    fn ch(&self) -> char {
        self.c
    }
    #[inline]
    fn same_look(&self, o: &Self) -> bool {
        self.flags == o.flags && self.fg == o.fg && self.bg == o.bg && self.extra == o.extra
    }
    fn run_end(cells: &[Cell], from: usize, to: usize) -> usize {
        let first = &cells[from];
        let rest = &cells[from + 1..to];
        // Unswitched for the common look, named colours and no extras: the
        // generic colour compare costs several times more per cell.
        let n = match (first.fg, first.bg, &first.extra) {
            (Color::Named(fg), Color::Named(bg), None) => rest.iter().position(|c| {
                !(c.flags == first.flags
                    && c.extra.is_none()
                    && matches!(c.fg, Color::Named(x) if x == fg)
                    && matches!(c.bg, Color::Named(x) if x == bg))
            }),
            _ => rest.iter().position(|c| !c.same_look(first)),
        };
        n.map_or(to, |n| from + 1 + n)
    }
    #[inline]
    fn default_blank(&self) -> bool {
        self.c == ' '
            && self.flags.is_empty()
            && self.fg == Color::Named(NamedColor::Foreground)
            && self.bg == Color::Named(NamedColor::Background)
            && self
                .extra
                .as_deref()
                .is_none_or(|e| *e == CellExtra::default())
    }
    fn look(&self) -> SnapCell {
        SnapCell {
            c: self.c,
            fg: self.fg,
            bg: self.bg,
            flags: self.flags,
            zerowidth: self.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
            underline_color: self.underline_color(),
            hyperlink: self.hyperlink(),
        }
    }
}

/// Bytes of a run in [`Row::data`]: its length in cells (`u16`) and its
/// look id (`u32`), little-endian.
const RUN: usize = 6;

#[derive(Debug)]
struct Row {
    /// The text, then the runs, [`RUN`] bytes each: one allocation a row.
    data: Box<[u8]>,
    /// The bytes of the text.
    text: u32,
    /// The cells the row had when pushed.
    width: u32,
    used: u32,
    wrapped: bool,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Cold {
    rows: VecDeque<Row>,
    looks: Looks,
    limit: usize,
    /// Reused by [`Cold::encode`].
    runs: Vec<(u16, u32)>,
}

#[allow(dead_code)]
impl Cold {
    pub fn new(limit: usize) -> Cold {
        Cold {
            rows: VecDeque::new(),
            looks: Looks::new(),
            limit,
            runs: Vec::new(),
        }
    }

    /// Stores a row; returns how many of the oldest rows the limit dropped.
    pub fn push(&mut self, cells: &[SnapCell], used: usize, wrapped: bool) -> usize {
        let row = self.encode(cells, used, wrapped);
        self.rows.push_back(row);
        self.trim()
    }

    /// [`Self::push`] for an emulator row, read in place. Its used width
    /// is where its last cell that is not a default blank ends, which
    /// covers every [`super::written::WRITTEN`] cell.
    pub fn push_cells(&mut self, cells: &[Cell], wrapped: bool) -> usize {
        let row = self.encode(cells, 0, wrapped);
        self.rows.push_back(row);
        self.trim()
    }

    /// Looks interned so far (a hash lookup each), for tests that count them.
    pub fn lookups(&self) -> usize {
        self.looks.lookups()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Row `i` (0 is the oldest) into `out`, padded with default blanks to
    /// `cols` cells.
    pub fn row(&self, i: usize, cols: usize, out: &mut Vec<SnapCell>) -> ColdMeta {
        self.decode(&self.rows[i], cols, out)
    }

    pub fn pop_newest(&mut self, cols: usize, out: &mut Vec<SnapCell>) -> Option<ColdMeta> {
        let row = self.rows.pop_back()?;
        Some(self.decode(&row, cols, out))
    }

    /// Returns how many of the oldest rows the new limit dropped.
    pub fn set_limit(&mut self, limit: usize) -> usize {
        self.limit = limit;
        self.trim()
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.looks = Looks::new();
    }

    pub fn shapes(&self) -> Vec<RowShape> {
        self.rows
            .iter()
            .map(|r| RowShape {
                len: if r.wrapped { r.width } else { r.used } as usize,
                wrapped: r.wrapped,
            })
            .collect()
    }

    /// The newest rows that hold the last `lines` logical lines (the last
    /// of them may run on past the store, into the screen).
    pub fn tail_rows(&self, lines: usize) -> usize {
        let len = self.rows.len();
        let mut seen = 0;
        for i in (0..len).rev() {
            if i == 0 || !self.rows[i - 1].wrapped {
                seen += 1;
                if seen == lines {
                    return len - i;
                }
            }
        }
        len
    }

    /// Re-lays the rows out `cols` wide; returns how many of the oldest
    /// rows the limit then dropped.
    pub fn reflow(&mut self, cols: usize) -> usize {
        let rows: Vec<(Vec<SnapCell>, ColdMeta)> = (0..self.rows.len())
            .map(|i| {
                let mut cells = Vec::new();
                let meta = self.row(i, 0, &mut cells);
                (cells, meta)
            })
            .collect();
        self.clear();
        let mut dropped = 0;
        for (cells, meta) in reflow_rows(&rows, cols) {
            dropped += self.push(&cells, meta.used, meta.wrapped);
        }
        dropped
    }

    /// Heap bytes of the rows and the looks table.
    pub fn bytes(&self) -> usize {
        let rows: usize = self
            .rows
            .iter()
            .map(|r| size_of::<Row>() + r.data.len())
            .sum();
        let looks: usize = (0..self.looks.len() as u32)
            .map(|id| {
                size_of::<SnapCell>() + self.looks.get(id).zerowidth.len() * size_of::<char>()
            })
            .sum();
        rows + looks
    }

    fn trim(&mut self) -> usize {
        let dropped = self.rows.len().saturating_sub(self.limit);
        self.rows.drain(..dropped);
        dropped
    }

    fn encode<C: Look>(&mut self, cells: &[C], used: usize, wrapped: bool) -> Row {
        let last = cells
            .iter()
            .rposition(|c| !c.default_blank())
            .map_or(0, |i| i + 1);
        let stored = used.min(cells.len()).max(last);
        self.runs.clear();
        // One lookup per run of equal look, not per cell.
        let mut i = 0;
        while i < stored {
            let end = C::run_end(cells, i, stored);
            let id = self.looks.intern(&cells[i].look());
            let mut n = end - i;
            if let Some((len, look)) = self.runs.last_mut()
                && *look == id
            {
                let add = n.min(usize::from(u16::MAX - *len));
                *len += add as u16;
                n -= add;
            }
            while n > 0 {
                let take = n.min(usize::from(u16::MAX));
                self.runs.push((take as u16, id));
                n -= take;
            }
            i = end;
        }
        let mut data = Vec::with_capacity(stored + self.runs.len() * RUN);
        let mut ascii = true;
        data.extend(cells[..stored].iter().map(|c| {
            ascii &= c.ch().is_ascii();
            c.ch() as u8
        }));
        if !ascii {
            data.clear();
            for c in &cells[..stored] {
                data.extend_from_slice(c.ch().encode_utf8(&mut [0; 4]).as_bytes());
            }
        }
        let text = data.len() as u32;
        for &(n, id) in &self.runs {
            data.extend_from_slice(&n.to_le_bytes());
            data.extend_from_slice(&id.to_le_bytes());
        }
        Row {
            data: data.into_boxed_slice(),
            text,
            width: cells.len() as u32,
            used: stored as u32,
            wrapped,
        }
    }

    fn decode(&self, row: &Row, cols: usize, out: &mut Vec<SnapCell>) -> ColdMeta {
        out.clear();
        let (text, runs) = row.data.split_at(row.text as usize);
        let mut chars = std::str::from_utf8(text).unwrap_or_default().chars();
        for run in runs.as_chunks::<RUN>().0 {
            let n = u16::from_le_bytes([run[0], run[1]]);
            let id = u32::from_le_bytes([run[2], run[3], run[4], run[5]]);
            for c in chars.by_ref().take(n as usize) {
                let mut cell = self.looks.get(id).clone();
                cell.c = c;
                out.push(cell);
            }
        }
        let blank = self.looks.get(0);
        while out.len() < cols {
            out.push(blank.clone());
        }
        ColdMeta {
            wrapped: row.wrapped,
            used: row.used as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::pane::CellFlags;
    use crate::server::pane::written::WRITTEN;
    use alacritty_terminal::vte::ansi::Color;

    fn row(s: &str) -> Vec<SnapCell> {
        s.chars()
            .map(|c| {
                let mut cell = Looks::new().get(0).clone();
                cell.c = c;
                cell
            })
            .collect()
    }

    fn text(cells: &[SnapCell]) -> String {
        cells.iter().map(|c| c.c).collect()
    }

    #[test]
    fn limit_drops_oldest() {
        let mut c = Cold::new(2);
        assert_eq!(c.push(&row("ab"), 2, false), 0);
        assert_eq!(c.push(&row("cd"), 2, false), 0);
        assert_eq!(c.push(&row("ef"), 2, false), 1);
        assert_eq!(c.len(), 2);
        let mut out = Vec::new();
        c.row(0, 2, &mut out);
        assert_eq!(text(&out), "cd");
    }

    #[test]
    fn row_pads_with_default_blanks() {
        let mut c = Cold::new(2);
        c.push(&row("ab"), 2, false);
        let mut out = Vec::new();
        let meta = c.row(0, 4, &mut out);
        assert_eq!(text(&out), "ab  ");
        assert_eq!(
            meta,
            ColdMeta {
                wrapped: false,
                used: 2
            }
        );
        assert_eq!(out[3], Looks::new().get(0).clone());
    }

    #[test]
    fn shapes_of_wrapped_and_plain() {
        let mut c = Cold::new(5);
        c.push(&row("abcd"), 4, true);
        c.push(&row("ef"), 2, false);
        assert_eq!(
            c.shapes(),
            [
                RowShape {
                    len: 4,
                    wrapped: true
                },
                RowShape {
                    len: 2,
                    wrapped: false
                }
            ]
        );
    }

    #[test]
    fn written_blank_kept() {
        let mut cells = row("ab   ");
        for cell in &mut cells[2..] {
            cell.flags.insert(WRITTEN);
        }
        let mut c = Cold::new(5);
        c.push(&cells, 5, false);
        let mut out = Vec::new();
        let meta = c.row(0, 5, &mut out);
        assert_eq!(meta.used, 5);
        assert_eq!(out, cells);
        assert!(out[4].flags.contains(WRITTEN));
    }

    #[test]
    fn bytes_per_row_budget() {
        let mut c = Cold::new(2000);
        for i in 0..1000 {
            let mut cells = row(&"x".repeat(115));
            for cell in &mut cells {
                cell.fg = Color::Indexed((i % 8) as u8);
            }
            c.push(&cells, 115, false);
        }
        assert!(c.bytes() / 1000 <= 400, "{}", c.bytes() / 1000);
    }

    #[test]
    fn limit_zero_keeps_nothing() {
        let mut c = Cold::new(0);
        assert_eq!(c.push(&row("ab"), 2, false), 1);
        assert_eq!(c.len(), 0);
        let mut out = Vec::new();
        assert_eq!(c.pop_newest(2, &mut out), None);
    }

    #[test]
    fn tail_rows_counts_whole_logical_lines() {
        let mut c = Cold::new(10);
        assert_eq!(c.tail_rows(3), 0);
        c.push(&row("ab"), 2, true);
        c.push(&row("c"), 1, false);
        c.push(&row("d"), 1, false);
        c.push(&row("ef"), 2, true);
        // `ef` runs on into the screen: it is a line of its own here.
        assert_eq!((c.tail_rows(1), c.tail_rows(2), c.tail_rows(3)), (1, 2, 4));
        assert_eq!(c.tail_rows(9), 4);
    }

    #[test]
    fn pop_newest_returns_last_pushed() {
        let mut c = Cold::new(5);
        c.push(&row("ab"), 2, false);
        c.push(&row("cd"), 2, true);
        let mut out = Vec::new();
        let meta = c.pop_newest(3, &mut out);
        assert_eq!(
            meta,
            Some(ColdMeta {
                wrapped: true,
                used: 2
            })
        );
        assert_eq!(text(&out), "cd ");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn wide_char_and_spacer_round_trip() {
        let mut cells = row("a世 b");
        cells[1].flags.insert(CellFlags::WIDE_CHAR);
        cells[2].flags.insert(CellFlags::WIDE_CHAR_SPACER);
        let mut c = Cold::new(5);
        c.push(&cells, 4, false);
        let mut out = Vec::new();
        c.row(0, 4, &mut out);
        assert_eq!(out, cells);
    }

    #[test]
    fn emulator_cells_split_runs_on_every_look_change() {
        use alacritty_terminal::vte::ansi::NamedColor;
        let mut cells: Vec<Cell> = "abcdefg  "
            .chars()
            .map(|c| Cell {
                c,
                ..Cell::default()
            })
            .collect();
        for cell in &mut cells[..7] {
            cell.fg = Color::Named(NamedColor::Red);
        }
        cells[2].bg = Color::Named(NamedColor::Blue);
        cells[3].flags.insert(CellFlags::BOLD);
        cells[4].set_underline_color(Some(Color::Indexed(3)));
        cells[5].fg = Color::Indexed(1);
        cells[8].bg = Color::Named(NamedColor::Green);
        let mut c = Cold::new(5);
        c.push_cells(&cells, false);
        let mut out = Vec::new();
        let meta = c.row(0, 9, &mut out);
        assert_eq!(meta.used, 9);
        let want: Vec<SnapCell> = cells.iter().map(Look::look).collect();
        assert_eq!(out, want);
    }
}
