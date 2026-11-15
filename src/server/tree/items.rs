use super::*;

// --- Building ---------------------------------------------------------

/// tmux's `format_true`.
pub(crate) fn format_true(s: &str) -> bool {
    !s.is_empty() && s != "0"
}

/// The items, and whether a filter matched nothing (then everything is
/// listed, as tmux does).
pub(super) fn build(state: &State, tree: &Tree) -> (Vec<Item>, bool) {
    let filtered = |filter: Option<&str>| match &tree.kind {
        Kind::Clients => client_items(state, tree, filter),
        Kind::Buffers => buffer_items(state, tree, filter),
        Kind::Tree { find } => tree_items(state, tree, find.as_ref(), filter),
        Kind::Customize { hide_global } => custom_items(state, tree, filter, *hide_global),
    };
    let items = filtered(tree.filter.as_deref());
    let finding = matches!(&tree.kind, Kind::Tree { find: Some(_) });
    if items.is_empty() && (tree.filter.is_some() || finding) {
        let all = match &tree.kind {
            Kind::Tree { .. } => tree_items(state, tree, None, None),
            _ => filtered(None),
        };
        return (all, true);
    }
    (items, false)
}

pub(super) fn tree_items(
    state: &State,
    tree: &Tree,
    find: Option<&Find>,
    filter: Option<&str>,
) -> Vec<Item> {
    use std::cmp::Ordering::Equal;
    let model = &state.model;
    let (order, reversed) = (tree.order, tree.reversed);
    let format = tree.item_format();
    let filter = filter.map(|f| tree.native(f));
    let filter = filter.as_deref();
    // The session the target pane is in, for `-G`.
    let current = model
        .panes
        .get(&tree.pane)
        .and_then(|p| model.windows.get(&p.window))
        .map(|w| w.session);
    let mut sessions: Vec<&crate::server::model::Session> = model.sessions.values().collect();
    sessions.sort_by(|a, b| {
        let o = match order {
            Order::Index => a.id.cmp(&b.id),
            Order::Creation => a.created.cmp(&b.created),
            Order::Activity => b.activity.cmp(&a.activity),
            _ => Equal,
        };
        let r = o.then(a.name.cmp(&b.name));
        if reversed { r.reverse() } else { r }
    });
    if order == Order::Unsorted && reversed {
        sessions.reverse();
    }
    let mut out = Vec::new();
    for s in sessions {
        if tree.squash_groups {
            let peers = model.group_peers(s.id);
            if !peers.is_empty() {
                let mut group = peers.clone();
                group.push(s.id);
                let first = group.iter().min().copied();
                let in_current = current.is_some_and(|c| group.contains(&c));
                if (in_current && Some(s.id) != current) || (!in_current && first != Some(s.id)) {
                    continue;
                }
            }
        }
        let mut windows: Vec<&crate::server::model::Window> = s
            .windows
            .iter()
            .filter_map(|w| model.windows.get(w))
            .collect();
        windows.sort_by(|a, b| {
            let o = match order {
                Order::Index => model
                    .index_in(s.id, a.id)
                    .unwrap_or(a.index)
                    .cmp(&model.index_in(s.id, b.id).unwrap_or(b.index)),
                Order::Creation => a.id.cmp(&b.id),
                Order::Activity => b.activity.cmp(&a.activity),
                Order::Size => (u32::from(a.cols) * u32::from(a.rows))
                    .cmp(&(u32::from(b.cols) * u32::from(b.rows))),
                _ => Equal,
            };
            let r = o.then(a.name.cmp(&b.name));
            if reversed { r.reverse() } else { r }
        });
        if order == Order::Unsorted && reversed {
            windows.reverse();
        }
        let mut kids = Vec::new();
        for w in windows {
            let ids = w.pane_ids();
            // tmux drops a one-pane window whose pane does not match, but
            // keeps a window of several panes even when none does.
            let Some(first) = ids.first() else {
                continue;
            };
            if ids.len() == 1
                && !(matches(state, *first, find) && keeps(state, tree.pane, s.id, *first, filter))
            {
                continue;
            }
            let base = model.pane_base_index(w.id);
            let mut panes: Vec<(usize, PaneId)> = ids.iter().copied().enumerate().collect();
            panes.sort_by(|(ia, a), (ib, b)| {
                let title = |p: &PaneId| state.passthrough.title(*p);
                let area = |p: &PaneId| {
                    model
                        .panes
                        .get(p)
                        .map_or(0, |p| u32::from(p.cols) * u32::from(p.rows))
                };
                let o = match order {
                    Order::Index | Order::Z => ia.cmp(ib),
                    Order::Creation => a.cmp(b),
                    Order::Size => area(a).cmp(&area(b)),
                    _ => Equal,
                };
                let r = o.then(title(a).cmp(&title(b)));
                if reversed { r.reverse() } else { r }
            });
            if order == Order::Unsorted && reversed {
                panes.reverse();
            }
            let children = panes
                .iter()
                .filter(|(_, p)| {
                    matches(state, *p, find) && keeps(state, tree.pane, s.id, *p, filter)
                })
                .map(|(i, p)| Item {
                    tag: Tag::Pane(*p),
                    session: Some(s.id),
                    name: (base + *i as u32).to_string(),
                    text: expand_tag(state, tree.pane, Some(s.id), Tag::Pane(*p), &format, None),
                    align: true,
                    children: Vec::new(),
                    ..Item::plain_defaults()
                })
                .collect();
            kids.push(Item {
                tag: Tag::Window(w.id),
                session: Some(s.id),
                name: model.index_in(s.id, w.id).unwrap_or(w.index).to_string(),
                text: expand_tag(
                    state,
                    tree.pane,
                    Some(s.id),
                    Tag::Window(w.id),
                    &format,
                    None,
                ),
                align: true,
                children,
                ..Item::plain_defaults()
            });
        }
        if kids.is_empty() {
            continue;
        }
        out.push(Item {
            tag: Tag::Session(s.id),
            session: Some(s.id),
            name: s.name.clone(),
            text: expand_tag(
                state,
                tree.pane,
                Some(s.id),
                Tag::Session(s.id),
                &format,
                None,
            ),
            align: false,
            children: kids,
            ..Item::plain_defaults()
        });
    }
    out
}

/// `window_tree_filter_pane`: the filter (a format) is true for the pane.
pub(super) fn keeps(
    state: &State,
    at: PaneId,
    session: SessionId,
    pane: PaneId,
    filter: Option<&str>,
) -> bool {
    filter.is_none_or(|f| {
        format_true(&expand_tag(
            state,
            at,
            Some(session),
            Tag::Pane(pane),
            f,
            None,
        ))
    })
}

/// `find-window`'s filter for one pane, as tmux 3.7b builds it
/// (`cmd-find-window.c`): with none of `-C`, `-N`, `-T` all three; the window
/// name or the pane title as `#{m<suffix>:<glob>,...}`, or a line of the
/// pane's visible text as `#{C<suffix>:<pattern>}` (a `*<pattern>*` glob per
/// line, trailing blanks dropped; with `-r` a regular expression, as the
/// names and titles are). The pattern reaches the format through a
/// variable, so its `#`, `,` and `}` are text, never format syntax.
// ponytail: evaluated on every draw of the chooser (tmux filters when it
// rebuilds the tree); fine for the dozens of panes a server holds.
pub(super) fn matches(state: &State, pane: PaneId, find: Option<&Find>) -> bool {
    let Some(find) = find else {
        return true;
    };
    let (mut content, mut name, mut title) = (find.content, find.name, find.title);
    if !content && !name && !title {
        (content, name, title) = (true, true, true);
    }
    let suffix = match (find.regex, find.ignore) {
        (true, true) => "/ri",
        (true, false) => "/r",
        (false, true) => "/i",
        (false, false) => "",
    };
    let vars = FormatCtx::new(&state.model, pane);
    let server = ServerVars::new(state, pane, &vars);
    let glob = if find.regex {
        find.pattern.clone()
    } else {
        format!("*{}*", find.pattern)
    };
    for (on, variable) in [(name, "window_name"), (title, "pane_title")] {
        let format = format!("#{{m{suffix}:#{{find_window_glob}},#{{{variable}}}}}");
        let vars = FindVars {
            inner: &server,
            glob: &glob,
            line: "",
        };
        if on && cmd::expand(&format, &vars) == "1" {
            return true;
        }
    }
    let format = format!("#{{m{suffix}:#{{find_window_glob}},#{{find_window_line}}}}");
    content
        && state.model.panes.get(&pane).is_some_and(|p| {
            let visible = crate::types::CaptureOpts {
                start: None,
                end: None,
                escapes: false,
            };
            p.handle.capture(&visible).lines().any(|line| {
                let line = line.trim_end_matches(|c: char| c.is_ascii_whitespace());
                let vars = FindVars {
                    inner: &server,
                    glob: &glob,
                    line,
                };
                cmd::expand(&format, &vars) == "1"
            })
        })
}

/// The variables `find-window`'s filter adds: its glob and the text line
/// being tried.
pub(super) struct FindVars<'a> {
    pub(super) inner: &'a dyn cmd::FormatVars,
    pub(super) glob: &'a str,
    pub(super) line: &'a str,
}

impl cmd::FormatVars for FindVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "find_window_glob" => Some(self.glob.to_string()),
            "find_window_line" => Some(self.line.to_string()),
            _ => self.inner.get(name),
        }
    }
}

/// The variables of an item's format plus the line number a key format
/// reads.
pub(super) struct WithLine<'a> {
    pub(super) inner: &'a dyn cmd::FormatVars,
    pub(super) line: Option<usize>,
}

impl cmd::FormatVars for WithLine<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match (name, self.line) {
            ("line", Some(n)) => Some(n.to_string()),
            _ => self.inner.get(name),
        }
    }
}

/// `fmt` expanded for the item `tag`, as tmux's `format_defaults` sets up
/// a session, a winlink or a pane (`session` is the session a window or
/// pane is listed under), a client or a paste buffer; `line` is what a key
/// format sees as `#{line}`.
pub(super) fn expand_tag(
    state: &State,
    at: PaneId,
    session: Option<SessionId>,
    tag: Tag,
    fmt: &str,
    line: Option<usize>,
) -> String {
    let model = &state.model;
    let active_of = |s: SessionId| {
        model
            .sessions
            .get(&s)
            .and_then(|s| model.windows.get(&s.active))
            .map(|w| w.active)
    };
    let (pane, ctx) = match tag {
        Tag::Session(s) => match active_of(s) {
            Some(p) => (p, FormatCtx::at(model, p, Level::Session).in_session(s)),
            None => return fmt.to_string(),
        },
        Tag::Window(w) => {
            let Some(win) = model.windows.get(&w) else {
                return fmt.to_string();
            };
            let ctx = FormatCtx::at(model, win.active, Level::Window);
            let ctx = match session {
                Some(s) => ctx.in_session(s),
                None => ctx,
            };
            (win.active, ctx)
        }
        Tag::Pane(p) => {
            let ctx = FormatCtx::at(model, p, Level::Pane);
            let ctx = match session {
                Some(s) => ctx.in_session(s),
                None => ctx,
            };
            (p, ctx)
        }
        Tag::Client(id) => {
            let Some(c) = state.clients.get(&id) else {
                return fmt.to_string();
            };
            let Some(p) = active_of(c.session) else {
                return fmt.to_string();
            };
            let vars = clients::ClientVars::new(model, c, Some(FormatCtx::new(model, p)));
            let server = ServerVars::new(state, p, &vars);
            return cmd::expand(
                fmt,
                &WithLine {
                    inner: &server,
                    line,
                },
            );
        }
        Tag::Custom(_) => return fmt.to_string(),
        Tag::Buffer(id) => {
            let Some(b) = state.buffers.by_id(id) else {
                return fmt.to_string();
            };
            if !model.panes.contains_key(&at) {
                return cmd::expand(fmt, &crate::server::buffers::BufferVars(b));
            }
            let ctx = FormatCtx::new(model, at);
            let server = ServerVars::new(state, at, &ctx);
            let vars = BufferOver {
                buffer: crate::server::buffers::BufferVars(b),
                inner: &server,
            };
            return cmd::expand(fmt, &WithLine { inner: &vars, line });
        }
    };
    let server = ServerVars::new(state, pane, &ctx);
    cmd::expand(
        fmt,
        &WithLine {
            inner: &server,
            line,
        },
    )
}

/// A buffer's variables over a pane's.
pub(super) struct BufferOver<'a> {
    pub(super) buffer: crate::server::buffers::BufferVars<'a>,
    pub(super) inner: &'a dyn cmd::FormatVars,
}

impl cmd::FormatVars for BufferOver<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.buffer.get(name).or_else(|| self.inner.get(name))
    }
}

/// tmux's `window-buffer.c` list: each line its name and the format
/// (`#{t/p:buffer_created}: #{buffer_sample}`).
pub(super) fn buffer_items(state: &State, tree: &Tree, filter: Option<&str>) -> Vec<Item> {
    let format = tree.item_format();
    let format = format.as_str();
    let filter = filter.map(|f| tree.native(f));
    let filter = filter.as_deref();
    let mut list: Vec<&crate::server::buffers::Buffer> = state.buffers.iter().collect();
    let order = tree.order;
    list.sort_by(|a, b| {
        let o = match order {
            Order::Creation => b.id.cmp(&a.id),
            Order::Size => a.data.len().cmp(&b.data.len()),
            _ => std::cmp::Ordering::Equal,
        };
        let r = o.then(a.name.cmp(&b.name));
        if tree.reversed { r.reverse() } else { r }
    });
    list.into_iter()
        .filter(|b| {
            filter.is_none_or(|f| {
                format_true(&expand_tag(
                    state,
                    tree.pane,
                    None,
                    Tag::Buffer(b.id),
                    f,
                    None,
                ))
            })
        })
        .map(|b| Item {
            tag: Tag::Buffer(b.id),
            session: None,
            name: b.name.clone(),
            text: expand_tag(state, tree.pane, None, Tag::Buffer(b.id), format, None),
            align: false,
            children: Vec::new(),
            ..Item::plain_defaults()
        })
        .collect()
}

pub(super) fn client_items(state: &State, tree: &Tree, filter: Option<&str>) -> Vec<Item> {
    let format = tree.item_format();
    let format = format.as_str();
    let filter = filter.map(|f| tree.native(f));
    let filter = filter.as_deref();
    let mut list: Vec<(&ClientId, &clients::ClientEntry)> = state.clients.iter().collect();
    let order = tree.order;
    list.sort_by(|(ia, a), (ib, b)| {
        let o = match order {
            Order::Size => a.size.0.cmp(&b.size.0).then(a.size.1.cmp(&b.size.1)),
            Order::Creation => ia.cmp(ib),
            Order::Activity => b.activity_time.cmp(&a.activity_time),
            _ => std::cmp::Ordering::Equal,
        };
        let r = o.then(a.name.cmp(&b.name));
        if tree.reversed { r.reverse() } else { r }
    });
    list.into_iter()
        .filter(|(id, _)| {
            filter.is_none_or(|f| {
                format_true(&expand_tag(
                    state,
                    tree.pane,
                    None,
                    Tag::Client(**id),
                    f,
                    None,
                ))
            })
        })
        .map(|(id, c)| Item {
            tag: Tag::Client(*id),
            session: None,
            name: c.name.clone(),
            text: expand_tag(state, tree.pane, None, Tag::Client(*id), format, None),
            align: false,
            children: Vec::new(),
            ..Item::plain_defaults()
        })
        .collect()
}

pub(super) fn lines_of(items: &[Item], tree: &Tree) -> Vec<Line> {
    let mut out = Vec::new();
    add_lines(items, 0, None, tree, &mut out);
    out
}

pub(super) fn add_lines(
    items: &[Item],
    depth: usize,
    parent: Option<usize>,
    tree: &Tree,
    out: &mut Vec<Line>,
) {
    let flat = items.iter().all(|i| i.children.is_empty());
    for (k, item) in items.iter().enumerate() {
        let expanded = tree
            .expanded
            .get(&item.tag)
            .copied()
            .or(item.start_expanded)
            .unwrap_or(true);
        let at = out.len();
        out.push(Line {
            tag: item.tag,
            session: item.session,
            depth,
            last: k + 1 == items.len(),
            flat,
            parent,
            has_children: !item.children.is_empty(),
            expanded,
            name: item.name.clone(),
            text: item.text.clone(),
            align: item.align,
            plain: item.plain,
            no_tag: item.no_tag,
            as_parent: item.as_parent,
        });
        if expanded {
            add_lines(&item.children, depth + 1, Some(at), tree, out);
        }
    }
}

/// `mode_tree_set_height` for the preview `mode`: the list's rows out of
/// the pane's `rows`.
pub(super) fn list_height(rows: usize, lines: usize, mode: PreviewMode) -> usize {
    let mut h = match mode {
        PreviewMode::Off => rows,
        PreviewMode::Normal => {
            let mut h = (rows / 3) * 2;
            if h > lines {
                h = rows / 2;
            }
            if h < 10 {
                h = rows;
            }
            h
        }
        PreviewMode::Big => (rows / 4).min(lines).max(2),
    };
    if rows - h.min(rows) < 2 {
        h = rows;
    }
    h
}

/// `mode_tree_set_current`: the line for `tag`, scrolled into view against
/// the previous `height`; with no such line, `current` clamped.
pub(super) fn set_current(
    lines: &[Line],
    tag: Option<Tag>,
    current: usize,
    offset: usize,
    height: usize,
) -> (usize, usize) {
    let scrolled = |c: usize| {
        if height > 0 && c > height - 1 {
            c - height + 1
        } else {
            0
        }
    };
    if let Some(found) = tag.and_then(|t| lines.iter().position(|l| l.tag == t)) {
        return (found, scrolled(found));
    }
    if current >= lines.len() && !lines.is_empty() {
        let last = lines.len() - 1;
        return (last, scrolled(last));
    }
    (current, offset)
}

/// Whether line `current` is still on the list and still shows `tag`, so
/// the current line and the scroll stay as they are.
pub(super) fn still_current(lines: &[Line], current: usize, tag: Option<Tag>) -> bool {
    lines.get(current).is_some_and(|l| Some(l.tag) == tag)
}

/// The order after `now` in `orders`, wrapping; none when there are none.
pub(super) fn next_order(orders: &[Order], now: Order) -> Option<Order> {
    let i = orders.iter().position(|o| *o == now).map_or(0, |i| i + 1);
    orders.get(i % orders.len().max(1)).copied()
}

/// The line key `(0)`..`(9)`, `(M-a)`..`(M-z)` of line `i`.
pub(super) fn line_key(i: usize) -> Option<String> {
    match i {
        0..=9 => Some(i.to_string()),
        10..=35 => Some(format!("M-{}", char::from(b'a' + (i - 10) as u8))),
        _ => None,
    }
}

/// The tree as it is now: rebuilt from the model, the current line found
/// again by its item (keeping the scroll while it is where it was).
pub(super) fn view(state: &State, tree: &Tree) -> View {
    let (items, no_matches) = build(state, tree);
    let lines = lines_of(&items, tree);
    let rows = state
        .model
        .panes
        .get(&tree.pane)
        .map_or(0, |p| usize::from(p.rows));
    let (current, offset) = if still_current(&lines, tree.current, tree.tag) {
        (tree.current, tree.offset)
    } else {
        set_current(&lines, tree.tag, tree.current, tree.offset, tree.height)
    };
    let height = tree.list_height(rows, lines.len());
    // `mode_tree_check_selected`: a current line below the list scrolls.
    let offset = if height > 0 && current > height - 1 {
        offset.max(current - height + 1)
    } else {
        offset
    };
    let key_format = tree.key_format.as_deref().map(|f| tree.native(f));
    let keys = lines
        .iter()
        .enumerate()
        .map(|(i, l)| match &key_format {
            None => line_key(i),
            Some(fmt) => {
                let text = expand_tag(state, tree.pane, l.session, l.tag, fmt, Some(i));
                crate::server::input::parse_key(text.trim())
                    .ok()
                    .map(|k| k.to_string())
            }
        })
        .collect();
    View {
        lines,
        keys,
        current,
        offset,
        height,
        no_matches,
        rows,
    }
}
