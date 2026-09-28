//! The tmux golden recorder (task TC.0b, spec §9). `record_goldens` is an
//! ignored test driven by `scripts/record-compat-goldens.sh`, which runs it
//! inside the `containers/compat/golden.Containerfile` image. It refuses to
//! run outside a container or against any tmux but 3.7b.
//!
//! Each `*.scenario` under `tests/fixtures/compat/golden/` is replayed
//! against a fresh tmux server and its output written, normalized, to the
//! sibling `*.golden`. `tmux list-commands` goes to `list-commands.txt`.
//!
//! Hermetic by construction: every tmux runs with a cleared environment
//! (`env -i`, which also drops `TMUX`) plus [`fixed_env`], from its absolute
//! path, on a private `-L` socket under a per-scenario `TMUX_TMPDIR`, with
//! `-f tmux-defaults.conf`.
//!
//! Scenario lines (blank lines and `#` comments are skipped):
//!
//! - `$ <args>`: run `tmux <args>`; the args are split like a POSIX shell
//!   would (single quotes, double quotes, backslash). Recorded as the `$` line,
//!   stdout verbatim, each stderr line as `! <line>`, and `? <exit status>`.
//! - `client <cols>x<rows> <args>`: start `tmux <args>` (normally `attach -t
//!   <session>`) on a pty of that size, as the scenario's one attached client,
//!   and wait until tmux lists it. In later `$` steps the word `{client}`
//!   stands for its tty, for `-c`/`-t` target-client flags.
//! - `$& <args>`: start `tmux <args>` and go on without waiting, for commands
//!   that block until something closes (`display-popup`, `display-menu`).
//!   Recorded as the `$&` line only; the command is killed with the server.
//! - `screen [<text>]`: wait until the client's screen shows `<text>` (if
//!   given) and its output has settled, then record the screen, each row as
//!   `| <row>` with trailing blanks trimmed. Give `<text>` whenever the screen
//!   is waiting on something a step started, or the capture races it.

use std::fs::{self, File};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, test::TermSize};
use alacritty_terminal::vte::ansi::Processor;
use nix::pty::{Winsize, openpty};
use tempfile::TempDir;

use crate::normalize::{Context, Normalizer, fixtures_dir};

/// The only tmux the recorder accepts.
pub const TMUX_VERSION: &str = "tmux 3.7b";

/// The private `-L` socket name, under each scenario's `TMUX_TMPDIR`.
const SOCKET_NAME: &str = "golden";

/// How long one tmux command, or a screen settling, may take.
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// A client's screen counts as settled after this long without output.
const SCREEN_QUIET: Duration = Duration::from_millis(500);

/// Pids at or above this are 5 digits, so the `pid` rule cannot hit a
/// width, a count or an index. See [`raise_pids`].
const PID_FLOOR: u32 = 20000;

/// The whole environment of every tmux the recorder runs, pane shells
/// included (before tmux adds its own variables).
pub fn fixed_env(tmpdir: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("SHELL", "/bin/sh".into()),
        ("TERM", "xterm-256color".into()),
        ("LANG", "C.UTF-8".into()),
        ("PATH", "/usr/local/bin:/usr/bin:/bin".into()),
        ("HOME", tmpdir.join("home").display().to_string()),
        ("TMUX_TMPDIR", tmpdir.display().to_string()),
    ]
}

/// Refuse any tmux whose `-V` is not exactly [`TMUX_VERSION`], naming what
/// it printed instead.
pub fn check_version(tmux: &Path) -> Result<(), String> {
    if !tmux.is_absolute() {
        return Err(format!(
            "tmux must be an absolute path, got {}",
            tmux.display()
        ));
    }
    let out = Command::new(tmux)
        .env_clear()
        .arg("-V")
        .output()
        .map_err(|e| format!("running {} -V: {e}", tmux.display()))?;
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if printed == TMUX_VERSION {
        Ok(())
    } else {
        Err(format!(
            "refusing to record: {} -V printed {printed:?}, goldens need {TMUX_VERSION:?}",
            tmux.display()
        ))
    }
}

fn in_container() -> bool {
    Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists()
}

/// `GROFE_COMPAT_TMUX`, else `/usr/local/bin/tmux` (where the image puts it).
fn tmux_path() -> PathBuf {
    std::env::var_os("GROFE_COMPAT_TMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/tmux"))
}

#[test]
#[ignore = "records goldens from real tmux 3.7b; run scripts/record-compat-goldens.sh"]
fn record_goldens() {
    assert!(
        in_container(),
        "refusing to record outside a container; run scripts/record-compat-goldens.sh"
    );
    let tmux = tmux_path();
    if let Err(e) = check_version(&tmux) {
        panic!("{e}");
    }
    raise_pids();

    let src = fixtures_dir().join("golden");
    let out = std::env::var_os("GROFE_COMPAT_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| src.clone());
    let only: Vec<String> = std::env::var("GROFE_COMPAT_SCENARIOS")
        .unwrap_or_default()
        .split_whitespace()
        .map(String::from)
        .collect();
    let normalizer = Normalizer::load();

    let list = record(
        &tmux,
        &normalizer,
        &[Step::Tmux {
            line: "$ list-commands".into(),
            argv: vec!["list-commands".into()],
            background: false,
        }],
    );
    let list = list
        .strip_prefix("$ list-commands\n")
        .and_then(|s| s.strip_suffix("? 0\n"))
        .unwrap_or_else(|| panic!("list-commands failed:\n{list}"))
        .to_string();
    write(&out.join("list-commands.txt"), &list);

    let mut scenarios = Vec::new();
    find_scenarios(&src, &mut scenarios);
    scenarios.sort();
    for path in scenarios {
        let rel = path.strip_prefix(&src).unwrap_or(&path).to_path_buf();
        let rel_str = rel.display().to_string();
        if !only.is_empty() && !only.iter().any(|o| rel_str.contains(o.as_str())) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel_str}: {e}"));
        let steps = parse_scenario(&text).unwrap_or_else(|e| panic!("{rel_str}: {e}"));
        let golden = record(&tmux, &normalizer, &steps);
        write(&out.join(rel.with_extension("golden")), &golden);
        eprintln!("recorded {rel_str}");
    }
}

fn find_scenarios(dir: &Path, found: &mut Vec<PathBuf>) {
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

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{}: {e}", parent.display()));
    }
    fs::write(path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
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
    Screen {
        until: Option<String>,
    },
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

/// A fresh tempdir and tmux server for one scenario.
struct Run {
    dir: TempDir,
    tmux: PathBuf,
    conf: PathBuf,
    client: Option<PtyClient>,
    background: Vec<Child>,
}

impl Run {
    fn new(tmux: &Path) -> Run {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("home")).expect("home dir");
        Run {
            dir,
            tmux: tmux.to_path_buf(),
            conf: fixtures_dir().join("tmux-defaults.conf"),
            client: None,
            background: Vec::new(),
        }
    }

    fn command(&self, argv: &[String]) -> Command {
        let mut cmd = Command::new(&self.tmux);
        cmd.env_clear()
            .envs(fixed_env(self.dir.path()))
            .current_dir(self.dir.path().join("home"))
            .arg("-L")
            .arg(SOCKET_NAME)
            .arg("-f")
            .arg(&self.conf)
            .args(argv);
        cmd
    }

    fn tmux(&self, argv: &[String]) -> (ExitStatus, String, String) {
        let child = self
            .command(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning tmux {argv:?}: {e}"));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx
            .recv_timeout(CMD_TIMEOUT)
            .unwrap_or_else(|_| panic!("tmux {argv:?} still running after {CMD_TIMEOUT:?}"))
            .unwrap_or_else(|e| panic!("tmux {argv:?}: {e}"));
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Wait until the server lists the client on `tty`.
    fn wait_attached(&self, tty: &Path) {
        let deadline = Instant::now() + CMD_TIMEOUT;
        let tty = tty.display().to_string();
        let list = [
            "list-clients".to_string(),
            "-F".into(),
            "#{client_tty}".into(),
        ];
        while !self.tmux(&list).1.lines().any(|l| l == tty) {
            assert!(Instant::now() < deadline, "client on {tty} never attached");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn socket(&self) -> PathBuf {
        let uid = nix::unistd::getuid();
        self.dir
            .path()
            .join(format!("tmux-{uid}"))
            .join(SOCKET_NAME)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let client = self.client.take().map(|c| c.child);
        for mut child in self.background.drain(..).chain(client) {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self
            .command(&["kill-server".to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Replay `steps` on a fresh server and return the normalized transcript.
fn record(tmux: &Path, normalizer: &Normalizer, steps: &[Step]) -> String {
    let started = unix_now();
    let first_pid = next_pid();
    let mut run = Run::new(tmux);
    let mut out = String::new();
    for step in steps {
        match step {
            Step::Tmux {
                line,
                argv,
                background,
            } => {
                let argv: Vec<String> = argv
                    .iter()
                    .map(|a| match (a.as_str(), &run.client) {
                        ("{client}", Some(c)) => c.tty.display().to_string(),
                        _ => a.clone(),
                    })
                    .collect();
                out.push_str(&format!("{line}\n"));
                if *background {
                    let child = run
                        .command(&argv)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap_or_else(|e| panic!("spawning tmux {argv:?}: {e}"));
                    run.background.push(child);
                    continue;
                }
                let (status, stdout, stderr) = run.tmux(&argv);
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
            }
            Step::Client {
                line,
                cols,
                rows,
                argv,
            } => {
                assert!(run.client.is_none(), "one client per scenario");
                let client = PtyClient::spawn(run.command(argv), *cols, *rows);
                run.wait_attached(&client.tty);
                run.client = Some(client);
                out.push_str(&format!("{line}\n"));
            }
            Step::Screen { until } => {
                let client = run
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

    let mut ctx = Context::default();
    ctx.vars
        .insert("socket".into(), run.socket().display().to_string());
    ctx.vars
        .insert("tmpdir".into(), run.dir.path().display().to_string());
    ctx.vars.insert("host".into(), hostname());
    ctx.vars.insert("user".into(), username());
    drop(run);
    ctx.pids = Some(first_pid..=next_pid());
    ctx.epochs = Some(started - 5..=unix_now() + 5);
    normalizer.apply(&ctx, &out)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The pid the next process gets (Linux hands them out in order).
fn next_pid() -> u64 {
    let mut child = Command::new("/bin/true").spawn().expect("spawn /bin/true");
    let pid = child.id();
    let _ = child.wait();
    u64::from(pid)
}

/// A fresh container's pid namespace starts at 1, so tmux's pids would be
/// small numbers the `pid` rule could confuse with any other number. Burn
/// pids until they are 5 digits.
fn raise_pids() {
    while next_pid() < u64::from(PID_FLOOR) {}
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
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
    output: Arc<Mutex<(Vec<u8>, Instant)>>,
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
            output,
            cols,
            rows,
        }
    }

    /// Wait until the screen shows `until` (if given) and [`SCREEN_QUIET`]
    /// has passed without output, then return every row.
    fn screen(&self, until: Option<&str>) -> Vec<String> {
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
                    return rows;
                }
            }
            assert!(
                Instant::now() < deadline,
                "client screen never settled{}",
                until.map(|t| format!(" showing {t:?}")).unwrap_or_default()
            );
        }
    }

    /// Replay everything the client printed into a fresh emulator.
    fn render(&self, bytes: &[u8]) -> Vec<String> {
        let size = TermSize::new(self.cols.into(), self.rows.into());
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
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
