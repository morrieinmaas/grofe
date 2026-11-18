//! The roaming transport (spec ssh-remote §4): `grofe proxy --quic` as a
//! child on a private socket, reached over loopback UDP with the library's
//! client.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use common::{TestServer, process_gone, wait_child, wait_until};
use grofe::protocol::{Hello, Reply, Request, encode_hello};
use grofe::remote::proxy_proto::{Channel, OpenSpec, ProxyConn};
use grofe::remote::quic::server::proxy_files;
use grofe::remote::quic::{self, Bootstrap, PORTS_ENV, QuicLink};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const WAIT: Duration = Duration::from_secs(10);
/// Kept away from the default range, so a test never takes a port a real
/// proxy would.
const PORTS: &str = "47100-47999";

/// The detached proxy, killed by its pid when the test ends.
struct Proxy {
    boot: Bootstrap,
    pid: Pid,
    pid_file: PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = kill(self.pid, Signal::SIGTERM);
    }
}

/// Runs `grofe proxy --quic`, which must print the bootstrap line and exit
/// with its stdout closed (the detached process holds none of it), as ssh
/// needs; the pid file names the detached process.
fn start(srv: &TestServer) -> Proxy {
    let mut cmd = srv.command();
    cmd.args(["proxy", "--quic"])
        .env(PORTS_ENV, PORTS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let mut err = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = stderr.read_to_string(&mut err);
        let _ = tx.send((out, err));
    });
    let status = wait_child(child, WAIT).unwrap();
    let (out, err) = rx
        .recv_timeout(WAIT)
        .expect("stdout and stderr close once the foreground process exits");
    assert!(status.success(), "{status:?}: {err}");
    assert_eq!(out.lines().count(), 1, "exactly one line: {out:?}");
    let boot = quic::parse_bootstrap(&out).unwrap();
    assert!((47100..=47999).contains(&boot.port), "{}", boot.port);
    let (_, pid_file) = proxy_files(&srv.log_path(), boot.port);
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    Proxy {
        boot,
        pid: Pid::from_raw(pid),
        pid_file,
    }
}

fn link(boot: &Bootstrap) -> (QuicLink, ProxyConn) {
    let mut link = quic::connect("127.0.0.1", boot).unwrap();
    let (r, w) = link.streams().unwrap();
    let conn = ProxyConn::new(r, w).unwrap();
    (link, conn)
}

/// A control channel with its hello sent.
fn control(srv: &TestServer, conn: &ProxyConn) -> Channel {
    let mut chan = conn.open(&OpenSpec::Control).unwrap();
    let hello = encode_hello(&Hello::Control(srv.ctx())).unwrap();
    writeln!(chan, "{hello}").unwrap();
    chan
}

/// Asks for the session names on `chan`; the reply's output.
fn sessions(chan: &mut Channel, reader: &mut BufReader<Channel>, id: u64) -> String {
    let req = serde_json::to_string(&Request {
        id,
        argv: ["list-sessions", "-F", "#{session_name}"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    })
    .unwrap();
    writeln!(chan, "{req}").unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    match serde_json::from_str::<Reply>(&line).unwrap() {
        Reply::Ok { out, .. } => out,
        other => panic!("{other:?}"),
    }
}

fn session_server() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "roam"]).unwrap();
    srv
}

#[test]
fn a_control_channel_runs_a_command_over_quic() {
    let srv = session_server();
    let proxy = start(&srv);
    let (_link, conn) = link(&proxy.boot);
    let mut chan = control(&srv, &conn);
    let mut reader = BufReader::new(chan.clone());
    assert!(sessions(&mut chan, &mut reader, 1).contains("roam"));
    assert!(conn.ping(WAIT).unwrap() < WAIT);
}

#[test]
fn a_wrong_token_is_refused_and_the_right_one_still_gets_in() {
    let srv = session_server();
    let proxy = start(&srv);
    let mut wrong = proxy.boot.clone();
    wrong.token[0] ^= 1;
    let mut bad = quic::connect("127.0.0.1", &wrong).unwrap();
    let (r, w) = bad.streams().unwrap();
    let err = ProxyConn::new(r, w).err().expect("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");

    let (_link, conn) = link(&proxy.boot);
    let mut chan = control(&srv, &conn);
    let mut reader = BufReader::new(chan.clone());
    assert!(sessions(&mut chan, &mut reader, 1).contains("roam"));
}

#[test]
fn a_wrong_certificate_hash_is_refused() {
    let srv = session_server();
    let proxy = start(&srv);
    let mut wrong = proxy.boot.clone();
    wrong.cert_sha256[5] ^= 0x40;
    let err = quic::connect("127.0.0.1", &wrong).err().expect("refused");
    assert_ne!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
    assert!(err.to_string().contains("handshake"), "{err}");
}

#[test]
fn a_rebound_client_keeps_its_channel() {
    let srv = session_server();
    let proxy = start(&srv);
    let (link, conn) = link(&proxy.boot);
    let mut chan = control(&srv, &conn);
    let mut reader = BufReader::new(chan.clone());
    assert!(sessions(&mut chan, &mut reader, 1).contains("roam"));
    let before = link.local_addr().unwrap();
    link.rebind().unwrap();
    assert_ne!(link.local_addr().unwrap(), before);
    // The same channel, over the new path.
    assert!(sessions(&mut chan, &mut reader, 2).contains("roam"));
    assert!(sessions(&mut chan, &mut reader, 3).contains("roam"));
}

#[test]
fn a_lost_client_comes_back_with_the_same_token() {
    let srv = session_server();
    let proxy = start(&srv);
    let (first, conn) = link(&proxy.boot);
    first.close();
    drop(first);
    wait_until("the old connection to end", WAIT, || conn.is_closed());

    let (_link, conn) = link(&proxy.boot);
    let mut chan = control(&srv, &conn);
    let mut reader = BufReader::new(chan.clone());
    assert!(sessions(&mut chan, &mut reader, 1).contains("roam"));
}

#[test]
fn a_new_client_replaces_the_current_one() {
    let srv = session_server();
    let proxy = start(&srv);
    let (_first, old) = link(&proxy.boot);
    let (_second, conn) = link(&proxy.boot);
    wait_until("the replaced connection to end", WAIT, || old.is_closed());
    let mut chan = control(&srv, &conn);
    let mut reader = BufReader::new(chan.clone());
    assert!(sessions(&mut chan, &mut reader, 1).contains("roam"));
}

#[test]
fn with_no_client_the_proxy_exits_after_remote_quic_idle() {
    let srv = session_server();
    let dir = srv.config_home.join("grofe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "remote-quic-idle = 1\n").unwrap();
    let proxy = start(&srv);
    // A client keeps it alive past the idle time.
    let (link, conn) = link(&proxy.boot);
    common::quiet_period(Duration::from_millis(1500));
    assert!(!process_gone(proxy.pid), "exited with a client connected");
    assert!(conn.ping(WAIT).is_ok());
    link.close();
    drop(link);
    wait_until("the idle proxy to exit", WAIT, || process_gone(proxy.pid));
    assert!(!proxy.pid_file.exists(), "pid file left behind");
}

#[test]
fn a_new_proxy_stops_the_older_one_of_its_instance() {
    let srv = session_server();
    let old = start(&srv);
    let new = start(&srv);
    wait_until("the older proxy to stop", WAIT, || process_gone(old.pid));
    assert!(!old.pid_file.exists(), "the older pid file is left");
    assert!(new.pid_file.exists());
    assert!(!process_gone(new.pid));
    let (_link, conn) = link(&new.boot);
    let mut chan = control(&srv, &conn);
    let mut reader = BufReader::new(chan.clone());
    assert!(sessions(&mut chan, &mut reader, 1).contains("roam"));
}

#[test]
fn a_stale_pid_file_is_removed_and_its_pid_left_alone() {
    let srv = session_server();
    // A pid file no live proxy holds the lock on, naming a live process
    // that is not a proxy (as after a crash and pid reuse).
    let mut bystander = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let (_, stale) = proxy_files(&srv.log_path(), 1);
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, format!("{}\n", bystander.id())).unwrap();
    let _proxy = start(&srv);
    assert!(!stale.exists(), "the stale pid file is left");
    assert!(
        bystander.try_wait().unwrap().is_none(),
        "an unrelated pid was signalled"
    );
    let _ = bystander.kill();
    let _ = bystander.wait();
}

#[test]
fn proxies_started_at_once_leave_one() {
    let srv = session_server();
    let starts: Vec<_> = (0..4)
        .map(|_| {
            let mut cmd = srv.command();
            cmd.args(["proxy", "--quic"])
                .env(PORTS_ENV, PORTS)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            std::thread::spawn(move || cmd.output().unwrap())
        })
        .collect();
    let mut said = String::new();
    for start in starts {
        let out = start.join().unwrap();
        assert!(out.status.success(), "{out:?}");
        said.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    // Starts take turns, so each after the first stops the one before it,
    // which may be gone before it ever logs: the stops are counted from
    // what the starts said, the survivor from the one pid file left.
    let stopped: Vec<Pid> = said
        .lines()
        .filter_map(|l| {
            l.split("stopped the older proxy ")
                .nth(1)?
                .trim()
                .parse()
                .ok()
        })
        .map(Pid::from_raw)
        .collect();
    let (log, _) = proxy_files(&srv.log_path(), 0);
    let dir = log.parent().unwrap();
    let pid_files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".pid"))
        .map(|e| e.path())
        .collect();
    let survivor: Vec<Pid> = pid_files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok()?.trim().parse().ok())
        .map(Pid::from_raw)
        .collect();
    let _stop = StopEvery(stopped.iter().chain(&survivor).copied().collect());
    assert_eq!(stopped.len(), 3, "{said}");
    assert_eq!(pid_files.len(), 1, "{pid_files:?}");
    assert_eq!(survivor.len(), 1);
    assert!(!stopped.contains(&survivor[0]), "{stopped:?} {survivor:?}");
    wait_until("the stopped proxies to exit", WAIT, || {
        stopped.iter().all(|p| process_gone(*p))
    });
    assert!(!process_gone(survivor[0]));
}

/// SIGTERM to every pid when the test ends, passed or not.
struct StopEvery(Vec<Pid>);

impl Drop for StopEvery {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = kill(*pid, Signal::SIGTERM);
        }
    }
}
