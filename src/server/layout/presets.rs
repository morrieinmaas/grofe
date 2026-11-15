use super::*;

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

/// Sizes for `n` cells side by side along `len`, as tmux's
/// `layout_spread_cell`: the cells left after the `n - 1` borders are
/// split evenly and the leftover go to the first cells, one each. `None`
/// if some cell would get nothing.
pub(super) fn spread(len: u16, n: usize) -> Option<Vec<u32>> {
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
pub(super) fn even(dir: Dir, len: u16, panes: &[PaneId]) -> Option<Node> {
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
pub(super) fn main_split(
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
pub(super) fn tiled(area: Rect, panes: &[PaneId], max_columns: u16) -> Option<Node> {
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
