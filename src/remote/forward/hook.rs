//! [`ForwardHook`]: forwarding plugged into the remote attach loop
//! ([`session::Hook`](crate::remote::session::Hook)). Keys, events and ticks
//! go into [`Forwarding`]; prompts go through the client's prompt; slow
//! actions (uploads, the upload directory, opening a URL, messages) run on
//! a worker thread whose results come back on the next tick; forwards run
//! on the input thread (a bind is quick) over whatever link is current.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::exec::{LinkCell, is_slow, opener, run_slow};
use super::ports::Forwarder;
use super::{Action, ForwardMode, Forwarding, Input, Settings, UploadMode};
use crate::event::Event;
use crate::remote::connect::Link;
use crate::remote::session::{self, Hook, HookCtx, Prompt};

/// Events forwarding subscribes to.
pub const EVENTS: [&str; 3] = ["open-url", "ports-changed", "pane-exited"];

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
    done: Receiver<Done>,
    /// Whether the remote's options were asked for.
    asked: bool,
}

/// Work for the worker thread.
enum Job {
    Settings,
    Slow(Action),
    Message(String),
}

/// What the worker hands back.
enum Done {
    Settings(Settings),
    Input(Input),
}

impl ForwardHook {
    /// Forwarding for a client of `host`, whose control requests use
    /// `hello`; its forwards are listed under `state_dir`
    /// ([`list_forwards`](super::list_forwards)).
    pub fn new(host: &str, hello: &str, state_dir: Option<PathBuf>) -> ForwardHook {
        let link = LinkCell::default();
        let (jobs, job_rx) = channel();
        let (done_tx, done) = channel();
        let worker_link = link.clone();
        let hello = hello.to_string();
        std::thread::spawn(move || worker(worker_link, hello, job_rx, done_tx));
        let core = Core {
            fw: Forwarding::new(host, Settings::default()),
            forwarder: Forwarder::new(host, opener(link.clone()), state_dir),
            link,
            jobs,
            done,
            asked: false,
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
    /// Points the forwards at `link` (a reconnect gives a new one) and asks
    /// for the remote's options the first time.
    fn connected(&mut self, link: &Link) {
        self.link.set(link.clone());
        if !self.asked {
            self.asked = true;
            let _ = self.jobs.send(Job::Settings);
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
        if let Ok(event) = serde_json::from_value::<Event>(event.clone()) {
            let send = self.feed(cx, Input::Event(event));
            cx.send(send);
        }
    }

    fn tick(&mut self, cx: &mut HookCtx<'_>, now: Instant) {
        let mut core = self.lock();
        core.connected(cx.link());
        let mut inputs = vec![Input::Tick];
        while let Ok(done) = core.done.try_recv() {
            match done {
                Done::Settings(s) => core.fw.set_settings(s),
                Done::Input(input) => inputs.push(input),
            }
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
}

impl Prompt for Answer {
    fn text(&self) -> String {
        // The upload directory comes from the remote: nothing in it may
        // reach the terminal as a control.
        crate::remote::ssh::sanitize(&format!("{} {}", self.text, self.line))
            .replace(['\n', '\t'], " ")
    }

    fn key(&mut self, cx: &mut HookCtx<'_>, bytes: &[u8]) -> Option<session::Input> {
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

fn worker(link: LinkCell, hello: String, jobs: Receiver<Job>, done: Sender<Done>) {
    for job in jobs {
        let Some(link) = link.get() else { continue };
        let reply = match job {
            Job::Settings => Done::Settings(read_settings(&link, &hello)),
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
            Job::Slow(action) => {
                let target = attached_session(&link, &hello);
                match run_slow(&link, target.as_deref(), action) {
                    Some(input) => Done::Input(input),
                    None => continue,
                }
            }
        };
        if done.send(reply).is_err() {
            return;
        }
    }
}

/// The remote options forwarding follows; a default for one that cannot be
/// read.
fn read_settings(link: &Link, hello: &str) -> Settings {
    let get = |name: &str| {
        let argv = ["show-options", "-gv", name].map(String::from);
        link.request(hello, &argv)
            .ok()
            .map(|v| v.trim().to_string())
    };
    let default = Settings::default();
    Settings {
        upload_paste: get("remote-upload-paste")
            .and_then(|v| UploadMode::parse(&v))
            .unwrap_or(default.upload_paste),
        upload_max: get("remote-upload-max")
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(default.upload_max, |mib| mib << 20),
        forward_ports: get("remote-forward-ports")
            .and_then(|v| ForwardMode::parse(&v))
            .unwrap_or(default.forward_ports),
    }
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
