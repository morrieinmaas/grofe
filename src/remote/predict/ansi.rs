//! The escape sequences the predictor reads from typed input and writes to the terminal.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::{Key, Pos};

pub(super) fn push_cell(out: &mut Vec<u8>, cell: &alacritty_terminal::term::cell::Cell) {
    let mut utf8 = [0; 4];
    out.extend(cell.c.encode_utf8(&mut utf8).bytes());
    for z in cell.zerowidth().into_iter().flatten() {
        out.extend(z.encode_utf8(&mut utf8).bytes());
    }
}

pub(super) const DEFAULT_FG: Color = Color::Named(NamedColor::Foreground);
pub(super) const DEFAULT_BG: Color = Color::Named(NamedColor::Background);
/// The attributes `sgr` can express.
pub(super) const PAINTED: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// The key at the start of `buf` (an ESC) and how many bytes it takes; None if the sequence
/// is cut off by the end of `buf`.
pub(super) fn escape(buf: &[u8]) -> Option<(Key, usize)> {
    match buf.get(1)? {
        b'[' => {
            let mut j = 2;
            while buf.get(j).is_some_and(|b| (0x20..=0x3f).contains(b)) {
                j += 1;
            }
            buf.get(j)?;
            let key = match &buf[2..=j] {
                b"D" => Key::Left,
                b"C" => Key::Right,
                b"200~" => Key::Paste(true),
                b"201~" => Key::Paste(false),
                _ => Key::Flush,
            };
            Some((key, j + 1))
        }
        b'O' => {
            let key = match buf.get(2)? {
                b'D' => Key::Left,
                b'C' => Key::Right,
                _ => Key::Flush,
            };
            Some((key, 3))
        }
        _ => Some((Key::Flush, 2)),
    }
}

pub(super) fn cup((row, col): Pos) -> Vec<u8> {
    format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
}

pub(super) fn sgr(flags: Flags, fg: Color, bg: Color) -> String {
    let mut s = String::from("\x1b[0");
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if flags.contains(flag) {
            s.push(';');
            s.push_str(code);
        }
    }
    color(&mut s, fg, 30);
    color(&mut s, bg, 40);
    s.push('m');
    s
}

/// `base` is 30 for foreground, 40 for background.
fn color(s: &mut String, c: Color, base: u8) {
    use std::fmt::Write;
    let _ = match c {
        Color::Named(n) => match n as usize {
            i @ 0..=7 => write!(s, ";{}", base + i as u8),
            i @ 8..=15 => write!(s, ";{}", base + 60 + (i - 8) as u8),
            _ => return,
        },
        Color::Indexed(i) => write!(s, ";{};5;{i}", base + 8),
        Color::Spec(rgb) => write!(s, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b),
    };
}
