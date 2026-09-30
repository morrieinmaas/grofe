//! `default-shell`: a new pane with no command starts it, and sees it as
//! `$SHELL`, as tmux starts a pane's shell from the option.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::TestServer;

#[test]
fn a_new_pane_starts_the_default_shell_when_it_is_set() {
    let srv = TestServer::start();
    let shell = srv.home.join("marker-shell");
    fs::write(
        &shell,
        "#!/bin/sh\necho \"started $0 with SHELL=$SHELL\"\nexec /bin/sh\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let shell = shell.display().to_string();

    srv.cmd(&["new-session", "-d", "-s", "before"]).unwrap();
    srv.cmd(&["set-option", "-g", "default-shell", &shell])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "after"]).unwrap();
    srv.cmd(&["new-window", "-t", "after"]).unwrap();

    // The pane wraps long lines; read it as one.
    let text = |target: &str| srv.capture(target).replace('\n', "");
    for target in ["after:1", "after:2"] {
        let want = format!("marker-shell with SHELL={shell}");
        srv.wait_for(|| text(target).contains(&want), Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{target}: {e}\n{}", srv.capture(target)));
    }
    // A pane started before the option was set kept the user's shell.
    assert!(!srv.capture("before:1").contains("marker-shell"));
}

#[test]
fn an_unusable_default_shell_falls_back_as_shell_does() {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "default-shell", "/nonexistent/shell"])
        .unwrap();
    srv.cmd(&["new-session", "-d", "-s", "s"]).unwrap();
    srv.cmd(&["send-keys", "-t", "s", "echo alive-$((6*7))", "Enter"])
        .unwrap();
    srv.wait_for(
        || srv.capture("s").contains("alive-42"),
        Duration::from_secs(5),
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", srv.capture("s")));
}
