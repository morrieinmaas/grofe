//! `lock-server`, `lock-session`, `lock-client` and `lock-after-time`,
//! `server-access`, and `show-prompt-history` / `clear-prompt-history`
//! (B4).
//!
//! tmux's lock sends the client a message to run `lock-command` on its own
//! terminal (`system(3)` with the tty given back). The attach protocol is
//! frozen (`docs/protocol-v1.md`: no new frame kinds), so grofe runs the
//! command in a popup the size of the client, with no border, that closes
//! when the command exits and takes every key until then.

use std::time::{Duration, Instant};

use crate::cmd::{Lock, LockScope, PromptHistory, Resolve, ServerAccess};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::Owner;
use crate::server::popup::{self, Frame};
use crate::server::{State, clients, hooks, prompt};
use crate::types::ClientId;

/// `lock-server`, `lock-session`, `lock-client`.
pub fn lock(state: &mut State, ctx: &ClientCtx, c: Lock) -> Result<String, String> {
    let ids: Vec<ClientId> = match c.scope {
        LockScope::Server => state.clients.keys().copied().collect(),
        LockScope::Session(target) => {
            let session = state.model.resolve_session(&target, ctx)?;
            state
                .clients
                .iter()
                .filter(|(_, cl)| cl.session == session)
                .map(|(id, _)| *id)
                .collect()
        }
        LockScope::Client(name) => vec![clients::target_client(state, ctx, name.as_deref())?],
    };
    for id in ids {
        lock_client(state, id);
    }
    Ok(String::new())
}

/// Locks client `id`: `lock-command` of its session runs on the client
/// until it exits. Nothing for a client already locked or stopped, or when
/// the command is empty.
pub(crate) fn lock_client(state: &mut State, id: ClientId) {
    let Some(c) = state.clients.get(&id) else {
        return;
    };
    if c.is_suspended() || state.registry.locked.contains(&id) {
        return;
    }
    let (session, (cols, rows)) = (c.session, c.size);
    let command = state
        .model
        .options
        .lookup("lock-command", &[Owner::Session(session)])
        .unwrap_or_default()
        .to_string();
    if command.is_empty() {
        return;
    }
    let Some(pane) = state
        .model
        .sessions
        .get(&session)
        .and_then(|s| state.model.windows.get(&s.active))
        .map(|w| w.active)
    else {
        return;
    };
    let frame = Frame {
        at: (0, 0),
        size: (cols, rows),
        lines: None,
        title: None,
        border_style: None,
        style: None,
        close_on_exit: true,
        any_key: false,
    };
    let spawn = popup_spawn(state, session, pane, command);
    if popup::open(state, id, frame, spawn).unwrap_or(false) {
        state.registry.locked.insert(id);
    }
}

/// The popup's process: `sh -c <command>` through the session's shell, as
/// `system(3)` runs it.
fn popup_spawn(
    state: &State,
    session: crate::types::SessionId,
    pane: crate::types::PaneId,
    command: String,
) -> crate::server::pane::PaneSpawn {
    use crate::server::pane::{pane_shell, usable_cwd};
    let model = &state.model;
    let mut env = hooks::shell_env(state, Some(session));
    let shell = pane_shell(model.default_shell(session).as_deref());
    env.push(("SHELL".to_string(), shell.clone()));
    let cwd = usable_cwd(model.sessions[&session].cwd.clone());
    crate::server::dispatch::popup::spawn_for(
        state,
        session,
        pane,
        (Some(vec![command]), shell),
        cwd,
        env,
    )
}

/// The next time a client is due to lock: `lock-after-time` seconds after
/// the last input of its session's clients.
pub fn deadline(state: &State) -> Option<Instant> {
    state
        .clients
        .iter()
        .filter(|(id, _)| !state.registry.locked.contains(id))
        .filter_map(|(_, c)| lock_at(state, c.session))
        .min()
}

/// When session `session` locks, if it ever does.
fn lock_at(state: &State, session: crate::types::SessionId) -> Option<Instant> {
    let secs: u64 = state
        .model
        .options
        .lookup("lock-after-time", &[Owner::Session(session)])?
        .parse()
        .ok()
        .filter(|s| *s > 0)?;
    let last = state
        .clients
        .values()
        .filter(|c| c.session == session)
        .map(|c| c.activity)
        .max()?;
    Some(last + Duration::from_secs(secs))
}

/// Locks the clients of the sessions whose time is up, and notes the
/// clients whose lock command has exited: they count as active again.
pub fn tick(state: &mut State, now: Instant) {
    let unlocked: Vec<ClientId> = state
        .registry
        .locked
        .iter()
        .filter(|id| !state.popups.is_open(**id))
        .copied()
        .collect();
    for id in unlocked {
        state.registry.locked.remove(&id);
        clients::touch(state, id, now);
    }
    let due: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(id, _)| !state.registry.locked.contains(id))
        .filter(|(_, c)| lock_at(state, c.session).is_some_and(|at| at <= now))
        .map(|(id, _)| *id)
        .collect();
    for id in due {
        lock_client(state, id);
    }
}

/// `server-access`: who else may use the server. The list starts with the
/// server's own user (and root), as tmux's `server_acl_init`.
pub fn server_access(
    state: &mut State,
    ctx: &ClientCtx,
    c: ServerAccess,
) -> Result<String, String> {
    use nix::unistd::{Uid, User};
    let acl_has = |state: &State, uid: u32| state.registry.acl_contains(uid);
    if c.list {
        let mut out = String::new();
        for (uid, read_only) in state.registry.acl_entries() {
            if uid == 0 {
                continue;
            }
            let name = User::from_uid(Uid::from_raw(uid))
                .ok()
                .flatten()
                .map_or_else(|| "unknown".to_string(), |u| u.name);
            out.push_str(&format!("{name} ({})\n", if read_only { 'R' } else { 'W' }));
        }
        return Ok(out);
    }
    let Some(given) = c.user.as_deref() else {
        return Err("missing user argument".to_string());
    };
    let name = prompt::expand_command(state, ctx, given);
    let user = if name.is_empty() {
        None
    } else {
        User::from_name(&name).ok().flatten()
    };
    let Some(user) = user else {
        return Err(format!("unknown user: {name}"));
    };
    let uid = user.uid.as_raw();
    if uid == 0 || uid == nix::unistd::getuid().as_raw() {
        return Err(format!(
            "{} owns the server, can't change access",
            user.name
        ));
    }
    if c.add && c.deny {
        return Err("-a and -d cannot be used together".to_string());
    }
    if c.write && c.read_only {
        return Err("-r and -w cannot be used together".to_string());
    }
    if c.deny {
        if !acl_has(state, uid) {
            return Err(format!("user {} not found", user.name));
        }
        state.registry.acl_deny(uid);
        return Ok(String::new());
    }
    if c.add {
        if acl_has(state, uid) {
            return Err(format!("user {} is already added", user.name));
        }
        state.registry.acl_allow(uid);
    } else if (c.read_only || c.write) && !acl_has(state, uid) {
        // `-r` or `-w` implies `-a` for a user not yet added.
        state.registry.acl_allow(uid);
    }
    if c.write {
        state.registry.acl_set_read_only(uid, false);
    } else if c.read_only {
        state.registry.acl_set_read_only(uid, true);
    }
    Ok(String::new())
}

/// `show-prompt-history` and `clear-prompt-history`, for one prompt type
/// (`-T`) or every type.
pub fn prompt_history(
    state: &mut State,
    _ctx: &ClientCtx,
    c: PromptHistory,
) -> Result<String, String> {
    let kinds: Vec<usize> = match c.kind.as_deref() {
        Some(kind) => {
            vec![prompt::kind_index(kind).ok_or_else(|| format!("invalid type: {kind}"))?]
        }
        None => (0..prompt::KIND_NAMES.len()).collect(),
    };
    if c.clear {
        for kind in kinds {
            state.prompts.clear_history(kind);
        }
        return Ok(String::new());
    }
    let mut out = String::new();
    for kind in kinds {
        out.push_str(&format!("History for {}:\n\n", prompt::KIND_NAMES[kind]));
        for (i, line) in state.prompts.history(kind).iter().enumerate() {
            out.push_str(&format!("{}: {line}\n", i + 1));
        }
        out.push('\n');
    }
    Ok(out)
}
