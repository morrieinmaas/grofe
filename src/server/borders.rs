//! Pane borders, ported from tmux 3.7b's `screen-redraw.c`: which window
//! cells are border, which glyph each gets, which pane owns it, whether it
//! is part of the active pane's border, and where `pane-border-status`
//! puts a pane's title.
//!
//! The geometry is the one tmux works on: every pane box is the pane's
//! content, a border is the line of cells round it, and with a border
//! status the panes at the window's top (or bottom) edge have already given
//! up the row the title takes.

use crate::types::PaneId;

/// `pane-border-status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneStatus {
    #[default]
    Off,
    Top,
    Bottom,
}

impl PaneStatus {
    pub fn parse(value: &str) -> PaneStatus {
        match value {
            "top" => PaneStatus::Top,
            "bottom" => PaneStatus::Bottom,
            _ => PaneStatus::Off,
        }
    }
}

/// `pane-border-lines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneLines {
    #[default]
    Single,
    Double,
    Heavy,
    Simple,
    Number,
    Spaces,
}

impl PaneLines {
    pub fn parse(value: &str) -> PaneLines {
        match value {
            "double" => PaneLines::Double,
            "heavy" => PaneLines::Heavy,
            "simple" => PaneLines::Simple,
            "number" => PaneLines::Number,
            "spaces" => PaneLines::Spaces,
            _ => PaneLines::Single,
        }
    }
}

/// `pane-border-indicators`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Indicators {
    Off,
    #[default]
    Colour,
    Arrows,
    Both,
}

impl Indicators {
    pub fn parse(value: &str) -> Indicators {
        match value {
            "off" => Indicators::Off,
            "arrows" => Indicators::Arrows,
            "both" => Indicators::Both,
            _ => Indicators::Colour,
        }
    }
}

/// One tiled pane's content box in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneBox {
    pub id: PaneId,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The pane's place in the window's pane list (`pane-border-lines
    /// number` shows it, as `pane-base-index` numbers it).
    pub index: usize,
}

/// A border in relation to a pane (tmux's `screen_redraw_border_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Outside,
    Inside,
    Left,
    Right,
    Top,
    Bottom,
}

// tmux's CELL_* border cell types.
pub const CELL_INSIDE: u8 = 0;
pub const CELL_TOPBOTTOM: u8 = 1;
pub const CELL_LEFTRIGHT: u8 = 2;
pub const CELL_TOPLEFT: u8 = 3;
pub const CELL_TOPRIGHT: u8 = 4;
pub const CELL_BOTTOMLEFT: u8 = 5;
pub const CELL_BOTTOMRIGHT: u8 = 6;
pub const CELL_TOPJOIN: u8 = 7;
pub const CELL_BOTTOMJOIN: u8 = 8;
pub const CELL_LEFTJOIN: u8 = 9;
pub const CELL_RIGHTJOIN: u8 = 10;
pub const CELL_JOIN: u8 = 11;
pub const CELL_OUTSIDE: u8 = 12;

/// The glyph for border cell type `cell` in `lines`; `pane` is the pane
/// the cell belongs to (`number` shows its index).
pub fn glyph(lines: PaneLines, cell: u8, pane: Option<&PaneBox>) -> char {
    const SINGLE: [char; 13] = [
        ' ', '│', '─', '┌', '┐', '└', '┘', '┬', '┴', '├', '┤', '┼', '·',
    ];
    const DOUBLE: [char; 13] = [
        ' ', '║', '═', '╔', '╗', '╚', '╝', '╦', '╩', '╠', '╣', '╬', '·',
    ];
    const HEAVY: [char; 13] = [
        ' ', '┃', '━', '┏', '┓', '┗', '┛', '┳', '┻', '┣', '┫', '╋', '·',
    ];
    const SIMPLE: [char; 13] = [
        ' ', '|', '-', '+', '+', '+', '+', '+', '+', '+', '+', '+', '.',
    ];
    let i = usize::from(cell.min(CELL_OUTSIDE));
    match lines {
        PaneLines::Single => SINGLE[i],
        PaneLines::Double => DOUBLE[i],
        PaneLines::Heavy => HEAVY[i],
        PaneLines::Simple => SIMPLE[i],
        PaneLines::Spaces => ' ',
        PaneLines::Number => {
            if cell == CELL_OUTSIDE {
                '·'
            } else {
                pane.map_or('*', |p| char::from(b'0' + (p.index % 10) as u8))
            }
        }
    }
}

/// The window's panes and the options that decide how their borders look.
pub struct Geometry<'a> {
    pub cols: i32,
    pub rows: i32,
    pub status: PaneStatus,
    pub indicators: Indicators,
    /// The visible tiled panes, in the window's pane order.
    pub panes: &'a [PaneBox],
    /// The window's active pane, an index into `panes`.
    pub active: Option<usize>,
    /// `panes` indexes in tmux's z_index order, which for tiled panes is the
    /// order they were made in (a pane's id grows with each), not the order
    /// of the pane list: it decides which pane owns a cell two panes claim.
    z: Vec<usize>,
}

impl<'a> Geometry<'a> {
    pub fn new(
        (cols, rows): (i32, i32),
        status: PaneStatus,
        indicators: Indicators,
        panes: &'a [PaneBox],
        active: Option<usize>,
    ) -> Self {
        let mut z: Vec<usize> = (0..panes.len()).collect();
        z.sort_by_key(|&i| panes[i].id);
        Geometry {
            cols,
            rows,
            status,
            indicators,
            panes,
            active,
            z,
        }
    }

    /// `Some(side by side)` when the window has exactly two panes
    /// (tmux's `screen_redraw_two_panes`).
    fn two_panes(&self) -> Option<bool> {
        match self.panes {
            [a, b] => Some(a.y == b.y),
            _ => None,
        }
    }

    /// tmux's `screen_redraw_pane_border`.
    pub fn pane_border(&self, wp: &PaneBox, px: i32, py: i32) -> Side {
        let (ex, ey) = (wp.x + wp.w, wp.y + wp.h);
        if px >= wp.x && px < ex && py >= wp.y && py < ey {
            return Side::Inside;
        }
        let (mut hsplit, mut vsplit) = (false, false);
        if matches!(self.indicators, Indicators::Colour | Indicators::Both)
            && let Some(side_by_side) = self.two_panes()
        {
            hsplit = side_by_side;
            vsplit = !side_by_side;
        }
        let (sx, sy) = (wp.w, wp.h);

        // The sy / 2 test colours only half of the shared border when
        // there are two panes.
        if (wp.y == 0 || py >= wp.y - 1) && py <= ey {
            if wp.x == 0 && px == sx && (!hsplit || py <= sy / 2) {
                return Side::Right;
            }
            if wp.x != 0 {
                if px == wp.x - 1 && (!hsplit || py > sy / 2) {
                    return Side::Left;
                }
                if px == wp.x + sx {
                    return Side::Right;
                }
            }
        }

        if vsplit && self.status == PaneStatus::Off {
            if wp.y == 0 && py == sy && px <= sx / 2 {
                return Side::Bottom;
            }
            if wp.y != 0 && py == wp.y - 1 && px > sx / 2 {
                return Side::Top;
            }
        } else if (wp.x == 0 || px >= wp.x) && px <= ex {
            if self.status != PaneStatus::Bottom && wp.y != 0 && py == wp.y - 1 {
                return Side::Top;
            }
            if self.status != PaneStatus::Top && py == ey {
                return Side::Bottom;
            }
        }
        Side::Outside
    }

    /// tmux's `screen_redraw_cell_border1`: 1 border, 0 inside, -1 not
    /// this pane's.
    fn cell_border1(&self, wp: &PaneBox, px: i32, py: i32) -> i8 {
        if (px < wp.x - 1 || px > wp.x + wp.w) && (py < wp.y - 1 || py > wp.y + wp.h) {
            return -1;
        }
        match self.pane_border(wp, px, py) {
            Side::Inside => 0,
            Side::Outside => -1,
            _ => 1,
        }
    }

    /// The last window row a border can be on: the bottom status line is
    /// one.
    fn last_row(&self) -> i32 {
        if self.status == PaneStatus::Bottom {
            self.rows - 1
        } else {
            self.rows
        }
    }

    /// tmux's `screen_redraw_cell_border`.
    fn cell_border(&self, px: i32, py: i32) -> bool {
        let sy = self.last_row();
        if px > self.cols || py > sy {
            return false;
        }
        if px == self.cols || py == sy {
            return true;
        }
        for wp in self.panes {
            match self.cell_border1(wp, px, py) {
                -1 => {}
                n => return n == 1,
            }
        }
        false
    }

    /// tmux's `screen_redraw_type_of_cell`: the border cell type from
    /// which of the four neighbours are border too.
    pub fn type_of_cell(&self, px: i32, py: i32) -> u8 {
        let sy = self.last_row();
        if px > self.cols || py > sy {
            return CELL_OUTSIDE;
        }
        let mut borders = 0;
        if px == 0 || self.cell_border(px - 1, py) {
            borders |= 8;
        }
        if px <= self.cols && self.cell_border(px + 1, py) {
            borders |= 4;
        }
        match self.status {
            PaneStatus::Top => {
                if py != 0 && self.cell_border(px, py - 1) {
                    borders |= 2;
                }
                if self.cell_border(px, py + 1) {
                    borders |= 1;
                }
            }
            PaneStatus::Bottom => {
                if py == 0 || self.cell_border(px, py - 1) {
                    borders |= 2;
                }
                if py != sy && self.cell_border(px, py + 1) {
                    borders |= 1;
                }
            }
            PaneStatus::Off => {
                if py == 0 || self.cell_border(px, py - 1) {
                    borders |= 2;
                }
                if self.cell_border(px, py + 1) {
                    borders |= 1;
                }
            }
        }
        match borders {
            15 => CELL_JOIN,
            14 => CELL_BOTTOMJOIN,
            13 => CELL_TOPJOIN,
            12 => CELL_LEFTRIGHT,
            11 => CELL_RIGHTJOIN,
            10 => CELL_BOTTOMRIGHT,
            9 => CELL_TOPRIGHT,
            7 => CELL_LEFTJOIN,
            6 => CELL_BOTTOMLEFT,
            5 => CELL_TOPLEFT,
            3 => CELL_TOPBOTTOM,
            _ => CELL_OUTSIDE,
        }
    }

    /// The width of pane `wp`'s title: nothing under 4 columns, else the
    /// pane's width less 2, cut to the window.
    pub fn title_width(&self, wp: &PaneBox) -> i32 {
        let width = if wp.w < 4 { 0 } else { wp.w - 2 };
        width.min((self.cols - (wp.x + 2)).max(0))
    }

    /// The window row `wp`'s title is on.
    pub fn title_row(&self, wp: &PaneBox) -> i32 {
        if self.status == PaneStatus::Top {
            wp.y - 1
        } else {
            wp.y + wp.h
        }
    }

    /// tmux's `screen_redraw_check_cell`: the cell's type and the pane it
    /// belongs to. `CELL_INSIDE` is a pane's content, or its title.
    pub fn check_cell(&self, px: i32, py: i32) -> (u8, Option<usize>) {
        if px > self.cols || py > self.rows {
            return (CELL_OUTSIDE, None);
        }
        let near = |wp: &PaneBox| {
            px >= wp.x - 1 && px <= wp.x + wp.w && py >= wp.y - 1 && py <= wp.y + wp.h
        };
        let start = self
            .z
            .iter()
            .position(|&i| near(&self.panes[i]))
            .or_else(|| {
                self.active
                    .and_then(|a| self.z.iter().position(|&i| i == a))
            });
        let Some(start) = start else {
            return (CELL_OUTSIDE, None);
        };
        if px == self.cols || py == self.rows {
            return (self.type_of_cell(px, py), Some(self.z[start]));
        }
        let n = self.panes.len();
        let mut owner = None;
        for k in 0..n {
            let i = self.z[(start + k) % n];
            let wp = &self.panes[i];
            owner = Some(i);
            if (px < wp.x - 1 || px > wp.x + wp.w) && (py < wp.y - 1 || py > wp.y + wp.h) {
                continue;
            }
            if self.status != PaneStatus::Off {
                let line = self.title_row(wp);
                let (left, right) = (wp.x + 2, wp.x + 2 + self.title_width(wp) - 1);
                if py == line && px >= left && px <= right {
                    return (CELL_INSIDE, owner);
                }
            }
            match self.pane_border(wp, px, py) {
                Side::Inside => return (CELL_INSIDE, owner),
                Side::Outside => continue,
                _ => return (self.type_of_cell(px, py), owner),
            }
        }
        (CELL_OUTSIDE, owner)
    }

    /// Whether the cell is on the border of pane `wp`
    /// (`screen_redraw_check_is`).
    pub fn is_border_of(&self, wp: &PaneBox, px: i32, py: i32) -> bool {
        !matches!(self.pane_border(wp, px, py), Side::Inside | Side::Outside)
    }

    /// The arrow `pane-border-indicators` puts on the cell, if any
    /// (`screen_redraw_draw_border_arrows`); `owner` is the pane the
    /// cell belongs to.
    pub fn arrow(&self, x: i32, y: i32, cell: u8, owner: &PaneBox) -> Option<char> {
        let active = &self.panes[self.active?];
        if x != owner.x + 1 && y != owner.y + 1 {
            return None;
        }
        if !matches!(self.indicators, Indicators::Arrows | Indicators::Both) {
            return None;
        }
        let mut border = self.pane_border(active, x, y);
        if border == Side::Inside {
            return None;
        }
        let first_active = self.panes.first() == Some(active);
        let mut arrows = false;
        if x == owner.x + 1 {
            if border == Side::Outside {
                if self.two_panes().is_some() {
                    border = if first_active {
                        Side::Bottom
                    } else {
                        Side::Top
                    };
                    arrows = true;
                }
            } else if cell == CELL_LEFTRIGHT
                || (cell == CELL_TOPJOIN && border == Side::Bottom)
                || (cell == CELL_BOTTOMJOIN && border == Side::Top)
            {
                arrows = true;
            }
        }
        if y == owner.y + 1 {
            if border == Side::Outside {
                if self.two_panes().is_some() {
                    border = if first_active {
                        Side::Right
                    } else {
                        Side::Left
                    };
                    arrows = true;
                }
            } else if cell == CELL_TOPBOTTOM
                || (cell == CELL_LEFTJOIN && border == Side::Right)
                || (cell == CELL_RIGHTJOIN && border == Side::Left)
            {
                arrows = true;
            }
        }
        // tmux's BORDER_MARKERS "  +,.-" are ACS arrows.
        arrows.then_some(match border {
            Side::Left => '→',
            Side::Right => '←',
            Side::Top => '↓',
            Side::Bottom => '↑',
            _ => ' ',
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(id: u32, x: i32, y: i32, w: i32, h: i32, index: usize) -> PaneBox {
        PaneBox {
            id: PaneId(id),
            x,
            y,
            w,
            h,
            index,
        }
    }

    /// Two side-by-side panes in a 21x6 window: the border is column 10.
    fn side_by_side(indicators: Indicators, active: usize) -> (Vec<PaneBox>, Indicators, usize) {
        (
            vec![boxed(1, 0, 0, 10, 6, 0), boxed(2, 11, 0, 10, 6, 1)],
            indicators,
            active,
        )
    }

    fn geometry<'a>(
        panes: &'a [PaneBox],
        indicators: Indicators,
        active: usize,
        status: PaneStatus,
    ) -> Geometry<'a> {
        Geometry::new((21, 6), status, indicators, panes, Some(active))
    }

    #[test]
    fn lines_choose_the_glyph_set() {
        let p = boxed(1, 0, 0, 1, 1, 7);
        assert_eq!(glyph(PaneLines::Single, CELL_TOPBOTTOM, Some(&p)), '│');
        assert_eq!(glyph(PaneLines::Double, CELL_JOIN, Some(&p)), '╬');
        assert_eq!(glyph(PaneLines::Heavy, CELL_TOPLEFT, Some(&p)), '┏');
        assert_eq!(glyph(PaneLines::Simple, CELL_LEFTRIGHT, Some(&p)), '-');
        assert_eq!(glyph(PaneLines::Number, CELL_JOIN, Some(&p)), '7');
        assert_eq!(glyph(PaneLines::Number, CELL_JOIN, None), '*');
        assert_eq!(glyph(PaneLines::Spaces, CELL_JOIN, Some(&p)), ' ');
    }

    #[test]
    fn a_title_is_the_pane_width_less_two_and_none_under_four() {
        let panes = [boxed(1, 0, 1, 3, 4, 0), boxed(2, 4, 1, 17, 4, 1)];
        let g = geometry(&panes, Indicators::Colour, 0, PaneStatus::Top);
        assert_eq!(g.title_width(&panes[0]), 0);
        assert_eq!(g.title_width(&panes[1]), 15);
        assert_eq!(g.title_row(&panes[1]), 0);
    }

    #[test]
    fn colour_indicators_split_a_two_pane_border_between_the_panes() {
        let (panes, ind, active) = side_by_side(Indicators::Colour, 0);
        let g = geometry(&panes, ind, active, PaneStatus::Off);
        // Rows 0 to 3 of the shared column are the left pane's, the rest
        // the right pane's; with indicators off the left pane has all.
        assert!(g.is_border_of(&panes[0], 10, 3));
        assert!(!g.is_border_of(&panes[0], 10, 4));
        assert!(g.is_border_of(&panes[1], 10, 4));
        let g = geometry(&panes, Indicators::Off, 0, PaneStatus::Off);
        assert!(g.is_border_of(&panes[0], 10, 5));
    }

    #[test]
    fn a_cell_two_panes_claim_is_owned_by_the_pane_made_first() {
        // Left column top (made first), bottom (made last), right (second).
        let panes = [
            boxed(0, 0, 0, 15, 4, 1),
            boxed(2, 0, 5, 15, 4, 2),
            boxed(1, 16, 0, 14, 9, 3),
        ];
        let g = Geometry::new(
            (30, 9),
            PaneStatus::Off,
            Indicators::Colour,
            &panes,
            Some(0),
        );
        let owner = |x, y| g.check_cell(x, y).1.map(|i| panes[i].index);
        assert_eq!(owner(15, 4), Some(1));
        assert_eq!(owner(15, 5), Some(3));
        assert_eq!(owner(15, 8), Some(3));
        assert_eq!(owner(3, 4), Some(1));
    }

    #[test]
    fn a_border_status_row_is_a_border_across_the_window() {
        let panes = [boxed(1, 0, 1, 10, 5, 0), boxed(2, 11, 1, 10, 5, 1)];
        let g = geometry(&panes, Indicators::Colour, 0, PaneStatus::Top);
        assert_eq!(g.type_of_cell(0, 0), CELL_LEFTRIGHT);
        assert_eq!(g.type_of_cell(10, 0), CELL_TOPJOIN);
        assert_eq!(g.type_of_cell(10, 3), CELL_TOPBOTTOM);
        // The title cells are the pane's, not border.
        assert_eq!(g.check_cell(2, 0).0, CELL_INSIDE);
        assert_eq!(g.check_cell(1, 0).0, CELL_LEFTRIGHT);
    }
}
