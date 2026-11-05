//! PaneHandle, spawn, pre_exec.
//!
//! A pane is a child process on a pty, fed through `alacritty_terminal`. Each
//! pane runs two tokio tasks (spec §4.8): the reader (owns the master's read
//! side, the emulator's listener, the sync timeout and dead detection) and the
//! writer (owns writes to the master through a bounded queue). The foreground
//! command, argv and cwd are sampled on demand ([`procinfo::Cache`]).

mod capture_sgr;
pub mod io;
pub mod marks;
mod pipe;
pub mod procinfo;
mod written;

use std::ffi::OsString;
use std::io as stdio;
use std::ops::Range;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{self, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape as VteCursorShape, Processor, Rgb};
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::libc;
use nix::pty::{Winsize, openpty};
use nix::unistd::{AccessFlags, access};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

pub use alacritty_terminal::term::cell::Flags as CellFlags;
pub use alacritty_terminal::term::cell::Hyperlink;

use crate::server::imgstore::{PaneImages, Store};
use crate::server::passthrough::Scanner;
use crate::types::{
    CaptureBound, CaptureOpts, InputModes, MouseEncoding, MouseMode, PaneId, SessionId,
};
use io::{InputQueue, Listener};
use marks::{HISTORY_SLACK, Marks};
use pipe::Pipe;

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
    /// OSC 52 query from the scanner: the selection parameter, and whether
    /// the pane ended it with BEL (else ST), which the answer copies.
    ClipboardQuery {
        param: String,
        bel: bool,
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
    /// What to run, as tmux's `spawn_pane`: one word is a command string for
    /// `shell -c`, several are executed directly, none starts `shell`.
    pub argv: Option<Vec<String>>,
    /// The shell (`default-shell` once tmux's `checkshell` passed it, else
    /// `/bin/sh`): what a one-word `argv` runs through and what no `argv`
    /// starts.
    pub shell: String,
    /// With no `argv`, whether `shell` is a login shell (`-zsh` as its
    /// `argv[0]`, as a pane's); a popup's runs plain.
    pub login: bool,
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
    /// The server's identity, which names it in the XTVERSION reply.
    pub identity: crate::compat::Identity,
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
    /// The OSC 133 marks of the normal screen's lines.
    pub(crate) marks: Marks,
    /// The spawn whose reader may feed this emulator: a respawn hands it to
    /// the new reader, and the old one's last read must not land on it.
    pub(crate) generation: u64,
}

/// What a respawned pane's emulator is sent: the modes tmux's `screen_reinit`
/// resets (it leaves a screen with the cursor shown and wrapping on, nothing
/// else), the tab stops, the character sets and the kitty keyboard flags.
const REINIT: &[u8] = b"\x1b[?1049l\x1b[0m\x1b[?25h\x1b[?7h\x1b[4l\x1b[?6l\x1b[?1l\x1b>\
    \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?2004l\
    \x1b[?2026l\x1b[r\x1b[?5W\x1b(B\x0f\x1b[<99u\x1b[=0;1u\x1b[H";

impl Emu {
    /// tmux's `screen_reinit`: the visible lines (and their marks), the
    /// cursor, modes, tab stops and hyperlinks are reset; the scrollback and
    /// the title stay.
    /// The pane's image placements go too, as a kitty terminal drops its
    /// images on a reset.
    fn reinit(&mut self, generation: u64) {
        self.generation = generation;
        let Emu {
            term,
            parser,
            marks,
            images,
            ..
        } = self;
        *parser = Processor::new();
        parser.advance(&mut written::Written(term, marks, None), REINIT);
        images.reset();
        let history = term.grid().history_size();
        term.grid_mut().reset_region(..);
        marks.erase_from(history);
        forget_hyperlinks(term);
    }
}

/// `screen_reset_hyperlinks`: no cell, and not the one being written, links
/// anywhere any more.
fn forget_hyperlinks(term: &mut Term<Listener>) {
    alacritty_terminal::vte::ansi::Handler::set_hyperlink(term, None);
    let grid = term.grid_mut();
    for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
        for col in 0..grid.columns() {
            let cell = &mut grid[Line(line)][Column(col)];
            if cell.hyperlink().is_some() {
                cell.set_hyperlink(None);
            }
        }
    }
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

/// How long a held resize waits after the pty last learned its size
/// (tmux's 250 ms pane `resize_timer`).
const RESIZE_HOLD: Duration = Duration::from_millis(250);

/// The sizes a held resize ([`PaneHandle::resize_held`]) has not told the
/// pty yet, as tmux's pane `resize_queue`: a mouse drag resizes the pane
/// on every report, and its program hears of it at most every
/// [`RESIZE_HOLD`].
#[derive(Debug, Default)]
struct WinsizeGate {
    /// What the pty was last told (rows, cols, x and y pixels).
    sent: Option<[u16; 4]>,
    /// Held sizes wait until then.
    hold_until: Option<Instant>,
    /// The held sizes, oldest first.
    queue: Vec<[u16; 4]>,
    /// A task is waiting to tell the pty the queue.
    flushing: bool,
}

fn winsize_key(ws: &Winsize) -> [u16; 4] {
    [ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel]
}

fn send_winsize(master: &AsyncFd<OwnedFd>, key: [u16; 4]) -> stdio::Result<()> {
    let [ws_row, ws_col, ws_xpixel, ws_ypixel] = key;
    let ws = Winsize {
        ws_row,
        ws_col,
        ws_xpixel,
        ws_ypixel,
    };
    // SAFETY: a valid fd we hold and a pointer to a live Winsize.
    unsafe { set_winsize(master.get_ref().as_raw_fd(), &ws) }?;
    Ok(())
}

/// Tells the pty `gate`'s held sizes once their wait is over, as tmux's
/// `server_client_check_pane_resize`: the newest, unless the sizes came
/// back to the one the pty has, which takes the one before it now and the
/// newest 10 ms later so the program still redraws.
async fn flush_winsize(master: Arc<AsyncFd<OwnedFd>>, gate: Arc<Mutex<WinsizeGate>>) {
    loop {
        let wait = lock(&gate)
            .hold_until
            .map(|t| t.saturating_duration_since(Instant::now()));
        // A resize told in the meantime puts the wait off again.
        if let Some(wait) = wait.filter(|w| !w.is_zero()) {
            tokio::time::sleep(wait).await;
            continue;
        }
        let mut g = lock(&gate);
        let queue = std::mem::take(&mut g.queue);
        let (key, rest, hold) = match queue.as_slice() {
            [] => {
                g.flushing = false;
                return;
            }
            [.., before, last] if Some(*last) == g.sent => {
                (*before, vec![*last], Duration::from_millis(10))
            }
            [.., last] => (*last, Vec::new(), RESIZE_HOLD),
        };
        if let Err(e) = send_winsize(&master, key) {
            eprintln!("grofe server: resizing a pane's pty: {e}");
        }
        g.sent = Some(key);
        g.hold_until = Some(Instant::now() + hold);
        g.queue = rest;
    }
}

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
pub(crate) fn nofile_limit() -> u32 {
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
pub(crate) fn cloexec_inherited_fds(limit: u32) {
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
/// What `capture-pane`'s `-L`, `-F`, `-C` and `-H` add to a capture.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureExtra {
    /// `-L`: each row starts with its line number.
    pub numbered: bool,
    /// `-F`: each row starts with its flags (`H`, `O`, `P`, `W`, or `-`).
    pub flags: bool,
    /// `-C`: a backslash is `\\\\` and an ESC of `-e` is `\033`.
    pub escaped: bool,
    /// `-H`: the rows list their hyperlinks instead of their text.
    pub links: bool,
}

pub struct PaneHandle {
    pid: Option<u32>,
    generation: u64,
    master: Arc<AsyncFd<OwnedFd>>,
    emu: Arc<Mutex<Emu>>,
    /// What the emulator's listener has queued for the reader.
    events: io::EventBuf,
    input: Arc<InputQueue>,
    /// The pane's `pipe-pane` command, if any; it survives a respawn.
    pipe: Arc<Pipe>,
    dirty: Arc<AtomicBool>,
    /// Set from the server when an attached client has kitty graphics;
    /// the reader answers the pane's graphics queries from it (T3.8).
    graphics_ok: Arc<AtomicBool>,
    /// The `graphics` option, read by the reader for every chunk (G3).
    images_on: Arc<AtomicBool>,
    /// The foreground and background `window-style` gives the pane (see
    /// [`io::pack_colour`]): what its OSC 10 and OSC 11 queries are told.
    default_colours: Arc<[AtomicU32; 2]>,
    /// One cell's size in pixels, for the pty's pixel size.
    cell_px: Mutex<Option<(u16, u16)>>,
    sample: procinfo::Cache,
    /// What the pane itself reported with OSC 7 (T3.3), as tmux keeps it.
    reported_path: Mutex<Option<String>>,
    /// The pty slave's path (`#{pane_tty}`), `None` if it has no name.
    tty: Option<PathBuf>,
    /// Held resizes not yet told to the pty.
    winsize: Arc<Mutex<WinsizeGate>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for PaneHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.pipe.close_for(self.generation);
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
        Self::spawn_inner(spec, to_actor, wake, None)
    }

    /// [`Self::spawn`] for `respawn-pane`: the new process runs in `old`'s
    /// emulator, reinitialised as tmux's `screen_reinit` does ([`Emu::reinit`]),
    /// so the scrollback (and its prompt marks) stays.
    pub fn spawn_over(
        spec: PaneSpawn,
        to_actor: mpsc::Sender<PaneEvent>,
        wake: Arc<Notify>,
        old: &PaneHandle,
    ) -> stdio::Result<Self> {
        Self::spawn_inner(spec, to_actor, wake, Some(old))
    }

    fn spawn_inner(
        spec: PaneSpawn,
        to_actor: mpsc::Sender<PaneEvent>,
        wake: Arc<Notify>,
        over: Option<&PaneHandle>,
    ) -> stdio::Result<Self> {
        let size = Size {
            cols: spec.cols.max(MIN_COLS),
            rows: spec.rows.max(1),
        };
        let (master, slave, child, tty) = spawn_child(&spec, size)?;
        let pid = child.as_ref().and_then(|c| c.id());
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let master = Arc::new(AsyncFd::new(master)?);

        let (events, emu) = match over {
            Some(old) => {
                // Its reader must not feed what it already read after the
                // reinitialisation.
                for task in &old.tasks {
                    task.abort();
                }
                lock(&old.emu).reinit(generation);
                // What the old process's output queued (a reply to its
                // query) is not for the new one.
                lock(&old.events).clear();
                (Arc::clone(&old.events), Arc::clone(&old.emu))
            }
            None => {
                let events = Arc::<Mutex<Vec<_>>>::default();
                let config = term::Config {
                    // `Marks::sync` holds the history to the limit, counting
                    // the rows it drops; the emulator never drops one on its
                    // own.
                    scrolling_history: spec.history_limit.saturating_add(HISTORY_SLACK),
                    // Off by default in alacritty_terminal: push/pop/query
                    // would be ignored and a pane could never ask for kitty
                    // keys (spec §4.7).
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
                    marks: Marks::new(spec.history_limit),
                    generation,
                }));
                (events, emu)
            }
        };
        let dirty = Arc::new(AtomicBool::new(false));
        let graphics_ok = Arc::new(AtomicBool::new(spec.graphics_ok));
        let images_on = Arc::new(AtomicBool::new(spec.images));
        let default_colours = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let sample = procinfo::Cache::default();
        let (input, input_rx) = InputQueue::new();
        let pipe = match over {
            Some(old) => {
                old.pipe.adopt(Arc::clone(&input), generation);
                Arc::clone(&old.pipe)
            }
            None => Pipe::new(Arc::clone(&input), generation),
        };

        let reader = tokio::spawn(io::reader(io::Reader {
            id: spec.id,
            generation,
            master: Arc::clone(&master),
            slave,
            child,
            emu: Arc::clone(&emu),
            events: Arc::clone(&events),
            input: Arc::clone(&input),
            pipe: Arc::clone(&pipe),
            dirty: Arc::clone(&dirty),
            wake,
            to_actor,
            scanner: Scanner::default(),
            xtversion: crate::compat::xtversion_reply(spec.identity),
            graphics_ok: Arc::clone(&graphics_ok),
            images_on: Arc::clone(&images_on),
            default_colours: Arc::clone(&default_colours),
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
            events,
            input,
            pipe,
            dirty,
            sample,
            graphics_ok,
            images_on,
            default_colours,
            cell_px: Mutex::new(spec.cell_px),
            // A respawned pane keeps what it reported, as tmux's does.
            reported_path: Mutex::new(over.and_then(PaneHandle::reported_path)),
            tty,
            winsize: Arc::default(),
            tasks: vec![reader, writer],
        })
    }

    /// Whether a `pipe-pane` command is tied to the pane (`#{pane_pipe}`).
    pub fn pipe_open(&self) -> bool {
        self.pipe.is_open()
    }

    /// The pid of the pane's `pipe-pane` command (`#{pane_pipe_pid}`).
    pub fn pipe_pid(&self) -> Option<u32> {
        self.pipe.pid()
    }

    /// Closes the pane's `pipe-pane` command's pipe, if any.
    pub fn pipe_close(&self) {
        self.pipe.close();
    }

    /// Ties `command` to the pane: `output` feeds it what the pane's program
    /// writes, `input` types what it writes into the pane. Needs a tokio
    /// runtime; closes nothing, so [`Self::pipe_close`] first.
    pub fn pipe_start(&self, command: &str, input: bool, output: bool) -> stdio::Result<()> {
        self.pipe.start(command, input, output)
    }

    /// Queue bytes for the pane's writer. Never blocks.
    pub fn enqueue_input(&self, bytes: Vec<u8>) -> Result<(), QueueFull> {
        self.input.push(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> stdio::Result<()> {
        let size = self.resize_screen(cols, rows);
        self.set_winsize(size)
    }

    /// [`PaneHandle::resize`] for a mouse drag's report: the screen now,
    /// the pty (and so the program's `SIGWINCH`) at most every
    /// [`RESIZE_HOLD`], always ending at the last size ([`WinsizeGate`]).
    pub fn resize_held(&self, cols: u16, rows: u16) -> stdio::Result<()> {
        let size = self.resize_screen(cols, rows);
        let key = winsize_key(&size.winsize(*lock(&self.cell_px)));
        let mut g = lock(&self.winsize);
        let held = g.flushing || g.hold_until.is_some_and(|t| Instant::now() < t);
        let runtime = tokio::runtime::Handle::try_current().ok();
        let Some(runtime) = runtime.filter(|_| held) else {
            send_winsize(&self.master, key)?;
            g.sent = Some(key);
            g.hold_until = Some(Instant::now() + RESIZE_HOLD);
            return Ok(());
        };
        if g.queue.last().or(g.sent.as_ref()) != Some(&key) {
            g.queue.push(key);
        }
        if !g.flushing && !g.queue.is_empty() {
            g.flushing = true;
            runtime.spawn(flush_winsize(self.master.clone(), self.winsize.clone()));
        }
        Ok(())
    }

    /// The emulator resized to `cols` by `rows` (at least the minimum).
    fn resize_screen(&self, cols: u16, rows: u16) -> Size {
        let size = Size {
            cols: cols.max(MIN_COLS),
            rows: rows.max(1),
        };
        // ponytail: alacritty's reflow drops trailing blanks it counts as
        // empty, written ones too ([`written`]), so a line shrunk narrower
        // than its written blanks loses them and `-J` then trims them, where
        // tmux keeps its width. Carrying widths across resizes misplaced them
        // when lines moved or re-wrapped; the loss is the accepted divergence.
        let mut emu = lock(&self.emu);
        let Emu {
            term,
            marks,
            images,
            ..
        } = &mut *emu;
        let before = term.grid().history_size();
        marks.resize(term, size);
        let after = term.grid().history_size();
        images.resized(u32::from(size.cols), u32::from(size.rows), (before, after));
        size
    }

    /// `clear-history`: drops the scrollback and the marks of its lines
    /// (tmux's `grid_clear_history`); `hyperlinks` (`-H`) also forgets every
    /// hyperlink, as `screen_reset_hyperlinks` does.
    // ponytail: on the alternate screen alacritty's grid holds no scrollback,
    // so the normal screen's is left (and its image placements with it).
    pub fn clear_history(&self, hyperlinks: bool) {
        let mut emu = lock(&self.emu);
        let Emu {
            term,
            marks,
            images,
            ..
        } = &mut *emu;
        let history = term.grid().history_size();
        term.grid_mut().clear_history();
        marks.trim(history);
        images.history_cleared();
        if hyperlinks {
            forget_hyperlinks(term);
        }
        drop(emu);
        self.dirty.store(true, Ordering::Release);
    }

    /// `send-keys -R`, as tmux's `input_reset` with `screen_write_reset`:
    /// the parser leaves any half-read sequence, the modes go back to
    /// their start (cursor shown, wrapping, nothing else), tab stops
    /// and the scroll region reset, and the screen is cleared (into the
    /// scrollback) with the cursor home.
    pub fn reset_state(&self) {
        // `screen_write_reset` leaves only MODE_CURSOR|MODE_WRAP, so the
        // kitty keyboard flags, synchronized output and the character sets
        // go too (as [`REINIT`], but the alternate screen stays and the
        // screen is cleared). `screen_write_clearscreen` drops the images.
        const RESET: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?7h\x1b[4l\x1b[20l\x1b[?6l\x1b[?1l\x1b>\
            \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?2004l\
            \x1b[?2026l\x1b[r\x1b[?5W\x1b(B\x0f\x1b[<99u\x1b[=0;1u\x1b[2J\x1b[H";
        let mut emu = lock(&self.emu);
        let Emu {
            term,
            marks,
            parser,
            images,
            ..
        } = &mut *emu;
        // `input_reset`: the parser is back at ground.
        *parser = Processor::new();
        parser.advance(&mut written::Written(term, marks, None), RESET);
        images.reset();
        drop(emu);
        self.dirty.store(true, Ordering::Release);
    }

    /// Tells the pty `size` now, dropping any held size ([`WinsizeGate`]).
    fn set_winsize(&self, size: Size) -> stdio::Result<()> {
        let key = winsize_key(&size.winsize(*lock(&self.cell_px)));
        let mut g = lock(&self.winsize);
        g.queue.clear();
        send_winsize(&self.master, key)?;
        g.sent = Some(key);
        g.hold_until = Some(Instant::now() + RESIZE_HOLD);
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

    /// The colours `window-style` gives the pane's default foreground and
    /// background, `None` for the terminal's: the reader answers OSC 10 and
    /// OSC 11 queries with them, as tmux does.
    pub fn set_default_colours(&self, fg: Option<Rgb>, bg: Option<Rgb>) {
        self.default_colours[0].store(io::pack_colour(fg), Ordering::Relaxed);
        self.default_colours[1].store(io::pack_colour(bg), Ordering::Relaxed);
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
        self.capture_blanks(opts, join, false, false)
    }

    /// [`Self::capture_lines`] with `-N` (`keep_blanks`: the blanks a line
    /// has, up to the cells tmux allocated for it, are kept) and `-T`
    /// (`written_only`: a line ends at its last written cell). Both
    /// together are `-J` without the joining; `join` overrides both.
    // ponytail: tmux allocates a line's cells in steps of a quarter, a half
    // and the whole width as its writes reach them, so a line written in
    // several runs can have more or fewer than the step its used width
    // alone gives, which is what `-N` pads to here.
    pub fn capture_blanks(
        &self,
        opts: &CaptureOpts,
        join: bool,
        keep_blanks: bool,
        written_only: bool,
    ) -> String {
        self.capture_rows(
            opts,
            join,
            keep_blanks,
            written_only,
            CaptureExtra::default(),
        )
    }

    /// [`Self::capture_blanks`] with the flags that change what a row
    /// starts with or holds (`-L`, `-F`, `-C`, `-H`).
    pub fn capture_rows(
        &self,
        opts: &CaptureOpts,
        join: bool,
        keep_blanks: bool,
        written_only: bool,
        extra: CaptureExtra,
    ) -> String {
        let mut pen = capture_sgr::Pen::default();
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
        // `-H`: the links already listed, across rows.
        let mut listed: Vec<(String, String)> = Vec::new();
        for line in start..=end {
            let row = &grid[Line(line)];
            let wrapped = row[Column(grid.columns() - 1)]
                .flags
                .contains(CellFlags::WRAPLINE);
            let wraps = join && wrapped;
            let linked = (0..grid.columns()).any(|c| row[Column(c)].hyperlink().is_some());
            let mut prefix = String::new();
            let mut link_text = None;
            if extra.links {
                let mut uris: Vec<String> = Vec::new();
                for c in 0..grid.columns() {
                    let Some(link) = row[Column(c)].hyperlink() else {
                        continue;
                    };
                    let key = (link.id().to_string(), link.uri().to_string());
                    if !listed.contains(&key) {
                        uris.push(key.1.clone());
                        listed.push(key);
                    }
                }
                // A row with no new link is not listed at all.
                if uris.is_empty() {
                    continue;
                }
                link_text = Some(uris.join(" "));
            }
            if extra.numbered {
                prefix.push_str(&format!("{line} "));
            }
            if extra.flags {
                // tmux's `grid_line` flags: the dead ones never show; a
                // prompt and output start come from OSC 133, a wrap from the
                // row, a hyperlink from any cell.
                let bits = emu.marks.get((line + history) as usize);
                let mut letters = String::new();
                for (on, letter) in [
                    (linked, 'H'),
                    (bits & marks::OUTPUT != 0, 'O'),
                    (bits & marks::PROMPT != 0, 'P'),
                    (wrapped, 'W'),
                    // Cells with a hyperlink are extended cells in tmux.
                    (linked, 'X'),
                ] {
                    if on {
                        letters.push(letter);
                    }
                }
                if letters.is_empty() {
                    letters.push('-');
                }
                prefix.push_str(&letters);
                prefix.push(' ');
            }
            // Without a wrap, `-J` stops at the used width; a non-blank
            // cell the program did not write (`DECALN`) still counts.
            let cols = grid.columns();
            let tab_stop = |col: usize| ((col / TAB_STOP + 1) * TAB_STOP).min(cols);
            let used = || {
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
            };
            // `-J` leaves out the empty cells and the trimming; so do `-T`
            // and `-N` for their half.
            let empty_cells = !join && !written_only;
            let trim = !join && !keep_blanks;
            let end = if join && wraps {
                cols
            } else if !empty_cells {
                used()
            } else if trim {
                cols
            } else {
                allocated(used(), cols)
            };
            let mut text = String::new();
            let mut has_link = false;
            // A tab is its `\t` alone, as tmux captures it: the blank cells
            // it moved over, up to the next tab stop, are not printed. One
            // written over before its stop is a blank, like those cells.
            let mut tab_end = 0;
            // `-C`: an `-e` code's ESC is written as text, `\033`, and the
            // backslash of an OSC 8's `ESC \` is doubled as in `-C` text.
            let esc = |code: String| {
                if extra.escaped {
                    code.replace("\x1b\\", "\\033\\\\").replace('\x1b', "\\033")
                } else {
                    code
                }
            };
            for col in 0..end {
                let cell = &row[Column(col)];
                if cell.flags.intersects(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if col < tab_end && cell.c == ' ' && !cell.flags.contains(written::WRITTEN) {
                    continue;
                }
                if opts.escapes {
                    let now = capture_sgr::Pen::of(cell);
                    let code = capture_sgr::code(&pen, &now, &mut has_link);
                    text.push_str(&esc(code));
                    pen = now;
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
                } else if extra.escaped && cell.c == '\\' {
                    text.push_str("\\\\");
                } else {
                    text.push(cell.c);
                }
                if let Some(zw) = cell.zerowidth() {
                    text.extend(zw);
                }
            }
            if has_link {
                text.push_str(&esc(capture_sgr::close_link()));
            }
            if let Some(links) = link_text {
                text = links;
            }
            out.push_str(&prefix);
            if wraps {
                out.push_str(&text);
                continue;
            }
            if trim {
                out.push_str(text.trim_end_matches(' '));
            } else {
                out.push_str(&text);
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
    /// Both are buffers reused for the next row. The third is the row's
    /// OSC 133 [`marks`] bits.
    pub fn copy_grid(
        &self,
        mut row: impl FnMut(&mut [SnapCell], &[bool], u8),
    ) -> (usize, CursorState) {
        let emu = lock(&self.emu);
        let grid = emu.term.grid();
        let history = grid.history_size();
        let cols = grid.columns();
        // The marks are the normal screen's.
        let marked = !emu.term.mode().contains(TermMode::ALT_SCREEN);
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
            let bits = if marked {
                emu.marks.get((r + history as i32) as usize)
            } else {
                0
            };
            row(&mut cells, &covered, bits);
        }
        (history, cursor_state(&emu.term))
    }

    pub fn modes(&self) -> InputModes {
        let mode = *lock(&self.emu).term.mode();
        let mouse = mouse_mode(mode);
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

    /// tmux's screen-state format variables (`cursor_x`, `wrap_flag`,
    /// `mouse_sgr_flag`, ...), read from the emulator; `None` for a name
    /// that is not one of them.
    pub fn term_var(&self, name: &str) -> Option<String> {
        let emu = lock(&self.emu);
        let mode = *emu.term.mode();
        let flag = |m: TermMode| Some(u8::from(mode.contains(m)).to_string());
        match name {
            // tmux's cursor sits one past the last column after a write
            // there, where alacritty holds it on the column and a wrap pending.
            "cursor_x" => {
                let cursor = &emu.term.grid().cursor;
                Some((cursor.point.column.0 + usize::from(cursor.input_needs_wrap)).to_string())
            }
            "cursor_y" => Some(emu.term.grid().cursor.point.line.0.max(0).to_string()),
            // The cell under the cursor; empty on the spacer of a wide character.
            "cursor_character" => {
                let point = emu.term.grid().cursor.point;
                if emu.term.grid().cursor.input_needs_wrap {
                    return Some(" ".to_string());
                }
                let cell = &emu.term.grid()[point];
                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    return Some(String::new());
                }
                let mut out = cell.c.to_string();
                out.extend(cell.zerowidth().into_iter().flatten());
                Some(out)
            }
            "cursor_flag" => flag(TermMode::SHOW_CURSOR),
            "insert_flag" => flag(TermMode::INSERT),
            "keypad_cursor_flag" => flag(TermMode::APP_CURSOR),
            "keypad_flag" => flag(TermMode::APP_KEYPAD),
            "wrap_flag" => flag(TermMode::LINE_WRAP),
            "origin_flag" => flag(TermMode::ORIGIN),
            // A set of 1000, 1002 or 1003 replaces the others and a reset
            // of any clears them all ([`written`]), as in tmux, so at most
            // one is on.
            "mouse_standard_flag" | "mouse_button_flag" | "mouse_all_flag" => {
                let want = match name {
                    "mouse_standard_flag" => MouseMode::Click,
                    "mouse_button_flag" => MouseMode::Drag,
                    _ => MouseMode::Motion,
                };
                Some(u8::from(mouse_mode(mode) == want).to_string())
            }
            "mouse_utf8_flag" => flag(TermMode::UTF8_MOUSE),
            "mouse_sgr_flag" => flag(TermMode::SGR_MOUSE),
            _ => None,
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

/// How many cells tmux's `grid_expand_line` has allocated for a line
/// written up to cell `used` of a `cols` wide grid: a quarter, a half or
/// all of the width.
fn allocated(used: usize, cols: usize) -> usize {
    if used == 0 {
        0
    } else if used < cols / 4 {
        cols / 4
    } else if used < cols / 2 {
        cols / 2
    } else {
        cols.max(used)
    }
}

/// The mouse reporting `mode` asks for: the widest of 1000, 1002, 1003.
fn mouse_mode(mode: TermMode) -> MouseMode {
    if mode.contains(TermMode::MOUSE_MOTION) {
        MouseMode::Motion
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        MouseMode::Drag
    } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        MouseMode::Click
    } else {
        MouseMode::Off
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

/// tmux's `checkshell`: an absolute path that may be executed and is not
/// tmux itself.
pub fn check_shell(shell: &str) -> bool {
    shell.starts_with('/')
        && shell.rsplit('/').next() != Some("tmux")
        && access(shell, AccessFlags::X_OK).is_ok()
}

/// The shell tmux starts panes and `default-shell` commands with:
/// `candidate` if it passes [`check_shell`], else `/bin/sh`.
pub fn pane_shell(candidate: Option<&str>) -> String {
    candidate
        .filter(|s| check_shell(s))
        .unwrap_or("/bin/sh")
        .to_string()
}

/// `candidate` when it is an executable absolute path (tmux's
/// `checkshell`), else the user's passwd shell when that is, else
/// `/bin/sh`, as tmux's `getshell` falls back (spec D2 in the M3.6 ledger).
pub fn pick_shell(candidate: Option<String>) -> String {
    let usable = |s: &String| check_shell(s);
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

/// What [`spawn_child`] gives: the master, the held slave, the child and
/// the slave's path.
type Spawned = (
    OwnedFd,
    Option<OwnedFd>,
    Option<tokio::process::Child>,
    Option<PathBuf>,
);

/// Open a pty and spawn the child on it (spec §4.8). Returns the master,
/// non-blocking and close-on-exec, and the child, or `None` when the
/// program could not be started (it failed to exec): like tmux, that
/// pane is born dead rather than failing the command.
///
/// With a child, the slave is returned too, for the reader to hold until
/// it has drained the master: on macOS the last close of a slave waits
/// about 0.6 s for unread output to be read and then throws it away, so a
/// child that exits while the reader is not running left nothing to read.
fn spawn_child(spec: &PaneSpawn, size: Size) -> stdio::Result<Spawned> {
    let name = Path::new(&spec.shell)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| spec.shell.clone());
    let mut cmd = match spec.argv.as_deref() {
        Some([script]) => {
            let mut cmd = tokio::process::Command::new(&spec.shell);
            cmd.arg0(&name).arg("-c").arg(script);
            cmd
        }
        Some(argv) => {
            let (prog, args) = argv
                .split_first()
                .ok_or_else(|| stdio::Error::new(stdio::ErrorKind::InvalidInput, "empty argv"))?;
            let mut cmd = tokio::process::Command::new(prog);
            cmd.args(args);
            cmd
        }
        None => {
            let mut cmd = tokio::process::Command::new(&spec.shell);
            if spec.login {
                cmd.arg0(format!("-{name}"));
            }
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
    let held = child.is_some().then_some(pty.slave);
    drop(_guard);

    let flags = OFlag::from_bits_truncate(fcntl(&pty.master, FcntlArg::F_GETFL)?);
    fcntl(&pty.master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok((pty.master, held, child, tty))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_term(term: &'static str) -> PaneSpawn {
        PaneSpawn {
            id: PaneId(0),
            session: SessionId(0),
            argv: None,
            shell: "/bin/sh".into(),
            login: true,
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
            identity: Default::default(),
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
            spec_for(&["/nonexistent/grofe-no-such-program", "arg"]),
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

    /// macOS drops a pty's unread output once its last slave fd closes, so
    /// a child that has written and exited before the reader's first read
    /// left an empty pane: the reader holds a slave until it has drained.
    #[tokio::test]
    async fn output_of_a_child_gone_before_the_first_read_is_kept() {
        let (tx, mut rx) = mpsc::channel(8);
        let spec = PaneSpawn {
            rows: 10,
            ..spec_for(&["seq", "1", "30"])
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        // The current-thread runtime runs no reader while this blocks. seq
        // writes and exits; its exit waits for the output to drain, about
        // 0.6 s on macOS with no other slave open, then throws it away. A
        // held slave keeps it waiting until the reader has read.
        let pid = handle.pid().expect("seq runs");
        let since = Instant::now();
        while !has_exited(pid) && since.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let exited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(ev) = rx.recv().await {
                if matches!(ev.kind, PaneEventKind::Exited(_)) {
                    return;
                }
            }
        })
        .await;
        assert!(exited.is_ok(), "seq exited");
        assert_eq!(handle.history_size(), 21);
    }

    /// Whether the child `pid` has exited, without reaping it.
    fn has_exited(pid: u32) -> bool {
        // SAFETY: a zeroed siginfo_t is valid, and waitid only writes it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
        // SAFETY: `info` outlives the call.
        let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) };
        r == 0 && info.si_signo == libc::SIGCHLD
    }

    /// A 5-row pane running `argv`, once it has `history` rows of scrollback.
    async fn linked_pane(argv: &[&str], history: usize) -> (PaneHandle, mpsc::Sender<PaneEvent>) {
        let (tx, _rx) = mpsc::channel(64);
        let spec = PaneSpawn {
            rows: 5,
            ..spec_for(argv)
        };
        let handle = PaneHandle::spawn(spec, tx.clone(), Arc::new(Notify::new())).expect("spawn");
        for _ in 0..500 {
            if handle.history_size() >= history {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (handle, tx)
    }

    const LINKED: &[&str] = &[
        "sh",
        "-c",
        "printf '\\033]133;A\\007\\033]8;;http://x.test/\\033\\\\link\\033]8;;\\033\\\\\\n'; \
         seq 1 20; exec sleep 100",
    ];

    fn links(handle: &PaneHandle) -> usize {
        handle
            .snapshot_rows(0..5)
            .rows
            .iter()
            .flatten()
            .filter(|c| c.hyperlink.is_some())
            .count()
    }

    /// F1: `clear-history -H` forgets the hyperlinks of the screen, and
    /// `clear-history` drops the scrollback and its marks.
    #[tokio::test]
    async fn clear_history_drops_scrollback_marks_and_with_h_links() {
        let (handle, _tx) = linked_pane(
            &[
                "sh",
                "-c",
                "printf '\\033]8;;http://x.test/\\033\\\\ab\\033]8;;\\033\\\\'; sleep 100",
            ],
            0,
        )
        .await;
        for _ in 0..500 {
            if links(&handle) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(links(&handle), 2);
        handle.clear_history(false);
        assert_eq!(links(&handle), 2, "without -H the links stay");
        handle.clear_history(true);
        assert_eq!(links(&handle), 0);

        let (handle, _tx) = linked_pane(LINKED, 17).await;
        assert!(handle.history_size() >= 17);
        let mut marked = 0;
        handle.copy_grid(|_, _, bits| marked += usize::from(bits != 0));
        assert_eq!(marked, 1);
        handle.clear_history(false);
        assert_eq!(handle.history_size(), 0);
        marked = 0;
        handle.copy_grid(|_, _, bits| marked += usize::from(bits != 0));
        assert_eq!(marked, 0, "the mark went with its line");
    }

    /// Waits until `ready` holds of the pane's `term_var` values.
    async fn until_vars(handle: &PaneHandle, names: &[&str], ready: impl Fn(&[String]) -> bool) {
        for _ in 0..500 {
            let got: Vec<String> = names.iter().filter_map(|n| handle.term_var(n)).collect();
            if ready(&got) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the screen never reached the state");
    }

    /// F1: tmux's cursor is one past the last column after a write there
    /// (alacritty holds it on the column, a wrap pending), with a blank under it.
    #[tokio::test]
    async fn a_cursor_past_the_last_column_is_one_past_it() {
        let (tx, _rx) = mpsc::channel(8);
        let spec = PaneSpawn {
            cols: 10,
            ..spec_for(&["sh", "-c", "printf 1234567890; sleep 100"])
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        until_vars(&handle, &["cursor_x"], |v| v[0] != "0").await;
        assert_eq!(handle.term_var("cursor_x").as_deref(), Some("10"));
        assert_eq!(handle.term_var("cursor_character").as_deref(), Some(" "));
    }

    /// F1: tmux resets 1000, 1002 and 1003 together, whichever is named.
    #[tokio::test]
    async fn resetting_one_mouse_mode_resets_them_all() {
        let (tx, _rx) = mpsc::channel(8);
        let handle = PaneHandle::spawn(
            spec_for(&[
                "sh",
                "-c",
                "printf '\\033[?1003h\\033[?1000l\\033[?6h'; sleep 100",
            ]),
            tx,
            Arc::new(Notify::new()),
        )
        .expect("spawn");
        let names = [
            "origin_flag",
            "mouse_standard_flag",
            "mouse_button_flag",
            "mouse_all_flag",
        ];
        until_vars(&handle, &names, |v| v[0] == "1").await;
        let got: Vec<_> = names.iter().map(|n| handle.term_var(n).unwrap()).collect();
        assert_eq!(got, ["1", "0", "0", "0"]);
    }

    /// F1: a respawned pane keeps what it reported (OSC 7), as tmux's does,
    /// and the old process's output does not reach the new screen.
    #[tokio::test]
    async fn a_respawn_keeps_the_reported_path_and_drops_old_output() {
        let (tx, _rx) = mpsc::channel(64);
        let old = PaneHandle::spawn(
            spec_for(&["sh", "-c", "while :; do printf OLD; sleep 0.01; done"]),
            tx.clone(),
            Arc::new(Notify::new()),
        )
        .expect("spawn");
        old.set_reported_path("file:///there".into());
        let text = |h: &PaneHandle| {
            h.capture(&CaptureOpts {
                start: None,
                end: None,
                escapes: false,
            })
        };
        for _ in 0..500 {
            if text(&old).contains("OLD") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let new = PaneHandle::spawn_over(
            spec_for(&["sleep", "100"]),
            tx,
            Arc::new(Notify::new()),
            &old,
        )
        .expect("respawn");
        assert_eq!(new.reported_path().as_deref(), Some("file:///there"));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!text(&new).contains("OLD"), "{:?}", text(&new));
    }

    /// A 5-row pane with images on, showing one 1x1-cell kitty placement on
    /// its first row that `seq` then scrolls into the scrollback.
    async fn pane_with_scrolled_image() -> (PaneHandle, mpsc::Sender<PaneEvent>) {
        let (tx, _rx) = mpsc::channel(64);
        let spec = PaneSpawn {
            rows: 5,
            images: true,
            identity: Default::default(),
            cell_px: Some((10, 20)),
            ..spec_for(&[
                "sh",
                "-c",
                "printf '\\033_Ga=T,f=24,s=10,v=20,i=7,C=1;AAAA\\033\\\\'; \
                 seq 1 20; exec sleep 100",
            ])
        };
        let handle = PaneHandle::spawn(spec, tx.clone(), Arc::new(Notify::new())).expect("spawn");
        for _ in 0..500 {
            if handle.history_size() >= 15 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (handle, tx)
    }

    /// G3 + F1: `clear-history` drops the placements that scrolled into the
    /// scrollback; the ones on the screen stay.
    #[tokio::test]
    async fn clear_history_drops_placements_in_the_scrollback() {
        let (handle, tx) = pane_with_scrolled_image().await;
        assert!(handle.copy_images().is_some(), "the image is in history");
        assert!(handle.snapshot_rows(0..5).images.is_none());
        handle.clear_history(false);
        assert!(handle.copy_images().is_none(), "no placement in history");

        // One still on the screen is kept.
        let spec = PaneSpawn {
            rows: 5,
            images: true,
            identity: Default::default(),
            cell_px: Some((10, 20)),
            ..spec_for(&[
                "sh",
                "-c",
                "seq 1 10; printf '\\033_Ga=T,f=24,s=10,v=20,i=7,C=1;AAAA\\033\\\\'; exec sleep 100",
            ])
        };
        let on_screen = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        for _ in 0..500 {
            if on_screen.snapshot_rows(0..5).images.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(on_screen.snapshot_rows(0..5).images.is_some());
        on_screen.clear_history(false);
        assert!(on_screen.snapshot_rows(0..5).images.is_some());
    }

    /// G1 + G3: an OSC 133 mark and an image placement in one read each land
    /// on the row the stream had reached.
    #[tokio::test]
    async fn marks_and_placements_in_one_read_keep_their_rows() {
        let (tx, _rx) = mpsc::channel(64);
        let spec = PaneSpawn {
            rows: 5,
            images: true,
            identity: Default::default(),
            cell_px: Some((10, 20)),
            ..spec_for(&[
                "sh",
                "-c",
                "printf 'a\\n\\033]133;A\\007b\\n\\033_Ga=T,f=24,s=10,v=20,i=7,C=1;AAAA\\033\\\\\\033]133;A\\007c'; exec sleep 100",
            ])
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        for _ in 0..500 {
            if handle.snapshot_rows(0..5).images.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let images = handle.snapshot_rows(0..5).images.expect("a placement");
        assert_eq!(images.kitty[0].row, 2, "placed on the third row");
        let mut marked = Vec::new();
        let mut row = 0;
        handle.copy_grid(|_, _, bits| {
            if bits != 0 {
                marked.push(row);
            }
            row += 1;
        });
        assert_eq!(marked, [1, 2], "marks on the rows of `b` and `c`");
    }

    /// G3 + F1: a respawn clears the pane's placements, history ones too.
    #[tokio::test]
    async fn a_respawn_clears_the_panes_placements() {
        let (old, tx) = pane_with_scrolled_image().await;
        assert!(old.copy_images().is_some());
        let spec = PaneSpawn {
            rows: 5,
            images: true,
            identity: Default::default(),
            ..spec_for(&["sleep", "100"])
        };
        let new = PaneHandle::spawn_over(spec, tx, Arc::new(Notify::new()), &old).expect("respawn");
        assert!(new.copy_images().is_none());
        assert!(new.snapshot_rows(0..5).images.is_none());
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

    /// `send-keys -R` leaves only the cursor and wrapping on, as tmux's
    /// `screen_write_reset`: the kitty keyboard flags go with the rest,
    /// the alternate screen stays, and a half-read sequence is dropped.
    #[tokio::test]
    async fn reset_state_clears_every_mode_but_the_alternate_screen() {
        let (tx, _rx) = mpsc::channel(8);
        let handle = PaneHandle::spawn(spec_for(&["sleep", "30"]), tx, Arc::new(Notify::new()))
            .expect("spawn");
        let feed = |bytes: &[u8]| {
            let mut emu = lock(&handle.emu);
            let Emu {
                term,
                marks,
                parser,
                ..
            } = &mut *emu;
            parser.advance(&mut written::Written(term, marks, None), bytes);
        };
        feed(b"\x1b[?1049h\x1b[?2004h\x1b=\x1b[?1h\x1b[?1000h\x1b[?1006h\x1b[>1u\x1b[?7l\x1b[31");
        let m = handle.modes();
        assert!(m.bracketed_paste && m.app_keypad && m.app_cursor && m.kitty_flags != 0);
        handle.reset_state();
        let m = handle.modes();
        assert!(
            !m.bracketed_paste && !m.app_keypad && !m.app_cursor,
            "{m:?}"
        );
        assert_eq!((m.kitty_flags, m.mouse_encoding), (0, MouseEncoding::X10));
        assert!(m.alt_screen, "the alternate screen stays");
        // The `\x1b[31` cut short above does not swallow what comes next.
        feed(b"x");
        let emu = lock(&handle.emu);
        let mode = *emu.term.mode();
        assert!(mode.contains(TermMode::LINE_WRAP) && mode.contains(TermMode::SHOW_CURSOR));
        assert_eq!(emu.term.grid()[Line(0)][Column(0)].c, 'x');
    }

    /// A drag's resizes reach the pty at most every [`RESIZE_HOLD`], the
    /// first at once and the last in the end (tmux's pane resize queue); a
    /// plain resize goes at once and drops the held ones.
    #[tokio::test]
    async fn held_resizes_are_coalesced_and_end_at_the_last() {
        let (tx, _rx) = mpsc::channel(8);
        let handle = PaneHandle::spawn(spec_for(&["sleep", "30"]), tx, Arc::new(Notify::new()))
            .expect("spawn");
        handle.resize_held(40, 10).expect("first");
        assert_eq!(winsize_of(&handle), (40, 10, 0, 0));
        for cols in 41..=50 {
            handle.resize_held(cols, 10).expect("held");
        }
        assert_eq!(winsize_of(&handle), (40, 10, 0, 0));
        tokio::time::sleep(RESIZE_HOLD + Duration::from_millis(150)).await;
        assert_eq!(winsize_of(&handle), (50, 10, 0, 0));
        handle.resize_held(60, 10).expect("held again");
        handle.resize(70, 10).expect("plain");
        assert_eq!(winsize_of(&handle), (70, 10, 0, 0));
        tokio::time::sleep(RESIZE_HOLD * 2).await;
        assert_eq!(winsize_of(&handle), (70, 10, 0, 0));
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

    /// A `default-shell` that fails tmux's `checkshell` is `/bin/sh` when a
    /// pane starts, where tmux's `spawn_pane` and `job_run` fall back.
    #[test]
    fn the_server_default_shell_is_never_tmux_itself() {
        assert_eq!(
            pick_shell(Some("/usr/bin/tmux".into())),
            pick_shell(Some("/nonexistent/x".into()))
        );
    }

    #[test]
    fn an_unusable_default_shell_is_bin_sh() {
        for bad in [
            "/nonexistent/grofe-shell",
            "sh",
            "/etc/hosts",
            "/usr/bin/tmux",
        ] {
            assert_eq!(pane_shell(Some(bad)), "/bin/sh", "{bad}");
        }
        assert_eq!(pane_shell(None), "/bin/sh");
        let sh = std::fs::canonicalize("/bin/sh")
            .unwrap()
            .display()
            .to_string();
        assert_eq!(pane_shell(Some(&sh)), sh);
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
