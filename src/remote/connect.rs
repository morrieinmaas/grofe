//! The client's link to a remote `grofe proxy` (spec ssh-remote §1, §2):
//! a [`Transport`] makes the byte stream, a [`Link`] runs the proxy
//! protocol over it, opens attach and control channels, pings every 5 s
//! for the smoothed RTT, and notices a drop (EOF, the ssh process exiting,
//! two missed pings).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::proxy_proto::{Channel, OpenSpec, PROXY_VERSION, ProxyConn, ProxyHello};
use super::ssh::{self, MISSING_MARKER, MISSING_STATUS, SshOpts, Target};
use crate::protocol::{Reply, Request};

/// How often the client pings.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// Test hook: the ping interval in milliseconds, so a missed-ping drop
/// shows up in a test's time. Test-only.
pub const TEST_PING_ENV: &str = "GROFE_TEST_PING_MS";

/// Missed pings in a row that make a drop.
pub const MISSED_PINGS: u32 = 2;

/// What ends a link's transport; see [`Link::over`].
pub type End = Box<dyn FnOnce() + Send>;

/// How the bytes reach the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// `ssh host -- <remote command>`, the proxy on ssh's stdio.
    Ssh(Target),
    /// `ssh host -- <remote grofe> proxy --quic` once, then QUIC (see
    /// `super::roam`).
    Quic(Target),
}

impl Transport {
    /// The host, for messages.
    pub fn host(&self) -> &str {
        match self {
            Transport::Ssh(t) | Transport::Quic(t) => t.host_name(),
        }
    }
}

/// Why a link could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The remote has no grofe.
    Missing,
    /// The remote grofe cannot speak this proxy protocol.
    Incompatible(String),
    /// Anything else (ssh failed, ...), with what ssh said.
    Failed(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Missing => write!(f, "grofe is not installed on the remote"),
            ConnectError::Incompatible(why) => write!(f, "incompatible remote grofe: {why}"),
            ConnectError::Failed(why) => f.write_str(why),
        }
    }
}

/// Options for [`Link::connect`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectOpts {
    /// Never prompt (reconnects run with the terminal raw).
    pub batch: bool,
}

struct Shared {
    conn: ProxyConn,
    /// Ends the transport (kills the ssh child), run once at the drop.
    end: Mutex<Option<End>>,
    /// Smoothed RTT (RFC 6298's 1/8 gain).
    rtt: Mutex<Option<Duration>>,
    dropped: Mutex<Option<String>>,
    dropped_cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    /// Marks the link dropped (once) and ends the transport, so every
    /// channel and blocked write fails at once.
    fn drop_with(&self, why: &str) {
        {
            let mut d = lock(&self.dropped);
            if d.is_some() {
                return;
            }
            *d = Some(why.to_string());
        }
        if let Some(end) = lock(&self.end).take() {
            end();
        }
        self.dropped_cv.notify_all();
    }
}

/// One live connection to a remote proxy. Clones share it; dropping the
/// last one ends it.
#[derive(Clone)]
pub struct Link {
    shared: Arc<Shared>,
    host: String,
    _owner: Arc<Owner>,
}

/// Ends the link when the last [`Link`] clone goes.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.drop_with("closed");
    }
}

impl Link {
    /// Connects over `transport`, checking the proxy's hello.
    pub fn connect(transport: &Transport, opts: ConnectOpts) -> Result<Link, ConnectError> {
        match transport {
            Transport::Ssh(target) => connect_ssh(target, opts),
            Transport::Quic(target) => super::roam::connect_once(target, opts),
        }
    }

    /// A link over an already made byte stream (the ssh child's stdio,
    /// a pipe in tests, a QUIC stream pair). `end`, when given, runs once
    /// when the link drops or closes; it must end the stream (kill the
    /// process, close the socket) so blocked reads and writes fail.
    pub fn over<R, W>(
        host: &str,
        reader: R,
        writer: W,
        end: Option<End>,
    ) -> Result<Link, ConnectError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let conn = ProxyConn::new(reader, writer).map_err(|e| {
            let msg = e.to_string();
            if msg.starts_with("proxy protocol version") {
                ConnectError::Incompatible(format!(
                    "{msg}; the remote grofe speaks another proxy protocol than this one (version {PROXY_VERSION})"
                ))
            } else {
                ConnectError::Failed(msg)
            }
        })?;
        Ok(Link::start(host, conn, end))
    }

    fn start(host: &str, conn: ProxyConn, end: Option<End>) -> Link {
        let shared = Arc::new(Shared {
            conn,
            end: Mutex::new(end),
            rtt: Mutex::new(None),
            dropped: Mutex::new(None),
            dropped_cv: Condvar::new(),
        });
        let watch = Arc::clone(&shared);
        std::thread::spawn(move || {
            let why = watch.conn.wait_closed();
            watch.drop_with(&why);
        });
        let pinger = Arc::downgrade(&shared);
        std::thread::spawn(move || ping_loop(pinger, ping_interval()));
        Link {
            _owner: Arc::new(Owner(Arc::clone(&shared))),
            shared,
            host: host.to_string(),
        }
    }

    /// The host, for messages.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The proxy's hello (version, os, arch, features).
    pub fn peer(&self) -> &ProxyHello {
        self.shared.conn.peer()
    }

    /// Opens a channel as `spec` asks.
    pub fn open(&self, spec: &OpenSpec) -> io::Result<Channel> {
        self.shared.conn.open(spec)
    }

    /// A v1 attach connection to the remote server.
    pub fn open_attach(&self) -> io::Result<Channel> {
        self.open(&OpenSpec::Attach)
    }

    /// A v1 control connection to the remote server.
    pub fn open_control(&self) -> io::Result<Channel> {
        self.open(&OpenSpec::Control)
    }

    /// The underlying proxy connection (uploads, raw pings).
    pub fn conn(&self) -> &ProxyConn {
        &self.shared.conn
    }

    /// The smoothed round trip time, once a ping came back.
    pub fn rtt(&self) -> Option<Duration> {
        *lock(&self.shared.rtt)
    }

    /// Why the link dropped, if it has.
    pub fn dropped(&self) -> Option<String> {
        lock(&self.shared.dropped).clone()
    }

    /// Blocks until the link drops; why.
    pub fn wait_dropped(&self) -> String {
        let mut d = lock(&self.shared.dropped);
        loop {
            if let Some(why) = d.as_ref() {
                return why.clone();
            }
            d = self
                .shared
                .dropped_cv
                .wait(d)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Ends the link now.
    pub fn close(&self) {
        self.shared.drop_with("closed");
    }

    /// One control request on a fresh control channel: `hello`, then
    /// `argv`; the command's output, or its error.
    pub fn request(&self, hello: &str, argv: &[String]) -> Result<String, String> {
        let chan = self.open_control().map_err(|e| e.to_string())?;
        match request_on(chan, hello, argv)? {
            Reply::Ok { out, .. } => Ok(out),
            Reply::Err { err, .. } => Err(err),
            Reply::Event(_) => Err("unexpected event reply".to_string()),
        }
    }
}

/// Sends `hello` and one request for `argv` on `stream`, and reads until
/// its reply (events before it are skipped).
pub fn request_on<S: Read + Write>(
    mut stream: S,
    hello: &str,
    argv: &[String],
) -> Result<Reply, String> {
    let req = serde_json::to_string(&Request {
        id: 1,
        argv: argv.to_vec(),
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{hello}\n{req}\n").as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    read_reply(BufReader::new(stream))
}

/// Reads until a reply that is not an event.
pub fn read_reply<R: BufRead>(mut reader: R) -> Result<Reply, String> {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("server closed the connection without replying".to_string()),
            Ok(_) => {}
            Err(e) => return Err(format!("reading reply: {e}")),
        }
        match serde_json::from_str::<Reply>(&line) {
            Ok(Reply::Event(_)) => continue,
            Ok(reply) => return Ok(reply),
            Err(e) => return Err(format!("bad reply from server: {e}")),
        }
    }
}

fn ping_interval() -> Duration {
    std::env::var(TEST_PING_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(PING_INTERVAL, Duration::from_millis)
}

/// Pings every `every`; each ping waits at most `every`. Two misses in a
/// row drop the link. Ends with the link.
fn ping_loop(shared: std::sync::Weak<Shared>, every: Duration) {
    let mut missed = 0;
    loop {
        let started = std::time::Instant::now();
        let Some(s) = shared.upgrade() else { return };
        if lock(&s.dropped).is_some() {
            return;
        }
        match s.conn.ping(every) {
            Ok(sample) => {
                missed = 0;
                let mut rtt = lock(&s.rtt);
                *rtt = Some(match *rtt {
                    None => sample,
                    Some(old) => (old * 7 + sample) / 8,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                missed += 1;
                if missed >= MISSED_PINGS {
                    s.drop_with(&format!("no answer to {MISSED_PINGS} pings"));
                    return;
                }
            }
            Err(e) => {
                s.drop_with(&e.to_string());
                return;
            }
        }
        drop(s);
        if let Some(left) = every.checked_sub(started.elapsed()) {
            std::thread::sleep(left);
        }
    }
}

/// How long an unattended ([`ConnectOpts::batch`]) connect waits for the
/// proxy's hello once ssh runs.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// Test hook: [`HELLO_TIMEOUT`] in milliseconds. Test-only.
pub const TEST_HELLO_ENV: &str = "GROFE_TEST_HELLO_MS";

/// Why a connect whose ssh never brought a hello failed.
pub const NO_ANSWER: &str = "no answer from the remote proxy";

fn hello_timeout() -> Duration {
    std::env::var(TEST_HELLO_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(HELLO_TIMEOUT, Duration::from_millis)
}

/// Runs `ssh ... -- <proxy command>` and checks the hello.
fn connect_ssh(target: &Target, opts: ConnectOpts) -> Result<Link, ConnectError> {
    let (mut child, stdin, stdout, said) = spawn_ssh(target, opts)?;
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let hello = || Link::over(target.host_name(), stdout, stdin, None);
    // Only an unattended connect is bounded: an interactive one may be
    // waiting on a password, a host key question or a security key touch.
    let (linked, timed_out) = if opts.batch {
        within_hello_timeout(pid, hello)
    } else {
        (hello(), false)
    };
    match linked {
        Ok(link) if !timed_out => {
            *lock(&link.shared.end) = Some(kill_child(child));
            Ok(link)
        }
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(ConnectError::Failed(NO_ANSWER.to_string()))
        }
        Err(_) if timed_out => {
            let _ = child.wait();
            Err(ConnectError::Failed(NO_ANSWER.to_string()))
        }
        Err(e) => {
            // No hello: ssh or the remote command ended; say why.
            let status = exit_status(&mut child);
            let stderr = said.finish();
            Err(classify(e, status, &stderr))
        }
    }
}

/// Starts `ssh ... -- <proxy command>` with its stdio piped; its stdin and
/// stdout, and a thread keeping what it says on stderr.
fn spawn_ssh(
    target: &Target,
    opts: ConnectOpts,
) -> Result<(Child, ChildStdin, ChildStdout, Stderr), ConnectError> {
    let ssh_opts = SshOpts {
        keepalive: ssh::wants_keepalive(target),
        batch: opts.batch,
    };
    let remote = ssh::proxy_command(env!("CARGO_PKG_VERSION"));
    let mut child = ssh::command()
        .args(ssh::argv(target, ssh_opts, &remote))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ConnectError::Failed(format!("cannot run {}: {e}", ssh::program())))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ConnectError::Failed("ssh has no stdio".to_string()));
    };
    Ok((child, stdin, stdout, keep_stderr(stderr)))
}

/// Runs `hello` (the blocking hello exchange) with a bound: ssh may connect
/// and the remote never answer, so after [`hello_timeout`] the process
/// `pid` is killed, which ends the read. What `hello` gave, and whether
/// the bound was hit.
fn within_hello_timeout<T>(pid: nix::unistd::Pid, hello: impl FnOnce() -> T) -> (T, bool) {
    let (answered, wait) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let timed_out = matches!(
            wait.recv_timeout(hello_timeout()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        if timed_out {
            // Not reaped yet (only the caller waits for it), so the pid is
            // still this child's.
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
        }
        timed_out
    });
    let got = hello();
    let _ = answered.send(());
    (got, watchdog.join().unwrap_or(false))
}

/// The exit code of a child whose stdout ended: waited for briefly, then
/// killed (a child still running after a bad hello has no code).
fn exit_status(child: &mut Child) -> Option<i32> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(s)) => return s.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    child.wait().ok().and_then(|s| s.code())
}

/// An [`End`] that kills and reaps `child`.
pub fn kill_child(mut child: Child) -> End {
    Box::new(move || {
        let _ = child.kill();
        let _ = child.wait();
    })
}

/// What a connect that got no usable hello means, from the hello error,
/// ssh's exit status and its stderr.
pub fn classify(err: ConnectError, status: Option<i32>, stderr: &str) -> ConnectError {
    // Remote-controlled: shown on the terminal only once made safe.
    let stderr = &ssh::sanitize(stderr);
    if status == Some(MISSING_STATUS) && stderr.contains(MISSING_MARKER) {
        return ConnectError::Missing;
    }
    match err {
        ConnectError::Incompatible(_) => err,
        // The remote command ran and ended without a hello (ssh itself
        // fails with 255): a grofe too old to have `proxy`.
        ConnectError::Failed(_) if status.is_some_and(|s| s != 255 && s != 0) => {
            ConnectError::Incompatible(format!(
                "the remote grofe has no usable proxy ({})",
                stderr.trim()
            ))
        }
        ConnectError::Failed(why) => {
            let stderr = stderr.trim();
            ConnectError::Failed(if stderr.is_empty() {
                why
            } else {
                format!("{why}: {stderr}")
            })
        }
        ConnectError::Missing => err,
    }
}

/// The last bytes ssh wrote on stderr, kept by a thread.
struct Stderr {
    buf: Arc<Mutex<Vec<u8>>>,
    thread: std::thread::JoinHandle<()>,
}

/// Most stderr kept.
const STDERR_KEEP: usize = 16 * 1024;

// ponytail: ssh's stderr (banners, warnings) is kept for error messages,
// not shown: the terminal may be raw by the time it writes.
fn keep_stderr(mut stderr: impl Read + Send + 'static) -> Stderr {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&buf);
    let thread = std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut b = lock(&into);
            b.extend_from_slice(&chunk[..n]);
            if b.len() > STDERR_KEEP {
                let cut = b.len() - STDERR_KEEP;
                b.drain(..cut);
            }
        }
    });
    Stderr { buf, thread }
}

impl Stderr {
    /// Waits for ssh to close stderr; what it said.
    fn finish(self) -> String {
        let _ = self.thread.join();
        String::from_utf8_lossy(&lock(&self.buf)).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proxy_proto::{Mux, Role, encode_hello};
    use std::os::unix::net::UnixStream;

    /// A proxy end on a socketpair that answers pings (or not) and
    /// sends `hello` first.
    fn fake_proxy(hello: &ProxyHello) -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (&b).write_all(encode_hello(hello).as_bytes()).unwrap();
        (a, b)
    }

    #[test]
    fn a_peer_of_another_proxy_version_is_refused_clearly() {
        let mut hello = ProxyHello::current(&[]);
        hello.proxy = 2;
        let (a, _b) = fake_proxy(&hello);
        let err = Link::over("box", a.try_clone().unwrap(), a, None)
            .err()
            .unwrap();
        match err {
            ConnectError::Incompatible(why) => {
                assert!(why.contains("version 2"), "{why}");
                assert!(why.contains("version 1"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_link_measures_rtt_and_drops_on_eof() {
        let (a, b) = fake_proxy(&ProxyHello::current(&[]));
        let link = Link::over("box", a.try_clone().unwrap(), a, None).unwrap();
        let mut br = BufReader::new(b.try_clone().unwrap());
        let mut line = String::new();
        br.read_line(&mut line).unwrap();
        let closer = b.try_clone().unwrap();
        let _proxy = Mux::start(br, b, Role::Proxy, |c, _| c.close("no"));
        assert_eq!(link.peer().proxy, PROXY_VERSION);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while link.rtt().is_none() {
            assert!(std::time::Instant::now() < deadline, "no rtt");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(link.dropped().is_none());
        closer.shutdown(std::net::Shutdown::Both).unwrap();
        // The proxy end closing is EOF on the link.
        let why = link.wait_dropped();
        assert!(!why.is_empty());
    }

    #[test]
    fn missing_and_old_remotes_are_told_apart() {
        let failed = || ConnectError::Failed("connection closed before the proxy hello".into());
        assert_eq!(
            classify(failed(), Some(127), &format!("{MISSING_MARKER}\n")),
            ConnectError::Missing
        );
        assert!(matches!(
            classify(failed(), Some(1), "unknown command: proxy"),
            ConnectError::Incompatible(_)
        ));
        match classify(failed(), Some(1), "old\x1b]0;pwned\x07") {
            ConnectError::Incompatible(why) => assert!(!why.contains('\x1b'), "{why:?}"),
            other => panic!("{other:?}"),
        }
        match classify(failed(), Some(255), "ssh: Could not resolve hostname") {
            ConnectError::Failed(why) => assert!(why.contains("resolve"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
