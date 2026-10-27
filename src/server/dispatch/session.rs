//! session commands.

use std::time::Instant;

use crate::cmd::{
    self, HasSession, KillSession, ListSessions, NewSession, Rename, Resolve, SwitchClient, Target,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::{FormatCtx, Owner, ServerVars};
use crate::server::dispatch::{resolve_cwd, spawn_pane, window::default_name};
use crate::server::layout::LayoutTree;
use crate::server::model::Model;
use crate::server::model::{DEFAULT_SIZE, Session, Window};
use crate::server::{State, clients};
use crate::types::{SessionId, cap_size};

/// `new-session`: a session with one window holding one pane. The server
/// always creates it detached; without `-d` the CLI then attaches to it
/// (`new_and_attach` in `main.rs`). With `-A` and an existing `-s` name it
/// attaches to that session instead ([`attach_existing`]).
pub fn new_session(state: &mut State, ctx: &ClientCtx, c: NewSession) -> Result<String, String> {
    if c.attach
        && let Some(id) = c
            .name
            .as_deref()
            .and_then(|n| state.model.session_by_name(n))
    {
        attach_existing(state, ctx, id);
        return Ok(String::new());
    }
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
    // Only what `update-environment` names comes from the client (TC.5).
    let env = crate::server::hooks::session_environment(state, id, &ctx.env);
    let pane = spawn_pane(
        state,
        (id, window),
        env.clone(),
        c.command,
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
        env,
        cwd: cwd.clone(),
        cols: size.0,
        rows: size.1,
        last_windows: Vec::new(),
        activity: Instant::now(),
        attached: 0,
        created: std::time::SystemTime::now(),
    });
    let named = c.window_name.is_some();
    model.insert_window(Window {
        id: window,
        session: id,
        index: model.base_index(id),
        name: c.window_name.unwrap_or_else(|| default_name(&cwd)),
        layout: LayoutTree::new(pane),
        active: pane,
        zoomed: false,
        cols: size.0,
        rows: size.1,
        bell_flag: false,
        activity_flag: false,
        silence_flag: false,
        last_panes: Vec::new(),
        floating: Vec::new(),
    });
    // `-n` names the window for good, as `rename-window` does.
    if named {
        model
            .options
            .set(Owner::Window(window), "automatic-rename", "off", false)?;
    }
    crate::server::hooks::session_created(state, id);
    if !c.print {
        return Ok(String::new());
    }
    let format = c.format.as_deref().unwrap_or(NEW_SESSION_TEMPLATE);
    let vars = FormatCtx::new(&state.model, pane);
    Ok(format!(
        "{}\n",
        cmd::expand(format, &ServerVars::new(state, pane, &vars))
    ))
}

/// What `new-session -P` prints without `-F` (tmux's
/// `NEW_SESSION_TEMPLATE`).
const NEW_SESSION_TEMPLATE: &str = "#{session_name}:";

/// `new-session -A` for a session that exists, as tmux 3.7b's
/// `attach-session`: an attached client (a binding's) switches to it.
/// From the CLI the session becomes the most recently active one, so the
/// `display-message -p #{session_id}` `new_and_attach` sends after it names
/// this session and the CLI attaches to it; a second `-A` call therefore
/// makes no duplicate.
// The CLI's own `new -A` for an existing session never gets here: `main.rs`
// attaches to it first (despite `-d`, as tmux 3.7b), or fails without a
// terminal. This covers a session made between that check and the create.
fn attach_existing(state: &mut State, ctx: &ClientCtx, id: SessionId) {
    match ctx.client.filter(|c| state.clients.contains_key(c)) {
        Some(client) => clients::show_session(state, client, id),
        None => {
            if let Some(s) = state.model.sessions.get_mut(&id) {
                s.activity = Instant::now();
            }
        }
    }
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
pub fn list_sessions(state: &State, c: ListSessions) -> Result<String, String> {
    let model = &state.model;
    Ok(crate::server::dispatch::options::listed_sessions(state, &c)
        .into_iter()
        .filter_map(|id| model.sessions.get(&id))
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

/// In the tmux dialect the name is a format, expanded against the
/// session's current pane, as tmux 3.7b expands it.
pub fn rename_session(state: &mut State, ctx: &ClientCtx, c: Rename) -> Result<String, String> {
    let id = state.model.resolve_session(&c.target, ctx)?;
    let pane = state
        .model
        .sessions
        .get(&id)
        .and_then(|s| state.model.windows.get(&s.active))
        .map(|w| w.active);
    let name = match pane {
        Some(pane) if ctx.dialect == crate::protocol::Dialect::Tmux => {
            let vars = FormatCtx::new(&state.model, pane);
            cmd::expand(&c.name, &ServerVars::new(state, pane, &vars))
        }
        _ => c.name,
    };
    let model = &mut state.model;
    if model.sessions[&id].name == name {
        return Ok(String::new());
    }
    check_name(model, &name)?;
    if let Some(s) = model.sessions.get_mut(&id) {
        s.name = name;
    }
    Ok(String::new())
}

/// `switch-client` for the `-c` client, else the current client (the one
/// whose binding ran it, else the most recently active one, as tmux
/// picks): `-T` sets its key table for its next key; `-l` goes to its last
/// session, `-n` / `-p` to the next or previous session by name
/// (wrapping), otherwise to `-t`.
pub fn switch_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: SwitchClient,
) -> Result<String, String> {
    let id = clients::target_client(state, ctx, c.client.as_deref())?;
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
