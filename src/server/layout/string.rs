use super::*;

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

pub(super) fn append_cell(g: &LayoutGeometry, out: &mut String) {
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
pub(super) fn digits(rest: &mut &[u8]) -> Option<u32> {
    let n = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let (num, tail) = rest.split_at(n);
    *rest = tail;
    std::str::from_utf8(num).ok()?.parse().ok()
}

pub(super) fn expect(rest: &mut &[u8], byte: u8) -> Option<()> {
    let (&first, tail) = rest.split_first()?;
    (first == byte).then(|| *rest = tail)
}

/// One cell and its children (`layout_construct`).
pub(super) fn construct(rest: &mut &[u8], depth: usize, panes: &mut u32) -> Option<LayoutGeometry> {
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

pub(super) fn count_cells(g: &LayoutGeometry, n: &mut usize) {
    match &g.cell {
        GeometryCell::Pane(_) => *n += 1,
        GeometryCell::Split { children, .. } => children.iter().for_each(|c| count_cells(c, n)),
    }
}

/// tmux's fix for layouts older versions wrote with a wrong top cell size:
/// a split root takes its size from its children.
pub(super) fn fix_root_size(g: &mut LayoutGeometry) {
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
