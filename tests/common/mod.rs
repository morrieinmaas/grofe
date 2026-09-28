//! TestServer harness: every integration test drives a real `grofe` server
//! through this module (spec §9).
//!
//! Isolation. Each [`TestServer`] owns one tempdir holding:
//!
//! - `home/`: `HOME` (so `~/.config/grofe` and `~/.local/state` are the test's),
//! - `config/`: `XDG_CONFIG_HOME`,
//! - `state/`: `XDG_STATE_HOME` (the server log lands in `state/grofe/`,
//!   named by `grofe::paths::log_path`; use [`TestServer::log_path`]),
//! - `run/`: `XDG_RUNTIME_DIR`, and the unique `-S` socket `run/test.sock`.
//!
//! `GROFE` and `GROFE_PANE` are removed from the environment, so a test run
//! from inside a grofe pane never talks to (or targets) the real server.
//!
//! Server defaults. Until config exists (M3), the server reads two variables
//! at start, which the harness always sets (see `grofe::server::StartOptions`):
//!
//! - `GROFE_RESTORE_ON_START=0`: restore-on-start forced off. A no-op until
//!   M4, there so M4 can never restore the user's real sessions into a test.
//! - `GROFE_REMAIN_ON_EXIT=1`: tests run with `remain-on-exit` on (spec §4.2).
//!
//! Every wait is bounded, and dropping a `TestServer` (including while a test
//! panics) stops its server: `kill-server` first, then SIGKILL as a fallback.

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use grofe::event::Event;
use grofe::paths::{self, Env};
use grofe::protocol::{ClientCtx, Hello, Reply, Request, encode_hello};
use grofe::server::{START_MARKER, StartOptions};
use tempfile::TempDir;

/// How long a single CLI invocation may take before the test fails.
pub const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// The output of one bounded CLI run.
#[derive(Debug)]
pub struct CmdOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

pub struct TestServer {
    dir: TempDir,
    /// The server [`Self::start`] spawned, when this harness is its parent.
    server: Option<Child>,
    pub socket: PathBuf,
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub runtime_dir: PathBuf,
}

impl TestServer {
    /// Isolated dirs and a socket path, with no server started yet. For
    /// lifecycle tests that exercise auto-start themselves.
    pub fn unstarted() -> TestServer {
        let dir = tempfile::Builder::new()
            .prefix("grofe")
            .tempdir()
            .expect("create test tempdir");
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let config_home = root.join("config");
        let state_home = root.join("state");
        let runtime_dir = root.join("run");
        for d in [&home, &config_home, &state_home, &runtime_dir] {
            fs::create_dir_all(d).expect("create test dir");
        }
        TestServer {
            socket: runtime_dir.join("test.sock"),
            dir,
            server: None,
            home,
            config_home,
            state_home,
            runtime_dir,
        }
    }

    /// A running server on a unique `-S` socket, spawned as this process's
    /// own child so teardown can kill and reap it by pid. Panics, with the
    /// server log, if it does not come up.
    pub fn start() -> TestServer {
        let mut srv = TestServer::unstarted();
        let mut cmd = srv.command();
        cmd.arg("__server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        srv.server = Some(cmd.spawn().expect("spawn test server"));
        if wait_until(|| srv.is_running(), CMD_TIMEOUT).is_err() {
            panic!("test server failed to start; server log:\n{}", srv.log());
        }
        srv
    }

    /// A `grofe -S <socket>` command with the isolated environment applied.
    /// For tests that need custom stdio or spawning; prefer [`Self::cmd`].
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.arg("-S")
            .arg(&self.socket)
            .current_dir(&self.home)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env(StartOptions::RESTORE_ON_START_ENV, "0")
            .env(StartOptions::REMAIN_ON_EXIT_ENV, "1");
        cmd
    }

    /// Runs the one-shot CLI with `args`, bounded by [`CMD_TIMEOUT`].
    pub fn run(&self, args: &[&str]) -> CmdOutput {
        let mut cmd = self.command();
        cmd.args(args);
        run_bounded(cmd, CMD_TIMEOUT)
    }

    /// Runs the one-shot CLI: `Ok(stdout)` on exit status 0, else
    /// `Err(stderr)`.
    pub fn cmd(&self, args: &[&str]) -> Result<String, String> {
        let out = self.run(args);
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(out.stderr)
        }
    }

    /// `capture-pane -p -t <target>`, or an empty string on error, for use
    /// inside [`Self::wait_for`].
    pub fn capture(&self, target: &str) -> String {
        self.cmd(&["capture-pane", "-p", "-t", target])
            .unwrap_or_default()
    }

    /// Polls `cond` until it holds or `timeout` passes.
    pub fn wait_for(&self, cond: impl FnMut() -> bool, timeout: Duration) -> Result<(), String> {
        wait_until(cond, timeout)
    }

    /// Whether a server accepts connections on the socket right now.
    pub fn is_running(&self) -> bool {
        UnixStream::connect(&self.socket).is_ok()
    }

    /// Waits, without connecting, until a server logs that it is listening
    /// on the socket. Use this for an `--exit-when-empty` server: a probe
    /// connection is a client, and its close lets that server exit before
    /// the test's real connection arrives.
    pub fn wait_listening(&self, timeout: Duration) -> Result<(), String> {
        // The line `grofe::server::serve` logs once bind and listen are done.
        let line = format!("grofe server listening on {}", self.socket.display());
        wait_until(|| self.log().contains(&line), timeout)
    }

    /// The path-derivation env the server sees, for `grofe::paths`.
    pub fn paths_env(&self) -> Env {
        let s = |p: &Path| Some(p.display().to_string());
        Env {
            xdg_runtime_dir: s(&self.runtime_dir),
            tmpdir: None,
            xdg_state_home: s(&self.state_home),
            home: s(&self.home),
            uid: nix::unistd::geteuid().as_raw(),
        }
    }

    /// The server log path, derived by `grofe::paths::log_path` exactly as
    /// the server derives it (never re-implemented here).
    pub fn log_path(&self) -> PathBuf {
        paths::log_path(&self.paths_env(), &self.socket)
    }

    /// The server log's contents (empty if there is none yet).
    pub fn log(&self) -> String {
        fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    /// How many server processes have started on this socket.
    pub fn server_starts(&self) -> usize {
        self.log()
            .lines()
            .filter(|l| l.contains(START_MARKER))
            .count()
    }

    /// The client context the harness's own control connections send.
    pub fn ctx(&self) -> ClientCtx {
        ClientCtx {
            cwd: self.home.clone(),
            pane: None,
            env: vec![("HOME".to_string(), self.home.display().to_string())],
            client: None,
        }
    }

    /// Opens a control connection and sends `subscribe <events...>`.
    /// Returns once the server has accepted the subscription.
    pub fn subscribe(&self, events: &[&str]) -> Result<Subscription, String> {
        let mut argv = vec!["subscribe".to_string()];
        argv.extend(events.iter().map(|e| e.to_string()));
        let stream = UnixStream::connect(&self.socket).map_err(|e| format!("connect: {e}"))?;
        let mut sub = Subscription {
            reader: BufReader::new(stream.try_clone().map_err(|e| e.to_string())?),
            writer: stream,
        };
        let hello = encode_hello(&Hello::Control(self.ctx())).map_err(|e| e.to_string())?;
        let req = serde_json::to_string(&Request { id: 1, argv }).map_err(|e| e.to_string())?;
        writeln!(sub.writer, "{hello}\n{req}").map_err(|e| e.to_string())?;
        loop {
            match sub.read_reply(CMD_TIMEOUT)? {
                Reply::Ok { .. } => return Ok(sub),
                Reply::Err { err, .. } => return Err(err),
                Reply::Event(_) => continue,
            }
        }
    }
}

/// How long teardown waits for a server to go.
const TEARDOWN_WAIT: Duration = Duration::from_secs(5);

/// Stops the servers this harness started. The one [`Self::start`] spawned
/// is this process's child: it is killed by pid and reaped, whether or not
/// it still answers on its socket (a server unlinks its socket as the first
/// step of shutting down, so a live process can have none).
impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.kill();
            // Bounded, so a server that will not die cannot hang the test;
            // reaping it keeps it from lingering as a zombie.
            let _ = wait_until(|| !matches!(child.try_wait(), Ok(None)), TEARDOWN_WAIT);
        }
        // ponytail: servers a test started some other way (auto-start, `-L`)
        // are not our children, so they only get a polite `kill-server` over
        // their socket. One whose socket is already gone leaks. The upgrade
        // path is to have the harness spawn those as children too.
        for socket in self.sockets() {
            if kill_server_over(&socket, &self.ctx()) {
                let _ = wait_until(|| UnixStream::connect(&socket).is_err(), TEARDOWN_WAIT);
            }
        }
        let _ = fs::remove_file(&self.socket);
    }
}

impl TestServer {
    /// The `-S` socket plus any `-L` ones under this harness's runtime dir.
    fn sockets(&self) -> Vec<PathBuf> {
        let mut sockets = vec![self.socket.clone()];
        if let Ok(entries) = fs::read_dir(self.runtime_dir.join("grofe")) {
            sockets.extend(
                entries
                    .filter_map(|e| Some(e.ok()?.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "sock")),
            );
        }
        sockets
    }
}

/// Sends `kill-server` over a control connection to `socket`, waiting a
/// bounded time for the reply. Whether a server took the request.
fn kill_server_over(socket: &Path, ctx: &ClientCtx) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let Ok(hello) = encode_hello(&Hello::Control(ctx.clone())) else {
        return false;
    };
    let req = Request {
        id: 1,
        argv: vec!["kill-server".to_string()],
    };
    let Ok(req) = serde_json::to_string(&req) else {
        return false;
    };
    if writeln!(stream, "{hello}\n{req}").is_err() {
        return false;
    }
    let _ = stream.set_read_timeout(Some(TEARDOWN_WAIT));
    let mut line = String::new();
    let _ = BufReader::new(stream).read_line(&mut line);
    true
}

/// A control connection with an accepted subscription.
pub struct Subscription {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Subscription {
    /// The next pushed event, waiting at most `timeout`.
    pub fn next(&mut self, timeout: Duration) -> Result<Event, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out waiting for an event".to_string());
            }
            if let Reply::Event(ev) = self.read_reply(left)? {
                return Ok(ev);
            }
        }
    }

    fn read_reply(&mut self, timeout: Duration) -> Result<Reply, String> {
        // macOS refuses setsockopt with EINVAL once the peer has closed the
        // socket; what it sent before closing is still buffered, and reading
        // cannot block any more, so read it (then EOF) anyway.
        match self
            .reader
            .get_ref()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
        {
            Err(e) if e.raw_os_error() != Some(nix::libc::EINVAL) => return Err(e.to_string()),
            _ => {}
        }
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("server closed the connection".to_string()),
            Ok(_) => serde_json::from_str(&line).map_err(|e| format!("bad reply {line:?}: {e}")),
            Err(e) => Err(format!("reading reply: {e}")),
        }
    }
}

/// Whether process `pid` has exited. A zombie counts as gone: a detached
/// server is re-parented to PID 1, which in a container may never reap it,
/// so `kill(pid, 0)` alone would keep succeeding after it exited.
pub fn process_gone(pid: nix::unistd::Pid) -> bool {
    nix::sys::signal::kill(pid, None).is_err() || is_zombie(pid)
}

/// Linux: the state field of `/proc/<pid>/stat` (after the `(comm)`) is `Z`.
#[cfg(target_os = "linux")]
fn is_zombie(pid: nix::unistd::Pid) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
    })
}

/// Elsewhere (macOS) launchd reaps orphans promptly, so `kill` is enough.
#[cfg(not(target_os = "linux"))]
fn is_zombie(_pid: nix::unistd::Pid) -> bool {
    false
}

/// Polls `cond` every 10 ms until it holds or `timeout` passes.
pub fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("condition not met within {timeout:?}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Runs `cmd` with piped stdout/stderr and null stdin; panics if it does not
/// exit, or its output pipes do not reach EOF, within `timeout`.
pub fn run_bounded(mut cmd: Command, timeout: Duration) -> CmdOutput {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grofe");
    let stdout = drain(child.stdout.take().expect("stdout"));
    let stderr = drain(child.stderr.take().expect("stderr"));
    let status = wait_child(child, timeout).expect("grofe CLI did not exit in time");
    let left = deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100);
    let stdout = stdout
        .recv_timeout(left)
        .expect("grofe CLI stdout never reached EOF (leaked to a child?)");
    let stderr = stderr
        .recv_timeout(left)
        .expect("grofe CLI stderr never reached EOF (leaked to a child?)");
    CmdOutput {
        status,
        stdout,
        stderr,
    }
}

/// Reads `r` to EOF on a thread; the result arrives on the returned channel.
fn drain(mut r: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Waits for `child` to exit within `timeout`, killing it otherwise.
pub fn wait_child(mut child: Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("process did not exit within {timeout:?}"));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Whether `path` exists (as any file type, including a socket).
pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
