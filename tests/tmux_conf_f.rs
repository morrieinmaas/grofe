//! `tmux -f <file>` through the `tmux` shim: the file is a tmux.conf, sourced
//! as `source-file` reads it (`%if`, `%hidden`, `{}` blocks) when the call
//! starts the server. `-f /dev/null` and a missing file are no config, and
//! a server that already runs ignores `-f`. A `.toml` stays grofe's.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;

mod common;

use common::{CMD_TIMEOUT, CmdOutput, TestServer, run_bounded};

fn fixture(name: &str) -> String {
    format!(
        "{}/tests/fixtures/tmux-conf/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn tmux(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let root = srv.home.parent().expect("root").to_path_buf();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link: PathBuf = bin.join("tmux");
    if !link.exists() {
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let tmpdir = root.join("tmux-tmpdir");
    fs::create_dir_all(&tmpdir).unwrap();
    let mut cmd = Command::new(link);
    cmd.current_dir(&srv.home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", &srv.home)
        .env("XDG_CONFIG_HOME", &srv.config_home)
        .env("XDG_STATE_HOME", &srv.state_home)
        .env("XDG_RUNTIME_DIR", &srv.runtime_dir)
        .env("TMUX_TMPDIR", tmpdir)
        .env("GROFE_RESTORE_ON_START", "0")
        .env("GROFE_REMAIN_ON_EXIT", "1")
        .arg("-S")
        .arg(&srv.socket)
        .args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

fn ok(srv: &TestServer, args: &[&str]) -> String {
    let out = tmux(srv, args);
    assert!(out.status.success(), "{args:?}: {}", out.stderr);
    out.stdout
}

fn option(srv: &TestServer, name: &str) -> String {
    ok(srv, &["show-options", "-gqv", name]).trim().to_string()
}

fn kill(srv: &TestServer) {
    let _ = tmux(srv, &["kill-server"]);
}

#[test]
fn f_sources_a_tmux_conf_with_options_bindings_and_blocks() {
    let srv = TestServer::unstarted();
    ok(&srv, &["-f", &fixture("f.conf"), "new-session", "-d"]);
    assert_eq!(option(&srv, "history-limit"), "4321");
    assert_eq!(option(&srv, "@marker"), "from-conf");
    assert_eq!(option(&srv, "@hidden"), "after-hidden");
    assert_eq!(option(&srv, "base-index"), "3");
    let keys = ok(&srv, &["list-keys"]);
    let line = |table: &str, key: &str| {
        keys.lines()
            .find(|l| {
                l.split_whitespace()
                    .collect::<Vec<_>>()
                    .starts_with(&["bind-key", "-T", table, key])
            })
            .map(str::to_string)
    };
    let braced = line("prefix", "M").expect("prefix M is bound");
    assert!(
        braced.contains("@braces") && braced.contains("display-message"),
        "{braced}"
    );
    let f5 = line("root", "F5").expect("root F5 is bound");
    assert!(f5.contains("new-window"), "{f5}");
    kill(&srv);
}

#[test]
fn f_given_twice_loads_both_in_order() {
    let srv = TestServer::unstarted();
    let dir = srv.home.join("two");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.conf"), "set -g @a 1\nset -g @both a\n").unwrap();
    fs::write(dir.join("b.conf"), "set -g @b 2\nset -g @both b\n").unwrap();
    let (a, b) = (dir.join("a.conf"), dir.join("b.conf"));
    ok(
        &srv,
        &[
            "-f",
            a.to_str().unwrap(),
            "-f",
            b.to_str().unwrap(),
            "new-session",
            "-d",
        ],
    );
    assert_eq!(option(&srv, "@a"), "1");
    assert_eq!(option(&srv, "@b"), "2");
    assert_eq!(option(&srv, "@both"), "b");
    kill(&srv);
}

#[test]
fn f_dev_null_and_a_missing_file_are_no_config() {
    for file in ["/dev/null", "/nonexistent/tmux.conf"] {
        let srv = TestServer::unstarted();
        ok(&srv, &["-f", file, "new-session", "-d"]);
        assert_eq!(option(&srv, "base-index"), "0", "{file}");
        kill(&srv);
    }
}

#[test]
fn f_is_ignored_when_the_server_already_runs() {
    let srv = TestServer::unstarted();
    ok(&srv, &["new-session", "-d"]);
    ok(&srv, &["-f", &fixture("f.conf"), "new-window", "-d"]);
    assert_eq!(option(&srv, "@marker"), "");
    kill(&srv);
}

#[test]
fn f_a_toml_file_stays_grofes_config() {
    let srv = TestServer::unstarted();
    ok(&srv, &["-f", &fixture("f.toml"), "new-session", "-d"]);
    assert_eq!(option(&srv, "mouse"), "on");
    kill(&srv);
}

#[test]
fn a_conf_error_does_not_stop_the_rest_or_the_server() {
    let srv = TestServer::unstarted();
    let conf = srv.home.join("bad.conf");
    fs::write(&conf, "set -g nosuchoption 1\nset -g @after yes\n").unwrap();
    ok(&srv, &["-f", conf.to_str().unwrap(), "new-session", "-d"]);
    assert_eq!(option(&srv, "@after"), "yes");
    kill(&srv);
}

#[test]
fn native_file_stays_toml_even_for_a_tmux_conf_name() {
    let srv = TestServer::unstarted();
    let toml = srv.home.join("native.toml");
    fs::write(&toml, "mouse = true\n").unwrap();
    // The native path: a server started without the tmux identity.
    let server = srv
        .command()
        .args(["__server", "--file"])
        .arg(&toml)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    common::wait_until(|| srv.socket.exists(), CMD_TIMEOUT).expect("server socket");
    let out = run_bounded(
        {
            let mut c = srv.command();
            c.args(["show-options", "-gv", "mouse"]);
            c
        },
        CMD_TIMEOUT,
    );
    assert_eq!(out.stdout.trim(), "on", "{}", out.stderr);
    let _ = run_bounded(
        {
            let mut c = srv.command();
            c.arg("kill-server");
            c
        },
        CMD_TIMEOUT,
    );
    let _ = common::wait_child(server, CMD_TIMEOUT);
}
