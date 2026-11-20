use super::*;

/// tmux's default `display-message` text.
pub(super) const DEFAULT_MESSAGE: &str = "[#{session_name}] #{window_index}:#{window_name}, current pane #{pane_index} - (%H:%M %d-%b-%y)";

/// `display-message`: the text expanded against the target pane and the
/// current client (the one whose binding ran it, else the most recently
/// active one, as tmux's `cmd_find_best_client`), so `#{client_width}`
/// and the other `client_*` variables work. With `-p` it is printed;
/// without, it goes on that client's status line (`clients::
/// display_message`, which logs it for `show-messages`), and with no
/// client attached nothing happens, as in tmux 3.7b.
pub(super) fn display_message(
    state: &mut State,
    ctx: &ClientCtx,
    c: DisplayMessage,
) -> Result<String, String> {
    // `-d` is checked first, as tmux 3.7b's `args_strtonum` (recorded).
    let delay = match c.delay.as_deref().map(str::parse::<i128>) {
        None => None,
        Some(Ok(n)) if n < 0 => return Err("delay too small".to_string()),
        Some(Ok(n)) if n > i128::from(u32::MAX) => return Err("delay too large".to_string()),
        Some(Ok(n)) => Some(std::time::Duration::from_millis(
            u64::try_from(n).unwrap_or(0),
        )),
        Some(Err(_)) => return Err("delay invalid".to_string()),
    };
    let model = &state.model;
    let text = c.text.as_deref().unwrap_or(DEFAULT_MESSAGE);
    // A tmux client's `-v` text is left in tmux's language by the front end.
    let tmux = ctx.dialect == crate::protocol::Dialect::Tmux;
    // tmux's target here can fail (`CMD_FIND_CANFAIL`): a `~` with nothing
    // marked is quietly no pane, and every variable expands to nothing.
    let pane = match model.resolve_pane(&c.target, ctx) {
        Ok(pane) => pane,
        Err(_) if c.target == Target::Session(SessionRef::Marked) => {
            let (out, trace) = render_message(&c, tmux, text, &NoVars);
            return Ok(if c.all {
                out
            } else if c.print {
                format!("{trace}{out}\n")
            } else {
                trace
            });
        }
        Err(e) => return Err(e),
    };
    // The session the target names, for a window linked into several.
    let vars = match model.resolve_winlink(&c.target, ctx) {
        Ok((session, _)) => FormatCtx::new(model, pane).in_session(session),
        Err(_) => FormatCtx::new(model, pane),
    };
    // `-c` names the client for the message; one that is not there shows
    // nothing, as tmux's `CMD_CLIENT_CANFAIL`. Formats see that client if
    // it shows the pane's session, else the current one.
    let current = match &c.client {
        Some(name) => clients::client_named(state, name),
        None => clients::current_client(state, ctx),
    };
    let session = model
        .resolve_winlink(&c.target, ctx)
        .ok()
        .map(|(s, _)| s)
        .or_else(|| {
            let w = model.windows.get(&model.panes.get(&pane)?.window)?;
            Some(w.session)
        });
    let for_formats = current
        .filter(|id| state.clients.get(id).map(|e| e.session) == session)
        .or_else(|| clients::current_client(state, ctx));
    let (out, trace) = match for_formats.and_then(|id| state.clients.get(&id)) {
        Some(client) => {
            let client = clients::ClientVars::new(model, client, Some(vars));
            render_message(&c, tmux, text, &ServerVars::fresh(state, pane, &client))
        }
        None => render_message(&c, tmux, text, &ServerVars::fresh(state, pane, &vars)),
    };
    // `-a` lists the variables and shows no message.
    if c.all {
        return Ok(out);
    }
    if c.print {
        return Ok(format!("{trace}{out}\n"));
    }
    if let Some(id) = current {
        clients::display_message(
            state,
            id,
            &out,
            delay,
            c.ignore_keys,
            crate::server::render::Level::Info,
        );
    }
    Ok(trace)
}

/// What `display-message` shows for `text`, and the lines `-v` prints
/// before it: `-a` lists every variable instead, and `-l` shows the text
/// as it is.
pub(super) fn render_message(
    c: &DisplayMessage,
    tmux: bool,
    text: &str,
    vars: &dyn FormatVars,
) -> (String, String) {
    if c.all {
        return (crate::cmd::all_variables(vars), String::new());
    }
    if c.literal {
        return (text.to_string(), String::new());
    }
    if c.verbose {
        let (out, trace) = crate::cmd::expand_traced(text, vars, true, tmux);
        let lines: String = trace.iter().map(|l| format!("{l}\n")).collect();
        return (out, lines);
    }
    (expand_time(text, vars), String::new())
}

pub(super) fn lines(
    state: &State,
    fmt: &str,
    level: Level,
    panes: impl Iterator<Item = (Option<SessionId>, PaneId)>,
) -> String {
    panes
        .map(|(s, p)| {
            let vars = in_session(FormatCtx::at(&state.model, p, level), s);
            format!("{}\n", expand(fmt, &ServerVars::fresh(state, p, &vars)))
        })
        .collect()
}

pub(super) fn in_session(vars: FormatCtx<'_>, session: Option<SessionId>) -> FormatCtx<'_> {
    match session {
        Some(s) => vars.in_session(s),
        None => vars,
    }
}

/// The pane a window's formats expand against: its active pane.
pub(super) fn active_pane(model: &Model, window: WindowId) -> Option<PaneId> {
    model.windows.get(&window).map(|w| w.active)
}

// tmux 3.7b's default `list-*` templates (`cmd-list-*.c`), in tmux's
// format language (`compat::tmux::formats::to_native` rewrites them as a
// tmux `-F`): the output a tmux-dialect `list-*` with no `-F` gives (FE.2).

pub(crate) const TMUX_LIST_SESSIONS: &str = "#{session_name}: #{session_windows} windows \
     (created #{t:session_created})#{?session_grouped, (group ,}\
     #{session_group}#{?session_grouped,),}#{?session_attached, (attached),}";

pub(super) const TMUX_LIST_WINDOWS: &str = "#{window_index}: #{window_name}#{window_raw_flags} \
     (#{window_panes} panes) [#{window_width}x#{window_height}] \
     [layout #{window_layout}] #{window_id}#{?window_active, (active),}";

pub(super) const TMUX_LIST_WINDOWS_ALL: &str = "#{session_name}:#{window_index}: \
     #{window_name}#{window_raw_flags} (#{window_panes} panes) \
     [#{window_width}x#{window_height}] ";

/// `list-panes`' template after its line's own prefix.
pub(super) const TMUX_LIST_PANES: &str = "[#{pane_width}x#{pane_height}\
     #{?pane_floating_flag, #{pane_x}#,#{pane_y}#,#{pane_z}}] [history \
     #{history_size}/#{history_limit}, #{history_bytes} bytes] #{pane_id}\
     #{?pane_active, (active),}#{?pane_dead, (dead),}";

pub(crate) const TMUX_LIST_CLIENTS: &str = "#{client_name}: #{session_name} \
     [#{client_width}x#{client_height} #{client_termname}] \
     #{?#{!=:#{client_uid},#{uid}},[user #{?client_user,#{client_user},#{client_uid},}] ,}\
     #{?client_flags,(,}#{client_flags}#{?client_flags,),}";

/// A tmux template in grofe's format language.
pub(crate) fn tmux_template(template: &str) -> String {
    crate::compat::tmux::formats::to_native(template, crate::compat::tmux::formats::Jobs::Empty)
        .value
}

/// A `list-*` command's format: its `-F`, else tmux's default template
/// (only a tmux-dialect request with no `-F` gets here, [`super::super::route`]).
pub(super) fn list_format(format: Option<String>, template: impl FnOnce() -> String) -> String {
    format.unwrap_or_else(|| tmux_template(&template()))
}

pub(super) fn list_sessions(state: &State, c: ListSessions) -> Result<String, String> {
    let fmt = list_format(c.format.clone(), || TMUX_LIST_SESSIONS.to_string());
    let sessions = listed_sessions(state, &c)?;
    let panes = sessions
        .into_iter()
        .filter_map(|s| Some((Some(s), session_pane(&state.model, s)?)));
    Ok(lines(state, &fmt, Level::Session, panes))
}

pub(super) fn list_windows(
    state: &State,
    ctx: &ClientCtx,
    c: ListWindows,
) -> Result<String, String> {
    let fmt = list_format(c.format.clone(), || match c.all {
        true => TMUX_LIST_WINDOWS_ALL.to_string(),
        false => TMUX_LIST_WINDOWS.to_string(),
    });
    let windows = listed_windows(state, ctx, &c)?;
    let panes = windows
        .into_iter()
        .filter_map(|(s, w)| Some((Some(s), active_pane(&state.model, w)?)));
    Ok(lines(state, &fmt, Level::Window, panes))
}

pub(super) fn list_panes(state: &State, ctx: &ClientCtx, c: ListPanes) -> Result<String, String> {
    // `-a` lines start with the session and window, `-s` lines with the
    // window, others with the pane index alone.
    let fmt = list_format(c.format.clone(), || {
        let prefix = if c.all {
            "#{session_name}:#{window_index}.#{pane_index}: "
        } else if c.session {
            "#{window_index}.#{pane_index}: "
        } else {
            "#{pane_index}: "
        };
        format!("{prefix}{TMUX_LIST_PANES}")
    });
    let panes = listed_panes(state, ctx, &c)?;
    let panes = panes.into_iter().map(|(s, p)| (Some(s), p));
    Ok(lines(state, &fmt, Level::Pane, panes))
}

/// Whether a list's `-f` filter keeps the line of `pane` (tmux's
/// `format_true`: not empty and not `0`); no filter keeps every line.
pub(super) fn kept(
    state: &State,
    filter: Option<&str>,
    level: Level,
    session: Option<SessionId>,
    pane: PaneId,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let vars = in_session(FormatCtx::at(&state.model, pane, level), session);
    let got = expand(filter, &ServerVars::new(state, pane, &vars));
    !got.is_empty() && got != "0"
}

/// The pane a session's formats expand against: its current window's
/// active pane.
pub(super) fn session_pane(model: &Model, session: SessionId) -> Option<PaneId> {
    model
        .sessions
        .get(&session)
        .and_then(|s| active_pane(model, s.active))
}

/// Every session, by name as tmux keeps them.
pub(super) fn sessions_by_name(model: &Model) -> Vec<SessionId> {
    let mut sessions: Vec<_> = model.sessions.values().map(|s| (&s.name, s.id)).collect();
    sessions.sort();
    sessions.into_iter().map(|(_, id)| id).collect()
}

/// The sessions `list-sessions` lists: all of them, by name or in the order
/// `-O` asks for, that `-f` keeps.
pub(crate) fn listed_sessions(state: &State, c: &ListSessions) -> Result<Vec<SessionId>, String> {
    let sort = Sort::new(&c.sort)?;
    Ok(sessions_sorted(&state.model, &sort)
        .into_iter()
        .filter(|s| {
            session_pane(&state.model, *s)
                .is_some_and(|p| kept(state, c.filter.as_deref(), Level::Session, Some(*s), p))
        })
        .collect())
}

/// Every session, by name, or in the order `sort` asks for.
pub(crate) fn sessions_sorted(model: &Model, sort: &Sort) -> Vec<SessionId> {
    let mut all = sessions_by_name(model);
    sort.apply(&mut all, |order, a, b| {
        let (sa, sb) = (&model.sessions[a], &model.sessions[b]);
        let first = match order {
            Order::Index => sa.id.cmp(&sb.id),
            Order::Creation => sa.created.cmp(&sb.created),
            Order::Activity => sb.activity.cmp(&sa.activity),
            Order::Name => by_name(&sa.name, &sb.name),
            _ => Ordering::Equal,
        };
        first.then_with(|| by_name(&sa.name, &sb.name))
    });
    all
}

/// How tmux's `sort_winlink_cmp` compares two windows of `model` as the
/// sessions `a` and `b` have them.
pub(super) fn compare_windows(
    model: &Model,
    order: Order,
    a: (SessionId, WindowId),
    b: (SessionId, WindowId),
) -> Ordering {
    let (wa, wb) = (&model.windows[&a.1], &model.windows[&b.1]);
    let first = match order {
        Order::Index => {
            let index = |(s, w): (SessionId, WindowId)| {
                model.index_in(s, w).unwrap_or(model.windows[&w].index)
            };
            index(a).cmp(&index(b))
        }
        Order::Creation => a.1.cmp(&b.1),
        Order::Activity => wb.activity.cmp(&wa.activity),
        Order::Name => by_name(&wa.name, &wb.name),
        Order::Size => (u32::from(wa.cols) * u32::from(wa.rows))
            .cmp(&(u32::from(wb.cols) * u32::from(wb.rows))),
        _ => Ordering::Equal,
    };
    first.then_with(|| by_name(&wa.name, &wb.name))
}

/// The windows `list-windows` lists: the target session's, or with `-a`
/// every session's (by name), in index order or the order `-O` asks for,
/// that `-f` keeps. Each comes with the session it is listed in.
pub(crate) fn listed_windows(
    state: &State,
    ctx: &ClientCtx,
    c: &ListWindows,
) -> Result<Vec<(SessionId, WindowId)>, String> {
    let sort = Sort::new(&c.sort)?;
    let model = &state.model;
    let sessions = match c.all {
        true => sessions_by_name(model),
        false => vec![model.resolve_session(&c.target, ctx)?],
    };
    let mut all: Vec<(SessionId, WindowId)> = sessions
        .iter()
        .flat_map(|s| model.sessions[s].windows.iter().map(|w| (*s, *w)))
        .collect();
    sort.apply(&mut all, |order, a, b| {
        compare_windows(model, order, *a, *b)
    });
    Ok(all
        .into_iter()
        .filter(|(s, w)| {
            active_pane(model, *w)
                .is_some_and(|p| kept(state, c.filter.as_deref(), Level::Window, Some(*s), p))
        })
        .collect())
}

/// How tmux's `sort_pane_cmp` compares two panes of one window.
pub(super) fn compare_panes(state: &State, order: Order, a: PaneId, b: PaneId) -> Ordering {
    let model = &state.model;
    let title = |p: PaneId| state.passthrough.title(p);
    let Some(window) = model
        .panes
        .get(&a)
        .and_then(|p| model.windows.get(&p.window))
    else {
        return Ordering::Equal;
    };
    let size = |p: PaneId| {
        model
            .panes
            .get(&p)
            .map_or(0, |p| u32::from(p.cols) * u32::from(p.rows))
    };
    let ids = window.pane_ids();
    let index = |p: PaneId| ids.iter().position(|i| *i == p).unwrap_or(0);
    let first = match order {
        Order::Activity => window
            .active_points
            .get(a)
            .cmp(&window.active_points.get(b)),
        Order::Creation => a.cmp(&b),
        Order::Size => size(a).cmp(&size(b)),
        Order::Index => index(a).cmp(&index(b)),
        Order::Name => by_name(&title(a), &title(b)),
        // tmux's `z_index` list, the top pane first: the floating panes by
        // height, then the tiled ones, the last in pane order first.
        Order::Z => {
            let z = |p: PaneId| match window.floating(p) {
                Some(f) => (0u8, -i64::from(f.z)),
                None => (1u8, -(index(p) as i64)),
            };
            z(a).cmp(&z(b))
        }
        _ => Ordering::Equal,
    };
    first.then_with(|| by_name(&title(a), &title(b)))
}

/// The panes `list-panes` lists: the target window's, with `-s` its
/// session's, with `-a` every session's, that `-f` keeps; each window's
/// panes in pane order or the order `-O` asks for. Each comes with the
/// session it is listed in.
pub(crate) fn listed_panes(
    state: &State,
    ctx: &ClientCtx,
    c: &ListPanes,
) -> Result<Vec<(SessionId, PaneId)>, String> {
    // The order is checked once; each window is sorted on its own.
    Sort::new(&c.sort)?;
    let sort = Sort::lenient(&c.sort);
    let model = &state.model;
    let windows: Vec<(SessionId, WindowId)> = if c.all {
        sessions_by_name(model)
            .iter()
            .flat_map(|s| model.sessions[s].windows.iter().map(|w| (*s, *w)))
            .collect()
    } else if c.session {
        // tmux finds `-t` as a window, then takes its session.
        let (s, _) = model.resolve_winlink(&c.target, ctx)?;
        model.sessions[&s].windows.iter().map(|w| (s, *w)).collect()
    } else {
        let (s, w) = model.resolve_winlink(&c.target, ctx)?;
        vec![(s, w)]
    };
    let mut out = Vec::new();
    for (s, w) in windows {
        let Some(window) = model.windows.get(&w) else {
            continue;
        };
        let mut panes = window.pane_ids();
        sort.apply(&mut panes, |order, a, b| {
            compare_panes(state, order, *a, *b)
        });
        out.extend(
            panes
                .into_iter()
                .filter(|p| kept(state, c.filter.as_deref(), Level::Pane, Some(s), *p))
                .map(|p| (s, p)),
        );
    }
    Ok(out)
}

/// Format variables (spec §4.5) for one pane, its window and its session.
pub struct FormatCtx<'a> {
    pub(super) model: &'a Model,
    pub(super) pane: PaneId,
    pub(super) level: Level,
    /// The session the window is seen in, when it is linked into several
    /// (`link-window`): its index, current-ness and session variables are
    /// that session's. `None`: the window's first link.
    pub(super) session: Option<SessionId>,
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
    pub(super) fn active_sessions(&self, window: WindowId) -> Vec<String> {
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
    pub(super) fn session_for(&self, w: &crate::server::model::Window) -> SessionId {
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
    pub(super) fn option_chain(&self) -> Vec<Owner> {
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

pub(super) fn flag(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// Format variables that need the server state, not only the model
/// (`socket_path`, `pane_title`, `pane_tty`, TC.11's `host` and
/// `host_short`, the mouse's, and a running hook's `#{hook*}`), over
/// `inner` for the rest.
/// Built where dispatch holds `&State`, the way `ClientVars` layers the
/// client's variables.
pub struct ServerVars<'a> {
    pub(super) state: &'a State,
    pub(super) pane: PaneId,
    pub(super) inner: Inner<'a>,
    /// Read the foreground process now, not from the last sample.
    pub(super) fresh: bool,
    /// What a fresh expansion has read so far: each is read at most once
    /// per expansion, and the members only when a format asks for them.
    pub(super) group: OnceCell<Option<i32>>,
    pub(super) leader: OnceCell<ProcSample>,
    pub(super) members: OnceCell<Vec<Vec<String>>>,
}

impl<'a> ServerVars<'a> {
    /// The status line's and hooks' form: the foreground process comes
    /// from the pane's 1 s sample, so a redraw costs no syscalls for it.
    pub fn new(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars {
            state,
            pane,
            inner: Inner::Ref(inner),
            fresh: false,
            group: OnceCell::new(),
            leader: OnceCell::new(),
            members: OnceCell::new(),
        }
    }

    /// A command's form (a binding's conditional, `display-message`, `set
    /// -F`, `list-* -F`): `pane_current_command`, `pane_current_argv` and
    /// `pane_foreground_argv` read the pane's foreground process group at
    /// this moment, with syscalls and no fork, so a key pressed just after
    /// a program starts or exits sees the one in front.
    pub fn fresh(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars {
            fresh: true,
            ..ServerVars::new(state, pane, inner)
        }
    }

    pub(super) fn foreground(&self, name: &str) -> Option<String> {
        let group = *self.group.get_or_init(|| {
            let p = self.state.model.panes.get(&self.pane)?;
            p.handle.foreground_group()
        });
        let group = group?;
        let leader = || self.leader.get_or_init(|| procinfo::leader(group));
        match name {
            "pane_current_command" => leader().command.clone(),
            "pane_current_argv" => leader().argv.as_ref().map(|a| a.join(" ")),
            _ => {
                let members = self.members.get_or_init(|| procinfo::members(group));
                (!members.is_empty()).then(|| procinfo::argv_lines(members))
            }
        }
    }
}

impl FormatVars for ServerVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        // The clock is the newest mode of a pane showing it.
        if let Some(v) = crate::server::clock::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // Copy mode's variables while the pane is in it (TM.1).
        if let Some(v) = crate::server::copy::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // A chooser open over the pane is a mode too (TP.9b).
        if let Some(v) = crate::server::tree::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // tmux 3.7b reads it from the screen on view, so a pane in any mode
        // (whatever answers `pane_in_mode`) is 0 (TM.7).
        if name == "bracket_paste_flag"
            && (crate::server::clock::format_var(self.state, self.pane, "pane_in_mode").is_some()
                || crate::server::copy::format_var(self.state, self.pane, "pane_in_mode")
                    .as_deref()
                    == Some("1")
                || crate::server::tree::format_var(self.state, self.pane, "pane_in_mode").is_some())
        {
            return Some("0".to_string());
        }
        // The mouse event a mouse key's commands run for, and the view (TC.10).
        if let Some(v) = crate::server::mouse::format_var(self.state, name) {
            return Some(v);
        }
        // The newest automatic buffer's `#{buffer_*}` (TM.4).
        if let Some(v) = crate::server::buffers::format_var(self.state, name) {
            return Some(v);
        }
        // Inside a hook, its `#{hook*}` variables (TC.5).
        let hook = crate::server::hooks::current_formats(self.state);
        if let Some(v) = hook.and_then(|f| f.get(name)) {
            return Some(v);
        }
        match name {
            "socket_path" => Some(self.state.socket.display().to_string()),
            "config_file" => {
                config::config_file(&reload::config_env()).map(|f| f.display().to_string())
            }
            "pane_title" => Some(self.state.passthrough.title(self.pane)),
            "host" => Some(crate::server::passthrough::hostname().to_string()),
            // tmux: the host name up to its first dot.
            "host_short" => {
                let host = crate::server::passthrough::hostname();
                Some(host.split('.').next().unwrap_or(host).to_string())
            }
            "pane_tty" => {
                let p = self.state.model.panes.get(&self.pane)?;
                Some(p.handle.tty_path()?.display().to_string())
            }
            "pane_current_command" | "pane_current_argv" | "pane_foreground_argv" if self.fresh => {
                self.foreground(name)
                    .or_else(|| self.inner.vars().get(name))
            }
            "session_attached_list" => self.attached_list(),
            "agent_kind"
            | "agent_state"
            | "agent_msg"
            | "agent_blocked_kind"
            | "agent_since"
            | "agent_unseen"
            | "agent_attention"
            | "window_agent_state"
            | "session_agent_state"
            | "agents_total"
            | "agents_blocked"
            | "agents_working"
            | "agents_attention"
            | "agents_badge" => Some(self.agent_var(name)),
            "notifications_unread" => Some(self.state.notify.unread().to_string()),
            "session_group_attached"
            | "session_group_attached_list"
            | "session_group_many_attached" => {
                let session = self.session_id()?;
                self.state.model.group_of(session)?;
                let names = self.group_attached()?;
                Some(match name {
                    "session_group_attached" => names.len().to_string(),
                    "session_group_many_attached" => flag(names.len() > 1),
                    _ => names.join(","),
                })
            }
            _ => self.inner.vars().get(name),
        }
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        if kind == LoopKind::Clients {
            return self.client_items(sort);
        }
        let items = self.inner.vars().loop_items(kind, sort)?;
        Some(
            items
                .into_iter()
                .map(|it| {
                    let pane = it.pane.unwrap_or(self.pane);
                    LoopItem {
                        vars: Box::new(self.rebuild(Inner::Own(it.vars), pane)),
                        pane: Some(pane),
                        active: it.active,
                        extras: it.extras,
                    }
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.inner.vars().name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.inner.vars().pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        self.inner.vars().reexpand(text)
    }
}

impl<'a> ServerVars<'a> {
    /// This layer over other variables and the pane they read: what a
    /// loop's item sees.
    pub(super) fn rebuild<'x>(&'x self, inner: Inner<'x>, pane: PaneId) -> ServerVars<'x> {
        ServerVars {
            state: self.state,
            pane,
            inner,
            fresh: self.fresh,
            group: OnceCell::new(),
            leader: OnceCell::new(),
            members: OnceCell::new(),
        }
    }

    /// `#{session_attached_list}`: the names of the clients showing the
    /// pane's session, comma-joined.
    pub(super) fn attached_list(&self) -> Option<String> {
        let session = self.session_id()?;
        let names: Vec<&str> = self
            .state
            .clients
            .values()
            .filter(|c| c.session == session)
            .map(|c| c.name.as_str())
            .collect();
        Some(names.join(","))
    }

    /// The agent variables of spec §4.1; empty, not missing, when there is
    /// nothing to show.
    pub(super) fn agent_var(&self, name: &str) -> String {
        use crate::server::agents::{self, AgentState, Rank};
        let agents = &self.state.agents;
        let needs_you = |a: &agents::Agent| agents::rank(a) <= Rank::Unseen;
        let counts = |keep: &dyn Fn(&agents::Agent) -> bool| {
            agents.iter().filter(|(_, a)| keep(a)).count().to_string()
        };
        match name {
            "agents_total" => return counts(&|_| true),
            "agents_blocked" => return counts(&|a| a.state == AgentState::Blocked),
            "agents_working" => return counts(&|a| a.state == AgentState::Working),
            "agents_attention" => return counts(&needs_you),
            "agents_badge" => {
                let glyph = |n: &str| self.inner.vars().get(n).unwrap_or_default();
                let mut parts: Vec<String> = [
                    (Rank::Blocked, "agent-glyph-blocked"),
                    (Rank::Error, "agent-glyph-error"),
                    (Rank::Unseen, "agent-glyph-unseen"),
                ]
                .iter()
                .filter_map(|(rank, option)| {
                    let n = agents
                        .iter()
                        .filter(|(_, a)| agents::rank(a) == *rank)
                        .count();
                    (n > 0).then(|| format!("{}{n}", glyph(option)))
                })
                .collect();
                if self.state.notify.muted(std::time::Instant::now()) {
                    parts.push(glyph("notify-muted-glyph"));
                }
                return parts.join(" ");
            }
            "window_agent_state" | "session_agent_state" => {
                let model = &self.state.model;
                let Some(window) = model.panes.get(&self.pane).map(|p| p.window) else {
                    return String::new();
                };
                let windows = if name == "window_agent_state" {
                    vec![window]
                } else {
                    let session = self
                        .session_id()
                        .or_else(|| model.windows.get(&window).map(|w| w.session));
                    match session.and_then(|s| model.sessions.get(&s)) {
                        Some(s) => s.windows.clone(),
                        None => vec![window],
                    }
                };
                let best = agents
                    .iter()
                    .filter(|(p, _)| {
                        model
                            .panes
                            .get(p)
                            .is_some_and(|pane| windows.contains(&pane.window))
                    })
                    .map(|(_, a)| agents::rank(a))
                    .min();
                return best.map(|r| r.as_str().to_string()).unwrap_or_default();
            }
            _ => {}
        }
        let Some(a) = agents.get(self.pane) else {
            return String::new();
        };
        match name {
            "agent_kind" => a.kind.clone().unwrap_or_default(),
            "agent_state" => a.state.as_str().to_string(),
            "agent_msg" => a.msg.clone().unwrap_or_default(),
            "agent_blocked_kind" => a.blocked.clone().unwrap_or_default(),
            "agent_since" => std::time::Instant::now()
                .saturating_duration_since(a.since)
                .as_secs()
                .to_string(),
            "agent_unseen" => flag(a.unseen),
            _ => flag(needs_you(a)),
        }
    }

    /// The session the formats are for (`#{session_id}`).
    pub(super) fn session_id(&self) -> Option<SessionId> {
        let id = self.inner.vars().get("session_id")?;
        Some(SessionId(id.strip_prefix('$')?.parse().ok()?))
    }

    /// The clients showing any session of the session's group, else of the
    /// session: names, in client order.
    pub(super) fn group_attached(&self) -> Option<Vec<&str>> {
        let session = self.session_id()?;
        let m = &self.state.model;
        let mut sessions = m.group_peers(session);
        sessions.insert(0, session);
        Some(
            self.state
                .clients
                .values()
                .filter(|c| sessions.contains(&c.session))
                .map(|c| c.name.as_str())
                .collect(),
        )
    }

    /// `#{L:}`: every client, each over the pane this layer reads.
    pub(super) fn client_items(&self, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let model = &self.state.model;
        let mut all: Vec<&clients::ClientEntry> = self.state.clients.values().collect();
        sort.apply(&mut all, |order, a, b| {
            let first = match order {
                SortOrder::Name => a.name.as_bytes().cmp(b.name.as_bytes()),
                SortOrder::Activity => b.activity_time.cmp(&a.activity_time),
                _ => std::cmp::Ordering::Equal,
            };
            first.then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
        });
        Some(
            all.into_iter()
                .map(|c| {
                    let client =
                        clients::ClientVars::new(model, c, Some(FormatCtx::new(model, self.pane)));
                    LoopItem {
                        vars: Box::new(self.rebuild(Inner::Own(Box::new(client)), self.pane)),
                        pane: Some(self.pane),
                        active: false,
                        extras: Vec::new(),
                    }
                })
                .collect(),
        )
    }
}

/// The tmux version a server with tmux identity reports (`#{version}`,
/// `tmux -V`'s number): the release the compat suite is recorded against.
pub(super) const TMUX_COMPAT_VERSION: &str = "3.7b";

impl FormatVars for FormatCtx<'_> {
    fn get(&self, name: &str) -> Option<String> {
        use crate::compat::Identity;
        use crate::types::MouseMode;
        let m = self.model;
        if name.starts_with('@') || builtin(name).is_some() {
            let value = m.options.lookup(name, &self.option_chain())?;
            // A flag is 1 or 0 in a format (tmux's `options_to_string`
            // with `numeric`), so `#{?synchronize-panes,...}` is false off.
            return Some(match (builtin(name).map(|b| b.kind), value) {
                (Some(Kind::Bool), "on") => "1".to_string(),
                (Some(Kind::Bool), "off") => "0".to_string(),
                _ => value.to_string(),
            });
        }
        match name {
            "version" => {
                return Some(
                    match m.identity {
                        Identity::Grofe => env!("CARGO_PKG_VERSION"),
                        Identity::Tmux => TMUX_COMPAT_VERSION,
                    }
                    .to_string(),
                );
            }
            "grofe_version" => return Some(env!("CARGO_PKG_VERSION").to_string()),
            // Outside a loop; the loop's items say otherwise.
            "loop_last_flag" => return Some("0".to_string()),
            "server_sessions" => return Some(m.sessions.len().to_string()),
            "uid" => return Some(nix::unistd::getuid().as_raw().to_string()),
            "user" => {
                let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
                return Some(user.name);
            }
            "next_session_id" => return Some(m.next_session_id().to_string()),
            "tree_mode_format" => return Some(TREE_MODE_FORMAT.to_string()),
            "buffer_mode_format" => return Some(BUFFER_MODE_FORMAT.to_string()),
            "client_mode_format" => return Some(CLIENT_MODE_FORMAT.to_string()),
            "pid" => return Some(std::process::id().to_string()),
            "start_time" => {
                let since = m.started?.duration_since(std::time::UNIX_EPOCH).ok()?;
                return Some(since.as_secs().to_string());
            }
            // ponytail: stubs until the features exist: the status range under
            // the mouse, which only a mouse-bound command sees (TC.10).
            // `client_tty` is a client's (`ClientVars`), empty with none.
            "copy_cursor_word" | "pane_mode" | "mouse_status_range" | "client_tty"
            | "client_pid" => return Some(String::new()),
            "pane_in_mode" => return Some("0".to_string()),
            "pane_marked" => return Some(flag(m.marked_pane() == Some(self.pane))),
            "pane_marked_set" => return Some(flag(m.marked_pane().is_some())),
            "window_marked_flag" | "session_marked" => {
                let here = m.panes.get(&self.pane)?.window;
                let in_session = self.session_for(m.windows.get(&here)?);
                // The mark names the session and window link it was made in.
                let same = match m.marked_pane().and(m.marked) {
                    Some((session, _, _)) if name == "session_marked" => session == in_session,
                    Some((session, window, _)) => session == in_session && window == here,
                    None => false,
                };
                return Some(flag(same));
            }
            // The search string of a copy mode, which answers first.
            "pane_search_string" => return Some(String::new()),
            _ => {}
        }
        let p = m.panes.get(&self.pane)?;
        let w = m.windows.get(&p.window)?;
        let s = m.sessions.get(&self.session_for(w))?;
        let index_of = |id: &WindowId| {
            m.index_in(s.id, *id)
                .or_else(|| Some(m.windows.get(id)?.index))
        };
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "window_index" => index_of(&w.id)?.to_string(),
            "window_name" => w.name.clone(),
            "window_id" => w.id.to_string(),
            "window_active" => flag(s.active == w.id),
            "window_zoomed_flag" => flag(w.zoomed),
            "window_width" => w.cols.to_string(),
            "window_bell_flag" => flag(w.bell_flag),
            "window_activity_flag" => flag(w.activity_flag),
            "window_silence_flag" => flag(w.silence_flag),
            "window_height" => w.rows.to_string(),
            "window_panes" => w.pane_ids().len().to_string(),
            "pane_floating_flag" => flag(w.floating(p.id).is_some()),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s
                .created
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string(),
            "session_attached" => s.attached.to_string(),
            "session_path" => s.cwd.display().to_string(),
            "window_layout" => window_layout(w),
            // ponytail: a zoomed window's visible layout is its one zoomed
            // pane; not built, so no answer then.
            "window_visible_layout" if !w.zoomed => window_layout(w),
            "window_raw_flags" => window_flags(m, s, w),
            "pane_id" => p.id.to_string(),
            "pane_uid" => p.uid.clone(),
            "pane_index" => {
                let pos = w.pane_ids().iter().position(|id| *id == p.id)?;
                (pos as u32 + m.pane_base_index(w.id)).to_string()
            }
            "pane_active" => flag(w.active == p.id),
            "pane_pid" => p.handle.pid()?.to_string(),
            // The TCP ports the pane's processes listen on, comma-separated.
            "pane_ports" => crate::server::remote::join(&p.handle.ports()),
            "pane_current_command" => p.handle.current_command()?,
            "pane_current_argv" => p.handle.current_argv()?.join(" "),
            // Every member of the foreground group, one argv per line; the
            // sample holds only the leader's (a command reads them all,
            // `ServerVars::fresh`).
            "pane_foreground_argv" => procinfo::argv_lines(&[p.handle.current_argv()?]),
            "pane_current_path" => p
                .handle
                .current_path()
                .unwrap_or_else(|| p.cwd.clone())
                .display()
                .to_string(),
            // What the pane reported with OSC 7, as tmux shows it.
            "pane_path" => p.handle.reported_path().unwrap_or_default(),
            "pane_left" => match w.floating(p.id) {
                Some(f) => f.x.to_string(),
                None => pane_origin(w, p.id)?.0.to_string(),
            },
            "pane_top" => match w.floating(p.id) {
                Some(f) => f.y.to_string(),
                None => pane_origin(w, p.id)?.1.to_string(),
            },
            // tmux's names for the same corner, and the far one.
            "pane_x" => self.get("pane_left")?,
            "pane_y" => self.get("pane_top")?,
            "pane_right" => {
                let left: u32 = self.get("pane_left")?.parse().ok()?;
                (left + u32::from(p.cols)).saturating_sub(1).to_string()
            }
            "pane_bottom" => {
                let top: u32 = self.get("pane_top")?.parse().ok()?;
                (top + u32::from(p.rows)).saturating_sub(1).to_string()
            }
            // Whether the pane touches that edge of the window, as tmux's
            // format.c decides: a border status moves the top and bottom
            // edges in by the row it takes.
            "pane_at_left" => flag(self.get("pane_left")? == "0"),
            "pane_at_top" => {
                let top = if w.pane_status == PaneStatus::Top {
                    "1"
                } else {
                    "0"
                };
                flag(self.get("pane_top")? == top)
            }
            "pane_at_right" => {
                let left: u32 = self.get("pane_left")?.parse().ok()?;
                flag(left + u32::from(p.cols) >= u32::from(w.cols))
            }
            "pane_at_bottom" => {
                let top: u32 = self.get("pane_top")?.parse().ok()?;
                let edge = u32::from(w.rows) - u32::from(w.pane_status == PaneStatus::Bottom);
                flag(top + u32::from(p.rows) >= edge)
            }
            // The place in the stack, 0 on top: the floating panes, then
            // every tiled pane one below them all (tmux's z_index).
            "pane_z" => match w.floating(p.id) {
                Some(f) => w.floating.iter().filter(|o| o.z > f.z).count(),
                None => w.floating.len() + 1,
            }
            .to_string(),
            "pane_width" => p.cols.to_string(),
            "pane_height" => p.rows.to_string(),
            "pane_dead" => flag(p.dead.is_some()),
            // Spec §6: the pane's resume argv, quoted as
            // `pane_start_command`; the reason a resume is expected and
            // none is known; whether the pane came from a snapshot.
            "pane_resume" => p
                .resume
                .as_deref()
                .map(|argv| {
                    argv.iter()
                        .map(|a| quote::tmux_arg(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            "pane_resume_expected" => p.expect.clone().unwrap_or_default(),
            "pane_restored" => flag(p.restored),
            "pane_pipe" => flag(p.handle.pipe_open()),
            "pane_pipe_pid" => p.handle.pipe_pid()?.to_string(),
            // Empty for a live pane and for one a signal killed, as tmux.
            "pane_dead_status" => p.dead.flatten()?.to_string(),
            "pane_dead_time" => epoch_secs(p.dead_time?),
            "pane_start_command" => p
                .argv
                .as_deref()
                .map(|argv| {
                    argv.iter()
                        .map(|a| quote::tmux_arg(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            "alternate_on" => flag(p.handle.modes().alt_screen),
            "mouse_any_flag" => flag(p.handle.modes().mouse != MouseMode::Off),
            // 0 in a mode: `ServerVars::get`.
            "bracket_paste_flag" => flag(p.handle.modes().bracketed_paste),
            "history_size" => p.handle.history_size().to_string(),
            "history_limit" => m.history_limit(s.id).to_string(),
            // tmux's grid: 40 bytes a line, history and screen.
            // ponytail: tmux adds 5 bytes for each cell a line has
            // allocated, which depends on how its grid grew; left out.
            "history_bytes" => ((p.handle.history_size() + usize::from(p.rows)) * 40).to_string(),
            "window_flags" => window_flags(m, s, w),
            "pane_format" => flag(self.level == Level::Pane),
            "window_format" => flag(self.level == Level::Window),
            "session_format" => flag(self.level == Level::Session),
            "window_last_flag" => flag(s.last_windows.last() == Some(&w.id)),
            "pane_last" => flag(w.last_pane() == Some(p.id)),
            // tmux's `window_pane_printable_flags`.
            "pane_flags" => [
                (w.active == p.id, '*'),
                (w.last_pane() == Some(p.id), '-'),
                (w.zoomed && w.active == p.id, 'Z'),
                (w.floating(p.id).is_some(), 'F'),
            ]
            .into_iter()
            .filter_map(|(on, c)| on.then_some(c))
            .collect(),
            "window_start_flag" => flag(s.windows.first() == Some(&w.id)),
            "window_end_flag" => flag(s.windows.last() == Some(&w.id)),
            "window_activity" => epoch_secs(w.activity),
            "session_activity" => epoch_secs(std::time::SystemTime::now() - s.activity.elapsed()),
            "active_window_index" => index_of(&s.active)?.to_string(),
            "last_window_index" => s.windows.iter().filter_map(index_of).max()?.to_string(),
            "session_many_attached" => flag(s.attached > 1),
            "pane_zoomed_flag" => flag(w.zoomed && w.active == p.id),
            // The sessions (or groups, once each) whose current window it is.
            "window_active_sessions" => self.active_sessions(w.id).len().to_string(),
            "window_active_sessions_list" => self.active_sessions(w.id).join(","),
            "window_linked_sessions_list" => w
                .links
                .iter()
                .filter_map(|(l, _)| m.sessions.get(l).map(|s| s.name.as_str()))
                .collect::<Vec<_>>()
                .join(","),
            "session_grouped" => flag(m.group_of(s.id).is_some()),
            "session_group" => m.group_of(s.id)?.name.clone(),
            "session_group_size" => m.group_of(s.id)?.members.len().to_string(),
            "session_group_list" => m
                .group_of(s.id)?
                .members
                .iter()
                .filter_map(|l| m.sessions.get(l).map(|s| s.name.as_str()))
                .collect::<Vec<_>>()
                .join(","),
            // Each kind of alert once, windows in index order.
            "session_alert" => {
                let windows: Vec<_> = s
                    .windows
                    .iter()
                    .filter_map(|id| m.windows.get(id))
                    .collect();
                [
                    ('#', windows.iter().any(|w| w.activity_flag)),
                    ('!', windows.iter().any(|w| w.bell_flag)),
                    ('~', windows.iter().any(|w| w.silence_flag)),
                ]
                .into_iter()
                .filter_map(|(c, on)| on.then_some(c))
                .collect()
            }
            "session_activity_flag" | "session_bell_flag" | "session_silence_flag" => {
                let any = s
                    .windows
                    .iter()
                    .filter_map(|id| m.windows.get(id))
                    .any(|w| match name {
                        "session_activity_flag" => w.activity_flag,
                        "session_bell_flag" => w.bell_flag,
                        _ => w.silence_flag,
                    });
                flag(any)
            }
            // `idx` and its alerts, for each window that has any.
            "session_alerts" => s
                .windows
                .iter()
                .filter_map(|id| m.windows.get(id))
                .filter(|w| w.activity_flag || w.bell_flag || w.silence_flag)
                .map(|w| {
                    format!(
                        "{}{}{}{}",
                        index_of(&w.id).unwrap_or(w.index),
                        if w.activity_flag { "#" } else { "" },
                        if w.bell_flag { "!" } else { "" },
                        if w.silence_flag { "~" } else { "" },
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
            // The windows before this one, most recent first.
            "session_stack" => std::iter::once(index_of(&s.active)?)
                .chain(s.last_windows.iter().rev().filter_map(index_of))
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(","),
            "window_stack_index" => s
                .last_windows
                .iter()
                .rev()
                .position(|id| *id == w.id)
                .map_or(0, |at| at + 1)
                .to_string(),
            "pane_input_off" => {
                return Some(flag(m.panes.get(&self.pane)?.input_off));
            }
            "pane_synchronized" => {
                flag(m.options.lookup("synchronize-panes", &self.option_chain()) == Some("on"))
            }
            "pane_start_path" => p.cwd.display().to_string(),
            "window_linked" => flag(w.links.len() > 1),
            "window_linked_sessions" => m.linked_units(w.id).to_string(),
            // `client_width`/`client_height` are `ClientVars`', `host` and
            // `host_short` are `ServerVars`'.
            // The emulator's screen state (`cursor_x`, `wrap_flag`, ...).
            _ => return p.handle.term_var(name),
        })
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let specs = loop_specs(self.model, self.pane, self.session, None, kind, sort)?;
        Some(
            specs
                .into_iter()
                .map(|s| LoopItem {
                    vars: Box::new(in_session(
                        FormatCtx::at(self.model, s.pane, s.level),
                        s.session,
                    )),
                    pane: Some(s.pane),
                    active: s.active,
                    extras: s.extras,
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        let m = self.model;
        match kind {
            NameKind::Session => Some(m.sessions.values().any(|s| s.name == name)),
            NameKind::Window => {
                let p = m.panes.get(&self.pane)?;
                let s = m.sessions.get(&m.windows.get(&p.window)?.session)?;
                Some(
                    s.windows
                        .iter()
                        .filter_map(|id| m.windows.get(id))
                        .any(|w| w.name == name),
                )
            }
        }
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        let p = self.model.panes.get(&self.pane)?;
        let visible = crate::types::CaptureOpts {
            start: None,
            end: None,
            escapes: false,
        };
        let text = p.handle.capture(&visible);
        Some(crate::cmd::format::search_lines(
            text.lines(),
            term,
            regex,
            ignore,
        ))
    }

    fn reexpand(&self, text: &str) -> String {
        use crate::compat::Identity;
        match self.model.identity {
            Identity::Tmux => {
                crate::compat::tmux::formats::to_native(
                    text,
                    crate::compat::tmux::formats::Jobs::Empty,
                )
                .value
            }
            Identity::Grofe => text.to_string(),
        }
    }
}

/// Seconds since the epoch, as tmux prints a time variable.
pub(super) fn epoch_secs(t: std::time::SystemTime) -> String {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .to_string()
}

/// tmux 3.7b's `window_tree_mode.default_format`, `window_buffer_mode`'s and
/// `window_client_mode`'s (`window-tree.c`, `window-buffer.c`,
/// `window-client.c`): what `#{tree_mode_format}` and the like are.
pub(super) const TREE_MODE_FORMAT: &str = concat!(
    "#{?pane_format,",
    "#{?pane_marked,#[reverse],}#{?pane_floating_flag,#[italics],}",
    "#{pane_current_command}#{pane_flags}",
    "#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}",
    ",window_format,",
    "#{?window_marked_flag,#[reverse],}",
    "#{window_name}#{window_flags}",
    "#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}",
    ",",
    "#{session_windows} windows",
    "#{?session_grouped, (group #{session_group}: #{session_group_list}),}",
    "#{?session_attached, (attached),}",
    "}"
);
pub(super) const BUFFER_MODE_FORMAT: &str = "#{t/p:buffer_created}: #{buffer_sample}";
pub(super) const CLIENT_MODE_FORMAT: &str = "#{t/p:client_activity}: session #{session_name}";

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

/// tmux's `window_flags`: `#` activity (written `##`, as tmux escapes it
/// there), `!` bell, `~` silence, `*` current, `-` last, `M` holds the
/// marked pane, `Z` zoomed, in tmux's order.
pub(super) fn window_flags(
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
pub(super) fn pane_origin(w: &crate::server::model::Window, pane: PaneId) -> Option<(u16, u16)> {
    if w.zoomed && w.active == pane {
        let r = w.zoom_rect();
        return Some((r.x, r.y));
    }
    w.tiled_rects()
        .into_iter()
        .find(|(id, _)| *id == pane)
        .map(|(_, r)| (r.x, r.y))
}

// TC.2b: list-valued options.
//
// A list option is tmux's array option: a sparse map from index to text,
// read and written with the same grammar tmux 3.7b uses, so the native
// form and FE.3's tmux form are one form:
//
// - `set name[N] value` sets element N; the list may have holes.
// - `set name value` replaces the whole list with element 0.
// - `set -a name value` fills the lowest free index (a hole first);
//   `set -a name[N] value` appends the text to element N.
// - `set -u name[N]` removes element N and leaves a hole: later indices
//   do not shift (tmux's answer, recorded). `set -u name` puts a global
//   list back to its default and removes a list set anywhere else.
// - `show name` prints `name[N] value` per element (none for an empty
//   list: the bare name); `show name[N]` prints `''` for a hole; `-v`
//   drops the names.
// - The index is read as C's `%u` (blanks, a sign, digits, wrapping
//   modulo 2^32); anything malformed, or above `i32::MAX`, is
//   `ambiguous option: <name>`. An index on a scalar is `not an array`
//   for `set` and shows the scalar's value for `show`.
//
// A lookup takes the whole list from the first owner that has one (a
// session's list hides the global one entirely), as tmux's does.
