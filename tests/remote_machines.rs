//! `grofe machine add|ls|rm` (spec ssh-remote §5): the saved machines in
//! `<config-dir>/machines.toml`, driven through the real binary with a
//! private `XDG_CONFIG_HOME`. No server is started or contacted.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::TestServer;

fn grofe(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grofe"))
        .arg("-S")
        .arg(config.join("never.sock"))
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .env_remove("TMUX")
        .env_remove("GROFE")
        .output()
        .expect("run grofe")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn add_ls_rm_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let o = grofe(d.path(), &["machine", "add", "box", "dev@box.example"]);
    assert!(o.status.success(), "{}", err(&o));
    let o = grofe(
        d.path(),
        &[
            "machine",
            "add",
            "quick",
            "quick",
            "--session",
            "work",
            "--transport",
            "quic",
        ],
    );
    assert!(o.status.success(), "{}", err(&o));

    let o = grofe(d.path(), &["machine", "ls"]);
    assert!(o.status.success(), "{}", err(&o));
    let ls = out(&o);
    let rows: Vec<Vec<&str>> = ls.lines().map(|l| l.split_whitespace().collect()).collect();
    assert_eq!(
        rows,
        vec![
            vec!["box", "dev@box.example", "-", "-", "on"],
            vec!["quick", "quick", "work", "quic", "on"],
        ],
        "{ls}"
    );

    let file = std::fs::read_to_string(d.path().join("grofe/machines.toml")).unwrap();
    assert!(
        file.find("box").unwrap() < file.find("quick").unwrap(),
        "{file}"
    );

    let o = grofe(d.path(), &["machine", "rm", "box"]);
    assert!(o.status.success(), "{}", err(&o));
    let o = grofe(d.path(), &["machine", "ls"]);
    assert_eq!(out(&o).lines().count(), 1, "{}", out(&o));
    assert!(out(&o).starts_with("quick "), "{}", out(&o));
}

#[test]
fn ls_with_no_file_prints_nothing() {
    let d = tempfile::tempdir().unwrap();
    let o = grofe(d.path(), &["machine", "ls"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "");
}

#[test]
fn duplicate_add_names_the_existing_machine() {
    let d = tempfile::tempdir().unwrap();
    assert!(
        grofe(d.path(), &["machine", "add", "box", "a"])
            .status
            .success()
    );
    let o = grofe(d.path(), &["machine", "add", "box", "b"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        err(&o).contains("box") && err(&o).contains("already"),
        "{}",
        err(&o)
    );
    // The first entry is untouched.
    let ls = out(&grofe(d.path(), &["machine", "ls"]));
    assert!(ls.contains(" a "), "{ls}");
}

#[test]
fn bad_input_exits_1_with_a_message() {
    let d = tempfile::tempdir().unwrap();
    for args in [
        &["machine", "add", "a/b", "t"][..],
        &["machine", "add", "local", "t"][..],
        &["machine", "add", "x", "-oProxyCommand=sh"][..],
        &["machine", "add", "x", "u@-h"][..],
        &["machine", "add", "x", "t", "--transport", "mosh"][..],
        &["machine", "add", "x"][..],
        &["machine", "add", "x", "t", "--session"][..],
        &["machine", "rm", "nope"][..],
    ] {
        let o = grofe(d.path(), args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(
            err(&o).starts_with("grofe machine"),
            "{args:?}: {}",
            err(&o)
        );
    }
    assert!(!d.path().join("grofe/machines.toml").exists());
}

#[test]
fn no_or_unknown_subcommand_prints_usage() {
    let d = tempfile::tempdir().unwrap();
    for args in [&["machine"][..], &["machine", "frob"][..]] {
        let o = grofe(d.path(), args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(err(&o).contains("usage: grofe machine"), "{}", err(&o));
    }
}

#[test]
fn a_broken_file_is_reported_not_overwritten() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("grofe/machines.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "[[machine]]\nname = 1\n").unwrap();
    let o = grofe(d.path(), &["machine", "add", "box", "t"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("machines.toml"), "{}", err(&o));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "[[machine]]\nname = 1\n"
    );
}

/// `machine-key` (the client's picker key) is a global key option, `M-m`
/// by default, and takes another key.
#[test]
fn machine_key_option() {
    let srv = TestServer::start();
    assert_eq!(
        srv.cmd(&["show-options", "-gv", "machine-key"]).unwrap(),
        "M-m\n"
    );
    srv.cmd(&["set-option", "-g", "machine-key", "M-j"])
        .unwrap();
    assert_eq!(
        srv.cmd(&["show-options", "-gv", "machine-key"]).unwrap(),
        "M-j\n"
    );
}
