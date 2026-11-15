//! Key names for `send-keys` (spec §4.5). One encoder: the names and bytes are
//! those of `server::input::{parse_key, encode}`, which the key bindings and
//! typed keys use too; the unit tests pin what tmux 3.7b sends.

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
/// where a key's encoding depends on it. `None` if `name` is not a key
/// tmux's `send-keys` sends: it types the name as literal text instead
/// (`F13`, `C-BSpace`, a mouse key). The names and bytes are those of the key
/// bindings and typed keys (`input::parse_key`, `input::encode`).
pub fn key_bytes(name: &str, modes: &InputModes) -> Option<Vec<u8>> {
    use crate::server::input::{KeyCode, encode, parse_key};
    let key = parse_key(name).ok()?;
    let bytes = encode(&key, modes);
    // A key with no bytes still is a key when tmux sends it (`User1`, `Any`,
    // `C-KP5`: nothing reaches the pane); the others it types as text.
    let sends_nothing = match key.code {
        KeyCode::User(_) | KeyCode::Keypad(_) => true,
        KeyCode::Any => key.mods == Default::default(),
        _ => false,
    };
    (!bytes.is_empty() || sends_nothing).then_some(bytes)
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

    /// What tmux 3.7b's `send-keys` put into a raw pty pane (`od -tx1`), on
    /// a private `-L` server with `-f /dev/null`. `None` is a name tmux
    /// types as literal text instead of sending a key.
    #[test]
    fn matches_real_tmux_bytes() {
        let cases: &[(&str, Option<&[u8]>)] = &[
            ("C-Enter", Some(b"\r")),
            ("S-Enter", Some(b"\r")),
            ("C-S-Enter", Some(b"\r")),
            ("M-C-Enter", Some(b"\x1b\r")),
            ("M-Enter", Some(b"\x1b\r")),
            ("S-a", Some(b"a")),
            ("s-a", Some(b"a")),
            ("M-S-a", Some(b"\x1ba")),
            ("C-S-a", Some(b"\x01")),
            ("C-1", Some(b"1")),
            ("S-1", Some(b"1")),
            ("S-!", Some(b"!")),
            ("C-2", Some(&[0])),
            ("C-8", Some(&[0x7f])),
            ("C-/", Some(&[0x1f])),
            ("C-?", Some(&[0x7f])),
            ("C-~", Some(&[0x1e])),
            ("C-%", None),
            ("C-#", None),
            ("C-*", None),
            ("C-!", Some(b"1")),
            ("C-:", Some(b";")),
            ("C-{", Some(&[0x1b])),
            ("C-|", Some(&[0x1c])),
            ("C--", Some(&[0x1f])),
            ("C-`", Some(&[0])),
            ("C-3", Some(&[0x1b])),
            ("C-M-1", Some(b"\x1b1")),
            ("C-0x41", None),
            ("C-\u{e9}", Some("\u{e9}".as_bytes())),
            ("M-\u{e9}", Some(b"\x1b\xc3\xa9")),
            ("F13", None),
            ("M-F13", None),
            ("C-F13", None),
            ("C-BSpace", None),
            ("C-Escape", None),
            ("C-Any", None),
            ("MouseDown1Pane", None),
            ("WheelUpPane", None),
            ("S-BSpace", Some(&[0x7f])),
            ("M-BSpace", Some(b"\x1b\x7f")),
            ("M-Escape", Some(b"\x1b\x1b")),
            ("S-Escape", Some(b"\x1b")),
            ("S-Space", Some(b" ")),
            ("M-Space", Some(b"\x1b ")),
            ("C-Space", Some(&[0])),
            ("C-Tab", Some(b"\t")),
            ("M-Tab", Some(b"\x1b\t")),
            ("M-S-Tab", Some(b"\x1b\t")),
            ("S-Tab", Some(b"\t")),
            ("M-BTab", Some(b"\x1b[Z")),
            ("C-BTab", Some(b"\x1b[Z")),
            ("C-M-a", Some(b"\x1b\x01")),
            ("KP5", Some(b"5")),
            ("KPEnter", Some(b"\n")),
            ("KP+", Some(b"+")),
            ("C-KP5", Some(b"")),
            ("S-KP5", Some(b"")),
            ("C-KPEnter", Some(b"")),
            ("C-KP+", Some(b"")),
            ("M-KP5", Some(b"\x1b5")),
            ("User1", Some(b"")),
            ("M-User1", Some(b"")),
            ("Any", Some(b"")),
            ("[LF]", Some(b"\n")),
            ("S-[LF]", Some(b"\n")),
            ("C-[LF]", Some(b"\n")),
            ("M-[LF]", Some(b"\x1b\n")),
            ("0x41", Some(b"A")),
            ("0x1", Some(&[1])),
            ("0x7f", Some(&[0x7f])),
            ("0x100", Some(b"\xc4\x80")),
            ("^a", Some(&[1])),
            ("^[", Some(&[0x1b])),
            ("c-a", Some(&[1])),
            ("m-a", Some(b"\x1ba")),
            ("f1", Some(b"\x1bOP")),
            ("enter", Some(b"\r")),
            ("S-F1", Some(b"\x1b[1;2P")),
            ("M-F5", Some(b"\x1b[15;3~")),
            ("F12", Some(b"\x1b[24~")),
            ("M-C-Up", Some(b"\x1b[1;7A")),
            ("C-S-Up", Some(b"\x1b[1;6A")),
            ("M-Up", Some(b"\x1b[1;3A")),
            ("S-Home", Some(b"\x1b[1;2H")),
            ("S-End", Some(b"\x1b[1;2F")),
            ("C-Home", Some(b"\x1b[1;5H")),
            ("C-IC", Some(b"\x1b[2;5~")),
            ("S-IC", Some(b"\x1b[2;2~")),
            ("C-PgUp", Some(b"\x1b[5;5~")),
            ("S-NPage", Some(b"\x1b[6;2~")),
        ];
        for (name, want) in cases {
            assert_eq!(
                key_bytes(name, &modes(false)),
                want.map(<[u8]>::to_vec),
                "{name}"
            );
        }
    }

    #[test]
    fn is_key_name_follows_the_same_split() {
        assert!(is_key_name("C-Enter"));
        assert!(is_key_name("User1"));
        assert!(!is_key_name("F13"));
        assert!(!is_key_name("C-BSpace"));
    }
}
