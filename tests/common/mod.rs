//! TestServer harness: every integration test drives a real `grofe` server
//! through this module (spec §9).
//!
//! Isolation. Each [`TestServer`] owns one tempdir holding:
//!
//! - `home/`: `HOME` (so `~/.config/grofe` and `~/.local/state` are the test's),
//! - `config/`: `XDG_CONFIG_HOME`,
//! - `state/`: `XDG_STATE_HOME` (the server log lands in `state/grofe/`),
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
            home,
            config_home,
            state_home,
            runtime_dir,
        }
    }

    /// A running server on a unique `-S` socket (via `start-server`).
    /// Panics, with the server log, if it does not come up.
    pub fn start() -> TestServer {
        let srv = TestServer::unstarted();
        let out = srv.run(&["start-server"]);
        if !out.status.success() || !srv.is_running() {
            panic!(
                "test server failed to start: {out:?}\nserver log:\n{}",
                srv.log()
            );
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

    /// The server log path: `$XDG_STATE_HOME/grofe/<socket stem>.log`.
    pub fn log_path(&self) -> PathBuf {
        self.state_home.join("grofe").join("test.log")
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

impl Drop for TestServer {
    fn drop(&mut self) {
        if !self.is_running() {
            return;
        }
        let _ = self.run_quiet_kill();
        if wait_until(|| !self.is_running(), Duration::from_secs(3)).is_ok() {
            return;
        }
        // Fallback for a hung server: SIGKILL the latest logged pid. Only
        // reached while the socket still answers, so that pid is live.
        if let Some(pid) = last_logged_pid(&self.log()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = fs::remove_file(&self.socket);
    }
}

impl TestServer {
    fn run_quiet_kill(&self) -> Option<ExitStatus> {
        let mut cmd = self.command();
        cmd.arg("kill-server");
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        wait_child(child, Duration::from_secs(3)).ok()
    }
}

/// The pid from the last start marker line, `grofe server starting: pid N, ...`.
fn last_logged_pid(log: &str) -> Option<i32> {
    let line = log.lines().rev().find(|l| l.contains(START_MARKER))?;
    let rest = line.split("pid ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
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
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("server closed the connection".to_string()),
            Ok(_) => serde_json::from_str(&line).map_err(|e| format!("bad reply {line:?}: {e}")),
            Err(e) => Err(format!("reading reply: {e}")),
        }
    }
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
