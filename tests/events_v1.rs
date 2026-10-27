//! The v1 event catalog on the `subscribe` stream (spec §7.2, T6.1): one JSON
//! golden per event (each fires exactly once per triggering action, with the
//! catalog's payload), `pane-focus`, and `subscribe -t`. T1.6's own tests
//! (`tests/events.rs`) cover `pane-exited` kills, `pane-output`,
//! `option-changed` and the queue.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::time::Duration;

use common::{Subscription, TestServer, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(5);
/// How long a test waits to be sure an event did not come.
const QUIET: Duration = Duration::from_millis(300);

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args)
        .unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
}

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    ok(srv, &["display-message", "-p", "-t", target, fmt])
        .trim_end()
        .to_string()
}

/// The next event, as the JSON line the stream carries.
fn next(sub: &mut Subscription) -> String {
    let event = sub.next(WAIT).unwrap_or_else(|e| panic!("no event: {e}"));
    serde_json::to_string(&event).unwrap()
}

/// Nothing more arrives.
fn quiet(sub: &mut Subscription) {
    if let Ok(event) = sub.next(QUIET) {
        panic!(
            "unexpected event: {}",
            serde_json::to_string(&event).unwrap()
        );
    }
}

/// The name of the one attached client, once it has attached.
fn client_name(srv: &TestServer) -> String {
    let listed = || ok(srv, &["list-clients", "-F", "#{client_name}"]);
    wait_until(|| !listed().trim().is_empty(), WAIT).expect("a client attached");
    listed().trim_end().to_string()
}

/// A session `s` with one window running `sleep`, and a subscription to
/// `events` taken after it exists.
fn start(events: &[&str]) -> (TestServer, Subscription) {
    let srv = TestServer::start();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "sleep 600",
        ],
    );
    let sub = srv.subscribe(events).unwrap();
    (srv, sub)
}

fn tty_write(srv: &TestServer, target: &str, bytes: &[u8]) {
    let tty = display(srv, target, "#{pane_tty}");
    fs::write(tty, bytes).unwrap();
}

// --- goldens, one per event ----------------------------------------------

#[test]
fn pane_exited_and_pane_died_golden() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    let cmd = format!(
        "while [ ! -e {} ]; do sleep 0.01; done; exit 4",
        flag.display()
    );
    ok(&srv, &["new-session", "-d", "-s", "s", &cmd]);
    let (pane, uid) = (
        display(&srv, "s", "#{pane_id}"),
        display(&srv, "s", "#{pane_uid}"),
    );
    let window = display(&srv, "s", "#{window_id}");
    let mut sub = srv.subscribe(&["pane-exited", "pane-died"]).unwrap();
    fs::write(&flag, "").unwrap();
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"pane-exited","pane":"{pane}","uid":"{uid}","status":4}}"#)
    );
    assert_eq!(
        next(&mut sub),
        format!(
            r#"{{"event":"pane-died","pane":"{pane}","uid":"{uid}","window":"{window}","status":4}}"#
        )
    );
    quiet(&mut sub);
}

#[test]
fn pane_exited_fires_once_per_process_across_remain_on_exit_and_kill() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    let cmd = format!(
        "while [ ! -e {} ]; do sleep 0.01; done; exit 0",
        flag.display()
    );
    ok(&srv, &["new-session", "-d", "-s", "s", "sleep 600"]);
    ok(&srv, &["split-window", "-d", "-t", "s", &cmd]);
    let pane = display(&srv, "s:1.2", "#{pane_id}");
    let mut sub = srv.subscribe(&["pane-exited"]).unwrap();
    fs::write(&flag, "").unwrap();
    let first = next(&mut sub);
    assert!(
        first.contains(&format!(r#""pane":"{pane}""#)) && first.ends_with(r#""status":0}"#),
        "{first}"
    );
    // remain-on-exit kept the dead pane; killing it reports nothing more.
    assert_eq!(display(&srv, &pane, "#{pane_dead}"), "1");
    ok(&srv, &["kill-pane", "-t", &pane]);
    quiet(&mut sub);
    // A pane killed alive reports once; a dead one in a killed window adds
    // nothing.
    ok(&srv, &["new-window", "-d", "-t", "s", "sleep 600"]);
    ok(&srv, &["kill-window", "-t", "s:2"]);
    assert!(next(&mut sub).ends_with(r#""status":null}"#));
    quiet(&mut sub);
}

#[test]
fn window_created_golden() {
    let (srv, mut sub) = start(&["window-created"]);
    ok(&srv, &["new-window", "-d", "-t", "s", "sleep 600"]);
    let window = display(&srv, "s:2", "#{window_id}");
    let pane = display(&srv, "s:2", "#{pane_id}");
    let session = display(&srv, "s", "#{session_id}");
    assert_eq!(
        next(&mut sub),
        format!(
            r#"{{"event":"window-created","window":"{window}","session":"{session}","pane":"{pane}"}}"#
        )
    );
    quiet(&mut sub);
}

#[test]
fn window_focused_golden_even_onto_the_current_window() {
    let (srv, mut sub) = start(&["window-focused"]);
    let window = display(&srv, "s:1", "#{window_id}");
    let pane = display(&srv, "s:1", "#{pane_id}");
    let session = display(&srv, "s", "#{session_id}");
    let golden = format!(
        r#"{{"event":"window-focused","window":"{window}","session":"{session}","pane":"{pane}"}}"#
    );
    ok(&srv, &["select-window", "-t", "s:1"]);
    assert_eq!(next(&mut sub), golden);
    quiet(&mut sub);
}

#[test]
fn pane_resized_and_window_resized_golden() {
    let (srv, mut sub) = start(&["pane-resized", "window-resized"]);
    ok(&srv, &["split-window", "-d", "-t", "s", "sleep 600"]);
    let pane = display(&srv, "s:1.2", "#{pane_id}");
    let window = display(&srv, "s:1", "#{window_id}");
    let session = display(&srv, "s", "#{session_id}");
    ok(&srv, &["resize-pane", "-t", &pane, "-D", "2"]);
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"pane-resized","pane":"{pane}","window":"{window}"}}"#)
    );
    ok(&srv, &["resize-window", "-t", "s", "-x", "100", "-y", "30"]);
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"window-resized","window":"{window}","session":"{session}"}}"#)
    );
    quiet(&mut sub);
}

#[test]
fn session_created_and_session_closed_golden() {
    let (srv, mut sub) = start(&["session-created", "session-closed", "pane-exited"]);
    ok(&srv, &["new-session", "-d", "-s", "b", "sleep 600"]);
    let session = display(&srv, "b", "#{session_id}");
    let pane = display(&srv, "b", "#{pane_id}");
    let uid = display(&srv, "b", "#{pane_uid}");
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"session-created","session":"{session}","name":"b"}}"#)
    );
    ok(&srv, &["kill-session", "-t", "b"]);
    // The session's panes go before the session does.
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"pane-exited","pane":"{pane}","uid":"{uid}","status":null}}"#)
    );
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"session-closed","session":"{session}","name":"b"}}"#)
    );
    quiet(&mut sub);
}

#[test]
fn a_session_whose_last_pane_exits_is_closed_after_its_pane() {
    let srv = TestServer::start();
    ok(&srv, &["set", "-g", "remain-on-exit", "off"]);
    ok(&srv, &["new-session", "-d", "-s", "keep", "sleep 600"]);
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("go");
    let cmd = format!(
        "while [ ! -e {} ]; do sleep 0.01; done; exit 2",
        flag.display()
    );
    ok(&srv, &["new-session", "-d", "-s", "gone", &cmd]);
    let session = display(&srv, "gone", "#{session_id}");
    let pane = display(&srv, "gone", "#{pane_id}");
    let mut sub = srv.subscribe(&["pane-exited", "session-closed"]).unwrap();
    fs::write(&flag, "").unwrap();
    let exited = next(&mut sub);
    assert!(
        exited.contains(&format!(r#""pane":"{pane}""#)) && exited.ends_with(r#""status":2}"#),
        "{exited}"
    );
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"session-closed","session":"{session}","name":"gone"}}"#)
    );
    quiet(&mut sub);
}

#[test]
fn client_events_golden() {
    let (srv, mut sub) = start(&["client-session-changed", "client-resized"]);
    let session = display(&srv, "s", "#{session_id}");
    let mut client = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    let name = client_name(&srv);
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"client-session-changed","client":"{name}","session":"{session}"}}"#)
    );
    assert_eq!(
        next(&mut sub),
        format!(
            r#"{{"event":"client-resized","client":"{name}","session":"{session}","cols":80,"rows":24}}"#
        )
    );
    // The client reports its size once it is up: that is a resize too.
    while let Ok(event) = sub.next(QUIET) {
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            format!(
                r#"{{"event":"client-resized","client":"{name}","session":"{session}","cols":80,"rows":24}}"#
            )
        );
    }
    client.resize(100, 30);
    assert_eq!(
        next(&mut sub),
        format!(
            r#"{{"event":"client-resized","client":"{name}","session":"{session}","cols":100,"rows":30}}"#
        )
    );
    // Switching to the session it already shows is a session change too.
    ok(&srv, &["switch-client", "-t", "s"]);
    assert_eq!(
        next(&mut sub),
        format!(r#"{{"event":"client-session-changed","client":"{name}","session":"{session}"}}"#)
    );
    quiet(&mut sub);
}

#[test]
fn alert_events_golden() {
    let srv = TestServer::start();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "sleep 600",
        ],
    );
    ok(&srv, &["new-window", "-d", "-t", "s", "sleep 600"]);
    let session = display(&srv, "s", "#{session_id}");
    let window = display(&srv, "s:2", "#{window_id}");
    let pane = display(&srv, "s:2", "#{pane_id}");
    ok(&srv, &["set", "-gw", "monitor-activity", "on"]);
    let mut sub = srv
        .subscribe(&["alert-bell", "alert-activity", "alert-silence"])
        .unwrap();
    let golden = |name: &str| {
        format!(r#"{{"event":"{name}","window":"{window}","session":"{session}","pane":"{pane}"}}"#)
    };
    tty_write(&srv, "s:2", b"\x07");
    // Output in a window other than the current one: bell and activity.
    let mut got = vec![next(&mut sub), next(&mut sub)];
    got.sort();
    assert_eq!(got, [golden("alert-activity"), golden("alert-bell")]);
    // With nothing more written, the window goes silent after a second.
    ok(&srv, &["set", "-gw", "monitor-silence", "1"]);
    assert_eq!(next(&mut sub), golden("alert-silence"));
    quiet(&mut sub);
}

// --- pane-focus ----------------------------------------------------------

/// A session `s` with two side-by-side panes (the left one active), a
/// client attached to it, and a `pane-focus` subscription taken once the
/// attach's own event is read.
fn focus_rig() -> (TestServer, Harness, Subscription, String, String) {
    let srv = TestServer::start();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "sleep 600",
        ],
    );
    ok(&srv, &["split-window", "-d", "-h", "-t", "s", "sleep 600"]);
    let mut sub = srv.subscribe(&["pane-focus"]).unwrap();
    let client = Harness::spawn_attach_with_env(srv.command(), "s", &[], 80, 24);
    let name = client_name(&srv);
    let (left, right) = (
        display(&srv, "s:1.1", "#{pane_id}"),
        display(&srv, "s:1.2", "#{pane_id}"),
    );
    let session = display(&srv, "s", "#{session_id}");
    let window = display(&srv, "s", "#{window_id}");
    // An attach is a focus change.
    assert_eq!(
        next(&mut sub),
        format!(
            r#"{{"event":"pane-focus","pane":"{left}","window":"{window}","session":"{session}","client":"{name}"}}"#
        )
    );
    quiet(&mut sub);
    (srv, client, sub, right, name)
}

fn focus(pane: &str, window: &str, session: &str, client: &str) -> String {
    format!(
        r#"{{"event":"pane-focus","pane":"{pane}","window":"{window}","session":"{session}","client":"{client}"}}"#
    )
}

#[test]
fn pane_focus_on_select_pane_once_and_not_when_nothing_moved() {
    let (srv, _client, mut sub, right, name) = focus_rig();
    let session = display(&srv, "s", "#{session_id}");
    let window = display(&srv, "s", "#{window_id}");
    ok(&srv, &["select-pane", "-t", &right]);
    assert_eq!(next(&mut sub), focus(&right, &window, &session, &name));
    // Selecting the active pane moves nothing.
    ok(&srv, &["select-pane", "-t", &right]);
    quiet(&mut sub);
    // Several verbs, one action: one event for the final view.
    ok(
        &srv,
        &[
            "select-pane",
            "-t",
            "s:1.1",
            ";",
            "select-pane",
            "-t",
            &right,
            ";",
            "select-pane",
            "-t",
            "s:1.1",
        ],
    );
    let left = display(&srv, "s:1.1", "#{pane_id}");
    assert_eq!(next(&mut sub), focus(&left, &window, &session, &name));
    quiet(&mut sub);
}

#[test]
fn pane_focus_on_a_split_and_a_following_select_pane() {
    let (srv, _client, mut sub, _right, name) = focus_rig();
    let session = display(&srv, "s", "#{session_id}");
    let window = display(&srv, "s", "#{window_id}");
    // The new pane takes focus.
    ok(&srv, &["split-window", "-t", "s", "sleep 600"]);
    let created = display(&srv, "s", "#{pane_id}");
    assert_eq!(next(&mut sub), focus(&created, &window, &session, &name));
    ok(&srv, &["select-pane", "-t", "s:1.1"]);
    let first = display(&srv, "s:1.1", "#{pane_id}");
    assert_eq!(next(&mut sub), focus(&first, &window, &session, &name));
    quiet(&mut sub);
}

#[test]
fn pane_focus_on_a_window_switch() {
    let (srv, _client, mut sub, _right, name) = focus_rig();
    let session = display(&srv, "s", "#{session_id}");
    ok(&srv, &["new-window", "-d", "-t", "s", "sleep 600"]);
    quiet(&mut sub);
    let window = display(&srv, "s:2", "#{window_id}");
    let pane = display(&srv, "s:2", "#{pane_id}");
    ok(&srv, &["select-window", "-t", "s:2"]);
    assert_eq!(next(&mut sub), focus(&pane, &window, &session, &name));
    quiet(&mut sub);
}

#[test]
fn pane_focus_on_a_click() {
    let (srv, mut client, mut sub, right, name) = focus_rig();
    let session = display(&srv, "s", "#{session_id}");
    let window = display(&srv, "s", "#{window_id}");
    ok(&srv, &["set", "-g", "mouse", "on"]);
    // Let the client take the mouse modes before it clicks.
    wait_until(
        || {
            ok(&srv, &["show", "-gv", "mouse"]).trim() == "on"
                && display(&srv, "s", "#{client_width}") == "80"
        },
        WAIT,
    )
    .unwrap();
    client.send("\x1b[<0;70;5M\x1b[<0;70;5m");
    assert_eq!(next(&mut sub), focus(&right, &window, &session, &name));
    quiet(&mut sub);
}

#[test]
fn pane_focus_on_terminal_focus_in_only_with_focus_events_on() {
    let (srv, mut client, mut sub, _right, name) = focus_rig();
    let session = display(&srv, "s", "#{session_id}");
    let window = display(&srv, "s", "#{window_id}");
    let pane = display(&srv, "s", "#{pane_id}");
    // Off: focus reports move the client's flag and nothing else.
    ok(&srv, &["set", "-g", "focus-events", "off"]);
    client.send("\x1b[O");
    wait_until(
        || ok(&srv, &["list-clients", "-F", "#{client_flags}"]).contains("focused") == false,
        WAIT,
    )
    .unwrap();
    client.send("\x1b[I");
    wait_until(
        || ok(&srv, &["list-clients", "-F", "#{client_flags}"]).contains("focused"),
        WAIT,
    )
    .unwrap();
    quiet(&mut sub);
    // On: losing focus is silent, regaining it is one event.
    ok(&srv, &["set", "-g", "focus-events", "on"]);
    client.send("\x1b[O");
    wait_until(
        || !ok(&srv, &["list-clients", "-F", "#{client_flags}"]).contains("focused"),
        WAIT,
    )
    .unwrap();
    quiet(&mut sub);
    client.send("\x1b[I");
    assert_eq!(next(&mut sub), focus(&pane, &window, &session, &name));
    quiet(&mut sub);
}

#[test]
fn pane_focus_never_fires_without_an_attached_client() {
    let (srv, mut sub) = start(&["pane-focus"]);
    ok(&srv, &["split-window", "-t", "s", "sleep 600"]);
    ok(&srv, &["select-pane", "-t", "s:1.1"]);
    ok(&srv, &["new-window", "-t", "s", "sleep 600"]);
    ok(&srv, &["select-window", "-t", "s:1"]);
    ok(&srv, &["new-session", "-d", "-s", "t", "sleep 600"]);
    ok(&srv, &["kill-pane", "-t", "s:1.2"]);
    quiet(&mut sub);
}

#[test]
fn pane_focus_only_for_the_session_a_client_shows() {
    let (srv, _client, mut sub, _right, _name) = focus_rig();
    ok(&srv, &["new-session", "-d", "-s", "other", "sleep 600"]);
    ok(&srv, &["split-window", "-t", "other", "sleep 600"]);
    ok(&srv, &["select-pane", "-t", "other:1.1"]);
    quiet(&mut sub);
}

// --- subscribe filters ---------------------------------------------------

#[test]
fn a_pane_target_receives_only_that_panes_events() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "s", "sleep 600"]);
    ok(&srv, &["split-window", "-d", "-t", "s", "sleep 600"]);
    ok(&srv, &["split-window", "-d", "-t", "s", "sleep 600"]);
    let ids: Vec<String> = ["s:1.1", "s:1.2", "s:1.3"]
        .iter()
        .map(|t| display(&srv, t, "#{pane_id}"))
        .collect();
    let mut sub = srv.subscribe(&["pane-exited", "-t", &ids[1]]).unwrap();
    ok(&srv, &["kill-pane", "-t", &ids[0]]);
    ok(&srv, &["kill-pane", "-t", &ids[2]]);
    quiet(&mut sub);
    ok(&srv, &["kill-pane", "-t", &ids[1]]);
    let got = next(&mut sub);
    assert!(
        got.contains(&format!(r#""pane":"{}""#, ids[1])) && got.contains("pane-exited"),
        "{got}"
    );
    quiet(&mut sub);
}

#[test]
fn window_and_session_targets_pass_their_panes_events() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "a", "sleep 600"]);
    ok(&srv, &["split-window", "-d", "-t", "a", "sleep 600"]);
    ok(&srv, &["new-window", "-d", "-t", "a", "sleep 600"]);
    ok(&srv, &["new-session", "-d", "-s", "b", "sleep 600"]);
    let in_w1 = display(&srv, "a:1.2", "#{pane_id}");
    let in_w2 = display(&srv, "a:2.1", "#{pane_id}");
    let in_b = display(&srv, "b", "#{pane_id}");
    let mut window = srv.subscribe(&["pane-exited", "-t", "a:1"]).unwrap();
    let mut session = srv.subscribe(&["pane-exited", "-t", "a"]).unwrap();
    ok(&srv, &["kill-pane", "-t", &in_b]);
    quiet(&mut window);
    quiet(&mut session);
    ok(&srv, &["kill-pane", "-t", &in_w2]);
    assert!(next(&mut session).contains(&in_w2));
    quiet(&mut window);
    ok(&srv, &["kill-pane", "-t", &in_w1]);
    assert!(next(&mut window).contains(&in_w1));
    assert!(next(&mut session).contains(&in_w1));
}

#[test]
fn a_window_target_also_passes_the_events_naming_its_window() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "a", "sleep 600"]);
    ok(&srv, &["new-window", "-d", "-t", "a", "sleep 600"]);
    let w2 = display(&srv, "a:2", "#{window_id}");
    let mut sub = srv
        .subscribe(&["window-focused", "window-resized", "-t", &w2])
        .unwrap();
    ok(&srv, &["select-window", "-t", "a:1"]);
    quiet(&mut sub);
    ok(&srv, &["select-window", "-t", "a:2"]);
    assert!(next(&mut sub).contains(&w2));
}

#[test]
fn a_target_that_does_not_exist_is_an_error() {
    let (srv, _sub) = start(&[]);
    let err = srv.subscribe(&["pane-exited", "-t", "%99"]).err().unwrap();
    assert!(err.contains("%99"), "{err}");
}

#[test]
fn an_unknown_literal_event_name_is_an_error_and_patterns_match_the_catalog() {
    let (srv, _sub) = start(&[]);
    let err = srv.subscribe(&["pane-exitted"]).err().unwrap();
    assert_eq!(err, "unknown event: pane-exitted");
    // A pattern may match nothing; `window-*` matches the window events.
    let mut sub = srv.subscribe(&["window-*", "nothing-*"]).unwrap();
    ok(&srv, &["new-window", "-d", "-t", "s", "sleep 600"]);
    assert!(next(&mut sub).contains("window-created"));
    // A bare subscribe is every event but pane-output.
    let mut all = srv.subscribe(&[]).unwrap();
    ok(&srv, &["new-session", "-d", "-s", "b", "sleep 600"]);
    assert!(next(&mut all).contains("session-created"));
}
