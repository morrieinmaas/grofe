//! Wall-clock helpers shared by format expansion, the status line, the
//! clock and the message log: local time, `strftime(3)` and tmux's
//! `format_pretty_time`.

use nix::libc;

/// Seconds since the epoch, 0 before it.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `t` as local broken-down time; `None` when it has none.
pub fn localtime(t: i64) -> Option<libc::tm> {
    let t = t as libc::time_t;
    // SAFETY: `tm` is plain old data, so all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals; localtime_r is reentrant.
    (!unsafe { libc::localtime_r(&t, &mut tm) }.is_null()).then_some(tm)
}

/// `strftime(3)` of `fmt` for `tm`, with a buffer of `max` bytes: `None`
/// when the result is empty or does not fit, as tmux reads a 0 return.
pub fn strftime(fmt: &str, tm: &libc::tm, max: usize) -> Option<String> {
    let cfmt = std::ffi::CString::new(fmt).ok()?;
    let mut buf = vec![0u8; max];
    // SAFETY: `buf` is `max` bytes long and `cfmt` a NUL-terminated string;
    // `tm` is a fully initialised broken-down time.
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), max, cfmt.as_ptr(), tm) };
    (n != 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// `fmt` through `strftime` at the current local time (the first thing
/// tmux's `format_expand_time` does).
pub fn strftime_now(fmt: &str, max: usize) -> Option<String> {
    strftime(fmt, &localtime(now_secs())?, max)
}

/// tmux's `format_pretty_time` without seconds (`#{t/p:...}`): `HH:MM`
/// within a day, `%a%d` within the month or 28 days, `%d%b` within the
/// year, else `%b%y`. Empty when the time has no local form.
pub fn pretty_time(t: i64) -> String {
    pretty_time_at(t, now_secs())
}

fn pretty_time_at(t: i64, now: i64) -> String {
    let now = now.max(t);
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
    // tmux formats into 9 bytes.
    strftime(fmt, &tm, 9).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 3600;

    fn digits(s: &str) -> bool {
        s.chars().all(|c| c.is_ascii_digit())
    }

    /// Every former caller's input: a time now (message log, `#{t/p:}`), a
    /// week, a year and a time in the future (clamped to now).
    #[test]
    fn pretty_time_by_age() {
        let now = now_secs();
        let t = pretty_time(now);
        assert!(t.len() == 5 && t.as_bytes()[2] == b':', "{t}");
        let t = pretty_time(now + 10 * DAY);
        assert!(t.len() == 5 && t.as_bytes()[2] == b':', "{t}");
        let week = pretty_time(now - 7 * DAY);
        assert!(week.len() == 5 && digits(&week[3..]), "{week}");
        let year = pretty_time(now - 400 * DAY);
        assert_eq!(year.len(), 5, "{year}");
        assert!(digits(&year[3..]), "{year}");
        // Fixed instants, well apart, take the older forms.
        let at = 1_700_000_000;
        assert_eq!(pretty_time_at(at, at + 60).as_bytes()[2], b':');
        assert!(digits(&pretty_time_at(at, at + 5 * 365 * DAY)[3..]));
    }

    #[test]
    fn strftime_reports_empty_and_overflow_as_none() {
        let tm = localtime(0).expect("local time");
        assert_eq!(strftime("%Y", &tm, 64).map(|s| s.len()), Some(4));
        assert_eq!(strftime("", &tm, 64), None);
        assert_eq!(strftime("%Y-%m-%d", &tm, 4), None);
        assert_eq!(strftime("a\0b", &tm, 64), None);
        assert_eq!(strftime("plain", &tm, 64).as_deref(), Some("plain"));
    }
}
