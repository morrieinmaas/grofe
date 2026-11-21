//! Running a control client on one server (real tmux or grofe) and
//! normalising what it prints.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::common::TestServer;

/// `tests/fixtures/compat`, written `{fixtures}` in scripts and goldens.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compat")
}

/// What a transcript is compared on: block times and numbers are dropped,
/// adjacent `%output` lines of one pane merge (tmux splits them by read),
/// the fixtures path is `{fixtures}` and the client `client` is `CLIENT`.
pub fn normalise(raw: &str, client: Option<&str>) -> String {
    let fixtures = fixtures_dir().display().to_string();
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut line = line.trim_end_matches('\r').replace(&fixtures, "{fixtures}");
        if let Some(name) = client {
            line = line.replace(name, "CLIENT");
        }
        let line = line.as_str();
        let mut words = line.splitn(4, ' ');
        let first = words.next().unwrap_or_default();
        if matches!(first, "%begin" | "%end" | "%error") {
            let flags = line.rsplit(' ').next().unwrap_or_default();
            out.push(format!("{first} T N {flags}"));
            continue;
        }
        // `%extended-output %p <age> : <data>`: the age is timing, so it goes.
        let extended = line.strip_prefix("%extended-output ").and_then(|rest| {
            let (pane, rest) = rest.split_once(' ')?;
            let (_age, data) = rest.split_once(" : ")?;
            Some((pane.to_string(), data.to_string()))
        });
        if let Some((pane, data)) = extended {
            let head = format!("%extended-output {pane} A : ");
            match out.last_mut() {
                Some(last) if last.starts_with(&head) => last.push_str(&data),
                _ => out.push(format!("{head}{data}")),
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("%output ")
            && let Some((pane, data)) = rest.split_once(' ')
        {
            let head = format!("%output {pane} ");
            if let Some(last) = out.last_mut()
                && last.starts_with(&head)
            {
                last.push_str(data);
                continue;
            }
        }
        out.push(line.to_string());
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// One server (real tmux or grofe) and how to start a client on it.
pub struct Side {
    dir: PathBuf,
    link: PathBuf,
    /// The arguments before the command: `-S sock` or `-L name -f /dev/null`.
    pre: Vec<String>,
    envs: Vec<(String, String)>,
    // Kept alive: dropping it stops grofe's server.
    _srv: Option<TestServer>,
    real: bool,
}

impl Side {
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.link);
        cmd.args(&self.pre)
            .current_dir(&self.dir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("GROFE")
            .env_remove("GROFE_PANE");
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd
    }

    pub fn run(&self, args: &[&str]) {
        let out = self
            .command()
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("run {args:?}: {e}"));
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

impl Drop for Side {
    fn drop(&mut self) {
        if self.real {
            let _ = self.command().arg("kill-server").output();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A short name unique to this process and call (socket paths are short).
fn unique() -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn short_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/gcm-{}-{tag}", unique()));
    fs::create_dir_all(&dir).expect("create dir");
    dir
}

/// Real tmux at `real`, on a private `-L` socket and `TMUX_TMPDIR`, with
/// no config.
pub fn real_side(real: &Path) -> Side {
    let dir = short_dir("real");
    let tmpdir = dir.join("tt");
    fs::create_dir_all(&tmpdir).expect("tmpdir");
    Side {
        link: real.to_path_buf(),
        pre: ["-L", &format!("gcm-{}", unique()), "-f", "/dev/null"]
            .map(String::from)
            .to_vec(),
        envs: vec![
            ("TMUX_TMPDIR".into(), tmpdir.display().to_string()),
            ("SHELL".into(), "/bin/sh".into()),
            ("TERM".into(), "xterm-256color".into()),
        ],
        dir,
        _srv: None,
        real: true,
    }
}

/// grofe's test server, driven through a `tmux` link.
pub fn grofe_side() -> Side {
    let srv = TestServer::start();
    let dir = short_dir("grofe");
    let link = dir.join("tmux");
    if !link.exists() {
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).expect("tmux link");
    }
    let envs = [
        ("HOME", srv.home.clone()),
        ("XDG_CONFIG_HOME", srv.config_home.clone()),
        ("XDG_STATE_HOME", srv.state_home.clone()),
        ("XDG_RUNTIME_DIR", srv.runtime_dir.clone()),
        ("TMUX_TMPDIR", dir.join("tt")),
    ]
    .map(|(k, v)| (k.to_string(), v.display().to_string()))
    .into_iter()
    .chain([
        ("SHELL".to_string(), "/bin/sh".to_string()),
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("GROFE_RESTORE_ON_START".to_string(), "0".to_string()),
    ])
    .collect();
    Side {
        link,
        pre: vec!["-S".into(), srv.socket.display().to_string()],
        envs,
        dir,
        _srv: Some(srv),
        real: false,
    }
}

/// A control client: its stdin, and what it prints, line by line.
pub struct Client {
    child: Child,
    lines: mpsc::Receiver<String>,
    pub seen: String,
}

impl Client {
    pub fn start(side: &Side, args: &[&str]) -> Client {
        let mut child = side
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn control client");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Client {
            child,
            lines,
            seen: String::new(),
        }
    }

    /// Reads lines until `done` says so, or `timeout`.
    fn read_until(&mut self, timeout: Duration, mut done: impl FnMut(&str) -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push_str(&line);
                    self.seen.push('\n');
                    if done(&line) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Reads whatever arrives until the client is quiet for `settle`.
    pub fn settle(&mut self, settle: Duration) {
        while let Ok(line) = self.lines.recv_timeout(settle) {
            self.seen.push_str(&line);
            self.seen.push('\n');
        }
    }

    /// Sends one command line and waits for its block, then for the
    /// notifications it caused.
    pub fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{line}").expect("write command");
        stdin.flush().expect("flush");
        let closed = self.read_until(Duration::from_secs(10), |l| {
            l.starts_with("%end ") || l.starts_with("%error ")
        });
        assert!(closed, "no block for {line:?}; saw:\n{}", self.seen);
        self.settle(Duration::from_millis(350));
    }

    /// Ends the input and reads to the end of the output.
    pub fn finish(mut self) -> String {
        drop(self.child.stdin.take());
        self.read_until(Duration::from_secs(10), |_| false);
        for _ in 0..100 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return self.seen;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!(
            "the control client did not exit at EOF; saw:\n{}",
            self.seen
        );
    }
}

/// An ordinary client attached on a pty of 80x25 (so its windows keep the
/// 80x24 of the test sessions), and its name as `list-clients` shows it
/// (the recorder's tmux runs in a container, with a tty of its own).
pub struct PtyClient {
    child: Child,
    pub name: String,
}

impl PtyClient {
    pub fn attach(side: &Side, session: &str) -> PtyClient {
        use nix::pty::{Winsize, openpty};
        let size = Winsize {
            ws_row: 25,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None).expect("openpty");
        let slave = || Stdio::from(pty.slave.as_fd().try_clone_to_owned().expect("dup"));
        let child = side
            .command()
            .args(["attach", "-t", session])
            .stdin(slave())
            .stdout(slave())
            .stderr(slave())
            .spawn()
            .expect("spawn pty client");
        drop(pty.slave);
        // Drain the terminal so the client never blocks writing to it.
        let mut master = fs::File::from(pty.master);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while matches!(master.read(&mut buf), Ok(n) if n > 0) {}
        });
        let mut client = PtyClient {
            child,
            name: String::new(),
        };
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            let out = side
                .command()
                .args(["list-clients", "-F", "#{client_name}"])
                .output()
                .expect("list-clients");
            if let Some(name) = String::from_utf8_lossy(&out.stdout).lines().next() {
                client.name = name.to_string();
                return client;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the pty client never attached");
    }
}

impl Drop for PtyClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A recorded control-mode case.
pub struct Case {
    pub name: &'static str,
    /// Commands run before the control client starts, after the `main`
    /// session (unless `start` makes its own).
    pub setup: &'static [&'static str],
    /// A session for an ordinary pty client to attach to before the control
    /// client starts; `{client}` in the script is its name.
    pub pty: Option<&'static str>,
    /// The control client's arguments.
    pub start: &'static [&'static str],
    /// One command line each; `!wait` waits 1.5 s (past one subscription
    /// check), `{fixtures}` is the fixtures directory.
    pub script: &'static [&'static str],
}

/// The `main` session every attaching case starts from: one `cat` pane,
/// names fixed.
pub fn session(side: &Side) {
    if !side.real {
        // The test server starts as `grofe __server`, whose windows and
        // panes count from 1; one the tmux front end starts counts from 0,
        // as tmux does.
        side.run(&["set-option", "-g", "base-index", "0"]);
        side.run(&["set-option", "-g", "pane-base-index", "0"]);
    }
    side.run(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "24",
        "-n",
        "w0",
        "cat",
    ]);
    side.run(&["set-option", "-g", "automatic-rename", "off"]);
    side.run(&["set-option", "-g", "allow-rename", "off"]);
}

/// Runs `case` on `side`; the normalised transcript.
pub fn run_case(side: &Side, case: &Case) -> String {
    let attaches = case.start.contains(&"attach");
    if attaches {
        session(side);
    }
    for line in case.setup {
        for argv in grofe::cmd::split_command_string(line).expect("setup line") {
            side.run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
    let pty = case.pty.map(|s| PtyClient::attach(side, s));
    let name = pty.as_ref().map(|p| p.name.clone());
    let mut client = Client::start(side, case.start);
    client.settle(Duration::from_millis(600));
    let fixtures = fixtures_dir().display().to_string();
    for line in case.script {
        if *line == "!wait" {
            client.settle(Duration::from_millis(1500));
            continue;
        }
        let mut line = line.replace("{fixtures}", &fixtures);
        if let Some(name) = &name {
            line = line.replace("{client}", name);
        }
        client.send(&line);
    }
    let text = client.finish();
    drop(pty);
    normalise(&text, name.as_deref())
}
