//! session commands.

use crate::cmd::{HasSession, KillSession, ListSessions, NewSession, Resolve};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane, window::default_name};
use crate::server::layout::LayoutTree;
use crate::server::model::{BASE_INDEX, DEFAULT_SIZE, Session, Window};

/// `new-session`: a session with one window holding one pane.
// ponytail: without `-d` tmux would attach; attach arrives in M2, so every
// session is created detached for now.
pub fn new_session(state: &mut State, ctx: &ClientCtx, c: NewSession) -> Result<String, String> {
    let model = &mut state.model;
    if let Some(name) = &c.name {
        if name.is_empty() || name.contains([':', '.']) {
            return Err(format!("bad session name: {name}"));
        }
        if model.session_by_name(name).is_some() {
            return Err(format!("duplicate session: {name}"));
        }
    }
    let id = model.alloc_session_id();
    let name = match c.name {
        Some(name) => name,
        None => (id.0..)
            .map(|n| n.to_string())
            .find(|n| model.session_by_name(n).is_none())
            .unwrap_or_else(|| id.0.to_string()),
    };
    let here = ctx.pane.filter(|p| model.panes.contains_key(p));
    let cwd = resolve_cwd(model, ctx, here, c.cwd.as_deref(), &ctx.cwd);
    let size = (
        c.width.unwrap_or(DEFAULT_SIZE.0).max(1),
        c.height.unwrap_or(DEFAULT_SIZE.1).max(1),
    );
    let window = model.alloc_window_id();
    let pane = spawn_pane(
        state,
        window,
        ctx.env.clone(),
        command_argv(c.command),
        cwd.clone(),
        size,
    )?;

    let model = &mut state.model;
    model.insert_session(Session {
        id,
        name,
        windows: Vec::new(),
        active: window,
        env: ctx.env.clone(),
        cwd: cwd.clone(),
        cols: size.0,
        rows: size.1,
        last_window: None,
    });
    model.insert_window(Window {
        id: window,
        session: id,
        index: BASE_INDEX,
        name: default_name(&cwd),
        layout: LayoutTree::new(pane),
        active: pane,
        zoomed: false,
        cols: size.0,
        rows: size.1,
    });
    Ok(String::new())
}

pub fn kill_session(state: &mut State, ctx: &ClientCtx, c: KillSession) -> Result<String, String> {
    let id = state.model.resolve_session(&c.target, ctx)?;
    let removed = state.model.remove_session(id);
    state.events.panes_killed(removed);
    Ok(String::new())
}

pub fn has_session(state: &mut State, ctx: &ClientCtx, c: HasSession) -> Result<String, String> {
    state.model.resolve_session(&c.target, ctx)?;
    Ok(String::new())
}

/// `list-sessions` in the default format; `-F` is routed to `options`.
/// tmux sorts sessions by name, not by id.
pub fn list_sessions(state: &State, _c: ListSessions) -> Result<String, String> {
    let model = &state.model;
    let mut sessions: Vec<&Session> = model.sessions.values().collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions
        .into_iter()
        .map(|s| format!("{}: {} windows\n", s.name, s.windows.len()))
        .collect())
}
