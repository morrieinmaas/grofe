//! Previewing and writing a batch of settings changes (spec AC3.9): a diff
//! computed on a scratch copy, then the real write through `config::write`.

use std::fs;
use std::path::Path;

use crate::config::write::{self, Saved};

/// One edit to `settings.toml`.
pub enum Change {
    Set {
        key: String,
        value: toml_edit::Item,
    },
    Unset {
        key: String,
    },
    Bind {
        table: String,
        key: String,
        cmd: String,
    },
}

fn apply(dir: &Path, changes: &[Change]) -> Result<Vec<Saved>, String> {
    let mut saved = Vec::new();
    for c in changes {
        let r = match c {
            Change::Set { key, value } => write::set(dir, key, value.clone()),
            Change::Unset { key } => write::unset(dir, key),
            Change::Bind { table, key, cmd } => write::set_binding(dir, table, key, cmd),
        };
        match r {
            Ok(s) => saved.push(s),
            Err(e) => {
                for s in saved.into_iter().rev() {
                    let _ = s.rollback();
                }
                return Err(e);
            }
        }
    }
    Ok(saved)
}

/// Writes `changes`; the first error rolls back everything already written.
pub fn write(dir: &Path, changes: &[Change]) -> Result<Vec<Saved>, String> {
    apply(dir, changes)
}

/// `-old` / `+new` lines of `settings.toml` before and after `changes`,
/// computed in a temp dir so the real file is untouched.
// ponytail: a line-set diff, not an LCS one; a line moved without change in
// content shows nothing, and duplicate lines are matched by count only.
pub fn diff(dir: &Path, changes: &[Change]) -> Result<String, String> {
    let scratch = tempfile_dir()?;
    let result = (|| {
        let file = scratch.join(write::FILE);
        let before = match fs::read_to_string(dir.join(write::FILE)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", write::FILE)),
        };
        if !before.is_empty() {
            fs::write(&file, &before).map_err(|e| e.to_string())?;
        }
        apply(&scratch, changes)?;
        let after = fs::read_to_string(&file).unwrap_or_default();
        Ok(line_diff(&before, &after))
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn tempfile_dir() -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = std::env::temp_dir().join(format!(
        "grofe-diff-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    Ok(p)
}

fn line_diff(before: &str, after: &str) -> String {
    let mut out = String::new();
    let mut remaining: Vec<&str> = after.lines().collect();
    for l in before.lines() {
        match remaining.iter().position(|a| *a == l) {
            Some(i) => {
                remaining.remove(i);
            }
            None => out.push_str(&format!("-{l}\n")),
        }
    }
    for l in remaining {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(key: &str, v: i64) -> Change {
        Change::Set {
            key: key.into(),
            value: toml_edit::value(v),
        }
    }

    #[test]
    fn diff_shows_one_plus_line_and_leaves_the_file_alone() {
        let d = tempfile_dir().unwrap();
        let f = d.join(write::FILE);
        fs::write(&f, "# mine\nhistory-limit = 1\n").unwrap();
        let out = diff(&d, &[set("scrollback", 5)]).unwrap();
        assert_eq!(out.lines().filter(|l| l.starts_with('+')).count(), 1);
        assert!(out.contains("+scrollback = 5"), "{out}");
        assert_eq!(fs::read(&f).unwrap(), b"# mine\nhistory-limit = 1\n");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn write_then_rollback_restores_bytes() {
        let d = tempfile_dir().unwrap();
        let f = d.join(write::FILE);
        fs::write(&f, "a = 1\n").unwrap();
        let saved = write(&d, &[set("b", 2), set("c", 3)]).unwrap();
        assert_ne!(fs::read(&f).unwrap(), b"a = 1\n");
        for s in saved.into_iter().rev() {
            s.rollback().unwrap();
        }
        assert_eq!(fs::read(&f).unwrap(), b"a = 1\n");
        let _ = fs::remove_dir_all(&d);
    }
}
