//! Compact history rows.
//!
//! A row is its text (one char per cell) plus runs of equal look, with the
//! looks interned once for the whole store. Default blanks past the used
//! width are not stored.

use std::collections::VecDeque;
use std::mem::size_of;

use super::SnapCell;
use super::looks::Looks;
use super::marks::RowShape;
use super::reflow::{ColdMeta, reflow_rows};

#[derive(Debug)]
struct Row {
    text: Box<str>,
    /// (length in cells, look id)
    runs: Box<[(u16, u32)]>,
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
}

#[allow(dead_code)]
impl Cold {
    pub fn new(limit: usize) -> Cold {
        Cold {
            rows: VecDeque::new(),
            looks: Looks::new(),
            limit,
        }
    }

    /// Stores a row; returns how many of the oldest rows the limit dropped.
    pub fn push(&mut self, cells: &[SnapCell], used: usize, wrapped: bool) -> usize {
        let row = self.encode(cells, used, wrapped);
        self.rows.push_back(row);
        self.trim()
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

    /// Re-lays the rows out `cols` wide.
    pub fn reflow(&mut self, cols: usize) {
        let rows: Vec<(Vec<SnapCell>, ColdMeta)> = (0..self.rows.len())
            .map(|i| {
                let mut cells = Vec::new();
                let meta = self.row(i, 0, &mut cells);
                (cells, meta)
            })
            .collect();
        let limit = self.limit;
        self.clear();
        for (cells, meta) in reflow_rows(&rows, cols) {
            self.push(&cells, meta.used, meta.wrapped);
        }
        self.limit = limit;
        self.trim();
    }

    /// Heap bytes of the rows and the looks table.
    pub fn bytes(&self) -> usize {
        let rows: usize = self
            .rows
            .iter()
            .map(|r| size_of::<Row>() + r.text.len() + r.runs.len() * size_of::<(u16, u32)>())
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

    fn encode(&mut self, cells: &[SnapCell], used: usize, wrapped: bool) -> Row {
        let ids: Vec<u32> = cells.iter().map(|c| self.looks.intern(c)).collect();
        let is_blank = |i: usize| cells[i].c == ' ' && ids[i] == 0;
        let last = (0..cells.len())
            .rev()
            .find(|&i| !is_blank(i))
            .map_or(0, |i| i + 1);
        let stored = used.min(cells.len()).max(last);
        let mut text = String::with_capacity(stored);
        let mut runs: Vec<(u16, u32)> = Vec::new();
        for (cell, &id) in cells[..stored].iter().zip(&ids) {
            text.push(cell.c);
            match runs.last_mut() {
                Some((n, look)) if *look == id && *n < u16::MAX => *n += 1,
                _ => runs.push((1, id)),
            }
        }
        Row {
            text: text.into(),
            runs: runs.into(),
            width: cells.len() as u32,
            used: stored as u32,
            wrapped,
        }
    }

    fn decode(&self, row: &Row, cols: usize, out: &mut Vec<SnapCell>) -> ColdMeta {
        out.clear();
        let mut chars = row.text.chars();
        for &(n, id) in row.runs.iter() {
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
}
