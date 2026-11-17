//! Client forwarding for a remote session (spec 2026-11-15-ssh-remote §3):
//! URLs a remote pane opens, files pasted as paths, and ports a remote pane
//! listens on.
//!
//! [`Forwarding`] is the pure part: it takes [`Input`]s (keys bound for the
//! server, control events, prompt answers, I/O results, time) and returns
//! [`Action`]s. The client does the screen actions ([`Action::Send`],
//! [`Action::Prompt`], [`Action::ClearPrompt`], [`Action::Message`]);
//! [`Executor`] does the I/O ones over the proxy connection and answers
//! with the [`Input`] to feed back.

mod exec;
mod hook;
mod ports;
#[cfg(test)]
mod tests;
mod text;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::event::Event;
use crate::types::PaneId;

pub use exec::{Executor, LinkCell, Opener, is_slow, opener, run_slow, upload_dir, upload_files};
pub use hook::ForwardHook;
pub use ports::{Forward, Forwarder, format_forwards, forwards_dir, list_forwards};
use text::count_files;
pub use text::{forward_prompt, human_size, parse_paths, shell_quote, upload_prompt};

/// The bracketed paste start marker.
pub const PASTE_START: &[u8] = b"\x1b[200~";
/// The bracketed paste end marker.
pub const PASTE_END: &[u8] = b"\x1b[201~";
/// The longest paste held back to look for file paths; a longer one passes
/// through as it is.
pub const MAX_PASTE: usize = 64 * 1024;
/// How long a paste may go without its end marker before it passes through.
const PASTE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a split start marker (`ESC [` at the end of a read) is held.
const HOLD_TIMEOUT: Duration = Duration::from_millis(30);
/// Where uploads go when the pane's directory is unknown.
pub const FALLBACK_UPLOAD_DIR: &str = "~/.cache/grofe/uploads";

/// `remote-upload-paste`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadMode {
    Ask,
    On,
    Off,
}

impl UploadMode {
    /// The option's value; `None` for another word.
    pub fn parse(s: &str) -> Option<UploadMode> {
        match s {
            "ask" => Some(UploadMode::Ask),
            "on" => Some(UploadMode::On),
            "off" => Some(UploadMode::Off),
            _ => None,
        }
    }
}

/// `remote-forward-ports`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardMode {
    Ask,
    Auto,
    Off,
}

impl ForwardMode {
    /// The option's value; `None` for another word.
    pub fn parse(s: &str) -> Option<ForwardMode> {
        match s {
            "ask" => Some(ForwardMode::Ask),
            "auto" => Some(ForwardMode::Auto),
            "off" => Some(ForwardMode::Off),
            _ => None,
        }
    }
}

/// The options forwarding follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub upload_paste: UploadMode,
    /// `remote-upload-max`, in bytes.
    pub upload_max: u64,
    pub forward_ports: ForwardMode,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            upload_paste: UploadMode::Ask,
            upload_max: 100 << 20,
            forward_ports: ForwardMode::Ask,
        }
    }
}

/// A bracketed paste of local files, waiting for its upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePaste {
    pub files: Vec<PathBuf>,
    /// Their total size in bytes.
    pub total: u64,
    /// The paste's content, without its markers, for when it passes
    /// through after all.
    pub original: Vec<u8>,
}

/// Where an upload goes on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadDir {
    /// The directory the proxy writes to (absolute, or `~/...`).
    pub path: String,
    /// The same for a prompt: the remote's home shown as `~`.
    pub shown: String,
}

/// What [`Forwarding::handle`] is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Bytes the user typed, bound for the server.
    Keys(Vec<u8>),
    /// An event from the remote server's control channel.
    Event(Event),
    /// The answer to the [`Action::Prompt`] on screen.
    Answer(String),
    /// [`Action::ResolveDir`]'s result.
    DirResolved(FilePaste, UploadDir),
    /// [`Action::Upload`]'s result: the remote paths, or why it failed.
    Uploaded(Result<Vec<String>, String>),
    /// [`Action::StartForward`]'s result: the local port, or why not.
    Forwarded {
        remote: u16,
        local: Result<u16, String>,
    },
    /// [`Action::OpenUrl`]'s result.
    Opened(Result<(), String>),
    /// Time passing, so a held key or a paste with no end goes out.
    Tick,
}

/// What [`Forwarding::handle`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write these bytes to the server (the attach stream).
    Send(Vec<u8>),
    /// Show this one-line prompt; its answer comes back as
    /// [`Input::Answer`]. Replaces any prompt on screen.
    Prompt(String),
    /// Take the prompt on screen away, unanswered.
    ClearPrompt,
    /// Show this one-line message.
    Message(String),
    /// Open this URL on this machine.
    OpenUrl(String),
    /// Find where the paste's files go ([`Input::DirResolved`]).
    ResolveDir(FilePaste),
    /// Upload `files` into `dir` ([`Input::Uploaded`]).
    Upload { dir: String, files: Vec<PathBuf> },
    /// Forward local connections to remote `port` ([`Input::Forwarded`]).
    StartForward(u16),
    /// Stop forwarding remote `port`.
    StopForward(u16),
}

/// A question waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ask {
    Upload(FilePaste, UploadDir),
    Forward(u16),
}

/// The forwarding state of one remote client.
#[derive(Debug)]
pub struct Forwarding {
    host: String,
    settings: Settings,
    /// Bytes held back: a split start marker, or a paste so far.
    held: Vec<u8>,
    /// When the paste being held started; `None` when not in one.
    paste_since: Option<Instant>,
    /// When the bytes in `held` were first held.
    held_since: Option<Instant>,
    /// The question on screen, then the ones after it.
    asks: VecDeque<Ask>,
    /// Each pane's listening ports.
    pane_ports: HashMap<PaneId, BTreeSet<u16>>,
    /// Ports being forwarded (asked for or running).
    forwarded: BTreeSet<u16>,
    /// Ports the user said no to, while they stay open.
    declined: BTreeSet<u16>,
}

impl Forwarding {
    /// Forwarding for a client of `host` (as the user named it).
    pub fn new(host: &str, settings: Settings) -> Forwarding {
        Forwarding {
            host: host.to_string(),
            settings,
            held: Vec::new(),
            paste_since: None,
            held_since: None,
            asks: VecDeque::new(),
            pane_ports: HashMap::new(),
            forwarded: BTreeSet::new(),
            declined: BTreeSet::new(),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the settings (once the remote's options are read).
    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
    }

    /// The remote ports being forwarded.
    pub fn forwarded(&self) -> Vec<u16> {
        self.forwarded.iter().copied().collect()
    }

    /// Takes one input at `now`; what to do about it, in order.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::Keys(bytes) => self.keys(&bytes, now, &mut out),
            Input::Tick => self.tick(now, &mut out),
            Input::Event(Event::OpenUrl { url, .. }) => out.push(Action::OpenUrl(url)),
            Input::Event(Event::PortsChanged { pane, ports }) => {
                self.ports(pane, ports.into_iter().collect(), &mut out)
            }
            Input::Event(Event::PaneExited { pane, .. }) => {
                self.ports(pane, BTreeSet::new(), &mut out);
                self.pane_ports.remove(&pane);
            }
            Input::Event(_) => {}
            Input::Answer(answer) => self.answer(&answer, &mut out),
            Input::DirResolved(paste, dir) => match self.settings.upload_paste {
                UploadMode::On => out.push(Action::Upload {
                    dir: dir.path,
                    files: paste.files,
                }),
                UploadMode::Ask => self.ask(Ask::Upload(paste, dir), &mut out),
                UploadMode::Off => send(&mut out, &bracketed(&paste.original)),
            },
            Input::Uploaded(Ok(paths)) => {
                let quoted: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
                send(&mut out, &bracketed(quoted.join(" ").as_bytes()));
            }
            Input::Uploaded(Err(e)) => out.push(Action::Message(format!("grofe: {e}"))),
            Input::Forwarded { remote, local } => match local {
                Ok(local) => out.push(Action::Message(format!(
                    "forwarding {}:{remote} to localhost:{local}",
                    self.host
                ))),
                Err(e) => {
                    self.forwarded.remove(&remote);
                    out.push(Action::Message(format!(
                        "grofe: cannot forward {}:{remote}: {e}",
                        self.host
                    )));
                }
            },
            Input::Opened(Ok(())) => {}
            Input::Opened(Err(e)) => {
                out.push(Action::Message(format!("grofe: not opening a url: {e}")))
            }
        }
        out
    }

    fn keys(&mut self, bytes: &[u8], now: Instant, out: &mut Vec<Action>) {
        if self.held.is_empty() {
            self.held_since = Some(now);
        }
        self.held.extend_from_slice(bytes);
        loop {
            if self.paste_since.is_some() {
                if let Some(i) = find(&self.held, PASTE_END) {
                    let content = self.held[..i].to_vec();
                    self.held.drain(..i + PASTE_END.len());
                    self.paste_since = None;
                    self.held_since = Some(now);
                    self.paste(content, out);
                    continue;
                }
                if self.held.len() > MAX_PASTE {
                    self.flush_paste(out);
                }
                return;
            }
            if let Some(i) = find(&self.held, PASTE_START) {
                let before: Vec<u8> = self.held.drain(..i + PASTE_START.len()).collect();
                send(out, &before[..i]);
                self.paste_since = Some(now);
                continue;
            }
            let keep = split_start(&self.held);
            let ready: Vec<u8> = self.held.drain(..self.held.len() - keep).collect();
            send(out, &ready);
            return;
        }
    }

    fn tick(&mut self, now: Instant, out: &mut Vec<Action>) {
        match (self.paste_since, self.held_since) {
            (Some(since), _) if now.duration_since(since) >= PASTE_TIMEOUT => self.flush_paste(out),
            (None, Some(since))
                if !self.held.is_empty() && now.duration_since(since) >= HOLD_TIMEOUT =>
            {
                let held = std::mem::take(&mut self.held);
                send(out, &held);
            }
            _ => {}
        }
    }

    /// Sends a paste held so far as it came, start marker and all.
    fn flush_paste(&mut self, out: &mut Vec<Action>) {
        let mut bytes = PASTE_START.to_vec();
        bytes.append(&mut self.held);
        self.paste_since = None;
        send(out, &bytes);
    }

    /// A whole bracketed paste: an upload when it names local files.
    fn paste(&mut self, content: Vec<u8>, out: &mut Vec<Action>) {
        let files = match self.settings.upload_paste {
            UploadMode::Off => None,
            _ => std::str::from_utf8(&content)
                .ok()
                .and_then(parse_paths)
                .and_then(|p| local_files(&p).map(|total| (p, total))),
        };
        let Some((files, total)) = files else {
            return send(out, &bracketed(&content));
        };
        if total > self.settings.upload_max {
            return out.push(Action::Message(format!(
                "grofe: not uploading {} ({}): over remote-upload-max ({} MiB)",
                count_files(files.len()),
                human_size(total),
                self.settings.upload_max >> 20
            )));
        }
        out.push(Action::ResolveDir(FilePaste {
            files,
            total,
            original: content,
        }));
    }

    fn ports(&mut self, pane: PaneId, ports: BTreeSet<u16>, out: &mut Vec<Action>) {
        let before = self.all_ports();
        self.pane_ports.insert(pane, ports);
        let after = self.all_ports();
        for &port in before.difference(&after) {
            self.declined.remove(&port);
            if self.forwarded.remove(&port) {
                out.push(Action::StopForward(port));
            }
            let pos = self.asks.iter().position(|a| *a == Ask::Forward(port));
            if let Some(pos) = pos {
                self.asks.remove(pos);
                if pos == 0 {
                    out.push(Action::ClearPrompt);
                    self.show_next(out);
                }
            }
        }
        for &port in after.difference(&before) {
            if self.forwarded.contains(&port) || self.declined.contains(&port) {
                continue;
            }
            match self.settings.forward_ports {
                ForwardMode::Auto => self.forward(port, out),
                ForwardMode::Ask => self.ask(Ask::Forward(port), out),
                ForwardMode::Off => {}
            }
        }
    }

    fn all_ports(&self) -> BTreeSet<u16> {
        self.pane_ports.values().flatten().copied().collect()
    }

    fn forward(&mut self, port: u16, out: &mut Vec<Action>) {
        if self.forwarded.insert(port) {
            out.push(Action::StartForward(port));
        }
    }

    fn ask(&mut self, ask: Ask, out: &mut Vec<Action>) {
        self.asks.push_back(ask);
        if self.asks.len() == 1 {
            self.show_next(out);
        }
    }

    fn show_next(&self, out: &mut Vec<Action>) {
        if let Some(ask) = self.asks.front() {
            out.push(Action::Prompt(self.prompt(ask)));
        }
    }

    fn prompt(&self, ask: &Ask) -> String {
        match ask {
            Ask::Upload(paste, dir) => upload_prompt(&self.host, paste, dir),
            Ask::Forward(port) => forward_prompt(&self.host, *port),
        }
    }

    fn answer(&mut self, answer: &str, out: &mut Vec<Action>) {
        let Some(ask) = self.asks.pop_front() else {
            return;
        };
        let answer = answer.trim().to_ascii_lowercase();
        let yes = answer == "y" || answer == "yes";
        match ask {
            Ask::Upload(paste, dir) if yes => out.push(Action::Upload {
                dir: dir.path,
                files: paste.files,
            }),
            Ask::Upload(paste, _) => send(out, &bracketed(&paste.original)),
            Ask::Forward(port) if answer == "always" || answer == "a" => {
                self.settings.forward_ports = ForwardMode::Auto;
                self.forward(port, out);
                let (ports, rest): (Vec<Ask>, Vec<Ask>) = self
                    .asks
                    .drain(..)
                    .partition(|a| matches!(a, Ask::Forward(_)));
                self.asks.extend(rest);
                for ask in ports {
                    if let Ask::Forward(p) = ask {
                        self.forward(p, out);
                    }
                }
            }
            Ask::Forward(port) if yes => self.forward(port, out),
            Ask::Forward(port) => {
                self.declined.insert(port);
            }
        }
        self.show_next(out);
    }
}

/// Appends `bytes` to the last [`Action::Send`] in `out`, or adds one.
fn send(out: &mut Vec<Action>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(Action::Send(last)) = out.last_mut() {
        last.extend_from_slice(bytes);
    } else {
        out.push(Action::Send(bytes.to_vec()));
    }
}

fn bracketed(content: &[u8]) -> Vec<u8> {
    [PASTE_START, content, PASTE_END].concat()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// How many bytes at the end of `bytes` could be a start marker cut by a
/// read. A lone ESC is a key of its own and is not held.
fn split_start(bytes: &[u8]) -> usize {
    (2..PASTE_START.len())
        .rev()
        .find(|&n| bytes.ends_with(&PASTE_START[..n]))
        .unwrap_or(0)
}

/// The total size of `paths` when every one is a local regular file.
/// Directories are not uploaded, so a paste naming one stays text.
fn local_files(paths: &[PathBuf]) -> Option<u64> {
    paths.iter().try_fold(0u64, |total, p| {
        let meta = std::fs::metadata(p).ok().filter(|m| m.is_file())?;
        Some(total + meta.len())
    })
}
