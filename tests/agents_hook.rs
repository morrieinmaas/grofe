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

/// Spec §2.7 end to end: hook stdin's message and `agent report --msg` land
/// sanitised, a hook kind that is no kind name and a session id with shell
/// syntax are dropped, and a status format showing the message runs nothing.
#[test]
fn hook_and_cli_text_reach_the_server_sanitised() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "100", "-y", "30"])
        .expect("new-session");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id")
        .trim()
        .to_string();
    // Short marker path: agent text is capped at 200 chars, and a long temp
    // path would cut the probes off.
    let short = std::path::PathBuf::from(format!(
        "/tmp/g{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    std::fs::create_dir_all(&short).expect("short dir");
    let x = short.join("X");
    let evil = format!(
        "A\x1b]2;PWN\x07B\u{85}C#(touch {})D##[range=user|x]E$(touch {})",
        x.display(),
        x.display()
    );
    let want = format!(
        "A]2;PWNBC_(touch {})D####[range=user|x]E$(touch {})",
        x.display(),
        x.display()
    );
    let payload = serde_json::json!({"message": evil, "session_id": "s;touch X"}).to_string();
    let mut cmd = srv.command();
    cmd.args(["agent", "report", "blocked", "--hook", "bad kind!"])
        .env("GROFE_PANE", &pane)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("payload");
    assert!(child.wait().expect("wait").success());
    let list = || {
        srv.cmd(&["agent", "list", "-F", "#{agent_kind}|#{agent_msg}"])
            .unwrap_or_default()
    };
    common::wait_until(|| list().contains("PWN"), Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("{e}: {:?}", list()));
    assert_eq!(list().trim_end(), format!("|{want}"));
    let resume = srv
        .cmd(&["display-message", "-p", "-t", &pane, "#{pane_resume}"])
        .unwrap_or_default();
    assert!(!resume.contains("touch"), "{resume:?}");

    srv.cmd(&[
        "agent", "report", "working", "-t", &pane, "--kind", "claude", "--msg", &evil,
    ])
    .expect("report --msg");
    assert_eq!(list().trim_end(), format!("claude|{want}"));

    srv.cmd(&["set-option", "-g", "status-right", "#{agent_msg}"])
        .expect("status-right");
    let shown = srv
        .cmd(&["display-message", "-p", "-t", &pane, "#{agent_msg}"])
        .expect("display");
    assert_eq!(shown.trim_end(), want);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!x.exists(), "agent text ran a command");
    let _ = std::fs::remove_dir_all(&short);
}

#[test]
fn hook_does_not_wait_for_a_stdin_that_never_closes() {
    let srv = TestServer::unstarted();
    let mut cmd = srv.command();
    cmd.args(REPORT)
        .env("GROFE_PANE", "%0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn hook");
    // The pipe stays open and silent until the hook has exited.
    let _held = child.stdin.take();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait hook") {
            break status;
        }
        if start.elapsed() > Duration::from_secs(1) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("hook still waiting on stdin after {:?}", start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "exited {status}");
}
