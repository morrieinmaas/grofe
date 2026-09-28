//! T2.5a: clients and frames. `grofe attach` in a pty against a real server:
//! the first frame shows the pane, input reaches it, detach and reattach
//! resize the window to the new client (spec D6), two clients of different
//! sizes letterbox and crop with a debounced size owner, and a client that
//! stops reading never delays another one.
//!
//! Panes run a fixed marker command instead of a real shell prompt (M2
//! addendum): `PROMPT` prints `grofe-prompt$ ` once, then for every line it
//! reads prints `got-<line>` at row 20, column 60, which is also where it
//! leaves the cursor.

#[path = "../common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use grofe::protocol::{Caps, ClientCtx, Frame, FrameCodec, Hello, Reply, Request, encode_hello};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

use crate::harness::Harness;

const PROMPT: &str = "stty -echo; printf 'grofe-prompt$ '; \
                      while read l; do printf '\\033[20;60Hgot-%s' \"$l\"; done";

fn attach(srv: &TestServer, target: &str) -> Command {
    let mut cmd = srv.command();
    cmd.args(["attach", "-t", target])
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    cmd
}

fn start(size: (u16, u16)) -> TestServer {
    let srv = TestServer::start();
    let (x, y) = (size.0.to_string(), size.1.to_string());
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        &x,
        "-y",
        &y,
        PROMPT,
    ])
    .expect("new-session");
    srv
}

fn pane_size(srv: &TestServer) -> String {
    srv.cmd(&[
        "display-message",
        "-p",
        "-t",
        "main",
        "#{pane_width}x#{pane_height}",
    ])
    .unwrap_or_default()
    .trim()
    .to_string()
}

fn wait_pane_size(srv: &TestServer, want: &str) {
    if let Err(e) = wait_until(|| pane_size(srv) == want, Duration::from_secs(10)) {
        panic!("pane never became {want} (is {}): {e}", pane_size(srv));
    }
}

#[test]
fn attach_shows_the_prompt_and_input_reaches_the_pane() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("grofe-prompt$ ");
    // The status line: default status-left, then the window list.
    a.wait_for("[main] 1:");
    let clients = srv.cmd(&["list-clients"]).expect("list-clients");
    assert!(
        clients.contains(": main [80x24 xterm-256color]"),
        "{clients}"
    );
    let widths = srv
        .cmd(&[
            "list-clients",
            "-F",
            "#{client_width}x#{client_height} #{session_name}",
        ])
        .expect("list-clients -F");
    assert_eq!(widths, "80x24 main\n");

    a.send("hello\r");
    a.wait_for("got-hello");
    // The cursor follows the pane's: row 20, just past `got-hello`.
    assert_eq!(a.cursor(), (68, 19));

    srv.cmd(&["detach-client"]).expect("detach-client");
    a.wait_for("[detached (from session main)]");
    a.wait_exit();
    assert_eq!(srv.cmd(&["list-clients"]).expect("list-clients"), "");
}

#[test]
fn reattach_resizes_the_window_to_the_new_client() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "80x23");
    srv.cmd(&["detach-client"]).expect("detach");
    a.wait_exit();

    // Smaller: the window shrinks to the client, less the status row.
    let mut small = Harness::spawn(attach(&srv, "main"), 40, 12);
    small.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "40x11");
    srv.cmd(&["detach-client"]).expect("detach");
    small.wait_exit();

    // Larger: it grows again.
    let mut large = Harness::spawn(attach(&srv, "main"), 120, 40);
    large.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "120x39");
    large.send("big\r");
    large.wait_for("got-big");
    srv.cmd(&["detach-client"]).expect("detach");
    large.wait_exit();
}

/// Spec D6 with two clients (M2 addendum): the latest active client owns
/// the window size, switching owner is debounced by 500 ms, a larger client
/// letterboxes and a smaller one crops with the viewport on the cursor.
#[test]
fn two_clients_letterbox_and_crop_with_a_debounced_owner() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "80x23");
    assert!(!a.text().contains('·'));

    // B attaches (activity): it is the latest, the window follows it and
    // A letterboxes.
    let mut b = Harness::spawn(attach(&srv, "main"), 40, 12);
    b.wait_for("grofe-prompt$ ");
    wait_pane_size(&srv, "40x11");
    a.wait_for("·");

    // A types: after the debounce the window is A's size again.
    let typed = Instant::now();
    a.send("a\r");
    wait_pane_size(&srv, "80x23");
    let took = typed.elapsed();
    assert!(
        took >= Duration::from_millis(450),
        "switched after {took:?}"
    );
    a.wait_until("no letterbox", Duration::from_secs(5), |t| !t.contains('·'));

    // The pane draws far from the top-left: B crops, with its viewport
    // moved just enough to show the cursor at its bottom-right.
    a.send("x\r");
    a.wait_for("got-x");
    b.wait_for("got-x");
    let screen = b.text();
    assert!(!screen.contains("grofe-prompt"), "not cropped:\n{screen}");
    // Window cursor (64, 19), 40x11 visible: viewport (25, 9).
    assert_eq!(b.cursor(), (39, 10), "screen:\n{screen}");
    let row = screen.lines().nth(10).unwrap_or_default();
    assert!(row.trim_end().ends_with("got-x"), "row 10 is {row:?}");
}

/// A client that stops reading (a raw socket with a tiny receive buffer,
/// never read) while a pane floods must not delay a second client or the
/// actor (M2 addendum).
///
/// Requests go over one control connection, so what is timed is the actor
/// and the second client's frame path, not spawning a CLI (a fork and exec
/// of the debug binary alone can take seconds on a loaded machine). The
/// timings are compared with a baseline taken the same way, under the same
/// flood, before the stuck client attaches. Each timing is the best of
/// three status updates, and the median of five `list-sessions`. A delay
/// the stuck client causes lasts as long as it stays stuck, so it slows
/// every sample. A scheduling hiccup on a loaded machine (seen at 2 to 4 s
/// in one debug run in eight) slows one.
#[test]
fn a_client_that_stops_reading_does_not_delay_another() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "exec seq 1 1000000000",
    ])
    .expect("new-session");
    let mut b = Harness::spawn(attach(&srv, "main"), 80, 24);
    b.wait_for("[main]");
    let mut control = Control::connect(&srv);
    let (base_status, base_list) = latencies(&mut control, &mut b, "B");

    // A 1 KiB receive buffer: on macOS, below the send low-water mark, the
    // server cannot write to it at all, the most stuck a client can be.
    let stuck = raw_attach(&srv, (300, 100), Some(1024), &[]);
    // Let the flood fill the stuck client's socket. `b` keeps reading
    // meanwhile: an unread pty would make it a lagging client too.
    let settle = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < settle {
        b.text();
        sleep(Duration::from_millis(20));
    }
    let (status, list) = latencies(&mut control, &mut b, "S");

    let status_bound = (base_status * 5).max(Duration::from_secs(1));
    assert!(
        status < status_bound,
        "status update took {status:?} with a stuck client, {base_status:?} without"
    );
    let list_bound = (base_list * 5).max(Duration::from_millis(200));
    assert!(
        list < list_bound,
        "list-sessions took {list:?} with a stuck client, {base_list:?} without"
    );
    drop(stuck);
}

/// One control connection to the server, for timing requests without a
/// CLI spawn each.
struct Control {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Control {
    fn connect(srv: &TestServer) -> Control {
        let writer = UnixStream::connect(&srv.socket).expect("connect");
        writer
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let reader = BufReader::new(writer.try_clone().expect("clone"));
        let mut control = Control {
            reader,
            writer,
            next_id: 1,
        };
        let hello = encode_hello(&Hello::Control(srv.ctx())).expect("hello");
        writeln!(control.writer, "{hello}").expect("send hello");
        control
    }

    /// Runs one command: its output, or its error.
    fn cmd(&mut self, argv: &[&str]) -> Result<String, String> {
        let id = self.next_id;
        self.next_id += 1;
        let argv = argv.iter().map(|a| a.to_string()).collect();
        let req = serde_json::to_string(&Request { id, argv }).expect("request");
        writeln!(self.writer, "{req}").expect("send request");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read reply");
        match serde_json::from_str(&line).expect("reply") {
            Reply::Ok { out, .. } => Ok(out),
            Reply::Err { err, .. } => Err(err),
            Reply::Event(e) => Err(format!("unexpected event {e:?}")),
        }
    }
}

/// The fastest of three times client `b` takes to show a new
/// `status-left` (`<tag>N-MARK`, within `status-left-length`'s default
/// 10 columns), and the median of five `list-sessions` round trips.
fn latencies(control: &mut Control, b: &mut Harness, tag: &str) -> (Duration, Duration) {
    let status = (0..3)
        .map(|n| {
            let marker = format!("{tag}{n}-MARK ");
            let asked = Instant::now();
            control
                .cmd(&["set-option", "-g", "status-left", &marker])
                .expect("set status-left");
            b.wait_for_timeout(marker.trim_end(), Duration::from_secs(10));
            asked.elapsed()
        })
        .min()
        .unwrap_or_default();
    let mut list: Vec<Duration> = (0..5)
        .map(|_| {
            let asked = Instant::now();
            let sessions = control.cmd(&["list-sessions"]).expect("list-sessions");
            assert!(sessions.starts_with("main:"), "{sessions}");
            asked.elapsed()
        })
        .collect();
    list.sort();
    (status, list[2])
}

/// A raw attach client that has sent its hello, followed in the same write
/// by `typeahead` (raw frame bytes), and read nothing, not even the reply
/// line. With `rcvbuf`, its socket receive buffer is set that small first.
fn raw_attach(
    srv: &TestServer,
    (cols, rows): (u16, u16),
    rcvbuf: Option<i32>,
    typeahead: &[u8],
) -> UnixStream {
    let mut stream = UnixStream::connect(&srv.socket).expect("connect");
    if let Some(size) = rcvbuf {
        let size: nix::libc::c_int = size;
        // SAFETY: a valid socket fd and a pointer to a live c_int of the size given.
        let rc = unsafe {
            nix::libc::setsockopt(
                stream.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                nix::libc::SO_RCVBUF,
                (&size as *const nix::libc::c_int).cast(),
                std::mem::size_of::<nix::libc::c_int>() as nix::libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "setsockopt SO_RCVBUF");
    }
    let hello = Hello::Attach {
        ctx: ClientCtx {
            cwd: srv.home.clone(),
            pane: None,
            env: Vec::new(),
            client: None,
        },
        cols,
        rows,
        term: "xterm-256color".to_string(),
        caps: Caps::default(),
        target: Some("main".to_string()),
        detach_others: false,
        nest: false,
    };
    let mut bytes = (encode_hello(&hello).expect("hello") + "\n").into_bytes();
    bytes.extend_from_slice(typeahead);
    stream.write_all(&bytes).expect("send hello");
    stream
}

/// Reads a raw client's stream (the reply line, then frames) until its
/// output holds `needle`; false if it has not after `timeout`.
fn read_until(stream: &mut UnixStream, needle: &str, timeout: Duration) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("read timeout");
    let deadline = Instant::now() + timeout;
    let mut buf = BytesMut::new();
    let mut output = Vec::new();
    let mut reply_read = false;
    let mut chunk = [0u8; 64 * 1024];
    while Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("reading the raw client: {e}"),
        }
        if !reply_read {
            let Some(end) = buf.iter().position(|b| *b == b'\n') else {
                continue;
            };
            let _ = buf.split_to(end + 1);
            reply_read = true;
        }
        while let Some(frame) = FrameCodec.decode(&mut buf).expect("frame") {
            if let Frame::Output(bytes) = frame {
                output.extend_from_slice(&bytes);
            }
        }
        if String::from_utf8_lossy(&output).contains(needle) {
            return true;
        }
    }
    false
}

/// A client whose frame was dropped while it lagged gets the screen as it
/// is once it reads again, even when nothing changes after that (fix round
/// 1). A huge raw client that never reads is letterboxed by a small one,
/// so its frame is ~600 KB, more than any socket buffer, and blocks the
/// writer. The pane then prints `B-MARK`, whose frame waits in the one
/// slot, and a second later `FINAL-MARK`, whose frame finds the slot full
/// and is dropped. With no status line (no clock), no input and no
/// command after that, only the dropped frame's own retry can deliver it.
#[test]
fn a_lagging_client_catches_up_with_a_pane_that_went_quiet() {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "status", "off"])
        .expect("status off");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "read x; printf B-MARK; sleep 1; printf FINAL-MARK; exec sleep 100",
    ])
    .expect("new-session");
    let mut big = raw_attach(&srv, (1000, 300), None, &[]);
    sleep(Duration::from_millis(300));
    // The small client attaches last, so it owns the size: `big`
    // letterboxes.
    let mut small = Harness::spawn(attach(&srv, "main"), 80, 24);
    wait_pane_size(&srv, "80x24");
    sleep(Duration::from_millis(500));
    srv.cmd(&["send-keys", "-t", "main", "Enter"])
        .expect("send-keys");
    small.wait_for("FINAL-MARK");
    // No command until `big` reads: every command redraws every client,
    // which would hide a dropped frame that is never retried.
    sleep(Duration::from_millis(500));
    assert!(
        read_until(&mut big, "FINAL-MARK", Duration::from_secs(10)),
        "the lagging client never got the pane's last output"
    );
}

/// Frames that arrive in the same read as the hello (typeahead) are
/// decoded at once, not held until the client sends more (fix round 1).
#[test]
fn input_sent_with_the_hello_reaches_the_pane_without_more_input() {
    let srv = start((80, 23));
    let mut input = BytesMut::new();
    FrameCodec
        .encode(Frame::Input(b"early\r".to_vec()), &mut input)
        .expect("encode input");
    let _stream = raw_attach(&srv, (80, 24), None, &input);
    let arrived = wait_until(
        || srv.capture("main").contains("got-early"),
        Duration::from_secs(5),
    );
    arrived.expect("typeahead reached the pane");
}

/// A 1-column client: the window is 1 column wide, but the pane's pty and
/// emulator stay 2 wide, since alacritty panics writing a wide character
/// into a 1-column terminal (M2 addendum). The pane keeps working.
#[test]
fn a_one_column_window_keeps_its_pane_two_columns_wide() {
    let srv = start((80, 23));
    let mut a = Harness::spawn(attach(&srv, "main"), 20, 6);
    a.wait_for("grofe-prompt$");
    a.resize(1, 6);
    wait_pane_size(&srv, "1x5");
    srv.cmd(&["send-keys", "-t", "main", "-l", "你好"])
        .expect("send-keys");
    srv.cmd(&["send-keys", "-t", "main", "Enter"])
        .expect("send-keys");
    a.resize(40, 12);
    wait_pane_size(&srv, "40x11");
    srv.cmd(&["send-keys", "-t", "main", "still-alive", "Enter"])
        .expect("send-keys");
    // Printed at column 60 of a 40-column pane: it wraps.
    a.wait_for("still-alive");
    assert!(srv.cmd(&["has-session", "-t", "main"]).is_ok());
}

/// `attach` from inside a pane (spec §4.6): the pane's own session is
/// refused without `--nest`, another session is allowed.
#[test]
fn attach_from_inside_a_pane_refuses_only_self_nesting() {
    let srv = start((80, 23));
    srv.cmd(&["new-session", "-d", "-s", "other", PROMPT])
        .expect("new-session other");
    let pane = srv
        .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
        .expect("pane id");
    let inside = |target: &str, extra: &[&str]| {
        let mut cmd = attach(&srv, target);
        cmd.args(extra)
            .env("GROFE", &srv.socket)
            .env("GROFE_PANE", pane.trim());
        cmd
    };

    let mut refused = Harness::spawn(inside("main", &[]), 80, 24);
    refused.wait_for("refusing to attach session main from inside itself");
    refused.wait_exit();

    let mut other = Harness::spawn(inside("other", &[]), 80, 24);
    other.wait_for("[other]");
    srv.cmd(&["detach-client"]).expect("detach");
    other.wait_exit();

    let mut nested = Harness::spawn(inside("main", &["--nest"]), 80, 24);
    nested.wait_for("[main]");
    srv.cmd(&["detach-client"]).expect("detach");
    nested.wait_exit();
}

/// The client is told why it goes away: its session was killed, or the
/// server exited.
#[test]
fn the_client_hears_why_it_was_let_go() {
    let srv = start((80, 23));
    srv.cmd(&["new-session", "-d", "-s", "other", PROMPT])
        .expect("new-session other");
    let mut a = Harness::spawn(attach(&srv, "main"), 80, 24);
    a.wait_for("[main]");
    srv.cmd(&["kill-session", "-t", "main"])
        .expect("kill-session");
    a.wait_for("[exited]");
    a.wait_exit();

    let mut b = Harness::spawn(attach(&srv, "other"), 80, 24);
    b.wait_for("[other]");
    srv.cmd(&["kill-server"]).expect("kill-server");
    b.wait_for("[server exited]");
    b.wait_exit();
}
