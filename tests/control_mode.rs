//! tmux control mode (`-C`, docs/specs/2026-11-15-control-mode.md): the same
//! command script runs against real tmux 3.7b and against grofe through a
//! `tmux` link, and the two transcripts must match once the things that
//! differ by run (times, command numbers, output chunking) are normalised.
//!
//! Real tmux is `GROFE_REAL_TMUX` or `/opt/nanobrew/prefix/bin/tmux`, always
//! run with `-L` on a private `TMUX_TMPDIR` and `-f /dev/null`; the parity
//! test is skipped when it is not there. Nothing here touches a default
//! tmux server.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::TestServer;

/// What a transcript is compared on: block times and numbers are dropped,
/// adjacent `%output` lines of one pane merge (tmux splits them by read).
fn normalise(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
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
            out.push(line.to_string());
            continue;
        }
        out.push(line.to_string());
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[test]
fn normalise_drops_times_and_merges_output() {
    let raw = "%begin 1794733318 285 0\n%end 1794733318 285 0\n\
               %output %0 hi\\015\n%output %0 \\012\n%output %1 x\n%output %0 y\n\
               %begin 5 6 1\nparse error: x\n%error 5 6 1\n";
    assert_eq!(
        normalise(raw),
        "%begin T N 0\n%end T N 0\n%output %0 hi\\015\\012\n%output %1 x\n%output %0 y\n\
         %begin T N 1\nparse error: x\n%error T N 1\n"
    );
}

/// One server (real tmux or grofe) and how to start a control client on it.
struct Side {
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
    fn command(&self) -> Command {
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

    fn run(&self, args: &[&str]) {
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
    }
}

fn real_tmux() -> Option<PathBuf> {
    let path = std::env::var("GROFE_REAL_TMUX")
        .unwrap_or_else(|_| "/opt/nanobrew/prefix/bin/tmux".to_string());
    Path::new(&path).exists().then(|| PathBuf::from(path))
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

fn real_side(real: &Path) -> Side {
    let dir = short_dir("real");
    let tmpdir = dir.join("tt");
    fs::create_dir_all(&tmpdir).expect("tmpdir");
    Side {
        link: real.to_path_buf(),
        pre: ["-L", &format!("gcm-{}", unique()), "-f", "/dev/null"]
            .map(String::from)
            .to_vec(),
        envs: vec![("TMUX_TMPDIR".into(), tmpdir.display().to_string())],
        dir,
        _srv: None,
        real: true,
    }
}

fn grofe_side() -> Side {
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
struct Client {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: String,
}

impl Client {
    fn start(side: &Side, args: &[&str]) -> Client {
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
    fn settle(&mut self, settle: Duration) {
        while let Ok(line) = self.lines.recv_timeout(settle) {
            self.seen.push_str(&line);
            self.seen.push('\n');
        }
    }

    /// Sends one command line and waits for its block, then for the
    /// notifications it caused.
    fn send(&mut self, line: &str) {
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
    fn finish(mut self) -> String {
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

fn session(side: &Side) {
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

/// The commands both servers get, one line each, and the transcript's text.
fn run_script(side: &Side, script: &[&str]) -> String {
    session(side);
    let mut client = Client::start(side, &["-C", "attach", "-t", "main"]);
    client.settle(Duration::from_millis(500));
    for line in script {
        client.send(line);
    }
    client.finish()
}

const SCRIPT: &[&str] = &[
    "list-windows -F '#{window_id} #{window_name} #{window_layout}'",
    "new-window -d -n w2 'sleep 999'",
    "split-window -d -h -t @0 'sleep 998'",
    "select-window -t @1",
    "select-window -t @0",
    "rename-window -t @1 renamed",
    "send-keys -t %0 hi Enter",
    "bogus",
    "new-session -d -s two 'sleep 997'",
    "switch-client -t two",
    "kill-window -t @1",
    "kill-session -t two",
];

/// Both servers run `script`; the normalised transcripts must be equal.
fn assert_parity(script: &[&str]) {
    let Some(real) = real_tmux() else {
        eprintln!("skipped: no real tmux (set GROFE_REAL_TMUX)");
        return;
    };
    let real = real_side(&real);
    let want = normalise(&run_script(&real, script));
    let grofe = grofe_side();
    let got = normalise(&run_script(&grofe, script));
    assert_eq!(got, want, "grofe's transcript differs from tmux's");
}

#[test]
fn grofe_matches_tmux_on_the_command_script() {
    assert_parity(SCRIPT);
}

/// Panes, sizing, and the flow control of `refresh-client`.
const FLOW_SCRIPT: &[&str] = &[
    "split-window -h -t @0 cat",
    "select-pane -t %0",
    "kill-pane -t %1",
    "refresh-client -C 100x30",
    "refresh-client -f pause-after=1",
    "send-keys -t %0 abc Enter",
    "refresh-client -A '%0:pause'",
    "send-keys -t %0 def Enter",
    "refresh-client -A '%0:continue'",
    "send-keys -t %0 DEF Enter",
    "refresh-client -A '%0:off'",
    "send-keys -t %0 ghi Enter",
    "refresh-client -A '%0:on'",
    "refresh-client -f no-output",
    "send-keys -t %0 jkl Enter",
    "refresh-client -f '!no-output'",
    "send-keys -t %0 JKL Enter",
    "detach-client",
];

#[test]
fn grofe_matches_tmux_on_panes_sizing_and_flow_control() {
    assert_parity(FLOW_SCRIPT);
}

/// The shape alone, with no real tmux: the attach block, the session, the
/// command blocks, and `%exit` at EOF.
#[test]
fn grofe_speaks_the_framing() {
    let grofe = grofe_side();
    let text = normalise(&run_script(
        &grofe,
        &["list-windows -F '#{window_id}'", "bogus"],
    ));
    let want = "%begin T N 0\n%end T N 0\n%session-changed $0 main\n\
                %begin T N 1\n@0\n%end T N 1\n\
                %begin T N 1\nparse error: unknown command: bogus\n%error T N 1\n%exit\n";
    assert_eq!(text, want);
}

/// `-C new-session` creates the session it attaches to: the same
/// transcript, from an empty server.
#[test]
fn grofe_matches_tmux_for_new_session() {
    let Some(real) = real_tmux() else {
        eprintln!("skipped: no real tmux (set GROFE_REAL_TMUX)");
        return;
    };
    let run = |side: &Side| {
        let mut client = Client::start(side, &["-C", "new-session", "-s", "x", "-n", "w0", "cat"]);
        client.settle(Duration::from_millis(600));
        client.send("list-windows -F '#{window_id} #{window_name}'");
        client.send("detach-client");
        normalise(&client.finish())
    };
    let want = run(&real_side(&real));
    assert!(
        want.contains("%window-add @0\n%sessions-changed\n"),
        "{want}"
    );
    assert_eq!(run(&grofe_side()), want);
}

/// With no session to attach to, the attach block is an error and the
/// client leaves.
#[test]
fn grofe_matches_tmux_when_the_target_does_not_exist() {
    let Some(real) = real_tmux() else {
        eprintln!("skipped: no real tmux (set GROFE_REAL_TMUX)");
        return;
    };
    let run = |side: &Side| {
        session(side);
        normalise(&Client::start(side, &["-C", "attach", "-t", "nosuch"]).finish())
    };
    let want = run(&real_side(&real));
    assert!(want.starts_with("%begin T N 0\n"), "{want}");
    assert!(want.ends_with("%error T N 0\n%exit\n"), "{want}");
    assert_eq!(run(&grofe_side()), want);
}

/// `-CC` on a terminal: the stream is a DCS, `ESC P 1000 p` first and ST
/// after `%exit`, and the terminal does not echo.
#[test]
fn double_c_wraps_the_stream_in_a_dcs() {
    use nix::pty::openpty;
    use std::io::Read;
    use std::os::fd::AsFd;

    let grofe = grofe_side();
    session(&grofe);
    let pty = openpty(None, None).expect("openpty");
    let slave = |fd: &std::os::fd::OwnedFd| Stdio::from(fd.as_fd().try_clone_to_owned().unwrap());
    let mut child = grofe
        .command()
        .args(["-CC", "attach", "-t", "main"])
        .stdin(slave(&pty.slave))
        .stdout(slave(&pty.slave))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn -CC");
    drop(pty.slave);
    let mut master = fs::File::from(pty.master);
    std::thread::sleep(Duration::from_millis(600));
    master.write_all(b"detach-client\n").expect("write");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && !got.ends_with(b"\x1b\\") {
        match master.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    let _ = child.wait();
    let text = String::from_utf8_lossy(&got).replace("\r\n", "\n");
    assert!(text.starts_with("\x1bP1000p%begin "), "{text:?}");
    assert!(text.ends_with("%exit\n\x1b\\"), "{text:?}");
    assert!(
        !text.contains("detach-client"),
        "the terminal echoed: {text:?}"
    );
}

/// A new pane's first output never precedes the `%window-add` that
/// introduces it, and a pane's `%output` is escaped as tmux does.
#[test]
fn output_follows_the_window_add_of_its_pane() {
    let grofe = grofe_side();
    let mut client = Client::start(&grofe, &["-C", "new-session", "-s", "x", "cat"]);
    client.settle(Duration::from_millis(500));
    client.send("new-window -d 'printf \"zzmark\\\\\\\\\\n\"; sleep 30'");
    let text = client.finish();
    let add = text.find("%window-add @1").expect(&text);
    let out = text
        .lines()
        .position(|l| l.starts_with("%output ") && l.contains("zzmark"));
    let out = out.expect(&text);
    let add_line = text[..add].lines().count();
    assert!(add_line < out, "output before its window-add:\n{text}");
    assert!(text.contains("zzmark\\134\\015\\012"), "{text}");
}
