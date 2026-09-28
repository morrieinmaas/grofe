//! PaneHandle, spawn, pre_exec.
//!
//! A pane is a child process on a pty, fed through `alacritty_terminal`. Each
//! pane runs three tokio tasks (spec §4.8): the reader (owns the master's read
//! side, the emulator's listener, the sync timeout and dead detection), the
//! writer (owns writes to the master through a bounded queue) and a sampler
//! (foreground command, argv and cwd, once a second).

pub mod io;
pub mod procinfo;

use std::ffi::OsString;
use std::io as stdio;
use std::ops::Range;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{self, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape as VteCursorShape, Processor};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::libc;
use nix::pty::{Winsize, openpty};
use nix::unistd::{AccessFlags, access};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

pub use alacritty_terminal::term::cell::Flags as CellFlags;

use crate::types::{CaptureBound, CaptureOpts, InputModes, MouseEncoding, MouseMode, PaneId};
use io::{InputQueue, Listener};
use procinfo::ProcSample;

/// Events a pane reports to the server actor. Contract stub landed before M1
/// Wave A so the actor (T1.1b) and the pane (T1.4) can be built in parallel.
///
/// Every event carries the spawn's `generation` ([`PaneHandle::generation`]),
/// so events from a process a `respawn-pane` replaced (already queued when
/// it ran) can be told apart and ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneEvent {
    pub pane: PaneId,
    pub generation: u64,
    pub kind: PaneEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEventKind {
    /// Child exited and the master drained (EOF/EIO or grace period).
    Exited(Option<i32>),
    /// Raw bytes read from the master, sent only while the output tap is
    /// on ([`set_output_tap`]).
    Output(Vec<u8>),
    Title(String),
    Bell,
    /// OSC 52 clipboard store, already decoded.
    Clipboard(String),
    /// Emitted from M3 (OSC 7 scanner).
    Cwd(PathBuf),
    /// M3: one framed APC `_G` command from the scanner.
    Graphics(Vec<u8>),
}

/// What to spawn in a new pane.
#[derive(Debug, Clone)]
pub struct PaneSpawn {
    pub id: PaneId,
    /// `None` starts the user's shell (`$SHELL`, else `/bin/sh`) as a login shell.
    pub argv: Option<Vec<String>>,
    pub cwd: PathBuf,
    /// Applied over the server's own environment; grofe's `TERM`, `COLORTERM`,
    /// `GROFE` and `GROFE_PANE` are applied after it and always win.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    /// The `history-limit` option: scrollback lines kept.
    pub history_limit: usize,
    /// The server's socket path, exported as `GROFE`.
    pub socket: PathBuf,
    /// `TERM` for the pane, resolved once at server start ([`inner_term`]).
    pub term: &'static str,
}

/// `enqueue_input` refused bytes because the pane's writer queue is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("pane input queue is full")]
pub struct QueueFull;

/// One cell of a [`RowsSnapshot`]: the character plus its style.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapCell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    /// Zero-width characters combined with `c`.
    pub zerowidth: Vec<char>,
    /// The SGR 58 underline colour; `None` underlines in the foreground
    /// colour.
    pub underline_color: Option<Color>,
}

/// The cursor's shape, as DECSCUSR sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Where the pane's cursor is and how it looks. `row` and `col` are 0-based
/// in the pane's visible screen (row 0 is the top row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    /// DECTCEM (`\e[?25h` / `\e[?25l`).
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

/// An owned copy of some visible rows, taken under the pane lock (M2 renderer
/// input), with the cursor as it was under that same lock.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowsSnapshot {
    pub rows: Vec<Vec<SnapCell>>,
    pub cursor: CursorState,
}

/// The emulator: `Term` and its parser, always locked together.
pub(crate) struct Emu {
    pub(crate) term: Term<Listener>,
    pub(crate) parser: Processor,
}

#[derive(Clone, Copy)]
pub(crate) struct Size {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

impl Size {
    fn winsize(self) -> Winsize {
        Winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}

nix::ioctl_write_ptr_bad!(set_winsize, libc::TIOCSWINSZ, Winsize);

/// Ceiling on how many fds the fallback cloexec loop scans (macOS, or a Linux
/// kernel too old for `CLOSE_RANGE_CLOEXEC`, i.e. before 5.11). `getrlimit`'s current
/// soft limit is normally in the low thousands; a runaway or `RLIM_INFINITY`
/// value is capped here rather than scanned one fd at a time.
// ponytail: 65536 fds is the scan ceiling; a pane's child never legitimately
// holds more open fds than that.
const MAX_FD_SCAN: u32 = 65536;

/// The current `RLIMIT_NOFILE` soft limit, capped at [`MAX_FD_SCAN`]. Must be
/// read before `fork` and passed into `pre_exec` by value: `pre_exec` runs in
/// the child between `fork` and `exec`, where only async-signal-safe calls
/// are allowed, and `getrlimit` is not one of them.
fn nofile_limit() -> u32 {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid, writable `rlimit`.
    let cur = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_cur
    } else {
        MAX_FD_SCAN as libc::rlim_t
    };
    u32::try_from(cur).unwrap_or(MAX_FD_SCAN).min(MAX_FD_SCAN)
}

/// Mark every fd above 2 in the child close-on-exec (spec §4.8). macOS has
/// no `accept4`/`SOCK_CLOEXEC`, so tokio (and std, for some other fds) sets
/// `FD_CLOEXEC` on an accepted socket with a separate, non-atomic `fcntl`
/// after `accept()`. A pane forked while another thread is between those two
/// calls would otherwise inherit that live client connection, or any other
/// transient fd, into the child.
///
/// They are marked, not closed: std reports the `exec` result to the parent
/// over a close-on-exec pipe, and `spawn` returns when that pipe closes.
/// Closing it here made `spawn` return while the child was still the forked
/// server, whose SIGTERM handler would then swallow a signal sent to the
/// pane's pid, and made a failed `exec` look like success.
///
/// Async-signal-safe: no allocation, no locks, and never reads `/dev/fd` or
/// `/proc` (`opendir` allocates).
fn cloexec_inherited_fds(limit: u32) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a bare syscall, no allocation. Marks every fd in
        // [3, u32::MAX] that is actually open; others are skipped.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                u32::MAX as libc::c_uint,
                libc::CLOSE_RANGE_CLOEXEC as libc::c_int,
            )
        };
        if ret == 0 {
            return;
        }
        // Falls through to the loop below on ENOSYS or EINVAL (kernel older
        // than 5.11).
    }
    for fd in 3..limit as i32 {
        // SAFETY: fcntl on a possibly-unopened fd is safe; EBADF is ignored.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}

/// Serialises spawns from `openpty` to `spawn`: `FD_CLOEXEC` is set by a
/// separate `fcntl`, so without this a concurrent fork could inherit a master.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Generations handed out by [`PaneHandle::spawn`]: unique per spawn in
/// this process, so also per respawn of the same pane.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Turns the output tap on or off for every pane in this process (one
/// server per process): while on, each reader also sends what it reads as
/// [`PaneEventKind::Output`], for `pane-output` subscribers.
pub fn set_output_tap(on: bool) {
    io::OUTPUT_TAP.store(on, Ordering::Relaxed);
}

/// The inner `TERM`, resolved on first use. The server calls this once at
/// start so the terminfo walk does not land on the first spawn.
pub fn inner_term() -> &'static str {
    static TERM: OnceLock<&'static str> = OnceLock::new();
    TERM.get_or_init(crate::term::resolve_term)
}

/// A running (or dead, until dropped) pane. Dropping it stops its tasks and
/// closes the master, which hangs up the child.
pub struct PaneHandle {
    pid: Option<u32>,
    generation: u64,
    master: Arc<AsyncFd<OwnedFd>>,
    emu: Arc<Mutex<Emu>>,
    input: Arc<InputQueue>,
    dirty: Arc<AtomicBool>,
    sample: Arc<Mutex<ProcSample>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for PaneHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PaneHandle {
    /// Spawn `spec` on a new pty and start its tasks. Must be called from
    /// within a tokio runtime.
    pub fn spawn(
        spec: PaneSpawn,
        to_actor: mpsc::Sender<PaneEvent>,
        wake: Arc<Notify>,
    ) -> stdio::Result<Self> {
        let size = Size {
            cols: spec.cols.max(1),
            rows: spec.rows.max(1),
        };
        let (master, child) = spawn_child(&spec, size)?;
        let pid = child.as_ref().and_then(|c| c.id());
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let master = Arc::new(AsyncFd::new(master)?);

        let events = Arc::default();
        let config = term::Config {
            scrolling_history: spec.history_limit,
            // Off by default in alacritty_terminal: push/pop/query would be
            // ignored and a pane could never ask for kitty keys (spec §4.7).
            kitty_keyboard: true,
            ..term::Config::default()
        };
        let emu = Arc::new(Mutex::new(Emu {
            term: Term::new(config, &size, Listener(Arc::clone(&events))),
            parser: Processor::new(),
        }));
        let dirty = Arc::new(AtomicBool::new(false));
        let sample = Arc::new(Mutex::new(ProcSample::default()));
        let (input, input_rx) = InputQueue::new();

        let reader = tokio::spawn(io::reader(io::Reader {
            id: spec.id,
            generation,
            master: Arc::clone(&master),
            child,
            emu: Arc::clone(&emu),
            events,
            input: Arc::clone(&input),
            dirty: Arc::clone(&dirty),
            wake,
            to_actor,
        }));
        let writer = tokio::spawn(io::writer(
            Arc::clone(&master),
            input_rx,
            Arc::clone(&input),
        ));
        let sampler = tokio::spawn(procinfo::sampler(Arc::clone(&master), Arc::clone(&sample)));

        Ok(Self {
            pid,
            generation,
            master,
            emu,
            input,
            dirty,
            sample,
            tasks: vec![reader, writer, sampler],
        })
    }

    /// Queue bytes for the pane's writer. Never blocks.
    pub fn enqueue_input(&self, bytes: Vec<u8>) -> Result<(), QueueFull> {
        self.input.push(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> stdio::Result<()> {
        let size = Size {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        lock(&self.emu).term.resize(size);
        // SAFETY: a valid fd we hold and a pointer to a live Winsize.
        unsafe { set_winsize(self.master.get_ref().as_raw_fd(), &size.winsize()) }?;
        Ok(())
    }

    /// Plain-text lines per tmux's `capture-pane -p`: one line per row,
    /// trailing blanks trimmed, each ending in `\n`.
    pub fn capture(&self, opts: &CaptureOpts) -> String {
        // ponytail: `escapes` (-e) is ignored until the M2 renderer can emit SGR.
        let emu = lock(&self.emu);
        let grid = emu.term.grid();
        let history = grid.history_size() as i32;
        let last = grid.screen_lines() as i32 - 1;
        let bound = |b: Option<CaptureBound>, default: i32| match b {
            None => default,
            Some(CaptureBound::Line(n)) => n.clamp(-history, last),
            Some(CaptureBound::HistoryStart) => -history,
            Some(CaptureBound::VisibleEnd) => last,
        };
        let (start, end) = (bound(opts.start, 0), bound(opts.end, last));
        let mut out = String::new();
        for line in start..=end {
            let row = &grid[Line(line)];
            let mut text = String::new();
            for col in 0..grid.columns() {
                let cell = &row[Column(col)];
                if cell.flags.intersects(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if cell.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER) {
                    text.push(' ');
                    continue;
                }
                text.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    text.extend(zw);
                }
            }
            out.push_str(text.trim_end_matches(' '));
            out.push('\n');
        }
        out
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// This spawn's generation, as carried by every [`PaneEvent`] it sends.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The foreground process's name, from the last sample.
    pub fn current_command(&self) -> Option<String> {
        lock(&self.sample).command.clone()
    }

    /// The foreground process's full argv, from the last sample.
    pub fn current_argv(&self) -> Option<Vec<String>> {
        lock(&self.sample).argv.clone()
    }

    /// The foreground process's cwd, from the last sample.
    pub fn current_path(&self) -> Option<PathBuf> {
        lock(&self.sample).path.clone()
    }

    pub fn modes(&self) -> InputModes {
        let mode = *lock(&self.emu).term.mode();
        let mouse = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseMode::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseMode::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::Click
        } else {
            MouseMode::Off
        };
        let mouse_encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::X10
        };
        InputModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            kitty_flags: ((mode & TermMode::KITTY_KEYBOARD_PROTOCOL).bits()
                / TermMode::DISAMBIGUATE_ESC_CODES.bits()) as u8,
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            mouse,
            mouse_encoding,
            focus: mode.contains(TermMode::FOCUS_IN_OUT),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        }
    }

    /// Whether the screen changed since the last call; clears the flag.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// The cursor's position, visibility and shape.
    pub fn cursor(&self) -> CursorState {
        cursor_state(&lock(&self.emu).term)
    }

    /// Copy the visible rows in `rows` (0 is the top row; out-of-range rows
    /// are skipped), and the cursor, under one lock.
    pub fn snapshot_rows(&self, rows: Range<u16>) -> RowsSnapshot {
        let emu = lock(&self.emu);
        let cursor = cursor_state(&emu.term);
        let grid = emu.term.grid();
        let end = rows.end.min(grid.screen_lines() as u16);
        let rows = (rows.start..end)
            .map(|r| {
                let row = &grid[Line(r as i32)];
                (0..grid.columns())
                    .map(|c| {
                        let cell = &row[Column(c)];
                        SnapCell {
                            c: cell.c,
                            fg: cell.fg,
                            bg: cell.bg,
                            flags: cell.flags,
                            zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                            underline_color: cell.underline_color(),
                        }
                    })
                    .collect()
            })
            .collect();
        RowsSnapshot { rows, cursor }
    }
}

fn cursor_state(term: &Term<Listener>) -> CursorState {
    let point = term.grid().cursor.point;
    let style = term.cursor_style();
    let shape = match style.shape {
        VteCursorShape::Underline => CursorShape::Underline,
        VteCursorShape::Beam => CursorShape::Bar,
        VteCursorShape::Block | VteCursorShape::HollowBlock | VteCursorShape::Hidden => {
            CursorShape::Block
        }
    };
    CursorState {
        row: point.line.0.max(0) as u16,
        col: point.column.0 as u16,
        visible: term.mode().contains(TermMode::SHOW_CURSOR)
            && style.shape != VteCursorShape::Hidden,
        shape,
        blink: style.blinking,
    }
}

/// The environment a pane's child gets on top of the server's own: the spawn
/// env, a UTF-8 locale if none is set, then grofe's variables.
fn child_env(spec: &PaneSpawn) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> =
        spec.env.iter().map(|(k, v)| (k.into(), v.into())).collect();
    let has_locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|k| {
        let set = |v: &str| !v.is_empty();
        match spec.env.iter().rev().find(|(name, _)| name == k) {
            Some((_, v)) => set(v),
            None => std::env::var(k).is_ok_and(|v| set(&v)),
        }
    });
    if !has_locale {
        let utf8 = if cfg!(target_os = "macos") {
            "UTF-8"
        } else {
            "C.UTF-8"
        };
        env.push(("LC_CTYPE".into(), utf8.into()));
    }
    env.push(("TERM".into(), spec.term.into()));
    env.push(("COLORTERM".into(), crate::term::COLORTERM.into()));
    env.push(("GROFE".into(), spec.socket.clone().into_os_string()));
    env.push(("GROFE_PANE".into(), spec.id.to_string().into()));
    env
}

/// The shell for a pane with no argv: `SHELL` from the spawn env, then the
/// server's; `/bin/sh` when neither is set or the one found is not an
/// executable absolute path (tmux's `checkshell`).
fn default_shell(spec: &PaneSpawn) -> String {
    spec.env
        .iter()
        .rev()
        .find(|(k, _)| k == "SHELL")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("SHELL").ok())
        .filter(|s| s.starts_with('/') && access(s.as_str(), AccessFlags::X_OK).is_ok())
        .unwrap_or_else(|| "/bin/sh".into())
}

/// The directory a pane starts in: `cwd` if it is one, else `$HOME`, else
/// `/`, as tmux falls back when it cannot `chdir`.
pub fn usable_cwd(cwd: PathBuf) -> PathBuf {
    if cwd.is_dir() {
        return cwd;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Whether a spawn error came from `fork` itself rather than from the
/// child's setup or `exec`: only those fail the command, as in tmux.
fn is_fork_error(e: &stdio::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::ENOMEM))
}

/// Open a pty and spawn the child on it (spec §4.8). Returns the master,
/// non-blocking and close-on-exec, and the child, or `None` when the
/// program could not be started (it failed to exec): like tmux, that
/// pane is born dead rather than failing the command.
fn spawn_child(
    spec: &PaneSpawn,
    size: Size,
) -> stdio::Result<(OwnedFd, Option<tokio::process::Child>)> {
    let mut cmd = match &spec.argv {
        Some(argv) => {
            let (prog, args) = argv
                .split_first()
                .ok_or_else(|| stdio::Error::new(stdio::ErrorKind::InvalidInput, "empty argv"))?;
            let mut cmd = tokio::process::Command::new(prog);
            cmd.args(args);
            cmd
        }
        None => {
            let shell = default_shell(spec);
            let name = Path::new(&shell)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| shell.clone());
            let mut cmd = tokio::process::Command::new(&shell);
            cmd.arg0(format!("-{name}"));
            cmd
        }
    };
    cmd.current_dir(&spec.cwd).envs(child_env(spec));

    let _guard = lock(&SPAWN_LOCK);
    let pty = openpty(&size.winsize(), None)?;
    fcntl(&pty.master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    // The original slave fd must not survive exec; dup2 clears CLOEXEC on 0/1/2.
    fcntl(&pty.slave, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    let slave = pty.slave.as_raw_fd();
    let fd_limit = nofile_limit();
    // SAFETY: the closure only makes async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(stdio::Error::last_os_error());
            }
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(stdio::Error::last_os_error());
            }
            for fd in 0..3 {
                if libc::dup2(slave, fd) < 0 {
                    return Err(stdio::Error::last_os_error());
                }
            }
            cloexec_inherited_fds(fd_limit);
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) if is_fork_error(&e) => return Err(e),
        Err(_) => None,
    };
    drop(pty.slave);
    drop(_guard);

    let flags = OFlag::from_bits_truncate(fcntl(&pty.master, FcntlArg::F_GETFL)?);
    fcntl(&pty.master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok((pty.master, child))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_term(term: &'static str) -> PaneSpawn {
        PaneSpawn {
            id: PaneId(0),
            argv: None,
            cwd: PathBuf::from("/"),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            history_limit: 100,
            socket: PathBuf::from("/tmp/grofe-test.sock"),
            term,
        }
    }

    /// `child_env` must use the spawn's own resolved `term`, not a fresh
    /// read of the live [`inner_term`] cache: the two would happen to
    /// agree in the real server (both trace back to the same start-time
    /// resolution), but a `PaneSpawn` built with a different value proves
    /// which one `child_env` actually reads from.
    #[test]
    fn child_env_term_comes_from_the_spawn_not_the_live_cache() {
        let spec = spec_with_term("a-term-that-is-not-the-live-cache");
        let env = child_env(&spec);
        let term = env
            .iter()
            .find(|(k, _)| k.to_str() == Some("TERM"))
            .map(|(_, v)| v.to_str().unwrap());
        assert_eq!(term, Some("a-term-that-is-not-the-live-cache"));
        assert_ne!(term, Some(inner_term()));
    }

    fn spec_for(argv: &[&str]) -> PaneSpawn {
        PaneSpawn {
            argv: Some(argv.iter().map(|s| s.to_string()).collect()),
            ..spec_with_term("xterm-256color")
        }
    }

    /// std learns whether `exec` worked from a close-on-exec pipe; the
    /// child must keep it open until `exec`, or a failed `exec` reads as
    /// success (and `spawn` returns before the child has exec'd at all).
    /// A program that fails to exec gives a pane born dead with status 1,
    /// as tmux does, and no pid.
    #[tokio::test]
    async fn a_failed_exec_gives_a_dead_pane_with_status_1() {
        let (tx, mut rx) = mpsc::channel(8);
        let handle = PaneHandle::spawn(
            spec_for(&["/nonexistent/grofe-no-such-program"]),
            tx,
            Arc::new(Notify::new()),
        )
        .expect("a failed exec is not a spawn error");
        assert_eq!(handle.pid(), None);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("Exited in time")
            .expect("an event");
        assert_eq!(ev.kind, PaneEventKind::Exited(Some(1)));
    }

    #[test]
    fn a_stale_shell_falls_back_to_bin_sh() {
        let spec = |shell: &str| PaneSpawn {
            env: vec![("SHELL".to_string(), shell.to_string())],
            ..spec_with_term("xterm-256color")
        };
        assert_eq!(default_shell(&spec("/nonexistent/grofe-shell")), "/bin/sh");
        assert_eq!(default_shell(&spec("sh")), "/bin/sh");
        assert_eq!(default_shell(&spec("/etc/hosts")), "/bin/sh");
        assert_eq!(default_shell(&spec("/bin/sh")), "/bin/sh");
        let sh = std::fs::canonicalize("/bin/sh")
            .unwrap()
            .display()
            .to_string();
        assert_eq!(default_shell(&spec(&sh)), sh);
    }

    #[test]
    fn a_missing_cwd_falls_back_to_home() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        assert_eq!(usable_cwd(PathBuf::from("/nonexistent/grofe-dir")), home);
        assert_eq!(usable_cwd(PathBuf::from("/")), PathBuf::from("/"));
    }

    /// The server catches SIGTERM. A pid handed out while the child was
    /// still the forked server (before `exec`) would have a SIGTERM sent to
    /// it run the server's handler and vanish, leaving the child running.
    /// So once `spawn` returns, a caught signal must always reach the
    /// child. SIGUSR1 stands in for SIGTERM, so this test binary keeps
    /// SIGTERM's default action (a CI timeout must still end it).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_caught_signal_right_after_spawn_ends_the_child() {
        let _catch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .expect("install SIGUSR1 handler");
        for i in 0..50 {
            let (tx, mut rx) = mpsc::channel(8);
            let handle = PaneHandle::spawn(spec_for(&["sleep", "30"]), tx, Arc::new(Notify::new()))
                .expect("spawn");
            let pid = handle.pid().expect("pid") as i32;
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGUSR1,
            )
            .expect("kill");
            let exited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Some(ev) = rx.recv().await {
                    if matches!(ev.kind, PaneEventKind::Exited(_)) {
                        return true;
                    }
                }
                false
            })
            .await;
            assert_eq!(exited, Ok(true), "iteration {i}: SIGUSR1 to {pid} was lost");
        }
    }
}
