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

/// The `BROWSER` a pane had before `forward-browser` set it to the
/// `grofe-open` shim: `grofe open` runs it when no client opens the URL.
pub const PREV_BROWSER_ENV: &str = "GROFE_PREV_BROWSER";

/// Set in what an opener runs: a `grofe open` that finds it is a loop.
pub const OPENING_ENV: &str = "GROFE_OPENING";

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

/// `arg` as a `file:` URL when it names a path that exists (absolute, or
/// relative to the current directory), as programs pass `$BROWSER` a plain
/// path; `None` for anything else. Bytes outside the unreserved set and
/// `/` are percent-encoded, so [`file_path`] gives the path back.
pub fn path_url(arg: &str) -> Option<String> {
    let path = std::path::Path::new(arg);
    if arg.is_empty() || !path.exists() {
        return None;
    }
    let path = std::path::absolute(path).ok()?;
    let mut url = "file://".to_string();
    for b in path.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(*b as char)
            }
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    Some(url)
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
    open_here(url)
}

/// Opens `url` here with no allow-list: `GROFE_OPENER`, else the user's
/// own browser from before `forward-browser` ([`PREV_BROWSER_ENV`]), else
/// `open` / `xdg-open`. What it runs has [`OPENING_ENV`] set and no
/// `BROWSER` that names the `grofe-open` shim, so an opener that runs
/// `$BROWSER` (xdg-open with no desktop) cannot loop back into `grofe open`.
pub fn open_here(url: &str) -> Result<(), String> {
    if std::env::var_os(OPENING_ENV).is_some() {
        return Err(
            "not opening: grofe open ran inside an opener (BROWSER names grofe open?)".to_string(),
        );
    }
    let set = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (program, mut cmd) = match (set(OPENER_ENV), set(PREV_BROWSER_ENV)) {
        (Some(p), _) => (p.clone(), Command::new(p)),
        (None, Some(browser)) => {
            // As programs run `$BROWSER`: a command, given the URL.
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(format!("{browser} \"$1\"")).arg("sh");
            (browser, cmd)
        }
        (None, None) => {
            let p = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            (p.to_string(), Command::new(p))
        }
    };
    cmd.arg(url).stdin(Stdio::null()).env(OPENING_ENV, "1");
    match set(PREV_BROWSER_ENV) {
        Some(prev) => cmd.env("BROWSER", prev),
        None if set("BROWSER").is_some_and(|b| names_shim(&b)) => cmd.env_remove("BROWSER"),
        None => &mut cmd,
    };
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {status}"))
    }
}

/// Whether `browser` is the `grofe-open` shim.
fn names_shim(browser: &str) -> bool {
    std::path::Path::new(browser.trim()).file_name()
        == Some(std::ffi::OsStr::new(crate::server::remote::OPEN_SHIM_NAME))
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
    fn an_existing_path_is_a_file_url_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a b%c.html");
        std::fs::write(&file, "x").unwrap();
        let url = path_url(file.to_str().unwrap()).unwrap();
        assert!(
            url.starts_with("file:///") && url.ends_with("/a%20b%25c.html"),
            "{url}"
        );
        assert_eq!(file_path(&url), Some(std::path::absolute(&file).unwrap()));
        assert_eq!(check(&url), Ok(()));
        assert_eq!(path_url("/no/such/file"), None);
        assert_eq!(path_url("https://a/"), None);
        assert_eq!(path_url(""), None);
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
