//! M2 robustness review: hostile or unlucky client input (huge sizes,
//! resize storms, idle connections) must not stall the actor.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use grofe::protocol::{
    Caps, ClientCtx, Dialect, Frame, FrameCodec, Hello, Reply, Request, encode_hello,
};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

const IDLE: &str = "printf 'idle-pane'; exec sleep 600";

fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", IDLE])
        .expect("new-session");
    srv
}

/// One control connection, for timing requests without a CLI spawn each.
struct Control {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Control {
    fn connect(srv: &TestServer) -> Control {
        let writer = UnixStream::connect(&srv.socket).expect("connect");
        writer
            .set_read_timeout(Some(Duration::from_secs(30)))
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

    fn cmd(&mut self, argv: &[&str]) -> Result<String, String> {
        let id = self.next_id;
        self.next_id += 1;
        let argv = argv.iter().map(|a| a.to_string()).collect();
        let req = serde_json::to_string(&Request {
            id,
            argv,
            ..Default::default()
        })
        .expect("request");
        writeln!(self.writer, "{req}").expect("send request");
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read reply");
        match serde_json::from_str(&line).expect("reply") {
            Reply::Ok { out, .. } => Ok(out),
            Reply::Err { err, .. } => Err(err),
            Reply::Event(e) => Err(format!("unexpected event {e:?}")),
        }
    }

    /// How long one `list-sessions` round trip takes.
    fn time_list(&mut self) -> Duration {
        let asked = Instant::now();
        let out = self.cmd(&["list-sessions"]).expect("list-sessions");
        assert!(out.starts_with("main:"), "{out}");
        asked.elapsed()
    }

    /// The median of five `list-sessions` round trips.
    fn median_list(&mut self) -> Duration {
        let mut samples: Vec<Duration> = (0..5).map(|_| self.time_list()).collect();
        samples.sort();
        samples[2]
    }
}

/// A raw attach client that has sent its hello to `main` at `size`.
fn raw_attach(srv: &TestServer, (cols, rows): (u16, u16)) -> UnixStream {
    let mut stream = UnixStream::connect(&srv.socket).expect("connect");
    let hello = Hello::Attach {
        ctx: ClientCtx {
            cwd: srv.home.clone(),
            pane: None,
            env: Vec::new(),
            client: None,
            dialect: Dialect::Grofe,
        },
        cols,
        rows,
        term: "xterm-256color".to_string(),
        caps: Caps::default(),
        target: Some("main".to_string()),
        detach_others: false,
        nest: false,
        tty: None,
        created: false,
        extras: Default::default(),
    };
    let line = encode_hello(&hello).expect("hello") + "\n";
    stream.write_all(line.as_bytes()).expect("send hello");
    stream
}

fn send_frame(stream: &mut UnixStream, frame: Frame) {
    let mut buf = BytesMut::new();
    FrameCodec.encode(frame, &mut buf).expect("encode");
    stream.write_all(&buf).expect("send frame");
}

/// What a raw client read: output bytes, and whether an exit frame or a
/// hang-up ended it.
#[derive(Default)]
struct Received {
    output: Vec<u8>,
    frames: usize,
    ended: Option<String>,
}

/// Reads a raw client's stream: the reply line once, then frames.
struct Reader {
    buf: BytesMut,
    reply_read: bool,
}

impl Reader {
    fn new() -> Reader {
        Reader {
            buf: BytesMut::new(),
            reply_read: false,
        }
    }

    /// Reads until `done` holds for what arrived in this call, the client
    /// ended, or `timeout` passes.
    fn read(
        &mut self,
        stream: &mut UnixStream,
        timeout: Duration,
        done: impl Fn(&Received) -> bool,
    ) -> Received {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("read timeout");
        let deadline = Instant::now() + timeout;
        let mut got = Received::default();
        let mut chunk = vec![0u8; 1024 * 1024];
        let mut finished = false;
        // `done` runs only when a frame arrived: a socket read returns a few
        // KB, and rescanning megabytes of output after each is too slow.
        while Instant::now() < deadline && got.ended.is_none() && !finished {
            match stream.read(&mut chunk) {
                Ok(0) => got.ended = Some("hang-up".to_string()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => got.ended = Some(format!("read error: {e}")),
            }
            if !self.reply_read {
                let Some(end) = self.buf.iter().position(|b| *b == b'\n') else {
                    continue;
                };
                let _ = self.buf.split_to(end + 1);
                self.reply_read = true;
            }
            while let Some(frame) = FrameCodec.decode(&mut self.buf).expect("frame") {
                match frame {
                    Frame::Output(bytes) => {
                        got.frames += 1;
                        got.output.extend_from_slice(&bytes);
                        finished = done(&got);
                    }
                    Frame::Exit(reason) => got.ended = Some(format!("exit: {reason}")),
                    _ => {}
                }
            }
        }
        got
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn client_size(control: &mut Control) -> String {
    control
        .cmd(&["list-clients", "-F", "#{client_width}x#{client_height}"])
        .expect("list-clients")
}

/// A 65535x65535 resize is capped at tmux's WINDOW_MAXIMUM (10000) wide
/// and grofe's 1M-cell area (100 rows at that width), and the actor keeps
/// answering once the resized frame is out.
#[test]
fn a_huge_resize_frame_is_capped_and_the_server_keeps_answering() {
    let srv = start();
    let mut control = Control::connect(&srv);
    let mut client = raw_attach(&srv, (80, 24));
    let mut reader = Reader::new();
    reader.read(&mut client, Duration::from_secs(10), |r| {
        contains(&r.output, b"idle-pane")
    });
    send_frame(&mut client, Frame::Resize(65535, 65535));
    let got = reader.read(&mut client, Duration::from_secs(30), |r| {
        contains(&r.output, b"idle-pane")
    });
    assert_eq!(got.ended, None);
    assert!(
        contains(&got.output, b"idle-pane"),
        "no frame at the new size"
    );
    assert_eq!(client_size(&mut control), "10000x100\n");
    let took = control.median_list();
    eprintln!("list-sessions after the capped size: {took:?}");
    assert!(took < Duration::from_secs(1), "list-sessions took {took:?}");
}

/// Reads `client` until no frame has come for `quiet`, so a check that
/// follows starts from a client with nothing pending.
fn drain(client: &mut UnixStream, reader: &mut Reader, quiet: Duration) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while reader.read(client, quiet, |_| false).frames > 0 {
        assert!(Instant::now() < deadline, "the client never went quiet");
    }
}

/// Commands that only read do not redraw an attached client: each redraw
/// of a large client is actor time every other request waits behind. A
/// command that changes what is drawn still redraws.
///
/// A redraw whose screen is unchanged sends nothing, so the status line
/// shows seconds: any redraw a second after the last one sends new bytes.
#[test]
fn read_only_commands_do_not_redraw_clients() {
    let srv = start();
    let mut control = Control::connect(&srv);
    // No clock redraws of its own, only redraws something asked for.
    control
        .cmd(&["set", "-g", "status-interval", "0"])
        .expect("set");
    control
        .cmd(&["set", "-g", "status-right", "%H:%M:%S"])
        .expect("set");
    let mut client = raw_attach(&srv, (80, 24));
    let mut reader = Reader::new();
    reader.read(&mut client, Duration::from_secs(10), |r| {
        contains(&r.output, b"idle-pane")
    });
    // Longer than NAME_INTERVAL, so automatic-rename has settled.
    drain(&mut client, &mut reader, Duration::from_millis(700));
    // The clock has moved on since the last redraw.
    std::thread::sleep(Duration::from_millis(1100));

    let reads: &[&[&str]] = &[
        &["list-sessions"],
        &["list-windows", "-t", "main"],
        &["list-panes", "-t", "main", "-F", "#{pane_id}"],
        &["list-clients"],
        &["show-options", "-g"],
        &["has-session", "-t", "main"],
        &["display-message", "-p", "-t", "main", "#{window_name}"],
        &["capture-pane", "-p", "-t", "main"],
        &["__attach-check", "", "-t", "main"],
    ];
    for argv in reads {
        control
            .cmd(argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
    }
    let got = reader.read(&mut client, Duration::from_millis(500), |_| false);
    assert_eq!(got.frames, 0, "a read-only command redrew the client");

    control
        .cmd(&["rename-window", "-t", "main", "renamed-win"])
        .expect("rename-window");
    let got = reader.read(&mut client, Duration::from_secs(10), |r| {
        contains(&r.output, b"renamed-win")
    });
    assert!(
        contains(&got.output, b"renamed-win"),
        "rename-window did not redraw the status line"
    );
}

/// The same size in the attach hello is capped the same way.
#[test]
fn a_huge_size_in_the_hello_is_capped() {
    let srv = start();
    let mut control = Control::connect(&srv);
    let mut client = raw_attach(&srv, (65535, 65535));
    let got = Reader::new().read(&mut client, Duration::from_secs(30), |r| {
        contains(&r.output, b"idle-pane")
    });
    assert!(contains(&got.output, b"idle-pane"), "no first frame");
    assert_eq!(client_size(&mut control), "10000x100\n");
    let took = control.median_list();
    eprintln!("list-sessions after the capped size: {took:?}");
    assert!(took < Duration::from_secs(1), "list-sessions took {took:?}");
}

/// tmux 3.7b's `new-session -x/-y` range: below 1 or above 65535 is an
/// error, anything else is capped at 10000.
#[test]
fn new_session_sizes_are_range_checked_and_capped() {
    let srv = start();
    for (args, err) in [
        (&["-x", "0"][..], "width too small\n"),
        (&["-x", "-5"], "width too small\n"),
        (&["-x", "abc"], "width invalid\n"),
        (&["-x", "65536"], "width too large\n"),
        (&["-y", "0"], "height too small\n"),
        (&["-y", "99999999999"], "height too large\n"),
    ] {
        let mut argv = vec!["new-session", "-d"];
        argv.extend_from_slice(args);
        assert_eq!(srv.cmd(&argv), Err(err.to_string()), "{args:?}");
    }
    srv.cmd(&["new-session", "-d", "-s", "big", "-x", "20000", "-y", "30"])
        .expect("new-session -x 20000");
    assert_eq!(
        srv.cmd(&[
            "display-message",
            "-p",
            "-t",
            "big",
            "#{window_width}x#{window_height}"
        ]),
        Ok("10000x30\n".to_string())
    );
}

/// A 1000x1000 client's first frame goes over the 16 MiB frame cap: a
/// screen of wide characters in two alternating many-attribute styles
/// costs a full SGR and a cursor move for each one, some 23 MB. It arrives split across frames, and the client stays attached.
#[test]
fn a_very_large_client_gets_its_full_frame_without_a_disconnect() {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "status", "off"])
        .expect("status off");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "1000",
        "-y",
        "1000",
        "awk 'BEGIN { for (i = 0; i < 249750; i++) \
         printf \"\\033[1;2;3;4;7;9;38;5;196;48;5;21m\\344\\270\\255\\033[1;2;3;4;7;9;38;5;46;48;5;19m\\344\\270\\255\" }'; \
         printf END-MARK; exec sleep 600",
    ])
    .expect("new-session");
    wait_until(
        || {
            srv.cmd(&[
                "display-message",
                "-p",
                "-t",
                "main",
                "#{pane_current_command}",
            ])
            // `gsleep` where GNU coreutils come first on PATH.
            .is_ok_and(|c| c.trim().ends_with("sleep"))
        },
        Duration::from_secs(120),
    )
    .expect("the pane drew its screen");
    let mut client = raw_attach(&srv, (1000, 1000));
    let got = Reader::new().read(&mut client, Duration::from_secs(60), |r| {
        contains(&r.output, b"END-MARK")
    });
    assert_eq!(got.ended, None, "the client was let go");
    assert!(
        contains(&got.output, b"END-MARK"),
        "no full frame: {} bytes in {} frames",
        got.output.len(),
        got.frames
    );
    assert!(got.frames >= 2, "{} bytes in one frame", got.output.len());
    let mut control = Control::connect(&srv);
    assert_eq!(client_size(&mut control), "1000x1000\n");
}

/// A burst of resizes costs one reflow, not one each: 1000 alternating
/// Resize frames against 50k lines of history leave `list-sessions` as
/// quick as before them, and the last size wins.
#[test]
fn a_resize_storm_is_coalesced() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "seq 1 50000; exec sleep 600",
    ])
    .expect("new-session");
    wait_until(
        || {
            srv.cmd(&["capture-pane", "-p", "-t", "main"])
                .is_ok_and(|c| c.contains("50000"))
        },
        Duration::from_secs(30),
    )
    .expect("history filled");
    let mut control = Control::connect(&srv);
    let mut client = raw_attach(&srv, (80, 24));
    wait_until(
        || client_size(&mut control) == "80x24\n",
        Duration::from_secs(10),
    )
    .expect("attached");
    let base = control.median_list();

    let mut burst = BytesMut::new();
    for i in 0..1000u16 {
        let size = if i % 2 == 0 { (100, 30) } else { (120, 40) };
        FrameCodec
            .encode(Frame::Resize(size.0, size.1), &mut burst)
            .expect("encode");
    }
    FrameCodec
        .encode(Frame::Resize(90, 25), &mut burst)
        .expect("encode");
    client.write_all(&burst).expect("send burst");
    let during = control.median_list();
    // A floor of 1 s: a shared CI runner took 264 ms. Uncoalesced, a
    // thousand reflows of 50000 lines of history take many seconds.
    let bound = (base * 5).max(Duration::from_secs(1));
    assert!(
        during < bound,
        "list-sessions took {during:?} after the burst, {base:?} before"
    );
    wait_until(
        || client_size(&mut control) == "90x25\n",
        Duration::from_secs(10),
    )
    .expect("the last size is applied");
    assert_eq!(
        control.cmd(&[
            "display-message",
            "-p",
            "-t",
            "main",
            "#{pane_width}x#{pane_height}"
        ]),
        Ok("90x24\n".to_string())
    );
}

/// Eleven panes in a 1x1 client: none drops below tmux's PANE_MINIMUM of
/// one cell each way.
#[test]
fn panes_never_shrink_below_one_cell() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "300",
        "-y",
        "100",
        IDLE,
    ])
    .expect("new-session");
    for i in 0..10 {
        let dir = if i % 2 == 0 { "-h" } else { "-v" };
        srv.cmd(&["split-window", dir, "-t", "main", IDLE])
            .unwrap_or_else(|e| panic!("split {i}: {e}"));
    }
    let mut control = Control::connect(&srv);
    let _client = raw_attach(&srv, (1, 1));
    wait_until(
        || client_size(&mut control) == "1x1\n",
        Duration::from_secs(10),
    )
    .expect("attached");
    let sizes = control
        .cmd(&[
            "list-panes",
            "-t",
            "main",
            "-F",
            "#{pane_width}x#{pane_height}",
        ])
        .expect("list-panes");
    assert_eq!(sizes.lines().count(), 11, "{sizes}");
    for size in sizes.lines() {
        let (w, h) = size.split_once('x').expect("WxH");
        assert!(
            w.parse::<u16>().expect("width") >= 1 && h.parse::<u16>().expect("height") >= 1,
            "a pane is {size}:\n{sizes}"
        );
    }
}

/// Reads from `stream` until EOF; how long that took, or `None` if it
/// stayed open for `timeout`.
fn closed_within(stream: &mut UnixStream, timeout: Duration) -> Option<Duration> {
    let start = Instant::now();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("read timeout");
    let mut chunk = [0u8; 4096];
    while start.elapsed() < timeout {
        match stream.read(&mut chunk) {
            Ok(0) => return Some(start.elapsed()),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return Some(start.elapsed()),
        }
    }
    None
}

/// A connection that never sends its hello is closed after a few seconds.
#[test]
fn an_idle_connection_without_a_hello_is_closed() {
    let srv = start();
    let mut idle = UnixStream::connect(&srv.socket).expect("connect");
    let took = closed_within(&mut idle, Duration::from_secs(15)).expect("never closed");
    assert!(took >= Duration::from_secs(3), "closed after only {took:?}");
    assert!(srv.cmd(&["list-sessions"]).is_ok());
}

/// Requests after the hello keep the full message cap.
#[test]
fn a_large_request_after_the_hello_works() {
    let srv = start();
    let mut control = Control::connect(&srv);
    let value = "v".repeat(100 * 1024);
    control
        .cmd(&["set-option", "-g", "@big", &value])
        .expect("a 100 KiB request after the hello");
    let shown = control
        .cmd(&["display-message", "-p", "#{@big}"])
        .expect("display");
    assert!(shown.trim_end() == value, "@big is {} bytes", shown.len());
}

/// A real-sized environment (big direnv, nix or CI envs) fits in the
/// hello: 200 KB of variables, in two values since Linux caps one at
/// 128 KiB.
#[test]
fn a_command_with_a_large_environment_works() {
    let srv = start();
    let mut cmd = srv.command();
    cmd.env("GROFE_TEST_BIG_A", "a".repeat(100 * 1024))
        .env("GROFE_TEST_BIG_B", "b".repeat(100 * 1024))
        .args(["list-sessions", "-F", "#{session_name}"]);
    let out = common::run_bounded(cmd, common::CMD_TIMEOUT);
    assert!(out.status.success(), "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "main\n");
}

/// A hello past the 4 MiB cap, from a raw socket, is closed.
#[test]
fn an_over_cap_hello_is_closed() {
    let srv = start();
    let mut big = UnixStream::connect(&srv.socket).expect("connect");
    // The server may hang up before the whole line is written.
    let closed = big.write_all(&vec![b'x'; 5 * 1024 * 1024]).is_err()
        || closed_within(&mut big, Duration::from_secs(10)).is_some();
    assert!(closed, "a 5 MiB hello line was kept open");
    assert!(srv.cmd(&["list-sessions"]).is_ok());
}
