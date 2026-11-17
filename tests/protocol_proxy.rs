//! The proxy protocol `grofe-proxy 1` (`docs/protocol-proxy.md`): byte for
//! byte fixtures in `tests/fixtures/proxy/` for the hellos, every frame kind
//! and the flow-control constants; the multiplexer's flow control over a
//! socketpair; and `grofe proxy` as a child over pipes against a real server.
//!
//! `GROFE_BLESS=1 cargo test --test protocol_proxy blessing` writes the
//! fixtures the samples below have and the directory lacks. It never rewrites
//! one that exists; changing a frozen fixture takes `GROFE_BLESS=break`.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::TestServer;
use grofe::protocol::{Caps, Dialect, Hello, Reply, Request, encode_hello};
use grofe::remote::proxy_proto::{
    Frame, FrameError, INITIAL_WINDOW, MAX_CHUNK, MAX_PAYLOAD, Mux, OpenSpec, PROXY_VERSION,
    ProxyConn, ProxyHello, RecvWindow, Role, SendWindow, decode_hello,
    encode_hello as encode_phello, read_frame, write_frame,
};

const WAIT: Duration = Duration::from_secs(10);

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proxy")
}

// --- fixtures ----------------------------------------------------------------

fn hello_sample() -> ProxyHello {
    ProxyHello {
        proxy: 1,
        grofe: "0.2.0".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        features: vec![
            "attach".into(),
            "control".into(),
            "tcp".into(),
            "upload".into(),
        ],
    }
}

fn frames() -> Vec<(&'static str, Frame)> {
    vec![
        (
            "frame-open-attach",
            Frame::Open(1, serde_json::to_vec(&OpenSpec::Attach).unwrap_or_default()),
        ),
        (
            "frame-open-tcp",
            Frame::Open(
                3,
                serde_json::to_vec(&OpenSpec::Tcp { port: 8080 }).unwrap_or_default(),
            ),
        ),
        (
            "frame-open-upload",
            Frame::Open(
                5,
                serde_json::to_vec(&OpenSpec::Upload {
                    dir: "/srv/x".into(),
                    name: "a.txt".into(),
                })
                .unwrap_or_default(),
            ),
        ),
        ("frame-data", Frame::Data(1, b"hello".to_vec())),
        ("frame-eof", Frame::Eof(1)),
        ("frame-close", Frame::Close(1, String::new())),
        (
            "frame-close-reason",
            Frame::Close(2, "connection refused".into()),
        ),
        ("frame-ping", Frame::Ping(0x0102_0304_0506_0708)),
        ("frame-pong", Frame::Pong(0x0102_0304_0506_0708)),
        ("frame-window", Frame::Window(1, 131_072)),
    ]
}

fn limits() -> String {
    format!(
        "{{\"max_payload\":{MAX_PAYLOAD},\"initial_window\":{INITIAL_WINDOW},\"max_chunk\":{MAX_CHUNK},\"grant_at\":{}}}\n",
        INITIAL_WINDOW / 2
    )
}

fn wanted() -> Vec<(String, Vec<u8>)> {
    let mut out = vec![
        (
            "hello-proxy.json".to_string(),
            encode_phello(&hello_sample()).into_bytes(),
        ),
        ("limits.json".to_string(), limits().into_bytes()),
    ];
    for (name, f) in frames() {
        out.push((format!("{name}.bin"), f.encode()));
    }
    out
}

fn read_fixture(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

#[test]
fn blessing() {
    let Some(mode) = std::env::var_os("GROFE_BLESS") else {
        return;
    };
    std::fs::create_dir_all(dir()).unwrap();
    for (name, bytes) in wanted() {
        let path = dir().join(&name);
        match std::fs::read(&path) {
            Ok(old) if old == bytes => {}
            Ok(_) if mode != "break" => {
                panic!("fixture {name} would change; only GROFE_BLESS=break rewrites one")
            }
            _ => std::fs::write(&path, bytes).unwrap(),
        }
    }
}

#[test]
fn every_fixture_matches_byte_for_byte() {
    for (name, bytes) in wanted() {
        assert_eq!(read_fixture(&name), bytes, "{name}");
    }
}

#[test]
fn frame_fixtures_decode_to_their_frames() {
    for (name, frame) in frames() {
        let bytes = read_fixture(&format!("{name}.bin"));
        let got = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(got, Some(frame), "{name}");
    }
}

#[test]
fn hello_fixture_decodes_and_current_hello_has_the_version() {
    let line = String::from_utf8(read_fixture("hello-proxy.json")).unwrap();
    assert_eq!(decode_hello(&line).unwrap(), hello_sample());
    assert!(line.starts_with(r#"{"grofe-proxy":1,"#), "{line}");
    assert_eq!(ProxyHello::current(&[]).proxy, PROXY_VERSION);
}

#[test]
fn a_newer_proxy_version_is_refused() {
    let err = decode_hello(r#"{"grofe-proxy":2,"grofe":"9","os":"x","arch":"y"}"#).unwrap_err();
    assert!(err.contains("version 2"), "{err}");
}

#[test]
fn framing_rules() {
    // header layout: kind, channel BE, len BE
    let bytes = Frame::Data(0x0A0B_0C0D, vec![1, 2, 3]).encode();
    assert_eq!(bytes, [2, 0x0A, 0x0B, 0x0C, 0x0D, 0, 0, 0, 3, 1, 2, 3]);
    // clean end between frames
    assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
    // cut mid header
    assert!(matches!(
        read_frame(&mut [2u8, 0].as_slice()),
        Err(FrameError::Io(_))
    ));
    // unknown kind
    let unknown = [99u8, 0, 0, 0, 1, 0, 0, 0, 0];
    assert!(matches!(
        read_frame(&mut unknown.as_slice()),
        Err(FrameError::UnknownKind(99))
    ));
    // one byte over 1 MiB
    let mut big = vec![2u8, 0, 0, 0, 1];
    big.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
    assert!(matches!(
        read_frame(&mut big.as_slice()),
        Err(FrameError::TooLong(_))
    ));
    // exactly 1 MiB is fine
    let mut out = Vec::new();
    write_frame(&mut out, &Frame::Data(1, vec![0; MAX_PAYLOAD])).unwrap();
    assert!(read_frame(&mut out.as_slice()).unwrap().is_some());
    // a window frame with a wrong size
    let bad = [7u8, 0, 0, 0, 1, 0, 0, 0, 1, 0];
    assert!(matches!(
        read_frame(&mut bad.as_slice()),
        Err(FrameError::Malformed("window"))
    ));
}

#[test]
fn credit_windows() {
    let mut send = SendWindow::new();
    assert_eq!(send.take(1 << 30), INITIAL_WINDOW as usize);
    assert_eq!(send.take(1), 0);
    send.add(10);
    assert_eq!(send.take(100), 10);

    let mut recv = RecvWindow::default();
    assert_eq!(recv.consumed(INITIAL_WINDOW as usize / 2 - 1), None);
    assert_eq!(recv.consumed(1), Some(INITIAL_WINDOW / 2));
    assert_eq!(recv.consumed(5), None);
}

// --- the multiplexer over a socketpair -----------------------------------------

/// A client `Mux` joined to a proxy-side `Mux` whose `on_open` is `handler`.
fn pair(
    handler: impl Fn(grofe::remote::proxy_proto::Channel, Vec<u8>) + Send + 'static,
) -> (Mux, Mux) {
    let (a, b) = UnixStream::pair().unwrap();
    let client = Mux::start(a.try_clone().unwrap(), a, Role::Client, |c, _| {
        c.close("no")
    });
    let proxy = Mux::start(b.try_clone().unwrap(), b, Role::Proxy, handler);
    (client, proxy)
}

fn echo(mut chan: grofe::remote::proxy_proto::Channel, _: Vec<u8>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = chan.read(&mut buf) {
            if n == 0 || chan.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        chan.close("");
    });
}

#[test]
fn a_busy_channel_cannot_starve_another() {
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&held);
    let (client, _proxy) = pair(move |chan, payload| {
        // The tcp channel is never read; the attach one echoes.
        match serde_json::from_slice::<OpenSpec>(&payload) {
            Ok(OpenSpec::Tcp { .. }) => keep.lock().unwrap().push(chan),
            _ => echo(chan, payload),
        }
    });
    let mut tcp = client.open(&OpenSpec::Tcp { port: 1 }).unwrap();
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sent);
    std::thread::spawn(move || {
        let block = vec![7u8; 8192];
        while tcp.write_all(&block).is_ok() {
            counter.fetch_add(block.len(), Ordering::SeqCst);
        }
    });
    // The writer stops at the credit the unread channel granted.
    common::wait_until("the tcp writer to stall", WAIT, || {
        let a = sent.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        a == sent.load(Ordering::SeqCst) && a > 0
    });
    assert_eq!(sent.load(Ordering::SeqCst), INITIAL_WINDOW as usize);

    let mut attach = client.open(&OpenSpec::Attach).unwrap();
    attach.write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    attach.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping");
}

#[test]
fn a_reader_that_consumes_keeps_a_big_transfer_going() {
    let (client, _proxy) = pair(echo);
    let chan = client.open(&OpenSpec::Control).unwrap();
    let mut w = chan.clone();
    let total = 3 * INITIAL_WINDOW as usize;
    let writer = std::thread::spawn(move || {
        w.write_all(&vec![9u8; total]).unwrap();
        w.shutdown_write().unwrap();
    });
    let mut r = chan;
    let mut n = 0;
    let mut buf = vec![0u8; 8192];
    loop {
        let k = r.read(&mut buf).unwrap();
        if k == 0 {
            break;
        }
        n += k;
    }
    writer.join().unwrap();
    assert_eq!(n, total);
}

#[test]
fn ping_measures_a_round_trip_and_odd_even_ids() {
    let (client, _proxy) = pair(echo);
    assert!(client.ping(WAIT).unwrap() < WAIT);
    assert_eq!(client.open(&OpenSpec::Attach).unwrap().id(), 1);
    assert_eq!(client.open(&OpenSpec::Attach).unwrap().id(), 3);
}

#[test]
fn a_peer_going_away_fails_channels_and_pings() {
    let (a, b) = UnixStream::pair().unwrap();
    let client = Mux::start(a.try_clone().unwrap(), a, Role::Client, |c, _| c.close(""));
    let mut chan = client.open(&OpenSpec::Attach).unwrap();
    drop(b);
    assert!(client.ping(WAIT).is_err());
    assert!(chan.read(&mut [0u8; 1]).is_err());
    assert!(client.is_closed());
}

#[test]
fn an_unknown_kind_closes_the_connection_with_an_error_line() {
    let (a, mut b) = UnixStream::pair().unwrap();
    let client = Mux::start(a.try_clone().unwrap(), a, Role::Client, |c, _| c.close(""));
    b.write_all(&[99, 0, 0, 0, 1, 0, 0, 0, 0]).unwrap();
    assert!(client.wait_closed().contains("unknown frame kind 99"));
    let mut b = BufReader::new(b);
    let Ok(Some(Frame::Close(0, why))) = read_frame(&mut b) else {
        panic!("no connection close frame");
    };
    assert!(why.contains("unknown frame kind 99"), "{why}");
}

// --- grofe proxy as a child ------------------------------------------------------

fn spawn_proxy(srv: &TestServer) -> (Child, ProxyConn) {
    let mut cmd = srv.command();
    cmd.arg("proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let conn = ProxyConn::new(child.stdout.take().unwrap(), child.stdin.take().unwrap()).unwrap();
    (child, conn)
}

fn finish(mut child: Child, conn: ProxyConn) {
    drop(conn);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn proxy_says_hello_and_answers_pings() {
    let srv = TestServer::start();
    let (child, conn) = spawn_proxy(&srv);
    assert_eq!(conn.peer().proxy, PROXY_VERSION);
    assert!(conn.peer().features.iter().any(|f| f == "upload"));
    assert!(conn.ping(WAIT).unwrap() < WAIT);
    finish(child, conn);
}

#[test]
fn control_channel_runs_a_command() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "remote"]).unwrap();
    let (child, conn) = spawn_proxy(&srv);
    let mut chan = conn.open(&OpenSpec::Control).unwrap();
    let hello = encode_hello(&Hello::Control(srv.ctx())).unwrap();
    let req = serde_json::to_string(&Request {
        id: 1,
        argv: ["list-sessions", "-F", "#{session_name}"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    })
    .unwrap();
    writeln!(chan, "{hello}\n{req}").unwrap();
    let mut line = String::new();
    BufReader::new(chan.clone()).read_line(&mut line).unwrap();
    match serde_json::from_str::<Reply>(&line).unwrap() {
        Reply::Ok { out, .. } => assert!(out.contains("remote"), "{out}"),
        other => panic!("{other:?}"),
    }
    finish(child, conn);
}

#[test]
fn attach_channel_reaches_the_server() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "remote"]).unwrap();
    let (child, conn) = spawn_proxy(&srv);
    let mut chan = conn.open(&OpenSpec::Attach).unwrap();
    let hello = encode_hello(&Hello::Attach {
        ctx: srv.ctx(),
        cols: 80,
        rows: 24,
        term: "xterm-256color".into(),
        caps: Caps::default(),
        target: Some("remote".into()),
        detach_others: false,
        nest: false,
        tty: None,
        created: false,
        extras: Default::default(),
    })
    .unwrap();
    writeln!(chan, "{hello}").unwrap();
    // The server answers an attach with v1 frames: at least a header comes back.
    let mut head = [0u8; 4];
    chan.read_exact(&mut head).unwrap();
    assert!(srv.cmd(&["list-clients"]).unwrap().contains("remote"));
    finish(child, conn);
}

#[test]
fn tcp_channel_round_trips_past_the_window() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut r = s.try_clone().unwrap();
            let _ = std::io::copy(&mut r, &mut s);
        }
    });
    let srv = TestServer::start();
    let (child, conn) = spawn_proxy(&srv);
    let chan = conn.open(&OpenSpec::Tcp { port }).unwrap();
    let total = 2 * INITIAL_WINDOW as usize + 12_345;
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    let mut w = chan.clone();
    let sent = data.clone();
    let writer = std::thread::spawn(move || {
        w.write_all(&sent).unwrap();
        w.shutdown_write().unwrap();
    });
    let mut back = Vec::new();
    let mut r = chan;
    r.read_to_end(&mut back).unwrap();
    writer.join().unwrap();
    assert!(
        back == data,
        "echoed bytes differ ({} vs {})",
        back.len(),
        data.len()
    );
    finish(child, conn);
}

#[test]
fn tcp_channel_to_a_closed_port_fails_with_a_reason() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let srv = TestServer::start();
    let (child, conn) = spawn_proxy(&srv);
    let mut chan = conn.open(&OpenSpec::Tcp { port }).unwrap();
    let err = chan.read(&mut [0u8; 1]).unwrap_err();
    assert!(err.to_string().contains("cannot connect"), "{err}");
    finish(child, conn);
}

#[test]
fn upload_never_overwrites() {
    let srv = TestServer::start();
    let (child, conn) = spawn_proxy(&srv);
    let dest = srv.home.join("up/sub");
    let dest = dest.to_str().unwrap();
    let first = conn.upload(dest, "a.txt", &b"one"[..]).unwrap();
    let second = conn.upload(dest, "a.txt", &b"two"[..]).unwrap();
    let third = conn.upload(dest, "a.txt", &b"three"[..]).unwrap();
    assert_eq!(first, format!("{dest}/a.txt"));
    assert_eq!(second, format!("{dest}/a.txt (2)"));
    assert_eq!(third, format!("{dest}/a.txt (3)"));
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    assert_eq!(std::fs::read(&second).unwrap(), b"two");
    // A name with a path in it is refused.
    let err = conn.upload(dest, "../evil", &b"x"[..]).unwrap_err();
    assert!(err.to_string().contains("bad upload name"), "{err}");
    finish(child, conn);
}

#[test]
fn a_hello_with_another_version_fails_the_connection() {
    let srv = TestServer::start();
    let mut cmd = srv.command();
    cmd.arg("proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"grofe-proxy":2,"grofe":"9","os":"x","arch":"y"}}"#
    )
    .unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(err.contains("version 2"), "{err}");
    let _ = Dialect::Grofe;
}

/// A client that stays connected but sends nothing (its network went
/// away under a lingering ssh) is taken for gone: the proxy exits and the
/// server drops the attach behind it.
#[test]
fn a_silent_client_ends_the_proxy_and_its_attach() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "remote"]).unwrap();
    let mut cmd = srv.command();
    cmd.arg("proxy")
        .env(grofe::remote::proxy::TEST_SILENCE_ENV, "1000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let conn = ProxyConn::new(child.stdout.take().unwrap(), child.stdin.take().unwrap()).unwrap();
    let mut chan = conn.open(&OpenSpec::Attach).unwrap();
    let hello = encode_hello(&Hello::Attach {
        ctx: srv.ctx(),
        cols: 80,
        rows: 24,
        term: "xterm-256color".into(),
        caps: Caps::default(),
        target: Some("remote".into()),
        detach_others: false,
        nest: false,
        tty: None,
        created: false,
        extras: Default::default(),
    })
    .unwrap();
    writeln!(chan, "{hello}").unwrap();
    let mut head = [0u8; 4];
    chan.read_exact(&mut head).unwrap();
    assert!(srv.cmd(&["list-clients"]).unwrap().contains("remote"));
    // From here the client sends nothing, and its pipes stay open.
    let status = common::wait_child(child, Duration::from_secs(10)).expect("the proxy exits");
    assert!(status.success(), "{status:?}");
    common::wait_until(
        "the server to drop the attach",
        Duration::from_secs(10),
        || {
            srv.cmd(&["list-clients"])
                .is_ok_and(|o| o.trim().is_empty())
        },
    );
    drop(conn);
}
