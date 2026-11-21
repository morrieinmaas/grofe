use super::*;

/// Format variables (spec §4.5) for one pane, its window and its session.
pub struct FormatCtx<'a> {
    pub(in crate::server::dispatch::options) model: &'a Model,
    pub(in crate::server::dispatch::options) pane: PaneId,
    pub(in crate::server::dispatch::options) level: Level,
    /// The session the window is seen in, when it is linked into several
    /// (`link-window`): its index, current-ness and session variables are
    /// that session's. `None`: the window's first link.
    pub(in crate::server::dispatch::options) session: Option<SessionId>,
}

/// What a format is expanded for, as tmux's `format_tree` type: the
/// `pane_format`, `window_format` and `session_format` variables say which
/// (a `list-windows -F` is for windows, even though its variables read the
/// window's active pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Session,
    Window,
    Pane,
}

impl<'a> FormatCtx<'a> {
    pub fn new(model: &'a Model, pane: PaneId) -> FormatCtx<'a> {
        FormatCtx::at(model, pane, Level::Pane)
    }

    pub fn at(model: &'a Model, pane: PaneId, level: Level) -> FormatCtx<'a> {
        FormatCtx {
            model,
            pane,
            level,
            session: None,
        }
    }

    /// These variables for the window as `session` sees it.
    pub fn in_session(mut self, session: SessionId) -> FormatCtx<'a> {
        self.session = Some(session);
        self
    }

    /// Names of the sessions whose current window is `window`, a group
    /// once (by its first member), as tmux's `window_active_sessions_list`.
    pub(in crate::server::dispatch::options) fn active_sessions(
        &self,
        window: WindowId,
    ) -> Vec<String> {
        let m = self.model;
        let mut seen: Vec<SessionId> = Vec::new();
        let mut names = Vec::new();
        for s in m.sessions.values().filter(|s| s.active == window) {
            if seen.contains(&s.id) {
                continue;
            }
            seen.push(s.id);
            seen.extend(m.group_peers(s.id));
            names.push(s.name.clone());
        }
        names
    }

    /// These variables for `session`, unless one is set already.
    pub fn or_session(mut self, session: SessionId) -> FormatCtx<'a> {
        self.session = self.session.or(Some(session));
        self
    }

    /// The session the window of `w` is seen in.
    pub(in crate::server::dispatch::options) fn session_for(
        &self,
        w: &crate::server::model::Window,
    ) -> SessionId {
        self.session
            .filter(|s| self.model.has_link(*s, w.id))
            .unwrap_or(w.session)
    }

    pub fn pane_id(&self) -> PaneId {
        self.pane
    }

    /// The options a variable reads: the pane's chain, with the session the
    /// window is seen in (`status-left` of `b:7`, not of the window's first
    /// link).
    pub(in crate::server::dispatch::options) fn option_chain(&self) -> Vec<Owner> {
        let mut chain = chain(self.model, self.pane);
        if let Some(w) = self
            .model
            .panes
            .get(&self.pane)
            .and_then(|p| self.model.windows.get(&p.window))
            && let Some(Owner::Session(s)) = chain.last_mut()
        {
            *s = self.session_for(w);
        }
        chain
    }
}

/// One thing a session, window or pane loop visits.
pub(crate) struct LoopSpec {
    /// The pane its formats read: the active pane of a session's current
    /// window or of a window, else the pane itself.
    pub pane: PaneId,
    pub level: Level,
    pub active: bool,
    pub extras: Vec<(String, String)>,
    /// The session the item is seen in.
    pub session: Option<SessionId>,
}

/// What `#{S:}`, `#{W:}` and `#{P:}` visit from `pane`'s point of view,
/// sorted as tmux sorts them (`sort.c`): every session; the windows of
/// `pane`'s session; the panes of its window. `client_session` is the
/// session of the client the formats are for, which `#{S:}`'s second
/// branch is for. `None` for the clients loop (the server's) and a pane
/// that is gone.
pub(crate) fn loop_specs(
    model: &Model,
    pane: PaneId,
    in_session: Option<SessionId>,
    client_session: Option<SessionId>,
    kind: LoopKind,
    sort: LoopSort,
) -> Option<Vec<LoopSpec>> {
    let p = model.panes.get(&pane)?;
    let w = model.windows.get(&p.window)?;
    let s = in_session
        .filter(|s| model.has_link(*s, w.id))
        .unwrap_or(w.session);
    let s = model.sessions.get(&s)?;
    let by_name = |a: &str, b: &str| a.as_bytes().cmp(b.as_bytes());
    match kind {
        LoopKind::Sessions => {
            // tmux keeps its sessions in a tree by name.
            let mut all: Vec<&crate::server::model::Session> = model.sessions.values().collect();
            all.sort_by(|a, b| by_name(&a.name, &b.name));
            sort.apply(&mut all, |order, a, b| {
                let first = match order {
                    SortOrder::Index => a.id.cmp(&b.id),
                    SortOrder::Activity => b.activity.cmp(&a.activity),
                    SortOrder::Name => by_name(&a.name, &b.name),
                    _ => std::cmp::Ordering::Equal,
                };
                first.then_with(|| by_name(&a.name, &b.name))
            });
            Some(
                all.into_iter()
                    .filter_map(|o| {
                        let pane = model.windows.get(&o.active)?.active;
                        Some(LoopSpec {
                            pane,
                            level: Level::Session,
                            active: client_session == Some(o.id),
                            extras: Vec::new(),
                            session: Some(o.id),
                        })
                    })
                    .collect(),
            )
        }
        LoopKind::Windows => {
            let mut all: Vec<&crate::server::model::Window> = s
                .windows
                .iter()
                .filter_map(|id| model.windows.get(id))
                .collect();
            sort.apply(&mut all, |order, a, b| {
                let first = match order {
                    SortOrder::Name => by_name(&a.name, &b.name),
                    SortOrder::Activity => b.activity.cmp(&a.activity),
                    _ => std::cmp::Ordering::Equal,
                };
                first.then_with(|| by_name(&a.name, &b.name))
            });
            let n = all.len();
            let flag = |on: bool| if on { "1" } else { "0" }.to_string();
            Some(
                (0..n)
                    .map(|i| {
                        let this = all[i];
                        let mut extras = vec![
                            (
                                "window_after_active".to_string(),
                                flag(i > 0 && all[i - 1].id == s.active),
                            ),
                            (
                                "window_before_active".to_string(),
                                flag(i + 1 < n && all[i + 1].id == s.active),
                            ),
                        ];
                        for (prefix, other) in [
                            ("next", all.get(i + 1)),
                            ("prev", i.checked_sub(1).and_then(|j| all.get(j))),
                        ] {
                            let Some(other) = other else { continue };
                            let index = model.index_in(s.id, other.id).unwrap_or(other.index);
                            extras.push((format!("{prefix}_window_index"), index.to_string()));
                            extras.push((
                                format!("{prefix}_window_active"),
                                flag(other.id == s.active),
                            ));
                            for (name, entry) in model.options.at(Owner::Window(other.id)) {
                                if name.starts_with('@') {
                                    extras.push((format!("{prefix}_{name}"), entry.value.clone()));
                                }
                            }
                        }
                        LoopSpec {
                            pane: this.active,
                            level: Level::Window,
                            active: this.id == s.active,
                            extras,
                            session: Some(s.id),
                        }
                    })
                    .collect(),
            )
        }
        LoopKind::Panes => {
            // The sort is by pane id, whatever flags a `P` modifier has;
            // `r` reverses it.
            let mut all = w.pane_ids();
            sort.apply(&mut all, |_, a, b| a.cmp(b));
            Some(
                all.into_iter()
                    .map(|id| LoopSpec {
                        pane: id,
                        level: Level::Pane,
                        active: id == w.active,
                        extras: Vec::new(),
                        session: Some(s.id),
                    })
                    .collect(),
            )
        }
        LoopKind::Clients => None,
    }
}

pub(in crate::server::dispatch::options) fn flag(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// Seconds since the epoch, as tmux prints a time variable.
pub(in crate::server::dispatch::options) fn epoch_secs(t: std::time::SystemTime) -> String {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .to_string()
}

/// tmux's `window_layout`: the layout string of the window's tiled and
/// floating panes (`layout::tmux_layout`).
pub(crate) fn window_layout(w: &crate::server::model::Window) -> String {
    use crate::server::layout::{FloatingCell, tmux_layout};
    let floating: Vec<FloatingCell> = w
        .floating
        .iter()
        .map(|f| FloatingCell {
            pane: f.pane,
            x: f.x,
            y: f.y,
            cols: f.cols,
            rows: f.rows,
            z: f.z,
        })
        .collect();
    tmux_layout(&w.layout.export(w.area()), &floating, !w.zoomed)
}

/// tmux's `window_visible_layout`: the layout, or for a zoomed window its
/// one zoomed pane over the whole window.
pub(in crate::server::dispatch::options) fn window_visible_layout(
    w: &crate::server::model::Window,
) -> String {
    use crate::server::layout::{GeometryCell, LayoutGeometry, tmux_layout};
    if !w.zoomed {
        return window_layout(w);
    }
    let area = w.area();
    let zoomed = LayoutGeometry {
        x: area.x,
        y: area.y,
        cols: area.cols,
        rows: area.rows,
        cell: GeometryCell::Pane(w.active),
    };
    tmux_layout(&zoomed, &[], true)
}

/// tmux's `window_flags`: `#` activity (written `##`, as tmux escapes it
/// there), `!` bell, `~` silence, `*` current, `-` last, `M` holds the
/// marked pane, `Z` zoomed, in tmux's order.
pub(in crate::server::dispatch::options) fn window_flags(
    m: &Model,
    s: &crate::server::model::Session,
    w: &crate::server::model::Window,
) -> String {
    let marked = m
        .marked_pane()
        .and(m.marked)
        .is_some_and(|(session, window, _)| session == s.id && window == w.id);
    let activity = if w.activity_flag { "##" } else { "" };
    let rest: String = [
        (w.bell_flag, '!'),
        (w.silence_flag, '~'),
        (s.active == w.id, '*'),
        (s.last_windows.last() == Some(&w.id), '-'),
        (marked, 'M'),
        (w.zoomed, 'Z'),
    ]
    .into_iter()
    .filter_map(|(on, c)| on.then_some(c))
    .collect();
    format!("{activity}{rest}")
}

/// A pane's top-left cell in its window: the layout's, or the window's
/// origin for a zoomed window's active pane.
pub(in crate::server::dispatch::options) fn pane_origin(
    w: &crate::server::model::Window,
    pane: PaneId,
) -> Option<(u16, u16)> {
    if w.zoomed && w.active == pane {
        let r = w.zoom_rect();
        return Some((r.x, r.y));
    }
    w.tiled_rects()
        .into_iter()
        .find(|(id, _)| *id == pane)
        .map(|(_, r)| (r.x, r.y))
}
