//! tmux 3.7b's format modifiers beyond the plain comparisons: the modifier
//! list (`format_build_modifiers`), what `format_replace` does with it, the
//! loops, and the text helpers they use. Everything here follows
//! `format.c`, `format-draw.c` and `regsub.c` of that release, and the
//! cases are recorded in `golden/fe_3/format-modifiers` and
//! `format-loops`.
//!
//! The text this reads is grofe's native format language, in which the
//! front end has already resolved tmux's `#,` and `#}` escapes, so the
//! bracket-aware splitting ([`skip1`]) only has the `##` and `#{` forms
//! left to step over.

use nix::libc;
use unicode_width::UnicodeWidthChar;

use super::x11::X11;
use super::{
    Expander, Fail, FormatVars, LoopItem, LoopKind, LoopSort, NameKind, SortOrder, ctime, flag,
    fnmatch, is_truthy, posix_basename, regex_match,
};

/// `FORMAT_MAX_WIDTH`, `_REPEAT` and `_PRECISION`.
const MAX_WIDTH: i64 = 10000;
const MAX_REPEAT: i64 = 10000;
const MAX_PRECISION: i64 = 100;

/// The most one `#{R:}` may produce. tmux gives up after 100 ms of
/// expansion and returns nothing; a size is what keeps a nest of repeats
/// bounded here.
const REPEAT_BYTES: usize = 1 << 22;

/// One modifier of a `#{...}` and its arguments, already expanded.
#[derive(Debug)]
pub(super) struct Modifier {
    name: String,
    args: Vec<String>,
}

fn is_end(c: u8) -> bool {
    c == b';' || c == b':'
}

/// tmux's `format_skip1`: the index of the first byte of `end` in `s` that
/// is not inside a nested `#{...}` and not escaped (`#,` `##` `#{` `#}`
/// `#:`).
fn skip1(s: &str, end: &[u8]) -> Option<usize> {
    let b = s.as_bytes();
    let mut brackets = 0i32;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
            brackets += 1;
        }
        if b[i] == b'#' && b.get(i + 1).is_some_and(|c| b",#{}:".contains(c)) {
            i += 2;
            continue;
        }
        if b[i] == b'}' {
            brackets -= 1;
        }
        if end.contains(&b[i]) && brackets == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// OpenBSD's `strtonum`: a whole decimal number within `min..=max`.
fn strtonum(s: &str, min: i64, max: i64) -> Option<i64> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = digits.parse::<i128>().ok()?;
    let n = i64::try_from(if neg { -n } else { n }).ok()?;
    (min..=max).contains(&n).then_some(n)
}

/// C's `strtod` over the whole of `s`: `None` when anything is left over.
/// An empty string is 0, as `strtod` reads no digits and stops at the end.
fn strtod(s: &str) -> Option<f64> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    if t.is_empty() {
        return Some(0.0);
    }
    let (neg, body) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let v = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        hex_float(hex)?
    } else if body.eq_ignore_ascii_case("inf")
        || body.eq_ignore_ascii_case("infinity")
        || body.eq_ignore_ascii_case("nan")
    {
        body.to_ascii_lowercase().parse::<f64>().ok()?
    } else if body
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        body.parse::<f64>().ok()?
    } else {
        return None;
    };
    Some(if neg { -v } else { v })
}

/// The digits after `0x` in a hexadecimal float: `1f`, `1.8`, `1p3`.
fn hex_float(s: &str) -> Option<f64> {
    let (mantissa, exp) = match s.find(['p', 'P']) {
        Some(at) => (&s[..at], s[at + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let digit = |c: char| c.to_digit(16).map(f64::from);
    let mut v = 0.0;
    for c in int.chars() {
        v = v * 16.0 + digit(c)?;
    }
    let mut scale = 1.0 / 16.0;
    for c in frac.chars() {
        v += digit(c)? * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(exp))
}

/// `printf("%.*f", prec, v)`: a negative precision is the default, 6.
fn format_float(v: f64, prec: i64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    let prec = if prec < 0 {
        6
    } else {
        usize::try_from(prec).unwrap_or(6)
    };
    format!("{v:.prec$}")
}

/// POSIX `dirname(3)`.
fn posix_dirname(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        return if s.is_empty() { "." } else { "/" }.to_string();
    }
    match trimmed.rfind('/') {
        None => ".".to_string(),
        Some(at) => match trimmed[..at].trim_end_matches('/') {
            "" => "/".to_string(),
            dir => dir.to_string(),
        },
    }
}

/// `#`s before a `[` or not: tmux's `format_leading_hashes`. The count of
/// `#`s, the width they draw, and the index where the caller goes on
/// (the `#` that starts a style, or past the run).
fn leading_hashes(b: &[u8], at: usize) -> (usize, usize, usize) {
    let mut n = 0;
    while b.get(at + n) == Some(&b'#') {
        n += 1;
    }
    if b.get(at + n) != Some(&b'[') {
        return (n, n.div_ceil(2), at + n);
    }
    if n % 2 == 0 {
        // All the `#`s are escaped, so this is no style.
        return (n, n / 2, at + n);
    }
    (n, n / 2, at + n - 1)
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// tmux's `format_width`: the columns of text, counting `#[...]` styles as
/// nothing and each `##` as one.
fn style_width(s: &str) -> usize {
    let b = s.as_bytes();
    let mut width = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            let (_, lead, end) = leading_hashes(b, i);
            width += lead;
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(close) = skip1(&s[i + 2..], b"]") else {
                    return 0;
                };
                i += 2 + close + 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().expect("a char boundary");
        i += ch.len_utf8();
        width += char_width(ch);
    }
    width
}

/// tmux's `utf8_cstrwidth`: the columns of text with no style handling.
fn cstr_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if c.is_ascii() {
                usize::from(c > '\x1f' && c != '\x7f')
            } else {
                char_width(c)
            }
        })
        .sum()
}

/// tmux's `format_trim_left`: the first `limit` columns.
fn trim_left(s: &str, limit: usize) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut width = 0;
    let mut i = 0;
    while i < b.len() {
        if width >= limit {
            break;
        }
        if b[i] == b'#' {
            let (n, lead, end) = leading_hashes(b, i);
            let lead = lead.min(limit - width);
            if lead != 0 {
                if n == 1 {
                    out.push('#');
                } else {
                    out.push_str(&"#".repeat(2 * lead));
                }
                width += lead;
            }
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(close) = skip1(&s[i + 2..], b"]") else {
                    break;
                };
                let stop = i + 2 + close + 1;
                out.push_str(&s[i..stop]);
                i = stop;
            }
            continue;
        }
        let ch = s[i..].chars().next().expect("a char boundary");
        i += ch.len_utf8();
        if ch.is_ascii() && !(ch > '\x1f' && ch != '\x7f') {
            continue;
        }
        let w = if ch.is_ascii() { 1 } else { char_width(ch) };
        if width + w <= limit {
            out.push(ch);
        }
        width += w;
    }
    out
}

/// tmux's `format_trim_right`: the last `limit` columns.
fn trim_right(s: &str, limit: usize) -> String {
    let total = style_width(s);
    if total <= limit {
        return s.to_string();
    }
    let skip = total - limit;
    let b = s.as_bytes();
    let mut out = String::new();
    let mut width = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' {
            let (n, lead, end) = leading_hashes(b, i);
            let mut copy = lead;
            if width <= skip {
                copy = if skip - width >= copy {
                    0
                } else {
                    copy - (skip - width)
                };
            }
            if copy != 0 {
                if n == 1 {
                    out.push('#');
                } else {
                    out.push_str(&"#".repeat(2 * copy));
                }
            }
            width += lead;
            i = end;
            if b.get(i) == Some(&b'#') {
                let Some(close) = skip1(&s[i + 2..], b"]") else {
                    break;
                };
                let stop = i + 2 + close + 1;
                out.push_str(&s[i..stop]);
                i = stop;
            }
            continue;
        }
        let ch = s[i..].chars().next().expect("a char boundary");
        i += ch.len_utf8();
        if ch.is_ascii() && !(ch > '\x1f' && ch != '\x7f') {
            continue;
        }
        if width >= skip {
            out.push(ch);
        }
        width += if ch.is_ascii() { 1 } else { char_width(ch) };
    }
    out
}

/// `s` padded with spaces on the right to `width` columns.
fn pad_right(s: &str, width: usize) -> String {
    let n = cstr_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

/// `s` padded with spaces on the left to `width` columns.
fn pad_left(s: &str, width: usize) -> String {
    let n = cstr_width(s);
    format!("{}{s}", " ".repeat(width.saturating_sub(n)))
}

/// tmux's `format_unescape`: `#,` `##` `#{` `#}` `#:` outside any `#{...}`
/// lose their `#`.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut brackets = 0i32;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
            brackets += 1;
        }
        if brackets == 0 && b[i] == b'#' && b.get(i + 1).is_some_and(|c| b",#{}:".contains(c)) {
            out.push(b[i + 1]);
            i += 2;
            continue;
        }
        if b[i] == b'}' {
            brackets -= 1;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// tmux's `format_strip`: every `#,` `##` `#{` `#}` `#:` loses its `#`,
/// except inside a `#{...}`, where it stays.
fn strip(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut brackets = 0i32;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && b.get(i + 1) == Some(&b'{') {
            brackets += 1;
        }
        if b[i] == b'#' && b.get(i + 1).is_some_and(|c| b",#{}:".contains(c)) {
            if brackets != 0 {
                out.push(b[i]);
            }
            i += 1;
            continue;
        }
        if b[i] == b'}' {
            brackets -= 1;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn localtime(t: i64) -> Option<libc::tm> {
    let t = t as libc::time_t;
    // SAFETY: `tm` is plain old data, so all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals; localtime_r is reentrant.
    (!unsafe { libc::localtime_r(&t, &mut tm) }.is_null()).then_some(tm)
}

/// `strftime(3)` of `fmt` for `tm`, with a buffer of `max` bytes: `None`
/// when the result is empty or does not fit, as tmux reads a 0 return.
fn strftime(fmt: &str, tm: &libc::tm, max: usize) -> Option<String> {
    let cfmt = std::ffi::CString::new(fmt).ok()?;
    let mut buf = vec![0u8; max];
    // SAFETY: `buf` is `max` bytes long and `cfmt` a NUL-terminated string;
    // `tm` is a fully initialised broken-down time.
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), max, cfmt.as_ptr(), tm) };
    (n != 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// `fmt` through `strftime` at the current local time (the first thing
/// tmux's `format_expand_time` does).
pub(super) fn strftime_now(fmt: &str, max: usize) -> Option<String> {
    // SAFETY: `time` with a null pointer only returns the time.
    let now = i64::from(unsafe { libc::time(std::ptr::null_mut()) });
    strftime(fmt, &localtime(now)?, max)
}

/// tmux's `format_pretty_time`: how long ago, as short as it can be.
fn pretty_time(t: i64) -> String {
    // SAFETY: `time` with a null pointer only returns the time.
    let now = i64::from(unsafe { libc::time(std::ptr::null_mut()) }).max(t);
    let age = now - t;
    let (Some(now_tm), Some(tm)) = (localtime(now), localtime(t)) else {
        return String::new();
    };
    let fmt = if age < 24 * 3600 {
        "%H:%M"
    } else if (tm.tm_year == now_tm.tm_year && tm.tm_mon == now_tm.tm_mon) || age < 28 * 24 * 3600 {
        "%a%d"
    } else if (tm.tm_year == now_tm.tm_year && tm.tm_mon < now_tm.tm_mon)
        || (tm.tm_year == now_tm.tm_year - 1 && tm.tm_mon > now_tm.tm_mon)
    {
        "%d%b"
    } else {
        "%h%y"
    };
    strftime(fmt, &tm, 9).unwrap_or_default()
}

/// tmux's `regsub`: every match of the extended regular expression
/// `pattern` in `text` replaced by `with`, whose `\0` to `\9` are the match
/// and its groups. `None` when the pattern does not compile.
fn regsub(pattern: &str, with: &str, text: &str, icase: bool) -> Option<String> {
    if text.is_empty() {
        return Some(String::new());
    }
    if pattern.is_empty() {
        return Some(text.to_string());
    }
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(icase)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .ok()?;
    let end = text.len();
    let (mut start, mut last, mut empty) = (0, 0, false);
    let mut buf = String::new();
    while start <= end {
        let Some(m) = re.captures(&text[start..]) else {
            buf.push_str(&text[start.min(end)..end]);
            break;
        };
        let whole = m.get(0).expect("group 0");
        buf.push_str(&text[last..start + whole.start()]);
        if empty || start + whole.start() != last || whole.start() != whole.end() {
            expand_replacement(&mut buf, with, &m);
            last = start + whole.end();
            start += whole.end();
            empty = false;
        } else {
            last = start + whole.end();
            start += whole.end()
                + text[start + whole.end()..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
            empty = true;
        }
        if pattern.starts_with('^') {
            buf.push_str(&text[start.min(end)..end]);
            break;
        }
    }
    Some(buf)
}

/// `regsub_expand`: `with` with each `\N` replaced by group N when it
/// matched something; any other `\c` is `c`.
fn expand_replacement(buf: &mut String, with: &str, m: &regex::Captures) {
    let mut chars = with.chars().peekable();
    while let Some(c) = chars.next() {
        let mut c = c;
        if c == '\\'
            && let Some(&next) = chars.peek()
        {
            chars.next();
            c = next;
            if let Some(i) = next.to_digit(10)
                && let Some(g) = m.get(i as usize)
                && !g.is_empty()
            {
                buf.push_str(g.as_str());
                continue;
            }
        }
        buf.push(c);
    }
}

/// tmux's `colour_fromstring` then `colour_force_rgb`: the `rrggbb` of a
/// colour name, `colourN`, `#rrggbb` or a number, `None` for what tmux
/// refuses or cannot make RGB (`default`, `terminal`).
fn colour_rgb(s: &str) -> Option<u32> {
    let lower = s.to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#')
        && s.len() == 7
    {
        return hex
            .bytes()
            .all(|c| c.is_ascii_hexdigit())
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten();
    }
    for prefix in ["colour", "color"] {
        if let Some(n) = lower.strip_prefix(prefix) {
            return strtonum(n, 0, 255).map(|n| rgb_256(n as usize));
        }
    }
    let n: Option<usize> = match lower.as_str() {
        "default" | "terminal" => return None,
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "brightblack" => Some(8),
        "brightred" => Some(9),
        "brightgreen" => Some(10),
        "brightyellow" => Some(11),
        "brightblue" => Some(12),
        "brightmagenta" => Some(13),
        "brightcyan" => Some(14),
        "brightwhite" => Some(15),
        _ => None,
    };
    if let Some(n) = n {
        return Some(rgb_256(n));
    }
    match s {
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" => return Some(rgb_256(s.parse().ok()?)),
        "90" | "91" | "92" | "93" | "94" | "95" | "96" | "97" => {
            return Some(rgb_256(8 + s.parse::<usize>().ok()? - 90));
        }
        _ => {}
    }
    colour_byname(s)
}

/// tmux's `colour_256toRGB`.
fn rgb_256(n: usize) -> u32 {
    const BASE: [u32; 16] = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    const CUBE: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    match n & 0xff {
        i @ 0..16 => BASE[i],
        i @ 16..232 => {
            let i = i - 16;
            (CUBE[i / 36] << 16) | (CUBE[(i / 6) % 6] << 8) | CUBE[i % 6]
        }
        i => {
            let g = (8 + 10 * (i - 232)) as u32;
            (g << 16) | (g << 8) | g
        }
    }
}

/// tmux's `colour_byname`: `greyN`, then the X11 names, ignoring case.
fn colour_byname(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("grey")
        .or_else(|| lower.strip_prefix("gray"))
    {
        if rest.is_empty() {
            return Some(0xbebebe);
        }
        let n = strtonum(rest, 0, 100)?;
        let c = (2.55 * n as f64).round() as u32;
        return (c <= 255).then_some((c << 16) | (c << 8) | c);
    }
    X11.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, c)| *c)
}

/// Shell-quotes with a backslash before each character a shell treats
/// specially (`format_quote_shell`).
fn quote_shell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "|&;<>()$`\\\"'*?[# =%".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Doubles each `#` (`format_quote_style`).
fn quote_style(s: &str) -> String {
    s.replace('#', "##")
}

/// One word of a command line, quoted as tmux's `args_escape` quotes it
/// (checked against tmux 3.7b): double quotes around a word with any of
/// ` #';${}%`, else single quotes around one with a space or `"`; a lone
/// special character, or a leading `~`, gets a backslash; control
/// characters and backslashes are escaped C-style.
pub fn escape_arg(s: &str) -> String {
    const DQUOTED: &str = " #';${}%";
    const SQUOTED: &str = " \"";
    if s.is_empty() {
        return "''".to_string();
    }
    let quote = if s.contains(|c| DQUOTED.contains(c)) {
        Some('"')
    } else if s.contains(|c| SQUOTED.contains(c)) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c != ' '
        && (quote.is_some() || c == '~')
    {
        return format!("\\{c}");
    }
    let bytes = s.as_bytes();
    let mut body = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        match c {
            '\\' => body.push_str("\\\\"),
            '\n' => body.push_str("\\n"),
            '\t' => body.push_str("\\t"),
            '\r' => body.push_str("\\r"),
            '\x07' => body.push_str("\\a"),
            '\x08' => body.push_str("\\b"),
            '\x0b' => body.push_str("\\v"),
            '\x0c' => body.push_str("\\f"),
            c if c.is_ascii_control() => body.push_str(&format!("\\{:03o}", c as u32)),
            '"' if quote == Some('"') => body.push_str("\\\""),
            // `$` only where a shell would start a variable after it.
            '$' if quote == Some('"')
                && bytes.get(i + 1).is_some_and(|&n| {
                    n == b'_' || n == b'{' || n.is_ascii_alphabetic() || n >= 0x80
                }) =>
            {
                body.push_str("\\$")
            }
            c => body.push(c),
        }
    }
    let tilde = if body.starts_with('~') { "\\" } else { "" };
    match quote {
        Some('\'') => format!("'{body}'"),
        Some(q) => format!("{q}{tilde}{body}{q}"),
        None => format!("{tilde}{body}"),
    }
}

/// What the modifiers of one `#{...}` ask for (tmux's `FORMAT_*` bits).
#[derive(Default)]
struct Flags {
    literal: bool,
    character: bool,
    basename: bool,
    colour: bool,
    dirname: bool,
    length: bool,
    width: bool,
    not: bool,
    not_not: bool,
    quote_shell: bool,
    quote_style: bool,
    quote_args: bool,
    expand: bool,
    expand_time: bool,
    timestring: bool,
    pretty: bool,
    sessions: bool,
    windows: bool,
    panes: bool,
    clients: bool,
    window_name: bool,
    session_name: bool,
    repeat: bool,
}

/// The variables of one loop iteration: the item's own, `loop_last_flag`,
/// and what the loop adds.
struct Overlay<'a> {
    inner: Box<dyn FormatVars + 'a>,
    extras: Vec<(String, String)>,
    last: bool,
}

impl FormatVars for Overlay<'_> {
    fn get(&self, name: &str) -> Option<String> {
        if name == "loop_last_flag" {
            return Some(flag(self.last));
        }
        if let Some((_, v)) = self.extras.iter().find(|(k, _)| k == name) {
            return Some(v.clone());
        }
        self.inner.get(name)
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        self.inner.loop_items(kind, sort)
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.inner.name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.inner.pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        self.inner.reexpand(text)
    }
}

impl Expander<'_> {
    /// tmux's `format_build_modifiers`: the `;`-separated modifiers at the
    /// start of `body` and the index just past the `:` that ends them, or
    /// `None` when `body` does not start with a modifier list. Arguments
    /// follow a modifier with any punctuation character for delimiter
    /// (`s/a/b/`, `=|5|~`), or run to the next `;` or `:` (`=5`); each is
    /// expanded as a format.
    pub(super) fn build_modifiers(&self, body: &str) -> Option<(Vec<Modifier>, usize)> {
        let b = body.as_bytes();
        let at = |i: usize| b.get(i).copied().unwrap_or(0);
        let mut list: Vec<Modifier> = Vec::new();
        let mut cp = 0;
        while at(cp) != 0 && at(cp) != b':' {
            if at(cp) == b';' {
                cp += 1;
            }
            if at(cp) == 0 {
                break;
            }
            let c = at(cp);
            // Single characters with no arguments.
            if b"labcdnwETSWPL!<>".contains(&c) && is_end(at(cp + 1)) {
                list.push(Modifier::new(&body[cp..cp + 1], Vec::new()));
                cp += 1;
                continue;
            }
            // Then two characters with no arguments.
            if ["||", "&&", "!!", "!=", "==", "<=", ">="]
                .iter()
                .any(|m| body[cp..].starts_with(m))
                && is_end(at(cp + 2))
            {
                list.push(Modifier::new(&body[cp..cp + 2], Vec::new()));
                cp += 2;
                continue;
            }
            // Now one character with arguments.
            if !b"mCLNPSst=pReqW".contains(&c) {
                break;
            }
            let name = &body[cp..cp + 1];
            if is_end(at(cp + 1)) {
                list.push(Modifier::new(name, Vec::new()));
                cp += 1;
                continue;
            }
            // One argument, with no delimiter around it.
            if !at(cp + 1).is_ascii_punctuation() || at(cp + 1) == b'-' {
                let Some(len) = skip1(&body[cp + 1..], b":;") else {
                    break;
                };
                let arg = self.expand(&body[cp + 1..cp + 1 + len]);
                list.push(Modifier::new(name, vec![arg]));
                cp += 1 + len;
                continue;
            }
            // Several, each between delimiters.
            let wrap = at(cp + 1);
            let stops = [wrap, b';', b':'];
            let mut args = Vec::new();
            cp += 1;
            loop {
                if at(cp) == wrap && is_end(at(cp + 1)) {
                    cp += 1;
                    break;
                }
                let Some(len) = skip1(&body[cp + 1..], &stops) else {
                    break;
                };
                cp += 1;
                args.push(self.expand(&body[cp..cp + len]));
                cp += len;
                if is_end(at(cp)) {
                    break;
                }
            }
            list.push(Modifier::new(name, args));
        }
        (at(cp) == b':').then_some((list, cp + 1))
    }

    /// A value `key` names, with the text modifiers applied (tmux's
    /// `format_find` after the lookup): a time as text, a basename or
    /// dirname, a quoting.
    fn find(&self, key: &str, f: &Flags, time_format: Option<&str>) -> Option<String> {
        let found = self.vars.get(key)?;
        if f.timestring {
            let t = strtonum(&found, 0, i64::MAX).filter(|t| *t != 0)?;
            return Some(if f.pretty {
                pretty_time(t)
            } else if let Some(tf) = time_format {
                localtime(t)
                    .and_then(|tm| strftime(tf, &tm, 512))
                    .unwrap_or_default()
            } else {
                ctime(t).unwrap_or_default()
            });
        }
        let mut v = found;
        if f.basename {
            v = posix_basename(&v);
        }
        if f.dirname {
            v = posix_dirname(&v);
        }
        if f.quote_shell {
            v = quote_shell(&v);
        }
        if f.quote_style {
            v = quote_style(&v);
        }
        if f.quote_args {
            v = escape_arg(&v);
        }
        Some(v)
    }

    /// tmux's `format_choose` with expansion: the two halves of `s` at its
    /// first top-level comma, each expanded.
    fn choose(&self, s: &str) -> Option<(String, String)> {
        let c = skip1(s, b",")?;
        Some((self.expand(&s[..c]), self.expand(&s[c + 1..])))
    }

    /// tmux's `format_replace`, from the modifier list on: what `copy`
    /// (the text after the `:`) means under `list`. `Err` is a failure, which
    /// ends the format.
    pub(super) fn replace(&self, list: &[Modifier], copy: &str) -> Result<String, Fail> {
        let mut f = Flags::default();
        let mut sort = LoopSort {
            order: SortOrder::Order,
            reversed: false,
        };
        let (mut cmp, mut search, mut mexp, mut bool_op) = (None, None, None, None);
        let mut subs: Vec<&Modifier> = Vec::new();
        let (mut limit, mut width) = (0i64, 0i64);
        let mut marker: Option<&str> = None;
        let mut time_format: Option<String> = None;
        for fm in list {
            let arg = |i: usize| fm.args.get(i).map(String::as_str);
            let has = |c: char| arg(0).is_some_and(|a| a.contains(c));
            if fm.name.len() == 1 {
                match fm.name.as_bytes()[0] {
                    b'm' | b'<' | b'>' => cmp = Some(fm),
                    b'!' => f.not = true,
                    b'C' => search = Some(fm),
                    b's' => {
                        if fm.args.len() >= 2 {
                            subs.push(fm);
                        }
                    }
                    b'=' => {
                        if let Some(a) = arg(0) {
                            limit = strtonum(a, -MAX_WIDTH, MAX_WIDTH).unwrap_or(0);
                            if let Some(m) = arg(1) {
                                marker = Some(m);
                            }
                        }
                    }
                    b'p' => {
                        if let Some(a) = arg(0) {
                            width = strtonum(a, -MAX_WIDTH, MAX_WIDTH).unwrap_or(0);
                        }
                    }
                    b'w' => f.width = true,
                    b'e' => {
                        if (1..=3).contains(&fm.args.len()) {
                            mexp = Some(fm);
                        }
                    }
                    b'l' => f.literal = true,
                    b'a' => f.character = true,
                    b'b' => f.basename = true,
                    b'c' => f.colour = true,
                    b'd' => f.dirname = true,
                    b'n' => f.length = true,
                    b't' => {
                        f.timestring = true;
                        if has('p') {
                            f.pretty = true;
                        } else if has('f')
                            && let Some(tf) = arg(1)
                        {
                            time_format = Some(strip(tf));
                        }
                    }
                    b'q' => {
                        if fm.args.is_empty() {
                            f.quote_shell = true;
                        } else if has('e') || has('h') {
                            f.quote_style = true;
                        } else if has('a') {
                            f.quote_args = true;
                        }
                    }
                    b'E' => f.expand = true,
                    b'T' => f.expand_time = true,
                    b'N' => {
                        if fm.args.is_empty() || has('w') {
                            f.window_name = true;
                        } else if has('s') {
                            f.session_name = true;
                        }
                    }
                    b'S' | b'W' | b'L' => {
                        let default = if fm.name == "S" {
                            f.sessions = true;
                            SortOrder::Index
                        } else {
                            if fm.name == "W" {
                                f.windows = true;
                            } else {
                                f.clients = true;
                            }
                            SortOrder::Order
                        };
                        sort.order = if has('n') {
                            SortOrder::Name
                        } else if has('t') {
                            SortOrder::Activity
                        } else {
                            default
                        };
                        sort.reversed = has('r');
                    }
                    b'P' => {
                        f.panes = true;
                        sort.order = SortOrder::Creation;
                        sort.reversed = has('r');
                    }
                    b'R' => f.repeat = true,
                    _ => {}
                }
            } else if fm.name.len() == 2 {
                match fm.name.as_str() {
                    "||" | "&&" => bool_op = Some(fm),
                    "!!" => f.not_not = true,
                    "==" | "!=" | ">=" | "<=" => cmp = Some(fm),
                    _ => {}
                }
            }
        }

        let mut value = if f.literal {
            unescape(copy)
        } else if f.character {
            let new = self.expand(copy);
            match strtonum(&new, 32, 126) {
                Some(c) => char::from(c as u8).to_string(),
                None => String::new(),
            }
        } else if f.colour {
            colour_rgb(&self.expand(copy)).map_or_else(String::new, |c| format!("{c:06x}"))
        } else if f.sessions {
            self.loop_over(LoopKind::Sessions, sort, copy)?
        } else if f.windows {
            self.loop_over(LoopKind::Windows, sort, copy)?
        } else if f.panes {
            self.loop_over(LoopKind::Panes, sort, copy)?
        } else if f.clients {
            self.loop_over(LoopKind::Clients, sort, copy)?
        } else if f.window_name {
            let name = self.expand(copy);
            let found = self.vars.name_exists(NameKind::Window, &name).ok_or(Fail)?;
            flag(found)
        } else if f.session_name {
            let name = self.expand(copy);
            flag(
                self.vars
                    .name_exists(NameKind::Session, &name)
                    .unwrap_or(false),
            )
        } else if let Some(fm) = search {
            let term = self.expand(copy);
            let (regex, ignore) = (
                fm.args.first().is_some_and(|a| a.contains('r')),
                fm.args.first().is_some_and(|a| a.contains('i')),
            );
            self.vars
                .pane_search(&term, regex, ignore)
                .unwrap_or(0)
                .to_string()
        } else if f.repeat {
            let (left, right) = self.choose(copy).ok_or(Fail)?;
            match strtonum(&right, 1, MAX_REPEAT) {
                Some(n) if left.len().saturating_mul(n as usize) <= REPEAT_BYTES => {
                    left.repeat(n as usize)
                }
                _ => String::new(),
            }
        } else if f.not {
            flag(!is_truthy(&self.expand(copy)))
        } else if f.not_not {
            flag(is_truthy(&self.expand(copy)))
        } else if let Some(fm) = bool_op {
            self.bool_op_n(copy, fm.name == "&&")
        } else if let Some(fm) = cmp {
            let (left, right) = self.choose(copy).ok_or(Fail)?;
            flag(match fm.name.as_str() {
                "==" => left == right,
                "!=" => left != right,
                "<" => left < right,
                ">" => left > right,
                "<=" => left <= right,
                ">=" => left >= right,
                _ => {
                    let flags = fm.args.first().map_or("", String::as_str);
                    let icase = flags.contains('i');
                    if flags.contains('r') {
                        regex_match(&left, &right, icase)
                    } else if icase {
                        fnmatch(
                            left.to_ascii_lowercase().as_bytes(),
                            right.to_ascii_lowercase().as_bytes(),
                        )
                    } else {
                        fnmatch(left.as_bytes(), right.as_bytes())
                    }
                }
            })
        } else if let Some(rest) = copy.strip_prefix('?') {
            self.expand_ternary(rest, copy)
        } else if let Some(fm) = mexp {
            self.expression(fm, copy).unwrap_or_default()
        } else if copy.contains("#{") {
            self.expand(copy)
        } else {
            self.find(copy, &f, time_format.as_deref())
                .unwrap_or_default()
        };

        // Expand again.
        if f.expand {
            value = self.expand_owned(&self.vars.reexpand(&value));
        } else if f.expand_time {
            let before = self.shared.expand_time.replace(true);
            value = self.expand_owned(&self.vars.reexpand(&value));
            self.shared.expand_time.set(before);
        }

        // Substitute.
        for sub in subs {
            let pattern = self.expand_owned(&sub.args[0]);
            let with = self.expand_owned(&sub.args[1]);
            let icase = sub.args.get(2).is_some_and(|a| a.contains('i'));
            value = regsub(&pattern, &with, &value, icase).unwrap_or(value);
        }

        // Truncate.
        if limit != 0 {
            let cols = usize::try_from(limit.unsigned_abs()).unwrap_or(0);
            let new = if limit > 0 {
                trim_left(&value, cols)
            } else {
                trim_right(&value, cols)
            };
            value = match marker {
                Some(m) if new != value => {
                    if limit > 0 {
                        format!("{new}{m}")
                    } else {
                        format!("{m}{new}")
                    }
                }
                _ => new,
            };
        }

        // Pad.
        if width > 0 {
            value = pad_right(&value, width as usize);
        } else if width < 0 {
            value = pad_left(&value, width.unsigned_abs() as usize);
        }

        // Replace with the length or the width.
        if f.length {
            value = value.len().to_string();
        }
        if f.width {
            value = style_width(&value).to_string();
        }
        Ok(value)
    }

    /// tmux's `format_bool_op_n`: `&&` or `||` over the comma-separated
    /// operands of `fmt`, stopping at the first that decides it.
    fn bool_op_n(&self, fmt: &str, and: bool) -> String {
        let mut result = and;
        let mut rest = fmt;
        while if and { result } else { !result } {
            let (operand, tail) = match skip1(rest, b",") {
                Some(c) => (&rest[..c], Some(&rest[c + 1..])),
                None => (rest, None),
            };
            let truthy = is_truthy(&self.expand(operand));
            result = if and {
                result && truthy
            } else {
                result || truthy
            };
            match tail {
                Some(t) => rest = t,
                None => break,
            }
        }
        flag(result)
    }

    /// tmux's `format_replace_expression`: `#{e|op|flags|precision:a,b}`.
    fn expression(&self, fm: &Modifier, copy: &str) -> Option<String> {
        let op = fm.args[0].as_str();
        if ![
            "+", "-", "*", "/", "%", "m", "==", "!=", ">", "<", ">=", "<=",
        ]
        .contains(&op)
        {
            return None;
        }
        let (mut use_fp, mut prec) = (false, 0i64);
        if fm.args.len() >= 2 && fm.args[1].contains('f') {
            use_fp = true;
            prec = 2;
        }
        if fm.args.len() >= 3 {
            prec = strtonum(&fm.args[2], -MAX_PRECISION, MAX_PRECISION)?;
        }
        let (left, right) = self.choose(copy)?;
        let (mut l, mut r) = (strtod(&left)?, strtod(&right)?);
        if !use_fp {
            l = l as i64 as f64;
            r = r as i64 as f64;
        }
        let result = match op {
            "+" => l + r,
            "-" => l - r,
            "*" => l * r,
            "/" => l / r,
            "%" | "m" => l % r,
            "==" => f64::from((l - r).abs() < 1e-9),
            "!=" => f64::from((l - r).abs() > 1e-9),
            ">" => f64::from(l > r),
            ">=" => f64::from(l >= r),
            "<" => f64::from(l < r),
            _ => f64::from(l <= r),
        };
        Some(if use_fp {
            format_float(result, prec)
        } else {
            format_float(result as i64 as f64, prec)
        })
    }

    /// `#{S:}`, `#{W:}`, `#{P:}` and `#{L:}`: `copy` once for each item, or,
    /// with a comma at its top level, its first half for each and its second
    /// for the current one.
    fn loop_over(&self, kind: LoopKind, sort: LoopSort, copy: &str) -> Result<String, Fail> {
        let (all, active) = match skip1(copy, b",") {
            Some(c) => (&copy[..c], Some(&copy[c + 1..])),
            None => (copy, None),
        };
        let items = self.vars.loop_items(kind, sort).ok_or(Fail)?;
        // A loop inside the last item of another is `loop_last_flag` for
        // all of its own: tmux ORs the flag onto the parent's.
        let parent_last = self.vars.get("loop_last_flag").as_deref() == Some("1");
        let n = items.len();
        let mut out = String::new();
        for (i, item) in items.into_iter().enumerate() {
            let use_text = match active {
                Some(a) if item.active => a,
                _ => all,
            };
            let overlay = Overlay {
                inner: item.vars,
                extras: item.extras,
                last: parent_last || i + 1 == n,
            };
            out.push_str(&self.expand_with(&overlay, use_text));
        }
        Ok(out)
    }
}

impl Modifier {
    fn new(name: &str, args: Vec<String>) -> Modifier {
        Modifier {
            name: name.to_string(),
            args,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::expand;
    use super::*;

    struct Vars(BTreeMap<&'static str, &'static str>);

    impl FormatVars for Vars {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|v| v.to_string())
        }
    }

    fn vars() -> Vars {
        Vars(
            [
                ("session_name", "main"),
                ("@opt", "value"),
                ("@path", "/usr/local/bin/tool/"),
                ("@wide", "日本語ab"),
                ("@nest", "#{@opt}!"),
                ("@num", "12"),
                ("@ts", "1700000000"),
                ("@spaced", "a b|c#d"),
                ("@f", "#{session_name}-100%%"),
            ]
            .into_iter()
            .collect(),
        )
    }

    /// Every case here is a line of `golden/fe_3/format-modifiers`, which
    /// tmux 3.7b recorded; the full set runs against a real server in
    /// `tests/compat/fe_3.rs`.
    #[track_caller]
    fn check(cases: &[(&str, &str)]) {
        for (fmt, want) in cases {
            assert_eq!(expand(fmt, &vars()), *want, "{fmt}");
        }
    }

    #[test]
    fn trimming() {
        check(&[
            ("#{=3:session_name}", "mai"),
            ("#{=-3:session_name}", "ain"),
            ("#{=0:session_name}", "main"),
            ("#{=10:session_name}", "main"),
            ("#{=x:session_name}", "main"),
            ("#{=/3/...:session_name}", "mai..."),
            ("#{=/-3/...:session_name}", "...ain"),
            ("#{=/10/...:session_name}", "main"),
            ("#{=/3/:session_name}", "mai"),
            ("#{=|3|~~:session_name}", "mai~~"),
            ("#{=3:@wide}", "日"),
            ("#{=4:@wide}", "日本"),
            ("#{=-3:@wide}", "ab"),
            ("#{=2:#{@opt}}", "va"),
            ("#{=2:@nest}", "#{"),
        ]);
    }

    #[test]
    fn padding_length_and_width() {
        check(&[
            ("[#{p8:session_name}]", "[main    ]"),
            ("[#{p-8:session_name}]", "[    main]"),
            ("#{p2:session_name}", "main"),
            ("#{p0:session_name}", "main"),
            ("#{p8:@wide}", "日本語ab"),
            ("#{p:session_name}", "main"),
            ("[#{p6:#{@opt}}]", "[value ]"),
            ("#{n:session_name}", "4"),
            ("#{n:@wide}", "11"),
            ("#{w:@wide}", "8"),
            ("#{n:nosuchvar}", "0"),
            ("#{n:#{@opt}}", "5"),
        ]);
    }

    #[test]
    fn substitution() {
        check(&[
            ("#{s/a/A/:session_name}", "mAin"),
            ("#{s/A/x/i:session_name}", "mxin"),
            ("#{s/A/x/:session_name}", "main"),
            ("#{s/a|i/_/:session_name}", "m__n"),
            (r"#{s/(m)(a)/\2\1/:session_name}", "amin"),
            (r"#{s/(m)/<\0>/:session_name}", "<m>ain"),
            ("#{s/x*/-/:session_name}", "m-a-i-n-"),
            ("#{s/^m/M/:session_name}", "Main"),
            ("#{s/n$/N/:session_name}", "maiN"),
            ("#{s|a|b|:session_name}", "mbin"),
            ("#{s,a,b,:session_name}", "mbin"),
            ("#{s/a/A/;s/i/I/:session_name}", "mAIn"),
            ("#{s/a/b/;s/b/c/:session_name}", "mcin"),
            ("#{s/a/A:session_name}", "mAin"),
            ("#{s/a:session_name}", "main"),
            ("#{s/(/x/:session_name}", "main"),
            ("#{s//x/:session_name}", "main"),
            ("#{s/a//:session_name}", "min"),
            ("#{s/#{@opt}/Z/:@opt}", "Z"),
            ("#{s/a/A/:nosuchvar}", ""),
        ]);
    }

    #[test]
    fn chains() {
        check(&[
            ("[#{=2;p5:session_name}]", "[ma   ]"),
            ("#{s/a/A/;=2:session_name}", "mA"),
            ("[#{=-2;p-5:session_name}]", "[   in]"),
            ("#{n;p3:session_name}", "4"),
            ("#{s/a/AA/;n:session_name}", "5"),
            ("#{b;=2:@path}", "to"),
            ("#{d;b:@path}", "."),
            ("#{;:session_name}", "main"),
            ("#{=3;:session_name}", "mai"),
            ("#{=3;;=2:session_name}", ""),
            ("#{X:session_name}", ""),
            ("#{=3,session_name}", ""),
            ("[#{p5;:session_name}]", "[main ]"),
            ("#{=/3/x/y:session_name}", "maix"),
            ("#{=/3:session_name}", "mai"),
            ("#{s/a/b/x/y:session_name}", "mbin"),
        ]);
    }

    #[test]
    fn paths() {
        check(&[
            ("#{b:@path}", "tool"),
            ("#{d:@path}", "/usr/local/bin"),
            ("#{d:nosuchvar}", ""),
        ]);
    }

    #[test]
    fn arithmetic() {
        check(&[
            ("#{e|+:3,4}", "7"),
            ("#{e|-:3,4}", "-1"),
            ("#{e|*:3,4}", "12"),
            ("#{e|/:7,2}", "3"),
            ("#{e|m:7,2}", "1"),
            ("#{e|%:7,2}", "1"),
            ("#{e|/|f:7,2}", "3.50"),
            ("#{e|/|f|4:7,3}", "2.3333"),
            ("#{e|+|f:1.5,2.25}", "3.75"),
            ("#{e|+:1.5,2.25}", "3"),
            ("#{e|*|f|0:1.5,3}", "4"),
            ("#{e|==:1,1}", "1"),
            ("#{e|!=:1,1}", "0"),
            ("#{e|>:3,2}", "1"),
            ("#{e|>=:2,2}", "1"),
            ("#{e|<:3,2}", "0"),
            ("#{e|<=:2,3}", "1"),
            ("#{e|==|f:0.1,0.1}", "1.00"),
            ("#{e|+:#{@num},#{@num}}", "24"),
            ("#{e|+:x,1}", ""),
            ("#{e|+:1}", ""),
            ("#{e|?:1,2}", ""),
            ("#{e|/|f:1,0}", "inf"),
            ("#{e|-:-3,-4}", "1"),
            ("#{e|+|f|2:1e2,1}", "101.00"),
            ("#{e|+|f|-1:1,1}", "2.000000"),
            ("#{e|+|f|abc:1,1}", ""),
            ("#{e|+|x:1,2}", "3"),
            ("#{e|+|f|3|9:1,2}", ""),
            ("#{e|+:#{e|*:2,3},4}", "10"),
            ("#{e|*:2,3;=1:4}", ""),
        ]);
    }

    #[test]
    fn characters_and_colours() {
        check(&[
            ("#{a:65}", "A"),
            ("#{a:126}", "~"),
            ("#{a:127}", ""),
            ("#{a:31}", ""),
            ("#{a:x}", ""),
            ("#{a:#{e|+:60,5}}", "A"),
            ("#{c:red}", "800000"),
            ("#{c:#336699}", "336699"),
            ("#{c:colour39}", "00afff"),
            ("#{c:colour200}", "ff00d7"),
            ("#{c:3}", "808000"),
            ("#{c:default}", ""),
            ("#{c:nonsense}", ""),
            ("#{c:brightred}", "ff0000"),
            ("#{c:#{@opt}}", ""),
            ("#{c:grey50}", "7f7f7f"),
            ("#{c:orange}", "ffa500"),
        ]);
    }

    #[test]
    fn repeat() {
        check(&[
            ("#{R:ab,3}", "ababab"),
            ("#{R:ab,0}", ""),
            ("#{R:ab,x}", ""),
            ("#{R:#{@opt},2}", "valuevalue"),
            ("#{R:-,#{@num}}", "------------"),
        ]);
        // No comma fails the `#{...}`, and the format ends there.
        check(&[("a#{R:ab}b", "a")]);
        // One repeat may not grow past a few megabytes.
        let got = expand("#{R:#{R:ab,10000},10000}", &vars());
        assert!(got.len() <= REPEAT_BYTES);
    }

    #[test]
    fn expand_again() {
        check(&[
            ("#{E:@nest}", "value!"),
            ("#{@nest}", "#{@opt}!"),
            ("#{E:@opt}", "value"),
            ("#{T:@f}", "main-100%"),
            ("#{T:@nest}", "value!"),
            ("#{E;=3:@nest}", "val"),
            ("#{E:nosuchvar}", ""),
            ("#{T:session_name}", "main"),
        ]);
    }

    #[test]
    fn quoting() {
        check(&[
            ("#{q:@spaced}", r"a\ b\|c\#d"),
            ("#{q/e:@spaced}", "a b|c##d"),
            ("#{q/h:@spaced}", "a b|c##d"),
            ("#{q/a:@spaced}", "\"a b|c#d\""),
            ("#{q/x:@spaced}", "a b|c#d"),
            ("#{q/eh:@spaced}", "a b|c##d"),
            ("#{q;=3:@spaced}", r"a\ "),
            ("#{q:#{@spaced}}", "a b|c#d"),
        ]);
    }

    #[test]
    fn times() {
        check(&[
            ("#{t/f/%Y:@ts}", "2023"),
            ("#{t/f/%Y|=4:@ts}", "2023|=4"),
            ("#{t/p:@ts}", "Nov23"),
            ("#{t:@opt}", ""),
            ("#{t:nosuchvar}", ""),
        ]);
    }

    #[test]
    fn other_forms_with_the_modifier_parser() {
        check(&[
            ("#{&&;!:1,0}", "0"),
            ("#{||:0,0,1}", "1"),
            ("#{!;!:1}", "0"),
            ("#{==;p5:a,a}", "1    "),
            ("#{m;=2:a*,abc}", "1"),
            ("#{m/i:A*,abc}", "1"),
            ("#{=2:#{?1,abc,def}}", "de"),
            ("#{l:#{x}}", "#{x}"),
        ]);
    }

    #[test]
    fn a_loop_with_no_session_ends_the_format() {
        check(&[("a#{W:x}b", "a"), ("a#{S:x}b", "a"), ("a#{N:first}b", "a")]);
    }
}
