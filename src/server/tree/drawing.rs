use super::*;

// --- Help ---------------------------------------------------------------

/// `mode_tree_help_start`: the keys every chooser has; `%1` is its item.
pub(super) const HELP_START: &[(&str, &str)] = &[
    ("Up, k", "Move cursor up"),
    ("Down, j", "Move cursor down"),
    ("g", "Go to top"),
    ("G", "Go to bottom"),
    ("PPage, C-b", "Page up"),
    ("NPage, C-f", "Page down"),
    ("Left, h", "Collapse %1"),
    ("Right, l", "Expand %1"),
    ("M--", "Collapse all %1s"),
    ("M-+", "Expand all %1s"),
    ("t", "Toggle %1 tag"),
    ("T", "Untag all %1s"),
    ("C-t", "Tag all %1s"),
    ("C-s", "Search forward"),
    ("n", "Repeat search forward"),
    ("N", "Repeat search backward"),
    ("f", "Filter %1s"),
    ("O", "Change sort order"),
    ("r", "Reverse sort order"),
    ("v", "Toggle preview"),
];

/// `mode_tree_help_end`.
pub(super) const HELP_END: &[(&str, &str)] = &[("q, Escape", "Exit mode")];

/// What each mode adds (`window_*_help_lines`), its minimum popup width
/// and the word `%1` stands for.
pub(super) fn help_of(
    kind: &Kind,
) -> (usize, &'static str, &'static [(&'static str, &'static str)]) {
    match kind {
        Kind::Tree { .. } => (
            51,
            "item",
            &[
                ("Enter", "Choose selected item"),
                ("S-Up", "Swap current and previous window"),
                ("S-Down", "Swap current and next window"),
                ("x", "Kill selected item"),
                ("X", "Kill tagged items"),
                ("<", "Scroll previews left"),
                (">", "Scroll previews right"),
                ("m", "Set the marked pane"),
                ("M", "Clear the marked pane"),
                (":", "Run a command for each tagged item"),
                ("f", "Enter a format"),
                ("H", "Jump to the starting pane"),
            ],
        ),
        Kind::Clients => (
            0,
            "client",
            &[
                ("Enter", "Choose selected %1"),
                ("d", "Detach selected %1"),
                ("D", "Detach tagged %1s"),
                ("x", "Detach selected %1"),
                ("X", "Detach tagged %1s"),
                ("z", "Suspend selected %1"),
                ("Z", "Suspend tagged %1s"),
                ("f", "Enter a filter"),
            ],
        ),
        Kind::Buffers => (
            0,
            "buffer",
            &[
                ("Enter", "Paste selected %1"),
                ("p", "Paste selected %1"),
                ("P", "Paste tagged %1s"),
                ("d", "Delete selected %1"),
                ("D", "Delete tagged %1s"),
                ("e", "Open %1 in editor"),
                ("f", "Enter a filter"),
            ],
        ),
        Kind::Customize { .. } => (
            52,
            "option",
            &[
                ("Enter, s", "Set %1 value"),
                ("S", "Set global %1 value"),
                ("w", "Set window %1 value"),
                ("d", "Set to default value"),
                ("D", "Set tagged %1s to default value"),
                ("u", "Unset an %1"),
                ("U", "Unset tagged %1s"),
                ("f", "Enter a filter"),
                ("v", "Toggle information"),
            ],
        ),
    }
}

/// `mode_tree_display_help`'s popup: its box on a client of `size`, and the
/// lines it is written with. tmux writes every line into a popup of the
/// lines' count less its border, so the first two scroll off the top.
#[allow(clippy::type_complexity)] // (origin, size, lines)
pub(super) fn help_box(
    kind: &Kind,
    size: (u16, u16),
) -> Option<((u16, u16), (u16, u16), Vec<(String, String)>)> {
    let (width, item, extra) = help_of(kind);
    let width = width.max(39);
    let lines: Vec<(String, String)> = HELP_START
        .iter()
        .chain(extra)
        .chain(HELP_END)
        .map(|(k, d)| ((*k).to_string(), d.replace("%1", item)))
        .collect();
    let (w, h) = (u16::try_from(width).ok()?, u16::try_from(lines.len()).ok()?);
    if size.0 < w || size.1 < h {
        return None;
    }
    let skip = lines.len().saturating_sub(usize::from(h).saturating_sub(2));
    let shown = lines.into_iter().skip(skip).collect();
    Some((((size.0 - w) / 2, (size.1 - h) / 2), (w, h), shown))
}

/// Opens the help popup, if it fits the client.
pub(super) fn open_help(state: &mut State, client: ClientId) {
    let Some(size) = state.clients.get(&client).map(|c| c.size) else {
        return;
    };
    let Some(tree) = state.trees.open.get_mut(&client) else {
        return;
    };
    if help_box(&tree.kind, size).is_some() {
        tree.help = true;
    }
}

/// Draws the help popup over the chooser.
pub(super) fn draw_help(state: &State, client: ClientId, tree: &Tree, screen: &mut Screen) {
    use crate::server::popup::{self, Frame, Lines};
    let Some(((px, py), (w, h), lines)) = help_box(&tree.kind, (screen.cols, screen.rows)) else {
        return;
    };
    let chain = options::chain(&state.model, tree.pane);
    let lines_style = state
        .model
        .options
        .lookup("popup-border-lines", &chain)
        .map(|v| Lines::parse(v).unwrap_or(Some(Lines::Single)))
        .unwrap_or(Some(Lines::Single));
    let frame = Frame {
        at: (i32::from(px), i32::from(py)),
        size: (w, h),
        lines: lines_style,
        title: None,
        border_style: None,
        style: None,
        close_on_exit: false,
        close_on_zero: false,
        any_key: false,
    };
    let (fill, border) = popup::styles(state, client, &frame);
    let ((ix, iy), (iw, ih)) = (
        (i32::from(px) + 1, i32::from(py) + 1),
        (
            usize::from(w).saturating_sub(2),
            usize::from(h).saturating_sub(2),
        ),
    );
    for y in 0..ih {
        for x in 0..iw {
            popup::set(
                screen,
                (ix + x as i32, iy + y as i32),
                popup::cell(' ', &fill),
            );
        }
    }
    if let Some(l) = frame.lines {
        popup::draw_box(screen, &frame, l, &border);
    }
    let mut bold = fill;
    bold.attrs |= CellFlags::BOLD;
    for (row, (key, desc)) in lines.iter().enumerate().take(ih) {
        let mut x = 0;
        let mut put = |text: &str, style: &Style, x: &mut usize| {
            for c in text.chars() {
                if *x >= iw {
                    return;
                }
                popup::set(
                    screen,
                    (ix + *x as i32, iy + row as i32),
                    popup::cell(c, style),
                );
                *x += 1;
            }
        };
        put(&format!("{key:>11} "), &bold, &mut x);
        // The vertical line of the alternate character set.
        put("│", &fill, &mut x);
        put(&format!(" {desc}"), &fill, &mut x);
    }
}

// --- Drawing ------------------------------------------------------------

/// Draws a client's chooser over its pane, when that pane is on screen.
pub struct TreeOverlay;

impl Overlay for TreeOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        let Some(tree) = state.trees.open.get(&client) else {
            return;
        };
        let Some(geom) = geometry(state, tree.pane, cx) else {
            return;
        };
        let v = view(state, tree);
        let mut canvas = Canvas { screen, geom };
        draw(state, tree, &v, &mut canvas);
        canvas.screen.cursor = None;
        if tree.help {
            draw_help(state, client, tree, canvas.screen);
        }
    }
}

/// Where a pane's cells land on a client's screen: its origin in client
/// cells (off screen when the client crops the window), the part that is
/// visible, and the pane's size.
pub(super) struct Geom {
    pub(super) origin: (i32, i32),
    pub(super) clip: Rect,
    pub(super) cols: usize,
    pub(super) rows: usize,
}

pub(super) fn geometry(state: &State, pane: PaneId, cx: &OverlayCtx) -> Option<Geom> {
    let clip = cx.panes.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r)?;
    let model = &state.model;
    let w = model.windows.get(&model.panes.get(&pane)?.window)?;
    let (_, at) = clients::visible_panes(w)
        .into_iter()
        .find(|(id, _)| *id == pane)?;
    Some(Geom {
        origin: (
            i32::from(at.x) - i32::from(cx.viewport.0),
            i32::from(at.y) - i32::from(cx.viewport.1) + i32::from(cx.window_top),
        ),
        clip,
        cols: usize::from(at.cols),
        rows: usize::from(at.rows),
    })
}

/// A pane-sized area of a client's screen, drawn in pane cells.
pub(super) struct Canvas<'a> {
    pub(super) screen: &'a mut Screen,
    pub(super) geom: Geom,
}

pub(super) fn blank(style: &Style) -> SnapCell {
    SnapCell {
        c: ' ',
        fg: style.fg,
        bg: style.bg,
        flags: style.attrs,
        zerowidth: Vec::new(),
        underline_color: style.underline_color,
        hyperlink: None,
    }
}

impl Canvas<'_> {
    /// The screen index of pane cell `(x, y)`, if it is visible.
    pub(super) fn at(&self, x: usize, y: usize) -> Option<usize> {
        let g = &self.geom;
        if x >= g.cols || y >= g.rows {
            return None;
        }
        let (sx, sy) = (g.origin.0 + x as i32, g.origin.1 + y as i32);
        let c = g.clip;
        let inside = sx >= i32::from(c.x)
            && sx < i32::from(c.x) + i32::from(c.cols)
            && sy >= i32::from(c.y)
            && sy < i32::from(c.y) + i32::from(c.rows);
        let (sx, sy) = (usize::try_from(sx).ok()?, usize::try_from(sy).ok()?);
        (inside && sx < usize::from(self.screen.cols) && sy < usize::from(self.screen.rows))
            .then(|| sy * usize::from(self.screen.cols) + sx)
    }

    pub(super) fn cell(&mut self, x: usize, y: usize, cell: SnapCell) {
        if let Some(i) = self.at(x, y) {
            self.screen.cells[i] = cell;
        }
    }

    /// Puts `c` at `(x, y)`; a wide character takes two cells. Returns its
    /// width. A control character (no width) is dropped: text from a pane
    /// never reaches the terminal as bytes.
    pub(super) fn put(&mut self, x: usize, y: usize, c: char, style: &Style) -> usize {
        let Some(width) = UnicodeWidthChar::width(c) else {
            return 0;
        };
        if width == 0 {
            return 0;
        }
        let mut cell = blank(style);
        cell.c = c;
        if width == 2 {
            cell.flags |= CellFlags::WIDE_CHAR;
            let mut spacer = blank(style);
            spacer.flags |= CellFlags::WIDE_CHAR_SPACER;
            self.cell(x + 1, y, spacer);
        }
        self.cell(x, y, cell);
        width
    }

    /// Writes `text` from `(x, y)` in at most `max` cells; the cells used.
    pub(super) fn text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        style: &Style,
        max: usize,
    ) -> usize {
        let mut col = 0;
        for c in text.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + w > max {
                break;
            }
            col += self.put(x + col, y, c, style);
        }
        col
    }

    pub(super) fn fill(&mut self, x: usize, y: usize, n: usize, style: &Style) {
        for i in 0..n {
            self.cell(x + i, y, blank(style));
        }
    }

    /// `screen_write_box`: a `w` by `h` box in the default lines.
    pub(super) fn frame(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if w < 2 || h < 2 {
            return;
        }
        let style = Style::default();
        for i in 0..w {
            let (top, bottom) = match i {
                0 => ('┌', '└'),
                i if i + 1 == w => ('┐', '┘'),
                _ => ('─', '─'),
            };
            self.put(x + i, y, top, &style);
            self.put(x + i, y + h - 1, bottom, &style);
        }
        for j in 1..h - 1 {
            self.put(x, y + j, '│', &style);
            self.put(x + w - 1, y + j, '│', &style);
        }
    }

    pub(super) fn vline(&mut self, x: usize, y: usize, h: usize) {
        for j in 0..h {
            self.put(x, y + j, '│', &Style::default());
        }
    }
}

/// `mode_tree_draw`.
pub(super) fn draw(state: &State, tree: &Tree, v: &View, cv: &mut Canvas) {
    let (w, sy) = (cv.geom.cols, cv.geom.rows);
    let plain = Style::default();
    let chain = options::chain(&state.model, tree.pane);
    let mode = status::parse_style(
        state
            .model
            .options
            .lookup("mode-style", &chain)
            .unwrap_or(MODE_STYLE),
    );
    let mode_bg = Style {
        bg: mode.bg,
        ..Style::default()
    };
    for y in 0..sy {
        cv.fill(0, y, w, &plain);
    }
    if v.lines.is_empty() {
        return;
    }
    let keylen = v
        .keys
        .iter()
        .flatten()
        .map(|k| k.len() + 3)
        .max()
        .unwrap_or(0);
    let mut alignlen: BTreeMap<usize, usize> = BTreeMap::new();
    for line in v.lines.iter().filter(|l| l.align) {
        let len = alignlen.entry(line.depth).or_default();
        *len = (*len).max(line.name.len());
    }
    let h = v.height;
    for (i, line) in v.lines.iter().enumerate().skip(v.offset).take(h) {
        let y = i - v.offset;
        let key = v.keys[i]
            .as_ref()
            .map(|k| format!("({k})"))
            .unwrap_or_default();
        let symbol = if line.flat {
            ""
        } else if !line.has_children {
            "  "
        } else if line.expanded {
            "- "
        } else {
            "+ "
        };
        let mut start = String::new();
        if line.depth > 0 {
            let parent_last = line.parent.is_some_and(|p| v.lines[p].last);
            for _ in 1..line.depth {
                start.push_str(if parent_last { "    " } else { "│   " });
            }
            start.push_str(if line.last { "└─> " } else { "├─> " });
        }
        start.push_str(symbol);
        let name = if line.align {
            let width = alignlen.get(&line.depth).copied().unwrap_or(0);
            format!("{:>width$}", line.name)
        } else {
            line.name.clone()
        };
        let tagged = if tree.tagged.contains(&line.tag) {
            "*"
        } else {
            ""
        };
        let colon = if line.plain { "" } else { ": " };
        let text = format!("{key:<keylen$}{start}{name}{tagged}{colon}");
        let width = UnicodeWidthStr::width(text.as_str()).min(w);
        let mut style = if i == v.current {
            cv.fill(0, y, w, &mode_bg);
            mode
        } else {
            plain
        };
        if !tagged.is_empty() {
            style.attrs ^= CellFlags::BOLD;
        }
        let used = cv.text(0, y, &text, &style, w);
        // The item's text is drawn as a format (`#[...]` styles work).
        let drawn = status::draw(&line.text, style, (w - width) as u16);
        let mut x = used;
        for span in &drawn.line.spans {
            x += cv.text(x, y, &span.text, &span.style, w.saturating_sub(x));
        }
    }

    if tree.preview == PreviewMode::Off || sy <= 4 || h < 2 || sy - h <= 4 || w <= 4 {
        return;
    }
    let current = &v.lines[v.current];
    // A row under a key shows the key (`mode_tree_draw_as_parent`).
    let line = match current.parent {
        Some(p) if current.as_parent => &v.lines[p],
        _ => current,
    };
    cv.frame(0, h, w, sy - h);
    let title = if tree.kind.orders().is_empty() {
        format!(" {}", line.name)
    } else {
        format!(
            " {} (sort: {}{})",
            line.name,
            tree.order.name(),
            if tree.reversed { ", reversed" } else { "" }
        )
    };
    if w - 2 >= title.len() {
        let mut x = 1 + cv.text(1, h, &title, &plain, w - 2);
        let filter = tree.filter.is_some() || matches!(&tree.kind, Kind::Tree { find: Some(_) });
        let n = if v.no_matches {
            "no matches".len()
        } else {
            "active".len()
        };
        if filter && w - 2 >= title.len() + 10 + n + 2 {
            x += cv.text(x, h, " (filter: ", &plain, w);
            x += if v.no_matches {
                cv.text(x, h, "no matches", &mode, w)
            } else {
                cv.text(x, h, "active", &plain, w)
            };
            cv.text(x, h, ") ", &plain, w);
        } else {
            cv.text(x, h, " ", &plain, w);
        }
    }
    let (box_x, box_y) = (w - 4, sy - h - 2);
    if box_x == 0 || box_y == 0 {
        return;
    }
    let (x, y) = (2, h + 1);
    match line.tag {
        Tag::Session(_) | Tag::Window(_) => {
            if let Some((entries, current)) = strip_of(state, line.tag) {
                draw_strip(
                    state,
                    cv,
                    (x, y, box_x, box_y),
                    &entries,
                    current,
                    tree.preview_offset,
                    line.session,
                );
            }
        }
        Tag::Pane(p) => preview(state, cv, p, (x, y, box_x, box_y)),
        Tag::Client(c) => draw_client(state, cv, c, (x, y, box_x, box_y)),
        Tag::Buffer(id) => draw_buffer(state, cv, id, (x, y, box_x, box_y)),
        Tag::Custom(t) => draw_custom(state, tree, cv, t, (x, y, box_x, box_y)),
    }
}

/// customize-mode's preview: what the item is, as `customize::info` says.
pub(super) fn draw_custom(
    state: &State,
    tree: &Tree,
    cv: &mut Canvas,
    tag: u64,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
) {
    let hide = matches!(tree.kind, Kind::Customize { hide_global: true });
    let Some((built, fs)) = custom_build(state, tree, tree.filter.as_deref(), hide) else {
        return;
    };
    let Some(entry) = built.entries.get(&tag) else {
        return;
    };
    let ctx = FormatCtx::new(&state.model, fs.pane);
    let vars = ServerVars::new(state, fs.pane, &ctx);
    let page = customize::info(state, fs, entry, sx, sy, &vars);
    for (row, segments) in page.lines.iter().take(sy).enumerate() {
        let mut x = 0;
        for (text, style) in segments {
            x += cv.text(
                cx + x,
                cy + row,
                text,
                &style.unwrap_or_default(),
                sx.saturating_sub(x),
            );
        }
    }
}

/// `window_buffer_draw`: the buffer's first lines, each escaped as
/// `vis(3)` writes it.
pub(super) fn draw_buffer(
    state: &State,
    cv: &mut Canvas,
    id: u64,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
) {
    let Some(b) = state.buffers.by_id(id) else {
        return;
    };
    let lines = b.data.split(|c| *c == b'\n');
    for (i, line) in lines.take(sy).enumerate() {
        let text = crate::server::buffers::escape(line);
        cv.text(cx, cy + i, &text, &Style::default(), sx);
    }
}

/// `window_tree_draw_session` and `_window`: the previews side by side,
/// each with its label, `<` and `>` when some are scrolled off.
pub(super) fn draw_strip(
    state: &State,
    cv: &mut Canvas,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
    entries: &[Preview],
    current: usize,
    offset: i32,
    session: Option<SessionId>,
) {
    let Some(g) = strip(entries.len(), current, sx, offset) else {
        return;
    };
    let plain = Style::default();
    if g.left {
        cv.vline(cx + 2, cy, sy);
        cv.put(cx, cy + sy / 2, '<', &plain);
    }
    if g.right {
        cv.vline(cx + sx - 3, cy, sy);
        cv.put(cx + sx - 1, cy + sy / 2, '>', &plain);
    }
    for (i, (pane, tag, _active)) in entries[g.start..g.end].iter().enumerate() {
        let x = if g.left { 3 } else { 0 } + i * g.each;
        let last = g.start + i + 1 == g.end;
        let width = if last {
            g.each + g.remaining
        } else {
            g.each - 1
        };
        preview(state, cv, *pane, (cx + x, cy, width, sy));
        // The label and its style are formats of the window or pane, from
        // the options it sees.
        let chain = options::chain(&state.model, *pane);
        let option = |name: &str| state.model.options.lookup(name, &chain).unwrap_or("");
        let expand = |text: &str| expand_tag(state, *pane, session, *tag, text, None);
        let style = status::parse_style(&expand(option("tree-mode-preview-style")));
        let format = option("tree-mode-preview-format");
        let label = if format.is_empty() {
            String::new()
        } else {
            expand(format)
        };
        if !label.is_empty() {
            draw_label(cv, (cx + x, cy, width, sy), &style, &label);
        }
        if !last {
            cv.vline(cx + x + width, cy, sy);
        }
    }
}

/// `window_tree_draw_label`: `label` in a small box in the middle of the
/// `sx` by `sy` area at `(px, py)`.
pub(super) fn draw_label(
    cv: &mut Canvas,
    (px, py, sx, sy): (usize, usize, usize, usize),
    style: &Style,
    label: &str,
) {
    if sx < 5 || sy < 3 {
        return;
    }
    let mut label: String = label.chars().filter(|c| !c.is_control()).collect();
    if UnicodeWidthStr::width(label.as_str()) > sx - 4 {
        let mut kept = String::new();
        let mut used = 0;
        for c in label.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if used + w > sx - 4 {
                break;
            }
            used += w;
            kept.push(c);
        }
        label = kept;
    }
    let width = UnicodeWidthStr::width(label.as_str());
    if width == 0 {
        return;
    }
    let (ox, oy) = ((sx - width).div_ceil(2), sy.div_ceil(2));
    cv.frame(px + ox - 2, py + oy - 1, width + 4, 3);
    cv.fill(px + ox - 1, py + oy, width + 2, &Style::default());
    cv.text(px + ox, py + oy, &label, style, width);
}

/// `screen_write_preview`: an `nx` by `ny` part of `pane`'s screen at
/// `(x, y)`, around its cursor when the cursor is on (its cell reversed),
/// else from the top left.
pub(super) fn preview(
    state: &State,
    cv: &mut Canvas,
    pane: PaneId,
    (x, y, nx, ny): (usize, usize, usize, usize),
) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    let (cols, rows) = (usize::from(p.cols), usize::from(p.rows));
    let snap = p.handle.snapshot_rows(0..p.rows);
    let cursor = snap.cursor;
    let around = |c: usize, n: usize, size: usize| {
        let at = c.saturating_sub(n / 3);
        if at + n > size {
            size.saturating_sub(n)
        } else {
            at
        }
    };
    let (px, py) = if cursor.visible {
        (
            around(usize::from(cursor.col), nx, cols),
            around(usize::from(cursor.row), ny, rows),
        )
    } else {
        (0, 0)
    };
    let plain = Style::default();
    for j in 0..ny {
        for i in 0..nx {
            let cell = snap
                .rows
                .get(py + j)
                .and_then(|r| r.get(px + i))
                .cloned()
                .unwrap_or_else(|| blank(&plain));
            cv.cell(x + i, y + j, cell);
        }
    }
    if cursor.visible {
        let (ccol, crow) = (usize::from(cursor.col), usize::from(cursor.row));
        if let Some(mut cell) = snap.rows.get(crow).and_then(|r| r.get(ccol)).cloned() {
            cell.flags |= CellFlags::INVERSE;
            cv.cell(x + ccol - px, y + crow - py, cell);
        }
    }
}

/// `window_client_draw`: the client's current pane, a line, and its
/// status line as last drawn.
pub(super) fn draw_client(
    state: &State,
    cv: &mut Canvas,
    client: ClientId,
    (cx, cy, sx, sy): (usize, usize, usize, usize),
) {
    let Some(c) = state.clients.get(&client) else {
        return;
    };
    let model = &state.model;
    let Some(pane) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
        .map(|w| w.active)
    else {
        return;
    };
    let (rows, top) = clients::drawn_status(state, client).unwrap_or((&[], false));
    let lines = if rows.len() >= sy { 0 } else { rows.len() };
    let at = if top { lines } else { 0 };
    if sy < 2 + lines {
        return;
    }
    preview(state, cv, pane, (cx, cy + at, sx, sy - 2 - lines));
    let rule = if at != 0 { cy + 2 } else { cy + sy - 1 - lines };
    for i in 0..sx {
        cv.put(cx + i, rule, '─', &Style::default());
    }
    let first = if at != 0 { cy } else { cy + sy - lines };
    for (j, row) in rows.iter().take(lines).enumerate() {
        let mut col = 0;
        for span in &row.line.spans {
            let used = cv.text(cx + col, first + j, &span.text, &span.style, sx - col);
            col += used;
            if col >= sx {
                break;
            }
        }
    }
}
