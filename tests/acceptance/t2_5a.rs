//! T2.5a: clients and frames. `grofe attach` in a pty against a real server:
//! the first frame shows the pane, input reaches it, detach and reattach
//! resize the window to the new client (spec D6), two clients of different
//! sizes letterbox and crop with a debounced size owner, and a client that
//! stops reading never delays another one.
//!
//! Panes run a fixed marker command instead of a real shell prompt (M2
//! addendum): `PROMPT` prints `grofe-prompt$ ` once, then for every line it
//! reads prints `got-<line>` at row 20, column 60, which is also where it
//! leaves the cursor.

#[path = "../common/mod.rs"]
mod common;

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use grofe::protocol::{Caps, ClientCtx, Hello, encode_hello};

use crate::harness::Harness;

const PROMPT: &str = "stty -echo; printf 'grofe-prompt$ '; \
                      while read l; do printf '\\033[20;60Hgot-%s' \"$l\"; done";

fn attach(srv: &TestServer, target: &str) -> Command {
    let mut cmd = srv.command();
    cmd.args(["attach", "-t", target])
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    cmd
}

fn start(size: (u16, u16)) -> TestServer {
    let srv = TestServer::start();
    let (x, y) = (size.0.to_string(), size.1.to_string());
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        &x,
        "-y",
        &y,
        PROMPT,
    ])
    .expect("new-session");
    srv
}

fn pane_size(srv: &TestServer) -> String {
    srv.cmd(&[
        "display-message",
        "-p",
        "-t",
        "main",
        "#{pane_width}x#{pane_height}",
    ])
    .unwrap_or_default()
    .trim()
    .to_string()
}

fn wait_pane_size(srv: &TestServer, want: &str) {
    if let Err(e) = wait_until(|| pane_size(srv) == want, Duration::from_secs(10)) {
        panic!("pane never became {want} (is {}): {e}", pane_size(srv));
    }
}

#[test]
fn attach_shows_the_prompt_and_input_reaches_the_pane() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("grofe-prompt$ ");
    // The status line: default status-left, then the window list.
    a.wait_for("[main] 1:");
    let clients = srv.cmd(&["list-clients"]).expect("list-clients");
    assert!(
        clients.contains(": main [80x24 xterm-256color]"),
        "{clients}"
    );
    let widths = srv
        .cmd(&[
            "list-clients",
            "-F",
            "#{client_width}x#{client_height} #{session_name}",
        ])
        .expect("list-clients -F");
    assert_eq!(widths, "80x24 main\n");

    a.send("hello\r");
    a.wait_for("got-hello");
    // The cursor follows the pane's: row 20, just past `got-hello`.
    assert_eq!(a.cursor(), (68, 19));

    srv.cmd(&["detach-client"]).expect("detach-client");
    a.wait_for("[detached (from session main)]");
    a.wait_exit();
    assert_eq!(srv.cmd(&["list-clients"]).expect("list-clients"), "");
}

#[test]
fn reattach_resizes_the_window_to_the_new_client() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "80x23");
    srv.cmd(&["detach-client"]).expect("detach");
    a.wait_exit();

    // Smaller: the window shrinks to the client, less the status row.
    let mut small = Harness::spawn(attach(&srv, "main"), 40, 12);
    small.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "40x11");
    srv.cmd(&["detach-client"]).expect("detach");
    small.wait_exit();

    // Larger: it grows again.
    let mut large = Harness::spawn(attach(&srv, "main"), 120, 40);
    large.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "120x39");
    large.send("big\r");
    large.wait_for("got-big");
    srv.cmd(&["detach-client"]).expect("detach");
    large.wait_exit();
}

/// Spec D6 with two clients (M2 addendum): the latest active client owns
/// the window size, switching owner is debounced by 500 ms, a larger client
/// letterboxes and a smaller one crops with the viewport on the cursor.
#[test]
fn two_clients_letterbox_and_crop_with_a_debounced_owner() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "80x23");
    assert!(!a.text().contains('·'));

    // B attaches (activity): it is the latest, the window follows it and
    // A letterboxes.
    let mut b = Harness::spawn(attach(&srv, "main"), 40, 12);
    b.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "40x11");
    a.wait_for("·");

    // A types: after the debounce the window is A's size again.
    let typed = Instant::now();
    a.send("a\r");
    wait_pane_size(&srv, "80x23");
    let took = typed.elapsed();
    assert!(
        took >= Duration::from_millis(450),
        "switched after {took:?}"
    );
    a.wait_until("no letterbox", Duration::from_secs(5), |t| !t.contains('·'));

    // The pane draws far from the top-left: B crops, with its viewport
    // moved just enough to show the cursor at its bottom-right.
    a.send("x\r");
    a.wait_for("got-x");
    b.wait_for("got-x");
    let screen = b.text();
    assert!(!screen.contains("grofe-prompt"), "not cropped:\n{screen}");
    // Window cursor (64, 19), 40x11 visible: viewport (25, 9).
    assert_eq!(b.cursor(), (39, 10), "screen:\n{screen}");
    let row = screen.lines().nth(10).unwrap_or_default();
    assert!(row.trim_end().ends_with("got-x"), "row 10 is {row:?}");
}

/// A client that stops reading (a raw socket with a tiny receive buffer,
/// never read) while a pane floods must not delay a second client or the
/// actor (M2 addendum).
#[test]
fn a_client_that_stops_reading_does_not_delay_another() {
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
        "exec seq 1 1000000000",
    ])
    .expect("new-session");

    let mut stuck = UnixStream::connect(&srv.socket).expect("connect");
    let tiny: nix::libc::c_int = 1024;
    // SAFETY: a valid socket fd and a pointer to a live c_int of the size given.
    let rc = unsafe {
        nix::libc::setsockopt(
            stuck.as_raw_fd(),
            nix::libc::SOL_SOCKET,
            nix::libc::SO_RCVBUF,
            (&tiny as *const nix::libc::c_int).cast(),
            std::mem::size_of::<nix::libc::c_int>() as nix::libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "setsockopt SO_RCVBUF");
    let hello = Hello::Attach {
        ctx: ClientCtx {
            cwd: srv.home.clone(),
            pane: None,
            env: Vec::new(),
            client: None,
        },
        cols: 300,
        rows: 100,
        term: "xterm-256color".to_string(),
        caps: Caps::default(),
        target: Some("main".to_string()),
        detach_others: false,
        nest: false,
    };
    let line = encode_hello(&hello).expect("hello") + "\n";
    stuck.write_all(line.as_bytes()).expect("send hello");
    // Let the flood fill the stuck client's socket.
    sleep(Duration::from_millis(1000));

    let mut b = Harness::spawn(attach(&srv, "main"), 80, 24);
    b.wait_for("[main]");

    let asked = Instant::now();
    srv.cmd(&["set-option", "-g", "status-left", "MARK-1 "])
        .expect("set status-left");
    b.wait_for_timeout("MARK-1", Duration::from_secs(1));
    assert!(asked.elapsed() < Duration::from_secs(1));

    for _ in 0..5 {
        let asked = Instant::now();
        let sessions = srv.cmd(&["list-sessions"]).expect("list-sessions");
        let took = asked.elapsed();
        assert!(sessions.starts_with("main:"), "{sessions}");
        assert!(
            took < Duration::from_millis(200),
            "list-sessions took {took:?}"
        );
    }
    drop(stuck);
}

/// A 1-column client: the window is 1 column wide, but the pane's pty and
/// emulator stay 2 wide, since alacritty panics writing a wide character
/// into a 1-column terminal (M2 addendum). The pane keeps working.
#[test]
fn a_one_column_window_keeps_its_pane_two_columns_wide() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 20, 6);
    a.wait_for("grofe-prompt$");
    a.resize(1, 6);
    wait_pane_size(&srv, "1x5");
    srv.cmd(&["send-keys", "-t", "main", "-l", "你好"])
        .expect("send-keys");
    srv.cmd(&["send-keys", "-t", "main", "Enter"])
        .expect("send-keys");
    a.resize(40, 12);
    wait_pane_size(&srv, "40x11");
    srv.cmd(&["send-keys", "-t", "main", "still-alive", "Enter"])
        .expect("send-keys");
    // Printed at column 60 of a 40-column pane: it wraps.
    a.wait_for("still-alive");
    assert!(srv.cmd(&["has-session", "-t", "main"]).is_ok());
}

/// `attach` from inside a pane (spec §4.6): the pane's own session is
/// refused without `--nest`, another session is allowed.
#[test]
fn attach_from_inside_a_pane_refuses_only_self_nesting() {
    let srv = start((80, 23));
    srv.cmd(&["new-session", "-d", "-s", "other", PROMPT])
        .expect("new-session other");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let inside = |target: &str, extra: &[&str]| {
        let mut cmd = attach(&srv, target);
        cmd.args(extra)
            .env("GROFE", &srv.socket)
            .env("GROFE_PANE", pane.trim());
        cmd
    };

    let mut refused = Harness::spawn(inside("main", &[]), 80, 24);
    refused.wait_for("refusing to attach session main from inside itself");
    refused.wait_exit();

    let mut other = Harness::spawn(inside("other", &[]), 80, 24);
    other.wait_for("[other]");
    srv.cmd(&["detach-client"]).expect("detach");
    other.wait_exit();

    let mut nested = Harness::spawn(inside("main", &["--nest"]), 80, 24);
    nested.wait_for("[main]");
    srv.cmd(&["detach-client"]).expect("detach");
    nested.wait_exit();
}

/// The client is told why it goes away: its session was killed, or the
/// server exited.
#[test]
fn the_client_hears_why_it_was_let_go() {
    let srv = start((80, 23));
    srv.cmd(&["new-session", "-d", "-s", "other", PROMPT])
        .expect("new-session other");
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main]");
    srv.cmd(&["kill-session", "-t", "main"])
        .expect("kill-session");
    a.wait_for("[exited]");
    a.wait_exit();

    let mut b = Harness::spawn(attach(&srv, "other"), 80, 24);
    b.wait_for("[other]");
    srv.cmd(&["kill-server"]).expect("kill-server");
    b.wait_for("[server exited]");
    b.wait_exit();
}
