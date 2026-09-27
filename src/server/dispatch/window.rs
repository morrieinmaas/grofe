//! window commands, plus `list-panes` in its default format.

use std::path::Path;

use crate::cmd::{
    KillWindow, ListPanes, ListWindows, NewWindow, Resolve, SelectWindow, Target, WindowRef,
};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane};
use crate::server::layout::LayoutTree;
use crate::server::model::{PANE_BASE_INDEX, Window};

/// A new window's name when `-n` is not given: the basename of its cwd.
// ponytail: M2's `automatic-rename` keeps this following the pane's cwd.
pub fn default_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string())
}

/// `new-window`: `-t session:N` creates window N (which must be free);
/// any other target picks the session and the lowest free index.
pub fn new_window(state: &mut State, ctx: &ClientCtx, c: NewWindow) -> Result<String, String> {
    let model = &state.model;
    let (session, index) = match &c.target {
        Target::Window(s, WindowRef::Index(index)) => {
            let session = model.resolve_session(&Target::Session(s.clone()), ctx)?;
            if model.window_at_index(session, *index).is_some() {
                return Err(format!("index {index} in use"));
            }
            (session, *index)
        }
        target => {
            let session = model.resolve_session(target, ctx)?;
            (session, model.free_window_index(session))
        }
    };
    let s = &model.sessions[&session];
    let env = s.env.clone();
    let cwd = resolve_cwd(ctx, c.cwd.as_deref(), &s.cwd);
    let size = (s.cols, s.rows);

    let window = state.model.alloc_window_id();
    let pane = spawn_pane(
        state,
        window,
        env,
        command_argv(c.command),
        cwd.clone(),
        size,
    )?;
    let model = &mut state.model;
    model.insert_window(Window {
        id: window,
        session,
        index,
        name: c.name.unwrap_or_else(|| default_name(&cwd)),
        layout: LayoutTree::new(pane),
        active: pane,
        zoomed: false,
        cols: size.0,
        rows: size.1,
    });
    if !c.detached
        && let Some(s) = model.sessions.get_mut(&session)
    {
        s.active = window;
    }
    Ok(String::new())
}

pub fn kill_window(state: &mut State, ctx: &ClientCtx, c: KillWindow) -> Result<String, String> {
    let window = state.model.resolve_window(&c.target, ctx)?;
    state.model.remove_window(window);
    Ok(String::new())
}

pub fn select_window(
    state: &mut State,
    ctx: &ClientCtx,
    c: SelectWindow,
) -> Result<String, String> {
    let model = &mut state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    let session = model.windows[&window].session;
    if let Some(s) = model.sessions.get_mut(&session) {
        s.active = window;
    }
    Ok(String::new())
}

/// `list-windows` in the default format; `-F` is routed to `options`.
pub fn list_windows(state: &State, ctx: &ClientCtx, c: ListWindows) -> Result<String, String> {
    let model = &state.model;
    let session = model.resolve_session(&c.target, ctx)?;
    let s = &model.sessions[&session];
    Ok(s.windows
        .iter()
        .filter_map(|id| model.windows.get(id))
        .map(|w| {
            let active = w.id == s.active;
            format!(
                "{}: {}{} ({} panes) [{}x{}] {}{}\n",
                w.index,
                w.name,
                if active { "*" } else { "" },
                w.pane_ids().len(),
                w.cols,
                w.rows,
                w.id,
                if active { " (active)" } else { "" },
            )
        })
        .collect())
}

/// `list-panes` in the default format; `-F` is routed to `options`.
pub fn list_panes(state: &State, ctx: &ClientCtx, c: ListPanes) -> Result<String, String> {
    let model = &state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    let w = &model.windows[&window];
    Ok(w.pane_ids()
        .iter()
        .zip(PANE_BASE_INDEX..)
        .filter_map(|(id, index)| Some((index, model.panes.get(id)?)))
        .map(|(index, p)| {
            format!(
                "{index}: [{}x{}] {}{}{}\n",
                p.cols,
                p.rows,
                p.id,
                if p.id == w.active { " (active)" } else { "" },
                if p.dead.is_some() { " (dead)" } else { "" },
            )
        })
        .collect())
}
