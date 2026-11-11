//! Inside grofe, `tmux` is grofe: the server puts a `tmux` link to itself
//! first on the `PATH` its panes and jobs get, so scripts and plugins that
//! call `tmux` reach this server rather than a real tmux, which would hang
//! on grofe's socket.

mod common;

use std::time::Duration;

use common::TestServer;

#[test]
fn a_run_shell_job_calling_tmux_reaches_this_server() {
    let srv = TestServer::start();
    srv.cmd(&["run-shell", "tmux set -g @from-job yes"])
        .expect("run-shell");
    let value = srv.cmd(&["show-options", "-gv", "@from-job"]);
    assert_eq!(value.as_deref(), Ok("yes\n"));
}

#[test]
fn a_pane_calling_tmux_reaches_this_server() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "tmux set -g @from-pane yes; exec sleep 600",
    ])
    .expect("new-session");
    srv.wait_for(
        || srv.cmd(&["show-options", "-gv", "@from-pane"]).as_deref() == Ok("yes\n"),
        Duration::from_secs(10),
    )
    .expect("the pane's tmux set the option");
}

/// tmux's `^` and `$` window targets (cmd-find.c) name a session's first
/// and last window; plugins use them, and start-from-tmux's tests did.
#[test]
fn tmux_reads_the_first_and_last_window_targets() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "s"])
        .expect("new-session");
    srv.cmd(&["new-window", "-d", "-t", "s:5"])
        .expect("new-window");
    srv.cmd(&["new-window", "-d", "-t", "s:9"])
        .expect("new-window");
    srv.cmd(&[
        "run-shell",
        "tmux set -g @first \"$(tmux display -p -t 's:^' '##{window_index}')\"; \
         tmux set -g @last \"$(tmux display -p -t 's:$' '##{window_index}')\"",
    ])
    .expect("run-shell");
    let show = |name: &str| srv.cmd(&["show-options", "-gv", name]);
    let first = srv.cmd(&["list-windows", "-t", "s", "-F", "#{window_index}"]);
    let first = first.expect("list-windows");
    let first = first.lines().next().expect("a window").to_string();
    assert_eq!(show("@first").as_deref(), Ok(format!("{first}\n").as_str()));
    assert_eq!(show("@last").as_deref(), Ok("9\n"));
}
