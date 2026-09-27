//! ids, Rect, Dir, Edge, SplitSize, CaptureBound, CaptureOpts, MouseEncoding,
//! InputModes.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Error returned when a runtime id fails to parse.
#[derive(Debug, thiserror::Error)]
#[error("invalid {kind} id: {raw:?}")]
pub struct IdParseError {
    kind: &'static str,
    raw: String,
}

/// Defines a runtime id newtype (`$N`/`@N`/`%N`) with Display, FromStr and
/// serde (via the Display/FromStr round trip) impls.
macro_rules! runtime_id {
    ($name:ident, $sigil:literal, $kind:literal) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(into = "String", try_from = "String")]
        pub struct $name(pub u32);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let invalid = || IdParseError {
                    kind: $kind,
                    raw: s.to_string(),
                };
                let digits = s.strip_prefix($sigil).ok_or_else(invalid)?;
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                digits.parse::<u32>().map($name).map_err(|_| invalid())
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.to_string()
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdParseError;

            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }
    };
}

runtime_id!(SessionId, "$", "session");
runtime_id!(WindowId, "@", "window");
runtime_id!(PaneId, "%", "pane");

/// A rectangular region of the client's screen, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

/// The direction a layout cell is split in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Horizontal,
    Vertical,
}

/// A direction relative to a pane, used for directional pane selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Up,
    Down,
}

/// The size given to a new split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitSize {
    Half,
    Percent(u8),
    Cells(u16),
}

/// One end of a `capture-pane` line range, as tmux's `-S` / `-E` take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBound {
    /// A line number: 0 is the first visible line, negative lines are
    /// history (-1 is the line just above the visible screen).
    Line(i32),
    /// `-S -`: the oldest line of scrollback history.
    HistoryStart,
    /// `-E -`: the last line of the visible screen.
    VisibleEnd,
}

/// Options for `capture-pane`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOpts {
    /// `-S`; `None` is tmux's default, the first visible line.
    pub start: Option<CaptureBound>,
    /// `-E`; `None` is tmux's default, the last visible line.
    pub end: Option<CaptureBound>,
    pub escapes: bool,
}

/// The mouse reporting mode a pane's terminal has requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    Click,
    Drag,
    Motion,
}

/// How mouse reports are encoded: legacy X10 bytes, UTF-8 extended (mode
/// 1005) or SGR (mode 1006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    #[default]
    X10,
    Utf8,
    Sgr,
}

/// The terminal input modes a pane's emulator has requested, used to decode
/// client input into the right escape sequences for that pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputModes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub kitty_flags: u8,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub mouse_encoding: MouseEncoding,
    pub focus: bool,
    /// The pane is on the alternate screen (mode 1049/47/1047).
    pub alt_screen: bool,
    /// Wheel events become cursor keys on the alternate screen (mode 1007).
    pub alternate_scroll: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_id_round_trips() {
        assert_eq!("%3".parse::<PaneId>().unwrap(), PaneId(3));
        assert_eq!(PaneId(3).to_string(), "%3");
    }

    #[test]
    fn window_id_round_trips() {
        assert_eq!("@2".parse::<WindowId>().unwrap(), WindowId(2));
        assert_eq!(WindowId(2).to_string(), "@2");
    }

    #[test]
    fn session_id_round_trips() {
        assert_eq!("$1".parse::<SessionId>().unwrap(), SessionId(1));
        assert_eq!(SessionId(1).to_string(), "$1");
    }

    #[test]
    fn rejects_missing_sigil() {
        assert!("3".parse::<PaneId>().is_err());
    }

    #[test]
    fn rejects_wrong_sigil() {
        assert!("@3".parse::<PaneId>().is_err());
        assert!("$3".parse::<PaneId>().is_err());
    }

    #[test]
    fn rejects_non_digits() {
        assert!("%x3".parse::<PaneId>().is_err());
        assert!("%3x".parse::<PaneId>().is_err());
        assert!("%".parse::<PaneId>().is_err());
        assert!("%-3".parse::<PaneId>().is_err());
    }

    #[test]
    fn rejects_overflow() {
        let too_big = format!("%{}", u64::from(u32::MAX) + 1);
        assert!(too_big.parse::<PaneId>().is_err());
    }

    #[test]
    fn error_message_is_clear() {
        let err = "x3".parse::<PaneId>().unwrap_err();
        assert_eq!(err.to_string(), r#"invalid pane id: "x3""#);
    }

    #[test]
    fn ids_serialize_as_display_string() {
        assert_eq!(serde_json::to_string(&PaneId(4)).unwrap(), "\"%4\"");
        assert_eq!(serde_json::from_str::<PaneId>("\"%4\"").unwrap(), PaneId(4));
    }

    #[test]
    fn ids_deserialize_reject_invalid_string() {
        assert!(serde_json::from_str::<PaneId>("\"@4\"").is_err());
    }

    #[test]
    fn mouse_mode_defaults_to_off() {
        assert_eq!(MouseMode::default(), MouseMode::Off);
    }

    #[test]
    fn input_modes_default_is_off_and_false() {
        let modes = InputModes::default();
        assert_eq!(modes.mouse, MouseMode::Off);
        assert_eq!(modes.mouse_encoding, MouseEncoding::X10);
        assert!(!modes.app_cursor);
        assert!(!modes.bracketed_paste);
        assert!(!modes.alt_screen);
        assert!(!modes.alternate_scroll);
    }

    #[test]
    fn mouse_encoding_defaults_to_x10() {
        assert_eq!(MouseEncoding::default(), MouseEncoding::X10);
    }

    #[test]
    fn ids_order_by_number() {
        let mut ids = vec![PaneId(10), PaneId(2), PaneId(7)];
        ids.sort();
        assert_eq!(ids, [PaneId(2), PaneId(7), PaneId(10)]);
        assert!(WindowId(1) < WindowId(2));
        assert!(SessionId(3) > SessionId(0));
    }

    #[test]
    fn capture_bounds_distinguish_dash_from_line_numbers() {
        // `-S -` / `-E -` are their own variants, not a line-number sentinel.
        assert_ne!(CaptureBound::HistoryStart, CaptureBound::Line(i32::MIN));
        assert_ne!(CaptureBound::VisibleEnd, CaptureBound::Line(i32::MAX));
    }
}
