use super::*;

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
pub(super) fn xterm_mod(m: Mods) -> Option<u8> {
    let bits = (m.shift as u8) | ((m.alt as u8) << 1) | ((m.ctrl as u8) << 2);
    (bits != 0).then_some(1 + bits)
}

/// The kitty modifier parameter, `1` when unmodified.
pub(super) fn kitty_mod(m: Mods) -> u8 {
    1 + ((m.shift as u8) | ((m.alt as u8) << 1) | ((m.ctrl as u8) << 2) | ((m.super_ as u8) << 3))
}

/// How a functional key is spelled: `CSI 1;m X` / `CSI X` (`letter`), or
/// `CSI n;m ~` (`tilde`).
pub(super) enum Functional {
    Letter(u8),
    Tilde(u8),
}

/// A functional key's modified (CSI) form. F1-F4 use letters as xterm does,
/// except kitty's F3, which is `13 ~` because `CSI R` is a cursor report.
pub(super) fn functional(code: KeyCode, kitty: bool) -> Option<Functional> {
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

pub(super) fn functional_bytes(f: Functional, param: Option<u8>) -> Vec<u8> {
    match (f, param) {
        (Functional::Letter(l), None) => vec![ESC, b'[', l],
        (Functional::Letter(l), Some(m)) => format!("\x1b[1;{m}{}", l as char).into_bytes(),
        (Functional::Tilde(n), None) => format!("\x1b[{n}~").into_bytes(),
        (Functional::Tilde(n), Some(m)) => format!("\x1b[{n};{m}~").into_bytes(),
    }
}

/// Legacy C0 mapping of ctrl with an ASCII key (kitty spec table, which
/// matches tmux 3.7b: `C-/` is 0x1f, `C-?` 0x7f, `C-1` stays `1`).
pub(super) fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        ' ' | '@' | '2' => 0,
        'a'..='z' | 'A'..='Z' => (c as u8) & 0x1f,
        '[' | '3' => 27,
        '\\' | '4' => 28,
        ']' | '5' => 29,
        '^' | '6' | '~' => 30,
        '_' | '/' | '7' => 31,
        '8' | '?' => 127,
        // tmux 3.7b folds a shifted US key to its base key first: `C-!` is
        // `C-1` (so `1`), `C-"` is `'`, `C-:` is `;`, `C-{` is `C-[`.
        '!' => b'1',
        '(' => b'9',
        ')' => b'0',
        '"' | '\'' => b'\'',
        '<' | ',' => b',',
        '>' | '.' => b'.',
        ':' => b';',
        '-' => 31,
        '`' => 0,
        '{' => 27,
        '|' => 28,
        '}' => 29,
        _ => return None,
    })
}

pub(super) fn utf8(c: char) -> Vec<u8> {
    let mut b = [0u8; 4];
    c.encode_utf8(&mut b).as_bytes().to_vec()
}

/// What tmux-256color panes expect, as tmux 3.7b sends it: modifiers a
/// legacy form cannot carry are dropped (`C-Enter` is `\r`, `S-a` is `a`,
/// `S-Tab` is `\t`), and keys tmux cannot send at all (`C-BSpace`,
/// `C-Escape`: its send-keys types them as text) send nothing.
// ponytail: no modifyOtherKeys or tmux `extended-keys` encodings; a pane
// that wants richer keys pushes kitty flags.
pub(super) fn legacy_encode(key: &Key, modes: &InputModes) -> Vec<u8> {
    let m = key.mods;
    let mut out = if m.alt { vec![ESC] } else { Vec::new() };
    match key.code {
        // Ctrl with these has no form: tmux's send-keys types `C-%` as text.
        KeyCode::Char('#' | '$' | '%' | '&' | '*') if m.ctrl => return Vec::new(),
        KeyCode::Char(c) => match m.ctrl.then(|| ctrl_byte(c)).flatten() {
            Some(b) => out.push(b),
            None => out.extend(utf8(c)),
        },
        KeyCode::Enter => out.push(b'\r'),
        // A keypad key: the byte of the key itself, or in a pane that asked
        // for application keypad mode its `ESC O` sequence. tmux sends
        // nothing for one with Ctrl or Shift.
        KeyCode::Keypad(_) if m.ctrl || m.shift => return Vec::new(),
        KeyCode::Keypad(k) if modes.app_keypad => out.extend([ESC, b'O', k.row().3]),
        KeyCode::Keypad(k) => out.push(k.row().2),
        // A C0 code goes as itself, as `input_key_vt10x` writes it, whatever
        // the modifiers (recorded on tmux 3.7b: `C-[LF]` and `S-[LF]` are `\n`).
        KeyCode::Control(c) => out.push(c),
        // User keys are for bindings; no pane gets them.
        KeyCode::User(_) => return Vec::new(),
        KeyCode::Literal(c) => out.push(c as u8),
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
pub(super) fn kitty_encode(key: &Key, modes: &InputModes, flags: u8) -> Vec<u8> {
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
        KeyCode::Keypad(_) | KeyCode::Control(_) | KeyCode::User(_) | KeyCode::Literal(_) => {
            legacy_encode(key, modes)
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
