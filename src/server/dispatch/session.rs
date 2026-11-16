//! session commands.

use std::time::Instant;

use crate::cmd::{
    self, HasSession, KillSession, ListSessions, NewSession, Rename, Resolve, SessionRef,
    SwitchClient, Target,
};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::{self, FormatCtx, Owner, ServerVars};
use crate::server::dispatch::sorting::Sort;
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
    let tmux = ctx.dialect == crate::protocol::Dialect::Tmux;
    let mut c = c;
    if let Some(name) = c.name.take() {
        c.name = Some(check_name(model, &name, tmux)?);
    }
    // `-t`: the session to group with, else the name of a new group.
    let group = c.group.as_deref().map(|raw| {
        let word = raw.split([':', '.']).next().unwrap_or(raw);
        let found =
            model.resolve_session(&Target::Session(SessionRef::Name(word.to_string())), ctx);
        (word.to_string(), found.ok())
    });
    if group.is_some() && (c.command.is_some() || c.window_name.is_some()) {
        return Err("command or window name given with target".to_string());
    }
    let id = model.alloc_session_id();
    let prefix = group.as_ref().map(|(word, found)| {
        found
            .and_then(|s| model.group_of(s).map(|g| g.name.clone()))
            .or_else(|| found.map(|s| model.sessions[&s].name.clone()))
            .unwrap_or_else(|| word.clone())
    });
    let here = ctx.pane.filter(|p| model.panes.contains_key(p));
    let cwd = resolve_cwd(model, ctx, here, c.cwd.as_deref(), &ctx.cwd);
    let name = match (c.name, &prefix) {
        (Some(name), _) => name,
        (None, None) if model.identity != crate::compat::Identity::Tmux => {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            match place_name(&cwd, home.as_deref()) {
                Some(base) => std::iter::once(base.clone())
                    .chain((2..).map(|n| format!("{base}-{n}")))
                    .find(|n| model.session_by_name(n).is_none())
                    .unwrap_or(base),
                None => (id.0..)
                    .map(|n| n.to_string())
                    .find(|n| model.session_by_name(n).is_none())
                    .unwrap_or_else(|| id.0.to_string()),
            }
        }
        (None, Some(prefix)) => (id.0..)
            .map(|n| format!("{prefix}-{n}"))
            .find(|n| model.session_by_name(n).is_none())
            .unwrap_or_else(|| format!("{prefix}-{}", id.0)),
        (None, None) => (id.0..)
            .map(|n| n.to_string())
            .find(|n| model.session_by_name(n).is_none())
            .unwrap_or_else(|| id.0.to_string()),
    };
    if let (Some(prefix), Some((_, Some(target)))) = (&prefix, &group) {
        return new_grouped(
            state,
            id,
            name,
            prefix,
            *target,
            c.print,
            c.format.as_deref(),
        );
    }
    // A missing -x or -y is `default-size`'s, as tmux's detached sessions.
    let default = model
        .options
        .lookup("default-size", &[])
        .and_then(super::options::parse_size)
        .unwrap_or(DEFAULT_SIZE);
    let size = cap_size(c.width.unwrap_or(default.0), c.height.unwrap_or(default.1));
    let window = model.alloc_window_id();
    // Only what `update-environment` names comes from the client (TC.5).
    // `-E` takes nothing from the client, and marks nothing removed.
    let mut env = if c.no_env {
        crate::server::hooks::blank_session_environment(state, id)
    } else {
        crate::server::hooks::session_environment(state, id, &ctx.env)
    };
    // `-e` variables belong to the session, so its later panes get them.
    env.extend(c.env);
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
    if let Some(p) = state.model.panes.get_mut(&pane) {
        crate::server::resume::bind_root(p);
    }
    crate::server::resume::spawned(state, pane, Default::default());

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
        activity: std::time::SystemTime::now(),
        last_panes: Vec::new(),
        active_points: Default::default(),
        floating: Vec::new(),
        pane_status: Default::default(),
        frames: false,
        pane_gap: 0,
        drop_preview: None,
        links: Vec::new(),
    });
    // `-n` names the window for good, as `rename-window` does.
    if named {
        model
            .options
            .set(Owner::Window(window), "automatic-rename", "off", false)?;
    }
    if let Some(prefix) = prefix {
        state.model.join_group(&prefix, id);
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

/// The name grofe gives a session made without `-s` in `cwd`: the git
/// repository it is in (the nearest folder above with a `.git`), else the
/// folder itself; `home` for `home`, so `~` and a repository at `~` give
/// it. `.`, `:` and spaces, which a target cannot hold, become `_`. `None`
/// for the root folder, which keeps tmux's number.
pub fn place_name(cwd: &std::path::Path, home: Option<&std::path::Path>) -> Option<String> {
    let repo = cwd
        .ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(cwd);
    if Some(repo) == home {
        return Some("home".to_string());
    }
    let base = repo
        .file_name()?
        .to_string_lossy()
        .replace(['.', ':', ' '], "_");
    (!base.is_empty()).then_some(base)
}

/// `new-session -t`: session `id` joins `target`'s group (made, named
/// `group`, if it had none) and takes its windows at their indexes, none
/// of its own. Its current window is the first.
fn new_grouped(
    state: &mut State,
    id: SessionId,
    name: String,
    group: &str,
    target: SessionId,
    print: bool,
    format: Option<&str>,
) -> Result<String, String> {
    let model = &mut state.model;
    // tmux makes a window and a pane for the session and drops them: the
    // ids they took are gone (recorded).
    model.alloc_window_id();
    model.alloc_pane_id();
    let t = &model.sessions[&target];
    let session = Session {
        id,
        name,
        windows: Vec::new(),
        active: t.active,
        env: t.env.clone(),
        cwd: t.cwd.clone(),
        cols: t.cols,
        rows: t.rows,
        last_windows: Vec::new(),
        activity: Instant::now(),
        attached: 0,
        created: std::time::SystemTime::now(),
    };
    model.insert_session(session);
    if model.group_of(target).is_none() {
        model.join_group(group, target);
    }
    model.join_group(group, id);
    model.sync_group(target);
    crate::server::hooks::session_created(state, id);
    if !print {
        return Ok(String::new());
    }
    let model = &state.model;
    let pane = model
        .windows
        .get(&model.sessions[&id].active)
        .map(|w| w.active)
        .ok_or("session has no windows")?;
    let format = format.unwrap_or(NEW_SESSION_TEMPLATE);
    let vars = FormatCtx::new(model, pane).in_session(id);
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
    // As tmux, `-C` wins over `-a`, and `-a` over `-g`.
    let doomed: Vec<SessionId> = if c.clear_alerts {
        let model = &mut state.model;
        let windows = model.sessions[&id].windows.clone();
        for id in windows {
            if let Some(w) = model.windows.get_mut(&id) {
                w.bell_flag = false;
                w.activity_flag = false;
                w.silence_flag = false;
            }
        }
        Vec::new()
    } else if c.others {
        state
            .model
            .sessions
            .keys()
            .copied()
            .filter(|s| *s != id)
            .collect()
    } else if c.group && state.model.group_of(id).is_some() {
        let mut all = state.model.group_peers(id);
        all.insert(0, id);
        all
    } else {
        vec![id]
    };
    for session in doomed {
        let removed = state.model.remove_session(session);
        state.events.panes_killed(removed);
    }
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
    Ok(
        crate::server::dispatch::options::listed_sessions(state, &c)?
            .into_iter()
            .filter_map(|id| model.sessions.get(&id))
            .map(|s| format!("{}: {} windows\n", s.name, s.windows.len()))
            .collect(),
    )
}

/// A new session name, checked and cleaned. tmux 3.7b (recorded) accepts
/// any name of valid UTF-8 and printable ASCII, `:` and `.` and the empty
/// name too, storing a backslash doubled (`clean_name`); control characters
/// are `invalid session name`. Natively a name must be usable in a target.
fn clean_name(name: &str) -> Result<String, String> {
    if name.chars().any(|c| c.is_ascii_control()) {
        return Err(format!("invalid session name: {name}"));
    }
    Ok(name.replace('\\', "\\\\"))
}

fn check_name(model: &Model, name: &str, tmux: bool) -> Result<String, String> {
    let name = if tmux {
        clean_name(name)?
    } else {
        if name.is_empty() || name.contains([':', '.']) {
            return Err(format!("bad session name: {name}"));
        }
        name.to_string()
    };
    if model.session_by_name(&name).is_some() {
        return Err(format!("duplicate session: {name}"));
    }
    Ok(name)
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
    let tmux = ctx.dialect == crate::protocol::Dialect::Tmux;
    let name = if tmux { clean_name(&name)? } else { name };
    let model = &mut state.model;
    if model.sessions[&id].name == name {
        return Ok(String::new());
    }
    let name = if tmux {
        if model.session_by_name(&name).is_some() {
            return Err(format!("duplicate session: {name}"));
        }
        name
    } else {
        check_name(model, &name, false)?
    };
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
    // `-r` flips the client between read-only and not (with its size
    // ignored while it is).
    if c.toggle_read_only {
        clients::toggle_read_only(state, id);
    }
    if let Some(table) = &c.key_table {
        if !state.registry.key_tables.exists(table) {
            return Err(format!("table {table} doesn't exist"));
        }
        clients::set_key_table(state, id, table);
        return Ok(String::new());
    }
    // `-n` and `-p` walk the sessions in `-O`'s order, `-r` reversing it.
    let sort = Sort::new(&crate::cmd::SortFlags {
        order: c.order.clone(),
        reversed: c.toggle_read_only,
    })?;
    let current = state.clients[&id].session;
    let model = &mut state.model;
    let session = if c.last {
        model
            .client_last_session
            .get(&id)
            .copied()
            .filter(|s| model.sessions.contains_key(s))
            // A client with no history yet (a new attach, or the first
            // after a restart, whose restore keeps the order of use) takes
            // the session used most recently before its own.
            .or_else(|| {
                model
                    .sessions
                    .values()
                    .filter(|s| s.id != current)
                    .max_by_key(|s| s.activity)
                    .map(|s| s.id)
            })
            .ok_or_else(|| "can't find last session".to_string())?
    } else if c.next || c.previous {
        neighbour_session(model, current, c.next, &sort).ok_or_else(|| {
            format!(
                "can't find {} session",
                if c.next { "next" } else { "previous" }
            )
        })?
    } else {
        let target = c.target.unwrap_or(Target::Current);
        clients::select_target(model, &target, ctx, c.keep_zoom)?
    };
    clients::show_session_with(state, id, session, !c.no_env);
    Ok(String::new())
}

/// The session after (or before) `current` in `sort`'s order (by name, with
/// none), wrapping.
fn neighbour_session(
    model: &Model,
    current: SessionId,
    next: bool,
    sort: &Sort,
) -> Option<SessionId> {
    let sessions = options::sessions_sorted(model, sort);
    let n = sessions.len();
    let pos = sessions.iter().position(|id| *id == current)?;
    let pos = if next {
        (pos + 1) % n
    } else {
        (pos + n - 1) % n
    };
    Some(sessions[pos])
}

#[cfg(test)]
mod tests {
    use super::place_name;
    use std::fs;

    /// A session is named after its git repository from anywhere inside
    /// it, else after its folder; `~` is `home`, and `/` keeps a number.
    #[test]
    fn a_session_is_named_after_its_repository_or_folder() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("me");
        let repo = home.join("code").join("my.app");
        let deep = repo.join("src").join("bin");
        let plain = home.join("notes");
        for d in [&deep, &plain] {
            fs::create_dir_all(d).expect("mkdir");
        }
        fs::create_dir(repo.join(".git")).expect("mkdir .git");
        let name = |p: &std::path::Path| place_name(p, Some(&home));
        assert_eq!(name(&repo).as_deref(), Some("my_app"));
        assert_eq!(name(&deep).as_deref(), Some("my_app"));
        assert_eq!(name(&plain).as_deref(), Some("notes"));
        assert_eq!(name(&home).as_deref(), Some("home"));
        assert_eq!(place_name(std::path::Path::new("/"), Some(&home)), None);
    }
}
