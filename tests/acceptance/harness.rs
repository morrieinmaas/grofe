//! Shared pty + reconstruction harness for grofe's acceptance suite (spec 9).
//!
//! [`Harness`] spawns a target program in a pty at a fixed size, feeds it
//! keys, and reconstructs what an outer terminal would see from its output
//! through `alacritty_terminal`, the same crate grofe's panes use (§4.4's
//! round-trip trick). `self_test` (this crate) drives the T0.3 spike binary
//! through it, since `grofe attach` does not exist until T2.5a; T2.5a,
//! T2.5b and T3.x each add one file here that drives `grofe attach` (via
//! `env!("CARGO_BIN_EXE_grofe")`) the same way.

use std::io::{self, Write};
use std::process::Command;
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

/// Default bound for [`Harness::wait_for`]. Use [`Harness::wait_for_timeout`]
/// when a scenario has a good reason to wait longer or shorter.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Drives one program in a pty and reconstructs what an outer terminal
/// would see through `alacritty_terminal`.
pub struct Harness {
    session: OsSession,
    term: Term<VoidListener>,
    parser: Processor,
    /// Every byte the child wrote so far, for checks on the raw stream.
    raw: Vec<u8>,
}

impl Harness {
    /// Spawn `cmd` in a pty fixed at `cols`x`rows` from the first byte it
    /// reads: `cmd` is wrapped in a shell that waits for a go line before
    /// `exec`ing the real program, and the size is set before that line is
    /// sent, so a TUI that reads the window size once at startup (rather
    /// than waiting for a `SIGWINCH`) still sees the right size. (A `stty`
    /// in the wrapper is not enough: ptyprocess sets 80x24 on the master
    /// after the wrapper has exec'd, and under load that lands after the
    /// `stty`.) Env vars and the working directory set on `cmd`
    /// are carried through the wrapper; `env_clear` is not; the wrapper
    /// shell otherwise inherits this process's environment as usual.
    ///
    /// `cmd`'s own args reach the target exactly as given, with no extra
    /// `--` inserted ahead of them: grofe's global parser treats a leading
    /// `--` as end-of-globals, so `grofe -S <sock> attach` spawned through a
    /// wrapper that added one would silently fall back to the default
    /// socket (M2 fix round 1).
    pub fn spawn(cmd: Command, cols: u16, rows: u16) -> Harness {
        let mut wrapper = Command::new("/bin/sh");
        wrapper.arg("-c").arg("read -r go && exec \"$0\" \"$@\"");
        wrapper.arg(cmd.get_program());
        wrapper.args(cmd.get_args());
        if let Some(dir) = cmd.get_current_dir() {
            wrapper.current_dir(dir);
        }
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => {
                    wrapper.env(key, value);
                }
                None => {
                    wrapper.env_remove(key);
                }
            }
        }

        let session = Session::spawn(wrapper).expect("spawn harness target");
        let term = Term::new(
            Config::default(),
            &TermSize::new(cols as usize, rows as usize),
            VoidListener,
        );
        let mut harness = Harness {
            session,
            term,
            parser: Processor::new(),
            raw: Vec::new(),
        };
        harness
            .session
            .get_process_mut()
            .set_window_size(cols, rows)
            .expect("size the pty");
        harness.send("\n");
        harness
    }

    /// `grofe attach -t <target>` (`grofe` is a `TestServer::command()`,
    /// already pointing at its socket) with `env` set, e.g.
    /// `GROFE_TEST_CAPS=kitty_graphics` to claim capabilities the
    /// reconstruction cannot answer the probe for (T3.8).
    pub fn spawn_attach_with_env(
        mut grofe: Command,
        target: &str,
        env: &[(&str, &str)],
        cols: u16,
        rows: u16,
    ) -> Harness {
        grofe
            .args(["attach", "-t", target])
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        for (key, value) in env {
            grofe.env(key, value);
        }
        Harness::spawn(grofe, cols, rows)
    }

    /// Feed raw bytes to the pty, as if typed (or pasted) at the keyboard.
    pub fn send(&mut self, bytes: impl AsRef<[u8]>) {
        self.session
            .write_all(bytes.as_ref())
            .expect("write to pty");
        self.session.flush().expect("flush pty");
    }

    /// Drain whatever the child wrote so far into the reconstruction.
    /// Returns false once the pty has closed.
    fn pump(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.session.try_read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.parser.advance(&mut self.term, &buf[..n]);
                    self.raw.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false, // EIO after the child exits
            }
        }
    }

    /// The reconstructed screen, one line per row. Wide characters' spacer
    /// cells are skipped so the visible glyph is not duplicated.
    fn render(&self) -> String {
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

    /// The reconstructed screen, after draining pending output.
    pub fn text(&mut self) -> String {
        self.pump();
        self.render()
    }

    /// Every byte the child wrote so far (the raw outer stream), after
    /// draining pending output.
    pub fn output(&mut self) -> &[u8] {
        self.pump();
        &self.raw
    }

    /// Wait until the raw stream contains `needle` at or after byte `from`
    /// (an earlier [`Self::output`] length), or panic after
    /// [`DEFAULT_TIMEOUT`]. Returns the offset just past the match.
    pub fn wait_for_output(&mut self, needle: &[u8], from: usize) -> usize {
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        loop {
            let open = self.pump();
            let start = from.min(self.raw.len());
            if let Some(i) = self.raw[start..]
                .windows(needle.len())
                .position(|w| w == needle)
            {
                return start + i + needle.len();
            }
            assert!(
                open && Instant::now() < deadline,
                "timed out waiting for {:?} in the output; screen:\n{}",
                String::from_utf8_lossy(needle),
                self.render()
            );
            sleep(Duration::from_millis(20));
        }
    }

    /// 0-based `(col, row)` of the cursor in the reconstructed screen, after
    /// draining pending output.
    pub fn cursor(&mut self) -> (u16, u16) {
        self.pump();
        let point = self.term.grid().cursor.point;
        (point.column.0 as u16, point.line.0.max(0) as u16)
    }

    /// Wait until the screen contains `needle`, or panic after
    /// [`DEFAULT_TIMEOUT`].
    pub fn wait_for(&mut self, needle: &str) {
        self.wait_for_timeout(needle, DEFAULT_TIMEOUT);
    }

    /// Wait until the screen contains `needle`, or panic after `timeout`.
    pub fn wait_for_timeout(&mut self, needle: &str, timeout: Duration) {
        self.wait_until(&format!("{needle:?} on screen"), timeout, |text| {
            text.contains(needle)
        });
    }

    /// Wait until `done` holds for the reconstructed screen text, or panic
    /// after `timeout`, printing `what` and the screen on failure.
    pub fn wait_until(&mut self, what: &str, timeout: Duration, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let open = self.pump();
            let text = self.render();
            if done(&text) {
                return;
            }
            assert!(
                open && Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{text}"
            );
            sleep(Duration::from_millis(20));
        }
    }

    /// Resize both the pty, so the child sees a `SIGWINCH` with the new
    /// size, and the reconstruction terminal.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.session
            .get_process_mut()
            .set_window_size(cols, rows)
            .expect("resize pty");
        self.term
            .resize(TermSize::new(cols as usize, rows as usize));
    }

    /// Wait for the child to exit, or panic after [`DEFAULT_TIMEOUT`].
    pub fn wait_exit(&mut self) {
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        loop {
            self.pump();
            if !self.session.get_process().is_alive().unwrap_or(false) {
                return;
            }
            assert!(Instant::now() < deadline, "target did not exit");
            sleep(Duration::from_millis(20));
        }
    }
}

/// True if `bin` resolves on `PATH`.
pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
}

/// Tool paths are discovered, never hardcoded (spec 9): true if a scenario
/// needing `bin` should proceed. Missing under `CI` fails the test (the §8
/// acceptance suite must pass on every OS before a milestone claims
/// daily-driver status); missing locally just skips, so a laptop without
/// every tool installed does not block iteration.
pub fn require_tool(bin: &str) -> bool {
    if on_path(bin) {
        return true;
    }
    if std::env::var_os("CI").is_some() {
        panic!("{bin} not found on PATH (required under CI)");
    }
    eprintln!("skipping: {bin} not found on PATH");
    false
}
