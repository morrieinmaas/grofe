//! FR.3: moving panes with the mouse (spec 2026-11-05-frames.md "The
//! mouse", "Drag and drop"), through SGR reports from an attached client.
//! A framed tile's title drag and `drag-pane -M` (`C-MouseDrag1Pane`) move
//! a pane: onto another's middle they swap, onto the outer quarter of a
//! side the pane docks there, onto itself nothing changes. A drag on a gap
//! or any other frame edge resizes. A Ctrl-drop of a floating pane
//! unfloats it where it lands. Under tmux identity `drag-pane` is unknown
//! and `C-MouseDrag1Pane` unbound.
//!
//! grofe's built-in `pane-frames` is `off` until FR.2 draws the frames, so
//! these servers turn it on in the config (gap 1, the default).
//!
//! Each test runs its own server on a private `-S` socket.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;
use std::time::Duration;

use common::{CMD_TIMEOUT, CmdOutput, TestServer, run_bounded, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
/// What an attached client sees once the server has turned the mouse on.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// SGR's Ctrl bit.
const CTRL: u8 = 16;

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn var(srv: &TestServer, pane: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", pane, fmt])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `(left, top, width, height)` of `pane`'s content.
fn rect(srv: &TestServer, pane: &str) -> (u16, u16, u16, u16) {
    let out = var(
        srv,
        pane,
        "#{pane_left} #{pane_top} #{pane_width} #{pane_height}",
    );
    let n: Vec<u16> = out
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    match n[..] {
        [l, t, w, h] => (l, t, w, h),
        _ => panic!("{pane}: {out:?}"),
    }
}

/// Every pane of the window as `%N left,top,WxH`, in index order.
fn layout(srv: &TestServer) -> Vec<String> {
    ok(
        srv,
        &[
            "list-panes",
            "-t",
            "main",
            "-F",
            "#{pane_id} #{pane_left},#{pane_top},#{pane_width}x#{pane_height}",
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

/// A framed grofe-identity server with session `main`, one 80x23 window
/// running `cat`: `%0` on the left and `%1` on the right.
fn framed() -> TestServer {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("mkdir config");
    fs::write(dir.join("config.toml"), "pane-frames = true\n").expect("write config");
    ok(&srv, &["start-server"]);
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "80",
            "-y",
            "23",
            "cat",
        ],
    );
    ok(&srv, &["split-window", "-h", "-t", "%0", "cat"]);
    srv
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for_output(MOUSE_ON, 0);
    a.wait_for("[main]");
    a
}

/// An SGR 1006 report of `button` at 0-based window cell `(x, y)`.
fn sgr(button: u8, (x, y): (u16, u16), release: bool) -> String {
    let end = if release { 'm' } else { 'M' };
    format!("\x1b[<{button};{};{}{end}", x + 1, y + 1)
}

/// A left drag from `from` through `path`, released at its last cell,
/// with `mods` (0 or [`CTRL`]) held throughout.
fn drag(a: &mut Harness, mods: u8, from: (u16, u16), path: &[(u16, u16)]) {
    // Past the click timeout, so the press is a plain press.
    std::thread::sleep(Duration::from_millis(350));
    a.send(sgr(mods, from, false));
    let mut last = from;
    for &at in path {
        a.send(sgr(32 + mods, at, false));
        last = at;
    }
    a.send(sgr(mods, last, true));
}

/// A cell of `pane`'s title: its frame's top edge, a few cells in.
fn title(srv: &TestServer, pane: &str) -> (u16, u16) {
    let (l, t, _, _) = rect(srv, pane);
    (l + 3, t - 1)
}

/// The middle of `pane`'s content.
fn middle(srv: &TestServer, pane: &str) -> (u16, u16) {
    let (l, t, w, h) = rect(srv, pane);
    (l + w / 2, t + h / 2)
}

/// The window's layout is still `want` once the server had time to act.
fn stays(srv: &TestServer, want: &[String]) {
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(layout(srv), want);
}

#[test]
fn dropping_a_title_on_a_panes_middle_swaps_them() {
    let srv = framed();
    let mut a = attach(&srv);
    let (left, right) = (rect(&srv, "%0"), rect(&srv, "%1"));
    let (pid, uid) = (
        var(&srv, "%0", "#{pane_pid}"),
        var(&srv, "%0", "#{pane_uid}"),
    );
    let to = middle(&srv, "%1");
    drag(&mut a, 0, title(&srv, "%0"), &[(30, 8), (50, 9), to]);
    let swapped = wait_until(
        || rect(&srv, "%0") == right && rect(&srv, "%1") == left,
        WAIT,
    );
    assert!(swapped.is_ok(), "{:?}", layout(&srv));
    // The same pane, moved: process and uid stay; it is the active one.
    assert_eq!(var(&srv, "%0", "#{pane_pid}"), pid);
    assert_eq!(var(&srv, "%0", "#{pane_uid}"), uid);
    assert_eq!(var(&srv, "%0", "#{pane_active}"), "1");
}

#[test]
fn dropping_on_a_panes_left_quarter_docks_left() {
    let srv = framed();
    // A shell, to ask its pty's size.
    ok(&srv, &["split-window", "-v", "-t", "%1"]);
    let mut a = attach(&srv);
    let full = rect(&srv, "%0").3;
    let (l0, t0, _, h0) = rect(&srv, "%0");
    // %2, bottom right, onto %0's left quarter.
    drag(&mut a, 0, title(&srv, "%2"), &[(l0 + 1, t0 + h0 / 2)]);
    let docked = wait_until(|| rect(&srv, "%2").0 < rect(&srv, "%0").0, WAIT);
    assert!(docked.is_ok(), "{:?}", layout(&srv));
    // %2 has the left half of %0's place; %1 took the whole right column.
    let (l2, t2, w2, h2) = rect(&srv, "%2");
    assert_eq!((l2, t2, h2), (l0, t0, full), "{:?}", layout(&srv));
    assert!(w2 < rect(&srv, "%1").2 + 2, "{:?}", layout(&srv));
    assert_eq!(rect(&srv, "%1").3, full, "{:?}", layout(&srv));
    // The pty learnt its size.
    let size = format!("{h2} {w2}");
    ok(&srv, &["send-keys", "-t", "%2", "stty size", "Enter"]);
    wait_until(|| srv.capture("%2").contains(&size), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {}", srv.capture("%2")));
}

#[test]
fn dropping_a_pane_on_itself_or_a_gap_changes_nothing() {
    let srv = framed();
    let mut a = attach(&srv);
    let before = layout(&srv);
    drag(&mut a, 0, title(&srv, "%0"), &[(20, 5), middle(&srv, "%0")]);
    stays(&srv, &before);
    // The gap between the two frames.
    let (l0, _, w0, _) = rect(&srv, "%0");
    let gap = l0 + w0 + 1;
    drag(&mut a, 0, title(&srv, "%0"), &[(50, 5), (gap, 10)]);
    stays(&srv, &before);
    // An edge drop that leaves the shape as it is: %0 onto %1's left
    // quarter, with the divider moved first, which stays put.
    ok(&srv, &["resize-pane", "-t", "%0", "-R", "6"]);
    let moved = layout(&srv);
    let (l1, t1, _, h1) = rect(&srv, "%1");
    drag(&mut a, 0, title(&srv, "%0"), &[(l1 + 1, t1 + h1 / 2)]);
    stays(&srv, &moved);
}

#[test]
fn dragging_the_gap_between_two_tiles_resizes_both() {
    let srv = framed();
    let mut a = attach(&srv);
    let (l0, t0, w0, h0) = rect(&srv, "%0");
    let (_, _, w1, _) = rect(&srv, "%1");
    let gap = l0 + w0 + 1;
    let row = t0 + h0 / 2;
    drag(&mut a, 0, (gap, row), &[(gap + 2, row), (gap + 5, row)]);
    let resized = wait_until(
        || rect(&srv, "%0").2 == w0 + 5 && rect(&srv, "%1").2 == w1 - 5,
        WAIT,
    );
    assert!(resized.is_ok(), "{:?}", layout(&srv));
    // Another frame edge resizes too: %1's left edge, back by 3.
    let left_edge = rect(&srv, "%1").0 - 1;
    drag(&mut a, 0, (left_edge, row), &[(left_edge - 3, row)]);
    let back = wait_until(|| rect(&srv, "%0").2 == w0 + 2, WAIT);
    assert!(back.is_ok(), "{:?}", layout(&srv));
}

#[test]
fn a_ctrl_drag_anywhere_in_a_pane_moves_it() {
    let srv = framed();
    let mut a = attach(&srv);
    let (left, right) = (rect(&srv, "%0"), rect(&srv, "%1"));
    let shown = ok(&srv, &["list-keys", "-T", "root", "C-MouseDrag1Pane"]);
    assert!(shown.contains("drag-pane -M"), "{shown}");
    drag(
        &mut a,
        CTRL,
        middle(&srv, "%0"),
        &[(30, 10), middle(&srv, "%1")],
    );
    let swapped = wait_until(
        || rect(&srv, "%0") == right && rect(&srv, "%1") == left,
        WAIT,
    );
    assert!(swapped.is_ok(), "{:?}", layout(&srv));
}

#[test]
fn a_ctrl_drop_of_a_floating_pane_unfloats_it_there() {
    let srv = framed();
    ok(&srv, &["split-window", "-v", "-t", "%1", "cat"]);
    ok(
        &srv,
        &[
            "float-pane",
            "-t",
            "%2",
            "-x",
            "20",
            "-y",
            "5",
            "-X",
            "45",
            "-Y",
            "4",
        ],
    );
    assert_eq!(var(&srv, "%2", "#{pane_floating_flag}"), "1");
    let pid = var(&srv, "%2", "#{pane_pid}");
    let mut a = attach(&srv);
    let (l0, t0, _, h0) = rect(&srv, "%0");
    drag(
        &mut a,
        CTRL,
        middle(&srv, "%2"),
        &[(30, 10), (l0 + 1, t0 + h0 / 2)],
    );
    let tiled = wait_until(|| var(&srv, "%2", "#{pane_floating_flag}") == "0", WAIT);
    assert!(tiled.is_ok(), "{:?}", layout(&srv));
    assert!(
        rect(&srv, "%2").0 < rect(&srv, "%0").0,
        "{:?}",
        layout(&srv)
    );
    assert_eq!(rect(&srv, "%2").1, t0);
    assert_eq!(var(&srv, "%2", "#{pane_pid}"), pid);
    assert_eq!(var(&srv, "%2", "#{window_panes}"), "3");
}

/// On a tiled pane's middle a floating pane does not swap: the pane it
/// lands on stays tiled and is split along its longer axis, as
/// `unfloat-pane -s` splits it.
#[test]
fn a_ctrl_drop_of_a_floating_pane_on_a_middle_splits_it() {
    let srv = framed();
    ok(&srv, &["split-window", "-v", "-t", "%1", "cat"]);
    ok(
        &srv,
        &[
            "float-pane",
            "-t",
            "%2",
            "-x",
            "20",
            "-y",
            "5",
            "-X",
            "45",
            "-Y",
            "4",
        ],
    );
    let mut a = attach(&srv);
    let (l0, t0, w0, h0) = rect(&srv, "%0");
    drag(
        &mut a,
        CTRL,
        middle(&srv, "%2"),
        &[(30, 10), middle(&srv, "%0")],
    );
    let tiled = wait_until(|| var(&srv, "%2", "#{pane_floating_flag}") == "0", WAIT);
    assert!(tiled.is_ok(), "{:?}", layout(&srv));
    // %0 is wider than tall, so it is split side by side and %2 takes the
    // right half; nothing floats now.
    assert!(w0 > h0);
    assert_eq!(var(&srv, "%0", "#{pane_floating_flag}"), "0");
    let (l2, t2, _, h2) = rect(&srv, "%2");
    assert_eq!(rect(&srv, "%0").0, l0, "{:?}", layout(&srv));
    assert!(l2 > l0 && (t2, h2) == (t0, h0), "{:?}", layout(&srv));
    assert_eq!(var(&srv, "%2", "#{pane_active}"), "1");
}

/// The moved pane's program learns its new size once, on release: the
/// reports during the drag only move the preview.
#[test]
fn the_program_learns_its_size_once_on_release() {
    let srv = framed();
    let log = srv.config_home.join("winch");
    let script = format!(
        "trap 'echo w >> {0}' WINCH; echo ready > {0}; while :; do sleep 0.05; done",
        log.display()
    );
    ok(&srv, &["split-window", "-v", "-t", "%1", &script]);
    let lines = || fs::read_to_string(&log).unwrap_or_default().lines().count();
    wait_until(|| lines() == 1, WAIT).expect("the trap is set");
    let mut a = attach(&srv);
    let (l0, t0, w0, h0) = rect(&srv, "%0");
    let mut path: Vec<(u16, u16)> = (0..20)
        .map(|i| (l0 + w0 / 2 + i % 7, t0 + 2 + i % 9))
        .collect();
    path.push((l0 + 1, t0 + h0 / 2));
    drag(&mut a, 0, title(&srv, "%2"), &path);
    wait_until(|| rect(&srv, "%2").0 < rect(&srv, "%0").0, WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", layout(&srv)));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(lines(), 2, "one resize: {:?}", fs::read_to_string(&log));
}

#[test]
fn a_zoomed_window_starts_no_move() {
    let srv = framed();
    let mut a = attach(&srv);
    ok(&srv, &["resize-pane", "-Z", "-t", "%0"]);
    let zoomed = rect(&srv, "%0");
    drag(&mut a, 0, title(&srv, "%0"), &[(60, 10), (70, 12)]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(var(&srv, "%0", "#{window_zoomed_flag}"), "1");
    assert_eq!(rect(&srv, "%0"), zoomed);
}

/// A press on a frame or gap cell is `MouseDown1Border`, aimed at the
/// cell's pane: a gap cell at the nearest frame's, a tie to the left.
#[test]
fn frame_and_gap_cells_are_borders_of_their_pane() {
    let srv = framed();
    ok(
        &srv,
        &[
            "bind",
            "-n",
            "MouseDown1Border",
            "set",
            "-gF",
            "@hit",
            "#{pane_id}",
        ],
    );
    let mut a = attach(&srv);
    let (l0, t0, w0, h0) = rect(&srv, "%0");
    let row = t0 + h0 / 2;
    for (cell, want) in [
        ((l0 + w0 + 1, row), "%0"),
        ((l0 + w0 + 2, row), "%1"),
        ((l0 + w0, row), "%0"),
        ((l0 + 4, t0 + h0), "%0"),
        ((0, row), "%0"),
    ] {
        ok(&srv, &["set", "-g", "@hit", "none"]);
        std::thread::sleep(Duration::from_millis(350));
        a.send(sgr(0, cell, false));
        a.send(sgr(0, cell, true));
        let got = wait_until(|| ok(&srv, &["show", "-gv", "@hit"]).trim() == want, WAIT);
        assert!(
            got.is_ok(),
            "{cell:?}: {}",
            ok(&srv, &["show", "-gv", "@hit"])
        );
    }
}

/// The server's binary run as `tmux` (tmux identity, spec D12).
fn tmux(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let root = srv.home.parent().expect("test root").to_path_buf();
    let link = root.join("bin").join("tmux");
    if !link.exists() {
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let tmpdir = root.join("tmux-tmpdir");
    fs::create_dir_all(&tmpdir).unwrap();
    let base = srv.command();
    let mut cmd = Command::new(link);
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.current_dir(&srv.home)
        .env("TMUX_TMPDIR", tmpdir)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .arg("-S")
        .arg(&srv.socket)
        .args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

#[test]
fn under_tmux_identity_drag_pane_is_unknown_and_the_ctrl_drag_unbound() {
    let srv = TestServer::unstarted();
    let out = tmux(&srv, &["start-server"]);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    let out = tmux(&srv, &["drag-pane", "-M"]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(out.stderr, "unknown command: drag-pane\n");
    let out = tmux(&srv, &["list-keys", "-T", "root"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(!out.stdout.contains("C-MouseDrag1Pane"), "{}", out.stdout);
    assert!(out.stdout.contains("MouseDrag1Border"), "{}", out.stdout);
    let _ = tmux(&srv, &["kill-server"]);
}

#[test]
fn drag_pane_outside_a_mouse_key_fails() {
    let srv = framed();
    assert_eq!(
        srv.cmd(&["drag-pane", "-M"]),
        Err("no mouse target\n".to_string())
    );
    assert!(srv.cmd(&["drag-pane"]).is_err());
}
