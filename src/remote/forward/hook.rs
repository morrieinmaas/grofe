//! [`ForwardHook`]: forwarding plugged into the remote attach loop
//! ([`session::Hook`](crate::remote::session::Hook)). Keys, events and ticks
//! go into [`Forwarding`]; prompts go through the client's prompt; slow
//! actions (the upload directory, opening a URL, messages) and every control
//! round trip run on a worker thread, each upload on a thread of its own,
//! with results that come back on the next tick; forwards run on the input thread (a bind is quick)
//! over whatever link is current.
//!
//! The client subscribes with `-c` (its own name), so the server sends only
//! events of its attached session, following a switch; the worker reads
//! that session's ports again on a connect and on a switch. The options that grant consent (uploads, ports) come
//! from this machine's config, never from the remote.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::exec::{LinkCell, is_slow, opener, run_slow, upload};
use super::ports::Forwarder;
use super::{Action, Forwarding, Input, Settings};
use crate::event::Event;
use crate::remote::connect::Link;
use crate::remote::proxy_proto::ProxyConn;
use crate::remote::session::{self, Hook, HookCtx, Prompt};
use crate::server::input::{Decoder, Input as Decoded, Key, KeyCode, parse_prefix};
use crate::types::PaneId;

/// Events forwarding subscribes to.
pub const EVENTS: [&str; 4] = [
    "open-url",
    "ports-changed",
    "pane-exited",
    "client-session-changed",
];

/// The longest answer a prompt takes.
const MAX_ANSWER: usize = 16;

/// Forwarding for one remote client.
pub struct ForwardHook {
    core: Arc<Mutex<Core>>,
}

struct Core {
    fw: Forwarding,
    forwarder: Forwarder,
    link: LinkCell,
    jobs: Sender<Job>,
    done: Receiver<Input>,
    /// The proxy connection last seen (its address), to tell a reconnect.
    conn: Option<usize>,
    /// The id of the forwarding prompt up, if one is; ids count up.
    current: Option<u64>,
    next_id: u64,
    /// Set to stop the upload running ([`Action::CancelUpload`]).
    cancel: Arc<AtomicBool>,
}

/// Work for the worker thread.
enum Job {
    /// Pass the event on, in order with a resync.
    Event(Event),
    /// Read the attached session's listening ports ([`Input::Resync`]).
    Resync,
    Slow(Action),
    /// An upload, stopped once its flag is set.
    Upload(Action, Arc<AtomicBool>),
    Message(String),
}

impl ForwardHook {
    /// Forwarding for a client of `host`, whose control requests use
    /// `hello`, with `settings` from this machine's config
    /// ([`Settings::from_options`]); its forwards are listed under
    /// `state_dir` ([`list_forwards`](super::list_forwards)).
    pub fn new(
        host: &str,
        hello: &str,
        state_dir: Option<PathBuf>,
        settings: Settings,
    ) -> ForwardHook {
        let link = LinkCell::default();
        let (jobs, job_rx) = channel();
        let (done_tx, done) = channel();
        let worker_link = link.clone();
        let hello = hello.to_string();
        std::thread::spawn(move || worker(worker_link, hello, job_rx, done_tx));
        let core = Core {
            fw: Forwarding::new(host, settings),
            forwarder: Forwarder::new(host, opener(link.clone()), state_dir),
            link,
            jobs,
            done,
            conn: None,
            current: None,
            next_id: 0,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        ForwardHook {
            core: Arc::new(Mutex::new(core)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Core> {
        lock(&self.core)
    }

    /// Feeds `input` and runs what comes of it; the bytes for the server.
    fn feed(&self, cx: &mut HookCtx<'_>, input: Input) -> Vec<u8> {
        let mut core = self.lock();
        core.connected(cx.link());
        let actions = core.fw.handle(input, Instant::now());
        core.apply(&self.core, cx, actions)
    }
}

fn lock(core: &Mutex<Core>) -> MutexGuard<'_, Core> {
    core.lock().unwrap_or_else(|p| p.into_inner())
}

impl Core {
    /// Points the forwards at `link` and, on a new connection (the first,
    /// or a reconnect), reads the ports again: what changed
    /// while the link was down sent no event this client saw.
    fn connected(&mut self, link: &Link) {
        let conn = std::ptr::from_ref::<ProxyConn>(link.conn()) as usize;
        if self.conn != Some(conn) {
            self.conn = Some(conn);
            self.link.set(link.clone());
            let _ = self.jobs.send(Job::Resync);
        }
    }

    /// Puts a question on the client's prompt, unless another prompt is up
    /// (the machine picker): then it waits, and [`Hook::tick`] shows it once
    /// the screen is free.
    fn raise(
        &mut self,
        me: &Arc<Mutex<Core>>,
        cx: &mut HookCtx<'_>,
        text: String,
        grace: Option<Grace>,
    ) {
        if cx.prompting() {
            return self.fw.dropped();
        }
        self.next_id += 1;
        self.current = Some(self.next_id);
        cx.prompt(Box::new(Answer {
            id: self.next_id,
            text,
            line: String::new(),
            core: Arc::clone(me),
            grace,
            decoder: Decoder::new(),
        }));
    }

    /// Runs `actions` (and what their answers lead to); the bytes for the
    /// server.
    fn apply(
        &mut self,
        me: &Arc<Mutex<Core>>,
        cx: &mut HookCtx<'_>,
        actions: Vec<Action>,
    ) -> Vec<u8> {
        let mut queue: VecDeque<Action> = actions.into();
        let mut send = Vec::new();
        while let Some(action) = queue.pop_front() {
            match action {
                Action::Send(bytes) => send.extend(bytes),
                Action::Prompt(text) => self.raise(me, cx, text, None),
                Action::Offer(text) => self.raise(me, cx, text, Some(Grace::new(Instant::now()))),
                Action::ClearPrompt => {
                    self.current = None;
                    cx.close_prompt_tagged(TAG);
                }
                Action::Message(text) => {
                    let _ = self.jobs.send(Job::Message(text));
                }
                Action::StartForward(remote) => {
                    let local = self.forwarder.start(remote);
                    let input = Input::Forwarded { remote, local };
                    queue.extend(self.fw.handle(input, Instant::now()));
                }
                Action::StopForward(remote) => self.forwarder.stop(remote),
                Action::CancelUpload => self.cancel.store(true, Ordering::SeqCst),
                upload @ Action::Upload { .. } => {
                    self.cancel = Arc::new(AtomicBool::new(false));
                    let _ = self
                        .jobs
                        .send(Job::Upload(upload, Arc::clone(&self.cancel)));
                }
                slow => {
                    debug_assert!(is_slow(&slow));
                    let _ = self.jobs.send(Job::Slow(slow));
                }
            }
        }
        send
    }
}

impl Hook for ForwardHook {
    fn events(&self) -> Vec<String> {
        EVENTS.iter().map(|e| e.to_string()).collect()
    }

    fn on_input(&mut self, cx: &mut HookCtx<'_>, bytes: Vec<u8>) -> session::Input {
        session::Input::send(self.feed(cx, Input::Keys(bytes)))
    }

    fn on_event(&mut self, cx: &mut HookCtx<'_>, event: &serde_json::Value) {
        let Ok(event) = serde_json::from_value::<Event>(event.clone()) else {
            return;
        };
        let mut core = self.lock();
        core.connected(cx.link());
        let jobs = match event {
            // Ours: the subscription follows this client.
            Event::ClientSessionChanged { .. } => vec![Job::Resync],
            // Through the worker, so events keep their order with a resync.
            event => vec![Job::Event(event)],
        };
        for job in jobs {
            let _ = core.jobs.send(job);
        }
    }

    fn tick(&mut self, cx: &mut HookCtx<'_>, now: Instant) {
        let mut core = self.lock();
        core.connected(cx.link());
        let mut inputs = vec![Input::Tick];
        while let Ok(done) = core.done.try_recv() {
            inputs.push(done);
        }
        let mut send = Vec::new();
        for input in inputs {
            let actions = core.fw.handle(input, now);
            send.extend(core.apply(&self.core, cx, actions));
        }
        // A question whose prompt went unanswered comes back once the
        // screen is free (after a reconnect, or the machine picker).
        if !cx.prompting() {
            let actions = core.fw.reshow();
            send.extend(core.apply(&self.core, cx, actions));
        }
        cx.send(send);
    }
}

/// The tag of forwarding's prompts ([`HookCtx::close_prompt_tagged`]).
const TAG: &str = "forward";

/// A forwarding question on the client's prompt: collects an answer line.
struct Answer {
    /// Which question this is ([`Core::current`] while it is up).
    id: u64,
    text: String,
    line: String,
    core: Arc<Mutex<Core>>,
    /// For a question the remote raised: keys still go to the pane while
    /// the user is typing.
    grace: Option<Grace>,
    /// Keys in any encoding the terminal uses (legacy, or the kitty
    /// keyboard protocol the client may have turned on).
    decoder: Decoder,
}

/// How long the keyboard must rest before a remote-raised prompt takes a
/// key, counted from when it came up and from each key since.
const GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// A prompt the remote raised (a port, a file to open) can come up while
/// the user is typing in the pane. Keys keep going to the pane until the
/// keyboard has rested [`GRACE`]: so typing is never swallowed, and a
/// prompt timed to land mid-typing cannot take a `y` meant for the pane.
/// The answer needs a fresh keypress after a pause.
struct Grace {
    last: Instant,
}

impl Grace {
    fn new(now: Instant) -> Grace {
        Grace { last: now }
    }

    /// Whether a key at `now` still goes to the pane (it restarts the wait).
    fn passes(&mut self, now: Instant) -> bool {
        let typing = now.duration_since(self.last) < GRACE;
        if typing {
            self.last = now;
        }
        typing
    }
}

impl Prompt for Answer {
    fn tag(&self) -> Option<&'static str> {
        Some(TAG)
    }

    fn dropped(&mut self) {
        // Locked: this prompt is being replaced by forwarding itself, which
        // already knows.
        let Ok(mut core) = self.core.try_lock() else {
            return;
        };
        if core.current == Some(self.id) {
            core.current = None;
            core.fw.dropped();
        }
    }

    fn text(&self) -> String {
        // The upload directory and a URL come from the remote: nothing in
        // them may reach the terminal as a control.
        crate::remote::ssh::sanitize(&format!("{} {}", self.text, self.line))
            .replace(['\n', '\t'], " ")
    }

    fn key(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Option<session::Input> {
        if let Some(grace) = &mut self.grace
            && grace.passes(Instant::now())
        {
            // Typing meant for the pane, through forwarding as any key is: a
            // file dropped now still asks before it uploads.
            let mut core = lock(&self.core);
            let actions = core.fw.handle(Input::Keys(bytes.to_vec()), Instant::now());
            let send = core.apply(&self.core, cx, actions);
            cx.send(send);
            return None;
        }
        let (answer, onward) = match answer_keys(&mut self.decoder, &mut self.line, bytes)? {
            Answered::Line(line) => (line, Vec::new()),
            Answered::No => (String::new(), Vec::new()),
            Answered::Elsewhere => (String::new(), bytes.to_vec()),
        };
        let mut core = lock(&self.core);
        if core.current != Some(self.id) {
            // A question already settled (its port went): just close.
            return Some(session::Input::send(onward));
        }
        core.current = None;
        let actions = core.fw.handle(Input::Answer(answer), Instant::now());
        let mut send = core.apply(&self.core, cx, actions);
        send.extend(onward);
        Some(session::Input::send(send))
    }
}

/// What typed keys did to a prompt's answer.
#[derive(Debug, PartialEq, Eq)]
enum Answered {
    /// Enter: the answer line.
    Line(String),
    /// Escape or C-c: no.
    No,
    /// A key that is no part of an answer (the prefix, an arrow, a control
    /// key): no, and the keys go on to the pane, so the prefix (and its
    /// detach) works while a prompt is up.
    Elsewhere,
}

/// Feeds typed `bytes` to a prompt's answer `line`; `None` while the answer
/// is still being typed.
fn answer_keys(decoder: &mut Decoder, line: &mut String, bytes: &[u8]) -> Option<Answered> {
    let ctrl_c = parse_prefix("C-c").ok().flatten();
    for input in decoder.feed(bytes, Instant::now(), std::time::Duration::ZERO) {
        let Decoded::Key(key) = input else {
            continue;
        };
        match key.code {
            KeyCode::Enter => return Some(Answered::Line(std::mem::take(line))),
            KeyCode::Escape => return Some(Answered::No),
            _ if ctrl_c.is_some_and(|c| key.matches(&c)) => return Some(Answered::No),
            KeyCode::Backspace => {
                line.pop();
            }
            KeyCode::Char(c)
                if (c.is_ascii_graphic() || c == ' ')
                    && line.len() < MAX_ANSWER
                    && key.matches(&Key::plain(KeyCode::Char(c))) =>
            {
                line.push(c)
            }
            // Past the longest answer: more typing is ignored.
            KeyCode::Char(c) if key.matches(&Key::plain(KeyCode::Char(c))) => {}
            _ => return Some(Answered::Elsewhere),
        }
    }
    None
}

fn worker(link: LinkCell, hello: String, jobs: Receiver<Job>, done: Sender<Input>) {
    // The attached session, read on a connect and a switch (both send a
    // `Resync`), not per event.
    let mut session: Option<String> = None;
    for job in jobs {
        let Some(link) = link.get() else { continue };
        let reply = match job {
            // The server sends only this client's session's events
            // (`subscribe -c`); through here they keep their order with a
            // resync.
            Job::Event(event) => Input::Event(event),
            Job::Resync => {
                session = attached_session(&link, &hello);
                // The session's prefix, which goes through while an upload
                // holds typing.
                if let Some(s) = &session {
                    let argv = ["show-options", "-Av", "-t", s, "prefix"].map(String::from);
                    let prefix = link
                        .request(&hello, &argv)
                        .ok()
                        .and_then(|p| crate::server::input::parse_prefix(p.trim()).ok().flatten());
                    let _ = done.send(Input::Prefix(prefix));
                }
                match &session {
                    Some(s) => Input::Resync(session_ports(&link, &hello, s)),
                    None => continue,
                }
            }
            Job::Message(text) => {
                // `#` starts a format in a message; `##` is a plain one.
                let tty = session::client_tty();
                let argv = ["display-message", "-c", tty.as_str()]
                    .map(String::from)
                    .into_iter()
                    .chain([text.replace('#', "##")])
                    .collect::<Vec<_>>();
                let _ = link.request(&hello, &argv);
                continue;
            }
            // An upload on a thread of its own: a big one must not hold up
            // the events behind it.
            Job::Upload(Action::Upload { dir, paste }, cancel) => {
                let done = done.clone();
                std::thread::spawn(move || {
                    let _ = done.send(upload(&link, &dir, paste, &cancel));
                });
                continue;
            }
            Job::Upload(..) => continue,
            Job::Slow(action) => {
                if session.is_none() {
                    session = attached_session(&link, &hello);
                }
                match run_slow(&link, session.as_deref(), action) {
                    Some(input) => input,
                    None => continue,
                }
            }
        };
        if done.send(reply).is_err() {
            return;
        }
    }
}

/// Each pane of `session` and the ports it listens on.
fn session_ports(link: &Link, hello: &str, session: &str) -> Vec<(PaneId, Vec<u16>)> {
    let argv = ["list-panes", "-s", "-t", session, "-F", "#{pane_id}"].map(String::from);
    let Ok(panes) = link.request(hello, &argv) else {
        return Vec::new();
    };
    let mut ports: Vec<(PaneId, Vec<u16>)> = panes
        .lines()
        .filter_map(|p| p.trim().parse().ok())
        .map(|p| (p, Vec::new()))
        .collect();
    let all = ["list-ports", "-a"].map(String::from);
    for line in link.request(hello, &all).unwrap_or_default().lines() {
        let Some((pane, list)) = line.split_once(' ') else {
            continue;
        };
        let Ok(pane) = pane.parse::<PaneId>() else {
            continue;
        };
        if let Some((_, slot)) = ports.iter_mut().find(|(p, _)| *p == pane) {
            *slot = list
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
        }
    }
    ports
}

/// The id of the session this client (its current connection) is attached
/// to.
fn attached_session(link: &Link, hello: &str) -> Option<String> {
    session::attached_session(link, hello, &session::client_tty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_prompt_reads_keys_in_legacy_and_kitty_encodings() {
        let answer = |bytes: &[u8]| {
            let mut line = String::new();
            answer_keys(&mut Decoder::new(), &mut line, bytes)
        };
        let line = |s: &str| Some(Answered::Line(s.to_string()));
        for (bytes, want) in [
            (&b"y\r"[..], line("y")),
            (b"always\r", line("always")),
            (b"ab\x7f\r", line("a")),
            (b"\x1b", Some(Answered::No)),
            (b"\x03", Some(Answered::No)),
            (b"y", None),
            // The prefix (its detach), an arrow: no, and on to the pane.
            (b"\x02", Some(Answered::Elsewhere)),
            (b"\x1b[A", Some(Answered::Elsewhere)),
            // The kitty keyboard protocol: Enter, Escape, C-c, Backspace, C-b.
            (b"y\x1b[13u", line("y")),
            (b"\x1b[27u", Some(Answered::No)),
            (b"\x1b[99;5u", Some(Answered::No)),
            (b"ab\x1b[127u\x1b[13u", line("a")),
            (b"\x1b[98;5u", Some(Answered::Elsewhere)),
        ] {
            assert_eq!(answer(bytes), want, "{bytes:?}");
        }
    }

    #[test]
    fn a_remote_prompt_takes_a_key_only_after_the_keyboard_rests() {
        let t0 = Instant::now();
        let mut g = Grace::new(t0);
        let ms = |n| t0 + Duration::from_millis(n);
        // Keys while typing go on to the pane, each one restarting the wait.
        assert!(g.passes(ms(100)));
        assert!(g.passes(ms(500)));
        assert!(g.passes(ms(900)));
        // A pause of the grace, then a fresh key: the prompt takes it.
        assert!(!g.passes(ms(1400)));
        assert!(!g.passes(ms(1450)));
    }
}
