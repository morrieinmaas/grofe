//! The remote attach client (spec ssh-remote §2): the local tty side of
//! `client.rs` (raw mode, probe, signal and stdin threads, teardown) over
//! a [`Link`] instead of a socket, and a loop over connections so a drop
//! does not end the client.
//!
//! One persistent stdin thread and the signal thread feed one queue; each
//! connection adds a reader thread (attach frames) and, when a hook wants
//! events, an event thread (a control channel `subscribe`d to them). The
//! main thread owns the tty writes, the attach channel's writes, the
//! hooks and the overlay. On a drop the last frame stays, the bottom row
//! shows the reconnect bar, input is dropped except the detach key
//! (prefix, then `d`), and [`Backoff`] times the retries; the reconnect
//! re-attaches to the same session by id and the server's full redraw
//! replaces the stale frame.
//!
//! # Hooks
//!
//! Other remote features plug in through [`Hook`] (all methods have no-op
//! defaults) and [`Prompt`]. Hooks run on the main thread, in the order
//! given, only while a connection is up:
//!
//! - [`Hook::events`]: event names to `subscribe` to on each connection's
//!   control channel (`open-url`, `ports-changed`, `pane-echo`, `notify`,
//!   agent events); each arrives in [`Hook::on_event`] as the whole JSON
//!   line.
//! - [`Hook::on_input`]: keys typed, before the server sees them; returns
//!   what to send (possibly changed) and what to draw locally. Each hook
//!   gets what the previous one decided to send.
//! - [`Hook::before_output`]: server output about to reach the tty;
//!   returns bytes to draw before it.
//! - [`Hook::on_output`]: server output, after it reached the tty; returns
//!   extra bytes to draw after it.
//! - [`Hook::tick`]: about every [`TICK`], for timeouts.
//! - [`HookCtx`]: the [`Link`] (channels, uploads, RTT), control requests
//!   (blocking, or off the main thread with [`HookCtx::request_bg`]), a
//!   redraw, [`HookCtx::draw`] to write to the tty from any hook method,
//!   and [`HookCtx::prompt`] to raise a one-line prompt on the
//!   bottom row; while it is up, input goes to [`Prompt::key`] instead of
//!   the hooks and the server.

use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder as _, Encoder as _};

use super::connect::Link;
use super::reconnect::{Backoff, Clocks, DetachKey, Jitter, bar_text, bottom_row, woke};
use crate::client::{self, AttachOptions, Out, RESIZE_PENDING};
use crate::protocol::{
    Caps, ClientCtx, Frame, FrameCodec, HELLO_REPLY_ID, Hello, Reply, encode_hello,
};
use crate::server::input::{Key, KeyCode, parse_prefix};

/// How often [`Hook::tick`] runs (and the bar's countdown is checked).
pub const TICK: Duration = Duration::from_millis(100);

/// The prefix assumed when the server's cannot be read.
const DEFAULT_PREFIX: &str = "C-b";

// ------------------------------------------------------------------ hooks

/// What to do with typed input: bytes for the server, bytes for the tty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    pub send: Vec<u8>,
    pub draw: Vec<u8>,
}

impl Input {
    /// Send `bytes` as they are, draw nothing.
    pub fn send(bytes: Vec<u8>) -> Input {
        Input {
            send: bytes,
            draw: Vec::new(),
        }
    }
}

/// A seam for a remote feature (forwarding, predictive echo, machines).
/// Every method has a no-op default.
pub trait Hook: Send {
    /// Events to subscribe to on every connection.
    fn events(&self) -> Vec<String> {
        Vec::new()
    }

    /// Typed input on its way to the server.
    fn on_input(&mut self, _cx: &mut HookCtx<'_>, bytes: Vec<u8>) -> Input {
        Input::send(bytes)
    }

    /// Server output about to be written to the tty; bytes to write before it.
    fn before_output(&mut self, _cx: &mut HookCtx<'_>, _bytes: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// Server output just written to the tty; extra bytes to write after it.
    fn on_output(&mut self, _cx: &mut HookCtx<'_>, _bytes: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// An event line from the control channel (the whole JSON object).
    fn on_event(&mut self, _cx: &mut HookCtx<'_>, _event: &serde_json::Value) {}

    /// About every [`TICK`].
    fn tick(&mut self, _cx: &mut HookCtx<'_>, _now: Instant) {}
}

/// A one-line prompt on the bottom row (reverse video). While it is up
/// every input goes to [`Prompt::key`].
pub trait Prompt: Send {
    /// The line to show.
    fn text(&self) -> String;

    /// Input while the prompt is up: `Some` closes it, sending and drawing
    /// what it carries (and the screen is redrawn); `None` keeps it up
    /// (and its text is drawn again).
    fn key(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Option<Input>;

    /// Bytes that draw the prompt as an overlay on a `cols` x `rows`
    /// screen instead of the bottom row (the machine picker's box).
    fn overlay(&self, _cols: u16, _rows: u16) -> Option<Vec<u8>> {
        None
    }
}

/// Another link to move the attach to ([`HookCtx::switch`]).
pub struct Switch {
    pub link: Link,
    /// The host, for the bar.
    pub host: String,
    /// The session to attach to there (`None`: the server picks).
    pub target: Option<String>,
    /// Makes a new link to that host for a reconnect; it must never prompt.
    pub relink: Arc<dyn Fn() -> Result<Link, String> + Send + Sync>,
}

/// What a hook can reach.
pub struct HookCtx<'a> {
    link: &'a Link,
    conn: &'a Conn,
    host: &'a str,
    hello: &'a str,
    tty: &'a str,
    prompt: &'a mut Option<Box<dyn Prompt>>,
    draw: Vec<u8>,
    generation: u64,
    switch: &'a mut Option<Switch>,
}

impl HookCtx<'_> {
    /// The live link: open channels (`tcp`, `upload`), read the RTT.
    pub fn link(&self) -> &Link {
        self.link
    }

    /// The remote host, for messages.
    pub fn host(&self) -> &str {
        self.host
    }

    /// Which connection this is: it changes with every reconnect, when
    /// the server forgets this client and draws its screen anew.
    pub fn connection(&self) -> u64 {
        self.generation
    }

    /// The smoothed round trip time ([`Link::rtt`]).
    pub fn rtt(&self) -> Option<Duration> {
        self.link.rtt()
    }

    /// A control request on its own channel; its output or error.
    pub fn request(&self, argv: &[String]) -> Result<String, String> {
        self.link.request(self.hello, argv)
    }

    /// A control request on its own thread, so a slow link does not stall
    /// the main loop; its output or error arrives on the receiver.
    pub fn request_bg(&self, argv: Vec<String>) -> Receiver<Result<String, String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (link, hello) = (self.link.clone(), self.hello.to_string());
        std::thread::spawn(move || {
            let _ = tx.send(link.request(&hello, &argv));
        });
        rx
    }

    /// Writes `bytes` to the tty once the hook call returns.
    pub fn draw(&mut self, bytes: &[u8]) {
        self.draw.extend_from_slice(bytes);
    }

    /// This client's name for `-t`/`-c` in requests.
    pub fn client(&self) -> &str {
        self.tty
    }

    /// Asks the server to draw this client's screen anew.
    pub fn redraw(&self) {
        let _ = self.request(&["refresh-client".into(), "-t".into(), self.tty.into()]);
    }

    /// Raises `prompt`, replacing one already up.
    pub fn prompt(&mut self, prompt: Box<dyn Prompt>) {
        *self.prompt = Some(prompt);
    }

    /// Takes the prompt down unanswered, if one is up, and redraws.
    pub fn close_prompt(&mut self) {
        if self.prompt.take().is_some() {
            self.redraw();
        }
    }

    /// Sends `bytes` to the server as typed input, outside [`Hook::on_input`]
    /// (a result that arrives later).
    pub fn send(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.conn.send(Frame::Input(bytes));
        }
    }

    /// Whether a prompt is up.
    pub fn prompting(&self) -> bool {
        self.prompt.is_some()
    }

    /// Moves the attach to `to` once the hooks return: the current attach
    /// channel closes (its session keeps running) and `to` attaches.
    pub fn switch(&mut self, to: Switch) {
        *self.switch = Some(to);
    }
}

// ------------------------------------------------------------------ events

/// Something a connection's thread reports to the main loop.
pub struct Event {
    generation: u64,
    kind: Kind,
}

enum Kind {
    Output(Vec<u8>),
    Exit(String),
    Suspend,
    /// The attach channel ended, with why.
    Lost(String),
    /// A control channel event line.
    Server(serde_json::Value),
    /// A reconnect attempt finished.
    Connected(Result<Box<Conn>, Failure>),
}

/// Why a (re)connect failed.
enum Failure {
    /// The link could not be made: try again later.
    Retry(String),
    /// The server refused (the session is gone): stop.
    Fatal(String),
}

fn post(out: &SyncSender<Out>, generation: u64, kind: Kind) -> bool {
    out.send(Out::Remote(Event { generation, kind })).is_ok()
}

// ------------------------------------------------------------------ attach

/// This client's tty name on the remote: unique, so requests can name the
/// client (`-t`) and find its session, and never a local device path the
/// remote could mistake for one of its own terminals.
pub fn client_tty() -> &'static str {
    static TTY: OnceLock<String> = OnceLock::new();
    TTY.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("/dev/grofe-remote/{}-{nanos}", std::process::id())
    })
}

/// A remote attach, as the CLI sets it up.
pub struct RemoteAttach {
    /// The host, for the bar.
    pub host: String,
    /// The control hello for requests to the remote server.
    pub control_hello: String,
    /// The attach hello's context and the first attach's options.
    pub ctx: ClientCtx,
    pub opts: AttachOptions,
    pub term: String,
    /// `remote-reconnect`: a drop reconnects (else it exits).
    pub reconnect: bool,
    /// Makes a new link for a reconnect; it must never prompt.
    pub relink: Arc<dyn Fn() -> Result<Link, String> + Send + Sync>,
    pub hooks: Vec<Box<dyn Hook>>,
}

/// What every (re)attach sends.
#[derive(Clone)]
struct Params {
    ctx: ClientCtx,
    opts: AttachOptions,
    term: String,
    caps: Caps,
    hello: String,
    events: Vec<String>,
}

/// One connection: its link and attach channel.
struct Conn {
    link: Link,
    attach: crate::remote::proxy_proto::Channel,
    /// The attached session's id, for a reconnect.
    session: Option<String>,
    prefix: Key,
}

impl Conn {
    fn send(&self, frame: Frame) {
        let mut buf = BytesMut::new();
        if FrameCodec.encode(frame, &mut buf).is_ok() {
            // A failed write is a drop the reader thread reports.
            let _ = self.attach.clone().write_all(&buf);
        }
    }
}

/// Runs a remote attach on `link` until it ends: `Ok` with the server's
/// exit reason (or `detached` when detached while offline), `Err` with a
/// refusal or failure. The tty is restored when this returns.
pub fn attach(link: Link, a: RemoteAttach) -> Result<String, String> {
    let (out_tx, out_rx) = client::enter_raw()?;
    let result = run(link, a, out_tx, out_rx);
    client::restore_tty();
    result
}

fn run(
    link: Link,
    mut a: RemoteAttach,
    out_tx: SyncSender<Out>,
    out_rx: Receiver<Out>,
) -> Result<String, String> {
    let probed = client::probe_terminal(&a.opts.features)?;
    let params = Params {
        ctx: a.ctx.clone(),
        opts: a.opts.clone(),
        term: a.term.clone(),
        caps: probed.caps,
        hello: a.control_hello.clone(),
        events: a.hooks.iter().flat_map(|h| h.events()).collect(),
    };
    let conn = match handshake(link, &params, 1, &out_tx) {
        Ok(c) => c,
        Err(Failure::Retry(e) | Failure::Fatal(e)) => return Err(e),
    };
    client::start_screen(&params.caps)?;
    let filter = probed.filter;
    let stdin_tx = out_tx.clone();
    std::thread::Builder::new()
        .name("stdin".to_string())
        .spawn(move || client::stdin_loop(stdin_tx, filter))
        .map_err(|e| format!("starting the input thread: {e}"))?;

    let prefix = conn.prefix;
    let mut lp = Loop {
        state: State::Online(Box::new(conn)),
        overlay: None,
        switch: None,
        backoff: Backoff::new(),
        jitter: Jitter::new(),
        detach: DetachKey::new(prefix),
        generation: 1,
        bar: String::new(),
        params,
        out_tx,
    };
    lp.backoff.connected(Instant::now());
    if !probed.typeahead.is_empty() {
        lp.input(&mut a, probed.typeahead);
    }
    lp.run(&mut a, &out_rx)
}

enum State {
    Online(Box<Conn>),
    Offline {
        /// When the next attempt starts (none while one is in flight).
        next: Option<Instant>,
    },
}

struct Loop {
    state: State,
    overlay: Option<Box<dyn Prompt>>,
    /// A switch a hook asked for, applied after it returns.
    switch: Option<Switch>,
    backoff: Backoff,
    jitter: Jitter,
    detach: DetachKey,
    /// The current connection's generation; events of others are stale.
    generation: u64,
    /// The bar as last drawn.
    bar: String,
    params: Params,
    out_tx: SyncSender<Out>,
}

impl Loop {
    fn run(&mut self, a: &mut RemoteAttach, rx: &Receiver<Out>) -> Result<String, String> {
        let mut clocks = Clocks::now();
        let mut last_tick = Instant::now();
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Out::Input(bytes)) => self.input(a, bytes),
                Ok(Out::Detach) => match &self.state {
                    State::Online(c) => c.send(Frame::Detach),
                    State::Offline { .. } => return Ok("detached".to_string()),
                },
                Ok(Out::Wake) => {
                    if let State::Online(c) = &self.state
                        && RESIZE_PENDING.swap(false, Ordering::SeqCst)
                    {
                        let (cols, rows) = client::tty_size();
                        c.send(Frame::Resize(cols, rows));
                    }
                }
                Ok(Out::Remote(ev)) => {
                    if let Some(end) = self.event(a, ev) {
                        return end;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("lost server".to_string()),
            }
            if self.detach.fired {
                return Ok("detached".to_string());
            }
            if let Some(to) = self.switch.take() {
                self.switch_to(a, to);
            }
            let now = Clocks::now();
            if let State::Offline { next } = &mut self.state {
                if woke(clocks, now) && next.is_some() {
                    *next = Some(now.mono);
                }
                if next.is_some_and(|t| now.mono >= t) {
                    *next = None;
                    self.attempt(a);
                }
                self.draw_bar(&a.host);
            }
            clocks = now;
            if now.mono.duration_since(last_tick) >= TICK {
                last_tick = now.mono;
                self.with_hooks(a, |hooks, cx| {
                    for h in hooks {
                        h.tick(cx, now.mono);
                    }
                });
            }
        }
    }

    /// Runs `f` on the hooks with a context, when online.
    fn with_hooks<T: Default>(
        &mut self,
        a: &mut RemoteAttach,
        f: impl FnOnce(&mut [Box<dyn Hook>], &mut HookCtx<'_>) -> T,
    ) -> T {
        let State::Online(c) = &self.state else {
            return T::default();
        };
        let link = c.link.clone();
        let had_prompt = self.overlay.is_some();
        let mut cx = HookCtx {
            link: &link,
            conn: c,
            host: &a.host,
            hello: &self.params.hello,
            tty: client_tty(),
            prompt: &mut self.overlay,
            draw: Vec::new(),
            generation: self.generation,
            switch: &mut self.switch,
        };
        let out = f(&mut a.hooks, &mut cx);
        let drew = !cx.draw.is_empty();
        if drew {
            let _ = client::write_tty(&cx.draw);
        }
        // A prompt raised by a tick or an event shows at once.
        if drew || !had_prompt {
            self.draw_prompt();
        }
        out
    }

    fn input(&mut self, a: &mut RemoteAttach, bytes: Vec<u8>) {
        if !matches!(self.state, State::Online(_)) {
            self.detach.feed(&bytes);
            return;
        }
        let (out, closed) = match self.overlay.take() {
            Some(mut prompt) => match self.with_hooks(a, |_, cx| p_key(&mut *prompt, cx, &bytes)) {
                Some(input) => (input, true),
                None => {
                    // Kept up, unless answering raised another.
                    self.overlay.get_or_insert(prompt);
                    (Input::default(), false)
                }
            },
            None => {
                let out = self.with_hooks(a, |hooks, cx| {
                    let mut input = Input::send(bytes);
                    let mut draw = Vec::new();
                    for h in hooks {
                        input = h.on_input(cx, input.send);
                        draw.append(&mut input.draw);
                    }
                    input.draw = draw;
                    input
                });
                (out, false)
            }
        };
        if let State::Online(c) = &self.state {
            if !out.send.is_empty() {
                c.send(Frame::Input(out.send));
            }
            if closed {
                let _ = c.link.request(
                    &self.params.hello,
                    &["refresh-client".into(), "-t".into(), client_tty().into()],
                );
            }
        }
        if !out.draw.is_empty() {
            let _ = client::write_tty(&out.draw);
        }
        self.draw_prompt();
    }

    fn draw_prompt(&self) {
        if let Some(p) = &self.overlay {
            let (cols, rows) = client::tty_size();
            match p.overlay(cols, rows) {
                Some(bytes) => {
                    let _ = client::write_tty(&bytes);
                }
                None => draw_bottom_row(&p.text()),
            }
        }
    }

    /// Closes the attach channel and attaches over `to.link` on its own
    /// thread; until that is up the bar shows, as for a reconnect. Later
    /// reconnects go to the new host.
    fn switch_to(&mut self, a: &mut RemoteAttach, to: Switch) {
        if let State::Online(c) = &self.state {
            c.attach.close("");
        }
        self.overlay = None;
        self.generation += 1;
        let generation = self.generation;
        a.host = to.host;
        a.relink = to.relink;
        self.params.opts.target = to.target;
        self.params.opts.created = false;
        self.params.opts.detach_others = false;
        self.params.opts.extras.cwd = None;
        self.state = State::Offline { next: None };
        self.bar.clear();
        let params = self.params.clone();
        let out = self.out_tx.clone();
        std::thread::spawn(move || {
            let result = handshake(to.link, &params, generation, &out);
            post(&out, generation, Kind::Connected(result.map(Box::new)));
        });
    }

    /// One event from a connection thread; `Some` ends the client.
    fn event(&mut self, a: &mut RemoteAttach, ev: Event) -> Option<Result<String, String>> {
        if ev.generation != self.generation {
            return None;
        }
        match ev.kind {
            Kind::Output(bytes) => {
                let before = self.with_hooks(a, |hooks, cx| {
                    let mut before = Vec::new();
                    for h in hooks {
                        before.extend(h.before_output(cx, &bytes));
                    }
                    before
                });
                if !before.is_empty() {
                    let _ = client::write_tty(&before);
                }
                if let Err(e) = client::write_tty(&bytes) {
                    return Some(Err(format!("writing to the terminal: {e}")));
                }
                let extra = self.with_hooks(a, |hooks, cx| {
                    let mut extra = Vec::new();
                    for h in hooks {
                        extra.extend(h.on_output(cx, &bytes));
                    }
                    extra
                });
                if !extra.is_empty() {
                    let _ = client::write_tty(&extra);
                }
                self.draw_prompt();
            }
            Kind::Exit(reason) => return Some(Ok(reason)),
            Kind::Suspend => client::suspend(&self.out_tx),
            Kind::Lost(why) => {
                let State::Online(c) = &self.state else {
                    return None;
                };
                // The link itself ended: a drop. A link still up means the
                // server closed the attach (it exited): as a local client.
                let dropped = c.link.conn().is_closed() || c.link.dropped().is_some();
                if !a.reconnect || !dropped {
                    return Some(Err(if why.is_empty() {
                        "lost server".to_string()
                    } else {
                        format!("lost server: {why}")
                    }));
                }
                let now = Instant::now();
                self.backoff.dropped(now);
                self.overlay = None;
                self.detach = DetachKey::new(c.prefix);
                let session = c.session.clone();
                self.params.opts.target = session.or(self.params.opts.target.take());
                self.state = State::Offline {
                    next: Some(now + self.backoff.next_delay(self.jitter.sample())),
                };
                self.bar.clear();
                self.draw_bar(&a.host);
            }
            Kind::Server(value) => self.with_hooks(a, |hooks, cx| {
                for h in hooks {
                    h.on_event(cx, &value);
                }
            }),
            Kind::Connected(result) => match result {
                Ok(conn) => {
                    self.backoff.connected(Instant::now());
                    self.detach = DetachKey::new(conn.prefix);
                    self.state = State::Online(conn);
                    self.bar.clear();
                    let _ = self.out_tx.try_send(Out::Wake);
                }
                Err(Failure::Fatal(e)) => return Some(Err(e)),
                Err(Failure::Retry(_)) => {
                    let delay = self.backoff.next_delay(self.jitter.sample());
                    self.state = State::Offline {
                        next: Some(Instant::now() + delay),
                    };
                }
            },
        }
        None
    }

    /// Starts a reconnect attempt on its own thread.
    fn attempt(&mut self, a: &RemoteAttach) {
        self.generation += 1;
        let generation = self.generation;
        let relink = Arc::clone(&a.relink);
        let mut params = self.params.clone();
        params.opts.created = false;
        params.opts.detach_others = false;
        params.opts.extras.cwd = None;
        let out = self.out_tx.clone();
        std::thread::spawn(move || {
            let result = match relink() {
                Ok(link) => handshake(link, &params, generation, &out),
                Err(e) => Err(Failure::Retry(e)),
            };
            post(&out, generation, Kind::Connected(result.map(Box::new)));
        });
    }

    /// Draws the reconnect bar when its text changed.
    fn draw_bar(&mut self, host: &str) {
        let State::Offline { next } = &self.state else {
            return;
        };
        let secs = next.map_or(0, |t| {
            t.saturating_duration_since(Instant::now())
                .as_secs_f64()
                .ceil() as u64
        });
        let text = bar_text(host, self.backoff.attempt(), secs);
        if text != self.bar {
            draw_bottom_row(&text);
            self.bar = text;
        }
    }
}

/// [`Prompt::key`] through a plain function, so the closure borrows only the prompt.
fn p_key(prompt: &mut dyn Prompt, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Option<Input> {
    prompt.key(cx, bytes)
}

fn draw_bottom_row(text: &str) {
    let (cols, rows) = client::tty_size();
    let _ = client::write_tty(&bottom_row(text, cols, rows));
}

// ------------------------------------------------------------------ connect

/// Attaches over `link`: the attach hello and its reply, the session id
/// and prefix, the reader thread and (when hooks want events) the event
/// thread.
fn handshake(
    link: Link,
    p: &Params,
    generation: u64,
    out: &SyncSender<Out>,
) -> Result<Conn, Failure> {
    let retry = |e: std::io::Error| Failure::Retry(e.to_string());
    let mut chan = link.open_attach().map_err(retry)?;
    let (cols, rows) = client::tty_size();
    let hello = encode_hello(&Hello::Attach {
        ctx: p.ctx.clone(),
        cols,
        rows,
        term: p.term.clone(),
        caps: p.caps,
        target: p.opts.target.clone(),
        detach_others: p.opts.detach_others,
        nest: p.opts.nest,
        created: p.opts.created,
        extras: p.opts.extras.clone(),
        tty: Some(client_tty().to_string()),
    })
    .map_err(|e| Failure::Fatal(e.to_string()))?;
    chan.write_all(format!("{hello}\n").as_bytes())
        .map_err(retry)?;
    match read_line_reply(&mut chan) {
        Ok(Reply::Ok {
            id: HELLO_REPLY_ID, ..
        }) => {}
        Ok(Reply::Err { err, .. }) => return Err(Failure::Fatal(err)),
        Ok(other) => {
            return Err(Failure::Fatal(format!(
                "unexpected reply to attach: {other:?}"
            )));
        }
        Err(e) if link.conn().is_closed() => return Err(Failure::Retry(e)),
        Err(e) => return Err(Failure::Fatal(e)),
    }
    let session = link
        .request(
            &p.hello,
            &[
                "list-clients".into(),
                "-F".into(),
                "#{client_tty} #{session_id}".into(),
            ],
        )
        .ok()
        .and_then(|out| {
            out.lines()
                .filter_map(|l| l.split_once(' '))
                .find(|(tty, _)| *tty == client_tty())
                .map(|(_, id)| id.trim().to_string())
        });
    // Ruling: the global prefix, read once per connection; a session or
    // client prefix set elsewhere is not followed. Cost: a session with
    // its own prefix detaches offline with the global one.
    let prefix = link
        .request(
            &p.hello,
            &["show-options".into(), "-gv".into(), "prefix".into()],
        )
        .ok()
        .and_then(|v| parse_prefix(v.trim()).ok().flatten())
        .or_else(|| parse_prefix(DEFAULT_PREFIX).ok().flatten())
        .unwrap_or(Key::plain(KeyCode::Char('b')));

    let reader = chan.clone();
    let tx = out.clone();
    std::thread::spawn(move || read_frames(reader, generation, tx));
    if !p.events.is_empty()
        && let Ok(ctl) = link.open_control()
    {
        let tx = out.clone();
        let (hello, events) = (p.hello.clone(), p.events.clone());
        std::thread::spawn(move || read_events(ctl, &hello, &events, generation, tx));
    }
    Ok(Conn {
        link,
        attach: chan,
        session,
        prefix,
    })
}

/// The attach reply line, read a byte at a time so no frame bytes after
/// it are consumed.
fn read_line_reply(chan: &mut impl Read) -> Result<Reply, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match chan.read(&mut byte) {
            Ok(0) => return Err("server closed the connection during attach".to_string()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() >= crate::protocol::MAX_MESSAGE_LEN => {
                return Err("attach reply too long".to_string());
            }
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("waiting for the server: {e}")),
        }
    }
    serde_json::from_slice(&line).map_err(|e| format!("bad reply from server: {e}"))
}

/// Attach frames to the main loop until the channel ends.
fn read_frames(mut chan: impl Read, generation: u64, out: SyncSender<Out>) {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        loop {
            match FrameCodec.decode(&mut buf) {
                Ok(Some(Frame::Output(bytes))) => {
                    if !post(&out, generation, Kind::Output(bytes.to_vec())) {
                        return;
                    }
                }
                Ok(Some(Frame::Exit(reason))) => {
                    post(&out, generation, Kind::Exit(reason));
                    return;
                }
                Ok(Some(Frame::Suspend)) => {
                    post(&out, generation, Kind::Suspend);
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    post(&out, generation, Kind::Lost(format!("bad frame: {e}")));
                    return;
                }
            }
        }
        match chan.read(&mut chunk) {
            Ok(0) => {
                post(&out, generation, Kind::Lost(String::new()));
                return;
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                post(&out, generation, Kind::Lost(e.to_string()));
                return;
            }
        }
    }
}

/// Subscribes on `chan` and posts every event line.
fn read_events(
    chan: crate::remote::proxy_proto::Channel,
    hello: &str,
    events: &[String],
    generation: u64,
    out: SyncSender<Out>,
) {
    pump_events(chan, hello, events, |value| {
        post(&out, generation, Kind::Server(value))
    });
}

/// Subscribes to `events` on `chan` and hands every event line to
/// `deliver` until it returns false or the channel ends. One reader serves
/// the reply and every line after it, so an event that came in the same
/// read as the reply (the subscribe-time snapshots) is kept.
fn pump_events<S: Read + Write>(
    chan: S,
    hello: &str,
    events: &[String],
    mut deliver: impl FnMut(serde_json::Value) -> bool,
) {
    use std::io::BufRead;
    let mut argv = vec!["subscribe".to_string()];
    argv.extend(events.iter().cloned());
    let Ok(req) = serde_json::to_string(&crate::protocol::Request {
        id: 1,
        argv,
        ..Default::default()
    }) else {
        return;
    };
    let mut reader = std::io::BufReader::new(chan);
    if reader
        .get_mut()
        .write_all(format!("{hello}\n{req}\n").as_bytes())
        .is_err()
    {
        return;
    }
    let mut subscribed = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("event").is_some() {
            if !deliver(value) {
                return;
            }
        } else if !subscribed {
            // The subscribe's reply: refused means no events.
            if value.get("ok") != Some(&serde_json::Value::Bool(true)) {
                return;
            }
            subscribed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subscribe reply and the first event in one read: the event is
    /// still delivered.
    #[test]
    fn an_event_sent_with_the_subscribe_reply_is_kept() {
        use std::io::BufRead;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(b.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap(); // hello
            r.read_line(&mut line).unwrap(); // subscribe
            (&b).write_all(
                b"{\"id\":1,\"ok\":true,\"out\":\"\"}\n{\"event\":\"pane-echo\",\"pane\":\"%0\",\"echo\":false}\n",
            )
            .unwrap();
            b.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let mut got = Vec::new();
        pump_events(a, "{}", &["pane-echo".to_string()], |v| {
            got.push(v);
            true
        });
        server.join().unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0]["event"], "pane-echo");
    }

    #[test]
    fn the_client_tty_is_unique_and_not_a_real_device() {
        let tty = client_tty();
        assert!(tty.starts_with("/dev/grofe-remote/"));
        assert_eq!(tty, client_tty());
    }
}
