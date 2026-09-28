//! Passthrough (T3.3, spec §4.9): what escapes the multiplexer to the
//! attached clients. The emulator consumes every byte, so each item is
//! re-emitted on purpose:
//!
//! - OSC 52 clipboard stores (the emulator decodes them into
//!   `PaneEventKind::Clipboard`), to the session's clients while
//!   `set-clipboard` is `on` (R14); reads are never forwarded;
//! - the title (OSC 0/2), to the clients whose active pane set it (R15);
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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use base64::Engine as _;

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
    /// they start an APC `_G`: an `ESC`, or `ESC _`.
    held: Vec<u8>,
    /// The OSC payload so far (after `ESC ]`), or the APC so far (from
    /// `ESC _ G`).
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

    // ponytail: a sequence past its bound (MAX_APC, MAX_OSC) is dropped
    // whole rather than streamed; kitty chunks real transmissions far
    // below it.
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
/// (another pane is active, or the pane changed them): a pane that set no
/// colour resets it with OSC 112, one that set no title clears it. When
/// the session's active pane moved, focus reports follow it
/// ([`move_focus`]).
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
    let title = active
        .and_then(|p| pt.titles.get(&p))
        .cloned()
        .unwrap_or_default();
    let shown = pt.sent.entry(id).or_default();
    let mut out = Vec::new();
    if shown.cursor != cursor {
        out.extend(osc(cursor.as_deref().unwrap_or(b"112")));
        shown.cursor = cursor;
    }
    if shown.title != title {
        out.extend(osc2(&title));
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
/// client's own focus-in later reaches whichever pane is active then. A
/// session with no client forgets its pane, so an attach sends focus-in.
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
    let report = |pane: PaneId, bytes: &[u8]| {
        if let Some(p) = state.model.panes.get(&pane)
            && p.dead.is_none()
            && p.handle.modes().focus
        {
            let _ = p.handle.enqueue_input(bytes.to_vec());
        }
    };
    if let Some(old) = before {
        report(old, b"\x1b[O");
    }
    report(active, b"\x1b[I");
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

/// OSC 2 setting the title, control characters dropped.
fn osc2(title: &str) -> Vec<u8> {
    format!("\x1b]2;{}\x07", clean(title.as_bytes())).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(osc2("a\x1bb\x07c"), b"\x1b]2;abc\x07");
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
        let out = osc2("t\u{9c}\u{9b}6n");
        assert_eq!(out, b"\x1b]2;t6n\x07");
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
