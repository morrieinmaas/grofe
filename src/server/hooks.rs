//! Event hooks (TC.5, spec §4.5 "Hooks and environment"): commands bound
//! to grofe events, run through the dispatcher with the event's target as
//! their context, plus the server environment `set-environment` keeps
//! (spec §4.2).
//!
//! **Events.** [`EVENTS`] lists grofe's event names, what fires each, and
//! the tmux hook FE.3 maps onto it. Each is emitted where it happens, with
//! the context tmux 3.7b gives its hook (recorded on a private socket):
//!
//! - the command-shaped ones (tmux's `after-*` hooks) by
//!   [`run_command`], which `dispatch` runs every command through; they run
//!   before the request's reply, as tmux's hold the CLI;
//! - the notify-shaped ones by the code that does the thing
//!   ([`session_created`], [`client_session_changed`], [`client_resized`],
//!   [`pane_exited`], and the model's `closed_sessions` for
//!   `session-closed`); they go through `State.jobs`, so they run after
//!   the reply, in the order they happened.
//!
//! Hooks run on one queue, in order, and a hook's `run-shell` without `-b`
//! holds the queue until its shell exits, as tmux's command queue does; a
//! CLI request whose `after-*` hook waits so gets its reply only then.
//! Commands a hook runs trigger no hooks, as in tmux.
//!
//! **Environment.** The global environment starts as the server process's
//! own, and is all a pane or `run-shell` gets from the server's. Hidden
//! (`-h`) variables never reach a process (recorded on tmux 3.7b); they,
//! like every global variable, expand in `$NAME` in a command string
//! (`cmd::split_command_string`, through [`Hooks::for_server`]). A
//! session's variables live in its `Session.env`, its hidden ones here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;

use crate::cmd::{self, Command, FormatVars, Resolve, Target};
use crate::config::Resolved;
use crate::protocol::{ClientCtx, Dialect};
use crate::server::dispatch::{self, options};
use crate::server::{Job, State};
use crate::types::{ClientId, PaneId, SessionId, WindowId};

/// One grofe event a hook can be bound to.
pub struct EventSpec {
    pub name: &'static str,
    /// What fires the event, for readers and FE.3.
    pub fires: &'static str,
    /// The tmux 3.7b hook FE.3's table maps onto this event.
    pub tmux: &'static str,
}

/// Every grofe event.
pub const EVENTS: &[EventSpec] = &[
    EventSpec {
        name: "pane-focused",
        fires: "select-pane made another pane active",
        tmux: "after-select-pane",
    },
    EventSpec {
        name: "window-focused",
        fires: "select-window succeeded, even on the current window",
        tmux: "after-select-window",
    },
    EventSpec {
        name: "window-created",
        fires: "new-window made a window",
        tmux: "after-new-window",
    },
    EventSpec {
        name: "pane-closed",
        fires: "kill-pane removed a pane",
        tmux: "after-kill-pane",
    },
    EventSpec {
        name: "pane-resized",
        fires: "resize-pane succeeded",
        tmux: "after-resize-pane",
    },
    EventSpec {
        name: "window-resized",
        fires: "resize-window succeeded",
        tmux: "after-resize-window",
    },
    EventSpec {
        name: "client-resized",
        fires: "a client attached, or its terminal reported a size",
        tmux: "client-resized",
    },
    EventSpec {
        name: "client-session-changed",
        fires: "a client attached, or switch-client or attach-session ran for it",
        tmux: "client-session-changed",
    },
    EventSpec {
        name: "session-created",
        fires: "new-session made a session",
        tmux: "session-created",
    },
    EventSpec {
        name: "session-closed",
        fires: "a session was removed",
        tmux: "session-closed",
    },
    EventSpec {
        name: "pane-exited",
        fires: "a pane's process exited and the pane was removed",
        tmux: "pane-exited",
    },
    EventSpec {
        name: "pane-died",
        fires: "a pane's process exited and remain-on-exit kept the pane",
        tmux: "pane-died",
    },
];

/// The event named `name`.
pub fn event(name: &str) -> Option<&'static EventSpec> {
    EVENTS.iter().find(|e| e.name == name)
}

/// A hook's command string, split when it was set (so `$NAME` expanded
/// then, as tmux 3.7b expands it), in the dialect of the request or config
/// file that set it.
#[derive(Clone)]
struct Stored {
    text: String,
    cmds: Vec<Vec<String>>,
    dialect: Dialect,
    /// Set by the config (`[hooks]`), so a reload may replace it.
    from_config: bool,
}

#[derive(Clone)]
struct Var {
    value: String,
    hidden: bool,
}

type Env = BTreeMap<String, Var>;

/// Bound hooks and the server environment (TC.5). Lives on `State.hooks`.
pub struct Hooks {
    bound: BTreeMap<&'static str, Stored>,
    /// Events `set-hook` set or unset: a reload leaves them alone (R10).
    runtime: BTreeSet<&'static str>,
    /// Notify hooks the job queue refused, run at the next [`run_pending`].
    overflow: Vec<Job>,
    /// Hook commands waiting to run, in order.
    queue: VecDeque<Queued>,
    /// Hooks are running: their commands trigger none.
    running: bool,
    /// The queue waits for a hook's `run-shell` to exit.
    waiting: bool,
    /// Told when the queue is next idle.
    idle: Vec<oneshot::Sender<()>>,
    /// See [`take_reply_hold`].
    reply_hold: Option<oneshot::Receiver<()>>,
    global: Arc<RwLock<Env>>,
    /// Hidden session variables (`set-environment -h -t`), by session.
    session_hidden: BTreeMap<SessionId, BTreeMap<String, String>>,
}

impl Default for Hooks {
    /// No hooks, and the global environment seeded from the process's.
    fn default() -> Hooks {
        let env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .map(|(k, value)| {
                let var = Var {
                    value,
                    hidden: false,
                };
                (k, var)
            })
            .collect();
        Hooks {
            bound: BTreeMap::new(),
            runtime: BTreeSet::new(),
            overflow: Vec::new(),
            queue: VecDeque::new(),
            running: false,
            waiting: false,
            idle: Vec::new(),
            reply_hold: None,
            global: Arc::new(RwLock::new(env)),
            session_hidden: BTreeMap::new(),
        }
    }
}

impl Hooks {
    /// The server's hooks: as [`Hooks::default`], and `$NAME` in command
    /// strings expands from this global environment from now on.
    pub fn for_server() -> Hooks {
        let hooks = Hooks::default();
        let global = Arc::clone(&hooks.global);
        cmd::parse::set_variable_source(Box::new(move |name| {
            let env = global.read().unwrap_or_else(PoisonError::into_inner);
            env.get(name).map(|v| v.value.clone())
        }));
        hooks
    }

    fn global(&self) -> std::sync::RwLockReadGuard<'_, Env> {
        self.global.read().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---- binding ----

/// A hook command string split and checked as `[hooks]` checks it: each
/// command must parse natively, with its formats (`-t #{hook_pane}` is not
/// a valid target until expanded) expanded to nothing. An empty string is
/// no commands (the hook is removed).
pub fn check_command(command: &str) -> Result<Vec<Vec<String>>, String> {
    if command.trim().is_empty() {
        return Ok(Vec::new());
    }
    let cmds = cmd::split_command_string(command).map_err(|e| e.to_string())?;
    for argv in &cmds {
        cmd::parse(&unformatted(argv)).map_err(|e| e.to_string())?;
    }
    Ok(cmds)
}

fn unformatted(argv: &[String]) -> Vec<String> {
    argv.iter().map(|w| cmd::expand(w, &NoVars)).collect()
}

/// Applies the config's `[hooks]`, at start and on each `reload`, after
/// its options: each event `set-hook` has not touched gets the config's
/// command, or none. The loader already checked every command, and a
/// reload with any error changes nothing (T3.5).
pub fn configure(state: &mut State, config: &Resolved) {
    let hooks = &mut state.hooks;
    let runtime = &hooks.runtime;
    hooks
        .bound
        .retain(|e, s| !s.from_config || runtime.contains(e));
    for (name, h) in &config.hooks {
        let Some(spec) = event(name) else {
            continue;
        };
        if hooks.runtime.contains(spec.name) || h.cmds.is_empty() {
            continue;
        }
        let stored = Stored {
            text: h.command.clone(),
            cmds: h.cmds.clone(),
            dialect: Dialect::Grofe,
            from_config: true,
        };
        hooks.bound.insert(spec.name, stored);
    }
}

/// Binds `event` to `command` (`set-hook -g`), replacing what was bound.
/// The string is split now and each command must parse (in the request's
/// dialect), as tmux checks at set time; formats in it expand when it runs.
pub fn bind(
    state: &mut State,
    ctx: &ClientCtx,
    event: &'static EventSpec,
    command: &str,
) -> Result<(), String> {
    let cmds = cmd::split_command_string(command).map_err(|e| e.to_string())?;
    if cmds.is_empty() {
        return Err("empty value".to_string());
    }
    for argv in &cmds {
        crate::compat::parse_split(ctx, &unformatted(argv))?;
    }
    let stored = Stored {
        text: command.to_string(),
        cmds,
        dialect: ctx.dialect,
        from_config: false,
    };
    state.hooks.bound.insert(event.name, stored);
    state.hooks.runtime.insert(event.name);
    Ok(())
}

/// Removes `event`'s hook (`set-hook -gu`); nothing bound is fine.
pub fn unbind(state: &mut State, event: &'static EventSpec) {
    state.hooks.bound.remove(event.name);
    state.hooks.runtime.insert(event.name);
}

/// `show-hooks -g`: `<event> <command>` for each bound event, or for
/// `name` alone (a bare `<event>` when nothing is bound to it).
pub fn show(state: &State, name: Option<&str>) -> Result<String, String> {
    let bound = &state.hooks.bound;
    let Some(name) = name else {
        return Ok(bound
            .iter()
            .map(|(event, s)| format!("{event} {}\n", s.text))
            .collect());
    };
    let spec = event(name).ok_or_else(|| format!("unknown hook: {name}"))?;
    Ok(match bound.get(spec.name) {
        Some(s) => format!("{} {}\n", spec.name, s.text),
        None => format!("{}\n", spec.name),
    })
}

// ---- emission ----

/// One event that happened, with the context its hook runs in.
struct Fired {
    event: &'static str,
    /// The pane the hook's formats and current target use; `None`, or a
    /// pane gone by the time it runs, means the current pane then.
    pane: Option<PaneId>,
    hook_pane: Option<PaneId>,
    hook_window: Option<WindowId>,
    hook_session_name: Option<String>,
    hook_client: Option<String>,
}

impl Fired {
    fn new(event: &'static str, pane: Option<PaneId>) -> Fired {
        Fired {
            event,
            pane,
            hook_pane: None,
            hook_window: None,
            hook_session_name: None,
            hook_client: None,
        }
    }
}

fn wanted(state: &State, name: &str) -> bool {
    !state.hooks.running && state.hooks.bound.contains_key(name)
}

/// A session's current window's active pane.
fn session_pane(state: &State, session: SessionId) -> Option<PaneId> {
    let s = state.model.sessions.get(&session)?;
    state.model.windows.get(&s.active).map(|w| w.active)
}

/// Queues a notify-shaped hook to run after the current reply; a full
/// queue keeps it for the next [`run_pending`].
fn notify(state: &mut State, f: Fired) {
    let job: Job = Box::new(move |state: &mut State| run_hook(state, &f, false));
    if let Err(TrySendError::Full(job) | TrySendError::Closed(job)) = state.jobs.try_send(job) {
        state.hooks.overflow.push(job);
    }
}

/// `session-created`: new-session made `session`. Its pane is the new
/// session's.
pub fn session_created(state: &mut State, session: SessionId) {
    if !wanted(state, "session-created") {
        return;
    }
    let mut f = Fired::new("session-created", session_pane(state, session));
    f.hook_session_name = state.model.sessions.get(&session).map(|s| s.name.clone());
    notify(state, f);
}

/// `client-session-changed` for client `id`, which now shows its session
/// (attach, switch-client, attach-session; tmux fires it even when the
/// session is the same one).
pub fn client_session_changed(state: &mut State, id: ClientId) {
    client_event(state, id, "client-session-changed");
}

/// `client-resized`: client `id` attached or reported a size (tmux fires
/// it for every report, even of the same size).
pub fn client_resized(state: &mut State, id: ClientId) {
    client_event(state, id, "client-resized");
}

fn client_event(state: &mut State, id: ClientId, name: &'static str) {
    if !wanted(state, name) {
        return;
    }
    let Some(client) = state.clients.get(&id) else {
        return;
    };
    let mut f = Fired::new(name, session_pane(state, client.session));
    f.hook_client = Some(client.name.clone());
    notify(state, f);
}

/// A pane's process exited and the model has handled it: `pane-died` if
/// remain-on-exit kept the pane (its own pane as context), else
/// `pane-exited` (the current pane as context, the gone one as
/// `#{hook_pane}`).
pub fn pane_exited(state: &mut State, pane: PaneId, window: WindowId) {
    let kept = state.model.panes.contains_key(&pane);
    let name = if kept { "pane-died" } else { "pane-exited" };
    if !wanted(state, name) {
        return;
    }
    let mut f = Fired::new(name, kept.then_some(pane));
    f.hook_pane = Some(pane);
    f.hook_window = Some(window);
    notify(state, f);
}

/// Runs what is pending after a command sequence (before its reply) or a
/// pane event: `session-closed` for each session the model removed (the
/// current pane as context), and hooks a full job queue refused.
pub fn run_pending(state: &mut State) {
    let closed = std::mem::take(&mut state.model.closed_sessions);
    if state.hooks.running {
        return;
    }
    for (_, name) in closed {
        if wanted(state, "session-closed") {
            let mut f = Fired::new("session-closed", None);
            f.hook_session_name = Some(name);
            notify(state, f);
        }
    }
    for job in std::mem::take(&mut state.hooks.overflow) {
        job(state);
    }
}

/// What a command-shaped hook needs from before its command ran.
enum Before {
    /// select-pane: the window and its active pane.
    SelectPane(WindowId, PaneId),
    /// select-window: the target window's session.
    SelectWindow(SessionId),
    /// new-window: the highest window id so far.
    NewWindow(Option<WindowId>),
    KillPane,
    ResizePane(PaneId),
    ResizeWindow(WindowId),
}

impl Before {
    fn of(state: &State, ctx: &ClientCtx, cmd: &Command) -> Option<(&'static str, Before)> {
        let m = &state.model;
        let before = match cmd {
            Command::SelectPane(c) => {
                let pane = m.resolve_pane(&c.target, ctx).ok()?;
                let window = m.panes.get(&pane)?.window;
                let active = m.windows.get(&window)?.active;
                ("pane-focused", Before::SelectPane(window, active))
            }
            Command::SelectWindow(c) => {
                let window = m.resolve_window(&c.target, ctx).ok()?;
                let session = m.windows.get(&window)?.session;
                ("window-focused", Before::SelectWindow(session))
            }
            Command::NewWindow(_) => (
                "window-created",
                Before::NewWindow(m.windows.keys().next_back().copied()),
            ),
            Command::KillPane(_) => ("pane-closed", Before::KillPane),
            Command::ResizePane(c) => (
                "pane-resized",
                Before::ResizePane(m.resolve_pane(&c.target, ctx).ok()?),
            ),
            Command::ResizeWindow(c) => (
                "window-resized",
                Before::ResizeWindow(m.resolve_window(&c.target, ctx).ok()?),
            ),
            _ => return None,
        };
        Some(before)
    }

    /// The hook's pane once its command succeeded, as tmux 3.7b gives it;
    /// `None` when the hook does not fire.
    fn pane(self, state: &State, ctx: &ClientCtx) -> Option<Option<PaneId>> {
        let m = &state.model;
        let active = |w: WindowId| m.windows.get(&w).map(|w| w.active);
        Some(match self {
            Before::SelectPane(window, was) => {
                let now = active(window)?;
                if now == was {
                    return None;
                }
                Some(now)
            }
            Before::SelectWindow(session) => session_pane(state, session),
            Before::NewWindow(last) => {
                let new = m.windows.keys().copied().find(|w| Some(*w) > last)?;
                active(new)
            }
            // The pane is gone: the current pane, when the hook runs.
            Before::KillPane => ctx.pane.filter(|p| m.panes.contains_key(p)),
            Before::ResizePane(pane) => Some(pane),
            Before::ResizeWindow(window) => active(window),
        })
    }
}

/// Runs one command through `route` and then, if it succeeded, its
/// command-shaped hook (tmux's `after-<verb>`), before the request goes on.
pub fn run_command(
    state: &mut State,
    ctx: &ClientCtx,
    cmd: Command,
    route: fn(&mut State, &ClientCtx, Command) -> Result<String, String>,
) -> Result<String, String> {
    let before = Before::of(state, ctx, &cmd).filter(|(name, _)| wanted(state, name));
    let result = route(state, ctx, cmd);
    if result.is_ok()
        && let Some((name, before)) = before
        && let Some(pane) = before.pane(state, ctx)
    {
        run_hook(state, &Fired::new(name, pane), true);
        // A request from the CLI waits for its hooks (a binding has no
        // reply to hold).
        if state.hooks.waiting && ctx.client.is_none() && state.hooks.reply_hold.is_none() {
            let (tx, rx) = oneshot::channel();
            state.hooks.idle.push(tx);
            state.hooks.reply_hold = Some(rx);
        }
    }
    result
}

/// A hook's commands, expanded, waiting to run in order on the hook queue.
struct Queued {
    event: &'static str,
    ctx: ClientCtx,
    cmds: VecDeque<Vec<String>>,
}

/// Queues the hook bound to `f`'s event (its formats expanded now,
/// against the event, with the event's pane as the current one), at the
/// front for a command-shaped hook, which runs next, and at the back for
/// a notify-shaped one; then runs the queue.
fn run_hook(state: &mut State, f: &Fired, front: bool) {
    if state.hooks.running {
        return;
    }
    let Some(stored) = state.hooks.bound.get(f.event).cloned() else {
        return;
    };
    let pane = context_pane(state, f.pane);
    let cwd = pane
        .and_then(|p| state.model.panes.get(&p))
        .and_then(|p| state.model.windows.get(&p.window))
        .and_then(|w| state.model.sessions.get(&w.session))
        .map_or_else(|| PathBuf::from("/"), |s| s.cwd.clone());
    let ctx = ClientCtx {
        cwd,
        pane,
        env: Vec::new(),
        client: None,
        dialect: stored.dialect,
    };
    let cmds = {
        let formats = pane.map(|p| options::FormatCtx::new(&state.model, p));
        let server = match (pane, &formats) {
            (Some(p), Some(inner)) => Some(options::ServerVars::new(state, p, inner)),
            _ => None,
        };
        let vars = HookVars {
            fired: f,
            inner: server.as_ref().map(|s| s as &dyn FormatVars),
        };
        stored
            .cmds
            .iter()
            .map(|argv| argv.iter().map(|w| cmd::expand(w, &vars)).collect())
            .collect()
    };
    let queued = Queued {
        event: f.event,
        ctx,
        cmds,
    };
    if front {
        state.hooks.queue.push_front(queued);
    } else {
        state.hooks.queue.push_back(queued);
    }
    drain(state);
}

/// Runs queued hook commands in order, as tmux's command queue does: a
/// `run-shell` without `-b` holds everything after it (later commands and
/// later hooks) until its shell exits. The first error ends a hook's
/// commands, and goes to the server log.
fn drain(state: &mut State) {
    if state.hooks.running || state.hooks.waiting {
        return;
    }
    while let Some(mut q) = state.hooks.queue.pop_front() {
        state.hooks.running = true;
        // The hook's `run-shell` answers here, not to the request that
        // triggered it; what its commands close fires nothing.
        let deferred = state.deferred_reply.take();
        let closed = std::mem::take(&mut state.model.closed_sessions);
        let mut shell = None;
        while let Some(argv) = q.cmds.pop_front() {
            if let Err(e) = dispatch::run_split(state, &q.ctx, &argv) {
                eprintln!("grofe hook {}: {e}", q.event);
                q.cmds.clear();
            }
            if let Some(rx) = state.deferred_reply.take() {
                shell = Some(rx);
                break;
            }
        }
        state.deferred_reply = deferred;
        state.model.closed_sessions = closed;
        state.hooks.running = false;
        if let Some(rx) = shell {
            if !q.cmds.is_empty() {
                state.hooks.queue.push_front(q);
            }
            state.hooks.waiting = true;
            let jobs = state.jobs.clone();
            tokio::spawn(async move {
                let _ = rx.await;
                let resume: Job = Box::new(|state: &mut State| {
                    state.hooks.waiting = false;
                    drain(state);
                });
                let _ = jobs.send(resume).await;
            });
            return;
        }
    }
    for idle in std::mem::take(&mut state.hooks.idle) {
        let _ = idle.send(());
    }
}

/// For the reply to the request that just ran (`finish`): a receiver that
/// fires once the hook queue is idle, when one of its command-shaped
/// hooks is still waiting on a shell (tmux 3.7b holds the CLI until its
/// `after-*` hooks finish).
pub fn take_reply_hold(state: &mut State) -> Option<oneshot::Receiver<()>> {
    state.hooks.reply_hold.take()
}

/// `pane` if it is still there, else the current pane: with no client and
/// no pane, the most recent session's (tmux's `cmd_find_best_session`).
fn context_pane(state: &State, pane: Option<PaneId>) -> Option<PaneId> {
    let model = &state.model;
    pane.filter(|p| model.panes.contains_key(p)).or_else(|| {
        let ctx = ClientCtx {
            cwd: PathBuf::from("/"),
            pane: None,
            env: Vec::new(),
            client: None,
            dialect: Dialect::Grofe,
        };
        model.resolve_pane(&Target::Current, &ctx).ok()
    })
}

/// A hook's own variables (`#{hook}`, `#{hook_pane}`, `#{hook_window}`,
/// `#{hook_session_name}`, `#{hook_client}`: set only where tmux 3.7b sets
/// them, else empty), over its pane's formats.
struct HookVars<'a> {
    fired: &'a Fired,
    inner: Option<&'a dyn FormatVars>,
}

impl FormatVars for HookVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let f = self.fired;
        let text = |v: Option<String>| Some(v.unwrap_or_default());
        match name {
            "hook" => Some(f.event.to_string()),
            "hook_pane" => text(f.hook_pane.map(|p| p.to_string())),
            "hook_window" => text(f.hook_window.map(|w| w.to_string())),
            "hook_session_name" => text(f.hook_session_name.clone()),
            "hook_client" => text(f.hook_client.clone()),
            _ => self.inner?.get(name),
        }
    }
}

struct NoVars;

impl FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

// ---- environment ----

/// The server environment a new or respawned pane of `session` starts
/// with, under the session's own: every global variable but the hidden
/// ones. A pane gets nothing else from the server's own environment, so
/// `set-environment -gu` removes even a variable the server started with.
pub fn pane_env(state: &State, _session: SessionId) -> Vec<(String, String)> {
    visible(&state.hooks.global())
}

/// The whole environment a shell the server runs for `session` gets
/// (`run-shell`, popups, status sources): the global environment, then
/// the session's, both without hidden variables, as tmux 3.7b's
/// `run-shell`.
pub fn shell_env(state: &State, session: Option<SessionId>) -> Vec<(String, String)> {
    let mut env = visible(&state.hooks.global());
    if let Some(s) = session.and_then(|s| state.model.sessions.get(&s)) {
        env.extend(s.env.iter().cloned());
    }
    env
}

fn visible(env: &Env) -> Vec<(String, String)> {
    env.iter()
        .filter(|(_, v)| !v.hidden)
        .map(|(k, v)| (k.clone(), v.value.clone()))
        .collect()
}

/// `set-environment -g [-h] name value`, or `-gu name` (`value` `None`).
pub fn set_global(state: &mut State, name: &str, value: Option<String>, hidden: bool) {
    let mut env = state
        .hooks
        .global
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    match value {
        Some(value) => {
            env.insert(name.to_string(), Var { value, hidden });
        }
        None => {
            env.remove(name);
        }
    }
}

/// `set-environment -t session [-h] name value`, or `-u` (`value` `None`).
pub fn set_session(
    state: &mut State,
    session: SessionId,
    name: &str,
    value: Option<String>,
    hidden: bool,
) {
    let live = &state.model.sessions;
    state
        .hooks
        .session_hidden
        .retain(|s, _| live.contains_key(s));
    let Some(s) = state.model.sessions.get_mut(&session) else {
        return;
    };
    s.env.retain(|(k, _)| k != name);
    let hidden_vars = state.hooks.session_hidden.entry(session).or_default();
    hidden_vars.remove(name);
    match value {
        Some(value) if hidden => {
            hidden_vars.insert(name.to_string(), value);
        }
        Some(value) => s.env.push((name.to_string(), value)),
        None => {}
    }
}

/// Every variable of the global environment (`session` `None`) or of a
/// session, by name, with whether it is hidden. A session variable set
/// twice shows its last value.
pub fn variables(state: &State, session: Option<SessionId>) -> Vec<(String, String, bool)> {
    let Some(session) = session else {
        return state
            .hooks
            .global()
            .iter()
            .map(|(k, v)| (k.clone(), v.value.clone(), v.hidden))
            .collect();
    };
    let mut vars: BTreeMap<&str, (&str, bool)> = BTreeMap::new();
    if let Some(s) = state.model.sessions.get(&session) {
        for (k, v) in &s.env {
            vars.insert(k, (v, false));
        }
    }
    if let Some(hidden) = state.hooks.session_hidden.get(&session) {
        for (k, v) in hidden {
            vars.insert(k, (v, true));
        }
    }
    vars.into_iter()
        .map(|(k, (v, h))| (k.to_string(), v.to_string(), h))
        .collect()
}

/// An empty global environment, for tests that list it.
#[cfg(test)]
pub(crate) fn clear_environment(state: &mut State) {
    state
        .hooks
        .global
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{test_ctx, test_state};

    fn fire(state: &mut State, argv: &[&str]) -> Result<String, String> {
        let argv: Vec<String> = argv.iter().map(|w| w.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv)
    }

    #[test]
    fn every_event_names_a_distinct_tmux_hook() {
        let mut tmux: Vec<&str> = EVENTS.iter().map(|e| e.tmux).collect();
        tmux.sort();
        tmux.dedup();
        assert_eq!(tmux.len(), EVENTS.len());
        assert_eq!(EVENTS.len(), 12);
    }

    #[test]
    fn a_hook_command_must_parse_and_formats_may_be_targets() {
        let mut state = test_state();
        let ctx = test_ctx();
        let spec = event("pane-focused").unwrap();
        assert_eq!(
            bind(&mut state, &ctx, spec, "no-such-verb"),
            Err("unknown command: no-such-verb".to_string())
        );
        assert_eq!(
            bind(&mut state, &ctx, spec, ""),
            Err("empty value".to_string())
        );
        let cmd = "select-pane -t #{hook_pane} ; list-sessions";
        assert_eq!(bind(&mut state, &ctx, spec, cmd), Ok(()));
        assert_eq!(show(&state, None), Ok(format!("pane-focused {cmd}\n")));
        assert_eq!(check_command(cmd).map(|c| c.len()), Ok(2));
        assert_eq!(check_command("  "), Ok(Vec::new()));
    }

    #[tokio::test]
    async fn a_session_hook_runs_once_and_its_commands_trigger_none() {
        let mut state = test_state();
        // The hook creates a session itself: that must not fire it again.
        let hook = "new-session -d -s from-hook";
        fire(&mut state, &["set-hook", "-g", "session-created", hook]).unwrap();
        fire(&mut state, &["new-session", "-d", "-s", "a"]).unwrap();
        let names: Vec<&str> = state
            .model
            .sessions
            .values()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"from-hook"));
    }

    #[test]
    fn hidden_variables_stay_out_of_processes() {
        let mut state = test_state();
        set_global(&mut state, "GROFE_TC5_A", Some("1".into()), false);
        set_global(&mut state, "GROFE_TC5_H", Some("2".into()), true);
        let env = shell_env(&state, None);
        assert!(env.contains(&("GROFE_TC5_A".to_string(), "1".to_string())));
        assert!(!env.iter().any(|(k, _)| k == "GROFE_TC5_H"));
        set_global(&mut state, "GROFE_TC5_A", None, false);
        assert!(
            !pane_env(&state, SessionId(0))
                .iter()
                .any(|(k, _)| k == "GROFE_TC5_A")
        );
        let vars = variables(&state, None);
        assert!(vars.contains(&("GROFE_TC5_H".to_string(), "2".to_string(), true)));
    }
}
