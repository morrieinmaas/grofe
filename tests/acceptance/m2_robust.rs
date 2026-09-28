//! M2 robustness review: hostile or unlucky client input (huge sizes,
//! resize storms, idle connections) must not stall the actor.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use grofe::protocol::{Caps, ClientCtx, Frame, FrameCodec, Hello, Reply, Request, encode_hello};
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
        },
        cols,
        rows,
        term: "xterm-256color".to_string(),
        caps: Caps::default(),
        target: Some("main".to_string()),
        detach_others: false,
        nest: false,
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
/// and grofe's 4M-cell area (400 rows at that width), and the actor keeps
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
    assert_eq!(client_size(&mut control), "10000x400\n");
    let took = control.median_list();
    assert!(took < Duration::from_secs(1), "list-sessions took {took:?}");
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
    assert_eq!(client_size(&mut control), "10000x400\n");
    let took = control.median_list();
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

/// A 2000x2000 client's first frame goes over the 16 MiB frame cap: a
/// screen of wide characters costs a cursor move after each one, some
/// 28 MB. It arrives split across frames, and the client stays attached.
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
        "2000",
        "-y",
        "2000",
        "awk 'BEGIN { for (i = 0; i < 1999000; i++) printf \"\\344\\270\\255\" }'; \
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
    let mut client = raw_attach(&srv, (2000, 2000));
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
    assert_eq!(client_size(&mut control), "2000x2000\n");
}
