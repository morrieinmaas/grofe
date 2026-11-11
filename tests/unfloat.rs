//! `unfloat-pane` and the `prefix F` toggle (FR.4): a floating pane goes
//! back into the layout with its process and history, splitting the tiled
//! pane under its centre, or `-s`, or the window's last active tiled pane.
//! Under tmux identity the verb is unknown and `F` is not bound, as in
//! tmux 3.7b.
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

fn var(srv: &TestServer, pane: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", pane, fmt])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `(left, top, width, height)` of `pane`.
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
    assert_eq!(n.len(), 4, "{pane}: {out:?}");
    (n[0], n[1], n[2], n[3])
}

/// A session `main` of 80x23 cells: `%0` on the left, `%1` on the right,
/// and `%2`, under `%1`, floated to the top-left of `%0` with `kept` in its
/// history.
fn three_panes() -> TestServer {
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
    srv.cmd(&["send-keys", "-t", "%2", "kept", "Enter"])
        .expect("keys");
    wait_until(|| srv.capture("%2").contains("kept"), WAIT).expect("kept shown");
    float(&srv, "2", "3");
    srv
}

/// Floats `%2`, 20x6, with its top-left content cell at `(x, y)`.
fn float(srv: &TestServer, x: &str, y: &str) {
    srv.cmd(&[
        "float-pane",
        "-t",
        "%2",
        "-x",
        "20",
        "-y",
        "6",
        "-X",
        x,
        "-Y",
        y,
    ])
    .expect("float-pane");
    assert_eq!(var(srv, "%2", "#{pane_floating_flag}"), "1");
}

#[test]
fn unfloat_splits_the_pane_under_the_centre_and_keeps_the_process() {
    let srv = three_panes();
    let pid = var(&srv, "%2", "#{pane_pid}");
    srv.cmd(&["select-pane", "-t", "%1"]).expect("select");
    // The centre, (12, 6), is over %0, which is taller in rows than it is
    // narrow in columns: 40x23, so the split is side by side.
    srv.cmd(&["unfloat-pane", "-t", "%2"])
        .expect("unfloat-pane");
    assert_eq!(var(&srv, "%2", "#{pane_floating_flag}"), "0");
    assert_eq!(var(&srv, "%2", "#{pane_pid}"), pid, "the process is kept");
    assert!(srv.capture("%2").contains("kept"), "history is kept");
    assert_eq!(var(&srv, "%2", "#{pane_active}"), "1");
    let (l0, t0, w0, h0) = rect(&srv, "%0");
    let (l2, t2, w2, h2) = rect(&srv, "%2");
    assert_eq!((t0, h0), (t2, h2), "side by side");
    assert!(l0 < l2 && l2 < rect(&srv, "%1").0, "%2 right of %0");
    assert!(w0 + w2 < 40, "they share %0's old cell");
    // Index order: right after the pane it split.
    let index = |p: &str| var(&srv, p, "#{pane_index}").parse::<u32>().ok();
    assert_eq!(index("%2"), index("%0").map(|i| i + 1));
    assert_eq!(var(&srv, "%2", "#{window_panes}"), "3");
}

#[test]
fn unfloat_s_v_and_h_choose_the_pane_and_the_direction() {
    let srv = three_panes();
    srv.cmd(&["unfloat-pane", "-t", "%2", "-s", "%1", "-v"])
        .expect("unfloat-pane -s -v");
    let (l1, t1, w1, _) = rect(&srv, "%1");
    let (l2, t2, w2, _) = rect(&srv, "%2");
    assert_eq!((l1, w1), (l2, w2), "one above the other");
    assert!(t1 < t2, "%2 below %1");

    float(&srv, "2", "3");
    srv.cmd(&["unfloat-pane", "-t", "%2", "-s", "%1", "-h"])
        .expect("unfloat-pane -s -h");
    let (l1, t1, _, h1) = rect(&srv, "%1");
    let (l2, t2, _, h2) = rect(&srv, "%2");
    assert_eq!((t1, h1), (t2, h2), "side by side");
    assert!(l1 < l2, "%2 right of %1");

    // A tall pane is split one above the other without -v.
    float(&srv, "2", "3");
    srv.cmd(&["resize-pane", "-t", "%1", "-x", "10"])
        .expect("resize");
    srv.cmd(&["unfloat-pane", "-t", "%2", "-s", "%1"])
        .expect("unfloat-pane -s");
    let (l1, t1, w1, _) = rect(&srv, "%1");
    let (l2, t2, w2, _) = rect(&srv, "%2");
    assert_eq!((l1, w1), (l2, w2), "one above the other");
    assert!(t1 < t2);
}

#[test]
fn unfloat_falls_back_to_the_last_active_tiled_pane() {
    let srv = three_panes();
    // Off the left edge: nothing tiled is under the centre, (-5, 6).
    srv.cmd(&["unfloat-pane", "-t", "%2", "-s", "%1", "-v"])
        .expect("unfloat-pane");
    float(&srv, "-15", "3");
    srv.cmd(&["select-pane", "-t", "%1"]).expect("select");
    srv.cmd(&["select-pane", "-t", "%2"]).expect("select");
    srv.cmd(&["unfloat-pane"]).expect("unfloat-pane");
    let (l1, t1, _, h1) = rect(&srv, "%1");
    let (l2, t2, _, h2) = rect(&srv, "%2");
    assert_eq!((t1, h1), (t2, h2), "split %1, side by side");
    assert!(l1 < l2);
}

#[test]
fn unfloat_into_a_window_with_no_tiled_pane_fills_it() {
    let srv = three_panes();
    srv.cmd(&["kill-pane", "-t", "%0"]).expect("kill");
    srv.cmd(&["kill-pane", "-t", "%1"]).expect("kill");
    srv.cmd(&["unfloat-pane", "-t", "%2"])
        .expect("unfloat-pane");
    assert_eq!(rect(&srv, "%2"), (0, 0, 80, 23));
    assert_eq!(var(&srv, "%2", "#{pane_floating_flag}"), "0");
}

#[test]
fn unfloat_of_a_tiled_pane_is_an_error() {
    let srv = three_panes();
    assert_eq!(
        srv.cmd(&["unfloat-pane", "-t", "%0"]),
        Err("pane is not floating\n".to_string())
    );
    assert_eq!(
        srv.cmd(&["unfloat-pane", "-t", "%2", "-s", "%2"]),
        Err("target pane is not a tiled pane of the window\n".to_string())
    );
}

#[test]
fn prefix_f_floats_and_unfloats_the_active_pane() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        "cat",
    ])
    .expect("new-session");
    srv.cmd(&["split-window", "-h", "-t", "%0", "cat"])
        .expect("split");
    let shown = srv
        .cmd(&["list-keys", "-T", "prefix", "F"])
        .expect("list-keys");
    assert!(shown.contains("unfloat-pane"), "{shown}");
    let mut client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    client.wait_for("[main]");
    let floating = |want: &'static str| {
        let srv = &srv;
        move || var(srv, "%1", "#{pane_floating_flag}") == want
    };
    client.send("\x02F");
    wait_until(floating("1"), WAIT).expect("prefix F floats %1");
    client.send("\x02F");
    wait_until(floating("0"), WAIT).expect("prefix F unfloats %1");
    assert_eq!(var(&srv, "%1", "#{window_panes}"), "2");
    assert_eq!(var(&srv, "%1", "#{pane_active}"), "1");
}

/// `tmux -S <socket> <args>` through a `tmux` symlink to grofe, in the
/// harness's environment.
fn tmux(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let root = srv.home.parent().expect("root");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join("tmux");
    if !link.exists() {
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
fn under_tmux_identity_unfloat_pane_is_unknown_and_f_is_unbound() {
    let srv = TestServer::unstarted();
    let out = tmux(&srv, &["start-server"]);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    let out = tmux(&srv, &["unfloat-pane"]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(out.stderr, "unknown command: unfloat-pane\n");
    let out = tmux(&srv, &["list-keys", "-T", "prefix"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        !out.stdout
            .lines()
            .any(|l| l.split_whitespace().nth(3) == Some("F")),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("split-window"), "{}", out.stdout);
    let _ = tmux(&srv, &["kill-server"]);
}

/// What an attached client sees once the server has turned the mouse on.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// An SGR press of the right button at 1-based client cell `(x, y)`; the
/// button stays down, as a release outside the menu would close it.
fn right_press(x: u16, y: u16) -> String {
    format!("\x1b[<2;{x};{y}M")
}

/// grofe identity's pane menu (`MouseDown3Pane`) floats the pane clicked
/// and, on a floating pane, unfloats it.
#[test]
fn the_pane_menu_floats_and_unfloats_the_pane_clicked() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        "cat",
    ])
    .expect("new-session");
    srv.cmd(&["split-window", "-h", "-t", "%0", "cat"])
        .expect("split");
    let shown = srv
        .cmd(&["list-keys", "-T", "root", "MouseDown3Pane"])
        .expect("list-keys");
    assert!(shown.contains("unfloat-pane"), "{shown}");
    assert!(shown.contains("float-pane"), "{shown}");
    let mut client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    client.wait_for_output(MOUSE_ON, 0);
    client.wait_for("[main]");
    let floating = |want: &'static str| {
        let srv = &srv;
        move || var(srv, "%0", "#{pane_floating_flag}") == want
    };
    // %1 is active; the click is on %0, cell (10, 5).
    std::thread::sleep(Duration::from_millis(400));
    client.send(right_press(11, 6));
    client.wait_until("the tiled pane's menu", WAIT, |t| t.contains("Float"));
    assert!(!client.text().contains("Unfloat"), "{}", client.text());
    client.send("f");
    wait_until(floating("1"), WAIT).expect("the menu floats %0");
    client.send("\x1b[<2;11;6m");
    // The floating pane is its tiled place moved to (2, 1): cell (10, 4)
    // is over it.
    let (l, t, _, _) = rect(&srv, "%0");
    assert_eq!((l, t), (2, 1));
    std::thread::sleep(Duration::from_millis(400));
    client.send(right_press(11, 5));
    client.wait_until("the floating pane's menu", WAIT, |t| t.contains("Unfloat"));
    client.send("f");
    wait_until(floating("0"), WAIT).expect("the menu unfloats %0");
    assert_eq!(var(&srv, "%0", "#{window_panes}"), "2");
}

/// A pane that asked for the mouse gets the right click, as tmux's stock
/// row sends it on, and no menu opens.
#[test]
fn the_pane_menu_leaves_a_click_to_a_pane_that_asked_for_the_mouse() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("log");
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        "cat",
    ])
    .expect("new-session");
    let app = format!(
        "stty raw -echo; printf '\\033[?1000h\\033[?1006h'; exec cat > '{}'",
        log.display()
    );
    srv.cmd(&["split-window", "-h", "-t", "%0", &app])
        .expect("split");
    wait_until(|| var(&srv, "%1", "#{mouse_any_flag}") == "1", WAIT).expect("%1 asks");
    srv.cmd(&["select-pane", "-t", "%0"]).expect("select");
    let mut client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    client.wait_for_output(MOUSE_ON, 0);
    client.wait_for("[main]");
    // %1 starts at column 41, so client cell (51, 6) is its cell (10, 6).
    std::thread::sleep(Duration::from_millis(400));
    client.send(right_press(51, 6));
    client.send("\x1b[<2;51;6m");
    let got = wait_until(
        || {
            fs::read(&log)
                .unwrap_or_default()
                .starts_with(b"\x1b[<2;10;6M")
        },
        WAIT,
    );
    assert!(got.is_ok(), "{:?}", fs::read(&log));
    assert!(!client.text().contains("Kill"), "{}", client.text());
    // As tmux's stock row does, the click focuses the pane it goes to.
    assert_eq!(var(&srv, "%1", "#{pane_active}"), "1");
}

/// Under tmux identity the root table's `MouseDown3Pane` is tmux 3.7b's
/// stock row, as tmux prints it, with nothing of grofe's.
#[test]
fn under_tmux_identity_the_pane_menu_is_stock() {
    let srv = TestServer::unstarted();
    let out = tmux(&srv, &["start-server"]);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    let out = tmux(&srv, &["list-keys", "-T", "root"]);
    assert!(out.status.success(), "{out:?}");
    let (_, stock) = grofe::server::input::TMUX_STOCK_ROOT
        .iter()
        .find(|(k, _)| *k == "MouseDown3Pane")
        .expect("the stock row");
    let row = out
        .stdout
        .lines()
        .find(|l| l.split_whitespace().nth(3) == Some("MouseDown3Pane"))
        .unwrap_or_else(|| panic!("{out:?}"));
    assert!(row.ends_with(stock), "{row}");
    assert!(!out.stdout.contains("float-pane"), "{}", out.stdout);
    let _ = tmux(&srv, &["kill-server"]);
}

fn snapshot(srv: &TestServer) -> Option<grofe::server::snapshot::Snapshot> {
    let text = fs::read_to_string(srv.snapshot_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The resume argv `state.json` holds for pane `uid`, `None` when the pane
/// is not saved, after waiting for `want` to hold.
fn saved(
    srv: &TestServer,
    uid: &str,
    want: impl Fn(Option<&Option<Vec<String>>>) -> bool,
) -> Option<Option<Vec<String>>> {
    let mut last = None;
    let found = srv.wait_for(
        || {
            let Some(snap) = snapshot(srv) else {
                return false;
            };
            last = snap
                .sessions
                .iter()
                .flat_map(|s| &s.windows)
                .flat_map(|w| &w.panes)
                .find(|p| p.uid == uid)
                .map(|p| p.resume.clone());
            want(last.as_ref())
        },
        Duration::from_secs(5),
    );
    assert!(found.is_ok(), "{uid}: state.json never matched: {last:?}");
    last
}

/// The same pane comes back: its uid and its resume binding (M4) stay, and
/// the snapshot, which leaves floating panes out, saves it again.
#[test]
fn unfloat_keeps_the_uid_and_resume_and_the_snapshot_saves_the_pane_again() {
    let srv = three_panes();
    let uid = var(&srv, "%2", "#{pane_uid}");
    assert!(!uid.is_empty());
    srv.cmd(&["pane-set-resume", "-t", "%2", "--", "vim", "notes"])
        .expect("pane-set-resume");
    // Floating: not saved.
    saved(&srv, &uid, |p| p.is_none());
    srv.cmd(&["unfloat-pane", "-t", "%2"])
        .expect("unfloat-pane");
    assert_eq!(var(&srv, "%2", "#{pane_uid}"), uid);
    // The argv as `pane-set-resume` stored it (its program resolved).
    let got = saved(&srv, &uid, |p| p.is_some()).flatten();
    let got = got.expect("the resume argv is kept");
    assert!(got[0].ends_with("vim"), "{got:?}");
    assert_eq!(got[1..], ["notes".to_string()]);
}

/// `float-pane` with no size or place keeps the pane's tiled size, moved 2
/// right and 1 down so the lift shows, and cut to stay in the window.
#[test]
fn float_pane_by_default_keeps_the_panes_tiled_size_and_place() {
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
    assert_eq!(rect(&srv, "%1"), (41, 0, 39, 23));
    srv.cmd(&["float-pane", "-t", "%1"]).expect("float-pane");
    assert_eq!(var(&srv, "%1", "#{pane_floating_flag}"), "1");
    assert_eq!(rect(&srv, "%1"), (43, 1, 37, 22));
}
