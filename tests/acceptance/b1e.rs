//! B1e: a window shown in two sessions at once, each with a client of its
//! own size. tmux's `window-size latest` sizes it for the client that was
//! active last; here too, and it does not flap between the two.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::process::Command;
use std::time::Duration;

use common::{TestServer, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

fn attach(srv: &TestServer, target: &str) -> Command {
    let mut cmd = srv.command();
    cmd.args(["attach", "-t", target])
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    cmd
}

fn size(srv: &TestServer) -> String {
    srv.cmd(&[
        "display-message",
        "-p",
        "-t",
        "a:1",
        "#{window_width}x#{window_height}",
    ])
    .unwrap_or_else(|e| panic!("display: {e}"))
    .trim_end()
    .to_string()
}

fn wait_size(srv: &TestServer, want: &str) {
    if let Err(e) = wait_until(|| size(srv) == want, WAIT) {
        panic!("the window never became {want} (is {}): {e}", size(srv));
    }
}

#[test]
fn a_shared_window_is_sized_for_the_client_that_was_active_last() {
    let srv = TestServer::start();
    for name in ["a", "b"] {
        srv.cmd(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "23",
            "sleep 600",
        ])
        .unwrap_or_else(|e| panic!("new-session {name}: {e}"));
    }
    srv.cmd(&["link-window", "-d", "-s", "a:1", "-t", "b:5"])
        .expect("link-window");
    let mut a = Harness::spawn(attach(&srv, "a"), 80, 24);
    a.wait_for("[a]");
    wait_size(&srv, "80x23");
    // b's client attaches later, so the window is b's size.
    let mut b = Harness::spawn(attach(&srv, "b"), 50, 12);
    b.wait_for("[b]");
    wait_size(&srv, "50x11");
    // Typing in a makes it the one that was active last, and back again.
    a.send("x");
    wait_size(&srv, "80x23");
    b.send("y");
    wait_size(&srv, "50x11");
}

/// A command run from a pane in a window linked into a and b, with the
/// only client on b, finds that client (`refresh-client` needs one), though
/// the window's first link is a.
#[test]
fn a_command_from_a_linked_pane_finds_the_client_of_the_other_session() {
    let srv = TestServer::start();
    for name in ["a", "b"] {
        srv.cmd(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "23",
            "sleep 600",
        ])
        .unwrap_or_else(|e| panic!("new-session {name}: {e}"));
    }
    srv.cmd(&["link-window", "-d", "-s", "a:1", "-t", "b:5"])
        .expect("link-window");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "a:1", "#{pane_id}"])
        .expect("pane id")
        .trim_end()
        .to_string();
    let mut b = Harness::spawn(attach(&srv, "b"), 80, 24);
    b.wait_for("[b]");
    let mut cmd = srv.command();
    cmd.args(["refresh-client", "-S"])
        .env("GROFE", &srv.socket)
        .env("GROFE_PANE", &pane);
    let out = common::run_bounded(cmd, WAIT);
    assert!(out.status.success(), "{}", out.stderr);
}
