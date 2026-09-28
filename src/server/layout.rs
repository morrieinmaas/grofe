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

use crate::types::{Dir, Edge, PaneId, Rect, SplitSize};

/// Error returned when a split can't be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("no such pane: {0}")]
    NotFound(PaneId),
    #[error("not enough room to split pane {0}")]
    TooSmall(PaneId),
}

/// The main pane's width in `main-vertical`, as tmux's `main-pane-width`:
/// cells, or a percent of the columns left after the border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainSize {
    Cells(u16),
    Percent(u8),
}

/// A `select-layout` preset (spec §4.3), sized as tmux 3.7b sizes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// The first pane full height on the left, the rest stacked on the
    /// right.
    MainVertical { main: MainSize },
    /// Every pane side by side.
    EvenHorizontal,
    /// Every pane stacked.
    EvenVertical,
}

impl Preset {
    /// The preset tmux names `name`, if M3 has it.
    pub fn from_name(name: &str) -> Option<Preset> {
        match name {
            // ponytail: tmux's `main-pane-width` default; TC.2b reads the option.
            "main-vertical" => Some(Preset::MainVertical {
                main: MainSize::Cells(80),
            }),
            "even-horizontal" => Some(Preset::EvenHorizontal),
            "even-vertical" => Some(Preset::EvenVertical),
            _ => None,
        }
    }
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
}

/// Which axis a point-in-rect lookup runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Row,
    Col,
}

/// Which side of a node the space freed by a removed sibling entered from,
/// along the growth axis `grow` is called with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Front,
    Back,
}

impl LayoutTree {
    /// A window with a single pane filling the whole area.
    pub fn new(pane: PaneId) -> Self {
        LayoutTree {
            root: Node::Leaf(pane),
            exact_for: None,
        }
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
        let mut path = Vec::new();
        if !find_path(&self.root, target, &mut path) {
            return Err(LayoutError::NotFound(target));
        }
        if !self.rebase(area) {
            return Err(LayoutError::TooSmall(target));
        }

        if path.is_empty() {
            let raw = axis_len(dir, area);
            let avail = raw.saturating_sub(1);
            let (target_size, new_size) =
                split_sizes(avail, size).ok_or(LayoutError::TooSmall(target))?;
            self.root = Node::Split {
                dir,
                children: vec![
                    Child {
                        weight: u32::from(target_size),
                        node: Node::Leaf(target),
                    },
                    Child {
                        weight: u32::from(new_size),
                        node: Node::Leaf(new),
                    },
                ],
            };
            return Ok(());
        }

        let idx = path[path.len() - 1];
        let parent_path = &path[..path.len() - 1];
        let parent_dir = match node_at(&self.root, parent_path) {
            Node::Split { dir, .. } => *dir,
            Node::Leaf(_) => unreachable!("parent path always resolves to a split"),
        };

        if parent_dir == dir {
            // Splitting in the direction the parent already splits: add new
            // as a sibling right after target, carved out of target's slice.
            // The tree is exact on `area` now, so target's weight is its
            // real size.
            let Node::Split { children, .. } = node_at_mut(&mut self.root, parent_path) else {
                unreachable!("parent path always resolves to a split")
            };
            let current = u16::try_from(children[idx].weight).unwrap_or(u16::MAX);
            let avail = current.saturating_sub(1);
            let (target_size, new_size) =
                split_sizes(avail, size).ok_or(LayoutError::TooSmall(target))?;
            children[idx].weight = u32::from(target_size);
            children.insert(
                idx + 1,
                Child {
                    weight: u32::from(new_size),
                    node: Node::Leaf(new),
                },
            );
        } else {
            // Splitting the other way: wrap target's leaf in a new split.
            let target_rect = rect_at_path(&self.root, area, &path);
            let raw = axis_len(dir, target_rect);
            let avail = raw.saturating_sub(1);
            let (target_size, new_size) =
                split_sizes(avail, size).ok_or(LayoutError::TooSmall(target))?;
            let slot = node_at_mut(&mut self.root, &path);
            *slot = Node::Split {
                dir,
                children: vec![
                    Child {
                        weight: u32::from(target_size),
                        node: Node::Leaf(target),
                    },
                    Child {
                        weight: u32::from(new_size),
                        node: Node::Leaf(new),
                    },
                ],
            };
        }
        Ok(())
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
        let idx = path[path.len() - 1];
        let parent_path = &path[..path.len() - 1];
        let Node::Split { dir, children } = node_at_mut(&mut self.root, parent_path) else {
            unreachable!("parent path always resolves to a split")
        };
        let dir = *dir;
        let removed = children.remove(idx);
        // The removed child's own weight, plus the one border cell that's
        // now gone too, is the extent whoever inherits its space gains
        // along `dir`. It goes to the side the removed child sat on: the
        // previous child if there was one, otherwise the new first child.
        let gain = removed.weight + 1;
        let side = if idx == 0 { Side::Front } else { Side::Back };
        if children.len() == 1 {
            // The survivor takes over the whole slot this split occupied,
            // growing by `gain` along `dir`. Unlike the non-collapse case,
            // `dir` here isn't necessarily this survivor's own split axis
            // (it's this split's axis, the one being discarded), so the
            // growth has to be pushed down through it explicitly.
            let mut remaining = children.pop().unwrap();
            grow(&mut remaining.node, dir, gain, side);
            let slot = node_at_mut(&mut self.root, parent_path);
            *slot = remaining.node;
        } else {
            // Like tmux's layout_destroy_cell: give the removed child's
            // cells to its neighbour directly (its own weight, in this
            // group's list). If the neighbour is itself a nested split,
            // push the same growth down into it too (grow's doc comment),
            // since a same-axis descendant further down would otherwise
            // keep weights summing to the neighbour's OLD extent.
            let neighbour = idx.saturating_sub(1);
            children[neighbour].weight += gain;
            grow(&mut children[neighbour].node, dir, gain, side);
        }
    }

    /// Moves the border on `pane`'s `edge` side by `delta` cells (positive
    /// grows `pane` toward that edge). Clamped so neither side's subtree
    /// would drop any leaf below one cell (`max_shrink`'s doc comment); a
    /// no-op (not a panic) if `pane` has no neighbour across `edge`, the
    /// clamp reduces the requested delta to zero, or `area` is below the
    /// floor (module docs).
    pub fn resize(&mut self, area: Rect, pane: PaneId, edge: Edge, delta: i32) {
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
            // Whichever of near/far comes first in the children list faces
            // the moved border from its own back edge, the other from its
            // own front; `grow`/`shrink` need that to know which child of a
            // nested same-axis group to touch first.
            let (near_side, far_side) = if far_idx > idx {
                (Side::Back, Side::Front)
            } else {
                (Side::Front, Side::Back)
            };
            // The tree is exact on `area` now, so each weight is that
            // child's real size and `max_shrink` reads real sizes too.
            let (grower, grow_side, shrinker, shrink_side) = if delta > 0 {
                (idx, near_side, far_idx, far_side)
            } else {
                (far_idx, far_side, idx, near_side)
            };
            let cap = max_shrink(&children[shrinker].node, children[shrinker].weight, dir);
            let amt = delta.unsigned_abs().min(cap);
            if amt == 0 {
                return;
            }
            debug_assert!(amt < children[shrinker].weight);
            children[shrinker].weight = children[shrinker].weight.saturating_sub(amt);
            children[grower].weight += amt;
            grow(&mut children[grower].node, dir, amt, grow_side);
            shrink(&mut children[shrinker].node, dir, amt, shrink_side);
            return;
        }
    }

    /// Replaces the tree with `preset` over `panes` (in pane-index order,
    /// which the new tree keeps), exact on `area`. Returns false, changing
    /// nothing, for fewer than two panes (tmux does nothing either) or when
    /// `area` cannot give every pane a cell.
    ///
    /// ponytail: tmux 3.7b grows the layout past a window too small for the
    /// preset and clips it; a tree exact on its area cannot, so the old
    /// layout stays.
    pub fn preset(&mut self, area: Rect, preset: Preset, panes: &[PaneId]) -> bool {
        if panes.len() < 2 {
            return false;
        }
        let root = match preset {
            Preset::EvenHorizontal => even(Dir::Horizontal, area.cols, panes),
            Preset::EvenVertical => even(Dir::Vertical, area.rows, panes),
            Preset::MainVertical { main } => main_vertical(area, main, panes),
        };
        let Some(root) = root else {
            return false;
        };
        self.root = root;
        self.exact_for = Some((area.cols, area.rows));
        true
    }

    /// The pane adjacent to `pane` across `edge`, among panes sharing the
    /// nearest ancestor split on that axis, chosen by overlap with `pane`'s
    /// centre line (deterministic: the far subtree's leaf whose span on the
    /// cross axis contains that centre coordinate).
    pub fn neighbour(&self, area: Rect, pane: PaneId, edge: Edge) -> Option<PaneId> {
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
    pub fn compute(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        collect_rects(&self.root, area, &mut out);
        out
    }

    /// Every border segment for `area`.
    pub fn borders(&self, area: Rect) -> Vec<BorderSegment> {
        let mut out = Vec::new();
        collect_borders(&self.root, area, &mut out);
        out
    }
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

/// tmux's `layout_set_main_v`: the first pane on the left, `main` wide but
/// leaving the right column at least one cell, and the rest spread down
/// the right column.
fn main_vertical(area: Rect, main: MainSize, panes: &[PaneId]) -> Option<Node> {
    let (first, rest) = panes.split_first()?;
    // The columns left after the border between the two columns.
    let avail = area.cols.checked_sub(1).filter(|&a| a >= 2)?;
    let want = match main {
        MainSize::Cells(c) => c,
        MainSize::Percent(p) => (u32::from(avail) * u32::from(p.min(100)) / 100) as u16,
    };
    // ponytail: `main-pane-width 0` or `0%` gives 1 cell here; tmux 3.7b gives a
    // degenerate 0-cell main pane, which a tree of >= 1-cell weights cannot hold.
    let main_cols = want.clamp(1, avail - 1);
    let right = match rest {
        [only] => Node::Leaf(*only),
        _ => even(Dir::Vertical, area.rows, rest)?,
    };
    Some(Node::Split {
        dir: Dir::Horizontal,
        children: vec![
            Child {
                weight: u32::from(main_cols),
                node: Node::Leaf(*first),
            },
            Child {
                weight: u32::from(avail - main_cols),
                node: right,
            },
        ],
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

/// Grows `node`'s own extent along `dir` by `delta` cells, freed by a
/// removed sibling elsewhere in the tree and entering from `side`. Matches
/// tmux's `layout_resize_adjust`, except that tmux spreads a resize's
/// growth evenly across a same-axis group's children; here the delta goes
/// entirely to the one child adjacent to `side`, the simpler rule `remove`
/// already needed (a removed sibling's cells have nowhere else to go).
///
/// A leaf has no weight of its own to grow: its size lives entirely in its
/// parent's `Child::weight`, which the caller already adjusted. A split
/// along `dir` (the growth axis) only has one child actually touching the
/// freed space: the last child if it came from the back, the first if it
/// came from the front. That child's own weight grows by `delta`, and
/// `grow` recurses into just that child, since none of the split's other
/// children changed extent. A split along the other axis doesn't subdivide
/// `dir` at all, so growing it doesn't change any of its children's own
/// weights (those are shares of the *other* axis); it changes what every
/// one of them inherits along `dir`, so all of them recurse with the same
/// delta and side.
fn grow(node: &mut Node, dir: Dir, delta: u32, side: Side) {
    if delta == 0 {
        return;
    }
    match node {
        Node::Leaf(_) => {}
        Node::Split {
            dir: node_dir,
            children,
        } => {
            if *node_dir == dir {
                let idx = match side {
                    Side::Front => 0,
                    Side::Back => children.len() - 1,
                };
                children[idx].weight += delta;
                grow(&mut children[idx].node, dir, delta, side);
            } else {
                for child in children.iter_mut() {
                    grow(&mut child.node, dir, delta, side);
                }
            }
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

/// Shrinks `node`'s own extent along `dir` by `delta` cells, taken from the
/// side facing the moved border (`side`) first, then its neighbours in
/// order if that side alone can't spare it all. The caller must first
/// clamp `delta` to `max_shrink(node, own_extent, dir)`, so every child
/// touched here has room to give up its share without dropping below one
/// cell; `shrink` itself doesn't re-check.
///
/// A leaf does nothing (mirrors `grow`'s leaf arm: its size lives in its
/// parent's weight, already reduced by the caller). A split along `dir`
/// takes as much as each child in turn (starting from `side`) can spare,
/// via `max_shrink`, until `delta` is used up; unlike `grow`, more than one
/// child can be touched, since one child's own ceiling might not cover the
/// whole request. A split along the other axis doesn't subdivide `dir`, so
/// every child inherits the same smaller extent: the full `delta` (not
/// divided) recurses into each of them.
fn shrink(node: &mut Node, dir: Dir, mut delta: u32, side: Side) {
    if delta == 0 {
        return;
    }
    match node {
        Node::Leaf(_) => {}
        Node::Split {
            dir: node_dir,
            children,
        } => {
            if *node_dir == dir {
                let order: Box<dyn Iterator<Item = usize>> = match side {
                    Side::Front => Box::new(0..children.len()),
                    Side::Back => Box::new((0..children.len()).rev()),
                };
                for idx in order {
                    if delta == 0 {
                        break;
                    }
                    let cap = max_shrink(&children[idx].node, children[idx].weight, dir);
                    let take = delta.min(cap);
                    if take == 0 {
                        continue;
                    }
                    debug_assert!(take < children[idx].weight);
                    children[idx].weight = children[idx].weight.saturating_sub(take);
                    shrink(&mut children[idx].node, dir, take, side);
                    delta -= take;
                }
            } else {
                for child in children.iter_mut() {
                    shrink(&mut child.node, dir, delta, side);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);
    const C: PaneId = PaneId(3);
    const D: PaneId = PaneId(4);
    const E: PaneId = PaneId(5);

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
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&B].rows, 12);
        assert_eq!(rects[&D].rows, 2);
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

        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 66);
        assert_eq!(rects[&D].cols, 2);

        // A follow-up resize inside the neighbour must still be exact.
        tree.resize(area, D, Edge::Left, 5);
        let after: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(after[&C].cols, 61);
        assert_eq!(after[&D].cols, 7);
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
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 12);
        assert_eq!(rects[&D].cols, 2);
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
        let rects: std::collections::HashMap<_, _> = tree.compute(area).into_iter().collect();
        assert_eq!(rects[&C].cols, 21);
        assert_eq!(rects[&D].cols, 2);
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
        assert_eq!(Preset::from_name("main-vertical"), Some(MAIN_80));
        assert_eq!(
            Preset::from_name("even-horizontal"),
            Some(Preset::EvenHorizontal)
        );
        assert_eq!(
            Preset::from_name("even-vertical"),
            Some(Preset::EvenVertical)
        );
        assert_eq!(Preset::from_name("tiled"), None);
        assert_eq!(Preset::from_name("nope"), None);
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
}
