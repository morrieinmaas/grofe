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
//!   legacy xterm/screen bytes (the pane's `TERM` is tmux-256color, whose
//!   unmodified keys are screen-256color's), or
//!   the kitty keyboard protocol for the flags the pane has pushed.
//!
//! [`InputHook`]s (M3: the prompt, the mouse) see every input before the
//! tables; running them is the caller's job (`clients::route`), since the
//! registry lives in T2.5a's clients.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::Dialect;
use crate::server::State;
use crate::types::{ClientId, InputModes};

/// A decoded key press. T2.3 may add [`KeyCode`] variants; the field names
/// are the contract.
///
/// A key keeps the spelling it was parsed or decoded with, as tmux does
/// (`C-A`, `C-S-a`, `S-a`, `S-Tab` and `BTab` are distinct keys). Matching
/// against bindings and the prefix also tries the [`Key::folded`] form, so
/// a binding matches however the outer terminal spelled the key.
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
    /// tmux's `BTab` (`CSI Z`), a key of its own, not `S-Tab`.
    BackTab,
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
    /// A bindable mouse key (`MouseDown1Pane`, TC.10); never sent to a
    /// pane as a key.
    Mouse(MouseKey),
}

/// A mouse key, as tmux 3.7b names them: what happened, with which button
/// (`0` for the wheel), and where (spec §4.5 bindings row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseKey {
    pub action: MouseAction,
    pub button: u8,
    pub place: MousePlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseAction {
    Down,
    Up,
    Drag,
    DragEnd,
    WheelUp,
    WheelDown,
    SecondClick,
    DoubleClick,
    TripleClick,
}

/// Where the pointer was: in a pane, on a border, or on the status line,
/// in a window, session or user range (`Status`), in `status-left` or
/// `status-right`'s own range, or in no range (`StatusDefault`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MousePlace {
    Pane,
    Border,
    Status,
    StatusLeft,
    StatusRight,
    StatusDefault,
    /// tmux 3.7's pane scrollbar parts and pane border controls (`Control0`
    /// to `Control9`): the names parse and bind, grofe draws neither, so
    /// they never fire.
    ScrollbarUp,
    ScrollbarSlider,
    ScrollbarDown,
    Control(u8),
}

const MOUSE_ACTIONS: &[(&str, MouseAction)] = &[
    // Longest first: `MouseDragEnd` before `MouseDrag`.
    ("MouseDragEnd", MouseAction::DragEnd),
    ("MouseDrag", MouseAction::Drag),
    ("MouseDown", MouseAction::Down),
    ("MouseUp", MouseAction::Up),
    ("WheelUp", MouseAction::WheelUp),
    ("WheelDown", MouseAction::WheelDown),
    ("SecondClick", MouseAction::SecondClick),
    ("DoubleClick", MouseAction::DoubleClick),
    ("TripleClick", MouseAction::TripleClick),
];

const MOUSE_PLACES: &[(&str, MousePlace)] = &[
    ("Pane", MousePlace::Pane),
    ("Border", MousePlace::Border),
    ("Status", MousePlace::Status),
    ("StatusLeft", MousePlace::StatusLeft),
    ("StatusRight", MousePlace::StatusRight),
    ("StatusDefault", MousePlace::StatusDefault),
    ("ScrollbarUp", MousePlace::ScrollbarUp),
    ("ScrollbarSlider", MousePlace::ScrollbarSlider),
    ("ScrollbarDown", MousePlace::ScrollbarDown),
];

impl MouseKey {
    /// A mouse key name without modifiers (`MouseDown1Pane`, `WheelUpStatus`),
    /// matched case-insensitively as tmux matches key names. Buttons are 1
    /// to 11; the wheel has none.
    fn parse(name: &str) -> Option<MouseKey> {
        let lower = name.to_ascii_lowercase();
        let (word, action) = MOUSE_ACTIONS
            .iter()
            .find(|(w, _)| lower.starts_with(&w.to_ascii_lowercase()))?;
        let rest = &lower[word.len()..];
        let wheel = matches!(action, MouseAction::WheelUp | MouseAction::WheelDown);
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let button = if wheel {
            (digits == 0).then_some(0)?
        } else {
            let n: u8 = rest[..digits].parse().ok()?;
            (1..=11).contains(&n).then_some(n)?
        };
        let place = &rest[digits..];
        let place = match place.strip_prefix("control") {
            Some(n) if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => {
                MousePlace::Control(n.as_bytes()[0] - b'0')
            }
            _ => {
                MOUSE_PLACES
                    .iter()
                    .find(|(p, _)| p.eq_ignore_ascii_case(place))?
                    .1
            }
        };
        Some(MouseKey {
            action: *action,
            button,
            place,
        })
    }
}

impl fmt::Display for MouseKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let action = MOUSE_ACTIONS
            .iter()
            .find(|(_, a)| *a == self.action)
            .map_or("", |(w, _)| w);
        let place = match self.place {
            MousePlace::Control(n) => format!("Control{n}"),
            other => MOUSE_PLACES
                .iter()
                .find(|(_, p)| *p == other)
                .map_or("", |(w, _)| w)
                .to_string(),
        };
        if self.button == 0 {
            write!(f, "{action}{place}")
        } else {
            write!(f, "{action}{}{place}", self.button)
        }
    }
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
    /// A bound command sequence, each command's argv, for dispatch in
    /// order (R12), in the dialect it was stored in (spec §4.5).
    Command(Vec<Vec<String>>, Dialect),
    /// Bytes for the active pane, encoded for its `InputModes`.
    ToPane(Vec<u8>),
    /// Eaten (a prefix key, an overlay's key).
    Consumed,
}

/// A hook that sees every input of every client before the key tables
/// (M3: the prompt, the mouse; registered in `register_builtins`). `None`
/// passes the input on; the first hook to answer `Some` wins (R1).
///
/// Hooks are stateless: what they remember lives in a typed field of
/// [`State`] owned by their module, which `state` gives them.
pub trait InputHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        now: Instant,
    ) -> Option<Routed>;
}

/// One decoded unit of client input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    /// Bracketed-paste content, without the markers. Never matched against
    /// bindings. A paste longer than the decoder holds arrives in pieces:
    /// `start` is set on the first, `end` on the last, both on a paste that
    /// fits in one, so a pane still sees one bracketed paste.
    Paste {
        data: Vec<u8>,
        start: bool,
        end: bool,
    },
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
        Key { code, mods }
    }

    /// A key with no modifiers.
    pub fn plain(code: KeyCode) -> Key {
        Key::new(code, Mods::default())
    }

    /// The form used only for matching: shift folded into an ASCII letter
    /// (`S-a` and `A` fold alike, as do `C-S-a` and `C-A`), and `S-Tab`
    /// folded into `BTab`.
    // ponytail: ASCII letters only; other scripts keep an explicit shift.
    pub fn folded(mut self) -> Key {
        match self.code {
            KeyCode::Char(c) if c.is_ascii_lowercase() && self.mods.shift => {
                self.code = KeyCode::Char(c.to_ascii_uppercase());
                self.mods.shift = false;
            }
            KeyCode::Char(c) if c.is_ascii_uppercase() => self.mods.shift = false,
            KeyCode::Tab | KeyCode::BackTab if self.mods.shift => {
                self.code = KeyCode::BackTab;
                self.mods.shift = false;
            }
            _ => {}
        }
        self
    }

    /// Whether `self` and `other` are the same key once folded.
    pub fn matches(&self, other: &Key) -> bool {
        self == other || self.folded() == other.folded()
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
    ("BTab", KeyCode::BackTab),
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
/// named keys and mouse keys (`MouseDown1Pane`, [`MouseKey`])
/// case-insensitively. The error is tmux's `bad key: X`.
// ponytail: tmux's keypad names (KP5, KPEnter) are not parsed; KeyCode
// has no keypad variants.
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
    let code = if let Some(&(_, code)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(rest)) {
        code
    } else if let Some(n) = function_key_number(rest) {
        KeyCode::F(n)
    } else if let Some(m) = MouseKey::parse(rest) {
        KeyCode::Mouse(m)
    } else {
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => KeyCode::Char(c),
            _ => return Err(bad()),
        }
    };
    Ok(Key { code, mods })
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
        if self.mods.ctrl {
            f.write_str("C-")?;
        }
        if self.mods.alt {
            f.write_str("M-")?;
        }
        if self.mods.shift {
            f.write_str("S-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::F(n) => write!(f, "F{n}"),
            KeyCode::Mouse(m) => write!(f, "{m}"),
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
    /// When the bytes now in `buf` started waiting; inside a paste, when
    /// the last paste bytes arrived.
    since: Option<Instant>,
    /// Inside a bracketed paste: `buf` is paste content.
    in_paste: bool,
    /// Paste bytes at the front of `buf` already searched for the end
    /// marker, so a long paste is scanned once.
    paste_scanned: usize,
    /// A piece of the current paste has already been passed on.
    paste_begun: bool,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Paste content held before it is passed on in chunks.
const PASTE_CAP: usize = 1 << 20;
/// A paste with no data for this long ends, marker or not.
const PASTE_TIMEOUT: Duration = Duration::from_secs(5);

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
                out.push(self.paste_piece(data, true));
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
    fn drain(&mut self, force: bool, out: &mut Vec<Input>) -> usize {
        let mut pos = 0;
        loop {
            if self.in_paste {
                let rest = &self.buf[pos..];
                let from = self.paste_scanned.saturating_sub(PASTE_END.len() - 1);
                if let Some(end) = find(&rest[from..], PASTE_END).map(|i| i + from) {
                    let data = rest[..end].to_vec();
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
                    out.push(self.paste_piece(data, false));
                    pos += n;
                    self.paste_scanned = keep;
                }
                break;
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
    fn paste_piece(&mut self, data: Vec<u8>, end: bool) -> Input {
        let start = !self.paste_begun;
        self.paste_begun = true;
        Input::Paste { data, start, end }
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

/// Where a binding came from (spec §5.4, key provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSource {
    /// The built-in default table.
    Default,
    /// A config file layer (spec §5.1), at the key's line.
    File { path: PathBuf, line: usize },
    /// `bind-key` or `source-file` at runtime: outside the config table.
    Runtime,
    /// A plugin, by name (M6; unused in M3).
    Plugin(String),
}

impl BindingSource {
    /// Whether the binding came from the config table (defaults or a
    /// file), which `reload` replaces (R10).
    pub fn is_config(&self) -> bool {
        matches!(self, BindingSource::Default | BindingSource::File { .. })
    }
}

/// A key binding: the commands to run, in order, whether it repeats
/// (`bind -r`), and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Each command's argv (R12: a binding may be a sequence), run as
    /// already split, so a literal `;` argument stays literal.
    pub cmds: Vec<Vec<String>>,
    pub repeat: bool,
    pub source: BindingSource,
    /// `desc` in the config, `bind-key -N` at runtime.
    pub desc: Option<String>,
    /// The config's stable `name` (spec §5.2).
    pub name: Option<String>,
    /// The language `cmds` are in (spec §4.5: a stored command keeps the
    /// dialect it was bound in). Grofe until the front end binds (FE.1).
    pub dialect: Dialect,
}

impl Binding {
    /// A default-table binding: no description or name, grofe's dialect.
    pub fn new(cmds: Vec<Vec<String>>, repeat: bool, source: BindingSource) -> Binding {
        Binding {
            cmds,
            repeat,
            source,
            desc: None,
            name: None,
            dialect: Dialect::Grofe,
        }
    }
}

/// Bindings by table name, then key.
#[derive(Debug, Clone, Default)]
pub struct KeyTables {
    tables: BTreeMap<String, HashMap<Key, Binding>>,
    /// Keys removed by a runtime `unbind-key`, which `reload` keeps
    /// removed (R10).
    runtime_unbound: Vec<(String, Key)>,
    /// Built by [`KeyTables::from_config`] from at least one binding.
    configured: bool,
}

impl KeyTables {
    /// Binds one command.
    pub fn bind(&mut self, table: &str, key: Key, argv: Vec<String>, repeat: bool) {
        self.bind_seq(table, key, vec![argv], repeat);
    }

    /// Binds a command sequence, run in order, as a default binding.
    pub fn bind_seq(&mut self, table: &str, key: Key, cmds: Vec<Vec<String>>, repeat: bool) {
        self.insert(
            table,
            key,
            Binding::new(cmds, repeat, BindingSource::Default),
        );
    }

    /// Binds `b` as it is. A runtime binding ends a runtime unbind of
    /// the same key.
    pub fn insert(&mut self, table: &str, key: Key, b: Binding) {
        if !b.source.is_config() {
            self.runtime_unbound
                .retain(|(t, k)| !(t == table && *k == key));
        }
        self.tables
            .entry(table.to_string())
            .or_default()
            .insert(key, b);
    }

    /// Remove the binding of exactly `key` (as spelled); whether one was
    /// there.
    pub fn unbind(&mut self, table: &str, key: &Key) -> bool {
        self.tables
            .get_mut(table)
            .is_some_and(|t| t.remove(key).is_some())
    }

    /// `unbind-key` at runtime: removed now and kept removed by `reload`.
    /// A key that was not bound is not recorded, so a later config
    /// binding of it still applies.
    pub fn unbind_runtime(&mut self, table: &str, key: &Key) {
        if self.unbind(table, key)
            && !self
                .runtime_unbound
                .iter()
                .any(|(t, k)| t == table && k == key)
        {
            self.runtime_unbound.push((table.to_string(), *key));
        }
    }

    /// Every key bound in `table`, sorted by name (stable output).
    pub fn keys(&self, table: &str) -> Vec<Key> {
        let mut keys: Vec<Key> = self
            .tables
            .get(table)
            .map(|t| t.keys().copied().collect())
            .unwrap_or_default();
        keys.sort_by_cached_key(|k| k.to_string());
        keys
    }

    /// Whether a config binding (or unbinding) shaped these tables.
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Every table's name, sorted.
    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    /// The binding of exactly `key`, with no folding.
    pub fn exact(&self, table: &str, key: &Key) -> Option<&Binding> {
        self.tables.get(table)?.get(key)
    }

    /// The default table with the config's bindings over it (spec §5.1):
    /// each binding from `keys` replaces the default one, and one with no
    /// commands unbinds the key (R11).
    pub fn from_config(keys: &[crate::config::KeyBinding]) -> KeyTables {
        let mut t = default_bindings();
        t.configured = !keys.is_empty();
        for k in keys {
            if k.cmds.is_empty() {
                t.unbind(&k.table, &k.key);
                continue;
            }
            let source = match &k.source.file {
                Some(path) => BindingSource::File {
                    path: path.clone(),
                    line: k.source.line,
                },
                None => BindingSource::Default,
            };
            let mut b = Binding::new(k.cmds.clone(), k.repeat, source);
            b.desc = k.desc.clone();
            b.name = k.name.clone();
            t.insert(&k.table, k.key, b);
        }
        t
    }

    /// `self`'s config bindings replaced by `fresh`'s (R10): runtime and
    /// plugin bindings, and runtime unbinds, carry over.
    pub fn with_config_from(&self, fresh: KeyTables) -> KeyTables {
        let mut t = fresh;
        for (table, key) in &self.runtime_unbound {
            t.unbind(table, key);
        }
        t.runtime_unbound = self.runtime_unbound.clone();
        for (table, bindings) in &self.tables {
            for (key, b) in bindings.iter().filter(|(_, b)| !b.source.is_config()) {
                t.insert(table, *key, b.clone());
            }
        }
        t
    }

    /// Whether `table` exists: `root` always does, as in tmux; any other
    /// once something was bound in it.
    pub fn exists(&self, table: &str) -> bool {
        table == ROOT_TABLE || self.tables.contains_key(table)
    }

    /// The binding for `key`: the one spelled exactly as `key`, else the
    /// one spelled as its folded form, else any whose folded form matches
    /// (the first by name, so the choice is stable).
    pub fn get(&self, table: &str, key: &Key) -> Option<&Binding> {
        let t = self.tables.get(table)?;
        if let Some(b) = t.get(key) {
            return Some(b);
        }
        let folded = key.folded();
        t.get(&folded).or_else(|| {
            t.iter()
                .filter(|(k, _)| k.folded() == folded)
                .min_by_key(|(k, _)| k.to_string())
                .map(|(_, b)| b)
        })
    }
}

/// The default key table (spec §5.3, the "tmux default bindings" row, D10):
/// tmux 3.7b's stock `prefix` bindings for the commands grofe has, with
/// repeat where tmux has `-r` and tmux's notes (`list-keys -N`). New panes
/// and windows open in the active pane's cwd (spec §5.3), which tmux leaves
/// to a `-c '#{pane_current_path}'` in the user's config, and prompts and
/// confirmations spell tmux's `#P`/`#W` as native formats. Left unbound:
/// `o` (needs `+`/`-` target offsets, FE.1's), `m`/`M` (marking), `C` and
/// `t` (out of scope, spec §4.5), `<`/`>` (tmux's window and pane menus),
/// `*` (`new-pane`), and copy mode and buffers (`[ ] = # - PPage`, M5).
pub fn default_bindings() -> KeyTables {
    let mut t = KeyTables::default();
    let mut bind = |key: &str, argv: &[&str], repeat: bool, note: &str| {
        let argv = argv.iter().map(|s| s.to_string()).collect();
        if let Ok(key) = parse_key(key) {
            let mut b = Binding::new(vec![argv], repeat, BindingSource::Default);
            b.desc = Some(note.to_string());
            t.insert(PREFIX_TABLE, key, b);
        }
    };
    let here = "#{pane_current_path}";
    bind(
        "%",
        &["split-window", "-h", "-c", here],
        false,
        "Split window horizontally",
    );
    bind(
        "\"",
        &["split-window", "-c", here],
        false,
        "Split window vertically",
    );
    bind(
        "c",
        &["new-window", "-c", here],
        false,
        "Create a new window",
    );
    bind("d", &["detach-client"], false, "Detach the current client");
    bind("n", &["next-window"], false, "Select the next window");
    bind(
        "p",
        &["previous-window"],
        false,
        "Select the previous window",
    );
    bind(
        "l",
        &["last-window"],
        false,
        "Select the previously current window",
    );
    bind(
        "(",
        &["switch-client", "-p"],
        false,
        "Switch to previous client",
    );
    bind(
        ")",
        &["switch-client", "-n"],
        false,
        "Switch to next client",
    );
    bind(
        "L",
        &["switch-client", "-l"],
        false,
        "Switch to the last client",
    );
    bind("z", &["resize-pane", "-Z"], false, "Zoom the active pane");
    bind("C-b", &["send-prefix"], false, "Send the prefix key");
    bind(":", &["command-prompt"], false, "Prompt for a command");
    bind(
        ",",
        &["command-prompt", "-I", "#W", "rename-window \"%%\""],
        false,
        "Rename current window",
    );
    bind(
        "$",
        &["command-prompt", "-I", "#S", "rename-session \"%%\""],
        false,
        "Rename current session",
    );
    for n in 0..=9 {
        let target = format!(":{n}");
        bind(
            &n.to_string(),
            &["select-window", "-t", &target],
            false,
            &format!("Select window {n}"),
        );
    }
    for (key, flag, side) in [
        ("Up", "-U", "above"),
        ("Down", "-D", "below"),
        ("Left", "-L", "to the left of"),
        ("Right", "-R", "to the right of"),
    ] {
        let dir = key.to_ascii_lowercase();
        bind(
            key,
            &["select-pane", flag],
            true,
            &format!("Select the pane {side} the active pane"),
        );
        bind(
            &format!("M-{key}"),
            &["resize-pane", flag, "5"],
            true,
            &format!("Resize the pane {dir} by 5"),
        );
        bind(
            &format!("C-{key}"),
            &["resize-pane", flag],
            true,
            &format!("Resize the pane {dir}"),
        );
    }
    // D10: the stock keys whose verbs M3.6 adds.
    bind(
        ";",
        &["last-pane"],
        false,
        "Move to the previously active pane",
    );
    bind("C-o", &["rotate-window"], false, "Rotate through the panes");
    bind(
        "M-o",
        &["rotate-window", "-D"],
        false,
        "Rotate through the panes in reverse",
    );
    bind("q", &["display-panes"], false, "Display pane numbers");
    bind(
        ".",
        &["command-prompt", "-T", "target", "move-window -t \"%%\""],
        false,
        "Move the current window",
    );
    bind(
        "f",
        &["command-prompt", "find-window -Z \"%%\""],
        false,
        "Search for a pane",
    );
    bind(
        "'",
        &[
            "command-prompt",
            "-T",
            "window-target",
            "-p",
            "index",
            "select-window -t \":%%\"",
        ],
        false,
        "Prompt for window index to select",
    );
    bind(
        "C-z",
        &["suspend-client"],
        false,
        "Suspend the current client",
    );
    bind(
        "D",
        &["choose-client", "-Z"],
        false,
        "Choose and detach a client from a list",
    );
    bind("~", &["show-messages"], false, "Show messages");
    bind(
        "E",
        &["select-layout", "-E"],
        false,
        "Spread panes out evenly",
    );
    bind(
        "M-n",
        &["next-window", "-a"],
        false,
        "Select the next window with an alert",
    );
    bind(
        "M-p",
        &["previous-window", "-a"],
        false,
        "Select the previous window with an alert",
    );
    bind(
        "/",
        &["command-prompt", "-k", "-p", "key", "list-keys -1N \"%%\""],
        false,
        "Describe key binding",
    );
    bind(
        "x",
        &[
            "confirm-before",
            "-p",
            "kill-pane #{pane_index}? (y/n)",
            "kill-pane",
        ],
        false,
        "Kill the active pane",
    );
    bind(
        "&",
        &[
            "confirm-before",
            "-p",
            "kill-window #{window_name}? (y/n)",
            "kill-window",
        ],
        false,
        "Kill current window",
    );
    bind("!", &["break-pane"], false, "Break pane to a new window");
    bind(
        "{",
        &["swap-pane", "-U"],
        false,
        "Swap the active pane with the pane above",
    );
    bind(
        "}",
        &["swap-pane", "-D"],
        false,
        "Swap the active pane with the pane below",
    );
    bind(
        "i",
        &["display-message"],
        false,
        "Display window information",
    );
    bind(
        "w",
        &["choose-tree", "-Zw"],
        false,
        "Choose a window from a list",
    );
    bind("Space", &["next-layout"], false, "Select next layout");
    bind("?", &["list-keys", "-N"], false, "List key bindings");
    bind("r", &["refresh-client"], false, "Redraw the current client");
    bind(
        "s",
        &["choose-tree", "-Zs"],
        false,
        "Choose a session from a list",
    );
    for (key, flag, note) in [
        ("S-Up", "-U", "Move the visible part of the window up"),
        ("S-Down", "-D", "Move the visible part of the window down"),
        ("S-Left", "-L", "Move the visible part of the window left"),
        ("S-Right", "-R", "Move the visible part of the window right"),
    ] {
        bind(key, &["refresh-client", flag, "10"], true, note);
    }
    bind(
        "DC",
        &["refresh-client", "-c"],
        true,
        "Reset so the visible part of the window follows the cursor",
    );
    for (key, layout, verb) in [
        ("M-1", "even-horizontal", "Set"),
        ("M-2", "even-vertical", "Set"),
        ("M-3", "main-horizontal", "Set"),
        ("M-4", "main-vertical", "Set"),
        ("M-5", "tiled", "Select"),
        ("M-6", "main-horizontal-mirrored", "Set"),
        ("M-7", "main-vertical-mirrored", "Set"),
    ] {
        bind(
            key,
            &["select-layout", layout],
            false,
            &format!("{verb} the {layout} layout"),
        );
    }
    t
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
            // Never matched; like tmux 3.7b, a paste also ends a pending
            // prefix or repeat (recorded: prefix, then a paste, leaves the
            // client in root).
            Input::Paste { data, start, end } => {
                self.set_table(ROOT_TABLE);
                Routed::ToPane(encode_paste(data, *start, *end, modes))
            }
            // The mouse hook (`server::mouse`) routes the mouse: mouse keys,
            // the built-in model and forwarding. What reaches the tables
            // (`mouse off`) is dropped.
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
        let key = *key;
        self.expire(now);
        let mut repeating = self.repeat_until.is_some();
        let mut first = self.table.clone();
        loop {
            // The prefix always wins, unless already in the prefix table.
            if settings.prefix.is_some_and(|p| p.matches(&key)) && self.table != PREFIX_TABLE {
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
                return Routed::Command(b.cmds.clone(), b.dialect);
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
            // D4, recorded on tmux 3.7b: back-tab typed with ctrl or meta
            // (kitty `9;4u`, `\e\e[Z`) reaches a legacy pane as nothing,
            // though `send-keys M-BTab` sends `\e[Z`.
            let legacy = modes.kitty_flags & (1 | 8) == 0;
            if legacy && key.code == KeyCode::BackTab && (key.mods.alt || key.mods.ctrl) {
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
    let flags = modes.kitty_flags;
    if flags & (1 | 8) != 0 {
        kitty_encode(key, modes, flags)
    } else {
        legacy_encode(key, modes)
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

/// What tmux-256color panes expect, as tmux 3.7b sends it: modifiers a
/// legacy form cannot carry are dropped (`C-Enter` is `\r`, `S-a` is `a`,
/// `S-Tab` is `\t`), and keys tmux cannot send at all (`C-BSpace`,
/// `C-Escape`: its send-keys types them as text) send nothing.
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
        KeyCode::Escape | KeyCode::Backspace if m.ctrl => return Vec::new(),
        KeyCode::Escape => out.push(ESC),
        KeyCode::Backspace => out.push(0x7f),
        // tmux sends back-tab with no meta prefix, whatever the modifiers.
        KeyCode::BackTab => return b"\x1b[Z".to_vec(),
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
                // tmux-256color's (and screen-256color's) khome and kend.
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
            // The key code is the unshifted key; shift is a modifier.
            let (base, shifted) = if c.is_ascii_uppercase() {
                mods.shift = true;
                (c.to_ascii_lowercase(), Some(c))
            } else if c.is_ascii_lowercase() && mods.shift {
                (c, Some(c.to_ascii_uppercase()))
            } else {
                (c, None)
            };
            // The text a kitty terminal would type: `S-a` types `A`.
            let typed = shifted.unwrap_or(c);
            let makes_text = !mods.ctrl && !mods.alt && !mods.super_;
            if makes_text && !all {
                return utf8(typed);
            }
            // ponytail: the shifted key is known only for ASCII letters
            // (a Key keeps no base layout key), so `!` goes as 33 rather
            // than kitty's 49 with shift.
            let mut s = format!("\x1b[{}", base as u32);
            if let (true, Some(sh)) = (flags & 4 != 0, shifted) {
                s.push_str(&format!(":{}", sh as u32));
            }
            let m = kitty_mod(mods);
            let text = (all && flags & 16 != 0 && makes_text).then_some(typed as u32);
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
        // Back-tab is shift+Tab to kitty.
        KeyCode::BackTab => csi_u(
            9,
            Mods {
                shift: true,
                ..mods
            },
        ),
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

/// A piece of paste content for a pane: with the bracketed-paste start
/// marker before the first piece and the end marker after the last when the
/// pane asked for mode 2004, as is otherwise.
pub fn encode_paste(data: &[u8], start: bool, end: bool, modes: &InputModes) -> Vec<u8> {
    let bracket = modes.bracketed_paste;
    let open: &[u8] = if bracket && start { PASTE_START } else { b"" };
    let close: &[u8] = if bracket && end { PASTE_END } else { b"" };
    [open, data, close].concat()
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

    fn whole(data: &[u8]) -> Input {
        Input::Paste {
            data: data.to_vec(),
            start: true,
            end: true,
        }
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
            // tmux keeps shift as written: these are keys of their own.
            ("C-S-a", "C-S-a"),
            ("S-a", "S-a"),
            ("M-S-a", "M-S-a"),
            ("S-Tab", "S-Tab"),
            ("C-S-A", "C-S-A"),
            ("S-BTab", "S-BTab"),
            ("C-BSpace", "C-BSpace"),
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

    /// tmux 3.7b lists C-A, C-S-a, S-a and A as four bindings, and S-Tab
    /// and BTab as two, so they stay distinct keys; only matching folds.
    #[test]
    fn shift_spellings_are_distinct_keys_that_fold_alike() {
        let distinct = [
            ("C-A", "C-S-a"),
            ("S-a", "A"),
            ("M-S-a", "M-A"),
            ("S-Tab", "BTab"),
        ];
        for (x, y) in distinct {
            assert_ne!(k(x), k(y), "{x} vs {y}");
            assert_eq!(k(x).folded(), k(y).folded(), "{x} folds like {y}");
            assert!(k(x).matches(&k(y)));
        }
        assert!(!k("S-Up").matches(&k("Up")));
        assert!(!k("a").matches(&k("A")));

        let mut t = KeyTables::default();
        t.bind(ROOT_TABLE, k("C-A"), argv(&["one"]), false);
        t.bind(ROOT_TABLE, k("C-S-a"), argv(&["two"]), false);
        assert_eq!(
            t.get(ROOT_TABLE, &k("C-A")).map(|b| &b.cmds[0][0][..]),
            Some("one")
        );
        assert_eq!(
            t.get(ROOT_TABLE, &k("C-S-a")).map(|b| &b.cmds[0][0][..]),
            Some("two")
        );
        t.bind(ROOT_TABLE, k("S-Tab"), argv(&["three"]), false);
        t.bind(ROOT_TABLE, k("BTab"), argv(&["four"]), false);
        assert_eq!(
            t.get(ROOT_TABLE, &k("S-Tab")).map(|b| &b.cmds[0][0][..]),
            Some("three")
        );
        assert_eq!(
            t.get(ROOT_TABLE, &k("BTab")).map(|b| &b.cmds[0][0][..]),
            Some("four")
        );
        assert!(t.unbind(ROOT_TABLE, &k("C-S-a")));
        assert_eq!(
            t.get(ROOT_TABLE, &k("C-A")).map(|b| &b.cmds[0][0][..]),
            Some("one")
        );
    }

    /// A binding matches whichever spelling the terminal sends.
    #[test]
    fn bindings_match_the_folded_spelling() {
        let mut t = KeyTables::default();
        t.bind(ROOT_TABLE, k("C-A"), argv(&["ca"]), false);
        t.bind(ROOT_TABLE, k("S-Tab"), argv(&["stab"]), false);
        t.bind(ROOT_TABLE, k("M-S-a"), argv(&["msa"]), false);
        t.bind(PREFIX_TABLE, k("BTab"), argv(&["btab"]), false);
        let got = |table, bytes: &[u8]| {
            let inputs = decode(bytes);
            let [Input::Key(key)] = inputs.as_slice() else {
                panic!("{bytes:?}");
            };
            t.get(table, key).map(|b| b.cmds[0][0].clone())
        };
        // kitty ctrl+shift+a (C-S-a) finds C-A.
        assert_eq!(got(ROOT_TABLE, b"\x1b[97;6u"), Some("ca".into()));
        // legacy back-tab (BTab) finds S-Tab; kitty shift+Tab finds BTab.
        assert_eq!(got(ROOT_TABLE, b"\x1b[Z"), Some("stab".into()));
        assert_eq!(got(PREFIX_TABLE, b"\x1b[9;2u"), Some("btab".into()));
        // A binding written S-Tab still catches kitty shift+Tab.
        assert_eq!(got(ROOT_TABLE, b"\x1b[9;2u"), Some("stab".into()));
        // legacy meta-A (M-A) finds M-S-a.
        assert_eq!(got(ROOT_TABLE, b"\x1bA"), Some("msa".into()));
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
            (b"\x1b[97;4u", "M-S-a"),
            (b"\x1b[97;6u", "C-S-a"),
            (b"\x1b[13;2u", "S-Enter"),
            (b"\x1b[13;5u", "C-Enter"),
            (b"\x1b[9;2u", "BTab"),
            (b"\x1b[9;6u", "C-BTab"),
            (b"\x1b[27;2;9~", "BTab"),
            (b"\x1b[27;3u", "M-Escape"),
            (b"\x1b[32;5u", "C-Space"),
            (b"\x1b[37;5u", "C-%"),
            // flag 4: shifted key; flag 8/16: all keys, with text.
            (b"\x1b[97:65;2u", "A"),
            (b"\x1b[49:33;2u", "!"),
            (b"\x1b[113u", "q"),
            (b"\x1b[97;;97u", "a"),
            (b"\x1b[97;2;65u", "S-a"),
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
        // Releases of functional keys use the legacy finals.
        assert_eq!(decode(b"\x1b[1;5:3A"), vec![]);
        assert_eq!(decode(b"\x1b[3;1:3~"), vec![]);
        assert_eq!(decode(b"\x1b[1;1:3P"), vec![]);
        assert_eq!(decode(b"\x1b[1;5:2A"), vec![key("C-Up")]);
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
    fn unknown_ss3_is_meta_o_and_the_next_byte_stays_a_key() {
        assert_eq!(decode(b"\x1bOz"), vec![key("M-O"), key("z")]);
        assert_eq!(decode(b"\x1bO\x1b[A"), vec![key("M-O"), key("Up")]);
    }

    #[test]
    fn broken_utf8_never_swallows_the_next_key() {
        assert_eq!(decode(b"\xc3a"), vec![key("a")]);
        assert_eq!(decode(b"\xe2\x82a"), vec![key("a")]);
        assert_eq!(decode(b"\xf0\x9f\x1b[A"), vec![key("Up")]);
        assert_eq!(decode(b"\xc3\xa9\xff\x01"), vec![key("é"), key("C-a")]);
        // Incomplete at the deadline: the lead and its continuations go.
        assert_eq!(decode(b"\xe2\x82"), vec![]);
    }

    #[test]
    fn paste_is_one_opaque_blob() {
        let got = decode(b"a\x1b[200~\x02%\x1b[A\x1b\x1b[201~b");
        assert_eq!(got, vec![key("a"), whole(b"\x02%\x1b[A\x1b"), key("b")]);
    }

    #[test]
    fn paste_split_across_reads_waits_for_its_end_marker() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[200~hello \x1b[20", t, ET), vec![]);
        // A paste does not end at escape-time; it waits 5 s for data.
        assert_eq!(d.deadline(ET), Some(t + PASTE_TIMEOUT));
        assert_eq!(d.flush(t + Duration::from_secs(1), ET), vec![]);
        let t1 = t + Duration::from_secs(4);
        assert_eq!(d.feed(b"1~x", t1, ET), vec![whole(b"hello "), key("x")]);
    }

    #[test]
    fn a_paste_without_its_end_marker_ends_after_5s_without_data() {
        let t = Instant::now();
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[200~abc", t, ET), vec![]);
        let t1 = t + Duration::from_secs(3);
        assert_eq!(d.feed(b"def", t1, ET), vec![]);
        // Each read restarts the wait.
        assert_eq!(d.deadline(ET), Some(t1 + PASTE_TIMEOUT));
        assert_eq!(d.flush(t + PASTE_TIMEOUT, ET), vec![]);
        assert_eq!(d.flush(t1 + PASTE_TIMEOUT, ET), vec![whole(b"abcdef")]);
        assert_eq!(d.deadline(ET), None);
        // Keys are keys again.
        assert_eq!(d.feed(b"\x02", t1 + PASTE_TIMEOUT, ET), vec![key("C-b")]);
    }

    /// Feeds 2.5 MiB of paste in 64 KiB reads with `end` as the last read
    /// (split across two reads), and returns the pane stream for a
    /// bracketed-paste pane.
    fn stream_long_paste(tail: &[&[u8]], flush_at: Option<Instant>) -> (Vec<u8>, Vec<u8>, Decoder) {
        let t = Instant::now();
        let mut d = Decoder::new();
        let mut state = KeyState::new();
        let bracketed = InputModes {
            bracketed_paste: true,
            ..InputModes::default()
        };
        let mut stream = Vec::new();
        let route = |out: Vec<Input>, state: &mut KeyState, stream: &mut Vec<u8>| {
            for i in out {
                assert!(matches!(i, Input::Paste { .. }), "{i:?}");
                let settings = KeySettings::default();
                match state.route(&i, t, &KeyTables::default(), &settings, &bracketed) {
                    Routed::ToPane(b) => stream.extend(b),
                    r => panic!("{r:?}"),
                }
            }
        };
        route(d.feed(PASTE_START, t, ET), &mut state, &mut stream);
        let chunk: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
        let mut sent = Vec::new();
        for _ in 0..40 {
            route(d.feed(&chunk, t, ET), &mut state, &mut stream);
            sent.extend_from_slice(&chunk);
            assert!(d.buf.len() <= PASTE_CAP + chunk.len(), "{}", d.buf.len());
        }
        for part in tail {
            route(d.feed(part, t, ET), &mut state, &mut stream);
        }
        if let Some(at) = flush_at {
            route(d.flush(at, ET), &mut state, &mut stream);
        }
        (stream, sent, d)
    }

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn a_long_paste_streams_as_one_bracketed_paste() {
        // The end marker is split across two reads.
        let (stream, mut sent, d) = stream_long_paste(&[b"tail\x1b[2", b"01~"], None);
        sent.extend_from_slice(b"tail");
        assert!(!d.in_paste);
        assert!(d.buf.is_empty());
        assert_eq!(count(&stream, PASTE_START), 1);
        assert_eq!(count(&stream, PASTE_END), 1);
        assert!(stream.starts_with(PASTE_START));
        assert!(stream.ends_with(PASTE_END));
        let body = &stream[PASTE_START.len()..stream.len() - PASTE_END.len()];
        assert_eq!(body.len(), sent.len());
        assert!(body == sent.as_slice());
    }

    #[test]
    fn a_long_paste_that_times_out_still_closes_its_bracket() {
        let late = Instant::now() + Duration::from_secs(60);
        let (stream, sent, d) = stream_long_paste(&[], Some(late));
        assert!(!d.in_paste);
        assert_eq!(count(&stream, PASTE_START), 1);
        assert_eq!(count(&stream, PASTE_END), 1);
        assert!(stream.ends_with(PASTE_END));
        let body = &stream[PASTE_START.len()..stream.len() - PASTE_END.len()];
        assert!(body == sent.as_slice());
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
        Routed::Command(vec![argv(words)], Dialect::Grofe)
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
        // tmux 3.7b (recorded through an attached client on a private
        // socket): the paste goes to the pane and the client is back in
        // root, so the next key is typing.
        assert_eq!(c.state.table(), ROOT_TABLE);
        assert_eq!(c.send(b"%", t), vec![pane(b"%")]);
        // A paste also ends a repeat.
        c.send(b"\x02\x1b[A", t);
        assert!(c.state.deadline().is_some());
        c.send(b"\x1b[200~x\x1b[201~", t);
        assert_eq!(c.state.deadline(), None);
        assert_eq!(c.send(b"\x1b[A", t), vec![pane(b"\x1b[A")]);
    }

    /// tmux 3.7b, through an attached client on a private socket: with both
    /// S-Tab and BTab bound, kitty `9;2u` fires BTab; unbound, it puts
    /// `\e[Z` on the pane.
    #[test]
    fn kitty_shift_tab_is_btab() {
        let t = Instant::now();
        let mut c = Client::new();
        c.tables
            .bind(ROOT_TABLE, k("S-Tab"), argv(&["stab"]), false);
        c.tables.bind(ROOT_TABLE, k("BTab"), argv(&["btab"]), false);
        assert_eq!(c.send(b"\x1b[9;2u", t), vec![cmd(&["btab"])]);
        assert_eq!(c.send(b"\x1b[27;2;9~", t), vec![cmd(&["btab"])]);
        let mut c = Client::new();
        assert_eq!(c.send(b"\x1b[9;2u", t), vec![pane(b"\x1b[Z")]);
    }

    #[test]
    fn kitty_ctrl_shift_letter_hits_a_prefix_written_c_s() {
        let t = Instant::now();
        let mut c = Client::new();
        c.settings.prefix = Some(k("C-S-a"));
        assert_eq!(
            c.send(b"\x1b[97;6u%", t),
            vec![Routed::Consumed, cmd(&["split-window", "-h"])]
        );
        c.settings.prefix = Some(k("C-A"));
        assert_eq!(c.send(b"\x1b[97;6u", t), vec![Routed::Consumed]);
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
    /// tmux-256color's terminfo, the same as screen-256color's for these
    /// (`infocmp -1x`: kcuu1 `\EOA` in keypad
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
            // Shift spellings, recorded the same way.
            ("S-a", b"a"),
            ("S-A", b"A"),
            ("M-S-a", b"\x1ba"),
            ("C-S-a", b"\x01"),
            ("C-S-A", b"\x01"),
            ("S-1", b"1"),
            ("S-Tab", b"\t"),
            ("C-S-Tab", b"\t"),
            ("M-S-Tab", b"\x1b\t"),
            ("S-BTab", b"\x1b[Z"),
            ("C-BTab", b"\x1b[Z"),
            ("S-BSpace", b"\x7f"),
            ("S-Escape", b"\x1b"),
            ("C-S-Enter", b"\r"),
        ];
        for (name, want) in cases {
            assert_eq!(enc(name, &m), want.to_vec(), "{name}");
        }
        // tmux cannot send these (its send-keys types them as text).
        for name in ["C-BSpace", "C-Escape", "C-M-BSpace", "C-M-Escape"] {
            assert_eq!(enc(name, &m), Vec::<u8>::new(), "{name}");
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
            ("S-Tab", b"\x1b[9;2u"),
            ("S-a", b"A"),
            ("C-S-a", b"\x1b[97;6u"),
            ("M-S-a", b"\x1b[97;4u"),
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
            ("S-a", b"\x1b[97;2u"),
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
        assert_eq!(enc("S-a", &modes(8 | 4 | 16)), b"\x1b[97:65;2;65u");
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
        assert_eq!(encode_paste(b"hi", true, true, &plain), b"hi");
        assert_eq!(
            encode_paste(b"hi", true, true, &bracketed),
            b"\x1b[200~hi\x1b[201~"
        );
        assert_eq!(encode_paste(b"hi", true, false, &bracketed), b"\x1b[200~hi");
        assert_eq!(encode_paste(b"hi", false, false, &bracketed), b"hi");
        assert_eq!(encode_paste(b"hi", false, true, &bracketed), b"hi\x1b[201~");
    }

    // ---- default key table (T2.5b) ----

    fn words(b: &Binding) -> Vec<&str> {
        assert_eq!(b.cmds.len(), 1, "one command");
        b.cmds[0].iter().map(String::as_str).collect()
    }

    /// tmux 3.7b's stock prefix bindings (`list-keys -T prefix` on a
    /// private server) for the commands M2 has, with new panes and windows
    /// in the active pane's cwd (spec §5.3).
    #[test]
    fn default_bindings_are_tmux_stock_for_the_m2_commands() {
        let t = default_bindings();
        let cases: &[(&str, &[&str], bool)] = &[
            (
                "%",
                &["split-window", "-h", "-c", "#{pane_current_path}"],
                false,
            ),
            ("\"", &["split-window", "-c", "#{pane_current_path}"], false),
            ("c", &["new-window", "-c", "#{pane_current_path}"], false),
            ("d", &["detach-client"], false),
            ("n", &["next-window"], false),
            ("p", &["previous-window"], false),
            ("l", &["last-window"], false),
            ("(", &["switch-client", "-p"], false),
            (")", &["switch-client", "-n"], false),
            ("L", &["switch-client", "-l"], false),
            ("z", &["resize-pane", "-Z"], false),
            ("0", &["select-window", "-t", ":0"], false),
            ("9", &["select-window", "-t", ":9"], false),
            ("C-b", &["send-prefix"], false),
            (":", &["command-prompt"], false),
            (
                ",",
                &["command-prompt", "-I", "#W", "rename-window \"%%\""],
                false,
            ),
            (
                "$",
                &["command-prompt", "-I", "#S", "rename-session \"%%\""],
                false,
            ),
            ("Up", &["select-pane", "-U"], true),
            ("Down", &["select-pane", "-D"], true),
            ("Left", &["select-pane", "-L"], true),
            ("Right", &["select-pane", "-R"], true),
            ("M-Up", &["resize-pane", "-U", "5"], true),
            ("M-Right", &["resize-pane", "-R", "5"], true),
            ("C-Down", &["resize-pane", "-D"], true),
            ("C-Left", &["resize-pane", "-L"], true),
        ];
        for (name, argv, repeat) in cases {
            let b = t
                .get(PREFIX_TABLE, &k(name))
                .unwrap_or_else(|| panic!("{name} unbound"));
            assert_eq!(words(b), *argv, "{name}");
            assert_eq!(b.repeat, *repeat, "{name} repeat");
        }
        // Nothing in root: every key there goes to the pane.
        assert!(t.get(ROOT_TABLE, &k("q")).is_none());
        // Stock keys grofe leaves unbound (out of scope, or M5's).
        for name in ["o", "m", "M", "t", "C", "<", ">", "*", "[", "]", "="] {
            assert!(t.get(PREFIX_TABLE, &k(name)).is_none(), "{name}");
        }
    }

    /// D10, recorded on tmux 3.7b (`list-keys -T prefix`, `list-keys -N`
    /// on a private `-f /dev/null` server): the stock keys whose verbs
    /// M3.6 adds, with tmux's notes. Formats tmux spells short (`#P`, `#W`)
    /// are native (`#{pane_index}`).
    #[test]
    fn default_bindings_add_the_m36_stock_keys_with_notes() {
        let t = default_bindings();
        let cases: &[(&str, &[&str], &str)] = &[
            (";", &["last-pane"], "Move to the previously active pane"),
            ("C-o", &["rotate-window"], "Rotate through the panes"),
            (
                "M-o",
                &["rotate-window", "-D"],
                "Rotate through the panes in reverse",
            ),
            ("q", &["display-panes"], "Display pane numbers"),
            (
                ".",
                &["command-prompt", "-T", "target", "move-window -t \"%%\""],
                "Move the current window",
            ),
            (
                "f",
                &["command-prompt", "find-window -Z \"%%\""],
                "Search for a pane",
            ),
            (
                "'",
                &[
                    "command-prompt",
                    "-T",
                    "window-target",
                    "-p",
                    "index",
                    "select-window -t \":%%\"",
                ],
                "Prompt for window index to select",
            ),
            ("C-z", &["suspend-client"], "Suspend the current client"),
            (
                "D",
                &["choose-client", "-Z"],
                "Choose and detach a client from a list",
            ),
            ("~", &["show-messages"], "Show messages"),
            ("E", &["select-layout", "-E"], "Spread panes out evenly"),
            (
                "M-n",
                &["next-window", "-a"],
                "Select the next window with an alert",
            ),
            (
                "M-p",
                &["previous-window", "-a"],
                "Select the previous window with an alert",
            ),
            (
                "/",
                &["command-prompt", "-k", "-p", "key", "list-keys -1N \"%%\""],
                "Describe key binding",
            ),
            (
                "x",
                &[
                    "confirm-before",
                    "-p",
                    "kill-pane #{pane_index}? (y/n)",
                    "kill-pane",
                ],
                "Kill the active pane",
            ),
            ("!", &["break-pane"], "Break pane to a new window"),
            (
                "}",
                &["swap-pane", "-D"],
                "Swap the active pane with the pane below",
            ),
            ("w", &["choose-tree", "-Zw"], "Choose a window from a list"),
            ("Space", &["next-layout"], "Select next layout"),
            ("?", &["list-keys", "-N"], "List key bindings"),
            (
                "M-5",
                &["select-layout", "tiled"],
                "Select the tiled layout",
            ),
            ("C-Left", &["resize-pane", "-L"], "Resize the pane left"),
            (
                "M-Up",
                &["resize-pane", "-U", "5"],
                "Resize the pane up by 5",
            ),
            (
                "Right",
                &["select-pane", "-R"],
                "Select the pane to the right of the active pane",
            ),
        ];
        for (name, argv, note) in cases {
            let b = t
                .get(PREFIX_TABLE, &k(name))
                .unwrap_or_else(|| panic!("{name} unbound"));
            assert_eq!(words(b), *argv, "{name}");
            assert_eq!(b.desc.as_deref(), Some(*note), "{name}");
        }
        // Every default carries a note, as every tmux stock binding does.
        let unnoted: Vec<String> = t
            .keys(PREFIX_TABLE)
            .into_iter()
            .filter(|key| t.exact(PREFIX_TABLE, key).is_some_and(|b| b.desc.is_none()))
            .map(|key| key.to_string())
            .collect();
        assert!(unnoted.is_empty(), "{unnoted:?}");
    }

    /// tmux 3.7b's mouse key names parse, case-insensitively, with
    /// modifiers, and print back in tmux's spelling.
    #[test]
    fn mouse_key_names_parse_and_print_as_tmux_does() {
        for (name, shown) in [
            ("MouseDown1Pane", "MouseDown1Pane"),
            ("mousedown1status", "MouseDown1Status"),
            ("MouseDown3Pane", "MouseDown3Pane"),
            ("MouseDrag1Border", "MouseDrag1Border"),
            ("MouseDragEnd1Pane", "MouseDragEnd1Pane"),
            ("MouseUp2StatusLeft", "MouseUp2StatusLeft"),
            ("MouseDown1StatusRight", "MouseDown1StatusRight"),
            ("MouseDown1StatusDefault", "MouseDown1StatusDefault"),
            ("WheelUpPane", "WheelUpPane"),
            ("WheelDownStatus", "WheelDownStatus"),
            ("DoubleClick1Pane", "DoubleClick1Pane"),
            ("TripleClick1Pane", "TripleClick1Pane"),
            ("SecondClick1Pane", "SecondClick1Pane"),
            ("M-MouseDown3Pane", "M-MouseDown3Pane"),
            ("C-MouseDown1Status", "C-MouseDown1Status"),
            // tmux 3.7's names, recorded binding on a private server.
            ("MouseDown1ScrollbarUp", "MouseDown1ScrollbarUp"),
            ("MouseDrag1ScrollbarSlider", "MouseDrag1ScrollbarSlider"),
            ("MouseDown1ScrollbarDown", "MouseDown1ScrollbarDown"),
            ("WheelUpScrollbarUp", "WheelUpScrollbarUp"),
            ("MouseDown1Control0", "MouseDown1Control0"),
            ("mousedown1control9", "MouseDown1Control9"),
            ("MouseUp3Control5", "MouseUp3Control5"),
        ] {
            let key = parse_key(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(matches!(key.code, KeyCode::Mouse(_)), "{name}");
            assert_eq!(key.to_string(), shown, "{name}");
        }
        assert_eq!(
            k("MouseDown1Pane").code,
            KeyCode::Mouse(MouseKey {
                action: MouseAction::Down,
                button: 1,
                place: MousePlace::Pane,
            })
        );
        for bad in [
            "MouseDown1",
            "MouseDownPane",
            "MouseDown0Pane",
            "MouseDown12Pane",
            "WheelUp1Pane",
            "MouseDown1Window",
            // tmux 3.7b: `unknown key: MouseDown1Control10`.
            "MouseDown1Control10",
            "MouseDown1Control",
        ] {
            assert_eq!(parse_key(bad), Err(format!("bad key: {bad}")), "{bad}");
        }
        // A mouse key is never bytes for a pane.
        let key = k("MouseDown1Pane");
        assert!(encode(&key, &InputModes::default()).is_empty());
        assert!(encode(&key, &modes(1 | 8)).is_empty());
    }

    /// D4, recorded on tmux 3.7b through an attached client: kitty `9;4u`
    /// (M-S-Tab), `9;6u` (C-S-Tab) and `\e\e[Z` (M-BTab) reach a legacy pane
    /// as nothing; `9;2u` (S-Tab) and `\e[Z` as `\e[Z`.
    #[test]
    fn back_tab_with_ctrl_or_meta_sends_a_legacy_pane_nothing() {
        let t = Instant::now();
        let mut c = Client::new();
        assert_eq!(c.send(b"\x1b[9;4u", t), vec![Routed::Consumed]);
        assert_eq!(c.send(b"\x1b[9;6u", t), vec![Routed::Consumed]);
        assert_eq!(c.send(b"\x1b[9;2u", t), vec![pane(b"\x1b[Z")]);
        assert_eq!(c.send(b"\x1b[Z", t), vec![pane(b"\x1b[Z")]);
        // `send-keys M-BTab` still sends `\e[Z`, as tmux's does.
        assert_eq!(
            encode(&k("M-BTab"), &InputModes::default()),
            b"\x1b[Z".to_vec()
        );
    }

    #[test]
    fn every_default_binding_parses_as_a_command() {
        let t = default_bindings();
        for argv in t
            .tables
            .values()
            .flat_map(|t| t.values())
            .flat_map(|b| &b.cmds)
        {
            if let Err(e) = crate::cmd::parse(argv) {
                panic!("{argv:?}: {e}");
            }
        }
    }

    /// R12: a binding may be a sequence; `bind` is the one-command case.
    #[test]
    fn a_sequence_binding_routes_every_command() {
        let t = Instant::now();
        let mut c = Client::new();
        c.tables.bind_seq(
            PREFIX_TABLE,
            k("Z"),
            vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
            false,
        );
        assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
        assert_eq!(
            c.send(b"Z", t),
            vec![Routed::Command(
                vec![argv(&["split-window"]), argv(&["select-pane", "-L"])],
                Dialect::Grofe
            )]
        );
        let b = c.tables.get(PREFIX_TABLE, &k("%")).expect("bound");
        assert_eq!(b.cmds, vec![argv(&["split-window", "-h"])]);
    }

    /// The stub edit: a binding's commands route in its own dialect.
    #[test]
    fn a_binding_routes_in_its_own_dialect() {
        let t = Instant::now();
        let mut c = Client::new();
        let mut b = Binding::new(vec![argv(&["neww"])], false, BindingSource::Runtime);
        b.dialect = Dialect::Tmux;
        c.tables.insert(ROOT_TABLE, k("F5"), b);
        assert_eq!(
            c.send(b"\x1b[15~", t),
            vec![Routed::Command(vec![argv(&["neww"])], Dialect::Tmux)]
        );
    }

    fn config_key(
        table: &str,
        key: &str,
        cmds: &[&[&str]],
        line: usize,
    ) -> crate::config::KeyBinding {
        crate::config::KeyBinding {
            table: table.to_string(),
            key: k(key),
            command: String::new(),
            cmds: cmds.iter().map(|c| argv(c)).collect(),
            desc: None,
            group: None,
            name: Some(format!("n-{key}")),
            repeat: false,
            source: crate::config::Source {
                layer: crate::config::Layer::Config,
                file: Some(PathBuf::from("/c/config.toml")),
                line,
            },
        }
    }

    /// The config's bindings go over the defaults; an empty one unbinds
    /// (R11); `reload` replaces config bindings and keeps runtime ones and
    /// runtime unbinds (R10).
    #[test]
    fn config_tables_layer_over_defaults_and_reload_keeps_runtime() {
        let t = KeyTables::from_config(&[
            config_key(PREFIX_TABLE, "X", &[&["split-window", "-h"]], 3),
            config_key(PREFIX_TABLE, "%", &[], 4),
        ]);
        let x = t.exact(PREFIX_TABLE, &k("X")).expect("X");
        assert_eq!(
            x.source,
            BindingSource::File {
                path: PathBuf::from("/c/config.toml"),
                line: 3
            }
        );
        assert_eq!(x.name.as_deref(), Some("n-X"));
        assert!(t.exact(PREFIX_TABLE, &k("%")).is_none());
        assert_eq!(
            t.exact(PREFIX_TABLE, &k("c")).map(|b| &b.source),
            Some(&BindingSource::Default)
        );

        let mut live = t;
        live.insert(
            PREFIX_TABLE,
            k("y"),
            Binding::new(vec![argv(&["new-window"])], false, BindingSource::Runtime),
        );
        live.unbind_runtime(PREFIX_TABLE, &k("c"));
        let fresh = KeyTables::from_config(&[]);
        let t = live.with_config_from(fresh);
        assert!(t.exact(PREFIX_TABLE, &k("X")).is_none(), "config X gone");
        assert!(t.exact(PREFIX_TABLE, &k("%")).is_some(), "stock % back");
        assert!(t.exact(PREFIX_TABLE, &k("y")).is_some(), "runtime y kept");
        assert!(
            t.exact(PREFIX_TABLE, &k("c")).is_none(),
            "runtime unbind kept"
        );
    }

    #[test]
    fn default_arrows_repeat_without_the_prefix() {
        let t = Instant::now();
        let mut c = Client::new();
        c.tables = default_bindings();
        assert_eq!(c.send(b"\x02", t), vec![Routed::Consumed]);
        assert_eq!(c.send(b"\x1b[D", t), vec![cmd(&["select-pane", "-L"])]);
        // Within repeat-time, another arrow needs no prefix.
        assert_eq!(c.send(b"\x1b[A", t), vec![cmd(&["select-pane", "-U"])]);
        // A key that does not repeat ends the repeat and goes to the pane.
        assert_eq!(c.send(b"x", t), vec![pane(b"x")]);
    }
}
