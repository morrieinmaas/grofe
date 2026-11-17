//! The server side of remote sessions (spec 2026-11-15-ssh-remote §3, §4):
//! the stable ssh-agent link, `open-url`, listening ports and the `pane-echo`
//! hint.
//!
//! **Agent.** A session keeps `<runtime dir>/agent/<instance>-<n>.sock`
//! ([`paths::agent_link`]), a symlink to the `SSH_AUTH_SOCK` of the client
//! that last attached or created it, repointed by rename so a pane never
//! sees it missing. New panes get the link as their own `SSH_AUTH_SOCK`
//! ([`pane_env`]), so an old pane's agent survives a reconnect. The session's
//! own environment (`show-environment`) is left as `update-environment`
//! made it.
//!
//! **Polling.** `ports-changed` and `pane-echo` come from one poll every
//! [`POLL`], run only while a subscription names one of them: a task sends
//! the actor a [`Job`] per tick, so an idle server has no timer. The job
//! reads each pty's ECHO flag (one syscall) and hands the process trees to
//! `spawn_blocking`, which reads `/proc` (or libproc) off the actor and
//! sends the result back as another job.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::cmd::{ListPanes, ListPorts, OpenUrl, SetClientInfo};
use crate::compat::Identity;
use crate::event::Event;
use crate::paths;
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::{self, Owner};
use crate::server::pane::procinfo;
use crate::server::{Job, State, clients};
use crate::types::{PaneId, SessionId};

/// How often the poller looks: `ports-changed` is "at most every 2 s".
pub const POLL: Duration = Duration::from_secs(2);

/// What the poller last told subscribers, per pane.
#[derive(Default)]
pub struct Remote {
    running: bool,
    /// How many subscriptions named `ports-changed` or `pane-echo` at the
    /// last look; a rise means someone new needs the current state.
    subs: usize,
    /// Set by a new subscription: the next echo poll reports every pane.
    snapshot: bool,
    ports: BTreeMap<PaneId, Vec<u16>>,
    echo: BTreeMap<PaneId, bool>,
}

fn session_flag(state: &State, session: SessionId, name: &str) -> bool {
    state.model.identity != Identity::Tmux
        && state.model.options.lookup(name, &[Owner::Session(session)]) == Some("on")
}

/// `SSH_AUTH_SOCK` as `client_env` has it, if it names an absolute path.
fn client_agent(client_env: &[(String, String)]) -> Option<&str> {
    client_env
        .iter()
        .rev()
        .find(|(k, _)| k == "SSH_AUTH_SOCK")
        .map(|(_, v)| v.as_str())
        .filter(|v| Path::new(v).is_absolute())
}

/// A client's environment reached `session` (new-session, attach,
/// switch-client): points the session's agent link at the client's agent.
pub fn stable_agent(state: &State, session: SessionId, client_env: &[(String, String)]) {
    if !session_flag(state, session, "stable-ssh-agent") {
        return;
    }
    let Some(target) = client_agent(client_env) else {
        return;
    };
    let link = paths::agent_link(&state.socket, session.0);
    // A client started in one of our own panes already has the link.
    if Path::new(target) == link {
        return;
    }
    if let Err(e) = repoint(&link, Path::new(target)) {
        eprintln!("grofe server: ssh-agent link {}: {e}", link.display());
    }
}

/// Makes `link` a symlink to `target`: made under another name and renamed
/// over, so a reader sees the old target or the new one, never none.
fn repoint(link: &Path, target: &Path) -> io::Result<()> {
    if let Some(dir) = link.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let mut temp = link.as_os_str().to_os_string();
    temp.push(format!(".{}", std::process::id()));
    let temp = std::path::PathBuf::from(temp);
    let _ = std::fs::remove_file(&temp);
    symlink(target, &temp)?;
    std::fs::rename(&temp, link).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// `session` is gone: its agent link goes too.
pub fn session_closed(state: &State, session: SessionId) {
    let _ = std::fs::remove_file(paths::agent_link(&state.socket, session.0));
}

/// The server is exiting: every agent link of this instance goes.
pub fn remove_links(state: &State) {
    let link = paths::agent_link(&state.socket, 0);
    let Some(prefix) = link
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split_once('-'))
        .map(|(hash, _)| format!("{hash}-"))
    else {
        return;
    };
    let Ok(dir) = std::fs::read_dir(paths::agent_dir(&state.socket)) else {
        return;
    };
    for entry in dir.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The variables a new pane of `session` gets from the remote features,
/// added last so they win: the agent link (dangling until a client with an
/// agent attaches) and
/// `BROWSER`.
pub fn pane_env(state: &State, session: SessionId, env: &mut Vec<(String, String)>) {
    if session_flag(state, session, "stable-ssh-agent") {
        // Even before a client with an agent has made the link: a session
        // made over a control channel (`grofe -r host`) has none yet, and its
        // first pane must follow the attach that comes next.
        let link = paths::agent_link(&state.socket, session.0);
        env.push(("SSH_AUTH_SOCK".to_string(), link.display().to_string()));
    }
    if session_flag(state, session, "forward-browser")
        && let Some(shim) = OPEN_SHIM.get()
    {
        env.push(("BROWSER".to_string(), shim.display().to_string()));
    }
}

/// This server's `grofe-open` shim, set before it runs ([`install_open_shim`]).
static OPEN_SHIM: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The name of the shim `BROWSER` names.
pub const OPEN_SHIM_NAME: &str = "grofe-open";

/// Writes `dir/grofe-open`, a script that runs `exe open "$@"`, and makes it
/// the `BROWSER` of new panes. `BROWSER` must be one program: programs run
/// `"$BROWSER" "$url"`, and grofe may not be on the pane's `PATH` (a
/// bootstrap install is not).
pub fn install_open_shim(dir: &Path, exe: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let exe = exe.display().to_string().replace('\'', "'\\''");
    let shim = dir.join(OPEN_SHIM_NAME);
    let temp = dir.join(format!(".{OPEN_SHIM_NAME}.{}", std::process::id()));
    std::fs::write(&temp, format!("#!/bin/sh\nexec '{exe}' open \"$@\"\n"))?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&temp, &shim)?;
    let _ = OPEN_SHIM.set(shim);
    Ok(())
}

/// `open-url <url>`: tells the subscribers of `open-url` to open `url`.
/// Refused unless [`crate::open::check`] passes; fails with
/// [`crate::open::NO_SUBSCRIBER`] when nobody listens, so `grofe open` can
/// open it itself.
pub fn open_url(state: &mut State, ctx: &ClientCtx, c: OpenUrl) -> Result<String, String> {
    crate::open::check(&c.url).map_err(|e| format!("open-url: refused {:?}: {e}", c.url))?;
    if !state.events.named("open-url") {
        return Err(crate::open::NO_SUBSCRIBER.to_string());
    }
    state.events.emit(Event::OpenUrl {
        url: c.url,
        pane: ctx.pane,
    });
    Ok(String::new())
}

/// `set-client-info [-t client] <key> <value>`: stores what a remote client
/// says about itself (`rtt` in whole milliseconds, `machines` as text) for
/// `#{client_rtt}` and `#{client_machines}`, and redraws its status line.
pub fn set_client_info(
    state: &mut State,
    ctx: &ClientCtx,
    c: SetClientInfo,
) -> Result<String, String> {
    let id = clients::target_client(state, ctx, c.client.as_deref())?;
    let Some(client) = state.clients.get_mut(&id) else {
        return Err("no current client".to_string());
    };
    match c.key.as_str() {
        "rtt" => {
            let ms = c
                .value
                .parse::<u64>()
                .map_err(|_| format!("set-client-info: rtt is whole milliseconds: {}", c.value))?;
            client.info.rtt_ms = Some(ms);
        }
        "machines" => client.info.machines = Some(c.value),
        key => return Err(format!("set-client-info: unknown key: {key}")),
    }
    clients::refresh(state, id, true);
    Ok(String::new())
}

/// `list-ports [-a] [-t target]`: `<pane> <port>,<port>` for each pane of the
/// window (or of every window) whose processes listen on a TCP port.
pub fn list_ports(state: &mut State, ctx: &ClientCtx, c: ListPorts) -> Result<String, String> {
    let panes = ListPanes {
        target: c.target,
        format: None,
        all: c.all,
        session: false,
        filter: None,
        sort: Default::default(),
    };
    let pids: Vec<(PaneId, i32)> = options::listed_panes(state, ctx, &panes)?
        .into_iter()
        .filter_map(|(_, id)| Some((id, state.model.panes.get(&id)?.handle.pid()? as i32)))
        .collect();
    // The scan reads /proc (or libproc) for every listed pane: off the
    // actor, as the poller's, with the answer sent from a job.
    // ponytail: a deferred answer, so as with `run-shell`, output of commands
    // before it in the same request is not printed.
    let reply = crate::server::dispatch::defer_reply(state);
    let jobs = state.jobs.clone();
    tokio::task::spawn_blocking(move || {
        let roots: Vec<i32> = pids.iter().map(|(_, pid)| *pid).collect();
        let scanned = procinfo::listening_ports(&roots);
        let job: Job = Box::new(move |state| {
            let mut out = String::new();
            for ((id, _), ports) in pids.iter().zip(scanned) {
                if let Some(p) = state.model.panes.get(id) {
                    p.handle.set_ports(ports.clone());
                }
                if !ports.is_empty() {
                    out.push_str(&format!("{id} {}\n", join(&ports)));
                }
            }
            let _ = reply.send(Ok(out));
        });
        let _ = jobs.blocking_send(job);
    });
    Ok(String::new())
}

/// `#{pane_ports}`: the ports comma-separated.
pub fn join(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn wanted(state: &State) -> bool {
    state.events.named("ports-changed") || state.events.named("pane-echo")
}

/// After a `subscribe`: starts the poller if the subscription needs it.
pub fn ensure_poller(state: &mut State) {
    let subs = state.events.named_count("ports-changed") + state.events.named_count("pane-echo");
    let grew = subs > state.remote.subs;
    state.remote.subs = subs;
    if !wanted(state) {
        return;
    }
    if grew {
        // A new subscriber gets the state of every pane once: forget what
        // was told, so the next poll reports it all (older subscribers see
        // the same whole-state events again, which is harmless).
        state.remote.ports.clear();
        state.remote.echo.clear();
        state.remote.snapshot = true;
        if state.remote.running {
            poll(state);
        }
    }
    if state.remote.running {
        return;
    }
    state.remote.running = true;
    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        loop {
            let (tx, rx) = oneshot::channel();
            let job: Job = Box::new(move |state| {
                let _ = tx.send(poll(state));
            });
            if jobs.send(job).await.is_err() || !rx.await.unwrap_or(false) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

/// One poll, on the actor; false once nobody wants it (the task ends).
fn poll(state: &mut State) -> bool {
    if !wanted(state) {
        state.remote = Remote::default();
        return false;
    }
    let live = &state.model.panes;
    state.remote.echo.retain(|p, _| live.contains_key(p));
    state.remote.ports.retain(|p, _| live.contains_key(p));
    if state.events.named("pane-echo") {
        poll_echo(state);
    }
    if state.events.named("ports-changed") {
        poll_ports(state);
    }
    true
}

fn poll_echo(state: &mut State) {
    let mut changes = Vec::new();
    for (id, pane) in &state.model.panes {
        let Some(echo) = pane.handle.echo() else {
            continue;
        };
        // A pane not seen before echoes, as a pty does at birth, unless a
        // new subscriber asked for the state of every pane.
        let changed = match state.remote.echo.insert(*id, echo) {
            Some(before) => before != echo,
            None => state.remote.snapshot || !echo,
        };
        if changed {
            changes.push(Event::PaneEcho { pane: *id, echo });
        }
    }
    for event in changes {
        state.events.emit(event);
    }
    state.remote.snapshot = false;
}

fn poll_ports(state: &mut State) {
    let panes: Vec<(PaneId, Option<i32>)> = state
        .model
        .panes
        .iter()
        .map(|(id, p)| {
            (
                *id,
                p.dead
                    .is_none()
                    .then(|| p.handle.pid())
                    .flatten()
                    .map(|p| p as i32),
            )
        })
        .collect();
    let jobs = state.jobs.clone();
    tokio::task::spawn_blocking(move || {
        let roots: Vec<i32> = panes.iter().filter_map(|(_, pid)| *pid).collect();
        let mut scanned = procinfo::listening_ports(&roots).into_iter();
        let ports: Vec<(PaneId, Vec<u16>)> = panes
            .into_iter()
            .map(|(id, pid)| (id, pid.and_then(|_| scanned.next()).unwrap_or_default()))
            .collect();
        let job: Job = Box::new(move |state| apply_ports(state, ports));
        let _ = jobs.blocking_send(job);
    });
}

fn apply_ports(state: &mut State, ports: Vec<(PaneId, Vec<u16>)>) {
    for (pane, ports) in ports {
        if !state.model.panes.contains_key(&pane) {
            continue;
        }
        let old = state.remote.ports.insert(pane, ports.clone());
        if old.as_ref() != Some(&ports) && !(old.is_none() && ports.is_empty()) {
            state.events.emit(Event::PortsChanged { pane, ports });
        }
    }
}
