//! The text of forwarding: paths in a paste, shell quoting, sizes, prompts,
//! and which local file a remote may ask to open.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::{FilePaste, UploadDir};

/// The absolute paths a paste of dropped files names, split the way
/// terminals write them: separated by spaces or newlines, with spaces
/// backslash-escaped (Terminal.app) or the path quoted (iTerm2). `None` when
/// any word is not an absolute path or the quoting is broken.
pub fn parse_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
            match c {
                '\\' => word.push(chars.next()?),
                '\'' => loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                },
                '"' => loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        c => word.push(c),
                    }
                },
                c => word.push(c),
            }
        }
        if !word.starts_with('/') {
            return None;
        }
        paths.push(PathBuf::from(word));
    }
    (!paths.is_empty()).then_some(paths)
}

/// `s` in single quotes for a POSIX shell, an embedded quote as `'\''`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A size for a prompt: bytes, or decimal kB, MB, GB with one decimal.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub(super) fn count_files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// `upload 2 files (3.1 MB) to host:~/src? [y/N]`.
pub fn upload_prompt(host: &str, paste: &FilePaste, dir: &UploadDir) -> String {
    format!(
        "upload {} ({}) to {host}:{}? [y/N]",
        count_files(paste.files.len()),
        human_size(paste.total),
        dir.shown
    )
}

/// `path` with the remote's `home` shown as `~`.
pub(super) fn tilde(path: &str, home: Option<&str>) -> String {
    let home = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty());
    match home.and_then(|h| path.strip_prefix(h)) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// `forward host:5173 to localhost:5173? [y/N/always]`.
pub fn forward_prompt(host: &str, port: u16) -> String {
    format!("forward {host}:{port} to localhost:{port}? [y/N/always]")
}

/// Extensions macOS (or a desktop) runs or follows rather than shows.
const RUNNABLE: &[&str] = &[
    "app",
    "command",
    "tool",
    "terminal",
    "workflow",
    "action",
    "pkg",
    "mpkg",
    "scpt",
    "scptd",
    "applescript",
    "jar",
    "webloc",
    "inetloc",
    "fileloc",
    "shortcut",
    "prefpane",
    "desktop",
    "sh",
];

/// Whether a `file:` URL from the remote may be offered for opening here:
/// an existing regular file that is neither executable nor of a kind that
/// runs ([`RUNNABLE`]). Directories go too: on macOS an app is one.
pub(super) fn local_file_to_open(url: &str) -> Result<(), String> {
    let path = crate::open::file_path(url).ok_or("not a file on this machine")?;
    let meta = std::fs::metadata(&path).map_err(|_| "that file does not exist here")?;
    if meta.is_dir() {
        return Err("a directory or bundle is not opened".to_string());
    }
    if meta.permissions().mode() & 0o111 != 0 {
        return Err("an executable file is not opened".to_string());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if RUNNABLE.contains(&ext.as_str()) {
        return Err(format!("a .{ext} file is not opened"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_files_are_offered_for_opening() {
        let dir = tempfile::tempdir().unwrap();
        let url = |p: &std::path::Path| format!("file://{}", p.display());
        let doc = dir.path().join("notes.pdf");
        std::fs::write(&doc, "x").unwrap();
        assert_eq!(local_file_to_open(&url(&doc)), Ok(()));
        let exe = dir.path().join("run");
        std::fs::write(&exe, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            local_file_to_open(&url(&exe))
                .unwrap_err()
                .contains("executable")
        );
        let command = dir.path().join("x.Command");
        std::fs::write(&command, "echo").unwrap();
        assert!(
            local_file_to_open(&url(&command))
                .unwrap_err()
                .contains(".command")
        );
        let app = dir.path().join("Evil.app");
        std::fs::create_dir(&app).unwrap();
        assert!(
            local_file_to_open(&url(&app))
                .unwrap_err()
                .contains("directory")
        );
        assert!(local_file_to_open(&url(dir.path())).is_err());
        assert!(local_file_to_open("file:///no/such/file").is_err());
        assert!(local_file_to_open("file://other.host/a").is_err());
    }

    #[test]
    fn paths_in_the_three_terminal_forms() {
        assert_eq!(
            parse_paths("/a/My\\ File.txt /b/c.png "),
            Some(vec!["/a/My File.txt".into(), "/b/c.png".into()])
        );
        assert_eq!(
            parse_paths("'/a/My File.txt' '/b/it'\\''s'"),
            Some(vec!["/a/My File.txt".into(), "/b/it's".into()])
        );
        assert_eq!(
            parse_paths("/a/x\n/b/y\r\n"),
            Some(vec!["/a/x".into(), "/b/y".into()])
        );
        assert_eq!(
            parse_paths("\"/a/q \\\"x\\\"\""),
            Some(vec!["/a/q \"x\"".into()])
        );
    }

    #[test]
    fn anything_else_is_not_a_path_paste() {
        for text in [
            "",
            "   ",
            "a/b",
            "/a b",
            "~/x",
            "/a 'unterminated",
            "/a\\",
            "hello",
        ] {
            assert_eq!(parse_paths(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_remote_home_shows_as_a_tilde() {
        assert_eq!(tilde("/home/u/src", Some("/home/u")), "~/src");
        assert_eq!(tilde("/home/u", Some("/home/u/")), "~");
        assert_eq!(tilde("/home/user2", Some("/home/u")), "/home/user2");
        assert_eq!(tilde("/srv", None), "/srv");
        assert_eq!(tilde("/srv", Some("/")), "/srv");
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/a/b"), "'/a/b'");
        assert_eq!(shell_quote("/a/it's x"), r"'/a/it'\''s x'");
    }

    #[test]
    fn sizes_have_one_decimal() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 kB");
        assert_eq!(human_size(3_100_000), "3.1 MB");
        assert_eq!(human_size(999_999), "1.0 MB");
        assert_eq!(human_size(104_857_600), "104.9 MB");
        assert_eq!(human_size(2_500_000_000), "2.5 GB");
    }

    #[test]
    fn prompt_texts() {
        let paste = FilePaste {
            files: vec!["/a".into(), "/b".into()],
            total: 3_100_000,
            original: Vec::new(),
        };
        let dir = UploadDir {
            path: "/home/u/src".into(),
            shown: "~/src".into(),
        };
        assert_eq!(
            upload_prompt("host", &paste, &dir),
            "upload 2 files (3.1 MB) to host:~/src? [y/N]"
        );
        let one = FilePaste {
            files: vec!["/a".into()],
            total: 12,
            original: Vec::new(),
        };
        assert_eq!(
            upload_prompt("h", &one, &dir),
            "upload 1 file (12 B) to h:~/src? [y/N]"
        );
        assert_eq!(
            forward_prompt("host", 5173),
            "forward host:5173 to localhost:5173? [y/N/always]"
        );
    }
}
