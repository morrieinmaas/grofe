use super::*;

/// Turns client bytes into [`Input`]s, holding back an incomplete sequence
/// until more bytes arrive or `escape-time` passes.
#[derive(Debug, Default)]
pub struct Decoder {
    pub(super) buf: Vec<u8>,
    /// When the bytes now in `buf` started waiting; inside a paste, when
    /// the last paste bytes arrived.
    pub(super) since: Option<Instant>,
    /// Inside a bracketed paste: `buf` is paste content.
    pub(super) in_paste: bool,
    /// Paste bytes at the front of `buf` already searched for the end
    /// marker, so a long paste is scanned once.
    pub(super) paste_scanned: usize,
    /// A piece of the current paste has already been passed on.
    pub(super) paste_begun: bool,
    /// The byte sequences the `user-keys` option names, longest first, each
    /// with its `UserN`: tried before the built-in sequences, as tmux's
    /// key tree takes them.
    pub(super) user_keys: Vec<(Vec<u8>, u16)>,
    /// With [`Decoder::with_spans`]: the bytes of each input drained so far,
    /// in order, and bytes consumed that belong to the next input (a paste
    /// start marker, an ignored report).
    pub(super) spans: Option<Spans>,
}

/// The bytes behind each decoded input ([`Decoder::with_spans`]).
#[derive(Debug, Default)]
pub(super) struct Spans {
    raw: Vec<Vec<u8>>,
    pending: Vec<u8>,
}

pub(super) const PASTE_START: &[u8] = b"\x1b[200~";
pub(super) const PASTE_END: &[u8] = b"\x1b[201~";
/// Paste content held before it is passed on in chunks.
pub(super) const PASTE_CAP: usize = 1 << 20;
/// A paste with no data for this long ends, marker or not.
pub const PASTE_TIMEOUT: Duration = Duration::from_secs(5);

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// A decoder that also tells the bytes each input came from
    /// ([`Decoder::feed_spans`]): for a client that must hold some keys back
    /// and pass others on as they were typed.
    pub fn with_spans() -> Decoder {
        Decoder {
            spans: Some(Spans::default()),
            ..Decoder::default()
        }
    }

    /// As [`Decoder::feed`], each input with the bytes it came from (a
    /// paste's include its markers). Needs [`Decoder::with_spans`].
    pub fn feed_spans(
        &mut self,
        bytes: &[u8],
        now: Instant,
        escape_time: Duration,
    ) -> Vec<(Input, Vec<u8>)> {
        let inputs = self.feed(bytes, now, escape_time);
        self.zip_spans(inputs)
    }

    /// As [`Decoder::flush`], with spans ([`Decoder::feed_spans`]).
    pub fn flush_spans(&mut self, now: Instant, escape_time: Duration) -> Vec<(Input, Vec<u8>)> {
        let inputs = self.flush(now, escape_time);
        self.zip_spans(inputs)
    }

    /// Whether a bracketed paste is open (its end not seen yet).
    pub fn in_paste(&self) -> bool {
        self.in_paste
    }

    /// The bytes received and not yet decoded (a sequence or a paste still
    /// waiting), as they were typed; the decoder starts afresh.
    pub fn take_waiting(&mut self) -> Vec<u8> {
        let mut out = self
            .spans
            .as_mut()
            .map(|s| std::mem::take(&mut s.pending))
            .unwrap_or_default();
        out.append(&mut self.buf);
        let spans = self.spans.take();
        *self = Decoder {
            user_keys: std::mem::take(&mut self.user_keys),
            spans: spans.map(|_| Spans::default()),
            ..Decoder::default()
        };
        out
    }

    fn zip_spans(&mut self, inputs: Vec<Input>) -> Vec<(Input, Vec<u8>)> {
        let raw = self
            .spans
            .as_mut()
            .map(|s| std::mem::take(&mut s.raw))
            .unwrap_or_default();
        let mut raw = raw.into_iter();
        inputs
            .into_iter()
            .map(|i| (i, raw.next().unwrap_or_default()))
            .collect()
    }

    /// Records the bytes behind the input just pushed (with spans on).
    fn span(&mut self, bytes: &[u8]) {
        if let Some(s) = &mut self.spans {
            let mut raw = std::mem::take(&mut s.pending);
            raw.extend_from_slice(bytes);
            s.raw.push(raw);
        }
    }

    /// Records bytes consumed for no input yet (with spans on).
    fn span_pending(&mut self, bytes: &[u8]) {
        if let Some(s) = &mut self.spans {
            s.pending.extend_from_slice(bytes);
        }
    }

    /// Sets the `user-keys` sequences (index and bytes).
    pub fn set_user_keys(&mut self, keys: impl IntoIterator<Item = (u16, Vec<u8>)>) {
        let mut keys: Vec<(Vec<u8>, u16)> = keys
            .into_iter()
            .filter(|(_, bytes)| !bytes.is_empty())
            .map(|(n, bytes)| (bytes, n))
            .collect();
        keys.sort_by_key(|(bytes, _)| std::cmp::Reverse(bytes.len()));
        self.user_keys = keys;
    }

    /// Decode `bytes` received at `now`. Bytes already waiting longer than
    /// `escape_time` are resolved first (a lone ESC becomes Escape).
    pub fn feed(&mut self, bytes: &[u8], now: Instant, escape_time: Duration) -> Vec<Input> {
        let mut out = self.flush(now, escape_time);
        let waiting = self.buf.len();
        self.buf.extend_from_slice(bytes);
        let used = self.drain(false, &mut out);
        if escape_time.is_zero() {
            self.drain(true, &mut out);
        }
        if self.in_paste {
            self.since = Some(now);
        } else if self.buf.is_empty() {
            self.since = None;
        } else if used >= waiting || self.since.is_none() {
            self.since = Some(now);
        }
        out
    }

    /// Resolve waiting bytes if their deadline has passed by `now`: an
    /// incomplete sequence after `escape_time`, an unterminated paste after
    /// 5 s without data. The caller runs this when [`Decoder::deadline`] is
    /// reached.
    pub fn flush(&mut self, now: Instant, escape_time: Duration) -> Vec<Input> {
        let mut out = Vec::new();
        if !self.deadline(escape_time).is_some_and(|d| now >= d) {
            return out;
        }
        if self.in_paste {
            if !self.buf.is_empty() || self.paste_begun {
                let data = std::mem::take(&mut self.buf);
                self.span(&data);
                out.push(self.paste_piece(data, true));
            } else if let Some(s) = &mut self.spans {
                // An empty paste that timed out is no input: its start marker
                // belongs to none.
                s.pending.clear();
            }
            self.in_paste = false;
        } else {
            self.drain(true, &mut out);
        }
        self.since = None;
        out
    }

    /// When waiting bytes must be resolved, if any are waiting.
    pub fn deadline(&self, escape_time: Duration) -> Option<Instant> {
        let wait = if self.in_paste {
            PASTE_TIMEOUT
        } else {
            escape_time
        };
        self.since.map(|t| t + wait)
    }

    /// Decode what `buf` holds; with `force`, an incomplete tail resolves
    /// to keys instead of waiting. Returns the bytes consumed.
    pub(super) fn drain(&mut self, force: bool, out: &mut Vec<Input>) -> usize {
        let mut pos = 0;
        loop {
            if self.in_paste {
                let rest = &self.buf[pos..];
                let from = self.paste_scanned.saturating_sub(PASTE_END.len() - 1);
                if let Some(end) = find(&rest[from..], PASTE_END).map(|i| i + from) {
                    let data = rest[..end].to_vec();
                    let raw = rest[..end + PASTE_END.len()].to_vec();
                    self.span(&raw);
                    out.push(self.paste_piece(data, true));
                    pos += end + PASTE_END.len();
                    self.in_paste = false;
                    continue;
                }
                self.paste_scanned = rest.len();
                if rest.len() > PASTE_CAP {
                    // Pass the content on, keeping only what could be the
                    // start of the end marker.
                    let keep = PASTE_END.len() - 1;
                    let n = rest.len() - keep;
                    let data = rest[..n].to_vec();
                    self.span(&data);
                    out.push(self.paste_piece(data, false));
                    pos += n;
                    self.paste_scanned = keep;
                }
                break;
            }
            if pos == self.buf.len() {
                break;
            }
            let step = match user_key(&self.user_keys, &self.buf[pos..], force) {
                Some(step) => step,
                None => step(&self.buf[pos..], force),
            };
            match step {
                Step::Need => break,
                Step::Done(n, tok) => {
                    if self.spans.is_some() {
                        let raw = self.buf[pos..pos + n].to_vec();
                        match tok {
                            Tok::Input(_) => self.span(&raw),
                            _ => self.span_pending(&raw),
                        }
                    }
                    pos += n;
                    match tok {
                        Tok::Input(i) => out.push(i),
                        Tok::PasteStart => {
                            self.in_paste = true;
                            self.paste_scanned = 0;
                            self.paste_begun = false;
                        }
                        Tok::Ignore => {}
                    }
                }
            }
        }
        self.buf.drain(..pos);
        pos
    }

    /// The next piece of the current paste; `end` if it is the last.
    pub(super) fn paste_piece(&mut self, data: Vec<u8>, end: bool) -> Input {
        let start = !self.paste_begun;
        self.paste_begun = true;
        Input::Paste { data, start, end }
    }
}

pub(super) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub(super) enum Tok {
    Input(Input),
    PasteStart,
    Ignore,
}

pub(super) enum Step {
    /// More bytes are needed to decide.
    Need,
    /// `n` bytes made this token.
    Done(usize, Tok),
}

pub(super) fn key_tok(code: KeyCode, mods: Mods) -> Tok {
    Tok::Input(Input::Key(Key::new(code, mods)))
}

/// A `user-keys` sequence at the front of `buf`: the key if one is all
/// there, `Need` if `buf` could still become one (and may wait), else
/// `None` for the built-in sequences to read.
pub(super) fn user_key(keys: &[(Vec<u8>, u16)], buf: &[u8], force: bool) -> Option<Step> {
    let whole = keys.iter().find(|(bytes, _)| buf.starts_with(bytes));
    let longer = !force
        && keys
            .iter()
            .any(|(bytes, _)| bytes.len() > buf.len() && bytes.starts_with(buf));
    match whole {
        Some((_, _)) if longer => Some(Step::Need),
        Some((bytes, n)) => Some(Step::Done(
            bytes.len(),
            key_tok(KeyCode::User(*n), Mods::default()),
        )),
        None if longer => Some(Step::Need),
        None => None,
    }
}

/// Decode one token from the front of `buf` (non-empty).
pub(super) fn step(buf: &[u8], force: bool) -> Step {
    let b0 = buf[0];
    if b0 != ESC {
        return plain(buf, force);
    }
    let Some(&b1) = buf.get(1) else {
        return if force {
            Step::Done(1, key_tok(KeyCode::Escape, Mods::default()))
        } else {
            Step::Need
        };
    };
    let meta = |c: char| {
        key_tok(
            KeyCode::Char(c),
            Mods {
                alt: true,
                ..Mods::default()
            },
        )
    };
    match b1 {
        b'[' => match csi(buf) {
            Some((n, tok)) => Step::Done(n, tok),
            None if force => Step::Done(2, meta('[')),
            None => Step::Need,
        },
        b'O' => match buf.get(2) {
            Some(&b) => match ss3(b) {
                Some(code) => Step::Done(3, key_tok(code, Mods::default())),
                // Not SS3: M-O, and the next byte is a key of its own.
                None => Step::Done(2, meta('O')),
            },
            None if force => Step::Done(2, meta('O')),
            None => Step::Need,
        },
        _ => match step(&buf[1..], force) {
            Step::Need => Step::Need,
            Step::Done(n, Tok::Input(Input::Key(mut k))) => {
                k.mods.alt = true;
                Step::Done(n + 1, Tok::Input(Input::Key(k)))
            }
            // ESC before a non-key (a paste, a report) is the Escape key.
            Step::Done(..) => Step::Done(1, key_tok(KeyCode::Escape, Mods::default())),
        },
    }
}

/// A byte that does not start an escape sequence: C0 control, ASCII or
/// UTF-8.
pub(super) fn plain(buf: &[u8], force: bool) -> Step {
    let b0 = buf[0];
    let code_mods = match b0 {
        b'\r' => Some((KeyCode::Enter, Mods::default())),
        b'\t' => Some((KeyCode::Tab, Mods::default())),
        0x7f => Some((KeyCode::Backspace, Mods::default())),
        0x00 => Some((KeyCode::Char(' '), ctrl())),
        0x01..=0x1a => Some((KeyCode::Char((b0 - 1 + b'a') as char), ctrl())),
        0x1c..=0x1f => Some((KeyCode::Char((b0 + 0x40) as char), ctrl())),
        0x20..=0x7e => Some((KeyCode::Char(b0 as char), Mods::default())),
        _ => None,
    };
    if let Some((code, mods)) = code_mods {
        return Step::Done(1, key_tok(code, mods));
    }
    let len = match b0 {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Step::Done(1, Tok::Ignore),
    };
    let avail = buf.len().min(len);
    if !buf[1..avail].iter().all(|b| (0x80..=0xbf).contains(b)) {
        // A broken character: drop the lead byte only, so the byte that
        // broke it is decoded as itself.
        return Step::Done(1, Tok::Ignore);
    }
    if buf.len() < len {
        // An incomplete character at the deadline is dropped (the lead and
        // its continuation bytes, all that is there).
        return if force {
            Step::Done(buf.len(), Tok::Ignore)
        } else {
            Step::Need
        };
    }
    match std::str::from_utf8(&buf[..len])
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) => Step::Done(len, key_tok(KeyCode::Char(c), Mods::default())),
        None => Step::Done(1, Tok::Ignore),
    }
}

/// SS3 final bytes: cursor keys, F1-F4, and the keypad in application
/// keypad mode (reported as the keys they stand for).
pub(super) fn ss3(b: u8) -> Option<KeyCode> {
    Some(match b {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'X' => KeyCode::Char('='),
        b'l' => KeyCode::Char(','),
        // The keypad keys, of a terminal in application keypad mode: tmux
        // names them (`KP5`) and a pane that has not asked for them gets
        // the plain key back ([`encode`]).
        b => return Keypad::from_ss3(b).map(KeyCode::Keypad),
    })
}

/// Longest CSI we wait for; anything longer is garbage.
pub(super) const CSI_MAX: usize = 64;

/// A CSI sequence at the front of `buf` (which starts `ESC [`): its length
/// and meaning, or `None` if it is still incomplete.
pub(super) fn csi(buf: &[u8]) -> Option<(usize, Tok)> {
    let mut i = 2;
    while i < buf.len() && (0x30..=0x3f).contains(&buf[i]) {
        i += 1;
    }
    let params_end = i;
    while i < buf.len() && (0x20..=0x2f).contains(&buf[i]) {
        i += 1;
    }
    let Some(&fin) = buf.get(i) else {
        return if buf.len() > CSI_MAX {
            Some((2, Tok::Ignore))
        } else {
            None
        };
    };
    if !(0x40..=0x7e).contains(&fin) {
        // Not a CSI after all: drop the introducer.
        return Some((2, Tok::Ignore));
    }
    let params = &buf[2..params_end];
    let intermediates = params_end != i;
    let tok = if intermediates {
        Tok::Ignore
    } else {
        csi_meaning(params, fin)
    };
    Some((i + 1, tok))
}

/// Parameters as fields separated by `;`, sub-fields by `:`; an empty
/// sub-field is `None`.
pub(super) fn fields(params: &[u8]) -> Vec<Vec<Option<u32>>> {
    let text = std::str::from_utf8(params).unwrap_or("");
    text.split(';')
        .map(|f| f.split(':').map(|s| s.parse().ok()).collect())
        .collect()
}

/// Modifiers from a `1 + bits` parameter (xterm and kitty agree on shift,
/// alt, ctrl, super; hyper, meta and the locks are ignored).
pub(super) fn mods_from(param: Option<u32>) -> Mods {
    let bits = param.unwrap_or(1).saturating_sub(1);
    Mods {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
        super_: bits & 8 != 0,
    }
}

pub(super) fn csi_meaning(params: &[u8], fin: u8) -> Tok {
    if params.first() == Some(&b'<') {
        return match (fin, sgr_mouse(&params[1..], fin == b'm')) {
            (b'M' | b'm', Some(m)) => Tok::Input(Input::Mouse(m)),
            _ => Tok::Ignore,
        };
    }
    if params.first().is_some_and(|b| (0x3c..=0x3f).contains(b)) {
        // Private replies (`CSI ? ... c`, `CSI ? flags u`).
        return Tok::Ignore;
    }
    let f = fields(params);
    if f.get(1).and_then(|sub| sub.get(1).copied().flatten()) == Some(3) {
        // A kitty release event (flag 2), whatever the final byte.
        return Tok::Ignore;
    }
    let field = |n: usize| f.get(n).and_then(|sub| sub.first().copied().flatten());
    let mods = mods_from(f.get(1).and_then(|sub| sub.first().copied().flatten()));
    let letter = |code| key_tok(code, mods);
    match fin {
        b'u' => kitty_key(&f),
        b'A' => letter(KeyCode::Up),
        b'B' => letter(KeyCode::Down),
        b'C' => letter(KeyCode::Right),
        b'D' => letter(KeyCode::Left),
        b'H' => letter(KeyCode::Home),
        b'F' => letter(KeyCode::End),
        b'P' => letter(KeyCode::F(1)),
        b'Q' => letter(KeyCode::F(2)),
        b'S' => letter(KeyCode::F(4)),
        b'Z' => letter(KeyCode::BackTab),
        b'I' if params.is_empty() => Tok::Input(Input::Focus(true)),
        b'O' if params.is_empty() => Tok::Input(Input::Focus(false)),
        b'~' => match field(0) {
            Some(200) => Tok::PasteStart,
            // xterm modifyOtherKeys: CSI 27 ; mods ; code ~
            Some(27) => match field(2) {
                Some(code) => unicode_key(code, None, mods),
                None => Tok::Ignore,
            },
            Some(n) => match tilde_key(n) {
                Some(code) => letter(code),
                None => Tok::Ignore,
            },
            None => Tok::Ignore,
        },
        // `CSI R` is a cursor position report, never F3 (kitty spec).
        _ => Tok::Ignore,
    }
}

pub(super) fn tilde_key(n: u32) -> Option<KeyCode> {
    Some(match n {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::F((n - 10) as u8),
        17..=21 => KeyCode::F((n - 11) as u8),
        23 | 24 => KeyCode::F((n - 12) as u8),
        _ => return None,
    })
}

/// `CSI code[:shifted[:base]] ; mods[:event] ; text u`.
pub(super) fn kitty_key(f: &[Vec<Option<u32>>]) -> Tok {
    let key_field = f.first().map(Vec::as_slice).unwrap_or(&[]);
    let code = key_field.first().copied().flatten().unwrap_or(0);
    let shifted = key_field.get(1).copied().flatten();
    let mod_field = f.get(1).map(Vec::as_slice).unwrap_or(&[]);
    let mods = mods_from(mod_field.first().copied().flatten());
    if code == 0 {
        // A pure text event: the key is the text.
        let text = f.get(2).and_then(|t| t.first().copied().flatten());
        return match text.and_then(char::from_u32) {
            Some(c) => key_tok(KeyCode::Char(c), mods),
            None => Tok::Ignore,
        };
    }
    unicode_key(code, shifted, mods)
}

/// A key by kitty (or modifyOtherKeys) number, with the shifted key if the
/// terminal reported one.
pub(super) fn unicode_key(code: u32, shifted: Option<u32>, mut mods: Mods) -> Tok {
    let kc = match code {
        27 => KeyCode::Escape,
        13 => KeyCode::Enter,
        // Shift+Tab is back-tab, as tmux 3.7b decodes it (kitty `9;2u` and
        // `CSI Z` both fire a BTab binding and send `\e[Z`).
        9 if mods.shift => {
            mods.shift = false;
            KeyCode::BackTab
        }
        9 => KeyCode::Tab,
        127 => KeyCode::Backspace,
        57376..=57398 => KeyCode::F((code - 57376 + 13) as u8),
        57399..=57408 => KeyCode::Char(char::from(b'0' + (code - 57399) as u8)),
        57409 => KeyCode::Char('.'),
        57410 => KeyCode::Char('/'),
        57411 => KeyCode::Char('*'),
        57412 => KeyCode::Char('-'),
        57413 => KeyCode::Char('+'),
        57414 => KeyCode::Enter,
        57415 => KeyCode::Char('='),
        57416 => KeyCode::Char(','),
        57417 => KeyCode::Left,
        57418 => KeyCode::Right,
        57419 => KeyCode::Up,
        57420 => KeyCode::Down,
        57421 => KeyCode::PageUp,
        57422 => KeyCode::PageDown,
        57423 => KeyCode::Home,
        57424 => KeyCode::End,
        57425 => KeyCode::Insert,
        57426 => KeyCode::Delete,
        // Locks, media, modifier keys and the rest of the private use area.
        57344..=63743 => return Tok::Ignore,
        _ => match char::from_u32(code) {
            Some(c) if !c.is_control() => {
                // The shifted key is what the user typed: `S-1` is `!`.
                match shifted.and_then(char::from_u32) {
                    Some(s) if mods.shift => {
                        mods.shift = false;
                        KeyCode::Char(s)
                    }
                    _ => KeyCode::Char(c),
                }
            }
            _ => return Tok::Ignore,
        },
    };
    key_tok(kc, mods)
}

/// `CSI < b ; x ; y M` (press, motion) or `m` (release).
pub(super) fn sgr_mouse(params: &[u8], release: bool) -> Option<Mouse> {
    let f = fields(params);
    let get = |n: usize| f.get(n).and_then(|s| s.first().copied().flatten());
    let (b, x, y) = (get(0)?, get(1)?, get(2)?);
    let mods = Mods {
        shift: b & 4 != 0,
        alt: b & 8 != 0,
        ctrl: b & 16 != 0,
        super_: false,
    };
    let low = b & 3;
    let button = if b & 128 != 0 {
        Some(MouseButton::Other(8 + low as u8))
    } else {
        match low {
            0 => Some(MouseButton::Left),
            1 => Some(MouseButton::Middle),
            2 => Some(MouseButton::Right),
            _ => None,
        }
    };
    let kind = if b & 64 != 0 && b & 128 == 0 {
        match low {
            0 => MouseKind::WheelUp,
            1 => MouseKind::WheelDown,
            2 => MouseKind::WheelLeft,
            _ => MouseKind::WheelRight,
        }
    } else {
        match (b & 32 != 0, button) {
            (true, Some(btn)) => MouseKind::Drag(btn),
            (true, None) => MouseKind::Move,
            (false, Some(btn)) if release => MouseKind::Up(btn),
            (false, Some(btn)) => MouseKind::Down(btn),
            // A release with no button (X10-style): nothing to report.
            (false, None) => return None,
        }
    };
    let cell = |v: u32| u16::try_from(v.saturating_sub(1)).unwrap_or(u16::MAX);
    Some(Mouse {
        kind,
        col: cell(x),
        row: cell(y),
        mods,
    })
}

#[cfg(test)]
mod span_tests {
    use super::*;

    #[test]
    fn each_input_comes_with_the_bytes_it_was_typed_as() {
        let mut d = Decoder::with_spans();
        let now = Instant::now();
        let wait = Duration::from_millis(25);
        let got = d.feed_spans(b"a\x1b[A\x1b[200~x\x1by\x1b[201~\x03\x1b", now, wait);
        let raws: Vec<&[u8]> = got.iter().map(|(_, r)| r.as_slice()).collect();
        assert_eq!(
            raws,
            [&b"a"[..], b"\x1b[A", b"\x1b[200~x\x1by\x1b[201~", b"\x03"]
        );
        // The lone Esc waits; what still waits comes back as typed.
        assert_eq!(d.take_waiting(), b"\x1b");
        // Without spans, nothing is recorded and `feed` is as before.
        let mut plain = Decoder::new();
        assert_eq!(plain.feed(b"a", now, wait).len(), 1);
        assert!(plain.spans.is_none());
    }

    #[test]
    fn a_paste_start_that_times_out_empty_is_glued_to_no_key() {
        let mut d = Decoder::with_spans();
        let now = Instant::now();
        let wait = Duration::from_millis(25);
        assert!(d.feed_spans(b"\x1b[200~", now, wait).is_empty());
        assert!(d.flush_spans(now + PASTE_TIMEOUT, wait).is_empty());
        let got = d.feed_spans(b"\x02", now + PASTE_TIMEOUT, wait);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"\x02");
    }
}
