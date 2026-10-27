//! T6.6: the in-tree `event-log` plugin (spec §7.4) logs a split and a
//! `select-pane` in order within a second, and passes `shellcheck`. The
//! server, its state dir and the plugin's files are all the test's own.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{CMD_TIMEOUT, TestServer, run_bounded, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/event-log/event-log.sh")
}

fn display(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .expect("display-message")
        .trim()
        .to_string()
}

fn events(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The `pane` of each `pane-focus` line, in file order.
fn focus_panes(path: &Path) -> Vec<String> {
    events(path)
        .iter()
        .filter(|l| l.contains(r#""event":"pane-focus""#))
        .map(|l| {
            let rest = l.split(r#""pane":""#).nth(1).expect("pane field");
            rest.split('"').next().expect("pane value").to_string()
        })
        .collect()
}

#[test]
fn a_split_and_a_select_pane_are_appended_in_order_within_a_second() {
    let srv = TestServer::unstarted();
    let config = format!(
        "[plugins.event-log]\ncmd = [\"sh\", {:?}]\nenv = {{ GROFE_BIN = {:?} }}\n",
        script_path().display().to_string(),
        env!("CARGO_BIN_EXE_grofe"),
    );
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), config).expect("write config");
    let mut cmd = srv.command();
    cmd.arg("start-server");
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        "80",
        "-y",
        "24",
        "sleep 600",
    ])
    .expect("new-session");

    let log = srv.state_home.join("grofe").join("events.log");
    // Nothing is replayed, so poke the server until the plugin is listening.
    let mut n = 0;
    wait_until(
        || {
            n += 1;
            let _ = srv.cmd(&["set-option", "-g", "@probe", &n.to_string()]);
            events(&log)
                .iter()
                .any(|l| l.contains(r#""event":"option-changed""#))
        },
        WAIT,
    )
    .expect("the plugin never logged a probe event");

    let _client = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    let first = display(&srv, "s", "#{pane_id}");
    wait_until(|| focus_panes(&log) == [first.clone()], WAIT).expect("attach focus event");

    let start = Instant::now();
    srv.cmd(&["split-window", "-h", "-t", "s", "sleep 600"])
        .expect("split-window");
    let second = display(&srv, "s", "#{pane_id}");
    assert_ne!(first, second, "the split took focus");
    srv.cmd(&["select-pane", "-t", &first])
        .expect("select-pane");
    wait_until(|| focus_panes(&log).len() == 3, Duration::from_secs(1))
        .unwrap_or_else(|e| panic!("{e}: {:?}", events(&log)));
    assert!(start.elapsed() < Duration::from_secs(1) + Duration::from_millis(200));
    assert_eq!(focus_panes(&log), [first.clone(), second, first]);
    for line in events(&log) {
        assert!(line.starts_with(r#"{"event":""#), "one JSON event: {line}");
    }
}

/// `shellcheck` on `path` (a PATH-style list, never anywhere else). Missing
/// under `ci` panics, so CI cannot skip the check unseen; locally it is
/// `None` and the test skips.
fn find_shellcheck(path: &std::ffi::OsStr, ci: bool) -> Option<PathBuf> {
    let found = std::env::split_paths(path)
        .map(|dir| dir.join("shellcheck"))
        .find(|candidate| candidate.is_file());
    if found.is_none() && ci {
        panic!("shellcheck not found on PATH (required under CI)");
    }
    found
}

#[test]
fn the_plugin_script_passes_shellcheck() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(bin) = find_shellcheck(&path, std::env::var_os("CI").is_some()) else {
        eprintln!("SKIP: shellcheck is not on PATH");
        return;
    };
    let out = Command::new(bin)
        .args(["-s", "sh"])
        .arg(script_path())
        .output()
        .expect("run shellcheck");
    assert!(
        out.status.success(),
        "shellcheck: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[should_panic(expected = "shellcheck not found on PATH (required under CI)")]
fn a_missing_shellcheck_fails_under_ci() {
    let empty = tempfile::tempdir().expect("tempdir");
    find_shellcheck(empty.path().as_os_str(), true);
}

#[test]
fn a_missing_shellcheck_skips_outside_ci() {
    let empty = tempfile::tempdir().expect("tempdir");
    assert_eq!(find_shellcheck(empty.path().as_os_str(), false), None);
}
