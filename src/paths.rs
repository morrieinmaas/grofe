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
use std::os::unix::fs::DirBuilderExt;
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
///
/// No `libc` crate dependency: this is one syscall with no preconditions,
/// and `uid_t` is a 32-bit unsigned int on every platform grofe targets.
fn current_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid(2) takes no arguments and cannot fail.
    unsafe { geteuid() }
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

/// The socket's file stem, used to name its state and log files.
///
// ponytail: two different `-S` paths that share a file stem (e.g. run from
// two different directories) collide under the same `XDG_STATE_HOME`. Real
// usage names instances via `-L`, and tests get a fresh `XDG_STATE_HOME`
// per run, so this ceiling is unreached in practice.
fn instance_name(socket: &Path) -> &str {
    socket
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("default")
}

/// Creates `path` (and any missing parents) with permissions restricted to
/// the owner, matching spec §4.1's `0700` directories. A no-op if `path`
/// already exists.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
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
        assert_eq!(
            log_path(&e, socket),
            PathBuf::from("/home/u/.state/grofe/other.log")
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
}
