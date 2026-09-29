//! `#{...}` format expansion (spec §4.5). M1 supports a bare variable
//! reference, `#{name}`, and a ternary, `#{?cond,then,else}`. M2 adds
//! nesting, `##` escaping, `#{==:a,b}` and the `#{b:var}` modifier.
//!
//! Format-bearing flag values (`-c`, `-F`, `display-message` text) stay
//! raw strings on the parsed `Command`; dispatch calls [`expand`] against
//! the target's context at execution time, not at parse time.

/// Supplies named values to [`expand`]. Implemented by whatever holds the
/// pane/window/session/client state a format string reaches for
/// (`server/model.rs`, from T1.5c onward).
pub trait FormatVars {
    fn get(&self, name: &str) -> Option<String>;
}

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
pub fn expand(fmt: &str, vars: &dyn FormatVars) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i..].starts_with("##") {
            out.push('#');
            i += 2;
            continue;
        }
        if fmt[i..].starts_with("#{")
            && let Some(end) = find_unnested(&fmt[i + 2..], b'}')
        {
            let body = &fmt[i + 2..i + 2 + end];
            out.push_str(&expand_body(body, vars));
            i += 2 + end + 1;
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
/// `#{...}`, so callers can split a body on its own top-level `}` or `,`
/// without being confused by commas or braces belonging to a nested
/// format. `s` is assumed to be otherwise balanced (it is either the
/// tail after an opening `#{`, or an already-extracted body).
fn find_unnested(s: &str, target: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        if depth == 0 && bytes[i] == target {
            return Some(i);
        }
        if s[i..].starts_with("#{") {
            depth += 1;
            i += 2;
            continue;
        }
        if bytes[i] == b'}' && depth > 0 {
            depth -= 1;
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().expect("i is a char boundary");
        i += ch.len_utf8();
    }
    None
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

/// Expands the body of one `#{...}`: `?cond,then,else`, a comparison from
/// [`COMPARISONS`], `&&:`/`||:` over any number of operands, a match
/// (`m:`, `m/r:`, `m/i:`, `m/ri:`), `!:text`, `!!:text`, `b:var`, `t:var`,
/// or a bare variable name.
fn expand_body(body: &str, vars: &dyn FormatVars) -> String {
    if let Some(rest) = body.strip_prefix('?') {
        return expand_ternary(rest, body, vars);
    }
    for (prefix, op) in COMPARISONS {
        if let Some(rest) = body.strip_prefix(prefix) {
            return expand_comparison(rest, body, prefix == "==:", op, vars);
        }
    }
    // `&&:`/`||:` fold over every operand, as in tmux 3.7b; one operand is
    // its own truth, and no operand at all is false.
    if let Some(rest) = body.strip_prefix("&&:") {
        return flag(operands(rest).all(|o| is_truthy(&expand(o, vars))));
    }
    if let Some(rest) = body.strip_prefix("||:") {
        return flag(operands(rest).any(|o| is_truthy(&expand(o, vars))));
    }
    if let Some((flags, rest)) = match_modifier(body) {
        let Some(c) = find_unnested(rest, b',') else {
            return String::new();
        };
        let pattern = expand(&rest[..c], vars);
        let s = expand(&rest[c + 1..], vars);
        let icase = flags.contains('i');
        return flag(if flags.contains('r') {
            regex_match(&pattern, &s, icase)
        } else if icase {
            fnmatch(
                pattern.to_ascii_lowercase().as_bytes(),
                s.to_ascii_lowercase().as_bytes(),
            )
        } else {
            fnmatch(pattern.as_bytes(), s.as_bytes())
        });
    }
    // `#{!:text}` is `1` when the expanded text is false, `#{!!:text}` when
    // it is true (tmux 3.7b).
    if let Some(rest) = body.strip_prefix("!!:") {
        return flag(is_truthy(&expand(rest, vars)));
    }
    if let Some(rest) = body.strip_prefix("!:") {
        return flag(!is_truthy(&expand(rest, vars)));
    }
    if let Some(rest) = body.strip_prefix("b:") {
        return expand_basename(rest, vars);
    }
    if let Some(rest) = body.strip_prefix("t:") {
        return expand_time(rest, vars);
    }
    resolve(body, vars)
}

fn flag(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// A body's operands, split at every comma outside a nested `#{...}`.
fn operands(mut rest: &str) -> impl Iterator<Item = &str> {
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        match find_unnested(rest, b',') {
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

/// `#{t:var}`: the variable's value, seconds since the epoch, as
/// `ctime(3)` prints it in local time (`Wed Nov 15 00:13:20 2023`); empty
/// when it is not a positive number. As with `b:`, a nested argument is
/// expanded and returned unchanged (verified against tmux 3.7b).
fn expand_time(arg: &str, vars: &dyn FormatVars) -> String {
    if arg.contains("#{") {
        return expand(arg, vars);
    }
    vars.get(arg)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|t| *t > 0)
        .and_then(ctime)
        .unwrap_or_default()
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

/// `#{b:var}`: tmux only applies the basename transform to a bare
/// variable-name lookup. When the argument itself contains a nested
/// `#{...}`, tmux expands it and returns the result unchanged -- so
/// `#{b:#{@p}}` with `@p` set to `/a/b/c` gives `/a/b/c`, not `c`, and
/// only `#{b:pane_current_path}` (a bare name) is basenamed.
fn expand_basename(arg: &str, vars: &dyn FormatVars) -> String {
    if arg.contains("#{") {
        expand(arg, vars)
    } else {
        basename(&vars.get(arg).unwrap_or_default())
    }
}

/// Resolves a ternary condition: a bare token (no nested `#{...}`) is
/// looked up directly as a variable name, exactly as if it had been
/// written `#{token}`; a token that itself contains `#{...}` is expanded
/// first and its result used as-is. `expand_basename` uses the same
/// bare-vs-nested split for `#{b:var}`'s argument, but only applies its
/// own basename transform on the bare branch.
fn resolve(s: &str, vars: &dyn FormatVars) -> String {
    if s.contains("#{") {
        expand(s, vars)
    } else {
        vars.get(s).unwrap_or_default()
    }
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

fn expand_ternary(rest: &str, original_body: &str, vars: &dyn FormatVars) -> String {
    let malformed = || format!("#{{{original_body}}}");
    let Some(c1) = find_unnested(rest, b',') else {
        return malformed();
    };
    let cond = &rest[..c1];
    let after = &rest[c1 + 1..];
    let Some(c2) = find_unnested(after, b',') else {
        return malformed();
    };
    let then_branch = &after[..c2];
    let else_branch = &after[c2 + 1..];
    if is_truthy(&resolve(cond, vars)) {
        expand(then_branch, vars)
    } else {
        expand(else_branch, vars)
    }
}

/// `#{==:a,b}` and the other [`COMPARISONS`]: both sides are literal text
/// that may itself contain nested `#{...}` (mixed-text, like a
/// `then`/`else` branch), not a bare variable lookup -- so
/// `#{==:#{window_name},bash}` compares the window's name against the
/// literal word `bash`. With no comma, `==:` (M2) passes through and the
/// rest give nothing.
// ponytail: tmux also drops the rest of the format after a comparison
// with no comma; grofe drops only that piece.
fn expand_comparison(
    rest: &str,
    original_body: &str,
    pass_through: bool,
    op: Comparison,
    vars: &dyn FormatVars,
) -> String {
    let Some(c) = find_unnested(rest, b',') else {
        return if pass_through {
            format!("#{{{original_body}}}")
        } else {
            String::new()
        };
    };
    let a = expand(&rest[..c], vars);
    let b = expand(&rest[c + 1..], vars);
    flag(op(&a, &b))
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
        let start = std::time::Instant::now();
        assert!(!fnmatch(pattern.as_bytes(), s.as_bytes()));
        assert!(fnmatch(pattern.as_bytes(), format!("{s}b").as_bytes()));
        let took = start.elapsed();
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

    #[test]
    fn a_job_is_literal_text() {
        assert_eq!(expand("#(date)", &vars(&[])), "#(date)");
    }
}
