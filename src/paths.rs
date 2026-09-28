//! socket, lock, state and log paths.
//!
//! Spec §4.1: `$XDG_RUNTIME_DIR/grofe/<name>.sock`, falling back to
//! `$TMPDIR/grofe-$UID/<name>.sock` (macOS has no `XDG_RUNTIME_DIR`). The
//! lock is `<sock>.lock`; state (and, from M4, the session snapshot) and the
//! server log live under `$XDG_STATE_HOME/grofe/` (falling back to
//! `~/.local/state/grofe/`), named after the socket so a server on a test
//! `-S` socket never touches the user's real state.
//!
//! Every derivation here is a pure function of an [`Env`] plus the `-L`/`-S`
//! flag values, so tests build an `Env` by hand instead of mutating
//! `std::env` (which every test in the process would otherwise share).
//! [`Env::from_process`] is the one impure entry point, for real callers.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// The env values path derivation reads, gathered up front so the
/// derivation functions themselves stay pure.
#[derive(Debug, Clone)]
pub struct Env {
    pub xdg_runtime_dir: Option<String>,
    pub tmpdir: Option<String>,
    pub xdg_state_home: Option<String>,
    pub home: Option<String>,
    pub uid: u32,
}

impl Env {
    /// Reads the real process environment and effective uid.
    pub fn from_process() -> Env {
        Env {
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
            tmpdir: std::env::var("TMPDIR").ok(),
            xdg_state_home: std::env::var("XDG_STATE_HOME").ok(),
            home: std::env::var("HOME").ok(),
            uid: current_uid(),
        }
    }
}

/// The real effective uid.
fn current_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// An env value, treated as absent when unset *or* empty (XDG basedir
/// convention).
fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.is_empty())
}

/// Where the control/attach socket for instance `name` lives.
///
/// `socket_flag` (`-S <path>`) wins outright, matching tmux; otherwise
/// `name` (`-L <name>`, defaulting to `"default"`) selects a socket under
/// `XDG_RUNTIME_DIR` or its fallback.
pub fn socket_path(env: &Env, socket_flag: Option<&Path>, name: Option<&str>) -> PathBuf {
    if let Some(path) = socket_flag {
        return path.to_path_buf();
    }
    let name = name.unwrap_or("default");
    if let Some(dir) = non_empty(&env.xdg_runtime_dir) {
        return Path::new(dir).join("grofe").join(format!("{name}.sock"));
    }
    let tmpdir = non_empty(&env.tmpdir).unwrap_or("/tmp");
    Path::new(tmpdir)
        .join(format!("grofe-{}", env.uid))
        .join(format!("{name}.sock"))
}

/// The flock path guarding server start-up for `socket` (spec §4.1).
pub fn lock_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// The directory grofe's own state (the server log, and from M4 the
/// session snapshot) lives under.
pub fn state_dir(env: &Env) -> PathBuf {
    if let Some(dir) = non_empty(&env.xdg_state_home) {
        return Path::new(dir).join("grofe");
    }
    let home = non_empty(&env.home).unwrap_or("/tmp");
    Path::new(home).join(".local").join("state").join("grofe")
}

/// The server log for the instance at `socket`, derived from it (spec
/// §4.1) rather than from `name`, so it stays correct whether the instance
/// was selected with `-L` or `-S`.
pub fn log_path(env: &Env, socket: &Path) -> PathBuf {
    state_dir(env).join(format!("{}.log", instance_name(socket)))
}

/// Resolves `socket` to a path that is stable whether or not the socket
/// file itself exists yet. Absolutises it lexically against the current
/// working directory (no filesystem access beyond reading the cwd, for a
/// relative `-S`), then canonicalises its *parent* directory, which always
/// exists by the time a server starts or a client connects even when the
/// socket file does not, and rejoins the file name. That resolves
/// `..`/symlinks in the parent (macOS's `/var` -> `/private/var`, any
/// symlinked home) the same way before and after bind, so the same `-S`
/// string always derives the same instance name. Falls back to the purely
/// lexical path if the parent doesn't exist either.
fn absolute_socket_path(socket: &Path) -> PathBuf {
    let absolute = if socket.is_absolute() {
        socket.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(socket))
            .unwrap_or_else(|_| socket.to_path_buf())
    };
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(file_name)) => std::fs::canonicalize(parent)
            .map(|canon| canon.join(file_name))
            .unwrap_or(absolute),
        _ => absolute,
    }
}

/// Whether `a` and `b` name the same socket: same canonical parent
/// directory plus file name (see [`absolute_socket_path`]), not merely the
/// same string, so `/var/...` and `/private/var/...` (macOS) compare equal.
pub fn same_socket(a: &Path, b: &Path) -> bool {
    absolute_socket_path(a) == absolute_socket_path(b)
}

/// FNV-1a (64-bit): a small hash that is stable across Rust/std versions
/// (unlike `std::hash::DefaultHasher`), used only to disambiguate
/// state/log file names for two sockets that happen to share a file stem.
/// Not used for anything security-sensitive.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// A name for `socket`'s state/log files: its file stem, suffixed with a
/// short stable hash of its absolute path so two `-S` sockets that happen
/// to share a stem (e.g. a per-project `grofe.sock`) never collide, and
/// the same socket always derives the same name.
fn instance_name(socket: &Path) -> String {
    let stem = socket
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default");
    let hash = fnv1a64(absolute_socket_path(socket).as_os_str().as_encoded_bytes());
    format!("{stem}-{:08x}", hash as u32)
}

/// Creates `path` (and any missing parents) with permissions restricted to
/// the owner, matching spec §4.1's `0700` directories.
///
/// `DirBuilder::create` with `recursive(true)` is a no-op if `path` already
/// exists, so it alone would silently accept a directory (or symlink)
/// another user pre-created in a shared `/tmp` fallback, loose or with the
/// wrong owner (the tmux CVE-2012-1180 class of attack). So after
/// creating it, this `lstat`s `path` itself, without following a trailing
/// symlink, and rejects it unless it is a plain directory, owned by us,
/// with no group/other permission bits set.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;

    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{} is a symlink, refusing to use it",
            path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    let uid = current_uid();
    if meta.uid() != uid {
        return Err(io::Error::other(format!(
            "{} is owned by uid {}, not us (uid {uid})",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} has group/other permissions ({:o}), refusing to use it",
            path.display(),
            meta.mode() & 0o777
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            xdg_runtime_dir: None,
            tmpdir: None,
            xdg_state_home: None,
            home: None,
            uid: 501,
        }
    }

    #[test]
    fn socket_path_honours_xdg_runtime_dir() {
        let e = Env {
            xdg_runtime_dir: Some("/run/user/501".to_string()),
            ..env()
        };
        assert_eq!(
            socket_path(&e, None, None),
            PathBuf::from("/run/user/501/grofe/default.sock")
        );
    }

    #[test]
    fn socket_path_uses_name_flag_under_xdg_runtime_dir() {
        let e = Env {
            xdg_runtime_dir: Some("/run/user/501".to_string()),
            ..env()
        };
        assert_eq!(
            socket_path(&e, None, Some("work")),
            PathBuf::from("/run/user/501/grofe/work.sock")
        );
    }

    #[test]
    fn socket_path_falls_back_to_tmpdir_uid_without_xdg_runtime_dir() {
        // The macOS case: no XDG_RUNTIME_DIR, but TMPDIR is always set.
        let e = Env {
            tmpdir: Some("/var/folders/xy/T".to_string()),
            ..env()
        };
        assert_eq!(
            socket_path(&e, None, None),
            PathBuf::from("/var/folders/xy/T/grofe-501/default.sock")
        );
    }

    #[test]
    fn socket_path_falls_back_to_tmp_without_tmpdir_either() {
        let e = env();
        assert_eq!(
            socket_path(&e, None, None),
            PathBuf::from("/tmp/grofe-501/default.sock")
        );
    }

    #[test]
    fn socket_path_treats_empty_xdg_runtime_dir_as_unset() {
        let e = Env {
            xdg_runtime_dir: Some(String::new()),
            tmpdir: Some("/tmp/x".to_string()),
            ..env()
        };
        assert_eq!(
            socket_path(&e, None, None),
            PathBuf::from("/tmp/x/grofe-501/default.sock")
        );
    }

    #[test]
    fn explicit_socket_flag_wins_over_xdg_runtime_dir_and_name() {
        let e = Env {
            xdg_runtime_dir: Some("/run/user/501".to_string()),
            ..env()
        };
        let explicit = Path::new("/custom/path.sock");
        assert_eq!(socket_path(&e, Some(explicit), Some("work")), explicit);
    }

    #[test]
    fn lock_path_appends_dot_lock() {
        let socket = Path::new("/run/user/501/grofe/default.sock");
        assert_eq!(
            lock_path(socket),
            PathBuf::from("/run/user/501/grofe/default.sock.lock")
        );
    }

    #[test]
    fn state_dir_honours_xdg_state_home() {
        let e = Env {
            xdg_state_home: Some("/home/u/.state".to_string()),
            ..env()
        };
        assert_eq!(state_dir(&e), PathBuf::from("/home/u/.state/grofe"));
    }

    #[test]
    fn state_dir_falls_back_to_home_local_state() {
        let e = Env {
            home: Some("/home/u".to_string()),
            ..env()
        };
        assert_eq!(state_dir(&e), PathBuf::from("/home/u/.local/state/grofe"));
    }

    #[test]
    fn log_path_is_derived_from_the_socket_not_the_name() {
        let e = Env {
            xdg_state_home: Some("/home/u/.state".to_string()),
            ..env()
        };
        let socket = Path::new("/custom/other.sock");
        let path = log_path(&e, socket);
        assert_eq!(path.parent(), Some(Path::new("/home/u/.state/grofe")));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("log"));
        assert!(
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .starts_with("other-"),
            "expected a stem-derived name, got {path:?}"
        );
    }

    #[test]
    fn log_path_differs_for_two_sockets_sharing_a_stem_in_different_dirs() {
        let e = Env {
            xdg_state_home: Some("/home/u/.state".to_string()),
            ..env()
        };
        let a = log_path(&e, Path::new("/proj-a/grofe.sock"));
        let b = log_path(&e, Path::new("/proj-b/grofe.sock"));
        assert_ne!(a, b);
        for path in [&a, &b] {
            assert!(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .starts_with("grofe-"),
                "expected a stem-derived name, got {path:?}"
            );
        }
    }

    #[test]
    fn log_path_is_deterministic_for_the_same_socket() {
        let e = Env {
            xdg_state_home: Some("/home/u/.state".to_string()),
            ..env()
        };
        let socket = Path::new("/proj-a/grofe.sock");
        assert_eq!(log_path(&e, socket), log_path(&e, socket));
    }

    #[test]
    fn log_path_is_stable_across_a_symlinked_parent_before_and_after_the_socket_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let e = Env {
            xdg_state_home: Some(tmp.path().join("state").display().to_string()),
            ..env()
        };
        let socket = link.join("test.sock");

        let before = log_path(&e, &socket);
        std::fs::File::create(&socket).unwrap();
        let after = log_path(&e, &socket);

        assert_eq!(
            before, after,
            "log_path must not depend on the socket file itself existing"
        );
    }

    #[test]
    fn log_path_for_test_socket_never_touches_real_state_dir() {
        // Two different `-S` sockets under two different (test) state dirs
        // never collide, which is the property spec §4.1 asks for.
        let a = Env {
            xdg_state_home: Some("/tmp/test-a/.state".to_string()),
            ..env()
        };
        let b = Env {
            xdg_state_home: Some("/tmp/test-b/.state".to_string()),
            ..env()
        };
        let socket = Path::new("/tmp/test-a/run/default.sock");
        assert_ne!(log_path(&a, socket), log_path(&b, socket));
    }

    #[test]
    fn same_socket_matches_an_identical_path() {
        let socket = Path::new("/run/user/501/grofe/default.sock");
        assert!(same_socket(socket, socket));
    }

    #[test]
    fn same_socket_matches_via_a_symlinked_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let via_link = link.join("default.sock");
        let via_real = real_dir.join("default.sock");
        assert!(same_socket(&via_link, &via_real));
    }

    #[test]
    fn same_socket_rejects_a_different_socket() {
        let a = Path::new("/run/user/501/grofe/default.sock");
        let b = Path::new("/run/user/501/grofe/other.sock");
        assert!(!same_socket(a, b));
    }

    #[test]
    fn ensure_private_dir_creates_0700_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("grofe").join("run");

        ensure_private_dir(&target).unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        // Calling it again on an existing directory is fine.
        ensure_private_dir(&target).unwrap();
    }

    #[test]
    fn ensure_private_dir_rejects_existing_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("loose");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(ensure_private_dir(&target).is_err());
    }

    #[test]
    fn ensure_private_dir_rejects_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        assert!(ensure_private_dir(&link).is_err());
    }
}
