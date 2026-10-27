//! Protocol v1, frozen (spec §4.6, §7.3, `docs/protocol-v1.md`): one fixture
//! line per message and event kind in `tests/fixtures/protocol/v1/`, each
//! parsing and re-serializing byte-identically; the server's behaviour on
//! the rules (version mismatch, unknown fields and verbs); and `grofe
//! control`, the stdio bridge.
//!
//! `GROFE_BLESS=1 cargo test --test protocol_v1 blessing` writes the fixtures
//! the samples below have and the directory lacks. It never rewrites one that
//! exists: a changed line is not additive (spec §7.3), so changing a frozen
//! fixture takes `GROFE_BLESS=break`, which a reviewer sees in the diff.

mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::thread;
use std::time::Duration;

use common::TestServer;
use grofe::event::Event;
use grofe::protocol::{
    Caps, ClientCtx, Dialect, Frame, FrameCodec, Hello, ProtocolError, Reply, Request, Side,
    decode_hello, encode_hello,
};
use grofe::types::{PaneId, SessionId, WindowId};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

const WAIT: Duration = Duration::from_secs(10);

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol/v1")
}

// --- the samples every fixture is written from -----------------------------

fn ctx(pane: Option<PaneId>, dialect: Dialect) -> ClientCtx {
    ClientCtx {
        cwd: "/home/u/src".into(),
        pane,
        env: vec![("SSH_AUTH_SOCK".into(), "/run/user/1000/ssh.sock".into())],
        client: None,
        dialect,
    }
}

fn hellos() -> Vec<(&'static str, Hello)> {
    vec![
        (
            "hello-control",
            Hello::Control(ctx(Some(PaneId(3)), Dialect::Grofe)),
        ),
        (
            "hello-control-tmux",
            Hello::Control(ctx(None, Dialect::Tmux)),
        ),
        (
            "hello-attach",
            Hello::Attach {
                ctx: ctx(None, Dialect::Tmux),
                cols: 200,
                rows: 50,
                term: "xterm-ghostty".into(),
                caps: Caps {
                    truecolor: true,
                    styled_underline: true,
                    sync: true,
                    kitty_keys: true,
                    kitty_graphics: true,
                    sixel: false,
                    cell_px: Some((9, 18)),
                },
                target: Some("main".into()),
                detach_others: false,
                nest: false,
                tty: Some("/dev/ttys004".into()),
            },
        ),
    ]
}

fn requests() -> Vec<(&'static str, Request)> {
    vec![
        (
            "request",
            Request {
                id: 7,
                argv: ["split-window", "-h", "-t", "%3"]
                    .map(String::from)
                    .to_vec(),
                ..Default::default()
            },
        ),
        (
            "request-subscribe",
            Request {
                id: 9,
                argv: ["subscribe", "pane-exited", "window-*"]
                    .map(String::from)
                    .to_vec(),
                ..Default::default()
            },
        ),
        (
            "request-stdin",
            Request {
                id: 10,
                argv: ["load-buffer", "-"].map(String::from).to_vec(),
                stdin: Some("aGk=".into()),
                umask: Some(0o022),
            },
        ),
    ]
}

fn mismatch(theirs: u32) -> String {
    let upgrade = if theirs < 1 {
        Side::Client
    } else {
        Side::Server
    };
    ProtocolError::VersionMismatch {
        ours: 1,
        theirs,
        upgrade,
    }
    .to_string()
}

fn replies() -> Vec<(&'static str, Reply)> {
    vec![
        (
            "reply-ok",
            Reply::Ok {
                id: 7,
                out: "%4\n".into(),
            },
        ),
        ("reply-err", Reply::err(8, "can't find pane: %99")),
        (
            "reply-err-status",
            Reply::Err {
                id: 11,
                err: "exited 3".into(),
                out: "partial\n".into(),
                status: Some(3),
            },
        ),
        (
            "reply-attach-ok",
            Reply::Ok {
                id: 0,
                out: String::new(),
            },
        ),
        ("error-version-newer", Reply::err(0, mismatch(2))),
        ("error-version-older", Reply::err(0, mismatch(0))),
        (
            "error-no-version",
            Reply::err(0, ProtocolError::MissingVersion.to_string()),
        ),
        (
            "error-unknown-verb",
            Reply::err(12, "unknown command: frobnicate"),
        ),
        ("error-unknown-event", Reply::err(13, "unknown event: nope")),
    ]
}

/// One sample per `Event` variant, in the order of [`index`].
fn event_samples() -> Vec<Event> {
    let (w, s, p) = (WindowId(1), SessionId(2), PaneId(3));
    vec![
        Event::PaneExited {
            pane: PaneId(4),
            uid: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
            status: Some(0),
        },
        Event::PaneDied {
            pane: p,
            uid: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
            window: w,
            status: Some(1),
        },
        Event::pane_output(p, b"hi\r\n"),
        Event::PaneFocus {
            pane: p,
            window: w,
            session: s,
            client: "client-1".into(),
        },
        Event::PaneResized { pane: p, window: w },
        Event::WindowFocused {
            window: w,
            session: s,
            pane: p,
        },
        Event::WindowCreated {
            window: w,
            session: s,
            pane: p,
        },
        Event::WindowResized {
            window: w,
            session: s,
        },
        Event::SessionCreated {
            session: s,
            name: "main".into(),
        },
        Event::SessionClosed {
            session: s,
            name: "main".into(),
        },
        Event::ClientSessionChanged {
            client: "client-1".into(),
            session: s,
        },
        Event::ClientResized {
            client: "client-1".into(),
            session: s,
            cols: 200,
            rows: 50,
        },
        Event::AlertBell {
            window: w,
            session: s,
            pane: p,
        },
        Event::AlertActivity {
            window: w,
            session: s,
            pane: p,
        },
        Event::AlertSilence {
            window: w,
            session: s,
            pane: p,
        },
        Event::OptionChanged {
            scope: "global".into(),
            name: "status".into(),
            value: Some("on".into()),
        },
        Event::EventsDropped { count: 3 },
    ]
}

/// The variant's position in [`event_samples`]. No wildcard: a new `Event`
/// variant does not compile here until it has a sample, and so a fixture.
fn index(event: &Event) -> usize {
    match event {
        Event::PaneExited { .. } => 0,
        Event::PaneDied { .. } => 1,
        Event::PaneOutput { .. } => 2,
        Event::PaneFocus { .. } => 3,
        Event::PaneResized { .. } => 4,
        Event::WindowFocused { .. } => 5,
        Event::WindowCreated { .. } => 6,
        Event::WindowResized { .. } => 7,
        Event::SessionCreated { .. } => 8,
        Event::SessionClosed { .. } => 9,
        Event::ClientSessionChanged { .. } => 10,
        Event::ClientResized { .. } => 11,
        Event::AlertBell { .. } => 12,
        Event::AlertActivity { .. } => 13,
        Event::AlertSilence { .. } => 14,
        Event::OptionChanged { .. } => 15,
        Event::EventsDropped { .. } => 16,
    }
}

/// Events whose second shape (a null) is fixed too.
fn event_extras() -> Vec<(&'static str, Event)> {
    vec![
        (
            "event-pane-exited-killed",
            Event::PaneExited {
                pane: PaneId(4),
                uid: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
                status: None,
            },
        ),
        (
            "event-option-changed-unset",
            Event::OptionChanged {
                scope: "$2".into(),
                name: "@x".into(),
                value: None,
            },
        ),
    ]
}

fn frames() -> Vec<(&'static str, Frame)> {
    vec![
        ("frame-input", Frame::Input(b"ls\r".to_vec())),
        ("frame-resize", Frame::Resize(200, 50)),
        ("frame-detach", Frame::Detach),
        ("frame-output", Frame::Output(b"\x1b[2Jhi".to_vec())),
        ("frame-exit", Frame::Exit("detached".into())),
        ("frame-suspend", Frame::Suspend),
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn encode_frame(frame: Frame) -> Vec<u8> {
    let mut buf = BytesMut::new();
    FrameCodec.encode(frame, &mut buf).unwrap();
    buf.to_vec()
}

/// Every fixture's name and its line, as the samples produce them.
fn blessed() -> Vec<(String, String)> {
    let mut all = Vec::new();
    for (name, hello) in hellos() {
        all.push((name.to_string(), encode_hello(&hello).unwrap()));
    }
    for (name, request) in requests() {
        all.push((name.to_string(), serde_json::to_string(&request).unwrap()));
    }
    for (name, reply) in replies() {
        all.push((name.to_string(), serde_json::to_string(&reply).unwrap()));
    }
    for event in event_samples() {
        let name = format!("event-{}", event.name());
        all.push((name, serde_json::to_string(&event).unwrap()));
    }
    for (name, event) in event_extras() {
        all.push((name.to_string(), serde_json::to_string(&event).unwrap()));
    }
    for (name, frame) in frames() {
        all.push((name.to_string(), hex(&encode_frame(frame))));
    }
    all
}

fn extension(name: &str) -> &'static str {
    if name.starts_with("frame-") {
        "hex"
    } else {
        "json"
    }
}

/// What blessing may do: the files to write, or the names of existing
/// fixtures it would change (refused unless `allow_break`).
fn bless_plan(
    existing: &[(String, String)],
    wanted: Vec<(String, String)>,
    allow_break: bool,
) -> Result<Vec<(String, String)>, Vec<String>> {
    let mut writes = Vec::new();
    let mut changed = Vec::new();
    for (name, line) in wanted {
        match existing.iter().find(|(n, _)| *n == name) {
            Some((_, old)) if *old == line => {}
            Some(_) if !allow_break => changed.push(name),
            _ => writes.push((name, line)),
        }
    }
    if changed.is_empty() {
        Ok(writes)
    } else {
        Err(changed)
    }
}

#[test]
fn blessing() {
    let Some(mode) = std::env::var_os("GROFE_BLESS") else {
        return;
    };
    std::fs::create_dir_all(dir()).unwrap();
    let existing = if dir().exists() {
        fixtures()
    } else {
        Vec::new()
    };
    let writes = bless_plan(&existing, blessed(), mode == "break").unwrap_or_else(|changed| {
        panic!(
            "the frozen v1 fixtures would change: {changed:?}; only GROFE_BLESS=break rewrites one"
        )
    });
    for (name, line) in writes {
        std::fs::write(
            dir().join(format!("{name}.{}", extension(&name))),
            line + "\n",
        )
        .unwrap();
    }
}

#[test]
fn blessing_adds_fixtures_but_refuses_to_change_a_frozen_one() {
    let line = |s: &str| (s.to_string(), format!("{{\"v\":\"{s}\"}}"));
    let existing = vec![line("a"), line("b")];
    let changed_a = ("a".to_string(), "{\"v\":\"other\"}".to_string());

    // The same lines plus a new one: only the new one is written.
    let mut wanted = existing.clone();
    wanted.push(line("c"));
    assert_eq!(
        bless_plan(&existing, wanted, false).unwrap(),
        vec![line("c")]
    );

    // A silent protocol change: the sample moved, the fixture did not.
    let wanted = vec![changed_a.clone(), line("b")];
    assert_eq!(
        bless_plan(&existing, wanted.clone(), false),
        Err(vec!["a".to_string()])
    );
    // Only the explicit break mode takes it.
    assert_eq!(
        bless_plan(&existing, wanted, true).unwrap(),
        vec![changed_a]
    );
}

// --- reading the fixtures back ---------------------------------------------

/// Every fixture file: its stem and its one line.
fn fixtures() -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = std::fs::read_dir(dir())
        .expect("tests/fixtures/protocol/v1 exists")
        .map(|entry| {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            assert_eq!(
                text.matches('\n').count(),
                1,
                "{stem}: one line, newline-terminated"
            );
            (stem, text.trim_end_matches('\n').to_string())
        })
        .collect();
    all.sort();
    all
}

/// Parses `line` as the message `name` says it is and serializes it again:
/// the result is what the current structs make of the fixture.
fn reserialize(name: &str, line: &str) -> Result<String, String> {
    let err = |e: &dyn std::fmt::Display| format!("{name}: {e}");
    if name.starts_with("hello-") {
        let hello = decode_hello(line).map_err(|e| err(&e))?;
        encode_hello(&hello).map_err(|e| err(&e))
    } else if name.starts_with("request") {
        let request: Request = serde_json::from_str(line).map_err(|e| err(&e))?;
        serde_json::to_string(&request).map_err(|e| err(&e))
    } else if name.starts_with("reply-") || name.starts_with("error-") {
        let reply: Reply = serde_json::from_str(line).map_err(|e| err(&e))?;
        serde_json::to_string(&reply).map_err(|e| err(&e))
    } else if name.starts_with("event-") {
        // As an `Event` and as the `Reply` a connection reads it as.
        let event: Event = serde_json::from_str(line).map_err(|e| err(&e))?;
        let reply: Reply = serde_json::from_str(line).map_err(|e| err(&e))?;
        assert_eq!(reply, Reply::Event(event.clone()), "{name}");
        serde_json::to_string(&event).map_err(|e| err(&e))
    } else if name.starts_with("frame-") {
        let mut buf = BytesMut::from(unhex(line).as_slice());
        let frame = FrameCodec.decode(&mut buf).map_err(|e| err(&e))?;
        let frame = frame.ok_or_else(|| err(&"short frame"))?;
        assert!(buf.is_empty(), "{name}: bytes after the frame");
        Ok(hex(&encode_frame(frame)))
    } else {
        Err(err(&"a fixture of no known kind"))
    }
}

#[test]
fn every_fixture_parses_and_reserializes_byte_identically() {
    let all = fixtures();
    assert!(all.len() >= 40, "only {} fixtures", all.len());
    for (name, line) in &all {
        assert_eq!(&reserialize(name, line).unwrap(), line, "{name}");
    }
}

#[test]
fn the_fixtures_are_exactly_what_the_samples_produce() {
    let mut expected = blessed();
    expected.sort();
    assert_eq!(fixtures(), expected);
}

#[test]
fn every_event_variant_has_a_fixture() {
    let samples = event_samples();
    for (i, event) in samples.iter().enumerate() {
        assert_eq!(index(event), i, "{} is out of order", event.name());
    }
    let names = fixtures();
    for event in &samples {
        let name = format!("event-{}", event.name());
        let line = &names
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("no fixture {name}"))
            .1;
        let parsed: Event = serde_json::from_str(line).unwrap();
        assert_eq!(index(&parsed), index(event), "{name}");
        assert_eq!(&parsed, event, "{name}");
    }
}

#[test]
fn every_frame_kind_and_message_kind_has_a_fixture() {
    let names: Vec<String> = fixtures().into_iter().map(|(n, _)| n).collect();
    for prefix in [
        "hello-control",
        "hello-attach",
        "request",
        "reply-ok",
        "reply-err",
    ] {
        assert!(names.iter().any(|n| n.starts_with(prefix)), "no {prefix}");
    }
    assert_eq!(
        names.iter().filter(|n| n.starts_with("frame-")).count(),
        6,
        "one fixture per frame kind"
    );
}

/// Every object in `value`, as a path of keys.
fn object_paths(value: &serde_json::Value, at: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    if let Some(map) = value.as_object() {
        out.push(at.clone());
        for (key, child) in map {
            at.push(key.clone());
            object_paths(child, at, out);
            at.pop();
        }
    }
}

/// A struct that lost a field parses a fixture without it, so the key is gone
/// when the fixture is written again and the byte-identity check goes red.
/// The same thing seen from the other side: a key the structs do not know is
/// dropped on the way back. Putting one into every object of every fixture,
/// the nested ones too, must therefore change the re-serialized line; where
/// it does not, a struct there would keep a removed field's key alive (a
/// catch-all) or the reader would not be looking at that object at all.
#[test]
fn removing_a_field_from_a_struct_turns_the_suite_red() {
    let mut probed = 0;
    for (name, line) in fixtures() {
        if name.starts_with("frame-") {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        let mut paths = Vec::new();
        object_paths(&value, &mut Vec::new(), &mut paths);
        for path in paths {
            let mut with = value.clone();
            let mut at = &mut with;
            for key in &path {
                at = at.get_mut(key).unwrap();
            }
            at.as_object_mut()
                .unwrap()
                .insert("zz_unknown".into(), true.into());
            let line = with.to_string();
            // `env` is a free-form map of strings: an unknown key there is
            // data, and a boolean is refused outright.
            let outcome = reserialize(&name, &line);
            assert!(
                outcome.as_ref().map_or(true, |again| *again != line),
                "{name}: an unknown key at {path:?} survives the round trip"
            );
            probed += 1;
        }
    }
    assert!(probed >= 34, "only {probed} probes");
}

// --- the rules, against a server -------------------------------------------

struct Raw {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Raw {
    fn open(srv: &TestServer) -> Raw {
        let writer = UnixStream::connect(&srv.socket).unwrap();
        writer.set_read_timeout(Some(WAIT)).unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        Raw { writer, reader }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.writer, "{line}").unwrap();
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }

    /// Whether the server has closed the connection.
    fn closed(&mut self) -> bool {
        let mut rest = String::new();
        self.reader.read_to_string(&mut rest).is_ok()
    }
}

fn fixture(name: &str) -> String {
    fixtures()
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no fixture {name}"))
        .1
}

/// A fixture hello with its version field replaced.
fn hello_with(name: &str, version: &str) -> String {
    let line = fixture(name);
    line.replace(r#""grofe":1"#, &format!(r#""grofe":{version}"#))
}

#[test]
fn a_hello_of_another_version_gets_the_named_error_and_a_close() {
    let srv = TestServer::start();
    for (hello, error) in [
        ("hello-control", "error-version-newer"),
        ("hello-attach", "error-version-newer"),
    ] {
        let mut raw = Raw::open(&srv);
        raw.send(&hello_with(hello, "2"));
        assert_eq!(raw.line(), fixture(error), "{hello}");
        assert!(raw.closed());
    }
    for hello in ["hello-control", "hello-attach"] {
        let mut raw = Raw::open(&srv);
        raw.send(&hello_with(hello, "0"));
        assert_eq!(raw.line(), fixture("error-version-older"), "{hello}");
        assert!(raw.closed());
    }
    let mut raw = Raw::open(&srv);
    raw.send(&fixture("hello-control").replace(r#""grofe":1,"#, ""));
    assert_eq!(raw.line(), fixture("error-no-version"));
    assert!(raw.closed());
    // The wording names the side that is behind.
    assert!(fixture("error-version-newer").ends_with("upgrade the server\"}"));
    assert!(fixture("error-version-older").ends_with("upgrade the client\"}"));
}

#[test]
fn an_unknown_mode_is_a_malformed_hello_and_closes() {
    let srv = TestServer::start();
    let mut raw = Raw::open(&srv);
    raw.send(&fixture("hello-control").replace(r#""mode":"control""#, r#""mode":"future""#));
    let reply: Reply = serde_json::from_str(&raw.line()).unwrap();
    assert!(matches!(reply, Reply::Err { err, .. } if err.starts_with("malformed hello")));
    assert!(raw.closed());
}

#[test]
fn a_hello_without_dialect_is_grofe_and_unknown_hello_fields_are_ignored() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "a"]).unwrap();
    let mut raw = Raw::open(&srv);
    let hello = fixture("hello-control").replace(r#""cwd""#, r#""future":{"x":1},"cwd""#);
    assert!(!hello.contains("dialect") || hello.contains(r#""dialect":"grofe""#));
    raw.send(&hello);
    raw.send(r##"{"id":1,"argv":["list-sessions","-F","#{session_name}"]}"##);
    assert_eq!(raw.line(), r#"{"id":1,"ok":true,"out":"a\n"}"#);
}

#[test]
fn an_unknown_request_field_is_ignored_and_an_unknown_verb_errors() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "a"]).unwrap();
    let mut raw = Raw::open(&srv);
    raw.send(&fixture("hello-control"));
    raw.send(r##"{"id":1,"argv":["list-sessions","-F","#{session_name}"],"future":{"a":[1]}}"##);
    assert_eq!(raw.line(), r#"{"id":1,"ok":true,"out":"a\n"}"#);

    raw.send(r#"{"id":12,"argv":["frobnicate"]}"#);
    assert_eq!(raw.line(), fixture("error-unknown-verb"));

    // The connection is still open and runs the next request.
    raw.send(r##"{"id":2,"argv":["list-sessions","-F","#{session_name}"]}"##);
    assert_eq!(raw.line(), r#"{"id":2,"ok":true,"out":"a\n"}"#);

    raw.send(r#"{"id":13,"argv":["subscribe","nope"]}"#);
    assert_eq!(raw.line(), fixture("error-unknown-event"));
}

#[test]
fn an_unknown_attach_frame_kind_is_an_error() {
    let mut buf = BytesMut::from(&[0xee_u8, 0, 0, 0, 0][..]);
    assert!(FrameCodec.decode(&mut buf).is_err());
}

#[test]
fn replies_come_in_request_order_with_events_between() {
    let srv = TestServer::start();
    let mut raw = Raw::open(&srv);
    raw.send(&fixture("hello-control"));
    raw.send(r#"{"id":1,"argv":["subscribe","option-changed"]}"#);
    assert_eq!(raw.line(), r#"{"id":1,"ok":true,"out":""}"#);
    raw.send(r#"{"id":2,"argv":["set","-g","@a","1"]}"#);
    raw.send(r#"{"id":3,"argv":["set","-g","@b","1"]}"#);
    let mut ids = Vec::new();
    let mut events = 0;
    while ids.len() < 2 || events < 2 {
        let line = raw.line();
        match serde_json::from_str::<Reply>(&line).unwrap() {
            Reply::Event(_) => events += 1,
            Reply::Ok { id, .. } => ids.push(id),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(ids, [2, 3]);
}

// --- grofe control ---------------------------------------------------------

fn bridge(srv: &TestServer) -> std::process::Child {
    let mut cmd = srv.command();
    cmd.arg("control")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn().unwrap()
}

struct Finished {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

/// Waits for the bridge to exit on its own (stdin already closed).
fn finish(mut child: std::process::Child) -> Finished {
    let mut stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let status = common::wait_child(child, WAIT).expect("the bridge exits");
    Finished {
        status,
        stdout: reader.join().unwrap(),
    }
}

#[test]
fn control_prints_one_reply_line_per_request_and_exits() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "a"]).unwrap();
    let mut child = bridge(&srv);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"id\":1,\"argv\":[\"list-sessions\"]}\n")
        .unwrap();
    let out = finish(child);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text:?}");
    let reply: Reply = serde_json::from_str(text.trim_end()).unwrap();
    assert!(matches!(reply, Reply::Ok { id: 1, ref out } if out.starts_with("a:")));
    assert!(out.status.success());
}

#[test]
fn control_answers_several_requests_and_relays_errors() {
    let srv = TestServer::start();
    let mut child = bridge(&srv);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"{\"id\":1,\"argv\":[\"frobnicate\"]}\n\n{\"id\":2,\"argv\":[\"display\",\"-p\",\"x\"]}\nnot json\n",
        )
        .unwrap();
    let out = finish(child);
    let lines: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(
        lines[0],
        fixture("error-unknown-verb").replace(r#""id":12"#, r#""id":1"#)
    );
    assert!(lines[2].contains("malformed request"));
}

#[test]
fn control_without_a_server_fails_with_a_message() {
    let srv = TestServer::unstarted();
    let mut cmd = srv.command();
    cmd.arg("control").stdin(Stdio::null());
    let out = common::run_bounded(cmd, WAIT);
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty());
    assert!(out.stdout.is_empty());
}

/// A subscribed bridge: the child, its stdin (kept open) and a reader on its
/// stdout, past the subscribe reply.
fn subscribed(
    srv: &TestServer,
    events: &str,
) -> (
    std::process::Child,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
) {
    let mut child = bridge(srv);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, r#"{{"id":1,"argv":["subscribe","{events}"]}}"#).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), r#"{"id":1,"ok":true,"out":""}"#);
    (child, stdin, stdout)
}

/// Sends `n` `set -g @n <i>-<pad>` requests on one connection, reading the
/// replies as it goes.
fn fire(srv: &TestServer, n: usize, pad: usize) {
    let mut raw = Raw::open(srv);
    raw.send(&fixture("hello-control"));
    let pad = "p".repeat(pad);
    let mut writer = raw.writer.try_clone().unwrap();
    let sender = thread::spawn(move || {
        for i in 0..n {
            writeln!(
                writer,
                r#"{{"id":{i},"argv":["set","-g","@n","{i}-{pad}"]}}"#
            )
            .unwrap();
        }
    });
    for _ in 0..n {
        assert!(raw.line().contains(r#""ok":true"#));
    }
    sender.join().unwrap();
}

#[test]
fn a_control_subscription_over_10000_events_loses_none_to_a_prompt_reader() {
    const N: usize = 10_000;
    let srv = TestServer::start();
    let (mut child, stdin, stdout) = subscribed(&srv, "option-changed");
    let reader = thread::spawn(move || {
        let mut seen = 0;
        for line in stdout.lines() {
            let line = line.unwrap();
            assert!(!line.contains("events-dropped"), "dropped: {line}");
            seen += 1;
            if seen == N {
                break;
            }
        }
        seen
    });
    fire(&srv, N, 0);
    assert_eq!(reader.join().unwrap(), N);
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_control_subscription_drops_only_when_its_reader_stalls() {
    const N: usize = 4000;
    let srv = TestServer::start();
    let (mut child, stdin, stdout) = subscribed(&srv, "option-changed");
    // Nobody reads the bridge's stdout while the events are made.
    fire(&srv, N, 512);

    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            let _ = tx.send(serde_json::from_str::<Event>(&line).unwrap());
        }
    });
    let mut received = Vec::new();
    while let Ok(event) = rx.recv_timeout(Duration::from_millis(500)) {
        received.push(event);
    }
    // One more event brings the notice for what was dropped before it.
    srv.cmd(&["set", "-g", "@done", "1"]).unwrap();
    received.push(rx.recv_timeout(WAIT).unwrap());
    received.push(rx.recv_timeout(WAIT).unwrap());

    let mut values = 0;
    let mut dropped = 0;
    for event in &received {
        match event {
            Event::OptionChanged { name, .. } if name == "@n" => values += 1,
            Event::OptionChanged { name, .. } => assert_eq!(name, "@done"),
            Event::EventsDropped { count } => dropped += *count as usize,
            other => panic!("{other:?}"),
        }
    }
    assert!(dropped > 0, "nothing was dropped");
    assert_eq!(values + dropped, N);
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn control_keeps_streaming_after_stdin_ends_when_subscribed() {
    let srv = TestServer::start();
    let mut child = bridge(&srv);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, r#"{{"id":1,"argv":["subscribe","option-changed"]}}"#).unwrap();
    drop(stdin);
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    srv.cmd(&["set", "-g", "@late", "1"]).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(
        line.trim_end(),
        r#"{"event":"option-changed","scope":"global","name":"@late","value":"1"}"#
    );
    let _ = child.kill();
    let _ = child.wait();
}
