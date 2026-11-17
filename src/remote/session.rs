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

use handshake::handshake;
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
        drawn: Vec::new(),
        held: None,
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
                if matches!(self.state, State::Online(_)) {
                    self.redraw_changed_overlay();
                }
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
        self.held = Some(Vec::new());
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
            Kind::Output(bytes) => return self.output(a, &bytes),
            Kind::Exit(reason) => return Some(Ok(reason)),
            Kind::Suspend => client::suspend(&self.out_tx),
            Kind::Lost(why) => return self.lost(a, why),
            Kind::Server(value) => self.with_hooks(a, |hooks, cx| {
                for h in hooks {
                    h.on_event(cx, &value);
                }
            }),
            Kind::Connected(result) => return self.connected(result),
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
        None
    }

    /// A reconnect or switch attempt finished.
    fn connected(&mut self, result: Result<Box<Conn>, Failure>) -> Option<Result<String, String>> {
        match result {
            Ok(conn) => {
                self.backoff.connected(Instant::now());
                self.detach = DetachKey::new(conn.prefix);
                if let Some(held) = self.held.take().filter(|h| !h.is_empty()) {
                    conn.send(Frame::Input(held));
                }
                self.state = State::Online(conn);
                self.bar.clear();
                let _ = self.out_tx.try_send(Out::Wake);
            }
            Err(Failure::Fatal(e)) => return Some(Err(e)),
            Err(Failure::Retry(_)) => {
                // A switch that failed is a reconnect: typing is dropped.
                self.held = None;
                let delay = self.backoff.next_delay(self.jitter.sample());
                self.state = State::Offline {
                    next: Some(Instant::now() + delay),
                };
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_tty_is_unique_and_not_a_real_device() {
        let tty = client_tty();
        assert!(tty.starts_with("/dev/grofe-remote/"));
        assert_eq!(tty, client_tty());
    }
}
