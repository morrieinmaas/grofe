//! PaneHandle, spawn, pre_exec.
//!
//! A pane is a child process on a pty, fed through `alacritty_terminal`. Each
//! pane runs two tokio tasks (spec §4.8): the reader (owns the master's read
//! side, the emulator's listener, the sync timeout and dead detection) and the
//! writer (owns writes to the master through a bounded queue). The foreground
//! command, argv and cwd are sampled on demand ([`procinfo::Cache`]).

pub mod io;
pub mod procinfo;
mod written;

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
pub use alacritty_terminal::term::cell::Hyperlink;

use crate::protocol::Caps;
use crate::server::imgstore::{PaneImages, Store};
use crate::server::passthrough::Scanner;
use crate::server::render::write_sgr;
use crate::server::status::Style;
use crate::types::{
    CaptureBound, CaptureOpts, InputModes, MouseEncoding, MouseMode, PaneId, SessionId,
};
use io::{InputQueue, Listener};

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
    /// OSC 52 clipboard store from the scanner (TM.5): the selection
    /// parameter, kept to tmux's characters, and the decoded data.
    Clipboard {
        param: String,
        data: Vec<u8>,
    },
    /// What an OSC 7 reported, as `#{pane_path}` shows it (the scanner's
    /// [`crate::server::passthrough`] `osc7_text`).
    Path(String),
    /// M3: one framed APC `_G` command from the scanner.
    Graphics(Vec<u8>),
    /// G3: one `DCS q` sixel image from the scanner, as the reader
    /// handles it; never sent to the actor. `body` starts with the `q`.
    Sixel {
        p2: u32,
        body: Vec<u8>,
    },
    /// M3: an OSC passthrough re-emits (9, 777, 12, 112), its body without
    /// the `ESC ]` and terminator.
    Osc(Vec<u8>),
}

/// What to spawn in a new pane.
#[derive(Debug, Clone)]
pub struct PaneSpawn {
    pub id: PaneId,
    /// The session the pane's window belongs to (for `TMUX`, TC.1).
    pub session: SessionId,
    /// `None` starts the user's shell (`$SHELL`, else `/bin/sh`) as a login shell.
    pub argv: Option<Vec<String>>,
    pub cwd: PathBuf,
    /// The child's whole environment (the server's own is not inherited;
    /// `dispatch::pane_env` builds it from the server environment, TC.5);
    /// grofe's `TERM`, `COLORTERM`, `GROFE` and `GROFE_PANE` are applied
    /// after it and always win.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    /// The `history-limit` option: scrollback lines kept.
    pub history_limit: usize,
    /// The server's socket path, exported as `GROFE`.
    pub socket: PathBuf,
    /// `TERM` for the pane, resolved once at server start ([`inner_term`]).
    pub term: &'static str,
    /// One cell's `(width, height)` in pixels, from the client the window
    /// is sized for (`Caps::cell_px`); `None` gives the pty 0 pixels.
    pub cell_px: Option<(u16, u16)>,
    /// Whether the pane's graphics queries are answered from the start
    /// (`graphics::panes_graphics_ok`).
    pub graphics_ok: bool,
    /// The `graphics` option (G3): kitty direct placements and sixel.
    pub images: bool,
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
    /// The OSC 8 hyperlink the cell is part of (R20; T3.3 emits it).
    pub hyperlink: Option<Hyperlink>,
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
    /// The pane's visible image placements (G3); `None` while it has none.
    pub images: Option<PaneImages>,
}

/// The emulator: `Term` and its parser, always locked together.
pub(crate) struct Emu {
    pub(crate) term: Term<Listener>,
    pub(crate) parser: Processor,
    /// The pane's image placements (G3).
    pub(crate) images: Store,
}

/// alacritty's tab stops, every 8 columns.
const TAB_STOP: usize = 8;

/// The narrowest a pane's pty and emulator ever get: alacritty_terminal
/// 0.26 panics writing a wide character into a 1-column terminal. A
/// 1-column layout cell still renders; its pane is just 2 wide inside.
const MIN_COLS: u16 = 2;

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
    /// The pty's size, in pixels too when the cell size is known.
    fn winsize(self, cell_px: Option<(u16, u16)>) -> Winsize {
        let (width, height) = cell_px.unwrap_or((0, 0));
        Winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: self.cols.saturating_mul(width),
            ws_ypixel: self.rows.saturating_mul(height),
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

/// Held while spawning any child process (a pane, `run-shell`), so no fork
/// inherits a pty master between its `openpty` and its `FD_CLOEXEC`.
pub(crate) fn spawn_lock() -> MutexGuard<'static, ()> {
    lock(&SPAWN_LOCK)
}

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
    /// Set from the server when an attached client has kitty graphics;
    /// the reader answers the pane's graphics queries from it (T3.8).
    graphics_ok: Arc<AtomicBool>,
    /// The `graphics` option, read by the reader for every chunk (G3).
    images_on: Arc<AtomicBool>,
    /// One cell's size in pixels, for the pty's pixel size.
    cell_px: Mutex<Option<(u16, u16)>>,
    sample: procinfo::Cache,
    /// What the pane itself reported with OSC 7 (T3.3), as tmux keeps it.
    reported_path: Mutex<Option<String>>,
    /// The pty slave's path (`#{pane_tty}`), `None` if it has no name.
    tty: Option<PathBuf>,
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
            cols: spec.cols.max(MIN_COLS),
            rows: spec.rows.max(1),
        };
        let (master, child, tty) = spawn_child(&spec, size)?;
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
            images: {
                let mut images = Store::new(
                    u32::from(size.cols),
                    u32::from(size.rows),
                    spec.history_limit,
                );
                images.set_cell_px(spec.cell_px);
                images
            },
        }));
        let dirty = Arc::new(AtomicBool::new(false));
        let graphics_ok = Arc::new(AtomicBool::new(spec.graphics_ok));
        let images_on = Arc::new(AtomicBool::new(spec.images));
        let sample = procinfo::Cache::default();
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
            scanner: Scanner::default(),
            graphics_ok: Arc::clone(&graphics_ok),
            images_on: Arc::clone(&images_on),
        }));
        let writer = tokio::spawn(io::writer(
            Arc::clone(&master),
            input_rx,
            Arc::clone(&input),
        ));

        Ok(Self {
            pid,
            generation,
            master,
            emu,
            input,
            dirty,
            sample,
            graphics_ok,
            images_on,
            cell_px: Mutex::new(spec.cell_px),
            reported_path: Mutex::new(None),
            tty,
            tasks: vec![reader, writer],
        })
    }

    /// Queue bytes for the pane's writer. Never blocks.
    pub fn enqueue_input(&self, bytes: Vec<u8>) -> Result<(), QueueFull> {
        self.input.push(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> stdio::Result<()> {
        let size = Size {
            cols: cols.max(MIN_COLS),
            rows: rows.max(1),
        };
        // ponytail: alacritty's reflow drops trailing blanks it counts as
        // empty, written ones too ([`written`]), so a line shrunk narrower
        // than its written blanks loses them and `-J` then trims them, where
        // tmux keeps its width. Carrying widths across resizes misplaced them
        // when lines moved or re-wrapped; the loss is the accepted divergence.
        {
            let mut emu = lock(&self.emu);
            let before = emu.term.grid().history_size();
            emu.term.resize(size);
            let after = emu.term.grid().history_size();
            emu.images
                .resized(u32::from(size.cols), u32::from(size.rows), (before, after));
        }
        self.set_winsize(size)
    }

    fn set_winsize(&self, size: Size) -> stdio::Result<()> {
        let winsize = size.winsize(*lock(&self.cell_px));
        // SAFETY: a valid fd we hold and a pointer to a live Winsize.
        unsafe { set_winsize(self.master.get_ref().as_raw_fd(), &winsize) }?;
        Ok(())
    }

    /// One cell's size in pixels, from the client the window is now sized
    /// for; the pty's pixel size follows when it changes.
    pub fn set_cell_px(&self, cell_px: Option<(u16, u16)>) -> stdio::Result<()> {
        {
            let mut current = lock(&self.cell_px);
            if *current == cell_px {
                return Ok(());
            }
            *current = cell_px;
        }
        let size = {
            let mut emu = lock(&self.emu);
            emu.images.set_cell_px(cell_px);
            Size {
                cols: emu.term.columns() as u16,
                rows: emu.term.screen_lines() as u16,
            }
        };
        self.set_winsize(size)
    }

    /// Whether an attached client shows kitty graphics: the reader answers
    /// the pane's graphics queries by it (T3.8), without the actor.
    pub fn set_graphics_ok(&self, ok: bool) {
        self.graphics_ok.store(ok, Ordering::Relaxed);
    }

    /// Turns the `graphics` option on or off for this pane (G3).
    pub fn set_images(&self, on: bool) {
        if !on && self.images_on.swap(false, Ordering::Relaxed) {
            // Not tracked while off: what it held would drift from the text.
            lock(&self.emu).images.reset();
            self.dirty.store(true, Ordering::Release);
        }
        self.images_on.store(on, Ordering::Relaxed);
    }

    /// See [`Self::set_images`].
    pub fn images(&self) -> bool {
        self.images_on.load(Ordering::Relaxed)
    }

    /// See [`Self::set_graphics_ok`].
    pub fn graphics_ok(&self) -> bool {
        self.graphics_ok.load(Ordering::Relaxed)
    }

    /// Lines per tmux's `capture-pane -p`: one line per row, trailing
    /// blanks trimmed, each ending in `\n`. With `escapes` (`-e`), an SGR
    /// sequence from the renderer's `write_sgr` goes wherever the style
    /// changes, carried across lines as tmux does.
    pub fn capture(&self, opts: &CaptureOpts) -> String {
        self.capture_lines(opts, false)
    }

    /// [`Self::capture`] with tmux's `-J` when `join`: a row that wraps
    /// onto the next one is joined to it, and a row that does not ends at
    /// its used width ([`written`]), the blanks the program wrote kept.
    pub fn capture_lines(&self, opts: &CaptureOpts, join: bool) -> String {
        // The SGR-bearing flags; layout flags (wide spacers, wrap) are not
        // a style change.
        let sgr_flags = CellFlags::BOLD
            | CellFlags::DIM
            | CellFlags::ITALIC
            | CellFlags::ALL_UNDERLINES
            | CellFlags::INVERSE
            | CellFlags::HIDDEN
            | CellFlags::STRIKEOUT;
        let caps = Caps {
            truecolor: true,
            styled_underline: true,
            ..Caps::default()
        };
        let mut pen = Style::default();
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
            let wraps = join
                && row[Column(grid.columns() - 1)]
                    .flags
                    .contains(CellFlags::WRAPLINE);
            // Without a wrap, `-J` stops at the used width; a non-blank
            // cell the program did not write (`DECALN`) still counts.
            let cols = grid.columns();
            let tab_stop = |col: usize| ((col / TAB_STOP + 1) * TAB_STOP).min(cols);
            let end = if join && !wraps {
                // A tab counts as far as the stop it moved to (tmux's
                // `cellused`), a blank or not.
                (0..cols)
                    .map(|c| match row[Column(c)].c {
                        ' ' => 0,
                        '\t' => tab_stop(c),
                        _ => c + 1,
                    })
                    .max()
                    .unwrap_or(0)
                    .max(written::used(row))
            } else {
                grid.columns()
            };
            let mut text = String::new();
            // A tab is its `\t` alone, as tmux captures it: the blank cells
            // it moved over, up to the next tab stop, are not printed. One
            // written over before its stop is a blank, like those cells.
            let mut tab_end = 0;
            for col in 0..end {
                let cell = &row[Column(col)];
                if cell.flags.intersects(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if col < tab_end && cell.c == ' ' && !cell.flags.contains(written::WRITTEN) {
                    continue;
                }
                if opts.escapes {
                    let style = Style {
                        fg: cell.fg,
                        bg: cell.bg,
                        attrs: cell.flags & sgr_flags,
                        underline_color: cell.underline_color(),
                    };
                    if style != pen {
                        let mut sgr = Vec::new();
                        write_sgr(&mut sgr, &style, &caps);
                        text.push_str(&String::from_utf8_lossy(&sgr));
                        pen = style;
                    }
                }
                if cell.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER) {
                    text.push(' ');
                    continue;
                }
                if cell.c == '\t' {
                    let stop = tab_stop(col);
                    let intact = (col + 1..stop).all(|c| {
                        let cell = &row[Column(c)];
                        cell.c == ' ' && !cell.flags.contains(written::WRITTEN)
                    });
                    if intact {
                        text.push('\t');
                        tab_end = stop;
                    } else {
                        text.push(' ');
                    }
                } else {
                    text.push(cell.c);
                }
                if let Some(zw) = cell.zerowidth() {
                    text.extend(zw);
                }
            }
            if wraps {
                out.push_str(&text);
                continue;
            }
            if join {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end_matches(' '));
            }
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

    /// The foreground process's name, sampled now (cached for a few ms).
    pub fn current_command(&self) -> Option<String> {
        self.sample.get(self.master.get_ref()).command
    }

    /// The foreground process's full argv, sampled now (cached for a few ms).
    pub fn current_argv(&self) -> Option<Vec<String>> {
        self.sample.get(self.master.get_ref()).argv
    }

    /// The pty's foreground process group, read now rather than sampled
    /// ([`procinfo::foreground_group`]): for a command's formats, which must
    /// see the process in front at the moment the key is pressed.
    pub fn foreground_group(&self) -> Option<i32> {
        procinfo::foreground_group(self.master.get_ref())
    }

    /// The foreground process's cwd, read now (else the last sample's):
    /// tmux's `#{pane_current_path}` (`osdep_get_cwd`), never what the
    /// pane reported, since that text is the pane's own.
    pub fn current_path(&self) -> Option<PathBuf> {
        self.foreground_group()
            .and_then(procinfo::cwd)
            .or_else(|| self.sample.get(self.master.get_ref()).path)
    }

    /// What the pane last reported with OSC 7: `#{pane_path}`.
    pub fn reported_path(&self) -> Option<String> {
        lock(&self.reported_path).clone()
    }

    /// Records what the pane reported with OSC 7.
    pub fn set_reported_path(&self, path: String) {
        *lock(&self.reported_path) = Some(path);
    }

    /// The pty slave's path, as `#{pane_tty}` shows it.
    pub fn tty_path(&self) -> Option<&Path> {
        self.tty.as_deref()
    }

    /// Scrollback lines kept above the visible screen.
    pub fn history_size(&self) -> usize {
        lock(&self.emu).term.grid().history_size()
    }

    /// The pane's kitty placements in the rows [`Self::copy_grid`] gives:
    /// row 0 is the oldest history row (copy mode's view of its images).
    pub fn copy_images(&self) -> Option<PaneImages> {
        let emu = lock(&self.emu);
        emu.images.copy_view(emu.term.grid().history_size())
    }

    /// Every row of the grid, the oldest history row first and then the
    /// screen, each passed to `row`, under one lock: copy mode's copy of
    /// the pane (M5, TM.1). Returns the number of history rows and the
    /// cursor.
    /// With each row goes which of its cells an intact tab covers (the
    /// blanks it moved over to its stop, as [`Self::capture_lines`] reads
    /// a tab), which copy mode treats as tmux treats a tab's padding.
    /// Both are buffers reused for the next row.
    pub fn copy_grid(&self, mut row: impl FnMut(&mut [SnapCell], &[bool])) -> (usize, CursorState) {
        let emu = lock(&self.emu);
        let grid = emu.term.grid();
        let history = grid.history_size();
        let cols = grid.columns();
        let mut cells = Vec::with_capacity(cols);
        let mut covered = vec![false; cols];
        for r in -(history as i32)..grid.screen_lines() as i32 {
            let line = &grid[Line(r)];
            covered.fill(false);
            for col in 0..cols {
                if line[Column(col)].c != '\t' || covered[col] {
                    continue;
                }
                let stop = ((col / TAB_STOP + 1) * TAB_STOP).min(cols);
                let intact = (col + 1..stop).all(|c| {
                    let cell = &line[Column(c)];
                    cell.c == ' ' && !cell.flags.contains(written::WRITTEN)
                });
                if intact {
                    covered[col + 1..stop].fill(true);
                }
            }
            snap_row_into(&mut cells, line, cols);
            row(&mut cells, &covered);
        }
        (history, cursor_state(&emu.term))
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
            .map(|r| snap_row(&grid[Line(r as i32)], grid.columns()))
            .collect();
        RowsSnapshot {
            rows,
            cursor,
            images: emu.images.view(),
        }
    }
}

fn snap_row(
    row: &alacritty_terminal::grid::Row<term::cell::Cell>,
    columns: usize,
) -> Vec<SnapCell> {
    let mut cells = Vec::with_capacity(columns);
    snap_row_into(&mut cells, row, columns);
    cells
}

/// [`snap_row`] into `cells`, replacing what it held.
fn snap_row_into(
    cells: &mut Vec<SnapCell>,
    row: &alacritty_terminal::grid::Row<term::cell::Cell>,
    columns: usize,
) {
    cells.clear();
    cells.extend((0..columns).map(|c| {
        let cell = &row[Column(c)];
        SnapCell {
            c: cell.c,
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags.difference(written::WRITTEN),
            zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
            underline_color: cell.underline_color(),
            hyperlink: cell.hyperlink(),
        }
    }));
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

/// The environment a pane's child gets: the spawn env, a UTF-8 locale if
/// it sets none, then grofe's variables.
fn child_env(spec: &PaneSpawn) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> =
        spec.env.iter().map(|(k, v)| (k.into(), v.into())).collect();
    let has_locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|k| {
        let set = |v: &str| !v.is_empty();
        spec.env
            .iter()
            .rev()
            .find(|(name, _)| name == k)
            .is_some_and(|(_, v)| set(v))
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
/// server's ([`pick_shell`]).
fn default_shell(spec: &PaneSpawn) -> String {
    pick_shell(
        spec.env
            .iter()
            .rev()
            .find(|(k, _)| k == "SHELL")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("SHELL").ok()),
    )
}

/// `candidate` when it is an executable absolute path (tmux's
/// `checkshell`), else the user's passwd shell when that is, else
/// `/bin/sh`, as tmux's `getshell` falls back (spec D2 in the M3.6 ledger).
pub fn pick_shell(candidate: Option<String>) -> String {
    let usable = |s: &String| s.starts_with('/') && access(s.as_str(), AccessFlags::X_OK).is_ok();
    candidate
        .filter(usable)
        .or_else(|| passwd_shell().filter(usable))
        .unwrap_or_else(|| "/bin/sh".into())
}

/// The login shell in the passwd entry of the server's user.
fn passwd_shell() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
    user.shell.into_os_string().into_string().ok()
}

/// The directory a pane starts in: `cwd` if it is one the server may enter
/// (search permission, so an EACCES directory does not leave a dead pane),
/// else `$HOME`, else `/`, as tmux falls back when it cannot `chdir`.
pub fn usable_cwd(cwd: PathBuf) -> PathBuf {
    if cwd.is_dir() && access(&cwd, AccessFlags::X_OK).is_ok() {
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
) -> stdio::Result<(OwnedFd, Option<tokio::process::Child>, Option<PathBuf>)> {
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
    cmd.current_dir(&spec.cwd).env_clear().envs(child_env(spec));

    let _guard = spawn_lock();
    let pty = openpty(&size.winsize(spec.cell_px), None)?;
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
    let tty = nix::unistd::ttyname(&pty.slave).ok();
    drop(pty.slave);
    drop(_guard);

    let flags = OFlag::from_bits_truncate(fcntl(&pty.master, FcntlArg::F_GETFL)?);
    fcntl(&pty.master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok((pty.master, child, tty))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_term(term: &'static str) -> PaneSpawn {
        PaneSpawn {
            id: PaneId(0),
            session: SessionId(0),
            argv: None,
            cwd: PathBuf::from("/"),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            history_limit: 100,
            socket: PathBuf::from("/tmp/grofe-test.sock"),
            term,
            cell_px: None,
            graphics_ok: false,
            images: false,
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

    /// T3.0's pane seams for the scrollback view (T3.2): lines scrolled off
    /// the top are counted. (The view became copy mode in TM.6, which reads
    /// the history through `copy_grid`; the offset read-back went with it.)
    #[tokio::test]
    async fn lines_scrolled_off_are_counted() {
        let (tx, mut rx) = mpsc::channel(8);
        let spec = PaneSpawn {
            rows: 10,
            ..spec_for(&["seq", "1", "30"])
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        let exited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(ev) = rx.recv().await {
                if matches!(ev.kind, PaneEventKind::Exited(_)) {
                    return;
                }
            }
        })
        .await;
        assert!(exited.is_ok(), "seq exited");
        // The exit can be seen before the reader has parsed the last
        // output, so wait for it under load.
        for _ in 0..500 {
            if handle.history_size() == 21 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // 30 lines and the empty one after them, 10 rows on screen.
        assert_eq!(handle.history_size(), 21);
    }

    fn winsize_of(handle: &PaneHandle) -> (u16, u16, u16, u16) {
        let mut ws = Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: a valid fd we hold and a pointer to a live Winsize.
        let rc = unsafe {
            libc::ioctl(
                handle.master.get_ref().as_raw_fd(),
                libc::TIOCGWINSZ,
                &mut ws,
            )
        };
        assert_eq!(rc, 0, "TIOCGWINSZ");
        (ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel)
    }

    /// The pty's pixel size is its cells times the client's cell size,
    /// from the spawn on, through resizes and cell size changes; unknown
    /// is 0.
    #[tokio::test]
    async fn the_pty_pixel_size_follows_the_cell_size() {
        let (tx, _rx) = mpsc::channel(8);
        let spec = PaneSpawn {
            cell_px: Some((10, 20)),
            ..spec_for(&["sleep", "30"])
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        assert_eq!(winsize_of(&handle), (80, 24, 800, 480));
        handle.resize(40, 10).expect("resize");
        assert_eq!(winsize_of(&handle), (40, 10, 400, 200));
        handle.set_cell_px(Some((8, 16))).expect("cell size");
        assert_eq!(winsize_of(&handle), (40, 10, 320, 160));
        handle.set_cell_px(None).expect("unknown");
        assert_eq!(winsize_of(&handle), (40, 10, 0, 0));
        let (tx, _rx) = mpsc::channel(8);
        let plain = PaneHandle::spawn(spec_for(&["sleep", "30"]), tx, Arc::new(Notify::new()))
            .expect("spawn");
        assert_eq!(winsize_of(&plain), (80, 24, 0, 0));
    }

    /// What the pane reported (OSC 7, T3.3) is kept apart from the
    /// sampled cwd, which alone is `#{pane_current_path}`, as in tmux.
    #[tokio::test]
    async fn a_reported_path_is_kept_apart() {
        let (tx, _rx) = mpsc::channel(8);
        let handle = PaneHandle::spawn(spec_for(&["sleep", "30"]), tx, Arc::new(Notify::new()))
            .expect("spawn");
        let there = "file:///nonexistent/grofe-reported".to_string();
        handle.set_reported_path(there.clone());
        assert_ne!(
            handle.current_path(),
            Some(PathBuf::from("/nonexistent/grofe-reported"))
        );
        assert_eq!(handle.reported_path(), Some(there));
    }

    /// D2: a stale `SHELL` falls back to the passwd shell, as tmux's
    /// `getshell` does, and to `/bin/sh` only when that is unusable too.
    #[test]
    fn a_stale_shell_falls_back_to_the_passwd_shell() {
        let spec = |shell: &str| PaneSpawn {
            env: vec![("SHELL".to_string(), shell.to_string())],
            ..spec_with_term("xterm-256color")
        };
        let passwd = passwd_shell()
            .filter(|s| s.starts_with('/') && access(s.as_str(), AccessFlags::X_OK).is_ok())
            .unwrap_or_else(|| "/bin/sh".into());
        assert_eq!(default_shell(&spec("/nonexistent/grofe-shell")), passwd);
        assert_eq!(default_shell(&spec("sh")), passwd);
        assert_eq!(default_shell(&spec("/etc/hosts")), passwd);
        assert_eq!(default_shell(&spec("/bin/sh")), "/bin/sh");
        let sh = std::fs::canonicalize("/bin/sh")
            .unwrap()
            .display()
            .to_string();
        assert_eq!(default_shell(&spec(&sh)), sh);
    }

    /// D2: with no `SHELL` anywhere, the passwd entry's shell.
    #[test]
    fn no_shell_at_all_is_the_passwd_shell() {
        let user = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .expect("a passwd entry");
        let shell = user.shell.display().to_string();
        assert!(shell.starts_with('/'), "{shell}");
        assert_eq!(pick_shell(None), shell);
    }

    /// D5: a directory the server cannot enter (EACCES) falls back to
    /// `$HOME`, where `is_dir` alone kept it and the pane died.
    #[test]
    fn an_unenterable_cwd_falls_back_to_home() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            return; // root enters any directory
        }
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(locked.is_dir());
        let got = usable_cwd(locked.clone());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(got, home);
        assert_eq!(usable_cwd(dir.path().to_path_buf()), dir.path());
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
