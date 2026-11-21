//! tmux control mode, `tmux -C` and `-CC` (docs/specs/2026-11-15-control-mode.md).
//!
//! A client-side bridge: it reads command lines on its input, runs each as
//! requests on grofe's JSON control protocol, writes the answer as a
//! `%begin`/`%end` block, and turns what happens in the server (snapshot
//! diffs, `pane-output` events) into tmux's notifications. The server's
//! protocol is untouched.
//!
//! [`Snapshot`] and [`diff`] are the pure part: two views of the server in,
//! the notification lines between them out.

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;

use crate::event::Event;
use crate::protocol::{ClientCtx, Reply};

mod command;
mod conn;
mod flow;
mod notify;
mod subscribe;

use command::session_target;
use conn::{ctx, exchange, subscribe};
use flow::{Flow, PaneState};
use subscribe::Subs;

pub use flow::escape_output;
pub use notify::{
    BUFFERS_FORMAT, CLIENTS_FORMAT, PANES_FORMAT, SESSIONS_FORMAT, Snapshot, WINDOWS_FORMAT,
    WindowInfo, diff, pairs,
};

/// How often the bridge looks at the server when nothing woke it, for
/// changes no event announces (an automatic rename).
// ponytail: a 250 ms poll, so a change nobody announces shows up at most
// that late, and two changes inside one poll merge into one notification.
const POLL: Duration = Duration::from_millis(250);

/// The DCS that opens a `-CC` stream, and the ST that closes it.
const DCS_OPEN: &[u8] = b"\x1bP1000p";
const DCS_CLOSE: &[u8] = b"\x1b\\";

/// The most pane output queued between the server and the main loop; past
/// it the event thread drops events rather than grow.
const QUEUE_MAX: usize = 8 << 20;

/// How the bridge was started.
#[derive(Debug, Clone)]
pub struct Options {
    pub socket: PathBuf,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// `attach -t`.
    pub target: Option<String>,
    /// `new-session`'s arguments (its verb first), when it is the command.
    pub new_session: Option<Vec<String>>,
    /// `attach -f`.
    pub flags: Option<String>,
    /// `-CC`: the stream is a DCS.
    pub double: bool,
}

/// One thing the main loop waits for.
enum Msg {
    Line(String),
    Eof,
    Event(Event, Instant),
    Tick,
}

struct Bridge<'a, W: Write> {
    opts: &'a Options,
    out: W,
    flow: Flow,
    /// The next command number.
    num: u64,
    attached: String,
    snap: Snapshot,
    /// Notification lines a command line asks for, written after its last
    /// block.
    after: Vec<String>,
    /// Lines written right after the current command's own block.
    after_block: Vec<String>,
    /// `refresh-client -B`.
    subs: Subs,
}

impl<W: Write> Bridge<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.out
            .write_all(bytes)
            .and_then(|()| self.out.flush())
            .is_ok()
    }

    fn line(&mut self, text: &str) -> bool {
        self.write(format!("{text}\n").as_bytes())
    }

    fn block(&mut self, flags: u8, outcome: &Result<String, String>) -> bool {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let num = self.num;
        self.num += 1;
        let mut text = format!("%begin {secs} {num} {flags}\n");
        let (body, end) = match outcome {
            Ok(body) => (body, "%end"),
            Err(body) => (body, "%error"),
        };
        if !body.is_empty() {
            text.push_str(body);
            if !body.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str(&format!("{end} {secs} {num} {flags}\n"));
        self.write(text.as_bytes())
    }

    fn ctx(&self) -> ClientCtx {
        let pane = self
            .snap
            .windows
            .get(
                self.snap
                    .current
                    .get(&self.attached)
                    .map_or("", String::as_str),
            )
            .map(|w| w.active_pane.clone());
        ctx(self.opts, pane.as_deref())
    }

    fn ask(&self, argv: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        exchange(&self.opts.socket, &self.ctx(), &argv)
    }

    fn take_snapshot(&self) -> Result<Snapshot, String> {
        let sessions = self.ask(&["list-sessions", "-F", SESSIONS_FORMAT])?;
        let windows = self.ask(&["list-windows", "-a", "-F", WINDOWS_FORMAT])?;
        let panes = self.ask(&["list-panes", "-a", "-F", PANES_FORMAT])?;
        let clients = self.ask(&["list-clients", "-F", CLIENTS_FORMAT])?;
        let buffers = self.ask(&["list-buffers", "-F", BUFFERS_FORMAT])?;
        let mut snap = Snapshot::parse(&sessions, &windows);
        snap.modes = pairs(&panes);
        snap.panes = snap.modes.keys().cloned().collect();
        snap.clients = pairs(&clients);
        snap.buffers = pairs(&buffers);
        Ok(snap)
    }

    /// Looks at the server and writes what changed. `false` when the
    /// attached session is gone or the output is.
    fn notify(&mut self) -> bool {
        let Ok(new) = self.take_snapshot() else {
            return false;
        };
        let lines = diff(&self.snap, &new, &self.attached);
        let alive = new.sessions.contains_key(&self.attached);
        self.flow.retain(&new.panes);
        self.snap = new;
        for l in lines {
            if !self.line(&l) {
                return false;
            }
        }
        alive
    }

    /// Writes the subscriptions that changed, once a second.
    fn check_subs(&mut self) -> bool {
        if !self.subs.due(Instant::now()) {
            return true;
        }
        let mut subs = std::mem::take(&mut self.subs);
        let lines = subs.check(&self.attached, |argv| self.ask(argv));
        self.subs = subs;
        lines.iter().all(|l| self.line(l))
    }

    /// Writes the pane output an event carried, as the flow allows.
    fn pane_output(&mut self, pane: &str, data: &str, since: Instant) -> bool {
        // Output of a pane the last snapshot does not know is a new pane's
        // (its `%window-add` goes first) or a closed one's (dropped).
        if !self.snap.panes.contains(pane) && (!self.notify() || !self.snap.panes.contains(pane)) {
            return true;
        }
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
            return true;
        };
        let age = u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX);
        match self.flow.output(pane, &bytes, age) {
            Some(line) => self.write(&line),
            None => true,
        }
    }

    /// Sends what `-A %p:on` released.
    fn release_all(&mut self) -> bool {
        let on: Vec<String> = self
            .flow
            .panes
            .iter()
            .filter(|(_, s)| **s == PaneState::On)
            .map(|(p, _)| p.clone())
            .collect();
        for pane in on {
            if let Some(line) = self.flow.release(&pane)
                && !self.write(&line)
            {
                return false;
            }
        }
        true
    }

    /// Ends the stream: `%exit`, then the ST of `-CC`.
    fn exit(&mut self) {
        self.line("%exit");
        if self.opts.double {
            self.write(DCS_CLOSE);
        }
    }
}

/// Runs control mode on `input` and `output` until the client leaves.
/// Returns the exit status.
pub fn run(opts: &Options, input: impl BufRead + Send + 'static, output: impl Write) -> i32 {
    let mut bridge = Bridge {
        opts,
        out: output,
        flow: Flow::default(),
        num: 0,
        attached: String::new(),
        snap: Snapshot::default(),
        after: Vec::new(),
        after_block: Vec::new(),
        subs: Subs::default(),
    };
    if opts.double {
        bridge.write(DCS_OPEN);
    }
    if let Some(flags) = &opts.flags {
        bridge.flow.set_flags(flags);
    }
    let created = match start(&mut bridge) {
        Ok(created) => created,
        Err(e) => {
            bridge.block(0, &Err(e));
            bridge.exit();
            return 1;
        }
    };
    bridge.block(0, &Ok(String::new()));
    for l in created {
        bridge.line(&l);
    }
    let name = bridge
        .snap
        .sessions
        .get(&bridge.attached)
        .cloned()
        .unwrap_or_default();
    let session = format!("%session-changed {} {name}", bridge.attached);
    bridge.line(&session);

    let events = match subscribe(opts) {
        Ok(s) => s,
        Err(e) => {
            bridge.line(&format!("%exit {e}"));
            if bridge.opts.double {
                bridge.write(DCS_CLOSE);
            }
            return 1;
        }
    };
    let (tx, rx) = mpsc::channel();
    spawn_input(input, tx.clone());
    let queued = Arc::new(AtomicUsize::new(0));
    spawn_events(events, tx.clone(), Arc::clone(&queued));
    spawn_ticks(tx);

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(line) if line.trim().is_empty() => {}
            Msg::Line(line) => {
                let Some(detach) = bridge.command_line(&line) else {
                    return 0;
                };
                for l in std::mem::take(&mut bridge.after) {
                    bridge.line(&l);
                }
                if detach || !bridge.notify() {
                    break;
                }
            }
            Msg::Eof => break,
            Msg::Event(Event::PaneOutput { pane, data }, since) => {
                queued.fetch_sub(data.len(), Ordering::Relaxed);
                if !bridge.pane_output(&pane.to_string(), &data, since) {
                    return 0;
                }
            }
            Msg::Event(e, _) => {
                queued.fetch_sub(weight(&e), Ordering::Relaxed);
                if !bridge.notify() {
                    break;
                }
            }
            Msg::Tick => {
                if !bridge.notify() || !bridge.check_subs() {
                    break;
                }
            }
        }
    }
    bridge.exit();
    if bridge.flow.wait_exit {
        // `wait-exit`: the client lingers until an empty line (or EOF).
        loop {
            match rx.recv() {
                Ok(Msg::Line(l)) if l.is_empty() => break,
                Ok(Msg::Eof) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
    0
}

/// Finds or makes the attached session; the notifications that come with
/// a new one (`-C new-session`).
fn start<W: Write>(bridge: &mut Bridge<'_, W>) -> Result<Vec<String>, String> {
    let mut created = Vec::new();
    let target = match &bridge.opts.new_session {
        Some(argv) => {
            let mut argv = argv.clone();
            argv.extend(["-d", "-P", "-F", "#{session_id}"].map(String::from));
            let out = exchange(&bridge.opts.socket, &ctx(bridge.opts, None), &argv)?;
            Some(out.trim().to_string())
        }
        None => bridge.opts.target.clone(),
    };
    let mut query = vec!["display-message", "-p"];
    let target = target.map(|t| session_target(&t));
    if let Some(t) = &target {
        query.extend(["-t", t]);
    }
    query.push("#{session_id}\t#{session_name}");
    let out = bridge.ask(&query).map_err(|e| {
        if target.is_some() {
            e
        } else {
            "no sessions".into()
        }
    })?;
    let (id, _) = out
        .trim_end()
        .split_once('\t')
        .ok_or_else(|| "no sessions".to_string())?;
    bridge.attached = id.to_string();
    bridge.snap = bridge.take_snapshot()?;
    if bridge.opts.new_session.is_some() {
        for w in bridge.snap.windows_of(id) {
            created.push(format!("%window-add {w}"));
        }
        created.push("%sessions-changed".to_string());
    }
    Ok(created)
}

fn spawn_input(input: impl BufRead + Send + 'static, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        for line in input.lines().map_while(Result::ok) {
            if tx.send(Msg::Line(line)).is_err() {
                return;
            }
        }
        let _ = tx.send(Msg::Eof);
    });
}

/// What an event queued costs against [`QUEUE_MAX`].
fn weight(e: &Event) -> usize {
    match e {
        Event::PaneOutput { data, .. } => data.len(),
        _ => 64,
    }
}

fn spawn_events(stream: UnixStream, tx: mpsc::Sender<Msg>, queued: Arc<AtomicUsize>) {
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            let Ok(Reply::Event(e)) = serde_json::from_str::<Reply>(&line) else {
                continue;
            };
            // A stalled consumer: drop events (the poll finds what they
            // announced), not memory.
            if queued.load(Ordering::Relaxed) > QUEUE_MAX {
                continue;
            }
            queued.fetch_add(weight(&e), Ordering::Relaxed);
            if tx.send(Msg::Event(e, Instant::now())).is_err() {
                return;
            }
        }
        // The server closed the connection: it is gone.
        let _ = tx.send(Msg::Eof);
    });
}

fn spawn_ticks(tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        while tx.send(Msg::Tick).is_ok() {
            std::thread::sleep(POLL);
        }
    });
}
