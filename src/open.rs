//! `open-url` (spec 2026-11-15-ssh-remote §3): the allow-list of what may be
//! opened and the local opener, shared by `grofe open` and a remote client.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What `open-url` answers when no client subscribed to the `open-url`
/// event: `grofe open` then opens the URL itself.
pub const NO_SUBSCRIBER: &str = "open-url: no client is listening for urls";

/// The program that opens a URL instead of `open` / `xdg-open`, for tests
/// and for hosts with another opener.
pub const OPENER_ENV: &str = "GROFE_OPENER";

/// The longest URL that is opened, in bytes.
pub const MAX_URL: usize = 4096;

/// Whether `url` may be opened: `http`, `https` or `file`, nothing with a
/// control character in it. The error names the reason.
pub fn check(url: &str) -> Result<(), String> {
    if url.len() > MAX_URL {
        return Err(format!("a url is at most {MAX_URL} bytes"));
    }
    if url.chars().any(char::is_control) {
        return Err("a url cannot hold control characters".to_string());
    }
    match url.split_once(':') {
        Some((scheme, rest))
            if !rest.is_empty()
                && ["http", "https", "file"]
                    .iter()
                    .any(|s| scheme.eq_ignore_ascii_case(s)) =>
        {
            Ok(())
        }
        _ => Err("only http, https and file urls are opened".to_string()),
    }
}

/// The path a `file:` URL names, percent-decoded; `None` for another
/// scheme or a host other than this one.
pub fn file_path(url: &str) -> Option<PathBuf> {
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).and_then(|b| hex(*b)),
            bytes.get(i + 2).and_then(|b| hex(*b)),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

/// Opens `url` with this machine's opener, once [`check`] passes (and, for
/// a `file:` URL, once the path exists here).
pub fn open_locally(url: &str) -> Result<(), String> {
    check(url)?;
    if url
        .split_once(':')
        .is_some_and(|(s, _)| s.eq_ignore_ascii_case("file"))
        && !file_path(url).is_some_and(|p| p.exists())
    {
        return Err("that file does not exist here".to_string());
    }
    let program = std::env::var(OPENER_ENV)
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            }
            .to_string()
        });
    let status = Command::new(&program)
        .arg(url)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_https_and_file_pass() {
        for ok in ["http://a/b", "https://a", "HTTPS://A", "file:///etc/hosts"] {
            assert_eq!(check(ok), Ok(()), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "ftp://a",
            "ssh://a",
            "data:text/html,x",
            "vscode://x",
            "a/b",
            "",
            "http:",
            "https://a\nb",
            "http://a\u{1b}[31m",
        ] {
            assert!(check(bad).is_err(), "{bad:?}");
        }
        let long = format!("https://a/{}", "x".repeat(MAX_URL));
        assert!(check(&long).is_err(), "over-long");
        let ok = format!("https://a/{}", "x".repeat(MAX_URL - 10));
        assert_eq!(check(&ok), Ok(()));
    }

    #[test]
    fn file_urls_name_a_local_path() {
        assert_eq!(file_path("file:///a/b%20c"), Some(PathBuf::from("/a/b c")));
        assert_eq!(file_path("file://localhost/a"), Some(PathBuf::from("/a")));
        assert_eq!(file_path("file:///a%zz"), Some(PathBuf::from("/a%zz")));
        assert_eq!(file_path("file://other.host/a"), None);
        assert_eq!(file_path("https://a/b"), None);
    }
}
