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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::exec::{LinkCell, is_slow, opener, run_slow};
use super::ports::Forwarder;
use super::{Action, Forwarding, Input, Settings};
use crate::event::Event;
use crate::remote::connect::Link;
use crate::remote::proxy_proto::ProxyConn;
use crate::remote::session::{self, Hook, HookCtx, Prompt};
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
}

/// Work for the worker thread.
enum Job {
    /// Pass the event on, in order with a resync.
    Event(Event),
    /// Read the attached session's listening ports ([`Input::Resync`]).
    Resync,
    Slow(Action),
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
                // ponytail: another hook's prompt replacing this one leaves
                // its question unanswered, and later ones queued behind it.
                Action::Prompt(text) => cx.prompt(Box::new(Answer {
                    text,
                    line: String::new(),
                    core: Arc::clone(me),
                    grace: None,
                })),
                Action::Offer(text) => cx.prompt(Box::new(Answer {
                    text,
                    line: String::new(),
                    core: Arc::clone(me),
                    grace: Some(Grace::new(Instant::now())),
                })),
                Action::ClearPrompt => cx.close_prompt(),
                Action::Message(text) => {
                    let _ = self.jobs.send(Job::Message(text));
                }
                Action::StartForward(remote) => {
                    let local = self.forwarder.start(remote);
                    let input = Input::Forwarded { remote, local };
                    queue.extend(self.fw.handle(input, Instant::now()));
                }
                Action::StopForward(remote) => self.forwarder.stop(remote),
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
        cx.send(send);
    }
}

/// A forwarding question on the client's prompt: collects an answer line.
struct Answer {
    text: String,
    line: String,
    core: Arc<Mutex<Core>>,
    /// For a question the remote raised: keys still go to the pane while
    /// the user is typing.
    grace: Option<Grace>,
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
            cx.send(bytes.to_vec());
            return None;
        }
        let answer = match bytes {
            // A lone escape or C-c: no.
            [0x1b] | [0x03] => Some(String::new()),
            _ if bytes.starts_with(b"\x1b") => None,
            _ => bytes.iter().find_map(|&b| match b {
                b'\r' | b'\n' => Some(std::mem::take(&mut self.line)),
                0x7f | 0x08 => {
                    self.line.pop();
                    None
                }
                0x20..=0x7e if self.line.len() < MAX_ANSWER => {
                    self.line.push(b as char);
                    None
                }
                _ => None,
            }),
        }?;
        let mut core = lock(&self.core);
        let actions = core.fw.handle(Input::Answer(answer), Instant::now());
        let send = core.apply(&self.core, cx, actions);
        Some(session::Input::send(send))
    }
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
                match &session {
                    Some(s) => Input::Resync(session_ports(&link, &hello, s)),
                    None => continue,
                }
            }
            Job::Message(text) => {
                // `#` starts a format in a message; `##` is a plain one.
                let argv = ["display-message", "-c", session::client_tty()]
                    .map(String::from)
                    .into_iter()
                    .chain([text.replace('#', "##")])
                    .collect::<Vec<_>>();
                let _ = link.request(&hello, &argv);
                continue;
            }
            // An upload on a thread of its own: a big one must not hold up
            // the events behind it.
            Job::Slow(action @ Action::Upload { .. }) => {
                let done = done.clone();
                std::thread::spawn(move || {
                    if let Some(input) = run_slow(&link, None, action) {
                        let _ = done.send(input);
                    }
                });
                continue;
            }
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

/// The id of the session this client is attached to, from the remote's
/// client list.
fn attached_session(link: &Link, hello: &str) -> Option<String> {
    let argv = ["list-clients", "-F", "#{client_tty} #{session_id}"].map(String::from);
    let out = link.request(hello, &argv).ok()?;
    out.lines()
        .filter_map(|l| l.rsplit_once(' '))
        .find(|(tty, _)| *tty == session::client_tty())
        .map(|(_, id)| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
