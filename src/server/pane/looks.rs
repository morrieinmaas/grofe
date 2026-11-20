//! Interned cell looks: everything about a cell except its character.

use std::collections::HashMap;

use alacritty_terminal::term::cell::Hyperlink;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use super::{CellFlags, SnapCell};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LookKey {
    fg: u32,
    bg: u32,
    flags: u16,
    underline: Option<u32>,
    zerowidth: Vec<char>,
    hyperlink: Option<Hyperlink>,
}

fn colour_key(c: Color) -> u32 {
    match c {
        Color::Named(n) => 0x0100_0000 | n as u32,
        Color::Indexed(i) => 0x0200_0000 | u32::from(i),
        Color::Spec(rgb) => {
            0x0300_0000 | u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
        }
    }
}

fn look_key(cell: &SnapCell) -> LookKey {
    LookKey {
        fg: colour_key(cell.fg),
        bg: colour_key(cell.bg),
        flags: cell.flags.bits(),
        underline: cell.underline_color.map(colour_key),
        zerowidth: cell.zerowidth.clone(),
        hyperlink: cell.hyperlink.clone(),
    }
}

/// Each distinct look once; id 0 is the default blank.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Looks {
    looks: Vec<SnapCell>,
    ids: HashMap<LookKey, u32>,
    /// Calls to [`Looks::intern`].
    lookups: usize,
}

#[allow(dead_code)]
impl Looks {
    pub fn new() -> Looks {
        let mut looks = Looks {
            looks: Vec::new(),
            ids: HashMap::new(),
            lookups: 0,
        };
        looks.intern(&SnapCell {
            c: ' ',
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: CellFlags::empty(),
            zerowidth: Vec::new(),
            underline_color: None,
            hyperlink: None,
        });
        looks
    }

    /// The id of `cell`'s look; its character is ignored.
    pub fn intern(&mut self, cell: &SnapCell) -> u32 {
        self.lookups += 1;
        let key = look_key(cell);
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = self.looks.len() as u32;
        let mut look = cell.clone();
        look.c = ' ';
        self.looks.push(look);
        self.ids.insert(key, id);
        id
    }

    pub fn get(&self, id: u32) -> &SnapCell {
        &self.looks[id as usize]
    }

    pub fn len(&self) -> usize {
        self.looks.len()
    }

    pub fn lookups(&self) -> usize {
        self.lookups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> SnapCell {
        Looks::new().get(0).clone()
    }

    #[test]
    fn default_blank_is_zero_and_stable() {
        let mut l = Looks::new();
        assert_eq!(l.intern(&blank()), 0);
        assert_eq!(l.intern(&blank()), 0);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn char_ignored_fg_distinguishes() {
        let mut l = Looks::new();
        let mut a = blank();
        a.c = 'x';
        assert_eq!(l.intern(&a), 0);
        a.fg = Color::Indexed(1);
        assert_eq!(l.intern(&a), 1);
        assert_eq!(l.len(), 2);
        assert_eq!(l.get(1).c, ' ');
    }

    #[test]
    fn zerowidth_and_hyperlink_in_key() {
        let mut l = Looks::new();
        let mut z = blank();
        z.zerowidth = vec!['\u{301}'];
        let zi = l.intern(&z);
        assert_ne!(zi, 0);
        assert_eq!(l.get(zi), &z);
        let mut h = blank();
        h.hyperlink = Some(Hyperlink::new(Some("id"), "http://x".to_string()));
        let hi = l.intern(&h);
        assert!(hi != 0 && hi != zi);
        assert_eq!(l.get(hi), &h);
        assert_eq!(l.intern(&h), hi);
    }
}
