//! T0.3 single-pane spike (throwaway): one pty, one `alacritty_terminal::Term`,
//! naive full redraw to the outer terminal. No server, no multiplexing.
//!
//! Usage: `cargo run --example spike [-- argv...]`. Quit with Ctrl-].
//! Set `GROFE_SPIKE_LOG=/path` to log title, bell and clipboard events.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use anyhow::{Context, Result};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::libc;
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::Pid;

const FRAME: Duration = Duration::from_millis(16);
const QUIT_BYTE: u8 = 0x1d; // Ctrl-]

nix::ioctl_write_ptr_bad!(set_winsize, libc::TIOCSWINSZ, Winsize);

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

/// Listener events are buffered while the Term lock is held and drained after.
type EventBuf = Arc<Mutex<Vec<Event>>>;

#[derive(Clone)]
struct Listener(EventBuf);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Ok(mut buf) = self.0.lock() {
            buf.push(event);
        }
    }
}

struct Emu {
    term: Term<Listener>,
    parser: Processor,
}

#[derive(Clone, Copy)]
struct Size {
    cols: u16,
    rows: u16,
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

enum Msg {
    Output,
    Exited,
    Quit,
}

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_tty();
        default_hook(info);
    }));

    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            restore_tty();
            eprintln!("spike: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }
    if args.is_empty() {
        args.push(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
    }

    let (cols, rows) = terminal::size().context("outer terminal size")?;
    let size = Size { cols, rows };
    let (master, child) = spawn(&args, size)?;
    let child_pid = Pid::from_raw(child.id() as i32);

    let events: EventBuf = Arc::default();
    let emu = Arc::new(Mutex::new(Emu {
        term: Term::new(Config::default(), &size, Listener(events.clone())),
        parser: Processor::new(),
    }));

    // SA_RESTART so the blocking reads in the worker threads are not interrupted.
    let winch = SigAction::new(
        SigHandler::Handler(on_winch),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe { signal::sigaction(Signal::SIGWINCH, &winch) }.context("sigaction")?;

    terminal::enable_raw_mode().context("raw mode")?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen).context("alt screen")?;

    let (tx, rx) = mpsc::channel::<Msg>();
    let (wtx, wrx) = mpsc::channel::<Vec<u8>>();

    // Writer: sole owner of writes to the master, so replies and input stay ordered.
    let mut master_w = master.try_clone()?;
    std::thread::spawn(move || {
        for bytes in wrx {
            if master_w.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    // Reader: parse up to 16 KiB per lock hold, then answer listener events unlocked.
    let mut master_r = master.try_clone()?;
    {
        let (emu, events, wtx, tx) = (emu.clone(), events.clone(), wtx.clone(), tx.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = match master_r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break, // EIO once the slave side is gone
                };
                let size = {
                    let Ok(mut guard) = emu.lock() else { break };
                    let Emu { term, parser } = &mut *guard;
                    parser.advance(term, &buf[..n]);
                    term_size(term)
                };
                drain_events(&events, &wtx, size);
                if tx.send(Msg::Output).is_err() {
                    break;
                }
            }
        });
    }

    // Stdin: raw bytes straight to the pty, except the Ctrl-] escape hatch.
    {
        let (wtx, tx) = (wtx.clone(), tx.clone());
        std::thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let chunk = &buf[..n];
                if let Some(i) = chunk.iter().position(|&b| b == QUIT_BYTE) {
                    let _ = wtx.send(chunk[..i].to_vec());
                    let _ = tx.send(Msg::Quit);
                    break;
                }
                if wtx.send(chunk.to_vec()).is_err() {
                    break;
                }
            }
        });
    }

    // Waiter: exit is observed via the child handle, not a global SIGCHLD handler.
    {
        let tx = tx.clone();
        let mut child: Child = child;
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = tx.send(Msg::Exited);
        });
    }
    drop(tx);

    let mut out = String::new();
    let mut dirty = true;
    let mut last_frame = Instant::now() - FRAME;
    loop {
        match rx.recv_timeout(FRAME) {
            Ok(Msg::Output) => dirty = true,
            Ok(Msg::Exited) => break,
            Ok(Msg::Quit) => {
                let _ = signal::kill(child_pid, Signal::SIGHUP);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if WINCH.swap(false, Ordering::Relaxed) {
            let (cols, rows) = terminal::size().context("outer terminal size")?;
            let size = Size { cols, rows };
            if let Ok(mut guard) = emu.lock() {
                guard.term.resize(size);
            }
            let ws = winsize(size);
            // SAFETY: valid fd we own and a pointer to a live Winsize.
            unsafe { set_winsize(master.as_raw_fd(), &ws) }.context("TIOCSWINSZ")?;
            dirty = true;
        }

        // Drive the synchronized-update (mode 2026) timeout so a stuck BSU cannot freeze us.
        let expired_sync = {
            let Ok(mut guard) = emu.lock() else { break };
            let Emu { term, parser } = &mut *guard;
            let expired = parser
                .sync_timeout()
                .sync_timeout()
                .is_some_and(|t| t <= Instant::now());
            if expired {
                parser.stop_sync(term);
            }
            expired.then(|| term_size(term))
        };
        if let Some(size) = expired_sync {
            drain_events(&events, &wtx, size);
            dirty = true;
        }

        if dirty && last_frame.elapsed() >= FRAME {
            if let Ok(guard) = emu.lock() {
                render(&guard.term, &mut out);
            }
            let mut stdout = io::stdout().lock();
            stdout.write_all(out.as_bytes())?;
            stdout.flush()?;
            dirty = false;
            last_frame = Instant::now();
        }
    }

    restore_tty();
    Ok(())
}

fn winsize(size: Size) -> Winsize {
    Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn term_size(term: &Term<Listener>) -> Size {
    Size {
        cols: term.columns() as u16,
        rows: term.screen_lines() as u16,
    }
}

/// Spawn `argv` on a fresh pty per spec §4.8. Returns the master and the child.
fn spawn(argv: &[String], size: Size) -> Result<(File, Child)> {
    let pty = openpty(&winsize(size), None).context("openpty")?;
    fcntl(&pty.master, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("cloexec master")?;
    // The original slave fd must not survive exec; dup2 clears CLOEXEC on 0/1/2.
    fcntl(&pty.slave, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("cloexec slave")?;
    let slave = pty.slave.as_raw_fd();

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env("TERM", "screen-256color")
        .env("COLORTERM", "truecolor");
    // SAFETY: the closure only makes async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            for fd in 0..3 {
                if libc::dup2(slave, fd) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("spawn {}", argv[0]))?;
    drop(pty.slave);
    Ok((File::from(pty.master), child))
}

/// Handle buffered listener events. The event lock is held while enqueueing so
/// replies from different drainers keep their order.
fn drain_events(events: &EventBuf, wtx: &Sender<Vec<u8>>, size: Size) {
    let Ok(mut buf) = events.lock() else { return };
    for event in buf.drain(..) {
        match event {
            Event::PtyWrite(text) => {
                let _ = wtx.send(text.into_bytes());
            }
            Event::ColorRequest(index, format) => {
                // ponytail: answers only fg/bg/cursor from a fixed palette; a real
                // palette comes from config.
                let rgb = match index {
                    256 | 258 => Rgb {
                        r: 0xd0,
                        g: 0xd0,
                        b: 0xd0,
                    },
                    257 => Rgb { r: 0, g: 0, b: 0 },
                    _ => continue,
                };
                let _ = wtx.send(format(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let ws = WindowSize {
                    num_lines: size.rows,
                    num_cols: size.cols,
                    cell_width: 0,
                    cell_height: 0,
                };
                let _ = wtx.send(format(ws).into_bytes());
            }
            Event::Title(_) | Event::Bell | Event::ClipboardStore(..) | Event::ResetTitle => {
                log(&format!("{event:?}"));
            }
            _ => {}
        }
    }
}

fn log(line: &str) {
    static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    let file = LOG.get_or_init(|| {
        let path = std::env::var_os("GROFE_SPIKE_LOG")?;
        let file = File::options().create(true).append(true).open(path).ok()?;
        Some(Mutex::new(file))
    });
    if let Some(Ok(mut f)) = file.as_ref().map(Mutex::lock) {
        let _ = writeln!(f, "{line}");
    }
}

const STYLE: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// Naive full redraw of the visible grid, then cursor position, shape and visibility.
fn render(term: &Term<Listener>, out: &mut String) {
    out.clear();
    out.push_str("\x1b[?25l");
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let mut pen = None;
    let mut row = i32::MIN;
    for cell in content.display_iter {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let line = cell.point.line.0 + offset;
        let col = cell.point.column.0;
        if line != row {
            row = line;
            let _ = write!(out, "\x1b[{};{}H", line + 1, col + 1);
        }
        let style = (cell.fg, cell.bg, cell.flags & STYLE);
        if pen != Some(style) {
            sgr(out, style);
            pen = Some(style);
        }
        out.push(cell.c);
        if let Some(zw) = cell.zerowidth() {
            out.extend(zw);
        }
        // Inner and outer width tables can disagree; resync after wide or zero-width cells.
        if cell.flags.contains(Flags::WIDE_CHAR) || cell.zerowidth().is_some() {
            let next = col
                + if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
            let _ = write!(out, "\x1b[{};{}H", line + 1, next + 1);
        }
    }
    out.push_str("\x1b[0m");

    let cursor = content.cursor;
    let _ = write!(
        out,
        "\x1b[{};{}H",
        cursor.point.line.0 + offset + 1,
        cursor.point.column.0 + 1
    );
    let blinking = term.cursor_style().blinking;
    let decscusr = match cursor.shape {
        CursorShape::Block | CursorShape::HollowBlock | CursorShape::Hidden => 2,
        CursorShape::Underline => 4,
        CursorShape::Beam => 6,
    } - u8::from(blinking);
    let _ = write!(out, "\x1b[{decscusr} q");
    if cursor.shape != CursorShape::Hidden {
        out.push_str("\x1b[?25h");
    }
}

fn sgr(out: &mut String, (fg, bg, flags): (Color, Color, Flags)) {
    out.push_str("\x1b[0");
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::ALL_UNDERLINES, "4"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if flags.intersects(flag) {
            out.push(';');
            out.push_str(code);
        }
    }
    color(out, fg, 30);
    color(out, bg, 40);
    out.push('m');
}

/// `base` is 30 for foreground, 40 for background.
fn color(out: &mut String, color: Color, base: u16) {
    let _ = match color {
        Color::Spec(Rgb { r, g, b }) => write!(out, ";{};2;{r};{g};{b}", base + 8),
        Color::Indexed(i) => write!(out, ";{};5;{i}", base + 8),
        Color::Named(named) => {
            let i = named as u16;
            let dim = NamedColor::DimBlack as u16..=NamedColor::DimWhite as u16;
            let code = match i {
                0..8 => base + i,
                8..16 => base + 60 + i - 8,
                _ if dim.contains(&i) => base + i - NamedColor::DimBlack as u16,
                _ => base + 9, // default fg/bg
            };
            write!(out, ";{code}")
        }
    };
}

fn restore_tty() {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x1b[0m\x1b[0 q\x1b[?25h");
    let _ = crossterm::execute!(stdout, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}
