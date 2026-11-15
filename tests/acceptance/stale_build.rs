//! A server whose binary was replaced on disk (an upgrade while it runs)
//! tells each client that attaches that it is still the old build.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::process::Command;

use crate::harness::Harness;
use common::TestServer;

/// `srv`'s CLI command, but running the grofe at `exe`.
fn command_at(srv: &TestServer, exe: &std::path::Path) -> Command {
    let base = srv.command();
    let mut cmd = Command::new(exe);
    cmd.args(base.get_args());
    if let Some(dir) = base.get_current_dir() {
        cmd.current_dir(dir);
    }
    for (key, value) in base.get_envs() {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    cmd
}

/// Runs the command, retrying while exec fails with "Text file busy". `fs::copy`
/// leaves the new file open for writing until it returns, and a concurrent test
/// thread that forks in that window inherits the descriptor until its own exec
/// closes it, so executing the fresh copy can briefly fail. Not a grofe bug.
fn status_retrying_etxtbsy(mut make: impl FnMut() -> Command) -> std::process::ExitStatus {
    let mut result = None;
    common::wait_until("the copied grofe to be executable", common::CMD_TIMEOUT, || {
        match make().status() {
            Ok(status) => {
                result = Some(status);
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => false,
            Err(e) => panic!("run grofe: {e}"),
        }
    });
    result.expect("status set")
}

#[test]
fn attaching_to_a_server_whose_binary_was_replaced_warns_about_the_old_build() {
    let srv = TestServer::unstarted();
    let bin = srv.home.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");
    let exe = bin.join("grofe");
    fs::copy(env!("CARGO_BIN_EXE_grofe"), &exe).expect("copy grofe");

    let made = status_retrying_etxtbsy(|| {
        let mut cmd = command_at(&srv, &exe);
        cmd.args(["new-session", "-d", "-s", "main"]);
        cmd
    });
    assert!(made.success());

    // An upgrade, the way an installer does it: a new file renamed over the
    // old one, so the running server keeps its own (now unlinked) copy.
    let next = bin.join("grofe.new");
    fs::copy(env!("CARGO_BIN_EXE_grofe"), &next).expect("copy the new build");
    // The attach below execs the replacement; wait until no stray descriptor
    // keeps it busy.
    status_retrying_etxtbsy(|| {
        let mut cmd = Command::new(&next);
        cmd.arg("--version");
        cmd
    });
    fs::rename(&next, &exe).expect("replace grofe");

    let mut client = Harness::spawn_attach_with_env(command_at(&srv, &exe), "main", &[], 120, 24);
    client.wait_for("old grofe build");

    let _ = command_at(&srv, &exe).arg("kill-server").status();
}

#[test]
fn attaching_to_an_up_to_date_server_says_nothing_about_its_build() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main"])
        .expect("new-session");
    let mut client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 120, 24);
    client.wait_for("main");
    let log = srv.cmd(&["show-messages"]).expect("show-messages");
    assert!(!log.contains("old grofe build"), "{log}");
}
