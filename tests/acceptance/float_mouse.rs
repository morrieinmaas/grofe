//! grofe's floating-pane mouse extension (option `floating-pane-mouse`,
//! default on; tmux 3.7b takes no mouse key on that border): dragging the
//! top or left edge moves the pane, the right or bottom edge or a corner
//! resizes it, both held inside the window, and the pane comes to the top.
//! Off, the border is inert as in tmux. Also the grofe-only verbs
//! `float-pane` and `place-pane`.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::Harness;

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const WAIT: Duration = Duration::from_secs(10);

/// Session `main` (80x23 window) running `cat`, with a floating pane of
/// 20x4 content at window cell (10, 5) running `cat`. Returns its id.
fn start() -> (TestServer, String) {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "cat",
    ])
    .expect("new-session");
    let float = new_float(&srv, (10, 5), (20, 4));
    (srv, float)
}

fn new_float(srv: &TestServer, (x, y): (u16, u16), (w, h): (u16, u16)) -> String {
    srv.cmd(&[
        "new-pane",
        "-P",
        "-F",
        "#{pane_id}",
        "-x",
        &w.to_string(),
        "-y",
        &h.to_string(),
        "-X",
        &x.to_string(),
        "-Y",
        &y.to_string(),
        "cat",
    ])
    .expect("new-pane")
    .trim()
    .to_string()
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for_output(MOUSE_ON, 0);
    a
}

/// An SGR 1006 report at 0-based window cell `(x, y)` (client cell + 1).
fn sgr(button: u8, (x, y): (u16, u16), release: bool) -> String {
    let end = if release { 'm' } else { 'M' };
    format!("\x1b[<{button};{};{}{end}", x + 1, y + 1)
}

fn press(a: &mut Harness, at: (u16, u16)) {
    a.send(sgr(0, at, false));
}

fn drag(a: &mut Harness, to: (u16, u16)) {
    a.send(sgr(32, to, false));
}

fn release(a: &mut Harness, at: (u16, u16)) {
    a.send(sgr(0, at, true));
}

/// `(left, top, width, height)` of `pane`.
fn rect(srv: &TestServer, pane: &str) -> (u16, u16, u16, u16) {
    let out = srv
        .cmd(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_left} #{pane_top} #{pane_width} #{pane_height}",
        ])
        .unwrap_or_default();
    let n: Vec<u16> = out
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    match n[..] {
        [l, t, w, h] => (l, t, w, h),
        _ => (0, 0, 0, 0),
    }
}

fn wait_rect(srv: &TestServer, pane: &str, want: (u16, u16, u16, u16)) {
    let got = wait_until(|| rect(srv, pane) == want, WAIT);
    assert!(got.is_ok(), "want {want:?}, got {:?}", rect(srv, pane));
}

fn var(srv: &TestServer, pane: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", pane, fmt])
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn the_option_defaults_on() {
    let (srv, _) = start();
    let on = srv.cmd(&["show-options", "-gv", "floating-pane-mouse"]);
    assert_eq!(on.as_deref(), Ok("on\n"));
}

#[test]
fn dragging_the_top_border_moves_the_pane() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    // The top border row is y = 4, over x = 10..30.
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    drag(&mut a, (13, 3));
    wait_rect(&srv, &float, (8, 4, 20, 4));
    release(&mut a, (13, 3));
    // The release ends the drag: later motion is not a drag of the pane.
    a.send(sgr(35, (40, 10), false));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rect(&srv, &float), (8, 4, 20, 4));
    // It is drawn where it went.
    a.wait_until("the border moved", WAIT, |t| {
        t.lines().nth(3).is_some_and(|l| l.contains('┌'))
    });
}

#[test]
fn dragging_the_left_edge_moves_it_too() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    press(&mut a, (9, 6));
    drag(&mut a, (6, 8));
    wait_rect(&srv, &float, (7, 7, 20, 4));
    release(&mut a, (6, 8));
}

#[test]
fn dragging_the_bottom_right_corner_resizes_the_pane_and_its_pty() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    // Content is x 10..30, y 5..9: the corner cell is (30, 9).
    press(&mut a, (30, 9));
    drag(&mut a, (35, 11));
    wait_rect(&srv, &float, (10, 5, 25, 6));
    let size = var(&srv, &float, "#{pane_width}x#{pane_height}");
    assert_eq!(size, "25x6");
    release(&mut a, (35, 11));
    // The right edge alone changes the width only, the bottom edge the height.
    press(&mut a, (35, 7));
    drag(&mut a, (33, 12));
    wait_rect(&srv, &float, (10, 5, 23, 6));
    release(&mut a, (33, 12));
    press(&mut a, (20, 11));
    drag(&mut a, (28, 13));
    wait_rect(&srv, &float, (10, 5, 23, 8));
    release(&mut a, (28, 13));
}

#[test]
fn the_top_left_corner_resizes_from_that_corner() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    press(&mut a, (9, 4));
    drag(&mut a, (7, 3));
    wait_rect(&srv, &float, (8, 4, 22, 5));
    release(&mut a, (7, 3));
}

#[test]
fn a_drag_is_clamped_to_the_window() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (79, 22));
    // Border inside the 80x23 window: x <= 80 - 20 - 1, y <= 23 - 4 - 1.
    wait_rect(&srv, &float, (59, 18, 20, 4));
    drag(&mut a, (0, 0));
    wait_rect(&srv, &float, (1, 1, 20, 4));
    // Coming back un-clamps: the press is the reference.
    drag(&mut a, (15, 4));
    wait_rect(&srv, &float, (10, 5, 20, 4));
    release(&mut a, (15, 4));

    press(&mut a, (30, 9));
    drag(&mut a, (79, 22));
    wait_rect(&srv, &float, (10, 5, 69, 17));
    drag(&mut a, (0, 0));
    wait_rect(&srv, &float, (10, 5, 1, 1));
    release(&mut a, (0, 0));
}

#[test]
fn dragging_a_lower_pane_raises_and_focuses_it() {
    let (srv, low) = start();
    let high = new_float(&srv, (12, 7), (20, 4));
    assert_eq!(var(&srv, &high, "#{pane_z} #{pane_active}"), "0 1");
    assert_eq!(var(&srv, &low, "#{pane_z} #{pane_active}"), "1 0");
    let mut a = attach(&srv);
    // The low pane's top border row is clear of the high one.
    press(&mut a, (15, 4));
    drag(&mut a, (15, 3));
    wait_rect(&srv, &low, (10, 4, 20, 4));
    assert_eq!(var(&srv, &low, "#{pane_z} #{pane_active}"), "0 1");
    assert_eq!(var(&srv, &high, "#{pane_z} #{pane_active}"), "1 0");
    release(&mut a, (15, 3));
}

#[test]
fn a_plain_click_on_the_border_raises_without_moving() {
    let (srv, low) = start();
    let high = new_float(&srv, (12, 7), (20, 4));
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    release(&mut a, (15, 4));
    let raised = wait_until(
        || var(&srv, &low, "#{pane_z}") == "0" && var(&srv, &high, "#{pane_z}") == "1",
        WAIT,
    );
    assert!(raised.is_ok());
    assert_eq!(rect(&srv, &low), (10, 5, 20, 4));
}

#[test]
fn the_option_off_leaves_the_border_inert_as_in_tmux() {
    let (srv, float) = start();
    srv.cmd(&["set-option", "-g", "floating-pane-mouse", "off"])
        .expect("set");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (25, 10));
    release(&mut a, (25, 10));
    press(&mut a, (30, 9));
    drag(&mut a, (35, 12));
    release(&mut a, (35, 12));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(rect(&srv, &float), (10, 5, 20, 4));
    // Nor does a click there focus it.
    srv.cmd(&["select-pane", "-t", "%0"]).expect("select");
    press(&mut a, (15, 4));
    release(&mut a, (15, 4));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(var(&srv, &float, "#{pane_active}"), "0");
}

#[test]
fn the_option_is_per_window() {
    let (srv, float) = start();
    let window = var(&srv, "%0", "#{window_id}");
    srv.cmd(&[
        "set-option",
        "-w",
        "-t",
        &window,
        "floating-pane-mouse",
        "off",
    ])
    .expect("set");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (25, 10));
    release(&mut a, (25, 10));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rect(&srv, &float), (10, 5, 20, 4));
}

/// A root binding for the border key runs there instead of the drag, except
/// tmux's stock `resize-pane -M` row, which is no use on a floating pane.
#[test]
fn a_root_binding_for_the_border_wins_but_the_stock_rows_do_not() {
    let (srv, float) = start();
    srv.cmd(&[
        "bind-key",
        "-n",
        "MouseDrag1Border",
        "set-option",
        "-g",
        "@dragged",
        "yes",
    ])
    .expect("bind");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (25, 10));
    release(&mut a, (25, 10));
    let ran = wait_until(
        || srv.cmd(&["show-options", "-gv", "@dragged"]).as_deref() == Ok("yes\n"),
        WAIT,
    );
    assert!(ran.is_ok(), "the user's binding did not run");
    assert_eq!(rect(&srv, &float), (10, 5, 20, 4));

    srv.cmd(&["bind-key", "-n", "MouseDrag1Border", "select-pane", "-M"])
        .expect("bind stock");
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    release(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
}

#[test]
fn float_pane_lifts_a_tiled_pane_and_place_pane_moves_it() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "cat",
    ])
    .expect("new-session");
    let tiled = srv
        .cmd(&["split-window", "-h", "-P", "-F", "#{pane_id}", "cat"])
        .expect("split")
        .trim()
        .to_string();
    srv.cmd(&["send-keys", "-t", &tiled, "kept", "Enter"])
        .expect("keys");
    let seen = wait_until(|| srv.capture(&tiled).contains("kept"), WAIT);
    assert!(seen.is_ok());
    srv.cmd(&[
        "float-pane",
        "-t",
        &tiled,
        "-x",
        "30",
        "-y",
        "6",
        "-X",
        "7",
        "-Y",
        "3",
    ])
    .expect("float-pane");
    assert_eq!(rect(&srv, &tiled), (7, 3, 30, 6));
    assert_eq!(var(&srv, &tiled, "#{pane_floating_flag}"), "1");
    assert!(srv.capture(&tiled).contains("kept"), "history is kept");
    // The other pane has the whole window again.
    assert_eq!(rect(&srv, "%0"), (0, 0, 80, 23));
    assert_eq!(
        srv.cmd(&["float-pane", "-t", &tiled]),
        Err("pane is already floating\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["float-pane", "-t", "%0"]),
        Err("can't float the only tiled pane\n".to_string())
    );

    srv.cmd(&["place-pane", "-t", &tiled, "-X", "20", "-Y", "9"])
        .expect("place-pane");
    assert_eq!(rect(&srv, &tiled), (20, 9, 30, 6));
    // One axis only; held inside the window like a drag.
    srv.cmd(&["place-pane", "-t", &tiled, "-X", "500"])
        .expect("place-pane x");
    assert_eq!(rect(&srv, &tiled), (49, 9, 30, 6));
    assert_eq!(
        srv.cmd(&["place-pane", "-t", "%0", "-X", "1"]),
        Err("pane is not floating\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["place-pane", "-t", &tiled]),
        Err("place-pane needs -X or -Y\n".to_string())
    );
}
