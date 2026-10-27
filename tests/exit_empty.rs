//! `exit-empty off` keeps the server up after its last session is killed
//! (B1a); the default still exits.

mod common;

use common::{CMD_TIMEOUT, TestServer, wait_until};

#[test]
fn exit_empty_off_keeps_the_server_after_the_last_session() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "a", "-x", "80", "-y", "24"])
        .expect("new-session");
    srv.cmd(&["set-option", "-g", "exit-empty", "off"])
        .expect("set-option");
    srv.cmd(&["kill-session", "-t", "a"]).expect("kill-session");
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(srv.is_running(), "the server stays up with no sessions");
    srv.cmd(&["new-session", "-d", "-s", "b", "-x", "80", "-y", "24"])
        .expect("a session on the same server");
    srv.cmd(&["set-option", "-g", "exit-empty", "on"])
        .expect("set-option");
    srv.cmd(&["kill-session", "-t", "b"]).expect("kill-session");
    wait_until(|| !srv.is_running(), CMD_TIMEOUT).expect("the server exits");
}
