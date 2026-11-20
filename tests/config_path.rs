//! `--config <file>` and `GROFE_CONFIG` (spec 2026-11-20-settings-menu, PR 1).
//! Every test uses a private `-S` socket and temp HOME/XDG dirs. T9 (`tmux -f`
//! unchanged) is the existing `tests/tmux_conf_f.rs` suite.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{CMD_TIMEOUT, CmdOutput, TestServer, run_bounded};

/// Runs the CLI with `GROFE_CONFIG` set or cleared, then `args`.
fn run_env(srv: &TestServer, env: Option<&Path>, args: &[&str]) -> CmdOutput {
    let mut cmd: Command = srv.command();
    cmd.env_remove("GROFE_CONFIG");
    if let Some(p) = env {
        cmd.env("GROFE_CONFIG", p);
    }
    cmd.args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

fn run(srv: &TestServer, args: &[&str]) -> CmdOutput {
    run_env(srv, None, args)
}

fn fmt(srv: &TestServer, format: &str) -> String {
    let out = run(srv, &["display-message", "-p", format]);
    assert!(out.status.success(), "display failed: {out:?}");
    out.stdout.trim_end().to_string()
}

fn status_left(srv: &TestServer) -> String {
    fmt(srv, "#{status-left}")
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn custom_dir(srv: &TestServer, name: &str) -> PathBuf {
    srv.home.join(name)
}

/// Starts the server with `--config <file>` and asserts it came up.
fn start_with(srv: &TestServer, file: &Path) {
    let out = run(
        srv,
        &[
            "--config",
            file.to_str().expect("utf8"),
            "new-session",
            "-d",
        ],
    );
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn t1_config_flag_loads_the_file() {
    let srv = TestServer::unstarted();
    let file = custom_dir(&srv, "x").join("a.toml");
    write(&file, "status-left = \"A\"\n");
    start_with(&srv, &file);
    assert_eq!(status_left(&srv), "A");
}

#[test]
fn t2_flag_beats_env() {
    let srv = TestServer::unstarted();
    let b = custom_dir(&srv, "b").join("b.toml");
    let c = custom_dir(&srv, "c").join("c.toml");
    write(&b, "status-left = \"B\"\n");
    write(&c, "status-left = \"C\"\n");
    let out = run_env(
        &srv,
        Some(&b),
        &["--config", c.to_str().expect("utf8"), "new-session", "-d"],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "C");
}

#[test]
fn t2b_env_beats_xdg_default() {
    let srv = TestServer::unstarted();
    let b = custom_dir(&srv, "b").join("b.toml");
    write(&b, "status-left = \"B\"\n");
    write(
        &srv.config_home.join("grofe").join("config.toml"),
        "status-left = \"X\"\n",
    );
    let out = run_env(&srv, Some(&b), &["new-session", "-d"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "B");
}

#[test]
fn t3_conf_d_comes_from_the_custom_folder() {
    let srv = TestServer::unstarted();
    let dir = custom_dir(&srv, "cust");
    write(&dir.join("config.toml"), "");
    write(
        &dir.join("conf.d").join("10.toml"),
        "status-right = \"D\"\n",
    );
    write(
        &srv.config_home.join("grofe").join("conf.d").join("20.toml"),
        "status-right = \"WRONG\"\n",
    );
    start_with(&srv, &dir.join("config.toml"));
    assert_eq!(fmt(&srv, "#{status-right}"), "D");
}

#[test]
fn t3b_user_toml_comes_from_the_custom_folder() {
    let srv = TestServer::unstarted();
    let dir = custom_dir(&srv, "cust");
    write(&dir.join("config.toml"), "");
    write(&dir.join("user.toml"), "status-left = \"U\"\n");
    write(
        &srv.config_home.join("grofe").join("user.toml"),
        "status-left = \"WRONG\"\n",
    );
    start_with(&srv, &dir.join("config.toml"));
    assert_eq!(status_left(&srv), "U");
}

#[test]
fn t4_missing_file_loads_defaults() {
    let baseline = {
        let srv = TestServer::unstarted();
        let out = run(&srv, &["new-session", "-d"]);
        assert!(out.status.success(), "{out:?}");
        status_left(&srv)
    };
    let srv = TestServer::unstarted();
    let f = custom_dir(&srv, "none").join("x.toml");
    start_with(&srv, &f);
    assert_eq!(status_left(&srv), baseline);
}

#[test]
fn t6_reload_rereads_the_custom_file() {
    let srv = TestServer::unstarted();
    let file = custom_dir(&srv, "x").join("a.toml");
    write(&file, "status-left = \"one\"\n");
    start_with(&srv, &file);
    assert_eq!(status_left(&srv), "one");

    write(&file, "status-left = \"two\"\n");
    // The reload client carries no --config: the server remembers its file.
    let out = run(&srv, &["reload"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "two");
}

#[test]
fn t7_show_config_explain_names_the_custom_file() {
    let srv = TestServer::unstarted();
    let file = custom_dir(&srv, "x").join("a.toml");
    write(&file, "status-left = \"A\"\n");
    let f = file.to_str().expect("utf8");

    let out = run(
        &srv,
        &["--config", f, "show-config", "--explain", "status-left"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.contains(f), "flag: {out:?}");

    let out = run_env(
        &srv,
        Some(&file),
        &["show-config", "--explain", "status-left"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.contains(f), "env: {out:?}");
}

#[test]
fn t8_client_with_another_config_gets_a_notice() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    let b = custom_dir(&srv, "b").join("b.toml");
    write(&a, "status-left = \"A\"\n");
    write(&b, "status-left = \"B\"\n");
    start_with(&srv, &a);

    let out = run(
        &srv,
        &["--config", b.to_str().expect("utf8"), "list-sessions"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stderr
            .contains(&format!("grofe: server uses {}", a.display())),
        "{out:?}"
    );
    assert_eq!(status_left(&srv), "A");

    let out = run(
        &srv,
        &["--config", a.to_str().expect("utf8"), "list-sessions"],
    );
    assert!(!out.stderr.contains("server uses"), "{out:?}");
}

#[test]
fn t33_config_naming_a_directory_is_refused() {
    let srv = TestServer::unstarted();
    let dir = custom_dir(&srv, "adir");
    fs::create_dir_all(&dir).expect("mkdir");
    let out = run(
        &srv,
        &["--config", dir.to_str().expect("utf8"), "new-session", "-d"],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        out.stderr.contains(&format!(
            "grofe: --config: {} is a directory",
            dir.display()
        )),
        "{out:?}"
    );
    assert!(!srv.is_running());
}

#[test]
fn t35_path_with_spaces_and_umlaut_loads() {
    let srv = TestServer::unstarted();
    let file = custom_dir(&srv, "my cfg ü").join("kon fig.toml");
    write(&file, "status-left = \"ü ok\"\n");
    write(
        &file
            .parent()
            .expect("parent")
            .join("conf.d")
            .join("10.toml"),
        "status-right = \"ö\"\n",
    );
    start_with(&srv, &file);
    assert_eq!(status_left(&srv), "ü ok");
    assert_eq!(fmt(&srv, "#{status-right}"), "ö");
}

#[test]
fn t5b_relative_flag_resolves_against_client_cwd() {
    let srv = TestServer::unstarted();
    write(
        &srv.home.join("cfg").join("a.toml"),
        "status-left = \"R\"\n",
    );
    // The harness runs the client in `home`.
    let out = run(&srv, &["--config", "cfg/a.toml", "new-session", "-d"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "R");
}

#[test]
fn t5c_tilde_flag_resolves_against_home() {
    let srv = TestServer::unstarted();
    write(&srv.home.join("t.toml"), "status-left = \"T\"\n");
    let out = run(&srv, &["--config", "~/t.toml", "new-session", "-d"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "T");
}
