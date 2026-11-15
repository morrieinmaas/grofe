//! Predictive local echo (spec section 4), mosh style. Pure: no I/O, no threads, time is
//! passed in.
//!
//! The predictor keeps a mirror of the attach output. Keystrokes the server will probably echo
//! are tracked as predictions; once enough of them were confirmed they are also drawn at once,
//! underlined. Every method returns the bytes to write to the user's terminal right away. For
//! `feed_output` those bytes must be written after the server bytes, and the server bytes
//! must end on an escape sequence boundary.

use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor, StdSyncHandler};
use unicode_width::UnicodeWidthChar;

/// Confirmed predictions in a row before they are drawn.
const CONFIDENT_AFTER: u32 = 3;
/// A prediction the server has not confirmed after this long is erased.
const TIMEOUT: Duration = Duration::from_secs(1);

/// (row, column), both 0-based.
type Pos = (usize, usize);

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
    Flush,
}

pub struct Predictor {
    term: Term<VoidListener>,
    parser: Processor<StdSyncHandler>,
    cols: usize,
    enabled: bool,
    echo_hint: bool,
    pending: VecDeque<Pred>,
    confirmed: u32,
    /// Start of a UTF-8 char split across two `on_input` calls.
    carry: Vec<u8>,
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
        }
    }

    /// The client screen changed size. Pending predictions are forgotten; the caller redraws.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        self.term.resize(Size { cols, rows });
        self.cols = cols;
        self.pending.clear();
        self.confirmed = 0;
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

    /// Server output for the attach channel. Updates the mirror, confirms or invalidates
    /// predictions. The returned bytes repaint affected cells and the cursor from the mirror.
    pub fn feed_output(&mut self, bytes: &[u8], _now: Instant) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return self.rollback();
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
        self.repaint(&done)
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
            _ => Vec::new(),
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
                    let (key, len) = escape(&buf[i..]);
                    keys.push(key);
                    i += len;
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
                    if i + need > buf.len() {
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
            && !self.term.mode().contains(TermMode::ALT_SCREEN)
            && !self.password_prompt()
    }

    fn password_prompt(&self) -> bool {
        let row = self.mirror_cursor().0;
        let line: String = (0..self.cols)
            .filter_map(|c| {
                let cell = &self.term.grid()[Line(row as i32)][Column(c)];
                (!cell.flags.contains(Flags::WIDE_CHAR_SPACER)).then_some(cell.c)
            })
            .collect();
        let line = line.trim_end();
        line.ends_with("assword:") || line.to_lowercase().contains("passphrase")
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
        let shown = self.confirmed >= CONFIDENT_AFTER;
        let pred = Pred {
            kind,
            before,
            after,
            prev: self.virtual_char(cell_pos),
            mirror_prev: self.mirror_char(cell_pos),
            at: now,
            shown,
        };
        let draw = shown.then(|| draw(&pred));
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

    /// Repaint the cells of the shown predictions from the mirror; the cursor ends where the
    /// mirror's is.
    fn repaint(&self, preds: &[Pred]) -> Vec<u8> {
        let mut cells = BTreeSet::new();
        let mut any = false;
        for p in preds.iter().filter(|p| p.shown) {
            any = true;
            match p.kind {
                Kind::Char(_, w) => cells.extend((0..w).map(|i| (p.before.0, p.before.1 + i))),
                Kind::Back => {
                    cells.insert(p.after);
                }
                Kind::Move => {}
            }
        }
        if !any {
            return Vec::new();
        }
        let mut out = b"\x1b7".to_vec();
        for (row, col) in cells {
            let cell = &self.term.grid()[Line(row as i32)][Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            out.extend(cup((row, col)));
            out.extend(sgr(cell.flags, cell.fg, cell.bg).bytes());
            let mut utf8 = [0; 4];
            out.extend(cell.c.encode_utf8(&mut utf8).bytes());
        }
        out.extend(b"\x1b[0m\x1b8");
        out.extend(cup(self.mirror_cursor()));
        out
    }
}

/// The key at the start of `buf` (an ESC) and how many bytes it takes.
fn escape(buf: &[u8]) -> (Key, usize) {
    // ponytail: a sequence cut off by the end of the read counts as a flush, not as pending.
    match buf.get(1) {
        None => (Key::Flush, 1),
        Some(b'[') => {
            let mut j = 2;
            while buf.get(j).is_some_and(|b| (0x20..=0x3f).contains(b)) {
                j += 1;
            }
            let end = (j + 1).min(buf.len());
            let key = match &buf[2..end] {
                b"D" => Key::Left,
                b"C" => Key::Right,
                _ => Key::Flush,
            };
            (key, end)
        }
        Some(b'O') => {
            let key = match buf.get(2) {
                Some(b'D') => Key::Left,
                Some(b'C') => Key::Right,
                _ => Key::Flush,
            };
            (key, 3.min(buf.len()))
        }
        Some(_) => (Key::Flush, 2),
    }
}

fn cup((row, col): Pos) -> Vec<u8> {
    format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
}

/// What to write for one shown prediction.
fn draw(p: &Pred) -> Vec<u8> {
    let mut out = b"\x1b7".to_vec();
    match p.kind {
        Kind::Char(c, _) => {
            out.extend(cup(p.before));
            out.extend(b"\x1b[0;4m");
            let mut utf8 = [0; 4];
            out.extend(c.encode_utf8(&mut utf8).bytes());
            out.extend(b"\x1b[0m");
        }
        Kind::Back => {
            out.extend(cup(p.after));
            out.extend(b"\x1b[0m ");
        }
        Kind::Move => {}
    }
    out.extend(b"\x1b8");
    out.extend(cup(p.after));
    out
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
        assert!(out.contains("\x1b[1;6H\x1b[0md"), "{out:?}");
        assert!(!out.contains(UL), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(5)), b"", "nothing pending");
    }

    #[test]
    fn mismatch_rolls_back_and_goes_tentative() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"y", t));
        assert!(out.contains("\x1b[1;6H\x1b[0my"), "{out:?}");
        assert!(out.ends_with("\x1b[1;7H"), "mirror cursor last: {out:?}");
        assert_eq!(p.on_input(b"z", t), b"", "tentative again");
    }

    #[test]
    fn alt_screen_switch_erases() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"\x1b[?1049h", t));
        assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
        assert_eq!(p.on_input(b"q", t), b"", "no prediction on the alt screen");
        p.feed_output(b"\x1b[?1049l", t);
        assert_eq!(p.on_input(b"q", t), b"", "tentative after leaving");
    }

    #[test]
    fn timeout_erases_after_one_second() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        assert_eq!(p.tick(t + Duration::from_millis(999)), b"");
        let out = s(p.tick(t + Duration::from_secs(1)));
        assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
        assert_eq!(p.tick(t + Duration::from_secs(2)), b"");
    }

    #[test]
    fn cursor_jump_erases() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"\x1b[3;3H", t));
        assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
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
        assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
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
        assert_eq!(s(p.on_input(b"\x1b[D", t)), "\x1b7\x1b8\x1b[1;5H");
        assert_eq!(s(p.on_input(b"\x1bOC", t)), "\x1b7\x1b8\x1b[1;6H");
        // The server moves left only: the first move confirms, the second times out.
        let out = s(p.feed_output(b"\x1b[D", t));
        assert!(out.ends_with("\x1b[1;5H"), "{out:?}");
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
        assert!(out.contains("\x1b[1;6H\x1b[0m "), "{out:?}");
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
        assert!(out.ends_with("\x1b[1;9H"), "{out:?}");
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
        assert!(out.contains("\x1b[1;7H\x1b[0m "), "{out:?}");
    }

    #[test]
    fn styled_cells_repaint_with_their_style() {
        let t = Instant::now();
        let mut p = confident(t);
        p.on_input(b"x", t);
        let out = s(p.feed_output(b"\x1b[1;31my", t));
        assert!(out.contains("\x1b[1;6H\x1b[0;1;31my"), "{out:?}");
    }
}
