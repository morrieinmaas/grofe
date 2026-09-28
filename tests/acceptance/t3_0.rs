//! T3.0: the M3 seams are in place and inert. Mouse modes reach the outer
//! terminal on attach and follow `set mouse`; focus reports stop at the
//! client while `focus-events` is off; mouse input still goes nowhere; the
//! M3 commands parse and answer `not implemented` (except the minimal
//! `bind-key`, R25, which binds sequences); the new format variables work;
//! and the server panics on cue for T3.6's report.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::process::Stdio;
use std::time::Duration;

use common::{TestServer, wait_child, wait_until};
use grofe::server::TEST_PANIC_ENV;

use crate::harness::{Harness, require_tool};

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";

/// A server with one session `main` running `cmd`, sized for an 80x24
/// client with a status line.
fn start(cmd: &str) -> TestServer {
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
        cmd,
    ])
    .expect("new-session");
    srv
}

fn attach(srv: &TestServer) -> Harness {
    Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24)
}

fn windows(srv: &TestServer) -> usize {
    srv.cmd(&["list-windows", "-t", "main"])
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn mouse_modes_follow_the_option_in_the_outer_stream() {
    let srv = start("cat");
    let mut a = attach(&srv);
    // grofe's default is `mouse on` (spec §5.3), written ahead of the
    // first frame's full redraw, in the same write.
    let at = a.wait_for_output(MOUSE_ON, 0);
    let redraw = a
        .output()
        .windows(4)
        .position(|w| w == b"\x1b[2J")
        .expect("a full redraw");
    assert!(
        at <= redraw,
        "mouse modes at {at}, first redraw at {redraw}"
    );
    srv.cmd(&["set", "-g", "mouse", "off"]).expect("mouse off");
    let at = a.wait_for_output(MOUSE_OFF, at);
    srv.cmd(&["set", "-g", "mouse", "on"]).expect("mouse on");
    a.wait_for_output(MOUSE_ON, at);
}

/// A pane that asked for focus reports gets them only while
/// `focus-events` is on (R28), and mouse input still reaches no pane until
/// T3.2.
#[test]
fn focus_events_gate_and_mouse_input_go_nowhere() {
    let srv = start("printf '\\033[?1004h'; stty -echo; exec cat -v");
    let mut a = attach(&srv);
    a.wait_for_output(MOUSE_ON, 0);
    a.send("\x1b[Ix\r");
    a.wait_for("^[[Ix");

    srv.cmd(&["set", "-g", "focus-events", "off"])
        .expect("focus-events off");
    a.send("\x1b[Oy\r");
    a.wait_until("the y line", Duration::from_secs(10), |text| {
        text.lines().any(|l| l.trim_end() == "y")
    });

    a.send("\x1b[<64;10;5M\x1b[<0;3;3M\x1b[<0;3;3mz\r");
    a.wait_until("the z line", Duration::from_secs(10), |text| {
        text.lines().any(|l| l.trim_end() == "z")
    });
    assert!(!a.text().contains("^[[<"), "{}", a.text());
}

#[test]
fn m3_commands_parse_and_their_stubs_change_nothing() {
    let srv = start("cat");
    let before = srv.cmd(&["list-panes", "-t", "main"]).expect("list-panes");
    for argv in [
        &["list-keys"][..],
        &["unbind-key", "x"],
        &["source-file", "sub.toml"],
        &["reload"],
        &["command-prompt", "-p", "name", "rename-window '%%'"],
        &["run-shell", "true"],
        &["select-layout", "even-horizontal"],
    ] {
        let err = srv.cmd(argv).expect_err("a stub");
        assert!(
            err.contains(&format!("not implemented: {}", argv[0])),
            "{argv:?}: {err}"
        );
    }
    assert_eq!(srv.cmd(&["list-panes", "-t", "main"]).as_ref(), Ok(&before));
    // Parse errors come first, worded as tmux words them.
    let err = srv.cmd(&["unbind-key"]).expect_err("no key");
    assert!(err.contains("missing key"), "{err}");
    let err = srv.cmd(&["bind-key", "NotAKey", "x"]).expect_err("bad key");
    assert!(err.contains("unknown key: NotAKey"), "{err}");
}

/// R25: the minimal `bind-key` binds at runtime, a command string with
/// `;` as a sequence (R12), and a binding runs every command in order.
#[test]
fn minimal_bind_key_binds_commands_and_sequences() {
    let srv = start("cat");
    srv.cmd(&["bind-key", "X", "new-window", "-d"])
        .expect("bind X");
    srv.cmd(&["bind-key", "Y", "new-window -d ; new-window -d"])
        .expect("bind Y");
    let mut a = attach(&srv);
    a.wait_for_output(MOUSE_ON, 0);
    a.send("\x02X");
    wait_until(|| windows(&srv) == 2, Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("C-b X: {e}; {} windows", windows(&srv)));
    a.send("\x02Y");
    wait_until(|| windows(&srv) == 4, Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("C-b Y: {e}; {} windows", windows(&srv)));
}

/// R26 and R13: `pane_left`, `pane_top` and `window_bell_flag` expand.
#[test]
fn pane_origin_and_bell_flag_format_variables() {
    let srv = start("cat");
    srv.cmd(&["split-window", "-h", "-t", "main", "cat"])
        .expect("split");
    let out = srv
        .cmd(&[
            "list-panes",
            "-t",
            "main",
            "-F",
            "#{pane_left},#{pane_top} #{pane_width}",
        ])
        .expect("list-panes");
    let rows: Vec<(u16, u16, u16)> = out
        .lines()
        .map(|l| {
            let (origin, width) = l.split_once(' ').expect("two fields");
            let (x, y) = origin.split_once(',').expect("x,y");
            let n = |s: &str| s.parse::<u16>().expect(l);
            (n(x), n(y), n(width))
        })
        .collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert_eq!((rows[0].0, rows[0].1), (0, 0), "{out}");
    // The right pane starts past the left one and its border.
    assert_eq!((rows[1].0, rows[1].1), (rows[0].2 + 1, 0), "{out}");
    let flag = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{window_bell_flag}"])
        .expect("display");
    assert_eq!(flag, "0\n");
}

/// R22's trigger: with `GROFE_TEST_SERVER_PANIC=1` the server panics on
/// its first command (T3.6 writes the report).
#[test]
fn the_test_trigger_panics_the_server_on_its_first_command() {
    let srv = TestServer::unstarted();
    let mut cmd = srv.command();
    cmd.arg("__server")
        .env(TEST_PANIC_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn server");
    wait_until(|| srv.is_running(), Duration::from_secs(10)).expect("server up");
    let out = srv.run(&["list-sessions"]);
    assert!(!out.status.success(), "{out:?}");
    let status = wait_child(child, Duration::from_secs(10)).expect("server exits");
    assert!(!status.success(), "{status:?}");
    assert!(srv.log().contains(TEST_PANIC_ENV), "{}", srv.log());
}

/// A pane that prints its pty size, cells then pixels, per line it reads.
const WINSIZE: &str = "exec python3 -c 'import sys, fcntl, termios, struct\n\
                       for _ in sys.stdin:\n    \
                       r, c, x, y = struct.unpack(\"4H\", fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8)))\n    \
                       print(\"ws\", c, r, x, y, flush=True)'";

/// The orchestrator's cell-size seam: a client's `Caps::cell_px` (here from
/// `GROFE_TEST_CAPS`) gives the panes it sizes a pixel size; before any
/// client says, it is 0. A pane split off later starts with it.
#[test]
fn panes_get_a_pixel_size_from_the_client_cell_size() {
    if !require_tool("python3") {
        return;
    }
    let srv = start(WINSIZE);
    let pane_text = |target: &str| srv.capture(target);
    srv.cmd(&["send-keys", "-t", "main", "Enter"])
        .expect("keys");
    wait_until(
        || pane_text("main").contains("ws 80 23 0 0"),
        Duration::from_secs(10),
    )
    .unwrap_or_else(|e| panic!("no client yet: {e}\n{}", pane_text("main")));

    let mut a = Harness::spawn_attach_with_env(
        srv.command(),
        "main",
        &[("GROFE_TEST_CAPS", "cell_px=10x20")],
        80,
        24,
    );
    a.wait_for_output(MOUSE_ON, 0);
    srv.cmd(&["send-keys", "-t", "main", "Enter"])
        .expect("keys");
    wait_until(
        || pane_text("main").contains("ws 80 23 800 460"),
        Duration::from_secs(10),
    )
    .unwrap_or_else(|e| panic!("after attach: {e}\n{}", pane_text("main")));

    srv.cmd(&["split-window", "-h", "-t", "main", WINSIZE])
        .expect("split");
    let id = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let id = id.trim();
    srv.cmd(&["send-keys", "-t", id, "Enter"]).expect("keys");
    let width: u16 = srv
        .cmd(&["display-message", "-p", "-t", id, "#{pane_width}"])
        .expect("width")
        .trim()
        .parse()
        .expect("a number");
    let want = format!("ws {width} 23 {} 460", width * 10);
    wait_until(|| pane_text(id).contains(&want), Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("split pane, {want}: {e}\n{}", pane_text(id)));
}
