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
//! - `GROFE_RESTORE_ON_START=0`: restore-on-start forced off, so M4 can
//!   never restore the user's real sessions into a test. A server built with
//!   [`TestServer::builder`]`.restore_on_start(true)` goes without it and
//!   restores as its config says.
//! - `GROFE_REMAIN_ON_EXIT=1`: tests run with `remain-on-exit` on (spec §4.2).
//!
//! Frames (MF, a contract edit). grofe identity frames its panes by default
//! (spec 2026-11-05-frames.md), while the suites written before MF test
//! tmux-style shared borders (and tmux's full-width message bar). So every
//! harness writes [`FRAMES_PIN`], a `conf.d` file holding `pane-frames =
//! false` and `message.pill = false`, into its config dir: the
//! server reads it through the real config chain, after `config.toml`, so a
//! test's own `config.toml` cannot undo it. A harness built with
//! [`TestServerBuilder::frames`]`(true)` writes none and sees grofe's
//! built-in defaults (the frames_* suites).
//!
//! Persistence (M4, a contract edit): [`TestServer::snapshot_path`] and
//! [`TestServer::edit_snapshot`] reach this server's `state.json`;
//! [`TestServer::kill_hard`] crashes the server, [`TestServer::signal_all`]
//! signals it and every process below it as a reboot or logout does, and
//! [`TestServer::restart`] starts a new server on the same socket and dirs.
//!
//! Every wait is bounded, and dropping a `TestServer` (including while a test
//! panics) stops its server: `kill-server` first, then SIGKILL as a fallback,
//! then SIGKILL to whatever it started that still runs ([`stop_server`]).

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use grofe::event::Event;
use grofe::paths::{self, Env};
use grofe::protocol::{ClientCtx, Dialect, Hello, Reply, Request, encode_hello};
use grofe::server::{START_MARKER, StartOptions};
use tempfile::TempDir;

pub use nix::sys::signal::Signal;

/// The prompt a pane's shell shows (see [`TestServer::command`]).
pub const PROMPT: &str = "$ ";

/// The file `ENV` names for a pane's `sh`, in the test root: it sets [`PROMPT`].
const SHRC: &str = "shrc";

/// The `conf.d` file, relative to the config dir `grofe/`, that pins a
/// harness to frames off (module docs).
pub const FRAMES_PIN: &str = "conf.d/00-harness-frames-off.toml";

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
    /// Behind a lock so [`Self::kill_hard`] works through `&self`.
    server: Mutex<Option<Child>>,
    /// Processes that were below a server [`Self::kill_hard`] or
    /// [`Self::signal_all`] hit (pid and start time), which outlive it as
    /// orphans; teardown kills what still runs.
    orphans: Mutex<Vec<(i32, String)>>,
    /// Whether servers this harness starts may restore on start; off
    /// unless [`TestServerBuilder::restore_on_start`] turned it on.
    restore_on_start: bool,
    /// [`TestServerBuilder::path_env`]: the only `PATH` its commands see.
    path_env: Option<String>,
    pub socket: PathBuf,
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub runtime_dir: PathBuf,
}

impl TestServer {
    /// Isolated dirs and a socket path, with no server started yet. For
    /// lifecycle tests that exercise auto-start themselves. Pinned to
    /// frames off (module docs); [`TestServerBuilder::unstarted`] with
    /// [`TestServerBuilder::frames`] is the unpinned one.
    pub fn unstarted() -> TestServer {
        let srv = TestServer::bare();
        let pin = srv.config_home.join("grofe").join(FRAMES_PIN);
        if let Some(dir) = pin.parent() {
            fs::create_dir_all(dir).expect("create harness conf.d");
        }
        fs::write(&pin, "pane-frames = false\n[message]\npill = false\n")
            .expect("write the frames pin");
        srv
    }

    /// [`Self::unstarted`] without the frames pin.
    fn bare() -> TestServer {
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
        // Only a login shell (`-sh`) reads the profile; a shell started with
        // its own `PS1` keeps it.
        fs::write(
            root.join(SHRC),
            format!("case $0 in -*) PS1='{PROMPT}' ;; esac\n"),
        )
        .expect("write the shell's ENV file");
        TestServer {
            socket: runtime_dir.join("test.sock"),
            dir,
            server: Mutex::new(None),
            orphans: Mutex::new(Vec::new()),
            restore_on_start: false,
            path_env: None,
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
        TestServer::builder().start()
    }

    /// A server with persistence settings: restore on start, the `PATH`
    /// it starts under.
    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::default()
    }

    /// Spawns `grofe __server` as this process's child, in its own process
    /// group (so [`Self::kill_hard`] can kill the group), and waits for it.
    fn spawn_server(&mut self) {
        let mut cmd = self.command();
        cmd.arg("__server")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn test server");
        *self
            .server
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = Some(child);
        if wait_until(|| self.is_running(), CMD_TIMEOUT).is_err() {
            panic!("test server failed to start; server log:\n{}", self.log());
        }
    }

    /// This server's snapshot, `state.json` under its temp `XDG_STATE_HOME`,
    /// derived by `grofe::paths::snapshot_path` as the server derives it.
    pub fn snapshot_path(&self) -> PathBuf {
        paths::snapshot_path(&self.paths_env(), &self.socket)
    }

    /// Rewrites the snapshot on disk through `edit`, for mutation proofs.
    /// Panics when there is no snapshot or it is not JSON.
    pub fn edit_snapshot(&self, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = self.snapshot_path();
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read snapshot {}: {e}", path.display()));
        let mut value: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("snapshot {} is not JSON: {e}", path.display()));
        edit(&mut value);
        let text = serde_json::to_string_pretty(&value).expect("serialize snapshot");
        fs::write(&path, text).unwrap_or_else(|e| panic!("write snapshot {}: {e}", path.display()));
    }

    /// SIGKILLs the server and its process group, as a crash would, and
    /// reaps it. Pane processes are left to their hangup; teardown kills
    /// any that remain.
    pub fn kill_hard(&self) {
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;
        let mut server = self.server.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(mut child) = server.take() else {
            panic!("kill_hard: this harness started no server");
        };
        self.orphans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(descendants(child.id()));
        let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
        let _ = child.kill();
        if wait_until(|| !matches!(child.try_wait(), Ok(None)), TEARDOWN_WAIT).is_err() {
            panic!("kill_hard: the server did not die");
        }
    }

    /// Sends `signal` to the server and every process below it (its panes,
    /// jobs and plugins), as a reboot or logout does. Returns at once:
    /// the caller waits for what it expects to follow.
    pub fn signal_all(&self, signal: Signal) {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        let server = self.server.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(child) = server.as_ref() else {
            panic!("signal_all: this harness started no server");
        };
        let below = descendants(child.id());
        let _ = kill(Pid::from_raw(child.id() as i32), signal);
        for (pid, _) in &below {
            let _ = kill(Pid::from_raw(*pid), signal);
        }
        self.orphans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(below);
    }

    /// A new server on the same socket, state and config dirs, restoring
    /// as this harness was built to. A server still answering is stopped
    /// first with `kill-server`; one already going (after
    /// [`Self::signal_all`]) is waited for.
    pub fn restart(mut self) -> TestServer {
        let ctx = self.ctx();
        if let Some(mut child) = self
            .server
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let reaped = |child: &mut Child| !matches!(child.try_wait(), Ok(None));
            if self.is_running() || wait_until(|| reaped(&mut child), TEARDOWN_WAIT).is_err() {
                stop_server(&mut child, &self.socket, &ctx);
            }
        }
        // A crashed server leaves its socket file behind.
        if !self.is_running() {
            let _ = fs::remove_file(&self.socket);
        }
        self.spawn_server();
        self
    }

    /// A `grofe -S <socket>` command with the isolated environment applied.
    /// For tests that need custom stdio or spawning; prefer [`Self::cmd`].
    ///
    /// A server it starts hands its environment to panes, so a pane's
    /// shell prompts [`PROMPT`] whoever runs the tests: `sh` would print
    /// `# ` as root (a CI container) and `$ ` otherwise. `SHELL` is pinned to
    /// `sh` (the pane command strings run through `$SHELL -c`, and a bash there
    /// drops the exported `PS1`). A pane's shell is a login shell, and a
    /// system `/etc/profile` may set its own `PS1` (Debian's and Ubuntu's set
    /// `# ` for root), so `ENV` names a file that sets [`PROMPT`] again: an
    /// interactive `sh` reads it after the profile. Only a login shell's
    /// prompt is set there, so a test's own `PS1` for a plain `sh` stands.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        if self.path_env.is_some() {
            // `env -i PATH=<path>`, plus the harness's own variables below.
            cmd.env_clear();
        }
        cmd.arg("-S")
            .arg(&self.socket)
            .current_dir(&self.home)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("SHELL", "/bin/sh")
            .env("PS1", PROMPT)
            .env("ENV", self.dir.path().join(SHRC))
            .env(StartOptions::REMAIN_ON_EXIT_ENV, "1");
        if self.restore_on_start {
            cmd.env_remove(StartOptions::RESTORE_ON_START_ENV);
        } else {
            cmd.env(StartOptions::RESTORE_ON_START_ENV, "0");
        }
        if let Some(path) = &self.path_env {
            cmd.env("PATH", path);
        }
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
            dialect: Dialect::Grofe,
        }
    }

    /// Sends one request over a control connection, so its arguments can
    /// be larger than `exec` allows: `Ok(out)` or `Err(err)`.
    pub fn request(&self, argv: &[String], timeout: Duration) -> Result<String, String> {
        let stream = UnixStream::connect(&self.socket).map_err(|e| format!("connect: {e}"))?;
        let mut sub = Subscription {
            reader: BufReader::new(stream.try_clone().map_err(|e| e.to_string())?),
            writer: stream,
        };
        let hello = encode_hello(&Hello::Control(self.ctx())).map_err(|e| e.to_string())?;
        let req = serde_json::to_string(&Request {
            id: 1,
            argv: argv.to_vec(),
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
        writeln!(sub.writer, "{hello}\n{req}").map_err(|e| e.to_string())?;
        loop {
            match sub.read_reply(timeout)? {
                Reply::Ok { out, .. } => return Ok(out),
                Reply::Err { err, .. } => return Err(err),
                Reply::Event(_) => continue,
            }
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
        let req = serde_json::to_string(&Request {
            id: 1,
            argv,
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
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

/// [`TestServer::builder`]: a server with persistence settings.
#[derive(Default)]
pub struct TestServerBuilder {
    restore_on_start: bool,
    path_env: Option<String>,
    frames: bool,
}

impl TestServerBuilder {
    /// `true` lets the server restore on start as its config says (the
    /// harness otherwise forces it off with `GROFE_RESTORE_ON_START=0`),
    /// for this server and every one [`TestServer::restart`] starts.
    pub fn restore_on_start(mut self, on: bool) -> TestServerBuilder {
        self.restore_on_start = on;
        self
    }

    /// Runs the server, and every CLI the harness runs, as under
    /// `env -i PATH=<path>`: only the harness's own variables and this
    /// `PATH`.
    pub fn path_env(mut self, path: &str) -> TestServerBuilder {
        self.path_env = Some(path.to_string());
        self
    }

    /// `true` leaves out the frames pin (module docs), so the server keeps
    /// grofe's built-in `pane-frames` default (on under grofe identity) and
    /// whatever the test's own config says.
    pub fn frames(mut self, built_in: bool) -> TestServerBuilder {
        self.frames = built_in;
        self
    }

    /// Isolated dirs as this builder says, with no server started yet, as
    /// [`TestServer::unstarted`].
    pub fn unstarted(self) -> TestServer {
        let mut srv = if self.frames {
            TestServer::bare()
        } else {
            TestServer::unstarted()
        };
        srv.restore_on_start = self.restore_on_start;
        srv.path_env = self.path_env;
        srv
    }

    /// Starts the server, as [`TestServer::start`].
    pub fn start(self) -> TestServer {
        let mut srv = self.unstarted();
        srv.spawn_server();
        srv
    }
}

/// How long teardown waits for a server to go.
const TEARDOWN_WAIT: Duration = Duration::from_secs(5);

/// Stops the servers this harness started. The one [`Self::start`] spawned
/// is this process's child: [`stop_server`] ends it, and what it started,
/// whether or not it still answers on its socket (a server unlinks its
/// socket as the first step of shutting down, so a live process can have
/// none).
impl Drop for TestServer {
    fn drop(&mut self) {
        let server = self
            .server
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(mut child) = server.take() {
            stop_server(&mut child, &self.socket, &self.ctx());
        }
        kill_leftovers(
            self.orphans
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner),
        );
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

/// Stops `child`, a server this process spawned on `socket`, and what it
/// left running. `kill-server` first, so the server ends its jobs, plugins
/// and popups as on any exit; SIGKILL if it has not gone within the bound;
/// the reap is bounded too, so a server that will not die cannot hang the
/// test. Then, as a backstop, SIGKILL to every process that was below it
/// and still runs: a job that ignored the server's SIGTERM, or a job's own
/// child, which the server never signals.
pub fn stop_server(child: &mut Child, socket: &Path, ctx: &ClientCtx) {
    let below = descendants(child.id());
    let reaped = |child: &mut Child| !matches!(child.try_wait(), Ok(None));
    if kill_server_over(socket, ctx) {
        let _ = wait_until(|| reaped(child), TEARDOWN_WAIT);
    }
    let _ = child.kill();
    let _ = wait_until(|| reaped(child), TEARDOWN_WAIT);
    kill_leftovers(&below);
}

/// Every process below `root` (pid and start time), from `ps`; empty if
/// `ps` cannot say.
fn descendants(root: u32) -> Vec<(i32, String)> {
    let procs = processes();
    let mut found = Vec::new();
    let mut parents = vec![root as i32];
    while let Some(parent) = parents.pop() {
        for (pid, ppid, start) in &procs {
            if *ppid == parent && !found.iter().any(|(p, _)| p == pid) {
                found.push((*pid, start.clone()));
                parents.push(*pid);
            }
        }
    }
    found
}

/// Every process's pid, parent pid and start time.
fn processes() -> Vec<(i32, i32, String)> {
    let Ok(out) = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "lstart="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            Some((pid, ppid, fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// SIGKILL to each of `procs` still running. A pane's shell goes on its
/// own once its pty hangs up, so they get a moment first; a pid is only
/// signalled while its start time is still the one recorded, never a
/// process that reused it.
fn kill_leftovers(procs: &[(i32, String)]) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let alive = |pid: i32| kill(Pid::from_raw(pid), None).is_ok();
    if wait_until(|| procs.iter().all(|(pid, _)| !alive(*pid)), LEFTOVER_GRACE).is_ok() {
        return;
    }
    for (pid, _, start) in processes() {
        if procs.iter().any(|(p, s)| *p == pid && *s == start) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

/// How long [`kill_leftovers`] lets processes end on their own.
const LEFTOVER_GRACE: Duration = Duration::from_millis(300);

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
        ..Default::default()
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
    let program = Path::new(cmd.get_program())
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
    let stdout = drain(child.stdout.take().expect("stdout"));
    let stderr = drain(child.stderr.take().expect("stderr"));
    let status = wait_child(child, timeout)
        .unwrap_or_else(|e| panic!("{program} did not exit in time: {e}"));
    let left = deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100);
    let stdout = stdout
        .recv_timeout(left)
        .unwrap_or_else(|_| panic!("{program} stdout never reached EOF (leaked to a child?)"));
    let stderr = stderr
        .recv_timeout(left)
        .unwrap_or_else(|_| panic!("{program} stderr never reached EOF (leaked to a child?)"));
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
                // Reaped if it goes within the teardown bound; a process
                // stuck in its exit (a tty that never drains) is left.
                let _ = child.kill();
                let _ = wait_until(|| !matches!(child.try_wait(), Ok(None)), TEARDOWN_WAIT);
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
