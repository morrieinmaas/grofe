//! `exit-unattached` and the CLI's hand-over from its target check to its
//! attach connection.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;

use common::{CMD_TIMEOUT, TestServer};

use crate::harness::Harness;

/// With `exit-unattached` on, an attaching `new-session` on a server it
/// starts shows its session, as tmux 3.7b's does: the CLI's own short
/// connections going does not end the server before it attaches.
#[test]
fn an_attaching_new_session_survives_exit_unattached() {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), "exit-unattached = true\n").unwrap();
    let mut cmd = srv.command();
    cmd.args([
        "new-session",
        "-s",
        "main",
        "printf 'up-and-attached'; exec cat",
    ])
    .env("TERM", "xterm-256color");
    let mut client = Harness::spawn(cmd, 80, 24);
    client.wait_until("the session", CMD_TIMEOUT, |text| {
        text.contains("up-and-attached")
    });
}
