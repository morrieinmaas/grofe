//! Client forwarding (spec 2026-11-15-ssh-remote §3) over a real `grofe
//! proxy` child and a real server, no ssh: file paste uploads, port
//! forwards and open-url, driven through `remote::forward`.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use grofe::event::Event;
use grofe::protocol::{Hello, Reply, Request, encode_hello};
use grofe::remote::connect::Link;
use grofe::remote::forward::LinkCell;
use grofe::remote::forward::{
    Action, Executor, ForwardMode, Forwarding, Input, Settings, UploadMode, list_forwards,
};
use grofe::remote::proxy_proto::OpenSpec;

const WAIT: Duration = Duration::from_secs(10);

fn spawn_proxy(srv: &TestServer) -> (Child, Link) {
    let mut cmd = srv.command();
    cmd.arg("proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let link = Link::over("host", stdout, stdin, None).unwrap();
    (child, link)
}

fn finish(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Feeds `input` to `fw`, runs every I/O action and feeds its answer back,
/// answering each prompt with `answer`; the client actions, in order.
fn drive(fw: &mut Forwarding, exec: &mut Executor, input: Input, answer: &str) -> Vec<Action> {
    let mut client = Vec::new();
    let mut inputs = vec![input];
    while let Some(input) = inputs.pop() {
        for action in fw.handle(input, Instant::now()) {
            if let Action::Prompt(_) = &action {
                inputs.push(Input::Answer(answer.to_string()));
            }
            match exec.run(action.clone()) {
                Some(next) => inputs.push(next),
                None => client.push(action),
            }
        }
    }
    client
}

fn settings(upload: UploadMode, ports: ForwardMode) -> Settings {
    Settings {
        upload_paste: upload,
        forward_ports: ports,
        ..Settings::default()
    }
}

#[test]
fn a_pasted_file_pair_is_uploaded_to_the_panes_directory() {
    let srv = TestServer::start();
    let dest = srv.home.join("sub");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("a.txt"), "already here").unwrap();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-c",
        dest.to_str().unwrap(),
    ])
    .unwrap();
    let (child, conn) = spawn_proxy(&srv);

    let local = tempfile::tempdir().unwrap();
    let a = local.path().join("a.txt");
    let b = local.path().join("it's b.txt");
    std::fs::write(&a, "one").unwrap();
    std::fs::write(&b, "two").unwrap();
    let pasted = format!(
        "\x1b[200~{} {}\x1b[201~",
        a.display(),
        b.display()
            .to_string()
            .replace(' ', "\\ ")
            .replace('\'', "\\'")
    );

    let mut fw = Forwarding::new("host", settings(UploadMode::Ask, ForwardMode::Off));
    let mut exec = Executor::new(
        LinkCell::new(conn.clone()),
        "host",
        Some("main".into()),
        None,
    );
    let mut prompts = Vec::new();
    let mut client = Vec::new();
    for action in drive(&mut fw, &mut exec, Input::Keys(pasted.into_bytes()), "y") {
        match action {
            Action::Prompt(p) => prompts.push(p),
            other => client.push(other),
        }
    }
    // The pane reports its real directory; macOS's /tmp is a link, so there
    // HOME (the link form) is not a prefix of it and no tilde shows.
    let real = std::fs::canonicalize(&dest).unwrap();
    let shown = if real == dest {
        "~/sub".to_string()
    } else {
        real.display().to_string()
    };
    assert_eq!(
        prompts,
        [format!("upload 2 files (6 B) to host:{shown}? [y/N]")]
    );
    let first = real.join("a.txt (2)");
    let second = real.join("it's b.txt");
    let want = format!(
        "\x1b[200~'{}' '{}'\x1b[201~",
        first.display(),
        second.display().to_string().replace('\'', r"'\''")
    );
    assert_eq!(client, [Action::Send(want.into_bytes())]);
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "two");
    assert_eq!(
        std::fs::read_to_string(dest.join("a.txt")).unwrap(),
        "already here"
    );
    finish(child);
}

#[test]
fn a_remote_port_is_forwarded_until_it_goes() {
    // The "remote web server": an echo on the proxy's host, which is this one,
    // so the forward cannot take the same port and moves to the next.
    let remote = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = remote.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in remote.incoming() {
            let Ok(mut s) = s else { return };
            std::thread::spawn(move || {
                let mut r = s.try_clone().unwrap();
                let _ = std::io::copy(&mut r, &mut s);
            });
        }
    });
    let srv = TestServer::start();
    let (child, conn) = spawn_proxy(&srv);
    let state = tempfile::tempdir().unwrap();
    let mut fw = Forwarding::new("host", settings(UploadMode::Off, ForwardMode::Auto));
    let mut exec = Executor::new(
        LinkCell::new(conn.clone()),
        "host",
        None,
        Some(state.path().to_path_buf()),
    );
    let pane = "%1".parse().unwrap();
    let opened = Input::Event(Event::PortsChanged {
        pane,
        ports: vec![port],
    });
    let got = drive(&mut fw, &mut exec, opened, "");
    let forwards = exec.forwards();
    assert_eq!(forwards.len(), 1);
    let local = forwards[0].local;
    assert_ne!(local, port, "the remote's own port is taken here");
    assert_eq!(
        got,
        [Action::Message(format!(
            "forwarding host:{port} to localhost:{local}"
        ))]
    );
    assert_eq!(list_forwards(state.path(), "host"), forwards);

    for msg in ["hello", "again"] {
        let mut s = TcpStream::connect(("127.0.0.1", local)).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        s.write_all(msg.as_bytes()).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut back = String::new();
        s.read_to_string(&mut back).unwrap();
        assert_eq!(back, msg);
    }

    let closed = Input::Event(Event::PortsChanged {
        pane,
        ports: vec![],
    });
    // `drive` lists the actions with no answer, which a stop is.
    assert_eq!(
        drive(&mut fw, &mut exec, closed, ""),
        [Action::StopForward(port)]
    );
    assert!(exec.forwards().is_empty());
    assert!(list_forwards(state.path(), "host").is_empty());
    wait_until("the forward stops listening", WAIT, || {
        TcpStream::connect(("127.0.0.1", local)).is_err()
    });
    finish(child);
}

/// A script standing in for `open`/`xdg-open`: writes its argument to `out`.
fn fake_opener(dir: &Path, out: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("opener");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", out.display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The open-url test proper, run in a child of this test binary so that
/// `GROFE_OPENER` is set from the start instead of changed under other
/// tests' threads.
#[test]
fn open_url_child() {
    let Some(out) = std::env::var_os("GROFE_TEST_OPENED") else {
        return;
    };
    let srv = TestServer::start();
    let (child, conn) = spawn_proxy(&srv);
    // Subscribe through the proxy, as the remote client does.
    let chan = conn.open(&OpenSpec::Control).unwrap();
    let hello = encode_hello(&Hello::Control(srv.ctx())).unwrap();
    let req = serde_json::to_string(&Request {
        id: 1,
        argv: vec!["subscribe".into(), "open-url".into()],
        ..Default::default()
    })
    .unwrap();
    writeln!(chan.clone(), "{hello}\n{req}").unwrap();
    let mut events = BufReader::new(chan);
    let mut next = || {
        let mut line = String::new();
        assert!(events.read_line(&mut line).unwrap() > 0, "channel closed");
        match serde_json::from_str::<Reply>(&line).unwrap() {
            Reply::Event(e) => Some(e),
            Reply::Ok { .. } => None,
            Reply::Err { err, .. } => panic!("{err}"),
        }
    };
    assert_eq!(next(), None, "subscribed");

    let mut fw = Forwarding::new("host", Settings::default());
    let mut exec = Executor::new(LinkCell::new(conn.clone()), "host", None, None);
    srv.cmd(&["open-url", "https://example.com/x?y=1"]).unwrap();
    let event = next().unwrap();
    assert_eq!(drive(&mut fw, &mut exec, Input::Event(event), ""), []);
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "https://example.com/x?y=1"
    );

    // A file url for a path that does not exist here is refused.
    srv.cmd(&["open-url", "file:///no/such/file/here"]).unwrap();
    let event = next().unwrap();
    assert_eq!(
        drive(&mut fw, &mut exec, Input::Event(event), ""),
        [Action::Message(
            "grofe: not opening a url: that file does not exist here".into()
        )]
    );
    finish(child);
}

#[test]
fn open_url_opens_through_the_local_opener() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("opened");
    let opener = fake_opener(dir.path(), &out);
    let res = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "open_url_child", "--nocapture"])
        .env(grofe::open::OPENER_ENV, &opener)
        .env("GROFE_TEST_OPENED", &out)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&res.stdout);
    assert!(
        res.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&res.stderr)
    );
}
