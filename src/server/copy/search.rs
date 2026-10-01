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
//! reads it ([`translate`]) and run with the `regex` crate, leftmost-longest
//! as POSIX wants (a lazy DFA from `regex-automata` finds the longest end);
//! what still differs is in `docs/copy-mode.md`.
//!
//! A search does not search each row in turn: one pass of a prefilter over
//! the whole copy as a single string ([`Text`], kept for the next search)
//! finds the rows that may match, and only those are searched as tmux
//! searches them.
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
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};
use regex_automata::hybrid::dfa::{Cache, DFA};
use regex_automata::nfa::thompson;
use regex_automata::util::syntax;
use regex_automata::{Anchored, Input, MatchKind};
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
// ponytail: tmux allows 200 ms, on its only thread. Off the actor the
// budget bounds the worker, not a frame, so grofe allows 1 s: a debug build
// on a loaded machine still counts, and a release one counts 100 000 lines
// in a few milliseconds.
const SEARCH_ALL_TIMEOUT: Duration = Duration::from_secs(1);
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
    bol: Engine,
    /// tmux's `REG_NOTBOL`: `^` never matches at the start.
    notbol: Engine,
    /// Matches wherever the pattern can and maybe more, run once over the
    /// whole copy to skip rows; `None` when it matches the empty string,
    /// so every row is searched.
    pre: Option<Regex>,
}

/// One compiled form of a pattern: the crate's leftmost-first regex, which
/// finds where the leftmost match starts, and a lazy DFA that reports the
/// longest match from there, as POSIX's leftmost-longest rule wants.
#[derive(Debug)]
struct Engine {
    re: Regex,
    longest: Option<Longest>,
}

#[derive(Debug)]
struct Longest {
    dfa: DFA,
    cache: Mutex<Cache>,
}

impl Engine {
    fn new(re: Regex, longest: Option<Longest>) -> Engine {
        Engine { re, longest }
    }

    /// The leftmost-longest match in `hay`, as tmux's `regexec`: its
    /// start and end.
    fn find(&self, hay: &str) -> Option<(usize, usize)> {
        let m = self.re.find(hay)?;
        let end = self
            .longest
            .as_ref()
            .and_then(|l| l.end(hay, m.start()))
            .map_or(m.end(), |e| e.max(m.end()));
        Some((m.start(), end))
    }
}

impl Longest {
    fn build(pattern: &str, cis: bool) -> Option<Longest> {
        let dfa = DFA::builder()
            .configure(
                DFA::config()
                    .match_kind(MatchKind::All)
                    .cache_capacity(DFA_SIZE_LIMIT)
                    .unicode_word_boundary(true),
            )
            .syntax(
                syntax::Config::new()
                    .case_insensitive(cis)
                    .nest_limit(NEST_LIMIT),
            )
            .thompson(thompson::Config::new().nfa_size_limit(Some(REGEX_SIZE_LIMIT)))
            .build(pattern)
            .ok()?;
        let cache = Mutex::new(dfa.create_cache());
        Some(Longest { dfa, cache })
    }

    /// The end of the longest match starting at `start`; `None` when the
    /// lazy DFA gives up (a Unicode word boundary next to a non-ASCII
    /// byte, or its cache thrashing), which leaves the leftmost-first end.
    fn end(&self, hay: &str, start: usize) -> Option<usize> {
        let mut cache = self.cache.lock().ok()?;
        let input = Input::new(hay).range(start..).anchored(Anchored::Yes);
        let hm = self.dfa.try_search_fwd(&mut cache, &input).ok()??;
        Some(hm.offset())
    }
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
        let (bol, notbol, pre) = if regex {
            let t = translate(text, cis)?;
            (
                Engine::new(build(&t.bol, cis)?, Longest::build(&t.bol, cis)),
                Engine::new(build(&t.notbol, cis)?, Longest::build(&t.notbol, cis)),
                build(&t.pre, cis).filter(|re| !re.is_match("")),
            )
        } else {
            // As tmux's `window_copy_search_compare`: a one-byte character
            // is compared lowered, any other exactly.
            let flags = if cis { "(?i-u:" } else { "(?-u:" };
            let re = build(&format!("{flags}{})", regex::escape(text)), false)?;
            (
                Engine::new(re.clone(), None),
                Engine::new(re.clone(), None),
                Some(re),
            )
        };
        Some(Pattern {
            regex,
            width: UnicodeWidthStr::width(text),
            bol,
            notbol,
            pre,
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

/// A POSIX pattern in the crate's syntax: with `^` as an anchor, with `^`
/// never matching (for `REG_NOTBOL`), and with no assertions at all, which
/// matches wherever the pattern does and more (the prefilter over the
/// whole copy).
#[derive(Debug, PartialEq, Eq)]
pub struct Translated {
    pub bol: String,
    pub notbol: String,
    pub pre: String,
}

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
pub fn translate(pattern: &str, cis: bool) -> Option<Translated> {
    let mut out = Out::default();
    // Where in the outputs the last atom starts, if the last thing written
    // was an atom or a repetition of one.
    let mut atom: Option<[usize; 3]> = None;
    // Whether the last thing written was a repetition.
    let mut repeated = false;
    // The open groups' starts.
    let mut groups: Vec<[usize; 3]> = Vec::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = out.at();
        // An assertion: written to `bol` and `notbol` as given, left out
        // of `pre`.
        let mut assert = |out: &mut Out, bol: &str, notbol: &str| {
            out.bol.push_str(bol);
            out.notbol.push_str(notbol);
            atom = None;
            repeated = false;
        };
        match c {
            '\\' => {
                let n = *chars.get(i + 1)?;
                i += 2;
                let text = match n {
                    'w' | 'W' | 's' | 'S' => format!("\\{n}"),
                    'b' | 'B' | '<' | '>' => {
                        let a = format!("\\{n}");
                        assert(&mut out, &a, &a);
                        continue;
                    }
                    '`' => {
                        assert(&mut out, "\\A", "\\A");
                        continue;
                    }
                    '\'' => {
                        assert(&mut out, "\\z", "\\z");
                        continue;
                    }
                    n if n.is_ascii_alphanumeric() => return None,
                    n => regex::escape(&n.to_string()),
                };
                out.push(&text);
                atom = Some(start);
                repeated = false;
                continue;
            }
            '[' => {
                let (class, next) = bracket(&chars, i, cis)?;
                out.push(&class);
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
                out.push("(");
                atom = None;
                repeated = false;
            }
            ')' => {
                match groups.pop() {
                    Some(open) => {
                        out.push(")");
                        atom = Some(open);
                    }
                    None => {
                        out.push("\\)");
                        atom = Some(start);
                    }
                }
                repeated = false;
            }
            '|' => {
                out.push("|");
                atom = None;
                repeated = false;
            }
            '*' | '+' | '?' | '{' => {
                let (op, next) = if c == '{' {
                    interval(&chars, i)?
                } else {
                    (c.to_string(), i + 1)
                };
                let at = atom?;
                if repeated {
                    // `a**`: repeat the repetition, not lazily.
                    out.insert(at, "(?:");
                    out.push(")");
                }
                out.push(&op);
                repeated = true;
                i = next;
                continue;
            }
            '^' => assert(&mut out, "^", "(?:\\b\\B)"),
            '$' => assert(&mut out, "$", "$"),
            '.' => {
                out.push(".");
                atom = Some(start);
                repeated = false;
            }
            c => {
                out.push(&regex::escape(&c.to_string()));
                atom = Some(start);
                repeated = false;
            }
        }
        i += 1;
    }
    if !groups.is_empty() {
        return None;
    }
    Some(Translated {
        bol: out.bol,
        notbol: out.notbol,
        pre: out.pre,
    })
}

/// The three spellings [`translate`] writes at once.
#[derive(Default)]
struct Out {
    bol: String,
    notbol: String,
    pre: String,
}

impl Out {
    fn push(&mut self, s: &str) {
        self.bol.push_str(s);
        self.notbol.push_str(s);
        self.pre.push_str(s);
    }

    fn at(&self) -> [usize; 3] {
        [self.bol.len(), self.notbol.len(), self.pre.len()]
    }

    fn insert(&mut self, at: [usize; 3], s: &str) {
        self.bol.insert_str(at[0], s);
        self.notbol.insert_str(at[1], s);
        self.pre.insert_str(at[2], s);
    }
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
fn bracket(chars: &[char], i: usize, cis: bool) -> Option<(String, usize)> {
    let mut out = String::from("[");
    let mut j = i + 1;
    if chars.get(j) == Some(&'^') {
        out.push('^');
        j += 1;
    }
    let mut first = true;
    // The last character written, which a `-` makes a range from.
    let mut prev: Option<char> = None;
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
                ':' => out.push_str(class(&body, cis)?),
                // A collating symbol or equivalence class of one character
                // is that character; anything else glibc's locale decides.
                '=' | '.' if body.chars().count() == 1 => out.push_str(&class_char(body)),
                _ => return None,
            }
            prev = None;
            j = k + 2;
            continue;
        }
        let next = chars.get(j + 1).copied();
        if c == '-' && prev.is_some() && next.is_some_and(|n| n != ']') {
            // glibc refuses a range with an end outside ASCII (recorded:
            // `[α-γ]` finds nothing).
            let to = next?;
            if !prev.is_some_and(|p| p.is_ascii()) || !to.is_ascii() || to == '[' {
                return None;
            }
            out.push('-');
            out.push_str(&class_char(to.to_string()));
            prev = None;
            j += 2;
            continue;
        }
        out.push_str(&class_char(c.to_string()));
        prev = Some(c);
        j += 1;
    }
}

/// A POSIX class as Unicode properties, as glibc's `C.UTF-8` tables read
/// them (recorded in the `tm_3/unicode` golden): letters, and digits of
/// other scripts, are `alpha`; only `0-9` are `digit`; symbols are
/// `punct`; ignoring case, `upper` and `lower` are every letter. `None`
/// for a name glibc does not know.
fn class(name: &str, cis: bool) -> Option<&'static str> {
    Some(match name {
        "upper" | "lower" if cis => r"[\p{Alphabetic}\p{Nd}--0-9]",
        "alpha" => r"[\p{Alphabetic}\p{Nd}--0-9]",
        "alnum" => r"\p{Alphabetic}\p{Nd}",
        "digit" => "0-9",
        "xdigit" => "0-9A-Fa-f",
        "upper" => r"\p{Uppercase}",
        "lower" => r"\p{Lowercase}",
        "space" => r"\p{White_Space}",
        "blank" => r"\t\p{Zs}",
        "punct" => r"\p{P}\p{S}",
        "graph" => r"\p{L}\p{M}\p{N}\p{P}\p{S}",
        "print" => r"\p{L}\p{M}\p{N}\p{P}\p{S}\p{Zs}",
        "cntrl" => r"\p{Cc}",
        _ => return None,
    })
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
    loaded: bool,
    /// Every cell is one byte, so cell and byte offsets are the same and
    /// `starts` is left empty.
    simple: bool,
    starts: Vec<usize>,
}

/// The whole copy as one string for the prefilter: each row as the search
/// writes it, a newline after each row that does not wrap, and where each
/// row starts (one more entry for the end). About a byte a cell.
#[derive(Debug)]
pub struct Text {
    text: String,
    rows: Vec<usize>,
}

impl Text {
    /// The row byte `off` is in.
    fn row_of(&self, off: usize) -> usize {
        self.rows
            .partition_point(|&s| s <= off)
            .saturating_sub(1)
            .min(self.rows.len() - 2)
    }
}

/// How many rows a search up asks the prefilter about at once.
const BLOCK_ROWS: usize = 256;

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
    /// For each of the copy's looks, whether a one-byte character in it
    /// is just that byte (no combining marks, not a wide character's half).
    plain: Vec<bool>,
}

impl<'a> Scanner<'a> {
    fn new(gd: &'a Grid) -> Self {
        let spacer = CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER;
        let plain = gd
            .looks
            .iter()
            .map(|l| l.zerowidth.is_empty() && !l.flags.intersects(spacer))
            .collect();
        Scanner {
            gd,
            line: Line::default(),
            plain,
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

    /// The prefilter's string of the whole copy.
    fn whole(&self) -> Text {
        let gd = self.gd;
        let sx = gd.sx;
        let mut text = String::with_capacity(gd.total() * (sx + 1));
        let mut rows = Vec::with_capacity(gd.total() + 1);
        for y in 0..gd.total() {
            rows.push(text.len());
            let cells = gd.rows.get(y).map_or(&[][..], |r| r.cells.as_slice());
            let n = cells.len().min(sx);
            let simple = cells[..n].iter().all(|cell| {
                let c = cell.c;
                c.is_ascii() && c != '\t' && c != TAB_PAD && self.plain[cell.look as usize]
            });
            if simple {
                text.extend(cells[..n].iter().map(|cell| cell.c));
                text.extend(std::iter::repeat_n(' ', sx - n));
            } else {
                for x in 0..sx {
                    push_cell(&mut text, gd, x, y);
                }
            }
            if !gd.wrapped(y) {
                text.push('\n');
            }
        }
        rows.push(text.len());
        Text { text, rows }
    }

    /// Writes rows `py..py + rows` as the line, unless they already are.
    /// Rows of plain one-byte cells, the common case, are copied as they
    /// are and padded with blanks; only a line with any other cell keeps
    /// where each cell starts.
    fn load(&mut self, py: usize, rows: usize) {
        let line = &mut self.line;
        if line.loaded && line.py == py && line.rows == rows {
            return;
        }
        let gd = self.gd;
        let sx = gd.sx;
        line.py = py;
        line.rows = rows;
        line.loaded = true;
        line.text.clear();
        line.starts.clear();
        line.simple = true;
        'rows: for y in py..py + rows {
            let cells = gd.rows.get(y).map_or(&[][..], |r| r.cells.as_slice());
            let n = cells.len().min(sx);
            for cell in &cells[..n] {
                let c = cell.c;
                if !c.is_ascii() || c == '\t' || c == TAB_PAD || !self.plain[cell.look as usize] {
                    line.simple = false;
                    break 'rows;
                }
                line.text.push(c);
            }
            line.text.extend(std::iter::repeat_n(' ', sx - n));
        }
        if line.simple {
            return;
        }
        line.text.clear();
        for y in py..py + rows {
            for x in 0..sx {
                line.starts.push(line.text.len());
                push_cell(&mut line.text, gd, x, y);
            }
        }
        line.starts.push(line.text.len());
    }

    /// Where cell `cell` (counted along the joined rows) starts in the line.
    fn byte_of(&self, cell: usize) -> usize {
        if self.line.simple {
            cell.min(self.line.text.len())
        } else {
            self.line.starts[cell.min(self.line.starts.len() - 1)]
        }
    }

    /// The cell (counted along the joined rows) where byte `off` of the
    /// line starts, as tmux's `window_copy_cstrtocellpos`: the first cell
    /// there, or one past the end.
    fn cell_at(&self, off: usize) -> usize {
        if self.line.simple {
            return off.min(self.line.text.len());
        }
        // A match that starts or ends inside a cell (a combining mark on
        // its own) maps one past the end, as tmux's does.
        let cells = self.line.starts.len() - 1;
        let c = self.line.starts[..cells].partition_point(|&s| s < off);
        if c < cells && self.line.starts[c] != off {
            return cells;
        }
        c
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
        let off = self.byte_of(first);
        let re = if first != 0 { &pat.notbol } else { &pat.bol };
        let (ms, me) = re.find(&self.line.text[off..])?;
        if ms == me {
            return None;
        }
        let (s, e) = (self.cell_at(off + ms), self.cell_at(off + me));
        (s < sx && s < last && e >= s).then(|| (s, e - s))
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
        let base = self.byte_of(first);
        let re = if first != 0 { &pat.notbol } else { &pat.bol };
        let mut pos = 0;
        let mut saved = None;
        while let Some((ms, me)) = re.find(&self.line.text[base + pos..]) {
            if ms == me {
                break;
            }
            let s = self.cell_at(base + pos + ms);
            if s >= sx || s >= last {
                break;
            }
            let e = self.cell_at(base + pos + me);
            if e >= sx || e >= last {
                return Some((s, e - s));
            }
            saved = Some((s, e - s));
            pos += me;
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
        let m = pat.bol.re.find_at(&self.line.text, self.byte_of(first))?;
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
        let mut at = self.byte_of(first);
        let mut best = None;
        while let Some(m) = pat.bol.re.find_at(&self.line.text, at) {
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
    /// The prefilter's string of `grid`, if an earlier search wrote it.
    whole: Option<Arc<Text>>,
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
    whole: Arc<Text>,
    view: View,
    kept: Kept,
}

/// One search over the copy, on a scratch view (tmux's `data` and the
/// functions that take it).
struct Run<'a> {
    scan: Scanner<'a>,
    /// The whole copy as one string, for the prefilter.
    text: Arc<Text>,
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
        fx: usize,
        fy: usize,
        endline: usize,
        wrap: bool,
        down: bool,
    ) -> bool {
        let sx = self.sx();
        let mut hit = None;
        if down {
            // The first row from `fx`, then each row the prefilter offers.
            let mut row = Some(fy).filter(|&r| r <= endline);
            let mut first = fx;
            while let Some(i) = row {
                if let Some(px) = self.down_at(pat, i, first) {
                    hit = Some((px, i));
                    break;
                }
                first = 0;
                row = self.candidate(pat, i + 1, endline);
            }
        } else if fy >= endline {
            // The first row up to `fx`, then blocks of rows upwards, each
            // row the prefilter offers there from the bottom up.
            hit = self.up_at(pat, fy, fx + 1, endline);
            let mut hi = fy;
            while hit.is_none() && hi > endline {
                let lo = hi.saturating_sub(BLOCK_ROWS).max(endline);
                let mut rows = Vec::new();
                let mut next = self.candidate(pat, lo, hi - 1);
                while let Some(r) = next {
                    rows.push(r);
                    next = self.candidate(pat, r + 1, hi - 1);
                }
                hit = rows
                    .into_iter()
                    .rev()
                    .find_map(|r| self.up_at(pat, r, sx, endline));
                hi = lo;
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

    /// The first match on row `py` from cell `first`.
    fn down_at(&mut self, pat: &Pattern, py: usize, first: usize) -> Option<usize> {
        let sx = self.sx();
        if pat.regex {
            self.scan.lr_regex(pat, py, first, sx).map(|(px, _)| px)
        } else {
            self.scan.lr(pat, py, first, sx)
        }
    }

    /// The last match on row `py` before cell `last`, and the row it ends
    /// up on (an earlier one for a match that wraps onto this row, as
    /// tmux's `back_overlap` finds).
    fn up_at(
        &mut self,
        pat: &Pattern,
        py: usize,
        last: usize,
        endline: usize,
    ) -> Option<(usize, usize)> {
        if pat.regex {
            let (mut px, w) = self.scan.rl_regex(pat, py, 0, last)?;
            let mut i = py + 1;
            self.back_overlap(pat, &mut px, w, &mut i, endline);
            Some((px, i - 1))
        } else {
            self.scan.rl(pat, py, 0, last).map(|px| (px, py))
        }
    }

    /// The first row in `from..=to` that may hold a match: by the
    /// prefilter, one regex pass over the whole copy, so the rows with no
    /// match cost no search of their own.
    fn candidate(&self, pat: &Pattern, from: usize, to: usize) -> Option<usize> {
        if from > to {
            return None;
        }
        let Some(pre) = &pat.pre else {
            return Some(from);
        };
        // A match never runs past the end of its line, so the search stops
        // there instead of reading on to the end of the copy.
        let text = &self.text.text;
        let at = self.text.rows[to];
        let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        let m = pre.find_at(&text[..end], self.text.rows[from])?;
        let row = self.text.row_of(m.start());
        (row <= to).then_some(row)
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
            let mut next = self.candidate(pat, start, end.saturating_sub(1));
            while let Some(py) = next {
                next = self.candidate(pat, py + 1, end.saturating_sub(1));
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
    let scanner = Scanner::new(&req.grid);
    let whole = req.whole.unwrap_or_else(|| Arc::new(scanner.whole()));
    let mut run = Run {
        scan: scanner,
        text: Arc::clone(&whole),
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
        whole,
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
    /// The prefilter's string of the copy, kept for the next search, and
    /// the copy it is of.
    whole: Option<(Weak<Grid>, Arc<Text>)>,
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
                // A scan that panics still lets the mode go on (unbusy, its
                // held commands run), rather than leaving it busy for good.
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan(req))).ok();
                let _ =
                    jobs.blocking_send(Box::new(move |state: &mut State| finish(state, pane, out)));
            });
        }
        // No runtime (unit tests): there is no actor to keep free.
        Err(_) => finish(state, pane, Some(scan(req))),
    }
}

fn request(state: &mut State, pane: PaneId) -> Option<Request> {
    // Most commands ask for no search; they cost no option lookup.
    state.copy.get(pane)?.search.pending?;
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
        whole: mode
            .search
            .whole
            .as_ref()
            .filter(|(of, _)| std::ptr::eq(of.as_ptr(), Arc::as_ptr(&mode.grid)))
            .map(|(_, t)| Arc::clone(t)),
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
    mode.search.whole = Some((Arc::downgrade(&out.grid), out.whole));
    select::update_selection(mode);
}

/// On the actor once the scan is done: the outcome lands, the held
/// commands run in order (one may start the next search), and once
/// nothing is in flight the waiting replies go.
fn finish(state: &mut State, pane: PaneId, out: Option<Outcome>) {
    let Some(mode) = state.copy.get_mut(pane) else {
        return;
    };
    if let Some(out) = out {
        apply(mode, out);
    }
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
/// just after, which a copy with no selection copies.
pub fn match_at_cursor(mode: &Mode) -> Option<String> {
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

    /// Alternation takes the longest match at the leftmost position, as
    /// POSIX does; a back-reference finds nothing (`docs/copy-mode.md`).
    #[test]
    fn alternation_is_leftmost_longest() {
        assert_eq!(first(&["xab", ""], "x(a|ab)"), Some("xab".into()));
        assert_eq!(first(&["abcd", ""], "(a|ab)(c|bcd)"), Some("abcd".into()));
        assert_eq!(first(&["a dee", ""], "d(e|ee)"), Some("dee".into()));
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
        let t = |p: &str| translate(p, false).map(|t| t.bol);
        assert_eq!(t("a**").as_deref(), Some("(?:a*)*"));
        assert_eq!(t("(ab)+?").as_deref(), Some("(?:(ab)+)?"));
        assert_eq!(t("a{,2}").as_deref(), Some("a{0,2}"));
        assert_eq!(t("[]a]").as_deref(), Some("[\\]a]"));
        assert_eq!(t("a)").as_deref(), Some("a\\)"));
        assert_eq!(t("*a"), None);
        assert_eq!(t("a{x"), None);
        assert_eq!(t("(a"), None);
        assert_eq!(t("\\1"), None);
        assert_eq!(translate("^a", false).unwrap().notbol, "(?:\\b\\B)a");
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

    /// A 100 000-line history, one row of text each and 80 wide.
    fn big(lines: usize) -> Mode {
        let rows: Vec<String> = (0..lines)
            .map(|i| format!("line {i} of the history, some text to search"))
            .collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        Mode::new(Grid::from_text(80, lines - 24, &refs), (0, 23), true)
    }

    /// A search over a 100 000-line history (`history-limit 100000`) is one
    /// pass of the prefilter over the copy, not a search per row: each of
    /// these, end to end and wrapping, stays under 250 ms in a release build
    /// (under 10 ms on the machine this was written on). Timing a debug
    /// build says little and loads the suite, so this runs with
    /// `cargo test --release -- 100k`.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing: run with --release")]
    fn a_search_over_a_100k_line_history_is_one_pass() {
        let mut m = big(100_000);
        let limit = Duration::from_millis(250);
        for (cmd, pat, count) in [
            ("search-backward", "no(such|line)[x-z]+", None),
            ("search-forward", "no(such|line)[x-z]+", None),
            ("search-backward", "nosuchtext", None),
            ("search-backward", "line 5 of", Some("1")),
            ("search-backward", "line [0-9]+5 of", Some("9999")),
        ] {
            let t = Instant::now();
            x(&mut m, &[cmd, pat]);
            let took = t.elapsed();
            eprintln!("{cmd} {pat}: {took:?}");
            assert!(took < limit, "{cmd} {pat} took {took:?}");
            if let Some(count) = count {
                assert_eq!(format_var(&m, "search_count").as_deref(), Some(count));
            }
        }
        assert_eq!(found(&m), "line 99995 of");
    }
}
