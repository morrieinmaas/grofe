//! Identity and entry (TC.1). Until TC.1 and FE.1 land, the `argv[0]`
//! hand-off is a stub: grofe run through a `tmux` symlink rejects every
//! tmux form with one clear error and starts no server, and grofe under any
//! other name behaves as before. TC.1 replaces these tests with its own.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

mod common;

use common::run_bounded;

const TIMEOUT: Duration = Duration::from_secs(10);

/// `name` symlinked to the grofe binary in `dir`, run with an environment
/// that points every grofe directory into `dir`.
fn linked(dir: &Path, name: &str) -> Command {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join(name);
    if !link.exists() {
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let mut cmd = Command::new(link);
    for var in ["TMUX", "TMUX_PANE", "GROFE", "GROFE_PANE", "TMUX_TMPDIR"] {
        cmd.env_remove(var);
    }
    cmd.current_dir(dir)
        .env("HOME", dir)
        .env("TMPDIR", dir.join("tmp"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("GROFE_RESTORE_ON_START", "0");
    cmd
}

#[test]
fn tmux_name_rejects_every_form_and_starts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &[][..],
        &["-V"],
        &["ls"],
        &["new-session", "-d", "-s", "x"],
        &["-L", "grofe-stub", "new", "-d"],
        &["list-sessions"],
        &["display-message", "-p", "#{version}"],
    ] {
        let mut cmd = linked(dir.path(), "tmux");
        cmd.args(args);
        let out = run_bounded(cmd, TIMEOUT);
        assert!(!out.status.success(), "{args:?} succeeded");
        assert_eq!(out.stdout, "", "{args:?}");
        assert!(
            out.stderr.contains("tmux front end is not implemented"),
            "{args:?}: {}",
            out.stderr
        );
    }
    for sub in ["run", "state", "tmp"] {
        assert!(
            !dir.path().join(sub).exists(),
            "{sub} was created: something started"
        );
    }
}

#[test]
fn other_names_behave_as_grofe() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["grofe", "tmuxx", "grofe-tmux"] {
        let out = run_bounded(linked(dir.path(), name), TIMEOUT);
        assert!(out.status.success(), "{name}: {}", out.stderr);
        assert!(out.stdout.starts_with("usage: grofe"), "{name}");
    }
}
