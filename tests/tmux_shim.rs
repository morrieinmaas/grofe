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
