//! M2 gate fixes: the format variables and current-client rules an
//! attached client exposes, checked against tmux 3.7b's answers.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{TestServer, wait_until};

use crate::harness::Harness;

const IDLE: &str = "printf 'idle-pane'; exec sleep 600";

fn attach(srv: &TestServer, target: &str) -> Command {
    let mut cmd = srv.command();
    cmd.args(["attach", "-t", target])
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    cmd
}

fn new_session(srv: &TestServer, name: &str) {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        "80",
        "-y",
        "23",
        IDLE,
    ])
    .unwrap_or_else(|e| panic!("new-session {name}: {e}"));
}

/// `display-message -p` with no target, from a CLI outside any pane.
fn display(srv: &TestServer, format: &str) -> String {
    srv.cmd(&["display-message", "-p", format])
        .unwrap_or_else(|e| panic!("display {format}: {e}"))
        .trim_end()
        .to_string()
}

fn display_t(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .unwrap_or_else(|e| panic!("display -t {target} {format}: {e}"))
        .trim_end()
        .to_string()
}

fn wait_display(srv: &TestServer, format: &str, want: &str) {
    if let Err(e) = wait_until(|| display(srv, format) == want, Duration::from_secs(10)) {
        panic!(
            "{format} never became {want:?} (is {:?}): {e}",
            display(srv, format)
        );
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

/// tmux 3.7b, one client attached to `alpha` of `alpha` and `beta`:
/// `client_session` is the client's session whatever the target, and a
/// command without a pane runs against the most recently active session
/// (created, attached, switched to or typed in).
#[test]
fn a_command_outside_any_pane_uses_the_best_client_and_session() {
    let srv = TestServer::start();
    new_session(&srv, "alpha");
    new_session(&srv, "beta");
    let mut a = Harness::spawn(attach(&srv, "alpha"), 80, 24);
    a.wait_for("[alpha]");

    assert_eq!(
        display(&srv, "#{client_session} #{session_name}"),
        "alpha alpha"
    );
    assert_eq!(
        display_t(&srv, "beta", "#{client_session} #{session_name}"),
        "alpha beta"
    );

    new_session(&srv, "gamma");
    assert_eq!(
        display(&srv, "#{client_session} #{session_name}"),
        "alpha gamma"
    );

    srv.cmd(&["switch-client", "-t", "beta"])
        .expect("switch-client");
    wait_display(&srv, "#{client_session}", "beta");
    assert_eq!(
        display(&srv, "#{session_name} #{client_last_session}"),
        "beta alpha"
    );

    srv.cmd(&["switch-client", "-t", "alpha"])
        .expect("switch-client");
    wait_display(&srv, "#{client_session}", "alpha");
    // Typing in alpha makes it the most recently active session again.
    new_session(&srv, "delta");
    assert_eq!(display(&srv, "#{session_name}"), "delta");
    a.send("x");
    wait_display(&srv, "#{session_name}", "alpha");
}

/// The values tmux 3.7b gives for the same attached 80x24 client.
#[test]
fn window_session_and_client_variables_have_tmux_values() {
    let srv = TestServer::start();
    new_session(&srv, "alpha");
    new_session(&srv, "beta");
    srv.cmd(&["split-window", "-t", "alpha", IDLE])
        .expect("split");
    srv.cmd(&["new-window", "-d", "-t", "alpha", IDLE])
        .expect("new-window");
    let before = now();
    let mut a = Harness::spawn(attach(&srv, "alpha"), 80, 24);
    a.wait_for("[alpha]");

    assert_eq!(
        display_t(
            &srv,
            "alpha",
            "#{window_width}x#{window_height} panes=#{window_panes} \
             windows=#{session_windows} attached=#{session_attached} \
             prefix=#{client_prefix} last=[#{client_last_session}]"
        ),
        "80x23 panes=2 windows=2 attached=1 prefix=0 last=[]"
    );
    assert_eq!(
        display_t(
            &srv,
            "beta",
            "#{session_attached} #{session_windows} #{window_panes}"
        ),
        "0 1 1"
    );
    assert_eq!(
        srv.cmd(&["list-sessions", "-F", "#{session_name}=#{session_attached}"])
            .expect("ls"),
        "alpha=1\nbeta=0\n"
    );

    let activity: u64 = display(&srv, "#{client_activity}")
        .parse()
        .expect("client_activity is a unix time");
    assert!(
        (before..=now()).contains(&activity),
        "client_activity {activity} not within [{before}, {}]",
        now()
    );

    a.send("\x02");
    wait_display(&srv, "#{client_prefix}", "1");
    srv.cmd(&["switch-client", "-T", "root"]).expect("-T root");
    wait_display(&srv, "#{client_prefix}", "0");

    a.send("\x02d");
    a.wait_for("[detached (from session alpha)]");
    a.wait_exit();
    assert_eq!(display_t(&srv, "alpha", "#{session_attached}"), "0");
}
