//! `#{client_pid}` is the attached client's process id (B1a), read from
//! the socket's peer credentials, not made up.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use common::{CMD_TIMEOUT, TestServer, wait_until};
use harness::Harness;

/// The pids of processes whose command line has `needle`.
fn pids_with(needle: &str) -> Vec<String> {
    let ps = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "args="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|l| l.contains(needle))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

#[test]
fn client_pid_is_the_attached_process() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        .expect("new-session");
    let _client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    wait_until(
        || {
            srv.cmd(&["list-clients", "-F", "#{client_pid}"])
                .is_ok_and(|out| !out.trim().is_empty())
        },
        CMD_TIMEOUT,
    )
    .expect("a client attached");
    let shown = srv
        .cmd(&["list-clients", "-F", "#{client_pid}"])
        .unwrap()
        .trim()
        .to_string();
    let socket = srv.socket.display().to_string();
    let ours = pids_with(&format!("{socket} attach"));
    assert!(
        ours.contains(&shown),
        "client_pid {shown} is not among the attach processes {ours:?}"
    );
}
