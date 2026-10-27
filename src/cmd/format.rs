//! `#{...}` format expansion (spec §4.5). M1 supports a bare variable
//! reference, `#{name}`, and a ternary, `#{?cond,then,else}`. M2 adds
//! nesting, `##` escaping, `#{==:a,b}` and the `#{b:var}` modifier.
//!
//! Format-bearing flag values (`-c`, `-F`, `display-message` text) stay
//! raw strings on the parsed `Command`; dispatch calls [`expand`] against
//! the target's context at execution time, not at parse time.

use std::cell::Cell;

mod modify;
mod x11;

pub use modify::escape_arg;

/// Supplies named values to [`expand`]. Implemented by whatever holds the
/// pane/window/session/client state a format string reaches for
/// (`server/model.rs`, from T1.5c onward).
pub trait FormatVars {
    fn get(&self, name: &str) -> Option<String>;

    /// What `#{S:}`, `#{W:}`, `#{P:}` and `#{L:}` visit, already in `sort`'s
    /// order, each with the variables of its own context. `None` when this
    /// context has no session (or window) to loop over: tmux fails the
    /// whole `#{...}` then, and the rest of the format with it.
    fn loop_items(&self, _kind: LoopKind, _sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        None
    }

    /// Whether the context's session has a window called `name` (for
    /// [`NameKind::Window`], `None` without a session) or a session of that
    /// name exists (`#{N:}`).
    fn name_exists(&self, _kind: NameKind, _name: &str) -> Option<bool> {
        None
    }

    /// The first screen line of the context's pane that matches `term`
    /// (`#{C:}`), 1-based, 0 for none; `None` with no pane.
    fn pane_search(&self, _term: &str, _regex: bool, _ignore: bool) -> Option<u32> {
        None
    }
}

/// What a loop modifier visits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopKind {
    Sessions,
    Windows,
    Panes,
    Clients,
}

/// What `#{N:}` asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Session,
    Window,
}

/// tmux 3.7b's `enum sort_order`, the part a loop reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// The list as tmux keeps it.
    Order,
    Index,
    Name,
    Activity,
    Creation,
}

/// A loop's sort: `#{W/nr:}` is [`SortOrder::Name`], reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopSort {
    pub order: SortOrder,
    pub reversed: bool,
}

impl LoopSort {
    /// Sorts `items` as tmux's `sort_qsort` does: [`SortOrder::Order`] keeps
    /// the list (or reverses it); any other order sorts by `cmp`, whose
    /// result is negated when reversed.
    pub fn apply<T>(self, items: &mut [T], cmp: impl Fn(SortOrder, &T, &T) -> std::cmp::Ordering) {
        if self.order == SortOrder::Order {
            if self.reversed {
                items.reverse();
            }
            return;
        }
        items.sort_by(|a, b| {
            let r = cmp(self.order, a, b);
            if self.reversed { r.reverse() } else { r }
        });
    }
}

/// One thing a loop visits: its variables, whether it takes the second
/// (`active`) branch, and variables that exist only inside this loop.
pub struct LoopItem<'a> {
    pub vars: Box<dyn FormatVars + 'a>,
    pub active: bool,
    pub extras: Vec<(String, String)>,
}

/// tmux 3.7b's `FORMAT_LOOP_LIMIT`: an expansion nested this deep gives
/// nothing, so `#{!:x}` nested 100 deep reads its innermost `x` as empty
/// (recorded: 99 levels give `0`, 100 and more give `0` whatever their
/// parity).
pub const LOOP_LIMIT: usize = 100;

/// Expands every `#{...}` in `fmt` against `vars`, and `##` to a literal
/// `#`.
///
/// An unknown variable expands to an empty string, and a malformed
/// `#{...}` (no closing brace, or a `?`/`==:` form missing one of its
/// arguments) is passed through unchanged, rather than failing the whole
/// format: tmux's own formats are equally forgiving, and a status line
/// built from several small `#{...}` pieces shouldn't go blank because
/// one of them is wrong. `#{...}` may nest: the closing `}` is found by
/// tracking nested `#{`/`}` pairs, not by the first `}` seen.
///
/// The work is bounded whatever the input: nesting stops at
/// [`LOOP_LIMIT`], and every `#{` is matched to its `}` once, up front, so
/// no level rescans the text of the levels inside it.
pub fn expand(fmt: &str, vars: &dyn FormatVars) -> String {
    let closers = scan_closers(fmt);
    let shared = Shared::default();
    Expander {
        src: fmt,
        vars,
        closers: &closers,
        shared: &shared,
    }
    .expand(fmt)
}

/// The state every level of one [`expand`] call shares.
#[derive(Default)]
struct Shared {
    depth: Cell<usize>,
    /// Inside `#{T:}`: a `%` in a format goes through strftime first.
    expand_time: Cell<bool>,
}

/// Each `#{` in `src` (its offset) and the offset of its `}`, if it has
/// one, in order: the `}` that [`Expander::find_unnested`] would find by
/// counting nested pairs from just after the `#{`.
type Closers = Vec<(usize, Option<usize>)>;

fn scan_closers(src: &str) -> Closers {
    let b = src.as_bytes();
    let mut closers = Vec::new();
    let mut open = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
            open.push(closers.len());
            closers.push((i, None));
            i += 2;
            continue;
        }
        if b[i] == b'}'
            && let Some(at) = open.pop()
        {
            closers[at].1 = Some(i);
        }
        i += 1;
    }
    closers
}

/// A failed `#{...}`: tmux stops expanding the format there, and what came
/// before it is the result (a loop with no session to loop over, a `#{R:}`
/// with no comma).
struct Fail;

/// One level of an [`expand`] call: the format it reads, where each `#{`
/// closes, the variables it reads and how deep the expansion is. Every
/// `&str` its methods take is a slice of `src`, so a slice's offset finds
/// its braces in `closers`; text from anywhere else goes through
/// [`Expander::expand_owned`].
struct Expander<'a> {
    src: &'a str,
    vars: &'a dyn FormatVars,
    closers: &'a [(usize, Option<usize>)],
    shared: &'a Shared,
}

impl<'a> Expander<'a> {
    /// Where `s`, a slice of `src`, starts in it.
    fn offset(&self, s: &str) -> usize {
        let at = (s.as_ptr() as usize).wrapping_sub(self.src.as_ptr() as usize);
        debug_assert!(at + s.len() <= self.src.len(), "not a slice of the format");
        at
    }

    /// The `}` closing the `#{` at `s[at..]`, as an index into `s`.
    fn closer(&self, s: &str, at: usize) -> Option<usize> {
        let base = self.offset(s);
        let i = self
            .closers
            .binary_search_by_key(&(base + at), |&(open, _)| open)
            .ok()?;
        let close = self.closers[i].1?.checked_sub(base)?;
        (close < s.len()).then_some(close)
    }

    /// `fmt`, which is not a slice of `src` (a variable's value, a result),
    /// expanded as a format of its own.
    fn expand_owned(&self, fmt: &str) -> String {
        let closers = scan_closers(fmt);
        Expander {
            src: fmt,
            vars: self.vars,
            closers: &closers,
            shared: self.shared,
        }
        .expand(fmt)
    }

    /// `fmt`, a slice of `src`, expanded against other variables: what a
    /// loop does for each of its items.
    fn expand_with(&self, vars: &dyn FormatVars, fmt: &str) -> String {
        Expander {
            src: self.src,
            vars,
            closers: self.closers,
            shared: self.shared,
        }
        .expand(fmt)
    }

    fn expand(&self, fmt: &str) -> String {
        let depth = self.shared.depth.get();
        if depth == LOOP_LIMIT {
            return String::new();
        }
        self.shared.depth.set(depth + 1);
        let out = self.expand_text(fmt);
        self.shared.depth.set(depth);
        out
    }

    fn expand_text(&self, fmt: &str) -> String {
        if self.shared.expand_time.get() && fmt.contains('%') {
            // tmux 3.7b: a format too long for strftime's 8 KiB buffer gives
            // nothing.
            let Some(timed) = modify::strftime_now(fmt, 8192) else {
                return String::new();
            };
            let closers = scan_closers(&timed);
            return Expander {
                src: &timed,
                vars: self.vars,
                closers: &closers,
                shared: self.shared,
            }
            .expand_pieces(&timed);
        }
        self.expand_pieces(fmt)
    }

    fn expand_pieces(&self, fmt: &str) -> String {
        let mut out = String::with_capacity(fmt.len());
        let mut i = 0;
        while i < fmt.len() {
            if fmt[i..].starts_with("##") {
                out.push('#');
                i += 2;
                continue;
            }
            if fmt[i..].starts_with("#{")
                && let Some(end) = self.closer(fmt, i)
            {
                match self.expand_body(&fmt[i + 2..end]) {
                    Ok(piece) => out.push_str(&piece),
                    // tmux stops there and keeps what it has.
                    Err(Fail) => return out,
                }
                i = end + 1;
                continue;
            }
            let ch = fmt[i..]
                .chars()
                .next()
                .expect("i is a char boundary within fmt");
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    /// Finds the first `target` byte in `s` that isn't inside a nested
    /// `#{...}`, so callers can split a body on its own top-level `}` or
    /// `,` without being confused by commas or braces belonging to a
    /// nested format. A nested `#{...}` is stepped over whole; one that
    /// never closes hides everything after it.
    fn find_unnested(&self, s: &str, target: u8) -> Option<usize> {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == target {
                return Some(i);
            }
            if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
                i = self.closer(s, i)? + 1;
                continue;
            }
            i += 1;
        }
        None
    }

    /// Expands the body of one `#{...}`: `?cond,then,else`, a comparison
    /// from [`COMPARISONS`], `&&:`/`||:` over any number of operands, a
    /// match (`m:`, `m/r:`, `m/i:`, `m/ri:`), `!:text`, `!!:text`,
    /// `b:var`, `t:var`, `q:var`, or a bare variable name.
    fn expand_simple(&self, body: &str) -> Option<String> {
        if let Some(rest) = body.strip_prefix('?') {
            return Some(self.expand_ternary(rest, body));
        }
        for (prefix, op) in COMPARISONS {
            if let Some(rest) = body.strip_prefix(prefix) {
                return Some(self.expand_comparison(rest, body, prefix == "==:", op));
            }
        }
        // `&&:`/`||:` fold over every operand, as in tmux 3.7b; one operand
        // is its own truth, and no operand at all is false.
        if let Some(rest) = body.strip_prefix("&&:") {
            return Some(flag(
                self.operands(rest).all(|o| is_truthy(&self.expand(o))),
            ));
        }
        if let Some(rest) = body.strip_prefix("||:") {
            return Some(flag(
                self.operands(rest).any(|o| is_truthy(&self.expand(o))),
            ));
        }
        if let Some((flags, rest)) = match_modifier(body) {
            let Some(c) = self.find_unnested(rest, b',') else {
                return Some(String::new());
            };
            let pattern = self.expand(&rest[..c]);
            let s = self.expand(&rest[c + 1..]);
            let icase = flags.contains('i');
            return Some(flag(if flags.contains('r') {
                regex_match(&pattern, &s, icase)
            } else if icase {
                fnmatch(
                    pattern.to_ascii_lowercase().as_bytes(),
                    s.to_ascii_lowercase().as_bytes(),
                )
            } else {
                fnmatch(pattern.as_bytes(), s.as_bytes())
            }));
        }
        // `#{!:text}` is `1` when the expanded text is false, `#{!!:text}`
        // when it is true (tmux 3.7b).
        if let Some(rest) = body.strip_prefix("!!:") {
            return Some(flag(is_truthy(&self.expand(rest))));
        }
        if let Some(rest) = body.strip_prefix("!:") {
            return Some(flag(!is_truthy(&self.expand(rest))));
        }
        if let Some(rest) = body.strip_prefix("b:") {
            return Some(self.expand_basename(rest));
        }
        if let Some(rest) = body.strip_prefix("t:") {
            return Some(self.expand_time(rest));
        }
        if let Some(rest) = body.strip_prefix("q:") {
            return Some(self.expand_quoted(rest));
        }
        None
    }

    /// One `#{...}`: the plain forms above, else tmux's modifier list
    /// (`format_build_modifiers`) and what it asks for, else a variable.
    fn expand_body(&self, body: &str) -> Result<String, Fail> {
        if let Some(done) = self.expand_simple(body) {
            return Ok(done);
        }
        if let Some((list, rest)) = self.build_modifiers(body) {
            return self.replace(&list, &body[rest..]);
        }
        Ok(self.resolve(body))
    }

    /// `#{q:var}`: the variable's value with a backslash before each
    /// character a shell treats specially (tmux 3.7b's
    /// `format_quote_shell`, recorded). As with `b:`, a nested argument
    /// is expanded and returned unquoted.
    fn expand_quoted(&self, arg: &str) -> String {
        if arg.contains("#{") {
            return self.expand(arg);
        }
        let value = self.vars.get(arg).unwrap_or_default();
        let mut out = String::with_capacity(value.len());
        for c in value.chars() {
            if "|&;<>()$`\\\"'*?[# =%".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    /// A body's operands, split at every comma outside a nested `#{...}`.
    fn operands<'s>(&self, mut rest: &'s str) -> impl Iterator<Item = &'s str> {
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            match self.find_unnested(rest, b',') {
                Some(c) => {
                    let (head, tail) = (&rest[..c], &rest[c + 1..]);
                    rest = tail;
                    Some(head)
                }
                None => {
                    done = true;
                    Some(rest)
                }
            }
        })
    }

    /// `#{t:var}`: the variable's value, seconds since the epoch, as
    /// `ctime(3)` prints it in local time (`Wed Nov 15 00:13:20 2023`);
    /// empty when it is not a positive number. As with `b:`, a nested
    /// argument is expanded and returned unchanged (verified against tmux
    /// 3.7b).
    fn expand_time(&self, arg: &str) -> String {
        if arg.contains("#{") {
            return self.expand(arg);
        }
        self.vars
            .get(arg)
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|t| *t > 0)
            .and_then(ctime)
            .unwrap_or_default()
    }

    /// `#{b:var}`: tmux only applies the basename transform to a bare
    /// variable-name lookup. When the argument itself contains a nested
    /// `#{...}`, tmux expands it and returns the result unchanged -- so
    /// `#{b:#{@p}}` with `@p` set to `/a/b/c` gives `/a/b/c`, not `c`, and
    /// only `#{b:pane_current_path}` (a bare name) is basenamed.
    fn expand_basename(&self, arg: &str) -> String {
        if arg.contains("#{") {
            self.expand(arg)
        } else {
            basename(&self.vars.get(arg).unwrap_or_default())
        }
    }

    /// Resolves a ternary condition: a bare token (no nested `#{...}`) is
    /// looked up directly as a variable name, exactly as if it had been
    /// written `#{token}`; a token that itself contains `#{...}` is
    /// expanded first and its result used as-is. `expand_basename` uses
    /// the same bare-vs-nested split for `#{b:var}`'s argument, but only
    /// applies its own basename transform on the bare branch.
    fn resolve(&self, s: &str) -> String {
        if s.contains("#{") {
            self.expand(s)
        } else {
            self.vars.get(s).unwrap_or_default()
        }
    }

    /// A branch that is one whole `#{?...}` is taken in this loop rather
    /// than one level deeper, so a chain of conditions (the tmux front
    /// end's `#{?c1,v1,c2,v2,...}`) costs no depth and no stack, as
    /// tmux's flat form costs none.
    fn expand_ternary<'s>(&self, mut rest: &'s str, mut original_body: &'s str) -> String {
        loop {
            let malformed = || format!("#{{{original_body}}}");
            let Some(c1) = self.find_unnested(rest, b',') else {
                return malformed();
            };
            let cond = &rest[..c1];
            let after = &rest[c1 + 1..];
            let Some(c2) = self.find_unnested(after, b',') else {
                return malformed();
            };
            let branch = if is_truthy(&self.resolve(cond)) {
                &after[..c2]
            } else {
                &after[c2 + 1..]
            };
            if branch.starts_with("#{?") && self.closer(branch, 0) == Some(branch.len() - 1) {
                original_body = &branch[2..branch.len() - 1];
                rest = &branch[3..branch.len() - 1];
                continue;
            }
            return self.expand(branch);
        }
    }

    /// `#{==:a,b}` and the other [`COMPARISONS`]: both sides are literal
    /// text that may itself contain nested `#{...}` (mixed-text, like a
    /// `then`/`else` branch), not a bare variable lookup -- so
    /// `#{==:#{window_name},bash}` compares the window's name against the
    /// literal word `bash`. With no comma, `==:` (M2) passes through and
    /// the rest give nothing.
    // ponytail: tmux also drops the rest of the format after a comparison
    // with no comma; grofe drops only that piece.
    fn expand_comparison(
        &self,
        rest: &str,
        original_body: &str,
        pass_through: bool,
        op: Comparison,
    ) -> String {
        let Some(c) = self.find_unnested(rest, b',') else {
            return if pass_through {
                format!("#{{{original_body}}}")
            } else {
                String::new()
            };
        };
        let a = self.expand(&rest[..c]);
        let b = self.expand(&rest[c + 1..]);
        flag(op(&a, &b))
    }
}

/// The two-argument modifiers: `#{op:a,b}` expands both sides as text and
/// gives `1` or `0`. Longer prefixes come first (`>=:` before `>:`). `==:`
/// is M2's and keeps passing a missing comma through; the others give
/// nothing then, as tmux 3.7b does.
type Comparison = fn(&str, &str) -> bool;

const COMPARISONS: [(&str, Comparison); 6] = [
    ("==:", |a, b| a == b),
    ("!=:", |a, b| a != b),
    // tmux compares these as strings (`strcmp`), so `10` sorts before `9`,
    // and `<` and `<=` likewise.
    (">=:", |a, b| a >= b),
    (">:", |a, b| a > b),
    ("<=:", |a, b| a <= b),
    ("<:", |a, b| a < b),
];

fn flag(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// `m:`, or `m/<flags>:` with flags from `r` (regex) and `i` (ignore
/// case), in either order: the flags and what follows the `:`.
fn match_modifier(body: &str) -> Option<(&str, &str)> {
    let rest = body.strip_prefix('m')?;
    if let Some(rest) = rest.strip_prefix(':') {
        return Some(("", rest));
    }
    let (flags, rest) = rest.strip_prefix('/')?.split_once(':')?;
    flags
        .chars()
        .all(|c| c == 'r' || c == 'i')
        .then_some((flags, rest))
}

/// `ctime(3)`'s text for `secs`, without the newline.
fn ctime(secs: i64) -> Option<String> {
    use nix::libc;
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // `time_t` is 64 bits wide on every target grofe builds for.
    let t = secs as libc::time_t;
    // SAFETY: `tm` is plain old data, so all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals; localtime_r is reentrant.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(format!(
        "{} {}{:3} {:02}:{:02}:{:02} {}",
        DAYS.get(usize::try_from(tm.tm_wday).ok()?)?,
        MONTHS.get(usize::try_from(tm.tm_mon).ok()?)?,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        i64::from(tm.tm_year) + 1900,
    ))
}

/// How many compiled `#{m/r:}` patterns are kept, most recent first.
const REGEX_CACHE: usize = 32;

/// The compiled-size cap for a `#{m/r:}` pattern, so a hostile pattern
/// fails to compile instead of costing milliseconds on every expansion.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// `#{m/r:pattern,string}`: an extended regular expression search, as
/// tmux's `regexec`; a pattern that does not compile matches nothing.
/// The `regex` crate's syntax is not POSIX ERE: Perl classes such as `\d`
/// work here and not in tmux. That divergence is permanent (accepted in
/// the TC.3 review); the corpus's patterns mean the same in both.
fn regex_match(pattern: &str, s: &str, icase: bool) -> bool {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    type Cache = VecDeque<(String, bool, Option<regex::Regex>)>;
    thread_local! {
        static CACHE: RefCell<Cache> = const { RefCell::new(VecDeque::new()) };
    }
    CACHE.with_borrow_mut(|cache| {
        let hit = cache
            .iter()
            .position(|(p, i, _)| p == pattern && *i == icase);
        let entry = match hit.and_then(|at| cache.remove(at)) {
            Some(entry) => entry,
            None => {
                let re = regex::RegexBuilder::new(pattern)
                    .case_insensitive(icase)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .dfa_size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .ok();
                (pattern.to_string(), icase, re)
            }
        };
        let matched = entry.2.as_ref().is_some_and(|re| re.is_match(s));
        cache.push_front(entry);
        cache.truncate(REGEX_CACHE);
        matched
    })
}

/// `fnmatch(3)` with no flags, as tmux's `#{m:}`: `*` and `?` also match
/// `/` and a leading `.`, `[...]` takes ranges and `!` or `^` to negate,
/// and a backslash quotes the next character. An unclosed `[` is an
/// ordinary character, as in glibc. Iterative, backtracking only to the
/// last `*`, so it takes O(pattern x string) steps whatever the input.
fn fnmatch(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    // Where to resume after the last `*`: the pattern after it, and the
    // string position it has absorbed up to.
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pi < p.len() && p[pi] == b'*' {
            pi += 1;
            star = Some((pi, si));
            continue;
        }
        if pi < p.len()
            && si < s.len()
            && let Some(next) = match_one(p, pi, s[si])
        {
            pi = next;
            si += 1;
            continue;
        }
        if pi == p.len() && si == s.len() {
            return true;
        }
        match star {
            Some((after, absorbed)) if absorbed < s.len() => {
                star = Some((after, absorbed + 1));
                pi = after;
                si = absorbed + 1;
            }
            _ => return false,
        }
    }
}

/// Whether the one-character element at `p[pi]` (`?`, a set, an escape or
/// a literal) matches `c`; the index after the element when it does.
fn match_one(p: &[u8], pi: usize, c: u8) -> Option<usize> {
    match p[pi] {
        b'?' => Some(pi + 1),
        b'[' => match bracket(&p[pi + 1..]) {
            Some((set, after)) => set.matches(c).then_some(p.len() - after.len()),
            None => (c == b'[').then_some(pi + 1),
        },
        b'\\' if pi + 1 < p.len() => (p[pi + 1] == c).then_some(pi + 2),
        lit => (lit == c).then_some(pi + 1),
    }
}

/// A `[...]` set, parsed from just after its `[`.
struct Bracket<'a> {
    negated: bool,
    body: &'a [u8],
}

impl Bracket<'_> {
    fn matches(&self, c: u8) -> bool {
        let b = self.body;
        let mut i = 0;
        let mut hit = false;
        while i < b.len() {
            let lo = b[i];
            if i + 2 < b.len() && b[i + 1] == b'-' {
                hit |= (lo..=b[i + 2]).contains(&c);
                i += 3;
            } else {
                hit |= lo == c;
                i += 1;
            }
        }
        hit != self.negated
    }
}

/// Splits a `[...]` set (after its `[`) from the pattern that follows its
/// `]`; a `]` right after the `[` (or the negation) is a member. `None`
/// when the set never closes.
fn bracket(p: &[u8]) -> Option<(Bracket<'_>, &[u8])> {
    let negated = matches!(p.first(), Some(b'!' | b'^'));
    let start = usize::from(negated);
    let close = p
        .iter()
        .skip(start + 1)
        .position(|&c| c == b']')
        .map(|i| i + start + 1)?;
    Some((
        Bracket {
            negated,
            body: &p[start..close],
        },
        &p[close + 1..],
    ))
}

/// tmux's basename modifier: the last `/`-separated component, with any
/// trailing slashes stripped first. Basename of an empty string is empty,
/// not `.` as POSIX `basename(1)` would give.
fn basename(s: &str) -> String {
    s.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// POSIX `basename(3)`, which `#{b:}` calls: trailing slashes go first,
/// and nothing left is `.` (or `/`, if there were slashes).
fn posix_basename(s: &str) -> String {
    if s.is_empty() {
        return ".".to_string();
    }
    match s.trim_end_matches('/') {
        "" => "/".to_string(),
        t => t.rsplit('/').next().unwrap_or(t).to_string(),
    }
}

/// tmux's truthiness for a format value: present, non-empty and not `"0"`.
fn is_truthy(value: &str) -> bool {
    !value.is_empty() && value != "0"
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct MapVars(BTreeMap<&'static str, &'static str>);

    impl FormatVars for MapVars {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|v| v.to_string())
        }
    }

    fn vars(pairs: &[(&'static str, &'static str)]) -> MapVars {
        MapVars(pairs.iter().copied().collect())
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(expand("hello world", &vars(&[])), "hello world");
    }

    #[test]
    fn bare_variable_expands() {
        assert_eq!(
            expand("#{session_name}", &vars(&[("session_name", "work")])),
            "work"
        );
    }

    #[test]
    fn unknown_variable_expands_to_empty() {
        assert_eq!(expand("[#{nope}]", &vars(&[])), "[]");
    }

    #[test]
    fn user_option_variable() {
        assert_eq!(
            expand("#{@agent_rollup}", &vars(&[("@agent_rollup", "3")])),
            "3"
        );
    }

    #[test]
    fn variable_embedded_in_text() {
        assert_eq!(
            expand("[#{a}-#{b}]", &vars(&[("a", "1"), ("b", "2")])),
            "[1-2]"
        );
    }

    #[test]
    fn ternary_true_branch() {
        assert_eq!(
            expand("#{?window_active,yes,no}", &vars(&[("window_active", "1")])),
            "yes"
        );
    }

    #[test]
    fn ternary_false_branch_when_zero() {
        assert_eq!(
            expand("#{?window_active,yes,no}", &vars(&[("window_active", "0")])),
            "no"
        );
    }

    #[test]
    fn ternary_false_branch_when_missing() {
        assert_eq!(expand("#{?window_active,yes,no}", &vars(&[])), "no");
    }

    #[test]
    fn ternary_false_branch_when_empty() {
        assert_eq!(
            expand("#{?window_active,yes,no}", &vars(&[("window_active", "")])),
            "no"
        );
    }

    #[test]
    fn ternary_branches_can_be_empty() {
        assert_eq!(expand("#{?x,,no}", &vars(&[("x", "1")])), "");
        assert_eq!(expand("#{?x,yes,}", &vars(&[])), "");
    }

    #[test]
    fn unterminated_brace_passes_through() {
        assert_eq!(expand("a #{b", &vars(&[])), "a #{b");
    }

    #[test]
    fn malformed_ternary_missing_comma_passes_through() {
        assert_eq!(expand("#{?onlyone}", &vars(&[])), "#{?onlyone}");
    }

    #[test]
    fn malformed_ternary_missing_second_comma_passes_through() {
        assert_eq!(expand("#{?cond,then}", &vars(&[])), "#{?cond,then}");
    }

    #[test]
    fn literal_hash_without_brace_passes_through() {
        assert_eq!(expand("50% #done", &vars(&[])), "50% #done");
    }

    #[test]
    fn multibyte_text_around_a_variable() {
        assert_eq!(
            expand("café #{x} → done", &vars(&[("x", "1")])),
            "café 1 → done"
        );
    }

    // --- M2: nesting, escaping, ==:, b: ---

    #[test]
    fn nested_ternary_condition_expands() {
        // A nested `#{...}` as the ternary's condition (spec §4.5, M2
        // nesting). The outer scanner must find the true closing `}`
        // past the nested one, and the condition itself is expanded
        // (rather than looked up literally as `#{@agent_rollup}`).
        let input = "#{?#{@agent_rollup},present,absent}";
        assert_eq!(expand(input, &vars(&[("@agent_rollup", "1")])), "present");
        assert_eq!(expand(input, &vars(&[])), "absent");
    }

    #[test]
    fn nested_variable_in_then_branch() {
        assert_eq!(
            expand("#{?x,[#{y}],no}", &vars(&[("x", "1"), ("y", "z")])),
            "[z]"
        );
    }

    #[test]
    fn escaped_hash_hash_is_literal_hash() {
        assert_eq!(expand("##", &vars(&[])), "#");
    }

    #[test]
    fn escaping_prevents_format_expansion() {
        assert_eq!(
            expand("##{not a format}", &vars(&[("not a format", "x")])),
            "#{not a format}"
        );
    }

    #[test]
    fn equality_true_and_false() {
        assert_eq!(expand("#{==:a,a}", &vars(&[])), "1");
        assert_eq!(expand("#{==:a,b}", &vars(&[])), "0");
    }

    #[test]
    fn equality_compares_expanded_nested_values_against_a_literal() {
        // `a`/`b` here are mixed text, not bare variable lookups: a plain
        // word is literal, and a nested `#{...}` is expanded first.
        assert_eq!(
            expand(
                "#{==:#{window_name},bash}",
                &vars(&[("window_name", "bash")])
            ),
            "1"
        );
        assert_eq!(
            expand(
                "#{==:#{window_name},bash}",
                &vars(&[("window_name", "zsh")])
            ),
            "0"
        );
    }

    #[test]
    fn equality_missing_comma_passes_through() {
        assert_eq!(expand("#{==:onlyone}", &vars(&[])), "#{==:onlyone}");
    }

    #[test]
    fn basename_modifier_looks_up_the_named_variable() {
        assert_eq!(
            expand(
                "#{b:pane_current_path}",
                &vars(&[("pane_current_path", "/a/b/c")])
            ),
            "c"
        );
    }

    /// tmux 3.7b, recorded: `q:` backslashes ` |&;<>()$`\"'*?[#=%` and
    /// leaves the rest (newlines too); a nested argument is not quoted.
    #[test]
    fn quote_modifier_escapes_what_a_shell_reads() {
        let x = r#"a b'c"d$e`f\g|h&i;j<k>l(m)n*o?p[q]r#s=t%u~v!w{x}y,z:A^B@C+D-E/F.G_H"#;
        let want = r#"a\ b\'c\"d\$e\`f\\g\|h\&i\;j\<k\>l\(m\)n\*o\?p\[q]r\#s\=t\%u~v!w{x}y,z:A^B@C+D-E/F.G_H"#;
        let v = vars(&[("@x", x), ("@nl", "a\nb\tc"), ("window_id", "@0")]);
        assert_eq!(expand("#{q:@x}", &v), want);
        assert_eq!(expand("#{q:#{@x}}", &v), x);
        assert_eq!(expand("pre#{q:window_id}post", &v), "pre@0post");
        assert_eq!(expand("#{q:@nl}", &v), "a\nb\tc");
        assert_eq!(expand("[#{q:@none}]", &v), "[]");
    }

    #[test]
    fn basename_modifier_of_a_literal_path_is_empty() {
        // Matches tmux 3.7b: `/a/b/c` here is a variable *name* to look
        // up, not literal text, and no such variable exists.
        assert_eq!(expand("#{b:/a/b/c}", &vars(&[])), "");
    }

    #[test]
    fn basename_modifier_does_not_apply_to_a_nested_argument() {
        // Verified against tmux 3.7b: `b:` only basenames a bare
        // variable-name lookup. A nested `#{...}` argument is expanded
        // and returned as-is, even though it looks like a path.
        assert_eq!(expand("#{b:#{@p}}", &vars(&[("@p", "/a/b/c")])), "/a/b/c");
        assert_eq!(expand("#{b:x#{@p}}", &vars(&[("@p", "/a/b/c")])), "x/a/b/c");
    }

    /// TC.3's modifiers, one true and one false case each, with the
    /// results tmux 3.7b gives for the same text.
    #[test]
    fn comparison_modifiers() {
        let v = vars(&[("one", "1"), ("name", "alpha")]);
        for (fmt, want) in [
            ("#{!=:a,b}", "1"),
            ("#{!=:a,a}", "0"),
            ("#{>:b,a}", "1"),
            ("#{>:10,9}", "0"),
            ("#{>=:a,a}", "1"),
            ("#{>=:a,b}", "0"),
            ("#{&&:1,x}", "1"),
            ("#{&&:#{one},0}", "0"),
            ("#{||:0,x}", "1"),
            ("#{||:0,}", "0"),
            ("#{<:a,b}", "1"),
            ("#{<:9,10}", "0"),
            ("#{<=:a,a}", "1"),
            ("#{<=:b,a}", "0"),
            ("#{!:0}", "1"),
            ("#{!:}", "1"),
            ("#{!:x}", "0"),
            ("#{!:#{one}}", "0"),
            ("#{!:#{nope}}", "1"),
            ("#{m:*ab*,xaby}", "1"),
            ("#{m:A*,abc}", "0"),
            ("#{m/r:^a.c$,abc}", "1"),
            ("#{m/r:(copy|view)-mode,}", "0"),
            ("#{m/r:(,abc}", "0"),
            ("#{m:al*,#{name}}", "1"),
            // Bare names are literal text in a comparison, as in `==:`.
            ("#{&&:name,1}", "1"),
            ("#{==:name,alpha}", "0"),
        ] {
            assert_eq!(expand(fmt, &v), want, "{fmt}");
        }
    }

    /// tmux 3.7b gives nothing for these with no comma; `==:` (M2) keeps
    /// passing through (`equality_missing_comma_passes_through`).
    #[test]
    fn comparison_missing_comma_gives_nothing() {
        for fmt in [
            "#{!=:a}",
            "#{>:a}",
            "#{<=:a}",
            "#{m:a}",
            "#{m/r:a}",
            "#{m/ri:a}",
        ] {
            assert_eq!(expand(&format!("[{fmt}]"), &vars(&[])), "[]", "{fmt}");
        }
    }

    /// `&&:`/`||:` take any number of operands (tmux 3.7b).
    #[test]
    fn and_or_fold_over_every_operand() {
        let v = vars(&[
            ("alternate_on", "0"),
            ("pane_in_mode", "0"),
            ("mouse_any_flag", "1"),
        ]);
        for (fmt, want) in [
            ("#{||:0,0,0}", "0"),
            ("#{||:0,0,1}", "1"),
            ("#{&&:1,0,1}", "0"),
            ("#{&&:1,1,1}", "1"),
            ("#{&&:1}", "1"),
            ("#{&&:0}", "0"),
            ("#{||:1}", "1"),
            ("#{||:0}", "0"),
            ("#{&&:}", "0"),
            ("#{||:}", "0"),
            (
                "#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}",
                "1",
            ),
            ("#{||:#{alternate_on},#{pane_in_mode}}", "0"),
            ("#{&&:1,#{?mouse_any_flag,x,0},1}", "1"),
        ] {
            assert_eq!(expand(fmt, &v), want, "{fmt}");
        }
    }

    /// `m/i`, `m/ri` (either order) and `!!`, as tmux 3.7b gives them.
    #[test]
    fn match_flags_and_double_negation() {
        let v = vars(&[("one", "1")]);
        for (fmt, want) in [
            ("#{m/i:A*,abc}", "1"),
            ("#{m/i:a*,ABC}", "1"),
            ("#{m:A*,abc}", "0"),
            ("#{m/ri:^A.C$,abc}", "1"),
            ("#{m/ir:^A.C$,abc}", "1"),
            ("#{m/r:^A.C$,abc}", "0"),
            ("#{!!:x}", "1"),
            ("#{!!:0}", "0"),
            ("#{!!:}", "0"),
            ("#{!!:#{one}}", "1"),
            ("#{!:#{one}}", "0"),
        ] {
            assert_eq!(expand(fmt, &v), want, "{fmt}");
        }
    }

    /// A pattern that makes naive backtracking exponential finishes at
    /// once (a pane can put such text in its title).
    #[test]
    fn fnmatch_does_not_backtrack_exponentially() {
        let pattern = "*a*a*a*a*a*a*a*a*a*a*b";
        let s = "a".repeat(30);
        let start = crate::thread_cpu_time();
        assert!(!fnmatch(pattern.as_bytes(), s.as_bytes()));
        assert!(fnmatch(pattern.as_bytes(), format!("{s}b").as_bytes()));
        let took = crate::thread_cpu_time() - start;
        assert!(took < std::time::Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn regex_that_is_too_big_matches_nothing() {
        // Over the size limit: `a{1000}{1000}` compiles to far more than 1 MiB.
        let big = vars(&[("pat", "a{1000}{1000}")]);
        assert_eq!(expand("#{m/r:#{pat},a}", &big), "0");
        // Cached patterns keep matching.
        for _ in 0..3 {
            assert_eq!(expand("#{m/r:^a+$,aaa}", &vars(&[])), "1");
        }
    }

    #[test]
    fn fnmatch_matches_as_fnmatch_with_no_flags() {
        for (pattern, s, want) in [
            ("", "", true),
            ("", "a", false),
            ("*", "", true),
            ("*", "a/b", true),
            ("*.rs", ".x.rs", true),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("[a-c]x", "bx", true),
            ("[a-c]x", "dx", false),
            ("[!x]b", "ab", true),
            ("[!x]b", "xb", false),
            ("[^a]x", "bx", true),
            ("[]]", "]", true),
            ("[ab", "[ab", true),
            ("s\\*", "s*", true),
            ("s\\*", "sx", false),
            ("*net*", "wifi-net-1", true),
            ("a*b*c", "axxbyyc", true),
            ("a*b*c", "axxbyy", false),
            ("*[0-9]", "win3", true),
            ("\\*a", "*a", true),
        ] {
            assert_eq!(
                fnmatch(pattern.as_bytes(), s.as_bytes()),
                want,
                "{pattern} ~ {s}"
            );
        }
    }

    #[test]
    fn time_modifier_needs_a_positive_number() {
        let v = vars(&[("stamp", "1700000000"), ("word", "soon"), ("zero", "0")]);
        let t = expand("#{t:stamp}", &v);
        assert_eq!(t, ctime(1_700_000_000).expect("ctime"));
        // `Wed Nov 15 00:13:20 2023` in UTC; the day and time move with
        // the zone, the shape does not.
        assert_eq!(t.len(), 24, "{t}");
        assert_eq!(&t[19..], " 2023", "{t}");
        assert_eq!(expand("[#{t:word}]", &v), "[]");
        assert_eq!(expand("[#{t:zero}]", &v), "[]");
        assert_eq!(expand("[#{t:nope}]", &v), "[]");
        assert_eq!(expand("#{t:#{stamp}}", &v), "1700000000");
    }

    /// tmux 3.7b's loop limit (recorded with `display -p`): `#{!:x}`
    /// nested 97, 98 and 99 deep alternates `0`, `1`, `0`; from 100 deep
    /// the innermost text expands to nothing, so the result stops
    /// alternating.
    #[test]
    fn nesting_stops_at_tmux_s_loop_limit() {
        let nest = |n: usize| format!("{}x{}", "#{!:".repeat(n), "}".repeat(n));
        for (n, want) in [
            (97, "0"),
            (98, "1"),
            (99, "0"),
            (100, "0"),
            (101, "0"),
            (102, "0"),
        ] {
            assert_eq!(expand(&nest(n), &vars(&[])), want, "{n} deep");
        }
        // A bare variable 100 deep is still found, as in tmux: only the
        // expansion of a 100-deep argument is cut.
        let var = format!("{}#{{x}}{}", "#{!!:".repeat(99), "}".repeat(99));
        assert_eq!(expand(&var, &vars(&[("x", "v")])), "1");
        let cut = format!("{}#{{x}}{}", "#{!!:".repeat(100), "}".repeat(100));
        assert_eq!(expand(&cut, &vars(&[("x", "v")])), "0");
    }

    /// Hostile input finishes at once: 100,000 nested levels, closed and
    /// unclosed, and as many unclosed `#{` side by side.
    #[test]
    fn deep_or_wide_input_is_linear() {
        let n = 100_000;
        let open = "#{!:".repeat(n);
        let closed = format!("{open}x{}", "}".repeat(n));
        let flat_open = "#{".repeat(n);
        let start = crate::thread_cpu_time();
        assert_eq!(expand(&closed, &vars(&[])), "0");
        assert_eq!(expand(&open, &vars(&[])), open);
        assert_eq!(expand(&flat_open, &vars(&[])), flat_open);
        let took = crate::thread_cpu_time() - start;
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
    }

    /// A chain of conditions in the else branch (the tmux front end's
    /// rewrite of `#{?c1,v1,c2,v2,...}`) is not cut by the loop limit and
    /// does not rescan the rest of the chain at every arm.
    #[test]
    fn a_long_chain_of_conditions_is_not_nesting() {
        let arms = 20_000;
        let chain = |last: &str| format!("{}{last}{}", "#{?0,x,".repeat(arms), "}".repeat(arms));
        let start = crate::thread_cpu_time();
        assert_eq!(expand(&chain("last"), &vars(&[])), "last");
        assert_eq!(
            expand(&chain("#{?on,hit,miss}"), &vars(&[("on", "1")])),
            "hit"
        );
        let took = crate::thread_cpu_time() - start;
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
    }

    #[test]
    fn a_job_is_literal_text() {
        assert_eq!(expand("#(date)", &vars(&[])), "#(date)");
    }
}
