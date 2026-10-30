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
    classic_key_bytes(name, modes).or_else(|| extended_key_bytes(name, modes))
}

/// The keys the table below has no row for, read as the key bindings read a
/// name (`input::parse_key`) and encoded as a typed key is: the keypad
/// (`KP5`), C0 codes (`[LF]`), `UserN` (no pane gets one: no bytes, but a
/// key all the same) and `0xNN`.
fn extended_key_bytes(name: &str, modes: &InputModes) -> Option<Vec<u8>> {
    use crate::server::input::{KeyCode, encode, parse_key};
    let key = parse_key(name).ok()?;
    let extended = name.starts_with("0x")
        || matches!(
            key.code,
            KeyCode::Keypad(_) | KeyCode::Control(_) | KeyCode::User(_)
        );
    extended.then(|| encode(&key, modes))
}

fn classic_key_bytes(name: &str, modes: &InputModes) -> Option<Vec<u8>> {
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

    // xterm's modifier parameter, for the named keys with a defined
    // modified CSI form: arrows, Home/End, Insert/Delete, PageUp/PageDown
    // and F1-F12 (verified against `infocmp -1x xterm-256color`: kUP5,
    // kLFT5, kHOM5, kEND5, kDC5, kNXT5, kPRV5, and the F-key modified
    // forms). 2 Shift, 3 Meta, 5 Ctrl; combinations add (C-S = 6, C-M = 7,
    // C-M-S = 8). This already encodes meta, so a hit here returns
    // directly with no further ESC-prefixing.
    let modifier = if shift || meta || ctrl {
        Some(1 + shift as u8 + 2 * meta as u8 + 4 * ctrl as u8)
    } else {
        None
    };
    if let Some(bytes) = modified_key_bytes(rest, modifier, modes) {
        return Some(bytes);
    }

    let base = if ctrl {
        if shift {
            return None; // C-S- combos on a plain char aren't meaningful.
        }
        ctrl_byte(rest)?
    } else if shift {
        // tmux has no modified-CSI form for Tab: S-Tab collapses to plain
        // Tab (verified on a live tmux 3.7b). `BTab`, the separate named
        // back-tab key, is handled by named_or_char_bytes below.
        if rest == "Tab" {
            named_or_char_bytes(rest)?
        } else {
            return None; // S- is only supported for the modifier-aware keys above.
        }
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

/// The named keys with a modifier-aware xterm CSI form. `modifier` is
/// `None` for the unmodified key, which keeps its existing encoding
/// (matching screen-256color: SS3 for arrows in `app_cursor` mode, CSI
/// otherwise; vt220 tilde forms for the rest), or `Some(n)` for xterm's
/// modifier parameter (see [`key_bytes`]). `None` return means `rest`
/// isn't one of these keys.
fn modified_key_bytes(rest: &str, modifier: Option<u8>, modes: &InputModes) -> Option<Vec<u8>> {
    if let Some(letter) = arrow_letter(rest) {
        return Some(match modifier {
            Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
            None if modes.app_cursor => format!("\x1bO{letter}").into_bytes(),
            None => format!("\x1b[{letter}").into_bytes(),
        });
    }
    if let Some(letter) = f1_to_f4_letter(rest) {
        return Some(match modifier {
            Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
            None => format!("\x1bO{letter}").into_bytes(),
        });
    }
    if rest == "Home" {
        return Some(match modifier {
            Some(m) => format!("\x1b[1;{m}H").into_bytes(),
            None => b"\x1b[1~".to_vec(),
        });
    }
    if rest == "End" {
        return Some(match modifier {
            Some(m) => format!("\x1b[1;{m}F").into_bytes(),
            None => b"\x1b[4~".to_vec(),
        });
    }
    if let Some(num) = tilde_num(rest) {
        return Some(match modifier {
            Some(m) => format!("\x1b[{num};{m}~").into_bytes(),
            None => format!("\x1b[{num}~").into_bytes(),
        });
    }
    None
}

/// `Up`/`Down`/`Left`/`Right`'s CSI/SS3 final letter.
fn arrow_letter(rest: &str) -> Option<char> {
    match rest {
        "Up" => Some('A'),
        "Down" => Some('B'),
        "Right" => Some('C'),
        "Left" => Some('D'),
        _ => None,
    }
}

/// `F1`-`F4`'s SS3/modified-CSI final letter (`F5`-`F12` use the tilde
/// form instead, via [`tilde_num`]).
fn f1_to_f4_letter(rest: &str) -> Option<char> {
    match rest {
        "F1" => Some('P'),
        "F2" => Some('Q'),
        "F3" => Some('R'),
        "F4" => Some('S'),
        _ => None,
    }
}

/// The leading number in the vt220 tilde form (`\x1b[<num>~`, or
/// `\x1b[<num>;<mod>~` when modified), shared by Insert/Delete/
/// PageUp/PageDown and F5-F12.
fn tilde_num(rest: &str) -> Option<u16> {
    match rest {
        "Insert" => Some(2),
        "Delete" => Some(3),
        "PageUp" | "PPage" => Some(5),
        "PageDown" | "NPage" => Some(6),
        "F5" => Some(15),
        "F6" => Some(17),
        "F7" => Some(18),
        "F8" => Some(19),
        "F9" => Some(20),
        "F10" => Some(21),
        "F11" => Some(23),
        "F12" => Some(24),
        _ => None,
    }
}

/// `C-<x>`: the control byte for a single ASCII letter, one of the handful
/// of punctuation characters with a defined control code (`@ [ \ ] ^ _`,
/// `@` giving NUL, same as `C-Space`'s own special case below), or the
/// named `Space` key. `None` for anything else (multi-character, or a
/// character with no control form).
fn ctrl_byte(rest: &str) -> Option<Vec<u8>> {
    // C-Space: no defined control code for a plain space, but tmux sends
    // NUL, same as its C-@ alias (verified on a live tmux 3.7b).
    if rest == "Space" {
        return Some(vec![0]);
    }
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

/// Everything `key_bytes` knows besides the modifier-aware named keys
/// (handled by [`modified_key_bytes`]): `Enter`, `Tab`, `BSpace`,
/// `Escape`, `Space`, and a single printable character sent as itself.
fn named_or_char_bytes(rest: &str) -> Option<Vec<u8>> {
    let named: &[u8] = match rest {
        "Enter" => b"\r",
        "Tab" => b"\t",
        // Back-tab, tmux's own named key for Shift-Tab (`kcbt` in
        // `infocmp -1x xterm-256color`; verified on a live tmux 3.7b).
        "BTab" => b"\x1b[Z",
        "BSpace" => b"\x7f",
        "Escape" => b"\x1b",
        "Space" => b" ",
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
        assert!(is_key_name("C-Up"));
        assert!(!is_key_name("hello"));
        assert!(!is_key_name(""));
    }

    /// xterm's modified-key CSI forms (spec: "Ctrl (and Shift/Meta
    /// combos) with named keys"), verified against `infocmp -1x
    /// xterm-256color`: kUP5, kLFT5, kHOM5, kEND5, kDC5, kNXT5, kPRV5 and
    /// the F-key modified forms. The modifier parameter is 2 Shift, 3
    /// Meta, 5 Ctrl, and combinations add (C-S = 6, C-M = 7, C-M-S = 8).
    #[test]
    fn modified_named_keys_use_xterms_csi_forms() {
        let cases: &[(&str, &[u8])] = &[
            // Arrows: infocmp kUP/kDN/kRIT/kLFT (2) and kUP5/kDN5/kRIT5/
            // kLFT5 (5), kUP6..7 etc for combos.
            ("S-Up", b"\x1b[1;2A"),
            ("C-Up", b"\x1b[1;5A"),
            ("C-Down", b"\x1b[1;5B"),
            ("C-Right", b"\x1b[1;5C"),
            ("C-Left", b"\x1b[1;5D"),
            ("C-S-Up", b"\x1b[1;6A"),
            ("C-M-Up", b"\x1b[1;7A"),
            // Home/End: kHOM/kEND (2), kHOM5/kEND5 (5).
            ("S-Home", b"\x1b[1;2H"),
            ("C-Home", b"\x1b[1;5H"),
            ("S-End", b"\x1b[1;2F"),
            ("C-End", b"\x1b[1;5F"),
            // Insert/Delete: kIC/kDC (2), kIC5/kDC5 (5).
            ("S-Insert", b"\x1b[2;2~"),
            ("C-Insert", b"\x1b[2;5~"),
            ("S-Delete", b"\x1b[3;2~"),
            ("C-Delete", b"\x1b[3;5~"),
            // PageUp/PageDown: kPRV/kNXT (2), kPRV5/kNXT5 (5).
            ("S-PageUp", b"\x1b[5;2~"),
            ("C-PageUp", b"\x1b[5;5~"),
            ("S-PageDown", b"\x1b[6;2~"),
            ("C-PageDown", b"\x1b[6;5~"),
            // F1-F4 (SS3 unmodified, CSI modified): kf13 (S-F1), kf25
            // (C-F1).
            ("S-F1", b"\x1b[1;2P"),
            ("C-F1", b"\x1b[1;5P"),
            ("C-F4", b"\x1b[1;5S"),
            // F5-F12 (tilde form): kf17 (S-F5), kf29 (C-F5), kf36
            // (C-F12).
            ("S-F5", b"\x1b[15;2~"),
            ("C-F5", b"\x1b[15;5~"),
            ("C-F12", b"\x1b[24;5~"),
        ];
        for (name, want) in cases {
            assert_eq!(
                key_bytes(name, &modes(false)),
                Some(want.to_vec()),
                "{name}"
            );
        }
    }

    /// Verified on a live tmux 3.7b (`tmux -f /dev/null -L <private socket>`,
    /// send-keys into a raw pty pane, `od -An -tx1` on what it received):
    /// tmux has no modified-CSI form for Tab, so `S-Tab` collapses to plain
    /// Tab (0x09); `BTab` is the separate named back-tab key and sends
    /// `\x1b[Z` (matches `kcbt` in `infocmp -1x xterm-256color`); `C-Space`
    /// and its `C-@` alias both send NUL.
    #[test]
    fn s_tab_and_c_space_match_real_tmux() {
        assert_eq!(key_bytes("S-Tab", &modes(false)), Some(b"\t".to_vec()));
        assert_eq!(key_bytes("BTab", &modes(false)), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key_bytes("C-Space", &modes(false)), Some(vec![0x00]));
        assert_eq!(key_bytes("C-@", &modes(false)), Some(vec![0x00]));
    }

    #[test]
    fn unmodified_named_keys_unchanged_by_the_modifier_table() {
        // The unmodified encodings stay exactly as before (screen-256color).
        assert_eq!(key_bytes("Home", &modes(false)), Some(b"\x1b[1~".to_vec()));
        assert_eq!(key_bytes("End", &modes(false)), Some(b"\x1b[4~".to_vec()));
        assert_eq!(
            key_bytes("Insert", &modes(false)),
            Some(b"\x1b[2~".to_vec())
        );
        assert_eq!(
            key_bytes("Delete", &modes(false)),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(key_bytes("F1", &modes(false)), Some(b"\x1bOP".to_vec()));
        assert_eq!(key_bytes("F5", &modes(false)), Some(b"\x1b[15~".to_vec()));
    }
}
