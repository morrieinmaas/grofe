//! Moving and resizing a floating pane by its border, as tmux 3.7b's stock
//! `MouseDrag1Border` row (`resize-pane -M`) does it
//! (`cmd_resize_pane_mouse_update_floating`), and `place-pane`'s clamp.
//!
//! A floating pane is its content rect plus a one-cell border all round.
//! Each report of a drag moves the part of the border the previous report
//! was on to the pointer: the top edge moves the pane, the other edges and
//! the corners resize it from that side. The pane may leave the window; it
//! is never smaller than 1x1. A report that would make it smaller changes
//! nothing on an edge (and on the bottom left corner), and the border then
//! no longer lies under the pointer, so the drag waits until the pointer
//! comes back to it, as in tmux.

use crate::server::model::Floating;

/// `f` after a drag report moved the pointer from window cell `(lx, ly)`
/// (the previous report) to `(x, y)`, or `None` when that changes nothing.
pub fn dragged(f: &Floating, (lx, ly): (i32, i32), (x, y): (i32, i32)) -> Option<Floating> {
    let (sx, sy) = (i32::from(f.cols), i32::from(f.rows));
    let (left, right) = (f.x - 1, f.x + sx);
    let (top, bottom) = (f.y - 1, f.y + sy);
    let at_left = lx == left || lx == left + 1;
    let at_right = lx == right || lx == right + 1;
    let min = |n: i32| n.max(1);
    // The same tests in the same order as tmux, corners first: a corner
    // takes the cell next to it on the top or bottom edge too.
    let (nx, ny, cols, rows) = if at_left && ly == top {
        (x + 1, y + 1, min(sx + lx - x), min(sy + ly - y))
    } else if at_right && ly == top {
        (f.x, y + 1, min(x - f.x), min(sy + ly - y))
    } else if at_left && ly == bottom {
        let rows = y - f.y;
        if rows < 1 {
            return None;
        }
        (x + 1, f.y, min(sx + lx - x), rows)
    } else if at_right && ly == bottom {
        (f.x, f.y, min(x - f.x), min(y - f.y))
    } else if lx == right {
        let cols = x - f.x;
        if cols < 1 {
            return None;
        }
        (f.x, f.y, cols, sy)
    } else if lx == left {
        let cols = sx + lx - x;
        if cols < 1 {
            return None;
        }
        (x + 1, f.y, cols, sy)
    } else if ly == bottom {
        let rows = y - f.y;
        if rows < 1 {
            return None;
        }
        (f.x, f.y, sx, rows)
    } else if ly == top {
        (f.x + x - lx, y + 1, sx, sy)
    } else {
        return None;
    };
    let size = |n: i32| u16::try_from(n).unwrap_or(u16::MAX);
    let out = Floating {
        x: nx,
        y: ny,
        cols: size(cols),
        rows: size(rows),
        ..*f
    };
    (out != *f).then_some(out)
}

/// `f` moved to content origin `(x, y)` (`place-pane`), held so that the
/// whole pane, border included, is inside a window of `window` cells; a
/// pane larger than the window is pinned to its top left.
pub fn placed(f: &Floating, (x, y): (i32, i32), window: (u16, u16)) -> Floating {
    let (w, h) = (i32::from(window.0), i32::from(window.1));
    Floating {
        x: x.clamp(1, (w - i32::from(f.cols) - 1).max(1)),
        y: y.clamp(1, (h - i32::from(f.rows) - 1).max(1)),
        ..*f
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PaneId;

    fn float(x: i32, y: i32, cols: u16, rows: u16) -> Floating {
        Floating {
            pane: PaneId(1),
            x,
            y,
            cols,
            rows,
            z: 1,
            scratch: false,
        }
    }

    fn rect(f: Option<Floating>) -> Option<(i32, i32, u16, u16)> {
        f.map(|f| (f.x, f.y, f.cols, f.rows))
    }

    /// The pane of `tc_6/float-border-mouse`: content at (10, 4), 20x5, so
    /// its border is column 9 and 30, row 3 and 9. Each want is tmux's.
    fn recorded() -> Floating {
        float(10, 4, 20, 5)
    }

    #[test]
    fn the_top_edge_moves_the_pane_report_by_report() {
        let f = recorded();
        let once = dragged(&f, (15, 3), (19, 5));
        assert_eq!(rect(once), Some((14, 6, 20, 5)));
        let twice = dragged(&once.unwrap(), (19, 5), (17, 4));
        assert_eq!(rect(twice), Some((12, 5, 20, 5)));
    }

    #[test]
    fn each_other_edge_resizes_from_its_side() {
        let f = recorded();
        assert_eq!(rect(dragged(&f, (30, 6), (34, 6))), Some((10, 4, 24, 5)));
        assert_eq!(rect(dragged(&f, (15, 9), (15, 11))), Some((10, 4, 20, 7)));
        assert_eq!(rect(dragged(&f, (9, 6), (5, 6))), Some((6, 4, 24, 5)));
    }

    #[test]
    fn a_corner_resizes_both_ways() {
        let f = recorded();
        assert_eq!(rect(dragged(&f, (30, 9), (33, 11))), Some((10, 4, 23, 7)));
        assert_eq!(rect(dragged(&f, (9, 3), (7, 2))), Some((8, 3, 22, 6)));
    }

    #[test]
    fn the_pane_may_leave_the_window() {
        let f = recorded();
        let corner = dragged(&f, (15, 3), (58, 0)).unwrap();
        assert_eq!(rect(Some(corner)), Some((53, 1, 20, 5)));
        // A pointer on the status line drags along the window's last row.
        assert_eq!(
            rect(dragged(&corner, (58, 0), (58, 13))),
            Some((53, 14, 20, 5))
        );
    }

    #[test]
    fn an_edge_never_shrinks_the_pane_past_one_cell() {
        let f = recorded();
        assert_eq!(dragged(&f, (30, 6), (10, 6)), None);
        assert_eq!(rect(dragged(&f, (30, 6), (11, 6))), Some((10, 4, 1, 5)));
        assert_eq!(dragged(&f, (15, 9), (15, 4)), None);
        // The far corner holds at 1x1 rather than refusing.
        assert_eq!(rect(dragged(&f, (30, 9), (0, 0))), Some((10, 4, 1, 1)));
    }

    #[test]
    fn a_report_off_the_border_or_in_place_changes_nothing() {
        let f = recorded();
        assert_eq!(dragged(&f, (15, 6), (20, 6)), None);
        assert_eq!(dragged(&f, (15, 3), (15, 3)), None);
    }

    #[test]
    fn placing_holds_the_pane_inside_the_window() {
        let f = float(10, 5, 20, 6);
        let p = placed(&f, (-9, 100), (80, 24));
        assert_eq!((p.x, p.y), (1, 24 - 6 - 1));
        let big = placed(&float(1, 1, 30, 10), (4, 4), (20, 8));
        assert_eq!((big.x, big.y), (1, 1));
    }
}
