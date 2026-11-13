//! Scratchpads end to end: a real server driven through the CLI
//! (`new-scratchpad`, `toggle-scratchpad`, `show-scratchpad`,
//! `list-scratchpads`), the `scratchpad-history` option and the default keys.

mod common;

use std::fs;
use std::time::Duration;

use common::TestServer;

const WAIT: Duration = Duration::from_secs(5);

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args)
        .unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
}

/// A detached session `name` whose first pane runs `sleep 60`.
fn session(srv: &TestServer, name: &str) {
    ok(
        srv,
        &[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "100",
            "-y",
            "30",
            "sleep 60",
        ],
    );
}

/// The pane ids of `target` (a window or session), in layout order.
fn pane_ids(srv: &TestServer, target: &str) -> Vec<String> {
    ok(srv, &["list-panes", "-t", target, "-F", "#{pane_id}"])
        .lines()
        .map(str::to_string)
        .collect()
}

fn all_pane_ids(srv: &TestServer) -> Vec<String> {
    ok(srv, &["list-panes", "-a", "-F", "#{pane_id}"])
        .lines()
        .map(str::to_string)
        .collect()
}

fn scratchpads(srv: &TestServer) -> Vec<String> {
    ok(srv, &["list-scratchpads"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The pane id of `target`'s active pane.
fn active_id(srv: &TestServer, target: &str) -> String {
    ok(srv, &["display-message", "-p", "-t", target, "#{pane_id}"])
        .trim()
        .to_string()
}

fn floating(srv: &TestServer, pane: &str) -> String {
    ok(
        srv,
        &["display-message", "-p", "-t", pane, "#{pane_floating_flag}"],
    )
    .trim()
    .to_string()
}

/// Opens a scratchpad running `cmd` in `target` and returns its pane id.
fn new_pad(srv: &TestServer, target: &str, cmd: &str) -> String {
    ok(srv, &["new-scratchpad", "-t", target, cmd]);
    active_id(srv, target)
}

#[test]
fn toggle_round_trip() {
    let srv = TestServer::start();
    session(&srv, "a");
    let base = pane_ids(&srv, "a");
    let pad = new_pad(&srv, "a", "sleep 60");
    assert!(pad.starts_with('%'), "{pad}");
    assert_eq!(pane_ids(&srv, "a").len(), 2);
    assert_eq!(floating(&srv, &pad), "1");

    // Visible scratchpads are not in the history.
    assert_eq!(scratchpads(&srv), Vec::<String>::new());

    // Hide.
    ok(&srv, &["toggle-scratchpad", "-t", &pad]);
    assert_eq!(pane_ids(&srv, "a"), base);
    assert_eq!(scratchpads(&srv), vec![format!("0: {pad} sleep 60 · home")]);

    // Show again: active and floating.
    ok(&srv, &["toggle-scratchpad", "-t", "a:"]);
    assert_eq!(scratchpads(&srv), Vec::<String>::new());
    assert_eq!(active_id(&srv, "a"), pad);
    assert_eq!(floating(&srv, &pad), "1");
    assert_eq!(pane_ids(&srv, "a").len(), 2);
}

#[test]
fn toggle_with_none_opens_one() {
    let srv = TestServer::start();
    session(&srv, "a");
    assert_eq!(pane_ids(&srv, "a").len(), 1);
    ok(&srv, &["toggle-scratchpad", "-t", "a:"]);
    let panes = pane_ids(&srv, "a");
    assert_eq!(panes.len(), 2);
    let pad = active_id(&srv, "a");
    assert!(panes.contains(&pad));
    assert_eq!(floating(&srv, &pad), "1");
    assert_eq!(scratchpads(&srv), Vec::<String>::new());
}

#[test]
fn show_into_another_session() {
    let srv = TestServer::start();
    session(&srv, "a");
    session(&srv, "b");
    let pad = new_pad(&srv, "a", "sleep 60");
    ok(&srv, &["toggle-scratchpad", "-t", &pad]);
    assert!(!all_pane_ids(&srv).contains(&pad));

    ok(&srv, &["show-scratchpad", "-t", "b:", &pad]);
    assert!(pane_ids(&srv, "b").contains(&pad));
    assert!(!pane_ids(&srv, "a").contains(&pad));
    assert_eq!(active_id(&srv, "b"), pad);
    assert_eq!(floating(&srv, &pad), "1");
    assert_eq!(scratchpads(&srv), Vec::<String>::new());
}

#[test]
fn history_limit_from_option() {
    let srv = TestServer::start();
    session(&srv, "a");
    ok(&srv, &["set", "-g", "scratchpad-history", "2"]);
    let mut pads = Vec::new();
    for i in 0..3 {
        let pad = new_pad(&srv, "a", &format!("sleep 6{i}"));
        ok(&srv, &["toggle-scratchpad", "-t", &pad]);
        pads.push(pad);
    }
    // Newest first; the oldest is evicted and gone everywhere.
    assert_eq!(
        scratchpads(&srv),
        vec![
            format!("0: {} sleep 62 · home", pads[2]),
            format!("1: {} sleep 61 · home", pads[1]),
        ]
    );
    assert!(!all_pane_ids(&srv).contains(&pads[0]));
    assert!(
        srv.cmd(&["show-scratchpad", "-t", "a:", &pads[0]]).is_err(),
        "an evicted scratchpad cannot be shown"
    );
}

#[test]
fn unfloat_makes_it_normal() {
    let srv = TestServer::start();
    session(&srv, "a");
    let pad = new_pad(&srv, "a", "sleep 60");
    ok(&srv, &["unfloat-pane", "-t", &pad]);
    assert_eq!(floating(&srv, &pad), "0");

    // The old pane is no scratchpad, so toggling opens a new one.
    ok(&srv, &["toggle-scratchpad", "-t", &pad]);
    let new = active_id(&srv, "a");
    assert_ne!(new, pad);
    assert_eq!(floating(&srv, &new), "1");
    assert_eq!(floating(&srv, &pad), "0");
    assert_eq!(pane_ids(&srv, "a").len(), 3);
    assert_eq!(scratchpads(&srv), Vec::<String>::new());
}

#[test]
fn kill_window_keeps_scratchpad() {
    let srv = TestServer::start();
    session(&srv, "a");
    ok(&srv, &["new-window", "-t", "a:", "-n", "w2", "sleep 60"]);
    let pad = new_pad(&srv, "a:w2", "/bin/sh");
    ok(&srv, &["kill-window", "-t", "a:w2"]);
    assert!(!all_pane_ids(&srv).contains(&pad));
    let list = scratchpads(&srv);
    assert_eq!(list.len(), 1, "{list:?}");
    assert!(list[0].starts_with(&format!("0: {pad} ")), "{list:?}");

    ok(&srv, &["show-scratchpad", "-t", "a:", &pad]);
    assert!(pane_ids(&srv, "a").contains(&pad));
    ok(
        &srv,
        &["send-keys", "-t", &pad, "echo scratch-alive", "Enter"],
    );
    srv.wait_for(
        || srv.capture(&pad).lines().any(|l| l == "scratch-alive"),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: {:?}", srv.capture(&pad)));
}

#[test]
fn hidden_process_exit_leaves_history() {
    let srv = TestServer::start();
    session(&srv, "a");
    let pad = new_pad(&srv, "a", "sleep 1");
    ok(&srv, &["toggle-scratchpad", "-t", &pad]);
    assert_eq!(scratchpads(&srv).len(), 1);
    srv.wait_for(|| scratchpads(&srv).is_empty(), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", scratchpads(&srv)));
    assert!(!all_pane_ids(&srv).contains(&pad));
}

#[test]
fn toml_option() {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), "scratchpad-history = 3\n").expect("write config");
    // Auto-starts the server, which loads the config.
    session(&srv, "a");
    assert_eq!(
        ok(&srv, &["show-options", "-g", "scratchpad-history"]),
        "scratchpad-history 3\n"
    );
}

#[test]
fn default_history_is_ten() {
    let srv = TestServer::start();
    session(&srv, "a");
    assert_eq!(
        ok(&srv, &["show-options", "-gv", "scratchpad-history"]),
        "10\n"
    );
}

#[test]
fn keys_listed() {
    let srv = TestServer::start();
    let keys = ok(&srv, &["list-keys", "-N", "-T", "prefix"]);
    for (key, note) in [
        ("S", "Hide or show a scratchpad"),
        ("a", "Open a scratchpad"),
        ("A", "Choose a scratchpad from a list"),
    ] {
        assert!(
            keys.lines().any(|l| l.contains(key) && l.contains(note)),
            "{key}: {keys}"
        );
    }
    let all = ok(&srv, &["list-keys", "-T", "prefix"]);
    for cmd in ["toggle-scratchpad", "new-scratchpad", "choose-scratchpad"] {
        assert!(all.lines().any(|l| l.contains(cmd)), "{cmd}: {all}");
    }
}
