//! Predictive local echo (spec section 4), mosh style. Pure: no I/O, no threads, time is
//! passed in.
//!
//! The predictor keeps a mirror of the attach output. Keystrokes the server will probably echo
//! are tracked as predictions; once enough of them were confirmed they are also drawn at once,
//! underlined. Every method returns the bytes to write to the user's terminal right away. For
//! `feed_output` those bytes must be written after the server bytes, and the server bytes
//! must end on an escape sequence boundary.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Processor, Timeout};
use unicode_width::UnicodeWidthChar;

mod ansi;

use ansi::{DEFAULT_BG, DEFAULT_FG, PAINTED, cup, escape, push_cell, sgr};

/// Confirmed predictions in a row before they are drawn.
const CONFIDENT_AFTER: u32 = 3;
/// A prediction the server has not confirmed after this long is erased.
const TIMEOUT: Duration = Duration::from_secs(1);

/// The mirror must see bytes as they arrive: no synchronized-update buffering.
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

/// (row, column), both 0-based.
type Pos = (usize, usize);

/// Tracks whether the server byte stream is between escape sequences and UTF-8 chars. The
/// attach stream is cut anywhere, and our own bytes must never land inside a sequence.
#[derive(Clone, Copy, PartialEq)]
enum Scan {
    Ground,
    /// After ESC.
    Esc,
    Csi,
    /// An OSC, DCS, SOS, PM or APC string; the flag is true for OSC (BEL ends it).
    Str(bool),
    /// An ESC inside a string.
    StrEsc(bool),
    /// This many UTF-8 continuation bytes to go.
    Utf8(u8),
}

impl Scan {
    fn at_ground(&self) -> bool {
        *self == Scan::Ground
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        *self = match *self {
            Scan::Utf8(n) if (0x80..=0xbf).contains(&b) => {
                if n > 1 {
                    Scan::Utf8(n - 1)
                } else {
                    Scan::Ground
                }
            }
            Scan::Ground | Scan::Utf8(_) => match b {
                0x1b => Scan::Esc,
                0xc0..=0xdf => Scan::Utf8(1),
                0xe0..=0xef => Scan::Utf8(2),
                0xf0..=0xf7 => Scan::Utf8(3),
                _ => Scan::Ground,
            },
            Scan::Esc => match b {
                b'[' => Scan::Csi,
                b']' => Scan::Str(true),
                b'P' | b'X' | b'^' | b'_' => Scan::Str(false),
                0x1b => Scan::Esc,
                0x20..=0x2f | 0x00..=0x1a | 0x1c..=0x1f => Scan::Esc,
                _ => Scan::Ground,
            },
            Scan::Csi => match b {
                0x1b => Scan::Esc,
                0x18 | 0x1a | 0x40..=0x7e => Scan::Ground,
                _ => Scan::Csi,
            },
            Scan::Str(osc) => match b {
                0x1b => Scan::StrEsc(osc),
                0x18 | 0x1a => Scan::Ground,
                0x07 if osc => Scan::Ground,
                _ => Scan::Str(osc),
            },
            Scan::StrEsc(osc) => match b {
                b'\\' => Scan::Ground,
                0x1b => Scan::StrEsc(osc),
                _ => {
                    let mut next = Scan::Esc;
                    next.step(b);
                    next
                }
            },
        };
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A char of the given display width drawn at `before`.
    Char(char, usize),
    /// Backspace: the cell at `after` becomes blank.
    Back,
    /// Left or right arrow.
    Move,
}

struct Pred {
    kind: Kind,
    before: Pos,
    after: Pos,
    /// The cell as the user would have seen it, predictions included.
    prev: char,
    /// The cell in the mirror when the prediction was made.
    mirror_prev: char,
    at: Instant,
    shown: bool,
}

impl Pred {
    /// The cell this prediction writes and what it writes there.
    fn cell(&self) -> Option<(Pos, char)> {
        match self.kind {
            Kind::Char(c, _) => Some((self.before, c)),
            Kind::Back => Some((self.after, ' ')),
            Kind::Move => None,
        }
    }
}

enum Status {
    Confirmed,
    Pending,
    Mismatch,
}

enum Key {
    Char(char),
    Back,
    Left,
    Right,
    Paste(bool),
    Flush,
}

pub struct Predictor {
    term: Term<VoidListener>,
    parser: Processor<NoSync>,
    cols: usize,
    enabled: bool,
    echo_hint: bool,
    pending: VecDeque<Pred>,
    confirmed: u32,
    /// Start of a UTF-8 char or an escape sequence split across two `on_input` calls.
    carry: Vec<u8>,
    /// Between bracketed paste markers: typed text is not predicted.
    in_paste: bool,
    /// Where the server stream stands; nothing may be written to the terminal mid-sequence.
    scan: Scan,
    /// The user's terminal may differ from the mirror: repaint it at the next safe moment.
    owed: bool,
    /// Same, but only for the main screen, which the alt screen is hiding.
    owed_main: bool,
    /// The terminal cursor is where a prediction left it, not at the mirror's cursor.
    displaced: bool,
}

impl Predictor {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        Predictor {
            term: Term::new(config, &Size { cols, rows }, VoidListener),
            parser: Processor::new(),
            cols,
            enabled: true,
            echo_hint: true,
            pending: VecDeque::new(),
            confirmed: 0,
            carry: Vec::new(),
            in_paste: false,
            scan: Scan::Ground,
            owed: false,
            owed_main: false,
            displaced: false,
        }
    }

    /// The client screen changed size. Pending predictions are forgotten; the caller redraws.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        self.term.resize(Size { cols, rows });
        self.cols = cols;
        self.pending.clear();
        self.confirmed = 0;
        self.owed = false;
        self.owed_main = false;
        self.displaced = false;
    }

    /// `auto` or `on` (true), `off` or not remote (false). Returns the erase of anything shown.
    pub fn set_enabled(&mut self, on: bool) -> Vec<u8> {
        self.enabled = on;
        self.rollback()
    }

    /// The server's `pane-echo` hint: false while the pane's pty has echo off.
    pub fn set_echo_hint(&mut self, on: bool) -> Vec<u8> {
        self.echo_hint = on;
        self.rollback()
    }

    /// Write these bytes to the terminal before the server bytes of the next `feed_output`:
    /// the server's output lands at the mirror's cursor, not where a prediction left it.
    pub fn before_output(&mut self) -> Vec<u8> {
        if !self.displaced || !self.scan.at_ground() {
            return Vec::new();
        }
        self.displaced = false;
        self.restore_cursor()
    }

    /// Server output for the attach channel. Updates the mirror, confirms or invalidates
    /// predictions. Write the bytes it returns after the server bytes (and after the bytes of
    /// `before_output`). Nothing is returned while the stream is inside an escape sequence.
    pub fn feed_output(&mut self, bytes: &[u8], _now: Instant) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        self.scan.feed(bytes);
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        if !alt && self.owed_main {
            self.owed_main = false;
            self.owed = true;
        }
        if alt {
            return self.rollback();
        }
        if !self.scan.at_ground() {
            return Vec::new();
        }
        let cursor = self.mirror_cursor();
        let mut done = Vec::new();
        let mut failed = false;
        while let Some(front) = self.pending.front() {
            match self.status(front, cursor) {
                Status::Confirmed => {
                    self.confirmed += 1;
                    done.extend(self.pending.pop_front());
                }
                Status::Pending => break,
                Status::Mismatch => {
                    failed = true;
                    break;
                }
            }
        }
        if !failed && !self.pending.is_empty() {
            // Valid cursors: before any pending prediction, or after the last one.
            let after_last = self.pending.back().map(|p| p.after);
            failed = !self
                .pending
                .iter()
                .map(|p| p.before)
                .chain(after_last)
                .any(|p| p == cursor);
        }
        if failed {
            self.confirmed = 0;
            done.extend(self.pending.drain(..));
        }
        let mut out = self.repaint(&done);
        if self.pending.iter().any(|p| p.shown) {
            out.extend(cup(self.cursor_now()));
            self.displaced = true;
        }
        out
    }

    /// Local keystrokes. Returns what to draw now: predicted cells, underlined.
    pub fn on_input(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        for key in self.parse(bytes) {
            if matches!(key, Key::Flush) || !self.can_predict() {
                out.extend(self.rollback());
            } else if let Some(draw) = self.predict(key, now) {
                out.extend(draw);
            } else {
                out.extend(self.rollback());
            }
        }
        out
    }

    /// Call from the client's timer. Erases predictions unconfirmed for 1 s.
    pub fn tick(&mut self, now: Instant) -> Vec<u8> {
        match self.pending.front() {
            Some(p) if now.saturating_duration_since(p.at) >= TIMEOUT => self.rollback(),
            // A repaint owed from mid-sequence output.
            _ => self.repaint(&[]),
        }
    }

    // ---- input ----

    fn parse(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            let b = buf[i];
            match b {
                0x1b => {
                    let Some((key, len)) = escape(&buf[i..]) else {
                        // Cut off by the end of the read: flush now, finish it next time.
                        keys.push(Key::Flush);
                        self.carry = buf[i..].to_vec();
                        break;
                    };
                    match key {
                        Key::Paste(on) => {
                            self.in_paste = on;
                            keys.push(Key::Flush);
                        }
                        _ if self.in_paste => keys.push(Key::Flush),
                        key => keys.push(key),
                    }
                    i += len;
                }
                _ if self.in_paste => {
                    keys.push(Key::Flush);
                    i += 1;
                }
                0x08 | 0x7f => {
                    keys.push(Key::Back);
                    i += 1;
                }
                0x20..=0x7e => {
                    keys.push(Key::Char(b as char));
                    i += 1;
                }
                0x00..=0x1f => {
                    keys.push(Key::Flush);
                    i += 1;
                }
                _ => {
                    let need = match b {
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf7 => 4,
                        _ => 1,
                    };
                    let have = buf.len() - i;
                    let tail = &buf[i + 1..i + need.min(have)];
                    if tail.iter().any(|b| !(0x80..=0xbf).contains(b)) {
                        // Not UTF-8: leave the following bytes (maybe an ESC) alone.
                        keys.push(Key::Flush);
                        i += 1;
                        continue;
                    }
                    if need > have {
                        self.carry = buf[i..].to_vec();
                        break;
                    }
                    let c = std::str::from_utf8(&buf[i..i + need])
                        .ok()
                        .and_then(|s| s.chars().next());
                    match c {
                        Some(c) if !c.is_control() => keys.push(Key::Char(c)),
                        _ => keys.push(Key::Flush),
                    }
                    i += need;
                }
            }
        }
        keys
    }

    fn can_predict(&self) -> bool {
        self.enabled
            && self.echo_hint
            && !self
                .term
                .mode()
                .intersects(TermMode::ALT_SCREEN | TermMode::ORIGIN | TermMode::INSERT)
            && !self.password_prompt()
    }

    /// The cursor's logical line (soft-wrapped rows joined) ends in a password prompt.
    fn password_prompt(&self) -> bool {
        let mut row = self.mirror_cursor().0;
        let mut line = self.row_text(row);
        while row > 0 && self.wraps(row - 1) {
            row -= 1;
            line.insert_str(0, &self.row_text(row));
        }
        let line = line.trim_end().to_lowercase();
        line.ends_with("assword:") || line.contains("password for") || line.contains("passphrase")
    }

    fn row_text(&self, row: usize) -> String {
        (0..self.cols)
            .filter_map(|c| {
                let cell = &self.term.grid()[Line(row as i32)][Column(c)];
                (!cell.flags.contains(Flags::WIDE_CHAR_SPACER)).then_some(cell.c)
            })
            .collect()
    }

    /// Row `row` continues on the next one.
    fn wraps(&self, row: usize) -> bool {
        let cell = &self.term.grid()[Line(row as i32)][Column(self.cols - 1)];
        cell.flags.contains(Flags::WRAPLINE)
    }

    /// Track (and maybe draw) one key. None means it cannot be predicted: flush.
    fn predict(&mut self, key: Key, now: Instant) -> Option<Vec<u8>> {
        let before = self.cursor_now();
        let (row, col) = before;
        let (kind, after) = match key {
            Key::Char(c) => {
                let w = c.width().filter(|w| *w >= 1)?;
                if col + w >= self.cols {
                    return None;
                }
                (Kind::Char(c, w), (row, col + w))
            }
            Key::Back if col > 0 => (Kind::Back, (row, col - 1)),
            Key::Left if col > 0 => (Kind::Move, (row, col - 1)),
            Key::Right if col + 1 < self.cols && self.virtual_char(before) != ' ' => {
                (Kind::Move, (row, col + 1))
            }
            _ => return None,
        };
        let cell_pos = match kind {
            Kind::Char(..) => before,
            _ => after,
        };
        let shown = self.confirmed >= CONFIDENT_AFTER && self.scan.at_ground();
        let pred = Pred {
            kind,
            before,
            after,
            prev: self.virtual_char(cell_pos),
            mirror_prev: self.mirror_char(cell_pos),
            at: now,
            shown,
        };
        let draw = shown.then(|| self.draw(&pred));
        self.displaced |= shown;
        self.pending.push_back(pred);
        Some(draw.unwrap_or_default())
    }

    // ---- mirror ----

    fn mirror_cursor(&self) -> Pos {
        let p = self.term.grid().cursor.point;
        (p.line.0.max(0) as usize, p.column.0)
    }

    fn mirror_char(&self, (row, col): Pos) -> char {
        self.term.grid()[Line(row as i32)][Column(col)].c
    }

    /// Where the cursor is, counting pending predictions.
    fn cursor_now(&self) -> Pos {
        self.pending
            .back()
            .map_or_else(|| self.mirror_cursor(), |p| p.after)
    }

    fn virtual_char(&self, pos: Pos) -> char {
        self.pending
            .iter()
            .rev()
            .find_map(|p| p.cell().filter(|(at, _)| *at == pos).map(|(_, c)| c))
            .unwrap_or_else(|| self.mirror_char(pos))
    }

    fn status(&self, p: &Pred, cursor: Pos) -> Status {
        let Some((pos, ch)) = p.cell() else {
            return if cursor == p.after {
                Status::Confirmed
            } else if cursor == p.before {
                Status::Pending
            } else {
                Status::Mismatch
            };
        };
        let m = self.mirror_char(pos);
        if m == ch && (p.mirror_prev != ch || cursor != p.before) {
            Status::Confirmed
        } else if m == ch || m == p.mirror_prev || m == p.prev {
            Status::Pending
        } else {
            Status::Mismatch
        }
    }

    // ---- output ----

    /// Forget every prediction, go tentative, and repaint what was shown.
    fn rollback(&mut self) -> Vec<u8> {
        self.confirmed = 0;
        let gone: Vec<Pred> = self.pending.drain(..).collect();
        self.repaint(&gone)
    }

    /// If any of `preds` was drawn (or a repaint is still owed), paint the whole screen from
    /// the mirror and put the cursor and pen where the mirror's are. Predictions are drawn at
    /// fixed screen cells, and the server may have scrolled, moved or cleared them since, so
    /// no smaller repaint is safe.
    // ponytail: a few KB per resolved prediction; per-row diffing against a shadow of the
    // terminal would shrink it.
    fn repaint(&mut self, preds: &[Pred]) -> Vec<u8> {
        if preds.iter().any(|p| p.shown) {
            if self.term.mode().contains(TermMode::ALT_SCREEN) {
                // The main screen is hidden; it is repainted on the way back.
                self.owed_main = true;
            } else {
                self.owed = true;
            }
        }
        if !self.owed || !self.scan.at_ground() {
            return Vec::new();
        }
        self.owed = false;
        self.displaced = false;
        let mut out = Vec::new();
        if self.term.mode().contains(TermMode::INSERT) {
            out.extend(b"\x1b[4l");
        }
        let rows = self.term.screen_lines();
        for row in 0..rows {
            out.extend(self.paint_row(row));
        }
        if self.term.mode().contains(TermMode::INSERT) {
            out.extend(b"\x1b[4h");
        }
        out.extend(self.restore_cursor());
        out
    }

    fn paint_row(&self, row: usize) -> Vec<u8> {
        let grid = self.term.grid();
        let default = (Flags::empty(), DEFAULT_FG, DEFAULT_BG);
        let cells: Vec<_> = (0..self.cols)
            .map(|c| &grid[Line(row as i32)][Column(c)])
            .collect();
        let style = |c: &alacritty_terminal::term::cell::Cell| (c.flags & PAINTED, c.fg, c.bg);
        let blank = |c: &&alacritty_terminal::term::cell::Cell| {
            c.c == ' ' && style(c) == default && c.zerowidth().is_none()
        };
        let used = cells.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1);
        let mut out = cup((row, 0));
        out.extend(b"\x1b[0m");
        let mut cur = default;
        for cell in &cells[..used] {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            if style(cell) != cur {
                cur = style(cell);
                out.extend(sgr(cell.flags, cell.fg, cell.bg).bytes());
            }
            push_cell(&mut out, cell);
        }
        if used < self.cols {
            if cur != default {
                out.extend(b"\x1b[0m");
            }
            out.extend(b"\x1b[K");
        }
        out
    }

    /// The pen the server left, then the cursor where the mirror's is.
    fn restore_cursor(&self) -> Vec<u8> {
        let cursor = &self.term.grid().cursor;
        let (row, col) = self.mirror_cursor();
        let mut out = Vec::new();
        if cursor.input_needs_wrap {
            // The next char must wrap: only writing into the last column sets that state.
            let mut at = col;
            if self.term.grid()[Line(row as i32)][Column(at)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
            {
                at = at.saturating_sub(1);
            }
            let cell = &self.term.grid()[Line(row as i32)][Column(at)];
            out.extend(cup((row, at)));
            out.extend(b"\x1b[0m");
            out.extend(sgr(cell.flags, cell.fg, cell.bg).bytes());
            push_cell(&mut out, cell);
            out.extend(self.pen().bytes());
        } else {
            out.extend(self.pen().bytes());
            out.extend(cup((row, col)));
        }
        out
    }

    /// The mirror's current SGR state.
    fn pen(&self) -> String {
        let t = &self.term.grid().cursor.template;
        sgr(t.flags, t.fg, t.bg)
    }

    /// What to write for one shown prediction.
    fn draw(&self, p: &Pred) -> Vec<u8> {
        let mut out = Vec::new();
        match p.kind {
            Kind::Char(c, _) => {
                out.extend(cup(p.before));
                out.extend(b"\x1b[0;4m");
                let mut utf8 = [0; 4];
                out.extend(c.encode_utf8(&mut utf8).bytes());
            }
            Kind::Back => {
                out.extend(cup(p.after));
                out.extend(b"\x1b[0m ");
            }
            Kind::Move => {}
        }
        out.extend(self.pen().bytes());
        out.extend(cup(p.after));
        out
    }
}

#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;
