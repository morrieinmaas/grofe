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
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Timeout};
use unicode_width::UnicodeWidthChar;

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

fn push_cell(out: &mut Vec<u8>, cell: &alacritty_terminal::term::cell::Cell) {
    let mut utf8 = [0; 4];
    out.extend(cell.c.encode_utf8(&mut utf8).bytes());
    for z in cell.zerowidth().into_iter().flatten() {
        out.extend(z.encode_utf8(&mut utf8).bytes());
    }
}

const DEFAULT_FG: Color = Color::Named(NamedColor::Foreground);
const DEFAULT_BG: Color = Color::Named(NamedColor::Background);
/// The attributes `sgr` can express.
const PAINTED: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// The key at the start of `buf` (an ESC) and how many bytes it takes; None if the sequence
/// is cut off by the end of `buf`.
fn escape(buf: &[u8]) -> Option<(Key, usize)> {
    match buf.get(1)? {
        b'[' => {
            let mut j = 2;
            while buf.get(j).is_some_and(|b| (0x20..=0x3f).contains(b)) {
                j += 1;
            }
            buf.get(j)?;
            let key = match &buf[2..=j] {
                b"D" => Key::Left,
                b"C" => Key::Right,
                b"200~" => Key::Paste(true),
                b"201~" => Key::Paste(false),
                _ => Key::Flush,
            };
            Some((key, j + 1))
        }
        b'O' => {
            let key = match buf.get(2)? {
                b'D' => Key::Left,
                b'C' => Key::Right,
                _ => Key::Flush,
            };
            Some((key, 3))
        }
        _ => Some((Key::Flush, 2)),
    }
}

fn cup((row, col): Pos) -> Vec<u8> {
    format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
}

fn sgr(flags: Flags, fg: Color, bg: Color) -> String {
    let mut s = String::from("\x1b[0");
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if flags.contains(flag) {
            s.push(';');
            s.push_str(code);
        }
    }
    color(&mut s, fg, 30);
    color(&mut s, bg, 40);
    s.push('m');
    s
}

/// `base` is 30 for foreground, 40 for background.
fn color(s: &mut String, c: Color, base: u8) {
    use std::fmt::Write;
    let _ = match c {
        Color::Named(n) => match n as usize {
            i @ 0..=7 => write!(s, ";{}", base + i as u8),
            i @ 8..=15 => write!(s, ";{}", base + 60 + (i - 8) as u8),
            _ => return,
        },
        Color::Indexed(i) => write!(s, ";{};5;{i}", base + 8),
        Color::Spec(rgb) => write!(s, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const UL: &str = "\x1b[0;4m";

    fn s(b: Vec<u8>) -> String {
        String::from_utf8_lossy(&b).into_owned()
    }

    /// A predictor whose mirror shows "$ abc" and which has just confirmed three echoes.
    fn confident(t: Instant) -> Predictor {
        let mut p = Predictor::new(20, 5);
        p.feed_output(b"$ ", t);
        for ch in ["a", "b", "c"] {
            assert_eq!(p.on_input(ch.as_bytes(), t), b"", "tentative draws nothing");
            p.feed_output(ch.as_bytes(), t);
        }
        p
    }

    #[test]
    fn tentative_until_confirmed_then_draws_underlined() {
        let t = Instant::now();
        let mut p = confident(t);
        let out = s(p.on_input(b"d", t));
        assert!(out.contains(&format!("\x1b[1;6H{UL}d")), "{out:?}");
        assert!(
            out.ends_with("\x1b[1;7H"),
            "cursor at the prediction: {out:?}"
        );
    }

    #[test]
    fn confirmation_repaints_without_underline() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"d", t);
        let out = s(p.feed_output(b"d", t));
        assert!(out.contains("$ abcd\x1b[K"), "{out:?}");
        assert!(!out.contains(UL), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "nothing pending");
    }

    #[test]
    fn mismatch_rolls_back_and_goes_tentative() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"y", t));
        assert!(out.contains("$ abcy\x1b[K"), "{out:?}");
        assert!(out.ends_with("\x1b[1;7H"), "mirror cursor last: {out:?}");
        assert_eq!(p.on_input(b"z", t), b"", "tentative again");
    }

    #[test]
    fn alt_screen_switch_erases() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"\x1b[?1049h", t));
        assert!(
            out.is_empty(),
            "the alt screen hides the stale cell: {out:?}"
        );
        assert_eq!(p.on_input(b"q", t), b"", "no prediction on the alt screen");
        let out = s(p.feed_output(b"\x1b[?1049l", t));
        assert!(
            out.contains("$ abc\x1b[K"),
            "main screen repainted on return: {out:?}"
        );
        assert_eq!(p.on_input(b"q", t), b"", "tentative after leaving");
    }

    #[test]
    fn timeout_erases_after_one_second() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        assert_eq!(p.tick(t + Duration::from_millis(999)), b"");
        let out = s(p.tick(t + Duration::from_secs(1)));
        assert!(out.contains("$ abc\x1b[K"), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
    }

    #[test]
    fn cursor_jump_erases() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"\x1b[3;3H", t));
        assert!(out.contains("$ abc\x1b[K"), "{out:?}");
        assert!(out.ends_with("\x1b[3;3H"), "{out:?}");
    }

    #[test]
    fn password_prompts_never_predict() {
        let t = Instant::now();
        let mut p = confident(t);
        p.feed_output(b"\r\nuser's Password: ", t);
        assert_eq!(p.on_input(b"s", t), b"");
        assert_eq!(p.tick(t + Duration::from_secs(3)), b"");
        p.feed_output(b"\r\nPassPhrase: ", t);
        p.feed_output(b"\r\npassphrase: ", t);
        assert_eq!(p.on_input(b"s", t), b"");
        let mut q = confident(t);
        q.feed_output(b"\r\nsudo password:", t);
        assert_eq!(q.on_input(b"s", t), b"");
    }

    #[test]
    fn echo_hint_off_stops_prediction() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.set_echo_hint(false));
        assert!(out.contains("$ abc\x1b[K"), "{out:?}");
        assert_eq!(p.on_input(b"y", t), b"");
        p.set_echo_hint(true);
        assert_eq!(p.on_input(b"y", t), b"", "tentative after the hint returns");
    }

    #[test]
    fn disabled_predicts_nothing() {
        let t = Instant::now();
        let mut p = confident(t);
        p.set_enabled(false);
        assert_eq!(p.on_input(b"x", t), b"");
    }

    #[test]
    fn backspace_predicts_and_confirms() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.on_input(b"\x7f", t));
        assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
        assert!(out.ends_with("\x1b[1;6H"), "{out:?}");
        // The server echoes the char, then the erase.
        p.feed_output(b"x", t);
        let out = s(p.feed_output(b"\x08 \x08", t));
        assert!(!out.contains(UL), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "nothing pending");
    }

    #[test]
    fn backspace_at_line_start_flushes() {
        let t = Instant::now();
        let mut p = confident(t);
        p.feed_output(b"\r", t);
        assert_eq!(p.on_input(b"\x7f", t), b"");
    }

    #[test]
    fn left_and_right_move_the_predicted_cursor() {
        let t = Instant::now();
        let mut p = confident(t);
        assert_eq!(s(p.on_input(b"\x1b[D", t)), "\x1b[0m\x1b[1;5H");
        assert_eq!(s(p.on_input(b"\x1bOC", t)), "\x1b[0m\x1b[1;6H");
        // The server moves left only: the first move confirms, the second times out.
        let out = s(p.feed_output(b"\x1b[D", t));
        assert!(
            out.ends_with("\x1b[1;6H"),
            "cursor stays at the unconfirmed move: {out:?}"
        );
        let out = s(p.tick(t + Duration::from_secs(1)));
        assert!(out.ends_with("\x1b[1;5H"), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
    }

    #[test]
    fn right_at_the_end_of_the_text_is_not_predicted() {
        let t = Instant::now();
        let mut p = confident(t);
        assert_eq!(p.on_input(b"\x1b[C", t), b"");
        assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
    }

    #[test]
    fn left_at_column_zero_is_not_predicted() {
        let t = Instant::now();
        let mut p = confident(t);
        p.feed_output(b"\r", t);
        assert_eq!(p.on_input(b"\x1b[D", t), b"");
    }

    #[test]
    fn enter_and_control_keys_flush() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.on_input(b"\r", t));
        assert!(out.contains("$ abc\x1b[K"), "{out:?}");
        assert_eq!(p.on_input(b"y", t), b"", "tentative after a flush");
        let mut p = confident(t);
        p.on_input(b"x", t);
        assert!(!p.on_input(b"\x03", t).is_empty());
        let mut p = confident(t);
        p.on_input(b"x", t);
        assert!(!p.on_input(b"\t", t).is_empty());
    }

    #[test]
    fn escape_sequences_are_not_typed_text() {
        let t = Instant::now();
        let mut p = confident(t);
        assert_eq!(p.on_input(b"\x1b[A", t), b"");
        assert_eq!(p.on_input(b"\x1b[1;5C", t), b"");
        assert_eq!(p.on_input(b"\x1b[200~", t), b"");
        assert_eq!(
            p.tick(t + Duration::from_secs(3)),
            b"",
            "nothing was predicted"
        );
    }

    #[test]
    fn wide_chars_predict_with_two_cells() {
        let t = Instant::now();
        let mut p = confident(t);
        let out = s(p.on_input("漢".as_bytes(), t));
        assert!(out.contains(&format!("\x1b[1;6H{UL}漢")), "{out:?}");
        assert!(out.ends_with("\x1b[1;8H"), "{out:?}");
        let out = s(p.feed_output("漢".as_bytes(), t));
        assert!(!out.contains(UL), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "confirmed");
        // The next char lands after both cells.
        let out = s(p.on_input(b"a", t));
        assert!(out.contains(&format!("\x1b[1;8H{UL}a")), "{out:?}");
    }

    #[test]
    fn wide_char_split_across_reads() {
        let t = Instant::now();
        let mut p = confident(t);
        let bytes = "漢".as_bytes();
        assert_eq!(p.on_input(&bytes[..2], t), b"");
        let out = s(p.on_input(&bytes[2..], t));
        assert!(out.contains("漢"), "{out:?}");
    }

    #[test]
    fn zero_width_and_edge_of_screen_are_not_predicted() {
        let t = Instant::now();
        let mut p = confident(t);
        assert!(p.on_input("\u{301}".as_bytes(), t).is_empty());
        p.feed_output(b"\x1b[1;20H", t);
        assert!(p.on_input(b"x", t).is_empty(), "would wrap");
        p.feed_output(b"\x1b[1;19H", t);
        assert!(
            p.on_input("漢".as_bytes(), t).is_empty(),
            "wide char would wrap"
        );
    }

    #[test]
    fn burst_of_typing_while_the_server_lags() {
        let t = Instant::now();
        let mut p = confident(t);
        for (i, ch) in "hello".chars().enumerate() {
            let at = t + Duration::from_millis(10 * i as u64);
            let out = s(p.on_input(ch.to_string().as_bytes(), at));
            assert!(out.contains(&format!("{UL}{ch}")), "{out:?}");
        }
        // 300 ms later the echoes arrive in two chunks.
        let late = t + Duration::from_millis(300);
        let out = s(p.feed_output(b"hel", late));
        assert!(!out.contains(UL), "{out:?}");
        assert!(out.ends_with("\x1b[1;11H"), "{out:?}");
        let out = s(p.feed_output(b"lo", late));
        assert!(!out.contains(UL), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "all confirmed");
        // Still confident.
        assert!(!p.on_input(b"!", late).is_empty());
    }

    #[test]
    fn partially_confirmed_burst_times_out_from_the_oldest() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"a", t);
        p.on_input(b"b", t + Duration::from_millis(500));
        p.feed_output(b"a", t + Duration::from_millis(600));
        assert_eq!(p.tick(t + Duration::from_millis(1400)), b"");
        let out = s(p.tick(t + Duration::from_millis(1500)));
        assert!(out.contains("$ abca\x1b[K"), "{out:?}");
    }

    #[test]
    fn styled_cells_repaint_with_their_style() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"\x1b[1;31my", t));
        assert!(out.contains("$ abc\x1b[0;1;31my"), "{out:?}");
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;
    use proptest::prelude::*;

    /// The user's terminal: fed the server bytes and everything the predictor returns.
    struct Model {
        term: Term<VoidListener>,
        parser: Processor<NoSync>,
    }

    impl Model {
        fn new(cols: usize, rows: usize) -> Self {
            let config = Config {
                scrolling_history: 0,
                ..Config::default()
            };
            Model {
                term: Term::new(config, &Size { cols, rows }, VoidListener),
                parser: Processor::new(),
            }
        }
        fn feed(&mut self, bytes: &[u8]) {
            self.parser.advance(&mut self.term, bytes);
        }
    }

    const MASK: Flags = Flags::BOLD
        .union(Flags::DIM)
        .union(Flags::ITALIC)
        .union(Flags::INVERSE)
        .union(Flags::HIDDEN)
        .union(Flags::STRIKEOUT)
        .union(Flags::UNDERLINE);

    fn snapshot(t: &Term<VoidListener>, cols: usize, rows: usize) -> String {
        let mut s = String::new();
        for r in 0..rows {
            for c in 0..cols {
                let cell = &t.grid()[Line(r as i32)][Column(c)];
                s.push(cell.c);
                let f = cell.flags & MASK;
                if f.bits() != 0
                    || cell.fg
                        != Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground)
                    || cell.bg
                        != Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background)
                {
                    s += &format!("{{{:?}{:?}{:?}}}", f, cell.fg, cell.bg);
                }
            }
            s.push('\n');
        }
        let p = t.grid().cursor.point;
        s += &format!(
            "cursor {} {} wrap={} show={} alt={}",
            p.line.0,
            p.column.0,
            t.grid().cursor.input_needs_wrap,
            t.mode().contains(TermMode::SHOW_CURSOR),
            t.mode().contains(TermMode::ALT_SCREEN)
        );
        s
    }

    #[derive(Clone, Debug)]
    enum Op {
        Input(&'static str),
        Server(Vec<&'static str>, usize),
        Tick(u64),
    }

    const INPUTS: &[&str] = &[
        "a",
        "b",
        "z",
        " ",
        "漢",
        "\x7f",
        "\x1b[D",
        "\x1b[C",
        "\x1bOD",
        "\r",
        "\x03",
        "\x1b",
        "\x1b[200~",
        "\x1b[201~",
        "\u{301}",
        "\x1b[A",
    ];
    const ATOMS: &[&str] = &[
        "a",
        "b",
        "z",
        "ab",
        "漢",
        "e\u{301}",
        "\r\n",
        "\n",
        "\x08",
        "\x08 \x08",
        "\x1b[D",
        "\x1b[C",
        "\x1b[2K",
        "\x1b[K",
        "\x1b[1;31m",
        "\x1b[0m",
        "\x1b[1m",
        "\x1b[3;5H",
        "\x1b[1;9H",
        "\x1b[?25l",
        "\x1b[?25h",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x1b[J",
        "$ ",
        "Password: ",
        "\x1b[?2026h",
        "\x1b[?2026l",
        "\x1b]0;title\x07",
        "\x1b7",
        "\x1b8",
        "0123456789012345678901234567",
        "\x1b[4;2r",
        "\x1b[r",
        "\x1bM",
        "\x1b[2J\x1b[H",
        "\x1b[38;5;200m",
        "\x1b[48;2;1;2;3m",
        "\x1b[4m",
        "\x1b[7m",
    ];

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0..INPUTS.len()).prop_map(|i| Op::Input(INPUTS[i])),
            4 => (prop::collection::vec(0..ATOMS.len(), 1..4), 1usize..40)
                .prop_map(|(v, cut)| Op::Server(v.into_iter().map(|i| ATOMS[i]).collect(), cut)),
            1 => (0u64..1500).prop_map(Op::Tick),
        ]
    }

    const COLS: usize = 12;
    const ROWS: usize = 5;

    fn run(ops: &[Op], preamble: &str) -> Result<(), String> {
        let t0 = Instant::now();
        let mut now = t0;
        let mut p = Predictor::new(COLS, ROWS);
        let mut m = Model::new(COLS, ROWS);
        let server = |p: &mut Predictor, m: &mut Model, chunk: &[u8], now: Instant| {
            m.feed(&p.before_output());
            m.feed(chunk);
            m.feed(&p.feed_output(chunk, now));
        };
        // Get confident first.
        server(&mut p, &mut m, preamble.as_bytes(), now);
        for ch in ["a", "b", "c", "d"] {
            m.feed(&p.on_input(ch.as_bytes(), now));
            server(&mut p, &mut m, ch.as_bytes(), now);
        }
        // The server stream is only ever cut, so the last chunk must end at ground.
        for o in ops {
            now += Duration::from_millis(20);
            match o {
                Op::Input(s) => m.feed(&p.on_input(s.as_bytes(), now)),
                Op::Server(atoms, cut) => {
                    let all: Vec<u8> = atoms.concat().into_bytes();
                    for chunk in all.chunks(*cut) {
                        server(&mut p, &mut m, chunk, now);
                    }
                }
                Op::Tick(ms) => {
                    now += Duration::from_millis(*ms);
                    m.feed(&p.tick(now));
                }
            }
        }
        // Make the stream end at ground, then let everything time out.
        server(&mut p, &mut m, b"\x1b\\\x07\x18", now);
        now += Duration::from_secs(2);
        m.feed(&p.tick(now));
        m.feed(&p.tick(now));
        let (a, b) = (snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
        if a == b {
            Ok(())
        } else {
            Err(format!("terminal:\n{a}\nmirror:\n{b}\nops: {ops:?}"))
        }
    }

    /// The user sees what the caller writes: before_output, the server bytes, feed_output.
    fn server(p: &mut Predictor, m: &mut Model, chunk: &[u8], now: Instant) -> Vec<u8> {
        m.feed(&p.before_output());
        m.feed(chunk);
        let after = p.feed_output(chunk, now);
        m.feed(&after);
        after
    }

    fn confident(cols: usize, t: Instant) -> (Predictor, Model) {
        let mut p = Predictor::new(cols, ROWS);
        let mut m = Model::new(cols, ROWS);
        server(&mut p, &mut m, b"$ ", t);
        for ch in ["a", "b", "c"] {
            m.feed(&p.on_input(ch.as_bytes(), t));
            server(&mut p, &mut m, ch.as_bytes(), t);
        }
        (p, m)
    }

    #[test]
    fn the_server_echo_lands_at_the_mirror_cursor_not_the_predicted_one() {
        let t = Instant::now();
        let (mut p, mut m) = confident(COLS, t);
        m.feed(&p.on_input(b"d", t));
        server(&mut p, &mut m, b"d", t);
        assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    }

    #[test]
    fn the_servers_saved_cursor_and_pen_survive_a_prediction() {
        let t = Instant::now();
        let (mut p, mut m) = confident(COLS, t);
        server(&mut p, &mut m, b"\x1b7\x1b[1;31m", t);
        m.feed(&p.on_input(b"d", t));
        server(&mut p, &mut m, b"d\x1b[3;3H\x1b8Z", t);
        m.feed(&p.tick(t + Duration::from_secs(2)));
        assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    }

    #[test]
    fn nothing_is_written_inside_a_split_escape_sequence() {
        let t = Instant::now();
        let (mut p, mut m) = confident(COLS, t);
        m.feed(&p.on_input(b"d", t));
        for chunk in [&b"\x1b"[..], b"[", b"3", b";3", b"H"] {
            let after = server(&mut p, &mut m, chunk, t);
            if chunk != b"H" {
                assert!(after.is_empty(), "mid-sequence: {after:?}");
            }
        }
        m.feed(&p.tick(t + Duration::from_secs(2)));
        assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    }

    #[test]
    fn a_split_utf8_char_from_the_server_is_not_interrupted() {
        let t = Instant::now();
        let (mut p, mut m) = confident(COLS, t);
        m.feed(&p.on_input("漢".as_bytes(), t));
        let bytes = "漢".as_bytes();
        assert!(server(&mut p, &mut m, &bytes[..1], t).is_empty());
        assert!(server(&mut p, &mut m, &bytes[1..2], t).is_empty());
        server(&mut p, &mut m, &bytes[2..], t);
        assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    }

    #[test]
    fn a_scroll_does_not_leave_a_stale_underlined_cell() {
        let t = Instant::now();
        let (mut p, mut m) = confident(COLS, t);
        server(&mut p, &mut m, b"\x1b[5;1Hxy", t);
        for ch in ["1", "2", "3"] {
            m.feed(&p.on_input(ch.as_bytes(), t));
            server(&mut p, &mut m, ch.as_bytes(), t);
        }
        m.feed(&p.on_input(b"4", t));
        server(&mut p, &mut m, b"\r\n\r\n", t);
        m.feed(&p.tick(t + Duration::from_secs(2)));
        assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    }

    #[test]
    fn leaving_the_alt_screen_repaints_the_main_one() {
        let t = Instant::now();
        let (mut p, mut m) = confident(COLS, t);
        m.feed(&p.on_input(b"d", t));
        server(&mut p, &mut m, b"\x1b[?1049hvim", t);
        server(&mut p, &mut m, b"\x1b[?1049l", t);
        assert_eq!(snapshot(&m.term, COLS, ROWS), snapshot(&p.term, COLS, ROWS));
    }

    #[test]
    fn an_arrow_key_split_across_reads_is_not_typed_text() {
        let t = Instant::now();
        let (mut p, _) = confident(COLS, t);
        p.on_input(b"\x1b", t);
        p.on_input(b"[D", t);
        assert_eq!(p.pending.len(), 1, "one Left, not the chars [ and D");
        assert!(matches!(p.pending[0].kind, Kind::Move));
    }

    #[test]
    fn pasted_text_is_not_predicted() {
        let t = Instant::now();
        let (mut p, _) = confident(COLS, t);
        p.on_input(b"\x1b[200~", t);
        assert!(p.on_input(b"hi", t).is_empty());
        assert!(p.pending.is_empty());
        p.on_input(b"\x1b[20", t);
        p.on_input(b"1~", t);
        p.on_input(b"x", t);
        assert_eq!(
            p.pending.len(),
            1,
            "typing is predicted again after the paste"
        );
    }

    #[test]
    fn a_wrapped_password_prompt_is_detected() {
        let t = Instant::now();
        let (mut p, _) = confident(COLS, t);
        p.feed_output(b"\r\nEnter passphrase for key abc: ", t);
        assert!(p.on_input(b"s", t).is_empty());
        assert!(p.pending.is_empty());
        let (mut p, _) = confident(COLS, t);
        p.feed_output(b"\r\n[sudo] password for me: ", t);
        p.on_input(b"s", t);
        assert!(p.pending.is_empty());
        let (mut p, _) = confident(COLS, t);
        p.feed_output(b"\r\nENTER PASSWORD:", t);
        p.on_input(b"s", t);
        assert!(p.pending.is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]
        #[test]
        fn rollback_leaves_the_terminal_equal_to_the_mirror(
            ops in prop::collection::vec(op(), 1..25)
        ) {
            run(&ops, "$ ").map_err(TestCaseError::fail)?;
        }
    }
}
