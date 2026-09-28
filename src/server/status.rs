//! render status.left/right formats to a StyledLine.
//!
//! T2.0 lands the output types; T2.6 fills in rendering (spec §4.10).

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::server::pane::CellFlags;

/// How a run of status text looks: the same colour and attribute vocabulary
/// as a pane cell ([`crate::server::pane::SnapCell`]), so the renderer draws
/// status runs and pane cells the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    /// Bold, dim, italic, underline, reverse, strikeout and the rest.
    pub attrs: CellFlags,
    /// As [`crate::server::pane::SnapCell::underline_color`]: `None`
    /// underlines in `fg`.
    pub underline_color: Option<Color>,
}

impl Default for Style {
    /// The terminal's default colours, no attributes.
    fn default() -> Self {
        Style {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            attrs: CellFlags::empty(),
            underline_color: None,
        }
    }
}

/// A run of text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One status row, left to right, already laid out for the width it was
/// rendered for: left part, window list, padding and right part are all
/// spans. The renderer draws it from column 0 and clips at the client width.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledLine {
    pub spans: Vec<Span>,
}
