//! attach client + capability probe (spec §4.4, §4.6, §4.7).
//!
//! `grofe attach` runs here, synchronously and without tokio (the CLI never
//! builds a runtime, spec §4.1). The order on the wire and on the tty:
//!
//! 1. Raw mode, then the probe: XTVERSION, DECRQM 2026, the kitty keyboard
//!    query and the kitty graphics query, with DA1 last as the end marker
//!    (every terminal answers DA1, and answers in order). The probe stops at
//!    DA1's reply or after [`PROBE_TIMEOUT`]; bytes that are not replies are
//!    typeahead and become the first `input` frame.
//! 2. The attach hello with the probed [`Caps`], then exactly one JSON reply
//!    line (protocol.rs handshake). A refusal is printed and nothing else
//!    touches the screen.
//! 3. Alternate screen, bracketed paste and the richest key mode: kitty
//!    flags 1 (disambiguate) when the terminal answered the kitty query,
//!    else modifyOtherKeys level 1.
//!    Focus reporting (1004) is the client's too.
//! 4. Frames: stdin to `input` (through the probe parser until DA1's reply
//!    arrives, so late replies are dropped, not sent as keys), SIGWINCH to
//!    `resize`, `output` to stdout, `exit` ends the client. One writer
//!    thread owns the connection; the signal thread never blocks on it.
//!
//! The tty is restored (termios plus the teardown sequences that undo step
//! 3) on every exit path: a clean exit, the server hanging up, SIGTERM,
//! SIGHUP, SIGINT or SIGQUIT (taken by a `sigwait` thread, so no work runs
//! in a signal handler) and a panic in any thread (the panic hook restores,
//! then exits the process with status 101). Signals and panics wait at
//! most [`RESTORE_BOUND`] for a stalled tty before restoring termios anyway.

use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::termios::{self, SetArg, Termios};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

use crate::protocol::{
    Caps, ClientCtx, Frame, FrameCodec, HELLO_REPLY_ID, Hello, Reply, encode_hello,
};

/// How long the probe waits for DA1's reply (the M2 ruling). A terminal
/// that answers nothing costs this much once, at attach.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// How long the client waits for the server's one reply line.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// The probe, in the order sent. DA1 goes last: its reply ends the probe.
pub const PROBE_QUERY: &[u8] = concat!(
    "\x1b[>0q",                                   // XTVERSION
    "\x1b[?2026$p",                               // DECRQM synchronized output
    "\x1b[?u",                                    // kitty keyboard flags
    "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\", // kitty graphics query
    "\x1b[c",                                     // DA1
)
.as_bytes();

const ALT_SCREEN_ON: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_OFF: &[u8] = b"\x1b[?1049l";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
/// Kitty keyboard flags 1 (disambiguate escape codes), pushed and popped.
/// The kitty stack is per screen, so the push comes after entering the
/// alternate screen and the pop before leaving it.
const KITTY_PUSH: &[u8] = b"\x1b[>1u";
const KITTY_POP: &[u8] = b"\x1b[<u";
/// modifyOtherKeys level 1, tmux's `extended-keys on` request.
const MOK_ON: &[u8] = b"\x1b[>4;1m";
const MOK_OFF: &[u8] = b"\x1b[>4;0m";
/// Focus reporting: the client owns mode 1004 on the outer terminal.
const FOCUS_ON: &[u8] = b"\x1b[?1004h";
/// Undone on exit whatever the server drew: focus reporting, mouse modes
/// (1000, 1002, 1003, SGR 1006), an unterminated synchronized update,
/// attributes, a hidden cursor, the cursor shape.
const RESET_TAIL: &[u8] = concat!(
    "\x1b[?1004l",
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l",
    "\x1b[?2026l\x1b[0m\x1b[?25h\x1b[0 q",
)
.as_bytes();

/// Set to `stdin` to make the stdin thread panic on its first read: the
/// test hook for "a panic in any thread restores the tty".
pub const TEST_PANIC_ENV: &str = "GROFE_TEST_CLIENT_PANIC";

// ------------------------------------------------------------------ probe

/// One recognised answer to [`PROBE_QUERY`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeReply {
    /// `CSI ? ... c`: the end marker.
    Da1,
    /// `DCS > | name version ST`.
    XtVersion(String),
    /// `CSI ? mode ; value $ y`.
    ModeReport { mode: u32, value: u32 },
    /// `CSI ? flags u`.
    KittyKeys,
    /// `APC G ... ; OK ST` (true) or an error message (false).
    KittyGraphics(bool),
}

enum Scan {
    Reply(ProbeReply, usize),
    /// A prefix of a reply: wait for more bytes.
    Incomplete,
    /// Not a reply: the ESC is typeahead.
    NotReply,
}

/// Replies longer than this are not replies (bounds a runaway DCS/APC).
const MAX_REPLY_LEN: usize = 512;

/// Accumulates what the terminal sends while the probe runs.
#[derive(Debug, Default)]
pub struct ProbeParser {
    pending: Vec<u8>,
    typeahead: Vec<u8>,
    xtversion: Option<String>,
    sync_mode: Option<u32>,
    kitty_keys: bool,
    kitty_graphics: bool,
    done: bool,
}

impl ProbeParser {
    /// Feeds bytes read from the terminal.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.done {
            self.typeahead.extend_from_slice(bytes);
            return;
        }
        self.pending.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.pending);
        let mut i = 0;
        while i < buf.len() {
            if self.done {
                self.typeahead.extend_from_slice(&buf[i..]);
                return;
            }
            if buf[i] != 0x1b {
                self.typeahead.push(buf[i]);
                i += 1;
                continue;
            }
            match scan_reply(&buf[i..]) {
                Scan::Reply(reply, len) => {
                    self.record(reply);
                    i += len;
                }
                Scan::Incomplete => {
                    self.pending = buf[i..].to_vec();
                    return;
                }
                Scan::NotReply => {
                    self.typeahead.push(buf[i]);
                    i += 1;
                }
            }
        }
    }

    fn record(&mut self, reply: ProbeReply) {
        match reply {
            ProbeReply::Da1 => self.done = true,
            ProbeReply::XtVersion(v) => self.xtversion = Some(v),
            ProbeReply::ModeReport { mode: 2026, value } => self.sync_mode = Some(value),
            ProbeReply::ModeReport { .. } => {}
            ProbeReply::KittyKeys => self.kitty_keys = true,
            ProbeReply::KittyGraphics(ok) => self.kitty_graphics = ok,
        }
    }

    /// Whether DA1's reply arrived (the probe is over).
    pub fn done(&self) -> bool {
        self.done
    }

    /// The capabilities the replies (and `$COLORTERM`) show. Truecolor
    /// comes from `$COLORTERM`, else from XTVERSION naming Ghostty or
    /// WezTerm (neither needs `$COLORTERM` to render 24-bit colour, and over
    /// ssh it is usually not forwarded); styled underlines only from that
    /// XTVERSION match, since no query reports them.
    pub fn caps(&self, colorterm: Option<&str>) -> Caps {
        let known = self.xtversion.as_deref().is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.starts_with("ghostty") || v.starts_with("wezterm")
        });
        let colorterm = colorterm.is_some_and(|c| c == "truecolor" || c == "24bit");
        Caps {
            truecolor: colorterm || known,
            styled_underline: known,
            // DECRPM 1 set, 2 reset, 3 permanently set: the mode exists.
            sync: matches!(self.sync_mode, Some(1..=3)),
            kitty_keys: self.kitty_keys,
            kitty_graphics: self.kitty_graphics,
        }
    }

    /// The non-reply bytes so far, in arrival order; an unfinished reply
    /// stays pending.
    pub fn take_typeahead(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.typeahead)
    }

    /// Whether an unfinished reply (or a lone ESC) is held back.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Gives up on the held-back bytes: they become typeahead.
    pub fn flush_pending(&mut self) {
        self.typeahead.append(&mut self.pending);
    }

    /// Ends the probe: everything that was not a reply, including an
    /// unfinished one, in arrival order.
    pub fn into_typeahead(mut self) -> Vec<u8> {
        self.flush_pending();
        self.typeahead
    }
}

/// Matches one probe reply at the start of `b` (`b[0]` is ESC).
fn scan_reply(b: &[u8]) -> Scan {
    match b.get(1) {
        None => Scan::Incomplete,
        Some(b'[') => scan_csi(b),
        Some(b'P') => match scan_string(b, b">|") {
            Ok((body, len)) => Scan::Reply(
                ProbeReply::XtVersion(String::from_utf8_lossy(body).into_owned()),
                len,
            ),
            Err(scan) => scan,
        },
        Some(b'_') => match scan_string(b, b"G") {
            Ok((body, len)) => {
                let ok = body.splitn(2, |&c| c == b';').nth(1) == Some(b"OK".as_slice());
                Scan::Reply(ProbeReply::KittyGraphics(ok), len)
            }
            Err(scan) => scan,
        },
        Some(_) => Scan::NotReply,
    }
}

/// `CSI ? params (c | u | $ y)`.
fn scan_csi(b: &[u8]) -> Scan {
    match b.get(2) {
        None => return Scan::Incomplete,
        Some(b'?') => {}
        Some(_) => return Scan::NotReply,
    }
    let start = 3;
    let mut i = start;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b';') {
        i += 1;
    }
    if i > MAX_REPLY_LEN {
        return Scan::NotReply;
    }
    let params = &b[start..i];
    match b.get(i) {
        None => Scan::Incomplete,
        Some(b'c') => Scan::Reply(ProbeReply::Da1, i + 1),
        Some(b'u') => Scan::Reply(ProbeReply::KittyKeys, i + 1),
        Some(b'$') => match b.get(i + 1) {
            None => Scan::Incomplete,
            Some(b'y') => {
                let text = String::from_utf8_lossy(params);
                let mut nums = text.split(';').map(|n| n.parse::<u32>().ok());
                match (nums.next().flatten(), nums.next().flatten()) {
                    (Some(mode), Some(value)) => {
                        Scan::Reply(ProbeReply::ModeReport { mode, value }, i + 2)
                    }
                    _ => Scan::NotReply,
                }
            }
            Some(_) => Scan::NotReply,
        },
        Some(_) => Scan::NotReply,
    }
}

/// `ESC <intro> prefix body ESC \`: the body and the whole length, or why not.
fn scan_string<'a>(b: &'a [u8], prefix: &[u8]) -> Result<(&'a [u8], usize), Scan> {
    let body_start = 2 + prefix.len();
    for (k, &want) in prefix.iter().enumerate() {
        match b.get(2 + k) {
            None => return Err(Scan::Incomplete),
            Some(&got) if got == want => {}
            Some(_) => return Err(Scan::NotReply),
        }
    }
    let mut i = body_start;
    while i < b.len() {
        if b[i] == 0x1b {
            return match b.get(i + 1) {
                None => Err(Scan::Incomplete),
                Some(b'\\') => Ok((&b[body_start..i], i + 2)),
                Some(_) => Err(Scan::NotReply),
            };
        }
        if i > MAX_REPLY_LEN {
            return Err(Scan::NotReply);
        }
        i += 1;
    }
    Err(Scan::Incomplete)
}

// ------------------------------------------------------------------ attach args

/// The flags of `attach-session` that go into the hello.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachOptions {
    /// `-t`, raw: the server resolves it.
    pub target: Option<String>,
    pub detach_others: bool,
    pub nest: bool,
}

/// The raw `-t` value of an already validated `attach-session` argv (the
/// verb stripped): `-t x`, `-tx` or `-dtx`.
pub fn raw_target(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" || !arg.starts_with('-') || arg.len() < 2 {
            break;
        }
        if arg.starts_with("--") {
            continue;
        }
        if let Some(t) = arg[1..].find('t') {
            let attached = &arg[1 + t + 1..];
            return if attached.is_empty() {
                it.next().cloned()
            } else {
                Some(attached.to_string())
            };
        }
    }
    None
}

// ------------------------------------------------------------------ tty state

/// The termios from before raw mode, as plain data so any thread can reach
/// it without a lock.
struct Saved(nix::libc::termios);

static SAVED: OnceLock<Saved> = OnceLock::new();

/// The bytes that undo the setup; set once the handshake succeeds.
static TEARDOWN: OnceLock<Vec<u8>> = OnceLock::new();

/// True while the tty is ours. Held for every tty write, so nothing lands
/// after the teardown.
static ACTIVE: Mutex<bool> = Mutex::new(false);

/// Set by SIGWINCH; the writer thread sends one `resize` for it.
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

/// How long a signal or a panic waits for the tty lock and for the
/// terminal to take the teardown before restoring termios regardless.
const RESTORE_BOUND: Duration = Duration::from_millis(200);

/// Frames for the writer thread. Bounded: a server that stops reading
/// stalls the stdin thread, never the signal thread (it only `try_send`s).
enum Out {
    Input(Vec<u8>),
    Detach,
    /// Look at [`RESIZE_PENDING`].
    Wake,
}

const OUT_QUEUE: usize = 64;

fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 stays open for the life of the process.
    unsafe { BorrowedFd::borrow_raw(0) }
}

fn stdout_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 1 stays open for the life of the process.
    unsafe { BorrowedFd::borrow_raw(1) }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes the teardown (bounded by `deadline` when given) and restores
/// termios. The caller has made sure it runs once.
fn undo(deadline: Option<Instant>) {
    if let Some(teardown) = TEARDOWN.get() {
        let _ = write_all(stdout_fd(), teardown, deadline);
    }
    if let Some(saved) = SAVED.get() {
        let _ = termios::tcsetattr(stdin_fd(), SetArg::TCSANOW, &Termios::from(saved.0));
    }
}

/// Restores the tty once; later calls do nothing. Waits for a tty write in
/// progress, so it is for the client's own exit paths.
pub fn restore_tty() {
    let mut active = lock(&ACTIVE);
    if *active {
        *active = false;
        undo(None);
    }
}

/// [`restore_tty`] for signals and panics: never waits longer than
/// [`RESTORE_BOUND`]. If another thread holds the tty lock (a write stuck
/// on a stalled terminal), termios is restored anyway and the teardown is
/// written best-effort beside it.
fn restore_tty_bounded() {
    let deadline = Instant::now() + RESTORE_BOUND;
    loop {
        match ACTIVE.try_lock() {
            Ok(mut active) => return take_and_undo(&mut active, deadline),
            Err(TryLockError::Poisoned(e)) => return take_and_undo(&mut e.into_inner(), deadline),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => {
                return undo(Some(Instant::now() + RESTORE_BOUND));
            }
        }
    }
}

fn take_and_undo(active: &mut bool, deadline: Instant) {
    if *active {
        *active = false;
        undo(Some(deadline));
    }
}

/// Writes to the tty unless it was already restored.
fn write_tty(bytes: &[u8]) -> io::Result<()> {
    let active = lock(&ACTIVE);
    if *active {
        write_all(stdout_fd(), bytes, None)?;
    }
    Ok(())
}

/// Writes all of `bytes`, waiting in `poll` (not spinning) when the fd is
/// full. With a `deadline`, gives up at it and writes in small pieces so a
/// single write cannot block for long.
fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8], deadline: Option<Instant>) -> io::Result<()> {
    while !bytes.is_empty() {
        if let Some(deadline) = deadline
            && !wait_ready(fd, PollFlags::POLLOUT, Some(deadline))?
        {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let piece = if deadline.is_some() {
            &bytes[..bytes.len().min(64)]
        } else {
            bytes
        };
        match nix::unistd::write(fd, piece) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) => {
                wait_ready(fd, PollFlags::POLLOUT, deadline)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Polls `fd` for `events` until `deadline` (for ever with `None`). True
/// when ready (or hung up, so the next read or write reports it).
fn wait_ready(
    fd: BorrowedFd<'_>,
    events: PollFlags,
    deadline: Option<Instant>,
) -> io::Result<bool> {
    loop {
        let timeout = match deadline {
            None => PollTimeout::NONE,
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(false);
                }
                PollTimeout::from(u16::try_from(left.as_millis().max(1)).unwrap_or(u16::MAX))
            }
        };
        let mut fds = [PollFd::new(fd, events)];
        match poll(&mut fds, timeout) {
            Ok(0) => {}
            Ok(_) => return Ok(true),
            Err(Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

fn tty_size() -> (u16, u16) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    (cols.max(1), rows.max(1))
}

// ------------------------------------------------------------------ attach

/// Runs an attach client on `stream` until it ends. `Ok` carries the
/// server's exit reason (`detached`, ...); `Err` a refusal or failure. The
/// tty is already restored when this returns.
pub fn attach(
    stream: UnixStream,
    ctx: ClientCtx,
    opts: AttachOptions,
    term: String,
) -> Result<String, String> {
    let is_tty = |fd| termios::tcgetattr(fd).is_ok();
    if !is_tty(stdin_fd()) || !is_tty(stdout_fd()) {
        return Err("open terminal failed: not a terminal".to_string());
    }
    let saved = termios::tcgetattr(stdin_fd()).map_err(|e| format!("tcgetattr: {e}"))?;
    let _ = SAVED.set(Saved(saved.clone().into()));
    *lock(&ACTIVE) = true;
    install_panic_hook();
    let (out_tx, out_rx) = mpsc::sync_channel(OUT_QUEUE);
    start_signal_thread(out_tx.clone())?;

    let mut raw = saved;
    termios::cfmakeraw(&mut raw);
    if let Err(e) = termios::tcsetattr(stdin_fd(), SetArg::TCSANOW, &raw) {
        restore_tty();
        return Err(format!("raw mode: {e}"));
    }

    let result = run(stream, ctx, opts, term, out_tx, out_rx);
    restore_tty();
    result
}

fn run(
    stream: UnixStream,
    ctx: ClientCtx,
    opts: AttachOptions,
    term: String,
    out_tx: SyncSender<Out>,
    out_rx: Receiver<Out>,
) -> Result<String, String> {
    let mut probe = probe().map_err(|e| format!("probing the terminal: {e}"))?;
    let caps = probe.caps(std::env::var("COLORTERM").ok().as_deref());
    let typeahead = probe.take_typeahead();

    let (cols, rows) = tty_size();
    let hello = encode_hello(&Hello::Attach {
        ctx,
        cols,
        rows,
        term,
        caps,
        target: opts.target,
        detach_others: opts.detach_others,
        nest: opts.nest,
    })
    .map_err(|e| e.to_string())?;
    io::Write::write_all(&mut &stream, format!("{hello}\n").as_bytes())
        .map_err(|e| format!("sending hello: {e}"))?;
    match read_reply(&stream)? {
        Reply::Ok {
            id: HELLO_REPLY_ID, ..
        } => {}
        Reply::Err { err, .. } => return Err(err),
        other => return Err(format!("unexpected reply to attach: {other:?}")),
    }

    let (keys_on, keys_off) = if caps.kitty_keys {
        (KITTY_PUSH, KITTY_POP)
    } else {
        (MOK_ON, MOK_OFF)
    };
    let _ = TEARDOWN.set([keys_off, PASTE_OFF, RESET_TAIL, ALT_SCREEN_OFF].concat());
    write_tty(&[ALT_SCREEN_ON, PASTE_ON, FOCUS_ON, keys_on].concat())
        .map_err(|e| format!("writing to the terminal: {e}"))?;

    let writer = stream
        .try_clone()
        .map_err(|e| format!("cloning the connection: {e}"))?;
    // The writer starts by sending the size if a SIGWINCH landed during the
    // handshake, and the queue is empty, so these never block.
    RESIZE_PENDING.fetch_or(tty_size() != (cols, rows), Ordering::SeqCst);
    if !typeahead.is_empty() {
        let _ = out_tx.try_send(Out::Input(typeahead));
    }
    std::thread::Builder::new()
        .name("writer".to_string())
        .spawn(move || writer_loop(writer, out_rx))
        .map_err(|e| format!("starting the writer thread: {e}"))?;
    // Late probe replies are filtered until DA1's arrives.
    let filter = (!probe.done()).then_some(probe);
    std::thread::Builder::new()
        .name("stdin".to_string())
        .spawn(move || stdin_loop(out_tx, filter))
        .map_err(|e| format!("starting the input thread: {e}"))?;

    read_frames(stream)
}

/// Writes the probe and collects replies until DA1's or the timeout.
fn probe() -> io::Result<ProbeParser> {
    write_tty(PROBE_QUERY)?;
    let mut parser = ProbeParser::default();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut chunk = [0u8; 1024];
    while !parser.done() {
        if !wait_ready(stdin_fd(), PollFlags::POLLIN, Some(deadline))? {
            break;
        }
        match nix::unistd::read(stdin_fd(), &mut chunk) {
            Ok(0) => break,
            Ok(n) => parser.feed(&chunk[..n]),
            Err(Errno::EINTR | Errno::EAGAIN) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(parser)
}
/// Reads the handshake's one reply line a byte at a time, so no frame
/// bytes after it are consumed.
fn read_reply(stream: &UnixStream) -> Result<Reply, String> {
    stream
        .set_read_timeout(Some(HELLO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match io::Read::read(&mut &*stream, &mut byte) {
            Ok(0) => return Err("server closed the connection during attach".to_string()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() >= crate::protocol::MAX_MESSAGE_LEN => {
                return Err("attach reply too long".to_string());
            }
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("waiting for the server: {e}")),
        }
    }
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;
    serde_json::from_slice(&line).map_err(|e| format!("bad reply from server: {e}"))
}

/// Server frames to the tty until an `exit` frame or the server hangs up.
fn read_frames(stream: UnixStream) -> Result<String, String> {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        while let Some(frame) = FrameCodec
            .decode(&mut buf)
            .map_err(|e| format!("bad frame from server: {e}"))?
        {
            match frame {
                Frame::Output(bytes) => {
                    write_tty(&bytes).map_err(|e| format!("writing to the terminal: {e}"))?
                }
                Frame::Exit(reason) => return Ok(reason),
                // Client-to-server kinds: ignored if a server sends them.
                Frame::Input(_) | Frame::Resize(..) | Frame::Detach => {}
            }
        }
        match io::Read::read(&mut &stream, &mut chunk) {
            Ok(0) => return Err("lost server".to_string()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("lost server: {e}")),
        }
    }
}

/// How long a held-back ESC (a lone Escape key, or the start of a late
/// reply) waits for the rest before it is forwarded as a key: the default
/// escape-time. Only a lone ESC or a reply prefix at the end of a read
/// waits, and only until DA1's reply ends the filtering.
const PENDING_WAIT: Duration = Duration::from_millis(25);

/// stdin to the writer; EOF on the tty detaches. With `filter`, bytes go
/// through the probe parser until DA1's reply arrives, so late replies are
/// dropped instead of reaching the server as keys.
fn stdin_loop(out: SyncSender<Out>, mut filter: Option<ProbeParser>) {
    let test_panic = std::env::var(TEST_PANIC_ENV).is_ok_and(|v| v == "stdin");
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(p) = filter.as_mut()
            && p.has_pending()
            && !wait_ready(
                stdin_fd(),
                PollFlags::POLLIN,
                Some(Instant::now() + PENDING_WAIT),
            )
            .unwrap_or(false)
        {
            p.flush_pending();
            let bytes = p.take_typeahead();
            if out.send(Out::Input(bytes)).is_err() {
                return;
            }
        }
        let n = match nix::unistd::read(stdin_fd(), &mut chunk) {
            Ok(0) => {
                let _ = out.send(Out::Detach);
                return;
            }
            Ok(n) => n,
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => {
                let _ = wait_ready(stdin_fd(), PollFlags::POLLIN, None);
                continue;
            }
            Err(_) => {
                let _ = out.send(Out::Detach);
                return;
            }
        };
        if test_panic {
            panic!("{TEST_PANIC_ENV}: test panic in the stdin thread");
        }
        let bytes = match filter.as_mut() {
            Some(p) => {
                p.feed(&chunk[..n]);
                let bytes = p.take_typeahead();
                if p.done() {
                    filter = None;
                }
                bytes
            }
            None => chunk[..n].to_vec(),
        };
        if !bytes.is_empty() && out.send(Out::Input(bytes)).is_err() {
            return; // the writer is gone; the reader sees the hang-up
        }
    }
}

/// The only writer on the connection: queued frames, plus one `resize`
/// whenever [`RESIZE_PENDING`] is set.
fn writer_loop(stream: UnixStream, queue: Receiver<Out>) {
    let mut buf = BytesMut::new();
    loop {
        let mut frames = Vec::new();
        if RESIZE_PENDING.swap(false, Ordering::SeqCst) {
            let (cols, rows) = tty_size();
            frames.push(Frame::Resize(cols, rows));
        }
        match queue.try_recv() {
            Ok(Out::Input(bytes)) => frames.push(Frame::Input(bytes)),
            Ok(Out::Detach) => frames.push(Frame::Detach),
            Ok(Out::Wake) => {}
            Err(TryRecvError::Empty) if frames.is_empty() => match queue.recv() {
                Ok(Out::Input(bytes)) => frames.push(Frame::Input(bytes)),
                Ok(Out::Detach) => frames.push(Frame::Detach),
                Ok(Out::Wake) => {}
                Err(_) => return,
            },
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return,
        }
        for frame in frames {
            buf.clear();
            if FrameCodec.encode(frame, &mut buf).is_err()
                || io::Write::write_all(&mut &stream, &buf).is_err()
            {
                return;
            }
        }
    }
}

/// A panic anywhere restores the tty, prints as usual, and ends the process.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_tty_bounded();
        default(info);
        std::process::exit(101);
    }));
}

extern "C" fn noop_handler(_: nix::libc::c_int) {}

/// Blocks the signals the client handles in this thread (inherited by
/// every thread spawned after) and takes them in one `sigwait` thread,
/// which never blocks on the connection or the tty: SIGWINCH only sets a
/// flag and nudges the writer; the others restore within
/// [`RESTORE_BOUND`] and exit.
fn start_signal_thread(out: SyncSender<Out>) -> Result<(), String> {
    let mut set = SigSet::empty();
    for sig in [
        Signal::SIGWINCH,
        Signal::SIGTERM,
        Signal::SIGHUP,
        Signal::SIGINT,
        Signal::SIGQUIT,
    ] {
        set.add(sig);
    }
    // SIGWINCH's default action is to ignore it, and macOS discards a
    // blocked signal whose action is ignore instead of leaving it pending
    // for sigwait; a no-op handler keeps it.
    let noop = SigAction::new(
        SigHandler::Handler(noop_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler does nothing, so it is async-signal-safe.
    unsafe { sigaction(Signal::SIGWINCH, &noop) }.map_err(|e| format!("SIGWINCH handler: {e}"))?;
    set.thread_block()
        .map_err(|e| format!("blocking signals: {e}"))?;
    std::thread::Builder::new()
        .name("signals".to_string())
        .spawn(move || {
            loop {
                match set.wait() {
                    Ok(Signal::SIGWINCH) => {
                        RESIZE_PENDING.store(true, Ordering::SeqCst);
                        // Full means the writer has work queued and will
                        // see the flag after it.
                        let _ = out.try_send(Out::Wake);
                    }
                    Ok(sig) => {
                        restore_tty_bounded();
                        std::process::exit(128 + sig as i32);
                    }
                    Err(_) => {}
                }
            }
        })
        .map(drop)
        .map_err(|e| format!("starting the signal thread: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture under tests/fixtures/probe: `#` lines are comments, `\e` is
    /// ESC and `\\` a backslash.
    fn fixture(text: &str) -> Vec<u8> {
        let body: String = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .concat();
        let mut out = Vec::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('e') => out.push(0x1b),
                    Some('\\') => out.push(b'\\'),
                    other => panic!("bad escape in fixture: \\{other:?}"),
                }
            } else {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
        out
    }

    fn parse(bytes: &[u8]) -> ProbeParser {
        let mut p = ProbeParser::default();
        p.feed(bytes);
        p
    }

    const ALL: Caps = Caps {
        truecolor: true,
        styled_underline: true,
        sync: true,
        kitty_keys: true,
        kitty_graphics: true,
    };

    #[test]
    fn ghostty_replies_give_every_cap() {
        let p = parse(&fixture(include_str!(
            "../tests/fixtures/probe/ghostty.txt"
        )));
        assert!(p.done());
        assert_eq!(p.caps(None), ALL);
        assert!(p.into_typeahead().is_empty());
    }

    #[test]
    fn wezterm_replies_give_every_cap() {
        let p = parse(&fixture(include_str!(
            "../tests/fixtures/probe/wezterm.txt"
        )));
        assert!(p.done());
        assert_eq!(p.caps(None), ALL);
    }

    #[test]
    fn wezterm_without_kitty_keyboard_has_kitty_keys_off_and_the_rest_on() {
        let p = parse(&fixture(include_str!(
            "../tests/fixtures/probe/wezterm-no-kitty-keys.txt"
        )));
        assert!(p.done());
        assert_eq!(
            p.caps(None),
            Caps {
                kitty_keys: false,
                ..ALL
            }
        );
    }

    #[test]
    fn replies_split_across_reads_still_parse() {
        let bytes = fixture(include_str!("../tests/fixtures/probe/ghostty.txt"));
        let mut p = ProbeParser::default();
        for b in &bytes {
            p.feed(std::slice::from_ref(b));
        }
        assert!(p.done());
        assert_eq!(p.caps(None), ALL);
        assert!(p.into_typeahead().is_empty());
    }

    #[test]
    fn typeahead_around_replies_is_kept_in_order() {
        let mut p = ProbeParser::default();
        p.feed(b"ab\x1b[A\x1b[?2026;2$yc");
        p.feed(b"\x1b[?62;22cde");
        assert!(p.done());
        assert!(p.caps(None).sync);
        assert_eq!(p.into_typeahead(), b"ab\x1b[Acde");
    }

    #[test]
    fn an_unfinished_reply_at_the_timeout_is_typeahead() {
        let p = parse(b"x\x1bP>|ghos");
        assert!(!p.done());
        assert_eq!(p.into_typeahead(), b"x\x1bP>|ghos");
        // A lone ESC typed by the user.
        assert_eq!(parse(b"\x1b").into_typeahead(), b"\x1b");
    }

    #[test]
    fn a_silent_terminal_gets_only_what_colorterm_says() {
        let p = ProbeParser::default();
        assert_eq!(p.caps(None), Caps::default());
        for (colorterm, truecolor) in [("truecolor", true), ("24bit", true), ("yes", false)] {
            assert_eq!(p.caps(Some(colorterm)).truecolor, truecolor, "{colorterm}");
        }
    }

    #[test]
    fn an_unknown_terminal_gets_what_it_answers() {
        let p = parse(
            b"\x1bP>|xterm(390)\x1b\\\x1b[?2026;0$y\x1b_Gi=31;ENOTSUPPORTED:x\x1b\\\x1b[?1;2c",
        );
        assert!(p.done());
        assert_eq!(p.caps(None), Caps::default());
        // DECRPM 4 is "permanently reset": the mode is not supported.
        assert!(!parse(b"\x1b[?2026;4$y").caps(None).sync);
        assert!(parse(b"\x1b[?2026;1$y").caps(None).sync);
    }

    #[test]
    fn a_late_reply_is_dropped_and_input_passes_through() {
        let mut p = ProbeParser::default();
        p.feed(b"k\x1b[?0u\x1b[?20");
        assert_eq!(p.take_typeahead(), b"k");
        assert!(p.has_pending());
        p.feed(b"26;2$y\x1b[?62cz");
        assert!(p.done());
        assert_eq!(p.take_typeahead(), b"z");
        // A lone ESC held back is released by the caller's timeout.
        let mut p = ProbeParser::default();
        p.feed(b"\x1b");
        assert!(p.take_typeahead().is_empty());
        p.flush_pending();
        assert_eq!(p.take_typeahead(), b"\x1b");
    }

    #[test]
    fn bytes_after_da1_are_typeahead() {
        let p = parse(b"\x1b[?62c\x1b[?0u");
        assert!(p.done());
        assert!(!p.caps(None).kitty_keys, "a reply after DA1 is input");
        assert_eq!(p.into_typeahead(), b"\x1b[?0u");
    }

    #[test]
    fn probe_query_sends_da1_last() {
        assert!(PROBE_QUERY.ends_with(b"\x1b[c"));
        assert_eq!(
            PROBE_QUERY.windows(3).filter(|w| *w == b"\x1b[c").count(),
            1
        );
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn raw_target_reads_every_spelling() {
        assert_eq!(raw_target(&args(&["-t", "s:1"])), Some("s:1".into()));
        assert_eq!(raw_target(&args(&["-ts:1"])), Some("s:1".into()));
        assert_eq!(
            raw_target(&args(&["-dtwork", "--nest"])),
            Some("work".into())
        );
        assert_eq!(
            raw_target(&args(&["--nest", "-d", "-t", "x"])),
            Some("x".into())
        );
        assert_eq!(raw_target(&args(&["-d"])), None);
        assert_eq!(raw_target(&args(&[])), None);
    }
}
