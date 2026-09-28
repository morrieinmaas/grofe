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
//! - OSC 7, read by the [`Scanner`] into `PaneEventKind::Cwd`.
//!
//! The [`Scanner`] runs over each pane's output ahead of the emulator,
//! which parses neither OSC 7/9/777 nor APC. It also frames kitty graphics
//! commands (APC `_G`) into `PaneEventKind::Graphics` for T3.8, and those
//! bytes never reach the emulator.

use std::borrow::Cow;
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

/// What the server keeps for passthrough. Nothing yet: every item is
/// relayed as it arrives.
#[derive(Debug, Default)]
pub struct Passthrough {}

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
    Some(PathBuf::from(OsStr::from_bytes(&percent_decode(path))))
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

/// Whether `host` names this machine, by its full or short hostname.
fn is_this_host(host: &str) -> bool {
    let me = hostname();
    let short = |h: &str| h.split('.').next().unwrap_or(h).to_string();
    !me.is_empty()
        && (host.eq_ignore_ascii_case(me)
            || host.eq_ignore_ascii_case(&short(me))
            || me.eq_ignore_ascii_case(&short(host)))
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

/// A `Title`, `Bell`, `Clipboard` or `Cwd` event from `pane`.
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
        // Every client on a session shows its current window, so they all
        // have the same active pane.
        PaneEventKind::Title(title) if active_pane(state, session) == Some(pane) => {
            send(state, clients_on(state, session), &osc2(&title));
        }
        _ => {}
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

fn send(state: &mut State, to: Vec<ClientId>, bytes: &[u8]) {
    for id in to {
        clients::queue_raw(state, id, bytes);
    }
}

/// OSC 52 storing `text` in the clipboard, as tmux writes it (`Ms`).
fn osc52(text: &str) -> Vec<u8> {
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{data}\x07").into_bytes()
}

/// OSC 2 setting the title, control characters dropped.
fn osc2(title: &str) -> Vec<u8> {
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{title}\x07").into_bytes()
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
            prop_assert!(out.len() + framed <= input.len());
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
