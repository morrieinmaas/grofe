//! Search and jumps in copy mode (TM.3, spec §4.11): `search-forward`,
//! `search-backward` and their `-text` forms, `search-again`,
//! `search-reverse`, the match styling, and the jumps `jump-forward`,
//! `jump-backward`, `jump-to-forward`, `jump-to-backward`, `jump-again`
//! and `jump-reverse`. `#{copy_cursor_word}` is `Mode::cursor_word`.
//!
//! The search is a port of tmux 3.7b's `window_copy_search` and the
//! functions it calls, quirks included: a pattern with none of
//! `^$*+()?[].\` is searched as plain text, one with no upper-case letter
//! ignores case, a line is searched with its soft-wrapped rows joined (up
//! to 2000 cells), padded to the pane's width with blanks, and an empty
//! match counts as none. tmux's POSIX extended syntax is read as glibc
//! reads it ([`translate`]) and run with the `regex` crate; what still
//! differs is in `docs/copy-mode.md`.
//!
//! A search runs off the actor (spec D5, D8): [`launch`] hands the scan a
//! shared reference to the mode's copy of the pane on the blocking pool,
//! and the result comes back as a [`Job`](crate::server::Job). Until then
//! the mode is busy: commands for it wait in order ([`hold`]), and so does
//! the reply of every request that sent one, so a script that searches and
//! then reads `#{copy_cursor_x}` sees the result.
//!
//! The search marks (tmux's `searchmark`) are not stored: they are a
//! function of the pattern, the copy and the view's offset, so whoever
//! needs them (the overlay, `#{search_match}`, the next search) works
//! them out for the rows on view, which bounds them by the pane's size.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};
use tokio::sync::oneshot;
use unicode_width::UnicodeWidthStr;

use super::{Action, Ctx, Grid, Mode, Reader, STYLE_FLAGS, TAB_PAD, positionals, select};
use crate::protocol::Failure;
use crate::server::dispatch::{self, options};
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::status::Style;
use crate::server::{State, clients};
use crate::types::PaneId;

/// `copy-mode-match-style`'s default.
pub const MATCH_STYLE: &str = "bg=cyan,fg=black";
/// `copy-mode-current-match-style`'s default.
pub const CURRENT_MATCH_STYLE: &str = "bg=magenta,fg=black";

/// tmux's `WINDOW_COPY_SEARCH_TIMEOUT`: a search marking for longer gives
/// up and shows no marks.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// tmux's `WINDOW_COPY_SEARCH_ALL_TIMEOUT`: how long counting every match
/// may take before only the rows on view are marked.
const SEARCH_ALL_TIMEOUT: Duration = Duration::from_millis(200);
/// tmux's `WINDOW_COPY_SEARCH_MAX_LINE`: the most cells of one line,
/// soft-wrapped rows joined, a regex sees.
const SEARCH_MAX_LINE: usize = 2000;

/// The most a compiled pattern may take (the crate's own default is
/// 10 MiB); a bigger one is refused, as a pattern `regcomp` refuses.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// The most each lazy DFA's cache may take.
const DFA_SIZE_LIMIT: usize = 1 << 20;
/// How deep groups and repetitions may nest.
const NEST_LIMIT: u32 = 64;

/// The most commands that wait for a busy mode; more are dropped.
// ponytail: tmux has no queue (its search blocks the server); 1024 waiting
// commands is a script gone wrong, not typing.
const MAX_HELD: usize = 1024;

/// The commands that clear the search marks in vi mode (tmux's
/// `WINDOW_COPY_CMD_CLEAR_ALWAYS`, less the `search-` ones, which never
/// clear them).
const CLEAR_ALWAYS: &[&str] = &[
    "append-selection",
    "append-selection-and-cancel",
    "back-to-indentation",
    "begin-selection",
    "cancel",
    "clear-selection",
    "copy-end-of-line",
    "copy-end-of-line-and-cancel",
    "copy-line",
    "copy-line-and-cancel",
    "copy-pipe",
    "copy-pipe-and-cancel",
    "copy-pipe-end-of-line",
    "copy-pipe-end-of-line-and-cancel",
    "copy-pipe-line",
    "copy-pipe-line-and-cancel",
    "copy-selection",
    "copy-selection-and-cancel",
    "cursor-down-and-cancel",
    "halfpage-down-and-cancel",
    "jump-to-mark",
    "next-matching-bracket",
    "next-prompt",
    "page-down-and-cancel",
    "pipe",
    "pipe-and-cancel",
    "previous-matching-bracket",
    "previous-prompt",
    "recentre-top-bottom",
    "rectangle-off",
    "rectangle-on",
    "rectangle-toggle",
    "refresh-from-pane",
    "scroll-bottom",
    "scroll-down-and-cancel",
    "scroll-exit-off",
    "scroll-exit-on",
    "scroll-exit-toggle",
    "scroll-middle",
    "scroll-top",
    "select-line",
    "select-word",
    "set-mark",
    "stop-selection",
];

/// The commands that never clear them (`WINDOW_COPY_CMD_CLEAR_NEVER`);
/// in emacs mode every other command does.
const CLEAR_NEVER: &[&str] = &[
    "copy-pipe-no-clear",
    "copy-selection-no-clear",
    "pipe-no-clear",
    "toggle-position",
];

// ---- The pattern -----------------------------------------------------------

/// A compiled search string.
#[derive(Debug)]
pub struct Pattern {
    /// Searched as a regex; else as plain text.
    regex: bool,
    /// The string's width in cells (tmux's `screen_write_strlen`), the
    /// width a plain match marks.
    width: usize,
    /// `^` matches at the start of what it is run on.
    bol: Regex,
    /// tmux's `REG_NOTBOL`: `^` never matches at the start.
    notbol: Regex,
}

/// tmux's `window_copy_is_lowercase`.
fn is_lowercase(s: &str) -> bool {
    !s.bytes().any(|b| b.is_ascii_uppercase())
}

/// Whether tmux searches `s` as a regex at all: one with none of these
/// characters is searched as plain text, even by `search-forward`.
fn has_regex_chars(s: &str) -> bool {
    s.contains(['^', '$', '*', '+', '(', ')', '?', '[', ']', '.', '\\'])
}

impl Pattern {
    /// `None` when the pattern does not compile (tmux's `regcomp` failing),
    /// which finds nothing.
    fn compile(text: &str, regex: bool) -> Option<Pattern> {
        let cis = is_lowercase(text);
        let (bol, notbol) = if regex {
            let t = translate(text)?;
            (build(&t.bol, cis)?, build(&t.notbol, cis)?)
        } else {
            // As tmux's `window_copy_search_compare`: a one-byte character
            // is compared lowered, any other exactly.
            let flags = if cis { "(?i-u:" } else { "(?-u:" };
            let re = build(&format!("{flags}{})", regex::escape(text)), false)?;
            (re.clone(), re)
        };
        Some(Pattern {
            regex,
            width: UnicodeWidthStr::width(text),
            bol,
            notbol,
        })
    }
}

fn build(pattern: &str, cis: bool) -> Option<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(cis)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(DFA_SIZE_LIMIT)
        .nest_limit(NEST_LIMIT)
        .build()
        .ok()
}

/// A POSIX pattern in the crate's syntax: with `^` as an anchor, and with
/// `^` never matching (for `REG_NOTBOL`).
#[derive(Debug, PartialEq, Eq)]
pub struct Translated {
    pub bol: String,
    pub notbol: String,
}

/// The POSIX character class names glibc knows.
const CLASSES: &[&str] = &[
    "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
    "upper", "xdigit",
];

/// Reads `pattern` as glibc's `regcomp` reads POSIX extended syntax
/// (`REG_EXTENDED`) and writes it in the `regex` crate's; `None` for one
/// glibc refuses, or uses a back-reference, which the crate cannot match.
///
/// - `\` before a letter or digit is GNU's word and space classes and
///   boundaries (`\w \W \s \S \b \B`, plus `` \` `` and `\'`), a
///   back-reference (`\1`), or refused (`\d`, `\n`); before anything else
///   it is that character.
/// - In a bracket expression `\` is itself, `[:digit:]` and the other
///   classes are classes, and `]` first is itself.
/// - `{` starts an interval (`{,n}` is `{0,n}`) or is refused; `}` alone is
///   itself, and so is a `)` with no `(`.
/// - `*`, `+`, `?` and an interval with nothing to repeat are refused;
///   stacked ones repeat what is before them (`a**`, `a+?`), where the
///   crate would read a lazy repetition.
pub fn translate(pattern: &str) -> Option<Translated> {
    let mut bol = String::new();
    let mut notbol = String::new();
    // Where in both outputs the last atom starts, if the last thing
    // written was an atom or a repetition of one.
    let mut atom: Option<(usize, usize)> = None;
    // Whether the last thing written was a repetition.
    let mut repeated = false;
    // The open groups' starts.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let push = |bol: &mut String, notbol: &mut String, s: &str| {
        bol.push_str(s);
        notbol.push_str(s);
    };
    while i < chars.len() {
        let c = chars[i];
        let start = (bol.len(), notbol.len());
        match c {
            '\\' => {
                let n = *chars.get(i + 1)?;
                i += 2;
                let text = match n {
                    'w' | 'W' | 's' | 'S' => format!("\\{n}"),
                    'b' | 'B' | '<' | '>' => {
                        push(&mut bol, &mut notbol, &format!("\\{n}"));
                        atom = None;
                        repeated = false;
                        continue;
                    }
                    '`' => {
                        push(&mut bol, &mut notbol, "\\A");
                        atom = None;
                        repeated = false;
                        continue;
                    }
                    '\'' => {
                        push(&mut bol, &mut notbol, "\\z");
                        atom = None;
                        repeated = false;
                        continue;
                    }
                    n if n.is_ascii_alphanumeric() => return None,
                    n => regex::escape(&n.to_string()),
                };
                push(&mut bol, &mut notbol, &text);
                atom = Some(start);
                repeated = false;
                continue;
            }
            '[' => {
                let (class, next) = bracket(&chars, i)?;
                push(&mut bol, &mut notbol, &class);
                i = next;
                atom = Some(start);
                repeated = false;
                continue;
            }
            '(' => {
                if chars.get(i + 1) == Some(&'?') {
                    return None;
                }
                groups.push(start);
                push(&mut bol, &mut notbol, "(");
                atom = None;
                repeated = false;
            }
            ')' => match groups.pop() {
                Some(open) => {
                    push(&mut bol, &mut notbol, ")");
                    atom = Some(open);
                    repeated = false;
                }
                None => {
                    push(&mut bol, &mut notbol, "\\)");
                    atom = Some(start);
                    repeated = false;
                }
            },
            '|' => {
                push(&mut bol, &mut notbol, "|");
                atom = None;
                repeated = false;
            }
            '*' | '+' | '?' | '{' => {
                let (op, next) = if c == '{' {
                    interval(&chars, i)?
                } else {
                    (c.to_string(), i + 1)
                };
                let (ab, an) = atom?;
                if repeated {
                    // `a**`: repeat the repetition, not lazily.
                    bol.insert_str(ab, "(?:");
                    notbol.insert_str(an, "(?:");
                    push(&mut bol, &mut notbol, ")");
                }
                push(&mut bol, &mut notbol, &op);
                repeated = true;
                i = next;
                continue;
            }
            '^' => {
                bol.push('^');
                notbol.push_str("(?:\\b\\B)");
                atom = None;
                repeated = false;
            }
            '$' => {
                push(&mut bol, &mut notbol, "$");
                atom = None;
                repeated = false;
            }
            '.' => {
                push(&mut bol, &mut notbol, ".");
                atom = Some(start);
                repeated = false;
            }
            c => {
                push(&mut bol, &mut notbol, &regex::escape(&c.to_string()));
                atom = Some(start);
                repeated = false;
            }
        }
        i += 1;
    }
    if !groups.is_empty() {
        return None;
    }
    Some(Translated { bol, notbol })
}

/// An interval at `chars[i] == '{'`: the crate's spelling and the index
/// after it.
fn interval(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut j = i + 1;
    let mut min = String::new();
    while chars.get(j).is_some_and(char::is_ascii_digit) {
        min.push(chars[j]);
        j += 1;
    }
    let mut max = None;
    if chars.get(j) == Some(&',') {
        j += 1;
        let mut m = String::new();
        while chars.get(j).is_some_and(char::is_ascii_digit) {
            m.push(chars[j]);
            j += 1;
        }
        max = Some(m);
    }
    if chars.get(j) != Some(&'}') || (min.is_empty() && max.as_ref().is_none_or(String::is_empty)) {
        return None;
    }
    let min = if min.is_empty() { "0".to_string() } else { min };
    let op = match max {
        None => format!("{{{min}}}"),
        Some(m) if m.is_empty() => format!("{{{min},}}"),
        Some(m) => format!("{{{min},{m}}}"),
    };
    Some((op, j + 1))
}

/// A bracket expression at `chars[i] == '['`: the crate's class and the
/// index after it.
fn bracket(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut out = String::from("[");
    let mut j = i + 1;
    if chars.get(j) == Some(&'^') {
        out.push('^');
        j += 1;
    }
    let mut first = true;
    loop {
        let c = *chars.get(j)?;
        if c == ']' && !first {
            out.push(']');
            return Some((out, j + 1));
        }
        first = false;
        if c == '[' && matches!(chars.get(j + 1), Some(':' | '=' | '.')) {
            let kind = chars[j + 1];
            let body_start = j + 2;
            let mut k = body_start;
            while !(chars.get(k) == Some(&kind) && chars.get(k + 1) == Some(&']')) {
                k += 1;
                if k >= chars.len() {
                    return None;
                }
            }
            let body: String = chars[body_start..k].iter().collect();
            match kind {
                ':' if CLASSES.contains(&body.as_str()) => {
                    out.push_str(&format!("[:{body}:]"));
                }
                // A collating symbol or equivalence class of one character
                // is that character; anything else glibc's locale decides.
                '=' | '.' if body.chars().count() == 1 => out.push_str(&class_char(body)),
                _ => return None,
            }
            j = k + 2;
            continue;
        }
        if c == '-' && chars.get(j + 1).is_some_and(|&n| n != ']') && out.len() > 1 {
            out.push('-');
        } else {
            out.push_str(&class_char(c.to_string()));
        }
        j += 1;
    }
}

/// One character as it is written inside the crate's class.
fn class_char(c: impl AsRef<str>) -> String {
    let c = c.as_ref();
    match c {
        "\\" | "[" | "]" | "^" | "-" | "&" | "~" => format!("\\{c}"),
        c => c.to_string(),
    }
}

// ---- The copy as tmux's search reads it ------------------------------------

/// One search line: rows `py..py + rows` of the copy, each its full width,
/// as tmux's `window_copy_stringify` writes them, and where each cell's
/// text starts (one more entry for the end).
#[derive(Debug, Default)]
struct Line {
    py: usize,
    rows: usize,
    text: String,
    starts: Vec<usize>,
}

/// A position on the view (tmux's `cx`, `cy`, `oy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct View {
    cx: usize,
    cy: usize,
    oy: usize,
}

/// The search marks of the rows on view (tmux's `searchmark`): a
/// generation per cell, one per match, 0 for none.
type Marks = Vec<u8>;

/// Reads the copy for a search. Owns its scratch buffers, so a whole search
/// allocates a line and the view's marks, not one buffer per row.
struct Scanner<'a> {
    gd: &'a Grid,
    line: Line,
}

impl<'a> Scanner<'a> {
    fn new(gd: &'a Grid) -> Self {
        Scanner {
            gd,
            line: Line::default(),
        }
    }

    fn total(&self) -> usize {
        self.gd.total()
    }

    /// The rows tmux's regex search joins for row `py` from cell `first`.
    fn regex_rows(&self, py: usize, first: usize) -> usize {
        let (sx, end) = (self.gd.sx, self.total());
        let mut len = sx.saturating_sub(first);
        let mut rows = 1;
        while py + rows < end && len < SEARCH_MAX_LINE && self.gd.wrapped(py + rows - 1) {
            rows += 1;
            len += sx;
        }
        rows
    }

    /// The rows a plain match from row `py` can reach: `width` cells past
    /// the row, a tab counting as up to a whole row.
    fn plain_rows(&self, py: usize, width: usize) -> usize {
        let (sx, end) = (self.gd.sx.max(1), self.total());
        let mut rows = 1;
        while py + rows < end && (rows - 1) * sx < width + sx && self.gd.wrapped(py + rows - 1) {
            rows += 1;
        }
        rows
    }

    fn load(&mut self, py: usize, rows: usize) {
        if self.line.py != py || self.line.rows != rows || self.line.starts.is_empty() {
            let gd = self.gd;
            let line = &mut self.line;
            line.py = py;
            line.rows = rows;
            line.text.clear();
            line.starts.clear();
            for y in py..py + rows {
                for x in 0..gd.sx {
                    line.starts.push(line.text.len());
                    push_cell(&mut line.text, gd, x, y);
                }
            }
            line.starts.push(line.text.len());
        }
    }

    /// The cell (counted along the joined rows) where byte `off` of the
    /// line starts, as tmux's `window_copy_cstrtocellpos`: the first cell
    /// there, or one past the end.
    fn cell_at(&self, off: usize) -> usize {
        let cells = self.line.starts.len() - 1;
        self.line.starts[..cells].partition_point(|&s| s < off)
    }

    /// tmux's `window_copy_search_lr_regex`: the first match starting on
    /// row `py` in `first..last`, and its width in cells.
    fn lr_regex(
        &mut self,
        pat: &Pattern,
        py: usize,
        first: usize,
        last: usize,
    ) -> Option<(usize, usize)> {
        if first >= last {
            return None;
        }
        let sx = self.gd.sx;
        let rows = self.regex_rows(py, first);
        self.load(py, rows);
        let off = self.line.starts[first];
        let re = if first != 0 { &pat.notbol } else { &pat.bol };
        let m = re.find(&self.line.text[off..])?;
        if m.is_empty() {
            return None;
        }
        let (s, e) = (self.cell_at(off + m.start()), self.cell_at(off + m.end()));
        (s < sx && s < last).then_some((s, e - s))
    }

    /// tmux's `window_copy_search_rl_regex` and `window_copy_last_regex`:
    /// the last match starting on row `py` in `first..last`. Each next
    /// match is looked for in the rest of the line as a string of its own,
    /// so `^` matches again there, as tmux's `regexec(buf + px)` does.
    fn rl_regex(
        &mut self,
        pat: &Pattern,
        py: usize,
        first: usize,
        last: usize,
    ) -> Option<(usize, usize)> {
        let sx = self.gd.sx;
        let rows = self.regex_rows(py, first);
        self.load(py, rows);
        let base = self.line.starts[first.min(self.line.starts.len() - 1)];
        let re = if first != 0 { &pat.notbol } else { &pat.bol };
        let mut pos = 0;
        let mut saved = None;
        while let Some(m) = re.find(&self.line.text[base + pos..]) {
            if m.is_empty() {
                break;
            }
            let s = self.cell_at(base + pos + m.start());
            if s >= sx || s >= last {
                break;
            }
            let e = self.cell_at(base + pos + m.end());
            if e >= sx || e >= last {
                return Some((s, e - s));
            }
            saved = Some((s, e - s));
            pos += m.end();
        }
        saved.filter(|&(_, w)| w > 0)
    }

    /// tmux's `window_copy_search_lr`: the first plain match starting on
    /// row `py` in `first..last`.
    fn lr(&mut self, pat: &Pattern, py: usize, first: usize, last: usize) -> Option<usize> {
        let sx = self.gd.sx;
        if first >= last || first >= sx {
            return None;
        }
        let rows = self.plain_rows(py, pat.width);
        self.load(py, rows);
        let m = pat.bol.find_at(&self.line.text, self.line.starts[first])?;
        let s = self.cell_at(m.start());
        (s < sx && s < last).then_some(s)
    }

    /// tmux's `window_copy_search_rl`: the last plain match starting on
    /// row `py` in `first..last`.
    fn rl(&mut self, pat: &Pattern, py: usize, first: usize, last: usize) -> Option<usize> {
        let sx = self.gd.sx;
        if first >= last || first >= sx {
            return None;
        }
        let rows = self.plain_rows(py, pat.width);
        self.load(py, rows);
        let mut at = self.line.starts[first];
        let mut best = None;
        while let Some(m) = pat.bol.find_at(&self.line.text, at) {
            let s = self.cell_at(m.start());
            if s >= sx || s >= last {
                break;
            }
            best = Some(s);
            let step = self.line.text[m.start()..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
            at = m.start() + step;
        }
        best
    }
}

/// One cell's text, as tmux's `window_copy_cellstring`: a tab is `\t`, a
/// wide character's second half or a tab's padding nothing, a cell past
/// the end of its row a blank.
fn push_cell(text: &mut String, gd: &Grid, x: usize, y: usize) {
    match gd.look(x, y) {
        None => text.push(' '),
        Some((TAB_PAD, _)) => {}
        Some(('\t', _)) => text.push('\t'),
        Some((_, look)) if look.flags.contains(CellFlags::WIDE_CHAR_SPACER) => {}
        Some((_, look)) if look.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER) => {
            text.push(' ');
        }
        Some((c, look)) => {
            text.push(c);
            text.extend(&look.zerowidth);
        }
    }
}

// ---- tmux's window_copy_search ---------------------------------------------

/// What a search keeps between searches (tmux's `window_copy_mode_data`
/// fields, and the pane's `searchstr`).
#[derive(Debug, Clone)]
struct Kept {
    /// `searchall`: the next search counts every match.
    all: bool,
    /// The pane's last search and whether it was a regex (`wp->searchstr`,
    /// `wp->searchregex`).
    last: Option<(String, bool)>,
    /// The pattern the marks are of; `None` when there are no marks
    /// (`searchmark == NULL`).
    marks: Option<Arc<Pattern>>,
    /// `searchcount`, -1 for none; `searchmore`; `timeout`.
    count: i32,
    more: bool,
    timeout: bool,
    /// `searchdirection`: the last search went down.
    down: bool,
}

impl Default for Kept {
    fn default() -> Self {
        Kept {
            all: true,
            last: None,
            marks: None,
            count: 0,
            more: false,
            timeout: false,
            down: false,
        }
    }
}

/// A search on its way to the blocking pool.
struct Request {
    grid: Arc<Grid>,
    view: View,
    text: String,
    kept: Kept,
    wrap: bool,
    vi: bool,
    down: bool,
    regex: bool,
    times: u32,
}

/// What it found.
pub struct Outcome {
    grid: Arc<Grid>,
    view: View,
    kept: Kept,
}

/// One search over the copy, on a scratch view (tmux's `data` and the
/// functions that take it).
struct Run<'a> {
    scan: Scanner<'a>,
    view: View,
    kept: Kept,
    /// The marks of the rows on view, as the last marking left them.
    marks: Option<Marks>,
    wrap: bool,
    vi: bool,
}

impl Run<'_> {
    fn gd(&self) -> &Grid {
        self.scan.gd
    }

    fn sx(&self) -> usize {
        self.gd().sx
    }

    fn last_row(&self) -> usize {
        self.gd().total() - 1
    }

    /// tmux's `window_copy_scroll_to`.
    fn scroll_to(&mut self, px: usize, py: usize) {
        let (hsize, sy) = (self.gd().hsize, self.gd().sy);
        let v = &mut self.view;
        v.cx = px;
        if py >= hsize - v.oy && py < hsize - v.oy + sy {
            v.cy = py - (hsize - v.oy);
        } else {
            let gap = sy / 4;
            let offset;
            if py < sy {
                offset = 0;
                v.cy = py;
            } else if py > hsize + sy - gap {
                offset = hsize;
                v.cy = py - hsize;
            } else {
                offset = py + gap - sy;
                v.cy = py - offset;
            }
            v.oy = hsize - offset;
        }
    }

    /// tmux's `window_copy_move_left`.
    fn move_left(&self, fx: &mut usize, fy: &mut usize, wrap: bool) {
        if *fx == 0 {
            if *fy == 0 {
                if wrap {
                    *fx = self.sx() - 1;
                    *fy = self.last_row();
                }
                return;
            }
            *fx = self.sx() - 1;
            *fy -= 1;
        } else {
            *fx -= 1;
        }
    }

    /// tmux's `window_copy_move_right`.
    fn move_right(&self, fx: &mut usize, fy: &mut usize, wrap: bool) {
        if *fx >= self.sx() - 1 {
            if *fy == self.last_row() {
                if wrap {
                    *fx = 0;
                    *fy = 0;
                }
                return;
            }
            *fx = 0;
            *fy += 1;
        } else {
            *fx += 1;
        }
    }

    /// tmux's `window_copy_search_mark_at`: the marks' index of a cell, if
    /// its row is on view.
    fn mark_at(&self, px: usize, py: usize) -> Option<usize> {
        mark_at(self.gd(), self.view.oy, px, py)
    }

    /// tmux's `window_copy_search_back_overlap`: for a match at the start
    /// of a soft-wrapped row, the longest earlier match ending in the
    /// same place. `py` is one past the row, as tmux counts it.
    fn back_overlap(
        &mut self,
        pat: &Pattern,
        px: &mut usize,
        w: usize,
        py: &mut usize,
        endline: usize,
    ) {
        let sx = self.sx();
        let end_of = |px: usize, w: usize, row: usize| {
            let (mut x, mut y) = (px + w, row);
            while x > sx - 1 {
                x -= sx;
                y += 1;
            }
            (x, y)
        };
        let old = end_of(*px, w, *py - 1);
        let mut end = old;
        let (mut cpx, mut cpy) = (*px, *py);
        let mut found = true;
        while found
            && cpx == 0
            && cpy >= 2
            && cpy - 1 > endline
            && self.gd().wrapped(cpy - 2)
            && end == old
        {
            cpy -= 1;
            match self.scan.rl_regex(pat, cpy - 1, 0, sx) {
                Some((x, sw)) => {
                    cpx = x;
                    end = end_of(x, sw, cpy - 1);
                    if end == old {
                        *px = cpx;
                        *py = cpy;
                    }
                }
                None => found = false,
            }
        }
    }

    /// tmux's `window_copy_search_jump`: searches from `fx, fy` towards
    /// `endline` and puts the cursor on what it finds, wrapping once if
    /// `wrap`.
    fn jump(
        &mut self,
        pat: &Pattern,
        mut fx: usize,
        fy: usize,
        endline: usize,
        wrap: bool,
        down: bool,
    ) -> bool {
        let sx = self.sx();
        let mut hit = None;
        if down {
            for i in fy..=endline {
                let found = if pat.regex {
                    self.scan.lr_regex(pat, i, fx, sx).map(|(px, _)| px)
                } else {
                    self.scan.lr(pat, i, fx, sx)
                };
                if let Some(px) = found {
                    hit = Some((px, i));
                    break;
                }
                fx = 0;
            }
        } else {
            let mut i = fy + 1;
            while endline < i {
                let found = if pat.regex {
                    self.scan
                        .rl_regex(pat, i - 1, 0, fx + 1)
                        .map(|(mut px, w)| {
                            self.back_overlap(pat, &mut px, w, &mut i, endline);
                            px
                        })
                } else {
                    self.scan.rl(pat, i - 1, 0, fx + 1)
                };
                if let Some(px) = found {
                    hit = Some((px, i - 1));
                    break;
                }
                fx = sx - 1;
                i -= 1;
            }
        }
        if let Some((px, py)) = hit {
            self.scroll_to(px, py);
            return true;
        }
        if wrap {
            let (x, y) = if down {
                (0, 0)
            } else {
                (sx - 1, self.last_row())
            };
            return self.jump(pat, x, y, fy, false, down);
        }
        false
    }

    /// tmux's `window_copy_move_after_search_mark`.
    fn move_after_mark(&self, fx: &mut usize, fy: &mut usize, wrap: bool) {
        let Some(marks) = &self.marks else {
            return;
        };
        let Some(start) = self.mark_at(*fx, *fy) else {
            return;
        };
        let first = marks.get(start).copied().unwrap_or(0);
        if first == 0 {
            return;
        }
        while let Some(at) = self.mark_at(*fx, *fy) {
            if marks.get(at).copied().unwrap_or(0) != first {
                break;
            }
            if !wrap && *fx == self.sx() - 1 && *fy == self.last_row() {
                break;
            }
            self.move_right(fx, fy, wrap);
        }
    }

    fn clear_marks(&mut self) {
        self.kept.count = -1;
        self.kept.more = false;
        self.kept.marks = None;
        self.marks = None;
    }

    /// tmux's `window_copy_search`: one search for `text`, down or up.
    fn search(&mut self, text: &str, down: bool, regex: bool) -> bool {
        let regex = regex && has_regex_chars(text);
        self.kept.down = down;
        if self.kept.timeout {
            return false;
        }
        let visible_only = match &self.kept.last {
            Some((last, r)) if !self.kept.all && *r == regex => last == text,
            _ => {
                self.kept.all = false;
                false
            }
        };
        if !visible_only && self.kept.marks.is_some() {
            self.clear_marks();
        }
        self.kept.last = Some((text.to_string(), regex));
        let hsize = self.gd().hsize;
        let (mut fx, mut fy) = (self.view.cx, hsize - self.view.oy + self.view.cy);
        if UnicodeWidthStr::width(text) == 0 {
            return false;
        }
        let Some(pat) = Pattern::compile(text, regex).map(Arc::new) else {
            return false;
        };
        let wrap = self.wrap;
        let endline = if down {
            if self.vi {
                if self.marks.is_some() {
                    self.move_after_mark(&mut fx, &mut fy, wrap);
                } else {
                    self.move_right(&mut fx, &mut fy, wrap);
                }
            }
            self.last_row()
        } else {
            self.move_left(&mut fx, &mut fy, wrap);
            0
        };
        let found = self.jump(&pat, fx, fy, endline, wrap, down);
        if !found {
            return false;
        }
        self.mark(&pat, visible_only);
        let hsize = self.gd().hsize;
        let (mut fx, mut fy) = (self.view.cx, hsize - self.view.oy + self.view.cy);
        let mark = |run: &Self, at: usize| run.marks.as_ref().and_then(|m| m.get(at).copied());
        // Searching down, if the cursor is not at the start of its match,
        // search again.
        if down
            && let Some(at) = self.mark_at(fx, fy)
            && at > 0
            && self.marks.is_some()
            && mark(self, at) == mark(self, at - 1)
        {
            self.move_after_mark(&mut fx, &mut fy, wrap);
            self.jump(&pat, fx, fy, endline, wrap, down);
            fx = self.view.cx;
            fy = hsize - self.view.oy + self.view.cy;
        }
        if down {
            if !self.vi {
                // Emacs puts the cursor just after the match.
                self.move_after_mark(&mut fx, &mut fy, wrap);
                self.view.cx = fx;
                self.view.cy = fy + self.view.oy - hsize;
            }
        } else if let Some(start) = self.mark_at(fx, fy) {
            // Searching up, the cursor goes to the start of the match.
            let first = mark(self, start);
            while let Some(at) = self.mark_at(fx, fy) {
                if self.marks.is_none() || mark(self, at) != first {
                    break;
                }
                self.view.cx = fx;
                self.view.cy = fy + self.view.oy - hsize;
                if at == 0 {
                    break;
                }
                self.move_left(&mut fx, &mut fy, false);
            }
        }
        true
    }

    /// tmux's `window_copy_search_marks`: marks the matches on view and,
    /// unless `visible_only`, counts every match, within its time limits.
    fn mark(&mut self, pat: &Arc<Pattern>, visible_only: bool) {
        let started = Instant::now();
        let gd = self.scan.gd;
        let (start, end, mut stop) = if visible_only {
            let (s, e) = visible_lines(gd, self.view.oy);
            (s, e, None)
        } else {
            (0, gd.total(), Some(started + SEARCH_ALL_TIMEOUT))
        };
        let (mut start, mut end) = (start, end);
        let mut nfound = 0usize;
        let mut stopped = false;
        let marks = loop {
            let mut marks = vec![0u8; gd.sx * gd.sy];
            let mut generation = 1u8;
            for py in start..end {
                nfound += mark_row(
                    &mut self.scan,
                    pat,
                    py,
                    self.view.oy,
                    &mut marks,
                    &mut generation,
                );
                let now = Instant::now();
                if now - started > SEARCH_TIMEOUT {
                    self.kept.timeout = true;
                    break;
                }
                if stop.is_some_and(|s| now > s) {
                    stopped = true;
                    break;
                }
            }
            if self.kept.timeout {
                self.clear_marks();
                return;
            }
            if stopped && stop.is_some() {
                (start, end) = visible_lines(gd, self.view.oy);
                stop = None;
                continue;
            }
            break marks;
        };
        if !visible_only {
            if stopped {
                self.kept.count = match nfound {
                    n if n > 1000 => 1000,
                    n if n > 100 => 100,
                    n if n > 10 => 10,
                    _ => -1,
                };
                self.kept.more = true;
            } else {
                self.kept.count = i32::try_from(nfound).unwrap_or(i32::MAX);
                self.kept.more = false;
            }
        }
        self.kept.marks = Some(Arc::clone(pat));
        self.marks = Some(marks);
    }
}

/// tmux's `window_copy_visible_lines`: the rows on view, from the start
/// of the soft-wrapped line the top row is part of.
fn visible_lines(gd: &Grid, oy: usize) -> (usize, usize) {
    let mut start = gd.hsize - oy;
    while start > 0 && gd.wrapped(start - 1) {
        start -= 1;
    }
    (start, gd.hsize - oy + gd.sy)
}

fn mark_at(gd: &Grid, oy: usize, px: usize, py: usize) -> Option<usize> {
    let top = gd.hsize - oy;
    (py >= top && py < top + gd.sy).then(|| (py - top) * gd.sx + px)
}

/// Marks and counts the matches starting on row `py` (the inner loop of
/// tmux's `window_copy_search_marks` and its `window_copy_search_mark_match`).
fn mark_row(
    scan: &mut Scanner<'_>,
    pat: &Pattern,
    py: usize,
    oy: usize,
    marks: &mut [u8],
    generation: &mut u8,
) -> usize {
    let gd = scan.gd;
    let sx = gd.sx;
    let size = marks.len();
    let clip = |width: usize, b: usize| if b + width > size { size - b } else { width };
    let mut px = 0;
    let mut found = 0;
    loop {
        let width = if pat.regex {
            let Some((x, mut w)) = scan.lr_regex(pat, py, px, sx) else {
                break;
            };
            px = x;
            let tab = gd.get(px + w - 1, py).tab;
            if tab > 2 {
                w += tab - 1;
            }
            w
        } else {
            let Some(x) = scan.lr(pat, py, px, sx) else {
                break;
            };
            px = x;
            pat.width
        };
        found += 1;
        let mut w = width;
        if let Some(b) = mark_at(gd, oy, px, py).filter(|&b| b < size) {
            w = clip(width, b);
            let mut i = b;
            while i < b + w {
                if !pat.regex {
                    let tab = gd.get(px + (i - b), py).tab;
                    if tab > 0 {
                        w += tab - 1;
                    }
                    w = clip(w, b);
                }
                if marks[i] == 0 {
                    marks[i] = *generation;
                }
                i += 1;
            }
            *generation = if *generation == u8::MAX {
                1
            } else {
                *generation + 1
            };
        }
        px += w.max(1);
    }
    found
}

/// Runs a search request: tmux's `window_copy_search_up` or `_down`, as
/// many times as its count.
fn scan(req: Request) -> Outcome {
    let mut run = Run {
        scan: Scanner::new(&req.grid),
        view: req.view,
        marks: None,
        kept: req.kept,
        wrap: req.wrap,
        vi: req.vi,
    };
    // The marks the last search left on this view, which a search down in
    // vi starts after.
    if let Some(pat) = run.kept.marks.clone() {
        run.marks = Some(view_marks_of(&mut run.scan, &pat, run.view.oy));
    }
    for _ in 0..req.times {
        run.search(&req.text, req.down, req.regex);
    }
    let (view, kept) = (run.view, run.kept);
    Outcome {
        grid: Arc::clone(&req.grid),
        view,
        kept,
    }
}

/// The marks of the rows on view at offset `oy`.
fn view_marks_of(scan: &mut Scanner<'_>, pat: &Pattern, oy: usize) -> Marks {
    let gd = scan.gd;
    let mut marks = vec![0u8; gd.sx * gd.sy];
    let mut generation = 1;
    let (start, end) = visible_lines(gd, oy);
    for py in start..end {
        mark_row(scan, pat, py, oy, &mut marks, &mut generation);
    }
    marks
}

// ---- The mode's side ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Kind {
    #[default]
    Off,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    Forward,
    Backward,
    ToForward,
    ToBackward,
}

/// A search a command asked for, which [`launch`] starts.
#[derive(Debug, Clone, Copy)]
struct Pending {
    down: bool,
    regex: bool,
    times: u32,
}

/// One mode's search and jump state. Lives on `Mode.search`.
#[derive(Debug, Default)]
pub struct Search {
    /// `searchtype`.
    kind: Kind,
    /// `searchregex`: the last search command was not a `-text` one.
    regex: bool,
    kept: Kept,
    /// `jumptype`, `jumpchar`.
    jump: Option<(Jump, String)>,
    pending: Option<Pending>,
    /// The copy a search in flight reads; commands wait until it ends.
    busy: Option<Arc<Grid>>,
    held: VecDeque<Vec<String>>,
    /// The replies waiting for the search and the commands held behind it.
    waiters: Vec<oneshot::Sender<Result<String, Failure>>>,
}

impl Drop for Search {
    /// A mode that goes while busy (its pane killed, a held `cancel`)
    /// answers whoever waited on it.
    fn drop(&mut self) {
        for w in self.waiters.drain(..) {
            let _ = w.send(Ok(String::new()));
        }
    }
}

/// The search a pane remembers between copy modes (tmux's `wp->searchstr`).
#[derive(Debug, Clone)]
pub struct Remembered(String, bool);

/// What the pane remembers when its mode ends.
pub fn remember(mode: &Mode) -> Option<Remembered> {
    let (text, regex) = mode.search.kept.last.clone()?;
    Some(Remembered(text, regex))
}

/// A new mode starts with the pane's last search, upwards, as tmux's
/// `window_copy_common_init`, so `n` repeats it.
pub fn restore(mode: &mut Mode, last: &Remembered) {
    mode.search.kind = Kind::Up;
    mode.search.regex = last.1;
    mode.search.kept.last = Some((last.0.clone(), last.1));
    mode.search_str = Some(last.0.clone());
}

/// tmux's `window_copy_clear_marks`: the search matches are dropped.
pub fn clear_marks(mode: &mut Mode) {
    let kept = &mut mode.search.kept;
    kept.count = -1;
    kept.more = false;
    kept.marks = None;
}

/// After each `send-keys -X` command that ran (`ran`), the marks go as
/// tmux's `window_copy_command` drops them: never for a `search-` one, in
/// vi only for the commands in [`CLEAR_ALWAYS`].
pub fn after_command(mode: &mut Mode, name: &str, ran: bool) {
    if !ran || name.starts_with("search-") || mode.search.kept.marks.is_none() {
        return;
    }
    let clear = if mode.vi {
        CLEAR_ALWAYS.contains(&name)
    } else {
        !CLEAR_NEVER.contains(&name)
    };
    if clear {
        clear_marks(mode);
    }
}

/// A search or jump command, or `None` if `name` is not one (or its
/// arguments do not fit). A search only records what to search for:
/// [`launch`] runs it.
pub fn command(mode: &mut Mode, name: &str, args: &[String], _ctx: &Ctx<'_>) -> Option<Action> {
    let np = mode.prefix;
    match name {
        // `mod.rs` has checked the string and kept it in `search_str`.
        "search-forward" | "search-backward" | "search-forward-text" | "search-backward-text" => {
            let down = name.starts_with("search-forward");
            let regex = !name.ends_with("-text");
            let search = &mut mode.search;
            search.kind = if down { Kind::Down } else { Kind::Up };
            search.regex = regex;
            search.kept.timeout = false;
            search.pending = Some(Pending {
                down,
                regex,
                times: np,
            });
        }
        "search-again" | "search-reverse" => {
            positionals(args, 0, 0)?;
            let reverse = name == "search-reverse";
            let down = match mode.search.kind {
                Kind::Off => return Some(Action::Nothing),
                Kind::Down => !reverse,
                Kind::Up => reverse,
            };
            if mode.search_str.is_some() {
                mode.search.pending = Some(Pending {
                    down,
                    regex: mode.search.regex,
                    times: np,
                });
            }
        }
        "jump-forward" | "jump-backward" | "jump-to-forward" | "jump-to-backward" => {
            let arg = positionals(args, 1, 1)?;
            let Some(c) = arg[0].chars().next() else {
                return Some(Action::Nothing);
            };
            let kind = match name {
                "jump-forward" => Jump::Forward,
                "jump-backward" => Jump::Backward,
                "jump-to-forward" => Jump::ToForward,
                _ => Jump::ToBackward,
            };
            mode.search.jump = Some((kind, c.to_string()));
            for _ in 0..np {
                jump(mode, kind);
            }
        }
        "jump-again" | "jump-reverse" => {
            positionals(args, 0, 0)?;
            if let Some((kind, _)) = mode.search.jump {
                let kind = if name == "jump-reverse" {
                    match kind {
                        Jump::Forward => Jump::Backward,
                        Jump::Backward => Jump::Forward,
                        Jump::ToForward => Jump::ToBackward,
                        Jump::ToBackward => Jump::ToForward,
                    }
                } else {
                    kind
                };
                for _ in 0..np {
                    jump(mode, kind);
                }
            }
        }
        _ => return None,
    }
    Some(Action::Nothing)
}

/// Whether `pane`'s mode has a search in flight.
pub fn busy(state: &State, pane: PaneId) -> bool {
    state
        .copy
        .get(pane)
        .is_some_and(|m| m.search.busy.is_some())
}

/// A command for a busy mode waits behind its search, and so does the
/// request that sent it.
pub fn hold(state: &mut State, pane: PaneId, args: &[String]) {
    let waiter = state
        .deferred_reply
        .is_none()
        .then(|| dispatch::defer_reply(state));
    let Some(mode) = state.copy.get_mut(pane) else {
        return;
    };
    let search = &mut mode.search;
    if search.held.len() < MAX_HELD {
        search.held.push_back(args.to_vec());
    }
    search.waiters.extend(waiter);
}

/// Starts the search the last command asked for, if any, off the actor.
/// With `wait`, the request that sent the command is answered when it
/// ends.
pub fn launch(state: &mut State, pane: PaneId, wait: bool) {
    let Some(req) = request(state, pane) else {
        return;
    };
    let waiter = (wait && state.deferred_reply.is_none()).then(|| dispatch::defer_reply(state));
    if let Some(mode) = state.copy.get_mut(pane) {
        mode.search.busy = Some(Arc::clone(&req.grid));
        mode.search.waiters.extend(waiter);
    }
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            let jobs = state.jobs.clone();
            rt.spawn_blocking(move || {
                let out = scan(req);
                let _ =
                    jobs.blocking_send(Box::new(move |state: &mut State| finish(state, pane, out)));
            });
        }
        // No runtime (unit tests): there is no actor to keep free.
        Err(_) => finish(state, pane, scan(req)),
    }
}

fn request(state: &mut State, pane: PaneId) -> Option<Request> {
    let model = &state.model;
    let wrap = model
        .options
        .lookup("wrap-search", &options::chain(model, pane))
        != Some("off");
    let mode = state.copy.get_mut(pane)?;
    let pending = mode.search.pending.take()?;
    Some(new_request(mode, pending, wrap))
}

fn new_request(mode: &Mode, p: Pending, wrap: bool) -> Request {
    Request {
        grid: Arc::clone(&mode.grid),
        view: View {
            cx: mode.cx,
            cy: mode.cy,
            oy: mode.oy,
        },
        text: mode.search_str.clone().unwrap_or_default(),
        kept: mode.search.kept.clone(),
        wrap,
        vi: mode.vi,
        down: p.down,
        regex: p.regex,
        times: p.times,
    }
}

/// Applies a search's outcome to the mode it ran for, unless that mode
/// took a new copy meanwhile.
fn apply(mode: &mut Mode, out: Outcome) {
    if !Arc::ptr_eq(&mode.grid, &out.grid) {
        return;
    }
    mode.cx = out.view.cx;
    mode.cy = out.view.cy;
    mode.oy = out.view.oy;
    mode.search.kept = out.kept;
    select::update_selection(mode);
}

/// On the actor once the scan is done: the outcome lands, the held
/// commands run in order (one may start the next search), and once
/// nothing is in flight the waiting replies go.
fn finish(state: &mut State, pane: PaneId, out: Outcome) {
    let Some(mode) = state.copy.get_mut(pane) else {
        return;
    };
    apply(mode, out);
    mode.search.busy = None;
    while let Some(mode) = state.copy.get_mut(pane) {
        if mode.search.busy.is_some() {
            break;
        }
        let Some(args) = mode.search.held.pop_front() else {
            for w in mode.search.waiters.drain(..) {
                let _ = w.send(Ok(String::new()));
            }
            break;
        };
        let _ = super::command_now(state, pane, &args);
        launch(state, pane, false);
    }
    clients::mark_all_dirty(state);
}

// ---- Jumps ---------------------------------------------------------------------

/// tmux's `grid_reader_cell_equals_data`.
fn cell_equals(gd: &Grid, x: usize, y: usize, jc: &str) -> bool {
    let gc = gd.get(x, y);
    if gc.padding {
        return false;
    }
    if gc.tab > 0 {
        return jc == "\t";
    }
    let mut data = String::new();
    push_cell(&mut data, gd, x, y);
    data == jc
}

/// tmux's `grid_reader_cursor_jump`: on along the line and its soft wraps.
fn reader_jump(r: &mut Reader<'_>, jc: &str) -> bool {
    let mut px = r.cx;
    let yy = r.last_row();
    for py in r.cy..=yy {
        let xx = r.gd.line_length(py);
        while px < xx {
            if cell_equals(r.gd, px, py, jc) {
                r.cx = px;
                r.cy = py;
                return true;
            }
            px += 1;
        }
        if py == yy || !r.gd.wrapped(py) {
            return false;
        }
        px = 0;
    }
    false
}

/// tmux's `grid_reader_cursor_jump_back`.
fn reader_jump_back(r: &mut Reader<'_>, jc: &str) -> bool {
    let mut xx = r.cx + 1;
    let mut py = r.cy + 1;
    while py > 0 {
        for px in (1..=xx).rev() {
            if cell_equals(r.gd, px - 1, py - 1, jc) {
                r.cx = px - 1;
                r.cy = py - 1;
                return true;
            }
        }
        if py == 1 || !r.gd.wrapped(py - 2) {
            return false;
        }
        xx = r.gd.line_length(py - 2);
        py -= 1;
    }
    false
}

/// One jump (tmux's `window_copy_cursor_jump` and the other three).
fn jump(mode: &mut Mode, kind: Jump) {
    let Some((_, jc)) = mode.search.jump.clone() else {
        return;
    };
    let (hsize, oy) = (mode.grid.hsize, mode.oy);
    let py = hsize + mode.cy - oy;
    let onemore = !mode.vi;
    let mut r = mode.reader(mode.cx, py);
    let found = match kind {
        Jump::Forward => {
            r.cx += 1;
            reader_jump(&mut r, &jc)
        }
        Jump::ToForward => {
            r.cx += 2;
            let found = reader_jump(&mut r, &jc);
            if found {
                r.cursor_left(true);
            }
            found
        }
        Jump::Backward => {
            r.cursor_left(false);
            reader_jump_back(&mut r, &jc)
        }
        Jump::ToBackward => {
            r.cursor_left(false);
            r.cursor_left(false);
            let found = reader_jump_back(&mut r, &jc);
            if found {
                r.cursor_right(true, false, onemore);
            }
            found
        }
    };
    let (px, py) = (r.cx, r.cy);
    if !found {
        return;
    }
    match kind {
        Jump::Forward | Jump::ToForward => mode.acquire_cursor_down(hsize, oy, px, py),
        Jump::Backward | Jump::ToBackward => mode.acquire_cursor_up(hsize, oy, px, py),
    }
}

// ---- Drawing and formats ---------------------------------------------------------

/// The marks on view and the match the cursor is on, worked out once per
/// frame for a pane ([`view_marks`]).
pub struct ViewMarks {
    marks: Marks,
    sx: usize,
    /// The current match's first and last marks index (tmux's
    /// `window_copy_match_start_end` at the cursor).
    current: Option<(usize, usize)>,
}

/// tmux's `window_copy_match_start_end`.
fn match_start_end(marks: &[u8], at: usize) -> (usize, usize) {
    let mark = marks[at];
    let (mut start, mut end) = (at, at);
    while start > 0 && marks[start - 1] == mark {
        start -= 1;
    }
    while end + 1 < marks.len() && marks[end + 1] == mark {
        end += 1;
    }
    (start, end)
}

/// The cursor's marks index.
fn cursor_at(mode: &Mode) -> Option<usize> {
    let py = mode.grid.hsize - mode.oy + mode.cy;
    mark_at(&mode.grid, mode.oy, mode.cx, py)
}

/// The marks of `mode`'s rows on view, if it has any.
pub fn view_marks(mode: &Mode) -> Option<ViewMarks> {
    let pat = mode.search.kept.marks.as_ref()?;
    let marks = view_marks_of(&mut Scanner::new(&mode.grid), pat, mode.oy);
    let at = |i: usize| marks.get(i).copied().unwrap_or(0);
    let current = cursor_at(mode).and_then(|cursor| {
        let c = if cursor != 0 && !mode.vi && mode.search.kept.down {
            cursor - 1
        } else {
            cursor
        };
        (at(c) != 0).then(|| match_start_end(&marks, c))
    });
    Some(ViewMarks {
        marks,
        sx: mode.grid.sx,
        current,
    })
}

impl ViewMarks {
    /// Styles view cell `x, y` as tmux's `window_copy_update_style` does a
    /// match: in `current` if it is in the cursor's match, else in `all`,
    /// fore- and background swapped on the mark's own cell (`inv`).
    pub fn restyle(
        &self,
        x: usize,
        y: usize,
        inv: bool,
        all: &Style,
        current: &Style,
        cell: &mut SnapCell,
    ) {
        if x >= self.sx {
            return;
        }
        let i = y * self.sx + x;
        if self.marks.get(i).copied().unwrap_or(0) == 0 {
            return;
        }
        let style = match self.current {
            Some((s, e)) if (s..=e).contains(&i) => current,
            _ => all,
        };
        let (fg, bg) = if inv {
            (style.bg, style.fg)
        } else {
            (style.fg, style.bg)
        };
        cell.fg = fg;
        cell.bg = bg;
        cell.flags = (cell.flags & !STYLE_FLAGS) | (style.attrs & STYLE_FLAGS);
    }
}

/// tmux's `window_copy_match_at_cursor`: the match the cursor is on or
/// just after.
fn match_at_cursor(mode: &Mode) -> Option<String> {
    let vm = view_marks(mode)?;
    let marks = &vm.marks;
    let mut at = cursor_at(mode)?;
    if marks.get(at).copied().unwrap_or(0) == 0 {
        if at == 0 || marks.get(at - 1).copied().unwrap_or(0) == 0 {
            return None;
        }
        at -= 1;
    }
    let (start, end) = match_start_end(marks, at);
    let gd = &mode.grid;
    let sx = gd.sx;
    let top = gd.hsize - mode.oy;
    let mut out = String::new();
    for i in start..=end {
        let (px, py) = (i % sx, top + i / sx);
        let gc = gd.get(px, py);
        if gc.tab > 0 {
            out.push('\t');
        } else if !gc.padding {
            push_cell(&mut out, gd, px, py);
        }
    }
    Some(out)
}

/// The search's format variables (tmux's `window_copy_formats`).
pub fn format_var(mode: &Mode, name: &str) -> Option<String> {
    let kept = &mode.search.kept;
    let flag = |b: bool| if b { "1" } else { "0" }.to_string();
    match name {
        "search_present" => Some(flag(kept.marks.is_some())),
        "search_timed_out" => Some(flag(kept.timeout)),
        "search_count" if kept.count != -1 => Some(kept.count.to_string()),
        "search_count_partial" if kept.count != -1 => Some(flag(kept.more)),
        "search_match" => Some(match_at_cursor(mode).unwrap_or_default()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::run;
    use super::*;

    fn mode(rows: &[&str], hsize: usize, cursor: (usize, usize)) -> Mode {
        Mode::new(Grid::from_text(20, hsize, rows), cursor, true)
    }

    fn ctx() -> Ctx<'static> {
        Ctx {
            separators: "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~",
        }
    }

    /// Runs a command and, as `launch` would, any search it asked for.
    fn x(m: &mut Mode, words: &[&str]) {
        let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        run(m, &args, &ctx());
        if let Some(p) = m.search.pending.take() {
            let out = scan(new_request(m, p, true));
            apply(m, out);
        }
    }

    fn at(m: &Mode) -> (usize, usize, usize) {
        (m.cx, m.cy, m.oy)
    }

    fn found(m: &Mode) -> String {
        format_var(m, "search_match").unwrap_or_default()
    }

    /// Where a regex search from the top first lands, and what it matches.
    fn first(rows: &[&str], pattern: &str) -> Option<String> {
        let mut m = mode(rows, 0, (0, rows.len() - 1));
        x(&mut m, &["top-line"]);
        x(&mut m, &["search-forward", pattern]);
        (format_var(&m, "search_present").as_deref() == Some("1")).then(|| found(&m))
    }

    #[test]
    fn a_search_steps_through_matches_and_wraps() {
        let rows = ["foo1 x fooo2", "nothing", "fo3", ""];
        let mut m = mode(&rows, 0, (0, 3));
        x(&mut m, &["top-line"]);
        x(&mut m, &["search-forward", "fo+[0-9]"]);
        assert_eq!((at(&m), found(&m)), ((7, 0, 0), "fooo2".into()));
        x(&mut m, &["search-again"]);
        assert_eq!(at(&m), (0, 2, 0));
        x(&mut m, &["search-again"]);
        assert_eq!(at(&m), (0, 0, 0), "wrap-search on wraps");
        assert_eq!(format_var(&m, "search_count").as_deref(), Some("3"));
        x(&mut m, &["search-reverse"]);
        assert_eq!(at(&m), (0, 2, 0));
    }

    #[test]
    fn text_search_takes_the_dot_literally_and_ignores_case() {
        let rows = ["axb a.b A.B", ""];
        let mut m = mode(&rows, 0, (0, 1));
        x(&mut m, &["top-line"]);
        x(&mut m, &["search-forward-text", "a.b"]);
        assert_eq!(m.cx, 4);
        x(&mut m, &["search-again"]);
        assert_eq!(m.cx, 8);
        x(&mut m, &["search-forward-text", "A.B"]);
        assert_eq!(
            (m.cx, format_var(&m, "search_count")),
            (8, Some("1".into()))
        );
    }

    /// tmux searches a pattern with none of `^$*+()?[].\` as text.
    #[test]
    fn a_pattern_without_regex_characters_is_text() {
        assert_eq!(first(&["ab abab", ""], "ab|abab"), None);
        assert_eq!(first(&["xx{3}f", ""], "x{3}f"), Some("x{3}f".into()));
    }

    /// The differences from glibc that remain, as `docs/copy-mode.md` lists
    /// them: alternation is the first alternative that matches, not the
    /// longest, and a back-reference finds nothing.
    #[test]
    fn documented_regex_differences() {
        assert_eq!(first(&["xab", ""], "x(a|ab)"), Some("xa".into()));
        assert_eq!(first(&["abab", ""], "(ab)\\1"), None);
    }

    /// glibc's reading of the escapes, recorded in the `tm_3/dialect`
    /// golden: `\d` is refused, a backslash in brackets is itself.
    #[test]
    fn escapes_read_as_glibc_reads_them() {
        let rows = ["digits 123 dee x\\y", ""];
        assert_eq!(first(&rows, "\\d"), None);
        assert_eq!(first(&rows, "\\n"), None);
        assert_eq!(first(&rows, "[\\d]+"), Some("d".into()));
        assert_eq!(first(&rows, "[[:digit:]]+"), Some("123".into()));
        assert_eq!(first(&rows, "x\\\\y"), Some("x\\y".into()));
        assert_eq!(first(&rows, "\\<dee\\>"), Some("dee".into()));
        assert_eq!(first(&rows, "(?i)dee"), None);
        assert_eq!(first(&rows, "[[:nope:]]"), None);
    }

    /// `docs/copy-mode.md`'s third difference: classes, boundaries and `.`
    /// follow Unicode.
    #[test]
    fn classes_follow_unicode() {
        let rows = ["naive café", ""];
        assert_eq!(first(&rows, "caf\\w"), Some("café".into()));
        assert_eq!(first(&rows, "f.\\>"), Some("fé".into()));
    }

    #[test]
    fn translation() {
        let t = |p: &str| translate(p).map(|t| t.bol);
        assert_eq!(t("a**").as_deref(), Some("(?:a*)*"));
        assert_eq!(t("(ab)+?").as_deref(), Some("(?:(ab)+)?"));
        assert_eq!(t("a{,2}").as_deref(), Some("a{0,2}"));
        assert_eq!(t("[]a]").as_deref(), Some("[\\]a]"));
        assert_eq!(t("a)").as_deref(), Some("a\\)"));
        assert_eq!(t("*a"), None);
        assert_eq!(t("a{x"), None);
        assert_eq!(t("(a"), None);
        assert_eq!(t("\\1"), None);
        assert_eq!(translate("^a").unwrap().notbol, "(?:\\b\\B)a");
    }

    /// A match the terminal split over a soft wrap is found whole.
    #[test]
    fn a_match_across_a_soft_wrap() {
        let mut g = Grid::from_text(8, 0, &["xxxxxxfo", "o7 end", ""]);
        g.rows[0].wrapped = true;
        let mut m = Mode::new(g, (0, 2), true);
        x(&mut m, &["top-line"]);
        x(&mut m, &["search-forward", "foo7"]);
        assert_eq!((at(&m), found(&m)), ((6, 0, 0), "foo7".into()));
        x(&mut m, &["history-bottom"]);
        x(&mut m, &["search-backward", "o+7"]);
        assert_eq!(at(&m), (7, 0, 0));
    }

    #[test]
    fn jumps_and_repeats() {
        let mut m = mode(&["a,b;c,d e,f", ""], 0, (0, 0));
        x(&mut m, &["jump-forward", ","]);
        assert_eq!(m.cx, 1);
        x(&mut m, &["jump-again"]);
        assert_eq!(m.cx, 5);
        x(&mut m, &["jump-reverse"]);
        assert_eq!(m.cx, 1);
        x(&mut m, &["jump-to-forward", ";"]);
        assert_eq!(m.cx, 2);
        x(&mut m, &["end-of-line"]);
        // As tmux: T steps back twice first, so it skips the `,` just
        // before the cursor.
        x(&mut m, &["jump-to-backward", ","]);
        assert_eq!(m.cx, 6);
        x(&mut m, &["jump-forward", ""]);
        assert_eq!(m.cx, 6);
    }

    /// Commands tmux clears the marks for, in vi: a selection command does,
    /// a motion does not.
    #[test]
    fn marks_clear_as_tmux_clears_them() {
        let mut m = mode(&["foo foo", ""], 0, (0, 1));
        x(&mut m, &["search-backward", "foo"]);
        x(&mut m, &["cursor-left"]);
        assert_eq!(format_var(&m, "search_present").as_deref(), Some("1"));
        x(&mut m, &["back-to-indentation"]);
        assert_eq!(format_var(&m, "search_present").as_deref(), Some("0"));
        assert_eq!(format_var(&m, "search_count"), None);
    }

    /// The marks and the overlay's per-frame work are the view's size,
    /// however long the history.
    #[test]
    fn marks_are_bounded_by_the_view() {
        let rows: Vec<String> = (0..5000).map(|i| format!("foo{i}")).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let mut m = Mode::new(Grid::from_text(20, 4990, &refs), (0, 9), true);
        x(&mut m, &["search-backward", "foo[0-9]+"]);
        assert_eq!(format_var(&m, "search_count").as_deref(), Some("5000"));
        let vm = view_marks(&m).unwrap();
        assert_eq!(vm.marks.len(), 20 * 10);
    }

    /// A pattern too big to compile finds nothing rather than taking the
    /// server's memory.
    #[test]
    fn a_huge_pattern_is_refused() {
        let p = "(\\w{100}){100}";
        assert!(Pattern::compile(p, true).is_none());
    }
}
