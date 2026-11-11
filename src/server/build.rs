//! The server's own binary as it was when it started, so an upgrade made
//! while it runs (a new grofe installed over the old one) can be told to
//! the clients that attach: until the server restarts they keep getting
//! the old build.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

/// What the warning on attach says.
pub const STALE: &str =
    "this server runs an old grofe build; restart it (kill-server) to use the new one";

/// The binary's path and what identified the file there at start.
#[derive(Debug, Clone)]
pub struct Binary {
    path: PathBuf,
    stamp: Stamp,
}

type Stamp = (u64, u64, u64, i64, i64);

fn stamp(path: &std::path::Path) -> Option<Stamp> {
    let m = fs::metadata(path).ok()?;
    Some((m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

impl Binary {
    /// This process's binary now; `None` when it cannot be found.
    pub fn at_start() -> Option<Binary> {
        let path = std::env::current_exe().ok()?;
        Binary::at(path)
    }

    fn at(path: PathBuf) -> Option<Binary> {
        let stamp = stamp(&path)?;
        Some(Binary { path, stamp })
    }

    /// Whether the file at the start path is no longer the one that was
    /// there. A binary removed without a replacement is not an upgrade.
    pub fn replaced(&self) -> bool {
        stamp(&self.path).is_some_and(|now| now != self.stamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_renamed_over_counts_as_replaced_and_a_removed_one_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("grofe");
        fs::write(&exe, "old").expect("write");
        let bin = Binary::at(exe.clone()).expect("stamp");
        assert!(!bin.replaced());

        let next = dir.path().join("grofe.new");
        fs::write(&next, "new build").expect("write");
        fs::rename(&next, &exe).expect("rename");
        assert!(bin.replaced());

        fs::remove_file(&exe).expect("remove");
        assert!(!bin.replaced());
    }
}
