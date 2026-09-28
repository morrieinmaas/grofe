//! `select-layout` presets (spec §4.3, T3.7): `main-vertical`,
//! `even-horizontal` and `even-vertical`, sized as tmux 3.7b sizes them.
//! The goldens were recorded with `-f /dev/null` and `status off`; tmux
//! indexes panes from 0 there and grofe from 1, so only geometry is
//! compared, in index order.

mod common;

use std::fs;
use std::time::Duration;

use common::TestServer;

const WAIT: Duration = Duration::from_secs(5);
const GEOMETRY: &str = "#{pane_index} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}";

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap_or_else(|e| panic!("display-message {fmt}: {e}"))
        .trim_end()
        .to_string()
}

fn panes(srv: &TestServer, target: &str) -> Vec<String> {
    srv.cmd(&["list-panes", "-t", target, "-F", GEOMETRY])
        .unwrap_or_else(|e| panic!("list-panes: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// A session `name` whose window is `cols`x`rows` (status off), holding
/// four panes made by three `split-window`s, as the goldens were recorded;
/// each pane runs `cmd`.
fn four_panes(srv: &TestServer, name: &str, cols: u16, rows: u16, cmd: &str) {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        &cols.to_string(),
        "-y",
        &rows.to_string(),
        cmd,
    ])
    .expect("new-session");
    srv.cmd(&["set-option", "-t", name, "status", "off"])
        .expect("status off");
    for _ in 0..3 {
        srv.cmd(&["split-window", "-t", name, cmd]).expect("split");
    }
    assert_eq!(
        display(srv, name, "#{window_width}x#{window_height}"),
        format!("{cols}x{rows}")
    );
}

/// Every golden row with grofe's pane indices (1 to 4).
fn indexed(rows: [&str; 4]) -> Vec<String> {
    rows.iter()
        .zip(1..)
        .map(|(r, i)| format!("{i} {r}"))
        .collect()
}

#[test]
fn main_vertical_matches_tmux_and_keeps_the_active_pane() {
    let srv = TestServer::start();
    four_panes(&srv, "m", 200, 50, "sleep 30");
    srv.cmd(&["select-pane", "-t", "m:1.3"])
        .expect("select-pane");

    srv.cmd(&["select-layout", "-t", "m", "main-vertical"])
        .expect("select-layout");

    assert_eq!(
        panes(&srv, "m"),
        indexed(["0,0 80x50", "81,0 119x16", "81,17 119x16", "81,34 119x16"])
    );
    assert_eq!(display(&srv, "m", "#{pane_index}"), "3");
}

#[test]
fn even_layouts_match_tmux() {
    let srv = TestServer::start();
    four_panes(&srv, "e", 200, 50, "sleep 30");

    srv.cmd(&["select-layout", "-t", "e", "even-horizontal"])
        .expect("even-horizontal");
    assert_eq!(
        panes(&srv, "e"),
        indexed(["0,0 50x50", "51,0 49x50", "101,0 49x50", "151,0 49x50"])
    );

    srv.cmd(&["select-layout", "-t", "e", "even-vertical"])
        .expect("even-vertical");
    assert_eq!(
        panes(&srv, "e"),
        indexed(["0,0 200x12", "0,13 200x12", "0,26 200x12", "0,39 200x11"])
    );
}

/// At 80x24 the right column is one cell wide. `pane_width` reports the
/// layout's 1, as tmux does, while the pty stays 2 columns (the M2
/// minimum).
#[test]
fn a_one_column_cell_reports_width_one_and_keeps_a_two_column_pty() {
    let srv = TestServer::start();
    let out = srv.home.join("size");
    let cmd = format!("read x; stty size > {}; sleep 30", out.display());
    four_panes(&srv, "n", 80, 24, &cmd);

    srv.cmd(&["select-layout", "-t", "n", "main-vertical"])
        .expect("select-layout");
    assert_eq!(
        panes(&srv, "n"),
        indexed(["0,0 78x24", "79,0 1x8", "79,9 1x7", "79,17 1x7"])
    );

    srv.cmd(&["send-keys", "-t", "n:1.2", "Enter"])
        .expect("send-keys");
    common::wait_until(
        || fs::read_to_string(&out).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("stty never wrote: {e}"));
    assert_eq!(fs::read_to_string(&out).unwrap_or_default(), "8 2\n");
}

#[test]
fn an_unknown_layout_fails() {
    let srv = TestServer::start();
    four_panes(&srv, "u", 80, 24, "sleep 30");
    let before = panes(&srv, "u");

    let out = srv.run(&["select-layout", "-t", "u", "nope"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stderr.contains("invalid layout: nope"),
        "stderr: {}",
        out.stderr
    );
    assert_eq!(panes(&srv, "u"), before);
}

/// tmux 3.7b unzooms first and succeeds.
#[test]
fn a_zoomed_window_is_unzoomed_first() {
    let srv = TestServer::start();
    four_panes(&srv, "z", 200, 50, "sleep 30");
    srv.cmd(&["resize-pane", "-Z", "-t", "z:1.2"])
        .expect("zoom");
    assert_eq!(display(&srv, "z", "#{window_zoomed_flag}"), "1");

    srv.cmd(&["select-layout", "-t", "z", "even-vertical"])
        .expect("select-layout");

    assert_eq!(display(&srv, "z", "#{window_zoomed_flag}"), "0");
    assert_eq!(display(&srv, "z", "#{pane_index}"), "2");
    assert_eq!(
        panes(&srv, "z"),
        indexed(["0,0 200x12", "0,13 200x12", "0,26 200x12", "0,39 200x11"])
    );
}

#[test]
fn one_pane_is_a_no_op() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "o",
        "-x",
        "80",
        "-y",
        "24",
        "sleep 30",
    ])
    .expect("new-session");
    let before = panes(&srv, "o");

    srv.cmd(&["select-layout", "-t", "o", "even-horizontal"])
        .expect("select-layout");

    assert_eq!(panes(&srv, "o"), before);
}

/// A split after a preset still works and its panes stay in bounds without
/// overlapping (the unit proptest checks exact tiling).
#[test]
fn a_split_after_a_preset_tiles_the_window() {
    let srv = TestServer::start();
    four_panes(&srv, "s", 200, 50, "sleep 30");
    srv.cmd(&["select-layout", "-t", "s", "main-vertical"])
        .expect("select-layout");

    srv.cmd(&["split-window", "-h", "-t", "s:1.3", "sleep 30"])
        .expect("split after preset");
    srv.cmd(&["split-window", "-t", "s:1.1", "sleep 30"])
        .expect("split main after preset");

    let rects: Vec<(u16, u16, u16, u16)> = panes(&srv, "s")
        .iter()
        .map(|l| {
            let n = |s: &str| s.parse::<u16>().unwrap_or_else(|_| panic!("{l}"));
            let mut f = l.split(' ').skip(1);
            let (x, y) = f.next().and_then(|o| o.split_once(',')).expect("x,y");
            let (w, h) = f.next().and_then(|s| s.split_once('x')).expect("wxh");
            (n(x), n(y), n(w), n(h))
        })
        .collect();
    assert_eq!(rects.len(), 6);
    let mut grid = vec![0u8; 200 * 50];
    for (x, y, w, h) in &rects {
        assert!(x + w <= 200 && y + h <= 50, "{rects:?}");
        for row in *y..y + h {
            for col in *x..x + w {
                grid[row as usize * 200 + col as usize] += 1;
            }
        }
    }
    assert!(grid.iter().all(|&c| c <= 1), "overlap: {rects:?}");
}
