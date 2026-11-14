//! Sanitising and validating strings an agent controls (spec §2.7, §2.6).

const MAX_CHARS: usize = 200;

/// Strip control characters, defuse format sequences, cut to 200 chars.
pub fn sanitize(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| !c.is_control()).collect();
    stripped
        .replace("#(", "_(")
        .replace("#[", "##[")
        .chars()
        .take(MAX_CHARS)
        .collect()
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
