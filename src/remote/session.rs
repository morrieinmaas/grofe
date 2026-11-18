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

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio_util::bytes::BytesMut;
use tokio_util::codec::Encoder as _;

use super::connect::Link;
use super::reconnect::{Backoff, Clocks, DetachKey, Jitter, bar_text, bottom_row, woke};
use crate::client::{self, AttachOptions, Out, RESIZE_PENDING};
use crate::protocol::{Caps, ClientCtx, Frame, FrameCodec};
use crate::server::input::Key;

/// How often [`Hook::tick`] runs (and the bar's countdown is checked).
pub const TICK: Duration = Duration::from_millis(100);

/// Most bytes typed during a switch that are kept for the new attach.
const MAX_HELD: usize = 64 * 1024;

/// The prefix assumed when the server's cannot be read.
const DEFAULT_PREFIX: &str = "C-b";

mod handshake;
mod hooks;

pub(crate) use handshake::attached_session;
use handshake::handshake;
pub(crate) use handshake::pump_events;
pub use hooks::{Hook, HookCtx, Input, Prompt, Switch};

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
    /// The session this connection's client is attached to now (it
    /// followed a `switch-client`).
    Session(String),
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

/// This client's tty name on the remote for connection `generation`:
/// unique per process and per connection, so requests can name the client
/// (`-t`) and find its session even while a dropped connection's stale
/// client still lingers on the server, and never a local device path the
/// remote could mistake for one of its own terminals.
pub fn tty_for(generation: u64) -> String {
    static BASE: OnceLock<String> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("/dev/grofe-remote/{}-{nanos}", std::process::id())
    });
    format!("{base}-{generation}")
}

/// The tty of the connection attaching or attached now ([`tty_for`]),
/// for code outside a hook call; inside one, [`HookCtx::client`].
pub fn client_tty() -> String {
    lock_tty().clone()
}

fn lock_tty() -> std::sync::MutexGuard<'static, String> {
    static CURRENT: OnceLock<std::sync::Mutex<String>> = OnceLock::new();
    CURRENT
        .get_or_init(|| std::sync::Mutex::new(tty_for(1)))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Makes `tty` the current one ([`client_tty`]).
fn set_client_tty(tty: &str) {
    *lock_tty() = tty.to_string();
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

/// The event a client's session switch raises; the loop always follows it.
const SESSION_CHANGED: &str = "client-session-changed";

/// The events the hooks name, plus [`SESSION_CHANGED`], each once.
fn hook_events(hooks: &[Box<dyn Hook>]) -> Vec<String> {
    let mut events: Vec<String> = Vec::new();
    for e in hooks
        .iter()
        .flat_map(|h| h.events())
        .chain([SESSION_CHANGED.to_string()])
    {
        if !events.contains(&e) {
            events.push(e);
        }
    }
    events
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
    /// A reattach whose target (the remembered session) is refused
    /// attaches to the server's choice instead of ending the client.
    fallback: bool,
}

/// One connection: its link and attach channel.
struct Conn {
    link: Link,
    /// This connection's tty on the remote ([`tty_for`]).
    tty: String,
    attach: crate::remote::proxy_proto::Channel,
    /// The attached session's id, for a reconnect.
    session: Option<String>,
    prefix: Key,
    /// The control channel subscribed to the hooks' events, if any: closed
    /// with the attach, so a switch or reconnect leaves no subscription.
    events: Option<crate::remote::proxy_proto::Channel>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(e) = &self.events {
            e.close("");
        }
    }
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
        events: hook_events(&a.hooks),
        fallback: false,
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
        drawn: Vec::new(),
        held: None,
        backoff: Backoff::new(),
        jitter: Jitter::new(),
        detach: DetachKey::new(prefix),
        generation: 1,
        bar: String::new(),
        attempt: 1,
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
    /// The overlay's bytes as last drawn.
    drawn: Vec<u8>,
    /// Keys typed while a switch attaches, to send once it is up.
    held: Option<Vec<u8>>,
    backoff: Backoff,
    jitter: Jitter,
    detach: DetachKey,
    /// The current connection's generation; events of others are stale.
    generation: u64,
    /// The bar as last drawn.
    bar: String,
    /// The attempt the bar names: the one the current wait leads to.
    attempt: u32,
    params: Params,
    out_tx: SyncSender<Out>,
}

impl Loop {
    fn run(&mut self, a: &mut RemoteAttach, rx: &Receiver<Out>) -> Result<String, String> {
        let mut clocks = Clocks::now();
        let mut last_tick = Instant::now();
        loop {
            if let Some(end) = self.receive(a, rx.recv_timeout(TICK)) {
                return end;
            }
            if self.detach.fired {
                return Ok("detached".to_string());
            }
            if let Some(to) = self.switch.take() {
                self.switch_to(a, to);
            }
            let now = Clocks::now();
            self.offline_step(a, clocks, now);
            clocks = now;
            if now.mono.duration_since(last_tick) >= TICK {
                last_tick = now.mono;
                self.tick_hooks(a, now.mono);
            }
        }
    }

    /// One message from the queue (or the lack of one); `Some` ends the client.
    fn receive(
        &mut self,
        a: &mut RemoteAttach,
        msg: Result<Out, RecvTimeoutError>,
    ) -> Option<Result<String, String>> {
        match msg {
            Ok(Out::Input(bytes)) => self.input(a, bytes),
            Ok(Out::Detach) => match &self.state {
                State::Online(c) => c.send(Frame::Detach),
                State::Offline { .. } => return Some(Ok("detached".to_string())),
            },
            Ok(Out::Wake) => {
                if let State::Online(c) = &self.state
                    && RESIZE_PENDING.swap(false, Ordering::SeqCst)
                {
                    let (cols, rows) = client::tty_size();
                    c.send(Frame::Resize(cols, rows));
                }
            }
            Ok(Out::Remote(ev)) => return self.event(a, ev),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Some(Err("lost server".to_string())),
        }
        None
    }

    /// While offline: starts the next attempt when it is due (at once after
    /// a sleep, `clocks` being the previous look) and keeps the bar drawn.
    fn offline_step(&mut self, a: &RemoteAttach, clocks: Clocks, now: Clocks) {
        if let State::Offline { next } = &mut self.state {
            if attempt_due(next, clocks, now) {
                *next = None;
                self.attempt(a);
            }
            self.draw_bar(&a.host);
        }
    }

    /// [`Hook::tick`] for every hook, then the overlay if it changed.
    fn tick_hooks(&mut self, a: &mut RemoteAttach, now: Instant) {
        self.with_hooks(a, |hooks, cx| {
            for h in hooks {
                h.tick(cx, now);
            }
        });
        if matches!(self.state, State::Online(_)) {
            self.redraw_changed_overlay();
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
            tty: &c.tty,
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
            // Typed during a switch: sent once the new attach is up.
            if let Some(held) = &mut self.held
                && held.len() + bytes.len() <= MAX_HELD
            {
                held.extend_from_slice(&bytes);
            }
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
                // In the background: a slow link must not stall the keys.
                let (link, hello, tty) = (c.link.clone(), self.params.hello.clone(), c.tty.clone());
                std::thread::spawn(move || {
                    let argv = ["refresh-client", "-t", tty.as_str()].map(String::from);
                    let _ = link.request(&hello, &argv);
                });
            }
        }
        if !out.draw.is_empty() {
            let _ = client::write_tty(&out.draw);
        }
        self.draw_prompt();
    }

    fn draw_prompt(&mut self) {
        if let Some(p) = &self.overlay {
            let (cols, rows) = client::tty_size();
            match p.overlay(cols, rows) {
                Some(bytes) => {
                    let _ = client::write_tty(&bytes);
                    self.drawn = bytes;
                }
                None => draw_bottom_row(&p.text()),
            }
        }
    }

    /// After a tick: an overlay whose content changed (the picker's
    /// machines did) is drawn again, with no key or output to prompt it.
    fn redraw_changed_overlay(&mut self) {
        let Some(p) = &self.overlay else {
            return;
        };
        let (cols, rows) = client::tty_size();
        if let Some(bytes) = p.overlay(cols, rows)
            && bytes != self.drawn
        {
            let _ = client::write_tty(&bytes);
            self.drawn = bytes;
        }
    }

    /// Closes the attach channel and attaches over `to.link` on its own
    /// thread; until that is up the bar shows, as for a reconnect. Later
    /// reconnects go to the new host.
    fn switch_to(&mut self, a: &mut RemoteAttach, to: Switch) {
        if let State::Online(c) = &self.state {
            c.attach.close("");
        }
        // The old connection's event channel closes with it (Conn's drop).
        self.overlay = None;
        self.generation += 1;
        let generation = self.generation;
        a.host = to.host;
        a.relink = to.relink;
        a.control_hello = to.hello.clone();
        self.params.ctx = to.ctx;
        self.params.hello = to.hello;
        // The hooks may want other events there (a new forwarding hook).
        self.params.events = hook_events(&a.hooks);
        self.params.opts.target = to.target;
        self.params.opts.created = false;
        self.params.opts.detach_others = false;
        self.params.opts.extras.cwd = None;
        self.attempt = self.backoff.attempt();
        self.state = State::Offline { next: None };
        self.held = Some(Vec::new());
        self.bar.clear();
        let params = self.params.clone();
        let out = self.out_tx.clone();
        std::thread::spawn(move || {
            let result = match (to.connect)() {
                Ok(link) => handshake(link, &params, generation, &out),
                Err(e) => Err(Failure::Retry(e)),
            };
            post(&out, generation, Kind::Connected(result.map(Box::new)));
        });
    }

    /// One event from a connection thread; `Some` ends the client.
    fn event(&mut self, a: &mut RemoteAttach, ev: Event) -> Option<Result<String, String>> {
        if ev.generation != self.generation {
            return None;
        }
        match ev.kind {
            Kind::Output(bytes) => return self.output(a, &bytes),
            Kind::Exit(reason) => return Some(Ok(reason)),
            Kind::Suspend => client::suspend(&self.out_tx),
            Kind::Lost(why) => return self.lost(a, why),
            Kind::Server(value) => {
                if value.get("event").and_then(|e| e.as_str()) == Some(SESSION_CHANGED) {
                    self.follow_session();
                }
                self.with_hooks(a, |hooks, cx| {
                    for h in hooks {
                        h.on_event(cx, &value);
                    }
                });
            }
            Kind::Connected(result) => return self.connected(a, result),
            Kind::Session(id) => {
                if let State::Online(c) = &mut self.state {
                    c.session = Some(id);
                }
            }
        }
        None
    }

    /// Server output: the hooks' bytes before it, the output, theirs after
    /// it, and the prompt over it all.
    fn output(&mut self, a: &mut RemoteAttach, bytes: &[u8]) -> Option<Result<String, String>> {
        let before = self.with_hooks(a, |hooks, cx| {
            let mut before = Vec::new();
            for h in hooks {
                before.extend(h.before_output(cx, bytes));
            }
            before
        });
        if !before.is_empty() {
            let _ = client::write_tty(&before);
        }
        if let Err(e) = client::write_tty(bytes) {
            return Some(Err(format!("writing to the terminal: {e}")));
        }
        let extra = self.with_hooks(a, |hooks, cx| {
            let mut extra = Vec::new();
            for h in hooks {
                extra.extend(h.on_output(cx, bytes));
            }
            extra
        });
        if !extra.is_empty() {
            let _ = client::write_tty(&extra);
        }
        self.draw_prompt();
        None
    }

    /// The attach channel ended: a dropped link goes offline to reconnect,
    /// anything else (or `remote-reconnect off`) ends the client.
    fn lost(&mut self, a: &mut RemoteAttach, why: String) -> Option<Result<String, String>> {
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
                format!("lost server: {}", crate::remote::ssh::sanitize(&why))
            }));
        }
        let now = Instant::now();
        self.backoff.dropped(now);
        self.overlay = None;
        self.detach = DetachKey::new(c.prefix);
        let session = c.session.clone();
        self.params.opts.target = session.or(self.params.opts.target.take());
        // Counted before the delay is: the first retry is attempt 1.
        self.attempt = self.backoff.attempt();
        self.state = State::Offline {
            next: Some(now + self.backoff.next_delay(self.jitter.sample())),
        };
        self.bar.clear();
        self.draw_bar(&a.host);
        None
    }

    /// A reconnect or switch attempt finished.
    fn connected(
        &mut self,
        a: &mut RemoteAttach,
        result: Result<Box<Conn>, Failure>,
    ) -> Option<Result<String, String>> {
        match result {
            Ok(conn) => {
                self.backoff.connected(Instant::now());
                self.detach = DetachKey::new(conn.prefix);
                self.state = State::Online(conn);
                self.bar.clear();
                // Typed during a switch: as if typed now, so the hooks see
                // it too (the machine key may be in it).
                if let Some(held) = self.held.take().filter(|h| !h.is_empty()) {
                    self.input(a, held);
                }
                let _ = self.out_tx.try_send(Out::Wake);
            }
            Err(Failure::Fatal(e)) => return Some(Err(e)),
            Err(Failure::Retry(_)) => {
                // A switch that failed is a reconnect: typing is dropped.
                self.held = None;
                self.attempt = self.backoff.attempt();
                let delay = self.backoff.next_delay(self.jitter.sample());
                self.state = State::Offline {
                    next: Some(Instant::now() + delay),
                };
            }
        }
        None
    }

    /// Re-reads, off the main thread, which session this connection's
    /// client is on, so a reconnect after a `switch-client` goes back to it.
    fn follow_session(&self) {
        let State::Online(c) = &self.state else {
            return;
        };
        let (link, hello, tty) = (c.link.clone(), self.params.hello.clone(), c.tty.clone());
        let (out, generation) = (self.out_tx.clone(), self.generation);
        std::thread::spawn(move || {
            if let Some(id) = handshake::attached_session(&link, &hello, &tty) {
                post(&out, generation, Kind::Session(id));
            }
        });
    }

    /// Starts a reconnect attempt on its own thread.
    fn attempt(&mut self, a: &RemoteAttach) {
        self.generation += 1;
        let generation = self.generation;
        let relink = Arc::clone(&a.relink);
        let mut params = self.params.clone();
        params.opts.created = false;
        // The remembered session may be gone: then the server picks.
        params.fallback = true;
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
        let text = bar_text(host, self.attempt, secs);
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

/// Whether the next attempt (`next`, none while one is in flight) is due at
/// `now`, `before` being the previous look at the clocks: a wake from sleep
/// (the wall clock jumped past the monotonic one) brings it forward to now.
fn attempt_due(next: &mut Option<Instant>, before: Clocks, now: Clocks) -> bool {
    if woke(before, now) && next.is_some() {
        *next = Some(now.mono);
    }
    next.is_some_and(|t| now.mono >= t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec: a resumed wake from sleep (a wall clock jump over 5 s) retries
    /// at once, however long the backoff had left.
    #[test]
    fn a_wake_from_sleep_makes_the_next_attempt_due_at_once() {
        let mono = Instant::now();
        let wall = std::time::SystemTime::now();
        let before = Clocks { mono, wall };
        let later = |m: u64, w: u64| Clocks {
            mono: mono + Duration::from_secs(m),
            wall: wall + Duration::from_secs(w),
        };
        let far = Some(mono + Duration::from_secs(30));
        // No jump: the 30 s wait stands.
        let mut next = far;
        assert!(!attempt_due(&mut next, before, later(1, 1)));
        assert_eq!(next, far);
        // A 10 s jump: due now.
        let mut next = far;
        assert!(attempt_due(&mut next, before, later(1, 11)));
        // While an attempt is in flight a wake starts no second one.
        let mut none = None;
        assert!(!attempt_due(&mut none, before, later(1, 11)));
        // The wait running out is due too.
        let mut next = far;
        assert!(attempt_due(&mut next, before, later(30, 30)));
    }

    #[test]
    fn the_client_tty_is_unique_per_connection_and_not_a_real_device() {
        let (one, two) = (tty_for(1), tty_for(2));
        assert!(one.starts_with("/dev/grofe-remote/"));
        assert_ne!(
            one, two,
            "a reconnect must not reuse the stale client's tty"
        );
        assert_eq!(one, tty_for(1));
        set_client_tty(&two);
        assert_eq!(client_tty(), two);
    }
}
