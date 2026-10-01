//! T2.1: the attach client in a real pty against a fake server.
//!
//! The server side of attach is T2.5a's, so each test plays both ends
//! itself: a `UnixListener` that speaks the attach handshake (one JSON
//! hello in, one JSON reply line out, then binary frames) and the outer
//! terminal (an expectrl pty whose output the test reads and whose input it
//! writes, including the probe's replies when a test wants them answered).
//!
//! Every client runs under `sh -c '...; stty -a'`, so after it exits the
//! pty's termios is printed: "restored" means `icanon` and `echo` are back
//! (expectrl starts the pty with echo off, so the script turns it on first).

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use expectrl::Session;
use expectrl::session::OsSession;
use grofe::protocol::{Caps, Frame, FrameCodec, Hello, Reply, decode_hello};
use grofe::types::PaneId;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The probe's last query, DA1: once it shows up the client is waiting for
/// replies.
const DA1_QUERY: &[u8] = b"\x1b[c";

/// Ghostty's answers to the whole probe, from the SYNTHESIZED fixture
/// tests/fixtures/probe/ghostty.txt (`#` comments, `\e` ESC, `\\` backslash).
fn ghostty_replies() -> Vec<u8> {
    let text = include_str!("fixtures/probe/ghostty.txt");
    let body: String = text.lines().filter(|l| !l.starts_with('#')).collect();
    body.replace("\\e", "\x1b")
        .replace("\\\\", "\\")
        .into_bytes()
}

const KITTY_PUSH: &[u8] = b"\x1b[>1u";
const KITTY_POP: &[u8] = b"\x1b[<u";
const MOK_ON: &[u8] = b"\x1b[>4;1m";
const MOK_OFF: &[u8] = b"\x1b[>4;0m";
const ALT_SCREEN_OFF: &[u8] = b"\x1b[?1049l";
const FOCUS_ON: &[u8] = b"\x1b[?1004h";
const FOCUS_OFF: &[u8] = b"\x1b[?1004l";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";

/// The teardown undoes what setup turned on.
fn assert_teardown(out: &[u8]) {
    for (seq, what) in [
        (ALT_SCREEN_OFF, "alternate screen left"),
        (FOCUS_OFF, "focus reporting off"),
        (PASTE_OFF, "bracketed paste off"),
    ] {
        assert!(contains(out, seq), "{what}");
    }
}

// ---------------------------------------------------------------- fake server

/// The attach client first checks its target on a control connection
/// (`__attach-check`), then attaches on a second one. A thread answers the
/// checks (`Ok`, or the configured refusal) and hands attach connections,
/// with the hello line still unread, to [`FakeServer::accept`].
struct FakeServer {
    _dir: TempDir,
    socket: PathBuf,
    attaches: mpsc::Receiver<(UnixStream, Vec<u8>)>,
    checks: Arc<Mutex<Vec<Vec<String>>>>,
    stop: Arc<AtomicBool>,
}

impl FakeServer {
    fn new() -> FakeServer {
        FakeServer::answering(None)
    }

    /// A server whose attach check fails with `err`.
    fn refusing(err: &str) -> FakeServer {
        FakeServer::answering(Some(err.to_string()))
    }

    fn answering(refusal: Option<String>) -> FakeServer {
        let dir = tempfile::Builder::new()
            .prefix("grofe-client")
            .tempdir()
            .expect("tempdir");
        let socket = dir.path().join("fake.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let (tx, attaches) = mpsc::channel();
        let checks = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, done) = (Arc::clone(&checks), Arc::clone(&stop));
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).expect("blocking stream");
                        match answer_check(&stream, refusal.as_deref()) {
                            Ok(argv) => seen.lock().expect("checks lock").push(argv),
                            Err(first) => {
                                if tx.send((stream, first)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
        });
        FakeServer {
            _dir: dir,
            socket,
            attaches,
            checks,
            stop,
        }
    }

    fn accept(&self) -> Conn {
        let (stream, first) = self
            .attaches
            .recv_timeout(TIMEOUT)
            .expect("client never connected to attach");
        Conn {
            stream,
            buf: BytesMut::from(&first[..]),
        }
    }

    /// The argv of every attach check so far.
    fn checks(&self) -> Vec<Vec<String>> {
        self.checks.lock().expect("checks lock").clone()
    }

    /// Whether an attach connection arrived within `wait`.
    fn attach_arrives(&self, wait: Duration) -> bool {
        self.attaches.recv_timeout(wait).is_ok()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Reads a new connection's first line: a control hello is the attach
/// check, answered here (`Ok(argv)`); any other line is handed back to go
/// with the stream.
fn answer_check(stream: &UnixStream, refusal: Option<&str>) -> Result<Vec<String>, Vec<u8>> {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let read_line = || {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while let Ok(1) = (&*stream).read(&mut byte) {
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        line
    };
    let first = read_line();
    let text = String::from_utf8_lossy(&first).into_owned();
    let Ok(Hello::Control(_)) = decode_hello(text.trim_end()) else {
        return Err(first);
    };
    let req: grofe::protocol::Request =
        serde_json::from_slice(&read_line()).expect("attach check request");
    let reply = match refusal {
        None => Reply::Ok {
            id: req.id,
            out: String::new(),
        },
        Some(err) => Reply::err(req.id, err),
    };
    let line = serde_json::to_string(&reply).expect("encode reply");
    let _ = (&*stream).write_all(format!("{line}\n").as_bytes());
    Ok(req.argv)
}

struct Conn {
    stream: UnixStream,
    buf: BytesMut,
}

impl Conn {
    /// Reads the client's JSON hello line.
    fn hello(&mut self) -> Hello {
        let line = self.read_line();
        decode_hello(&line).unwrap_or_else(|e| panic!("bad hello {line:?}: {e}"))
    }

    fn read_line(&mut self) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line = self.buf.split_to(i + 1);
                return String::from_utf8_lossy(&line[..i]).into_owned();
            }
            assert!(Instant::now() < deadline, "no hello line from the client");
            self.fill(deadline);
        }
    }

    fn reply(&mut self, reply: &Reply) {
        let line = serde_json::to_string(reply).expect("encode reply");
        self.stream
            .write_all(format!("{line}\n").as_bytes())
            .expect("write reply");
    }

    fn send(&mut self, frame: Frame) {
        let mut out = BytesMut::new();
        FrameCodec.encode(frame, &mut out).expect("encode frame");
        self.stream.write_all(&out).expect("write frame");
    }

    /// The next frame from the client, or `None` once it hung up.
    fn next_frame(&mut self) -> Option<Frame> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(frame) = FrameCodec.decode(&mut self.buf).expect("decode frame") {
                return Some(frame);
            }
            assert!(Instant::now() < deadline, "no frame from the client");
            if !self.fill(deadline) {
                return None;
            }
        }
    }

    /// Input frames until their bytes contain `needle`.
    fn input_until(&mut self, needle: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        while !contains(&got, needle) {
            match self.next_frame() {
                Some(Frame::Input(bytes)) => got.extend_from_slice(&bytes),
                Some(other) => panic!("expected input, got {other:?}"),
                None => panic!("client hung up; input so far {got:?}"),
            }
        }
        got
    }

    /// Reads more bytes into `buf`; false on EOF.
    fn fill(&mut self, deadline: Instant) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        // macOS refuses the timeout (EINVAL) once the peer has closed; the
        // read below then reports the EOF.
        let _ = self
            .stream
            .set_read_timeout(Some(left.max(Duration::from_millis(1))));
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                true
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => false,
            Err(e) => panic!("read from client: {e}"),
        }
    }
}

// ---------------------------------------------------------------- outer tty

struct Tty {
    session: OsSession,
    out: Vec<u8>,
}

impl Tty {
    /// Runs `script` under `sh -c` in a fresh pty, with `$0` the grofe
    /// binary and `$1` the fake server's socket. State and config dirs are
    /// beside the socket, in the fake server's tempdir, so a panic report
    /// never reaches the real state dir.
    fn spawn(script: &str, socket: &Path, env: &[(&str, &str)]) -> Tty {
        let dir = socket.parent().expect("socket dir");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .arg(env!("CARGO_BIN_EXE_grofe"))
            .arg(socket)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env_remove("COLORTERM")
            .env_remove("GROFE_TEST_CLIENT_PANIC")
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("TERM", "xterm-256color");
        for (k, v) in env {
            cmd.env(k, v);
        }
        Tty {
            session: Session::spawn(cmd).expect("spawn in pty"),
            out: Vec::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.session.write_all(bytes).expect("write to pty");
        self.session.flush().expect("flush pty");
    }

    /// Reads what the pty printed so far; false once it closed.
    fn pump(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.session.try_read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => self.out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false, // EIO once the child side is gone
            }
        }
    }

    fn wait_for(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let open = self.pump();
            if contains(&self.out, needle) {
                return;
            }
            assert!(
                open && Instant::now() < deadline,
                "timed out waiting for {:?}; pty output so far:\n{:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&self.out)
            );
            sleep(Duration::from_millis(10));
        }
    }

    /// Waits for `stty -a`'s output after the client exited and checks the
    /// tty came back in canonical mode with echo.
    fn assert_restored(&mut self) {
        self.wait_for(b"STTY-END");
        let text = String::from_utf8_lossy(&self.out).into_owned();
        let stty = &text[text.rfind("STTY-BEGIN").expect("stty output")..];
        let words: Vec<&str> = stty.split_whitespace().collect();
        for flag in ["icanon", "echo"] {
            assert!(
                words.contains(&flag),
                "tty not restored ({flag} missing):\n{stty}"
            );
        }
    }

    fn pid_line(&mut self) -> Pid {
        self.wait_for(b"CLIENTPID=");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            let text = String::from_utf8_lossy(&self.out).into_owned();
            if let Some(rest) = text.split("CLIENTPID=").nth(1)
                && let Some(end) = rest.find(|c: char| !c.is_ascii_digit())
                && end > 0
            {
                return Pid::from_raw(rest[..end].parse().expect("pid"));
            }
            assert!(Instant::now() < deadline, "no client pid line");
            sleep(Duration::from_millis(10));
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `sh` preamble and postamble: echo and canonical mode on (expectrl turns
/// echo off), the client, its status, then the tty's modes.
fn script(client: &str) -> String {
    format!(
        "stty echo icanon; {client}; echo \"STATUS=$?\"; echo STTY-BEGIN; stty -a; echo STTY-END"
    )
}

const ATTACH: &str = "\"$0\" -S \"$1\" attach -t work";

fn attach_hello(
    hello: Hello,
) -> (
    grofe::protocol::ClientCtx,
    u16,
    u16,
    String,
    Caps,
    Option<String>,
    bool,
) {
    match hello {
        Hello::Attach {
            ctx,
            cols,
            rows,
            term,
            caps,
            target,
            nest,
            ..
        } => (ctx, cols, rows, term, caps, target, nest),
        other => panic!("expected an attach hello, got {other:?}"),
    }
}

fn ok() -> Reply {
    Reply::Ok {
        id: grofe::protocol::HELLO_REPLY_ID,
        out: String::new(),
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn frames_flow_both_ways_after_a_probe_the_terminal_answers() {
    let srv = FakeServer::new();
    let mut tty = Tty::spawn(&script(ATTACH), &srv.socket, &[]);
    tty.wait_for(DA1_QUERY);
    tty.write(&ghostty_replies());

    let mut conn = srv.accept();
    let (ctx, cols, rows, term, caps, target, nest) = attach_hello(conn.hello());
    assert_eq!((cols, rows), (80, 24), "expectrl's default pty size");
    assert_eq!(term, "xterm-256color");
    assert_eq!(target.as_deref(), Some("work"));
    assert!(!nest);
    assert_eq!(ctx.pane, None);
    assert_eq!(
        caps,
        Caps {
            truecolor: true,
            styled_underline: true,
            sync: true,
            kitty_keys: true,
            kitty_graphics: true,
            cell_px: None,
        }
    );
    conn.reply(&ok());

    // The richest key mode this terminal offers: kitty flags 1.
    tty.wait_for(KITTY_PUSH);
    assert!(contains(&tty.out, FOCUS_ON), "focus reporting on");
    conn.send(Frame::Output(b"SERVER-SAYS-HI".to_vec()));
    tty.wait_for(b"SERVER-SAYS-HI");

    tty.write(b"typed-by-user");
    conn.input_until(b"typed-by-user");

    conn.send(Frame::Exit("detached".to_string()));
    tty.wait_for(b"[detached]");
    tty.wait_for(b"STATUS=0");
    assert!(contains(&tty.out, KITTY_POP), "kitty flags popped on exit");
    assert_teardown(&tty.out);
    tty.assert_restored();
}

#[test]
fn probe_times_out_cleanly_on_a_terminal_that_answers_nothing() {
    let srv = FakeServer::new();
    let mut tty = Tty::spawn(&script(ATTACH), &srv.socket, &[]);
    tty.wait_for(DA1_QUERY);
    let asked = Instant::now();
    // Typeahead during the probe is not a reply: it becomes the first input.
    tty.write(b"early");

    let mut conn = srv.accept();
    let (_, _, _, _, caps, _, _) = attach_hello(conn.hello());
    let waited = asked.elapsed();
    assert!(
        waited < Duration::from_secs(3),
        "probe took {waited:?}, its bound is 300 ms"
    );
    assert_eq!(caps, Caps::default(), "a silent terminal gets no extras");
    conn.reply(&ok());

    conn.input_until(b"early");
    // No kitty keys: fall back to modifyOtherKeys.
    tty.wait_for(MOK_ON);
    assert!(!contains(&tty.out, KITTY_PUSH));

    // Replies that arrive after the 300 ms are still filtered until DA1's,
    // not forwarded as keys; a lone ESC is not held back for ever.
    // Split across reads, within the client's wait for the rest of an ESC.
    tty.write(b"\x1b[?0u\x1b[?2026;2$");
    sleep(Duration::from_millis(2));
    tty.write(b"y\x1b[?62;22cafter");
    let got = conn.input_until(b"after");
    assert_eq!(got, b"after", "late replies are dropped");
    tty.write(b"\x1b");
    assert_eq!(conn.input_until(b"\x1b"), b"\x1b");

    // The server going away is an exit path too.
    drop(conn);
    tty.wait_for(b"STATUS=1");
    assert!(contains(&tty.out, MOK_OFF), "modifyOtherKeys reset on exit");
    tty.assert_restored();
}

#[test]
fn colorterm_gives_truecolor_without_any_reply() {
    let srv = FakeServer::new();
    let mut tty = Tty::spawn(&script(ATTACH), &srv.socket, &[("COLORTERM", "truecolor")]);
    tty.wait_for(DA1_QUERY);
    let mut conn = srv.accept();
    let (_, _, _, _, caps, _, _) = attach_hello(conn.hello());
    assert_eq!(
        caps,
        Caps {
            truecolor: true,
            ..Caps::default()
        }
    );
    conn.reply(&Reply::err(
        grofe::protocol::HELLO_REPLY_ID,
        "no such session: work",
    ));
    tty.wait_for(b"STATUS=1");
    tty.assert_restored();
}

#[test]
fn sigwinch_sends_a_resize_frame() {
    let srv = FakeServer::new();
    let mut tty = Tty::spawn(&script(ATTACH), &srv.socket, &[]);
    tty.wait_for(DA1_QUERY);
    tty.write(&ghostty_replies());
    let mut conn = srv.accept();
    conn.hello();
    conn.reply(&ok());
    tty.wait_for(KITTY_PUSH);

    tty.session
        .get_process_mut()
        .set_window_size(100, 40)
        .expect("resize pty");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match conn.next_frame() {
            Some(Frame::Resize(100, 40)) => break,
            Some(Frame::Resize(..) | Frame::Input(_)) => {}
            other => panic!("expected a resize frame, got {other:?}"),
        }
        assert!(Instant::now() < deadline, "no resize frame");
    }
    conn.send(Frame::Exit("detached".to_string()));
    tty.wait_for(b"STATUS=0");
}

#[test]
fn tty_is_restored_after_sigterm() {
    let srv = FakeServer::new();
    // In the background so the script can print the client's pid; with no
    // job control it stays in the foreground process group.
    let client = format!("{ATTACH} </dev/tty & echo \"CLIENTPID=$!\"; wait $!");
    let mut tty = Tty::spawn(&script(&client), &srv.socket, &[]);
    let pid = tty.pid_line();
    tty.wait_for(DA1_QUERY);
    tty.write(&ghostty_replies());
    let mut conn = srv.accept();
    conn.hello();
    conn.reply(&ok());
    tty.wait_for(KITTY_PUSH);

    // Our own grandchild, identified by the pid its parent script printed.
    kill(pid, Signal::SIGTERM).expect("SIGTERM the client");
    tty.wait_for(b"STATUS=143");
    assert!(
        contains(&tty.out, KITTY_POP),
        "kitty flags popped on SIGTERM"
    );
    assert_teardown(&tty.out);
    tty.assert_restored();
    assert_eq!(conn.next_frame(), None, "client closed its connection");
}

#[test]
fn a_panic_in_another_thread_restores_the_tty_and_exits() {
    let srv = FakeServer::new();
    let mut tty = Tty::spawn(
        &script(ATTACH),
        &srv.socket,
        &[("GROFE_TEST_CLIENT_PANIC", "stdin")],
    );
    tty.wait_for(DA1_QUERY);
    tty.write(&ghostty_replies());
    let mut conn = srv.accept();
    conn.hello();
    conn.reply(&ok());
    tty.wait_for(KITTY_PUSH);

    // The stdin thread panics on its first read.
    let real_before = real_state_files();
    tty.write(b"x");
    tty.wait_for(b"STATUS=101");
    assert!(contains(&tty.out, KITTY_POP), "kitty flags popped on panic");
    tty.assert_restored();
    // The panic report lands in the test's own state dir, never the real one.
    let state = srv.socket.parent().expect("socket dir").join("state/grofe");
    let reports: Vec<_> = std::fs::read_dir(&state)
        .map(|d| d.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert_eq!(reports.len(), 1, "{reports:?} in {}", state.display());
    assert_eq!(
        real_state_files(),
        real_before,
        "the real state dir changed"
    );
}

/// File names in the real state dir (from this process's own environment),
/// read only, for guards that nothing a test runs writes there.
fn real_state_files() -> std::collections::BTreeSet<String> {
    let dir = grofe::paths::state_dir(&grofe::paths::Env::from_process());
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn attach_from_inside_a_pane_sends_the_pane_and_prints_a_refusal() {
    let srv = FakeServer::new();
    let socket = srv.socket.to_string_lossy().into_owned();
    let env = [("GROFE", socket.as_str()), ("GROFE_PANE", "%3")];
    let mut tty = Tty::spawn(&script(ATTACH), &srv.socket, &env);
    tty.wait_for(DA1_QUERY);
    let mut conn = srv.accept();
    let (ctx, _, _, _, _, _, nest) = attach_hello(conn.hello());
    // The client refuses nothing itself: the server knows which session
    // holds %3 and decides (spec §4.6).
    assert_eq!(ctx.pane, Some(PaneId(3)));
    assert!(!nest);
    conn.reply(&Reply::err(
        grofe::protocol::HELLO_REPLY_ID,
        "sessions should be nested with care, use --nest to force",
    ));
    tty.wait_for(b"use --nest to force");
    tty.wait_for(b"STATUS=1");
    assert!(
        !contains(&tty.out, b"\x1b[?1049h"),
        "a refused attach never switches screens"
    );
    tty.assert_restored();
}

/// The target is checked before the terminal is touched: a refusal is
/// printed without any probe query (the kitty graphics APC among them) and
/// no attach connection follows.
#[test]
fn a_refused_target_is_reported_before_any_probe() {
    let srv = FakeServer::refusing("can't find session: nope");
    let attach = "\"$0\" -S \"$1\" attach -t nope --nest";
    let mut tty = Tty::spawn(&script(attach), &srv.socket, &[]);
    tty.wait_for(b"can't find session: nope");
    tty.wait_for(b"STATUS=1");
    assert!(!contains(&tty.out, b"\x1b[>0q"), "XTVERSION was sent");
    assert!(
        !contains(&tty.out, b"\x1b_G"),
        "the kitty graphics query was sent"
    );
    assert!(!srv.attach_arrives(Duration::from_millis(200)));
    // The check carries the client's terminal first (tmux's nesting rule
    // reads it), then attach-session's arguments.
    let checks = srv.checks();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0][0], "__attach-check");
    assert!(checks[0][1].starts_with("/dev/"), "{checks:?}");
    assert_eq!(checks[0][2..], ["-t", "nope", "--nest"]);
    tty.assert_restored();
}

#[test]
fn nest_flag_reaches_the_hello() {
    let srv = FakeServer::new();
    let attach = "\"$0\" -S \"$1\" attach -dt work --nest";
    let mut tty = Tty::spawn(&script(attach), &srv.socket, &[]);
    tty.wait_for(DA1_QUERY);
    let mut conn = srv.accept();
    match conn.hello() {
        Hello::Attach {
            target,
            nest,
            detach_others,
            ..
        } => {
            assert_eq!(target.as_deref(), Some("work"));
            assert!(nest);
            assert!(detach_others);
        }
        other => panic!("expected an attach hello, got {other:?}"),
    }
    // Closing without a reply line is a failed handshake, not a hang.
    drop(conn);
    tty.wait_for(b"STATUS=1");
    tty.assert_restored();
}

#[test]
fn attach_without_a_terminal_fails_before_the_hello() {
    let srv = FakeServer::new();
    let dir = srv.socket.parent().expect("socket dir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_grofe"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .arg("-S")
        .arg(&srv.socket)
        .arg("attach")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn grofe");
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "attach without a tty hung");
        sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert!(!status.success());
    assert!(stderr.contains("not a terminal"), "stderr: {stderr}");
}

#[test]
fn signals_are_handled_while_the_server_stops_reading() {
    let srv = FakeServer::new();
    let client = format!("{ATTACH} </dev/tty & echo \"CLIENTPID=$!\"; wait $!");
    let mut tty = Tty::spawn(&script(&client), &srv.socket, &[]);
    let pid = tty.pid_line();
    tty.wait_for(DA1_QUERY);
    tty.write(&ghostty_replies());
    let mut conn = srv.accept();
    conn.hello();
    conn.reply(&ok());
    tty.wait_for(KITTY_PUSH);

    // The server never reads again. Flood the client's input until nothing
    // more fits anywhere (socket buffers, the client, the pty).
    let flood = tty
        .session
        .get_process()
        .get_raw_handle()
        .expect("pty handle");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let written = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let (stop, written) = (stop.clone(), written.clone());
        std::thread::spawn(move || flood_until(flood, &stop, &written));
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut last = (0, Instant::now());
    loop {
        tty.pump();
        let now = written.load(std::sync::atomic::Ordering::SeqCst);
        if now != last.0 {
            last = (now, Instant::now());
        } else if now > 0 && last.1.elapsed() > Duration::from_millis(500) {
            break; // stalled
        }
        assert!(
            Instant::now() < deadline,
            "flood never stalled ({now} bytes)"
        );
        sleep(Duration::from_millis(20));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);

    tty.session
        .get_process_mut()
        .set_window_size(100, 40)
        .expect("resize pty");
    sleep(Duration::from_millis(100));
    kill(pid, Signal::SIGTERM).expect("SIGTERM the client");
    tty.wait_for(b"STATUS=143");
    assert_teardown(&tty.out);
    tty.assert_restored();
    drop(conn);
}

/// Writes to the pty master until `stop`, never blocking for long: expectrl
/// flips the shared description's O_NONBLOCK, so both EAGAIN and a
/// POLLOUT timeout mean "full".
fn flood_until(
    pty: std::fs::File,
    stop: &std::sync::atomic::AtomicBool,
    written: &std::sync::atomic::AtomicUsize,
) {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::AsFd;
    let chunk = [b'x'; 64];
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let mut fds = [PollFd::new(pty.as_fd(), PollFlags::POLLOUT)];
        if !matches!(poll(&mut fds, PollTimeout::from(50u16)), Ok(n) if n > 0) {
            continue;
        }
        match nix::unistd::write(&pty, &chunk) {
            Ok(n) => {
                written.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
            }
            Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {}
            Err(_) => return,
        }
    }
}

#[test]
fn stdin_eof_sends_detach() {
    // stdin is a second pty the test can hang up; output stays on the
    // expectrl pty.
    let pty = nix::pty::openpty(None, None).expect("openpty");
    let slave = nix::unistd::ttyname(&pty.slave).expect("ttyname");
    let srv = FakeServer::new();
    let client = format!("{ATTACH} <{}", slave.display());
    let mut tty = Tty::spawn(&script(&client), &srv.socket, &[]);
    tty.wait_for(DA1_QUERY);
    let mut conn = srv.accept();
    conn.hello(); // the probe timed out: nobody answers on the stdin pty
    conn.reply(&ok());
    tty.wait_for(MOK_ON);

    drop(pty); // closes master and slave: the client's stdin sees EOF
    loop {
        match conn.next_frame() {
            Some(Frame::Detach) => break,
            Some(Frame::Resize(..)) => {}
            other => panic!("expected detach, got {other:?}"),
        }
    }
    conn.send(Frame::Exit("detached".to_string()));
    tty.wait_for(b"STATUS=0");
    assert_teardown(&tty.out);
}

#[test]
fn resize_storms_during_the_handshake_never_lose_typeahead() {
    let srv = FakeServer::new();
    let mut tty = Tty::spawn(&script(ATTACH), &srv.socket, &[]);
    tty.wait_for(DA1_QUERY);
    tty.write(b"early"); // the probe times out: this is typeahead
    let mut conn = srv.accept();
    conn.hello();
    // More SIGWINCHs than the client's frame queue holds, before the
    // writer exists; paced so the kernel does not merge them.
    for i in 0..100u16 {
        tty.session
            .get_process_mut()
            .set_window_size(80 + i % 2, 24)
            .expect("resize pty");
        sleep(Duration::from_millis(2));
    }
    conn.reply(&ok());
    loop {
        match conn.next_frame() {
            Some(Frame::Resize(..)) => {}
            Some(Frame::Input(bytes)) => {
                assert_eq!(bytes, b"early", "typeahead is the first input");
                break;
            }
            other => panic!("expected the typeahead, got {other:?}"),
        }
    }
    conn.send(Frame::Exit("detached".to_string()));
    tty.wait_for(b"STATUS=0");
}
