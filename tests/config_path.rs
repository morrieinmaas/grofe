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
    write(&file, "status.left = \"A\"\n");
    start_with(&srv, &file);
    assert_eq!(status_left(&srv), "A");
}

#[test]
fn t2_flag_beats_env() {
    let srv = TestServer::unstarted();
    let b = custom_dir(&srv, "b").join("b.toml");
    let c = custom_dir(&srv, "c").join("c.toml");
    write(&b, "status.left = \"B\"\n");
    write(&c, "status.left = \"C\"\n");
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
    write(&b, "status.left = \"B\"\n");
    write(
        &srv.config_home.join("grofe").join("config.toml"),
        "status.left = \"X\"\n",
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
        "status.right = \"D\"\n",
    );
    write(
        &srv.config_home.join("grofe").join("conf.d").join("20.toml"),
        "status.right = \"WRONG\"\n",
    );
    start_with(&srv, &dir.join("config.toml"));
    assert_eq!(fmt(&srv, "#{status-right}"), "D");
}

#[test]
fn t3b_user_toml_comes_from_the_custom_folder() {
    let srv = TestServer::unstarted();
    let dir = custom_dir(&srv, "cust");
    write(&dir.join("config.toml"), "");
    write(&dir.join("user.toml"), "status.left = \"U\"\n");
    write(
        &srv.config_home.join("grofe").join("user.toml"),
        "status.left = \"WRONG\"\n",
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
    write(&file, "status.left = \"one\"\n");
    start_with(&srv, &file);
    assert_eq!(status_left(&srv), "one");

    write(&file, "status.left = \"two\"\n");
    // The reload client carries no --config: the server remembers its file.
    let out = run(&srv, &["reload"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "two");
}

#[test]
fn t7_show_config_explain_names_the_custom_file() {
    let srv = TestServer::unstarted();
    let file = custom_dir(&srv, "x").join("a.toml");
    write(&file, "status.left = \"A\"\n");
    let f = file.to_str().expect("utf8");
    // The file is under HOME, so `show-config` writes it with `~`.
    let shown = format!(
        "~/{}",
        file.strip_prefix(&srv.home).expect("under home").display()
    );

    let out = run(
        &srv,
        &["--config", f, "show-config", "--explain", "status.left"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.contains(&shown), "flag: {out:?}");

    let out = run_env(
        &srv,
        Some(&file),
        &["show-config", "--explain", "status.left"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.contains(&shown), "env: {out:?}");
}

#[test]
fn t8_client_with_another_config_gets_a_notice() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    let b = custom_dir(&srv, "b").join("b.toml");
    write(&a, "status.left = \"A\"\n");
    write(&b, "status.left = \"B\"\n");
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
    write(&file, "status.left = \"ü ok\"\n");
    write(
        &file
            .parent()
            .expect("parent")
            .join("conf.d")
            .join("10.toml"),
        "status.right = \"ö\"\n",
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
        "status.left = \"R\"\n",
    );
    // The harness runs the client in `home`.
    let out = run(&srv, &["--config", "cfg/a.toml", "new-session", "-d"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "R");
}

#[test]
fn t5c_tilde_flag_resolves_against_home() {
    let srv = TestServer::unstarted();
    write(&srv.home.join("t.toml"), "status.left = \"T\"\n");
    let out = run(&srv, &["--config", "~/t.toml", "new-session", "-d"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(status_left(&srv), "T");
}

fn uses(out: &CmdOutput, file: &Path) -> bool {
    out.stderr
        .contains(&format!("grofe: server uses {}", file.display()))
}

#[test]
fn t8b_the_notice_comes_with_attach_and_new_session_too() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    let b = custom_dir(&srv, "b").join("b.toml");
    start_with(&srv, &a);
    let b = b.to_str().expect("utf8");
    // No terminal here, so these fail after the notice, which is the point.
    for args in [["attach-session", "-t", "0"], ["new-session", "-s", "n"]] {
        let mut full = vec!["--config", b];
        full.extend(args);
        let out = run(&srv, &full);
        assert!(uses(&out, &a), "{args:?}: {out:?}");
    }
}

#[test]
fn t8c_a_server_without_sessions_still_answers_the_notice() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    let b = custom_dir(&srv, "b").join("b.toml");
    let out = run(
        &srv,
        &["--config", a.to_str().expect("utf8"), "start-server"],
    );
    assert!(out.status.success(), "{out:?}");
    let out = run(
        &srv,
        &["--config", b.to_str().expect("utf8"), "list-sessions"],
    );
    assert!(uses(&out, &a), "{out:?}");
}

#[test]
fn t8d_only_the_flag_warns_not_the_variable() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    let b = custom_dir(&srv, "b").join("b.toml");
    start_with(&srv, &a);
    let out = run_env(&srv, Some(&b), &["list-sessions"]);
    assert!(!out.stderr.contains("server uses"), "{out:?}");
}

#[test]
fn t8e_the_same_file_by_another_spelling_is_no_notice() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    write(&a, "");
    start_with(&srv, &a);
    let dotted = custom_dir(&srv, "a").join("..").join("a").join("a.toml");
    let out = run(
        &srv,
        &["--config", dotted.to_str().expect("utf8"), "list-sessions"],
    );
    assert!(!out.stderr.contains("server uses"), "{out:?}");
}

#[test]
fn t8f_under_tmux_f_the_notice_names_the_loaded_file() {
    let srv = TestServer::unstarted();
    let f = custom_dir(&srv, "f").join("f.toml");
    let b = custom_dir(&srv, "b").join("b.toml");
    write(&f, "status.left = \"F\"\n");
    let bin = srv.home.join("bin");
    fs::create_dir_all(&bin).expect("mkdir");
    let link = bin.join("tmux");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), &link).expect("link");
    let mut cmd = Command::new(&link);
    cmd.current_dir(&srv.home)
        .env_remove("GROFE_CONFIG")
        .env_remove("TMUX")
        .env("HOME", &srv.home)
        .env("XDG_CONFIG_HOME", &srv.config_home)
        .env("XDG_STATE_HOME", &srv.state_home)
        .env("XDG_RUNTIME_DIR", &srv.runtime_dir)
        .env("TMUX_TMPDIR", srv.home.join("tmuxtmp"))
        .env("GROFE_RESTORE_ON_START", "0")
        .args(["-S"])
        .arg(&srv.socket)
        .args(["-f"])
        .arg(&f)
        .args(["new-session", "-d"]);
    std::fs::create_dir_all(srv.home.join("tmuxtmp")).expect("mkdir");
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    let out = run(
        &srv,
        &["--config", b.to_str().expect("utf8"), "list-sessions"],
    );
    assert!(uses(&out, &f), "{out:?}");
}

#[test]
fn t9_layouts_come_from_the_custom_folder() {
    let srv = TestServer::unstarted();
    let dir = custom_dir(&srv, "cust");
    write(&dir.join("a.toml"), "");
    write(&dir.join("layouts").join("xlayout.toml"), "");
    let out = run(
        &srv,
        &[
            "--config",
            dir.join("a.toml").to_str().expect("utf8"),
            "layout",
            "list",
        ],
    );
    assert!(out.stdout.contains("xlayout"), "{out:?}");
}

#[test]
fn t10_the_flag_does_not_reach_the_panes() {
    let srv = TestServer::unstarted();
    let a = custom_dir(&srv, "a").join("a.toml");
    write(&a, "");
    let dump = srv.home.join("pane.env");
    let out = run(
        &srv,
        &[
            "--config",
            a.to_str().expect("utf8"),
            "new-session",
            "-d",
            &format!("env > {}", dump.display()),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    for _ in 0..100 {
        if fs::read_to_string(&dump).is_ok_and(|t| t.contains("HOME=")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let env = fs::read_to_string(&dump).expect("the pane ran");
    assert!(!env.contains("GROFE_CONFIG"), "{env}");
}
