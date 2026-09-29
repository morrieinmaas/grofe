//! Sessions and windows (spec §4.2, §4.5): create, list, kill; targets
//! resolved against the model and the request's `pane`; the lifecycle
//! cascade (dead pane, empty window, last session closes the server).

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, run_bounded};
use grofe::server::StartOptions;

const WAIT: Duration = Duration::from_secs(5);

/// A command that keeps its pane alive for the length of a test.
const LONG: &[&str] = &["sleep", "30"];

fn new_session(srv: &TestServer, name: &str) {
    let mut args = vec!["new-session", "-d", "-s", name];
    args.extend_from_slice(LONG);
    srv.cmd(&args).expect("new-session");
}

fn lines(out: &str) -> Vec<String> {
    out.lines().map(str::to_string).collect()
}

/// The `%N` ids in `list-panes` output, in order.
fn pane_ids(srv: &TestServer, target: &str) -> Vec<String> {
    let out = srv.cmd(&["list-panes", "-t", target]).expect("list-panes");
    out.lines()
        .filter_map(|l| l.split_whitespace().find(|w| w.starts_with('%')))
        .map(str::to_string)
        .collect()
}

/// Runs the CLI as if from inside pane `pane` of `srv`, with `GROFE` and
/// `GROFE_PANE` set as a real pane has them.
fn cmd_from_pane(srv: &TestServer, pane: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = srv.command();
    cmd.env("GROFE", &srv.socket)
        .env("GROFE_PANE", pane)
        .args(args);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(out.stderr)
    }
}

/// Auto-starts a server whose global `remain-on-exit` is off, so dead
/// panes are removed.
fn start_without_remain(srv: &TestServer, args: &[&str]) {
    let mut cmd = srv.command();
    cmd.env(StartOptions::REMAIN_ON_EXIT_ENV, "0").args(args);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}\nlog:\n{}", srv.log());
}

fn wait_for_file(path: &Path) -> String {
    common::wait_until(
        || fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{} never written: {e}", path.display()));
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn sessions_are_created_listed_and_found() {
    let srv = TestServer::start();
    assert_eq!(srv.cmd(&["list-sessions"]).unwrap(), "");

    // Created in reverse order, so a name-sorted listing (tmux's order)
    // differs from creation/id order.
    new_session(&srv, "beta");
    new_session(&srv, "alpha");

    let out = lines(&srv.cmd(&["list-sessions"]).unwrap());
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out[0].starts_with("alpha: 1 windows"), "{out:?}");
    assert!(out[1].starts_with("beta: 1 windows"), "{out:?}");

    assert!(srv.cmd(&["has-session", "-t", "alpha"]).is_ok());
    // Unique prefix.
    assert!(srv.cmd(&["has-session", "-t", "be"]).is_ok());
    let err = srv.cmd(&["has-session", "-t", "gamma"]).unwrap_err();
    assert!(err.contains("can't find session: gamma"), "{err}");
}

#[test]
fn duplicate_session_name_is_an_error() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    let err = srv.cmd(&["new-session", "-d", "-s", "a"]).unwrap_err();
    assert!(err.contains("duplicate session: a"), "{err}");
    assert_eq!(lines(&srv.cmd(&["list-sessions"]).unwrap()).len(), 1);
}

#[test]
fn unnamed_sessions_get_distinct_names() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "sleep", "30"]).unwrap();
    srv.cmd(&["new-session", "-d", "sleep", "30"]).unwrap();
    let out = lines(&srv.cmd(&["list-sessions"]).unwrap());
    assert_eq!(out.len(), 2, "{out:?}");
    let name = |l: &str| l.split(':').next().unwrap_or_default().to_string();
    assert_ne!(name(&out[0]), name(&out[1]));
}

#[test]
fn kill_session_removes_only_that_session() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    new_session(&srv, "b");

    srv.cmd(&["kill-session", "-t", "a"]).unwrap();

    assert!(srv.cmd(&["has-session", "-t", "a"]).is_err());
    assert!(srv.cmd(&["has-session", "-t", "b"]).is_ok());
    assert!(srv.is_running());
}

#[test]
fn killing_the_last_session_stops_the_server() {
    let srv = TestServer::start();
    new_session(&srv, "only");
    srv.cmd(&["kill-session", "-t", "only"]).unwrap();
    srv.wait_for(|| !srv.is_running(), WAIT)
        .expect("server still running after its last session was killed");
}

#[test]
fn a_server_with_no_sessions_yet_stays_up() {
    let srv = TestServer::start();
    let err = srv.cmd(&["has-session", "-t", "x"]).unwrap_err();
    assert!(err.contains("can't find session"), "{err}");
    assert!(srv.is_running());
}

/// The client's `update-environment` variables (here `DISPLAY`) and cwd;
/// since TC.5 nothing else of the client's environment, as tmux 3.7b.
#[test]
fn session_panes_get_the_clients_env_and_cwd() {
    let srv = TestServer::start();
    let dir = srv.home.join("work");
    fs::create_dir(&dir).unwrap();
    let out = srv.home.join("env.out");
    let script = format!(
        "echo \"$DISPLAY $(pwd -P)\" > '{}'; sleep 30",
        out.display()
    );

    let mut cmd = srv.command();
    cmd.env("DISPLAY", "from-client").args([
        "new-session",
        "-d",
        "-s",
        "e",
        "-c",
        dir.to_str().unwrap(),
        &script,
    ]);
    let res = run_bounded(cmd, CMD_TIMEOUT);
    assert!(res.status.success(), "{res:?}");

    let got = wait_for_file(&out);
    let want_dir = dir.canonicalize().unwrap();
    assert_eq!(got, format!("from-client {}\n", want_dir.display()));
}

#[test]
fn windows_are_created_listed_and_selected() {
    let srv = TestServer::start();
    new_session(&srv, "a");

    srv.cmd(&["new-window", "-t", "a", "-n", "editor", "sleep", "30"])
        .unwrap();
    let out = lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap());
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out[0].starts_with("1: "), "{out:?}");
    assert!(!out[0].contains("(active)"), "{out:?}");
    assert!(out[1].starts_with("2: editor* (1 panes)"), "{out:?}");
    assert!(out[1].contains("(active)"), "{out:?}");

    // -d leaves the active window alone; an explicit index is honoured.
    srv.cmd(&["new-window", "-d", "-t", "a:5", "sleep", "30"])
        .unwrap();
    let out = lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap());
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(out[2].starts_with("5: "), "{out:?}");
    assert!(out[1].contains("(active)"), "{out:?}");
    let err = srv
        .cmd(&["new-window", "-t", "a:5", "sleep", "30"])
        .unwrap_err();
    assert!(err.contains("index 5 in use"), "{err}");

    srv.cmd(&["select-window", "-t", "a:1"]).unwrap();
    let out = lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap());
    assert!(out[0].contains("(active)"), "{out:?}");
    assert!(!out[1].contains("(active)"), "{out:?}");

    // By name.
    srv.cmd(&["select-window", "-t", "a:editor"]).unwrap();
    let out = lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap());
    assert!(out[1].contains("(active)"), "{out:?}");

    let err = srv.cmd(&["select-window", "-t", "a:9"]).unwrap_err();
    assert!(err.contains("can't find window: 9"), "{err}");
}

#[test]
fn kill_window_removes_it_and_the_last_one_closes_session_then_server() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    new_session(&srv, "b");
    srv.cmd(&["new-window", "-t", "a", "sleep", "30"]).unwrap();

    srv.cmd(&["kill-window", "-t", "a:1"]).unwrap();
    let out = lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap());
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("2: "), "{out:?}");
    assert!(out[0].contains("(active)"), "{out:?}");

    // The last window closes its session; others are untouched.
    srv.cmd(&["kill-window", "-t", "a:2"]).unwrap();
    assert!(srv.cmd(&["has-session", "-t", "a"]).is_err());
    assert!(srv.is_running());

    // The last window of the last session closes the server.
    srv.cmd(&["kill-window", "-t", "b:1"]).unwrap();
    srv.wait_for(|| !srv.is_running(), WAIT)
        .expect("server still running after its last window was killed");
}

#[test]
fn current_target_resolves_from_the_requests_pane() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    new_session(&srv, "b");
    let b_pane = pane_ids(&srv, "b").remove(0);

    // No -t: the window lands in the session holding the request's pane.
    cmd_from_pane(&srv, &b_pane, &["new-window", "-n", "fromb", "sleep", "30"]).unwrap();
    let b = srv.cmd(&["list-windows", "-t", "b"]).unwrap();
    assert!(b.contains("fromb"), "{b}");
    let a = srv.cmd(&["list-windows", "-t", "a"]).unwrap();
    assert!(!a.contains("fromb"), "{a}");

    // list-windows with no -t lists that session too.
    let current = cmd_from_pane(&srv, &b_pane, &["list-windows"]).unwrap();
    assert_eq!(current, b);

    // kill-window with no -t kills the pane's own window, not the active one.
    cmd_from_pane(&srv, &b_pane, &["kill-window"]).unwrap();
    let out = lines(&srv.cmd(&["list-windows", "-t", "b"]).unwrap());
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("fromb"), "{out:?}");

    // A pane that no longer exists is an error, not a silent fallback.
    let err = cmd_from_pane(&srv, &b_pane, &["list-windows"]).unwrap_err();
    assert!(err.contains(&format!("can't find pane: {b_pane}")), "{err}");
}

#[test]
fn id_targets_resolve() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    let pane = pane_ids(&srv, "a").remove(0);

    let by_id = srv.cmd(&["list-panes", "-t", &pane]).unwrap();
    assert!(by_id.contains(&pane), "{by_id}");
    let by_pos = srv.cmd(&["list-panes", "-t", "a:1.1"]).unwrap();
    assert_eq!(by_id, by_pos);
    let err = srv.cmd(&["list-panes", "-t", "a:1.2"]).unwrap_err();
    assert!(err.contains("can't find pane: 2"), "{err}");
    let err = srv.cmd(&["list-panes", "-t", "%999"]).unwrap_err();
    assert!(err.contains("can't find pane: %999"), "{err}");
}

#[test]
fn dead_pane_stays_with_remain_on_exit() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "d", "exit 3"])
        .unwrap();
    srv.wait_for(
        || {
            srv.cmd(&["list-panes", "-t", "d"])
                .is_ok_and(|o| o.contains("(dead)"))
        },
        WAIT,
    )
    .expect("pane never shown as dead");
    assert!(srv.cmd(&["has-session", "-t", "d"]).is_ok());
    assert!(srv.is_running());
}

#[test]
fn exited_pane_closes_its_window_but_not_the_session() {
    let srv = TestServer::unstarted();
    start_without_remain(&srv, &["new-session", "-d", "-s", "a", "sleep", "30"]);
    srv.cmd(&["new-window", "-t", "a", "sleep 2"]).unwrap();
    assert_eq!(
        lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap()).len(),
        2
    );

    srv.wait_for(
        || {
            srv.cmd(&["list-windows", "-t", "a"])
                .is_ok_and(|o| o.lines().count() == 1)
        },
        WAIT,
    )
    .expect("window of the exited pane was not closed");
    let out = srv.cmd(&["list-windows", "-t", "a"]).unwrap();
    assert!(out.starts_with("1: ") && out.contains("(active)"), "{out}");
    assert!(srv.is_running());
}

#[test]
fn last_pane_exiting_closes_window_session_and_server() {
    let srv = TestServer::unstarted();
    start_without_remain(&srv, &["new-session", "-d", "-s", "a", "sleep 0.3"]);
    srv.wait_for(|| !srv.is_running(), WAIT)
        .expect("server still running after its last pane exited");
}

/// A pane's own exit can race an explicit `kill-window`/`kill-session` that
/// removes it first: the natural `PaneEvent::Exited` that later reaches the
/// actor for an id no longer in the model must be a no-op, not a crash or a
/// state mismatch. This uses fast-exiting panes so the race is plausible on
/// every run; the assertions hold regardless of which side won it.
#[test]
fn late_pane_exit_after_kill_is_ignored() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    new_session(&srv, "keep");

    // kill-window races the fast pane's own exit.
    srv.cmd(&["new-window", "-t", "a", "true"]).unwrap();
    srv.cmd(&["kill-window", "-t", "a:2"]).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let out = lines(&srv.cmd(&["list-windows", "-t", "a"]).unwrap());
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(srv.cmd(&["has-session", "-t", "keep"]).is_ok());
    assert!(srv.is_running());

    // kill-session races the fast pane's own exit.
    srv.cmd(&["new-session", "-d", "-s", "b", "true"]).unwrap();
    srv.cmd(&["kill-session", "-t", "b"]).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(srv.cmd(&["has-session", "-t", "b"]).is_err());
    assert!(srv.cmd(&["has-session", "-t", "a"]).is_ok());
    assert!(srv.cmd(&["has-session", "-t", "keep"]).is_ok());
    assert!(srv.is_running());
    assert!(srv.cmd(&["list-sessions"]).is_ok());
}

/// `GROFE_PANE` names a pane of the server in `GROFE`. A CLI whose `GROFE`
/// is another socket, while it targets `srv` with `-S`, must act as if no
/// pane was given, even though that pane id exists on `srv`.
#[test]
fn grofe_pane_from_another_server_is_ignored() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    let a_pane = pane_ids(&srv, "a")[0].clone();
    // Newest session, so the no-pane fallback resolves here instead.
    new_session(&srv, "b");
    let b_pane = pane_ids(&srv, "b")[0].clone();
    assert_ne!(a_pane, b_pane);

    let own = cmd_from_pane(&srv, &a_pane, &["display-message", "-p", "#{pane_id}"]).unwrap();
    assert_eq!(own.trim(), a_pane, "GROFE=srv resolves through the pane");

    let mut cmd = srv.command();
    cmd.env("GROFE", srv.home.join("other.sock"))
        .env("GROFE_PANE", &a_pane)
        .args(["display-message", "-p", "#{pane_id}"]);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout.trim(), b_pane, "a foreign GROFE_PANE was used");
}
