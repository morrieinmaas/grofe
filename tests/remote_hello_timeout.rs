//! The hello timeout bounds only unattended connects (spec ssh-remote §2):
//! an interactive one may sit on a password or a security key touch. Its
//! own test binary, since it sets `GROFE_SSH` and the test hook in this
//! process's environment.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::TestServer;
use grofe::remote::connect::{
    ConnectError, ConnectOpts, Link, NO_ANSWER, TEST_HELLO_ENV, Transport,
};
use grofe::remote::ssh::{SSH_ENV, Target};

#[test]
fn only_a_batch_connect_gives_up_on_a_slow_hello() {
    let srv = TestServer::start();
    let dir = tempfile::tempdir().unwrap();
    // A fake ssh whose remote takes a second before the proxy says hello.
    let script = format!(
        "#!/bin/sh\n\
         for a; do [ \"$a\" = -G ] && {{ echo 'serveraliveinterval 0'; exit 0; }}; done\n\
         sleep 1\n\
         GROFE='{sock}' XDG_RUNTIME_DIR='{run}' XDG_STATE_HOME='{state}' exec '{grofe}' proxy\n",
        sock = srv.socket.display(),
        run = srv.runtime_dir.display(),
        state = srv.state_home.display(),
        grofe = env!("CARGO_BIN_EXE_grofe"),
    );
    let ssh = dir.path().join("ssh");
    fs::write(&ssh, script).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary's only test; nothing else reads the environment
    // concurrently.
    unsafe {
        std::env::set_var(SSH_ENV, &ssh);
        std::env::set_var(TEST_HELLO_ENV, "200");
    }
    let to = Transport::Ssh(Target::host("box"));

    let link = Link::connect(&to, ConnectOpts { batch: false }).expect("interactive connect waits");
    assert!(link.dropped().is_none());
    link.close();

    match Link::connect(&to, ConnectOpts { batch: true }) {
        Err(ConnectError::Failed(why)) => assert_eq!(why, NO_ANSWER),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a batch connect waited past its bound"),
    }
}
