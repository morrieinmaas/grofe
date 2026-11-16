//! Bare `grofe` (no arguments) against a running server: it reattaches the
//! session used last and fires `client-attached`, as `attach-session`
//! does. Bare `tmux` keeps tmux 3.7b's `new-session`.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::process::Command;
use std::time::Duration;

use common::{TestServer, try_wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn attached_to(srv: &TestServer, session: &str) -> bool {
    srv.cmd(&["list-clients", "-F", "#{client_session}"])
        .is_ok_and(|out| out.trim() == session)
}

fn bare_grofe(srv: &TestServer) -> Harness {
    let mut cmd = srv.command();
    cmd.env("TERM", "xterm-256color");
    Harness::spawn(cmd, 80, 24)
}

/// Bare `grofe` lands on the session used last, not the one made last.
#[test]
fn bare_grofe_on_a_running_server_attaches_its_latest_session() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "first"]);
    ok(&srv, &["new-session", "-d", "-s", "second"]);
    let mut cmd = srv.command();
    cmd.args(["attach-session", "-t", "first"])
        .env("TERM", "xterm-256color");
    let client = Harness::spawn(cmd, 80, 24);
    try_wait_until(|| attached_to(&srv, "first"), WAIT).expect("first attached");
    drop(client);
    try_wait_until(
        || {
            srv.cmd(&["list-clients"])
                .is_ok_and(|out| out.trim().is_empty())
        },
        WAIT,
    )
    .expect("the client goes");

    let _client = bare_grofe(&srv);
    try_wait_until(|| attached_to(&srv, "first"), WAIT).expect("attached to the session used last");
    assert_eq!(
        ok(&srv, &["list-sessions", "-F", "#{session_name}"]),
        "first\nsecond\n"
    );
}

/// A bare reattach is an attach, so `client-attached` fires; a bare start
/// makes the session and attaches it, as `new-session` does, so it does not.
#[test]
fn bare_grofe_fires_client_attached_only_when_it_reattaches() {
    let srv = TestServer::start();
    let mark = srv.home.join("hooked");
    ok(&srv, &["new-session", "-d", "-s", "first"]);
    let hook = format!("run-shell 'touch {}'", mark.display());
    ok(&srv, &["set-hook", "-g", "client-attached", &hook]);
    let _client = bare_grofe(&srv);
    try_wait_until(|| attached_to(&srv, "first"), WAIT).expect("reattached");
    try_wait_until(|| mark.exists(), WAIT).expect("client-attached fired");
}

/// Bare `grofe` on a server without sessions makes one, and that attach
/// does not fire `client-attached`.
#[test]
fn bare_grofe_on_an_empty_server_makes_a_session_without_the_hook() {
    let srv = TestServer::start();
    let mark = srv.home.join("hooked");
    let hook = format!("run-shell 'touch {}'", mark.display());
    ok(&srv, &["set-hook", "-g", "client-attached", &hook]);
    let _client = bare_grofe(&srv);
    try_wait_until(
        || {
            srv.cmd(&["list-clients", "-F", "#{client_session}"])
                .is_ok_and(|out| !out.trim().is_empty())
        },
        WAIT,
    )
    .expect("attached to a new session");
    ok(&srv, &["run-shell", "true"]);
    assert!(!mark.exists(), "client-attached fired for a new session");
}

/// Bare `tmux` (grofe under tmux's name) against a running server still
/// makes a new session, as tmux 3.7b does.
#[test]
fn bare_tmux_on_a_running_server_makes_a_new_session() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "first"]);
    let link = srv.home.join("tmux");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), &link).expect("symlink tmux");
    // The harness's command, run under tmux's name.
    let base = srv.command();
    let mut cmd = Command::new(&link);
    cmd.args(base.get_args()).current_dir(&srv.home);
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.env("TERM", "xterm-256color");
    let _client = Harness::spawn(cmd, 80, 24);
    try_wait_until(
        || {
            srv.cmd(&["list-sessions", "-F", "#{session_name}"])
                .is_ok_and(|out| out.lines().count() == 2)
        },
        WAIT,
    )
    .expect("a second session");
}
