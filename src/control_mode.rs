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

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;

use crate::cmd::split_command_string;
use crate::event::Event;
use crate::protocol::{ClientCtx, Dialect, Hello, Reply, Request, encode_hello};
use crate::types::PaneId;

/// How often the bridge looks at the server when nothing woke it, for
/// changes no event announces (an automatic rename).
// ponytail: a 250 ms poll, so a change nobody announces shows up at most
// that late, and two changes inside one poll merge into one notification.
const POLL: Duration = Duration::from_millis(250);

/// The DCS that opens a `-CC` stream, and the ST that closes it.
const DCS_OPEN: &[u8] = b"\x1bP1000p";
const DCS_CLOSE: &[u8] = b"\x1b\\";

/// The most output held for one pane that is off.
const HELD_MAX: usize = 1 << 20;

/// The most pane output queued between the server and the main loop; past
/// it the event thread drops events rather than grow.
const QUEUE_MAX: usize = 8 << 20;

/// `data` as `%output` writes it: bytes below 0x20 and `\` as `\ooo` octal,
/// everything else as it is.
pub fn escape_output(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        if b < b' ' || b == b'\\' {
            out.extend_from_slice(format!("\\{b:03o}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

/// One window as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowInfo {
    /// The sessions the window is linked into.
    pub sessions: BTreeSet<String>,
    pub name: String,
    pub layout: String,
    /// `#{window_visible_layout}`; empty when the server has none (a zoomed
    /// window), then the layout stands in.
    pub visible_layout: String,
    pub flags: String,
    pub active_pane: String,
}

impl WindowInfo {
    fn visible(&self) -> &str {
        if self.visible_layout.is_empty() {
            &self.layout
        } else {
            &self.visible_layout
        }
    }
}

/// The sessions and windows of a server, as `$N` and `@N` ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Session id to its name.
    pub sessions: BTreeMap<String, String>,
    /// Session id to its current window.
    pub current: BTreeMap<String, String>,
    pub windows: BTreeMap<String, WindowInfo>,
    /// Every pane id (`%N`) the server has.
    pub panes: BTreeSet<String>,
}

/// The `-F` of the sessions query: id and name.
pub const SESSIONS_FORMAT: &str = "#{session_id}\t#{session_name}";

/// The `-F` of the windows query: session, window, whether the window is
/// its session's current one, its active pane, layout, flags and name.
pub const WINDOWS_FORMAT: &str = "#{session_id}\t#{window_id}\t#{window_active}\t#{pane_id}\t#{window_layout}\t#{window_visible_layout}\t#{window_flags}\t#{window_name}";

/// The `-F` of the panes query.
pub const PANES_FORMAT: &str = "#{pane_id}";

impl Snapshot {
    /// Reads the output of `list-sessions -F` [`SESSIONS_FORMAT`] and
    /// `list-windows -a -F` [`WINDOWS_FORMAT`].
    pub fn parse(sessions: &str, windows: &str) -> Snapshot {
        let mut snap = Snapshot::default();
        for line in sessions.lines() {
            if let Some((id, name)) = line.split_once('\t') {
                snap.sessions.insert(id.to_string(), name.to_string());
            }
        }
        for line in windows.lines() {
            let f: Vec<&str> = line.splitn(8, '\t').collect();
            let [session, window, active, pane, layout, visible, flags, name] = f[..] else {
                continue;
            };
            if active == "1" {
                snap.current.insert(session.to_string(), window.to_string());
            }
            let info = snap.windows.entry(window.to_string()).or_default();
            info.sessions.insert(session.to_string());
            if info.name.is_empty() || active == "1" {
                info.name = name.to_string();
                info.layout = layout.to_string();
                info.visible_layout = visible.to_string();
                info.flags = flags.to_string();
                info.active_pane = pane.to_string();
            }
        }
        snap
    }

    /// The windows linked into `session`, in id order.
    pub fn windows_of(&self, session: &str) -> Vec<&str> {
        self.windows
            .iter()
            .filter(|(_, w)| w.sessions.contains(session))
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

/// The notifications between `old` and `new` for a client attached to
/// `attached`, in the order tmux 3.7b sends them for one command.
///
/// A window that leaves with its session sends nothing; one that leaves
/// while a session still holds it sends `%unlinked-window-close`, which is
/// what tmux 3.7b sends for `kill-window` (the window is unlinked before
/// the notice).
// ponytail: `%window-close` (a window still linked in the client's session
// when it closes) is never sent.
pub fn diff(old: &Snapshot, new: &Snapshot, attached: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (session, window) in &new.current {
        if old.current.get(session).is_some_and(|w| w != window) {
            out.push(format!("%session-window-changed {session} {window}"));
        }
    }
    for (id, w) in &new.windows {
        if !old.windows.contains_key(id) {
            let kind = if w.sessions.contains(attached) {
                "window-add"
            } else {
                "unlinked-window-add"
            };
            out.push(format!("%{kind} {id}"));
        }
    }
    let ids = |s: &Snapshot| s.sessions.keys().cloned().collect::<Vec<_>>();
    if ids(old) != ids(new) {
        out.push("%sessions-changed".to_string());
    }
    if let (Some(a), Some(b)) = (old.sessions.get(attached), new.sessions.get(attached))
        && a != b
    {
        out.push(format!("%session-renamed {attached} {b}"));
    }
    for (id, w) in &new.windows {
        let Some(was) = old.windows.get(id) else {
            continue;
        };
        if was.name != w.name {
            let kind = if w.sessions.contains(attached) {
                "window-renamed"
            } else {
                "unlinked-window-renamed"
            };
            out.push(format!("%{kind} {id} {}", w.name));
        }
    }
    for (id, w) in &new.windows {
        if let Some(was) = old.windows.get(id)
            && was.active_pane != w.active_pane
        {
            out.push(format!("%window-pane-changed {id} {}", w.active_pane));
        }
    }
    for (id, w) in &new.windows {
        if let Some(was) = old.windows.get(id)
            && (was.layout != w.layout || was.visible() != w.visible())
            && w.sessions.contains(attached)
        {
            out.push(format!(
                "%layout-change {id} {} {} {}",
                w.layout,
                w.visible(),
                w.flags
            ));
        }
    }
    for (id, was) in &old.windows {
        let kept = was.sessions.iter().any(|s| new.sessions.contains_key(s));
        if !new.windows.contains_key(id) && kept {
            out.push(format!("%unlinked-window-close {id}"));
        }
    }
    out
}

/// What a pane's output does for this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneState {
    On,
    /// `-A %p:off`: held, and sent at `:on`.
    Off,
    /// `-A %p:pause`: dropped until `:continue`.
    Paused,
}

/// A client's flow control: which pane output is written, and how.
#[derive(Debug, Default)]
struct Flow {
    panes: BTreeMap<String, PaneState>,
    held: BTreeMap<String, Vec<u8>>,
    no_output: bool,
    pause_after: Option<u64>,
    wait_exit: bool,
}

impl Flow {
    /// `refresh-client -f` / `attach -f`: comma-separated flags, `!` clears.
    fn set_flags(&mut self, list: &str) {
        for flag in list.split(',').filter(|f| !f.is_empty()) {
            let (on, name) = match flag.strip_prefix('!') {
                Some(name) => (false, name),
                None => (true, flag),
            };
            match name.split_once('=').unwrap_or((name, "")) {
                ("no-output", _) => self.no_output = on,
                ("wait-exit", _) => self.wait_exit = on,
                ("pause-after", secs) => {
                    self.pause_after = on.then(|| secs.parse().unwrap_or(0));
                }
                _ => {}
            }
        }
    }

    /// `refresh-client -A %p:state`: the lines the command prints, or its
    /// error.
    fn set_pane(&mut self, arg: &str) -> Result<Vec<String>, String> {
        let (pane, state) = arg
            .split_once(':')
            .filter(|(p, _)| p.starts_with('%'))
            .ok_or_else(|| format!("bad pane argument: {arg}"))?;
        let mut printed = Vec::new();
        match state {
            "on" | "continue" => {
                self.panes.insert(pane.to_string(), PaneState::On);
                if state == "continue" {
                    printed.push(format!("%continue {pane}"));
                }
            }
            "off" => {
                self.panes.insert(pane.to_string(), PaneState::Off);
            }
            "pause" => {
                self.panes.insert(pane.to_string(), PaneState::Paused);
                self.held.remove(pane);
                printed.push(format!("%pause {pane}"));
            }
            _ => return Err(format!("bad pane argument: {arg}")),
        }
        Ok(printed)
    }

    /// Forgets the state of panes that are gone, so it cannot outgrow the
    /// server's panes.
    fn retain(&mut self, live: &BTreeSet<String>) {
        self.panes.retain(|p, _| live.contains(p));
        self.held.retain(|p, _| live.contains(p));
    }

    /// What to write for `data` printed by `pane`, `age_ms` after it came.
    /// With `pause-after`, output older than that pauses the pane instead,
    /// as tmux does.
    fn output(&mut self, pane: &str, data: &[u8], age_ms: u64) -> Option<Vec<u8>> {
        if self.no_output {
            return None;
        }
        match self.panes.get(pane).copied().unwrap_or(PaneState::On) {
            PaneState::Paused => None,
            PaneState::Off => {
                let held = self.held.entry(pane.to_string()).or_default();
                // ponytail: a pane held off keeps its first HELD_MAX bytes
                // and drops the rest.
                let room = HELD_MAX.saturating_sub(held.len());
                held.extend_from_slice(&data[..data.len().min(room)]);
                None
            }
            PaneState::On if self.pause_after.is_some_and(|s| age_ms / 1000 >= s.max(1)) => {
                self.panes.insert(pane.to_string(), PaneState::Paused);
                Some(format!("%pause {pane}\n").into_bytes())
            }
            PaneState::On => Some(self.frame(pane, data, age_ms)),
        }
    }

    /// The data held for `pane` while it was off, framed.
    fn release(&mut self, pane: &str) -> Option<Vec<u8>> {
        let data = self.held.remove(pane)?;
        Some(self.frame(pane, &data, 0))
    }

    fn frame(&self, pane: &str, data: &[u8], age_ms: u64) -> Vec<u8> {
        let mut line = if self.pause_after.is_some() {
            format!("%extended-output {pane} {age_ms} : ").into_bytes()
        } else {
            format!("%output {pane} ").into_bytes()
        };
        line.extend(escape_output(data));
        line.push(b'\n');
        line
    }
}

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

/// The ctx of a request: the hello of the front end, with `pane` the
/// attached session's active pane so a command with no target resolves there.
fn ctx(opts: &Options, pane: Option<&str>) -> ClientCtx {
    ClientCtx {
        cwd: opts.cwd.clone(),
        pane: pane.and_then(|p| p.parse::<PaneId>().ok()),
        env: opts.env.clone(),
        client: None,
        dialect: Dialect::Tmux,
    }
}

fn hello_line(ctx: &ClientCtx) -> Result<String, String> {
    encode_hello(&Hello::Control(ctx.clone())).map_err(|e| e.to_string())
}

/// Runs one request on its own connection: `Ok(out)`, or the error text.
fn exchange(socket: &Path, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("server gone: {e}"))?;
    let req = serde_json::to_string(&Request {
        id: 1,
        argv: argv.to_vec(),
        ..Request::default()
    })
    .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{}\n{req}\n", hello_line(ctx)?).as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Err("server closed the connection".to_string()),
            Ok(_) => {}
        }
        match serde_json::from_str::<Reply>(&line) {
            Ok(Reply::Ok { out, .. }) => return Ok(out),
            Ok(Reply::Err { err, out, .. }) => {
                return Err(format!("{out}{err}").trim_end().to_string());
            }
            Ok(Reply::Event(_)) => {}
            Err(e) => return Err(format!("bad reply from server: {e}")),
        }
    }
}

/// `target` as a session target: `display-message -t` reads a bare word as a
/// pane, so a word with no `:` gets one.
fn session_target(target: &str) -> String {
    if target.contains(':') {
        target.to_string()
    } else {
        format!("{target}:")
    }
}

/// tmux words its parser's errors `parse error: ...`; the front end's are
/// bare.
fn parse_error_text(err: &str) -> String {
    let parse = ["unknown command: ", "ambiguous command: "];
    if parse.iter().any(|p| err.starts_with(p)) {
        format!("parse error: {err}")
    } else {
        err.to_string()
    }
}

/// What a command line did to the client.
enum Outcome {
    Ok(String),
    Err(String),
    /// `detach-client`.
    Detach(String),
}

struct Bridge<'a, W: Write> {
    opts: &'a Options,
    out: W,
    flow: Flow,
    /// The next command number.
    num: u64,
    attached: String,
    snap: Snapshot,
    /// Notification lines a command asks for, written after its block.
    after: Vec<String>,
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
        let mut snap = Snapshot::parse(&sessions, &windows);
        snap.panes = panes.lines().map(str::to_string).collect();
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

    /// One command line: its commands in order, stopping at the first
    /// error. The text printed, or the error.
    fn command_line(&mut self, text: &str) -> Outcome {
        let cmds = match split_command_string(text) {
            Ok(c) => c,
            Err(e) => return Outcome::Err(format!("parse error: {e}")),
        };
        let mut printed = String::new();
        for argv in cmds {
            match self.command(&argv) {
                Outcome::Ok(out) => printed.push_str(&out),
                Outcome::Err(e) => return Outcome::Err(format!("{printed}{e}")),
                Outcome::Detach(out) => return Outcome::Detach(format!("{printed}{out}")),
            }
        }
        Outcome::Ok(printed)
    }

    fn command(&mut self, argv: &[String]) -> Outcome {
        let verb = crate::compat::tmux::resolve(&argv[0]).map_or(argv[0].as_str(), |s| s.name);
        match verb {
            "detach-client" => Outcome::Detach(String::new()),
            "switch-client" => self.switch_client(argv),
            "refresh-client" => self.refresh_client(argv),
            _ => match exchange(&self.opts.socket, &self.ctx(), argv) {
                Ok(out) => Outcome::Ok(out),
                Err(e) => Outcome::Err(parse_error_text(&e)),
            },
        }
    }

    /// `switch-client -t <session>`: the client's session changes, and
    /// `%session-changed` says so.
    fn switch_client(&mut self, argv: &[String]) -> Outcome {
        let Some(at) = argv.iter().position(|a| a == "-t") else {
            // ponytail: only an explicit `-t` (not `-n`, `-p`, `-l`).
            return Outcome::Err("switch-client: only -t is supported in control mode".into());
        };
        let Some(target) = argv.get(at + 1) else {
            return Outcome::Err("switch-client: -t needs a target".into());
        };
        match self.ask(&[
            "display-message",
            "-p",
            "-t",
            &session_target(target),
            "#{session_id}\t#{session_name}",
        ]) {
            Ok(out) => {
                let Some((id, name)) = out.trim_end().split_once('\t') else {
                    return Outcome::Err(format!("can't find session: {target}"));
                };
                self.attached = id.to_string();
                self.after.push(format!("%session-changed {id} {name}"));
                Outcome::Ok(String::new())
            }
            Err(e) => Outcome::Err(e),
        }
    }

    /// `refresh-client`: the flags of control mode are the bridge's own
    /// (`-C`, `-f`, `-A`).
    fn refresh_client(&mut self, argv: &[String]) -> Outcome {
        let mut printed = String::new();
        let mut i = 1;
        while i < argv.len() {
            let flag = argv[i].as_str();
            let value = argv.get(i + 1);
            match (flag, value) {
                ("-C", Some(size)) => {
                    if let Err(e) = self.resize(size) {
                        return Outcome::Err(e);
                    }
                }
                ("-f", Some(flags)) => self.flow.set_flags(flags),
                ("-A", Some(arg)) => match self.flow.set_pane(arg) {
                    Ok(lines) => {
                        for l in lines {
                            printed.push_str(&l);
                            printed.push('\n');
                        }
                    }
                    Err(e) => return Outcome::Err(e),
                },
                // ponytail: `-B` (subscriptions), `-r`, `-l`, `-S` and the
                // scroll flags are not control-mode features of this slice.
                ("-B" | "-r", Some(_)) => {
                    return Outcome::Err(format!("refresh-client {flag}: not supported yet"));
                }
                _ => {}
            }
            i += 2;
        }
        Outcome::Ok(printed)
    }

    /// `-C <w>x<h>` or `-C @<id>:<w>x<h>`: windows resize.
    fn resize(&self, arg: &str) -> Result<(), String> {
        let (window, size) = match arg.split_once(':') {
            Some((w, s)) if w.starts_with('@') => (Some(w), s),
            _ => (None, arg),
        };
        let (w, h) = size
            .split_once('x')
            .or_else(|| size.split_once(','))
            .filter(|(w, h)| w.parse::<u16>().is_ok() && h.parse::<u16>().is_ok())
            .ok_or_else(|| format!("bad size argument: {arg}"))?;
        let targets: Vec<String> = match window {
            Some(w) => vec![w.to_string()],
            None => self
                .snap
                .windows_of(&self.attached)
                .iter()
                .map(|w| w.to_string())
                .collect(),
        };
        for t in targets {
            self.ask(&["resize-window", "-t", &t, "-x", w, "-y", h])?;
        }
        Ok(())
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

/// Opens the event connection: every event, and `pane-output`.
fn subscribe(opts: &Options) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(&opts.socket).map_err(|e| format!("server gone: {e}"))?;
    let mut text = format!("{}\n", hello_line(&ctx(opts, None))?);
    for (id, argv) in [
        (1, vec!["subscribe"]),
        (2, vec!["subscribe", "pane-output"]),
    ] {
        let req = Request {
            id,
            argv: argv.into_iter().map(String::from).collect(),
            ..Request::default()
        };
        text.push_str(&serde_json::to_string(&req).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    stream
        .write_all(text.as_bytes())
        .map_err(|e| format!("subscribing: {e}"))?;
    Ok(stream)
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
                let (result, detach) = match bridge.command_line(&line) {
                    Outcome::Ok(out) => (Ok(out), false),
                    Outcome::Err(e) => (Err(e), false),
                    Outcome::Detach(out) => (Ok(out), true),
                };
                // What `refresh-client -A %p:on` released follows its block.
                if !bridge.block(1, &result) || (result.is_ok() && !bridge.release_all()) {
                    return 0;
                }
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
                if !bridge.notify() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(sessions: &[(&str, &str)], windows: &[&str]) -> Snapshot {
        let s: String = sessions
            .iter()
            .map(|(i, n)| format!("{i}\t{n}\n"))
            .collect();
        // A line is `session window active pane layout flags name`; the
        // visible layout is added empty unless `vis` follows the layout.
        let w: String = windows
            .iter()
            .map(|l| {
                let mut f: Vec<&str> = l.split('\t').collect();
                if f.len() == 7 {
                    f.insert(5, "");
                }
                format!("{}\n", f.join("\t"))
            })
            .collect();
        Snapshot::parse(&s, &w)
    }

    #[test]
    fn layout_change_reports_the_visible_layout() {
        let old = snap(&[("$0", "m")], &["$0\t@0\t1\t%0\tL0\t*\tw"]);
        let mut new = snap(&[("$0", "m")], &["$0\t@0\t1\t%0\tL0\t*Z\tw"]);
        // A zoomed window: the visible layout is the one zoomed pane.
        new.windows.get_mut("@0").unwrap().visible_layout = "V0".into();
        assert_eq!(diff(&old, &new, "$0"), ["%layout-change @0 L0 V0 *Z"]);
        let parsed = Snapshot::parse("$0\tm\n", "$0\t@0\t1\t%0\tL\tV\t*\tw\n");
        assert_eq!(parsed.windows["@0"].visible_layout, "V");
        assert_eq!(parsed.windows["@0"].name, "w");
    }

    #[test]
    fn flow_state_follows_live_panes_and_held_output_is_capped() {
        let mut flow = Flow::default();
        flow.set_pane("%0:off").unwrap();
        flow.set_pane("%1:pause").unwrap();
        assert!(flow.output("%0", &vec![b'x'; HELD_MAX + 10], 0).is_none());
        assert_eq!(flow.held["%0"].len(), HELD_MAX);
        flow.retain(&BTreeSet::from(["%1".to_string()]));
        assert!(flow.held.is_empty() && flow.panes.len() == 1);
    }

    #[test]
    fn a_pane_older_than_pause_after_is_paused() {
        let mut flow = Flow::default();
        flow.set_flags("pause-after=2");
        assert!(
            flow.output("%0", b"a", 1999)
                .unwrap()
                .starts_with(b"%extended")
        );
        assert_eq!(flow.output("%0", b"a", 2000).unwrap(), b"%pause %0\n");
        assert!(flow.output("%0", b"a", 0).is_none());
    }

    #[test]
    fn output_escapes_control_bytes_and_backslash_only() {
        let got = escape_output(b"a\r\n\\\x1b[1m\xc3\xa9~");
        assert_eq!(got, b"a\\015\\012\\134\\033[1m\xc3\xa9~");
    }

    #[test]
    fn a_new_window_is_added_and_a_new_session_is_unlinked() {
        let old = snap(&[("$0", "m")], &["$0\t@0\t1\t%0\tL0\t*\tw0"]);
        let new = snap(
            &[("$0", "m")],
            &["$0\t@0\t1\t%0\tL0\t*\tw0", "$0\t@1\t0\t%1\tL1\t-\tw1"],
        );
        assert_eq!(diff(&old, &new, "$0"), ["%window-add @1"]);
        let new = snap(
            &[("$0", "m"), ("$1", "two")],
            &["$0\t@0\t1\t%0\tL0\t*\tw0", "$1\t@2\t1\t%2\tL2\t*\tw2"],
        );
        assert_eq!(
            diff(&old, &new, "$0"),
            ["%unlinked-window-add @2", "%sessions-changed"]
        );
    }

    #[test]
    fn current_window_pane_layout_and_name_changes() {
        let old = snap(
            &[("$0", "m")],
            &["$0\t@0\t1\t%0\tL0\t*\ta", "$0\t@1\t0\t%1\tL1\t-\tb"],
        );
        let new = snap(
            &[("$0", "m")],
            &["$0\t@0\t0\t%3\tL9\t-\ta", "$0\t@1\t1\t%1\tL1\t*\tc"],
        );
        assert_eq!(
            diff(&old, &new, "$0"),
            [
                "%session-window-changed $0 @1",
                "%window-renamed @1 c",
                "%window-pane-changed @0 %3",
                "%layout-change @0 L9 L9 -",
            ]
        );
    }

    #[test]
    fn a_killed_window_closes_unlinked_but_a_killed_session_is_quiet() {
        let old = snap(
            &[("$0", "m"), ("$1", "t")],
            &[
                "$0\t@0\t1\t%0\tL\t*\ta",
                "$0\t@1\t0\t%1\tL\t-\tb",
                "$1\t@2\t1\t%2\tL\t*\tc",
            ],
        );
        let no_window = snap(
            &[("$0", "m"), ("$1", "t")],
            &["$0\t@0\t1\t%0\tL\t*\ta", "$1\t@2\t1\t%2\tL\t*\tc"],
        );
        assert_eq!(diff(&old, &no_window, "$0"), ["%unlinked-window-close @1"]);
        let no_session = snap(
            &[("$0", "m")],
            &["$0\t@0\t1\t%0\tL\t*\ta", "$0\t@1\t0\t%1\tL\t-\tb"],
        );
        assert_eq!(diff(&old, &no_session, "$0"), ["%sessions-changed"]);
    }

    #[test]
    fn pause_drops_off_holds_and_no_output_drops() {
        let mut flow = Flow::default();
        assert_eq!(flow.output("%0", b"a", 0).unwrap(), b"%output %0 a\n");
        flow.set_flags("pause-after=1");
        assert_eq!(
            flow.output("%0", b"a", 7).unwrap(),
            b"%extended-output %0 7 : a\n"
        );
        assert_eq!(flow.set_pane("%0:pause").unwrap(), ["%pause %0"]);
        assert!(flow.output("%0", b"lost", 0).is_none());
        assert_eq!(flow.set_pane("%0:continue").unwrap(), ["%continue %0"]);
        assert!(flow.output("%0", b"b", 0).is_some());
        flow.set_pane("%0:off").unwrap();
        assert!(flow.output("%0", b"held", 0).is_none());
        flow.set_pane("%0:on").unwrap();
        assert_eq!(
            flow.release("%0").unwrap(),
            b"%extended-output %0 0 : held\n"
        );
        flow.set_flags("no-output");
        assert!(flow.output("%0", b"x", 0).is_none());
        flow.set_flags("!no-output");
        assert!(flow.output("%0", b"x", 0).is_some());
    }
}
