//! The escape sequences `capture-pane -e` writes between cells, as tmux
//! 3.7b's `grid_string_cells_code` makes them (recorded, `golden/b4rest/
//! capture-sgr`): only what changed since the cell before, a `0` first when
//! an attribute goes, colours as `38;5;n`, `38;2;r;g;b` or the plain codes,
//! an attribute code of ten or more as `a:b`, and hyperlinks as OSC 8.

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// What a cell looks like to the capture.
#[derive(Clone, PartialEq)]
pub struct Pen {
    attrs: Flags,
    fg: Option<Color>,
    bg: Option<Color>,
    us: Option<Color>,
    /// The hyperlink's id (empty when the program gave none) and URI.
    link: Option<(String, String)>,
}

/// Attribute flags in the order tmux writes their codes.
const ATTRS: [(Flags, u32); 11] = [
    (Flags::BOLD, 1),
    (Flags::DIM, 2),
    (Flags::ITALIC, 3),
    (Flags::UNDERLINE, 4),
    (Flags::INVERSE, 7),
    (Flags::HIDDEN, 8),
    (Flags::STRIKEOUT, 9),
    (Flags::DOUBLE_UNDERLINE, 42),
    (Flags::UNDERCURL, 43),
    (Flags::DOTTED_UNDERLINE, 44),
    (Flags::DASHED_UNDERLINE, 45),
];

impl Default for Pen {
    fn default() -> Pen {
        Pen {
            attrs: Flags::empty(),
            fg: None,
            bg: None,
            us: None,
            link: None,
        }
    }
}

impl Pen {
    pub fn of(cell: &Cell) -> Pen {
        let default = |c: Color, d: NamedColor| (c != Color::Named(d)).then_some(c);
        Pen {
            attrs: cell.flags & ATTRS.iter().fold(Flags::empty(), |a, (f, _)| a | *f),
            fg: default(cell.fg, NamedColor::Foreground),
            bg: default(cell.bg, NamedColor::Background),
            us: cell.underline_color(),
            link: cell.hyperlink().map(|h| {
                let id = h.id();
                // alacritty names a link the program left unnamed.
                let id = if id.ends_with("_alacritty") { "" } else { id };
                (id.to_string(), h.uri().to_string())
            }),
        }
    }
}

fn codes(color: Option<Color>, base: u32) -> Vec<u32> {
    match color {
        Some(Color::Indexed(n)) => vec![base + 8, 5, u32::from(n)],
        Some(Color::Spec(c)) => vec![base + 8, 2, u32::from(c.r), u32::from(c.g), u32::from(c.b)],
        Some(Color::Named(n)) => {
            let (bright, i) = match n {
                NamedColor::Black | NamedColor::DimBlack => (false, 0),
                NamedColor::Red | NamedColor::DimRed => (false, 1),
                NamedColor::Green | NamedColor::DimGreen => (false, 2),
                NamedColor::Yellow | NamedColor::DimYellow => (false, 3),
                NamedColor::Blue | NamedColor::DimBlue => (false, 4),
                NamedColor::Magenta | NamedColor::DimMagenta => (false, 5),
                NamedColor::Cyan | NamedColor::DimCyan => (false, 6),
                NamedColor::White | NamedColor::DimWhite => (false, 7),
                NamedColor::BrightBlack => (true, 0),
                NamedColor::BrightRed => (true, 1),
                NamedColor::BrightGreen => (true, 2),
                NamedColor::BrightYellow => (true, 3),
                NamedColor::BrightBlue => (true, 4),
                NamedColor::BrightMagenta => (true, 5),
                NamedColor::BrightCyan => (true, 6),
                NamedColor::BrightWhite => (true, 7),
                _ => return vec![base + 9],
            };
            // Foreground 30 and 90; background 40 and 100.
            vec![if bright { base + 60 + i } else { base + i }]
        }
        None => vec![base + 9],
    }
}

/// `grid_string_cells_add_code`: the codes for one colour, if it changed
/// (or an attribute reset wiped it) and is not the default after a reset.
fn add_colour(out: &mut String, new: &[u32], old: &[u32], reset: bool) {
    if new.is_empty() || (!reset && new == old) {
        return;
    }
    if reset && (new[0] == 49 || new[0] == 39) {
        return;
    }
    out.push_str("\x1b[");
    let words: Vec<String> = new.iter().map(u32::to_string).collect();
    out.push_str(&words.join(";"));
    out.push('m');
}

fn hyperlink(out: &mut String, id: &str, uri: &str) {
    out.push_str("\x1b]8;");
    if id.is_empty() {
        out.push(';');
    } else {
        out.push_str(&format!("id={id};"));
    }
    out.push_str(uri);
    out.push_str("\x1b\\");
}

/// The code to write before a cell looking like `cur` after one looking like
/// `last`. `has_link` tells whether a link is open on this line.
pub fn code(last: &Pen, cur: &Pen, has_link: &mut bool) -> String {
    let mut out = String::new();
    let mut s: Vec<u32> = Vec::new();
    let mut lastattr = last.attrs;
    let removed = ATTRS
        .iter()
        .any(|(f, _)| !cur.attrs.contains(*f) && lastattr.contains(*f))
        || (last.us.is_some() && cur.us.is_none());
    if removed {
        s.push(0);
        lastattr = Flags::empty();
    }
    for (flag, code) in ATTRS {
        if cur.attrs.contains(flag) && !lastattr.contains(flag) {
            s.push(code);
        }
    }
    if !s.is_empty() {
        out.push_str("\x1b[");
        let words: Vec<String> = s
            .iter()
            .map(|c| {
                if *c < 10 {
                    c.to_string()
                } else {
                    format!("{}:{}", c / 10, c % 10)
                }
            })
            .collect();
        out.push_str(&words.join(";"));
        out.push('m');
    }
    let reset = s.first() == Some(&0);
    add_colour(&mut out, &codes(cur.fg, 30), &codes(last.fg, 30), reset);
    add_colour(&mut out, &codes(cur.bg, 40), &codes(last.bg, 40), reset);
    // Only the 256-colour and RGB forms of the underline colour.
    let us = |c: Option<Color>| match c {
        Some(Color::Indexed(n)) => vec![58, 5, u32::from(n)],
        Some(Color::Spec(c)) => vec![58, 2, u32::from(c.r), u32::from(c.g), u32::from(c.b)],
        _ => Vec::new(),
    };
    add_colour(&mut out, &us(cur.us), &us(last.us), reset);
    if last.link != cur.link {
        match &cur.link {
            Some((id, uri)) => {
                hyperlink(&mut out, id, uri);
                *has_link = true;
            }
            None if *has_link => {
                hyperlink(&mut out, "", "");
                *has_link = false;
            }
            None => {}
        }
    }
    out
}

/// The sequence that closes a link a line leaves open.
pub fn close_link() -> String {
    let mut out = String::new();
    hyperlink(&mut out, "", "");
    out
}
