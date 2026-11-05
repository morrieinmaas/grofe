//! Where a pane moved with the mouse lands (FR.3, spec 2026-11-05-frames.md
//! "Drag and drop"): a tile dragged by its title, or any pane by
//! `drag-pane -M` (Ctrl and drag), dropped over a tiled pane `T` of its
//! window.
//!
//! | Released over | Result |
//! |---|---|
//! | the middle of `T` | the two swap places, as `swap-pane` |
//! | the outer quarter of one of `T`'s sides | `T` is split and the pane takes the half on that side; the space it left is filled as `kill-pane` fills it |
//! | the pane itself, a gap, or no pane | nothing |
//!
//! An edge drop that would leave the layout's shape as it is changes
//! nothing, so a moved divider stays put; a side of `T` too small to split
//! is no edge, and counts as the middle. A floating pane drops into the
//! tiling by the same table, which is unfloating it with the mouse: on the
//! middle `T` floats in its place.
//!
//! [`plan`] works the drop out without changing anything, so the same
//! answer gives the preview while the button is down and the move on
//! release ([`apply`]).

use crate::server::layout::{GeometryCell, Insert, LayoutGeometry, LayoutTree};
use crate::server::model::Window;
use crate::server::model::{HOLE, Model};
use crate::types::{Dir, PaneId, Rect, SplitSize, WindowId};

/// A side of the pane dropped onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// What part of a pane the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Middle,
    Edge(Side),
}

/// A drop worked out: the layout after it, the content rect the dragged
/// pane gets (the preview), the pane dropped onto, and whether the two
/// swap.
#[derive(Debug, Clone)]
pub struct Drop {
    pub onto: PaneId,
    pub layout: LayoutTree,
    pub preview: Rect,
    pub swap: bool,
}

/// The zone of rect `r` that window cell `(x, y)`, inside it, is in: the
/// outer quarter of a side is that edge, the closest one (relative to the
/// rect's size) in a corner; the rest is the middle.
pub fn zone(r: Rect, (x, y): (u32, u32)) -> Zone {
    let (cols, rows) = (u32::from(r.cols).max(1), u32::from(r.rows).max(1));
    let (dx, dy) = (
        x.saturating_sub(u32::from(r.x)).min(cols - 1),
        y.saturating_sub(u32::from(r.y)).min(rows - 1),
    );
    let (near_x, left) = (dx.min(cols - 1 - dx), dx <= cols - 1 - dx);
    let (near_y, top) = (dy.min(rows - 1 - dy), dy <= rows - 1 - dy);
    let on_x = 4 * near_x < cols;
    let on_y = 4 * near_y < rows;
    let side_x = if left { Side::Left } else { Side::Right };
    let side_y = if top { Side::Top } else { Side::Bottom };
    match (on_x, on_y) {
        (false, false) => Zone::Middle,
        (true, false) => Zone::Edge(side_x),
        (false, true) => Zone::Edge(side_y),
        // A corner: the nearer edge, relative to each side's length.
        (true, true) if near_x * rows <= near_y * cols => Zone::Edge(side_x),
        (true, true) => Zone::Edge(side_y),
    }
}

/// The rect a drop onto `content` is measured on: the frame around it in a
/// framed window (the frame is the pane's), else the content itself.
fn drop_rect(content: Rect, framed: bool) -> Rect {
    if !framed {
        return content;
    }
    Rect {
        x: content.x.saturating_sub(1),
        y: content.y.saturating_sub(1),
        cols: content.cols + 2,
        rows: content.rows + 2,
    }
}

fn contains(r: Rect, (x, y): (u32, u32)) -> bool {
    let (left, top) = (u32::from(r.x), u32::from(r.y));
    (left..left + u32::from(r.cols)).contains(&x) && (top..top + u32::from(r.rows)).contains(&y)
}

/// Where `dragged`, a pane of `w` (tiled or floating), lands if released at
/// window cell `at`, or `None` when that changes nothing. A zoomed window
/// moves nothing.
pub fn plan(w: &Window, dragged: PaneId, at: (u32, u32)) -> Option<Drop> {
    if w.zoomed {
        return None;
    }
    let area = w.area();
    let framed = w.framed();
    let floating = w.floating(dragged).is_some();
    let (onto, content) = w
        .tiled_rects()
        .into_iter()
        .filter(|(id, _)| *id != HOLE)
        .find(|(_, r)| contains(drop_rect(*r, framed), at))?;
    if onto == dragged {
        return None;
    }
    let edge = match zone(drop_rect(content, framed), at) {
        // The same shape again: nothing, rather than the middle's swap.
        Zone::Edge(side) => match docked(w, dragged, onto, side) {
            Some(None) => return None,
            edge => edge.flatten(),
        },
        Zone::Middle => None,
    };
    let (layout, swap) = match edge {
        Some(layout) => (layout, false),
        None => {
            let mut layout = w.layout.clone();
            let swapped = if floating {
                layout.replace(onto, dragged)
            } else {
                layout.swap(dragged, onto)
            };
            if !swapped {
                return None;
            }
            (layout, true)
        }
    };
    let (_, preview) = layout
        .compute(area)
        .into_iter()
        .find(|(id, _)| *id == dragged)?;
    Some(Drop {
        onto,
        layout,
        preview,
        swap,
    })
}

/// The layout with `dragged` out of its place and in the half of `onto` on
/// `side`; `Some(None)` when the layout's shape would stay as it is, and
/// `None` when `onto` is too small to split that way.
fn docked(w: &Window, dragged: PaneId, onto: PaneId, side: Side) -> Option<Option<LayoutTree>> {
    let area = w.area();
    let mut layout = w.layout.clone();
    // Its space is filled as `kill-pane` fills it.
    layout.remove(dragged);
    let how = Insert {
        dir: match side {
            Side::Left | Side::Right => Dir::Horizontal,
            Side::Top | Side::Bottom => Dir::Vertical,
        },
        size: SplitSize::Half,
        before: matches!(side, Side::Left | Side::Top),
        full: false,
    };
    layout.insert(area, onto, dragged, how).ok()?;
    let same =
        w.floating(dragged).is_none() && same_shape(&w.layout.export(area), &layout.export(area));
    Some((!same).then_some(layout))
}

/// Whether two layouts split the same panes the same way, whatever the
/// sizes.
fn same_shape(a: &LayoutGeometry, b: &LayoutGeometry) -> bool {
    match (&a.cell, &b.cell) {
        (GeometryCell::Pane(x), GeometryCell::Pane(y)) => x == y,
        (
            GeometryCell::Split {
                dir: da,
                children: ca,
            },
            GeometryCell::Split {
                dir: db,
                children: cb,
            },
        ) => da == db && ca.len() == cb.len() && ca.iter().zip(cb).all(|(a, b)| same_shape(a, b)),
        _ => false,
    }
}

/// Makes `drop`, worked out by [`plan`] for `dragged` in `window`: the
/// layout changes, a floating pane leaves the floating set (or on a swap
/// hands its place to the pane it swapped with), and `dragged` becomes the
/// active pane. Each moved pane learns its new size now, once. The panes
/// are the same panes: process, history and uid stay.
pub fn apply(model: &mut Model, window: WindowId, dragged: PaneId, drop: Drop) {
    let Some(w) = model.windows.get_mut(&window) else {
        return;
    };
    if let Some(i) = w.floating.iter().position(|f| f.pane == dragged) {
        if drop.swap {
            w.floating[i].pane = drop.onto;
        } else {
            w.floating.remove(i);
        }
    }
    w.zoomed = false;
    w.layout = drop.layout;
    w.drop_preview = None;
    w.set_active(dragged);
    model.relayout(window);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::model::Floating;
    use crate::types::{SessionId, WindowId};

    fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
        Rect { x, y, cols, rows }
    }

    /// An 80x23 window of `%0` and `%1` side by side, framed with gap 1
    /// when `framed`.
    fn window(framed: bool) -> Window {
        let mut layout = LayoutTree::new(PaneId(0));
        let mut w = Window {
            id: WindowId(1),
            session: SessionId(0),
            index: 0,
            links: Vec::new(),
            name: "w".to_string(),
            layout: LayoutTree::new(PaneId(0)),
            active: PaneId(0),
            zoomed: false,
            cols: 80,
            rows: 23,
            bell_flag: false,
            activity_flag: false,
            silence_flag: false,
            activity: std::time::SystemTime::now(),
            last_panes: Vec::new(),
            active_points: Default::default(),
            floating: Vec::new(),
            pane_status: Default::default(),
            frames: false,
            pane_gap: 0,
            drop_preview: None,
        };
        w.set_frames(framed, 1);
        layout.set_pad(w.layout.pad());
        layout
            .split(
                w.area(),
                PaneId(0),
                Dir::Horizontal,
                PaneId(1),
                SplitSize::Half,
            )
            .unwrap();
        w.layout = layout;
        w
    }

    fn rects(l: &LayoutTree, w: &Window) -> Vec<(PaneId, Rect)> {
        l.compute(w.area())
    }

    #[test]
    fn the_outer_quarter_of_a_side_is_its_edge() {
        let r = rect(10, 0, 40, 20);
        assert_eq!(zone(r, (10, 10)), Zone::Edge(Side::Left));
        assert_eq!(zone(r, (19, 10)), Zone::Edge(Side::Left));
        assert_eq!(zone(r, (20, 10)), Zone::Middle);
        assert_eq!(zone(r, (49, 10)), Zone::Edge(Side::Right));
        assert_eq!(zone(r, (30, 0)), Zone::Edge(Side::Top));
        assert_eq!(zone(r, (30, 19)), Zone::Edge(Side::Bottom));
        assert_eq!(zone(r, (30, 10)), Zone::Middle);
        // A corner goes to the nearer edge for the side's length.
        assert_eq!(zone(r, (11, 4)), Zone::Edge(Side::Left));
        assert_eq!(zone(r, (18, 0)), Zone::Edge(Side::Top));
    }

    #[test]
    fn the_middle_swaps_and_the_preview_is_the_other_panes_place() {
        for framed in [false, true] {
            let w = window(framed);
            let before = rects(&w.layout, &w);
            let right = before[1].1;
            let mid = (u32::from(right.x + right.cols / 2), 10);
            let d = plan(&w, PaneId(0), mid).expect("a swap");
            assert!(d.swap);
            assert_eq!(d.onto, PaneId(1));
            assert_eq!(d.preview, right, "framed {framed}");
        }
    }

    #[test]
    fn the_preview_follows_the_pointer() {
        let w = window(true);
        let right = rects(&w.layout, &w)[1].1;
        let middle = plan(&w, PaneId(0), (60, 10)).unwrap().preview;
        let top = plan(&w, PaneId(0), (60, 1)).unwrap().preview;
        let bottom = plan(&w, PaneId(0), (60, 21)).unwrap().preview;
        assert_eq!(middle, right);
        // %1, alone once %0 has left, is split: %0 takes its top or
        // bottom half.
        assert_eq!((top.x, top.y, top.cols), (2, 2, 76));
        assert!(top.rows <= 10, "{top:?}");
        assert!(bottom.y > top.y + top.rows, "{bottom:?}");
        // On itself, a gap or nothing: no preview.
        assert!(plan(&w, PaneId(0), (10, 10)).is_none());
        let gap = rects(&w.layout, &w)[0].1;
        let gap_col = u32::from(gap.x + gap.cols + 1);
        assert!(plan(&w, PaneId(0), (gap_col, 10)).is_none());
        assert!(plan(&w, PaneId(0), (0, 10)).is_none());
    }

    #[test]
    fn a_left_edge_drop_docks_left_and_one_that_changes_no_shape_does_nothing() {
        let w = window(true);
        // %1 on %0's left quarter: %1 now left of %0.
        let d = plan(&w, PaneId(1), (3, 10)).expect("a dock");
        assert!(!d.swap);
        let after = rects(&d.layout, &w);
        let (l, r) = (after[0], after[1]);
        assert_eq!((l.0, r.0), (PaneId(0), PaneId(1)), "index order");
        assert!(r.1.x < l.1.x, "{after:?}");
        // %0 onto %1's left quarter: it is there already.
        let right = rects(&w.layout, &w)[1].1;
        assert!(plan(&w, PaneId(0), (u32::from(right.x), 10)).is_none());
    }

    #[test]
    fn a_pane_too_small_to_split_offers_no_edges_on_that_axis() {
        let mut w = window(true);
        // One content row each: no room for two one above the other.
        w.rows = 5;
        let right = rects(&w.layout, &w)[1].1;
        assert_eq!(right.rows, 1);
        let mid = u32::from(right.x + right.cols / 2);
        let d = plan(&w, PaneId(0), (mid, u32::from(right.y) - 1)).unwrap();
        assert!(d.swap, "{d:?}");
        // Side by side there is room: its right quarter docks.
        let x = u32::from(right.x + right.cols - 1);
        assert!(!plan(&w, PaneId(0), (x, 2)).unwrap().swap);
    }

    #[test]
    fn a_floating_pane_drops_into_the_tiling() {
        let mut w = window(true);
        w.layout.remove(PaneId(1));
        w.floating.push(Floating {
            pane: PaneId(1),
            x: 5,
            y: 5,
            cols: 20,
            rows: 5,
            z: 1,
        });
        // The left quarter of %0: %1 goes on its left.
        let d = plan(&w, PaneId(1), (3, 10)).expect("a dock");
        assert!(!d.swap);
        let after = rects(&d.layout, &w);
        assert_eq!(after.len(), 2);
        assert_eq!(
            d.preview,
            after.iter().find(|p| p.0 == PaneId(1)).unwrap().1
        );
        assert!(d.preview.x < after[0].1.x);
        // The middle: %1 takes %0's place.
        let d = plan(&w, PaneId(1), (40, 10)).expect("a swap");
        assert!(d.swap);
        assert_eq!(rects(&d.layout, &w)[0].0, PaneId(1));
    }

    #[test]
    fn a_zoomed_window_moves_nothing() {
        let mut w = window(true);
        w.zoomed = true;
        assert!(plan(&w, PaneId(0), (60, 10)).is_none());
    }
}
