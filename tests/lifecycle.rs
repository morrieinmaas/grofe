//! Server lifecycle (spec §4.1): auto-start by verb, one server per socket,
//! fd hygiene, kill-server, stale sockets, the server log.

mod common;

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use common::{TestServer, exists, wait_child, wait_until};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn new_session_auto_starts_a_server() {
    let srv = TestServer::unstarted();
    assert!(!srv.is_running());

    // The stub dispatcher rejects new-session until T1.5a; the auto-start
    // decision is the CLI's and happens regardless.
    let _ = srv.run(&["new-session", "-d", "-s", "t"]);

    assert!(srv.is_running(), "log:\n{}", srv.log());
    assert_eq!(srv.server_starts(), 1);
}

#[test]
fn second_new_session_reuses_the_server() {
    let srv = TestServer::unstarted();
    let _ = srv.run(&["new-session", "-d"]);
    let _ = srv.run(&["new-session", "-d"]);

    assert!(srv.is_running());
    assert_eq!(srv.server_starts(), 1, "log:\n{}", srv.log());
}

#[test]
fn ten_concurrent_new_sessions_start_exactly_one_server() {
    let srv = TestServer::unstarted();
    let children: Vec<_> = (0..10)
        .map(|_| {
            let mut cmd = srv.command();
            cmd.args(["new-session", "-d"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn grofe")
        })
        .collect();
    for child in children {
        wait_child(child, Duration::from_secs(10)).expect("CLI exits");
    }

    assert!(srv.is_running());
    assert_eq!(srv.server_starts(), 1, "log:\n{}", srv.log());
}

#[test]
fn set_without_a_server_fails_and_starts_nothing() {
    let srv = TestServer::unstarted();
    let out = srv.run(&["set", "-g", "@x", "1"]);

    assert!(!out.status.success());
    let want = format!("no server running on {}", srv.socket.display());
    assert!(out.stderr.contains(&want), "stderr: {:?}", out.stderr);
    assert!(!exists(&srv.socket));
    assert!(!srv.is_running());
    assert_eq!(srv.server_starts(), 0);
}

#[test]
fn start_server_exits_zero() {
    let srv = TestServer::unstarted();
    assert!(srv.cmd(&["start-server"]).is_ok());
    assert!(srv.is_running());
    // Idempotent: a second start-server reuses the running server.
    assert!(srv.cmd(&["start-server"]).is_ok());
    assert_eq!(srv.server_starts(), 1);
}

#[test]
fn cli_with_an_extra_open_pipe_exits_and_the_reader_sees_eof() {
    let srv = TestServer::unstarted();
    let (read_end, write_end) = cloexec_pipe();
    let raw = write_end.as_raw_fd();

    let mut cmd = srv.command();
    cmd.args(["new-session", "-d"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only an async-signal-safe fcntl between fork and exec. It
    // clears CLOEXEC so the CLI inherits the pipe's write end, as a caller
    // like `$(grofe ...)` or a test runner's pipe would leave it.
    unsafe {
        cmd.pre_exec(move || {
            fcntl(
                BorrowedFd::borrow_raw(raw),
                FcntlArg::F_SETFD(FdFlag::empty()),
            )
            .map(|_| ())
            .map_err(std::io::Error::from)
        });
    }
    let child = cmd.spawn().expect("spawn grofe");
    wait_child(child, Duration::from_secs(10)).expect("CLI exits");
    drop(write_end);

    assert!(
        reads_eof_within(&read_end, WAIT),
        "the server kept the CLI's inherited pipe open"
    );
    assert!(srv.is_running(), "log:\n{}", srv.log());
}

#[test]
fn kill_server_stops_it_and_removes_the_socket() {
    let srv = TestServer::start();
    assert!(exists(&srv.socket));

    srv.cmd(&["kill-server"]).expect("kill-server succeeds");

    assert!(!exists(&srv.socket), "socket still present");
    assert!(!srv.is_running());
}

#[test]
fn a_stale_socket_is_replaced() {
    let srv = TestServer::unstarted();
    // A socket file nobody listens on: what a crashed server leaves behind.
    drop(UnixListener::bind(&srv.socket).expect("bind stale socket"));
    assert!(exists(&srv.socket));
    // On macOS std sets CLOEXEC on the listener after socket(), so a CLI
    // forked by a concurrent test can hold it briefly: wait for that to end.
    wait_until(|| UnixStream::connect(&srv.socket).is_err(), WAIT)
        .expect("stale socket still accepts connections");

    // A non-starting verb reports no server and leaves it alone.
    let out = srv.run(&["list-sessions"]);
    assert!(!out.status.success());
    assert!(out.stderr.contains("no server running on"));

    let _ = srv.run(&["new-session", "-d"]);
    assert!(srv.is_running(), "log:\n{}", srv.log());
    assert_eq!(srv.server_starts(), 1);
}

#[test]
fn server_log_lives_under_the_test_state_dir() {
    let srv = TestServer::start();
    let log = srv.log_path();
    assert!(log.starts_with(&srv.state_home));
    assert!(log.exists(), "no log at {}", log.display());
    assert!(srv.log().contains(&srv.socket.display().to_string()));
}

#[test]
fn unknown_command_is_an_error_with_nonzero_exit() {
    let srv = TestServer::start();
    let err = srv.cmd(&["frobnicate"]).unwrap_err();
    assert!(err.contains("unknown command"), "stderr: {err:?}");
    // The server survives a bad request.
    assert!(srv.is_running());
}

#[test]
fn dropping_the_harness_stops_the_server_even_on_panic() {
    let socket = std::sync::Mutex::new(None);
    let result = std::panic::catch_unwind(|| {
        let srv = TestServer::start();
        *socket.lock().unwrap() = Some(srv.socket.clone());
        panic!("test failure while the server runs");
    });
    assert!(result.is_err());
    let socket = socket.lock().unwrap().clone().expect("server started");
    wait_until(|| UnixStream::connect(&socket).is_err(), WAIT)
        .expect("server still running after the harness dropped");
}

#[test]
fn subscribe_reports_the_servers_answer() {
    let srv = TestServer::start();
    // No events exist until T1.6; the stub dispatcher rejects the request
    // and the harness surfaces that as an error instead of hanging.
    assert!(srv.subscribe(&["pane-exited"]).is_err());
}

fn cloexec_pipe() -> (OwnedFd, OwnedFd) {
    let (r, w) = nix::unistd::pipe().expect("pipe");
    for fd in [&r, &w] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).expect("set cloexec");
    }
    (r, w)
}

/// Whether reading `fd` hits EOF within `timeout`.
fn reads_eof_within(fd: &OwnedFd, timeout: Duration) -> bool {
    let ms = u16::try_from(timeout.as_millis()).unwrap_or(u16::MAX);
    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    // Readable (or hung up) means the last writer closed: with no data ever
    // written, the read returns 0.
    match poll(&mut fds, PollTimeout::from(ms)) {
        Ok(n) if n > 0 => {
            let mut f = File::from(fd.try_clone().unwrap());
            let mut buf = [0u8; 1];
            matches!(f.read(&mut buf), Ok(0))
        }
        _ => false,
    }
}
