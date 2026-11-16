//! `grofe agent report ... --hook <kind>`: the fire-and-forget report a
//! coding agent's hook runs on every tool call (spec §3.2). It exits 0 on
//! everything: a Claude Code `PreToolUse` hook that exits 2 blocks the tool.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::protocol::{ClientCtx, Dialect, Hello, Request, encode_hello};

/// How long the hook waits to connect before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(30);

/// Most bytes of the hook payload read from stdin.
const MAX_PAYLOAD: u64 = 1 << 20;

/// How long the hook waits for its payload, in all, before reporting the
/// state without it.
const STDIN_DEADLINE: Duration = Duration::from_millis(200);

/// Most characters of a tool input summary.
const SUMMARY_LEN: usize = 60;

/// Runs a hook-mode `agent report`. `args` is the whole argv from `agent`
/// on, e.g. `agent report working --hook claude`. Always exits 0.
pub fn run(globals_socket: &Path, args: &[String]) -> ExitCode {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT").ok();
    let pane = std::env::var("GROFE_PANE")
        .ok()
        .filter(|_| owns_pane(entrypoint.as_deref()));
    hook(globals_socket, args, pane.as_deref(), std::io::stdin(), ts);
    ExitCode::SUCCESS
}

/// Whether a Claude Code that started with `entrypoint` is its pane's
/// agent. A `claude -p` helper run from inside the pane (`sdk-cli`, as an
/// SDK host starts it) inherits `GROFE_PANE`; its reports would take over
/// the pane's record and resume with the helper's session.
fn owns_pane(entrypoint: Option<&str>) -> bool {
    !entrypoint.is_some_and(|e| e.starts_with("sdk"))
}

fn hook<R: Read + Send + 'static>(
    socket: &Path,
    args: &[String],
    pane: Option<&str>,
    stdin: R,
    ts: u64,
) {
    let Some(pane) = pane.filter(|p| !p.is_empty()) else {
        return;
    };
    let Some(state) = args.get(2).filter(|s| !s.starts_with('-')) else {
        return;
    };
    let Some(kind) = args
        .iter()
        .position(|a| a == "--hook")
        .and_then(|i| args.get(i + 1))
    else {
        return;
    };
    let payload = read_payload(stdin);
    let mut argv = hook_argv(state, kind, &payload, pane, ts);
    // Only a value the server accepts, so a bad one cannot fail the report.
    if let Some(b) = args
        .iter()
        .position(|a| a == "--blocked")
        .and_then(|i| args.get(i + 1))
        .filter(|b| matches!(b.as_str(), "permission" | "question" | "auth"))
    {
        argv.extend(["--blocked".to_string(), b.clone()]);
    }
    send(socket, &argv);
}

/// Reads at most [`MAX_PAYLOAD`] bytes of `stdin` on a helper thread, waiting
/// [`STDIN_DEADLINE`] in all. A stdin that never closes yields no payload and
/// leaves the thread behind; the process exits without it.
fn read_payload<R: Read + Send + 'static>(stdin: R) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut payload = Vec::new();
        let _ = stdin.take(MAX_PAYLOAD).read_to_end(&mut payload);
        let _ = tx.send(payload);
    });
    rx.recv_timeout(STDIN_DEADLINE).unwrap_or_default()
}

/// Connects (30 ms at most), writes the hello and request, and does not
/// wait for a reply. Any failure is dropped.
fn send(socket: &Path, argv: &[String]) {
    let Some(stream) = connect(socket) else {
        return;
    };
    let ctx = ClientCtx {
        cwd: std::env::current_dir().unwrap_or_else(|_| "/".into()),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::default(),
        request: Default::default(),
    };
    let Ok(hello) = encode_hello(&Hello::Control(ctx)) else {
        return;
    };
    let Ok(req) = serde_json::to_string(&Request {
        id: 1,
        argv: argv.to_vec(),
        stdin: None,
        umask: None,
    }) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    let _ = (&stream).write_all(format!("{hello}\n{req}\n").as_bytes());
}

/// A connect bounded by [`CONNECT_TIMEOUT`]: a local socket connect either
/// completes at once or fails, so a blocked accept queue is the only wait,
/// and it is cut by doing the connect on a helper thread.
fn connect(socket: &Path) -> Option<UnixStream> {
    let (tx, rx) = std::sync::mpsc::channel();
    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(socket).ok());
    });
    rx.recv_timeout(CONNECT_TIMEOUT).ok().flatten()
}

/// The `agent report` argv for a hook payload. An unreadable payload still
/// reports the state, without a session id or message.
pub fn hook_argv(state: &str, kind: &str, payload: &[u8], pane: &str, ts: u64) -> Vec<String> {
    let mut argv: Vec<String> = [
        "agent",
        "report",
        state,
        "-t",
        pane,
        "--kind",
        kind,
        "--ts",
        &ts.to_string(),
    ]
    .map(String::from)
    .to_vec();
    let Ok(v) = serde_json::from_slice::<Value>(payload) else {
        return argv;
    };
    if let Some(id) = v.get("session_id").and_then(Value::as_str) {
        argv.extend(["--session-id".to_string(), id.to_string()]);
    }
    if let Some(msg) = message(&v) {
        argv.extend(["--msg".to_string(), msg]);
    }
    argv
}

/// The first string of `message`, `reason`, `title`, else the tool name and
/// a short summary of its input.
fn message(v: &Value) -> Option<String> {
    for key in ["message", "reason", "title"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    let tool = v.get("tool_name").and_then(Value::as_str)?;
    let input = v.get("tool_input");
    let summary = ["command", "file_path", "pattern", "url"]
        .iter()
        .find_map(|k| input?.get(k)?.as_str());
    Some(match summary {
        Some(s) => format!("{tool} {}", s.chars().take(SUMMARY_LEN).collect::<String>()),
        None => tool.to_string(),
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn an_sdk_launched_claude_does_not_own_its_pane() {
        assert!(owns_pane(None));
        assert!(owns_pane(Some("cli")));
        assert!(owns_pane(Some("claude-vscode")));
        assert!(!owns_pane(Some("sdk-cli")));
        assert!(!owns_pane(Some("sdk-ts")));
    }

    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hook_argv_with_payload() {
        let p = br#"{"session_id":"s1","tool_name":"Bash","tool_input":{"command":"ls -la"}}"#;
        assert_eq!(
            hook_argv("working", "claude", p, "%4", 7),
            strs(&[
                "agent",
                "report",
                "working",
                "-t",
                "%4",
                "--kind",
                "claude",
                "--ts",
                "7",
                "--session-id",
                "s1",
                "--msg",
                "Bash ls -la"
            ])
        );
    }

    #[test]
    fn hook_argv_garbage_has_no_msg() {
        assert_eq!(
            hook_argv("done", "claude", b"garbage", "%4", 7),
            strs(&[
                "agent", "report", "done", "-t", "%4", "--kind", "claude", "--ts", "7"
            ])
        );
    }

    #[test]
    fn msg_prefers_message_field() {
        let p = br#"{"message":"m","reason":"r","title":"t","tool_name":"Bash"}"#;
        assert_eq!(
            hook_argv("blocked", "claude", p, "%1", 1).last().unwrap(),
            "m"
        );
        let p = br#"{"reason":"r","title":"t"}"#;
        assert_eq!(
            hook_argv("blocked", "claude", p, "%1", 1).last().unwrap(),
            "r"
        );
    }

    #[test]
    fn tool_input_cut_60() {
        let cmd = "x".repeat(100);
        let p = format!(r#"{{"tool_name":"Bash","tool_input":{{"command":"{cmd}"}}}}"#);
        let argv = hook_argv("working", "claude", p.as_bytes(), "%1", 1);
        assert_eq!(argv.last().unwrap(), &format!("Bash {}", "x".repeat(60)));
    }

    fn no_server() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grofe-hook-none-{}.sock", std::process::id()))
    }

    #[test]
    fn hook_exit_0_no_server() {
        let args = strs(&["agent", "report", "done", "--hook", "claude"]);
        hook(&no_server(), &args, Some("%1"), &b"{}"[..], 1);
    }

    #[test]
    fn hook_exit_0_unknown_flag() {
        let args = strs(&["agent", "report", "--bogus", "--hook"]);
        hook(&no_server(), &args, Some("%1"), &b""[..], 1);
        let args = strs(&["agent", "report", "done", "--nope", "--hook", "claude"]);
        hook(&no_server(), &args, Some("%1"), &b""[..], 1);
        assert_eq!(
            format!("{:?}", run(&no_server(), &args)),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }

    #[test]
    fn hook_no_grofe_pane_exit_0() {
        let dir = tempfile_dir("nopane");
        let sock = dir.join("s.sock");
        let l = UnixListener::bind(&sock).unwrap();
        l.set_nonblocking(true).unwrap();
        let args = strs(&["agent", "report", "done", "--hook", "claude"]);
        hook(&sock, &args, None, &b""[..], 1);
        assert!(l.accept().is_err(), "no connection without a pane");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hook_never_waits() {
        let dir = tempfile_dir("nowait");
        let sock = dir.join("s.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let args = strs(&["agent", "report", "done", "--hook", "claude"]);
        let t = Instant::now();
        hook(&sock, &args, Some("%1"), &b"{}"[..], 1);
        assert!(t.elapsed() < Duration::from_millis(100));
        let (mut s, _) = l.accept().unwrap();
        let mut got = String::new();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
        assert!(got.contains(r#""agent""#) && got.contains("--ts"), "{got}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The hooks `agent setup` writes pass `--blocked permission` (spec §8);
    /// hook mode forwards a valid one and drops anything else.
    #[test]
    fn hook_forwards_blocked_kind() {
        let dir = tempfile_dir("blocked");
        let sock = dir.join("s.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let read = |l: &UnixListener| {
            let (mut s, _) = l.accept().unwrap();
            let mut got = String::new();
            s.read_to_string(&mut got).unwrap();
            got
        };
        let args = strs(&[
            "agent",
            "report",
            "blocked",
            "--blocked",
            "permission",
            "--hook",
            "claude",
        ]);
        hook(&sock, &args, Some("%1"), &b"{}"[..], 1);
        let got = read(&l);
        assert!(got.contains(r#""--blocked","permission""#), "{got}");
        let mut args = args;
        args[4] = "$(touch x)".into();
        hook(&sock, &args, Some("%1"), &b"{}"[..], 1);
        let got = read(&l);
        assert!(
            !got.contains("--blocked") && !got.contains("touch"),
            "{got}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("grofe-hook-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
