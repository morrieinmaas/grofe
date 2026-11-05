//! The M4 stub edit: the new verbs are routed, the format variables and
//! the `restore-*` options exist, and the harness extension (snapshot
//! path, crash, signals, restart, `PATH`) works. Written so no M4 task has
//! to edit it: a verb that has landed may answer anything except
//! `unknown command`.

mod common;

use std::time::Duration;

use common::{Signal, TestServer};

fn session(srv: &TestServer) {
    srv.cmd(&["new-session", "-d", "-s", "main"])
        .expect("new-session");
}

#[test]
fn the_m4_verbs_are_routed() {
    let srv = TestServer::start();
    session(&srv);
    for verb in ["pane-set-resume", "reboot-check"] {
        if let Err(e) = srv.cmd(&[verb]) {
            assert!(!e.contains("unknown command"), "{verb}: {e}");
        }
    }
}

#[test]
fn the_m4_format_variables_and_options_exist() {
    let srv = TestServer::start();
    session(&srv);
    let out = srv
        .cmd(&[
            "display-message",
            "-p",
            "-t",
            "main",
            "[#{pane_restored}][#{pane_resume_expected}]",
        ])
        .expect("display");
    assert_eq!(out, "[0][]\n");
    let opt = |name: &str| srv.cmd(&["show-options", "-gv", name]).expect(name);
    assert_eq!(opt("restore-on-start"), "on\n");
    assert_eq!(opt("restore-stagger-ms"), "1000\n");
    srv.cmd(&["set-option", "-p", "--no-persist", "@agent_session", "y"])
        .expect("--no-persist parses");
}

#[test]
fn the_snapshot_lives_in_the_instance_directory_beside_the_log() {
    let srv = TestServer::start();
    let snap = srv.snapshot_path();
    assert!(snap.starts_with(&srv.state_home), "{snap:?}");
    assert_eq!(
        snap.file_name().and_then(|n| n.to_str()),
        Some("state.json")
    );
    assert_eq!(
        snap.parent().and_then(|d| d.file_name()),
        srv.log_path().file_stem()
    );
}

#[test]
fn edit_snapshot_rewrites_the_file_on_disk() {
    let srv = TestServer::start();
    let snap = srv.snapshot_path();
    std::fs::create_dir_all(snap.parent().unwrap()).unwrap();
    std::fs::write(&snap, r#"{"version": 1, "sessions": []}"#).unwrap();
    srv.edit_snapshot(|v| v["version"] = 7.into());
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snap).unwrap()).unwrap();
    assert_eq!(back["version"], 7);
}

#[test]
fn a_hard_killed_server_restarts_on_the_same_socket() {
    let srv = TestServer::start();
    session(&srv);
    srv.kill_hard();
    assert!(!srv.is_running());
    let srv = srv.restart();
    assert!(srv.is_running());
    // Restore is off under the default harness: the server starts empty.
    session(&srv);
}

#[test]
fn a_server_signalled_with_its_panes_restarts() {
    let srv = TestServer::start();
    session(&srv);
    srv.signal_all(Signal::SIGTERM);
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(5))
        .expect("the server goes on SIGTERM");
    let srv = srv.restart();
    session(&srv);
}

#[test]
fn a_running_server_restarts_too() {
    let srv = TestServer::builder().restore_on_start(true).start();
    session(&srv);
    let srv = srv.restart();
    assert!(srv.is_running());
}

#[test]
fn path_env_is_the_only_path_the_server_and_its_panes_see() {
    let srv = TestServer::builder().path_env("/usr/bin:/bin").start();
    session(&srv);
    let out = srv
        .cmd(&["show-environment", "-g", "PATH"])
        .expect("show-environment");
    assert_eq!(out, "PATH=/usr/bin:/bin\n");
}
