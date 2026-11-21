//! Numbers and ids read as tmux reads them with `sscanf`: whitespace is
//! skipped before a number and before a `%`/`@` sigil, then an optional sign
//! and the leading digits; the rest is left over.

/// What C's `isspace` skips.
const SPACE: [char; 6] = [' ', '\t', '\n', '\x0b', '\x0c', '\r'];

/// A number as `sscanf("%d")` reads it, and the rest of `text`.
pub(super) fn scan_int(text: &str) -> Option<(i64, &str)> {
    let rest = text.trim_start_matches(SPACE);
    let (sign, rest) = match rest.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, rest.strip_prefix('+').unwrap_or(rest)),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let n: i64 = rest[..end].parse().ok()?;
    Some((sign * n, &rest[end..]))
}

/// A number as `sscanf("%u")` reads it: a negative one wraps, so it is
/// never a small size.
pub(super) fn scan_uint(text: &str) -> Option<(u32, &str)> {
    let (n, rest) = scan_int(text)?;
    Some((u32::try_from(n).unwrap_or(u32::MAX), rest))
}

/// `<sigil><n>` as `sscanf("%%%d")` (or `"@%d"`) reads it: `%0x` and
/// `% 0` are 0.
pub(super) fn scan_id(text: &str, sigil: char) -> Option<i64> {
    let rest = text.trim_start_matches(SPACE).strip_prefix(sigil)?;
    scan_int(rest).map(|(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_scan_as_sscanf_does() {
        assert_eq!(scan_id("%0x", '%'), Some(0));
        assert_eq!(scan_id("%00", '%'), Some(0));
        assert_eq!(scan_id("%-1", '%'), Some(-1));
        assert_eq!(scan_id("%+7", '%'), Some(7));
        assert_eq!(scan_id("% 1", '%'), Some(1));
        assert_eq!(scan_id(" %1", '%'), Some(1));
        assert_eq!(scan_id("@ 0", '@'), Some(0));
        assert_eq!(scan_id("\u{a0}%1", '%'), None);
        assert_eq!(scan_id("%x", '%'), None);
        assert_eq!(scan_id("@3", '%'), None);
        assert_eq!(scan_uint(" 90x"), Some((90, "x")));
        assert_eq!(scan_uint("-1").map(|(n, _)| n), Some(u32::MAX));
    }
}
