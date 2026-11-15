use super::*;

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
    /// tmux's `Any`: bound in a table, it takes every key that table
    /// does not bind (see [`KeyTables::lookup`]); never typed or sent.
    Any,
    /// A numeric keypad key (`KP5`, `KPEnter`), which a terminal in
    /// application keypad mode sends as its own sequence.
    Keypad(Keypad),
    /// A printable ASCII character given as `0xNN`: tmux makes such a key a
    /// key of its own (shown as the character, never typed), so a binding
    /// on it is not one on the character itself.
    Literal(char),
    /// A C0 control code as a key of its own (`[NUL]`, `[LF]`): tmux has
    /// names for them to show in logs, and `0x01` makes one. Never `Tab`,
    /// `Enter` or `Escape`, which are keys of theirs.
    Control(u8),
    /// tmux's `User0` to `User1000`, the keys the `user-keys` option names
    /// terminal sequences for.
    User(u16),
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

/// tmux's numeric keypad keys, in its key-code order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keypad {
    Slash,
    Star,
    Minus,
    Seven,
    Eight,
    Nine,
    Plus,
    Four,
    Five,
    Six,
    One,
    Two,
    Three,
    Enter,
    Zero,
    Period,
}

/// Each keypad key with tmux's name, the byte it types in a pane that has
/// not asked for application keypad mode, and the final byte of the `ESC O`
/// sequence it types in one that has (`input_key_table`).
pub(super) const KEYPAD: [(Keypad, &str, u8, u8); 16] = [
    (Keypad::Slash, "KP/", b'/', b'o'),
    (Keypad::Star, "KP*", b'*', b'j'),
    (Keypad::Minus, "KP-", b'-', b'm'),
    (Keypad::Seven, "KP7", b'7', b'w'),
    (Keypad::Eight, "KP8", b'8', b'x'),
    (Keypad::Nine, "KP9", b'9', b'y'),
    (Keypad::Plus, "KP+", b'+', b'k'),
    (Keypad::Four, "KP4", b'4', b't'),
    (Keypad::Five, "KP5", b'5', b'u'),
    (Keypad::Six, "KP6", b'6', b'v'),
    (Keypad::One, "KP1", b'1', b'q'),
    (Keypad::Two, "KP2", b'2', b'r'),
    (Keypad::Three, "KP3", b'3', b's'),
    (Keypad::Enter, "KPEnter", b'\n', b'M'),
    (Keypad::Zero, "KP0", b'0', b'p'),
    (Keypad::Period, "KP.", b'.', b'n'),
];

impl Keypad {
    pub(crate) fn row(self) -> &'static (Keypad, &'static str, u8, u8) {
        // KEYPAD lists every variant.
        KEYPAD.iter().find(|r| r.0 == self).unwrap_or(&KEYPAD[0])
    }

    /// tmux's name for the key.
    pub fn name(self) -> &'static str {
        self.row().1
    }

    /// Where the key sorts among tmux's key codes, from 0.
    pub fn order(self) -> u32 {
        KEYPAD.iter().position(|r| r.0 == self).unwrap_or(0) as u32
    }

    /// The key the `ESC O <final>` sequence of an application keypad is.
    pub fn from_ss3(final_byte: u8) -> Option<Keypad> {
        KEYPAD.iter().find(|r| r.3 == final_byte).map(|r| r.0)
    }
}

/// tmux's names for the C0 control codes (`[NUL]` to `[US]`, `Tab`, `Enter`
/// and `Escape` among them), by code; `ASC` is tmux's spelling of ACK.
pub(super) const C0_NAMES: [&str; 32] = [
    "[NUL]", "[SOH]", "[STX]", "[ETX]", "[EOT]", "[ENQ]", "[ASC]", "[BEL]", "[BS]", "Tab", "[LF]",
    "[VT]", "[FF]", "Enter", "[SO]", "[SI]", "[DLE]", "[DC1]", "[DC2]", "[DC3]", "[DC4]", "[NAK]",
    "[SYN]", "[ETB]", "[CAN]", "[EM]", "[SUB]", "Escape", "[FS]", "[GS]", "[RS]", "[US]",
];

/// The most `UserN` tmux numbers (`KEYC_NUSER`).
pub const MAX_USER_KEY: u16 = 1000;

/// The key C0 code `code` is: `Tab`, `Enter` and `Escape` are keys of
/// their own.
pub(super) fn control_key(code: u8) -> KeyCode {
    match code {
        9 => KeyCode::Tab,
        13 => KeyCode::Enter,
        27 => KeyCode::Escape,
        code => KeyCode::Control(code),
    }
}

pub(super) const MOUSE_ACTIONS: &[(&str, MouseAction)] = &[
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

pub(super) const MOUSE_PLACES: &[(&str, MousePlace)] = &[
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
    pub(super) fn parse(name: &str) -> Option<MouseKey> {
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
    /// A command sequence to parse when it runs (a prompt's, a menu
    /// item's), each command's argv, for dispatch in order (R12), in the
    /// dialect it was written in (spec §4.5). An alias expands then.
    Command(Vec<Vec<String>>, Dialect),
    /// A key binding's stored commands, as [`Routed::Command`] but with
    /// aliases already expanded when the key was bound, as tmux stores a
    /// parsed command list: a press does not expand them again.
    Bound(Vec<Vec<String>>, Dialect),
    /// Bytes for the active pane, encoded for its `InputModes`.
    ToPane(Vec<u8>),
    /// Eaten (a prefix key, an overlay's key).
    Consumed,
}

/// A hook that sees every input of every client before the key tables
/// (the prompt, the mouse; registered in `register_builtins`). `None`
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

impl Mouse {
    /// tmux's `m->b`: the button (0 to 2, 3 for a release), 32 for motion,
    /// 64 for the wheel, and 4, 8, 16 for Shift, Meta and Ctrl.
    pub fn code(&self) -> u32 {
        let button = |b: MouseButton| match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Other(n) => 128 + u32::from(n.saturating_sub(8)),
        };
        let base = match self.kind {
            MouseKind::Down(b) => button(b),
            MouseKind::Up(_) => 3,
            MouseKind::Drag(b) => 32 + button(b),
            MouseKind::Move => 35,
            MouseKind::WheelUp => 64,
            MouseKind::WheelDown => 65,
            MouseKind::WheelLeft => 66,
            MouseKind::WheelRight => 67,
        };
        base + 4 * u32::from(self.mods.shift)
            + 8 * u32::from(self.mods.alt)
            + 16 * u32::from(self.mods.ctrl)
    }
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

pub(super) const ESC: u8 = 0x1b;

pub(super) fn ctrl() -> Mods {
    Mods {
        ctrl: true,
        ..Mods::default()
    }
}

/// Named keys, tmux's canonical spelling first, then accepted aliases
/// (tmux 3.7b's `key_string_table`, matched case-insensitively).
pub(super) const NAMES: &[(&str, KeyCode)] = &[
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
    ("Any", KeyCode::Any),
];

/// Parse a tmux key name (`C-b`, `M-Up`, `^a`, `PPage`, `%`), as tmux 3.7b
/// accepts it: modifier prefixes `C-`, `M-`, `S-` in any case and order,
/// named keys and mouse keys (`MouseDown1Pane`, [`MouseKey`])
/// case-insensitively. The error is tmux's `bad key: X`.
// ponytail: tmux's keypad names (KP5, KPEnter) are not parsed; KeyCode
// has no keypad variants.
pub fn parse_key(name: &str) -> Result<Key, String> {
    let bad = || format!("bad key: {name}");
    // `0xNN`: a code point, or below 0x20 a C0 control code. No modifiers
    // go with it, and what follows the digits is ignored (`sscanf`).
    if let Some(hex) = name.strip_prefix("0x") {
        let digits = hex.bytes().take_while(u8::is_ascii_hexdigit).count();
        let code = u32::from_str_radix(&hex[..digits], 16).map_err(|_| bad())?;
        let code = match u8::try_from(code) {
            Ok(c) if c < 32 => control_key(c),
            _ => match char::from_u32(code).ok_or_else(bad)? {
                c if c.is_ascii() => KeyCode::Literal(c),
                c => KeyCode::Char(c),
            },
        };
        return Ok(Key::plain(code));
    }
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
    } else if let Some(row) = KEYPAD.iter().find(|r| r.1.eq_ignore_ascii_case(rest)) {
        KeyCode::Keypad(row.0)
    } else if let Some(c) = C0_NAMES
        .iter()
        .position(|n| n.starts_with('[') && n.eq_ignore_ascii_case(rest))
    {
        KeyCode::Control(c as u8)
    } else if let Some(m) = MouseKey::parse(rest) {
        KeyCode::Mouse(m)
    } else if let Some(n) = user_key_number(rest) {
        KeyCode::User(n)
    } else {
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => KeyCode::Char(c),
            _ => return Err(bad()),
        }
    };
    Ok(Key { code, mods })
}

/// `UserN`, N up to [`MAX_USER_KEY`]: tmux reads it with `sscanf("User%u")`,
/// which takes the digits and leaves what follows them.
pub(super) fn user_key_number(s: &str) -> Option<u16> {
    let digits = s.strip_prefix("User")?;
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    let n: u32 = digits[..end].parse().ok()?;
    u16::try_from(n).ok().filter(|n| *n <= MAX_USER_KEY)
}

/// `F1` to `F12` (tmux has no F13 and up).
pub(super) fn function_key_number(s: &str) -> Option<u8> {
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
            KeyCode::Literal(c) => write!(f, "{c}"),
            KeyCode::Keypad(k) => f.write_str(k.name()),
            KeyCode::Control(c) => f.write_str(C0_NAMES[usize::from(c) & 31]),
            // tmux writes it into a 7 byte buffer: `User1000` is `User100`.
            KeyCode::User(n) => {
                let name = format!("User{n}");
                f.write_str(&name[..name.len().min(7)])
            }
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
