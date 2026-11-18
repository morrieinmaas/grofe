//! A reconnect's QUIC bootstrap is bounded (spec ssh-remote §4): with a
//! fake ssh whose `proxy --quic` never answers, a batch bootstrap fails
//! within the hello bound and the ssh child is killed. Its own binary: the
//! test sets `GROFE_SSH` and the bound in this process's environment.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use grofe::remote::connect::{NO_ANSWER, TEST_HELLO_ENV};
use grofe::remote::roam::{self, BootError};
use grofe::remote::ssh::{SSH_ENV, Target};
use nix::unistd::Pid;

#[test]
fn a_hung_bootstrap_in_batch_is_killed_within_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let script = format!(
        r#"#!/bin/sh
for a; do [ "$a" = -G ] && {{ echo "serveraliveinterval 0"; exit 0; }}; done
case "$*" in *"proxy --quic"*) echo $$ > '{root}/pid'; exec sleep 60 ;; esac
exit 255
"#,
        root = root.display()
    );
    let ssh = root.join("ssh");
    fs::write(&ssh, script).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: the only test in this binary; no other thread reads the
    // environment while it is set.
    unsafe {
        std::env::set_var(SSH_ENV, &ssh);
        std::env::set_var(TEST_HELLO_ENV, "500");
    }
    let started = Instant::now();
    let got = roam::bootstrap(&Target::host("box"), true);
    let took = started.elapsed();
    match got {
        Err(BootError::Connect(e)) => assert!(e.to_string().contains(NO_ANSWER), "{e}"),
        other => panic!("{other:?}"),
    }
    assert!(took < Duration::from_secs(10), "took {took:?}");
    let pid: i32 = fs::read_to_string(root.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        common::process_gone(Pid::from_raw(pid)),
        "the ssh child still runs"
    );
}
