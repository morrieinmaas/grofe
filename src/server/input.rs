//! Client input: decode the bytes an attached client sends, route keys
//! through the key tables, and encode keys for a pane's `InputModes`
//! (spec §4.7).
//!
//! The pieces, in the order T2.5a wires them:
//!
//! - [`Decoder`] turns client bytes into [`Input`]s: keys (legacy CSI and
//!   SS3 forms, kitty `CSI u`, xterm modifyOtherKeys, UTF-8, C0 controls),
//!   SGR mouse reports, focus reports, and bracketed paste as one opaque
//!   blob. A lone ESC waits `escape-time` for the rest of a sequence; the
//!   clock is passed in, never read here.
//! - [`KeyTables`] holds bindings by table name (`root`, `prefix`, custom).
//! - [`KeyState`] is one client's position in the tables (current table,
//!   repeat deadline) and routes an [`Input`] to a [`Routed`], following
//!   tmux 3.7b's `server_client_key_callback`: the prefix switches to the
//!   `prefix` table, a binding runs and returns to `root` unless it repeats,
//!   an unbound key after the prefix is dropped, and an unbound key in
//!   `root` goes to the pane.
//! - [`encode`] writes a key the way a terminal would for the pane's modes:
//!   legacy xterm/screen bytes (the pane's `TERM` is screen-256color), or
//!   the kitty keyboard protocol for the flags the pane has pushed.
//!
//! [`InputHook`]s (M3 overlays) see keys before the tables; running them is
//! the caller's job, since the registry lives in T2.5a's clients.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::{Duration, Instant};

use crate::types::{ClientId, InputModes};

/// A decoded key press. T2.3 may add [`KeyCode`] variants; the field names
/// are the contract.
///
/// Keys are kept in one canonical form (see [`Key::normalized`]) so a
/// binding matches however the outer terminal spelled the key: an ASCII
/// letter typed with shift is `Char('A')` without `shift`, and back-tab is
/// `Tab` with `shift`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

/// The key itself, without modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// `F(1)` is F1.
    F(u8),
}

/// Modifiers held with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_: bool,
}

/// What a key turned into after routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// A bound command's argv, for dispatch.
    Command(Vec<String>),
    /// Bytes for the active pane, encoded for its `InputModes`.
    ToPane(Vec<u8>),
    /// Eaten (a prefix key, an overlay's key).
    Consumed,
}

/// A hook that sees keys before the key tables (M3 overlays and prompts
/// register one through T2.5a's API). `None` passes the key on.
pub trait InputHook {
    fn on_key(&mut self, key: &Key, client: ClientId) -> Option<Routed>;
}

/// One decoded unit of client input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    /// Bracketed-paste content, without the markers. Never matched against
    /// bindings.
    Paste(Vec<u8>),
    Mouse(Mouse),
    /// Focus in (`true`) or out, from the outer terminal's mode 1004.
    Focus(bool),
}

/// A decoded SGR (mode 1006) mouse report. Routing it is M3's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    /// 0-based client cell.
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down(MouseButton),
    Up(MouseButton),
    /// Motion with a button held.
    Drag(MouseButton),
    /// Motion with no button held (mode 1003).
    Move,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Buttons 8 to 11 (back, forward, ...).
    Other(u8),
}

impl Key {
    pub fn new(code: KeyCode, mods: Mods) -> Key {
        Key { code, mods }.normalized()
    }

    /// A key with no modifiers.
    pub fn plain(code: KeyCode) -> Key {
        Key::new(code, Mods::default())
    }

    /// The canonical form: shift is folded into an ASCII letter
    /// (`S-a` is `A`), and dropped from an uppercase one.
    // ponytail: ASCII letters only; other scripts keep an explicit shift.
    pub fn normalized(mut self) -> Key {
        if let KeyCode::Char(c) = self.code {
            if c.is_ascii_lowercase() && self.mods.shift {
                self.code = KeyCode::Char(c.to_ascii_uppercase());
                self.mods.shift = false;
            } else if c.is_ascii_uppercase() {
                self.mods.shift = false;
            }
        }
        self
    }
}

const ESC: u8 = 0x1b;

fn ctrl() -> Mods {
    Mods {
        ctrl: true,
        ..Mods::default()
    }
}

/// Named keys, tmux's canonical spelling first, then accepted aliases
/// (tmux 3.7b's `key_string_table`, matched case-insensitively).
const NAMES: &[(&str, KeyCode)] = &[
    ("Enter", KeyCode::Enter),
    ("Escape", KeyCode::Escape),
    ("Space", KeyCode::Char(' ')),
    ("Tab", KeyCode::Tab),
    ("BSpace", KeyCode::Backspace),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PPage", KeyCode::PageUp),
    ("PageUp", KeyCode::PageUp),
    ("PgUp", KeyCode::PageUp),
    ("NPage", KeyCode::PageDown),
    ("PageDown", KeyCode::PageDown),
    ("PgDn", KeyCode::PageDown),
    ("IC", KeyCode::Insert),
    ("Insert", KeyCode::Insert),
    ("DC", KeyCode::Delete),
    ("Delete", KeyCode::Delete),
];

/// Parse a tmux key name (`C-b`, `M-Up`, `^a`, `PPage`, `%`), as tmux 3.7b
/// accepts it: modifier prefixes `C-`, `M-`, `S-` in any case and order,
/// named keys case-insensitively. The error is tmux's `bad key: X`.
// ponytail: tmux's keypad names (KP5, KPEnter) and mouse keys are not
// parsed; KeyCode has no keypad variants and mouse binding is M3.
pub fn parse_key(name: &str) -> Result<Key, String> {
    let bad = || format!("bad key: {name}");
    let mut mods = Mods::default();
    let mut rest = name;
    loop {
        let b = rest.as_bytes();
        if b.len() > 1 && b[0] == b'^' {
            mods.ctrl = true;
            rest = &rest[1..];
        } else if b.len() > 2 && b[1] == b'-' {
            match b[0].to_ascii_uppercase() {
                b'C' => mods.ctrl = true,
                b'M' => mods.alt = true,
                b'S' => mods.shift = true,
                _ => break,
            }
            rest = &rest[2..];
        } else {
            break;
        }
    }
    let code = if rest.eq_ignore_ascii_case("BTab") {
        mods.shift = true;
        KeyCode::Tab
    } else if let Some(&(_, code)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(rest)) {
        code
    } else if let Some(n) = function_key_number(rest) {
        KeyCode::F(n)
    } else {
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => KeyCode::Char(c),
            _ => return Err(bad()),
        }
    };
    Ok(Key::new(code, mods))
}

/// `F1` to `F12` (tmux has no F13 and up).
fn function_key_number(s: &str) -> Option<u8> {
    let digits = s.strip_prefix(['F', 'f'])?;
    if digits.starts_with('0') {
        return None;
    }
    let n: u8 = digits.parse().ok()?;
    (1..=12).contains(&n).then_some(n)
}

/// Parse the `prefix` option: a key name or `None` (no prefix key).
pub fn parse_prefix(value: &str) -> Result<Option<Key>, String> {
    if value.eq_ignore_ascii_case("None") {
        Ok(None)
    } else {
        parse_key(value).map(Some)
    }
}

impl fmt::Display for Key {
    /// tmux's canonical name (`C-M-x`, `BTab`, `PPage`, `IC`, `Space`).
    // ponytail: tmux has no super modifier; it is not shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let back_tab = self.code == KeyCode::Tab && self.mods.shift;
        if self.mods.ctrl {
            f.write_str("C-")?;
        }
        if self.mods.alt {
            f.write_str("M-")?;
        }
        if self.mods.shift && !back_tab {
            f.write_str("S-")?;
        }
        match self.code {
            KeyCode::Tab if back_tab => f.write_str("BTab"),
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::F(n) => write!(f, "F{n}"),
            code => {
                let name = NAMES
                    .iter()
                    .find(|(_, c)| *c == code)
                    .map_or("", |(n, _)| n);
                f.write_str(name)
            }
        }
    }
}

/// Turns client bytes into [`Input`]s, holding back an incomplete sequence
/// until more bytes arrive or `escape-time` passes.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// When the bytes now in `buf` started waiting.
    since: Option<Instant>,
    /// Inside a bracketed paste: `buf` is paste content.
    in_paste: bool,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
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
        if self.buf.is_empty() || self.in_paste {
            self.since = None;
        } else if used >= waiting || self.since.is_none() {
            self.since = Some(now);
        }
        out
    }

    /// Resolve waiting bytes if `escape_time` has passed by `now`. The
    /// caller runs this when [`Decoder::deadline`] is reached.
    pub fn flush(&mut self, now: Instant, escape_time: Duration) -> Vec<Input> {
        let mut out = Vec::new();
        if self.deadline(escape_time).is_some_and(|d| now >= d) {
            self.drain(true, &mut out);
            self.since = None;
        }
        out
    }

    /// When waiting bytes must be resolved, if any are waiting. A paste
    /// waits for its end marker and has no deadline.
    // ponytail: a paste whose end marker never comes is held until it does.
    pub fn deadline(&self, escape_time: Duration) -> Option<Instant> {
        if self.in_paste {
            return None;
        }
        self.since.map(|t| t + escape_time)
    }

    /// Decode what `buf` holds; with `force`, an incomplete tail resolves
    /// to keys instead of waiting. Returns the bytes consumed.
    fn drain(&mut self, force: bool, out: &mut Vec<Input>) -> usize {
        let mut pos = 0;
        loop {
            if self.in_paste {
                let rest = &self.buf[pos..];
                let Some(end) = find(rest, PASTE_END) else {
                    break;
                };
                out.push(Input::Paste(rest[..end].to_vec()));
                pos += end + PASTE_END.len();
                self.in_paste = false;
                continue;
            }
            if pos == self.buf.len() {
                break;
            }
            match step(&self.buf[pos..], force) {
                Step::Need => break,
                Step::Done(n, tok) => {
                    pos += n;
                    match tok {
                        Tok::Input(i) => out.push(i),
                        Tok::PasteStart => self.in_paste = true,
                        Tok::Ignore => {}
                    }
                }
            }
        }
        self.buf.drain(..pos);
        pos
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

enum Tok {
    Input(Input),
    PasteStart,
    Ignore,
}

enum Step {
    /// More bytes are needed to decide.
    Need,
    /// `n` bytes made this token.
    Done(usize, Tok),
}

fn key_tok(code: KeyCode, mods: Mods) -> Tok {
    Tok::Input(Input::Key(Key::new(code, mods)))
}

/// Decode one token from the front of `buf` (non-empty).
fn step(buf: &[u8], force: bool) -> Step {
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
                None => Step::Done(3, Tok::Ignore),
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
fn plain(buf: &[u8], force: bool) -> Step {
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
    if buf.len() < len {
        // An incomplete character at the deadline is dropped.
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
fn ss3(b: u8) -> Option<KeyCode> {
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
        b'M' => KeyCode::Enter,
        b'X' => KeyCode::Char('='),
        b'j' => KeyCode::Char('*'),
        b'k' => KeyCode::Char('+'),
        b'l' => KeyCode::Char(','),
        b'm' => KeyCode::Char('-'),
        b'n' => KeyCode::Char('.'),
        b'o' => KeyCode::Char('/'),
        b'p'..=b'y' => KeyCode::Char((b - b'p' + b'0') as char),
        _ => return None,
    })
}

/// Longest CSI we wait for; anything longer is garbage.
const CSI_MAX: usize = 64;

/// A CSI sequence at the front of `buf` (which starts `ESC [`): its length
/// and meaning, or `None` if it is still incomplete.
fn csi(buf: &[u8]) -> Option<(usize, Tok)> {
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
fn fields(params: &[u8]) -> Vec<Vec<Option<u32>>> {
    let text = std::str::from_utf8(params).unwrap_or("");
    text.split(';')
        .map(|f| f.split(':').map(|s| s.parse().ok()).collect())
        .collect()
}

/// Modifiers from a `1 + bits` parameter (xterm and kitty agree on shift,
/// alt, ctrl, super; hyper, meta and the locks are ignored).
fn mods_from(param: Option<u32>) -> Mods {
    let bits = param.unwrap_or(1).saturating_sub(1);
    Mods {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
        super_: bits & 8 != 0,
    }
}

fn csi_meaning(params: &[u8], fin: u8) -> Tok {
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
        b'Z' => key_tok(
            KeyCode::Tab,
            Mods {
                shift: true,
                ..mods
            },
        ),
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

fn tilde_key(n: u32) -> Option<KeyCode> {
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
fn kitty_key(f: &[Vec<Option<u32>>]) -> Tok {
    let key_field = f.first().map(Vec::as_slice).unwrap_or(&[]);
    let code = key_field.first().copied().flatten().unwrap_or(0);
    let shifted = key_field.get(1).copied().flatten();
    let mod_field = f.get(1).map(Vec::as_slice).unwrap_or(&[]);
    let mods = mods_from(mod_field.first().copied().flatten());
    if mod_field.get(1).copied().flatten() == Some(3) {
        return Tok::Ignore; // release
    }
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
fn unicode_key(code: u32, shifted: Option<u32>, mut mods: Mods) -> Tok {
    let kc = match code {
        27 => KeyCode::Escape,
        13 => KeyCode::Enter,
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
fn sgr_mouse(params: &[u8], release: bool) -> Option<Mouse> {
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

pub const ROOT_TABLE: &str = "root";
pub const PREFIX_TABLE: &str = "prefix";

/// A key binding: the command to run and whether it repeats (`bind -r`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub argv: Vec<String>,
    pub repeat: bool,
}

/// Bindings by table name, then key.
#[derive(Debug, Clone, Default)]
pub struct KeyTables {
    tables: BTreeMap<String, HashMap<Key, Binding>>,
}

impl KeyTables {
    pub fn bind(&mut self, table: &str, key: Key, argv: Vec<String>, repeat: bool) {
        self.tables
            .entry(table.to_string())
            .or_default()
            .insert(key.normalized(), Binding { argv, repeat });
    }

    /// Remove a binding; whether one was there.
    pub fn unbind(&mut self, table: &str, key: &Key) -> bool {
        self.tables
            .get_mut(table)
            .is_some_and(|t| t.remove(&key.normalized()).is_some())
    }

    pub fn get(&self, table: &str, key: &Key) -> Option<&Binding> {
        self.tables.get(table)?.get(&key.normalized())
    }
}

/// The session options routing depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySettings {
    /// `prefix`; `None` is tmux's `None` (no prefix key).
    pub prefix: Option<Key>,
    /// `repeat-time`; zero turns repeat off.
    pub repeat_time: Duration,
}

impl Default for KeySettings {
    fn default() -> KeySettings {
        KeySettings {
            prefix: Some(Key::new(KeyCode::Char('b'), ctrl())),
            repeat_time: Duration::from_millis(500),
        }
    }
}

/// One client's place in the key tables.
#[derive(Debug, Clone)]
pub struct KeyState {
    table: String,
    /// Set while a repeating binding's `repeat-time` runs.
    repeat_until: Option<Instant>,
}

impl Default for KeyState {
    fn default() -> KeyState {
        KeyState {
            table: ROOT_TABLE.to_string(),
            repeat_until: None,
        }
    }
}

impl KeyState {
    pub fn new() -> KeyState {
        KeyState::default()
    }

    /// The current table (`client_key_table`).
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Switch table (`switch-client -T`), ending any repeat.
    pub fn set_table(&mut self, name: &str) {
        self.table = name.to_string();
        self.repeat_until = None;
    }

    /// When the running repeat ends, if one is running.
    pub fn deadline(&self) -> Option<Instant> {
        self.repeat_until
    }

    /// End an expired repeat; whether the table changed (status redraw).
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.repeat_until.is_some_and(|d| now >= d) {
            self.set_table(ROOT_TABLE);
            true
        } else {
            false
        }
    }

    /// Route one decoded input for a client whose active pane has `modes`.
    pub fn route(
        &mut self,
        input: &Input,
        now: Instant,
        tables: &KeyTables,
        settings: &KeySettings,
        modes: &InputModes,
    ) -> Routed {
        match input {
            Input::Key(key) => self.route_key(key, now, tables, settings, modes),
            Input::Paste(data) => Routed::ToPane(encode_paste(data, modes)),
            // ponytail: mouse routing (bindable mouse keys, pane forwarding)
            // is M3's (TC.10); M2 decodes and drops it.
            Input::Mouse(_) => Routed::Consumed,
            Input::Focus(_) if !modes.focus => Routed::Consumed,
            Input::Focus(true) => Routed::ToPane(b"\x1b[I".to_vec()),
            Input::Focus(false) => Routed::ToPane(b"\x1b[O".to_vec()),
        }
    }

    /// tmux 3.7b's `server_client_key_callback` table walk.
    fn route_key(
        &mut self,
        key: &Key,
        now: Instant,
        tables: &KeyTables,
        settings: &KeySettings,
        modes: &InputModes,
    ) -> Routed {
        let key = key.normalized();
        self.expire(now);
        let mut repeating = self.repeat_until.is_some();
        let mut first = self.table.clone();
        loop {
            // The prefix always wins, unless already in the prefix table.
            if settings.prefix == Some(key) && self.table != PREFIX_TABLE {
                self.set_table(PREFIX_TABLE);
                return Routed::Consumed;
            }
            if let Some(b) = tables.get(&self.table, &key) {
                if repeating && !b.repeat {
                    // A non-repeating key ends the repeat and is tried
                    // again from root, as if typed fresh.
                    self.set_table(ROOT_TABLE);
                    repeating = false;
                    first = ROOT_TABLE.to_string();
                    continue;
                }
                if b.repeat && !settings.repeat_time.is_zero() {
                    self.repeat_until = Some(now + settings.repeat_time);
                } else {
                    self.set_table(ROOT_TABLE);
                }
                return Routed::Command(b.argv.clone());
            }
            if self.table != ROOT_TABLE || repeating {
                self.set_table(ROOT_TABLE);
                if repeating {
                    first = ROOT_TABLE.to_string();
                }
                repeating = false;
                continue;
            }
            // Unbound after a prefix or in a custom table: dropped.
            if first != ROOT_TABLE {
                return Routed::Consumed;
            }
            let bytes = encode(&key, modes);
            return if bytes.is_empty() {
                Routed::Consumed
            } else {
                Routed::ToPane(bytes)
            };
        }
    }
}

/// The bytes a terminal would send for `key` to a program with `modes`.
/// Empty when the key has no encoding there.
pub fn encode(key: &Key, modes: &InputModes) -> Vec<u8> {
    let key = key.normalized();
    let flags = modes.kitty_flags;
    if flags & (1 | 8) != 0 {
        kitty_encode(&key, modes, flags)
    } else {
        legacy_encode(&key, modes)
    }
}

/// xterm's modifier parameter (`1 + shift + 2 alt + 4 ctrl`), `None` when
/// unmodified. Legacy forms have no super.
fn xterm_mod(m: Mods) -> Option<u8> {
    let bits = (m.shift as u8) | ((m.alt as u8) << 1) | ((m.ctrl as u8) << 2);
    (bits != 0).then_some(1 + bits)
}

/// The kitty modifier parameter, `1` when unmodified.
fn kitty_mod(m: Mods) -> u8 {
    1 + ((m.shift as u8) | ((m.alt as u8) << 1) | ((m.ctrl as u8) << 2) | ((m.super_ as u8) << 3))
}

/// How a functional key is spelled: `CSI 1;m X` / `CSI X` (`letter`), or
/// `CSI n;m ~` (`tilde`).
enum Functional {
    Letter(u8),
    Tilde(u8),
}

/// A functional key's modified (CSI) form. F1-F4 use letters as xterm does,
/// except kitty's F3, which is `13 ~` because `CSI R` is a cursor report.
fn functional(code: KeyCode, kitty: bool) -> Option<Functional> {
    use Functional::{Letter, Tilde};
    Some(match code {
        KeyCode::Up => Letter(b'A'),
        KeyCode::Down => Letter(b'B'),
        KeyCode::Right => Letter(b'C'),
        KeyCode::Left => Letter(b'D'),
        KeyCode::Home => Letter(b'H'),
        KeyCode::End => Letter(b'F'),
        KeyCode::Insert => Tilde(2),
        KeyCode::Delete => Tilde(3),
        KeyCode::PageUp => Tilde(5),
        KeyCode::PageDown => Tilde(6),
        KeyCode::F(1) => Letter(b'P'),
        KeyCode::F(2) => Letter(b'Q'),
        KeyCode::F(3) if kitty => Tilde(13),
        KeyCode::F(3) => Letter(b'R'),
        KeyCode::F(4) => Letter(b'S'),
        KeyCode::F(5) => Tilde(15),
        KeyCode::F(n @ 6..=10) => Tilde(n + 11),
        KeyCode::F(n @ 11..=12) => Tilde(n + 12),
        _ => return None,
    })
}

fn functional_bytes(f: Functional, param: Option<u8>) -> Vec<u8> {
    match (f, param) {
        (Functional::Letter(l), None) => vec![ESC, b'[', l],
        (Functional::Letter(l), Some(m)) => format!("\x1b[1;{m}{}", l as char).into_bytes(),
        (Functional::Tilde(n), None) => format!("\x1b[{n}~").into_bytes(),
        (Functional::Tilde(n), Some(m)) => format!("\x1b[{n};{m}~").into_bytes(),
    }
}

/// Legacy C0 mapping of ctrl with an ASCII key (kitty spec table, which
/// matches tmux 3.7b: `C-/` is 0x1f, `C-?` 0x7f, `C-1` stays `1`).
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        ' ' | '@' | '2' => 0,
        'a'..='z' | 'A'..='Z' => (c as u8) & 0x1f,
        '[' | '3' => 27,
        '\\' | '4' => 28,
        ']' | '5' => 29,
        '^' | '6' | '~' => 30,
        '_' | '/' | '7' => 31,
        '8' | '?' => 127,
        _ => return None,
    })
}

fn utf8(c: char) -> Vec<u8> {
    let mut b = [0u8; 4];
    c.encode_utf8(&mut b).as_bytes().to_vec()
}

/// What screen-256color panes expect, as tmux 3.7b sends it: modifiers a
/// legacy form cannot carry are dropped (`C-Enter` is `\r`).
// ponytail: no modifyOtherKeys or tmux `extended-keys` encodings; a pane
// that wants richer keys pushes kitty flags.
fn legacy_encode(key: &Key, modes: &InputModes) -> Vec<u8> {
    let m = key.mods;
    let mut out = if m.alt { vec![ESC] } else { Vec::new() };
    match key.code {
        KeyCode::Char(c) => match m.ctrl.then(|| ctrl_byte(c)).flatten() {
            Some(b) => out.push(b),
            None => out.extend(utf8(c)),
        },
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Escape => out.push(ESC),
        KeyCode::Backspace => out.push(if m.ctrl { 0x08 } else { 0x7f }),
        // tmux sends back-tab without the meta prefix.
        KeyCode::Tab if m.shift => return b"\x1b[Z".to_vec(),
        KeyCode::Tab => out.push(b'\t'),
        code => {
            let Some(f) = functional(code, false) else {
                return Vec::new();
            };
            let param = xterm_mod(m);
            return match (param, code) {
                (None, KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left)
                    if modes.app_cursor =>
                {
                    let Functional::Letter(l) = f else {
                        return Vec::new();
                    };
                    vec![ESC, b'O', l]
                }
                (None, KeyCode::F(n @ 1..=4)) => vec![ESC, b'O', b"PQRS"[n as usize - 1]],
                // screen-256color's khome and kend.
                (None, KeyCode::Home) => b"\x1b[1~".to_vec(),
                (None, KeyCode::End) => b"\x1b[4~".to_vec(),
                _ => functional_bytes(f, param),
            };
        }
    }
    out
}

/// The kitty keyboard protocol for the pane's pushed `flags`: 1
/// disambiguate, 2 event types (only presses are forwarded, so nothing to
/// add), 4 alternate keys, 8 all keys as escape codes, 16 associated text.
fn kitty_encode(key: &Key, modes: &InputModes, flags: u8) -> Vec<u8> {
    let all = flags & 8 != 0;
    let mut mods = key.mods;
    let csi_u = |n: u32, mods: Mods| match kitty_mod(mods) {
        1 => format!("\x1b[{n}u").into_bytes(),
        m => format!("\x1b[{n};{m}u").into_bytes(),
    };
    match key.code {
        KeyCode::Char(c) => {
            let (base, shifted) = if c.is_ascii_uppercase() {
                mods.shift = true;
                (c.to_ascii_lowercase(), Some(c))
            } else {
                (c, None)
            };
            let makes_text = !mods.ctrl && !mods.alt && !mods.super_;
            if makes_text && !all {
                return utf8(c);
            }
            // ponytail: the shifted key is known only for ASCII letters
            // (the canonical Key keeps no base layout key), so `!` goes as
            // 33 rather than kitty's 49 with shift.
            let mut s = format!("\x1b[{}", base as u32);
            if let (true, Some(sh)) = (flags & 4 != 0, shifted) {
                s.push_str(&format!(":{}", sh as u32));
            }
            let m = kitty_mod(mods);
            let text = (all && flags & 16 != 0 && makes_text).then_some(c as u32);
            match (m, text) {
                (1, None) => {}
                (m, None) => s.push_str(&format!(";{m}")),
                (1, Some(t)) => s.push_str(&format!(";;{t}")),
                (m, Some(t)) => s.push_str(&format!(";{m};{t}")),
            }
            s.push('u');
            s.into_bytes()
        }
        KeyCode::Escape => csi_u(27, mods),
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace => {
            if !all && mods == Mods::default() {
                return legacy_encode(key, modes);
            }
            let n = match key.code {
                KeyCode::Enter => 13,
                KeyCode::Tab => 9,
                _ => 127,
            };
            csi_u(n, mods)
        }
        KeyCode::F(n @ 13..=35) => csi_u(57376 + u32::from(n) - 13, mods),
        code => {
            let param = kitty_mod(mods);
            if !all && param == 1 {
                return legacy_encode(key, modes);
            }
            match functional(code, true) {
                Some(f) => functional_bytes(f, (param != 1).then_some(param)),
                None => Vec::new(),
            }
        }
    }
}

/// Paste content for a pane: wrapped in bracketed-paste markers when the
/// pane asked for mode 2004, as is otherwise.
pub fn encode_paste(data: &[u8], modes: &InputModes) -> Vec<u8> {
    if modes.bracketed_paste {
        [PASTE_START, data, PASTE_END].concat()
    } else {
        data.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ET: Duration = Duration::from_millis(25);

    fn k(name: &str) -> Key {
        parse_key(name).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn key(name: &str) -> Input {
        Input::Key(k(name))
    }

    fn decode(bytes: &[u8]) -> Vec<Input> {
        let t = Instant::now();
        let mut d = Decoder::new();
        let mut out = d.feed(bytes, t, ET);
        out.extend(d.flush(t + ET, ET));
        out
    }

    fn modes(kitty_flags: u8) -> InputModes {
        InputModes {
            kitty_flags,
            ..InputModes::default()
        }
    }

    // ---- key names ----

    /// Recorded on tmux 3.7b (`tmux -L grofe-t23 -f /dev/null`,
    /// `set -g prefix X; show -gv prefix`).
    #[test]
    fn key_names_parse_and_print_as_tmux_does() {
        let cases = [
            ("C-b", "C-b"),
            ("c-B", "C-B"),
            ("PgUp", "PPage"),
            ("pageup", "PPage"),
            ("ppage", "PPage"),
            ("IC", "IC"),
            ("dc", "DC"),
            ("BTAB", "BTab"),
            ("bspace", "BSpace"),
            ("M-C-x", "C-M-x"),
            ("S-Up", "S-Up"),
            ("^a", "C-a"),
            ("C-?", "C-?"),
            ("F12", "F12"),
            ("Escape", "Escape"),
            ("space", "Space"),
            ("%", "%"),
            ("M--", "M--"),
            ("-", "-"),
            ("C-Space", "C-Space"),
            ("M-Enter", "M-Enter"),
            ("é", "é"),
        ];
        for (input, shown) in cases {
            assert_eq!(k(input).to_string(), shown, "{input}");
            assert_eq!(k(shown), k(input), "{shown} round-trips");
        }
    }

    #[test]
    fn bad_key_names_are_refused_with_tmux_wording() {
        for bad in ["foo", "F13", "", "C-", "Nonsense", "ab"] {
            assert_eq!(parse_key(bad), Err(format!("bad key: {bad}")), "{bad}");
        }
    }

    #[test]
    fn prefix_accepts_none() {
        assert_eq!(parse_prefix("None"), Ok(None));
        assert_eq!(parse_prefix("none"), Ok(None));
        assert_eq!(parse_prefix("C-a"), Ok(Some(k("C-a"))));
        assert_eq!(parse_prefix("foo"), Err("bad key: foo".to_string()));
    }

    #[test]
    fn shift_folds_into_ascii_letters() {
        assert_eq!(k("S-a"), k("A"));
        assert_eq!(k("C-S-a"), k("C-A"));
        assert_eq!(k("S-A"), k("A"));
        assert_eq!(k("BTab"), k("S-Tab"));
        assert_ne!(k("S-Up"), k("Up"));
    }

    // ---- decoding ----

    #[test]
    fn decodes_text_and_controls() {
        let cases: &[(&[u8], &[&str])] = &[
            (b"a", &["a"]),
            (b"A", &["A"]),
            (b"%", &["%"]),
            (b" ", &["Space"]),
            (b"\r", &["Enter"]),
            (b"\t", &["Tab"]),
            (b"\x7f", &["BSpace"]),
            (b"\x02", &["C-b"]),
            (b"\x01\x1a", &["C-a", "C-z"]),
            (b"\x08", &["C-h"]),
            (b"\x0a", &["C-j"]),
            (b"\x00", &["C-Space"]),
            (b"\x1c\x1d\x1e\x1f", &["C-\\", "C-]", "C-^", "C-_"]),
            ("é€😀".as_bytes(), &["é", "€", "😀"]),
            (b"ab", &["a", "b"]),
        ];
        for (bytes, want) in cases {
            let want: Vec<Input> = want.iter().map(|n| key(n)).collect();
            assert_eq!(decode(bytes), want, "{bytes:?}");
        }
    }

    #[test]
    fn decodes_legacy_csi_and_ss3_keys() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x1b[A", "Up"),
            (b"\x1b[B", "Down"),
            (b"\x1b[C", "Right"),
            (b"\x1b[D", "Left"),
            (b"\x1bOA", "Up"),
            (b"\x1bOD", "Left"),
            (b"\x1b[H", "Home"),
            (b"\x1b[F", "End"),
            (b"\x1bOH", "Home"),
            (b"\x1bOF", "End"),
            (b"\x1b[1~", "Home"),
            (b"\x1b[7~", "Home"),
            (b"\x1b[4~", "End"),
            (b"\x1b[8~", "End"),
            (b"\x1b[2~", "IC"),
            (b"\x1b[3~", "DC"),
            (b"\x1b[5~", "PPage"),
            (b"\x1b[6~", "NPage"),
            (b"\x1bOP", "F1"),
            (b"\x1bOQ", "F2"),
            (b"\x1bOR", "F3"),
            (b"\x1bOS", "F4"),
            (b"\x1b[P", "F1"),
            (b"\x1b[13~", "F3"),
            (b"\x1b[15~", "F5"),
            (b"\x1b[17~", "F6"),
            (b"\x1b[21~", "F10"),
            (b"\x1b[24~", "F12"),
            (b"\x1b[Z", "BTab"),
            (b"\x1bOM", "Enter"),
            // xterm modifier parameter: 2 shift, 3 alt, 5 ctrl, 6 C-S, 7 C-M.
            (b"\x1b[1;2A", "S-Up"),
            (b"\x1b[1;3A", "M-Up"),
            (b"\x1b[1;5D", "C-Left"),
            (b"\x1b[1;6A", "C-S-Up"),
            (b"\x1b[1;7A", "C-M-Up"),
            (b"\x1b[1;5H", "C-Home"),
            (b"\x1b[3;5~", "C-DC"),
            (b"\x1b[5;2~", "S-PPage"),
            (b"\x1b[1;2P", "S-F1"),
            (b"\x1b[15;5~", "C-F5"),
        ];
        for (bytes, want) in cases {
            assert_eq!(decode(bytes), vec![key(want)], "{bytes:?}");
        }
    }

    #[test]
    fn decodes_meta_as_an_escape_prefix() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x1ba", "M-a"),
            (b"\x1bA", "M-A"),
            (b"\x1b\x01", "C-M-a"),
            (b"\x1b\r", "M-Enter"),
            (b"\x1b\x7f", "M-BSpace"),
            (b"\x1b\x1b", "M-Escape"),
            (b"\x1b\x1b[A", "M-Up"),
            ("\x1bé".as_bytes(), "M-é"),
        ];
        for (bytes, want) in cases {
            assert_eq!(decode(bytes), vec![key(want)], "{bytes:?}");
        }
    }

    /// The forms a terminal sends with kitty flags 1 pushed (what T2.1
    /// asks for), plus the richer forms flags 2, 4, 8 and 16 add.
    #[test]
    fn decodes_kitty_csi_u() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x1b[27u", "Escape"),
            (b"\x1b[13u", "Enter"),
            (b"\x1b[9u", "Tab"),
            (b"\x1b[127u", "BSpace"),
            (b"\x1b[97;5u", "C-a"),
            (b"\x1b[98;5u", "C-b"),
            (b"\x1b[97;3u", "M-a"),
            (b"\x1b[97;7u", "C-M-a"),
            (b"\x1b[97;4u", "M-A"),
            (b"\x1b[97;6u", "C-A"),
            (b"\x1b[13;2u", "S-Enter"),
            (b"\x1b[13;5u", "C-Enter"),
            (b"\x1b[9;2u", "BTab"),
            (b"\x1b[27;3u", "M-Escape"),
            (b"\x1b[32;5u", "C-Space"),
            (b"\x1b[37;5u", "C-%"),
            // flag 4: shifted key; flag 8/16: all keys, with text.
            (b"\x1b[97:65;2u", "A"),
            (b"\x1b[49:33;2u", "!"),
            (b"\x1b[113u", "q"),
            (b"\x1b[97;;97u", "a"),
            (b"\x1b[97;2;65u", "A"),
            (b"\x1b[0;;229u", "å"),
            // flag 2: press and repeat are presses.
            (b"\x1b[97;5:1u", "C-a"),
            (b"\x1b[97;5:2u", "C-a"),
            // lock bits are ignored.
            (b"\x1b[97;69u", "C-a"),
            // functional keys in the private use area.
            (b"\x1b[57399u", "0"),
            (b"\x1b[57414u", "Enter"),
            (b"\x1b[57419u", "Up"),
        ];
        for (bytes, want) in cases {
            assert_eq!(decode(bytes), vec![key(want)], "{bytes:?}");
        }
        // F13 has no tmux name but decodes.
        assert_eq!(
            decode(b"\x1b[57376u"),
            vec![Input::Key(Key::plain(KeyCode::F(13)))]
        );
    }

    #[test]
    fn kitty_releases_and_modifier_keys_are_dropped() {
        assert_eq!(decode(b"\x1b[97;5:3u"), vec![]);
        assert_eq!(decode(b"\x1b[57441;2u"), vec![]); // left shift
        assert_eq!(decode(b"\x1b[57358u"), vec![]); // caps lock
    }

    #[test]
    fn decodes_modify_other_keys() {
        assert_eq!(decode(b"\x1b[27;5;97~"), vec![key("C-a")]);
        assert_eq!(decode(b"\x1b[27;2;13~"), vec![key("S-Enter")]);
    }

    #[test]
    fn decodes_sgr_mouse() {
        let m = |kind, col, row, mods| {
            Input::Mouse(Mouse {
                kind,
                col,
                row,
                mods,
            })
        };
        let none = Mods::default();
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert_eq!(
            decode(b"\x1b[<0;10;5M"),
            vec![m(MouseKind::Down(MouseButton::Left), 9, 4, none)]
        );
        assert_eq!(
            decode(b"\x1b[<2;1;1m"),
            vec![m(MouseKind::Up(MouseButton::Right), 0, 0, none)]
        );
        assert_eq!(
            decode(b"\x1b[<32;3;4M"),
            vec![m(MouseKind::Drag(MouseButton::Left), 2, 3, none)]
        );
        assert_eq!(
            decode(b"\x1b[<35;3;4M"),
            vec![m(MouseKind::Move, 2, 3, none)]
        );
        assert_eq!(
            decode(b"\x1b[<64;1;1M"),
            vec![m(MouseKind::WheelUp, 0, 0, none)]
        );
        assert_eq!(
            decode(b"\x1b[<81;1;1M"),
            vec![m(MouseKind::WheelDown, 0, 0, ctrl)]
        );
        assert_eq!(
            decode(b"\x1b[<129;2;2M"),
            vec![m(MouseKind::Down(MouseButton::Other(9)), 1, 1, none)]
        );
    }

    #[test]
    fn decodes_focus_reports() {
        assert_eq!(
            decode(b"\x1b[I\x1b[O"),
            vec![Input::Focus(true), Input::Focus(false)]
        );
    }

    #[test]
    fn unknown_sequences_and_replies_are_dropped() {
        // A stray cursor position report, DA1 reply, kitty flags reply.
        assert_eq!(decode(b"\x1b[12;40R"), vec![]);
        assert_eq!(decode(b"\x1b[?62;22c"), vec![]);
        assert_eq!(decode(b"\x1b[?1u"), vec![]);
        assert_eq!(decode(b"\x1b[99x"), vec![]);
        assert_eq!(decode(b"\x1b[5xa"), vec![key("a")]);
    }

    #[test]
    fn paste_is_one_opaque_blob() {
        let got = decode(b"a\x1b[200~\x02%\x1b[A\x1b\x1b[201~b");
        assert_eq!(
            got,
            vec![
                key("a"),
                Input::Paste(b"\x02%\x1b[A\x1b".to_vec()),
                key("b")
            ]
        );
    }

    #[test]
    fn paste_split_across_reads_waits_for_its_end_marker() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[200~hello \x1b[20", t, ET), vec![]);
        // A paste never times out, however long the gap.
        assert_eq!(d.deadline(ET), None);
        assert_eq!(d.flush(t + Duration::from_secs(5), ET), vec![]);
        assert_eq!(
            d.feed(b"1~x", t + Duration::from_secs(6), ET),
            vec![Input::Paste(b"hello ".to_vec()), key("x")]
        );
    }

    #[test]
    fn sequences_split_across_reads_join_within_escape_time() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[1;", t, ET), vec![]);
        assert_eq!(
            d.feed(b"5A", t + Duration::from_millis(10), ET),
            vec![key("C-Up")]
        );
        assert_eq!(d.feed(&[0xc3], t, ET), vec![]);
        assert_eq!(d.feed(&[0xa9], t, ET), vec![key("é")]);
    }

    // ---- escape-time (injected clock) ----

    #[test]
    fn escape_time_resolves_a_lone_esc() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b", t, ET), vec![]);
        assert_eq!(d.deadline(ET), Some(t + ET));
        assert_eq!(d.flush(t + Duration::from_millis(24), ET), vec![]);
        assert_eq!(d.flush(t + ET, ET), vec![key("Escape")]);
        assert_eq!(d.deadline(ET), None);
        assert_eq!(d.flush(t + ET * 2, ET), vec![]);
    }

    #[test]
    fn esc_then_a_key_within_escape_time_is_meta() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b", t, ET), vec![]);
        assert_eq!(
            d.feed(b"x", t + Duration::from_millis(20), ET),
            vec![key("M-x")]
        );
    }

    #[test]
    fn esc_then_a_key_after_escape_time_is_two_keys() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b", t, ET), vec![]);
        // The timer did not fire before the next read: feed resolves it.
        assert_eq!(
            d.feed(b"x", t + Duration::from_millis(30), ET),
            vec![key("Escape"), key("x")]
        );
    }

    #[test]
    fn escape_time_resolves_an_incomplete_sequence_to_keys() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[", t, ET), vec![]);
        assert_eq!(d.flush(t + ET, ET), vec![key("M-[")]);
        assert_eq!(d.feed(b"\x1bO", t, ET), vec![]);
        assert_eq!(d.flush(t + ET, ET), vec![key("M-O")]);
        assert_eq!(d.feed(b"\x1b\x1b", t, ET), vec![]);
        assert_eq!(d.flush(t + ET, ET), vec![key("M-Escape")]);
    }

    #[test]
    fn zero_escape_time_resolves_at_once() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b", t, Duration::ZERO), vec![key("Escape")]);
    }

    #[test]
    fn the_wait_starts_when_the_bytes_arrive() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"a", t, ET), vec![key("a")]);
        let later = t + Duration::from_secs(1);
        assert_eq!(d.feed(b"\x1b", later, ET), vec![]);
        assert_eq!(d.deadline(ET), Some(later + ET));
    }

    // ---- routing ----

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn tables() -> KeyTables {
        let mut t = KeyTables::default();
        t.bind(PREFIX_TABLE, k("%"), argv(&["split-window", "-h"]), false);
        t.bind(PREFIX_TABLE, k("C-b"), argv(&["send-prefix"]), false);
        t.bind(PREFIX_TABLE, k("Up"), argv(&["select-pane", "-U"]), true);
        t.bind(
            PREFIX_TABLE,
            k("M-Up"),
            argv(&["resize-pane", "-U", "5"]),
            true,
        );
        t.bind(ROOT_TABLE, k("M-h"), argv(&["select-pane", "-L"]), false);
        t
    }

    struct Client {
        dec: Decoder,
        state: KeyState,
        tables: KeyTables,
        settings: KeySettings,
        modes: InputModes,
    }

    impl Client {
        fn new() -> Client {
            Client {
                dec: Decoder::new(),
                state: KeyState::new(),
                tables: tables(),
                settings: KeySettings::default(),
                modes: InputModes::default(),
            }
        }

        fn send(&mut self, bytes: &[u8], now: Instant) -> Vec<Routed> {
            let inputs = self.dec.feed(bytes, now, ET);
            inputs
                .iter()
                .map(|i| {
                    self.state
                        .route(i, now, &self.tables, &self.settings, &self.modes)
                })
                .collect()
        }
    }

    fn cmd(words: &[&str]) -> Routed {
        Routed::Command(argv(words))
    }

    fn pane(bytes: &[u8]) -> Routed {
        Routed::ToPane(bytes.to_vec())
    }

    #[test]
    fn prefix_then_percent_routes_to_the_bound_command() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
        assert_eq!(c.state.table(), PREFIX_TABLE);
        assert_eq!(c.send(b"%", t), vec![cmd(&["split-window", "-h"])]);
        assert_eq!(c.state.table(), ROOT_TABLE);
        // Back in root, `%` is typing.
        assert_eq!(c.send(b"%", t), vec![pane(b"%")]);
    }

    #[test]
    fn prefix_and_key_in_one_read_route_the_same() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(
            c.send(b"\x02%", t),
            vec![Routed::Consumed, cmd(&["split-window", "-h"])]
        );
    }

    #[test]
    fn kitty_spelled_prefix_matches_too() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(
            c.send(b"\x1b[98;5u%", t),
            vec![Routed::Consumed, cmd(&["split-window", "-h"])]
        );
    }

    #[test]
    fn paste_containing_the_prefix_key_is_never_matched() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(
            c.send(b"\x1b[200~x\x02%y\x1b[201~", t),
            vec![pane(b"x\x02%y")]
        );
        assert_eq!(c.state.table(), ROOT_TABLE);
        // A pane in mode 2004 gets the markers back.
        c.modes.bracketed_paste = true;
        assert_eq!(
            c.send(b"\x1b[200~\x02%\x1b[201~", t),
            vec![pane(b"\x1b[200~\x02%\x1b[201~")]
        );
        assert_eq!(c.state.table(), ROOT_TABLE);
    }

    #[test]
    fn paste_after_the_prefix_is_not_matched_either() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(
            c.send(b"\x02\x1b[200~%\x1b[201~", t),
            vec![Routed::Consumed, pane(b"%")]
        );
    }

    #[test]
    fn unbound_key_after_the_prefix_is_dropped() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(
            c.send(b"\x02z", t),
            vec![Routed::Consumed, Routed::Consumed]
        );
        assert_eq!(c.state.table(), ROOT_TABLE);
        assert_eq!(c.send(b"z", t), vec![pane(b"z")]);
    }

    #[test]
    fn prefix_twice_runs_the_prefix_tables_binding() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(
            c.send(b"\x02\x02", t),
            vec![Routed::Consumed, cmd(&["send-prefix"])]
        );
    }

    #[test]
    fn root_bindings_run_without_the_prefix() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(c.send(b"\x1bh", t), vec![cmd(&["select-pane", "-L"])]);
        assert_eq!(c.send(b"\x1bj", t), vec![pane(b"\x1bj")]);
    }

    #[test]
    fn repeat_keys_run_again_without_the_prefix_within_repeat_time() {
        let t = Instant::now();
        let mut c = Client::new();
        let up = cmd(&["select-pane", "-U"]);
        assert_eq!(c.send(b"\x02\x1b[A", t), vec![Routed::Consumed, up.clone()]);
        assert_eq!(c.state.deadline(), Some(t + Duration::from_millis(500)));
        let t1 = t + Duration::from_millis(400);
        assert_eq!(c.send(b"\x1b[A", t1), vec![up.clone()]);
        // Each repeat restarts the timer.
        let t2 = t1 + Duration::from_millis(400);
        assert_eq!(
            c.send(b"\x1b[1;3A", t2),
            vec![cmd(&["resize-pane", "-U", "5"])]
        );
        // After repeat-time the key is the pane's again.
        let t3 = t2 + Duration::from_millis(600);
        assert_eq!(c.send(b"\x1b[A", t3), vec![pane(b"\x1b[A")]);
        assert_eq!(c.state.table(), ROOT_TABLE);
    }

    #[test]
    fn repeat_ends_on_a_non_repeating_or_unbound_key() {
        let t = Instant::now();
        let mut c = Client::new();
        c.send(b"\x02\x1b[A", t);
        // `%` is bound but does not repeat: it is not run, and goes to the
        // pane from root, as tmux does.
        assert_eq!(c.send(b"%", t), vec![pane(b"%")]);
        assert_eq!(c.state.table(), ROOT_TABLE);

        c.send(b"\x02\x1b[A", t);
        assert_eq!(c.send(b"z", t), vec![pane(b"z")]);
        assert_eq!(c.state.deadline(), None);

        // The prefix during a repeat starts a new prefix.
        c.send(b"\x02\x1b[A", t);
        assert_eq!(
            c.send(b"\x02%", t),
            vec![Routed::Consumed, cmd(&["split-window", "-h"])]
        );
    }

    #[test]
    fn expire_reports_the_end_of_a_repeat() {
        let t = Instant::now();
        let mut c = Client::new();
        c.send(b"\x02\x1b[A", t);
        assert!(!c.state.expire(t + Duration::from_millis(499)));
        assert_eq!(c.state.table(), PREFIX_TABLE);
        assert!(c.state.expire(t + Duration::from_millis(500)));
        assert_eq!(c.state.table(), ROOT_TABLE);
    }

    #[test]
    fn zero_repeat_time_turns_repeat_off() {
        let t = Instant::now();
        let mut c = Client::new();
        c.settings.repeat_time = Duration::ZERO;
        c.send(b"\x02\x1b[A", t);
        assert_eq!(c.state.table(), ROOT_TABLE);
        assert_eq!(c.send(b"\x1b[A", t), vec![pane(b"\x1b[A")]);
    }

    #[test]
    fn no_prefix_key_means_none_is_special() {
        let t = Instant::now();
        let mut c = Client::new();
        c.settings.prefix = None;
        assert_eq!(c.send(b"\x02", t), vec![pane(b"\x02")]);
        c.settings.prefix = Some(k("C-a"));
        assert_eq!(c.send(b"\x02", t), vec![pane(b"\x02")]);
        assert_eq!(
            c.send(b"\x01%", t),
            vec![Routed::Consumed, cmd(&["split-window", "-h"])]
        );
    }

    #[test]
    fn custom_table_from_switch_client_t() {
        let t = Instant::now();
        let mut c = Client::new();
        c.tables
            .bind("resize", k("h"), argv(&["resize-pane", "-L"]), false);
        c.state.set_table("resize");
        assert_eq!(c.send(b"h", t), vec![cmd(&["resize-pane", "-L"])]);
        assert_eq!(c.state.table(), ROOT_TABLE);
        c.state.set_table("resize");
        assert_eq!(c.send(b"q", t), vec![Routed::Consumed]);
    }

    #[test]
    fn keys_are_encoded_for_the_active_panes_modes() {
        let t = Instant::now();
        let mut c = Client::new();
        c.modes.app_cursor = true;
        assert_eq!(c.send(b"\x1b[A", t), vec![pane(b"\x1bOA")]);
        c.modes = modes(1);
        assert_eq!(c.send(b"\x1b[27u", t), vec![pane(b"\x1b[27u")]);
        c.modes = modes(8);
        assert_eq!(c.send(b"q", t), vec![pane(b"\x1b[113u")]);
    }

    #[test]
    fn mouse_is_consumed_and_focus_follows_mode_1004() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(c.send(b"\x1b[<0;1;1M", t), vec![Routed::Consumed]);
        assert_eq!(c.send(b"\x1b[I", t), vec![Routed::Consumed]);
        c.modes.focus = true;
        assert_eq!(
            c.send(b"\x1b[I\x1b[O", t),
            vec![pane(b"\x1b[I"), pane(b"\x1b[O")]
        );
    }

    #[test]
    fn unbind_removes_a_binding() {
        let mut t = tables();
        assert!(t.unbind(PREFIX_TABLE, &k("%")));
        assert!(!t.unbind(PREFIX_TABLE, &k("%")));
        assert_eq!(t.get(PREFIX_TABLE, &k("%")), None);
        assert!(t.get(PREFIX_TABLE, &k("C-b")).is_some());
        assert_eq!(t.get("nosuch", &k("C-b")), None);
    }

    // ---- encoding ----

    fn enc(name: &str, m: &InputModes) -> Vec<u8> {
        encode(&k(name), m)
    }

    /// Legacy bytes, as tmux 3.7b sends them into a pane (recorded with
    /// `send-keys` into a raw pty on `tmux -L grofe-t23 -f /dev/null`), and
    /// screen-256color's terminfo (`infocmp -1x`: kcuu1 `\EOA` in keypad
    /// transmit mode, khome `\E[1~`, kend `\E[4~`, kf1 `\EOP`, kcbt `\E[Z`).
    #[test]
    fn legacy_encoding_matches_tmux() {
        let m = InputModes::default();
        let cases: &[(&str, &[u8])] = &[
            ("a", b"a"),
            ("A", b"A"),
            ("é", "é".as_bytes()),
            ("Space", b" "),
            ("Enter", b"\r"),
            ("Tab", b"\t"),
            ("BSpace", b"\x7f"),
            ("Escape", b"\x1b"),
            ("C-a", b"\x01"),
            ("C-b", b"\x02"),
            ("C-A", b"\x01"),
            ("C-Space", b"\x00"),
            ("C-@", b"\x00"),
            ("C-/", b"\x1f"),
            ("C-?", b"\x7f"),
            ("C-[", b"\x1b"),
            ("C-1", b"1"),
            ("C-i", b"\t"),
            ("C-m", b"\r"),
            ("C-h", b"\x08"),
            ("M-a", b"\x1ba"),
            ("M-A", b"\x1bA"),
            ("C-M-a", b"\x1b\x01"),
            ("M-Space", b"\x1b "),
            ("M-Enter", b"\x1b\r"),
            ("M-BSpace", b"\x1b\x7f"),
            ("M-Escape", b"\x1b\x1b"),
            ("S-Enter", b"\r"),
            ("C-Enter", b"\r"),
            ("C-Tab", b"\t"),
            ("BTab", b"\x1b[Z"),
            ("M-BTab", b"\x1b[Z"),
            ("S-Space", b" "),
            ("Up", b"\x1b[A"),
            ("Left", b"\x1b[D"),
            ("M-Up", b"\x1b[1;3A"),
            ("C-S-Up", b"\x1b[1;6A"),
            ("Home", b"\x1b[1~"),
            ("End", b"\x1b[4~"),
            ("C-Home", b"\x1b[1;5H"),
            ("S-End", b"\x1b[1;2F"),
            ("IC", b"\x1b[2~"),
            ("DC", b"\x1b[3~"),
            ("C-DC", b"\x1b[3;5~"),
            ("PPage", b"\x1b[5~"),
            ("NPage", b"\x1b[6~"),
            ("F1", b"\x1bOP"),
            ("F4", b"\x1bOS"),
            ("S-F1", b"\x1b[1;2P"),
            ("F5", b"\x1b[15~"),
            ("M-F5", b"\x1b[15;3~"),
            ("F12", b"\x1b[24~"),
        ];
        for (name, want) in cases {
            assert_eq!(enc(name, &m), want.to_vec(), "{name}");
        }
        // Keys with no legacy form send nothing.
        assert_eq!(encode(&Key::plain(KeyCode::F(13)), &m), Vec::<u8>::new());
    }

    #[test]
    fn app_cursor_mode_uses_ss3_for_unmodified_arrows() {
        let m = InputModes {
            app_cursor: true,
            ..InputModes::default()
        };
        assert_eq!(enc("Up", &m), b"\x1bOA");
        assert_eq!(enc("Right", &m), b"\x1bOC");
        assert_eq!(enc("C-Up", &m), b"\x1b[1;5A");
        assert_eq!(enc("Home", &m), b"\x1b[1~");
    }

    /// Kitty flag 1 (disambiguate), per the protocol spec: Escape, and
    /// ctrl/alt with any key, become `CSI code;mods u`; text keys stay text;
    /// unmodified Enter, Tab and Backspace keep their legacy bytes;
    /// functional keys keep their legacy forms.
    #[test]
    fn kitty_disambiguate_encoding() {
        let m = modes(1);
        let cases: &[(&str, &[u8])] = &[
            ("a", b"a"),
            ("A", b"A"),
            ("é", "é".as_bytes()),
            ("Space", b" "),
            ("Escape", b"\x1b[27u"),
            ("M-Escape", b"\x1b[27;3u"),
            ("C-a", b"\x1b[97;5u"),
            ("C-A", b"\x1b[97;6u"),
            ("M-a", b"\x1b[97;3u"),
            ("M-A", b"\x1b[97;4u"),
            ("C-M-a", b"\x1b[97;7u"),
            ("C-Space", b"\x1b[32;5u"),
            ("C-i", b"\x1b[105;5u"),
            ("C-[", b"\x1b[91;5u"),
            ("Enter", b"\r"),
            ("Tab", b"\t"),
            ("BSpace", b"\x7f"),
            ("S-Enter", b"\x1b[13;2u"),
            ("C-Enter", b"\x1b[13;5u"),
            ("M-Enter", b"\x1b[13;3u"),
            ("BTab", b"\x1b[9;2u"),
            ("C-BSpace", b"\x1b[127;5u"),
            ("Up", b"\x1b[A"),
            ("C-Up", b"\x1b[1;5A"),
            ("Home", b"\x1b[1~"),
            ("C-Home", b"\x1b[1;5H"),
            ("DC", b"\x1b[3~"),
            ("S-DC", b"\x1b[3;2~"),
            ("F1", b"\x1bOP"),
            ("C-F1", b"\x1b[1;5P"),
            ("C-F3", b"\x1b[13;5~"),
            ("F5", b"\x1b[15~"),
        ];
        for (name, want) in cases {
            assert_eq!(enc(name, &m), want.to_vec(), "{name}");
        }
        assert_eq!(encode(&Key::plain(KeyCode::F(13)), &m), b"\x1b[57376u");
        let super_a = Key::new(
            KeyCode::Char('a'),
            Mods {
                super_: true,
                ..Mods::default()
            },
        );
        assert_eq!(encode(&super_a, &m), b"\x1b[97;9u");
    }

    /// Flag 8 (report all keys as escape codes): text keys too, and
    /// Enter/Tab/Backspace, and functional keys in their CSI forms.
    #[test]
    fn kitty_report_all_keys_encoding() {
        let m = InputModes {
            kitty_flags: 1 | 8,
            app_cursor: true,
            ..InputModes::default()
        };
        let cases: &[(&str, &[u8])] = &[
            ("q", b"\x1b[113u"),
            ("A", b"\x1b[97;2u"),
            ("!", b"\x1b[33u"),
            ("Space", b"\x1b[32u"),
            ("Enter", b"\x1b[13u"),
            ("Tab", b"\x1b[9u"),
            ("BTab", b"\x1b[9;2u"),
            ("BSpace", b"\x1b[127u"),
            ("Escape", b"\x1b[27u"),
            ("C-a", b"\x1b[97;5u"),
            ("Up", b"\x1b[A"),
            ("S-Up", b"\x1b[1;2A"),
            ("Home", b"\x1b[H"),
            ("End", b"\x1b[F"),
            ("IC", b"\x1b[2~"),
            ("F1", b"\x1b[P"),
            ("F3", b"\x1b[13~"),
            ("F4", b"\x1b[S"),
            ("F5", b"\x1b[15~"),
        ];
        for (name, want) in cases {
            assert_eq!(enc(name, &m), want.to_vec(), "{name}");
        }
        // Flag 8 alone behaves the same.
        assert_eq!(enc("q", &modes(8)), b"\x1b[113u");
    }

    #[test]
    fn kitty_alternate_keys_and_associated_text() {
        // Flag 4: the shifted key after a colon, only with shift.
        assert_eq!(enc("A", &modes(8 | 4)), b"\x1b[97:65;2u");
        assert_eq!(enc("a", &modes(8 | 4)), b"\x1b[97u");
        assert_eq!(enc("C-A", &modes(1 | 4)), b"\x1b[97:65;6u");
        // Flag 16 with 8: the text as code points.
        assert_eq!(enc("a", &modes(8 | 16)), b"\x1b[97;;97u");
        assert_eq!(enc("A", &modes(8 | 16)), b"\x1b[97;2;65u");
        assert_eq!(enc("A", &modes(8 | 4 | 16)), b"\x1b[97:65;2;65u");
        // Keys that make no text carry none.
        assert_eq!(enc("C-a", &modes(8 | 16)), b"\x1b[97;5u");
        assert_eq!(enc("Enter", &modes(8 | 16)), b"\x1b[13u");
        // Flag 2 adds nothing for presses; flag 16 without 8 is undefined
        // and is ignored.
        assert_eq!(enc("C-a", &modes(1 | 2)), b"\x1b[97;5u");
        assert_eq!(enc("a", &modes(1 | 16)), b"a");
    }

    #[test]
    fn decode_then_encode_round_trips_legacy_bytes() {
        let m = InputModes::default();
        let samples: &[&[u8]] = &[
            b"a",
            b"\x01",
            b"\r",
            b"\t",
            b"\x7f",
            b"\x1b[A",
            b"\x1b[1;5C",
            b"\x1b[3~",
            b"\x1b[15;2~",
            b"\x1bOP",
            b"\x1bx",
            b"\x1b[Z",
        ];
        for bytes in samples {
            let inputs = decode(bytes);
            let [Input::Key(key)] = inputs.as_slice() else {
                panic!("{bytes:?}: {inputs:?}");
            };
            assert_eq!(encode(key, &m), bytes.to_vec(), "{bytes:?}");
        }
    }

    #[test]
    fn paste_encoding() {
        let plain = InputModes::default();
        let bracketed = InputModes {
            bracketed_paste: true,
            ..InputModes::default()
        };
        assert_eq!(encode_paste(b"hi", &plain), b"hi");
        assert_eq!(encode_paste(b"hi", &bracketed), b"\x1b[200~hi\x1b[201~");
    }
}
