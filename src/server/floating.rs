//! Moving and resizing a floating pane with the mouse (grofe's extension to
//! tmux 3.7b, whose floating-pane border takes no mouse key).
//!
//! A floating pane is its content rect plus a one-cell border all round.
//! Which part of that ring a press lands on decides the drag ([`grab_at`]):
//! the top and left edges move the pane, the right and bottom edges resize
//! it, and a corner resizes it from that corner. [`dragged`] applies the
//! pointer's whole movement since the press to the pane as it was then, so
//! a clamp that held it back lets go as the pointer comes back. The pane
//! (border included) stays inside the window; it may be as small as 1x1.

use crate::server::model::Floating;

/// What a press on a floating pane's border grabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    Move,
    /// The sides the pointer drags; a corner is two.
    Resize(Sides),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sides {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// The grab for cell `(dx, dy)` relative to `f`'s content origin, `None`
/// off its border ring.
pub fn grab_at(f: &Floating, dx: i64, dy: i64) -> Option<Grab> {
    let (cols, rows) = (i64::from(f.cols), i64::from(f.rows));
    let side = |d: i64, len: i64| (d == -1, d == len);
    let ((left, right), (top, bottom)) = (side(dx, cols), side(dy, rows));
    let on_x = (0..cols).contains(&dx);
    let on_y = (0..rows).contains(&dy);
    let ring = (left || right || on_x) && (top || bottom || on_y);
    if !ring || (on_x && on_y) {
        return None;
    }
    let sides = Sides {
        left,
        right,
        top,
        bottom,
    };
    Some(match (left || right, top || bottom) {
        (true, true) => Grab::Resize(sides),
        (false, true) if top => Grab::Move,
        (true, false) if left => Grab::Move,
        _ => Grab::Resize(sides),
    })
}

/// `f` after the pointer moved `(dx, dy)` cells from the press, with `grab`
/// held, inside a window of `window` cells.
pub fn dragged(f: &Floating, grab: Grab, (dx, dy): (i32, i32), window: (u16, u16)) -> Floating {
    let (w, h) = (i32::from(window.0), i32::from(window.1));
    let mut out = *f;
    match grab {
        Grab::Move => {
            out.x = (f.x + dx).clamp(1, (w - i32::from(f.cols) - 1).max(1));
            out.y = (f.y + dy).clamp(1, (h - i32::from(f.rows) - 1).max(1));
        }
        Grab::Resize(s) => {
            (out.x, out.cols) = span(f.x, f.cols, dx, (s.left, s.right), w);
            (out.y, out.rows) = span(f.y, f.rows, dy, (s.top, s.bottom), h);
        }
    }
    out
}

/// One axis of a resize: the content `start` and `len`, the near and far
/// side being dragged by `d`, in a window `limit` cells long. The far side
/// stops one cell short of the window's edge (the border), the near side at
/// cell 1, and neither passes the opposite side.
fn span(start: i32, len: u16, d: i32, (near, far): (bool, bool), limit: i32) -> (i32, u16) {
    let end = start + i32::from(len);
    if near {
        let start = (start + d).clamp(1.min(end - 1), end - 1);
        (start, (end - start) as u16)
    } else if far {
        let end = (end + d).clamp(start + 1, (limit - 1).max(start + 1));
        (start, (end - start) as u16)
    } else {
        (start, len)
    }
}

/// `f` moved to content origin `(x, y)`, held inside a window of `window`
/// cells as a drag holds it.
pub fn placed(f: &Floating, (x, y): (i32, i32), window: (u16, u16)) -> Floating {
    dragged(f, Grab::Move, (x - f.x, y - f.y), window)
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
        }
    }

    #[test]
    fn the_ring_is_the_border_and_the_inside_is_not() {
        let f = float(10, 5, 20, 6);
        assert_eq!(grab_at(&f, 0, 0), None);
        assert_eq!(grab_at(&f, 19, 5), None);
        assert_eq!(grab_at(&f, -2, 0), None);
        assert_eq!(grab_at(&f, 20, 7), None);
        assert_eq!(grab_at(&f, 5, -2), None);
        // The top row and the left edge move it.
        assert_eq!(grab_at(&f, 5, -1), Some(Grab::Move));
        assert_eq!(grab_at(&f, -1, 3), Some(Grab::Move));
        // The right and bottom edges resize one axis.
        let right = Sides {
            right: true,
            ..Sides::default()
        };
        let bottom = Sides {
            bottom: true,
            ..Sides::default()
        };
        assert_eq!(grab_at(&f, 20, 3), Some(Grab::Resize(right)));
        assert_eq!(grab_at(&f, 5, 6), Some(Grab::Resize(bottom)));
        // A corner resizes from that corner.
        assert_eq!(
            grab_at(&f, 20, 6),
            Some(Grab::Resize(Sides {
                right: true,
                bottom: true,
                ..Sides::default()
            }))
        );
        assert_eq!(
            grab_at(&f, -1, -1),
            Some(Grab::Resize(Sides {
                left: true,
                top: true,
                ..Sides::default()
            }))
        );
    }

    #[test]
    fn a_move_follows_the_pointer_and_stays_in_the_window() {
        let f = float(10, 5, 20, 6);
        let win = (80, 24);
        let moved = dragged(&f, Grab::Move, (7, -2), win);
        assert_eq!((moved.x, moved.y, moved.cols, moved.rows), (17, 3, 20, 6));
        let far = dragged(&f, Grab::Move, (500, 500), win);
        assert_eq!((far.x, far.y), (80 - 20 - 1, 24 - 6 - 1));
        let near = dragged(&f, Grab::Move, (-500, -500), win);
        assert_eq!((near.x, near.y), (1, 1));
        // The press is the reference, so coming back un-clamps.
        let back = dragged(&f, Grab::Move, (0, 0), win);
        assert_eq!(back, f);
    }

    #[test]
    fn a_pane_larger_than_the_window_pins_to_its_corner() {
        let f = float(1, 1, 30, 10);
        let m = dragged(&f, Grab::Move, (4, 4), (20, 8));
        assert_eq!((m.x, m.y), (1, 1));
    }

    #[test]
    fn the_far_corner_grows_and_shrinks_the_pane() {
        let f = float(10, 5, 20, 6);
        let sides = Sides {
            right: true,
            bottom: true,
            ..Sides::default()
        };
        let win = (80, 24);
        let bigger = dragged(&f, Grab::Resize(sides), (5, 3), win);
        assert_eq!(
            (bigger.x, bigger.y, bigger.cols, bigger.rows),
            (10, 5, 25, 9)
        );
        let smaller = dragged(&f, Grab::Resize(sides), (-100, -100), win);
        assert_eq!((smaller.cols, smaller.rows), (1, 1));
        let capped = dragged(&f, Grab::Resize(sides), (500, 500), win);
        assert_eq!((capped.cols, capped.rows), (80 - 1 - 10, 24 - 1 - 5));
    }

    #[test]
    fn the_near_corner_moves_the_origin_and_keeps_the_far_side() {
        let f = float(10, 5, 20, 6);
        let sides = Sides {
            left: true,
            top: true,
            ..Sides::default()
        };
        let win = (80, 24);
        let r = dragged(&f, Grab::Resize(sides), (-4, -2), win);
        assert_eq!((r.x, r.y, r.cols, r.rows), (6, 3, 24, 8));
        let far = dragged(&f, Grab::Resize(sides), (500, 500), win);
        assert_eq!((far.x + i32::from(far.cols), far.cols), (30, 1));
        assert_eq!((far.y + i32::from(far.rows), far.rows), (11, 1));
        let edge = dragged(&f, Grab::Resize(sides), (-500, -500), win);
        assert_eq!((edge.x, edge.y), (1, 1));
    }

    #[test]
    fn placing_clamps_like_a_drag() {
        let f = float(10, 5, 20, 6);
        let p = placed(&f, (-9, 100), (80, 24));
        assert_eq!((p.x, p.y), (1, 24 - 6 - 1));
    }
}
