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

use common::{TestServer, exists, process_gone, wait_child, wait_until};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn new_session_auto_starts_a_server() {
    let srv = TestServer::unstarted();
    assert!(!srv.is_running());

    let out = srv.run(&["new-session", "-d", "-s", "t"]);

    assert_reached_server(&out);
    assert!(srv.is_running(), "log:\n{}", srv.log());
    assert_eq!(srv.server_starts(), 1);
}

/// The CLI got its request to the server, which created the session.
fn assert_reached_server(out: &common::CmdOutput) {
    assert!(out.status.success(), "new-session failed: {out:?}");
}

#[test]
fn second_new_session_reuses_the_server() {
    let srv = TestServer::unstarted();
    assert_reached_server(&srv.run(&["new-session", "-d"]));
    assert_reached_server(&srv.run(&["new-session", "-d"]));

    assert!(srv.is_running());
    assert_eq!(srv.server_starts(), 1, "log:\n{}", srv.log());
}

#[test]
fn ten_concurrent_new_sessions_start_exactly_one_server() {
    let srv = TestServer::unstarted();
    let outs: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..10)
            .map(|_| s.spawn(|| srv.run(&["new-session", "-d"])))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for out in &outs {
        assert_reached_server(out);
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

/// A server that no longer accepts on its socket but still runs (it unlinks
/// the socket first when it shuts down, so a stalled shutdown looks like
/// this) is still killed when the harness drops.
#[test]
fn dropping_the_harness_stops_a_server_whose_socket_is_gone() {
    let srv = TestServer::start();
    let pid = logged_server_pid(&srv);
    std::fs::remove_file(&srv.socket).expect("remove the socket");
    assert!(!srv.is_running());
    assert!(!process_gone(pid));

    drop(srv);
    wait_until(|| process_gone(pid), WAIT).expect("server outlived its harness");
}

#[test]
fn subscribe_reports_the_servers_answer() {
    let srv = TestServer::start();
    // The harness surfaces the server's answer either way instead of
    // hanging: accepted, or refused with the server's error.
    assert!(srv.subscribe(&["pane-exited"]).is_ok());
    assert!(srv.subscribe(&["no-such-event"]).is_err());
}

/// Runs the CLI without `-S` (the harness always passes one), so the socket
/// lands in grofe's own directory under the test's `XDG_RUNTIME_DIR`.
fn run_without_s(srv: &TestServer, args: &[&str]) -> common::CmdOutput {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_grofe"));
    cmd.args(args)
        .current_dir(&srv.home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env("HOME", &srv.home)
        .env("XDG_STATE_HOME", &srv.state_home)
        .env("XDG_RUNTIME_DIR", &srv.runtime_dir);
    common::run_bounded(cmd, common::CMD_TIMEOUT)
}

#[test]
fn default_socket_dir_is_created_private() {
    use std::os::unix::fs::PermissionsExt;

    // This test cleans up its own server: TestServer only knows its `-S`.
    let srv = TestServer::unstarted();
    let started = run_without_s(&srv, &["-L", "t", "start-server"]);
    let dir = srv.runtime_dir.join("grofe");
    let mode = std::fs::metadata(&dir).map(|m| m.permissions().mode() & 0o777);
    let running = UnixStream::connect(dir.join("t.sock")).is_ok();
    let killed = run_without_s(&srv, &["-L", "t", "kill-server"]);

    assert!(started.status.success(), "{started:?}");
    assert_eq!(mode.ok(), Some(0o700));
    assert!(running);
    assert!(killed.status.success(), "{killed:?}");
}

#[test]
fn a_loose_default_socket_dir_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let srv = TestServer::unstarted();
    let dir = srv.runtime_dir.join("grofe");
    std::fs::create_dir(&dir).expect("create socket dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod 0777");

    let out = run_without_s(&srv, &["-L", "t", "start-server"]);
    // Clean up first in case a server started anyway.
    let running = UnixStream::connect(dir.join("t.sock")).is_ok();
    if running {
        let _ = run_without_s(&srv, &["-L", "t", "kill-server"]);
    }

    assert!(!out.status.success(), "{out:?}");
    assert!(
        out.stderr.contains(&dir.display().to_string()),
        "error does not name the directory: {:?}",
        out.stderr
    );
    assert!(!running);
    assert!(!exists(&dir.join("t.sock")));
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

/// The pid of the most recent server started on `srv`'s socket, from its log.
fn logged_server_pid(srv: &TestServer) -> nix::unistd::Pid {
    let log = srv.log();
    let pid = log
        .lines()
        .rev()
        .find(|l| l.contains(grofe::server::START_MARKER))
        .and_then(|l| l.split("pid ").nth(1))
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no server pid in log:\n{log}"));
    nix::unistd::Pid::from_raw(pid)
}

/// The server process is gone and so is its socket.
fn assert_server_gone(srv: &TestServer, pid: nix::unistd::Pid) {
    wait_until(|| process_gone(pid), WAIT)
        .unwrap_or_else(|e| panic!("server {pid} still alive: {e}\nlog:\n{}", srv.log()));
    assert!(!exists(&srv.socket), "socket left behind");
}

/// A `new-session` that fails in the server, as in tmux 3.7b ("width
/// invalid"). A missing `-c` directory no longer fails: it falls back to
/// `$HOME`.
const FAILING_NEW_SESSION: &[&str] = &["new-session", "-d", "-x", "not-a-width"];

#[test]
fn a_failed_first_new_session_leaves_no_server() {
    let srv = TestServer::unstarted();
    let out = srv.run(FAILING_NEW_SESSION);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(srv.server_starts(), 1, "log:\n{}", srv.log());
    assert_server_gone(&srv, logged_server_pid(&srv));
}

#[test]
fn a_successful_first_new_session_keeps_the_server() {
    let srv = TestServer::unstarted();
    assert_reached_server(&srv.run(&["new-session", "-d", "sleep", "30"]));
    let pid = logged_server_pid(&srv);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!process_gone(pid));
    assert!(srv.is_running(), "log:\n{}", srv.log());
}

#[test]
fn explicit_start_server_stays_up_after_a_failed_new_session() {
    let srv = TestServer::start();
    let out = srv.run(FAILING_NEW_SESSION);
    assert!(!out.status.success(), "{out:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(srv.is_running(), "log:\n{}", srv.log());
    assert_eq!(srv.cmd(&["list-sessions"]).unwrap(), "");
}

#[test]
fn killing_the_last_session_of_an_auto_started_server_stops_it() {
    let srv = TestServer::unstarted();
    assert_reached_server(&srv.run(&["new-session", "-d", "-s", "only", "sleep", "30"]));
    let pid = logged_server_pid(&srv);
    srv.cmd(&["kill-session", "-t", "only"]).unwrap();
    assert_server_gone(&srv, pid);
}

/// An open client connection keeps an empty auto-started server up: it
/// exits only once that connection closes too.
#[test]
fn an_empty_auto_started_server_waits_for_open_connections() {
    use std::io::Write;

    let srv = TestServer::unstarted();
    let mut cmd = srv.command();
    cmd.args(["__server", "--exit-when-empty"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let server = cmd.spawn().expect("spawn server");
    wait_until(|| srv.is_running(), WAIT).expect("server comes up");

    let mut idle = UnixStream::connect(&srv.socket).expect("connect");
    let hello = grofe::protocol::encode_hello(&grofe::protocol::Hello::Control(srv.ctx())).unwrap();
    writeln!(idle, "{hello}").unwrap();

    let out = srv.run(FAILING_NEW_SESSION);
    assert!(!out.status.success(), "{out:?}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        srv.is_running(),
        "exited with a connection open; log:\n{}",
        srv.log()
    );

    drop(idle);
    let status = wait_child(server, WAIT).expect("server exits once the last connection closes");
    assert!(status.success(), "{status:?}");
    assert!(!exists(&srv.socket), "socket left behind");
}

#[test]
fn concurrent_auto_starts_survive_one_failing_new_session() {
    let srv = TestServer::unstarted();
    let (bad, good): (common::CmdOutput, Vec<common::CmdOutput>) = std::thread::scope(|s| {
        let bad = s.spawn(|| srv.run(FAILING_NEW_SESSION));
        let good: Vec<_> = (0..6)
            .map(|_| s.spawn(|| srv.run(&["new-session", "-d", "sleep", "30"])))
            .collect();
        (
            bad.join().unwrap(),
            good.into_iter().map(|h| h.join().unwrap()).collect(),
        )
    });
    assert!(!bad.status.success(), "{bad:?}");
    for out in &good {
        assert_reached_server(out);
    }
    assert!(srv.is_running(), "log:\n{}", srv.log());
    let sessions = srv.cmd(&["list-sessions"]).unwrap();
    assert_eq!(
        sessions.lines().count(),
        6,
        "{sessions}\nlog:\n{}",
        srv.log()
    );
}

/// An auto-started server whose starter never connects (it timed out, was
/// interrupted, or died after the ready signal) still exits on its own.
#[test]
fn an_auto_started_server_nobody_connects_to_exits() {
    let srv = TestServer::unstarted();
    let mut cmd = srv.command();
    cmd.args(["__server", "--exit-when-empty"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let server = cmd.spawn().expect("spawn server");
    // Watch for the socket file only: connecting would count as a client.
    wait_until(|| exists(&srv.socket), WAIT).expect("server binds");

    let status = wait_child(server, Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("idle server did not exit: {e}\nlog:\n{}", srv.log()));
    assert!(status.success(), "{status:?}");
    assert!(!exists(&srv.socket), "socket left behind");
}

/// tmux 3.7b's attach starts a server, which then has no sessions: every
/// spelling and target says so, with or without a terminal.
#[test]
fn attach_without_a_server_says_no_sessions() {
    let srv = TestServer::unstarted();
    for args in [
        &["attach"][..],
        &["attach", "-t", "nope"],
        &["attach-session", "-t", "nope:1"],
        &["attach", "-d"],
    ] {
        let out = srv.run(args);
        assert!(!out.status.success(), "{args:?}");
        assert_eq!(out.stderr, "no sessions\n", "{args:?}");
    }
    assert!(!srv.is_running());
    assert_eq!(srv.server_starts(), 0);
}

/// tmux 3.7b resolves the target before it opens the terminal: a bad
/// target is reported even without one.
#[test]
fn attach_checks_its_target_before_the_terminal() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "alpha"])
        .expect("new-session");
    let out = srv.run(&["attach", "-t", "nope"]);
    assert!(!out.status.success());
    assert_eq!(out.stderr, "can't find session: nope\n");
    let out = srv.run(&["attach", "-t", "alpha"]);
    assert!(!out.status.success());
    assert_eq!(out.stderr, "open terminal failed: not a terminal\n");
    assert_eq!(
        srv.cmd(&["list-clients"]).expect("list-clients"),
        "",
        "a check attaches nothing"
    );
}
