//! B4 review: a locked client's keys reach only its `lock-command`, even
//! with a command prompt left open under the lock.

#[path = "../common/mod.rs"]
mod common;

use std::thread::sleep;
use std::time::Duration;

use common::TestServer;

use crate::harness::Harness;

#[test]
fn a_prompt_left_open_takes_no_keys_while_the_client_is_locked() {
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
        "sleep 100000",
    ])
    .expect("new-session");
    srv.cmd(&[
        "set-option",
        "-g",
        "lock-command",
        "printf LOCKED; cat >/dev/null",
    ])
    .expect("lock-command");
    let mut cmd = srv.command();
    cmd.args(["attach", "-t", "main"])
        .env("TERM", "xterm-256color");
    let mut a = Harness::spawn(cmd, 80, 24);
    a.wait_for("[main] 1:");
    // The prefix, then `:`: the command prompt opens on the client.
    a.send("\x02:");
    a.wait_until("the prompt", Duration::from_secs(10), |t| {
        t.lines().last().is_some_and(|l| l.starts_with(':'))
    });
    srv.cmd(&["lock-client"]).expect("lock-client");
    a.wait_for("LOCKED");
    a.send("set -g @pwned 1\r");
    sleep(Duration::from_millis(500));
    let got = srv
        .cmd(&["show-options", "-gqv", "@pwned"])
        .expect("show-options");
    assert_eq!(got.trim(), "", "the prompt behind the lock ran a command");
    let _ = srv.cmd(&["kill-server"]);
}
