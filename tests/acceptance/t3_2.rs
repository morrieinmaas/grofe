//! T3.2: mouse input and the scrollback view. SGR 1006 reports are typed
//! into the attach pty as the outer terminal would send them: a click
//! focuses the pane under the pointer, the wheel scrolls a read-only view of
//! history in a pane that did not ask for the mouse, and a pane that did
//! gets the events, encoded as it asked, in its own coordinates.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::{Harness, require_tool};

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";
const WAIT: Duration = Duration::from_secs(10);

/// 200 lines of history, then a pane that shows every byte it gets.
const HISTORY: &str = "seq 1 200; stty -echo; exec cat -v";

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

fn attach(srv: &TestServer, cols: u16, rows: u16) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], cols, rows);
    a.wait_for_output(MOUSE_ON, 0);
    a
}

/// An SGR 1006 report as the outer terminal sends it: `x` and `y` are
/// 1-based client cells.
fn sgr(button: u8, x: u16, y: u16, release: bool) -> String {
    let end = if release { 'm' } else { 'M' };
    format!("\x1b[<{button};{x};{y}{end}")
}

const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;

fn top_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// The `[offset/history]` indicator anywhere on screen.
fn indicator(text: &str) -> Option<String> {
    text.match_indices('[').find_map(|(i, _)| {
        let rest = &text[i + 1..];
        let end = rest.find(']')?;
        let (a, b) = rest[..end].split_once('/')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
        (digits(a) && digits(b)).then(|| format!("[{a}/{b}]"))
    })
}

fn pane_var(srv: &TestServer, target: &str, var: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, var])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Case 1: the wheel in a plain shell opens the view five lines up, and
/// scrolling back to the bottom closes it; the pane gets nothing.
#[test]
fn wheel_in_a_shell_scrolls_history() {
    let srv = start(HISTORY);
    let mut a = attach(&srv, 80, 24);
    a.wait_for("200");
    // 200 lines and the cursor's empty line on 23 rows: 178 in history.
    a.wait_until("179 on top", WAIT, |t| top_line(t).trim_end() == "179");

    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("the view at 5", WAIT, |t| {
        top_line(t).starts_with("174") && indicator(t).as_deref() == Some("[5/178]")
    });
    let text = a.text();
    let top = top_line(&text);
    assert!(top.trim_end().ends_with("[5/178]"), "{top:?}");

    a.send(sgr(WHEEL_DOWN, 10, 5, false));
    a.wait_until("the view closed", WAIT, |t| {
        top_line(t).trim_end() == "179" && indicator(t).is_none()
    });

    a.send("z\r");
    a.wait_until("the z line", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "z")
    });
    let pane = srv.capture("main");
    assert!(!pane.contains("^["), "the pane got bytes:\n{pane}");
}

/// Case 2: Neovim asked for the mouse, so it gets the wheel and grofe
/// draws no view.
#[test]
fn wheel_in_neovim_scrolls_neovim() {
    if !require_tool("nvim") {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("lines.txt");
    let body: String = (1..=200).map(|n| format!("{n}\n")).collect();
    std::fs::write(&file, body).expect("write lines");
    let srv = start(&format!(
        "exec nvim --clean -c 'set mouse=a' '{}'",
        file.display()
    ));
    let mut a = attach(&srv, 80, 24);
    a.wait_until("nvim showing line 1", WAIT, |t| {
        top_line(t).trim_end() == "1" && t.lines().any(|l| l.trim_end() == "21")
    });

    a.send(sgr(WHEEL_DOWN, 10, 5, false));
    a.wait_until("nvim scrolled", WAIT, |t| {
        top_line(t).trim_end().parse::<u32>().is_ok_and(|n| n > 1)
    });
    let text = a.text();
    assert_eq!(indicator(&text), None, "{text}");
}

/// Two panes side by side; the right one runs `cmd` and is active.
fn side_by_side(left: &str, right: &str) -> (TestServer, String, String) {
    let srv = start(left);
    let left_id = pane_var(&srv, "main", "#{pane_id}");
    srv.cmd(&["split-window", "-h", "-t", "main", right])
        .expect("split");
    let right_id = pane_var(&srv, "main", "#{pane_id}");
    assert_ne!(left_id, right_id);
    (srv, left_id, right_id)
}

/// Case 3: a pane that asked for SGR mouse reports gets the wheel as SGR,
/// in pane coordinates; one that asked for mode 1000 alone gets X10 bytes.
#[test]
fn a_pane_that_asked_for_the_mouse_gets_it_encoded_as_asked() {
    for (modes, want) in [
        ("\\033[?1000h\\033[?1006h", "^[[<64;6;5M"),
        ("\\033[?1000h", "^[[M`&%"),
    ] {
        let tui = format!("stty -echo; printf '{modes}ready'; exec cat -v");
        let (srv, _, right) = side_by_side("cat", &tui);
        let mut a = attach(&srv, 80, 24);
        let left: u16 = pane_var(&srv, &right, "#{pane_left}")
            .parse()
            .expect("pane_left");
        wait_until(|| srv.capture(&right).contains("ready"), WAIT)
            .unwrap_or_else(|e| panic!("{modes}: the fake TUI never started: {e}"));
        // Pane cell (5, 4), 1-based (6, 5) on the wire.
        a.send(sgr(WHEEL_UP, left + 6, 5, false));
        a.send("\r");
        wait_until(|| srv.capture(&right).contains(want), WAIT)
            .unwrap_or_else(|e| panic!("{modes}: want {want:?}: {e}\n{}", srv.capture(&right)));
        assert_eq!(indicator(&a.text()), None);
    }
}

/// Case 4: a left click in the right pane makes it active.
#[test]
fn a_click_focuses_the_pane() {
    let (srv, left, right) = side_by_side("cat", "cat");
    srv.cmd(&["select-pane", "-t", &left]).expect("select left");
    let mut a = attach(&srv, 80, 24);
    a.wait_until("both panes drawn", WAIT, |t| t.contains('│'));
    assert_eq!(pane_var(&srv, &right, "#{pane_active}"), "0");
    let x: u16 = pane_var(&srv, &right, "#{pane_left}")
        .parse::<u16>()
        .expect("pane_left")
        + 3;
    a.send(sgr(0, x, 3, false));
    a.send(sgr(0, x, 3, true));
    wait_until(
        || pane_var(&srv, &right, "#{pane_active}") == "1",
        Duration::from_secs(1),
    )
    .unwrap_or_else(|e| panic!("the click did not focus {right}: {e}"));
}

/// Case 5: with `mouse off` the wheel does nothing; in the view, `q`
/// closes it and is eaten, while `x` closes it and reaches the pane.
#[test]
fn mouse_off_ignores_the_wheel_and_keys_leave_the_view() {
    let srv = start(HISTORY);
    let mut a = attach(&srv, 80, 24);
    a.wait_until("179 on top", WAIT, |t| top_line(t).trim_end() == "179");

    srv.cmd(&["set", "-g", "mouse", "off"]).expect("mouse off");
    let at = a.wait_for_output(MOUSE_OFF, 0);
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.send("a\r");
    a.wait_until("the a line", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "a")
    });
    assert_eq!(indicator(&a.text()), None, "{}", a.text());
    assert!(
        !srv.capture("main").contains("^["),
        "{}",
        srv.capture("main")
    );

    srv.cmd(&["set", "-g", "mouse", "on"]).expect("mouse on");
    a.wait_for_output(MOUSE_ON, at);
    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("the view", WAIT, |t| indicator(t).is_some());
    a.send("q");
    a.wait_until("q closed the view", WAIT, |t| indicator(t).is_none());

    a.send(sgr(WHEEL_UP, 10, 5, false));
    a.wait_until("the view again", WAIT, |t| indicator(t).is_some());
    a.send("x");
    a.wait_until("x closed the view", WAIT, |t| indicator(t).is_none());
    a.send("\r");
    a.wait_until("the x line", WAIT, |t| {
        t.lines().any(|l| l.trim_end() == "x")
    });
    let pane = srv.capture("main");
    assert!(
        !pane.lines().any(|l| l.contains('q')),
        "q reached the pane:\n{pane}"
    );
}

/// Case 6: a client smaller than the window crops (D6); a click maps
/// through its viewport to the pane really under the pointer.
#[test]
fn a_click_on_a_cropped_client_maps_through_the_viewport() {
    // Each pane keeps its cursor at a fixed cell, whatever resizes it.
    let at =
        |row: u16, col: u16| format!("while :; do printf '\\033[{row};{col}H'; sleep 0.2; done");
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
        &at(5, 59),
    ])
    .expect("new-session");
    let left = pane_var(&srv, "main", "#{pane_id}");
    srv.cmd(&["split-window", "-h", "-l", "20", "-t", "main", &at(5, 20)])
        .expect("split");
    let right = pane_var(&srv, "main", "#{pane_id}");

    // The small client first; the large one attaching last owns the size.
    let mut small = attach(&srv, 40, 12);
    let _large = attach(&srv, 80, 24);
    wait_until(|| pane_var(&srv, &left, "#{pane_width}") == "59", WAIT)
        .unwrap_or_else(|e| panic!("left pane width: {e}"));
    assert_eq!(pane_var(&srv, &right, "#{pane_left}"), "60");

    // The right pane's cursor, at window column 79, moves the small
    // client's viewport to x = 40; the left pane's, at 58, keeps it there.
    wait_until(|| small.cursor() == (39, 4), WAIT)
        .unwrap_or_else(|e| panic!("viewport never followed: {e}, {:?}", small.cursor()));
    srv.cmd(&["select-pane", "-t", &left]).expect("select left");
    wait_until(|| small.cursor() == (18, 4), WAIT)
        .unwrap_or_else(|e| panic!("left cursor: {e}, {:?}", small.cursor()));

    // Client column 30 is window column 70: the right pane. Unmapped, it
    // would be column 30, the left pane.
    small.send(sgr(0, 31, 3, false));
    small.send(sgr(0, 31, 3, true));
    wait_until(
        || pane_var(&srv, &right, "#{pane_active}") == "1",
        Duration::from_secs(1),
    )
    .unwrap_or_else(|e| panic!("the click did not reach {right}: {e}"));
}
