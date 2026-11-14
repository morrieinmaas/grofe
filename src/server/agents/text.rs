//! Sanitising and validating strings an agent controls (spec §2.7, §2.6).

const MAX_CHARS: usize = 200;

/// Strip control characters, defuse format sequences, cut to 200 chars.
pub fn sanitize(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| !c.is_control()).collect();
    double_hashes_before_bracket(&stripped.replace("#(", "_("))
        .chars()
        .take(MAX_CHARS)
        .collect()
}

/// `#[` becomes `##[`, and so does each `#` of a longer run: the status line
/// reads `##` as a literal `#`, so only an even run before `[` keeps the
/// `[` literal (`##[` alone would leave `###[`, which opens a style).
fn double_hashes_before_bracket(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        if c == '#' {
            run += 1;
            continue;
        }
        let n = if c == '[' { run * 2 } else { run };
        out.extend(std::iter::repeat_n('#', n));
        run = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('#', run));
    out
}

/// A kind name: `[A-Za-z0-9._-]{1,32}`.
pub fn valid_kind(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A session id: `[A-Za-z0-9._:-]{1,128}`.
pub fn valid_session_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_example() {
        assert_eq!(
            sanitize("a\x1b[31m#(rm)#[range=user|x]\u{85}b"),
            "a[31m_(rm)##[range=user|x]b"
        );
    }

    #[test]
    fn sanitize_cuts_200_chars() {
        assert_eq!(sanitize(&"é".repeat(300)).chars().count(), 200);
    }

    #[test]
    fn sanitize_strips_c1() {
        assert_eq!(sanitize("a\u{80}b\u{9f}c"), "abc");
    }

    #[test]
    fn sanitize_strips_del() {
        assert_eq!(sanitize("a\x7fb"), "ab");
    }

    /// Whatever run of `#` comes before a `[`, the status line draws the
    /// sanitised text as literal text and opens no style or range.
    #[test]
    fn sanitized_text_opens_no_style_in_a_status_line() {
        use crate::server::status::{Style, draw};
        for evil in [
            "#[range=user|x]X",
            "##[range=user|x]X",
            "###[range=user|x]X",
            "#####[range=user|x]X",
            "#\x1b[range=user|x]X",
            "#\u{85}#[range=user|x]X",
        ] {
            let row = draw(&sanitize(evil), Style::default(), 60);
            let text: String = row.line.spans.iter().map(|s| s.text.as_str()).collect();
            assert!(row.ranges.is_empty(), "{evil:?} opened a range");
            assert!(text.contains("[range=user|x]X"), "{evil:?} drew {text:?}");
        }
    }

    #[test]
    fn validators() {
        assert!(valid_kind("claude"));
        assert!(!valid_kind("a b"));
        assert!(!valid_kind(""));
        assert!(valid_session_id("abc-1:2.x_y"));
        assert!(!valid_session_id("x;rm"));
        assert!(!valid_session_id(&"a".repeat(129)));
    }
}
