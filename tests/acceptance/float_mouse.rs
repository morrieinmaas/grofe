//! A floating pane's border and the mouse (`docs/floating-pane-mouse.md`):
//! tmux 3.7b's stock rows there (the full recorded set is the golden
//! `tc_6/float-border-mouse`), the drag's lifetime, and grofe's verbs
//! `float-pane` and `place-pane`.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::time::Duration;

use common::{TestServer, run_bounded, wait_until};

use crate::harness::Harness;

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const WAIT: Duration = Duration::from_secs(10);
/// Past tmux's click timeout, so the next press is no second click.
const CLICK_GAP: Duration = Duration::from_millis(350);

/// Session `main` (80x23 window) running `cat`, with a floating pane of
/// 20x4 content at window cell (10, 5) running `cat`, so its border is
/// column 9 and 30, row 4 and 9. Returns its id.
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
    let float = new_float(&srv, (10, 5), (20, 4), "cat");
    (srv, float)
}

fn new_float(srv: &TestServer, (x, y): (u16, u16), (w, h): (u16, u16), cmd: &str) -> String {
    srv.cmd(&[
        "new-pane",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "main",
        "-x",
        &w.to_string(),
        "-y",
        &h.to_string(),
        "-X",
        &x.to_string(),
        "-Y",
        &y.to_string(),
        cmd,
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

/// `rect` is still `want` once the server had time to act on the reports.
fn stays(srv: &TestServer, pane: &str, want: (u16, u16, u16, u16)) {
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rect(srv, pane), want);
}

fn var(srv: &TestServer, pane: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", pane, fmt])
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn user_option(srv: &TestServer, name: &str) -> Option<String> {
    srv.cmd(&["show-options", "-gv", name])
        .ok()
        .map(|v| v.trim().to_string())
}

/// `tmux <args>` against `srv`: grofe through a `tmux` symlink, so the
/// request is in tmux's dialect, where `resize-pane -M` is spelled.
fn tmux(srv: &TestServer, args: &[&str]) {
    let link = srv.home.join("tmux-link/tmux");
    if !link.exists() {
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let mut cmd = std::process::Command::new(&link);
    for (k, v) in srv.command().get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.current_dir(&srv.home)
        .arg("-S")
        .arg(&srv.socket)
        .args(args);
    let out = run_bounded(cmd, Duration::from_secs(20));
    assert!(out.status.success(), "tmux {args:?}: {}", out.stderr);
}

fn alive(srv: &TestServer) {
    assert_eq!(
        srv.cmd(&["display-message", "-p", "ok"]).as_deref(),
        Ok("ok\n")
    );
}

/// As tmux's `resize-pane -M`: the top edge moves the pane, the left edge
/// resizes it from the left.
#[test]
fn the_top_edge_moves_the_pane_and_the_left_edge_resizes_it() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    release(&mut a, (19, 7));
    std::thread::sleep(CLICK_GAP);
    press(&mut a, (13, 9));
    drag(&mut a, (10, 9));
    wait_rect(&srv, &float, (11, 8, 23, 4));
    release(&mut a, (10, 9));
}

/// The program in a pane resized by a drag ends at the pane's last size,
/// however many reports the drag had.
#[test]
fn a_drag_resize_reaches_the_pty_at_its_final_size() {
    let (srv, _) = start();
    let float = new_float(&srv, (10, 12), (20, 4), "sh");
    let mut a = attach(&srv);
    // The right edge is column 30, the pane's rows 12 to 15.
    press(&mut a, (30, 13));
    for x in 31..=40 {
        drag(&mut a, (x, 13));
    }
    release(&mut a, (40, 13));
    wait_rect(&srv, &float, (10, 12, 30, 4));
    std::thread::sleep(Duration::from_millis(400));
    srv.cmd(&["send-keys", "-t", &float, "stty size", "Enter"])
        .expect("keys");
    let seen = wait_until(|| srv.capture(&float).contains("4 30"), WAIT);
    assert!(seen.is_ok(), "{}", srv.capture(&float));
}

#[test]
fn killing_the_pane_mid_drag_leaves_no_drag_behind() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    srv.cmd(&["kill-pane", "-t", &float]).expect("kill-pane");
    drag(&mut a, (25, 10));
    drag(&mut a, (30, 12));
    release(&mut a, (30, 12));
    alive(&srv);
    std::thread::sleep(CLICK_GAP);
    let next = new_float(&srv, (10, 5), (20, 4), "cat");
    press(&mut a, (15, 4));
    drag(&mut a, (16, 4));
    wait_rect(&srv, &next, (11, 5, 20, 4));
    release(&mut a, (16, 4));
}

/// The drag holds the pane it started on; on another window the reports
/// find no such pane and move nothing.
#[test]
fn switching_window_mid_drag_moves_nothing() {
    let (srv, float) = start();
    srv.cmd(&["new-window", "-d", "-t", "main", "cat"])
        .expect("new-window");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    srv.cmd(&["next-window", "-t", "main"]).expect("next");
    drag(&mut a, (30, 15));
    release(&mut a, (30, 15));
    stays(&srv, &float, (14, 8, 20, 4));
    srv.cmd(&["previous-window", "-t", "main"])
        .expect("previous");
    alive(&srv);
}

#[test]
fn a_client_detached_mid_drag_leaves_the_pane_where_it_was() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    srv.cmd(&["detach-client", "-s", "main"]).expect("detach");
    a.wait_exit();
    let mut b = attach(&srv);
    press(&mut b, (50, 15));
    release(&mut b, (50, 15));
    stays(&srv, &float, (14, 8, 20, 4));
    std::thread::sleep(CLICK_GAP);
    press(&mut b, (20, 7));
    drag(&mut b, (21, 7));
    wait_rect(&srv, &float, (15, 8, 20, 4));
    release(&mut b, (21, 7));
}

/// Each client drags from its own last report against the pane as it is
/// now: a drag whose edge the other moved away waits, as in tmux.
#[test]
fn two_clients_drag_one_pane_in_turn() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    let mut b = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (17, 4));
    wait_rect(&srv, &float, (12, 5, 20, 4));
    press(&mut b, (32, 6));
    drag(&mut b, (35, 6));
    wait_rect(&srv, &float, (12, 5, 23, 4));
    drag(&mut a, (18, 4));
    wait_rect(&srv, &float, (13, 5, 23, 4));
    // The right edge is at column 36 now, not where b left it.
    drag(&mut b, (37, 6));
    stays(&srv, &float, (13, 5, 23, 4));
    release(&mut a, (18, 4));
    release(&mut b, (37, 6));
    alive(&srv);
}

/// A press while a drag is under way (its release was lost, say with the
/// mouse turned off meanwhile) ends the drag, as tmux's "stop dragging"
/// does: it fires the drag's end, not a press, and the pane stays.
#[test]
fn a_press_after_a_lost_release_ends_the_drag() {
    let (srv, float) = start();
    srv.cmd(&[
        "bind-key",
        "-n",
        "MouseDragEnd1Pane",
        "set-option",
        "-g",
        "@end",
        "pane",
    ])
    .expect("bind");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    press(&mut a, (50, 15));
    let ended = wait_until(
        || user_option(&srv, "@end").as_deref() == Some("pane"),
        WAIT,
    );
    assert!(ended.is_ok(), "no MouseDragEnd1Pane");
    release(&mut a, (50, 15));
    stays(&srv, &float, (14, 8, 20, 4));
}

/// The release of a drag fires `MouseDragEnd1Border` where it lands on a
/// border, as in tmux (grofe once swallowed it).
#[test]
fn the_release_of_a_floating_drag_fires_its_drag_end_key() {
    let (srv, float) = start();
    srv.cmd(&[
        "bind-key",
        "-n",
        "MouseDragEnd1Border",
        "set-option",
        "-g",
        "@end",
        "border",
    ])
    .expect("bind");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (19, 7));
    wait_rect(&srv, &float, (14, 8, 20, 4));
    release(&mut a, (19, 7));
    let ended = wait_until(
        || user_option(&srv, "@end").as_deref() == Some("border"),
        WAIT,
    );
    assert!(ended.is_ok(), "no MouseDragEnd1Border");
}

#[test]
fn dragging_a_lower_pane_raises_and_focuses_it() {
    let (srv, low) = start();
    let high = new_float(&srv, (12, 7), (20, 4), "cat");
    assert_eq!(var(&srv, &high, "#{pane_z} #{pane_active}"), "0 1");
    assert_eq!(var(&srv, &low, "#{pane_z} #{pane_active}"), "1 0");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (15, 3));
    wait_rect(&srv, &low, (10, 4, 20, 4));
    assert_eq!(var(&srv, &low, "#{pane_z} #{pane_active}"), "0 1");
    assert_eq!(var(&srv, &high, "#{pane_z} #{pane_active}"), "1 0");
    release(&mut a, (15, 3));
}

/// tmux's `select-pane -M`: a click on a floating border clears the mark
/// and focuses and raises the pane, which stays where it is.
#[test]
fn a_plain_click_on_the_border_clears_the_mark_and_raises() {
    let (srv, low) = start();
    let high = new_float(&srv, (12, 7), (20, 4), "cat");
    srv.cmd(&["select-pane", "-m", "-t", "%0"]).expect("mark");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    release(&mut a, (15, 4));
    let raised = wait_until(
        || {
            var(&srv, &low, "#{pane_z} #{pane_active}") == "0 1"
                && var(&srv, &high, "#{pane_z}") == "1"
        },
        WAIT,
    );
    assert!(raised.is_ok());
    assert_eq!(var(&srv, "%0", "#{pane_marked_set}"), "0");
    assert_eq!(rect(&srv, &low), (10, 5, 20, 4));
}

/// Unbinding the stock rows makes the border do nothing, as in tmux; a
/// binding of the user's own still runs there.
#[test]
fn unbinding_the_stock_rows_leaves_a_floating_border_to_bindings() {
    let (srv, float) = start();
    srv.cmd(&["unbind-key", "-n", "MouseDown1Border"])
        .expect("unbind");
    srv.cmd(&["unbind-key", "-n", "MouseDrag1Border"])
        .expect("unbind");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (25, 10));
    release(&mut a, (25, 10));
    std::thread::sleep(CLICK_GAP);
    press(&mut a, (30, 9));
    drag(&mut a, (35, 12));
    release(&mut a, (35, 12));
    stays(&srv, &float, (10, 5, 20, 4));
    srv.cmd(&["select-pane", "-t", "%0"]).expect("select");
    std::thread::sleep(CLICK_GAP);
    press(&mut a, (15, 4));
    release(&mut a, (15, 4));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(var(&srv, &float, "#{pane_active}"), "0");
    srv.cmd(&[
        "bind-key",
        "-n",
        "MouseDown1Border",
        "set-option",
        "-gF",
        "@down",
        "#{pane_id}",
    ])
    .expect("bind");
    std::thread::sleep(CLICK_GAP);
    press(&mut a, (15, 4));
    release(&mut a, (15, 4));
    let ran = wait_until(
        || user_option(&srv, "@down").as_deref() == Some(float.as_str()),
        WAIT,
    );
    assert!(ran.is_ok(), "the user's binding did not run");
}

/// The stock rows are in the root table, as tmux lists them.
#[test]
fn the_stock_border_rows_are_bound_by_default() {
    let (srv, _) = start();
    let keys = srv.cmd(&["list-keys", "-T", "root"]).expect("list-keys");
    assert!(keys.contains("MouseDown1Border"), "{keys}");
    assert!(keys.contains("MouseDrag1Border"), "{keys}");
}

/// A tiled border does nothing either once its stock row is unbound.
#[test]
fn unbinding_the_drag_row_leaves_a_tiled_border_alone() {
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
    let right = srv
        .cmd(&["split-window", "-h", "-P", "-F", "#{pane_id}", "cat"])
        .expect("split")
        .trim()
        .to_string();
    let before = rect(&srv, &right);
    let border = before.0 - 1;
    srv.cmd(&["unbind-key", "-n", "MouseDrag1Border"])
        .expect("unbind");
    let mut a = attach(&srv);
    press(&mut a, (border, 5));
    drag(&mut a, (border - 8, 5));
    release(&mut a, (border - 8, 5));
    stays(&srv, &right, before);
    tmux(
        &srv,
        &["bind", "-n", "MouseDrag1Border", "resize-pane", "-M"],
    );
    std::thread::sleep(CLICK_GAP);
    press(&mut a, (border, 5));
    drag(&mut a, (border - 8, 5));
    release(&mut a, (border - 8, 5));
    wait_rect(
        &srv,
        &right,
        (before.0 - 8, before.1, before.2 + 8, before.3),
    );
}

/// Meta, Control and Shift variants of the border keys act only if they are
/// bound themselves, as in tmux; a stock row bound to the variant is the
/// drag.
#[test]
fn a_modifier_variant_of_the_border_keys_acts_only_if_bound() {
    let (srv, float) = start();
    let mut a = attach(&srv);
    // Meta 8, Control 16, Shift 4 added to the button numbers.
    for mods in [8, 16, 4] {
        a.send(sgr(mods, (15, 4), false));
        a.send(sgr(32 + mods, (19, 7), false));
        a.send(sgr(mods, (19, 7), true));
        stays(&srv, &float, (10, 5, 20, 4));
        std::thread::sleep(CLICK_GAP);
    }
    assert_eq!(var(&srv, "%0", "#{pane_active}"), "0");
    tmux(
        &srv,
        &["bind", "-n", "M-MouseDrag1Border", "resize-pane", "-M"],
    );
    a.send(sgr(8, (15, 4), false));
    a.send(sgr(40, (19, 7), false));
    a.send(sgr(8, (19, 7), true));
    wait_rect(&srv, &float, (14, 8, 20, 4));
}

/// A root binding of the border key runs there instead of the drag, with
/// the floating pane as its target, as in tmux.
#[test]
fn a_root_binding_for_the_border_wins() {
    let (srv, float) = start();
    srv.cmd(&[
        "bind-key",
        "-n",
        "MouseDrag1Border",
        "set-option",
        "-gF",
        "@dragged",
        "#{pane_id}",
    ])
    .expect("bind");
    let mut a = attach(&srv);
    press(&mut a, (15, 4));
    drag(&mut a, (25, 10));
    release(&mut a, (25, 10));
    let ran = wait_until(
        || user_option(&srv, "@dragged").as_deref() == Some(float.as_str()),
        WAIT,
    );
    assert!(ran.is_ok(), "the user's binding did not run");
    assert_eq!(rect(&srv, &float), (10, 5, 20, 4));
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
    let pid = var(&srv, &tiled, "#{pane_pid}");
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
    assert_eq!(var(&srv, &tiled, "#{pane_pid}"), pid, "the process is kept");
    assert!(srv.capture(&tiled).contains("kept"), "history is kept");
    assert_eq!(rect(&srv, "%0"), (0, 0, 80, 23));
    // The floated pane was active and still is; the last pane stands.
    assert_eq!(var(&srv, &tiled, "#{pane_active}"), "1");
    srv.cmd(&["last-pane", "-t", "main"]).expect("last-pane");
    assert_eq!(var(&srv, "%0", "#{pane_active}"), "1");
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
    srv.cmd(&["place-pane", "-t", &tiled, "-X", "500"])
        .expect("place-pane x");
    assert_eq!(rect(&srv, &tiled), (49, 9, 30, 6));
    srv.cmd(&["place-pane", "-t", &tiled, "-X", "-5", "-Y", "-5"])
        .expect("place-pane negative");
    assert_eq!(rect(&srv, &tiled), (1, 1, 30, 6));
    assert_eq!(
        srv.cmd(&["place-pane", "-t", "%0", "-X", "1"]),
        Err("pane is not floating\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["place-pane", "-t", &tiled]),
        Err("place-pane needs -X or -Y\n".to_string())
    );
}

/// Floating a pane of a zoomed window unzooms it, and the tiled panes left
/// share the window again.
#[test]
fn float_pane_unzooms_and_relays_the_tiled_panes() {
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
    srv.cmd(&["split-window", "-h", "-t", "%0", "cat"])
        .expect("split");
    srv.cmd(&["split-window", "-v", "-t", "%1", "cat"])
        .expect("split");
    srv.cmd(&["resize-pane", "-Z", "-t", "%0"]).expect("zoom");
    assert_eq!(var(&srv, "%0", "#{window_zoomed_flag}"), "1");
    srv.cmd(&["float-pane", "-t", "%1"]).expect("float-pane");
    assert_eq!(var(&srv, "%0", "#{window_zoomed_flag}"), "0");
    assert_eq!(rect(&srv, "%0"), (0, 0, 40, 23));
    assert_eq!(rect(&srv, "%2"), (41, 0, 39, 23));
    // Its tiled size and place, (41, 0, 39, 11), moved 2 right and 1 down
    // and cut at the window's right edge.
    assert_eq!(rect(&srv, "%1"), (43, 1, 37, 11));
}
