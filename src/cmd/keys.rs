//! Key-name table for `send-keys` and (from M3) key bindings (spec §4.5).
//!
//! `key_bytes` never looks at `modes.kitty_flags`: M1 always encodes keys
//! in the legacy (non-kitty) style, even for a pane that has requested the
//! kitty keyboard protocol.
//! ponytail: kitty keyboard protocol encoding is M2 T2.3's job.

use crate::types::InputModes;

/// Whether `name` is a key `send-keys` recognises (a single printable
/// character, a named key like `Enter`, or a `C-`/`M-`/`S-` combo) as
/// opposed to literal text (spec §4.5: "each argument that matches a key
/// name ... is sent as that key, otherwise as literal text"). `-l` skips
/// this check entirely and forces literal.
pub fn is_key_name(name: &str) -> bool {
    key_bytes(name, &InputModes::default()).is_some()
}

/// The bytes a physical key press sends into a pane, honouring `modes`
/// where a key's encoding depends on it (the arrow keys: SS3 in
/// `app_cursor` mode, CSI otherwise). `None` if `name` isn't a recognised
/// key.
pub fn key_bytes(name: &str, modes: &InputModes) -> Option<Vec<u8>> {
    let mut ctrl = false;
    let mut meta = false;
    let mut shift = false;
    let mut rest = name;
    loop {
        if let Some(r) = rest.strip_prefix("C-") {
            ctrl = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-") {
            meta = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-") {
            shift = true;
            rest = r;
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return None;
    }

    let base = if ctrl {
        if shift {
            return None; // C-S- combos on a plain char aren't meaningful.
        }
        ctrl_byte(rest)?
    } else if let Some(bytes) = arrow_bytes(rest, shift, modes) {
        bytes
    } else if shift {
        return None; // S- is only supported for the arrow keys.
    } else {
        named_or_char_bytes(rest)?
    };

    Some(if meta {
        let mut v = Vec::with_capacity(base.len() + 1);
        v.push(0x1b);
        v.extend(base);
        v
    } else {
        base
    })
}

/// `C-<x>`: the control byte for a single ASCII letter, or one of the
/// handful of punctuation characters with a defined control code (`@ [ \
/// ] ^ _`). `None` for anything else (multi-character, or a character
/// with no control form).
fn ctrl_byte(rest: &str) -> Option<Vec<u8>> {
    let mut chars = rest.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if c.is_ascii_alphabetic() {
        return Some(vec![(c.to_ascii_uppercase() as u8) & 0x1f]);
    }
    match c {
        '@' | '[' | '\\' | ']' | '^' | '_' => Some(vec![(c as u8) & 0x1f]),
        _ => None,
    }
}

/// `Up`/`Down`/`Left`/`Right`, honouring `app_cursor` (SS3 vs CSI) and
/// `shift` (the extended CSI modifier form; xterm sends CSI, never SS3,
/// for a modified arrow key even in application-cursor mode).
fn arrow_bytes(rest: &str, shift: bool, modes: &InputModes) -> Option<Vec<u8>> {
    let letter = match rest {
        "Up" => 'A',
        "Down" => 'B',
        "Right" => 'C',
        "Left" => 'D',
        _ => return None,
    };
    Some(if shift {
        format!("\x1b[1;2{letter}").into_bytes()
    } else if modes.app_cursor {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[{letter}").into_bytes()
    })
}

/// Everything `key_bytes` knows besides the arrows: `Enter`, `Tab`,
/// `BSpace`, `Escape`, `Space`, `Home`/`End`, `PageUp`/`PageDown`
/// (`PPage`/`NPage` aliases), `Insert`/`Delete`, `F1`-`F12`, and a single
/// printable character sent as itself.
fn named_or_char_bytes(rest: &str) -> Option<Vec<u8>> {
    let named: &[u8] = match rest {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "BSpace" => b"\x7f",
        "Escape" => b"\x1b",
        "Space" => b" ",
        "Home" => b"\x1b[1~",
        "End" => b"\x1b[4~",
        "PageUp" | "PPage" => b"\x1b[5~",
        "PageDown" | "NPage" => b"\x1b[6~",
        "Insert" => b"\x1b[2~",
        "Delete" => b"\x1b[3~",
        "F1" => b"\x1bOP",
        "F2" => b"\x1bOQ",
        "F3" => b"\x1bOR",
        "F4" => b"\x1bOS",
        "F5" => b"\x1b[15~",
        "F6" => b"\x1b[17~",
        "F7" => b"\x1b[18~",
        "F8" => b"\x1b[19~",
        "F9" => b"\x1b[20~",
        "F10" => b"\x1b[21~",
        "F11" => b"\x1b[23~",
        "F12" => b"\x1b[24~",
        _ => return single_char_bytes(rest),
    };
    Some(named.to_vec())
}

/// A single printable character sent as its own UTF-8 bytes: `send-keys
/// a` is equivalent to typing the `a` key.
fn single_char_bytes(rest: &str) -> Option<Vec<u8>> {
    let mut chars = rest.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_control() {
        return None;
    }
    let mut buf = [0u8; 4];
    Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(app_cursor: bool) -> InputModes {
        InputModes {
            app_cursor,
            ..InputModes::default()
        }
    }

    #[test]
    fn printable_single_char() {
        assert_eq!(key_bytes("a", &modes(false)), Some(b"a".to_vec()));
        assert_eq!(key_bytes("Z", &modes(false)), Some(b"Z".to_vec()));
        assert_eq!(key_bytes("5", &modes(false)), Some(b"5".to_vec()));
    }

    #[test]
    fn multibyte_single_char() {
        assert_eq!(key_bytes("é", &modes(false)), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn named_control_keys() {
        assert_eq!(key_bytes("Enter", &modes(false)), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("Tab", &modes(false)), Some(b"\t".to_vec()));
        assert_eq!(key_bytes("BSpace", &modes(false)), Some(vec![0x7f]));
        assert_eq!(key_bytes("Escape", &modes(false)), Some(vec![0x1b]));
        assert_eq!(key_bytes("Space", &modes(false)), Some(b" ".to_vec()));
    }

    #[test]
    fn arrows_use_csi_by_default() {
        assert_eq!(key_bytes("Up", &modes(false)), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes("Down", &modes(false)), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_bytes("Right", &modes(false)), Some(b"\x1b[C".to_vec()));
        assert_eq!(key_bytes("Left", &modes(false)), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn arrows_use_ss3_in_app_cursor_mode() {
        assert_eq!(key_bytes("Up", &modes(true)), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes("Down", &modes(true)), Some(b"\x1bOB".to_vec()));
        assert_eq!(key_bytes("Right", &modes(true)), Some(b"\x1bOC".to_vec()));
        assert_eq!(key_bytes("Left", &modes(true)), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn shift_arrow_uses_extended_csi_regardless_of_app_cursor() {
        assert_eq!(
            key_bytes("S-Up", &modes(false)),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(key_bytes("S-Up", &modes(true)), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(
            key_bytes("S-Left", &modes(false)),
            Some(b"\x1b[1;2D".to_vec())
        );
    }

    #[test]
    fn shift_is_not_supported_outside_arrows() {
        assert_eq!(key_bytes("S-a", &modes(false)), None);
        assert_eq!(key_bytes("S-Enter", &modes(false)), None);
    }

    #[test]
    fn home_and_end() {
        assert_eq!(key_bytes("Home", &modes(false)), Some(b"\x1b[1~".to_vec()));
        assert_eq!(key_bytes("End", &modes(false)), Some(b"\x1b[4~".to_vec()));
    }

    #[test]
    fn page_up_and_down_and_their_aliases() {
        assert_eq!(
            key_bytes("PageUp", &modes(false)),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(key_bytes("PPage", &modes(false)), Some(b"\x1b[5~".to_vec()));
        assert_eq!(
            key_bytes("PageDown", &modes(false)),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(key_bytes("NPage", &modes(false)), Some(b"\x1b[6~".to_vec()));
    }

    #[test]
    fn insert_and_delete() {
        assert_eq!(
            key_bytes("Insert", &modes(false)),
            Some(b"\x1b[2~".to_vec())
        );
        assert_eq!(
            key_bytes("Delete", &modes(false)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        let cases: &[(&str, &[u8])] = &[
            ("F1", b"\x1bOP"),
            ("F2", b"\x1bOQ"),
            ("F3", b"\x1bOR"),
            ("F4", b"\x1bOS"),
            ("F5", b"\x1b[15~"),
            ("F6", b"\x1b[17~"),
            ("F7", b"\x1b[18~"),
            ("F8", b"\x1b[19~"),
            ("F9", b"\x1b[20~"),
            ("F10", b"\x1b[21~"),
            ("F11", b"\x1b[23~"),
            ("F12", b"\x1b[24~"),
        ];
        for (name, want) in cases {
            assert_eq!(
                key_bytes(name, &modes(false)),
                Some(want.to_vec()),
                "{name}"
            );
        }
    }

    #[test]
    fn ctrl_letters() {
        assert_eq!(key_bytes("C-a", &modes(false)), Some(vec![0x01]));
        assert_eq!(key_bytes("C-c", &modes(false)), Some(vec![0x03]));
        assert_eq!(key_bytes("C-A", &modes(false)), Some(vec![0x01]));
        assert_eq!(key_bytes("C-z", &modes(false)), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_punctuation() {
        assert_eq!(key_bytes("C-[", &modes(false)), Some(vec![0x1b]));
        assert_eq!(key_bytes("C-_", &modes(false)), Some(vec![0x1f]));
    }

    #[test]
    fn ctrl_on_multi_char_is_none() {
        assert_eq!(key_bytes("C-Enter", &modes(false)), None);
    }

    #[test]
    fn meta_prefixes_escape() {
        assert_eq!(key_bytes("M-a", &modes(false)), Some(vec![0x1b, b'a']));
        assert_eq!(key_bytes("M-Enter", &modes(false)), Some(vec![0x1b, b'\r']));
    }

    #[test]
    fn meta_and_ctrl_combo() {
        // M-C-a: meta wraps whatever the rest of the combo produces.
        assert_eq!(key_bytes("M-C-a", &modes(false)), Some(vec![0x1b, 0x01]));
    }

    #[test]
    fn unknown_name_is_none() {
        assert_eq!(key_bytes("Nonsense", &modes(false)), None);
        assert_eq!(key_bytes("", &modes(false)), None);
        assert_eq!(key_bytes("hello", &modes(false)), None);
    }

    #[test]
    fn is_key_name_matches_key_bytes_success() {
        assert!(is_key_name("Enter"));
        assert!(is_key_name("a"));
        assert!(is_key_name("C-c"));
        assert!(is_key_name("Up"));
        assert!(!is_key_name("hello"));
        assert!(!is_key_name(""));
    }
}
