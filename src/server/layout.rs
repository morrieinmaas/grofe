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

/// Error returned when a split can't be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("no such pane: {0}")]
    NotFound(PaneId),
    #[error("not enough room to split pane {0}")]
    TooSmall(PaneId),
    /// [`LayoutTree::import`]: the cells do not tile their area, or name
    /// other panes than the tree holds.
    #[error("invalid layout")]
    Invalid,
}

/// The main pane's size in the `main-*` presets, as tmux's
/// `main-pane-width` and `main-pane-height`: cells, or a percent of the
/// cells left after the border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainSize {
    Cells(u16),
    Percent(u8),
}

impl MainSize {
    /// tmux's `main-pane-width` default.
    pub const WIDTH: MainSize = MainSize::Cells(80);
    /// tmux's `main-pane-height` default.
    pub const HEIGHT: MainSize = MainSize::Cells(24);

    /// An option value as tmux reads it: `N` cells or `N%`. Anything else
    /// is `None`, which tmux replaces with the option's default.
    pub fn parse(s: &str) -> Option<MainSize> {
        match s.strip_suffix('%') {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= 100)
                .map(MainSize::Percent),
            None => s.parse().ok().map(MainSize::Cells),
        }
    }
}

/// A `select-layout` preset (spec §4.3), sized as tmux 3.7b sizes it. The
/// first pane in index order is the main pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Every pane side by side.
    EvenHorizontal,
    /// Every pane stacked.
    EvenVertical,
    /// The main pane full width on top, the rest side by side below.
    MainHorizontal { main: MainSize },
    /// As `MainHorizontal`, with the main pane at the bottom.
    MainHorizontalMirrored { main: MainSize },
    /// The main pane full height on the left, the rest stacked on the
    /// right.
    MainVertical { main: MainSize },
    /// As `MainVertical`, with the main pane on the right.
    MainVerticalMirrored { main: MainSize },
    /// A grid, filled row by row, at most `max_columns` wide (0: as wide as
    /// needs be; `tiled-layout-max-columns`).
    Tiled { max_columns: u16 },
}

impl Preset {
    /// tmux's preset names in its `next-layout` order.
    pub const NAMES: [&str; 7] = [
        "even-horizontal",
        "even-vertical",
        "main-horizontal",
        "main-horizontal-mirrored",
        "main-vertical",
        "main-vertical-mirrored",
        "tiled",
    ];

    /// The preset tmux names `name`, its main pane sized by `width` (the
    /// vertical ones) or `height` (the horizontal ones).
    pub fn from_name(
        name: &str,
        width: MainSize,
        height: MainSize,
        max_columns: u16,
    ) -> Option<Preset> {
        Some(match name {
            "even-horizontal" => Preset::EvenHorizontal,
            "even-vertical" => Preset::EvenVertical,
            "main-horizontal" => Preset::MainHorizontal { main: height },
            "main-horizontal-mirrored" => Preset::MainHorizontalMirrored { main: height },
            "main-vertical" => Preset::MainVertical { main: width },
            "main-vertical-mirrored" => Preset::MainVerticalMirrored { main: width },
            "tiled" => Preset::Tiled { max_columns },
            _ => return None,
        })
    }

    /// This preset's place in [`Preset::NAMES`].
    pub fn index(self) -> usize {
        match self {
            Preset::EvenHorizontal => 0,
            Preset::EvenVertical => 1,
            Preset::MainHorizontal { .. } => 2,
            Preset::MainHorizontalMirrored { .. } => 3,
            Preset::MainVertical { .. } => 4,
            Preset::MainVerticalMirrored { .. } => 5,
            Preset::Tiled { .. } => 6,
        }
    }
}

/// How [`LayoutTree::insert`] places a pane: the split direction and size,
/// as `split-window` takes them, and tmux's `-b` (before the target) and
/// `-f` (split the whole window rather than the target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insert {
    pub dir: Dir,
    pub size: SplitSize,
    pub before: bool,
    pub full: bool,
}

/// A layout as cells with their sizes and offsets (tmux's layout cells),
/// for the tmux layout-string codec: [`LayoutTree::export`] writes it and
/// [`LayoutTree::import`] reads it. Geometry only.
///
/// Cells name panes by runtime id; the M4 snapshot keeps the same shape
/// with cells keyed by pane `uid` (`LayoutGeometry<String>`, spec §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutGeometry<P = PaneId> {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
    pub cell: GeometryCell<P>,
}

/// What a [`LayoutGeometry`] cell holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GeometryCell<P = PaneId> {
    Pane(P),
    /// Children in order along `dir`, one border cell apart.
    Split {
        dir: Dir,
        children: Vec<LayoutGeometry<P>>,
    },
}

/// One contiguous, one-cell-wide run of border cells, as the M2 renderer will
/// draw it. `Vertical` runs come from horizontal (side-by-side) splits;
/// `Horizontal` runs come from vertical (stacked) splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderSegment {
    /// A vertical run at column `col`, spanning `len` rows starting at `row`.
    Vertical { col: u16, row: u16, len: u16 },
    /// A horizontal run at row `row`, spanning `len` cols starting at `col`.
    Horizontal { row: u16, col: u16, len: u16 },
}

#[derive(Debug, Clone)]
struct Child {
    /// Size in cells along the parent's axis on the tree's `exact_for`
    /// area, always >= 1; see the module docs.
    weight: u32,
    node: Node,
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(PaneId),
    Split { dir: Dir, children: Vec<Child> },
}

/// The n-ary split tree for one window.
#[derive(Debug, Clone)]
pub struct LayoutTree {
    root: Node,
    /// The area size (cols, rows) every weight is an exact cell count for.
    /// `None` until the first `split` or `resize`, while the tree is still a
    /// single leaf with no weights.
    exact_for: Option<(u16, u16)>,
    /// Every pane, in index order (tmux's pane list): the order `compute`
    /// returns them in. It is the leaves' order, except where tmux's is
    /// not: a pane joined before its target, or a mirrored preset.
    order: Vec<PaneId>,
    /// The last preset applied, as a [`Preset::index`]: where
    /// `next-layout` goes on from (tmux's `lastlayout`).
    last_preset: Option<usize>,
    /// `1 + pane-gap` in a framed window, else 0 (module docs).
    pad: u16,
    /// The pad `exact_for` was made under: `exact_for` is an inner area
    /// that much smaller than the window ([`Self::exact_size`]).
    exact_pad: u16,
    /// Rows of outer gap left out at the top and the bottom: the edge the
    /// status line sits on has none ([`Self::set_trim`]).
    trim: (u16, u16),
    /// The rows of `trim` in effect when `exact_for` was made.
    exact_trim: u16,
}

/// Which axis a point-in-rect lookup runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Row,
    Col,
}

impl LayoutTree {
    /// A window with a single pane filling the whole area.
    pub fn new(pane: PaneId) -> Self {
        LayoutTree {
            root: Node::Leaf(pane),
            exact_for: None,
            order: vec![pane],
            last_preset: None,
            pad: 0,
            exact_pad: 0,
            trim: (0, 0),
            exact_trim: 0,
        }
    }

    /// The window size the tree's weights are exact for, if any: after
    /// [`Self::apply_tmux_layout`], the layout string's own size.
    pub fn exact_size(&self) -> Option<(u16, u16)> {
        let pad = self.exact_pad;
        self.exact_for.map(|(cols, rows)| {
            let rows = rows.saturating_add(pad).saturating_sub(self.exact_trim);
            (cols.saturating_add(pad), rows)
        })
    }

    /// Frames the tree's panes with `pad` cells each (`1 + pane-gap`; 0
    /// for none), as the module docs describe. Nothing is rescaled: the
    /// next render lays the panes out on the new inner area.
    pub fn set_pad(&mut self, pad: u16) {
        self.pad = pad;
    }

    /// Leaves `top` and `bottom` rows of the outer gap out, at most all of
    /// it: the frames there meet the window's edge. Like [`Self::set_pad`],
    /// nothing is rescaled.
    pub fn set_trim(&mut self, top: u16, bottom: u16) {
        self.trim = (top, bottom);
    }

    /// What [`Self::set_trim`] last gave.
    pub fn trim(&self) -> (u16, u16) {
        self.trim
    }

    /// The trim in effect under `pad`: none unframed, never into a frame.
    fn trim_on(&self, pad: u16) -> (u16, u16) {
        let gap = pad.saturating_sub(1);
        (self.trim.0.min(gap), self.trim.1.min(gap))
    }

    /// The pad [`Self::set_pad`] last gave.
    pub fn pad(&self) -> u16 {
        self.pad
    }

    /// The pad the panes are framed with on `area`: [`Self::set_pad`]'s,
    /// unless some pane would have no content cell, when it is 0.
    // ponytail: a window too small to frame every pane is laid out
    // unframed as a whole, not framed where it fits.
    pub fn pad_on(&self, area: Rect) -> u16 {
        match inner(area, self.pad, self.trim_on(self.pad)) {
            Some(inner) if self.pad > 0 && self.fits(inner, self.pad) => self.pad,
            _ => 0,
        }
    }

    /// Whether every pane laid out on `inner` keeps a content cell after
    /// giving up `pad` cells each way.
    fn fits(&self, inner: Rect, pad: u16) -> bool {
        let mut out = Vec::new();
        collect_rects(&self.root, inner, &mut out);
        out.iter().all(|(_, r)| r.cols > pad && r.rows > pad)
    }

    /// Runs `op` on the inner area of `area` with the pad in effect there.
    /// When frames are in effect and `op` leaves some pane without a
    /// content cell, the tree is put back as it was and the answer is
    /// `None`.
    fn framed<T>(&mut self, area: Rect, op: impl FnOnce(&mut Self, Rect, u16) -> T) -> Option<T> {
        let pad = self.pad_on(area);
        let trim = self.trim_on(pad);
        let inner = inner(area, pad, trim)?;
        let before = (pad > 0).then(|| self.clone());
        let exact = self.exact_for;
        let out = op(self, inner, pad);
        if let Some(before) = before
            && !self.fits(inner, pad)
        {
            *self = before;
            return None;
        }
        if self.exact_for != exact {
            self.exact_pad = pad;
            self.exact_trim = trim.0 + trim.1;
        }
        Some(out)
    }

    /// Makes the tree exact on `area`: overwrites every split's weights with
    /// the sizes `compute(area)` renders and records `area` as `exact_for`.
    /// `compute(area)` is unchanged by this, since a group whose weights sum
    /// to its local avail renders to exactly those weights.
    ///
    /// Returns false, changing nothing, if `area` is below the floor: its
    /// render gives some split's child zero cells. Weights are always >= 1
    /// (a zero weight would render as a share of the area on the next
    /// rescale, not as nothing), so there is no exact tree to store there.
    fn rebase(&mut self, area: Rect) -> bool {
        let size = (area.cols, area.rows);
        if self.exact_for == Some(size) {
            return true;
        }
        if !renders_nonzero(&self.root, area) {
            return false;
        }
        store_rendered(&mut self.root, area);
        self.exact_for = Some(size);
        true
    }

    /// Splits `target` into itself and `new`, in direction `dir`, sized per
    /// `size`. If `target`'s parent already splits in `dir`, `new` becomes a
    /// sibling next to `target`; otherwise `target` is wrapped in a new split.
    /// Fails with `TooSmall` if `target` has no room for both panes on
    /// `area`, or `area` is below the floor (module docs).
    pub fn split(
        &mut self,
        area: Rect,
        target: PaneId,
        dir: Dir,
        new: PaneId,
        size: SplitSize,
    ) -> Result<(), LayoutError> {
        let how = Insert {
            dir,
            size,
            before: false,
            full: false,
        };
        self.insert(area, target, new, how)
    }

    /// Puts `new` next to `target` as [`Self::split`] does, or with
    /// `how.before` in front of it (left of or above), or with `how.full`
    /// beside the whole layout, which then shrinks to make room (tmux's
    /// `-b` and `-f`). With half the room, the first of the two cells gets
    /// the odd cell, as in tmux. `new` comes right after `target` in index
    /// order either way, as `join-pane` places it in tmux 3.7b.
    ///
    /// In a framed window `how.size` in cells is the new pane's content
    /// size, and a split that would leave a pane no content cell fails
    /// with `TooSmall`.
    pub fn insert(
        &mut self,
        area: Rect,
        target: PaneId,
        new: PaneId,
        how: Insert,
    ) -> Result<(), LayoutError> {
        self.framed(area, |tree, inner, pad| {
            let size = match how.size {
                SplitSize::Cells(n) => SplitSize::Cells(n.saturating_add(pad)),
                size => size,
            };
            tree.insert_inner(inner, target, new, Insert { size, ..how })
        })
        .unwrap_or(Err(LayoutError::TooSmall(target)))
    }

    /// [`Self::insert`] on the inner area.
    fn insert_inner(
        &mut self,
        area: Rect,
        target: PaneId,
        new: PaneId,
        how: Insert,
    ) -> Result<(), LayoutError> {
        let mut path = Vec::new();
        if !find_path(&self.root, target, &mut path) {
            return Err(LayoutError::NotFound(target));
        }
        if !self.rebase(area) {
            return Err(LayoutError::TooSmall(target));
        }
        if how.full && !path.is_empty() {
            self.insert_full(area, new, how)
                .ok_or(LayoutError::TooSmall(target))?;
        } else {
            self.insert_at(area, &path, new, how)
                .ok_or(LayoutError::TooSmall(target))?;
        }
        let pos = self
            .order
            .iter()
            .position(|p| *p == target)
            .map_or(self.order.len(), |i| i + 1);
        self.order.insert(pos, new);
        Ok(())
    }

    /// [`Self::insert`] next to the leaf at `path`. `None` when it has no
    /// room for both panes.
    fn insert_at(&mut self, area: Rect, path: &[usize], new: PaneId, how: Insert) -> Option<()> {
        let dir = how.dir;
        let leaf = |weight: u16, node: Node| Child {
            weight: u32::from(weight),
            node,
        };
        let Some((&idx, parent_path)) = path.split_last() else {
            // The root is the target's leaf.
            let avail = axis_len(dir, area).saturating_sub(1);
            let (old, fresh) = insert_sizes(avail, how)?;
            let target = std::mem::replace(&mut self.root, Node::Leaf(new));
            self.root = Node::Split {
                dir,
                children: pair(leaf(old, target), leaf(fresh, Node::Leaf(new)), how.before),
            };
            return Some(());
        };
        let parent_dir = match node_at(&self.root, parent_path) {
            Node::Split { dir, .. } => *dir,
            Node::Leaf(_) => unreachable!("parent path always resolves to a split"),
        };

        if parent_dir == dir {
            // Splitting in the direction the parent already splits: add new
            // as a sibling next to target, carved out of target's slice.
            // The tree is exact on `area` now, so target's weight is its
            // real size.
            let Node::Split { children, .. } = node_at_mut(&mut self.root, parent_path) else {
                unreachable!("parent path always resolves to a split")
            };
            let current = u16::try_from(children[idx].weight).unwrap_or(u16::MAX);
            let (old, fresh) = insert_sizes(current.saturating_sub(1), how)?;
            children[idx].weight = u32::from(old);
            let at = if how.before { idx } else { idx + 1 };
            children.insert(at, leaf(fresh, Node::Leaf(new)));
        } else {
            // Splitting the other way: wrap target's leaf in a new split.
            let target_rect = rect_at_path(&self.root, area, path);
            let avail = axis_len(dir, target_rect).saturating_sub(1);
            let (old, fresh) = insert_sizes(avail, how)?;
            let slot = node_at_mut(&mut self.root, path);
            let target = std::mem::replace(slot, Node::Leaf(new));
            *slot = Node::Split {
                dir,
                children: pair(leaf(old, target), leaf(fresh, Node::Leaf(new)), how.before),
            };
        }
        Some(())
    }

    /// [`Self::insert`] with `full`, for a tree that is a split: the whole
    /// layout shrinks along `how.dir` to make room, as tmux's
    /// `layout_resize_child_cells` shrinks it. A root already split along
    /// `how.dir` takes `new` as one more child, as in tmux; any other root
    /// is wrapped. `None` when the layout cannot shrink that far.
    fn insert_full(&mut self, area: Rect, new: PaneId, how: Insert) -> Option<()> {
        let avail = axis_len(how.dir, area).checked_sub(1)?;
        let (old, fresh) = insert_sizes(avail, how)?;
        let mut shrunk = area;
        match how.dir {
            Dir::Horizontal => shrunk.cols = old,
            Dir::Vertical => shrunk.rows = old,
        }
        let mut root = self.root.clone();
        resize_children(&mut root, shrunk)?;
        let new_child = Child {
            weight: u32::from(fresh),
            node: Node::Leaf(new),
        };
        self.root = match root {
            Node::Split { dir, mut children } if dir == how.dir => {
                if how.before {
                    children.insert(0, new_child);
                } else {
                    children.push(new_child);
                }
                Node::Split { dir, children }
            }
            other => {
                let old_child = Child {
                    weight: u32::from(old),
                    node: other,
                };
                Node::Split {
                    dir: how.dir,
                    children: pair(old_child, new_child, how.before),
                }
            }
        };
        Some(())
    }

    /// Removes `pane`. If its parent split is left with exactly one child,
    /// that split collapses into the remaining child. A no-op if `pane` is
    /// not in the tree, or is the tree's only pane (nothing to collapse into;
    /// callers discard the window instead in that case). Takes no area: it
    /// works on the stored sizes, which stay exact on `exact_for`.
    pub fn remove(&mut self, pane: PaneId) {
        let mut path = Vec::new();
        if !find_path(&self.root, pane, &mut path) {
            return;
        }
        if path.is_empty() {
            return;
        }
        self.order.retain(|p| *p != pane);
        self.remove_at(&path);
    }

    /// [`Self::remove`] of the leaf at `path` (not the root), without
    /// touching `order`.
    fn remove_at(&mut self, path: &[usize]) {
        let idx = path[path.len() - 1];
        let parent_path = &path[..path.len() - 1];
        let Node::Split { dir, children } = node_at_mut(&mut self.root, parent_path) else {
            unreachable!("parent path always resolves to a split")
        };
        let dir = *dir;
        let removed = children.remove(idx);
        // The removed child's own weight, plus the one border cell that's
        // now gone too, is the extent whoever inherits its space gains
        // along `dir`. It goes to the previous child if there was one,
        // otherwise the new first child, which hands it on round-robin as
        // tmux's `layout_resize_adjust` does.
        let gain = removed.weight + 1;
        if children.len() == 1 {
            // The survivor takes over the whole slot this split occupied,
            // growing by `gain` along `dir`. Unlike the non-collapse case,
            // `dir` here isn't necessarily this survivor's own split axis
            // (it's this split's axis, the one being discarded), so the
            // growth has to be pushed down through it explicitly.
            let mut remaining = children.pop().unwrap();
            grow(&mut remaining.node, dir, gain);
            let slot = node_at_mut(&mut self.root, parent_path);
            *slot = remaining.node;
        } else {
            // Like tmux's layout_destroy_cell: give the removed child's
            // cells to its neighbour directly (its own weight, in this
            // group's list), and push the growth down into it ([`adjust`]).
            let neighbour = idx.saturating_sub(1);
            children[neighbour].weight += gain;
            grow(&mut children[neighbour].node, dir, gain);
        }
    }

    /// Moves the border on `pane`'s `edge` side by `delta` cells (positive
    /// grows `pane` toward that edge). Clamped so neither side's subtree
    /// would drop any leaf below one cell (`max_shrink`'s doc comment); a
    /// no-op (not a panic) if `pane` has no neighbour across `edge`, the
    /// clamp reduces the requested delta to zero, or `area` is below the
    /// floor (module docs). In a framed window the clamp also keeps every
    /// pane a content cell.
    pub fn resize(&mut self, area: Rect, pane: PaneId, edge: Edge, delta: i32) {
        if self.pad_on(area) == 0 {
            self.resize_inner(area, pane, edge, delta);
            return;
        }
        let len = i32::from(area.cols.max(area.rows));
        let mut delta = delta.clamp(-len, len);
        while self
            .framed(area, |tree, inner, _| {
                tree.resize_inner(inner, pane, edge, delta)
            })
            .is_none()
            && delta != 0
        {
            delta -= delta.signum();
        }
    }

    /// [`Self::resize`] on the inner area.
    fn resize_inner(&mut self, area: Rect, pane: PaneId, edge: Edge, delta: i32) {
        let mut path = Vec::new();
        if !find_path(&self.root, pane, &mut path) {
            return;
        }
        if !self.rebase(area) {
            return;
        }
        for depth in (0..path.len()).rev() {
            let idx = path[depth];
            let Node::Split { dir, children } = node_at_mut(&mut self.root, &path[..depth]) else {
                unreachable!("parent path always resolves to a split")
            };
            let Some(far_idx) = far_sibling(edge, *dir, idx, children.len()) else {
                continue;
            };
            let dir = *dir;
            // The tree is exact on `area` now, so each weight is that
            // child's real size and `max_shrink` reads real sizes too.
            let (grower, shrinker) = if delta > 0 {
                (idx, far_idx)
            } else {
                (far_idx, idx)
            };
            let cap = max_shrink(&children[shrinker].node, children[shrinker].weight, dir);
            let amt = delta.unsigned_abs().min(cap);
            if amt == 0 {
                return;
            }
            debug_assert!(amt < children[shrinker].weight);
            children[shrinker].weight = children[shrinker].weight.saturating_sub(amt);
            children[grower].weight += amt;
            grow(&mut children[grower].node, dir, amt);
            shrink(&mut children[shrinker].node, dir, amt);
            return;
        }
    }

    /// Replaces the tree with `preset` over `panes` (in pane-index order,
    /// which the new tree keeps), exact on `area`. Returns false, changing
    /// nothing, for fewer than two panes (tmux does nothing either) or when
    /// `area` cannot give every pane a cell. Either way `preset` is where
    /// `next-layout` goes on from ([`Self::last_preset`]), as in tmux.
    ///
    /// D18 (deliberate, kept for M3.6): tmux 3.7b grows the layout past a
    /// window too small for the preset and clips it; a tree exact on its
    /// area cannot, so the old layout stays.
    pub fn preset(&mut self, area: Rect, preset: Preset, panes: &[PaneId]) -> bool {
        self.preset_bordered(area, preset, panes, PaneStatus::Off)
    }

    /// [`Self::preset`] in a window with a `pane-border-status`. The even
    /// vertical preset spreads the rows left after the title row
    /// (tmux's `layout_spread_cell`), which the pane along that edge then
    /// gets back, so the extra rows go to the first panes whichever edge.
    ///
    /// In a framed window a main pane's size in cells is its content size,
    /// and a preset that would leave a pane no content cell is not applied.
    pub fn preset_bordered(
        &mut self,
        area: Rect,
        preset: Preset,
        panes: &[PaneId],
        status: PaneStatus,
    ) -> bool {
        let applied = self.framed(area, |tree, inner, pad| {
            let grown = |main: MainSize| match main {
                MainSize::Cells(n) => MainSize::Cells(n.saturating_add(pad)),
                main => main,
            };
            let preset = match preset {
                Preset::MainHorizontal { main } => Preset::MainHorizontal { main: grown(main) },
                Preset::MainHorizontalMirrored { main } => {
                    Preset::MainHorizontalMirrored { main: grown(main) }
                }
                Preset::MainVertical { main } => Preset::MainVertical { main: grown(main) },
                Preset::MainVerticalMirrored { main } => {
                    Preset::MainVerticalMirrored { main: grown(main) }
                }
                other => other,
            };
            // With frames on, `pane-border-status` has no effect: the
            // title is drawn in the frame.
            let status = if pad > 0 { PaneStatus::Off } else { status };
            tree.preset_inner(inner, preset, panes, status)
        });
        self.last_preset = Some(preset.index());
        applied == Some(true)
    }

    /// [`Self::preset_bordered`] on the inner area.
    fn preset_inner(
        &mut self,
        area: Rect,
        preset: Preset,
        panes: &[PaneId],
        status: PaneStatus,
    ) -> bool {
        self.last_preset = Some(preset.index());
        if panes.len() < 2 {
            return false;
        }
        let (h, v) = (Dir::Horizontal, Dir::Vertical);
        let root = match preset {
            Preset::EvenHorizontal => even(h, area.cols, panes),
            Preset::EvenVertical if status != PaneStatus::Off => {
                even(v, area.rows.saturating_sub(1), panes).map(|mut node| {
                    if let Node::Split { children, .. } = &mut node {
                        let edge = match status {
                            PaneStatus::Top => children.first_mut(),
                            _ => children.last_mut(),
                        };
                        if let Some(edge) = edge {
                            edge.weight += 1;
                        }
                    }
                    node
                })
            }
            Preset::EvenVertical => even(v, area.rows, panes),
            Preset::MainHorizontal { main } => main_split(area, v, main, panes, false),
            Preset::MainHorizontalMirrored { main } => main_split(area, v, main, panes, true),
            Preset::MainVertical { main } => main_split(area, h, main, panes, false),
            Preset::MainVerticalMirrored { main } => main_split(area, h, main, panes, true),
            Preset::Tiled { max_columns } => tiled(area, panes, max_columns),
        };
        let Some(root) = root else {
            return false;
        };
        self.root = root;
        self.exact_for = Some((area.cols, area.rows));
        self.order = panes.to_vec();
        true
    }

    /// The last preset [`Self::preset`] was asked for, as a
    /// [`Preset::index`].
    pub fn last_preset(&self) -> Option<usize> {
        self.last_preset
    }

    /// The pane adjacent to `pane` across `edge`, among panes sharing the
    /// nearest ancestor split on that axis, chosen by overlap with `pane`'s
    /// centre line (deterministic: the far subtree's leaf whose span on the
    /// cross axis contains that centre coordinate).
    pub fn neighbour(&self, area: Rect, pane: PaneId, edge: Edge) -> Option<PaneId> {
        let pad = self.pad_on(area);
        let area = inner(area, pad, self.trim_on(pad))?;
        let mut path = Vec::new();
        if !find_path(&self.root, pane, &mut path) {
            return None;
        }
        let pane_rect = rect_at_path(&self.root, area, &path);
        for depth in (0..path.len()).rev() {
            let idx = path[depth];
            let parent_path = &path[..depth];
            let Node::Split { dir, children } = node_at(&self.root, parent_path) else {
                unreachable!("parent path always resolves to a split")
            };
            let Some(far_idx) = far_sibling(edge, *dir, idx, children.len()) else {
                continue;
            };
            let parent_rect = rect_at_path(&self.root, area, parent_path);
            let rects = child_rects(*dir, parent_rect, children);
            let far_rect = rects[far_idx];
            let (axis, coord) = match edge {
                Edge::Left | Edge::Right => (Axis::Row, pane_rect.y + pane_rect.rows / 2),
                Edge::Up | Edge::Down => (Axis::Col, pane_rect.x + pane_rect.cols / 2),
            };
            return Some(find_leaf_at(&children[far_idx].node, far_rect, axis, coord));
        }
        None
    }

    /// The rect of every pane for `area`, excluding borders.
    ///
    /// A pane's rect can be 0 cells wide or tall only when `area` is too
    /// small to hold every pane plus its borders; callers must not size a
    /// PTY to 0 from such a rect (M2's job, when it resizes panes).
    /// In index order (tmux's pane list, `order`). In a framed window the
    /// rects are the panes' content, frames and gaps excluded.
    pub fn compute(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let pad = self.pad_on(area);
        let mut out = Vec::new();
        if let Some(inner) = inner(area, pad, self.trim_on(pad)) {
            collect_rects(&self.root, inner, &mut out);
        }
        for (_, r) in &mut out {
            r.cols -= pad;
            r.rows -= pad;
        }
        out.sort_by_key(|(id, _)| self.order.iter().position(|p| p == id));
        out
    }

    /// Every border segment for `area`. None in a framed window, whose
    /// panes have frames rather than shared borders.
    pub fn borders(&self, area: Rect) -> Vec<BorderSegment> {
        let mut out = Vec::new();
        if self.pad_on(area) == 0 {
            collect_borders(&self.root, area, &mut out);
        }
        out
    }

    /// Whether `pane` is one of this tree's panes.
    pub fn contains(&self, pane: PaneId) -> bool {
        self.order.contains(&pane)
    }

    /// Moves `pane` to position `index` in index order (clamped to the
    /// end), leaving every cell where it is: `split-window` places a new
    /// pane in tmux's list apart from its cell.
    pub fn reorder(&mut self, pane: PaneId, index: usize) {
        if let Some(pos) = self.order.iter().position(|p| *p == pane) {
            self.order.remove(pos);
            self.order.insert(index.min(self.order.len()), pane);
        }
    }

    /// Swaps two panes' cells and index positions (`swap-pane` within a
    /// window). False, changing nothing, unless both are in the tree.
    pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool {
        if !self.contains(a) || !self.contains(b) {
            return false;
        }
        self.relabel(&|p| match p {
            p if p == a => b,
            p if p == b => a,
            p => p,
        });
        true
    }

    /// Puts `new` in `old`'s cell and index position (`swap-pane` between
    /// windows). False, changing nothing, unless `old` is in the tree and
    /// `new` is not.
    pub fn replace(&mut self, old: PaneId, new: PaneId) -> bool {
        if !self.contains(old) || self.contains(new) {
            return false;
        }
        self.relabel(&|p| if p == old { new } else { p });
        true
    }

    /// `rotate-window`: every pane moves one index position up (the first
    /// goes last) or, with `down`, down, taking over that position's cell,
    /// as tmux 3.7b does.
    pub fn rotate(&mut self, down: bool) {
        let mut next = self.order.clone();
        if down {
            next.rotate_right(1);
        } else {
            next.rotate_left(1);
        }
        let map: Vec<(PaneId, PaneId)> = self.order.iter().copied().zip(next).collect();
        self.relabel(&|p| map.iter().find(|(from, _)| *from == p).map_or(p, |m| m.1));
    }

    fn relabel(&mut self, map: &dyn Fn(PaneId) -> PaneId) {
        relabel_node(&mut self.root, map);
        for p in &mut self.order {
            *p = map(*p);
        }
    }

    /// `resize-pane -x`/`-y`: makes the cell holding `pane` under its
    /// nearest ancestor split along `dir` `cells` wide (or tall), as
    /// tmux's `layout_resize_pane_to` does, taking room from or giving it
    /// to the cells after it (or before it, for the last), clamped so every
    /// pane keeps a cell. A no-op without such an ancestor, or when `area`
    /// is below the floor (module docs).
    ///
    /// In a framed window `cells` is the content size, and the clamp also
    /// keeps every pane a content cell.
    pub fn resize_to(&mut self, area: Rect, pane: PaneId, dir: Dir, cells: u16) {
        if self.pad_on(area) == 0 {
            self.resize_to_inner(area, pane, dir, cells);
            return;
        }
        let Some((_, now)) = self.compute(area).into_iter().find(|(id, _)| *id == pane) else {
            return;
        };
        let now = axis_len(dir, now);
        let mut cells = cells.min(axis_len(dir, area));
        while self
            .framed(area, |tree, inner, pad| {
                tree.resize_to_inner(inner, pane, dir, cells.saturating_add(pad));
            })
            .is_none()
            && cells != now
        {
            cells = if cells > now { cells - 1 } else { cells + 1 };
        }
    }

    /// [`Self::resize_to`] on the inner area.
    fn resize_to_inner(&mut self, area: Rect, pane: PaneId, dir: Dir, cells: u16) {
        let mut path = Vec::new();
        if !find_path(&self.root, pane, &mut path) || !self.rebase(area) {
            return;
        }
        let Some(depth) = (0..path.len())
            .rev()
            .find(|d| matches!(node_at(&self.root, &path[..*d]), Node::Split { dir: d2, .. } if *d2 == dir))
        else {
            return;
        };
        let Node::Split { children, .. } = node_at_mut(&mut self.root, &path[..depth]) else {
            unreachable!("depth was found at a split")
        };
        let idx = path[depth];
        let size = i64::from(children[idx].weight);
        let last = idx + 1 == children.len();
        // tmux moves the border after the cell, or before the last one.
        let (cell, change) = if last {
            (idx - 1, size - i64::from(cells))
        } else {
            (idx, i64::from(cells) - size)
        };
        resize_cells(children, dir, cell, change);
    }

    /// `select-layout -E`: spreads the cells of `pane`'s parent split out
    /// evenly, or if they already are, its grandparent's, and so on up, as
    /// tmux's `layout_spread_out` does. Returns whether anything changed.
    pub fn spread(&mut self, area: Rect, pane: PaneId) -> bool {
        self.framed(area, |tree, inner, _| tree.spread_inner(inner, pane)) == Some(true)
    }

    /// [`Self::spread`] on the inner area.
    fn spread_inner(&mut self, area: Rect, pane: PaneId) -> bool {
        let mut path = Vec::new();
        if !find_path(&self.root, pane, &mut path) || !self.rebase(area) {
            return false;
        }
        for depth in (0..path.len()).rev() {
            let parent = &path[..depth];
            let rect = rect_at_path(&self.root, area, parent);
            let node = node_at_mut(&mut self.root, parent);
            let Node::Split { dir, children } = &mut *node else {
                unreachable!("parent path always resolves to a split")
            };
            let Some(sizes) = spread(axis_len(*dir, rect), children.len()) else {
                continue;
            };
            if children.iter().map(|c| c.weight).eq(sizes.iter().copied()) {
                continue;
            }
            let before = node.clone();
            if let Node::Split { children, .. } = &mut *node {
                for (child, size) in children.iter_mut().zip(sizes) {
                    child.weight = size;
                }
            }
            if !renders_nonzero(node, rect) {
                *node = before;
                return false;
            }
            store_rendered(node, rect);
            return true;
        }
        false
    }

    /// The layout's cells with their sizes and offsets on `area`, for the
    /// layout-string codec. In a framed window they are the content rects,
    /// the root's too: `pad` in from the window's edges, and neighbours
    /// `1 + pad` cells apart.
    pub fn export(&self, area: Rect) -> LayoutGeometry {
        let pad = self.pad_on(area);
        let Some(inner) = inner(area, pad, self.trim_on(pad)) else {
            return export_node(&self.root, area);
        };
        let mut g = export_node(&self.root, inner);
        shrink_cells(&mut g, pad);
        g
    }

    /// Replaces the layout with `geometry`, which must tile its area
    /// exactly (every split's children one border cell apart, spanning it)
    /// and hold exactly this tree's panes. Index order and the
    /// `next-layout` position stay, as a layout string
    /// leaves tmux's pane list alone (the codec puts the panes in the
    /// cells, in the order tmux would).
    ///
    /// A framed window's export (its root `pad` cells in from the corner,
    /// the same on both axes) imports as the layout it was exported from.
    pub fn import(&mut self, geometry: &LayoutGeometry) -> Result<(), LayoutError> {
        // Geometry can come from a user's layout string (FE.2): bound its
        // depth and size before walking it.
        let mut budget = self.order.len();
        if !bounded(geometry, 0, &mut budget) {
            return Err(LayoutError::Invalid);
        }
        let pad = framed_pad(geometry);
        let unframed;
        let geometry = if pad > 0 {
            let mut g = geometry.clone();
            unframe(&mut g, pad);
            unframed = g;
            &unframed
        } else {
            geometry
        };
        if (geometry.x, geometry.y) != (0, 0) {
            return Err(LayoutError::Invalid);
        }
        let root = import_node(geometry).ok_or(LayoutError::Invalid)?;
        let mut got = Vec::new();
        leaves(&root, &mut got);
        let mut want = self.order.clone();
        got.sort();
        want.sort();
        if got != want {
            return Err(LayoutError::Invalid);
        }
        self.root = root;
        self.exact_for = Some((geometry.cols, geometry.rows));
        self.exact_pad = pad;
        let trim = self.trim_on(pad);
        self.exact_trim = trim.0 + trim.1;
        Ok(())
    }

    /// Arranges `panes` (the window's, in index order) as the tmux layout
    /// string `layout` says, as tmux 3.7b's `layout_parse`: the cells get
    /// the panes in order whatever pane numbers the string names; with
    /// fewer panes than cells the bottom-right cell goes, again and again,
    /// its room given to its neighbour. The error is tmux's cause, which
    /// `select-layout` follows with `: <layout>`.
    // ponytail: tmux resizes the window to the layout's size; grofe keeps
    // the window's and scales the layout onto it.
    pub fn apply_tmux_layout(&mut self, layout: &str, panes: &[PaneId]) -> Result<(), String> {
        let invalid = || "invalid layout".to_string();
        let mut geometry = parse_tmux_layout(layout).ok_or_else(invalid)?;
        // A layout from a user's string is bounded as `import` bounds one,
        // with room for spare cells up to a cap.
        let mut budget = panes.len().max(MAX_LAYOUT_CELLS);
        if !bounded(&geometry, 0, &mut budget) {
            return Err(invalid());
        }
        let mut count = 0;
        count_cells(&geometry, &mut count);
        if panes.len() > count {
            return Err(format!("have {} panes but need {count}", panes.len()));
        }
        // Only a framed window reads a framed string (`#{window_layout}`
        // there): anywhere else a root off the corner is tmux's mismatch.
        let pad = if self.pad > 0 {
            framed_pad(&geometry)
        } else {
            0
        };
        unframe(&mut geometry, pad);
        fix_root_size(&mut geometry);
        let mismatch = || "size mismatch after applying layout".to_string();
        if (geometry.x, geometry.y) != (0, 0) {
            return Err(mismatch());
        }
        let root = import_node(&geometry).ok_or_else(mismatch)?;
        let mut cells = LayoutTree {
            root,
            exact_for: Some((geometry.cols, geometry.rows)),
            order: Vec::new(),
            last_preset: None,
            pad: 0,
            exact_pad: pad,
            trim: (0, 0),
            exact_trim: 0,
        };
        // The bottom-right cell is the last child's last child and so on
        // down (tmux's `layout_find_bottomright`): found in O(depth).
        for _ in panes.len()..count {
            let mut path = Vec::new();
            let mut node = &cells.root;
            while let Node::Split { children, .. } = node {
                path.push(children.len() - 1);
                node = &children[children.len() - 1].node;
            }
            if path.is_empty() {
                break;
            }
            cells.remove_at(&path);
        }
        relabel_node(&mut cells.root, &|p: PaneId| {
            panes.get(p.0 as usize).copied().unwrap_or(p)
        });
        self.root = cells.root;
        self.exact_for = cells.exact_for;
        self.exact_pad = cells.exact_pad;
        let trim = self.trim_on(self.exact_pad);
        self.exact_trim = trim.0 + trim.1;
        self.order = panes.to_vec();
        Ok(())
    }
}

/// `area` less `pad` cells at its left and top: the area a framed tree's
/// one-cell-border layout runs on (module docs), given back `top` and
/// `bottom` rows of the outer gap ([`LayoutTree::trim_on`]). `None` when
/// nothing is left.
fn inner(area: Rect, pad: u16, (top, bottom): (u16, u16)) -> Option<Rect> {
    if pad == 0 {
        return Some(area);
    }
    let rows = area.rows.checked_sub(pad)?.saturating_add(top + bottom);
    Some(Rect {
        x: area.x.saturating_add(pad),
        y: area.y.saturating_add(pad - top),
        cols: area.cols.checked_sub(pad).filter(|&c| c > 0)?,
        rows: Some(rows).filter(|&r| r > 0)?,
    })
}

/// Every cell of `g`, laid out on an inner area, as its content: `pad`
/// cells narrower and shorter.
fn shrink_cells(g: &mut LayoutGeometry, pad: u16) {
    g.cols = g.cols.saturating_sub(pad);
    g.rows = g.rows.saturating_sub(pad);
    if let GeometryCell::Split { children, .. } = &mut g.cell {
        children.iter_mut().for_each(|c| shrink_cells(c, pad));
    }
}

/// The pad a framed export ([`shrink_cells`]) was made with: its root's
/// column offset, its row offset that or less (a trimmed top gap); 0 for
/// a root at the corner or off those, which is no framed export.
fn framed_pad(g: &LayoutGeometry) -> u16 {
    if g.y > 0 && g.y <= g.x { g.x } else { 0 }
}

/// A framed export back on an inner area at the origin: every cell `pad`
/// cells wider and taller, and moved `pad` left and up by the root's row
/// offset. Nothing for 0.
fn unframe(g: &mut LayoutGeometry, pad: u16) {
    if pad > 0 {
        let dy = g.y;
        unframe_by(g, pad, dy);
    }
}

fn unframe_by(g: &mut LayoutGeometry, pad: u16, dy: u16) {
    g.x = g.x.saturating_sub(pad);
    g.y = g.y.saturating_sub(dy);
    g.cols = g.cols.saturating_add(pad);
    g.rows = g.rows.saturating_add(pad);
    if let GeometryCell::Split { children, .. } = &mut g.cell {
        children.iter_mut().for_each(|c| unframe_by(c, pad, dy));
    }
}

/// `target_size, new_size` for [`LayoutTree::insert`] on `avail` cells:
/// [`split_sizes`], except that a half split in front of the target gives
/// the new (first) cell the odd cell, as tmux does.
fn insert_sizes(avail: u16, how: Insert) -> Option<(u16, u16)> {
    let (old, new) = split_sizes(avail, how.size)?;
    Some(if how.before && how.size == SplitSize::Half {
        (new, old)
    } else {
        (old, new)
    })
}

/// `[old, new]`, or `[new, old]` when `new` goes before.
fn pair(old: Child, new: Child, before: bool) -> Vec<Child> {
    if before {
        vec![new, old]
    } else {
        vec![old, new]
    }
}

fn relabel_node(node: &mut Node, map: &dyn Fn(PaneId) -> PaneId) {
    match node {
        Node::Leaf(p) => *p = map(*p),
        Node::Split { children, .. } => {
            for child in children {
                relabel_node(&mut child.node, map);
            }
        }
    }
}

fn leaves(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(p) => out.push(*p),
        Node::Split { children, .. } => {
            for child in children {
                leaves(&child.node, out);
            }
        }
    }
}

/// tmux's `layout_resize_pane` on one split's `children` along `dir`:
/// moves the border after child `cell` by `change` cells, growing it by
/// taking from the first later child that can spare room (else the
/// nearest earlier one), or shrinking it (else the nearest earlier child
/// that can shrink) in favour of the child after it, until `change` is
/// used up or nothing can give.
fn resize_cells(children: &mut [Child], dir: Dir, cell: usize, change: i64) {
    let cap = |c: &Child| max_shrink(&c.node, c.weight, dir);
    let mut needed = change.unsigned_abs();
    while needed > 0 {
        let (grower, shrinker) = if change > 0 {
            let later = (cell + 1..children.len()).find(|&i| cap(&children[i]) > 0);
            let earlier = (0..cell).rev().find(|&i| cap(&children[i]) > 0);
            match later.or(earlier) {
                Some(s) => (cell, s),
                None => return,
            }
        } else {
            match (0..=cell).rev().find(|&i| cap(&children[i]) > 0) {
                Some(s) if cell + 1 < children.len() => (cell + 1, s),
                _ => return,
            }
        };
        let take = u32::try_from(needed)
            .unwrap_or(u32::MAX)
            .min(cap(&children[shrinker]));
        children[grower].weight += take;
        grow(&mut children[grower].node, dir, take);
        children[shrinker].weight -= take;
        shrink(&mut children[shrinker].node, dir, take);
        needed -= u64::from(take);
    }
}

/// tmux's `layout_resize_child_cells`, for weights exact on the node's
/// old size: gives each child of `node` its share of `rect` in proportion
/// to its size now (rounded down, never below what it can shrink to), the
/// last child taking what is left, and so on down the tree. `None` when
/// some child would get no cell.
fn resize_children(node: &mut Node, rect: Rect) -> Option<()> {
    let Node::Split { dir, children } = node else {
        return Some(());
    };
    let dir = *dir;
    let n = children.len() as u32;
    let previous: u32 = children.iter().map(|c| c.weight).sum::<u32>() + n - 1;
    let size = u32::from(axis_len(dir, rect));
    let mut left = size;
    for (i, c) in children.iter_mut().enumerate() {
        let count_left = n - i as u32;
        let new = if count_left == 1 {
            left
        } else {
            let floor = c.weight - max_shrink(&c.node, c.weight, dir);
            let min = (2 * (count_left - 1)).max(floor);
            (c.weight * size / previous)
                .min(left.saturating_sub(min))
                .max(1)
        };
        if new == 0 {
            return None;
        }
        c.weight = new;
        left = left.saturating_sub(new + 1);
    }
    let sum: u32 = children.iter().map(|c| c.weight).sum();
    if sum + n - 1 != size {
        return None;
    }
    let rects = child_rects(dir, rect, children);
    for (child, r) in children.iter_mut().zip(rects) {
        resize_children(&mut child.node, r)?;
    }
    Some(())
}

fn export_node(node: &Node, rect: Rect) -> LayoutGeometry {
    let cell = match node {
        Node::Leaf(p) => GeometryCell::Pane(*p),
        Node::Split { dir, children } => GeometryCell::Split {
            dir: *dir,
            children: children
                .iter()
                .zip(child_rects(*dir, rect, children))
                .map(|(c, r)| export_node(&c.node, r))
                .collect(),
        },
    };
    LayoutGeometry {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: rect.rows,
        cell,
    }
}

/// How deep [`LayoutTree::import`] lets cells nest. A tree of `n` panes
/// is at most `n - 1` splits deep, and no window holds this many panes.
const IMPORT_MAX_DEPTH: usize = 256;

/// How many cells a tmux layout string may hold past the window's panes
/// ([`LayoutTree::apply_tmux_layout`]). More cells than this cannot tile
/// any window a terminal shows.
const MAX_LAYOUT_CELLS: usize = 4096;

/// Whether `g` nests no deeper than [`IMPORT_MAX_DEPTH`] and holds no more
/// than `budget` panes, found without going past either bound: every
/// split's children each hold a pane at least, so a split with more
/// children than the budget left fails before they are walked.
fn bounded(g: &LayoutGeometry, depth: usize, budget: &mut usize) -> bool {
    if depth > IMPORT_MAX_DEPTH {
        return false;
    }
    match &g.cell {
        GeometryCell::Pane(_) => match budget.checked_sub(1) {
            Some(left) => {
                *budget = left;
                true
            }
            None => false,
        },
        GeometryCell::Split { children, .. } => {
            children.len() <= *budget && children.iter().all(|c| bounded(c, depth + 1, budget))
        }
    }
}

/// The node `geometry` describes, weighted by its cells' sizes; `None`
/// unless every cell has room and every split has two or more children
/// that span it along its axis, one border apart, and fill it across.
fn import_node(g: &LayoutGeometry) -> Option<Node> {
    if g.cols == 0 || g.rows == 0 {
        return None;
    }
    let GeometryCell::Split { dir, children } = &g.cell else {
        let GeometryCell::Pane(p) = g.cell else {
            unreachable!("a cell is a pane or a split")
        };
        return Some(Node::Leaf(p));
    };
    if children.len() < 2 {
        return None;
    }
    // (start, len) along the split's axis, and across it.
    let along = |c: &LayoutGeometry| match dir {
        Dir::Horizontal => (c.x, c.cols),
        Dir::Vertical => (c.y, c.rows),
    };
    let across = |c: &LayoutGeometry| match dir {
        Dir::Horizontal => (c.y, c.rows),
        Dir::Vertical => (c.x, c.cols),
    };
    let (start, len) = along(g);
    let mut next = u32::from(start);
    let mut out = Vec::with_capacity(children.len());
    for c in children {
        let (s, l) = along(c);
        if u32::from(s) != next || across(c) != across(g) {
            return None;
        }
        next = u32::from(s) + u32::from(l) + 1;
        out.push(Child {
            weight: u32::from(l),
            node: import_node(c)?,
        });
    }
    (next == u32::from(start) + u32::from(len) + 1).then_some(Node::Split {
        dir: *dir,
        children: out,
    })
}

/// Sizes for `n` cells side by side along `len`, as tmux's
/// `layout_spread_cell`: the cells left after the `n - 1` borders are
/// split evenly and the leftover go to the first cells, one each. `None`
/// if some cell would get nothing.
fn spread(len: u16, n: usize) -> Option<Vec<u32>> {
    let n = u16::try_from(n).ok()?;
    let avail = len.checked_sub(n - 1)?;
    let each = avail / n;
    if each == 0 {
        return None;
    }
    let extra = avail % n;
    Some(
        (0..n)
            .map(|i| u32::from(each + u16::from(i < extra)))
            .collect(),
    )
}

/// A split in `dir` of `panes` spread evenly along `len`.
fn even(dir: Dir, len: u16, panes: &[PaneId]) -> Option<Node> {
    let children = spread(len, panes.len())?
        .into_iter()
        .zip(panes)
        .map(|(weight, pane)| Child {
            weight,
            node: Node::Leaf(*pane),
        })
        .collect();
    Some(Node::Split { dir, children })
}

/// tmux's `layout_set_main_v` (`dir` horizontal: main pane on the left,
/// the rest stacked on the right) and `layout_set_main_h` (`dir`
/// vertical: main pane on top, the rest side by side below), or their
/// `-mirrored` twins with the main pane last. The main pane is `main`
/// cells or percent of the cells left after the border; a cell count
/// past them is tmux's default instead (80 wide, 24 tall), and either way
/// the other side keeps at least one cell.
fn main_split(
    area: Rect,
    dir: Dir,
    main: MainSize,
    panes: &[PaneId],
    mirrored: bool,
) -> Option<Node> {
    let (first, rest) = panes.split_first()?;
    let (other_dir, fallback) = match dir {
        Dir::Horizontal => (Dir::Vertical, MainSize::WIDTH),
        Dir::Vertical => (Dir::Horizontal, MainSize::HEIGHT),
    };
    // The cells left after the border between the main pane and the rest.
    let avail = axis_len(dir, area).checked_sub(1).filter(|&a| a >= 2)?;
    let cells = |m: MainSize| match m {
        MainSize::Cells(c) => c,
        MainSize::Percent(p) => (u32::from(avail) * u32::from(p.min(100)) / 100) as u16,
    };
    let want = match main {
        MainSize::Cells(c) if c > avail => cells(fallback),
        m => cells(m),
    };
    // ponytail: a main size of 0 gives 1 cell here; tmux 3.7b gives a
    // degenerate 0-cell main pane, which a tree of >= 1-cell weights cannot hold.
    let main_cells = want.clamp(1, avail - 1);
    let others = match rest {
        [only] => Node::Leaf(*only),
        _ => even(other_dir, axis_len(other_dir, area), rest)?,
    };
    let main_child = Child {
        weight: u32::from(main_cells),
        node: Node::Leaf(*first),
    };
    let others = Child {
        weight: u32::from(avail - main_cells),
        node: others,
    };
    Some(Node::Split {
        dir,
        children: pair(main_child, others, mirrored),
    })
}

/// tmux's `layout_set_tiled`: the fewest rows, then columns (up to
/// `max_columns`, if not 0), that hold every pane, filled row by row, each
/// cell the same size and the last row and each row's last cell taking
/// what is left over; a pane alone on the last row spans it. `None` when
/// the grid needs more room than `area` has (D18).
fn tiled(area: Rect, panes: &[PaneId], max_columns: u16) -> Option<Node> {
    let n = panes.len();
    let (mut rows, mut cols) = (1usize, 1usize);
    while rows * cols < n {
        rows += 1;
        if rows * cols < n && (max_columns == 0 || cols < usize::from(max_columns)) {
            cols += 1;
        }
    }
    let cells = |len: u16, k: usize| -> Option<u16> {
        let k = u16::try_from(k).ok()?;
        let each = len.checked_sub(k - 1)? / k;
        (each > 0).then_some(each)
    };
    let (width, height) = (cells(area.cols, cols)?, cells(area.rows, rows)?);
    let mut rest = panes;
    let mut row_nodes = Vec::with_capacity(rows);
    while !rest.is_empty() {
        let (row, more) = rest.split_at(cols.min(rest.len()));
        rest = more;
        let node = match row {
            [only] => Node::Leaf(*only),
            _ => {
                let mut children: Vec<Child> = row
                    .iter()
                    .map(|p| Child {
                        weight: u32::from(width),
                        node: Node::Leaf(*p),
                    })
                    .collect();
                let used = row.len() as u32 * (u32::from(width) + 1) - 1;
                if let Some(last) = children.last_mut() {
                    last.weight += u32::from(area.cols).saturating_sub(used);
                }
                Node::Split {
                    dir: Dir::Horizontal,
                    children,
                }
            }
        };
        row_nodes.push(Child {
            weight: u32::from(height),
            node,
        });
    }
    let used = row_nodes.len() as u32 * (u32::from(height) + 1) - 1;
    if let Some(last) = row_nodes.last_mut() {
        last.weight += u32::from(area.rows).saturating_sub(used);
    }
    Some(Node::Split {
        dir: Dir::Vertical,
        children: row_nodes,
    })
}

/// `target_size, new_size` for splitting `avail` cells (already short one
/// cell for the new border) between the existing pane and the new one.
/// `None` if there's no room for both to keep at least one cell.
fn split_sizes(avail: u16, size: SplitSize) -> Option<(u16, u16)> {
    if avail < 2 {
        return None;
    }
    let new_size = match size {
        SplitSize::Half => avail / 2,
        SplitSize::Percent(p) => {
            let p = u32::from(p.min(100));
            ((u32::from(avail) * p + 50) / 100) as u16
        }
        SplitSize::Cells(n) => n,
    };
    let new_size = new_size.clamp(1, avail - 1);
    Some((avail - new_size, new_size))
}

/// Splits `total` cells among `weights` proportionally, rounding down and
/// then handing the leftover cells one at a time to the largest fractional
/// remainders, ties broken by the lower index. Deterministic, and the result
/// always sums to exactly `total`.
///
/// Tries the plain proportional pass first: when `weights` already sum to
/// `total` (every group, rendered on the tree's `exact_for` area), this
/// reproduces those weights exactly, with no rounding drift. Only if that
/// pass would give some child zero cells despite its weight being >= 1
/// (only possible when `total` differs from the weights' sum, i.e. rendering
/// on another area) does it fall back to reserving one cell per child up
/// front and distributing the rest the same way, so a rescale never zeroes a
/// pane out.
fn distribute(total: u16, weights: &[u32]) -> Vec<u16> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    if (total as usize) < n {
        // Not enough cells for every child to get one. Simplest well-defined
        // floor: children at lower indices get one cell each until cells run
        // out, the rest get none. M2 doesn't shrink a window below one cell
        // per pane in practice, so this path exists to be safe, not fair.
        let mut sizes = vec![0u16; n];
        for size in sizes.iter_mut().take(total as usize) {
            *size = 1;
        }
        return sizes;
    }
    let plain = distribute_proportional(total, weights);
    if plain.iter().all(|&size| size >= 1) {
        return plain;
    }
    // The plain pass zeroed a child out: reserve one cell per child up front
    // so it can't, then distribute what's left the same way.
    let reserved = n as u16;
    let mut sizes = distribute_proportional(total - reserved, weights);
    for size in &mut sizes {
        *size += 1;
    }
    sizes
}

/// The proportional pass `distribute` tries first, and falls back to around
/// a one-cell-per-child reservation: splits `total` cells among `weights` by
/// weight, rounding down and handing the leftover cells one at a time to the
/// largest fractional remainders, ties broken by the lower index. The result
/// always sums to exactly `total` but, unlike `distribute`, may give a child
/// zero cells.
fn distribute_proportional(total: u16, weights: &[u32]) -> Vec<u16> {
    let n = weights.len();
    let total_weight: u64 = weights.iter().map(|&w| u64::from(w.max(1))).sum();
    let total = u64::from(total);
    let mut sizes = vec![0u16; n];
    let mut remainders = vec![0u64; n];
    let mut assigned = 0u64;
    for (i, &w) in weights.iter().enumerate() {
        let w = u64::from(w.max(1));
        let raw = total * w;
        let base = raw / total_weight;
        sizes[i] = base as u16;
        remainders[i] = raw % total_weight;
        assigned += base;
    }
    let mut leftover = total.saturating_sub(assigned);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| remainders[b].cmp(&remainders[a]).then(a.cmp(&b)));
    for i in order {
        if leftover == 0 {
            break;
        }
        sizes[i] += 1;
        leftover -= 1;
    }
    sizes
}

fn axis_len(dir: Dir, rect: Rect) -> u16 {
    match dir {
        Dir::Horizontal => rect.cols,
        Dir::Vertical => rect.rows,
    }
}

/// The rects of `children` laid out in `dir` across `area`.
fn child_rects(dir: Dir, area: Rect, children: &[Child]) -> Vec<Rect> {
    let weights: Vec<u32> = children.iter().map(|c| c.weight).collect();
    let n = weights.len() as u16;
    match dir {
        Dir::Horizontal => {
            let avail = area.cols.saturating_sub(n.saturating_sub(1));
            let sizes = distribute(avail, &weights);
            let bound = area.x + area.cols;
            let mut x = area.x;
            let mut rects = Vec::with_capacity(sizes.len());
            for w in sizes {
                rects.push(Rect {
                    x,
                    y: area.y,
                    cols: w,
                    rows: area.rows,
                });
                // Clamped to `bound`: when `area` itself is too short even
                // for the borders (a deeper split's local share can be
                // squeezed to 0 by an ancestor's below-floor rescale), the
                // "+1" for a border between children would otherwise walk
                // every remaining child's rect past the parent's own area.
                x = (x + w + 1).min(bound);
            }
            rects
        }
        Dir::Vertical => {
            let avail = area.rows.saturating_sub(n.saturating_sub(1));
            let sizes = distribute(avail, &weights);
            let bound = area.y + area.rows;
            let mut y = area.y;
            let mut rects = Vec::with_capacity(sizes.len());
            for w in sizes {
                rects.push(Rect {
                    x: area.x,
                    y,
                    cols: area.cols,
                    rows: w,
                });
                // See the Horizontal arm above: clamped so a degenerate
                // (too-small-for-its-own-borders) area can't push later
                // children's rects past it.
                y = (y + w + 1).min(bound);
            }
            rects
        }
    }
}

/// Whether rendering `node` on `rect` gives every split's every child at
/// least one cell along that split's axis (`LayoutTree::rebase`'s floor).
fn renders_nonzero(node: &Node, rect: Rect) -> bool {
    match node {
        Node::Leaf(_) => true,
        Node::Split { dir, children } => {
            let rects = child_rects(*dir, rect, children);
            children
                .iter()
                .zip(rects)
                .all(|(c, r)| axis_len(*dir, r) >= 1 && renders_nonzero(&c.node, r))
        }
    }
}

/// Overwrites every weight under `node` with the size rendering it on
/// `rect` gives that child (`LayoutTree::rebase`).
fn store_rendered(node: &mut Node, rect: Rect) {
    if let Node::Split { dir, children } = node {
        let rects = child_rects(*dir, rect, children);
        for (child, r) in children.iter_mut().zip(rects) {
            child.weight = u32::from(axis_len(*dir, r));
            store_rendered(&mut child.node, r);
        }
    }
}

fn find_path(node: &Node, pane: PaneId, path: &mut Vec<usize>) -> bool {
    match node {
        Node::Leaf(p) => *p == pane,
        Node::Split { children, .. } => {
            for (i, child) in children.iter().enumerate() {
                path.push(i);
                if find_path(&child.node, pane, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
    }
}

fn node_at<'a>(node: &'a Node, path: &[usize]) -> &'a Node {
    match path {
        [] => node,
        [idx, rest @ ..] => match node {
            Node::Split { children, .. } => node_at(&children[*idx].node, rest),
            Node::Leaf(_) => node,
        },
    }
}

fn node_at_mut<'a>(node: &'a mut Node, path: &[usize]) -> &'a mut Node {
    match path {
        [] => node,
        [idx, rest @ ..] => match node {
            Node::Split { children, .. } => node_at_mut(&mut children[*idx].node, rest),
            Node::Leaf(_) => node,
        },
    }
}

/// Grows `node`'s own extent along `dir` by `delta` cells ([`adjust`]).
fn grow(node: &mut Node, dir: Dir, delta: u32) {
    adjust(node, dir, i64::from(delta));
}

/// Shrinks `node`'s own extent along `dir` by `delta` cells ([`adjust`]).
/// The caller first clamps `delta` to `max_shrink(node, own_extent, dir)`.
fn shrink(node: &mut Node, dir: Dir, delta: u32) {
    adjust(node, dir, -i64::from(delta));
}

/// tmux's `layout_resize_adjust`: `node`'s own extent along `dir` changed
/// by `change` cells (its weight, which lives in its parent's `Child`,
/// the caller already changed); this passes the change down. A leaf needs
/// nothing more. A split along the other axis does not divide `dir`, so
/// every child takes the whole change. A split along `dir` hands it out
/// one cell at a time, round-robin from its first child, a shrink skipping
/// children that cannot give a cell, until it is used up (or, for a shrink
/// the caller did not clamp, no child can give).
fn adjust(node: &mut Node, dir: Dir, mut change: i64) {
    let Node::Split {
        dir: node_dir,
        children,
    } = node
    else {
        return;
    };
    if *node_dir != dir {
        for child in children.iter_mut() {
            adjust(&mut child.node, dir, change);
        }
        return;
    }
    while change != 0 {
        let before = change;
        for child in children.iter_mut() {
            if change == 0 {
                break;
            }
            if change > 0 {
                child.weight += 1;
                adjust(&mut child.node, dir, 1);
                change -= 1;
            } else if max_shrink(&child.node, child.weight, dir) > 0 {
                child.weight -= 1;
                adjust(&mut child.node, dir, -1);
                change += 1;
            }
        }
        if change == before {
            return;
        }
    }
}

/// The most `node`'s own extent along `dir` can shrink by without any leaf
/// under it dropping below one cell, where `own_extent` is `node`'s current
/// size along `dir` (`Node` doesn't store its own size). Every weight read
/// here is an exact cell count, since `resize` rebases the tree onto its
/// area first. Matches tmux's `layout_resize_check`; `resize`
/// clamps a requested shrink to this before touching any weight, so `shrink`
/// itself never has to fail partway through.
///
/// A leaf can give up everything but its last cell: `own_extent - 1`. A
/// split along `dir` divides its own extent among its children (plus their
/// shared borders, fixed either way), so its own ceiling is the sum of what
/// each child can individually spare, independent of which of them a real
/// shrink would actually take from. A split along the other axis passes
/// `dir` through to every child unchanged, so shrinking it by any amount
/// shrinks what every child inherits by that same amount; the group's
/// ceiling is however little its tightest child can spare.
fn max_shrink(node: &Node, own_extent: u32, dir: Dir) -> u32 {
    match node {
        Node::Leaf(_) => own_extent.saturating_sub(1),
        Node::Split {
            dir: node_dir,
            children,
        } => {
            if *node_dir == dir {
                children
                    .iter()
                    .map(|c| max_shrink(&c.node, c.weight, dir))
                    .sum()
            } else {
                children
                    .iter()
                    .map(|c| max_shrink(&c.node, own_extent, dir))
                    .min()
                    .unwrap_or(0)
            }
        }
    }
}

fn rect_at_path(node: &Node, area: Rect, path: &[usize]) -> Rect {
    match path {
        [] => area,
        [idx, rest @ ..] => match node {
            Node::Split { dir, children } => {
                let rects = child_rects(*dir, area, children);
                rect_at_path(&children[*idx].node, rects[*idx], rest)
            }
            Node::Leaf(_) => area,
        },
    }
}

/// The sibling index across `edge` from child `idx` in a split of direction
/// `dir` with `len` children, if `edge`'s axis matches `dir` and that
/// neighbour exists.
fn far_sibling(edge: Edge, dir: Dir, idx: usize, len: usize) -> Option<usize> {
    match (edge, dir) {
        (Edge::Left, Dir::Horizontal) if idx > 0 => Some(idx - 1),
        (Edge::Right, Dir::Horizontal) if idx + 1 < len => Some(idx + 1),
        (Edge::Up, Dir::Vertical) if idx > 0 => Some(idx - 1),
        (Edge::Down, Dir::Vertical) if idx + 1 < len => Some(idx + 1),
        _ => None,
    }
}

/// Descends `node` (occupying `rect`) to the leaf whose span along `axis`
/// contains `coord`. Falls back to the last child if `coord` lands exactly on
/// a border between children (deterministic, if an edge case).
fn find_leaf_at(node: &Node, rect: Rect, axis: Axis, coord: u16) -> PaneId {
    match node {
        Node::Leaf(pane) => *pane,
        Node::Split { dir, children } => {
            let rects = child_rects(*dir, rect, children);
            for (child, r) in children.iter().zip(rects.iter()) {
                let (start, len) = match axis {
                    Axis::Row => (r.y, r.rows),
                    Axis::Col => (r.x, r.cols),
                };
                if coord >= start && coord < start + len {
                    return find_leaf_at(&child.node, *r, axis, coord);
                }
            }
            let last = children.len() - 1;
            find_leaf_at(&children[last].node, rects[last], axis, coord)
        }
    }
}

fn collect_rects(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(pane) => out.push((*pane, area)),
        Node::Split { dir, children } => {
            let rects = child_rects(*dir, area, children);
            for (child, rect) in children.iter().zip(rects) {
                collect_rects(&child.node, rect, out);
            }
        }
    }
}

fn collect_borders(node: &Node, area: Rect, out: &mut Vec<BorderSegment>) {
    if let Node::Split { dir, children } = node {
        let rects = child_rects(*dir, area, children);
        for pair in rects.windows(2) {
            let r = pair[0];
            match dir {
                Dir::Horizontal => {
                    let col = r.x + r.cols;
                    // `area` itself can be a degenerate (zero-width) slot a
                    // parent's below-floor rescale squeezed a whole subtree
                    // into (see `child_rects`); its children then all clamp
                    // to the same point and there's no cell left for a
                    // border between them, only for panes drawn beyond it.
                    if col < area.x + area.cols {
                        out.push(BorderSegment::Vertical {
                            col,
                            row: area.y,
                            len: area.rows,
                        });
                    }
                }
                Dir::Vertical => {
                    let row = r.y + r.rows;
                    if row < area.y + area.rows {
                        out.push(BorderSegment::Horizontal {
                            row,
                            col: area.x,
                            len: area.cols,
                        });
                    }
                }
            }
        }
        for (child, rect) in children.iter().zip(rects) {
            collect_borders(&child.node, rect, out);
        }
    }
}

// ---- tmux layout strings (spec §4.3) ------------------------------------

/// A floating pane as a tmux layout string lists it: the pane, its
/// top-left content cell (which may be off the window), its size and its
/// place in the stack (highest on top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingCell {
    pub pane: PaneId,
    pub x: i32,
    pub y: i32,
    pub cols: u16,
    pub rows: u16,
    pub z: u32,
}

/// The tmux layout string (`#{window_layout}`) for `tiled` and the
/// window's floating panes (in the order they were made), as tmux 3.7b's
/// `layout_dump`: a four-digit hex checksum, then `WxH,X,Y,<pane>` for a
/// pane, `WxH,X,Y{...}` for cells side by side, `WxH,X,Y[...]` for
/// stacked ones, and the floating panes in `<...>`, topmost first.
///
/// tmux 3.7b also keeps each floating pane's cell in the root, after the
/// tiled cells, a lone tiled pane becoming a stack of it and them
/// (recorded: `80x24,0,0[80x24,0,0,0,20x5,3,4,1]<20x5,3,4,1>`), except
/// in a zoomed window, which shows the root saved when it zoomed
/// (`in_root` false).
// ponytail: tmux then splits a lone tiled pane inside that stack; grofe's
// root is the split itself, so after a float the string differs there.
pub fn tmux_layout(tiled: &LayoutGeometry, floating: &[FloatingCell], in_root: bool) -> String {
    let cell = |f: &FloatingCell| format!("{}x{},{},{},{}", f.cols, f.rows, f.x, f.y, f.pane.0);
    let mut body = String::new();
    append_cell(tiled, &mut body);
    if !floating.is_empty() {
        let made: Vec<String> = floating.iter().map(cell).collect();
        let made = made.join(",");
        body = match &tiled.cell {
            _ if !in_root => body,
            GeometryCell::Pane(_) => {
                let (w, h, x, y) = (tiled.cols, tiled.rows, tiled.x, tiled.y);
                format!("{w}x{h},{x},{y}[{body},{made}]")
            }
            GeometryCell::Split { .. } => {
                let close = body.pop().unwrap_or(']');
                format!("{body},{made}{close}")
            }
        };
        let mut stack: Vec<&FloatingCell> = floating.iter().collect();
        stack.sort_by_key(|f| std::cmp::Reverse(f.z));
        let stack: Vec<String> = stack.into_iter().map(cell).collect();
        body.push_str(&format!("<{}>", stack.join(",")));
    }
    format!("{:04x},{body}", tmux_checksum(&body))
}

fn append_cell(g: &LayoutGeometry, out: &mut String) {
    out.push_str(&format!("{}x{},{},{}", g.cols, g.rows, g.x, g.y));
    match &g.cell {
        GeometryCell::Pane(p) => out.push_str(&format!(",{}", p.0)),
        GeometryCell::Split { dir, children } => {
            let (open, close) = match dir {
                Dir::Horizontal => ('{', '}'),
                Dir::Vertical => ('[', ']'),
            };
            out.push(open);
            for (i, c) in children.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                append_cell(c, out);
            }
            out.push(close);
        }
    }
}

/// tmux's `layout_checksum`: a 16-bit rotate-and-add over the bytes.
pub fn tmux_checksum(s: &str) -> u16 {
    s.bytes().fold(0u16, |sum, b| {
        let sum = (sum >> 1).wrapping_add((sum & 1) << 15);
        sum.wrapping_add(u16::from(b))
    })
}

/// The cells of a tmux layout string, as tmux 3.7b's `layout_parse` and
/// `layout_construct` read them: the checksum must match the rest, and the
/// cells must nest and close. The panes are numbered in cell order from
/// `%0`; the string's own pane numbers are not kept. `None` for tmux's
/// `invalid layout`, and for cells nested past [`IMPORT_MAX_DEPTH`].
pub fn parse_tmux_layout(s: &str) -> Option<LayoutGeometry> {
    let (sum, body) = (s.get(..4)?, s.get(4..)?.strip_prefix(',')?);
    if !sum.bytes().all(|b| b.is_ascii_hexdigit())
        || u16::from_str_radix(sum, 16).ok()? != tmux_checksum(body)
    {
        return None;
    }
    let mut rest = body.as_bytes();
    let mut panes = 0;
    let g = construct(&mut rest, 0, &mut panes)?;
    rest.is_empty().then_some(g)
}

/// A run of digits at the start of `rest` as a number, consumed.
fn digits(rest: &mut &[u8]) -> Option<u32> {
    let n = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let (num, tail) = rest.split_at(n);
    *rest = tail;
    std::str::from_utf8(num).ok()?.parse().ok()
}

fn expect(rest: &mut &[u8], byte: u8) -> Option<()> {
    let (&first, tail) = rest.split_first()?;
    (first == byte).then(|| *rest = tail)
}

/// One cell and its children (`layout_construct`).
fn construct(rest: &mut &[u8], depth: usize, panes: &mut u32) -> Option<LayoutGeometry> {
    if depth > IMPORT_MAX_DEPTH {
        return None;
    }
    let cols = u16::try_from(digits(rest)?).ok()?;
    expect(rest, b'x')?;
    let rows = u16::try_from(digits(rest)?).ok()?;
    expect(rest, b',')?;
    let x = u16::try_from(digits(rest)?).ok()?;
    expect(rest, b',')?;
    let y = u16::try_from(digits(rest)?).ok()?;
    // A pane number, unless the digits after the comma start the next
    // cell (`WxH`).
    if let [b',', tail @ ..] = *rest {
        let mut after = tail;
        let _ = digits(&mut after);
        if after.first() != Some(&b'x') {
            *rest = after;
        }
    }
    let dir = match rest.first() {
        None | Some(b',' | b'}' | b']' | b'>') => {
            *panes += 1;
            let cell = GeometryCell::Pane(PaneId(*panes - 1));
            return Some(LayoutGeometry {
                x,
                y,
                cols,
                rows,
                cell,
            });
        }
        Some(b'{') => Dir::Horizontal,
        Some(b'[') => Dir::Vertical,
        Some(_) => return None,
    };
    let mut children = Vec::new();
    loop {
        *rest = &rest[1..];
        children.push(construct(rest, depth + 1, panes)?);
        if rest.first() != Some(&b',') {
            break;
        }
    }
    expect(rest, if dir == Dir::Horizontal { b'}' } else { b']' })?;
    Some(LayoutGeometry {
        x,
        y,
        cols,
        rows,
        cell: GeometryCell::Split { dir, children },
    })
}

fn count_cells(g: &LayoutGeometry, n: &mut usize) {
    match &g.cell {
        GeometryCell::Pane(_) => *n += 1,
        GeometryCell::Split { children, .. } => children.iter().for_each(|c| count_cells(c, n)),
    }
}

/// tmux's fix for layouts older versions wrote with a wrong top cell size:
/// a split root takes its size from its children.
fn fix_root_size(g: &mut LayoutGeometry) {
    let GeometryCell::Split { dir, children } = &g.cell else {
        return;
    };
    let (mut cols, mut rows) = (0u32, 0u32);
    for c in children {
        match dir {
            Dir::Horizontal => {
                rows = u32::from(c.rows) + 1;
                cols += u32::from(c.cols) + 1;
            }
            Dir::Vertical => {
                cols = u32::from(c.cols) + 1;
                rows += u32::from(c.rows) + 1;
            }
        }
    }
    g.cols = u16::try_from(cols.saturating_sub(1)).unwrap_or(u16::MAX);
    g.rows = u16::try_from(rows.saturating_sub(1)).unwrap_or(u16::MAX);
}

/// The pane `select-pane -L/-R/-U/-D` moves to from `pane`: tmux 3.7b's
/// `window_pane_find_left`/`_right`/`_up`/`_down`. Of the panes touching
/// the edge across `edge` (past the window's edge it wraps to the far
/// side) and overlapping `pane` along it, the most recently active wins
/// (`point` is tmux's `active_point`; the first in list order on a tie).
/// `panes` are the content rects in the window's pane order, `area` the
/// window.
pub fn find_adjacent(
    panes: &[(PaneId, Rect)],
    area: Rect,
    status: PaneStatus,
    pane: PaneId,
    edge: Edge,
    point: impl Fn(PaneId) -> u64,
) -> Option<PaneId> {
    let geom = |r: &Rect| {
        (
            i32::from(r.x),
            i32::from(r.y),
            i32::from(r.cols),
            i32::from(r.rows),
        )
    };
    // A framed window's content rects (spec 2026-11-05-frames.md) are its
    // tree's one-cell-border rects on the inner area, each `pad` cells
    // short on the right and bottom (module docs). An unframed window
    // always has a pane at column 0 and a framed one has none, its
    // leftmost pane `pad` cells in: so the rects go back on the inner
    // area, with no border status (frames ignore it), and the walk below
    // is tmux's unchanged.
    let pad = panes.iter().map(|(_, r)| r.x).min().unwrap_or(0);
    let unframed: Vec<(PaneId, Rect)>;
    let (panes, area, status) = if pad == 0 {
        (panes, area, status)
    } else {
        unframed = panes
            .iter()
            .map(|(id, r)| {
                let r = Rect {
                    x: r.x - pad,
                    y: r.y.saturating_sub(pad),
                    cols: r.cols.saturating_add(pad),
                    rows: r.rows.saturating_add(pad),
                };
                (*id, r)
            })
            .collect();
        let area = Rect {
            cols: area.cols.saturating_sub(pad),
            rows: area.rows.saturating_sub(pad),
            ..area
        };
        (unframed.as_slice(), area, PaneStatus::Off)
    };
    let (wx, wy) = (i32::from(area.cols), i32::from(area.rows));
    let (xoff, yoff, sx, sy) = geom(&panes.iter().find(|(id, _)| *id == pane)?.1);
    // `touches` says whether another pane sits on the edge; `span` is the
    // pane's extent along it, and `lo`/`hi` the extent to overlap.
    let (lo, hi) = match edge {
        Edge::Up | Edge::Down => (xoff, xoff + sx),
        Edge::Left | Edge::Right => (yoff, yoff + sy),
    };
    let edge_at = match edge {
        Edge::Up => match status {
            PaneStatus::Top if yoff == 1 => wy + 1,
            PaneStatus::Bottom if yoff == 0 => wy,
            PaneStatus::Off if yoff == 0 => wy + 1,
            _ => yoff,
        },
        Edge::Down => {
            let e = yoff + sy + 1;
            match status {
                PaneStatus::Top if e >= wy => 1,
                PaneStatus::Bottom if e >= wy - 1 => 0,
                PaneStatus::Off if e >= wy => 0,
                _ => e,
            }
        }
        Edge::Left => {
            if xoff == 0 {
                wx + 1
            } else {
                xoff
            }
        }
        Edge::Right => {
            let e = xoff + sx + 1;
            if e >= wx { 0 } else { e }
        }
    };
    let mut best: Option<PaneId> = None;
    for (id, r) in panes {
        if *id == pane {
            continue;
        }
        let (x, y, cols, rows) = geom(r);
        let on_edge = match edge {
            Edge::Up => y + rows + 1 == edge_at,
            Edge::Down => y == edge_at,
            Edge::Left => x + cols + 1 == edge_at,
            Edge::Right => x == edge_at,
        };
        if !on_edge {
            continue;
        }
        let (start, len) = match edge {
            Edge::Up | Edge::Down => (x, cols),
            Edge::Left | Edge::Right => (y, rows),
        };
        let end = start + len - 1;
        let overlaps =
            (start < lo && end > hi) || (start >= lo && start <= hi) || (end >= lo && end <= hi);
        if overlaps && best.is_none_or(|b| point(*id) > point(b)) {
            best = Some(*id);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);
    const C: PaneId = PaneId(3);
    const D: PaneId = PaneId(4);
    const E: PaneId = PaneId(5);

    /// `tiled-layout-max-columns` caps the grid's width: nine panes in
    /// three columns become five rows of two (tmux's `layout_set_tiled`).
    #[test]
    fn tiled_stops_at_the_maximum_columns() {
        let panes: Vec<PaneId> = (0..9).map(PaneId).collect();
        let area = Rect {
            x: 0,
            y: 0,
            cols: 200,
            rows: 60,
        };
        let rows_of = |max_columns: u16| match tiled(area, &panes, max_columns) {
            Some(Node::Split { children, .. }) => children.len(),
            _ => 0,
        };
        assert_eq!(rows_of(0), 3);
        assert_eq!(rows_of(9), 3);
        assert_eq!(rows_of(2), 5);
        assert_eq!(rows_of(1), 9);
    }

    #[test]
    fn single_pane_fills_area() {
        let tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 10,
        };
        assert_eq!(tree.compute(area), vec![(A, area)]);
        assert_eq!(tree.borders(area), vec![]);
    }

    #[test]
    fn split_not_found_errors() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 10,
        };
        assert_eq!(
            tree.split(area, PaneId(99), Dir::Horizontal, B, SplitSize::Half),
            Err(LayoutError::NotFound(PaneId(99)))
        );
    }

    #[test]
    fn split_too_small_errors() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 2,
            rows: 5,
        };
        assert_eq!(
            tree.split(area, A, Dir::Horizontal, B, SplitSize::Half),
            Err(LayoutError::TooSmall(A))
        );
    }

    #[test]
    fn split_minimum_width_succeeds() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 3,
            rows: 5,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols, 1);
        assert_eq!(rects[&B].cols, 1);
    }

    #[test]
    fn half_split_gives_target_the_larger_share_on_odd_avail() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 4,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        let rects = tree.compute(area);
        assert_eq!(
            rects,
            vec![
                (
                    A,
                    Rect {
                        x: 0,
                        y: 0,
                        cols: 5,
                        rows: 4
                    }
                ),
                (
                    B,
                    Rect {
                        x: 6,
                        y: 0,
                        cols: 4,
                        rows: 4
                    }
                ),
            ]
        );
        assert_eq!(
            tree.borders(area),
            vec![BorderSegment::Vertical {
                col: 5,
                row: 0,
                len: 4
            }]
        );
    }

    #[test]
    fn percent_split_rounds_to_nearest() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 41,
            rows: 3,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Percent(25))
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols, 30);
        assert_eq!(rects[&B].cols, 10);
    }

    #[test]
    fn cells_split_and_add_sibling_makes_three_columns() {
        // Matches spec §4.3: splitting in the direction the parent already
        // splits adds a sibling column rather than nesting.
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 17,
            rows: 5,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.split(area, B, Dir::Horizontal, C, SplitSize::Half)
            .unwrap();

        assert_eq!(
            tree.compute(area),
            vec![
                (
                    A,
                    Rect {
                        x: 0,
                        y: 0,
                        cols: 8,
                        rows: 5
                    }
                ),
                (
                    B,
                    Rect {
                        x: 9,
                        y: 0,
                        cols: 4,
                        rows: 5
                    }
                ),
                (
                    C,
                    Rect {
                        x: 14,
                        y: 0,
                        cols: 3,
                        rows: 5
                    }
                ),
            ]
        );
        assert_eq!(
            tree.borders(area),
            vec![
                BorderSegment::Vertical {
                    col: 8,
                    row: 0,
                    len: 5
                },
                BorderSegment::Vertical {
                    col: 13,
                    row: 0,
                    len: 5
                },
            ]
        );
    }

    /// Builds A | (B / C): A left, B and C stacked on the right. Used by the
    /// remove/collapse, neighbour and nested-borders tests below.
    fn a_over_bc_tree() -> (LayoutTree, Rect) {
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 10,
        };
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        tree.split(area, B, Dir::Vertical, C, SplitSize::Cells(3))
            .unwrap();
        (tree, area)
    }

    #[test]
    fn wrap_split_nests_perpendicular_to_parent() {
        let (tree, area) = a_over_bc_tree();
        assert_eq!(
            tree.compute(area),
            vec![
                (
                    A,
                    Rect {
                        x: 0,
                        y: 0,
                        cols: 10,
                        rows: 10
                    }
                ),
                (
                    B,
                    Rect {
                        x: 11,
                        y: 0,
                        cols: 10,
                        rows: 6
                    }
                ),
                (
                    C,
                    Rect {
                        x: 11,
                        y: 7,
                        cols: 10,
                        rows: 3
                    }
                ),
            ]
        );
        assert_eq!(
            tree.borders(area),
            vec![
                BorderSegment::Vertical {
                    col: 10,
                    row: 0,
                    len: 10
                },
                BorderSegment::Horizontal {
                    row: 6,
                    col: 11,
                    len: 10
                },
            ]
        );
    }

    #[test]
    fn remove_collapses_single_child_split() {
        let (mut tree, area) = a_over_bc_tree();
        tree.remove(B);
        assert_eq!(
            tree.compute(area),
            vec![
                (
                    A,
                    Rect {
                        x: 0,
                        y: 0,
                        cols: 10,
                        rows: 10
                    }
                ),
                (
                    C,
                    Rect {
                        x: 11,
                        y: 0,
                        cols: 10,
                        rows: 10
                    }
                ),
            ]
        );
        assert_eq!(
            tree.borders(area),
            vec![BorderSegment::Vertical {
                col: 10,
                row: 0,
                len: 10
            }]
        );
    }

    #[test]
    fn remove_last_pane_is_a_no_op() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 5,
        };
        tree.remove(A);
        assert_eq!(tree.compute(area), vec![(A, area)]);
    }

    #[test]
    fn remove_missing_pane_is_a_no_op() {
        let (mut tree, area) = a_over_bc_tree();
        let before = tree.compute(area);
        tree.remove(PaneId(99));
        assert_eq!(tree.compute(area), before);
    }

    #[test]
    fn remove_down_to_one_pane_collapses_root() {
        let (mut tree, area) = a_over_bc_tree();
        tree.remove(B);
        tree.remove(C);
        assert_eq!(tree.compute(area), vec![(A, area)]);
        assert_eq!(tree.borders(area), vec![]);
    }

    #[test]
    fn remove_first_child_gives_its_cells_to_the_next_sibling() {
        // Same three-pane build as three_pane_cells_split_reproduces_stored_sizes_on_the_build_area:
        // [A(58), C(10), B(10)], sum 78 == 80 - 2 borders.
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        tree.split(area, A, Dir::Horizontal, C, SplitSize::Cells(10))
            .unwrap();
        // A is first in the group; removing it hands its 58 cells, plus the
        // border that's now gone too, to the new first child, C.
        tree.remove(A);
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 69);
        assert_eq!(rects[&B].cols, 10);
    }

    #[test]
    fn split_after_remove_reproduces_the_requested_cells_exactly() {
        // Regression for the fix-round-3 bug: before remove() gave a
        // removed child's cells to its neighbour, the survivors' weights
        // fell short of the group's new local avail, so this Cells(10)
        // split (whose `current` is read from the group's *rendered* size)
        // picked up the drift and rendered 12 cols instead of 10.
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.split(area, A, Dir::Horizontal, C, SplitSize::Half)
            .unwrap();
        tree.remove(C);
        tree.split(area, B, Dir::Horizontal, D, SplitSize::Cells(10))
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&D].cols, 10);
    }

    #[test]
    fn resize_after_remove_in_a_three_pane_group_moves_the_border_by_exactly_the_delta() {
        // Regression for the same fix-round-3 bug, on resize instead of
        // split: three Cells(10) splits give [A(47), D(10), C(10), B(10)];
        // removing B (last) hands its cells to C. Before the fix, the
        // A|D border (unrelated to the removed group member) would still
        // move by less than the requested delta, because resize's own
        // `near`/`far` reads came from a rescaled, non-exact render.
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        tree.split(area, A, Dir::Horizontal, C, SplitSize::Cells(10))
            .unwrap();
        tree.split(area, A, Dir::Horizontal, D, SplitSize::Cells(10))
            .unwrap();
        tree.remove(B);

        let before: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        let border_before = before[&A].cols;

        tree.resize(area, D, Edge::Left, 5);

        let after: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        let border_after = after[&A].cols;
        assert_eq!(border_before - border_after, 5);
    }

    #[test]
    fn collapse_grows_a_nested_same_axis_descendant() {
        // Regression for the fix-round-4 bug: a 2-child collapse promotes
        // the surviving child by one tree level without touching that
        // child's own weight, which is correct for the child directly, but
        // a descendant two levels down whose own split axis realigns with
        // the collapsed level's used to keep weights summing to its OLD
        // extent, unaware its container just grew.
        //
        // root (Vertical) [A(9), B-side (Horizontal, weight 5) [
        //     B-slot (Vertical, weight 4) [B(2), D(2)],
        //     C(3),
        // ]]
        //
        // Removing A collapses root's 2-child Vertical split: the B-side
        // Horizontal split is promoted to root, growing from 5 rows to the
        // full 15 (its own axis, Horizontal/cols, is untouched by this).
        // B-slot is Vertical, the same axis root's collapse just grew, so
        // its own weights (summing to 4, valid for its old 5-row share)
        // would be short of the new 15-row share by 10 (9 removed + 1
        // border) unless that growth is pushed down into it.
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 8,
            rows: 15,
        };
        tree.split(area, A, Dir::Vertical, B, SplitSize::Cells(5))
            .unwrap();
        tree.split(area, B, Dir::Horizontal, C, SplitSize::Cells(3))
            .unwrap();
        tree.split(area, B, Dir::Vertical, D, SplitSize::Cells(2))
            .unwrap();
        tree.remove(A);
        // tmux 3.7b (8x15, the same splits, `kill-pane` A): the 10 rows
        // go round-robin to B and D, 7 and 7.
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&B].rows, 7);
        assert_eq!(rects[&D].rows, 7);
        assert_eq!(rects[&C].cols, 3);
    }

    #[test]
    fn remove_grows_a_nested_same_axis_split_inside_the_neighbour() {
        // Regression for the same fix-round-4 bug, in the non-collapsing
        // remove branch: the neighbour that inherits a removed sibling's
        // cells can itself be a group with a same-axis descendant of its
        // own, which needs the same growth pushed down into it.
        //
        // root (Horizontal) [A(59), neighbour (Vertical, weight 9) [
        //     B(6),
        //     C-slot (Horizontal, weight 3) [C(6), D(2)],
        // ], E(10)]
        //
        // Removing A (first, of 3 root children, so no collapse) hands its
        // 59 cells plus a border to the neighbour directly (root's own
        // weight for that slot). The neighbour's own axis is Vertical,
        // perpendicular to root's Horizontal, so B and C-slot both inherit
        // the wider share; C-slot's own axis is Horizontal again, the same
        // one root's remove just grew, so its C/D weights need that growth
        // pushed down into them too.
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 10,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(20))
            .unwrap();
        tree.split(area, B, Dir::Horizontal, E, SplitSize::Cells(10))
            .unwrap();
        tree.split(area, B, Dir::Vertical, C, SplitSize::Cells(3))
            .unwrap();
        tree.split(area, C, Dir::Horizontal, D, SplitSize::Cells(2))
            .unwrap();
        tree.remove(A);

        // tmux 3.7b (80x10, the same splits, `kill-pane` A, then
        // `resize-pane -t C -L 5`): the 60 columns go round-robin to C and
        // D.
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 36);
        assert_eq!(rects[&D].cols, 32);

        // A follow-up resize inside the neighbour must still be exact.
        tree.resize(area, D, Edge::Left, 5);
        let after: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(after[&C].cols, 31);
        assert_eq!(after[&D].cols, 37);
    }

    /// root (Horizontal) [A(59), P (Vertical, weight 20) [
    ///     B(6),
    ///     Q (Horizontal, weight 3) [C(17), D(2)],
    /// ]]
    ///
    /// on an 80x10 area, built the same way in each of the next three
    /// tests: split A|B by Cells(20), wrap B into B-over-C by Cells(3),
    /// wrap C into C|D by Cells(2).
    fn resize_hole_tree() -> (LayoutTree, Rect) {
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 10,
        };
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(20))
            .unwrap();
        tree.split(area, B, Dir::Vertical, C, SplitSize::Cells(3))
            .unwrap();
        tree.split(area, C, Dir::Horizontal, D, SplitSize::Cells(2))
            .unwrap();
        (tree, area)
    }

    #[test]
    fn resize_grows_into_a_nested_same_axis_group() {
        // Regression for the fix-round-5 bug: resize only set the near and
        // far weights in the immediate parent, never pushing the change
        // into either side's own subtree, so a nested same-axis group
        // (here Q, two levels down from root, on the same Horizontal axis
        // as this resize) kept weights summing to its OLD extent. Growing
        // A shrinks P; P's own axis is Vertical (perpendicular, untouched),
        // but P's child Q is Horizontal again, so Q's C/D weights need
        // that shrink pushed down into them too.
        let (mut tree, area) = resize_hole_tree();
        tree.resize(area, A, Edge::Right, 5);
        // tmux 3.7b (`resize-pane -t A -R 5`): the shrink is taken
        // round-robin, C 13 and D 1.
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 13);
        assert_eq!(rects[&D].cols, 1);
    }

    #[test]
    fn resize_after_a_collapse_stays_exact_in_the_flattened_split() {
        // Same root cause as above, exercised through a collapse first:
        // removing B collapses P (a 2-child Vertical split), promoting Q
        // (Horizontal) to root's own slot. Root's axis is also Horizontal,
        // so root ends up with a same-axis child, same as any nested
        // same-axis descendant elsewhere; resizing C from there (there's no
        // sibling to C's own left inside Q, so this walks up to resize Q
        // itself against A) must still push the change into Q's own C/D.
        let (mut tree, area) = resize_hole_tree();
        tree.remove(B);
        tree.resize(area, C, Edge::Left, 4);
        // tmux 3.7b (`kill-pane` B, `resize-pane -t A -L 4`): C 19, D 4.
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 19);
        assert_eq!(rects[&D].cols, 4);
    }

    #[test]
    fn resize_shrink_spills_across_more_than_one_nested_child() {
        // A big enough shrink request can't be satisfied by the one child
        // adjacent to the moved border alone (unlike grow, which always
        // has somewhere to put an unlimited amount): C's own ceiling (16,
        // it can't go below 1) isn't enough for all 17 cells this resize's
        // clamp allows Q to give up, so the extra cell has to come from
        // C's neighbour, D, too, taking D down to its own floor of 1.
        let (mut tree, area) = resize_hole_tree();
        tree.resize(area, A, Edge::Right, 17);
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 1);
        assert_eq!(rects[&D].cols, 1);
    }

    #[test]
    fn neighbour_picks_by_centre_line_overlap() {
        let (tree, area) = a_over_bc_tree();
        assert_eq!(tree.neighbour(area, A, Edge::Right), Some(B));
        assert_eq!(tree.neighbour(area, B, Edge::Left), Some(A));
        assert_eq!(tree.neighbour(area, C, Edge::Left), Some(A));
        assert_eq!(tree.neighbour(area, C, Edge::Up), Some(B));
        assert_eq!(tree.neighbour(area, B, Edge::Up), None);
        assert_eq!(tree.neighbour(area, A, Edge::Left), None);
        assert_eq!(tree.neighbour(area, A, Edge::Up), None);
    }

    #[test]
    fn find_adjacent_returns_to_the_most_recently_active() {
        // A | B over C: the right column B (rows 0..4), C (rows 5..9).
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 10,
        };
        let r = |x, y, cols, rows| Rect { x, y, cols, rows };
        let panes = [
            (A, r(0, 0, 10, 10)),
            (B, r(11, 0, 10, 4)),
            (C, r(11, 5, 10, 5)),
        ];
        let points = |p: PaneId| if p == C { 5 } else { 1 };
        let find = |p, e, pt: &dyn Fn(PaneId) -> u64| {
            find_adjacent(&panes, area, PaneStatus::Off, p, e, pt)
        };
        // Right from A: both touch; the more recent one wins, not the
        // centre-line one (B).
        assert_eq!(find(A, Edge::Right, &points), Some(C));
        assert_eq!(find(A, Edge::Right, &|p| u64::from(p == B)), Some(B));
        // A tie keeps the first in list order.
        assert_eq!(find(A, Edge::Right, &|_| 0), Some(B));
        assert_eq!(find(B, Edge::Left, &points), Some(A));
        assert_eq!(find(C, Edge::Up, &points), Some(B));
        // Past the edge it wraps.
        assert_eq!(find(A, Edge::Left, &points), Some(C));
        assert_eq!(find(B, Edge::Up, &points), Some(C));
        assert_eq!(find(C, Edge::Down, &points), Some(B));
        assert_eq!(find(B, Edge::Right, &points), Some(A));
        assert_eq!(find(PaneId(99), Edge::Right, &points), None);
    }

    #[test]
    fn find_adjacent_with_a_pane_border_status() {
        // Top status: A's title is row 0, B's is the border row 5.
        let area = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 11,
        };
        let r = |y, rows| Rect {
            x: 0,
            y,
            cols: 10,
            rows,
        };
        let panes = [(A, r(1, 4)), (B, r(6, 5))];
        let find = |p, e| find_adjacent(&panes, area, PaneStatus::Top, p, e, |_| 0);
        assert_eq!(find(A, Edge::Down), Some(B));
        assert_eq!(find(B, Edge::Up), Some(A));
        assert_eq!(find(A, Edge::Up), Some(B));
        assert_eq!(find(B, Edge::Down), Some(A));
    }

    #[test]
    fn neighbour_missing_pane_is_none() {
        let (tree, area) = a_over_bc_tree();
        assert_eq!(tree.neighbour(area, PaneId(99), Edge::Right), None);
    }

    #[test]
    fn resize_grows_one_side_and_shrinks_the_other() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 5,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.resize(area, A, Edge::Right, 3);
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols, 13);
        assert_eq!(rects[&B].cols, 7);
    }

    #[test]
    fn resize_clamps_to_minimum_one_cell() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 5,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.resize(area, A, Edge::Right, 100);
        // resize's own clamp pins weights to [19, 1] (B at its one-cell
        // minimum). Rendered on the same area those weights were set
        // against, compute() reproduces them exactly: B stays at 1 cell,
        // never below.
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols, 19);
        assert_eq!(rects[&B].cols, 1);
    }

    #[test]
    fn resize_past_the_clamp_is_a_no_op() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 5,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.resize(area, A, Edge::Right, 100);
        let before = tree.compute(area);
        tree.resize(area, A, Edge::Right, 5);
        assert_eq!(tree.compute(area), before);
    }

    #[test]
    fn resize_with_no_neighbour_is_a_no_op() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 5,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        let before = tree.compute(area);
        tree.resize(area, A, Edge::Left, 5);
        assert_eq!(tree.compute(area), before);
    }

    #[test]
    fn resize_missing_pane_is_a_no_op() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 21,
            rows: 5,
        };
        let before = tree.compute(area);
        tree.resize(area, PaneId(99), Edge::Left, 5);
        assert_eq!(tree.compute(area), before);
    }

    #[test]
    fn distribute_breaks_ties_by_lower_index() {
        assert_eq!(distribute(10, &[1, 1, 1]), vec![4, 3, 3]);
    }

    #[test]
    fn distribute_sum_always_matches_total() {
        assert_eq!(distribute(20, &[11, 10]).iter().sum::<u16>(), 20);
        assert_eq!(distribute(7, &[3, 5, 2]).iter().sum::<u16>(), 7);
        assert_eq!(distribute(0, &[3, 5, 2]).iter().sum::<u16>(), 0);
    }

    #[test]
    fn distribute_never_zeroes_a_child_when_total_fits_all() {
        // total == the weights' own sum (the "same area they were set
        // against" case): the plain pass is exact on the first try, so no
        // fallback is needed and none of the review's original numbers
        // apply here.
        assert_eq!(distribute(199, &[1, 198]), vec![1, 198]);
        // Reproduces the cases the review found: on a smaller total (the
        // area shrank since these weights were set), the plain proportional
        // pass alone gave distribute(9, [1, 198]) = [0, 9] and
        // distribute(20, [1, 198]) = [0, 20], even though every weight is
        // >= 1. Both children must get at least one cell whenever
        // total >= children.
        assert_eq!(distribute(9, &[1, 198]), vec![1, 8]);
        assert_eq!(distribute(20, &[1, 198]), vec![1, 19]);
        for result in [
            distribute(199, &[1, 198]),
            distribute(9, &[1, 198]),
            distribute(20, &[1, 198]),
        ] {
            assert!(result.iter().all(|&size| size >= 1));
        }
    }

    #[test]
    fn distribute_below_floor_gives_lowest_indices_one_cell_each() {
        // total < children: there's no well-defined "fair" split (that's the
        // area-too-small-for-every-pane case), so the lowest-index children
        // get one cell each until cells run out, regardless of weight.
        assert_eq!(distribute(0, &[1, 1, 1]), vec![0, 0, 0]);
        assert_eq!(distribute(1, &[1, 1, 1]), vec![1, 0, 0]);
        assert_eq!(distribute(2, &[1, 1, 1]), vec![1, 1, 0]);
        assert_eq!(distribute(1, &[1, 1000]), vec![1, 0]);
    }

    #[test]
    fn cells_split_reproduces_stored_sizes_on_the_build_area() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols, 69);
        assert_eq!(rects[&B].cols, 10);
    }

    #[test]
    fn resize_after_cells_split_moves_the_border_by_exactly_the_delta() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        let before: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        let border_before = before[&A].cols;

        tree.resize(area, B, Edge::Left, 5);

        let after: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        let border_after = after[&A].cols;
        assert_eq!(border_before - border_after, 5);
        assert_eq!(after[&A].cols, 64);
        assert_eq!(after[&B].cols, 15);
    }

    #[test]
    fn three_pane_cells_split_reproduces_stored_sizes_on_the_build_area() {
        let mut tree = LayoutTree::new(A);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        tree.split(area, A, Dir::Horizontal, C, SplitSize::Cells(10))
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols, 58);
        assert_eq!(rects[&C].cols, 10);
        assert_eq!(rects[&B].cols, 10);
    }

    #[test]
    fn compute_on_a_shrunk_area_never_zeroes_a_pane() {
        // Built against a large area with a skewed weight ratio (198:1),
        // then rendered on a much smaller area: exactly the rescale path the
        // review's repro exercises, at the LayoutTree level rather than
        // distribute() directly.
        let big = Rect {
            x: 0,
            y: 0,
            cols: 200,
            rows: 50,
        };
        let mut tree = LayoutTree::new(A);
        tree.split(big, A, Dir::Horizontal, B, SplitSize::Cells(1))
            .unwrap();

        let small = Rect {
            x: 0,
            y: 0,
            cols: 9,
            rows: 50,
        };
        let rects: std::collections::HashMap<_, _> = tree.compute(small).into_iter().collect();
        assert!(rects[&A].cols >= 1);
        assert!(rects[&B].cols >= 1);
        assert_eq!(rects[&A].cols + rects[&B].cols + 1, small.cols);
    }

    fn rect(cols: u16, rows: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        }
    }

    fn root_weights(tree: &LayoutTree) -> Vec<u32> {
        match &tree.root {
            Node::Leaf(_) => Vec::new(),
            Node::Split { children, .. } => children.iter().map(|c| c.weight).collect(),
        }
    }

    #[test]
    fn resize_on_a_non_build_area_never_wraps_a_weight() {
        // Regression for the round-5 review's HIGH bug: max_shrink read
        // weights calibrated to the 80x10 build area while resize ran on a
        // 20x10 one, so the far side's weight wrapped to 65516.
        //
        // root (H) [A, P (V) [H [B, E], H [C, F]]]. Rebased onto 20 cols,
        // root is [A(9), P(10)] and both rows of P are [5, 4]. A can grow
        // by P's tightest row's spare cells, (5 - 1) + (4 - 1) = 7, which
        // takes B, E, C and F all down to one cell.
        let build = rect(80, 10);
        let mut tree = LayoutTree::new(A);
        tree.split(build, A, Dir::Horizontal, B, SplitSize::Cells(40))
            .unwrap();
        tree.split(build, B, Dir::Vertical, C, SplitSize::Half)
            .unwrap();
        tree.split(build, B, Dir::Horizontal, E, SplitSize::Half)
            .unwrap();
        tree.split(build, C, Dir::Horizontal, PaneId(6), SplitSize::Half)
            .unwrap();

        let small = rect(20, 10);
        tree.resize(small, A, Edge::Right, 30);

        assert_eq!(root_weights(&tree), vec![16, 3]);
        assert!(weights_within(&tree.root, small));
        assert!(all_exact(&tree.root, small));
        let rects: std::collections::HashMap<_, _> = tree.compute(small).into_iter().collect();
        assert_eq!(rects[&A].cols, 16);
        for pane in [B, C, E, PaneId(6)] {
            assert_eq!(rects[&pane].cols, 1);
        }
    }

    #[test]
    fn split_on_a_resized_area_gives_exactly_the_requested_cells() {
        let mut tree = LayoutTree::new(A);
        tree.split(rect(80, 24), A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        let resized = rect(40, 24);
        tree.split(resized, B, Dir::Horizontal, C, SplitSize::Cells(10))
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(resized).into_iter().collect();
        assert_eq!(rects[&C].cols, 10);
        assert!(all_exact(&tree.root, resized));
    }

    #[test]
    fn resize_on_a_resized_area_moves_the_border_by_exactly_the_delta() {
        let mut tree = LayoutTree::new(A);
        tree.split(rect(80, 24), A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        let resized = rect(50, 24);
        let before: std::collections::HashMap<_, _> = tree.compute(resized).into_iter().collect();
        tree.resize(resized, B, Edge::Left, 5);
        let after: std::collections::HashMap<_, _> = tree.compute(resized).into_iter().collect();
        assert_eq!(before[&A].cols - after[&A].cols, 5);
        assert_eq!(after[&B].cols - before[&B].cols, 5);
    }

    #[test]
    fn rebase_keeps_compute_unchanged_and_makes_the_tree_exact() {
        for (cols, rows) in [(80, 10), (30, 7), (120, 40), (20, 7)] {
            let (mut tree, _) = resize_hole_tree();
            let area = rect(cols, rows);
            let rects = tree.compute(area);
            let borders = tree.borders(area);
            assert!(tree.rebase(area));
            assert_eq!(tree.compute(area), rects);
            assert_eq!(tree.borders(area), borders);
            assert!(all_exact(&tree.root, area));
            assert_eq!(tree.exact_for, Some((cols, rows)));
        }
        // 9 cols renders Q (C|D) on 2 cols, one short of C, D and their
        // border: below the floor, so nothing changes.
        let (mut tree, _) = resize_hole_tree();
        assert!(!tree.rebase(rect(9, 5)));
        assert_eq!(tree.exact_for, Some((80, 10)));
    }

    #[test]
    fn below_floor_area_refuses_to_mutate() {
        // Three columns need 3 cells plus 2 borders; a 4-col area renders
        // one of them at zero cells, so ops there refuse rather than store
        // a zero-cell pane, and the tree stays exact on the build area.
        let build = rect(80, 10);
        let mut tree = LayoutTree::new(A);
        tree.split(build, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.split(build, B, Dir::Horizontal, C, SplitSize::Half)
            .unwrap();
        let before = tree.compute(build);

        let tiny = rect(4, 10);
        assert_eq!(
            tree.split(tiny, A, Dir::Vertical, D, SplitSize::Half),
            Err(LayoutError::TooSmall(A))
        );
        tree.resize(tiny, A, Edge::Right, 1);

        assert_eq!(tree.exact_for, Some((80, 10)));
        assert_eq!(tree.compute(build), before);
        assert!(all_exact(&tree.root, build));
    }

    #[test]
    fn remove_after_a_rebase_stays_exact_on_the_new_area() {
        let (mut tree, _) = resize_hole_tree();
        let area = rect(33, 12);
        tree.resize(area, A, Edge::Right, 2);
        tree.remove(B);
        tree.remove(D);
        assert_eq!(tree.exact_for, Some((33, 12)));
        assert!(all_exact(&tree.root, area));
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&A].cols + rects[&C].cols + 1, 33);
    }

    /// Whether `rects` and `borders` (as `LayoutTree::compute`/`borders`
    /// return for `area`) cover every cell of `area` exactly once: no
    /// overlaps, no gaps.
    fn tiles_exactly(area: Rect, rects: &[(PaneId, Rect)], borders: &[BorderSegment]) -> bool {
        let mut grid = vec![0u32; area.cols as usize * area.rows as usize];
        let mut mark = |r: Rect| {
            for y in r.y..r.y + r.rows {
                for x in r.x..r.x + r.cols {
                    grid[y as usize * area.cols as usize + x as usize] += 1;
                }
            }
        };
        for (_, r) in rects {
            mark(*r);
        }
        for b in borders {
            match *b {
                BorderSegment::Vertical { col, row, len } => {
                    mark(Rect {
                        x: col,
                        y: row,
                        cols: 1,
                        rows: len,
                    });
                }
                BorderSegment::Horizontal { row, col, len } => {
                    mark(Rect {
                        x: col,
                        y: row,
                        cols: len,
                        rows: 1,
                    });
                }
            }
        }
        grid.iter().all(|&c| c == 1)
    }

    /// Whether every split node in `node`'s subtree, rendered at `rect`,
    /// keeps all of its own immediate children >= 1 cell along its own axis
    /// whenever its own area (after reserving one cell per border, same as
    /// `child_rects`) can fit one cell for each of its own children.
    ///
    /// A single top-level "is the area big enough" check can't characterise
    /// this for a nested tree: a split several levels down can still fall
    /// short even when the top-level area is generously larger than the
    /// total pane count, because a deeper split only ever receives whatever
    /// share its parent's (weight-proportional, not need-aware)
    /// distribution gave it. Nodes that fall short are allowed to zero a
    /// child out (see the module docs); this only checks nodes that have
    /// enough room not to.
    fn no_child_zeroed_where_it_fits(node: &Node, rect: Rect) -> bool {
        match node {
            Node::Leaf(_) => true,
            Node::Split { dir, children } => {
                let n = children.len();
                let local_total = axis_len(*dir, rect);
                let avail = local_total.saturating_sub((n as u16).saturating_sub(1));
                let fits = (avail as usize) >= n;
                let rects = child_rects(*dir, rect, children);
                let ok_here = !fits || rects.iter().all(|r| axis_len(*dir, *r) >= 1);
                ok_here
                    && children
                        .iter()
                        .zip(rects)
                        .all(|(c, r)| no_child_zeroed_where_it_fits(&c.node, r))
            }
        }
    }

    /// Whether every split in `node`'s subtree, rendered on `rect`, has
    /// weights summing to exactly its own local avail: the general,
    /// whole-tree form of the exactness invariant `distribute`'s doc
    /// comment describes, checked recursively at every level rather than
    /// just for whichever group a single op happens to touch directly.
    fn all_exact(node: &Node, rect: Rect) -> bool {
        match node {
            Node::Leaf(_) => true,
            Node::Split { dir, children } => {
                let n = children.len() as u32;
                let avail = u32::from(axis_len(*dir, rect)).saturating_sub(n.saturating_sub(1));
                let sum: u32 = children.iter().map(|c| c.weight).sum();
                if sum != avail {
                    return false;
                }
                let rects = child_rects(*dir, rect, children);
                children
                    .iter()
                    .zip(rects)
                    .all(|(c, r)| all_exact(&c.node, r))
            }
        }
    }

    /// Whether every weight in `node`'s subtree, rendered on `rect`, is at
    /// least one cell and no larger than its parent's own extent.
    fn weights_within(node: &Node, rect: Rect) -> bool {
        match node {
            Node::Leaf(_) => true,
            Node::Split { dir, children } => {
                let extent = u32::from(axis_len(*dir, rect));
                let rects = child_rects(*dir, rect, children);
                children
                    .iter()
                    .zip(rects)
                    .all(|(c, r)| c.weight >= 1 && c.weight <= extent && weights_within(&c.node, r))
            }
        }
    }

    fn op_strategy() -> impl Strategy<Value = (u8, bool, u8, u16, usize, u8, i32, u8)> {
        (
            0u8..3,
            any::<bool>(),
            0u8..3,
            0u16..80,
            any::<usize>(),
            0u8..4,
            -40i32..40,
            // Which area an area-taking op uses: the build area twice as
            // often as each of the others.
            0u8..4,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn split_and_remove_sequences_tile_exactly(
            ops in prop::collection::vec(op_strategy(), 0..24),
            build in (8u16..40, 6u16..30),
            other in (1u16..40, 1u16..30),
            small in (1u16..12, 1u16..10),
        ) {
            let areas = [
                rect(build.0, build.1),
                rect(build.0, build.1),
                rect(other.0, other.1),
                rect(small.0, small.1),
            ];
            let mut tree = LayoutTree::new(PaneId(0));
            let mut alive = vec![PaneId(0)];
            let mut next_id = 1u32;

            for (kind, horiz, size_kind, size_val, target, edge_kind, delta, area_kind) in ops {
                let area = areas[area_kind as usize];
                let kind = kind % 3;
                let fits = renders_nonzero(&tree.root, area);

                if kind != 1 {
                    // rebase leaves compute(area) and borders(area) alone,
                    // and makes the tree exact on area unless it's below
                    // the floor, in which case it changes nothing.
                    let mut probe = tree.clone();
                    let rects = probe.compute(area);
                    let borders = probe.borders(area);
                    prop_assert_eq!(probe.rebase(area), fits);
                    prop_assert_eq!(probe.compute(area), rects);
                    prop_assert_eq!(probe.borders(area), borders);
                    if fits {
                        prop_assert!(all_exact(&probe.root, area));
                    } else {
                        prop_assert_eq!(probe.exact_for, tree.exact_for);
                    }
                }

                match kind {
                    0 => {
                        let target_pane = alive[target % alive.len()];
                        let dir = if horiz { Dir::Horizontal } else { Dir::Vertical };
                        let size = match size_kind % 3 {
                            0 => SplitSize::Half,
                            1 => SplitSize::Percent(size_val as u8),
                            _ => SplitSize::Cells(size_val),
                        };
                        let new_pane = PaneId(next_id);
                        if tree.split(area, target_pane, dir, new_pane, size).is_ok() {
                            next_id += 1;
                            alive.push(new_pane);
                        }
                    }
                    1 => {
                        if alive.len() > 1 {
                            let idx = target % alive.len();
                            tree.remove(alive[idx]);
                            alive.remove(idx);
                        }
                    }
                    _ => {
                        let target_pane = alive[target % alive.len()];
                        let edges = [Edge::Left, Edge::Right, Edge::Up, Edge::Down];
                        tree.resize(area, target_pane, edges[edge_kind as usize % 4], delta);
                    }
                }

                if kind != 1 && fits {
                    prop_assert_eq!(tree.exact_for, Some((area.cols, area.rows)));
                }

                match tree.exact_for {
                    None => prop_assert!(matches!(tree.root, Node::Leaf(_))),
                    Some((cols, rows)) => {
                        // The whole tree is exact on exact_for, every weight
                        // is within its extent, every pane has at least one
                        // cell there, and it tiles exactly.
                        let exact = rect(cols, rows);
                        prop_assert!(all_exact(&tree.root, exact));
                        prop_assert!(weights_within(&tree.root, exact));
                        let rects = tree.compute(exact);
                        prop_assert!(rects.iter().all(|(_, r)| r.cols >= 1 && r.rows >= 1));
                        prop_assert!(tiles_exactly(exact, &rects, &tree.borders(exact)));
                    }
                }

                // On the op's own area (possibly below the floor), tiling is
                // still exact and a split only zeroes a child where its own
                // area can't fit them all.
                let rects = tree.compute(area);
                let borders = tree.borders(area);
                prop_assert!(tiles_exactly(area, &rects, &borders));
                prop_assert!(no_child_zeroed_where_it_fits(&tree.root, area));
            }
            prop_assert_eq!(tree.compute(areas[0]).len(), alive.len());
        }
    }

    /// The panes of a preset in index order as `left,top widthxheight`,
    /// the shape the tmux goldens are recorded in.
    fn geometry(tree: &LayoutTree, area: Rect) -> Vec<String> {
        tree.compute(area)
            .iter()
            .map(|(_, r)| format!("{},{} {}x{}", r.x, r.y, r.cols, r.rows))
            .collect()
    }

    /// A tree over `A` to `D`, built by splits that look nothing like any
    /// preset, so a preset has to throw the old shape away.
    fn four_panes(area: Rect) -> LayoutTree {
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Vertical, B, SplitSize::Half)
            .unwrap();
        tree.split(area, B, Dir::Horizontal, C, SplitSize::Cells(7))
            .unwrap();
        tree.split(area, A, Dir::Vertical, D, SplitSize::Percent(30))
            .unwrap();
        tree
    }

    const MAIN_80: Preset = Preset::MainVertical {
        main: MainSize::Cells(80),
    };

    /// tmux 3.7b goldens, four panes, recorded on a private socket with
    /// `status off` (the T3.7 brief's table).
    #[test]
    fn presets_match_the_tmux_goldens() {
        let cases: [((u16, u16), Preset, [&str; 4]); 6] = [
            (
                (200, 50),
                MAIN_80,
                ["0,0 80x50", "81,0 119x16", "81,17 119x16", "81,34 119x16"],
            ),
            (
                (200, 50),
                Preset::EvenHorizontal,
                ["0,0 50x50", "51,0 49x50", "101,0 49x50", "151,0 49x50"],
            ),
            (
                (200, 50),
                Preset::EvenVertical,
                ["0,0 200x12", "0,13 200x12", "0,26 200x12", "0,39 200x11"],
            ),
            (
                (80, 24),
                MAIN_80,
                ["0,0 78x24", "79,0 1x8", "79,9 1x7", "79,17 1x7"],
            ),
            (
                (80, 24),
                Preset::EvenHorizontal,
                ["0,0 20x24", "21,0 19x24", "41,0 19x24", "61,0 19x24"],
            ),
            (
                (80, 24),
                Preset::EvenVertical,
                ["0,0 80x6", "0,7 80x5", "0,13 80x5", "0,19 80x5"],
            ),
        ];
        for ((cols, rows), preset, want) in cases {
            let area = rect(cols, rows);
            let mut tree = four_panes(area);
            let panes: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
            assert!(
                tree.preset(area, preset, &panes),
                "{preset:?} {cols}x{rows}"
            );
            assert_eq!(geometry(&tree, area), want, "{preset:?} {cols}x{rows}");
            // Index order is kept: pane i of the preset is pane i before.
            let after: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
            assert_eq!(after, panes);
            assert!(all_exact(&tree.root, area));
            assert_eq!(tree.exact_for, Some((cols, rows)));
            assert!(tiles_exactly(
                area,
                &tree.compute(area),
                &tree.borders(area)
            ));
        }
    }

    /// `main-pane-width 50%` in tmux 3.7b: half of the 199 columns left
    /// after the border, rounded down. TC.2b wires the option; the
    /// parameter is here now.
    #[test]
    fn main_vertical_takes_a_percent_width() {
        let area = rect(200, 50);
        let mut tree = four_panes(area);
        let main = MainSize::Percent(50);
        assert!(tree.preset(area, Preset::MainVertical { main }, &[A, B, C, D]));
        assert_eq!(
            geometry(&tree, area),
            [
                "0,0 99x50",
                "100,0 100x16",
                "100,17 100x16",
                "100,34 100x16"
            ]
        );
    }

    #[test]
    fn main_vertical_over_two_panes_puts_one_pane_on_the_right() {
        // tmux 3.7b: 80x24, two panes, 78x24 and 1x24.
        let area = rect(80, 24);
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Vertical, B, SplitSize::Half)
            .unwrap();
        assert!(tree.preset(area, MAIN_80, &[A, B]));
        assert_eq!(geometry(&tree, area), ["0,0 78x24", "79,0 1x24"]);
        assert!(all_exact(&tree.root, area));
    }

    /// The acceptance shape: one full-height pane on the left, the rest
    /// stacked in one column on the right.
    #[test]
    fn main_vertical_gives_one_full_height_left_pane_and_a_stacked_column() {
        let area = rect(120, 40);
        let mut tree = four_panes(area);
        assert!(tree.preset(area, MAIN_80, &[A, B, C, D]));
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!((rects[&A].x, rects[&A].y, rects[&A].rows), (0, 0, 40));
        for pane in [B, C, D] {
            assert_eq!(rects[&pane].x, rects[&A].cols + 1);
            assert_eq!(rects[&pane].cols, 120 - rects[&A].cols - 1);
        }
        assert!(rects[&B].y < rects[&C].y && rects[&C].y < rects[&D].y);
    }

    #[test]
    fn even_presets_give_sizes_within_one_cell_extra_cells_first() {
        for n in 2..=9u32 {
            let panes: Vec<PaneId> = (1..=n).map(PaneId).collect();
            for (cols, rows) in [(80, 24), (200, 50), (37, 29), (17, 17)] {
                let area = rect(cols, rows);
                for preset in [Preset::EvenHorizontal, Preset::EvenVertical] {
                    let mut tree = LayoutTree::new(panes[0]);
                    for (i, p) in panes.iter().enumerate().skip(1) {
                        tree.split(area, panes[i - 1], Dir::Vertical, *p, SplitSize::Half)
                            .ok();
                    }
                    let alive: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
                    if !tree.preset(area, preset, &alive) {
                        continue;
                    }
                    let sizes: Vec<u16> = tree
                        .compute(area)
                        .iter()
                        .map(|(_, r)| match preset {
                            Preset::EvenHorizontal => r.cols,
                            _ => r.rows,
                        })
                        .collect();
                    let max = *sizes.iter().max().unwrap();
                    let min = *sizes.iter().min().unwrap();
                    assert!(max - min <= 1, "{preset:?} {cols}x{rows}: {sizes:?}");
                    // The bigger cells come first.
                    assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "{sizes:?}");
                }
            }
        }
    }

    #[test]
    fn preset_over_one_pane_is_a_no_op() {
        let area = rect(80, 24);
        let mut tree = LayoutTree::new(A);
        assert!(!tree.preset(area, Preset::EvenHorizontal, &[A]));
        assert_eq!(tree.compute(area), vec![(A, area)]);
        assert_eq!(tree.exact_for, None);
    }

    /// Five columns need nine cells; tmux 3.7b grows the layout past an
    /// 8-column window, which a tree exact on its area cannot hold, so the
    /// old layout stays.
    #[test]
    fn preset_too_big_for_the_area_changes_nothing() {
        let area = rect(8, 10);
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Vertical, B, SplitSize::Half)
            .unwrap();
        tree.split(area, A, Dir::Vertical, C, SplitSize::Half)
            .unwrap();
        tree.split(area, B, Dir::Vertical, D, SplitSize::Half)
            .unwrap();
        tree.split(area, D, Dir::Horizontal, E, SplitSize::Half)
            .unwrap();
        let before = tree.compute(area);
        let panes: Vec<PaneId> = before.iter().map(|(p, _)| *p).collect();
        assert!(!tree.preset(area, Preset::EvenHorizontal, &panes));
        assert_eq!(tree.compute(area), before);
        // Main-vertical needs three columns and 2n - 3 rows on the right.
        assert!(!tree.preset(rect(2, 10), MAIN_80, &panes));
        assert!(!tree.preset(rect(80, 6), MAIN_80, &panes));
        assert!(tree.preset(rect(80, 7), MAIN_80, &panes));
    }

    #[test]
    fn preset_names_parse_as_tmux_spells_them() {
        let (w, h) = (MainSize::Percent(50), MainSize::Cells(10));
        let want = [
            Preset::EvenHorizontal,
            Preset::EvenVertical,
            Preset::MainHorizontal { main: h },
            Preset::MainHorizontalMirrored { main: h },
            Preset::MainVertical { main: w },
            Preset::MainVerticalMirrored { main: w },
            Preset::Tiled { max_columns: 0 },
        ];
        for (i, (name, preset)) in Preset::NAMES.iter().zip(want).enumerate() {
            assert_eq!(Preset::from_name(name, w, h, 0), Some(preset), "{name}");
            assert_eq!(preset.index(), i, "{name}");
        }
        assert_eq!(Preset::from_name("nope", w, h, 0), None);
    }

    #[test]
    fn a_split_after_a_preset_tiles_exactly() {
        let area = rect(200, 50);
        let mut tree = four_panes(area);
        assert!(tree.preset(area, MAIN_80, &[A, B, C, D]));
        tree.split(area, C, Dir::Horizontal, E, SplitSize::Half)
            .unwrap();
        tree.split(area, A, Dir::Vertical, PaneId(6), SplitSize::Half)
            .unwrap();
        assert!(all_exact(&tree.root, area));
        assert!(tiles_exactly(
            area,
            &tree.compute(area),
            &tree.borders(area)
        ));
        assert_eq!(tree.compute(area).len(), 6);
    }

    fn preset_strategy() -> impl Strategy<Value = Preset> {
        prop_oneof![
            Just(Preset::EvenHorizontal),
            Just(Preset::EvenVertical),
            Just(Preset::Tiled { max_columns: 0 }),
            (0u16..60).prop_map(|c| Preset::MainHorizontal {
                main: MainSize::Cells(c)
            }),
            (0u8..=100).prop_map(|p| Preset::MainHorizontalMirrored {
                main: MainSize::Percent(p)
            }),
            (0u16..120).prop_map(|c| Preset::MainVerticalMirrored {
                main: MainSize::Cells(c)
            }),
            (0u16..120).prop_map(|c| Preset::MainVertical {
                main: MainSize::Cells(c)
            }),
            (0u8..=100).prop_map(|p| Preset::MainVertical {
                main: MainSize::Percent(p)
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// T1.3's invariant survives a preset: random splits and removes,
        /// a random preset on a random area, then more splits.
        #[test]
        fn presets_keep_the_tree_tiling_exactly(
            before in prop::collection::vec((any::<bool>(), any::<bool>(), any::<usize>(), 0u8..3), 0..16),
            after in prop::collection::vec((any::<bool>(), any::<usize>(), 0u8..3), 0..8),
            build in (8u16..60, 6u16..40),
            area in (1u16..60, 1u16..40),
            preset in preset_strategy(),
        ) {
            let build = rect(build.0, build.1);
            let area = rect(area.0, area.1);
            let size = |k: u8| match k {
                0 => SplitSize::Half,
                1 => SplitSize::Percent(30),
                _ => SplitSize::Cells(3),
            };
            let mut tree = LayoutTree::new(PaneId(0));
            let mut alive = vec![PaneId(0)];
            let mut next_id = 1u32;
            for (split, horiz, target, k) in before {
                if split || alive.len() == 1 {
                    let dir = if horiz { Dir::Horizontal } else { Dir::Vertical };
                    let t = alive[target % alive.len()];
                    if tree.split(build, t, dir, PaneId(next_id), size(k)).is_ok() {
                        alive.push(PaneId(next_id));
                        next_id += 1;
                    }
                } else {
                    let idx = target % alive.len();
                    tree.remove(alive[idx]);
                    alive.remove(idx);
                }
            }

            let order: Vec<PaneId> = tree.compute(build).iter().map(|(p, _)| *p).collect();
            let old = tree.clone();
            if tree.preset(area, preset, &order) {
                prop_assert_eq!(tree.exact_for, Some((area.cols, area.rows)));
                let after: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
                prop_assert_eq!(after, order);
            } else {
                prop_assert_eq!(tree.compute(area), old.compute(area));
                prop_assert_eq!(tree.exact_for, old.exact_for);
            }

            for (horiz, target, k) in after {
                let dir = if horiz { Dir::Horizontal } else { Dir::Vertical };
                let t = alive[target % alive.len()];
                if tree.split(area, t, dir, PaneId(next_id), size(k)).is_ok() {
                    alive.push(PaneId(next_id));
                    next_id += 1;
                }
            }

            if let Some((cols, rows)) = tree.exact_for {
                let exact = rect(cols, rows);
                prop_assert!(all_exact(&tree.root, exact));
                prop_assert!(weights_within(&tree.root, exact));
                let rects = tree.compute(exact);
                prop_assert!(rects.iter().all(|(_, r)| r.cols >= 1 && r.rows >= 1));
                prop_assert!(tiles_exactly(exact, &rects, &tree.borders(exact)));
            }
            prop_assert!(tiles_exactly(area, &tree.compute(area), &tree.borders(area)));
            prop_assert_eq!(tree.compute(area).len(), alive.len());
        }
    }

    // ---- TC.8: structure operations. Goldens recorded on tmux 3.7b
    // (`-L grofe-tc8 -f /dev/null`, `status off`), as `left,top WxH` in
    // index order. ----

    fn panes(n: u32) -> Vec<PaneId> {
        (1..=n).map(PaneId).collect()
    }

    /// A tree over `panes`, of any shape: every preset throws it away.
    fn stacked(area: Rect, panes: &[PaneId]) -> LayoutTree {
        let mut tree = LayoutTree::new(panes[0]);
        for (i, p) in panes.iter().enumerate().skip(1) {
            let dir = if i % 2 == 0 {
                Dir::Vertical
            } else {
                Dir::Horizontal
            };
            tree.split(area, panes[i - 1], dir, *p, SplitSize::Half)
                .unwrap();
        }
        tree
    }

    fn rows(golden: &str) -> Vec<String> {
        golden
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_other_presets_match_the_tmux_goldens() {
        let mh = Preset::MainHorizontal {
            main: MainSize::HEIGHT,
        };
        let mhm = Preset::MainHorizontalMirrored {
            main: MainSize::HEIGHT,
        };
        let mvm = Preset::MainVerticalMirrored {
            main: MainSize::WIDTH,
        };
        let t = Preset::Tiled { max_columns: 0 };
        let cases: &[((u16, u16), u32, Preset, &str)] = &[
            ((200, 50), 2, mh, "0,0 200x24;0,25 200x25"),
            ((200, 50), 2, mhm, "0,26 200x24;0,0 200x25"),
            ((200, 50), 2, mvm, "120,0 80x50;0,0 119x50"),
            ((200, 50), 2, t, "0,0 200x24;0,25 200x25"),
            ((200, 50), 3, mh, "0,0 200x24;0,25 100x25;101,25 99x25"),
            ((200, 50), 3, mvm, "120,0 80x50;0,0 119x25;0,26 119x24"),
            ((200, 50), 3, t, "0,0 99x24;100,0 100x24;0,25 200x25"),
            (
                (200, 50),
                4,
                mhm,
                "0,26 200x24;0,0 66x25;67,0 66x25;134,0 66x25",
            ),
            (
                (200, 50),
                4,
                t,
                "0,0 99x24;100,0 100x24;0,25 99x25;100,25 100x25",
            ),
            (
                (200, 50),
                5,
                mvm,
                "120,0 80x50;0,0 119x12;0,13 119x12;0,26 119x12;0,39 119x11",
            ),
            (
                (200, 50),
                5,
                t,
                "0,0 99x16;100,0 100x16;0,17 99x16;100,17 100x16;0,34 200x16",
            ),
            (
                (200, 50),
                7,
                mh,
                "0,0 200x24;0,25 33x25;34,25 33x25;68,25 33x25;102,25 32x25;135,25 32x25;168,25 32x25",
            ),
            (
                (200, 50),
                8,
                t,
                "0,0 66x16;67,0 66x16;134,0 66x16;0,17 66x16;67,17 66x16;134,17 66x16;0,34 66x16;67,34 133x16",
            ),
            ((80, 24), 2, mh, "0,0 80x22;0,23 80x1"),
            ((80, 24), 2, mhm, "0,2 80x22;0,0 80x1"),
            ((80, 24), 2, mvm, "2,0 78x24;0,0 1x24"),
            ((80, 24), 2, t, "0,0 80x11;0,12 80x12"),
            ((80, 24), 4, mh, "0,0 80x22;0,23 26x1;27,23 26x1;54,23 26x1"),
            ((80, 24), 4, mvm, "2,0 78x24;0,0 1x8;0,9 1x7;0,17 1x7"),
            (
                (80, 24),
                5,
                t,
                "0,0 39x7;40,0 40x7;0,8 39x7;40,8 40x7;0,16 80x8",
            ),
            (
                (80, 24),
                8,
                t,
                "0,0 26x7;27,0 26x7;54,0 26x7;0,8 26x7;27,8 26x7;54,8 26x7;0,16 26x8;27,16 53x8",
            ),
            ((37, 29), 2, mh, "0,0 37x24;0,25 37x4"),
            ((37, 29), 4, mhm, "0,5 37x24;0,0 12x4;13,0 12x4;26,0 11x4"),
            ((37, 29), 3, mvm, "2,0 35x29;0,0 1x14;0,15 1x14"),
            (
                (37, 29),
                7,
                t,
                "0,0 11x9;12,0 11x9;24,0 13x9;0,10 11x9;12,10 11x9;24,10 13x9;0,20 37x9",
            ),
        ];
        for &((cols, rows_), n, preset, want) in cases {
            let area = rect(cols, rows_);
            let order = panes(n);
            let mut tree = stacked(area, &order);
            assert!(
                tree.preset(area, preset, &order),
                "{preset:?} {cols}x{rows_} n={n}"
            );
            assert_eq!(
                geometry(&tree, area),
                rows(want),
                "{preset:?} {cols}x{rows_} n={n}"
            );
            // The main pane stays first in index order, mirrored or not.
            let after: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
            assert_eq!(after, order);
            assert!(all_exact(&tree.root, area));
            assert!(tiles_exactly(
                area,
                &tree.compute(area),
                &tree.borders(area)
            ));
        }
    }

    /// `main-pane-width` past the columns left is tmux's default of 80
    /// instead, then clamped as any width is (tmux 3.7b, three panes).
    #[test]
    fn main_pane_width_past_the_window_falls_back_to_80() {
        let cases = [
            (100, MainSize::Cells(150), 80),
            (60, MainSize::Cells(150), 58),
            (100, MainSize::Cells(99), 98),
            (200, MainSize::Cells(98), 98),
            (200, MainSize::Cells(150), 150),
            (100, MainSize::Percent(100), 98),
            (100, MainSize::Percent(50), 49),
        ];
        for (cols, main, want) in cases {
            let area = rect(cols, 30);
            let order = panes(3);
            let mut tree = stacked(area, &order);
            assert!(tree.preset(area, Preset::MainVertical { main }, &order));
            assert_eq!(tree.compute(area)[0].1.cols, want, "{cols} {main:?}");
        }
        assert_eq!(MainSize::parse("50%"), Some(MainSize::Percent(50)));
        assert_eq!(MainSize::parse("80"), Some(MainSize::Cells(80)));
        for bad in ["abc", "101%", "-1", ""] {
            assert_eq!(MainSize::parse(bad), None, "{bad}");
        }
    }

    /// Each preset that needs more room than the area has changes nothing
    /// (D18, deliberate for M3.6), but still counts as the last preset.
    #[test]
    fn a_preset_that_does_not_fit_changes_nothing_but_is_remembered() {
        let area = rect(8, 10);
        let order = panes(5);
        let mut tree = stacked(area, &order);
        let before = tree.compute(area);
        for (i, name) in Preset::NAMES.iter().enumerate() {
            let preset = Preset::from_name(name, MainSize::WIDTH, MainSize::HEIGHT, 0).unwrap();
            let mut t = tree.clone();
            let applied = t.preset(rect(3, 3), preset, &order);
            assert!(!applied, "{name}");
            assert_eq!(t.compute(area), before, "{name}");
            assert_eq!(t.last_preset(), Some(i), "{name}");
        }
        assert_eq!(tree.last_preset(), None);
        assert!(tree.preset(area, Preset::EvenVertical, &order));
    }

    /// The tmux 3.7b base for the join goldens: `A | B / C` on 200x50.
    fn a_beside_bc() -> (LayoutTree, Rect) {
        let area = rect(200, 50);
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.split(area, B, Dir::Vertical, C, SplitSize::Half)
            .unwrap();
        assert_eq!(
            geometry(&tree, area),
            ["0,0 100x50", "101,0 99x25", "101,26 99x24"]
        );
        (tree, area)
    }

    fn ins(dir: Dir, size: SplitSize, before: bool, full: bool) -> Insert {
        Insert {
            dir,
            size,
            before,
            full,
        }
    }

    #[test]
    fn insert_matches_the_tmux_join_goldens() {
        let h = Dir::Horizontal;
        let cases = [
            // join-pane -d -t A (below A).
            (
                A,
                ins(Dir::Vertical, SplitSize::Half, false, false),
                "0,0 100x25;0,26 100x24;101,0 99x25;101,26 99x24",
                [A, E, B, C],
            ),
            // join-pane -h -t B.
            (
                B,
                ins(h, SplitSize::Half, false, false),
                "0,0 100x50;101,0 49x25;151,0 49x25;101,26 99x24",
                [A, B, E, C],
            ),
            // join-pane -b -h -l 30 -t B: left of B, yet after it in
            // index order.
            (
                B,
                ins(h, SplitSize::Cells(30), true, false),
                "0,0 100x50;132,0 68x25;101,0 30x25;101,26 99x24",
                [A, B, E, C],
            ),
            // join-pane -f -h -t B: the whole window's right.
            (
                B,
                ins(h, SplitSize::Half, false, true),
                "0,0 50x50;51,0 49x25;101,0 99x50;51,26 49x24",
                [A, B, E, C],
            ),
            // join-pane -b -f -h -l 40 -t B: the whole window's left.
            (
                B,
                ins(h, SplitSize::Cells(40), true, true),
                "41,0 79x50;121,0 79x25;0,0 40x50;121,26 79x24",
                [A, B, E, C],
            ),
        ];
        for (target, how, want, order) in cases {
            let (mut tree, area) = a_beside_bc();
            tree.insert(area, target, E, how).unwrap();
            assert_eq!(geometry(&tree, area), rows(want), "{how:?}");
            let got: Vec<PaneId> = tree.compute(area).iter().map(|(p, _)| *p).collect();
            assert_eq!(got, order, "{how:?}");
            assert!(all_exact(&tree.root, area), "{how:?}");
            assert!(tiles_exactly(
                area,
                &tree.compute(area),
                &tree.borders(area)
            ));
        }
    }

    /// tmux's `split-window -b -v` half split: the new (upper) pane gets
    /// the odd row.
    #[test]
    fn a_half_split_before_gives_the_new_pane_the_odd_cell() {
        let (mut tree, area) = a_beside_bc();
        tree.remove(C);
        tree.insert(area, B, C, ins(Dir::Vertical, SplitSize::Half, true, false))
            .unwrap();
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!((rects[&C].y, rects[&C].rows), (0, 25));
        assert_eq!((rects[&B].y, rects[&B].rows), (26, 24));
    }

    #[test]
    fn insert_full_needs_room_for_the_whole_layout() {
        let area = rect(9, 5);
        let order = panes(4);
        let mut tree = LayoutTree::new(order[0]);
        for p in &order[1..] {
            tree.split(area, order[0], Dir::Horizontal, *p, SplitSize::Cells(1))
                .ok();
        }
        let before = tree.compute(area);
        let how = ins(Dir::Horizontal, SplitSize::Cells(3), false, true);
        assert_eq!(
            tree.insert(area, order[0], E, how),
            Err(LayoutError::TooSmall(order[0]))
        );
        assert_eq!(tree.compute(area), before);
        assert_eq!(
            tree.insert(area, PaneId(99), E, how),
            Err(LayoutError::NotFound(PaneId(99)))
        );
    }

    /// `rotate-window` on tmux 3.7b: `-D` moves the last pane first, each
    /// pane taking the next position's cell; the default `-U` the other way.
    #[test]
    fn rotate_moves_every_pane_one_position() {
        let (mut tree, area) = a_beside_bc();
        let ids =
            |t: &LayoutTree| -> Vec<PaneId> { t.compute(area).iter().map(|(p, _)| *p).collect() };
        let cells = geometry(&tree, area);
        tree.rotate(true);
        assert_eq!(ids(&tree), [C, A, B]);
        assert_eq!(geometry(&tree, area), cells);
        tree.rotate(false);
        tree.rotate(false);
        assert_eq!(ids(&tree), [B, C, A]);
        assert_eq!(geometry(&tree, area), cells);
    }

    #[test]
    fn swap_and_replace_exchange_cells_and_positions() {
        let (mut tree, area) = a_beside_bc();
        let ids =
            |t: &LayoutTree| -> Vec<PaneId> { t.compute(area).iter().map(|(p, _)| *p).collect() };
        let cells = geometry(&tree, area);
        assert!(tree.swap(A, C));
        assert_eq!(ids(&tree), [C, B, A]);
        assert_eq!(geometry(&tree, area), cells);
        assert!(!tree.swap(A, D));
        assert!(tree.replace(B, D));
        assert_eq!(ids(&tree), [C, D, A]);
        assert!(!tree.replace(B, E), "B is gone");
        assert!(!tree.replace(C, A), "A is already in the tree");
        assert!(tree.contains(D) && !tree.contains(B));
    }

    /// `resize-pane -x`/`-y` goldens on `A | B / C` (tmux 3.7b).
    #[test]
    fn resize_to_matches_the_tmux_goldens() {
        type Step = (PaneId, Dir, u16);
        let (h, v) = (Dir::Horizontal, Dir::Vertical);
        let cases: [(&[Step], &str); 8] = [
            (&[(A, h, 60)], "0,0 60x50;61,0 139x25;61,26 139x24"),
            (&[(B, h, 60)], "0,0 139x50;140,0 60x25;140,26 60x24"),
            (&[(B, v, 10)], "0,0 100x50;101,0 99x10;101,11 99x39"),
            (&[(C, v, 10)], "0,0 100x50;101,0 99x39;101,40 99x10"),
            (
                &[(C, h, 30), (C, v, 40)],
                "0,0 169x50;170,0 30x9;170,10 30x40",
            ),
            // No split across A: nothing to do.
            (&[(A, v, 10)], "0,0 100x50;101,0 99x25;101,26 99x24"),
            (&[(A, h, 199)], "0,0 198x50;199,0 1x25;199,26 1x24"),
            (&[(A, h, 0)], "0,0 1x50;2,0 198x25;2,26 198x24"),
        ];
        for (ops, want) in cases {
            let (mut tree, area) = a_beside_bc();
            for &(pane, dir, cells) in ops {
                tree.resize_to(area, pane, dir, cells);
            }
            assert_eq!(geometry(&tree, area), rows(want), "{ops:?}");
            assert!(all_exact(&tree.root, area));
        }
    }

    /// Growing takes from later cells past one with no room left, as
    /// tmux's `layout_resize_pane_grow` does.
    #[test]
    fn resize_to_takes_room_from_more_than_one_cell() {
        let area = rect(200, 10);
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(66))
            .unwrap();
        tree.split(area, B, Dir::Horizontal, C, SplitSize::Cells(33))
            .unwrap();
        assert_eq!(
            geometry(&tree, area),
            ["0,0 133x10", "134,0 32x10", "167,0 33x10"]
        );
        let steps = [
            (A, 190, ["0,0 190x10", "191,0 1x10", "193,0 7x10"]),
            // The last cell moves the border before it; the cell there
            // has no room, so it comes from the first.
            (C, 50, ["0,0 147x10", "148,0 1x10", "150,0 50x10"]),
            (B, 50, ["0,0 147x10", "148,0 50x10", "199,0 1x10"]),
        ];
        for (pane, cells, want) in steps {
            tree.resize_to(area, pane, Dir::Horizontal, cells);
            assert_eq!(geometry(&tree, area), want, "{pane} {cells}");
            assert!(all_exact(&tree.root, area));
        }
    }

    /// `select-layout -E` goldens (tmux 3.7b).
    #[test]
    fn spread_matches_the_tmux_goldens() {
        let area = rect(200, 50);
        let h = Dir::Horizontal;
        let v = Dir::Vertical;
        // Three columns 148, 30, 20.
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, h, B, SplitSize::Cells(20)).unwrap();
        tree.split(area, A, h, C, SplitSize::Cells(30)).unwrap();
        assert!(tree.spread(area, A));
        assert_eq!(
            geometry(&tree, area),
            ["0,0 66x50", "67,0 66x50", "134,0 66x50"]
        );
        // Already even: nothing above to spread.
        assert!(!tree.spread(area, A));

        // A | (B / D / C), the right column uneven: -E on B spreads it.
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, h, B, SplitSize::Cells(20)).unwrap();
        tree.split(area, B, v, C, SplitSize::Cells(10)).unwrap();
        tree.split(area, B, v, D, SplitSize::Cells(5)).unwrap();
        assert_eq!(
            geometry(&tree, area),
            ["0,0 179x50", "180,0 20x33", "180,34 20x5", "180,40 20x10"]
        );
        let mut on_b = tree.clone();
        assert!(on_b.spread(area, B));
        assert_eq!(
            geometry(&on_b, area),
            ["0,0 179x50", "180,0 20x16", "180,17 20x16", "180,34 20x16"]
        );
        // On A, whose parent is the root: the columns even out.
        assert!(tree.spread(area, A));
        assert_eq!(
            geometry(&tree, area),
            ["0,0 100x50", "101,0 99x33", "101,34 99x5", "101,40 99x10"]
        );
        // An even column goes on to its parent.
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, h, B, SplitSize::Cells(150)).unwrap();
        tree.split(area, B, v, C, SplitSize::Cells(24)).unwrap();
        assert!(tree.spread(area, C));
        assert_eq!(
            geometry(&tree, area),
            ["0,0 100x50", "101,0 99x25", "101,26 99x24"]
        );

        // Four columns: the extra cell goes first.
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, h, B, SplitSize::Cells(150)).unwrap();
        tree.split(area, B, h, C, SplitSize::Cells(37)).unwrap();
        tree.split(area, C, h, D, SplitSize::Cells(12)).unwrap();
        assert!(tree.spread(area, A));
        assert_eq!(
            geometry(&tree, area),
            ["0,0 50x50", "51,0 49x50", "101,0 49x50", "151,0 49x50"]
        );
        assert!(!LayoutTree::new(A).spread(area, A));
    }

    #[test]
    fn export_then_import_reproduces_the_layout() {
        let (mut tree, area) = a_beside_bc();
        tree.split(area, A, Dir::Vertical, D, SplitSize::Cells(7))
            .unwrap();
        let before = tree.compute(area);
        let borders = tree.borders(area);
        let exported = tree.export(area);
        assert_eq!(
            (exported.x, exported.y, exported.cols, exported.rows),
            (0, 0, 200, 50)
        );

        tree.resize(area, A, Edge::Right, 30);
        tree.resize_to(area, D, Dir::Vertical, 3);
        assert!(tree.preset(area, Preset::Tiled { max_columns: 0 }, &[A, D, B, C]));
        assert_ne!(tree.compute(area), before);

        tree.import(&exported).unwrap();
        assert_eq!(tree.compute(area), before);
        assert_eq!(tree.borders(area), borders);
        assert_eq!(tree.export(area), exported);
        assert!(all_exact(&tree.root, area));
    }

    /// Geometry from a user's string is bounded before it is walked: a
    /// chain nested far too deep, or a split with more cells than the
    /// tree has panes, is refused without recursing through it.
    #[test]
    fn import_bounds_depth_and_size_first() {
        let (mut tree, area) = a_beside_bc();
        let before = tree.compute(area);
        let pane = |p| LayoutGeometry {
            x: 0,
            y: 0,
            cols: 10,
            rows: 10,
            cell: GeometryCell::Pane(p),
        };
        let mut deep = pane(A);
        for _ in 0..100_000 {
            deep = LayoutGeometry {
                cell: GeometryCell::Split {
                    dir: Dir::Horizontal,
                    children: vec![deep],
                },
                ..pane(A)
            };
        }
        assert_eq!(tree.import(&deep), Err(LayoutError::Invalid));
        let wide = LayoutGeometry {
            cell: GeometryCell::Split {
                dir: Dir::Horizontal,
                children: vec![pane(A); 1_000_000],
            },
            ..pane(A)
        };
        assert_eq!(tree.import(&wide), Err(LayoutError::Invalid));
        assert_eq!(tree.compute(area), before);
        // Dropping the deep chain recurses too; unwind it by hand.
        let mut node = Some(deep);
        while let Some(LayoutGeometry {
            cell: GeometryCell::Split { mut children, .. },
            ..
        }) = node
        {
            node = children.pop();
        }
    }

    #[test]
    fn import_refuses_geometry_that_does_not_tile() {
        let (mut tree, area) = a_beside_bc();
        let good = tree.export(area);
        let before = tree.compute(area);
        let GeometryCell::Split { children, .. } = &good.cell else {
            panic!("a split");
        };
        let mut bad = Vec::new();
        // A gap before the second column.
        let mut g = good.clone();
        if let GeometryCell::Split { children, .. } = &mut g.cell {
            children[1].x += 1;
        }
        bad.push(g);
        // A column of the wrong height.
        let mut g = good.clone();
        if let GeometryCell::Split { children, .. } = &mut g.cell {
            children[0].rows = 49;
        }
        bad.push(g);
        // A pane the tree does not hold.
        let mut g = good.clone();
        if let GeometryCell::Split { children, .. } = &mut g.cell {
            children[0].cell = GeometryCell::Pane(E);
        }
        bad.push(g);
        // A split of one.
        let mut g = good.clone();
        g.cell = GeometryCell::Split {
            dir: Dir::Horizontal,
            children: vec![children[0].clone()],
        };
        bad.push(g);
        // Not at the origin.
        let mut g = good.clone();
        g.x = 1;
        bad.push(g);
        for g in bad {
            assert_eq!(tree.import(&g), Err(LayoutError::Invalid), "{g:?}");
            assert_eq!(tree.compute(area), before);
        }
    }
    fn structure_op_strategy() -> impl Strategy<Value = (u8, [bool; 3], usize, usize, u16, u8)> {
        (
            0u8..10,
            any::<[bool; 3]>(),
            any::<usize>(),
            any::<usize>(),
            0u16..60,
            // Which area: the build area twice as often as each other.
            0u8..4,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// T1.3's invariant after every TC.8 operation (the acceptance
        /// clause): random inserts (before, full), removes, swaps,
        /// replaces, rotations, `resize_to`, spreads, presets and
        /// export/import round trips, on random areas.
        #[test]
        fn structure_ops_keep_the_tree_tiling_exactly(
            ops in prop::collection::vec(structure_op_strategy(), 0..32),
            build in (8u16..60, 6u16..40),
            other in (1u16..60, 1u16..40),
            small in (1u16..12, 1u16..10),
        ) {
            let areas = [
                rect(build.0, build.1),
                rect(build.0, build.1),
                rect(other.0, other.1),
                rect(small.0, small.1),
            ];
            let mut tree = LayoutTree::new(PaneId(0));
            let mut alive = vec![PaneId(0)];
            let mut next_id = 1u32;
            for (kind, flags, a, b, n, area_kind) in ops {
                let area = areas[area_kind as usize];
                let pa = alive[a % alive.len()];
                let pb = alive[b % alive.len()];
                let dir = if flags[0] { Dir::Horizontal } else { Dir::Vertical };
                match kind {
                    0 | 1 => {
                        let size = if flags[1] { SplitSize::Half } else { SplitSize::Cells(n) };
                        let how = Insert { dir, size, before: flags[1] ^ flags[2], full: flags[2] };
                        if tree.insert(area, pa, PaneId(next_id), how).is_ok() {
                            alive.push(PaneId(next_id));
                            next_id += 1;
                        }
                    }
                    2 => {
                        if alive.len() > 1 {
                            tree.remove(pa);
                            alive.retain(|p| *p != pa);
                        }
                    }
                    3 => {
                        prop_assert_eq!(tree.swap(pa, pb), true);
                    }
                    4 => {
                        prop_assert!(tree.replace(pa, PaneId(next_id)));
                        alive.retain(|p| *p != pa);
                        alive.push(PaneId(next_id));
                        next_id += 1;
                    }
                    5 => tree.rotate(flags[0]),
                    6 => tree.resize_to(area, pa, dir, n),
                    7 => {
                        tree.spread(area, pa);
                    }
                    8 => {
                        let name = Preset::NAMES[a % Preset::NAMES.len()];
                        let main = if flags[1] { MainSize::Cells(n) } else { MainSize::Percent(n as u8) };
                        let preset = Preset::from_name(name, main, main, 0).unwrap();
                        let order: Vec<PaneId> = tree.order.clone();
                        let old = tree.clone();
                        if !tree.preset(area, preset, &order) {
                            prop_assert_eq!(tree.compute(area), old.compute(area));
                        }
                    }
                    _ => {
                        let before = tree.compute(area);
                        let exported = tree.export(area);
                        let mut copy = tree.clone();
                        copy.rotate(true);
                        // Every pane gets its cell back; index order is
                        // the pane list's, which a layout leaves alone.
                        // Below the floor some cell has no room, which no
                        // layout can say.
                        let fits = renders_nonzero(&tree.root, area);
                        prop_assert_eq!(copy.import(&exported).is_ok(), fits);
                        if !fits {
                            continue;
                        }
                        let by_id = |mut r: Vec<(PaneId, Rect)>| {
                            r.sort_by_key(|(p, _)| *p);
                            r
                        };
                        prop_assert_eq!(by_id(copy.compute(area)), by_id(before));
                        prop_assert_eq!(&copy.order, &{
                            let mut o = tree.order.clone();
                            o.rotate_right(1);
                            o
                        });
                    }
                }

                // The order is always every pane, once.
                let mut order = tree.order.clone();
                order.sort();
                let mut leaves_now = Vec::new();
                leaves(&tree.root, &mut leaves_now);
                leaves_now.sort();
                prop_assert_eq!(&order, &leaves_now);
                let mut want = alive.clone();
                want.sort();
                prop_assert_eq!(&order, &want);

                match tree.exact_for {
                    None => prop_assert!(matches!(tree.root, Node::Leaf(_))),
                    Some((cols, rows)) => {
                        let exact = rect(cols, rows);
                        prop_assert!(all_exact(&tree.root, exact));
                        prop_assert!(weights_within(&tree.root, exact));
                        let rects = tree.compute(exact);
                        prop_assert!(rects.iter().all(|(_, r)| r.cols >= 1 && r.rows >= 1));
                        prop_assert!(tiles_exactly(exact, &rects, &tree.borders(exact)));
                    }
                }
                let rects = tree.compute(area);
                prop_assert!(tiles_exactly(area, &rects, &tree.borders(area)));
                prop_assert!(no_child_zeroed_where_it_fits(&tree.root, area));
            }
        }
    }

    /// The tmux 3.7b layout string after `split -h`, `split -v` on the new
    /// pane, then `split -h` on the first, at 200x50 (recorded).
    const RECORDED: &str =
        "423d,200x50,0,0{50x50,0,0,0,49x50,51,0,3,99x50,101,0[99x25,101,0,1,99x24,101,26,2]}";

    fn recorded_tree() -> LayoutTree {
        let area = rect(200, 50);
        let p = PaneId;
        let mut tree = LayoutTree::new(p(0));
        tree.split(area, p(0), Dir::Horizontal, p(1), SplitSize::Half)
            .unwrap();
        tree.split(area, p(1), Dir::Vertical, p(2), SplitSize::Half)
            .unwrap();
        tree.split(area, p(0), Dir::Horizontal, p(3), SplitSize::Half)
            .unwrap();
        tree
    }

    #[test]
    fn a_layout_writes_tmux_s_string() {
        let tree = recorded_tree();
        assert_eq!(
            tmux_layout(&tree.export(rect(200, 50)), &[], true),
            RECORDED
        );
        let one = LayoutTree::new(PaneId(7));
        let g = one.export(rect(80, 24));
        let sum = tmux_checksum("80x24,0,0,7");
        assert_eq!(tmux_layout(&g, &[], true), format!("{sum:04x},80x24,0,0,7"));
        let float = |pane, z| FloatingCell {
            pane: PaneId(pane),
            x: -2,
            y: 3,
            cols: 10,
            rows: 5,
            z,
        };
        let s = tmux_layout(&g, &[float(9, 1)], true);
        assert!(
            s.ends_with(",80x24,0,0[80x24,0,0,7,10x5,-2,3,9]<10x5,-2,3,9>"),
            "{s}"
        );
        // `fe_2/layout`'s recorded string, byte for byte.
        let lone = LayoutTree::new(PaneId(5)).export(rect(200, 50));
        let recorded = FloatingCell {
            pane: PaneId(6),
            x: 3,
            y: 4,
            cols: 20,
            rows: 5,
            z: 1,
        };
        assert_eq!(
            tmux_layout(&lone, &[recorded], true),
            "27a7,200x50,0,0[200x50,0,0,5,20x5,3,4,6]<20x5,3,4,6>"
        );
        // In the root in the order made, in `<>` topmost first (recorded).
        let tree = recorded_tree();
        let s = tmux_layout(
            &tree.export(rect(200, 50)),
            &[float(8, 1), float(9, 2)],
            true,
        );
        assert!(
            s.ends_with(",99x24,101,26,2],10x5,-2,3,8,10x5,-2,3,9}<10x5,-2,3,9,10x5,-2,3,8>"),
            "{s}"
        );
    }

    #[test]
    fn a_tmux_string_round_trips_with_the_window_s_panes() {
        let mut tree = LayoutTree::new(PaneId(5));
        let panes: Vec<PaneId> = [5, 6, 7, 8].map(PaneId).to_vec();
        for p in &panes[1..] {
            let area = rect(200, 50);
            tree.split(area, PaneId(5), Dir::Vertical, *p, SplitSize::Half)
                .unwrap();
        }
        tree.apply_tmux_layout(RECORDED, &panes).unwrap();
        // The cells take the panes in order, whatever the string numbers.
        let want = RECORDED
            .replace(",0,0,0,", ",0,0,5,")
            .replace(",51,0,3,", ",51,0,6,")
            .replace(",101,0,1,", ",101,0,7,")
            .replace(",101,26,2]", ",101,26,8]");
        let got = tmux_layout(&tree.export(rect(200, 50)), &[], true);
        assert_eq!(got[5..], want[5..]);
        assert_eq!(tree.order, panes);
    }

    #[test]
    fn tmux_s_layout_errors() {
        let mut tree = recorded_tree();
        let panes: Vec<PaneId> = (0..4).map(PaneId).collect();
        let with_sum = |body: &str| format!("{:04x},{body}", tmux_checksum(body));
        let mut err = |s: &str, panes: &[PaneId]| tree.apply_tmux_layout(s, panes).unwrap_err();
        assert_eq!(err("nope", &panes), "invalid layout");
        assert_eq!(
            err(&format!("ffff{}", &RECORDED[4..]), &panes),
            "invalid layout"
        );
        let two = with_sum("80x24,0,0{40x24,0,0,1,39x24,41,0,2}");
        assert_eq!(err(&two, &panes), "have 4 panes but need 2");
        let gap = with_sum("80x24,0,0{40x24,0,0,1,39x23,41,0,2}");
        assert_eq!(
            err(&gap, &panes[..2]),
            "size mismatch after applying layout"
        );
        let deep = "1x1,0,0[".repeat(300);
        assert!(parse_tmux_layout(&with_sum(&deep)).is_none());
        // Nothing changed.
        assert_eq!(
            tmux_layout(&tree.export(rect(200, 50)), &[], true),
            RECORDED
        );
    }

    /// Fewer panes than cells: the bottom-right cell goes, its room to its
    /// neighbour, until they match, as tmux's `layout_parse`.
    #[test]
    fn spare_cells_go_from_the_bottom_right() {
        let mut tree = LayoutTree::new(PaneId(0));
        let area = rect(200, 50);
        tree.split(area, PaneId(0), Dir::Horizontal, PaneId(1), SplitSize::Half)
            .unwrap();
        tree.apply_tmux_layout(RECORDED, &[PaneId(0), PaneId(1)])
            .unwrap();
        let got = tmux_layout(&tree.export(area), &[], true);
        assert_eq!(&got[5..], "200x50,0,0{50x50,0,0,0,149x50,51,0,1}");
    }

    /// A string of `n` one-cell panes side by side, checksummed.
    fn flat_layout(n: usize) -> String {
        let cells: Vec<String> = (0..n).map(|i| format!("1x1,{},0,{i}", 2 * i)).collect();
        let body = format!("{}x1,0,0{{{}}}", 2 * n - 1, cells.join(","));
        format!("{:04x},{body}", tmux_checksum(&body))
    }

    /// Spare cells go in O(depth) each, and a string past the cell cap is
    /// refused before any is walked, so a huge layout (from `source-file`)
    /// cannot stall the actor.
    #[test]
    fn a_big_layout_is_quick_and_a_huge_one_refused() {
        let panes = [PaneId(0), PaneId(1)];
        let mut tree = LayoutTree::new(PaneId(0));
        let area = rect(80, 24);
        tree.split(area, PaneId(0), Dir::Horizontal, PaneId(1), SplitSize::Half)
            .unwrap();
        let start = crate::thread_cpu_time();
        tree.apply_tmux_layout(&flat_layout(MAX_LAYOUT_CELLS), &panes)
            .unwrap();
        assert!(crate::thread_cpu_time() - start < std::time::Duration::from_secs(2));
        let got = tmux_layout(&tree.export(rect(8191, 1)), &[], true);
        assert_eq!(&got[5..], "8191x1,0,0{1x1,0,0,0,8189x1,2,0,1}");
        let start = crate::thread_cpu_time();
        let err = tree.apply_tmux_layout(&flat_layout(30000), &panes);
        assert_eq!(err.unwrap_err(), "invalid layout");
        assert!(crate::thread_cpu_time() - start < std::time::Duration::from_secs(2));
    }

    /// FR.1: a framed tree (pad 2: frame plus gap 1) keeps tmux's
    /// one-cell-border layout on the inner area; each content rect is
    /// that cell less the pad on its right and bottom.
    #[test]
    fn a_framed_tree_insets_every_pane() {
        let area = rect(80, 24);
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        let unframed = tree.compute(area);
        tree.set_pad(2);
        let r = |x, y, cols, rows| Rect { x, y, cols, rows };
        assert_eq!(
            tree.compute(area),
            vec![(A, r(2, 2, 37, 20)), (B, r(42, 2, 36, 20))]
        );
        assert!(tree.borders(area).is_empty());
        // Frames off again: the same rects as before, nothing rescaled.
        tree.set_pad(0);
        assert_eq!(tree.compute(area), unframed);
        assert!(!tree.borders(area).is_empty());
    }

    /// Too small to frame every pane: shared borders for that area.
    #[test]
    fn a_framed_tree_too_small_lays_out_unframed() {
        let mut tree = LayoutTree::new(A);
        tree.split(rect(20, 10), A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.set_pad(2);
        assert_eq!(tree.pad_on(rect(20, 10)), 2);
        assert_eq!(tree.pad_on(rect(6, 10)), 0);
        let small = tree.compute(rect(6, 10));
        assert_eq!(small[0].1.x, 0);
        assert_eq!(small[0].1.cols + small[1].1.cols + 1, 6);
        // A split that would leave a content rect empty is refused, and
        // the tree is as it was.
        let narrow = rect(14, 10);
        assert_eq!(tree.pad_on(narrow), 2);
        let before = tree.compute(narrow);
        let err = tree.split(narrow, B, Dir::Horizontal, C, SplitSize::Half);
        assert_eq!(err, Err(LayoutError::TooSmall(B)));
        assert_eq!(tree.compute(narrow), before);
    }

    /// A content size in cells is a content size; a resize stops where
    /// every pane keeps a content cell.
    #[test]
    fn framed_sizes_are_content_sizes() {
        let area = rect(80, 24);
        let mut tree = LayoutTree::new(A);
        tree.set_pad(2);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(10))
            .unwrap();
        assert_eq!(tree.compute(area)[1].1.cols, 10);
        tree.resize_to(area, A, Dir::Horizontal, 20);
        assert_eq!(tree.compute(area)[0].1.cols, 20);
        tree.resize(area, A, Edge::Right, 1000);
        let rects = tree.compute(area);
        assert_eq!(rects[1].1.cols, 1);
        assert_eq!(rects[1].1.x + 1 + 2, 80);
    }

    /// The framed export is the content rects, and imports back as the
    /// tree it came from, framed or not.
    #[test]
    fn a_framed_export_imports_as_the_same_layout() {
        let area = rect(80, 24);
        let mut tree = LayoutTree::new(A);
        tree.set_pad(2);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Cells(30))
            .unwrap();
        tree.split(area, B, Dir::Vertical, C, SplitSize::Half)
            .unwrap();
        let g = tree.export(area);
        assert_eq!((g.x, g.y, g.cols, g.rows), (2, 2, 76, 20));
        // As restore does: a tree of every pane, for `import` to rearrange.
        let mut back = LayoutTree::new(A);
        back.preset(rect(u16::MAX, u16::MAX), Preset::EvenHorizontal, &[A, B, C]);
        back.import(&g).unwrap();
        assert_eq!(back.exact_size(), Some((80, 24)));
        back.set_pad(2);
        assert_eq!(back.compute(area), tree.compute(area));
        let s = tmux_layout(&g, &[], true);
        let mut again = LayoutTree::new(A);
        again.set_pad(2);
        again.apply_tmux_layout(&s, &[A, B, C]).unwrap();
        assert_eq!(again.exact_size(), Some((80, 24)));
        assert_eq!(again.compute(area), tree.compute(area));
    }

    /// `select-pane -L/-R/-U/-D` finds framed neighbours as it finds
    /// unframed ones.
    #[test]
    fn find_adjacent_in_a_framed_window() {
        let area = rect(80, 24);
        let mut tree = LayoutTree::new(A);
        tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
            .unwrap();
        tree.split(area, B, Dir::Vertical, C, SplitSize::Half)
            .unwrap();
        let find = |tree: &LayoutTree, p, e| {
            find_adjacent(&tree.compute(area), area, PaneStatus::Off, p, e, |_| 0)
        };
        let unframed: Vec<_> = [A, B, C]
            .iter()
            .flat_map(|p| [Edge::Left, Edge::Right, Edge::Up, Edge::Down].map(|e| (*p, e)))
            .map(|(p, e)| find(&tree, p, e))
            .collect();
        tree.set_pad(2);
        let framed: Vec<_> = [A, B, C]
            .iter()
            .flat_map(|p| [Edge::Left, Edge::Right, Edge::Up, Edge::Down].map(|e| (*p, e)))
            .map(|(p, e)| find(&tree, p, e))
            .collect();
        assert_eq!(framed, unframed);
        assert_eq!(find(&tree, A, Edge::Right), Some(B));
        assert_eq!(find(&tree, B, Edge::Down), Some(C));
    }

    /// Every framed content rect has a cell, sits `pad` in from the
    /// window's edges, and the rects grown by `pad + 1` on their right
    /// and bottom tile the window grown by 1 less `pad`: neighbours are
    /// exactly `1 + pad` apart, as the module docs say.
    fn assert_framed_tiling(tree: &LayoutTree, area: Rect) -> Result<(), TestCaseError> {
        let pad = tree.pad_on(area);
        prop_assert!(pad > 0);
        let rects = tree.compute(area);
        let mut covered = 0u64;
        for (id, r) in &rects {
            prop_assert!(r.cols >= 1 && r.rows >= 1, "{id:?} {r:?}");
            prop_assert!(r.x >= pad && r.y >= pad, "{id:?} {r:?}");
            prop_assert!(r.x + r.cols + pad <= area.cols, "{id:?} {r:?}");
            prop_assert!(r.y + r.rows + pad <= area.rows, "{id:?} {r:?}");
            covered += u64::from(r.cols + pad + 1) * u64::from(r.rows + pad + 1);
        }
        let want = u64::from(area.cols - pad + 1) * u64::from(area.rows - pad + 1);
        prop_assert_eq!(covered, want);
        for (i, (_, a)) in rects.iter().enumerate() {
            for (_, b) in &rects[i + 1..] {
                let apart_x = a.x + a.cols + pad < b.x || b.x + b.cols + pad < a.x;
                let apart_y = a.y + a.rows + pad < b.y || b.y + b.rows + pad < a.y;
                prop_assert!(apart_x || apart_y, "{a:?} {b:?} pad {pad}");
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// FR.1: with frames on (gap 0, 1 or 8, so pad 1, 2 or 9) every
        /// op on a window that frames its panes leaves it framed, each
        /// pane a content cell, and the content rects tiled as the spec
        /// says; an export imports back as the same content rects.
        #[test]
        fn framed_ops_keep_every_pane_a_content_cell(
            ops in prop::collection::vec(structure_op_strategy(), 0..32),
            size in (10u16..90, 8u16..40),
            gap in prop::sample::select(vec![0u16, 1, 8]),
        ) {
            let area = rect(size.0, size.1);
            let pad = 1 + gap;
            let mut tree = LayoutTree::new(PaneId(0));
            tree.set_pad(pad);
            let mut alive = vec![PaneId(0)];
            let mut next_id = 1u32;
            for (kind, flags, a, b, n, _) in ops {
                if tree.pad_on(area) == 0 {
                    break;
                }
                let pa = alive[a % alive.len()];
                let pb = alive[b % alive.len()];
                let dir = if flags[0] { Dir::Horizontal } else { Dir::Vertical };
                match kind {
                    0 | 1 => {
                        let size = if flags[1] { SplitSize::Half } else { SplitSize::Cells(n) };
                        let how = Insert { dir, size, before: flags[1] ^ flags[2], full: flags[2] };
                        let before = tree.compute(area);
                        match tree.insert(area, pa, PaneId(next_id), how) {
                            Ok(()) => {
                                alive.push(PaneId(next_id));
                                next_id += 1;
                            }
                            Err(_) => prop_assert_eq!(tree.compute(area), before),
                        }
                    }
                    2 => {
                        if alive.len() > 1 {
                            tree.remove(pa);
                            alive.retain(|p| *p != pa);
                        }
                    }
                    3 => {
                        prop_assert!(tree.swap(pa, pb));
                    }
                    4 => {
                        let edge = [Edge::Left, Edge::Right, Edge::Up, Edge::Down][b % 4];
                        let delta = i32::from(n) * if flags[1] { 1 } else { -1 };
                        tree.resize(area, pa, edge, delta);
                    }
                    5 => tree.rotate(flags[0]),
                    6 => tree.resize_to(area, pa, dir, n),
                    7 => {
                        tree.spread(area, pa);
                    }
                    8 => {
                        let name = Preset::NAMES[a % Preset::NAMES.len()];
                        let main = if flags[1] { MainSize::Cells(n) } else { MainSize::Percent(n as u8) };
                        let preset = Preset::from_name(name, main, main, 0).unwrap();
                        let order: Vec<PaneId> = tree.order.clone();
                        let old = tree.compute(area);
                        if !tree.preset(area, preset, &order) {
                            prop_assert_eq!(tree.compute(area), old);
                        }
                    }
                    _ => {
                        let before = tree.compute(area);
                        let exported = tree.export(area);
                        let mut copy = tree.clone();
                        copy.rotate(true);
                        copy.import(&exported).unwrap();
                        prop_assert_eq!(copy.exact_size(), Some((area.cols, area.rows)));
                        let by_id = |mut r: Vec<(PaneId, Rect)>| {
                            r.sort_by_key(|(p, _)| *p);
                            r
                        };
                        prop_assert_eq!(by_id(copy.compute(area)), by_id(before));
                    }
                }
                assert_framed_tiling(&tree, area)?;
            }
        }
    }

    /// Four panes in nested splits, gap 0 (separator 2) and gap 8 (pad
    /// 9), on odd sizes: the content rects tmux's tree gives the inner
    /// area, each `pad` short on its right and bottom.
    #[test]
    fn nested_framed_splits_at_gap_0_and_8() {
        let r = |x, y, cols, rows| Rect { x, y, cols, rows };
        let build = |area: Rect, pad: u16| {
            let mut tree = LayoutTree::new(A);
            tree.set_pad(pad);
            tree.split(area, A, Dir::Horizontal, B, SplitSize::Half)
                .unwrap();
            tree.split(area, B, Dir::Vertical, C, SplitSize::Half)
                .unwrap();
            tree.split(area, C, Dir::Horizontal, D, SplitSize::Half)
                .unwrap();
            tree
        };
        // Gap 0: inner 80x24 at (1,1); 40 | 39, then 39 rows 12 / 11,
        // then 39 columns 19 | 19.
        let area = rect(81, 25);
        let tree = build(area, 1);
        assert_eq!(
            tree.compute(area),
            vec![
                (A, r(1, 1, 39, 23)),
                (B, r(42, 1, 38, 11)),
                (C, r(42, 14, 18, 10)),
                (D, r(62, 14, 18, 10)),
            ]
        );
        // Gap 8: inner 72x31 at (9,9).
        let area = rect(81, 40);
        let tree = build(area, 9);
        let rects = tree.compute(area);
        assert_eq!(rects[0].1, r(9, 9, 27, 22));
        assert_eq!(rects[1].1.x, 9 + 36 + 1);
        for (_, c) in &rects {
            assert!(c.x + c.cols + 9 <= 81 && c.y + c.rows + 9 <= 40, "{c:?}");
        }
        // Resized to the minimum and back out.
        let mut tree = build(area, 9);
        tree.resize_to(area, D, Dir::Horizontal, 0);
        assert_eq!(tree.compute(area)[3].1.cols, 1);
        tree.resize(area, D, Edge::Left, 1000);
        assert_eq!(tree.compute(area)[2].1.cols, 1);
        // Nothing is left to split C with.
        let err = tree.split(area, C, Dir::Horizontal, PaneId(5), SplitSize::Half);
        assert_eq!(err, Err(LayoutError::TooSmall(C)));
    }
}
