//! Hook mode (agents spec §3.2): `agent report --hook` exits 0 and returns
//! at once whatever is, or is not, at the socket. A Claude Code hook runs on
//! every tool call, and a `PreToolUse` hook that exits 2 blocks the tool.

mod common;

use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::TestServer;

/// What the spec allows a hook to cost.
const BUDGET: Duration = Duration::from_millis(200);

/// Runs `grofe -S <socket> <args>` from inside pane `%0`, feeding `stdin`.
/// The best of three runs is timed, so one slow process start on a loaded
/// machine is not taken for a hook that waits; every run must still exit 0
/// and none may hang.
fn hook(srv: &TestServer, args: &[&str], stdin: &[u8]) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let (status, took) = once(srv, args, stdin);
        assert!(status.success(), "{args:?} exited {status}");
        assert!(took < Duration::from_secs(2), "{args:?} hung: {took:?}");
        best = best.min(took);
    }
    best
}

fn once(srv: &TestServer, args: &[&str], stdin: &[u8]) -> (ExitStatus, Duration) {
    let mut cmd = srv.command();
    cmd.args(args)
        .env("GROFE_PANE", "%0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn hook");
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(stdin);
    }
    let status = child.wait().expect("wait hook");
    (status, start.elapsed())
}

const REPORT: &[&str] = &["agent", "report", "working", "--hook", "claude"];
const PAYLOAD: &[u8] = br#"{"session_id":"s1","tool_name":"Bash","tool_input":{"command":"ls"}}"#;

#[test]
fn hook_returns_at_once_with_no_server() {
    let srv = TestServer::unstarted();
    assert!(!srv.socket.exists());
    let took = hook(&srv, REPORT, PAYLOAD);
    assert!(took < BUDGET, "{took:?}");
    assert!(!srv.is_running(), "a hook never starts a server");
}

#[test]
fn hook_returns_at_once_on_a_dead_socket_file() {
    let srv = TestServer::unstarted();
    drop(UnixListener::bind(&srv.socket).expect("bind"));
    let meta = std::fs::symlink_metadata(&srv.socket).expect("socket file stays");
    assert!(meta.file_type().is_socket());
    let took = hook(&srv, REPORT, PAYLOAD);
    assert!(took < BUDGET, "{took:?}");
}

/// The listener never accepts and never replies; the connection sits in
/// its backlog. The hook still wrote its request and left.
#[test]
fn hook_returns_at_once_when_the_server_never_replies() {
    let srv = TestServer::unstarted();
    let listener = UnixListener::bind(&srv.socket).expect("bind");
    let took = hook(&srv, REPORT, PAYLOAD);
    assert!(took < BUDGET, "{took:?}");
    listener.set_nonblocking(true).expect("nonblocking");
    let (mut conn, _) = listener.accept().expect("the hook connected");
    // The hook has exited, so the read ends at its EOF.
    conn.set_nonblocking(false).expect("blocking");
    let mut got = Vec::new();
    conn.read_to_end(&mut got)
        .expect("the hook wrote its request");
    let got = String::from_utf8_lossy(&got);
    assert!(got.contains(r#""report","working""#), "{got}");
    drop(conn);
    drop(listener);
}

#[test]
fn hook_returns_at_once_with_garbage_stdin_and_unknown_flags() {
    let srv = TestServer::unstarted();
    let listener = UnixListener::bind(&srv.socket).expect("bind");
    let garbage: &[u8] = b"\x00{not json\xff\x1b[31m";
    let big = vec![b'x'; 2 << 20];
    for (args, stdin) in [
        (REPORT, garbage),
        (REPORT, &big[..]),
        (
            &["agent", "report", "done", "--nope", "--hook", "claude"][..],
            garbage,
        ),
        (&["agent", "report", "--bogus", "--hook"], garbage),
        (
            &[
                "agent",
                "report",
                "blocked",
                "--blocked",
                "$(touch x)",
                "--hook",
                "claude",
            ],
            garbage,
        ),
    ] {
        let took = hook(&srv, args, stdin);
        assert!(took < BUDGET, "{args:?}: {took:?}");
    }
    drop(listener);
}
