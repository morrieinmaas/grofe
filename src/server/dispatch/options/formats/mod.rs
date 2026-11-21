use super::*;

mod client;
mod ctx;
mod server;
mod vars;

pub use self::ctx::*;
pub use self::server::*;

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
