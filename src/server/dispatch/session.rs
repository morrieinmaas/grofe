//! session commands.

use std::time::Instant;

use crate::cmd::{
    HasSession, KillSession, ListSessions, NewSession, Rename, Resolve, SwitchClient, Target,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane, window::default_name};
use crate::server::layout::LayoutTree;
use crate::server::model::Model;
use crate::server::model::{DEFAULT_SIZE, Session, Window};
use crate::server::{State, clients};
use crate::types::{SessionId, cap_size};

/// `new-session`: a session with one window holding one pane. The server
/// always creates it detached; without `-d` the CLI then attaches to it
/// (`new_and_attach` in `main.rs`).
pub fn new_session(state: &mut State, ctx: &ClientCtx, c: NewSession) -> Result<String, String> {
    let model = &mut state.model;
    if let Some(name) = &c.name {
        check_name(model, name)?;
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
    let size = cap_size(
        c.width.unwrap_or(DEFAULT_SIZE.0),
        c.height.unwrap_or(DEFAULT_SIZE.1),
    );
    let window = model.alloc_window_id();
    let pane = spawn_pane(
        state,
        (id, window),
        ctx.env.clone(),
        command_argv(c.command),
        cwd.clone(),
        size,
        // A new session has no client yet.
        None,
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
        last_windows: Vec::new(),
        activity: Instant::now(),
        attached: 0,
    });
    model.insert_window(Window {
        id: window,
        session: id,
        index: model.base_index(id),
        name: default_name(&cwd),
        layout: LayoutTree::new(pane),
        active: pane,
        zoomed: false,
        cols: size.0,
        rows: size.1,
        bell_flag: false,
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

/// A new session name must be usable in a target and not taken.
// ponytail: tmux 3.7b accepts `:` and `.` (and the empty name) and makes
// such a session hard to target; grofe refuses them, as new-session does.
fn check_name(model: &Model, name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains([':', '.']) {
        return Err(format!("bad session name: {name}"));
    }
    if model.session_by_name(name).is_some() {
        return Err(format!("duplicate session: {name}"));
    }
    Ok(())
}

pub fn rename_session(state: &mut State, ctx: &ClientCtx, c: Rename) -> Result<String, String> {
    let model = &mut state.model;
    let id = model.resolve_session(&c.target, ctx)?;
    if model.sessions[&id].name == c.name {
        return Ok(String::new());
    }
    check_name(model, &c.name)?;
    if let Some(s) = model.sessions.get_mut(&id) {
        s.name = c.name;
    }
    Ok(String::new())
}

/// `switch-client` for the current client (the one whose binding ran it,
/// else the most recently active one, as tmux picks): `-T` sets its key
/// table; `-l` goes to its last session, `-n` / `-p` to the next or
/// previous session by name (wrapping), otherwise to `-t`.
pub fn switch_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: SwitchClient,
) -> Result<String, String> {
    let id = clients::current_client(state, ctx).ok_or_else(|| "no current client".to_string())?;
    if let Some(table) = &c.key_table {
        if !state.registry.key_tables.exists(table) {
            return Err(format!("table {table} doesn't exist"));
        }
        clients::set_key_table(state, id, table);
        return Ok(String::new());
    }
    let current = state.clients[&id].session;
    let model = &mut state.model;
    let session = if c.last {
        model
            .client_last_session
            .get(&id)
            .copied()
            .filter(|s| model.sessions.contains_key(s))
            .ok_or_else(|| "can't find last session".to_string())?
    } else if c.next || c.previous {
        neighbour_session(model, current, c.next)
    } else {
        let target = c.target.unwrap_or(Target::Current);
        clients::select_target(model, &target, ctx)?
    };
    clients::show_session(state, id, session);
    Ok(String::new())
}

/// The session after (or before) `current` in name order, wrapping.
fn neighbour_session(model: &Model, current: SessionId, next: bool) -> SessionId {
    let mut sessions: Vec<(&str, SessionId)> = model
        .sessions
        .values()
        .map(|s| (s.name.as_str(), s.id))
        .collect();
    sessions.sort();
    let n = sessions.len();
    let Some(pos) = sessions.iter().position(|(_, id)| *id == current) else {
        return current;
    };
    let pos = if next {
        (pos + 1) % n
    } else {
        (pos + n - 1) % n
    };
    sessions[pos].1
}
