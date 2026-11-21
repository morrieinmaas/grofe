use super::*;

/// `caps` with what the `terminal-features` option adds for a client whose
/// `TERM` is `term`, as tmux reads it: entries `pattern:feature:...`, the
/// pattern a `fnmatch` glob of the terminal name. Only `sixel` is read (G3);
/// the probe cannot ask for it on a terminal that lists it nowhere else.
pub(super) fn terminal_features(model: &Model, term: &str, mut caps: Caps) -> Caps {
    let features = model.options.lookup_list("terminal-features", &[]);
    for entry in features.into_iter().flat_map(|f| f.values()) {
        let mut parts = entry.split(':');
        let matches = parts
            .next()
            .is_some_and(|pattern| fnmatch(pattern.as_bytes(), term.as_bytes()));
        if matches && parts.any(|feature| feature == "sixel") {
            caps.sixel = true;
        }
    }
    caps
}

/// Client `c`'s current window, and whether it fits the client (then
/// there is nothing to pan).
pub(crate) fn window_fits(
    state: &State,
    id: ClientId,
    c: &ClientEntry,
) -> Option<(WindowId, bool)> {
    let model = &state.model;
    let w = model.windows.get(&model.sessions.get(&c.session)?.active)?;
    let (cols, rows) = visible_size(state, id, c);
    Some((w.id, w.cols <= cols && w.rows <= rows))
}

/// The part of client `c` that shows the window: its size less the
/// status rows and the columns the agent sidebar holds.
pub(super) fn visible_size(state: &State, id: ClientId, c: &ClientEntry) -> (u16, u16) {
    let lines = status_geometry(&state.model, c.session, c.size.1).lines;
    let reserved = crate::server::sidebar::reserved(state, id, c.size.0);
    (
        c.size.0.saturating_sub(reserved),
        c.size.1.saturating_sub(lines),
    )
}

/// The agent sidebar was shown or hidden for `client`: its window changes
/// size, so size the windows again (the normal resize path, which gives the
/// panes their new size) and redraw every client.
pub(crate) fn resize_for_sidebar(state: &mut State, _client: ClientId) {
    changed(state);
}

/// `refresh-client -c/-L/-R/-U/-D` (TC.9): moves the visible part of a
/// window larger than client `id` by `by` cells, or with `Reset` goes
/// back to following the cursor, as tmux 3.7b's `cmd_refresh_client_exec`.
pub(crate) fn pan(state: &mut State, id: ClientId, pan: cmd::Pan, by: u16) {
    let model = &state.model;
    let Some(c) = state.clients.get(&id) else {
        return;
    };
    let Some(w) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
    else {
        return;
    };
    let (cols, rows) = visible_size(state, id, c);
    let (wid, wcols, wrows) = (w.id, w.cols, w.rows);
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    if pan == cmd::Pan::Reset {
        // tmux 3.7b recomputes the offset from the cursor, which is 0 while
        // the cursor is within the first screenful (recorded).
        // ponytail: past that tmux centres the cursor; `follow` scrolls
        // only as far as it must.
        c.pan = None;
        c.viewport = (0, 0);
    } else {
        let (_, mut x, mut y) = match c.pan {
            Some(p) if p.0 == wid => p,
            _ => (wid, c.viewport.0, c.viewport.1),
        };
        match pan {
            cmd::Pan::Left => x = x.saturating_sub(by),
            cmd::Pan::Right => x = x.saturating_add(by).min(wcols.saturating_sub(cols)),
            cmd::Pan::Up => y = y.saturating_sub(by),
            cmd::Pan::Down => y = y.saturating_add(by).min(wrows.saturating_sub(rows)),
            cmd::Pan::Reset => {}
        }
        c.pan = Some((wid, x, y));
    }
    c.dirty = true;
    request_render(state, Instant::now());
}

/// How many status rows `session` shows: `status` is `off`, `on` or 2 to
/// 5 (spec §4.10).
pub(super) fn status_lines(model: &Model, session: SessionId) -> u16 {
    match model.options.lookup("status", &[Owner::Session(session)]) {
        Some("off") | None => 0,
        Some("on") => 1,
        Some(n) => n.parse().unwrap_or(1),
    }
}

/// Where a client's status rows are, for a client `rows` tall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusGeometry {
    /// How many rows the status line takes (never more than the client).
    pub lines: u16,
    /// `status-position top`: the rows are above the window.
    pub top: bool,
    /// The client row of the first status row, where a message or prompt
    /// goes (tmux's `message-line 0`).
    pub(super) first_row: u16,
    /// The client row the window's top row is drawn on.
    pub(super) window_top: u16,
}

pub(crate) fn status_geometry(model: &Model, session: SessionId, rows: u16) -> StatusGeometry {
    let lines = status_lines(model, session).min(rows);
    let top = model
        .options
        .lookup("status-position", &[Owner::Session(session)])
        == Some("top");
    StatusGeometry {
        lines,
        top,
        first_row: if top { 0 } else { rows - lines },
        window_top: if top { lines } else { 0 },
    }
}

/// The window size the size-owning `client` of `session` wants: its terminal
/// less the status rows and the sidebar's columns, never below 1x1.
pub(super) fn window_size(state: &State, client: ClientId, session: SessionId) -> (u16, u16) {
    let (cols, rows) = state.clients.get(&client).map_or((1, 1), |c| c.size);
    let status = status_lines(&state.model, session);
    let reserved = crate::server::sidebar::reserved(state, client, cols);
    (
        cols.saturating_sub(reserved).max(1),
        rows.saturating_sub(status).max(1),
    )
}

/// One cell's size in pixels on the client `session`'s windows are sized
/// for, if it said; what a new pane there starts with.
pub(crate) fn cell_px(state: &State, session: SessionId) -> Option<(u16, u16)> {
    let owner = state.registry.owners.get(&session)?;
    state.clients.get(&owner.client)?.caps.cell_px
}

/// Sizes every session that has attached clients to its owner (spec D6),
/// in cells and, where the owner said its cell size, in pixels. A session
/// no client shows keeps what it had (0 pixels until a client says).
pub(super) fn apply_sizes(state: &mut State) {
    let sizes: Vec<(SessionId, (u16, u16), Instant)> = state
        .registry
        .owners
        .iter()
        .filter_map(|(s, o)| {
            let c = state.clients.get(&o.client)?;
            Some((*s, window_size(state, o.client, *s), c.activity))
        })
        .collect();
    for &(session, size, activity) in &sizes {
        // A window shown to several sessions with a client follows the
        // client that was active last (`window-size latest`), so two
        // clients of different sizes do not make it flap.
        let newer =
            |other: &&(SessionId, (u16, u16), Instant)| (other.2, other.0) > (activity, session);
        let yielded: Vec<WindowId> = state
            .model
            .sessions
            .get(&session)
            .map(|s| s.windows.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|w| {
                sizes
                    .iter()
                    .filter(|o| newer(o))
                    .any(|o| state.model.has_link(o.0, *w))
            })
            .collect();
        resize_session(&mut state.model, session, size, &yielded);
        let cell_px = cell_px(state, session);
        let model = &state.model;
        let windows = model.sessions.get(&session).map(|s| &s.windows);
        for w in windows.into_iter().flatten() {
            let panes = model.panes.values().filter(|p| p.window == *w);
            for p in panes {
                if let Err(e) = p.handle.set_cell_px(cell_px) {
                    eprintln!("grofe server: resizing {}: {e}", p.id);
                }
            }
        }
    }
}

/// Gives `session` and all its windows `(cols, rows)`, laying out again
/// (a zoomed window's active pane keeps filling it).
pub(super) fn resize_session(
    model: &mut Model,
    session: SessionId,
    (cols, rows): (u16, u16),
    yielded: &[WindowId],
) {
    let Some(s) = model.sessions.get_mut(&session) else {
        return;
    };
    s.cols = cols;
    s.rows = rows;
    for id in s.windows.clone() {
        if yielded.contains(&id) {
            continue;
        }
        // A window `resize-window` sized (`window-size manual`) keeps it.
        let manual = model.windows.get(&id).is_some_and(|w| {
            model
                .options
                .lookup("window-size", &options::chain(model, w.active))
                == Some("manual")
        });
        let Some(w) = model.windows.get_mut(&id) else {
            continue;
        };
        if manual || (w.cols, w.rows) == (cols, rows) {
            continue;
        }
        w.cols = cols;
        w.rows = rows;
        model.relayout(id);
        fit_zoomed(model, id);
    }
}

/// A zoomed window's active pane fills the window, less the row a border
/// status takes.
pub(super) fn fit_zoomed(model: &mut Model, window: WindowId) {
    let Some(w) = model.windows.get(&window).filter(|w| w.zoomed) else {
        return;
    };
    let (active, zoom) = (w.active, w.zoom_rect());
    if let Some(p) = model.panes.get_mut(&active) {
        p.cols = zoom.cols;
        p.rows = zoom.rows;
        if let Err(e) = p.handle.resize(zoom.cols, zoom.rows) {
            eprintln!("grofe server: resizing {active}: {e}");
        }
    }
}

/// Gives every window the `pane-border-status` its options now say, laying
/// it out again where that changed: the panes along its top or bottom edge
/// give up a row to their title.
pub(super) fn sync_pane_status(model: &mut Model) {
    let wanted: Vec<(WindowId, PaneStatus)> = model
        .windows
        .values()
        .filter_map(|w| {
            let chain = options::chain(model, w.active);
            let value = model.options.lookup("pane-border-status", &chain)?;
            Some((w.id, PaneStatus::parse(value)))
        })
        .filter(|(id, status)| model.windows[id].pane_status != *status)
        .collect();
    for (id, status) in wanted {
        if let Some(w) = model.windows.get_mut(&id) {
            w.pane_status = status;
        }
        model.relayout(id);
        fit_zoomed(model, id);
    }
}

/// Moves a viewport of `visible` cells over a `window`, along one axis at
/// a time, as little as needed to keep `cursor` inside (spec D6 crop). A
/// window that fits has viewport 0.
pub fn follow(
    viewport: (u16, u16),
    cursor: Option<(u16, u16)>,
    window: (u16, u16),
    visible: (u16, u16),
) -> (u16, u16) {
    let axis = |v: u16, c: Option<u16>, win: u16, vis: u16| {
        if win <= vis {
            return 0;
        }
        let max = win - vis;
        let mut v = v.min(max);
        if let Some(c) = c {
            if c < v {
                v = c;
            } else if c >= v + vis {
                v = c + 1 - vis;
            }
        }
        v.min(max)
    };
    (
        axis(viewport.0, cursor.map(|c| c.0), window.0, visible.0),
        axis(viewport.1, cursor.map(|c| c.1), window.1, visible.1),
    )
}

/// The visible panes of `w` and their rects in window coordinates (only
/// the active pane when zoomed).
pub(crate) fn visible_panes(w: &crate::server::model::Window) -> Vec<(PaneId, Rect)> {
    w.pane_rects()
}
