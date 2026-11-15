//! Agents step 3 (spec §6): reports raise notifications into the tray, the
//! command delivery runs the configured argv with agent text as plain data,
//! mute silences it, and the badge counts blocked agents. Private `-S` socket.

mod common;
#[allow(dead_code, clippy::duplicate_mod)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TestServer;
use harness::Harness;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(1500);

fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "100", "-y", "30"])
        .expect("new-session");
    srv
}

fn pane_id(srv: &TestServer, target: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, "#{pane_id}"])
        .expect("display-message")
        .trim()
        .to_string()
}

fn report(srv: &TestServer, pane: &str, state: &str, msg: &str) {
    srv.cmd(&[
        "agent", "report", state, "-t", pane, "--kind", "claude", "--msg", msg,
    ])
    .expect("agent report");
}

fn notify_list(srv: &TestServer) -> String {
    srv.cmd(&["notify", "list"]).expect("notify list")
}

/// A recording script that appends its argv, one element per line, to a file.
fn recorder(dir: &TempDir) -> (PathBuf, PathBuf) {
    let out = dir.path().join("argv.txt");
    let script = dir.path().join("record.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", out.display()),
    )
    .expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    (script, out)
}

fn write_notify_config(srv: &TestServer, script: &Path) {
    let conf = srv.config_home.join("grofe").join("conf.d");
    fs::create_dir_all(&conf).expect("conf.d");
    fs::write(
        conf.join("50-notify.toml"),
        format!(
            "[notify]\ndelivery = \"command\"\ncommand = [\"{}\", \"--\", \"{{title}}\", \"{{message}}\"]\n",
            script.display()
        ),
    )
    .expect("write config");
    srv.cmd(&["reload"]).expect("reload");
}

#[test]
fn blocked_report_lands_in_notify_list() {
    let srv = start();
    let id = pane_id(&srv, "main");
    report(&srv, &id, "blocked", "perm");
    let list = notify_list(&srv);
    let line = list.lines().next().unwrap_or_default();
    assert!(
        line.contains("claude") && line.contains("blocked") && line.ends_with("perm"),
        "got {list:?}"
    );
}

#[test]
fn seen_pane_raises_nothing() {
    let srv = start();
    let id = pane_id(&srv, "main");
    let _client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 100, 30);
    srv.wait_for(
        || {
            srv.cmd(&["list-clients"])
                .map(|o| !o.trim().is_empty())
                .unwrap_or(false)
        },
        WAIT,
    )
    .expect("client attached");
    report(&srv, &id, "blocked", "perm");
    assert_eq!(notify_list(&srv), "", "a pane in view raises nothing");
}

#[test]
fn notify_command_runs_recording_script_with_argv() {
    let srv = start();
    let dir = tempfile::tempdir().expect("tempdir");
    let (script, out) = recorder(&dir);
    write_notify_config(&srv, &script);
    let touched = dir.path().join("x");
    let msg = format!("perm $(touch {}) \"q\"", touched.display());
    let id = pane_id(&srv, "main");
    report(&srv, &id, "blocked", &msg);
    // The shell creates the file before `printf` writes to it.
    srv.wait_for(
        || fs::read_to_string(&out).is_ok_and(|s| s.lines().count() >= 3),
        WAIT,
    )
    .expect("script ran");
    let got = fs::read_to_string(&out).expect("read record");
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(lines.len(), 3, "got {got:?}");
    assert_eq!(lines[0], "--");
    assert_eq!(lines[1], "grofe: claude");
    assert!(
        lines[2].ends_with(&format!("blocked: {msg}")),
        "got {:?}",
        lines[2]
    );
    common::quiet_period(QUIET);
    assert!(!touched.exists(), "the $(...) must never run");
}

#[test]
fn mute_suppresses_command() {
    let srv = start();
    let dir = tempfile::tempdir().expect("tempdir");
    let (script, out) = recorder(&dir);
    write_notify_config(&srv, &script);
    srv.cmd(&["new-window", "-d", "-t", "main"])
        .expect("new-window");
    let panes = srv
        .cmd(&["list-panes", "-s", "-t", "main", "-F", "#{pane_id}"])
        .expect("list-panes");
    let ids: Vec<&str> = panes.lines().collect();
    assert_eq!(ids.len(), 2, "got {panes:?}");
    let (first, second) = (ids[0], ids[1]);
    srv.cmd(&["notify", "mute"]).expect("mute");
    report(&srv, first, "blocked", "muted-one");
    assert!(
        notify_list(&srv).contains("muted-one"),
        "tray still gets it"
    );
    common::quiet_period(QUIET);
    assert!(!out.exists(), "no command while muted");
    srv.cmd(&["notify", "unmute"]).expect("unmute");
    report(&srv, second, "blocked", "loud-two");
    srv.wait_for(
        || fs::read_to_string(&out).is_ok_and(|s| s.contains("loud-two")),
        WAIT,
    )
    .expect("script ran");
    let got = fs::read_to_string(&out).expect("read record");
    assert!(
        got.contains("loud-two") && !got.contains("muted-one"),
        "got {got:?}"
    );
}

#[test]
fn badge_in_status_format() {
    let srv = start();
    let id = pane_id(&srv, "main");
    report(&srv, &id, "blocked", "perm");
    let badge = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{agents_badge}"])
        .expect("display");
    assert_eq!(badge.trim(), "⏸1");
}
