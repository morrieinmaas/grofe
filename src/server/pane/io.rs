//! reader + writer tasks, EventListener.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::vte::ansi::Rgb;
use nix::errno::Errno;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use super::{Emu, PaneEvent, QueueFull, lock};
use crate::types::PaneId;

/// Bytes parsed per lock hold (spec §4.8).
const READ_CHUNK: usize = 16 * 1024;
/// Most bytes the writer queue holds before `enqueue_input` refuses more.
const QUEUE_BYTES: usize = 4 * 1024 * 1024;
/// Chunks the writer channel holds; the byte budget is the real bound.
const QUEUE_CHUNKS: usize = 4096;
/// Most bytes per `write` syscall. macOS takes a whole large write into a
/// canonical-mode pty in one call and handles every byte in the kernel, which
/// would pin a runtime worker for seconds; small writes let the task yield.
const WRITE_CHUNK: usize = 4096;
/// After the child exits, the master counts as drained once it has been quiet
/// this long, for when a background grandchild still holds the slave.
const EXIT_GRACE: Duration = Duration::from_millis(200);

/// See [`super::set_output_tap`].
pub(crate) static OUTPUT_TAP: AtomicBool = AtomicBool::new(false);

/// Listener events are buffered while the emulator lock is held and handled
/// by the reader after unlocking.
pub(crate) type EventBuf = Arc<Mutex<Vec<Event>>>;

pub struct Listener(pub(crate) EventBuf);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        lock(&self.0).push(event);
    }
}

/// The pane's bounded writer queue: a channel plus a byte budget.
pub(crate) struct InputQueue {
    tx: mpsc::Sender<Vec<u8>>,
    queued: AtomicUsize,
}

impl InputQueue {
    pub(crate) fn new() -> (Arc<Self>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(QUEUE_CHUNKS);
        let queue = Self {
            tx,
            queued: AtomicUsize::new(0),
        };
        (Arc::new(queue), rx)
    }

    /// Enqueue without blocking. Bytes for a pane whose writer has stopped
    /// (the pane is dead) are dropped silently.
    pub(crate) fn push(&self, bytes: Vec<u8>) -> Result<(), QueueFull> {
        if bytes.is_empty() {
            return Ok(());
        }
        let len = bytes.len();
        let before = self.queued.fetch_add(len, Ordering::AcqRel);
        // An empty queue takes any single chunk, so one large paste is not
        // refused outright.
        if before > 0 && before + len > QUEUE_BYTES {
            self.queued.fetch_sub(len, Ordering::AcqRel);
            return Err(QueueFull);
        }
        match self.tx.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.queued.fetch_sub(len, Ordering::AcqRel);
                Err(QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.queued.fetch_sub(len, Ordering::AcqRel);
                Ok(())
            }
        }
    }

    fn written(&self, len: usize) {
        self.queued.fetch_sub(len, Ordering::AcqRel);
    }
}

/// Writer task: sole owner of writes to the master, so replies and input
/// keep their order.
pub(crate) async fn writer(
    master: Arc<AsyncFd<OwnedFd>>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    queue: Arc<InputQueue>,
) {
    while let Some(chunk) = rx.recv().await {
        let mut off = 0;
        while off < chunk.len() {
            let Ok(mut guard) = master.writable().await else {
                return;
            };
            match guard.try_io(|fd| {
                let end = chunk.len().min(off + WRITE_CHUNK);
                nix::unistd::write(fd.get_ref(), &chunk[off..end]).map_err(std::io::Error::from)
            }) {
                Ok(Ok(n)) => off += n,
                Ok(Err(e)) if e.raw_os_error() == Some(Errno::EINTR as i32) => {}
                Ok(Err(_)) => return,
                Err(_would_block) => {}
            }
        }
        queue.written(chunk.len());
    }
}

/// Everything the reader task owns.
pub(crate) struct Reader {
    pub(crate) id: PaneId,
    pub(crate) generation: u64,
    pub(crate) master: Arc<AsyncFd<OwnedFd>>,
    pub(crate) child: tokio::process::Child,
    pub(crate) emu: Arc<Mutex<Emu>>,
    pub(crate) events: EventBuf,
    pub(crate) input: Arc<InputQueue>,
    pub(crate) dirty: Arc<AtomicBool>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) to_actor: mpsc::Sender<PaneEvent>,
}

/// Reader task: parse output, answer the emulator's queries, drive the sync
/// timeout, and report `Exited` once the child has exited and the master has
/// drained (EOF/EIO, or `EXIT_GRACE` of quiet).
pub(crate) async fn reader(mut r: Reader) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut drained = false;
    let mut exit: Option<Option<i32>> = None;
    let mut quiet_since = Instant::now();

    while !(drained && exit.is_some()) {
        let mut tapped = None;
        let sync = lock(&r.emu)
            .parser
            .sync_timeout()
            .sync_timeout()
            .map(Instant::from_std);
        let grace = quiet_since + EXIT_GRACE;
        tokio::select! {
            ready = r.master.readable(), if !drained => {
                let Ok(mut guard) = ready else {
                    drained = true;
                    continue;
                };
                match guard.try_io(|fd| {
                    nix::unistd::read(fd.get_ref(), &mut buf).map_err(std::io::Error::from)
                }) {
                    Ok(Ok(0)) => drained = true,
                    Ok(Ok(n)) => {
                        r.feed(&buf[..n]);
                        quiet_since = Instant::now();
                        if OUTPUT_TAP.load(Ordering::Relaxed) {
                            tapped = Some(buf[..n].to_vec());
                        }
                    }
                    Ok(Err(e)) if e.raw_os_error() == Some(Errno::EINTR as i32) => {}
                    // EIO once every slave holder has gone.
                    Ok(Err(_)) => drained = true,
                    Err(_would_block) => {}
                }
            }
            status = r.child.wait(), if exit.is_none() => {
                exit = Some(status.ok().and_then(|s| s.code()));
                quiet_since = quiet_since.max(Instant::now());
            }
            _ = tokio::time::sleep_until(sync.unwrap_or(grace)), if sync.is_some() => {
                r.flush_sync();
            }
            _ = tokio::time::sleep_until(grace), if exit.is_some() && !drained => {
                drained = true;
            }
        }
        // With backpressure, like `Exited`, so a subscriber sees every byte
        // and the output keeps its order ahead of the exit.
        if let Some(bytes) = tapped {
            let _ = r.to_actor.send(PaneEvent::Output(r.id, bytes)).await;
        }
    }
    // A child that began a synchronized update and exited before its timeout
    // leaves text in the parser's sync buffer; show it before reporting exit.
    r.flush_sync();
    let _ = r
        .to_actor
        .send(PaneEvent::Exited(r.id, r.generation, exit.flatten()))
        .await;
}

impl Reader {
    /// End a pending synchronized update, if any, and handle its events.
    fn flush_sync(&self) {
        let pending = {
            let mut emu = lock(&self.emu);
            let Emu { term, parser } = &mut *emu;
            let pending = parser.sync_timeout().sync_timeout().is_some();
            if pending {
                parser.stop_sync(term);
            }
            pending
        };
        if pending {
            self.after_parse();
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        {
            let mut emu = lock(&self.emu);
            let Emu { term, parser } = &mut *emu;
            parser.advance(term, bytes);
        }
        self.after_parse();
    }

    /// Handle buffered listener events, then mark the pane dirty and wake the
    /// render scheduler.
    fn after_parse(&self) {
        let events = std::mem::take(&mut *lock(&self.events));
        let mut bell = false;
        for event in events {
            match event {
                Event::PtyWrite(text) => self.reply(text),
                Event::ColorRequest(index, format) => {
                    // ponytail: answers fg/bg/cursor from a fixed palette until
                    // the configured palette exists (M3).
                    let rgb = match index {
                        256 | 258 => Rgb {
                            r: 0xd0,
                            g: 0xd0,
                            b: 0xd0,
                        },
                        257 => Rgb { r: 0, g: 0, b: 0 },
                        _ => continue,
                    };
                    self.reply(format(rgb));
                }
                Event::TextAreaSizeRequest(format) => {
                    let (cols, rows) = {
                        let emu = lock(&self.emu);
                        (emu.term.columns() as u16, emu.term.screen_lines() as u16)
                    };
                    self.reply(format(WindowSize {
                        num_lines: rows,
                        num_cols: cols,
                        cell_width: 0,
                        cell_height: 0,
                    }));
                }
                Event::Title(title) => self.notify(PaneEvent::Title(self.id, title)),
                Event::ResetTitle => self.notify(PaneEvent::Title(self.id, String::new())),
                Event::Bell => bell = true,
                Event::ClipboardStore(_, text) => {
                    self.notify(PaneEvent::Clipboard(self.id, text));
                }
                _ => {}
            }
        }
        // One bell per batch: a flood of BELs (a tty discarding overflow input
        // rings once per byte) must not become a flood of events.
        if bell {
            self.notify(PaneEvent::Bell(self.id));
        }
        self.dirty.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    fn reply(&self, text: String) {
        // A reply that does not fit is dropped; the queue is only full when
        // the child has stopped reading, so it would not see it anyway.
        let _ = self.input.push(text.into_bytes());
    }

    /// Best effort: a full actor queue drops titles and bells rather than
    /// stalling output. Only `Exited` is sent with backpressure.
    fn notify(&self, event: PaneEvent) {
        let _ = self.to_actor.try_send(event);
    }
}
