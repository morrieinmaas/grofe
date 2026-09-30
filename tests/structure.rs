//! Structure verbs (TC.8): each verb's effect checked through `list-panes
//! -F` geometry, against goldens recorded on tmux 3.7b (`tmux -L
//! grofe-tc8 -f /dev/null`, `status off`, `base-index 0`, sessions made
//! detached with `-x 200 -y 50`). Pane and window ids are compared too:
//! both servers number them from `%0` and `@0` in creation order.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::time::Duration;

use common::TestServer;
use grofe::server::layout::{Insert, LayoutTree, Preset};
use grofe::types::{Dir, PaneId, Rect, SplitSize};

const WAIT: Duration = Duration::from_secs(5);
const SLEEP: &str = "sleep 300";

/// Every pane of `target`'s window in index order, as the goldens were
/// recorded: `%id left,top WxH`, with ` *` on the active pane.
fn panes(srv: &TestServer, target: &str) -> Vec<String> {
    let fmt = "#{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}#{?pane_active, *,}";
    srv.cmd(&["list-panes", "-t", target, "-F", fmt])
        .unwrap_or_else(|e| panic!("list-panes -t {target}: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// `session`'s windows in index order: `index @id`, ` *` on the current.
fn windows(srv: &TestServer, session: &str) -> Vec<String> {
    let fmt = "#{window_index} #{window_id}#{?window_active, *,}";
    srv.cmd(&["list-windows", "-t", session, "-F", fmt])
        .unwrap_or_else(|e| panic!("list-windows -t {session}: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap_or_else(|e| panic!("display-message {fmt}: {e}"))
        .trim_end()
        .to_string()
}

fn ok(srv: &TestServer, args: &[&str]) {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
}

fn err(srv: &TestServer, args: &[&str]) -> String {
    let e = srv.cmd(args).expect_err("the command should fail");
    e.trim_end().to_string()
}

fn server() -> TestServer {
    let srv = TestServer::start();
    ok(&srv, &["set-option", "-g", "base-index", "0"]);
    ok(&srv, &["set-option", "-g", "pane-base-index", "0"]);
    ok(&srv, &["set-option", "-g", "status", "off"]);
    srv
}

/// Session `s`, 200x50: `%0 | %1 / %2`, active `%2`.
fn three_panes() -> TestServer {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    ok(&srv, &["split-window", "-h", "-t", "s", SLEEP]);
    ok(&srv, &["split-window", "-v", "-t", "s", SLEEP]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%0 0,0 100x50", "%1 101,0 99x25", "%2 101,26 99x24 *"]
    );
    srv
}

/// [`three_panes`] plus window 5, `%3 | %4`, active `%4`.
fn with_window_five() -> TestServer {
    let srv = three_panes();
    ok(&srv, &["new-window", "-d", "-t", "s:5", SLEEP]);
    ok(&srv, &["split-window", "-h", "-t", "s:5", SLEEP]);
    assert_eq!(panes(&srv, "s:5"), ["%3 0,0 100x50", "%4 101,0 99x50 *"]);
    srv
}

#[test]
fn join_pane_d_s_t_matches_tmux() {
    let srv = with_window_five();
    ok(&srv, &["join-pane", "-d", "-s", "%4", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%0 0,0 100x25",
            "%4 0,26 100x24",
            "%1 101,0 99x25",
            "%2 101,26 99x24 *"
        ]
    );
    assert_eq!(panes(&srv, "s:5"), ["%3 0,0 200x50 *"]);
    assert_eq!(windows(&srv, "s"), ["0 @0 *", "5 @1"]);
}

#[test]
fn join_pane_without_d_makes_the_pane_active_and_its_window_current() {
    let srv = with_window_five();
    ok(&srv, &["select-window", "-t", "s:5"]);
    ok(&srv, &["join-pane", "-s", "%4", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%0 0,0 100x25",
            "%4 0,26 100x24 *",
            "%1 101,0 99x25",
            "%2 101,26 99x24"
        ]
    );
    assert_eq!(windows(&srv, "s"), ["0 @0 *", "5 @1"]);
}

#[test]
fn join_pane_flags_match_tmux() {
    let cases: [(&[&str], [&str; 4]); 4] = [
        (
            &["-h", "-t", "%1"],
            [
                "%0 0,0 100x50",
                "%1 101,0 49x25",
                "%4 151,0 49x25 *",
                "%2 101,26 99x24",
            ],
        ),
        (
            &["-b", "-h", "-l", "30", "-t", "%1"],
            [
                "%0 0,0 100x50",
                "%1 132,0 68x25",
                "%4 101,0 30x25 *",
                "%2 101,26 99x24",
            ],
        ),
        (
            &["-f", "-h", "-t", "%1"],
            [
                "%0 0,0 50x50",
                "%1 51,0 49x25",
                "%4 101,0 99x50 *",
                "%2 51,26 49x24",
            ],
        ),
        // The opensessions sidebar's form: a full-height pane on the left.
        (
            &["-b", "-f", "-d", "-h", "-l", "40", "-t", "%1"],
            [
                "%0 41,0 79x50",
                "%1 121,0 79x25",
                "%4 0,0 40x50",
                "%2 121,26 79x24 *",
            ],
        ),
    ];
    for (flags, want) in cases {
        let srv = with_window_five();
        let mut args = vec!["join-pane", "-s", "%4"];
        args.extend_from_slice(flags);
        ok(&srv, &args);
        assert_eq!(panes(&srv, "s:0"), want, "{flags:?}");
    }
}

#[test]
fn joining_every_pane_away_closes_the_window() {
    let srv = with_window_five();
    ok(&srv, &["join-pane", "-d", "-s", "%3", "-t", "%0"]);
    ok(&srv, &["join-pane", "-d", "-s", "%4", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%0 0,0 100x12",
            "%4 0,13 100x12",
            "%3 0,26 100x24",
            "%1 101,0 99x25",
            "%2 101,26 99x24 *"
        ]
    );
    assert_eq!(windows(&srv, "s"), ["0 @0 *"]);
}

#[test]
fn join_pane_within_a_window_and_onto_itself() {
    let srv = three_panes();
    ok(&srv, &["join-pane", "-s", "%0", "-t", "%1"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%1 0,0 200x12", "%0 0,13 200x12 *", "%2 0,26 200x24"]
    );
    assert_eq!(
        err(&srv, &["join-pane", "-s", "%1", "-t", "%1"]),
        "source and target panes must be different"
    );
}

#[test]
fn move_pane_is_join_pane() {
    let srv = with_window_five();
    ok(&srv, &["move-pane", "-d", "-s", "%4", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%0 0,0 100x25",
            "%4 0,26 100x24",
            "%1 101,0 99x25",
            "%2 101,26 99x24 *"
        ]
    );
}

#[test]
fn swap_pane_matches_tmux() {
    let cases: [(&[&str], [&str; 3]); 5] = [
        (
            &["-D", "-t", "%0"],
            ["%1 0,0 100x50", "%0 101,0 99x25 *", "%2 101,26 99x24"],
        ),
        (
            &["-D"],
            ["%2 0,0 100x50 *", "%1 101,0 99x25", "%0 101,26 99x24"],
        ),
        (
            &["-U"],
            ["%0 0,0 100x50", "%2 101,0 99x25 *", "%1 101,26 99x24"],
        ),
        (
            &["-s", "%0", "-t", "%2"],
            ["%2 0,0 100x50 *", "%1 101,0 99x25", "%0 101,26 99x24"],
        ),
        (
            &["-t", "%0"],
            ["%2 0,0 100x50", "%1 101,0 99x25", "%0 101,26 99x24 *"],
        ),
    ];
    for (flags, want) in cases {
        let srv = three_panes();
        let mut args = vec!["swap-pane"];
        args.extend_from_slice(flags);
        ok(&srv, &args);
        assert_eq!(panes(&srv, "s:0"), want, "{flags:?}");
    }
}

#[test]
fn swap_pane_between_windows_swaps_their_cells() {
    let srv = with_window_five();
    ok(&srv, &["swap-pane", "-s", "%3", "-t", "%1"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%0 0,0 100x50", "%3 101,0 99x25 *", "%2 101,26 99x24"]
    );
    assert_eq!(panes(&srv, "s:5"), ["%1 0,0 100x50 *", "%4 101,0 99x50"]);
}

/// Session `o`, then session `s` (the current one: the most recent) with
/// windows 0, 3 (current) and 5.
fn three_windows() -> TestServer {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "o",
            "-x",
            "100",
            "-y",
            "30",
            SLEEP,
        ],
    );
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    ok(&srv, &["new-window", "-t", "s:3", SLEEP]);
    ok(&srv, &["new-window", "-d", "-t", "s:5", SLEEP]);
    assert_eq!(windows(&srv, "o"), ["0 @0 *"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "3 @2 *", "5 @3"]);
    srv
}

#[test]
fn swap_window_matches_tmux() {
    let srv = three_windows();
    ok(&srv, &["swap-window", "-s", "s:0", "-t", "s:5"]);
    assert_eq!(windows(&srv, "s"), ["0 @3", "3 @2 *", "5 @1"]);

    // The current window's place stays current.
    let srv = three_windows();
    ok(&srv, &["swap-window", "-t", "s:5"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "3 @3 *", "5 @2"]);

    let srv = three_windows();
    ok(&srv, &["swap-window", "-s", "s:3", "-t", "o:0"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "3 @0 *", "5 @3"]);
    assert_eq!(windows(&srv, "o"), ["0 @2 *"]);
    // A window keeps its size in another session, as in tmux.
    assert_eq!(
        display(&srv, "o:0", "#{window_width}x#{window_height}"),
        "200x50"
    );
}

#[test]
fn break_pane_matches_tmux() {
    let srv = three_panes();
    ok(&srv, &["new-window", "-d", "-t", "s:5", SLEEP]);
    ok(&srv, &["break-pane", "-d", "-n", "foo", "-s", "%1"]);
    assert_eq!(panes(&srv, "s:0"), ["%0 0,0 100x50", "%2 101,0 99x50 *"]);
    assert_eq!(panes(&srv, "s:1"), ["%1 0,0 200x50 *"]);
    assert_eq!(windows(&srv, "s"), ["0 @0 *", "1 @2", "5 @1"]);
    assert_eq!(display(&srv, "s:1", "#{window_name}"), "foo");
    let rename = srv
        .cmd(&["show-options", "-w", "-t", "s:1", "automatic-rename"])
        .expect("show-options");
    assert_eq!(rename.trim_end(), "automatic-rename off");

    // Without -d the new window becomes current; -t picks its index.
    ok(&srv, &["break-pane", "-s", "%0", "-t", "s:7"]);
    assert_eq!(windows(&srv, "s"), ["0 @0", "1 @2", "5 @1", "7 @3 *"]);
    assert_eq!(panes(&srv, "s:0"), ["%2 0,0 200x50 *"]);
    assert_eq!(
        err(&srv, &["break-pane", "-s", "%4", "-t", "s:0"]),
        "can't find pane: %4"
    );
}

#[test]
fn break_pane_refuses_a_used_index_and_moves_a_lone_pane_window() {
    let srv = three_panes();
    ok(&srv, &["new-window", "-d", "-t", "s:5", SLEEP]);
    assert_eq!(
        err(&srv, &["break-pane", "-s", "%1", "-t", "s:5"]),
        "index in use: 5"
    );
    assert_eq!(
        err(&srv, &["break-pane", "-s", "%1", "-t", "s:0"]),
        "index in use: 0"
    );
    assert_eq!(panes(&srv, "s:0").len(), 3);
    // A pane alone in its window moves the window, as tmux 3.7b does.
    ok(&srv, &["break-pane", "-s", "%3"]);
    assert_eq!(windows(&srv, "s"), ["0 @0", "1 @1 *"]);
    assert_eq!(panes(&srv, "s:1"), ["%3 0,0 200x50 *"]);
}

#[test]
fn resize_window_matches_tmux() {
    let cases: [(&[&str], [&str; 3]); 4] = [
        (
            &["-x", "100", "-y", "30"],
            ["%0 0,0 50x30", "%1 51,0 49x15", "%2 51,16 49x14 *"],
        ),
        (
            &["-x", "300"],
            ["%0 0,0 150x50", "%1 151,0 149x25", "%2 151,26 149x24 *"],
        ),
        (
            &["-y", "20"],
            ["%0 0,0 100x20", "%1 101,0 99x10", "%2 101,11 99x9 *"],
        ),
        (
            &["-x", "5", "-y", "3"],
            ["%0 0,0 2x3", "%1 3,0 2x1", "%2 3,2 2x1 *"],
        ),
    ];
    for (flags, want) in cases {
        let srv = three_panes();
        let mut args = vec!["resize-window", "-t", "s"];
        args.extend_from_slice(flags);
        ok(&srv, &args);
        assert_eq!(panes(&srv, "s:0"), want, "{flags:?}");
    }
    // Only that window: a new one takes the session's size.
    let srv = three_panes();
    ok(&srv, &["resize-window", "-t", "s", "-x", "100", "-y", "30"]);
    ok(&srv, &["new-window", "-t", "s", SLEEP]);
    assert_eq!(
        display(&srv, "s", "#{window_width}x#{window_height}"),
        "200x50"
    );
    // -A with no client showing the session: the session's size.
    ok(&srv, &["resize-window", "-A", "-t", "s:0"]);
    assert_eq!(panes(&srv, "s:0")[0], "%0 0,0 100x50");
}

#[test]
fn last_pane_matches_tmux() {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    assert_eq!(err(&srv, &["last-pane", "-t", "s"]), "no last pane");
    ok(&srv, &["split-window", "-t", "s", SLEEP]);
    let active = || display(&srv, "s", "#{pane_id}");
    assert_eq!(active(), "%1");
    ok(&srv, &["last-pane", "-t", "s"]);
    assert_eq!(active(), "%0");
    ok(&srv, &["last-pane", "-t", "s"]);
    assert_eq!(active(), "%1");

    // The stack survives a pane leaving it.
    ok(&srv, &["split-window", "-t", "s", SLEEP]);
    ok(&srv, &["select-pane", "-t", "%0"]);
    ok(&srv, &["kill-pane", "-t", "%2"]);
    assert_eq!(active(), "%0");
    ok(&srv, &["last-pane", "-t", "s"]);
    assert_eq!(active(), "%1");

    // It leaves zoom.
    ok(&srv, &["resize-pane", "-Z", "-t", "%0"]);
    assert_eq!(display(&srv, "s", "#{window_zoomed_flag}"), "1");
    ok(&srv, &["last-pane", "-t", "s"]);
    assert_eq!(
        display(&srv, "s", "#{window_zoomed_flag} #{pane_id}"),
        "0 %1"
    );
}

#[test]
fn rotate_window_matches_tmux() {
    let srv = three_panes();
    ok(&srv, &["rotate-window", "-D", "-t", "s"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%2 0,0 100x50", "%0 101,0 99x25", "%1 101,26 99x24 *"]
    );
    let srv = three_panes();
    ok(&srv, &["rotate-window", "-t", "s"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%1 0,0 100x50", "%2 101,0 99x25", "%0 101,26 99x24 *"]
    );
}

#[test]
fn move_window_matches_tmux() {
    let srv = three_windows();
    ok(&srv, &["move-window", "-s", "s:0", "-t", "s:9"]);
    assert_eq!(windows(&srv, "s"), ["3 @2", "5 @3", "9 @1 *"]);

    let srv = three_windows();
    assert_eq!(
        err(&srv, &["move-window", "-s", "s:0", "-t", "s:5"]),
        "index in use: 5"
    );
    assert_eq!(
        err(&srv, &["move-window", "-s", "s:0", "-t", "s:0"]),
        "same index: 0"
    );
    assert_eq!(windows(&srv, "s"), ["0 @1", "3 @2 *", "5 @3"]);

    // -a: after -t, moving the windows in the way up.
    let srv = three_windows();
    ok(&srv, &["move-window", "-a", "-s", "s:0", "-t", "s:3"]);
    assert_eq!(windows(&srv, "s"), ["3 @2", "4 @1 *", "5 @3"]);
    let srv = three_windows();
    ok(&srv, &["new-window", "-d", "-t", "s:4", SLEEP]);
    ok(&srv, &["move-window", "-a", "-s", "s:0", "-t", "s:3"]);
    assert_eq!(windows(&srv, "s"), ["3 @2", "4 @1 *", "5 @4", "6 @3"]);

    // -r renumbers from base-index, in order.
    let srv = three_windows();
    ok(&srv, &["move-window", "-r"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "1 @2 *", "2 @3"]);
    let srv = three_windows();
    ok(&srv, &["move-window", "-r", "-s", "s:0", "-t", "s"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "1 @2 *", "2 @3"]);

    // To another session: its first free index, current there; the
    // source session's current becomes its last window.
    let srv = three_windows();
    ok(&srv, &["move-window", "-s", "s:3", "-t", "o"]);
    assert_eq!(windows(&srv, "o"), ["0 @0", "1 @2 *"]);
    assert_eq!(windows(&srv, "s"), ["0 @1 *", "5 @3"]);

    // No -s: the current window; no -t: the first free index.
    let srv = three_windows();
    ok(&srv, &["move-window", "-t", "s:8"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "5 @3", "8 @2 *"]);
    let srv = three_windows();
    ok(&srv, &["move-window", "-s", "s:5"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "1 @3 *", "3 @2"]);

    // A session's only window moving away closes the session.
    let srv = three_windows();
    ok(&srv, &["move-window", "-s", "o:0", "-t", "s:9"]);
    assert_eq!(windows(&srv, "s"), ["0 @1", "3 @2", "5 @3", "9 @0 *"]);
    assert!(srv.cmd(&["has-session", "-t", "o"]).is_err());
}

/// Four panes made by three plain splits, then `next-layout` seven times:
/// every preset in tmux's order, and back round.
#[test]
fn next_layout_cycles_the_presets_as_tmux_does() {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    for _ in 0..3 {
        ok(&srv, &["split-window", "-t", "s", SLEEP]);
    }
    let geometry = |srv: &TestServer| -> String {
        panes(srv, "s:0")
            .iter()
            .map(|p| p.trim_end_matches(" *").to_string() + ";")
            .collect()
    };
    let cycle = [
        "%0 0,0 50x50;%1 51,0 49x50;%2 101,0 49x50;%3 151,0 49x50;",
        "%0 0,0 200x12;%1 0,13 200x12;%2 0,26 200x12;%3 0,39 200x11;",
        "%0 0,0 200x24;%1 0,25 66x25;%2 67,25 66x25;%3 134,25 66x25;",
        "%0 0,26 200x24;%1 0,0 66x25;%2 67,0 66x25;%3 134,0 66x25;",
        "%0 0,0 80x50;%1 81,0 119x16;%2 81,17 119x16;%3 81,34 119x16;",
        "%0 120,0 80x50;%1 0,0 119x16;%2 0,17 119x16;%3 0,34 119x16;",
        "%0 0,0 99x24;%1 100,0 100x24;%2 0,25 99x25;%3 100,25 100x25;",
        "%0 0,0 50x50;%1 51,0 49x50;%2 101,0 49x50;%3 151,0 49x50;",
    ];
    for (i, want) in cycle.iter().enumerate() {
        ok(&srv, &["next-layout", "-t", "s"]);
        assert_eq!(geometry(&srv), *want, "next-layout {}", i + 1);
    }
    // select-layout sets where it goes on from.
    ok(&srv, &["select-layout", "-t", "s", "even-vertical"]);
    ok(&srv, &["next-layout", "-t", "s"]);
    assert_eq!(geometry(&srv), cycle[2]);
}

#[test]
fn respawn_window_keeps_the_first_pane_only() {
    let srv = three_panes();
    ok(&srv, &["select-pane", "-t", "%1"]);
    assert_eq!(
        err(&srv, &["respawn-window", "-t", "s"]),
        "respawn window failed: window s:0 still active"
    );
    assert_eq!(panes(&srv, "s:0").len(), 3);
    ok(&srv, &["respawn-window", "-k", "-t", "s", "sleep 301"]);
    assert_eq!(panes(&srv, "s:0"), ["%0 0,0 200x50 *"]);
    let start = display(&srv, "s", "#{pane_start_command}");
    assert!(start.contains("sleep 301"), "{start}");
}

#[test]
fn resize_pane_x_y_match_tmux() {
    let cases: [(&[&str], [&str; 3]); 5] = [
        (
            &["-t", "%0", "-x", "60"],
            ["%0 0,0 60x50", "%1 61,0 139x25", "%2 61,26 139x24 *"],
        ),
        (
            &["-t", "%1", "-y", "10"],
            ["%0 0,0 100x50", "%1 101,0 99x10", "%2 101,11 99x39 *"],
        ),
        (
            &["-t", "%2", "-x", "30", "-y", "40"],
            ["%0 0,0 169x50", "%1 170,0 30x9", "%2 170,10 30x40 *"],
        ),
        (
            &["-t", "%0", "-x", "25%"],
            ["%0 0,0 50x50", "%1 51,0 149x25", "%2 51,26 149x24 *"],
        ),
        // -x first, then the direction.
        (
            &["-t", "%1", "-L", "-x", "60"],
            ["%0 0,0 138x50", "%1 139,0 61x25", "%2 139,26 61x24 *"],
        ),
    ];
    for (flags, want) in cases {
        let srv = three_panes();
        let mut args = vec!["resize-pane"];
        args.extend_from_slice(flags);
        ok(&srv, &args);
        assert_eq!(panes(&srv, "s:0"), want, "{flags:?}");
    }
}

#[test]
fn select_layout_e_spreads_the_panes_out() {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    ok(&srv, &["split-window", "-h", "-l", "20", "-t", "s", SLEEP]);
    ok(&srv, &["split-window", "-h", "-l", "30", "-t", "%0", SLEEP]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%0 0,0 148x50", "%2 149,0 30x50 *", "%1 180,0 20x50"]
    );
    ok(&srv, &["select-layout", "-E", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%0 0,0 66x50", "%2 67,0 66x50 *", "%1 134,0 66x50"]
    );
}

/// The acceptance case: `main-vertical` with `main-pane-width 50%` over
/// four panes gives a half-width main pane (tmux 3.7b: half of the 199
/// columns left after the border, rounded down).
#[test]
fn main_vertical_takes_main_pane_width_50_percent() {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    for _ in 0..3 {
        ok(&srv, &["split-window", "-t", "s", SLEEP]);
    }
    ok(
        &srv,
        &["set-option", "-w", "-t", "s", "main-pane-width", "50%"],
    );
    ok(&srv, &["select-layout", "-t", "s", "main-vertical"]);
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%0 0,0 99x50",
            "%1 100,0 100x16",
            "%2 100,17 100x16",
            "%3 100,34 100x16 *"
        ]
    );
    ok(
        &srv,
        &["set-option", "-w", "-t", "s", "main-pane-width", "150"],
    );
    ok(&srv, &["select-layout", "-t", "s", "main-vertical"]);
    assert_eq!(panes(&srv, "s:0")[0], "%0 0,0 150x50");
}

/// `next-window -a` / `previous-window -a` go to the next or previous
/// window with a bell, wrapping; with none it is an error (tmux 3.7b).
#[test]
fn next_and_previous_window_with_an_alert() {
    let srv = server();
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
            SLEEP,
        ],
    );
    for i in 1..=3 {
        ok(&srv, &["new-window", "-d", "-t", &format!("s:{i}"), SLEEP]);
    }
    assert_eq!(
        err(&srv, &["next-window", "-a", "-t", "s"]),
        "no next window"
    );
    assert_eq!(
        err(&srv, &["previous-window", "-a", "-t", "s"]),
        "no previous window"
    );
    let bell = |window: &str| {
        let cmd = "printf '\\a'; sleep 300";
        ok(&srv, &["respawn-pane", "-k", "-t", window, cmd]);
        srv.wait_for(|| display(&srv, window, "#{window_bell_flag}") == "1", WAIT)
            .unwrap_or_else(|e| panic!("bell in {window}: {e}"));
    };
    let current = || display(&srv, "s", "#{window_index}");

    bell("s:2");
    ok(&srv, &["next-window", "-a", "-t", "s"]);
    assert_eq!(current(), "2");
    bell("s:1");
    ok(&srv, &["next-window", "-a", "-t", "s"]);
    assert_eq!(current(), "1", "wraps past the last window");
    bell("s:3");
    ok(&srv, &["previous-window", "-a", "-t", "s"]);
    assert_eq!(current(), "3", "wraps past the first window");
    assert_eq!(
        err(&srv, &["previous-window", "-a", "-t", "s"]),
        "no previous window"
    );
}

/// `automatic-rename` is a window option: off on one window, the others
/// keep renaming; on again for one window, only it renames.
#[test]
fn automatic_rename_is_per_window() {
    let srv = server();
    ok(
        &srv,
        &["set-option", "-g", "automatic-rename-format", "r#{pane_id}"],
    );
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
            SLEEP,
        ],
    );
    // -n turns it off for window 1 alone.
    ok(
        &srv,
        &["new-window", "-d", "-n", "keep", "-t", "s:1", SLEEP],
    );
    ok(&srv, &["new-window", "-d", "-t", "s:2", SLEEP]);
    ok(&srv, &["rename-window", "-t", "s:2", "fixed"]);
    ok(
        &srv,
        &["set-option", "-w", "-t", "s:2", "automatic-rename", "on"],
    );
    let names = || -> Vec<String> {
        srv.cmd(&["list-windows", "-t", "s", "-F", "#{window_name}"])
            .expect("list-windows")
            .lines()
            .map(str::to_string)
            .collect()
    };
    assert_eq!(names(), ["r%0", "keep", "r%2"]);

    ok(
        &srv,
        &["set-option", "-w", "-t", "s:0", "automatic-rename", "off"],
    );
    ok(
        &srv,
        &["set-option", "-g", "automatic-rename-format", "n#{pane_id}"],
    );
    assert_eq!(names(), ["r%0", "keep", "n%2"]);
}

#[test]
fn new_window_n_name_and_command() {
    let srv = server();
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
            SLEEP,
        ],
    );
    ok(
        &srv,
        &["new-window", "-d", "-n", "logs", "-t", "s:4", "sleep 302"],
    );
    assert_eq!(windows(&srv, "s"), ["0 @0 *", "4 @1"]);
    let start = display(&srv, "s:4", "#{pane_start_command}");
    assert!(start.contains("sleep 302"), "{start}");
    // The name sticks: -n turns automatic-rename off.
    assert_eq!(display(&srv, "s:4", "#{window_name}"), "logs");
}

#[test]
fn respawn_pane_k_runs_the_new_command_in_place() {
    let srv = three_panes();
    ok(&srv, &["respawn-pane", "-k", "-t", "%1", "sleep 303"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%0 0,0 100x50", "%1 101,0 99x25", "%2 101,26 99x24 *"]
    );
    let start = display(&srv, "%1", "#{pane_start_command}");
    assert!(start.contains("sleep 303"), "{start}");
}

/// Whether the rects (`%id left,top WxH`) tile a `cols`x`rows` window with
/// one-cell borders: every pane inside it and at least 1x1, no cell
/// covered twice, no uncovered 2x2 block (a gap wider than a border), and
/// a border reaching an edge only along its own line.
fn tiles(lines: &[String], cols: usize, rows: usize) -> bool {
    let mut grid = vec![0u8; cols * rows];
    for line in lines {
        let mut words = line.split(' ').skip(1);
        let (geo, size) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        let (x, y) = geo.split_once(',').unwrap_or_default();
        let (w, h) = size.split_once('x').unwrap_or_default();
        let [x, y, w, h] = [x, y, w, h].map(|n| n.parse::<usize>().unwrap_or(0));
        if w == 0 || h == 0 || x + w > cols || y + h > rows {
            return false;
        }
        for row in y..y + h {
            for col in x..x + w {
                grid[row * cols + col] += 1;
            }
        }
    }
    let free = |c: usize, r: usize| grid[r * cols + c] == 0;
    let mut ok = grid.iter().all(|n| *n <= 1);
    for r in 0..rows {
        for c in 0..cols {
            if !free(c, r) {
                continue;
            }
            if c + 1 < cols && r + 1 < rows {
                ok &= !(free(c + 1, r) && free(c, r + 1) && free(c + 1, r + 1));
            }
            if (c == 0 || c + 1 == cols) && cols > 1 {
                ok &= free(if c == 0 { 1 } else { c - 1 }, r);
            }
            if (r == 0 || r + 1 == rows) && rows > 1 {
                ok &= free(c, if r == 0 { 1 } else { r - 1 });
            }
        }
    }
    ok
}

/// T1.3's invariant through the CLI after a run of every structure verb:
/// the panes never overlap and stay inside the window.
#[test]
fn every_verb_keeps_the_window_tiled() {
    let srv = with_window_five();
    let steps: &[&[&str]] = &[
        &[
            "join-pane",
            "-b",
            "-f",
            "-h",
            "-l",
            "40",
            "-s",
            "%4",
            "-t",
            "%1",
        ],
        &["swap-pane", "-D", "-t", "%0"],
        &["rotate-window", "-D", "-t", "s:0"],
        &["resize-pane", "-t", "%2", "-x", "30", "-y", "40"],
        &["select-layout", "-E", "-t", "%1"],
        &["break-pane", "-d", "-s", "%2"],
        &["move-pane", "-h", "-s", "%3", "-t", "%0"],
        &["next-layout", "-t", "s:0"],
        &["resize-window", "-t", "s:0", "-x", "123", "-y", "37"],
        &["swap-window", "-s", "s:0", "-t", "s:1"],
        &["move-window", "-r"],
        &["last-pane", "-t", "s:1"],
        &["next-layout", "-t", "s:1"],
        &["respawn-window", "-k", "-t", "s:0", SLEEP],
    ];
    for step in steps {
        ok(&srv, step);
        let ids = srv
            .cmd(&["list-windows", "-t", "s", "-F", "#{window_id}"])
            .expect("list-windows");
        for w in ids.lines() {
            let size = display(&srv, w, "#{window_width}x#{window_height}");
            let (cols, rows) = size.split_once('x').expect("a size");
            let lines = panes(&srv, w);
            assert!(
                tiles(&lines, cols.parse().unwrap(), rows.parse().unwrap()),
                "{step:?} {w}: {lines:?}"
            );
        }
    }
}

/// The acceptance clause for the geometry export: export a layout,
/// change it, import the export, and the geometry is exactly what it was.
#[test]
fn export_then_import_reproduces_the_geometry() {
    let area = Rect {
        x: 0,
        y: 0,
        cols: 200,
        rows: 50,
    };
    let (a, b, c, d) = (PaneId(0), PaneId(1), PaneId(2), PaneId(3));
    let mut tree = LayoutTree::new(a);
    tree.split(area, a, Dir::Horizontal, b, SplitSize::Half)
        .unwrap();
    tree.split(area, b, Dir::Vertical, c, SplitSize::Cells(10))
        .unwrap();
    let how = Insert {
        dir: Dir::Horizontal,
        size: SplitSize::Cells(40),
        before: true,
        full: true,
    };
    tree.insert(area, c, d, how).unwrap();
    let render = |t: &LayoutTree| -> Vec<String> {
        t.compute(area)
            .iter()
            .map(|(p, r)| format!("{p} {},{} {}x{}", r.x, r.y, r.cols, r.rows))
            .collect()
    };
    let before = render(&tree);
    let exported = tree.export(area);

    tree.resize_to(area, a, Dir::Horizontal, 17);
    tree.spread(area, b);
    let (width, height) = (
        grofe::server::layout::MainSize::Percent(50),
        grofe::server::layout::MainSize::Cells(24),
    );
    let tiled = Preset::from_name("tiled", width, height).unwrap();
    let order: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
    assert!(tree.preset(area, tiled, &order));
    assert_ne!(render(&tree), before);

    tree.import(&exported).expect("import");
    assert_eq!(render(&tree), before);
    assert_eq!(tree.export(area), exported);
}

// ---- The corpus flags on split-window, new-window, select-pane,
// respawn-pane and capture-pane, and window sizing (TC.8, orchestrator
// ruling). Goldens from `env -u TMUX -u TMUX_PANE <tmux 3.7b>`. ----

/// `split-window -b -f -d -P -F`: where the pane goes, where it comes in
/// index order (tmux's `window_add_pane`), and what `-P` prints.
#[test]
fn split_window_b_f_d_p_match_tmux() {
    let cases: [(&[&str], &str, &[&str]); 7] = [
        (
            &["-b", "-t", "%1"],
            "s:0.1",
            &[
                "%0 0,0 100x50",
                "%3 101,0 99x12 *",
                "%1 101,13 99x12",
                "%2 101,26 99x24",
            ],
        ),
        (
            &["-b", "-h", "-t", "%1"],
            "s:0.1",
            &[
                "%0 0,0 100x50",
                "%3 101,0 49x25 *",
                "%1 151,0 49x25",
                "%2 101,26 99x24",
            ],
        ),
        (
            &["-f", "-h", "-t", "%1"],
            "s:0.3",
            &[
                "%0 0,0 50x50",
                "%1 51,0 49x25",
                "%2 51,26 49x24",
                "%3 101,0 99x50 *",
            ],
        ),
        (
            &["-b", "-f", "-h", "-l", "40", "-t", "%1"],
            "s:0.0",
            &[
                "%3 0,0 40x50 *",
                "%0 41,0 79x50",
                "%1 121,0 79x25",
                "%2 121,26 79x24",
            ],
        ),
        (
            &["-d", "-t", "%1"],
            "s:0.2",
            &[
                "%0 0,0 100x50",
                "%1 101,0 99x12",
                "%3 101,13 99x12",
                "%2 101,26 99x24 *",
            ],
        ),
        (
            &["-b", "-f", "-v", "-t", "%2"],
            "s:0.0",
            &[
                "%3 0,0 200x25 *",
                "%0 0,26 100x24",
                "%1 101,26 99x12",
                "%2 101,39 99x11",
            ],
        ),
        (
            &["-f", "-v", "-l", "10", "-t", "%0"],
            "s:0.3",
            &[
                "%0 0,0 100x39",
                "%1 101,0 99x19",
                "%2 101,20 99x19",
                "%3 0,40 200x10 *",
            ],
        ),
    ];
    for (flags, printed, want) in cases {
        let srv = three_panes();
        let mut args = vec!["split-window", "-P"];
        args.extend_from_slice(flags);
        args.push(SLEEP);
        let out = srv.cmd(&args).unwrap_or_else(|e| panic!("{flags:?}: {e}"));
        assert_eq!(out, format!("{printed}\n"), "{flags:?}");
        assert_eq!(panes(&srv, "s:0"), want, "{flags:?}");
    }
    // The opensessions sidebar's form.
    let srv = three_panes();
    let out = srv
        .cmd(&[
            "split-window",
            "-P",
            "-F",
            "#{pane_id}",
            "-b",
            "-f",
            "-h",
            "-l",
            "30",
            "-t",
            "%1",
            SLEEP,
        ])
        .expect("split-window");
    assert_eq!(out, "%3\n");
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%3 0,0 30x50 *",
            "%0 31,0 84x50",
            "%1 116,0 84x25",
            "%2 116,26 84x24"
        ]
    );
}

#[test]
fn new_window_a_p_f_match_tmux() {
    let srv = three_panes();
    ok(&srv, &["new-window", "-d", "-t", "s:3", SLEEP]);
    ok(&srv, &["new-window", "-d", "-t", "s:4", SLEEP]);
    let out = srv
        .cmd(&["new-window", "-a", "-P", "-t", "s:0", SLEEP])
        .expect("new-window -a");
    assert_eq!(out, "s:1.0\n");
    assert_eq!(windows(&srv, "s"), ["0 @0", "1 @3 *", "3 @1", "4 @2"]);
    let out = srv
        .cmd(&[
            "new-window",
            "-d",
            "-a",
            "-P",
            "-F",
            "#{window_id} #{window_index}",
            "-t",
            "s:5",
            SLEEP,
        ])
        .expect("new-window -a -F");
    assert_eq!(out, "@4 5\n");
    // No -t: after the current window, the ones in the way moving up.
    ok(&srv, &["new-window", "-a", "-d", SLEEP]);
    assert_eq!(
        windows(&srv, "s"),
        ["0 @0", "1 @3 *", "2 @5", "3 @1", "4 @2", "5 @4"]
    );
}

#[test]
fn select_pane_l_and_z_match_tmux() {
    let srv = three_panes();
    let state = || display(&srv, "s", "#{window_zoomed_flag} #{pane_id}");
    ok(&srv, &["resize-pane", "-Z", "-t", "%1"]);
    ok(&srv, &["select-pane", "-t", "%0", "-Z"]);
    assert_eq!(state(), "1 %0");
    ok(&srv, &["select-pane", "-L", "-Z", "-t", "%0"]);
    assert_eq!(state(), "1 %1");
    ok(&srv, &["select-pane", "-l", "-Z"]);
    assert_eq!(state(), "1 %0");
    ok(&srv, &["select-pane", "-l"]);
    assert_eq!(state(), "0 %1");

    let srv = three_panes();
    ok(&srv, &["select-pane", "-t", "%0"]);
    ok(&srv, &["select-pane", "-l", "-t", "s"]);
    assert_eq!(display(&srv, "s", "#{pane_id}"), "%2");
}

/// With no last pane (`split-window -d` never visits the new one), a
/// window of two panes goes to the other; of three it is an error.
#[test]
fn last_pane_falls_back_to_the_other_of_two() {
    let srv = server();
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
            SLEEP,
        ],
    );
    ok(&srv, &["split-window", "-d", "-t", "s", SLEEP]);
    ok(&srv, &["last-pane", "-t", "s"]);
    assert_eq!(display(&srv, "s", "#{pane_id}"), "%1");
    let srv = server();
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
            SLEEP,
        ],
    );
    ok(&srv, &["split-window", "-d", "-t", "s", SLEEP]);
    ok(&srv, &["split-window", "-d", "-t", "s", SLEEP]);
    assert_eq!(err(&srv, &["last-pane", "-t", "s"]), "no last pane");
}

/// `respawn-pane -c` (which-key) starts the pane in that directory, which
/// it then keeps; `respawn-window -c` likewise.
#[test]
fn respawn_c_sets_the_start_directory() {
    let srv = server();
    let (one, two) = (srv.home.join("one"), srv.home.join("two"));
    std::fs::create_dir_all(&one).expect("mkdir");
    std::fs::create_dir_all(&two).expect("mkdir");
    let (one, two) = (one.canonicalize().unwrap(), two.canonicalize().unwrap());
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
            SLEEP,
        ],
    );
    let path = || {
        let p = display(&srv, "s", "#{pane_current_path}");
        std::path::PathBuf::from(p)
            .canonicalize()
            .unwrap_or_default()
    };
    ok(
        &srv,
        &[
            "respawn-pane",
            "-k",
            "-c",
            one.to_str().unwrap(),
            "-t",
            "s",
            SLEEP,
        ],
    );
    srv.wait_for(|| path() == one, WAIT).expect("in one");
    ok(&srv, &["respawn-pane", "-k", "-t", "s", SLEEP]);
    srv.wait_for(|| path() == one, WAIT).expect("still in one");
    ok(
        &srv,
        &[
            "respawn-window",
            "-k",
            "-c",
            two.to_str().unwrap(),
            "-t",
            "s",
            SLEEP,
        ],
    );
    srv.wait_for(|| path() == two, WAIT).expect("in two");
}

/// `capture-pane -J` joins a wrapped row to the next and keeps the
/// trailing blanks a line was written with (tmux 3.7b).
#[test]
fn capture_pane_j_joins_wrapped_lines() {
    let srv = server();
    let cmd = "printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\nxyz  \\n'; sleep 300";
    ok(
        &srv,
        &["new-session", "-d", "-s", "s", "-x", "20", "-y", "5", cmd],
    );
    let a29 = "a".repeat(29);
    srv.wait_for(|| srv.capture("s").contains("xyz"), WAIT)
        .expect("printed");
    let joined = srv
        .cmd(&["capture-pane", "-p", "-J", "-t", "s"])
        .expect("-J");
    assert_eq!(joined, format!("{a29}\nxyz  \n\n\n"));
    let plain = srv.cmd(&["capture-pane", "-p", "-t", "s"]).expect("-p");
    assert_eq!(plain, format!("{}\n{}\nxyz\n\n\n", &a29[..20], &a29[20..]));
}

/// `capture-pane -J` ends a line at its used width, as tmux 3.7b's
/// `cellused` (recorded on a 20x8 pane): written blanks stay, a partial
/// erase keeps the width, a coloured erase adds none, a whole-line erase
/// empties it, a delete widens it to 19 and an insert to 20.
#[test]
fn capture_pane_j_ends_each_line_at_its_used_width() {
    let srv = server();
    let cmd = concat!(
        "printf 'ab   \\nef   \\r\\033[3C\\033[K\\nij\\033[41m\\033[K\\033[0m\\n",
        "cd   \\r\\033[2Kx\\ngh   \\033[2D\\033[1P\\nkl   \\r\\033[1@\\n'; sleep 300"
    );
    ok(
        &srv,
        &["new-session", "-d", "-s", "s", "-x", "20", "-y", "8", cmd],
    );
    srv.wait_for(|| srv.capture("s").contains("kl"), WAIT)
        .expect("printed");
    let joined = srv
        .cmd(&["capture-pane", "-p", "-J", "-t", "s"])
        .expect("-J");
    let want = [
        "ab   ".to_string(),
        "ef   ".to_string(),
        "ij".to_string(),
        "x".to_string(),
        format!("gh{}", " ".repeat(17)),
        format!(" kl{}", " ".repeat(17)),
        String::new(),
        String::new(),
    ];
    assert_eq!(joined, want.map(|l| l + "\n").concat());
}

/// `capture-pane -J` as tmux 3.7b gives it (recorded on a 20x6 pane): a
/// line typed in insert mode is the whole width, a tab is its `\t` alone,
/// and a wide character wrapped to the next row joins it. After a shrink
/// to 4 columns and back the lines are the same, but written trailing
/// blanks may be lost (the accepted divergence in `pane::written`).
#[test]
fn capture_pane_j_prints_insert_mode_tabs_and_wide_characters() {
    let srv = server();
    let cmd = concat!(
        "printf 'ab   \\nabcdef  \\n'; printf '\\033[4hxy\\033[4l\\n'; ",
        "printf 'a\\tb\\n'; printf '%018d\\344\\270\\255\\344\\270\\255z\\n' 0; sleep 300"
    );
    ok(
        &srv,
        &["new-session", "-d", "-s", "s", "-x", "20", "-y", "6", cmd],
    );
    srv.wait_for(|| srv.capture("s").contains('z'), WAIT)
        .expect("printed");
    let want = [
        "abcdef  ".to_string(),
        format!("xy{}", " ".repeat(18)),
        "a\tb".to_string(),
        format!("{}中中z", "0".repeat(18)),
        String::new(),
    ]
    .map(|l| l + "\n")
    .concat();
    let joined = |srv: &common::TestServer| {
        srv.cmd(&["capture-pane", "-p", "-J", "-t", "s"])
            .expect("-J")
    };
    assert_eq!(joined(&srv), want);
    ok(&srv, &["resize-window", "-t", "s", "-x", "4"]);
    ok(&srv, &["resize-window", "-t", "s", "-x", "20"]);
    let after = joined(&srv);
    assert_eq!(after.lines().count(), want.lines().count(), "{after:?}");
    for (line, want) in after.lines().zip(want.lines()) {
        assert!(
            line == want || line == want.trim_end(),
            "{line:?} is not {want:?}"
        );
    }
}

/// `renumber-windows` as tmux 3.7b (recorded): off, a killed window leaves
/// its gap; on, the windows left are numbered again from `base-index` when
/// one goes (killed, or its last pane joined elsewhere), not when one is
/// made at a free index or moved to another session.
#[test]
fn renumber_windows_closes_the_gaps_a_removed_window_leaves() {
    let srv = server();
    ok(&srv, &["set", "-g", "base-index", "0"]);
    ok(&srv, &["new-session", "-d", "-s", "s", SLEEP]);
    for _ in 0..3 {
        ok(&srv, &["new-window", "-t", "s", SLEEP]);
    }
    ok(&srv, &["new-session", "-d", "-s", "u", SLEEP]);
    let indexes = |session: &str| {
        let out = srv
            .cmd(&["list-windows", "-t", session, "-F", "#{window_index}"])
            .expect("list-windows");
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    ok(&srv, &["set", "-g", "renumber-windows", "off"]);
    ok(&srv, &["kill-window", "-t", "s:0"]);
    assert_eq!(indexes("s"), "1 2 3");
    ok(&srv, &["set", "-g", "renumber-windows", "on"]);
    ok(&srv, &["kill-window", "-t", "s:2"]);
    assert_eq!(indexes("s"), "0 1");
    ok(&srv, &["new-window", "-t", "s:7", SLEEP]);
    assert_eq!(indexes("s"), "0 1 7");
    ok(&srv, &["move-window", "-s", "s:1", "-t", "u:5"]);
    assert_eq!(indexes("s"), "0 7");
    ok(&srv, &["join-pane", "-s", "s:7", "-t", "s:0"]);
    assert_eq!(indexes("s"), "0");
}

/// After a resize, `-J` gains no blank padding tmux 3.7b does not have
/// (recorded): a height change (6 rows to 4 and back), and a 26-character
/// line re-wrapped at 5 columns and back. Written trailing blanks may be
/// lost across a shrink (the accepted divergence in `pane::written`).
#[test]
fn capture_pane_j_gains_no_padding_across_a_resize() {
    let srv = server();
    let cases: [(&str, &[&str], [&str; 2]); 2] = [
        (
            "printf 'ab   \\nx  \\n'; sleep 300",
            &["ab   ", "x  ", "", "", "", ""],
            ["-y", "4"],
        ),
        (
            "printf 'abcdefghijklmnopqrstuvwxyz\\n'; sleep 300",
            &["abcdefghijklmnopqrstuvwxyz", "", "", "", "", ""],
            ["-x", "5"],
        ),
    ];
    for (i, (cmd, want, shrink)) in cases.iter().enumerate() {
        let name = format!("r{i}");
        ok(
            &srv,
            &["new-session", "-d", "-s", &name, "-x", "30", "-y", "6", cmd],
        );
        srv.wait_for(|| srv.capture(&name).contains('x'), WAIT)
            .expect("printed");
        let full = if shrink[0] == "-y" { "6" } else { "30" };
        ok(&srv, &["resize-window", "-t", &name, shrink[0], shrink[1]]);
        ok(&srv, &["resize-window", "-t", &name, shrink[0], full]);
        let got = srv
            .cmd(&["capture-pane", "-p", "-J", "-t", &name])
            .expect("-J");
        let got: Vec<&str> = got.lines().collect();
        assert_eq!(got.len(), want.len(), "{name}: {got:?}");
        for (line, want) in got.iter().zip(want.iter()) {
            assert!(
                *line == *want || *line == want.trim_end(),
                "{name}: {line:?} is not {want:?}"
            );
        }
    }
}

/// A tab written over captures as blanks, as tmux 3.7b (recorded):
/// `a\t\e[3Dxy` is `a    xy` with `-p`, and with `-J` keeps the tab's
/// width, `a    xy `.
#[test]
fn capture_pane_prints_a_tab_written_over_as_blanks() {
    let srv = server();
    let cmd = "printf 'a\\t\\033[3Dxy\\n'; sleep 300";
    ok(
        &srv,
        &["new-session", "-d", "-s", "s", "-x", "20", "-y", "3", cmd],
    );
    srv.wait_for(|| srv.capture("s").contains("xy"), WAIT)
        .expect("printed");
    let first = |args: &[&str]| {
        let out = srv.cmd(args).expect("capture");
        out.lines().next().unwrap_or_default().to_string()
    };
    assert_eq!(first(&["capture-pane", "-p", "-t", "s"]), "a    xy");
    assert_eq!(first(&["capture-pane", "-p", "-J", "-t", "s"]), "a    xy ");
}

#[test]
fn main_horizontal_takes_main_pane_height() {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "200",
            "-y",
            "50",
            SLEEP,
        ],
    );
    ok(&srv, &["split-window", "-t", "s", SLEEP]);
    ok(
        &srv,
        &["set-option", "-w", "-t", "s", "main-pane-height", "30"],
    );
    ok(&srv, &["select-layout", "-t", "s", "main-horizontal"]);
    assert_eq!(panes(&srv, "s:0"), ["%0 0,0 200x30", "%1 0,31 200x19 *"]);
    ok(
        &srv,
        &["set-option", "-w", "-t", "s", "main-pane-height", "50%"],
    );
    ok(&srv, &["select-layout", "-t", "s", "main-horizontal"]);
    assert_eq!(panes(&srv, "s:0"), ["%0 0,0 200x24", "%1 0,25 200x25 *"]);
}

/// `resize-window` makes the window `window-size manual` (tmux 3.7b, `-A`
/// too), so an attached client resizing leaves it alone while its other
/// windows follow the client.
#[test]
fn a_resized_window_keeps_its_size_when_a_client_resizes() {
    let srv = three_panes();
    ok(&srv, &["new-window", "-d", "-t", "s:1", SLEEP]);
    ok(
        &srv,
        &["resize-window", "-t", "s:0", "-x", "100", "-y", "30"],
    );
    let setting = srv
        .cmd(&["show-options", "-w", "-t", "s:0", "window-size"])
        .expect("show");
    assert_eq!(setting, "window-size manual\n");
    let size = |w: &str| display(&srv, w, "#{window_width}x#{window_height}");
    let mut client = harness::Harness::spawn_attach_with_env(srv.command(), "s", &[], 120, 40);
    srv.wait_for(|| size("s:1") == "120x40", WAIT)
        .expect("the other window follows the client");
    assert_eq!(size("s:0"), "100x30");
    client.resize(90, 20);
    srv.wait_for(|| size("s:1") == "90x20", WAIT)
        .expect("the other window follows the resize");
    assert_eq!(size("s:0"), "100x30");
    assert_eq!(panes(&srv, "s:0")[0], "%0 0,0 50x30");
    // -A takes the client's size, and stays manual.
    ok(&srv, &["resize-window", "-A", "-t", "s:0"]);
    assert_eq!(size("s:0"), "90x20");
    assert_eq!(
        srv.cmd(&["show-options", "-w", "-t", "s:0", "window-size"])
            .expect("show"),
        "window-size manual\n"
    );
}

// ---- Fix round 1: room freed by a leaving pane is handed out cell by
// cell across a nested split on the same axis, as tmux's
// `layout_resize_adjust` does (tmux 3.7b goldens, windows 200x49). ----

/// Session `s`, `cols`x`rows`, built by `splits` (each `split-window`'s
/// flags), every pane `sleep`ing.
fn built(cols: &str, rows: &str, splits: &[&[&str]]) -> TestServer {
    let srv = server();
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            cols,
            "-y",
            rows,
            SLEEP,
        ],
    );
    for flags in splits {
        let mut args = vec!["split-window"];
        args.extend_from_slice(flags);
        args.push(SLEEP);
        ok(&srv, &args);
    }
    srv
}

#[test]
fn freed_room_is_shared_across_a_nested_split() {
    let a_beside_bc: &[&[&str]] = &[&["-h", "-t", "%0"], &["-v", "-t", "%1"]];
    let deeper: &[&[&str]] = &[
        &["-h", "-t", "%0"],
        &["-v", "-t", "%1"],
        &["-h", "-t", "%2"],
    ];

    let srv = built("200", "49", a_beside_bc);
    ok(&srv, &["join-pane", "-h", "-s", "%0", "-t", "%2"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%1 0,0 200x24", "%2 0,25 100x24", "%0 101,25 99x24 *"]
    );

    let srv = built("200", "49", a_beside_bc);
    ok(&srv, &["join-pane", "-b", "-h", "-s", "%0", "-t", "%2"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%1 0,0 200x24", "%2 101,25 99x24", "%0 0,25 100x24 *"]
    );

    let srv = built("200", "49", deeper);
    ok(&srv, &["break-pane", "-d", "-s", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%1 0,0 200x24", "%2 0,25 100x24", "%3 101,25 99x24 *"]
    );

    let srv = built("200", "49", deeper);
    ok(&srv, &["kill-pane", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%1 0,0 200x24", "%2 0,25 100x24", "%3 101,25 99x24 *"]
    );

    let srv = built(
        "200",
        "50",
        &[
            &["-h", "-t", "%0"],
            &["-v", "-t", "%1"],
            &["-h", "-t", "%2"],
            &["-h", "-t", "%3"],
        ],
    );
    ok(&srv, &["kill-pane", "-t", "%0"]);
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%1 0,0 200x25",
            "%2 0,26 83x24",
            "%3 84,26 58x24",
            "%4 143,26 57x24 *"
        ]
    );

    let srv = built(
        "80",
        "50",
        &[
            &["-v", "-t", "%0"],
            &["-h", "-t", "%0"],
            &["-v", "-t", "%0"],
        ],
    );
    assert_eq!(
        panes(&srv, "s:0"),
        [
            "%0 0,0 40x12",
            "%3 0,13 40x12 *",
            "%2 41,0 39x25",
            "%1 0,26 80x24"
        ]
    );
    ok(&srv, &["kill-pane", "-t", "%1"]);
    assert_eq!(
        panes(&srv, "s:0"),
        ["%0 0,0 40x25", "%3 0,26 40x24 *", "%2 41,0 39x50"]
    );
}

/// With no last window, closing the current window selects the one before
/// it (tmux 3.7b's `session_detach`), for `kill-window` and `move-window`
/// alike.
#[test]
fn closing_the_current_window_falls_back_to_the_one_before() {
    let setup = || {
        let srv = server();
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
                SLEEP,
            ],
        );
        // tmux's default, as recorded: the indexes keep their gaps.
        ok(&srv, &["set", "-g", "renumber-windows", "off"]);
        for i in 1..=3 {
            ok(&srv, &["new-window", "-d", "-t", &format!("s:{i}"), SLEEP]);
        }
        ok(&srv, &["select-window", "-t", "s:2"]);
        ok(&srv, &["kill-window", "-t", "s:0"]);
        srv
    };
    let srv = setup();
    ok(&srv, &["kill-window", "-t", "s:2"]);
    assert_eq!(windows(&srv, "s"), ["1 @1 *", "3 @3"]);

    let srv = setup();
    ok(&srv, &["new-session", "-d", "-s", "o", SLEEP]);
    ok(&srv, &["move-window", "-s", "s:2", "-t", "o:5"]);
    assert_eq!(windows(&srv, "s"), ["1 @1 *", "3 @3"]);
}
