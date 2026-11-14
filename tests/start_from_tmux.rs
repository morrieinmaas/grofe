//! `start-from-tmux`: a grofe server that starts empty takes the sessions
//! of the running tmux server: names, windows, layout and directories,
//! with shells (the programs keep running in tmux).
//!
//! The tmux here is a private one: its own `TMUX_TMPDIR`, which the grofe
//! server inherits, so neither can reach a tmux the user runs. Its own
//! file, as it sets `TMUX_TMPDIR` for the whole test process.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use common::TestServer;

/// Kills the private tmux server when the test ends, however it ends.
struct PrivateTmux<'a>(&'a Path);

impl PrivateTmux<'_> {
    fn run(&self, args: &[&str]) {
        let ok = Command::new("tmux")
            .env("TMUX_TMPDIR", self.0)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .args(args)
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "tmux {args:?}");
    }
}

impl Drop for PrivateTmux<'_> {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .env("TMUX_TMPDIR", self.0)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .args(["kill-server"])
            .stderr(Stdio::null())
            .status();
    }
}

/// The first `PATH` directory whose `tmux` is not a link to a grofe binary.
fn real_tmux_dir() -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find(|dir| {
        fs::canonicalize(dir.join("tmux"))
            .is_ok_and(|exe| exe.file_name().is_some_and(|n| n != "grofe"))
    })
}

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

#[test]
fn an_empty_server_starts_with_the_running_tmux_sessions() {
    // A real tmux, never a `tmux` link to grofe: on a machine where grofe
    // answers to `tmux` (and in every grofe pane), the cleanup's
    // `kill-server` would otherwise stop the user's own grofe server.
    let Some(dir) = real_tmux_dir() else {
        eprintln!("skipping: no real tmux on PATH");
        return;
    };
    let path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: this file's only test; nothing else reads the environment.
    unsafe { std::env::set_var("PATH", format!("{}:{path}", dir.display())) };
    let tmpdir = tempfile::Builder::new()
        .prefix("gt")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let work = tempfile::tempdir().expect("work dir");
    let (a, b) = (work.path().join("a"), work.path().join("b"));
    fs::create_dir_all(&a).expect("mkdir");
    fs::create_dir_all(&b).expect("mkdir");
    let (a, b) = (a.to_str().expect("utf8"), b.to_str().expect("utf8"));
    let tmux = PrivateTmux(tmpdir.path());
    tmux.run(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-c",
        a,
        "-x",
        "100",
        "-y",
        "30",
    ]);
    tmux.run(&["split-window", "-h", "-t", "alpha", "-c", b]);
    tmux.run(&["new-window", "-d", "-t", "alpha", "-n", "second", "-c", b]);
    tmux.run(&["new-session", "-d", "-s", "beta", "-c", a]);

    // SAFETY: this file's only test; nothing else reads the environment.
    unsafe { std::env::set_var("TMUX_TMPDIR", tmpdir.path()) };
    // Any other server (`-L preview`) starts from its own snapshot only;
    // tmux's sessions go to the default one.
    let other = TestServer::builder().restore_on_start(true).unstarted();
    fs::write(
        other.config_home.join("grofe").join("config.toml"),
        "start-from-tmux = true\n",
    )
    .expect("write config");
    ok(&other, &["start-server"]);
    assert_eq!(
        other.cmd(&["list-sessions"]).unwrap_or_default(),
        "",
        "server log:\n{}",
        other.log()
    );

    let mut srv = TestServer::builder().restore_on_start(true).unstarted();
    srv.socket = srv.runtime_dir.join("default.sock");
    fs::write(
        srv.config_home.join("grofe").join("config.toml"),
        "start-from-tmux = true\n",
    )
    .expect("write config");
    ok(&srv, &["start-server"]);

    let sessions = ok(
        &srv,
        &["list-sessions", "-F", "#{session_name} #{session_windows}"],
    );
    let log = fs::read_to_string(srv.log_path()).unwrap_or_default();
    assert_eq!(sessions, "alpha 2\nbeta 1\n", "server log:\n{log}");
    let windows = ok(
        &srv,
        &["list-windows", "-t", "alpha", "-F", "#{window_name}"],
    );
    assert!(windows.ends_with("second\n"), "{windows}");
    let panes = ok(
        &srv,
        &[
            "list-panes",
            "-t",
            "alpha:0",
            "-F",
            "#{pane_left} #{pane_start_path}#{pane_current_path}",
        ],
    );
    let lines: Vec<&str> = panes.lines().collect();
    assert_eq!(lines.len(), 2, "{panes}");
    assert!(
        lines[0].starts_with("0 ") && lines[0].contains(a),
        "{panes}"
    );
    assert!(
        !lines[1].starts_with("0 ") && lines[1].contains(b),
        "{panes}"
    );
}
