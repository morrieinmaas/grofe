//! The scenario replayer, shared by both sides of the compat comparison
//! (spec §9): the golden recorder runs scenarios against real tmux 3.7b, and
//! compat tests run the same scenarios against grofe and compare with the
//! recorded golden. Everything that decides what a transcript looks like
//! lives here, so the two sides cannot drift apart.
//!
//! A [`Cli`] says how to run one command (program, argv prefix, the whole
//! environment, cwd) and which values normalization must replace; [`replay`]
//! runs a scenario's [`Step`]s through it and returns a [`Transcript`].
//!
//! Scenario lines (blank lines and `#` comments are skipped):
//!
//! - `$ <args>`: run `<program> <prefix> <args>`; the args are split like a
//!   POSIX shell would (single quotes, double quotes, backslash). Recorded as
//!   the `$` line, stdout verbatim, each stderr line as `! <line>`, and
//!   `? <exit status>`.
//! - `$& <args>`: start the command and go on without waiting, for commands
//!   that block until something closes (`display-popup`, `display-menu`).
//!   Recorded as the `$&` line only; the command is killed at the end.
//! - `client <cols>x<rows> <args>`: start `<args>` (normally `attach -t
//!   <session>`) on a pty of that size, as the scenario's one attached client,
//!   and wait until `list-clients` shows it.
//! - `screen [<text>]`: wait until the client's screen shows `<text>` (if
//!   given) and its output has settled, then record the screen, each row as
//!   `| <row>` with trailing blanks trimmed. Give `<text>` whenever the screen
//!   is waiting on something a step started, or the capture races it.
//! - `styles <first>-<last> [<text>]`: as `screen`, but record how rows
//!   `<first>` to `<last>` look rather than what they say: each run of cells
//!   with the same non-default colours or attributes as `~ <row>
//!   <col>-<col> fg=<colour> bg=<colour> <attrs>`. A colour is a name,
//!   `idx:<n>` or `rgb:<rr>/<gg>/<bb>`, so no digit run is long enough for
//!   the pid rule to read. A blank's foreground, bold, dim and italic are
//!   left out, since they draw nothing.
//! - `keys <key>...`: type keys on the client's pty, as a person would, by
//!   tmux key name: `C-<c>`, `C-Space`, `Space`, `Escape`, `Enter`, `Tab`,
//!   `BSpace`, the arrows (also `C-` and `M-`), `S-Up`, `S-Down`, `C-Left`, `C-Right`, `M-<char>`, `Home`, `End`, `PPage`, `NPage`, or a word typed
//!   as it is, then wait a moment, so an `Escape` is not
//!   read as Meta with the next step's key. Recorded as the `keys` line
//!   only; follow it with `screen <text>` to see what the keys did, and
//!   before typing into a menu a key opened.
//!
//! In `$`, `$&` and `client` args, `{client}` expands to the client's tty
//! (for `-c`) and `{fixtures}` to `tests/fixtures/compat` (for `source-file`).
//! The golden keeps the unexpanded line, so it reads the same on both sides.

use std::fs::{self, File};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, test::TermSize};
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::normalize::{Context, Normalizer, fixtures_dir};

/// How long one command, or a screen settling, may take.
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// A client's screen counts as settled after this long without output.
const SCREEN_QUIET: Duration = Duration::from_millis(500);

/// How long a `keys` step waits after typing: well past the recorder's
/// escape-time (25 ms, `tmux-defaults.conf`).
const KEYS_PAUSE: Duration = Duration::from_millis(200);

/// How to run one command against the server under test.
#[derive(Debug, Clone)]
pub struct Cli {
    /// Absolute path of the binary (real tmux, or a `tmux` symlink to grofe).
    pub program: PathBuf,
    /// Args before every scenario argv, e.g. `-L golden -f <defaults.conf>`.
    pub prefix: Vec<String>,
    /// The whole environment: the command starts from an empty one.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Values for the normalization vars (`socket`, `tmpdir`); [`replay`]
    /// adds `host` and `user` itself.
    pub vars: Vec<(String, String)>,
}

impl Cli {
    pub fn command(&self, argv: &[String]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .args(&self.prefix)
            .args(argv);
        cmd
    }

    /// Run to completion, bounded by [`CMD_TIMEOUT`].
    pub fn run(&self, argv: &[String]) -> (ExitStatus, String, String) {
        self.run_within(argv, CMD_TIMEOUT)
            .unwrap_or_else(|| panic!("{argv:?} still running after {CMD_TIMEOUT:?}"))
    }

    /// Run to completion, or kill the command once `limit` has passed and
    /// return `None`: a server that stops answering leaves no client behind.
    pub fn run_within(
        &self,
        argv: &[String],
        limit: Duration,
    ) -> Option<(ExitStatus, String, String)> {
        let deadline = Instant::now() + limit;
        let mut child = self
            .command(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {argv:?}: {e}"));
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        // Something the command started may still hold its output open.
        let left = deadline.saturating_duration_since(Instant::now());
        let stdout = stdout.recv_timeout(left).ok()?;
        let stderr = stderr.recv_timeout(left).ok()?;
        Some((
            status,
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        ))
    }

    /// The server's pid, if one answers within a second.
    fn server_pid(&self) -> Option<i32> {
        let argv = ["display-message".into(), "-p".into(), "#{pid}".into()];
        match self.run_within(&argv, Duration::from_secs(1))? {
            (status, out, _) if status.success() => out.trim().parse().ok(),
            _ => None,
        }
    }

    /// Wait until the server lists a client on `tty`.
    fn wait_attached(&self, tty: &Path) {
        let deadline = Instant::now() + CMD_TIMEOUT;
        let tty = tty.display().to_string();
        let list = [
            "list-clients".to_string(),
            "-F".into(),
            "#{client_tty}".into(),
        ];
        while !self.run(&list).1.lines().any(|l| l == tty) {
            assert!(Instant::now() < deadline, "client on {tty} never attached");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A raw transcript plus what normalizing it needs.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    /// Built by [`replay`] with a pid and an epoch window. A caller that
    /// knows the exact pids can set `ctx.pids = None` and fill
    /// `ctx.exact_pids` before normalizing (see `normalize.txt`).
    pub ctx: Context,
}

impl Transcript {
    pub fn normalized(&self, normalizer: &Normalizer) -> String {
        normalizer.apply(&self.ctx, &self.text)
    }
}

/// Read `pipe` to the end on a thread; the bytes arrive on the receiver.
fn drain(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// Whether `pid` is a process that has not exited (a zombie counts as gone).
fn alive(pid: i32) -> bool {
    if kill(Pid::from_raw(pid), None).is_err() {
        return false;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => true,
    }
}

/// SIGKILL every server pid it is told about if the test process goes
/// away (a SIGKILL, an abort, a hung `Drop`) before saying `done`: a
/// process of its own, in its own process group, reading the pids on a
/// pipe whose end closes when the test process dies. Daemonized servers
/// are not our children, so nothing else would take them down.
pub struct Reaper {
    child: Child,
    pids: Option<ChildStdin>,
}

const REAPER: &str = "pids=; while read -r p; do [ \"$p\" = done ] && exit 0; pids=\"$pids $p\"; done; [ -z \"$pids\" ] || kill -9 $pids 2>/dev/null; exit 0";

impl Reaper {
    pub fn spawn() -> Reaper {
        let mut child = Command::new("/bin/sh")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-c", REAPER])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn the reaper");
        let pids = child.stdin.take();
        Reaper { child, pids }
    }

    /// Have `pid` killed if this process dies before [`Reaper::done`].
    pub fn watch(&mut self, pid: i32) {
        use std::io::Write;
        if let Some(pipe) = self.pids.as_mut() {
            let _ = writeln!(pipe, "{pid}");
        }
    }

    /// Everything is dealt with: exit without killing anything.
    pub fn done(mut self) {
        use std::io::Write;
        if let Some(mut pipe) = self.pids.take() {
            let _ = writeln!(pipe, "done");
        }
        let _ = self.child.wait();
    }
}

/// Everything a replay started, killed (with the server) when it ends,
/// panic or not.
struct Live<'a> {
    cli: &'a Cli,
    client: Option<PtyClient>,
    background: Vec<Child>,
    /// Every server pid seen, so a server that ignores `kill-server` (or
    /// spins too hard to read it) still dies.
    servers: Vec<i32>,
    /// Whether the current server's pid is still to be asked for.
    ask_pid: bool,
    reaper: Option<Reaper>,
}

impl Live<'_> {
    /// Ask for the server's pid once a server is up (and again after a
    /// `kill-server` step), and hand it to the reaper.
    fn learn_pid(&mut self) {
        if !self.ask_pid {
            return;
        }
        if let Some(pid) = self.cli.server_pid() {
            self.ask_pid = false;
            self.note(pid);
        }
    }

    fn note(&mut self, pid: i32) {
        if !self.servers.contains(&pid) {
            self.servers.push(pid);
            if let Some(r) = self.reaper.as_mut() {
                r.watch(pid);
            }
        }
    }
}

impl Drop for Live<'_> {
    fn drop(&mut self) {
        let client = self.client.take().map(|c| c.child);
        for mut child in self.background.drain(..).chain(client) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(pid) = self.cli.server_pid() {
            self.note(pid);
        }
        // Bounded: a server that stops answering must not hang the test.
        let _ = self
            .cli
            .run_within(&["kill-server".to_string()], CMD_TIMEOUT);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.servers.iter().any(|&p| alive(p)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        for &pid in &self.servers {
            if alive(pid) {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
        if let Some(r) = self.reaper.take() {
            r.done();
        }
    }
}

fn expand(argv: &[String], client: Option<&PtyClient>) -> Vec<String> {
    let fixtures = fixtures_dir().display().to_string();
    argv.iter()
        .map(|a| {
            let a = a.replace("{fixtures}", &fixtures);
            match client {
                Some(c) => a.replace("{client}", &c.tty.display().to_string()),
                None => a,
            }
        })
        .collect()
}

/// Replay `steps` through `cli`, then kill the server.
pub fn replay(cli: &Cli, steps: &[Step]) -> Transcript {
    let started = unix_now();
    let first_pid = next_pid();
    let mut live = Live {
        cli,
        client: None,
        background: Vec::new(),
        servers: Vec::new(),
        ask_pid: true,
        reaper: Some(Reaper::spawn()),
    };
    let mut out = String::new();
    for step in steps {
        match step {
            Step::Tmux {
                line,
                argv,
                background,
            } => {
                let argv = expand(argv, live.client.as_ref());
                out.push_str(&format!("{line}\n"));
                if *background {
                    let child = cli
                        .command(&argv)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap_or_else(|e| panic!("spawning {argv:?}: {e}"));
                    live.background.push(child);
                    continue;
                }
                let (status, stdout, stderr) = cli.run(&argv);
                out.push_str(&stdout);
                if !stdout.is_empty() && !stdout.ends_with('\n') {
                    out.push('\n');
                }
                for l in stderr.lines() {
                    out.push_str(&format!("! {l}\n"));
                }
                match status.code() {
                    Some(code) => out.push_str(&format!("? {code}\n")),
                    None => out.push_str(&format!("? {status}\n")),
                }
                if argv.first().is_some_and(|a| a == "kill-server") {
                    live.ask_pid = true;
                } else {
                    live.learn_pid();
                }
            }
            Step::Client {
                line,
                cols,
                rows,
                argv,
            } => {
                assert!(live.client.is_none(), "one client per scenario");
                let argv = expand(argv, None);
                let client = PtyClient::spawn(cli.command(&argv), *cols, *rows);
                live.client = Some(client);
                if let Some(c) = live.client.as_ref() {
                    cli.wait_attached(&c.tty);
                }
                live.learn_pid();
                out.push_str(&format!("{line}\n"));
            }
            Step::Styles { first, last, until } => {
                let client = live
                    .client
                    .as_ref()
                    .expect("styles needs a client step first");
                match until {
                    Some(text) => out.push_str(&format!("styles {first}-{last} {text}\n")),
                    None => out.push_str(&format!("styles {first}-{last}\n")),
                }
                for run in client.styles(*first, *last, until.as_deref()) {
                    out.push_str(&format!("~ {run}\n"));
                }
            }
            Step::Keys { line, bytes } => {
                let client = live
                    .client
                    .as_mut()
                    .expect("keys needs a client step first");
                client.type_bytes(bytes);
                // Past any escape-time, so a key in the next step is not
                // read as Meta with an Escape here.
                thread::sleep(KEYS_PAUSE);
                out.push_str(&format!("{line}\n"));
            }
            Step::Bells => {
                let client = live
                    .client
                    .as_ref()
                    .expect("bells needs a client step first");
                // Let the last of the output arrive.
                let _ = client.settled(None);
                out.push_str(&format!("bells {}\n", client.bells()));
            }
            Step::Screen { until } => {
                let client = live
                    .client
                    .as_ref()
                    .expect("screen needs a client step first");
                match until {
                    Some(text) => out.push_str(&format!("screen {text}\n")),
                    None => out.push_str("screen\n"),
                }
                for row in client.screen(until.as_deref()) {
                    if row.is_empty() {
                        out.push_str("|\n");
                    } else {
                        out.push_str(&format!("| {row}\n"));
                    }
                }
            }
        }
    }
    drop(live);

    let mut ctx = Context::default();
    ctx.vars.extend(cli.vars.iter().cloned());
    ctx.vars.entry("host".into()).or_insert_with(hostname);
    ctx.vars.entry("user".into()).or_insert_with(username);
    ctx.pids = Some(first_pid..=next_pid());
    ctx.epochs = Some(started - 5..=unix_now() + 5);
    Transcript { text: out, ctx }
}

/// Every `*.scenario` file under `dir`, recursively.
pub fn find_scenarios(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_scenarios(&path, found);
        } else if path.extension().is_some_and(|e| e == "scenario") {
            found.push(path);
        }
    }
}

/// One parsed scenario line.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Tmux {
        line: String,
        argv: Vec<String>,
        background: bool,
    },
    Client {
        line: String,
        cols: u16,
        rows: u16,
        argv: Vec<String>,
    },
    /// The bell the client rang since the last `bells` step.
    Bells,
    Screen {
        until: Option<String>,
    },
    Styles {
        first: u16,
        last: u16,
        until: Option<String>,
    },
    Keys {
        line: String,
        bytes: Vec<u8>,
    },
}

/// The bytes a terminal sends for `keys`'s words (tmux key names).
fn key_bytes(words: &[String]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for word in words {
        match word.as_str() {
            "Space" => out.push(b' '),
            "Escape" => out.push(0x1b),
            "Enter" => out.push(b'\r'),
            "Tab" => out.push(b'\t'),
            "BSpace" => out.push(0x7f),
            "C-Space" => out.push(0),
            "Up" => out.extend_from_slice(b"\x1b[A"),
            "Down" => out.extend_from_slice(b"\x1b[B"),
            "Right" => out.extend_from_slice(b"\x1b[C"),
            "Left" => out.extend_from_slice(b"\x1b[D"),
            "C-Up" => out.extend_from_slice(b"\x1b[1;5A"),
            "C-Down" => out.extend_from_slice(b"\x1b[1;5B"),
            "S-Up" => out.extend_from_slice(b"\x1b[1;2A"),
            "S-Down" => out.extend_from_slice(b"\x1b[1;2B"),
            "C-Left" => out.extend_from_slice(b"\x1b[1;5D"),
            "C-Right" => out.extend_from_slice(b"\x1b[1;5C"),
            "M-Up" => out.extend_from_slice(b"\x1b[1;3A"),
            "M-Down" => out.extend_from_slice(b"\x1b[1;3B"),
            "Home" => out.extend_from_slice(b"\x1bOH"),
            "End" => out.extend_from_slice(b"\x1bOF"),
            "PPage" => out.extend_from_slice(b"\x1b[5~"),
            "NPage" => out.extend_from_slice(b"\x1b[6~"),
            // Meta and a character: an Escape right before it.
            w if w.strip_prefix("M-").is_some_and(|c| c.chars().count() == 1) => {
                out.push(0x1b);
                out.extend_from_slice(&w.as_bytes()[2..]);
            }
            w => match w.strip_prefix("C-").map(str::as_bytes) {
                Some([c]) if c.is_ascii_alphabetic() => out.push(c.to_ascii_lowercase() - b'a' + 1),
                Some(_) => return Err(format!("no control key {w:?}")),
                None => out.extend_from_slice(w.as_bytes()),
            },
        }
    }
    Ok(out)
}

pub fn parse_scenario(text: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let at = |e: String| format!("line {}: {e}", n + 1);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tmux = line
            .strip_prefix("$ ")
            .map(|rest| (rest, false))
            .or_else(|| line.strip_prefix("$& ").map(|rest| (rest, true)));
        if let Some((rest, background)) = tmux {
            let argv = split_words(rest).map_err(at)?;
            steps.push(Step::Tmux {
                line: line.to_string(),
                argv,
                background,
            });
        } else if let Some(rest) = line.strip_prefix("client ") {
            let (size, args) = rest
                .split_once(' ')
                .ok_or_else(|| at("client needs args".into()))?;
            let (cols, rows) = size
                .split_once('x')
                .and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)))
                .ok_or_else(|| at(format!("bad size {size:?}, want <cols>x<rows>")))?;
            let argv = split_words(args).map_err(at)?;
            steps.push(Step::Client {
                line: line.to_string(),
                cols,
                rows,
                argv,
            });
        } else if let Some(rest) = line.strip_prefix("styles ") {
            let (rows, until) = match rest.split_once(' ') {
                Some((rows, text)) => (rows, Some(text.to_string())),
                None => (rest, None),
            };
            let (first, last) = rows
                .split_once('-')
                .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                .filter(|(a, b)| a <= b)
                .ok_or_else(|| at(format!("bad rows {rows:?}, want <first>-<last>")))?;
            steps.push(Step::Styles { first, last, until });
        } else if let Some(rest) = line.strip_prefix("keys ") {
            let bytes = key_bytes(&split_words(rest).map_err(at)?).map_err(at)?;
            steps.push(Step::Keys {
                line: line.to_string(),
                bytes,
            });
        } else if line == "bells" {
            steps.push(Step::Bells);
        } else if line == "screen" {
            steps.push(Step::Screen { until: None });
        } else if let Some(text) = line.strip_prefix("screen ") {
            steps.push(Step::Screen {
                until: Some(text.to_string()),
            });
        } else {
            return Err(at(format!("unknown step {line:?}")));
        }
    }
    Ok(steps)
}

/// Split like a POSIX shell: whitespace separates words; single quotes are
/// literal; double quotes allow `\"`, `\\`, `\$` and `` \` ``; a backslash
/// outside quotes escapes the next character (so `\;` is the word `;`).
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().ok_or("trailing backslash")?);
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The pid the next process gets (Linux and macOS hand them out in order).
pub fn next_pid() -> u64 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id();
    let _ = child.wait();
    u64::from(pid)
}

fn hostname() -> String {
    let out = Command::new("uname").arg("-n").env_clear().output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn username() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_default()
}

/// An attached client on a pty; everything it prints is kept for [`screen`].
struct PtyClient {
    child: Child,
    tty: PathBuf,
    input: File,
    output: Arc<Mutex<(Vec<u8>, Instant)>>,
    /// How many bytes of output `bells` has counted.
    counted: std::cell::Cell<usize>,
    cols: u16,
    rows: u16,
}

impl PtyClient {
    fn spawn(mut cmd: Command, cols: u16, rows: u16) -> PtyClient {
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(&size, None).expect("openpty");
        let tty = nix::unistd::ttyname(&pty.slave).expect("pty slave name");
        let slave = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup pty slave"));
        cmd.stdin(slave(&pty.slave))
            .stdout(slave(&pty.slave))
            .stderr(slave(&pty.slave));
        // SAFETY: setsid and ioctl are async-signal-safe; nothing allocates.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn pty client");
        drop(pty.slave);

        let output = Arc::new(Mutex::new((Vec::new(), Instant::now())));
        let sink = Arc::clone(&output);
        let mut master = File::from(pty.master);
        let input = master.try_clone().expect("dup pty master");
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut out = sink.lock().unwrap_or_else(|e| e.into_inner());
                out.0.extend_from_slice(&buf[..n]);
                out.1 = Instant::now();
            }
        });
        PtyClient {
            child,
            tty,
            input,
            output,
            counted: std::cell::Cell::new(0),
            cols,
            rows,
        }
    }

    /// Type `bytes` on the client's terminal.
    fn type_bytes(&mut self, bytes: &[u8]) {
        use std::io::Write;
        self.input.write_all(bytes).expect("write to the pty");
    }

    /// Wait until the screen shows `until` (if given) and [`SCREEN_QUIET`]
    /// has passed without output, then return every row.
    fn screen(&self, until: Option<&str>) -> Vec<String> {
        let bytes = self.settled(until);
        self.render(&bytes)
    }

    /// Wait as [`PtyClient::screen`] does, then return the style runs of
    /// rows `first` to `last` (the `styles` step).
    fn styles(&self, first: u16, last: u16, until: Option<&str>) -> Vec<String> {
        let bytes = self.settled(until);
        let term = self.term(&bytes);
        let grid = term.grid();
        let mut runs = Vec::new();
        for y in first..=last.min(self.rows.saturating_sub(1)) {
            let row = &grid[Line(i32::from(y))];
            let mut x = 0;
            while x < grid.columns() {
                let look = style_of(&row[Column(x)]);
                let start = x;
                while x < grid.columns() && style_of(&row[Column(x)]) == look {
                    x += 1;
                }
                if let Some(look) = look {
                    runs.push(format!("{y} {start}-{} {look}", x - 1));
                }
            }
        }
        runs
    }

    /// Everything the client printed, once it shows `until` (if given) and
    /// [`SCREEN_QUIET`] has passed without output.
    fn settled(&self, until: Option<&str>) -> Vec<u8> {
        let deadline = Instant::now() + CMD_TIMEOUT;
        loop {
            thread::sleep(Duration::from_millis(50));
            let (bytes, quiet) = {
                let out = self.output.lock().unwrap_or_else(|e| e.into_inner());
                (out.0.clone(), out.1.elapsed() >= SCREEN_QUIET)
            };
            if quiet && !bytes.is_empty() {
                let rows = self.render(&bytes);
                if until.is_none_or(|t| rows.iter().any(|r| r.contains(t))) {
                    return bytes;
                }
            }
            assert!(
                Instant::now() < deadline,
                "client screen never settled{}",
                until.map(|t| format!(" showing {t:?}")).unwrap_or_default()
            );
        }
    }

    /// How many times the terminal rang since the last call: BEL as the
    /// emulator reads it, not as a sequence's terminator.
    fn bells(&self) -> usize {
        use alacritty_terminal::event::{Event, EventListener};
        #[derive(Clone)]
        struct Count(Arc<Mutex<usize>>);
        impl EventListener for Count {
            fn send_event(&self, event: Event) {
                if matches!(event, Event::Bell) {
                    *self.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                }
            }
        }
        let bytes = self
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone();
        let counter = Count(Arc::default());
        let size = TermSize::new(self.cols.into(), self.rows.into());
        let mut term = Term::new(Config::default(), &size, counter.clone());
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, &bytes[..self.counted.get().min(bytes.len())]);
        let before = *counter.0.lock().unwrap_or_else(|e| e.into_inner());
        parser.advance(&mut term, &bytes[self.counted.get().min(bytes.len())..]);
        self.counted.set(bytes.len());
        *counter.0.lock().unwrap_or_else(|e| e.into_inner()) - before
    }
    /// Replay everything the client printed into a fresh emulator.
    fn term(&self, bytes: &[u8]) -> Term<VoidListener> {
        let size = TermSize::new(self.cols.into(), self.rows.into());
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    fn render(&self, bytes: &[u8]) -> Vec<String> {
        let term = self.term(bytes);
        let grid = term.grid();
        (0..grid.screen_lines())
            .map(|y| {
                let row = &grid[Line(y as i32)];
                let text: String = (0..grid.columns()).map(|x| row[Column(x)].c).collect();
                text.trim_end().to_string()
            })
            .collect()
    }
}

const ANSI_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightblack",
    "brightred",
    "brightgreen",
    "brightyellow",
    "brightblue",
    "brightmagenta",
    "brightcyan",
    "brightwhite",
];

/// How a cell looks, for the `styles` step; `None` for the terminal's
/// defaults with no attribute. Only what shows: a blank's foreground, bold,
/// dim and italic draw nothing, and tmux and grofe clear blanks differently.
fn style_of(cell: &Cell) -> Option<String> {
    let blank = cell.c == ' ' || cell.c == '\0';
    let colour = |c: &AnsiColor, default: NamedColor| match c {
        AnsiColor::Named(n) if *n == default => None,
        AnsiColor::Named(n) => Some(format!("{n:?}").to_lowercase()),
        // The 16 ANSI colours are the same colour whichever way they were
        // sent (`SGR 32` or `SGR 38;5;2`), so they print by name.
        AnsiColor::Indexed(i @ 0..16) => Some(ANSI_NAMES[usize::from(*i)].to_string()),
        AnsiColor::Indexed(i) => Some(format!("idx:{i}")),
        AnsiColor::Spec(Rgb { r, g, b }) => Some(format!("rgb:{r:02x}/{g:02x}/{b:02x}")),
    };
    let fg = colour(&cell.fg, NamedColor::Foreground).filter(|_| !blank);
    let bg = colour(&cell.bg, NamedColor::Background);
    let attrs = [
        (Flags::BOLD, "bold"),
        (Flags::DIM, "dim"),
        (Flags::ITALIC, "italic"),
        (Flags::UNDERLINE, "underline"),
        (Flags::INVERSE, "reverse"),
        (Flags::STRIKEOUT, "strikeout"),
    ]
    .into_iter()
    .filter(|(f, _)| cell.flags.contains(*f))
    .filter(|(f, _)| !blank || !matches!(*f, Flags::BOLD | Flags::DIM | Flags::ITALIC))
    .map(|(_, name)| name)
    .collect::<Vec<_>>();
    if fg.is_none() && bg.is_none() && attrs.is_empty() {
        return None;
    }
    let mut look = format!(
        "fg={} bg={}",
        fg.as_deref().unwrap_or("default"),
        bg.as_deref().unwrap_or("default")
    );
    for a in attrs {
        look.push(' ');
        look.push_str(a);
    }
    Some(look)
}

fn script_cli(dir: &Path, body: &str) -> Cli {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join("tmux");
    fs::write(&program, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    Cli {
        program,
        prefix: Vec::new(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: dir.to_path_buf(),
        vars: Vec::new(),
    }
}

#[test]
fn a_server_that_ignores_kill_server_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("server.pid");
    let cli = script_cli(
        dir.path(),
        &format!(
            "case \"$1\" in\n\
             new-session) sleep 300 </dev/null >/dev/null 2>&1 & echo $! >{0} ;;\n\
             display-message) cat {0} ;;\n\
             esac\n",
            pidfile.display()
        ),
    );
    let steps = parse_scenario("$ new-session\n$ kill-server\n$ list-sessions\n").unwrap();
    replay(&cli, &steps);
    let pid: i32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!alive(pid), "server {pid} outlived the replay");
}

#[test]
fn a_command_past_its_limit_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("cmd.pid");
    let cli = script_cli(
        dir.path(),
        &format!("echo $$ >{}\nexec sleep 300\n", pidfile.display()),
    );
    let started = Instant::now();
    assert!(cli.run_within(&[], Duration::from_secs(1)).is_none());
    assert!(started.elapsed() < Duration::from_secs(6));
    // Under load the shell can be killed before it writes its pid; then
    // nothing ran that could be left behind.
    let Ok(pid) = fs::read_to_string(&pidfile) else {
        return;
    };
    let pid: i32 = pid.trim().parse().unwrap();
    assert!(!alive(pid), "the timed-out command {pid} is still running");
}

#[test]
fn the_reaper_kills_its_pids_when_the_test_goes_away() {
    let mut victim = Command::new("sleep").arg("300").spawn().unwrap();
    let mut reaper = Reaper::spawn();
    reaper.watch(victim.id() as i32);
    // What a SIGKILLed test process leaves: the pipe closes, no `done`.
    drop(reaper.pids.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    while victim.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the reaper never killed it");
        thread::sleep(Duration::from_millis(20));
    }
    let _ = reaper.child.wait();
}
