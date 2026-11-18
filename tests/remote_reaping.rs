//! Every ssh child a connect starts is reaped, even when the proxy ends
//! right after its hello, before the link has its child to kill. Its own
//! test binary: it sets `GROFE_SSH` in this process's environment and
//! counts this process's unreaped children.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use grofe::remote::connect::{ConnectOpts, Link, Transport};
use grofe::remote::proxy_proto::{ProxyHello, encode_hello};
use grofe::remote::ssh::{SSH_ENV, Target};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

#[test]
fn a_proxy_that_exits_after_its_hello_leaves_no_zombie() {
    let dir = tempfile::tempdir().unwrap();
    // A fake ssh whose proxy says hello and exits at once.
    let hello = encode_hello(&ProxyHello::current(&[]));
    let script = format!(
        "#!/bin/sh\n\
         for a; do [ \"$a\" = -G ] && {{ echo 'serveraliveinterval 0'; exit 0; }}; done\n\
         printf '%s' '{}'\n",
        hello.trim_end().to_string() + "\n"
    );
    let ssh = dir.path().join("ssh");
    fs::write(&ssh, script).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary's only test; nothing reads the environment
    // concurrently.
    unsafe { std::env::set_var(SSH_ENV, &ssh) };
    let to = Transport::Ssh(Target::host("box"));
    for _ in 0..30 {
        if let Ok(link) = Link::connect(&to, ConnectOpts { batch: true }) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while link.dropped().is_none() {
                assert!(Instant::now() < deadline, "the link never noticed the end");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    // Every child was waited for: none is left to reap.
    match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
        Err(nix::errno::Errno::ECHILD) | Ok(WaitStatus::StillAlive) => {}
        other => panic!("an unreaped ssh child: {other:?}"),
    }
}
