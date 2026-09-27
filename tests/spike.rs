//! Acceptance for the T0.3 spike: drive `examples/spike` inside an expectrl pty,
//! rebuild its outer screen with a second `alacritty_terminal::Term`, assert on text.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;
use expectrl::Session;
use expectrl::session::OsSession;

const COLS: usize = 100;
const ROWS: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(10);

fn spike_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "--example", "spike"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build --example spike failed");
        // tests run from target/<profile>/deps; examples live in target/<profile>/examples
        let exe = std::env::current_exe().expect("current_exe");
        exe.parent()
            .and_then(|deps| deps.parent())
            .expect("target profile dir")
            .join("examples/spike")
    })
    .clone()
}

struct Screen {
    session: OsSession,
    term: Term<VoidListener>,
    parser: Processor,
}

impl Screen {
    fn start(argv: &[&str]) -> Screen {
        // Size the outer pty before the spike starts, so it never sees a resize race.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!(
                "stty cols {COLS} rows {ROWS} && exec \"$0\" -- \"$@\""
            ))
            .arg(spike_bin())
            .args(argv);
        let session = Session::spawn(cmd).expect("spawn spike");
        let term = Term::new(Config::default(), &TermSize::new(COLS, ROWS), VoidListener);
        Screen {
            session,
            term,
            parser: Processor::new(),
        }
    }

    fn send(&mut self, bytes: &str) {
        self.session.write_all(bytes.as_bytes()).expect("write");
        self.session.flush().expect("flush");
    }

    /// Feed whatever the spike wrote so far. Returns false once the pty is closed.
    fn pump(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.session.try_read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => self.parser.advance(&mut self.term, &buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false, // EIO after the spike exits
            }
        }
    }

    fn text(&self) -> String {
        let grid = self.term.grid();
        let mut out = String::new();
        for line in 0..grid.screen_lines() {
            for col in 0..grid.columns() {
                let cell = &grid[Line(line as i32)][Column(col)];
                if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    out.push(cell.c);
                }
            }
            out.push('\n');
        }
        out
    }

    fn wait_for(&mut self, needle: &str) {
        self.wait_until(&format!("{needle:?}"), |text| text.contains(needle));
    }

    fn wait_until(&mut self, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let open = self.pump();
            if done(&self.text()) {
                return;
            }
            assert!(
                open && Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}",
                self.text()
            );
            sleep(Duration::from_millis(20));
        }
    }

    fn wait_exit(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if !self.session.get_process().is_alive().unwrap_or(false) {
                return;
            }
            assert!(Instant::now() < deadline, "spike did not exit");
            sleep(Duration::from_millis(20));
        }
    }
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
}

#[test]
fn echo_roundtrip() {
    let mut s = Screen::start(&["/bin/sh"]);
    s.send("echo grofe-$((40+2))\r");
    s.wait_for("grofe-42");
    s.send("\x1d");
    s.wait_exit();
}

#[test]
fn cpr_reply() {
    let mut s = Screen::start(&["/bin/sh"]);
    // POSIX (works in dash too): read the reply byte by byte until the final `R`.
    // The loop blocks until the spike answers through PtyWrite, and the marker is
    // printed only if the reply looks like `ESC[row;colR`.
    s.send(concat!(
        "stty raw -echo; printf '\\033[6n'; x=; ",
        "while c=$(dd bs=1 count=1 2>/dev/null); [ \"$c\" != R ]; do x=\"$x$c\"; done; ",
        "stty sane; case \"$x\" in *\\[*\\;*) echo got-cpr-$((6*7));; esac\r"
    ));
    s.wait_for("got-cpr-42");
    s.send("\x1d");
    s.wait_exit();
}

#[test]
fn nvim_edit() {
    if !on_path("nvim") {
        eprintln!("skipping nvim_edit: nvim not on PATH");
        return;
    }
    let mut s = Screen::start(&["nvim", "-u", "NONE", "-i", "NONE"]);
    s.wait_for("~");
    s.send("ihello-spike\x1b");
    s.wait_for("hello-spike");
    // Let nvim finish the Esc before `:` so it is not read as Alt-:.
    sleep(Duration::from_millis(300));
    s.send(":q!\r");
    s.wait_exit();
}

#[test]
fn nvim_exit_restores_primary_screen() {
    if !on_path("nvim") {
        eprintln!("skipping nvim_exit_restores_primary_screen: nvim not on PATH");
        return;
    }
    let mut s = Screen::start(&["/bin/sh"]);
    s.send("PS1='gp> '\r");
    s.send("echo primary-$((20+1))\r");
    s.wait_for("primary-21");
    s.send("nvim -u NONE -i NONE\r");
    s.wait_until("nvim on the alternate screen", |text| {
        text.contains('~') && !text.contains("primary-21")
    });
    s.send("ihello-alt\x1b");
    s.wait_for("hello-alt");
    // Let nvim finish the Esc before `:` so it is not read as Alt-:.
    sleep(Duration::from_millis(300));
    s.send(":q!\r");
    s.wait_until("the primary screen with a fresh prompt", |text| {
        text.contains("primary-21")
            && !text.contains("hello-alt")
            && text.lines().any(|line| line.trim_end() == "gp>")
    });
    s.send("\x1d");
    s.wait_exit();
}
