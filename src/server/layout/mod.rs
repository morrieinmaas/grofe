//! layout tree: n-ary splits, percent and cell sizes, collapse on remove.
//!
//! One algorithm, per spec §4.3: a window's layout is a tree of cells. Each
//! internal cell splits its area horizontally or vertically among children;
//! leaves are panes. Splitting a pane in the direction its parent already
//! splits adds a sibling; splitting in the other direction wraps the pane in
//! a new split. Borders are one cell wide and shared between neighbours, so a
//! split of `n` panes along an axis of length `W` gives pane sizes that sum
//! to `W - (n - 1)`.
//!
//! Like tmux's layout cells, the tree holds absolute sizes. Each child of a
//! split stores a `weight`: its actual size in cells along its parent's
//! axis, on one particular area, the tree's `exact_for`. Every split's
//! weights sum to exactly its local available cells on that area.
//!
//! [`LayoutTree::compute`] turns weights into concrete rects for any area by
//! distributing that area's cells proportionally to weight, rounding down
//! and handing leftover cells to the largest fractional remainders first,
//! ties broken by the lower child index. On `exact_for` this reproduces the
//! stored sizes exactly. On another area (the window resized) sizes rescale
//! proportionally; if that rounding would take a child to zero even though
//! the area has room for it, one cell per child is reserved up front and the
//! rest distributed the same way. If a split's area is too small for every
//! child to get one cell, the lowest-index children get one cell each until
//! cells run out and the rest get none.
//!
//! The mutating ops that take an area (`split`, `resize`) first rebase the
//! tree onto it: every weight is overwritten with the size `compute(area)`
//! renders, and `exact_for` becomes `area`. That changes nothing visible on
//! `area`, and means the op always works on real, current cell counts rather
//! than weights calibrated to some other area. `remove` takes no area and
//! works on the stored exact sizes, keeping them exact on `exact_for`. An
//! area whose render would give some pane zero cells is below the floor: a
//! weight can't record a zero-cell pane and an op can't act on one sensibly,
//! so the tree is not rebased there, `split` fails with
//! [`LayoutError::TooSmall`] and `resize` is a no-op.
//!
//! Signatures here differ from the contract sketch in one respect: `split`,
//! `resize` and `neighbour` take an explicit `area: Rect`. `SplitSize::Cells`,
//! `resize`'s cell delta, and `neighbour`'s centre-line pick are all
//! inherently geometric, and the tree otherwise has no way to learn the
//! window's current size (`LayoutTree::new` takes no area either). Threading
//! `area` through explicitly, and rebasing onto it, means the tree never
//! mutates from sizes that belong to some older window size.
//!
//! Framed windows (`pane-frames`, spec 2026-11-05-frames.md): every tile's
//! content has its own one-cell frame and `g` gap cells around it, so two
//! neighbouring content rects are `2 + g` cells apart and the content area
//! is the window inset by `1 + g` on every side. With `pad = 1 + g` that is
//! exactly the one-cell-border tree laid out on the window less `pad` at its
//! left and top (the *inner* area), each pane then giving up `pad` cells on
//! its right and bottom: the separator becomes `1 + pad`, and the last pane
//! along an axis ends `pad` cells short of the window. So the tree keeps its
//! one-cell borders and every op runs unchanged on the inner area; only the
//! entry points translate (`LayoutTree::set_pad`). A size the user gives in
//! cells (`split-window -l`, `resize-pane -x`, `main-pane-width`) is a
//! content size and gains `pad`; an op that would leave a framed pane with
//! no content cell is refused (or, for a resize, cut short). A window too
//! small for its frames is laid out unframed.

use serde::{Deserialize, Serialize};

use crate::server::borders::PaneStatus;
use crate::types::{Dir, Edge, PaneId, Rect, SplitSize};

mod presets;
mod string;
mod tree;

pub use self::presets::*;
pub use self::string::*;
pub use self::tree::*;

#[cfg(test)]
mod tests;
