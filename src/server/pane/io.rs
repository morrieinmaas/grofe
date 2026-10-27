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

use super::written::Written;
use super::{Emu, PaneEvent, PaneEventKind, QueueFull, lock};
use crate::server::graphics;
use crate::server::imgstore::{Cursor, Forward};
use crate::server::passthrough::Scanner;
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
    /// `None` when the program failed to exec: the pane is born dead.
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) emu: Arc<Mutex<Emu>>,
    pub(crate) events: EventBuf,
    pub(crate) input: Arc<InputQueue>,
    pub(crate) dirty: Arc<AtomicBool>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) to_actor: mpsc::Sender<PaneEvent>,
    /// Runs over every read before the emulator (T3.3).
    pub(crate) scanner: Scanner,
    /// See `PaneHandle::set_graphics_ok`.
    pub(crate) graphics_ok: Arc<AtomicBool>,
    /// See `PaneHandle::set_images`.
    pub(crate) images_on: Arc<AtomicBool>,
}

/// Reader task: parse output, answer the emulator's queries, drive the sync
/// timeout, and report `Exited` once the child has exited and the master has
/// drained (EOF/EIO, or `EXIT_GRACE` of quiet).
pub(crate) async fn reader(mut r: Reader) {
    let mut buf = vec![0u8; READ_CHUNK];
    // A pane whose program never ran has no output to drain.
    let mut drained = r.child.is_none();
    let mut exit: Option<Option<i32>> = None;
    let mut quiet_since = Instant::now();

    while !(drained && exit.is_some()) {
        let mut tapped = None;
        let mut found = Vec::new();
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
                        found = r.feed(&buf[..n]);
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
            status = wait_exit(&mut r.child), if exit.is_none() => {
                exit = Some(status);
                quiet_since = quiet_since.max(Instant::now());
            }
            _ = tokio::time::sleep_until(sync.unwrap_or(grace)), if sync.is_some() => {
                r.flush_sync();
            }
            _ = tokio::time::sleep_until(grace), if exit.is_some() && !drained => {
                drained = true;
            }
        }
        // With backpressure, like `Exited`: the actor gets every byte, in
        // order ahead of the exit (a subscriber whose queue overflows still
        // loses some, and is told with `events-dropped`).
        if let Some(bytes) = tapped {
            let event = r.event(PaneEventKind::Output(bytes));
            let _ = r.to_actor.send(event).await;
        }
        // What the scanner found, with backpressure too: a lost graphics
        // chunk or cwd would not come again.
        for kind in found {
            let _ = r.to_actor.send(r.event(kind)).await;
        }
    }
    // A child that began a synchronized update and exited before its timeout
    // leaves text in the parser's sync buffer; show it before reporting exit.
    r.flush_sync();
    let event = r.event(PaneEventKind::Exited(exit.flatten()));
    let _ = r.to_actor.send(event).await;
}

/// The child's exit code; 1 for a program that failed to exec, as tmux
/// reports it.
async fn wait_exit(child: &mut Option<tokio::process::Child>) -> Option<i32> {
    match child {
        Some(child) => child.wait().await.ok().and_then(|s| s.code()),
        None => Some(1),
    }
}

impl Reader {
    /// An event from this pane, stamped with this spawn's generation.
    fn event(&self, kind: PaneEventKind) -> PaneEvent {
        PaneEvent {
            pane: self.id,
            generation: self.generation,
            kind,
        }
    }

    /// End a pending synchronized update, if any, and handle its events.
    fn flush_sync(&self) {
        let pending = {
            let mut emu = lock(&self.emu);
            let Emu {
                term,
                parser,
                images,
            } = &mut *emu;
            let pending = parser.sync_timeout().sync_timeout().is_some();
            if pending {
                let store = self.images_on.load(Ordering::Relaxed).then_some(&mut *images);
                parser.stop_sync(&mut Written(term, store));
            }
            pending
        };
        if pending {
            self.after_parse();
        }
    }

    /// Parses one read: through the scanner, then the emulator. Returns
    /// what the scanner found, for the caller to send on. A graphics query
    /// the scanner framed is answered here, straight back to the pane and
    /// ahead of the emulator's own replies (its DA1 answer, which a tool
    /// sends after the query), never through the actor (T3.8).
    fn feed(&mut self, bytes: &[u8]) -> Vec<PaneEventKind> {
        let images = self.images_on.load(Ordering::Relaxed);
        self.scanner.set_sixel(images);
        let (bytes, found) = self.scanner.scan_at(bytes);
        for (_, kind) in &found {
            if let PaneEventKind::Graphics(apc) = kind
                && let Some(answer) = graphics::answer_query(apc, &self.graphics_ok)
            {
                self.reply(answer);
            }
        }
        let mut events = Vec::new();
        {
            let mut emu = lock(&self.emu);
            let Emu {
                term,
                parser,
                images: store,
            } = &mut *emu;
            if !images {
                parser.advance(&mut Written(term, None), &bytes);
                // A query is answered here only; the actor never sees it.
                events.extend(found.into_iter().map(|(_, kind)| kind).filter(
                    |k| !matches!(k, PaneEventKind::Graphics(apc) if graphics::is_query(apc)),
                ));
            } else {
                let mut fed = 0;
                for (at, kind) in found {
                    match kind {
                        PaneEventKind::Graphics(apc) if graphics::is_query(&apc) => {}
                        PaneEventKind::Graphics(_) | PaneEventKind::Sixel { .. } => {
                            // The cursor must be where the command's bytes
                            // put it: past what the parser holds back in a
                            // synchronized update, and what precedes it.
                            parser.advance(&mut Written(term, Some(&mut *store)), &bytes[fed..at]);
                            fed = at;
                            if parser.sync_timeout().sync_timeout().is_some() {
                                parser.stop_sync(&mut Written(term, Some(&mut *store)));
                            }
                            let point = term.grid().cursor.point;
                            let cur = Cursor {
                                line: point.line.0.max(0) as u32,
                                col: point.column.0 as u32,
                            };
                            let (inject, forward) = match kind {
                                PaneEventKind::Sixel { p2, body } => {
                                    (store.on_sixel(&body, p2, cur), Forward::Replace(vec![]))
                                }
                                PaneEventKind::Graphics(apc) => {
                                    let out = store.on_apc(&apc, cur);
                                    match out.forward {
                                        Forward::Same => (out.inject, Forward::Replace(vec![apc])),
                                        other => (out.inject, other),
                                    }
                                }
                                _ => (Vec::new(), Forward::Replace(vec![])),
                            };
                            if !inject.is_empty() {
                                parser.advance(&mut Written(term, Some(&mut *store)), &inject);
                            }
                            if let Forward::Replace(list) = forward {
                                events.extend(list.into_iter().map(PaneEventKind::Graphics));
                            }
                        }
                        other => events.push(other),
                    }
                }
                parser.advance(&mut Written(term, Some(&mut *store)), &bytes[fed..]);
                store.prune(term.grid().history_size());
            }
        }
        self.after_parse();
        events
    }

    /// Handle buffered listener events, then mark the pane dirty and wake the
    /// render scheduler.
    fn after_parse(&self) {
        let events = std::mem::take(&mut *lock(&self.events));
        let mut bell = false;
        for event in events {
            match event {
                // DA1 as tmux 3.7b answers it: kitty's tools refuse
                // alacritty's shorter `ESC[?6c` as an end marker.
                Event::PtyWrite(text) if text == "\x1b[?6c" => self.reply("\x1b[?1;2;4c"),
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
                Event::Title(title) => self.notify(PaneEventKind::Title(title)),
                Event::ResetTitle => self.notify(PaneEventKind::Title(String::new())),
                Event::Bell => bell = true,
                // OSC 52 stores come from the scanner, which keeps the
                // selection parameter as tmux does (TM.5).
                _ => {}
            }
        }
        // One bell per batch: a flood of BELs (a tty discarding overflow input
        // rings once per byte) must not become a flood of events.
        if bell {
            self.notify(PaneEventKind::Bell);
        }
        self.dirty.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    fn reply(&self, bytes: impl Into<Vec<u8>>) {
        // A reply that does not fit is dropped; the queue is only full when
        // the child has stopped reading, so it would not see it anyway.
        let _ = self.input.push(bytes.into());
    }

    /// Best effort: a full actor queue drops titles and bells rather than
    /// stalling output. Only `Exited` is sent with backpressure.
    fn notify(&self, kind: PaneEventKind) {
        let _ = self.to_actor.try_send(self.event(kind));
    }
}
