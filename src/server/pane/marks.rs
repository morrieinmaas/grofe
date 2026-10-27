//! OSC 133 marks: which lines of a pane's grid start a prompt (`A`) or the
//! output of a command (`C`), for copy mode's `next-prompt` and
//! `previous-prompt` (tmux 3.7b's `GRID_LINE_START_PROMPT` and
//! `GRID_LINE_START_OUTPUT`, set by `input_osc_133`).
//!
//! alacritty's grid has no spare line flag and the one free cell flag is
//! taken ([`super::written::WRITTEN`]), so the marks live beside it. A mark
//! is keyed by `origin + top`, where `top` counts rows from the oldest row
//! of the history. Scrolling a line into history leaves its `top` as it
//! was, so most of what a grid does needs no work here; the rest is done
//! where it happens:
//!
//! - history that outgrows `history-limit` loses its oldest rows, which
//!   raises `origin` ([`Marks::sync`]; the emulator is given
//!   [`HISTORY_SLACK`] rows more than the limit, so that this module, not
//!   alacritty, drops the rows and so counts them);
//! - `ED 3`, which drops the history, does the same ([`Marks::trim`]);
//! - a line cleared the way tmux resets a grid line's flags loses its mark
//!   (`written.rs` decides which clears);
//! - a resize that changes the width moves the marks as tmux's
//!   `grid_reflow` moves a line's flags ([`Marks::resize`]).
//!
//! Not followed: a line moved by a scroll region, `IL` or `DL` keeps its
//! `top` and so loses its mark to whatever takes its place; and a resize
//! while the alternate screen is up cannot see the normal screen's rows, so
//! it drops every mark.

use std::collections::BTreeMap;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;

use super::CellFlags;
use super::written;

/// `GRID_LINE_START_PROMPT`: `OSC 133 ; A`.
pub const PROMPT: u8 = 1;
/// `GRID_LINE_START_OUTPUT`: `OSC 133 ; C`.
pub const OUTPUT: u8 = 2;

/// The private modes the reader sets, in-band, to put a mark in the emulator's
/// stream (`CSI ? 65533 h` for a prompt, `65534` for output). No program
/// uses them.
pub const MARK_PROMPT: u16 = 65533;
pub const MARK_OUTPUT: u16 = 65534;

/// Rows of history the emulator keeps beyond `history-limit` between two
/// calls to [`Marks::sync`]. More than one operation can push (a scroll of
/// the whole screen, `ED 2`), so it exceeds any screen's height.
pub const HISTORY_SLACK: usize = 4096;

/// The marks of one pane's normal screen.
#[derive(Debug, Default)]
pub struct Marks {
    /// `history-limit`.
    limit: usize,
    /// Rows dropped from the top of the grid so far.
    origin: u64,
    /// Mark bits by `origin + top`.
    map: BTreeMap<u64, u8>,
}

/// A grid row as a reflow sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowShape {
    /// The cells it holds: all of them when it wraps, else its used width.
    pub len: usize,
    /// It runs on into the next row.
    pub wrapped: bool,
}

fn alt<L: EventListener>(term: &Term<L>) -> bool {
    term.mode().contains(TermMode::ALT_SCREEN)
}

impl Marks {
    pub fn new(limit: usize) -> Marks {
        Marks {
            limit,
            ..Marks::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The mark bits of the row `top` rows below the history's start.
    pub fn get(&self, top: usize) -> u8 {
        self.map
            .get(&(self.origin + top as u64))
            .copied()
            .unwrap_or(0)
    }

    pub fn set(&mut self, top: usize, bits: u8) {
        *self.map.entry(self.origin + top as u64).or_default() |= bits;
    }

    /// Resets the row's marks, as tmux's `grid_clear_lines` resets its flags.
    pub fn erase(&mut self, top: usize) {
        self.map.remove(&(self.origin + top as u64));
    }

    /// [`Self::erase`] for the rows `from..to`.
    pub fn erase_range(&mut self, from: usize, to: usize) {
        if from >= to || self.map.is_empty() {
            return;
        }
        let (from, to) = (self.origin + from as u64, self.origin + to as u64);
        let keys: Vec<u64> = self.map.range(from..to).map(|(k, _)| *k).collect();
        for k in keys {
            self.map.remove(&k);
        }
    }

    /// [`Self::erase`] for the rows `from` and after.
    pub fn erase_from(&mut self, from: usize) {
        self.map.split_off(&(self.origin + from as u64));
    }

    /// The `n` oldest rows are gone.
    pub fn trim(&mut self, n: usize) {
        self.origin += n as u64;
        let keep = self.map.split_off(&self.origin);
        self.map = keep;
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Row `top` of the grid the cursor is on.
    fn cursor_top<L: EventListener>(term: &Term<L>) -> usize {
        let grid = term.grid();
        grid.history_size() + grid.cursor.point.line.0.max(0) as usize
    }

    /// Records `bits` for the cursor's line (`input_osc_133`). The alternate
    /// screen has its own grid in tmux, which nothing reads.
    pub fn record<L: EventListener>(&mut self, term: &Term<L>, bits: u8) {
        if !alt(term) {
            self.set(Self::cursor_top(term), bits);
        }
    }

    /// Holds the history to `history-limit`, counting the rows it drops.
    pub fn sync<L: EventListener>(&mut self, term: &mut Term<L>) {
        if alt(term) {
            return;
        }
        let history = term.grid().history_size();
        if history > self.limit {
            let grid = term.grid_mut();
            grid.update_history(self.limit);
            grid.update_history(self.limit.saturating_add(HISTORY_SLACK));
            self.trim(history - self.limit);
        }
    }

    /// `term.resize(size)`, with the marks following their lines.
    pub fn resize<L: EventListener>(&mut self, term: &mut Term<L>, size: impl Dimensions) {
        let cols = size.columns();
        let reflows = cols != term.columns() && !self.map.is_empty();
        if reflows && alt(term) {
            self.map.clear();
        }
        let before = (reflows && !alt(term)).then(|| shapes(term));
        term.resize(size);
        if let Some(before) = before {
            let after = shapes(term);
            let full = term.grid().history_size() >= self.limit.saturating_add(HISTORY_SLACK);
            let dropped = if full {
                starts(&before).len().saturating_sub(starts(&after).len())
            } else {
                0
            };
            self.reflow(&before, &after, cols, dropped);
        }
        if !alt(term) {
            self.erase_from(term.grid().history_size() + term.screen_lines());
        }
        self.sync(term);
    }

    /// Moves every mark from where `before` had its row to where tmux's
    /// `grid_reflow` puts its flags in `after`, `cols` wide (`dropped`
    /// logical lines having gone off the top meanwhile).
    pub fn reflow(&mut self, before: &[RowShape], after: &[RowShape], cols: usize, dropped: usize) {
        let (sb, sa) = (starts(before), starts(after));
        let old = std::mem::take(&mut self.map);
        for (key, bits) in old {
            let top = (key - self.origin) as usize;
            if let Some(to) = reflowed(before, &sb, after, &sa, cols, dropped, top) {
                self.set(to, bits);
            }
        }
    }
}

/// The first row of each logical line.
fn starts(rows: &[RowShape]) -> Vec<usize> {
    (0..rows.len())
        .filter(|&i| i == 0 || !rows[i - 1].wrapped)
        .collect()
}

/// Where tmux's `grid_reflow` leaves the flags of row `top` of `before`.
///
/// A flag belongs to its line, not to a cell. A line that starts a logical
/// line keeps it, on the first piece it is split into. A line that
/// continues one starts `s` cells in, and the row the previous line's join
/// filled up to a multiple of `cols` has taken its first cells: what is
/// left starts the next row (so its flags are lost if nothing is left, as
/// when the join takes the whole line).
fn reflowed(
    before: &[RowShape],
    sb: &[usize],
    after: &[RowShape],
    sa: &[usize],
    cols: usize,
    dropped: usize,
    top: usize,
) -> Option<usize> {
    let line = sb.partition_point(|&s| s <= top).checked_sub(1)?;
    let first = sb[line];
    let s: usize = before[first..top].iter().map(|r| r.len).sum();
    let e = s + before.get(top)?.len;
    let at = if top == first {
        0
    } else if s.is_multiple_of(cols) {
        if e > s { s } else { return None }
    } else {
        let next = s.next_multiple_of(cols);
        if next < e { next } else { return None }
    };
    let line = line.checked_sub(dropped)?;
    let start = *sa.get(line)?;
    let end = sa.get(line + 1).copied().unwrap_or(after.len());
    let row = start + at / cols;
    (row < end).then_some(row)
}

/// Every row of the grid, oldest first.
fn shapes<L: EventListener>(term: &Term<L>) -> Vec<RowShape> {
    let grid = term.grid();
    let cols = grid.columns();
    (-(grid.history_size() as i32)..grid.screen_lines() as i32)
        .map(|i| {
            let row = &grid[Line(i)];
            let wrapped = row[Column(cols - 1)].flags.contains(CellFlags::WRAPLINE);
            RowShape {
                len: if wrapped { cols } else { written::used(row) },
                wrapped,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(len: usize, wrapped: bool) -> RowShape {
        RowShape { len, wrapped }
    }

    /// `text` split into rows `cols` wide, as a terminal wraps it.
    fn wrap(len: usize, cols: usize) -> Vec<RowShape> {
        let mut rows = Vec::new();
        let mut left = len;
        while left > cols {
            rows.push(row(cols, true));
            left -= cols;
        }
        rows.push(row(left, false));
        rows
    }

    #[test]
    fn marks_follow_their_rows_as_the_origin_moves() {
        let mut m = Marks::new(10);
        m.set(3, PROMPT);
        m.set(5, OUTPUT);
        m.set(5, PROMPT);
        assert_eq!((m.get(3), m.get(5), m.get(4)), (PROMPT, PROMPT | OUTPUT, 0));
        m.trim(4);
        assert_eq!((m.get(0), m.get(1)), (0, PROMPT | OUTPUT));
        assert_eq!(m.map.len(), 1, "the mark of a dropped row is gone");
        m.erase(1);
        assert!(m.is_empty());
    }

    #[test]
    fn erase_range_is_half_open() {
        let mut m = Marks::new(10);
        for t in 0..6 {
            m.set(t, PROMPT);
        }
        m.erase_range(2, 4);
        let kept: Vec<usize> = (0..6).filter(|&t| m.get(t) != 0).collect();
        assert_eq!(kept, [0, 1, 4, 5]);
    }

    /// Recorded from tmux 3.7b (tests/fixtures/compat/golden/tm_1): a mark
    /// on a logical line's first row stays on the first piece when the line
    /// is split or joined.
    #[test]
    fn a_first_row_keeps_its_mark_through_a_reflow() {
        for to in [10, 7, 30] {
            let before = wrap(25, 20);
            let after = wrap(25, to);
            let mut m = Marks::new(100);
            m.set(0, PROMPT);
            m.reflow(&before, &after, to, 0);
            assert_eq!(m.get(0), PROMPT, "{to} columns");
        }
    }

    /// tmux 3.7b, a mark in the second of three 10-column rows
    /// (`abcdefghij` `klmnopqrst` `u`) after the width becomes 5, 7 and 30.
    #[test]
    fn a_continuation_row_keeps_its_mark_on_what_is_left_of_it() {
        let before = wrap(21, 10);
        for (cols, row) in [(5, Some(2)), (7, Some(2)), (30, None)] {
            let after = wrap(21, cols);
            let mut m = Marks::new(100);
            m.set(1, PROMPT);
            m.reflow(&before, &after, cols, 0);
            let got: Vec<usize> = (0..after.len()).filter(|&t| m.get(t) != 0).collect();
            assert_eq!(got, row.into_iter().collect::<Vec<_>>(), "{cols} columns");
        }
    }

    #[test]
    fn lines_keep_their_place_among_the_other_lines() {
        // A wrapped line (rows 0 and 1), a short one, an empty one.
        let before = vec![row(10, true), row(4, false), row(3, false), row(0, false)];
        let after = vec![row(14, false), row(3, false), row(0, false)];
        let mut m = Marks::new(100);
        m.set(2, PROMPT);
        m.set(3, OUTPUT);
        m.reflow(&before, &after, 20, 0);
        assert_eq!((m.get(1), m.get(2), m.get(0)), (PROMPT, OUTPUT, 0));
        // The same, with one logical line gone off the top.
        let mut m = Marks::new(100);
        m.set(0, PROMPT);
        m.set(2, OUTPUT);
        m.reflow(&before, &after, 20, 1);
        assert_eq!((m.get(0), m.is_empty()), (OUTPUT, false));
        assert_eq!(m.map.len(), 1);
    }

    /// The emulator with its marks, fed as a pane reader feeds it.
    mod emulator {
        use alacritty_terminal::term::Config;
        use alacritty_terminal::vte::ansi::Processor;

        use super::super::super::io::Listener;
        use super::super::super::written::Written;
        use super::*;

        pub struct Size(pub usize, pub usize);

        impl Dimensions for Size {
            fn total_lines(&self) -> usize {
                self.0
            }
            fn screen_lines(&self) -> usize {
                self.0
            }
            fn columns(&self) -> usize {
                self.1
            }
        }

        pub struct Emu {
            pub term: Term<Listener>,
            parser: Processor,
            pub marks: Marks,
        }

        impl Emu {
            pub fn new(rows: usize, cols: usize, limit: usize) -> Emu {
                let config = Config {
                    scrolling_history: limit + HISTORY_SLACK,
                    ..Config::default()
                };
                Emu {
                    term: Term::new(config, &Size(rows, cols), Listener(Default::default())),
                    parser: Processor::new(),
                    marks: Marks::new(limit),
                }
            }

            /// `text` with `\x01` standing for `OSC 133 ; A` and `\x02` for
            /// `OSC 133 ; C`.
            pub fn feed(&mut self, text: &str) {
                let mut rest = text;
                while let Some(at) = rest.find(['\x01', '\x02']) {
                    let (head, tail) = rest.split_at(at);
                    let bits = if tail.starts_with('\x01') {
                        PROMPT
                    } else {
                        OUTPUT
                    };
                    self.parser.advance(
                        &mut Written(&mut self.term, &mut self.marks, None),
                        head.as_bytes(),
                    );
                    self.marks.record(&self.term, bits);
                    rest = &tail[1..];
                }
                self.parser.advance(
                    &mut Written(&mut self.term, &mut self.marks, None),
                    rest.as_bytes(),
                );
            }

            /// Bytes as they come from the pty, in-band marks included.
            pub fn raw(&mut self, bytes: &[u8]) {
                self.parser
                    .advance(&mut Written(&mut self.term, &mut self.marks, None), bytes);
            }

            pub fn history(&self) -> usize {
                self.term.grid().history_size()
            }

            /// The text of each marked row, oldest first, with `*` for a
            /// prompt start and `+` for an output start before it.
            pub fn marked(&self) -> Vec<String> {
                let grid = self.term.grid();
                let h = grid.history_size() as i32;
                (-h..grid.screen_lines() as i32)
                    .filter_map(|i| {
                        let bits = self.marks.get((i + h) as usize);
                        (bits != 0).then(|| {
                            let text: String = (0..grid.columns())
                                .map(|c| grid[Line(i)][Column(c)].c)
                                .collect();
                            let tag = if bits & PROMPT != 0 { "*" } else { "" };
                            let tag2 = if bits & OUTPUT != 0 { "+" } else { "" };
                            format!("{tag}{tag2}{}", text.trim_end())
                        })
                    })
                    .collect()
            }
        }
    }

    use emulator::{Emu, Size};

    #[test]
    fn a_mark_stays_on_its_line_as_the_line_scrolls_into_history() {
        let mut e = Emu::new(3, 10, 100);
        e.feed("\x01one\r\n\x02two\r\nthree\r\n\x01four\r\nfive");
        assert_eq!(e.history(), 2);
        assert_eq!(e.marked(), ["*one", "+two", "*four"]);
    }

    #[test]
    fn history_past_the_limit_takes_its_marks_with_it() {
        let mut e = Emu::new(3, 10, 4);
        for i in 1..=12 {
            e.feed(&format!("\x01l{i}\r\n"));
        }
        e.feed("end");
        // 12 lines and the cursor row: 4 of history and the 3 on screen.
        assert_eq!(e.history(), 4);
        assert_eq!(e.marked(), ["*l7", "*l8", "*l9", "*l10", "*l11", "*l12"]);
    }

    #[test]
    fn dropping_the_history_keeps_the_screens_marks() {
        let mut e = Emu::new(3, 10, 100);
        e.feed("\x01a\r\n\x01b\r\n\x01c\r\n\x01d\r\n\x01e");
        assert_eq!(e.marked(), ["*a", "*b", "*c", "*d", "*e"]);
        e.feed("\x1b[3J");
        assert_eq!(e.history(), 0);
        assert_eq!(e.marked(), ["*c", "*d", "*e"]);
        e.feed("\r\nf\r\ng");
        assert_eq!(
            (e.history(), e.marked()),
            (2, ["*c", "*d", "*e"].map(String::from).to_vec())
        );
    }

    #[test]
    fn clears_that_empty_a_whole_line_take_its_marks() {
        let mut e = Emu::new(5, 10, 100);
        // A line with cells cleared whole loses its mark; one without
        // none (tmux's `screen_write_clearline` returns early).
        e.feed("abc\r\x01\x1b[Kx\r\n\x01\x1b[2Ky\r\n");
        assert_eq!(e.marked(), ["*y"]);
        // A partial clear keeps it.
        let mut e = Emu::new(5, 10, 100);
        e.feed("abc\x01\x1b[1Kx");
        assert_eq!(e.marked(), ["*   x"]);
    }

    #[test]
    fn ed_2_moves_the_used_lines_into_history_and_clears_the_rest() {
        let mut e = Emu::new(4, 10, 100);
        e.feed("\x01a\r\n\x01b\r\n\x01");
        e.feed("\x1b[2J");
        assert_eq!(e.history(), 2);
        assert_eq!(e.marked(), ["*a", "*b"]);
    }

    #[test]
    fn a_resize_moves_marks_as_tmux_reflows_its_lines() {
        // tmux 3.7b: a mark in the second of three 10-column rows
        // (`abcdefghij` `klmnopqrst` `u`) is on the row of `opqrstu` at 7
        // columns, on `klmno` at 5, and lost at 30.
        for (cols, kept) in [(7, "*opqrstu"), (5, "*klmno"), (30, "")] {
            let mut e = Emu::new(8, 10, 100);
            e.feed("abcdefghijkl\x01mnopqrstu\r\nzz");
            assert_eq!(e.marked(), ["*klmnopqrst"]);
            e.marks.resize(&mut e.term, Size(8, cols));
            let got = e.marked();
            assert_eq!(got.first().map_or("", String::as_str), kept, "{cols}");
        }
    }

    #[test]
    fn a_resize_on_the_alternate_screen_drops_the_marks_it_cannot_see() {
        let mut e = Emu::new(5, 10, 100);
        e.feed("abcdefghijkl\x01mn\x1b[?1049h");
        e.marks.resize(&mut e.term, Size(5, 30));
        e.feed("\x1b[?1049l");
        assert!(e.marks.is_empty());
    }

    /// A synchronized update holds its bytes back; a mark inside it is
    /// made when they are applied, on the line the cursor is then on.
    #[test]
    fn a_mark_inside_a_synchronized_update_lands_on_its_line() {
        let mut e = Emu::new(5, 10, 100);
        e.raw(b"a\r\n\x1b[?2026hb\r\nc\r\n\x1b[?65533hd\x1b[?2026l");
        assert_eq!(e.marked(), ["*d"]);
        e.raw(b"\r\n\x1b[?65534h");
        assert_eq!(e.marked(), ["*d", "+"]);
    }

    #[test]
    fn the_in_band_marker_bytes_match_their_modes() {
        assert_eq!(MARK_PROMPT, 65533);
        assert_eq!(MARK_OUTPUT, 65534);
    }

    #[test]
    fn marks_are_not_made_on_the_alternate_screen() {
        let mut e = Emu::new(5, 10, 100);
        e.feed("\x1b[?1049h\x01alt\x1b[?1049l");
        assert!(e.marks.is_empty());
    }
}
