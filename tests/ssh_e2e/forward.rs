//! Forwarding over a real ssh link (spec ssh-remote §3): a port a pane in
//! the container listens on reaches localhost here, and a pasted local file
//! lands in the pane's directory there.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::harness::{Cleanup, E2e, Local, REMOTE_TIMEOUT, run, text};
use crate::pty::Harness;

/// A port nothing else in the container uses (the smoke web server is on
/// 8000).
const WEB_PORT: u16 = 18080;

/// Starts the container's server with `main` and the global `option` set,
/// so the client reads it at attach; then attaches.
fn attach_with(e: &E2e, local: &Local, option: &str, value: &str) -> Harness {
    let out = e.ssh(&format!(
        "grofe new-session -d -s main && grofe set-option -g {option} {value}"
    ));
    assert!(out.status.success(), "{out:?}");
    let mut h = Harness::spawn(local.grofe(&["-r", &e.host]), 120, 24);
    h.wait_for_timeout("$ ", REMOTE_TIMEOUT);
    // ponytail: the client reads the options on its first tick after the
    // attach and there is no signal for it, so give it a moment (as
    // tests/remote_forward_client.rs does).
    std::thread::sleep(Duration::from_millis(800));
    h
}

/// `GET /` from 127.0.0.1:`port`, the whole response.
fn http_get(port: u16) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to the forward");
    s.set_read_timeout(Some(REMOTE_TIMEOUT)).unwrap();
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut got = String::new();
    let _ = s.read_to_string(&mut got);
    got
}

#[test]
fn a_web_server_in_a_remote_pane_is_forwarded_to_localhost() {
    let e = e2e_or_skip!();
    let _cleanup = e.server_cleanup(false);
    let local = e.local();
    let mut h = attach_with(&e, &local, "remote-forward-ports", "auto");
    h.send(format!(
        "python3 -m http.server {WEB_PORT} --bind 127.0.0.1 --directory ~/www\r"
    ));
    // `grofe -r host ports` lists it once the client forwards it.
    let prefix = format!("{}:{WEB_PORT} -> localhost:", e.host);
    let deadline = Instant::now() + REMOTE_TIMEOUT;
    let listed = loop {
        let out = run(local.grofe(&["-r", &e.host, "ports"]));
        let listed = text(&out);
        if listed.starts_with(&prefix) {
            break listed;
        }
        assert!(Instant::now() < deadline, "never forwarded: {out:?}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let port: u16 = listed
        .lines()
        .next()
        .and_then(|l| l.strip_prefix(&prefix))
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("no local port in {listed:?}"));
    let page = http_get(port);
    assert!(
        page.contains("hello from the container web server"),
        "{page}"
    );
    h.send("\x03");
    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn a_pasted_local_file_uploads_to_the_pane_s_directory() {
    let e = e2e_or_skip!();
    let name = format!("e2e-upload-{}.txt", std::process::id());
    let _rm = Cleanup {
        e: &e,
        host: e.host.clone(),
        command: format!("rm -f ~/{name}"),
        netem: false,
    };
    let _cleanup = e.server_cleanup(false);
    let local = e.local();
    let body = format!("from the local side {}\n", std::process::id());
    let path = local.root().join(&name);
    std::fs::write(&path, &body).unwrap();
    let mut h = attach_with(&e, &local, "remote-upload-paste", "on");
    h.send("echo ");
    h.send(format!("\x1b[200~{}\x1b[201~", path.display()));
    // The pane's directory is the login's home; its remote path is typed in.
    let remote = format!("/home/tester/{name}");
    h.wait_for_timeout(&format!("echo '{remote}'"), REMOTE_TIMEOUT);
    h.send("\r");
    // The typed line and the echo's output.
    h.wait_until("the echo of the path", REMOTE_TIMEOUT, |t| {
        t.matches(remote.as_str()).count() >= 2
    });
    let out = e.ssh(&format!("cat {remote}"));
    assert_eq!(text(&out), body, "{out:?}");
    h.send("\x02d");
    h.wait_exit();
}
