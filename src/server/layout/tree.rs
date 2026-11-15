use super::*;

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
/// Cells name panes by runtime id; the snapshot keeps the same shape
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

/// One contiguous, one-cell-wide run of border cells, as the renderer
/// draws it. `Vertical` runs come from horizontal (side-by-side) splits;
/// `Horizontal` runs come from vertical (stacked) splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderSegment {
    /// A vertical run at column `col`, spanning `len` rows starting at `row`.
    Vertical { col: u16, row: u16, len: u16 },
    /// A horizontal run at row `row`, spanning `len` cols starting at `col`.
    Horizontal { row: u16, col: u16, len: u16 },
}

#[derive(Debug, Clone)]
pub(super) struct Child {
    /// Size in cells along the parent's axis on the tree's `exact_for`
    /// area, always >= 1; see the module docs.
    pub(super) weight: u32,
    pub(super) node: Node,
}

#[derive(Debug, Clone)]
pub(super) enum Node {
    Leaf(PaneId),
    Split { dir: Dir, children: Vec<Child> },
}

/// The n-ary split tree for one window.
#[derive(Debug, Clone)]
pub struct LayoutTree {
    pub(super) root: Node,
    /// The area size (cols, rows) every weight is an exact cell count for.
    /// `None` until the first `split` or `resize`, while the tree is still a
    /// single leaf with no weights.
    pub(super) exact_for: Option<(u16, u16)>,
    /// Every pane, in index order (tmux's pane list): the order `compute`
    /// returns them in. It is the leaves' order, except where tmux's is
    /// not: a pane joined before its target, or a mirrored preset.
    pub(super) order: Vec<PaneId>,
    /// The last preset applied, as a [`Preset::index`]: where
    /// `next-layout` goes on from (tmux's `lastlayout`).
    pub(super) last_preset: Option<usize>,
    /// `1 + pane-gap` in a framed window, else 0 (module docs).
    pub(super) pad: u16,
    /// The pad `exact_for` was made under: `exact_for` is an inner area
    /// that much smaller than the window ([`Self::exact_size`]).
    pub(super) exact_pad: u16,
    /// Rows of outer gap left out at the top and the bottom: the edge the
    /// status line sits on has none ([`Self::set_trim`]).
    pub(super) trim: (u16, u16),
    /// The rows of `trim` in effect when `exact_for` was made.
    pub(super) exact_trim: u16,
}

/// Which axis a point-in-rect lookup runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Axis {
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
    pub(super) fn trim_on(&self, pad: u16) -> (u16, u16) {
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
    pub(super) fn fits(&self, inner: Rect, pad: u16) -> bool {
        let mut out = Vec::new();
        collect_rects(&self.root, inner, &mut out);
        out.iter().all(|(_, r)| r.cols > pad && r.rows > pad)
    }

    /// Runs `op` on the inner area of `area` with the pad in effect there.
    /// When frames are in effect and `op` leaves some pane without a
    /// content cell, the tree is put back as it was and the answer is
    /// `None`.
    pub(super) fn framed<T>(
        &mut self,
        area: Rect,
        op: impl FnOnce(&mut Self, Rect, u16) -> T,
    ) -> Option<T> {
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
    pub(super) fn rebase(&mut self, area: Rect) -> bool {
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
    pub(super) fn insert_inner(
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
    pub(super) fn insert_at(
        &mut self,
        area: Rect,
        path: &[usize],
        new: PaneId,
        how: Insert,
    ) -> Option<()> {
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
    pub(super) fn insert_full(&mut self, area: Rect, new: PaneId, how: Insert) -> Option<()> {
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
    pub(super) fn remove_at(&mut self, path: &[usize]) {
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
        if children.len() == 1
            && let Some(mut remaining) = children.pop()
        {
            // The survivor takes over the whole slot this split occupied,
            // growing by `gain` along `dir`. Unlike the non-collapse case,
            // `dir` here isn't necessarily this survivor's own split axis
            // (it's this split's axis, the one being discarded), so the
            // growth has to be pushed down through it explicitly.
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
    pub(super) fn resize_inner(&mut self, area: Rect, pane: PaneId, edge: Edge, delta: i32) {
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
    /// D18 (deliberate): tmux 3.7b grows the layout past a
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
    pub(super) fn preset_inner(
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

    pub(super) fn relabel(&mut self, map: &dyn Fn(PaneId) -> PaneId) {
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
    pub(super) fn resize_to_inner(&mut self, area: Rect, pane: PaneId, dir: Dir, cells: u16) {
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
    pub(super) fn spread_inner(&mut self, area: Rect, pane: PaneId) -> bool {
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
pub(super) fn inner(area: Rect, pad: u16, (top, bottom): (u16, u16)) -> Option<Rect> {
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
pub(super) fn shrink_cells(g: &mut LayoutGeometry, pad: u16) {
    g.cols = g.cols.saturating_sub(pad);
    g.rows = g.rows.saturating_sub(pad);
    if let GeometryCell::Split { children, .. } = &mut g.cell {
        children.iter_mut().for_each(|c| shrink_cells(c, pad));
    }
}

/// The pad a framed export ([`shrink_cells`]) was made with: its root's
/// column offset, its row offset that or less (a trimmed top gap); 0 for
/// a root at the corner or off those, which is no framed export.
pub(super) fn framed_pad(g: &LayoutGeometry) -> u16 {
    if g.y > 0 && g.y <= g.x { g.x } else { 0 }
}

/// A framed export back on an inner area at the origin: every cell `pad`
/// cells wider and taller, and moved `pad` left and up by the root's row
/// offset. Nothing for 0.
pub(super) fn unframe(g: &mut LayoutGeometry, pad: u16) {
    if pad > 0 {
        let dy = g.y;
        unframe_by(g, pad, dy);
    }
}

pub(super) fn unframe_by(g: &mut LayoutGeometry, pad: u16, dy: u16) {
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
pub(super) fn insert_sizes(avail: u16, how: Insert) -> Option<(u16, u16)> {
    let (old, new) = split_sizes(avail, how.size)?;
    Some(if how.before && how.size == SplitSize::Half {
        (new, old)
    } else {
        (old, new)
    })
}

/// `[old, new]`, or `[new, old]` when `new` goes before.
pub(super) fn pair(old: Child, new: Child, before: bool) -> Vec<Child> {
    if before {
        vec![new, old]
    } else {
        vec![old, new]
    }
}

pub(super) fn relabel_node(node: &mut Node, map: &dyn Fn(PaneId) -> PaneId) {
    match node {
        Node::Leaf(p) => *p = map(*p),
        Node::Split { children, .. } => {
            for child in children {
                relabel_node(&mut child.node, map);
            }
        }
    }
}

pub(super) fn leaves(node: &Node, out: &mut Vec<PaneId>) {
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
pub(super) fn resize_cells(children: &mut [Child], dir: Dir, cell: usize, change: i64) {
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
pub(super) fn resize_children(node: &mut Node, rect: Rect) -> Option<()> {
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

pub(super) fn export_node(node: &Node, rect: Rect) -> LayoutGeometry {
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
pub(super) const IMPORT_MAX_DEPTH: usize = 256;

/// How many cells a tmux layout string may hold past the window's panes
/// ([`LayoutTree::apply_tmux_layout`]). More cells than this cannot tile
/// any window a terminal shows.
pub(super) const MAX_LAYOUT_CELLS: usize = 4096;

/// Whether `g` nests no deeper than [`IMPORT_MAX_DEPTH`] and holds no more
/// than `budget` panes, found without going past either bound: every
/// split's children each hold a pane at least, so a split with more
/// children than the budget left fails before they are walked.
pub(super) fn bounded(g: &LayoutGeometry, depth: usize, budget: &mut usize) -> bool {
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
pub(super) fn import_node(g: &LayoutGeometry) -> Option<Node> {
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

/// `target_size, new_size` for splitting `avail` cells (already short one
/// cell for the new border) between the existing pane and the new one.
/// `None` if there's no room for both to keep at least one cell.
pub(super) fn split_sizes(avail: u16, size: SplitSize) -> Option<(u16, u16)> {
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
pub(super) fn distribute(total: u16, weights: &[u32]) -> Vec<u16> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    if (total as usize) < n {
        // Not enough cells for every child to get one. Simplest well-defined
        // floor: children at lower indices get one cell each until cells run
        // out, the rest get none. grofe doesn't shrink a window below one cell
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
pub(super) fn distribute_proportional(total: u16, weights: &[u32]) -> Vec<u16> {
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

pub(super) fn axis_len(dir: Dir, rect: Rect) -> u16 {
    match dir {
        Dir::Horizontal => rect.cols,
        Dir::Vertical => rect.rows,
    }
}

/// The rects of `children` laid out in `dir` across `area`.
pub(super) fn child_rects(dir: Dir, area: Rect, children: &[Child]) -> Vec<Rect> {
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
pub(super) fn renders_nonzero(node: &Node, rect: Rect) -> bool {
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
pub(super) fn store_rendered(node: &mut Node, rect: Rect) {
    if let Node::Split { dir, children } = node {
        let rects = child_rects(*dir, rect, children);
        for (child, r) in children.iter_mut().zip(rects) {
            child.weight = u32::from(axis_len(*dir, r));
            store_rendered(&mut child.node, r);
        }
    }
}

pub(super) fn find_path(node: &Node, pane: PaneId, path: &mut Vec<usize>) -> bool {
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

pub(super) fn node_at<'a>(node: &'a Node, path: &[usize]) -> &'a Node {
    match path {
        [] => node,
        [idx, rest @ ..] => match node {
            Node::Split { children, .. } => node_at(&children[*idx].node, rest),
            Node::Leaf(_) => node,
        },
    }
}

pub(super) fn node_at_mut<'a>(node: &'a mut Node, path: &[usize]) -> &'a mut Node {
    match path {
        [] => node,
        [idx, rest @ ..] => match node {
            Node::Split { children, .. } => node_at_mut(&mut children[*idx].node, rest),
            Node::Leaf(_) => node,
        },
    }
}

/// Grows `node`'s own extent along `dir` by `delta` cells ([`adjust`]).
pub(super) fn grow(node: &mut Node, dir: Dir, delta: u32) {
    adjust(node, dir, i64::from(delta));
}

/// Shrinks `node`'s own extent along `dir` by `delta` cells ([`adjust`]).
/// The caller first clamps `delta` to `max_shrink(node, own_extent, dir)`.
pub(super) fn shrink(node: &mut Node, dir: Dir, delta: u32) {
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
pub(super) fn adjust(node: &mut Node, dir: Dir, mut change: i64) {
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
pub(super) fn max_shrink(node: &Node, own_extent: u32, dir: Dir) -> u32 {
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

pub(super) fn rect_at_path(node: &Node, area: Rect, path: &[usize]) -> Rect {
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
pub(super) fn far_sibling(edge: Edge, dir: Dir, idx: usize, len: usize) -> Option<usize> {
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
pub(super) fn find_leaf_at(node: &Node, rect: Rect, axis: Axis, coord: u16) -> PaneId {
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

pub(super) fn collect_rects(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
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

pub(super) fn collect_borders(node: &Node, area: Rect, out: &mut Vec<BorderSegment>) {
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
    // is tmux's unchanged. The top pane sits `pad` rows down less any
    // trim, so the rows shift by the topmost pane's offset, and the inner
    // area is the panes' bounding box.
    let pad = panes.iter().map(|(_, r)| r.x).min().unwrap_or(0);
    let top = panes.iter().map(|(_, r)| r.y).min().unwrap_or(0);
    let unframed: Vec<(PaneId, Rect)>;
    let (panes, area, status) = if pad == 0 {
        (panes, area, status)
    } else {
        unframed = panes
            .iter()
            .map(|(id, r)| {
                let r = Rect {
                    x: r.x - pad,
                    y: r.y - top,
                    cols: r.cols.saturating_add(pad),
                    rows: r.rows.saturating_add(pad),
                };
                (*id, r)
            })
            .collect();
        let area = Rect {
            cols: unframed
                .iter()
                .map(|(_, r)| r.x + r.cols)
                .max()
                .unwrap_or(0),
            rows: unframed
                .iter()
                .map(|(_, r)| r.y + r.rows)
                .max()
                .unwrap_or(0),
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
