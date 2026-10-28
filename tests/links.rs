//! Linked windows and session groups (B1e) beyond what the goldens
//! (`golden/b1e`) record: grofe's own limits, and the cases a recording
//! cannot hold.

mod common;

use common::TestServer;

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .trim_end()
        .to_string()
}

fn err(srv: &TestServer, args: &[&str]) -> String {
    let e = srv.cmd(args).expect_err("the command should fail");
    e.trim_end().to_string()
}

fn server() -> TestServer {
    let srv = TestServer::start();
    ok(&srv, &["set-option", "-g", "status", "off"]);
    srv
}

const SLEEP: &str = "sleep 300";

fn session(srv: &TestServer, name: &str) {
    ok(
        srv,
        &[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            SLEEP,
        ],
    );
}

fn window(srv: &TestServer, session: &str) {
    ok(
        srv,
        &["new-window", "-d", "-t", &format!("{session}:"), SLEEP],
    );
}

/// Every window as `session:index @id linked`, `*` on a current one.
fn windows(srv: &TestServer, args: &[&str]) -> Vec<String> {
    let fmt = "#{session_name}:#{window_index} #{window_id} #{window_linked}#{?window_active, *,}";
    let mut argv = vec!["list-windows"];
    argv.extend_from_slice(args);
    argv.extend(["-F", fmt]);
    ok(srv, &argv).lines().map(str::to_string).collect()
}

fn groups(srv: &TestServer) -> Vec<String> {
    let fmt = "#{session_name} #{session_grouped} #{session_group} [#{session_group_list}]";
    ok(srv, &["list-sessions", "-F", fmt])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_window_is_linked_into_a_session_once() {
    let srv = server();
    session(&srv, "a");
    session(&srv, "b");
    ok(&srv, &["link-window", "-d", "-s", "a:1", "-t", "b:5"]);
    // tmux links it twice; grofe keys a session's windows by window.
    let e = err(&srv, &["link-window", "-d", "-s", "a:1", "-t", "b:7"]);
    assert_eq!(e, "window is already in that session");
    assert_eq!(
        windows(&srv, &["-a"]),
        ["a:1 @0 1 *", "b:1 @1 0 *", "b:5 @0 1"]
    );
}

#[test]
fn the_current_window_that_goes_hands_over_to_the_last_window_else_the_one_before() {
    let srv = server();
    session(&srv, "a");
    session(&srv, "b");
    window(&srv, "a");
    window(&srv, "a");
    ok(&srv, &["link-window", "-d", "-s", "a:2", "-t", "b:2"]);
    ok(&srv, &["link-window", "-d", "-s", "a:3", "-t", "b:3"]);
    // b: 1, 2, 3 with 1 current; selecting 3 then 2 makes 3 its last.
    ok(&srv, &["select-window", "-t", "b:3"]);
    ok(&srv, &["select-window", "-t", "b:2"]);
    ok(&srv, &["unlink-window", "-t", "b:2"]);
    assert_eq!(
        windows(&srv, &["-t", "b:"]),
        ["b:1 @1 0", "b:3 @3 1 *"],
        "the last window took over"
    );
    // No last window left: the one before, and past the first it wraps.
    ok(&srv, &["select-window", "-t", "b:1"]);
    ok(&srv, &["unlink-window", "-k", "-t", "b:1"]);
    assert_eq!(windows(&srv, &["-t", "b:"]), ["b:3 @3 1 *"]);
}

#[test]
fn the_last_unlink_of_a_sessions_windows_closes_the_session() {
    let srv = server();
    session(&srv, "a");
    session(&srv, "b");
    ok(&srv, &["link-window", "-d", "-s", "a:1", "-t", "b:5"]);
    ok(&srv, &["unlink-window", "-k", "-t", "b:1"]);
    ok(&srv, &["unlink-window", "-t", "b:5"]);
    assert_eq!(ok(&srv, &["list-sessions", "-F", "#{session_name}"]), "a");
    assert_eq!(windows(&srv, &["-a"]), ["a:1 @0 0 *"]);
}

#[test]
fn a_group_lives_on_with_one_member_and_goes_with_the_last() {
    let srv = server();
    session(&srv, "a");
    ok(&srv, &["new-session", "-d", "-s", "b", "-t", "a"]);
    ok(&srv, &["new-session", "-d", "-s", "c", "-t", "b"]);
    assert_eq!(
        groups(&srv),
        ["a 1 a [a,b,c]", "b 1 a [a,b,c]", "c 1 a [a,b,c]"]
    );
    ok(&srv, &["kill-session", "-t", "a"]);
    ok(&srv, &["kill-session", "-t", "b"]);
    // The group stays with its one member, as tmux's (recorded).
    assert_eq!(groups(&srv), ["c 1 a [c]"]);
    // A new session in a group with that name joins it again.
    ok(&srv, &["new-session", "-d", "-s", "d", "-t", "a"]);
    assert_eq!(groups(&srv), ["c 1 a [c,d]", "d 1 a [c,d]"]);
}

#[test]
fn emptying_one_member_empties_the_group() {
    let srv = server();
    session(&srv, "a");
    ok(&srv, &["new-session", "-d", "-s", "b", "-t", "a"]);
    session(&srv, "z");
    ok(&srv, &["link-window", "-d", "-s", "a:1", "-t", "z:4"]);
    // The window is in z too: leaving a and its group is not the last link.
    ok(&srv, &["unlink-window", "-t", "a:1"]);
    assert_eq!(ok(&srv, &["list-sessions", "-F", "#{session_name}"]), "z");
    assert_eq!(windows(&srv, &["-a"]), ["z:1 @2 0 *", "z:4 @0 0"]);
}

#[test]
fn the_window_made_in_one_member_is_current_only_there() {
    let srv = server();
    session(&srv, "a");
    ok(&srv, &["new-session", "-d", "-s", "b", "-t", "a"]);
    ok(&srv, &["new-window", "-t", "b:"]);
    assert_eq!(
        windows(&srv, &["-a"]),
        ["a:1 @0 1 *", "a:2 @2 1", "b:1 @0 1", "b:2 @2 1 *"]
    );
    // Selecting in one member leaves the other's current window alone.
    ok(&srv, &["select-window", "-t", "a:2"]);
    assert_eq!(
        windows(&srv, &["-a"]),
        ["a:1 @0 1", "a:2 @2 1 *", "b:1 @0 1", "b:2 @2 1 *"]
    );
}

#[test]
fn a_grouped_session_has_the_group_formats_and_no_window_of_its_own() {
    let srv = server();
    session(&srv, "a");
    let name = ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_name}",
            "-t",
            "a",
        ],
    );
    assert_eq!(name, "a-1");
    assert_eq!(
        ok(
            &srv,
            &[
                "display-message",
                "-p",
                "-t",
                "a-1:",
                "#{session_windows} #{session_group_size}"
            ]
        ),
        "1 2"
    );
    // Window and pane ids burnt by the dropped window, as tmux (recorded).
    assert_eq!(
        ok(
            &srv,
            &[
                "new-window",
                "-d",
                "-t",
                "a:",
                "-P",
                "-F",
                "#{window_id} #{pane_id}"
            ]
        ),
        "@2 %2"
    );
    let e = err(&srv, &["new-session", "-d", "-t", "a", "-n", "x"]);
    assert_eq!(e, "command or window name given with target");
}
