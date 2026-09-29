//! Structure verbs (TC.8): each verb's effect checked through `list-panes
//! -F` geometry, against goldens recorded on tmux 3.7b (`tmux -L
//! grofe-tc8 -f /dev/null`, `status off`, `base-index 0`, sessions made
//! detached with `-x 200 -y 50`). Pane and window ids are compared too:
//! both servers number them from `%0` and `@0` in creation order.

mod common;

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
    // -A with no client showing the session changes nothing.
    ok(&srv, &["resize-window", "-A", "-t", "s:0"]);
    assert_eq!(panes(&srv, "s:0")[0], "%0 0,0 50x30");
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
