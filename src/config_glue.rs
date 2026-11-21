//! `--config` and `$GROFE_CONFIG` on the binary's side: resolving the file
//! once, and telling the user when the running server loaded another one.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use grofe::config;
use grofe::protocol::{ClientCtx, Dialect, Hello, Reply, encode_hello};

use super::request_with;

/// The `--config` file as an absolute path; unset without the flag.
static FLAG: OnceLock<PathBuf> = OnceLock::new();

/// Resolves `--config` (else `$GROFE_CONFIG`) to an absolute path and records
/// it for this process's config reads. It is not exported as a variable: the
/// environment this CLI forwards would carry it into every pane. A server
/// this CLI starts gets it as an argument ([`server_args`]).
pub fn apply(flag: Option<&str>) -> Result<(), String> {
    let path = match flag {
        Some(arg) => {
            let path = config::resolve_from_process(arg)?;
            if path.is_dir() {
                return Err(format!("--config: {} is a directory", path.display()));
            }
            path
        }
        None => match config::env_config_checked()? {
            Some(path) => path,
            None => return Ok(()),
        },
    };
    if flag.is_some() {
        let _ = FLAG.set(path.clone());
    }
    config::set_process_config(path);
    Ok(())
}

/// The arguments that give a server this CLI starts its config file.
pub fn server_args() -> Vec<std::ffi::OsString> {
    match config::process_config() {
        Some(file) => vec!["--config".into(), file.into()],
        None => Vec::new(),
    }
}

/// Warns about a server on `socket` running other config than `--config`,
/// except for the verbs that never need it.
pub fn warn_for(verb: &str, socket: &Path) {
    if !matches!(verb, "__server" | "show-config" | "import-tmux" | "pack") {
        warn_if_other(socket);
    }
}

/// How long the server gets to answer [`warn_if_other`].
const TIMEOUT: Duration = Duration::from_secs(5);

/// Tells the user when the server running on `socket` loaded other config
/// files than the `--config` this CLI was given. Never starts a server and
/// never fails the command.
pub fn warn_if_other(socket: &Path) {
    let Some(mine) = FLAG.get() else {
        return;
    };
    let Ok(stream) = UnixStream::connect(socket) else {
        return;
    };
    let Some(theirs) = ask_files(stream) else {
        return;
    };
    if !theirs.iter().any(|f| same_file(f, mine)) {
        let named: Vec<_> = theirs.iter().map(|f| f.display().to_string()).collect();
        let named = if named.is_empty() {
            "no config file".to_string()
        } else {
            named.join(", ")
        };
        eprintln!("grofe: server uses {named}");
    }
}

/// The folder of the config file the server on `socket` loaded, or `None`
/// when no server answers or it loaded none.
pub fn server_config_dir(socket: &Path) -> Option<PathBuf> {
    let stream = UnixStream::connect(socket).ok()?;
    let files = ask_files(stream)?;
    files.first()?.parent().map(Path::to_path_buf)
}

/// The files the server loaded, asked in grofe's dialect whatever name this
/// CLI runs as.
fn ask_files(stream: UnixStream) -> Option<Vec<PathBuf>> {
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;
    let ctx = ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Grofe,
        request: Default::default(),
    };
    let hello = encode_hello(&Hello::Control(ctx)).ok()?;
    let _done = super::HangUp(stream.try_clone().ok());
    match request_with(stream, &hello, &["__config-file".to_string()]) {
        Ok(Reply::Ok { out, .. }) => Some(out.lines().map(PathBuf::from).collect()),
        _ => None,
    }
}

/// Whether two paths name one file: compared resolved when both exist
/// (`/tmp` is `/private/tmp` on macOS), else as given.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => normalize(a) == normalize(b),
    }
}

/// `path` with `.` and `..` resolved lexically, for files that do not exist.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_compare_after_lexical_normalising() {
        assert!(same_file(
            Path::new("/nonexistent-grofe/a/../a/x.toml"),
            Path::new("/nonexistent-grofe/a/x.toml")
        ));
        assert!(same_file(
            Path::new("nonexistent-grofe/a/../a/x.toml"),
            Path::new("nonexistent-grofe/a/x.toml")
        ));
        assert!(!same_file(
            Path::new("/nonexistent-grofe/a/x.toml"),
            Path::new("/nonexistent-grofe/b/x.toml")
        ));
    }
}
