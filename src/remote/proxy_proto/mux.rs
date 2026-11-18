//! The multiplexer: [`Mux`] runs the codec and credit windows over a
//! blocking reader and writer with one reader thread, and hands out a
//! [`Channel`] per stream.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::*;

/// Which side of the connection this is: it picks the ids it opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Opens odd channels.
    Client,
    /// Opens even channels.
    Proxy,
}

struct Closed {
    reason: String,
    local: bool,
}

#[derive(Default)]
struct ChanState {
    buf: VecDeque<u8>,
    eof_in: bool,
    eof_out: bool,
    closed: Option<Closed>,
    send: SendWindow,
    recv: RecvWindow,
    /// Credit the peer still holds: what it may send before a `window`.
    left: u32,
}

struct ChanShared {
    id: u32,
    st: Mutex<ChanState>,
    cv: Condvar,
}

/// Pings in flight and their pongs. A pong is kept only while its ping
/// still waits, so one that comes back after its ping timed out is dropped
/// instead of piling up.
#[derive(Default)]
struct Pongs {
    waiting: HashSet<u64>,
    seen: HashSet<u64>,
}

impl Pongs {
    /// A pong for `nonce` came in.
    fn arrived(&mut self, nonce: u64) {
        if self.waiting.contains(&nonce) {
            self.seen.insert(nonce);
        }
    }

    /// The ping for `nonce` is done waiting, answered or not.
    fn done(&mut self, nonce: u64) -> bool {
        self.waiting.remove(&nonce);
        self.seen.remove(&nonce)
    }
}

struct MuxInner {
    writer: Mutex<Box<dyn Write + Send>>,
    chans: Mutex<HashMap<u32, Arc<ChanShared>>>,
    next_id: AtomicU32,
    next_nonce: AtomicU64,
    pongs: Mutex<Pongs>,
    pong_cv: Condvar,
    dead: Mutex<Option<String>>,
    dead_cv: Condvar,
    is_dead: AtomicBool,
    /// When the last frame came in.
    last_rx: Mutex<Instant>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn wait<'a, T>(cv: &Condvar, g: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(g).unwrap_or_else(|p| p.into_inner())
}

impl MuxInner {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        if self.is_dead.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "proxy connection closed",
            ));
        }
        let result = write_frame(&mut **lock(&self.writer), frame);
        if let Err(e) = &result {
            self.fail_all(&format!("proxy connection lost: {e}"));
        }
        result
    }

    /// The connection is over: every channel ends with `reason`.
    fn fail_all(&self, reason: &str) {
        self.is_dead.store(true, Ordering::SeqCst);
        let chans: Vec<_> = lock(&self.chans).drain().map(|(_, c)| c).collect();
        for c in chans {
            let mut st = lock(&c.st);
            st.closed.get_or_insert(Closed {
                reason: reason.to_string(),
                local: false,
            });
            c.cv.notify_all();
        }
        lock(&self.dead).get_or_insert_with(|| reason.to_string());
        self.dead_cv.notify_all();
        self.pong_cv.notify_all();
    }
}

struct ChanHandle {
    shared: Arc<ChanShared>,
    mux: Arc<MuxInner>,
}

impl ChanHandle {
    fn close(&self, reason: &str) {
        {
            let mut st = lock(&self.shared.st);
            if st.closed.is_some() {
                return;
            }
            st.closed = Some(Closed {
                reason: reason.to_string(),
                local: true,
            });
            self.shared.cv.notify_all();
        }
        lock(&self.mux.chans).remove(&self.shared.id);
        let _ = self
            .mux
            .send(&Frame::Close(self.shared.id, reason.to_string()));
    }
}

impl Drop for ChanHandle {
    fn drop(&mut self) {
        self.close("");
    }
}

/// One channel. Clones share it, so one thread can read while another
/// writes; the last clone dropped closes it. Reads end with `Ok(0)` on `eof`
/// or a normal close and fail with the peer's reason on an error close.
#[derive(Clone)]
pub struct Channel(Arc<ChanHandle>);

impl Channel {
    pub fn id(&self) -> u32 {
        self.0.shared.id
    }

    /// Tells the peer no more data is coming (half close).
    pub fn shutdown_write(&self) -> io::Result<()> {
        {
            let mut st = lock(&self.0.shared.st);
            if st.eof_out || st.closed.is_some() {
                return Ok(());
            }
            st.eof_out = true;
        }
        self.0.mux.send(&Frame::Eof(self.id()))
    }

    /// Whether the peer sent `eof`: it finished sending, rather than
    /// closing the channel mid way.
    pub fn got_eof(&self) -> bool {
        lock(&self.0.shared.st).eof_in
    }

    /// Whether the channel is closed, by either side or by the connection ending.
    pub fn is_closed(&self) -> bool {
        lock(&self.0.shared.st).closed.is_some()
    }

    /// Closes the channel with `reason` (empty: normal).
    pub fn close(&self, reason: &str) {
        self.0.close(reason);
    }

    fn read_into(&self, out: &mut [u8]) -> io::Result<usize> {
        let sh = &self.0.shared;
        let mut st = lock(&sh.st);
        loop {
            if !st.buf.is_empty() {
                let n = out.len().min(st.buf.len());
                for (slot, b) in out.iter_mut().zip(st.buf.drain(..n)) {
                    *slot = b;
                }
                let grant = st.recv.consumed(n);
                if let Some(credit) = grant {
                    st.left = st.left.saturating_add(credit);
                }
                let open = st.closed.is_none();
                drop(st);
                if let Some(credit) = grant
                    && open
                {
                    let _ = self.0.mux.send(&Frame::Window(sh.id, credit));
                }
                return Ok(n);
            }
            if st.eof_in {
                return Ok(0);
            }
            if let Some(c) = &st.closed {
                return if c.local || c.reason.is_empty() {
                    Ok(0)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        c.reason.clone(),
                    ))
                };
            }
            st = wait(&sh.cv, st);
        }
    }

    fn write_from(&self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let sh = &self.0.shared;
        let mut st = lock(&sh.st);
        let n = loop {
            if st.eof_out {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "write after shutdown",
                ));
            }
            if let Some(c) = &st.closed {
                let why = if c.reason.is_empty() {
                    "channel closed"
                } else {
                    &c.reason
                };
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, why.to_string()));
            }
            let n = st.send.take(data.len().min(MAX_CHUNK));
            if n > 0 {
                break n;
            }
            st = wait(&sh.cv, st);
        };
        drop(st);
        self.0.mux.send(&Frame::Data(sh.id, data[..n].to_vec()))?;
        Ok(n)
    }
}

impl Read for Channel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_into(buf)
    }
}

impl Write for Channel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_from(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A running connection: a reader thread dispatching frames to channels.
#[derive(Clone)]
pub struct Mux {
    inner: Arc<MuxInner>,
}

impl Mux {
    /// Starts the reader thread over `reader` (after the hello), writing
    /// frames to `writer`. `on_open` gets each channel the peer opens and its
    /// payload; it runs on the reader thread, so it must not block.
    pub fn start<R, W>(
        reader: R,
        writer: W,
        role: Role,
        on_open: impl Fn(Channel, Vec<u8>) + Send + 'static,
    ) -> Mux
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let inner = Arc::new(MuxInner {
            writer: Mutex::new(Box::new(writer)),
            chans: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(if role == Role::Client { 1 } else { 2 }),
            next_nonce: AtomicU64::new(1),
            pongs: Mutex::new(Pongs::default()),
            pong_cv: Condvar::new(),
            dead: Mutex::new(None),
            dead_cv: Condvar::new(),
            is_dead: AtomicBool::new(false),
            last_rx: Mutex::new(Instant::now()),
        });
        let thread_inner = Arc::clone(&inner);
        std::thread::spawn(move || read_loop(thread_inner, reader, role, on_open));
        Mux { inner }
    }

    fn register(&self, id: u32) -> Channel {
        let shared = Arc::new(ChanShared {
            id,
            st: Mutex::new(ChanState {
                left: INITIAL_WINDOW,
                ..ChanState::default()
            }),
            cv: Condvar::new(),
        });
        lock(&self.inner.chans).insert(id, Arc::clone(&shared));
        Channel(Arc::new(ChanHandle {
            shared,
            mux: Arc::clone(&self.inner),
        }))
    }

    /// Opens a channel as asked by `spec`.
    pub fn open(&self, spec: &OpenSpec) -> io::Result<Channel> {
        if self.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "proxy connection closed",
            ));
        }
        let id = self.inner.next_id.fetch_add(2, Ordering::SeqCst);
        let payload = serde_json::to_vec(spec).map_err(io::Error::other)?;
        let chan = self.register(id);
        self.inner.send(&Frame::Open(id, payload))?;
        Ok(chan)
    }

    /// Round trip time of a ping, or an error after `timeout` or when the
    /// connection ends.
    pub fn ping(&self, timeout: Duration) -> io::Result<Duration> {
        let nonce = self.inner.next_nonce.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.pongs).waiting.insert(nonce);
        let start = Instant::now();
        if let Err(e) = self.inner.send(&Frame::Ping(nonce)) {
            lock(&self.inner.pongs).done(nonce);
            return Err(e);
        }
        let deadline = start + timeout;
        let mut pongs = lock(&self.inner.pongs);
        loop {
            if pongs.seen.contains(&nonce) {
                pongs.done(nonce);
                return Ok(start.elapsed());
            }
            if self.is_closed() {
                pongs.done(nonce);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "proxy connection closed",
                ));
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                pongs.done(nonce);
                return Err(io::Error::new(io::ErrorKind::TimedOut, "ping timed out"));
            };
            pongs = self
                .inner
                .pong_cv
                .wait_timeout(pongs, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_dead.load(Ordering::SeqCst)
    }

    /// How long since the peer last sent a frame.
    pub fn idle(&self) -> Duration {
        lock(&self.inner.last_rx).elapsed()
    }

    /// Ends the connection here, without writing to the peer (which may
    /// not be reading): every channel fails with `reason`.
    pub fn end(&self, reason: &str) {
        self.inner.fail_all(reason);
    }

    /// Blocks until the connection ends; why it ended.
    pub fn wait_closed(&self) -> String {
        let mut dead = lock(&self.inner.dead);
        loop {
            if let Some(reason) = dead.as_ref() {
                return reason.clone();
            }
            dead = wait(&self.inner.dead_cv, dead);
        }
    }
}

fn read_loop<R: Read>(
    inner: Arc<MuxInner>,
    reader: R,
    role: Role,
    on_open: impl Fn(Channel, Vec<u8>),
) {
    let mux = Mux {
        inner: Arc::clone(&inner),
    };
    let mut reader = BufReader::new(reader);
    let peer_parity = if role == Role::Client { 0 } else { 1 };
    let why = loop {
        let frame = match next_frame(&inner, &mut reader) {
            Ok(f) => f,
            Err(why) => break why,
        };
        match frame {
            Frame::Open(id, payload) => {
                if id == 0 || id % 2 != peer_parity || lock(&inner.chans).contains_key(&id) {
                    let _ = inner.send(&Frame::Close(id, "bad channel id".into()));
                } else {
                    on_open(mux.register(id), payload);
                }
            }
            Frame::Data(id, bytes) => on_data(&inner, id, bytes),
            Frame::Eof(id) => {
                if let Some(c) = channel(&inner, id) {
                    lock(&c.st).eof_in = true;
                    c.cv.notify_all();
                }
            }
            Frame::Close(0, reason) => break format!("proxy closed the connection: {reason}"),
            Frame::Close(id, reason) => on_close(&inner, id, reason),
            Frame::Ping(n) => {
                let _ = inner.send(&Frame::Pong(n));
            }
            Frame::Pong(n) => {
                lock(&inner.pongs).arrived(n);
                inner.pong_cv.notify_all();
            }
            Frame::Window(id, credit) => {
                if let Some(c) = channel(&inner, id) {
                    lock(&c.st).send.add(credit);
                    c.cv.notify_all();
                }
            }
        }
    };
    inner.fail_all(&why);
}

/// The next frame, noting when it came; why the connection ends when there
/// is none (a protocol error is answered with `close` on channel 0 first).
fn next_frame<R: Read>(inner: &MuxInner, reader: &mut BufReader<R>) -> Result<Frame, String> {
    match read_frame(reader) {
        Ok(Some(f)) => {
            *lock(&inner.last_rx) = Instant::now();
            Ok(f)
        }
        Ok(None) => Err("proxy connection closed".to_string()),
        Err(
            e @ (FrameError::UnknownKind(_) | FrameError::TooLong(_) | FrameError::Malformed(_)),
        ) => {
            let _ = inner.send(&Frame::Close(0, e.to_string()));
            Err(format!("proxy protocol error: {e}"))
        }
        Err(e) => Err(format!("proxy connection lost: {e}")),
    }
}

fn channel(inner: &MuxInner, id: u32) -> Option<Arc<ChanShared>> {
    lock(&inner.chans).get(&id).cloned()
}

/// `data` for channel `id`: buffered within the credit it was given; past
/// it the channel is closed, so the buffer stays bounded.
fn on_data(inner: &MuxInner, id: u32, bytes: Vec<u8>) {
    let Some(c) = channel(inner, id) else {
        return;
    };
    let mut st = lock(&c.st);
    match st.left.checked_sub(bytes.len() as u32) {
        Some(left) => {
            st.left = left;
            st.buf.extend(bytes);
        }
        None => {
            let reason = OVERRUN.to_string();
            st.buf.clear();
            st.closed.get_or_insert(Closed {
                reason: reason.clone(),
                local: false,
            });
            drop(st);
            lock(&inner.chans).remove(&id);
            let _ = inner.send(&Frame::Close(id, reason));
        }
    }
    c.cv.notify_all();
}

/// The peer closed channel `id`.
fn on_close(inner: &MuxInner, id: u32, reason: String) {
    if let Some(c) = lock(&inner.chans).remove(&id) {
        lock(&c.st).closed.get_or_insert(Closed {
            reason,
            local: false,
        });
        c.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pong_after_its_ping_timed_out_is_dropped() {
        let mut p = Pongs::default();
        p.waiting.insert(1);
        p.arrived(1);
        assert!(p.done(1), "an answered ping sees its pong");
        assert!(p.waiting.is_empty() && p.seen.is_empty());
        // Ping 2 timed out (done without a pong); its pong comes late.
        p.waiting.insert(2);
        assert!(!p.done(2));
        p.arrived(2);
        // A pong nobody asked for.
        p.arrived(99);
        assert!(p.seen.is_empty(), "late pongs kept: {:?}", p.seen);
    }
}
