//! `detach-on-destroy no-detached` picks the new session once per destroyed
//! session (tmux's `server_destroy_session` computes `s_new` before it moves
//! any client), so two clients of the killed session land in the same one.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use common::{CMD_TIMEOUT, TestServer, wait_until};
use harness::Harness;

#[test]
fn two_clients_of_a_killed_session_go_to_the_same_session() {
    let srv = TestServer::start();
    for name in ["a", "b", "c"] {
        srv.cmd(&["new-session", "-d", "-s", name, "-x", "80", "-y", "24"])
            .expect("new-session");
    }
    srv.cmd(&["set-option", "-g", "detach-on-destroy", "no-detached"])
        .expect("set-option");
    let _one = Harness::spawn_attach_with_env(srv.command(), "c", &[], 80, 24);
    let _two = Harness::spawn_attach_with_env(srv.command(), "c", &[], 80, 24);
    let sessions = || {
        srv.cmd(&["list-clients", "-F", "#{client_session}"])
            .map(|out| out.lines().map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    wait_until(|| sessions() == ["c", "c"], CMD_TIMEOUT).expect("both attached");
    srv.cmd(&["kill-session", "-t", "c"]).expect("kill-session");
    wait_until(|| sessions().iter().all(|s| s != "c"), CMD_TIMEOUT).expect("moved");
    let after = sessions();
    assert_eq!(after.len(), 2, "both clients stay attached: {after:?}");
    assert_eq!(after[0], after[1], "one session for both: {after:?}");
}
