//! Passthrough (T3.3, spec §4.9): what escapes the multiplexer to the
//! attached clients. The emulator consumes every byte, so each item is
//! re-emitted on purpose:
//!
//! - OSC 52 clipboard stores (the emulator decodes them into
//!   `PaneEventKind::Clipboard`), to the session's clients while
//!   `set-clipboard` is `on` (R14); reads are never forwarded;
//! - the outer terminal's title (OSC 0), the client's current pane's
//!   `set-titles-string` expanded, while `set-titles` is on (R15; off,
//!   tmux's default, sends nothing);
//! - BEL, to the session's clients, also setting `window_bell_flag` (R13);
//! - OSC 8 hyperlinks, re-emitted by `render::diff` from the cells (R20);
//! - OSC 7, read by the [`Scanner`] into `PaneEventKind::Cwd`;
//! - OSC 9 and 777 notifications, verbatim to the session's clients;
//! - OSC 12/112 cursor colour, to each client the active pane's (R16),
//!   re-sent when a client's active pane changes ([`on_active_changed`]).
//!
//! The [`Scanner`] runs over each pane's output ahead of the emulator,
//! which parses neither OSC 7/9/777 nor APC. It also frames kitty graphics
//! commands (APC `_G`) into `PaneEventKind::Graphics` for T3.8, and those
//! bytes never reach the emulator.
//!
//! The `DCS tmux;` wrapper that apps send once `$TMUX` is set (TC.12) is
//! unwrapped by the same [`Scanner`]: none of its bytes reach the emulator
//! as they are. Its content, `ESC ESC` read as one `ESC`, is fed through a
//! fresh scanner and on to the emulator as if sent bare, but only when it
//! is nothing but complete sequences the bare path acts on (OSC 0, 2, 7,
//! 8, 9, 12, 52, 112 and 777, BEL, APC `_G`). Anything else is dropped
//! whole, so a wrapper lets out nothing that the same bytes sent bare
//! would not, and each sequence it lets out meets the same filters.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use base64::Engine as _;

use crate::cmd::{FormatVars, expand};
use crate::server::dispatch::options::{self, FormatCtx};
use crate::server::model::Model;
use crate::server::pane::PaneEventKind;
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId, SessionId};

/// Most bytes of one APC `_G` command kept, from `ESC _ G` on. kitty sends
/// image data in chunks of 4096 base64 bytes, so a real command is far
/// smaller.
const MAX_APC: usize = 4 * 1024 * 1024;
/// Most payload bytes of one OSC kept. The OSCs the scanner reads (7) are
/// short; longer ones (a large OSC 52) are the emulator's alone.
const MAX_OSC: usize = 4 * 1024;

/// What opens the wrapper TC.12 unwraps: `ESC P tmux;`.
const DCS_TMUX: &[u8] = b"\x1bPtmux;";
/// Most content bytes (after `tmux;`, `ESC ESC` counted once) of one
/// wrapper, as tmux 3.7b's `input-buffer-size` of 1048576 allows once it
/// has counted `tmux;` and a terminator: recorded, 1048570 passes and
/// 1048571 is dropped.
const MAX_DCS_TMUX: usize = 1024 * 1024 - 6;
/// The OSC numbers a wrapper may carry: those the bare path acts on. Title
/// (0, 2), cwd (7), hyperlink (8), notifications (9, 777), cursor colour
/// (12, 112) and clipboard (52); each then meets its bare filters (the
/// OSC 7 host check, [`relayed`] leaving `12;?` to the emulator).
const WRAPPED_OSC: &[&[u8]] = &[b"0", b"2", b"7", b"8", b"9", b"12", b"52", b"112", b"777"];

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// CAN and SUB abort any sequence in progress.
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// What the server keeps for passthrough: what a client must be re-sent
/// when it shows another pane (title, cursor colour), and which pane each
/// session last gave focus to.
#[derive(Debug, Default)]
pub struct Passthrough {
    /// Each pane's cursor colour (its OSC 12 body), while it has set one.
    cursor: BTreeMap<PaneId, Vec<u8>>,
    /// Each pane's title, once it has set one.
    titles: BTreeMap<PaneId, String>,
    /// What each client was last sent.
    sent: BTreeMap<ClientId, Shown>,
    /// The pane each session with a client last gave focus to (mode 1004).
    focus: BTreeMap<SessionId, PaneId>,
}

impl Passthrough {
    /// `pane`'s title (`#{pane_title}`, `#T`): the one it set, else the
    /// host name, which tmux titles every new pane with.
    pub(crate) fn title(&self, pane: PaneId) -> String {
        pane_title(&self.titles, pane)
    }
}

/// [`Passthrough::title`], for callers that hold only the titles.
fn pane_title(titles: &BTreeMap<PaneId, String>, pane: PaneId) -> String {
    titles
        .get(&pane)
        .cloned()
        .unwrap_or_else(|| hostname().to_string())
}

/// A client's title and cursor colour as last sent; the defaults are the
/// outer terminal's own (no colour set, no title set).
#[derive(Debug, Default)]
struct Shown {
    cursor: Option<Vec<u8>>,
    title: String,
}

/// Scans a pane's output ahead of the emulator for what `vte` does not
/// parse (OSC 7, APC `_G`). One per pane reader; its state carries over
/// between reads, so a sequence split across reads at any byte is found.
#[derive(Debug, Default)]
pub struct Scanner {
    state: ScanState,
    /// Bytes held back from the emulator until the next byte says whether
    /// they start an APC `_G` or a `DCS tmux;` wrapper: an `ESC`, `ESC _`,
    /// or a prefix of `ESC P tmux;`.
    held: Vec<u8>,
    /// The OSC payload so far (after `ESC ]`), the APC so far (from
    /// `ESC _ G`), or the wrapper's content so far.
    buf: Vec<u8>,
    /// The sequence in `buf` outgrew its bound; it is dropped at its end.
    overlong: bool,
    /// The held `ESC` left an OSC the emulator has seen open: if it starts
    /// an APC `_G`, which the emulator never sees, the OSC must be closed
    /// for it some other way.
    osc_open: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    /// After an `ESC` (held).
    Esc,
    /// After `ESC _` (held).
    ApcIntro,
    /// A prefix of `ESC P tmux;` (held).
    DcsIntro,
    /// Inside a `DCS tmux;` wrapper; its content, un-doubled, goes to
    /// `buf`, never the emulator.
    Dcs,
    /// An `ESC` inside the wrapper: `\` ends it, another `ESC` is one
    /// literal `ESC`.
    DcsEsc,
    /// Inside an APC `_G`; its bytes go to `buf`, never the emulator.
    Apc,
    /// An `ESC` inside an APC `_G`: `\` ends it.
    ApcEsc,
    /// Inside an OSC; its bytes reach the emulator and `buf`.
    Osc,
    /// An `ESC` (held) inside an OSC: `\` ends it.
    OscEsc,
}

impl Scanner {
    /// The bytes the emulator should parse, and the events found: one
    /// `Graphics` per APC `_G` (bytes from `ESC _ G` to `ESC \` inclusive,
    /// none of which reach the emulator) and one `Cwd` per usable OSC 7.
    /// Other sequences, OSC 7 included, pass to the emulator unchanged.
    pub fn scan<'a>(&mut self, bytes: &'a [u8]) -> (Cow<'a, [u8]>, Vec<PaneEventKind>) {
        let mut found = Vec::new();
        if self.state == ScanState::Ground && !bytes.contains(&ESC) {
            return (Cow::Borrowed(bytes), found);
        }
        let mut out = Vec::with_capacity(bytes.len() + self.held.len());
        for &b in bytes {
            self.step(b, &mut out, &mut found);
        }
        (Cow::Owned(out), found)
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>, found: &mut Vec<PaneEventKind>) {
        use ScanState::*;
        match self.state {
            Ground => self.ground(b, out),
            Esc => match b {
                b'_' => {
                    self.held.push(b);
                    self.state = ApcIntro;
                }
                b'P' => {
                    self.held.push(b);
                    self.state = DcsIntro;
                }
                b']' => {
                    self.release(out);
                    out.push(b);
                    self.begin(Osc);
                }
                _ => {
                    self.release(out);
                    self.ground(b, out);
                }
            },
            ApcIntro => {
                if b == b'G' {
                    self.held.clear();
                    if std::mem::take(&mut self.osc_open) {
                        // CAN aborts the OSC in the emulator, which would
                        // otherwise swallow the output after the APC.
                        out.push(CAN);
                    }
                    self.begin(Apc);
                    self.buf.extend_from_slice(b"\x1b_G");
                } else {
                    // Another APC: the emulator drops it.
                    self.release(out);
                    self.ground(b, out);
                }
            }
            DcsIntro => {
                if DCS_TMUX.get(self.held.len()) != Some(&b) {
                    // Another DCS: the emulator's.
                    self.release(out);
                    self.ground(b, out);
                } else if self.held.len() + 1 < DCS_TMUX.len() {
                    self.held.push(b);
                } else {
                    self.held.clear();
                    if std::mem::take(&mut self.osc_open) {
                        out.push(CAN);
                    }
                    self.begin(Dcs);
                }
            }
            // CAN and SUB are content here, as tmux 3.7b keeps them: a
            // wrapper holding one is dropped whole, not cut short.
            Dcs => match b {
                ESC => self.state = DcsEsc,
                _ => self.keep(b, MAX_DCS_TMUX),
            },
            DcsEsc => match b {
                b'\\' => {
                    self.state = Ground;
                    let content = std::mem::take(&mut self.buf);
                    if !self.overlong {
                        unwrap_tmux(&content, out, found);
                    }
                }
                ESC => {
                    self.state = Dcs;
                    self.keep(ESC, MAX_DCS_TMUX);
                }
                // A lone ESC is dropped and the byte after it kept, as
                // tmux 3.7b reads it.
                _ => {
                    self.state = Dcs;
                    self.step(b, out, found);
                }
            },
            Apc => match b {
                ESC => self.state = ApcEsc,
                CAN | SUB => {
                    self.state = Ground;
                    out.push(b);
                }
                _ => self.keep(b, MAX_APC),
            },
            ApcEsc => {
                if b == b'\\' {
                    self.state = Ground;
                    if !self.overlong {
                        let mut apc = std::mem::take(&mut self.buf);
                        apc.extend_from_slice(b"\x1b\\");
                        found.push(PaneEventKind::Graphics(apc));
                    }
                } else {
                    // An ESC ends the string and starts a new sequence;
                    // the unterminated APC is dropped.
                    self.state = Ground;
                    self.ground(ESC, out);
                    self.step(b, out, found);
                }
            }
            Osc => match b {
                BEL => {
                    out.push(b);
                    self.state = Ground;
                    self.osc_end(found);
                }
                ESC => {
                    self.held.push(b);
                    self.state = OscEsc;
                }
                CAN | SUB => {
                    out.push(b);
                    self.state = Ground;
                }
                _ => {
                    out.push(b);
                    self.keep(b, MAX_OSC);
                }
            },
            OscEsc => {
                if b == b'\\' {
                    self.release(out);
                    out.push(b);
                    self.state = Ground;
                    self.osc_end(found);
                } else {
                    // The held ESC starts a new sequence.
                    self.state = Esc;
                    self.osc_open = true;
                    self.step(b, out, found);
                }
            }
        }
    }

    /// `b` read in the ground state (which this enters, or leaves for
    /// `Esc`).
    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        if b == ESC {
            self.held.push(b);
            self.state = ScanState::Esc;
        } else {
            out.push(b);
            self.state = ScanState::Ground;
        }
    }

    /// The held bytes turned out not to start an APC `_G`.
    fn release(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
        self.osc_open = false;
    }

    fn begin(&mut self, state: ScanState) {
        self.state = state;
        self.buf.clear();
        self.overlong = false;
    }

    // ponytail: a sequence past its bound (MAX_APC, MAX_OSC,
    // MAX_DCS_TMUX) is dropped whole rather than streamed; kitty chunks
    // real transmissions far below it.
    fn keep(&mut self, b: u8, max: usize) {
        if self.overlong {
            return;
        }
        if self.buf.len() >= max {
            self.overlong = true;
            self.buf = Vec::new();
        } else {
            self.buf.push(b);
        }
    }

    fn osc_end(&mut self, found: &mut Vec<PaneEventKind>) {
        let payload = std::mem::take(&mut self.buf);
        if self.overlong {
            return;
        }
        if let Some(uri) = payload.strip_prefix(b"7;")
            && let Some(path) = osc7_path(uri)
        {
            found.push(PaneEventKind::Cwd(path));
        } else if relayed(&payload) {
            found.push(PaneEventKind::Osc(payload));
        }
    }
}

/// A wrapper's `content` (un-doubled), handled as if sent bare when it is
/// nothing but complete sequences from the §4.9 set; otherwise dropped.
fn unwrap_tmux(content: &[u8], out: &mut Vec<u8>, found: &mut Vec<PaneEventKind>) {
    let mut items = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        let Some(len) = wrapped_len(rest) else {
            return;
        };
        let (item, tail) = rest.split_at(len);
        items.push(item);
        rest = tail;
    }
    for item in items {
        let (bytes, events) = Scanner::default().scan(item);
        out.extend_from_slice(&bytes);
        found.extend(events);
    }
}

/// The length of the set sequence `s` starts with, if it starts with one:
/// a BEL, an OSC from [`WRAPPED_OSC`] ended by BEL or ST, or an APC `_G`
/// ended by ST. An ESC, CAN or SUB anywhere else inside ends the sequence
/// early, for the emulator as for the scanner, so it is not one.
fn wrapped_len(s: &[u8]) -> Option<usize> {
    if s[0] == BEL {
        return Some(1);
    }
    let osc = s.starts_with(b"\x1b]");
    if !osc && !s.starts_with(b"\x1b_G") {
        return None;
    }
    let body = &s[2..];
    let end = body
        .iter()
        .position(|&b| matches!(b, ESC | CAN | SUB) || (osc && b == BEL))?;
    if osc {
        let num = body[..end].split(|&b| b == b';').next().unwrap_or_default();
        if !WRAPPED_OSC.contains(&num) {
            return None;
        }
        if body[end] == BEL {
            return Some(2 + end + 1);
        }
    }
    (body[end] == ESC && body.get(end + 1) == Some(&b'\\')).then_some(2 + end + 2)
}

/// The directory an OSC 7 `file://host/path` names: percent-decoded, and
/// only for a host that is empty, `localhost` or this machine (a shell on
/// another host over ssh reports a path that is not ours).
fn osc7_path(uri: &[u8]) -> Option<PathBuf> {
    let rest = uri.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&c| c == b'/')?;
    let (host, path) = rest.split_at(slash);
    let host = std::str::from_utf8(host).ok()?;
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost") || is_this_host(host)) {
        return None;
    }
    let path = percent_decode(path);
    if path.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&path)))
}

/// Whether passthrough re-emits this OSC body: a notification (9, 777)
/// or the cursor colour (12, 112), but not a colour query, which the
/// emulator answers.
fn relayed(body: &[u8]) -> bool {
    if body == b"12;?" {
        return false;
    }
    body == b"112"
        || [&b"9;"[..], b"777;", b"12;", b"112;"]
            .iter()
            .any(|p| body.starts_with(p))
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                s.get(i + 1).copied().and_then(hex),
                s.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// Whether `host` is exactly this machine's hostname or its short form
/// (up to the first dot). A longer name that only shares the short form
/// is another machine.
fn is_this_host(host: &str) -> bool {
    let me = hostname();
    let short = me.split('.').next().unwrap_or(me);
    !me.is_empty() && (host.eq_ignore_ascii_case(me) || host.eq_ignore_ascii_case(short))
}

fn hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
        let rc = unsafe { nix::libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return String::new();
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    })
}

/// A `Title`, `Bell`, `Clipboard`, `Cwd` or `Osc` (notification or cursor
/// colour) event from `pane`.
pub fn on_pane_event(state: &mut State, pane: PaneId, kind: PaneEventKind) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    let window = p.window;
    let Some(session) = state.model.windows.get(&window).map(|w| w.session) else {
        return;
    };
    match kind {
        PaneEventKind::Cwd(path) => p.handle.set_reported_cwd(path),
        PaneEventKind::Bell => {
            if let (Some(s), Some(w)) = (
                state.model.sessions.get(&session),
                state.model.windows.get_mut(&window),
            ) && (s.active != window || s.attached == 0)
            {
                w.bell_flag = true;
            }
            send(state, clients_on(state, session), b"\x07");
        }
        PaneEventKind::Clipboard(text)
            if state.model.options.lookup("set-clipboard", &[]) == Some("on") =>
        {
            send(state, clients_on(state, session), &osc52(&text));
        }
        PaneEventKind::Title(title) => {
            state.passthrough.titles.insert(pane, title);
            refresh_session(state, session);
        }
        PaneEventKind::Osc(body) if body.starts_with(b"12;") || body.starts_with(b"112") => {
            if body.starts_with(b"12;") {
                state.passthrough.cursor.insert(pane, body);
            } else {
                state.passthrough.cursor.remove(&pane);
            }
            refresh_session(state, session);
        }
        PaneEventKind::Osc(body) => send(state, clients_on(state, session), &osc(&body)),
        _ => {}
    }
}

fn refresh_session(state: &mut State, session: SessionId) {
    for id in clients_on(state, session) {
        on_active_changed(state, id);
    }
}

/// Client `id` is about to be drawn. Its active pane's title (R15) and
/// cursor colour (R16) are sent if they are not what it was last sent
/// (another pane is active, the pane changed them, or `set-titles`/
/// `set-titles-string` changed): a pane that set no colour resets it with
/// OSC 112. While `set-titles` is off, no title is sent at all, even a
/// clearing one, and nothing already shown is touched. When the session's
/// active pane moved, focus reports follow it ([`move_focus`]).
pub fn on_active_changed(state: &mut State, id: ClientId) {
    let Some(session) = state.clients.get(&id).map(|c| c.session) else {
        return;
    };
    let active = active_pane(state, session);
    let pt = &mut state.passthrough;
    let panes = &state.model.panes;
    let clients = &state.clients;
    pt.cursor.retain(|p, _| panes.contains_key(p));
    pt.titles.retain(|p, _| panes.contains_key(p));
    pt.sent.retain(|c, _| clients.contains_key(c));
    pt.focus
        .retain(|s, _| clients.values().any(|c| c.session == *s));
    let cursor = active.and_then(|p| pt.cursor.get(&p)).cloned();
    let title = active.and_then(|p| outer_title(&state.model, &pt.titles, p));
    let shown = pt.sent.entry(id).or_default();
    let mut out = Vec::new();
    if shown.cursor != cursor {
        out.extend(osc(cursor.as_deref().unwrap_or(b"112")));
        shown.cursor = cursor;
    }
    if let Some(title) = title
        && shown.title != title
    {
        out.extend(osc0(&title));
        shown.title = title;
    }
    if !out.is_empty() {
        clients::queue_raw(state, id, &out);
    }
    if let Some(active) = active {
        move_focus(state, session, active);
    }
}

/// `session`'s active pane is `active`: if it was another pane, that one
/// gets a focus-out report and `active` a focus-in, each only if it asked
/// for them (mode 1004). As tmux 3.7b does, nothing is sent while
/// `focus-events` is off or while no client of the session has focus; the
/// client's own focus-in later reaches whichever pane is active then. The
/// last client leaving is [`on_client_gone`]'s.
// ponytail: tmux keeps a per-pane focused flag, so a terminal that also
// reports focus-in on attach gives the pane only one; grofe may send two.
fn move_focus(state: &mut State, session: SessionId, active: PaneId) {
    let before = state.passthrough.focus.insert(session, active);
    if before == Some(active)
        || state.model.options.lookup("focus-events", &[]) != Some("on")
        || !state
            .clients
            .values()
            .any(|c| c.session == session && c.focused)
    {
        return;
    }
    if let Some(old) = before {
        report_focus(state, old, b"\x1b[O");
    }
    report_focus(state, active, b"\x1b[I");
}

/// Client `id` is about to go (detach, exit, lost connection); it is still
/// in `state.clients`. If it is its session's last client, the pane that
/// has focus gets a focus-out (unless the client had lost focus, which
/// already sent one) and the session forgets it, so the next attach sends
/// focus-in, as tmux 3.7b does.
pub fn on_client_gone(state: &mut State, id: ClientId) {
    state.passthrough.sent.remove(&id);
    let Some((session, focused)) = state.clients.get(&id).map(|c| (c.session, c.focused)) else {
        return;
    };
    if state
        .clients
        .iter()
        .any(|(other, c)| *other != id && c.session == session)
    {
        return;
    }
    let Some(pane) = state.passthrough.focus.remove(&session) else {
        return;
    };
    if focused && state.model.options.lookup("focus-events", &[]) == Some("on") {
        report_focus(state, pane, b"\x1b[O");
    }
}

/// Writes a focus report to `pane` if it asked for them (mode 1004).
fn report_focus(state: &State, pane: PaneId, bytes: &[u8]) {
    if let Some(p) = state.model.panes.get(&pane)
        && p.dead.is_none()
        && p.handle.modes().focus
    {
        let _ = p.handle.enqueue_input(bytes.to_vec());
    }
}

/// The clients attached to `session`.
fn clients_on(state: &State, session: SessionId) -> Vec<ClientId> {
    state
        .clients
        .iter()
        .filter(|(_, c)| c.session == session)
        .map(|(id, _)| *id)
        .collect()
}

/// The active pane of `session`'s current window.
fn active_pane(state: &State, session: SessionId) -> Option<PaneId> {
    let s = state.model.sessions.get(&session)?;
    state.model.windows.get(&s.active).map(|w| w.active)
}

/// `pane`'s outer title (R15): `set-titles-string` expanded against it,
/// with `pane_title` (`#T`) layered on top of the ordinary format
/// variables from `titles` (which `FormatCtx` cannot reach, since it
/// lives in `Passthrough`, not the model). `None` while `set-titles` is
/// off, so the caller sends nothing at all.
fn outer_title(model: &Model, titles: &BTreeMap<PaneId, String>, pane: PaneId) -> Option<String> {
    let chain = options::chain(model, pane);
    if model.options.lookup("set-titles", &chain) != Some("on") {
        return None;
    }
    let fmt = model
        .options
        .lookup("set-titles-string", &chain)
        .unwrap_or_default();
    let title = pane_title(titles, pane);
    let vars = TitleVars {
        base: FormatCtx::new(model, pane),
        title: &title,
    };
    Some(expand(fmt, &vars))
}

/// [`FormatVars`] for `set-titles-string`: `pane_title` (`#T`) from the
/// pane's stored OSC title, everything else from `FormatCtx`.
struct TitleVars<'a> {
    base: FormatCtx<'a>,
    title: &'a str,
}

impl FormatVars for TitleVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        if name == "pane_title" {
            return Some(self.title.to_string());
        }
        self.base.get(name)
    }
}

fn send(state: &mut State, to: Vec<ClientId>, bytes: &[u8]) {
    for id in to {
        clients::queue_raw(state, id, bytes);
    }
}

/// Pane-supplied text made safe to write inside an OSC: decoded as UTF-8
/// (invalid bytes become U+FFFD), with every control character dropped,
/// C1 included (a UTF-8 ST or CSI would otherwise end the OSC and inject
/// a sequence into the outer terminal).
fn clean(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// An OSC with `body` ([`clean`]ed), terminated by BEL.
fn osc(body: &[u8]) -> Vec<u8> {
    format!("\x1b]{}\x07", clean(body)).into_bytes()
}

/// OSC 52 storing `text` in the clipboard, as tmux writes it (`Ms`).
fn osc52(text: &str) -> Vec<u8> {
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{data}\x07").into_bytes()
}

/// OSC 0 setting the outer terminal's title (R15, tmux's own choice:
/// `tsl`/`fsl`), control characters dropped.
fn osc0(title: &str) -> Vec<u8> {
    format!("\x1b]0;{}\x07", clean(title.as_bytes())).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TC.3: one default for `#{pane_title}` and `#T`, the host name,
    /// until the pane sets a title.
    #[test]
    fn a_pane_title_defaults_to_the_host_name() {
        let mut pt = Passthrough::default();
        assert_eq!(pt.title(PaneId(4)), hostname());
        pt.titles.insert(PaneId(4), "mine".to_string());
        assert_eq!(pt.title(PaneId(4)), "mine");
    }
    use proptest::prelude::*;

    const APC: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

    /// Runs `chunks` through one scanner: what the emulator saw, and the
    /// events.
    fn scan_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<PaneEventKind>) {
        let mut s = Scanner::default();
        let mut out = Vec::new();
        let mut events = Vec::new();
        for chunk in chunks {
            let (bytes, found) = s.scan(chunk);
            out.extend_from_slice(&bytes);
            events.extend(found);
        }
        (out, events)
    }

    #[test]
    fn plain_output_is_borrowed_unchanged() {
        let mut s = Scanner::default();
        let (out, events) = s.scan(b"hello\r\n");
        assert!(matches!(out, Cow::Borrowed(b"hello\r\n")));
        assert!(events.is_empty());
    }

    #[test]
    fn other_sequences_reach_the_emulator_unchanged() {
        let input: &[u8] =
            b"a\x1b[1mb\x1b]2;title\x07c\x1b]52;c;aGVsbG8K\x1b\\d\x1b_Xother\x1b\\\x1bPq\x1b\\e";
        let (out, events) = scan_all(&[input]);
        assert_eq!(out, input);
        assert!(events.is_empty());
    }

    /// Case 8: at every split point, one `Graphics` event with the whole
    /// command, and the emulator sees exactly the bytes around it.
    #[test]
    fn an_apc_split_anywhere_is_framed_once_and_hidden() {
        let mut input = b"before\x1b[1m".to_vec();
        input.extend_from_slice(APC);
        input.extend_from_slice(b"after");
        let mut expect = b"before\x1b[1m".to_vec();
        expect.extend_from_slice(b"after");
        for cut in 0..=input.len() {
            let (a, b) = input.split_at(cut);
            let (out, events) = scan_all(&[a, b]);
            assert_eq!(out, expect, "split at {cut}");
            assert_eq!(
                events,
                vec![PaneEventKind::Graphics(APC.to_vec())],
                "split at {cut}"
            );
        }
    }

    #[test]
    fn an_apc_fed_byte_by_byte_is_framed_once() {
        let chunks: Vec<&[u8]> = APC.chunks(1).collect();
        let (out, events) = scan_all(&chunks);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(events, vec![PaneEventKind::Graphics(APC.to_vec())]);
    }

    #[test]
    fn two_apcs_in_one_read_are_two_events() {
        let mut input = APC.to_vec();
        input.extend_from_slice(b"x");
        input.extend_from_slice(APC);
        let (out, events) = scan_all(&[&input]);
        assert_eq!(out, b"x");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_trailing_esc_is_held_until_the_next_read() {
        let mut s = Scanner::default();
        let (out, _) = s.scan(b"ab\x1b");
        assert_eq!(&*out, b"ab");
        let (out, _) = s.scan(b"[1m");
        assert_eq!(&*out, b"\x1b[1m");
    }

    #[test]
    fn an_apc_cut_short_by_another_escape_is_dropped() {
        let (out, events) = scan_all(&[b"\x1b_Ga=q;AAAA\x1b[1mx"]);
        assert_eq!(out, b"\x1b[1mx");
        assert!(events.is_empty());
        let (out, events) = scan_all(&[b"\x1b_Ga=q\x18x"]);
        assert_eq!(out, b"\x18x");
        assert!(events.is_empty());
    }

    #[test]
    fn an_overlong_apc_is_dropped_and_scanning_goes_on() {
        let mut input = b"\x1b_G".to_vec();
        input.resize(MAX_APC + 10, b'A');
        input.extend_from_slice(b"\x1b\\x");
        input.extend_from_slice(APC);
        let (out, events) = scan_all(&[&input]);
        assert_eq!(out, b"x");
        assert_eq!(events, vec![PaneEventKind::Graphics(APC.to_vec())]);
    }

    #[test]
    fn osc_7_gives_the_cwd_and_still_reaches_the_emulator() {
        for input in [
            &b"\x1b]7;file:///tmp/a%20b\x07"[..],
            b"\x1b]7;file://localhost/tmp/a%20b\x1b\\",
            b"\x1b]7;file:///tmp/a b\x07",
        ] {
            let (out, events) = scan_all(&[input]);
            assert_eq!(out, input);
            assert_eq!(events, vec![PaneEventKind::Cwd(PathBuf::from("/tmp/a b"))]);
        }
    }

    #[test]
    fn osc_7_split_anywhere_gives_the_cwd_once() {
        let input: &[u8] = b"x\x1b]7;file://localhost/srv/data\x1b\\y";
        for cut in 0..=input.len() {
            let (a, b) = input.split_at(cut);
            let (out, events) = scan_all(&[a, b]);
            assert_eq!(out, input, "split at {cut}");
            assert_eq!(
                events,
                vec![PaneEventKind::Cwd(PathBuf::from("/srv/data"))],
                "split at {cut}"
            );
        }
    }

    #[test]
    fn osc_7_from_another_host_or_not_a_file_uri_is_ignored() {
        for input in [
            &b"\x1b]7;file://elsewhere.example.com/tmp\x07"[..],
            b"\x1b]7;https://example.com/tmp\x07",
            b"\x1b]7;file://localhost\x07",
        ] {
            let (_, events) = scan_all(&[input]);
            assert!(events.is_empty(), "{input:?}: {events:?}");
        }
    }

    #[test]
    fn osc_7_naming_this_host_is_accepted() {
        let me = hostname();
        if me.is_empty() {
            return;
        }
        let input = format!("\x1b]7;file://{me}/tmp\x07");
        let (_, events) = scan_all(&[input.as_bytes()]);
        assert_eq!(events, vec![PaneEventKind::Cwd(PathBuf::from("/tmp"))]);
    }

    #[test]
    fn an_overlong_osc_is_ignored() {
        let mut input = b"\x1b]7;file:///".to_vec();
        input.resize(MAX_OSC + 10, b'a');
        input.push(BEL);
        let (out, events) = scan_all(&[&input]);
        assert_eq!(out, input);
        assert!(events.is_empty());
    }

    #[test]
    fn percent_decoding_leaves_bad_escapes_alone() {
        assert_eq!(percent_decode(b"/a%2Fb%zz%4"), b"/a/b%zz%4");
    }

    #[test]
    fn relayed_sequences() {
        assert_eq!(osc52("hello\n"), b"\x1b]52;c;aGVsbG8K\x07");
        assert_eq!(osc0("a\x1bb\x07c"), b"\x1b]0;abc\x07");
        assert_eq!(osc(b"9;a\x01b"), b"\x1b]9;ab\x07");
    }

    /// A pane cannot end a relayed OSC early and inject a sequence: C1 ST
    /// and CSI, in UTF-8 or as raw bytes, never reach the outer terminal.
    #[test]
    fn relayed_text_drops_c1_controls() {
        let c1 = |out: &[u8]| {
            out.windows(2).any(|w| w == b"\xc2\x9c" || w == b"\xc2\x9b")
                || out.contains(&0x9c)
                || out.contains(&0x9b)
        };
        for body in [
            &b"9;x\xc2\x9c\xc2\x9b6n"[..],
            b"9;x\x9c\x9b6n",
            b"777;notify;\xc2\x9cT;B",
            b"12;red\xc2\x9b6n",
        ] {
            let out = osc(body);
            assert!(!c1(&out), "{body:?} gave {out:?}");
            assert_eq!(out.iter().filter(|&&b| b == ESC).count(), 1, "{out:?}");
            assert!(out.ends_with(&[BEL]));
        }
        let out = osc0("t\u{9c}\u{9b}6n");
        assert_eq!(out, b"\x1b]0;t6n\x07");
    }

    /// An APC that begins where an OSC's ESC was, closes that OSC for the
    /// emulator with CAN, which it would otherwise never see closed.
    #[test]
    fn an_apc_that_cuts_an_osc_short_closes_it_with_can() {
        let mut input = b"\x1b]2;t".to_vec();
        input.extend_from_slice(APC);
        input.extend_from_slice(b"x");
        for cut in 0..=input.len() {
            let (a, b) = input.split_at(cut);
            let (out, events) = scan_all(&[a, b]);
            assert_eq!(out, b"\x1b]2;t\x18x", "split at {cut}");
            assert_eq!(
                events,
                vec![PaneEventKind::Graphics(APC.to_vec())],
                "split at {cut}"
            );
        }
        // An APC after an OSC that was closed needs no CAN.
        let mut input = b"\x1b]2;t\x07".to_vec();
        input.extend_from_slice(APC);
        let (out, _) = scan_all(&[&input]);
        assert_eq!(out, b"\x1b]2;t\x07");
    }

    #[test]
    fn osc_7_hosts_must_match_exactly() {
        let me = hostname();
        if me.is_empty() {
            return;
        }
        let short = me.split('.').next().unwrap_or(me);
        assert!(is_this_host(me));
        assert!(is_this_host(short));
        assert!(!is_this_host(&format!("{short}.elsewhere.example.com")));
        assert!(!is_this_host(&format!("{me}x")));
    }

    #[test]
    fn osc_7_with_a_nul_in_the_path_is_ignored() {
        let (_, events) = scan_all(&[b"\x1b]7;file:///tmp/a%00b\x07"]);
        assert!(events.is_empty(), "{events:?}");
    }

    /// Notifications and cursor colours become `Osc` with their bare body,
    /// split anywhere; colour queries and other OSCs do not.
    #[test]
    fn notifications_and_cursor_colours_are_found() {
        for (input, body) in [
            (&b"\x1b]9;done\x07"[..], &b"9;done"[..]),
            (b"\x1b]777;notify;T;B\x1b\\", b"777;notify;T;B"),
            (b"\x1b]12;#ff0000\x07", b"12;#ff0000"),
            (b"\x1b]112\x07", b"112"),
        ] {
            for cut in 0..=input.len() {
                let (a, b) = input.split_at(cut);
                let (out, events) = scan_all(&[a, b]);
                assert_eq!(out, input, "split at {cut}");
                assert_eq!(
                    events,
                    vec![PaneEventKind::Osc(body.to_vec())],
                    "split at {cut}"
                );
            }
        }
        for input in [
            &b"\x1b]12;?\x07"[..],
            b"\x1b]2;title\x07",
            b"\x1b]99;x\x07",
            b"\x1b]1122\x07",
        ] {
            let (_, events) = scan_all(&[input]);
            assert!(events.is_empty(), "{input:?}: {events:?}");
        }
    }

    // ---- TC.12: `DCS tmux;` ----

    /// `inner` wrapped as apps wrap it once `$TMUX` is set: every ESC
    /// doubled, inside `ESC P tmux; ... ESC \`.
    fn wrap(inner: &[u8]) -> Vec<u8> {
        let mut out = b"\x1bPtmux;".to_vec();
        for &b in inner {
            if b == ESC {
                out.push(ESC);
            }
            out.push(b);
        }
        out.extend_from_slice(b"\x1b\\");
        out
    }

    /// `mid` between some text and an SGR before it, and text after it.
    fn around(mid: &[u8]) -> Vec<u8> {
        [&b"a\x1b[1m"[..], mid, b"z"].concat()
    }

    /// At every split point, `input` gives `expect_out` to the emulator
    /// and `expect` as events.
    fn assert_every_split(input: &[u8], expect_out: &[u8], expect: &[PaneEventKind]) {
        for cut in 0..=input.len() {
            let (a, b) = input.split_at(cut);
            let (out, events) = scan_all(&[a, b]);
            assert_eq!(out, expect_out, "split at {cut}");
            assert_eq!(events, expect, "split at {cut}");
        }
    }

    /// A wrapped APC `_G` is the same `Graphics` event as the bare one,
    /// split anywhere, a split inside an `ESC ESC` pair included; none of
    /// the wrapper reaches the emulator.
    #[test]
    fn a_wrapped_apc_is_the_bare_apc_split_anywhere() {
        let (bare_out, bare) = scan_all(&[&around(APC)]);
        assert_eq!(bare, vec![PaneEventKind::Graphics(APC.to_vec())]);
        assert_every_split(&around(&wrap(APC)), &bare_out, &bare);
        let chunks: Vec<Vec<u8>> = wrap(APC).chunks(1).map(<[u8]>::to_vec).collect();
        let chunks: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        assert_eq!(scan_all(&chunks), (Vec::new(), bare));
    }

    /// Wrapped sequences from the §4.9 set reach the emulator (and the
    /// scanner's own recognizers) exactly as their bare form does.
    #[test]
    fn wrapped_passthrough_sequences_are_their_bare_form() {
        for inner in [
            &b"\x1b]52;c;aGVsbG8K\x07"[..],
            b"\x1b]52;c;aGVsbG8K\x1b\\",
            b"\x1b]0;title\x07",
            b"\x1b]2;title\x1b\\",
            b"\x1b]8;id=1;https://example.com/\x1b\\",
            b"\x1b]8;;\x1b\\",
            b"\x1b]9;done\x07",
            b"\x1b]777;notify;T;B\x1b\\",
            b"\x07",
            b"\x1b]7;file:///tmp\x07",
            b"\x1b]7;file://elsewhere.example.com/tmp\x07",
            b"\x1b]12;#ff0000\x07",
            b"\x1b]12;?\x07",
            b"\x1b]112\x07",
            // Several in one wrapper.
            b"\x1b]9;one\x07\x1b]9;two\x07\x07",
        ] {
            let (bare_out, bare) = scan_all(&[&around(inner)]);
            assert_eq!(bare_out, around(inner), "{inner:?}");
            assert_every_split(&around(&wrap(inner)), &bare_out, &bare);
        }
    }

    /// Anything outside the set is dropped whole: it reaches neither the
    /// emulator nor the events. So is a wrapper holding a set sequence
    /// next to anything else.
    #[test]
    fn wrapped_sequences_outside_the_set_are_dropped() {
        for inner in [
            &b"LEAK"[..],
            b"\x1b[31mLEAK",
            b"\x1b[?1049h",
            b"\x1bPq#0;2;0;0;0\x1b\\",
            b"\x1b_Xother\x1b\\",
            b"\x1b]1;icon\x07",
            b"\x1b]1122\x07",
            b"\x1b]4;1;rgb:ff/00/00\x07",
            b"\x1b]52;c;aGVsbG8K\x07LEAK",
            b"LEAK\x1b]52;c;aGVsbG8K\x07",
            b"\x1b]52;c;aGVsbG8K\x07\x1b[2J",
            b"\x1b]52;c;aGVsbG8K",
            b"\x1b]52;c;aGVsbG8K\x1b",
            b"\x1b]52;c;aGVsbG8K\x1bX",
            b"\x1b_Ga=q;AAAA",
            b"\x1b_Ga=q;AAAA\x1b[1m",
            b"\x1b]9;x\x1b\\\x1b",
            b"\x18LEAK\x1b[31mX",
            b"\x1b]52;c;aGV\x18sbG8K\x07",
            b"\x1b]9;x\x1a\x07",
            b"",
        ] {
            assert_every_split(&around(&wrap(inner)), b"a\x1b[1mz", &[]);
        }
    }

    /// A wrapper inside a wrapper is not unwrapped twice: dropped.
    #[test]
    fn a_nested_wrapper_is_dropped() {
        let twice = wrap(&wrap(b"\x1b]52;c;aGVsbG8K\x07"));
        assert_every_split(&around(&twice), b"a\x1b[1mz", &[]);
        // Unescaped nesting, as tmux 3.7b reads it: the inner ST ends the
        // outer wrapper, the rest is left over.
        let raw = b"\x1bPtmux;\x1bPtmux;\x1b\x1b]9;x\x07\x1b\\\x1b\\";
        let (out, events) = scan_all(&[&around(raw)]);
        assert_eq!(out, b"a\x1b[1m\x1b\\z");
        assert!(events.is_empty(), "{events:?}");
    }

    /// An odd number of ESCs: `ESC ESC` is one ESC, `ESC \` ends the
    /// wrapper, and a lone ESC before anything else is dropped (tmux 3.7b
    /// keeps the byte after it). Only what then parses as a set sequence
    /// gets out.
    #[test]
    fn odd_esc_counts_inside_a_wrapper() {
        // ESC ESC ESC \: a literal ESC, then the end; `...K ESC` is cut.
        let input = around(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8K\x1b\x1b\x1b\\");
        assert_every_split(&input, b"a\x1b[1mz", &[]);
        // A lone ESC before `]`: the payload no longer starts an OSC.
        let input = around(b"\x1bPtmux;\x1b]52;c;aGVsbG8K\x07\x1b\\");
        assert_every_split(&input, b"a\x1b[1mz", &[]);
        // A lone ESC before `\` inside the payload ends the wrapper early:
        // `9;x` is cut short, the rest is plain output.
        let input = around(b"\x1bPtmux;\x1b\x1b]9;x\x1b\\\x1b\\");
        assert_every_split(&input, b"a\x1b[1m\x1b\\z", &[]);
        // A lone ESC before `Y` is dropped and `Y` kept, so this payload
        // is `ESC ] 9;xY ESC \`: a set sequence, which gets out.
        let input = around(b"\x1bPtmux;\x1b\x1b]9;x\x1bY\x1b\x1b\\\x1b\\");
        assert_every_split(
            &input,
            b"a\x1b[1m\x1b]9;xY\x1b\\z",
            &[PaneEventKind::Osc(b"9;xY".to_vec())],
        );
    }

    /// C1 controls in UTF-8 inside a wrapped notification are what the
    /// bare one carries: the event body is the same, and `osc` drops them
    /// before anything reaches a client.
    #[test]
    fn c1_in_a_wrapped_notification_is_cleaned_like_a_bare_one() {
        let inner: &[u8] = b"\x1b]9;x\xc2\x9c\xc2\x9b6n\x07";
        let (_, bare) = scan_all(&[inner]);
        let (_, wrapped) = scan_all(&[&wrap(inner)]);
        assert_eq!(wrapped, bare);
        let [PaneEventKind::Osc(body)] = &wrapped[..] else {
            panic!("{wrapped:?}");
        };
        assert_eq!(osc(body), b"\x1b]9;x6n\x07");
        // Raw C1 bytes as well.
        let inner: &[u8] = b"\x1b]9;x\x9c\x9b6n\x07";
        let (_, wrapped) = scan_all(&[&wrap(inner)]);
        let [PaneEventKind::Osc(body)] = &wrapped[..] else {
            panic!("{wrapped:?}");
        };
        assert!(!osc(body).iter().any(|&b| b == 0x9c || b == 0x9b));
    }

    /// An unterminated wrapper hides what follows until its ST, like tmux
    /// 3.7b; CAN and SUB are content, as tmux 3.7b keeps them, so they
    /// neither end it nor reach the emulator.
    #[test]
    fn an_unterminated_wrapper_hides_output_until_its_end() {
        let (out, events) = scan_all(&[b"a\x1bPtmux;\x1b\x1b]9;x\x07", b"LEAK", b"more"]);
        assert_eq!(out, b"a");
        assert!(events.is_empty());
        let (out, events) = scan_all(&[b"a\x1bPtmux;\x1b\x1b]9;x\x07", b"LEAK\x1b\\z"]);
        assert_eq!(out, b"az");
        assert!(events.is_empty(), "the wrapper held more than the OSC");
        for c in [CAN, SUB] {
            let input = [&b"a\x1bPtmux;\x1b\x1b]9;x\x07"[..], &[c], b"LEAK"].concat();
            assert_every_split(&input, b"a", &[]);
            let input = [&input[..], b"\x1b\\z"].concat();
            assert_every_split(&input, b"az", &[]);
        }
    }

    /// Fix round 1: a CAN inside a wrapper once cut it short and let the
    /// rest (text, an SGR) reach the emulator.
    #[test]
    fn a_can_inside_a_wrapper_leaks_nothing() {
        for c in [CAN, SUB] {
            let inner = [&[c][..], b"LEAK\x1b[31mX"].concat();
            assert_every_split(&around(&wrap(&inner)), b"a\x1b[1mz", &[]);
        }
    }

    /// A wrapper past tmux 3.7b's bound is dropped whole, and scanning
    /// goes on; one at the bound still unwraps.
    #[test]
    fn an_overlong_wrapper_is_dropped_and_scanning_goes_on() {
        let mut big = b"\x1b_Ga=q;".to_vec();
        big.resize(MAX_DCS_TMUX - 1, b'A');
        big.extend_from_slice(b"\x1b\\");
        let input = [&wrap(&big)[..], b"x", &wrap(APC)].concat();
        let (out, events) = scan_all(&[&input]);
        assert_eq!(out, b"x");
        assert_eq!(events, vec![PaneEventKind::Graphics(APC.to_vec())]);

        let mut fits = b"\x1b_Ga=t;".to_vec();
        fits.resize(MAX_DCS_TMUX - 2, b'A');
        fits.extend_from_slice(b"\x1b\\");
        let (out, events) = scan_all(&[&wrap(&fits)]);
        assert!(out.is_empty());
        assert_eq!(events, vec![PaneEventKind::Graphics(fits)]);
    }

    /// A DCS that is not `tmux;` still reaches the emulator unchanged,
    /// whatever the split.
    #[test]
    fn other_dcs_sequences_reach_the_emulator() {
        for input in [
            &b"a\x1bPq#0\x1b\\z"[..],
            b"a\x1bPtmu\x1b\\z",
            b"a\x1bPtmux\x1b\\z",
            b"a\x1bPtmuxx;\x1b\\z",
            b"a\x1bP1tmux;\x1b\\z",
            b"a\x1bPtmu\x1b_Gi=1;AAAA\x1b\\z",
        ] {
            let expect: Vec<u8> = if input.contains(&b'_') {
                b"a\x1bPtmuz".to_vec()
            } else {
                input.to_vec()
            };
            let events = if input.contains(&b'_') {
                vec![PaneEventKind::Graphics(b"\x1b_Gi=1;AAAA\x1b\\".to_vec())]
            } else {
                vec![]
            };
            assert_every_split(input, &expect, &events);
        }
    }

    /// A wrapper that begins where an OSC's ESC was closes that OSC for
    /// the emulator with CAN, as an APC does.
    #[test]
    fn a_wrapper_that_cuts_an_osc_short_closes_it_with_can() {
        let input = [&b"\x1b]2;t"[..], &wrap(APC), b"x"].concat();
        assert_every_split(
            &input,
            b"\x1b]2;t\x18x",
            &[PaneEventKind::Graphics(APC.to_vec())],
        );
        let input = [&b"\x1b]2;t"[..], &wrap(b"LEAK"), b"x"].concat();
        assert_every_split(&input, b"\x1b]2;t\x18x", &[]);
    }

    proptest! {
        /// Whatever a wrapper holds, split however, the emulator sees the
        /// text around it plus at most set sequences: no CSI, no DCS, no
        /// text from inside, and no OSC outside 0, 2, 7, 8, 9, 12, 52, 112
        /// and 777.
        #[test]
        fn a_wrapper_lets_out_only_set_sequences(
            inner in proptest::collection::vec(
                prop_oneof![
                    Just(ESC), Just(b']'), Just(b'\\'), Just(b'_'), Just(b'G'),
                    Just(b'P'), Just(BEL), Just(b';'), Just(b'5'), Just(b'2'),
                    Just(b'9'), Just(b'['), Just(0x9c), Just(0xc2), Just(CAN),
                    Just(SUB), any::<u8>(),
                ],
                0..200,
            ),
            cuts in proptest::collection::vec(0usize..220, 0..5),
        ) {
            let input = [&b"<"[..], &wrap(&inner), b">"].concat();
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(input.len())).collect();
            cuts.sort();
            let mut chunks = Vec::new();
            let mut at = 0;
            for c in cuts {
                chunks.push(&input[at..c]);
                at = c;
            }
            chunks.push(&input[at..]);
            let (out, events) = scan_all(&chunks);
            prop_assert!(out.starts_with(b"<") && out.ends_with(b">"), "{:?}", out);
            let mid = &out[1..out.len() - 1];
            let mut i = 0;
            while i < mid.len() {
                if mid[i] == BEL {
                    i += 1;
                    continue;
                }
                prop_assert!(mid[i..].starts_with(b"\x1b]"), "{:?} at {}", mid, i);
                let body = &mid[i + 2..];
                let num: Vec<u8> = body
                    .iter()
                    .take_while(|&&b| b != b';' && b != BEL && b != ESC)
                    .copied()
                    .collect();
                prop_assert!(
                    [&b"0"[..], b"2", b"7", b"8", b"9", b"12", b"52", b"112", b"777"]
                        .contains(&&num[..]),
                    "OSC {:?} in {:?}", num, mid
                );
                let end = body.iter().position(|&b| b == BEL || b == ESC);
                prop_assert!(end.is_some(), "{:?}", mid);
                let end = end.unwrap_or(0);
                if body[end] == ESC {
                    prop_assert_eq!(body.get(end + 1), Some(&b'\\'));
                    i += 2 + end + 2;
                } else {
                    i += 2 + end + 1;
                }
            }
            for e in &events {
                match e {
                    PaneEventKind::Graphics(b) => {
                        prop_assert!(b.starts_with(b"\x1b_G") && b.ends_with(b"\x1b\\"));
                    }
                    PaneEventKind::Osc(b) => {
                        prop_assert!(relayed(b));
                    }
                    PaneEventKind::Cwd(_) => {}
                    other => prop_assert!(false, "{:?}", other),
                }
            }
        }
    }

    proptest! {
        /// Hostile bytes never panic, whatever the split, and the
        /// scanner only ever removes bytes: whatever reaches the emulator
        /// plus the framed APCs is no more than was read.
        #[test]
        fn hostile_bytes_never_panic(
            input in proptest::collection::vec(
                prop_oneof![
                    Just(ESC), Just(b'_'), Just(b'G'), Just(b']'), Just(b'\\'),
                    Just(BEL), Just(CAN), Just(b'7'), Just(b';'), any::<u8>(),
                ],
                0..400,
            ),
            cuts in proptest::collection::vec(0usize..400, 0..6),
        ) {
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(input.len())).collect();
            cuts.sort();
            let mut chunks = Vec::new();
            let mut at = 0;
            for c in cuts {
                chunks.push(&input[at..c]);
                at = c;
            }
            chunks.push(&input[at..]);
            let (out, events) = scan_all(&chunks);
            let framed: usize = events
                .iter()
                .map(|e| match e {
                    PaneEventKind::Graphics(b) => b.len(),
                    _ => 0,
                })
                .sum();
            // Each APC that cut an OSC short adds one CAN.
            prop_assert!(out.len() + framed <= input.len() + events.len());
            for e in &events {
                if let PaneEventKind::Graphics(b) = e {
                    prop_assert!(b.starts_with(b"\x1b_G") && b.ends_with(b"\x1b\\"));
                }
            }
        }

        /// Without APCs (no `_` at all), every byte reaches the emulator in
        /// order, once the scanner is given something to release a trailing
        /// ESC.
        #[test]
        fn without_apc_the_emulator_sees_everything(
            input in proptest::collection::vec(any::<u8>(), 0..300),
            cut in 0usize..300,
        ) {
            let text: Vec<u8> = input
                .into_iter()
                .map(|b| if b == b'_' { b'-' } else { b })
                .collect();
            let cut = cut.min(text.len());
            let (a, b) = text.split_at(cut);
            let (out, _) = scan_all(&[a, b, b"x"]);
            let mut expect = text.clone();
            expect.push(b'x');
            prop_assert_eq!(out, expect);
        }
    }
}
