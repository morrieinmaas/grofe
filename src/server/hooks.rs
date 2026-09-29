//! Event hooks (TC.5, spec §4.5 "Hooks and environment"): commands bound
//! to grofe events, run through the dispatcher with the event's target as
//! their context, plus the server environment `set-environment` keeps
//! (spec §4.2).
//!
//! **Events.** [`EVENTS`] lists grofe's event names, what each means, and
//! the tmux hook FE.3 maps onto it. An event is found by comparing the
//! model after a command sequence or a pane event with the model the last
//! check saw ([`run_pending`]), so it names what changed, not which command
//! changed it.
//!
//! **Timing.** Command-shaped events (tmux's `after-*` hooks) run their
//! hook inside [`run_pending`], before the triggering request's reply goes
//! out, as tmux 3.7b's `after-*` hooks hold the CLI. Notify-shaped events
//! (tmux's `session-created`, `client-resized` and the like) fire after the
//! reply: they are sent through `State.jobs`, and run inline only when the
//! queue is full. Commands a hook runs trigger no hooks of their own, as in
//! tmux.
//!
//! **Environment.** The global environment starts as the server process's
//! own. Hidden (`-h`) variables never reach a process (a new pane or
//! `run-shell`'s shell, recorded on tmux 3.7b); they, like every global
//! variable, expand in `$NAME` in a command string
//! (`cmd::split_command_string`, through [`Hooks::for_server`]). A
//! session's variables live in its `Session.env`, its hidden ones here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use tokio::sync::mpsc::error::TrySendError;

use crate::cmd::{self, FormatVars, Resolve, Target};
use crate::config::Resolved;
use crate::protocol::{ClientCtx, Dialect};
use crate::server::dispatch::{self, options};
use crate::server::pane::PaneEventKind;
use crate::server::{Job, State};
use crate::types::{ClientId, PaneId, SessionId, WindowId};

/// One grofe event a hook can be bound to.
pub struct EventSpec {
    pub name: &'static str,
    /// What the event means, for `show-hooks` readers and FE.3.
    pub fires: &'static str,
    /// The tmux 3.7b hook FE.3's table maps onto this event.
    pub tmux: &'static str,
    /// Fired after the triggering reply (tmux's notify hooks), not before.
    notify: bool,
}

/// Every grofe event, in the order hooks for one change run.
pub const EVENTS: &[EventSpec] = &[
    EventSpec {
        name: "session-created",
        fires: "a session was created",
        tmux: "session-created",
        notify: true,
    },
    EventSpec {
        name: "window-created",
        fires: "a window was added to an existing session",
        tmux: "after-new-window",
        notify: false,
    },
    EventSpec {
        name: "window-focused",
        fires: "a session's current window became another existing window",
        tmux: "after-select-window",
        notify: false,
    },
    EventSpec {
        name: "pane-focused",
        fires: "a window's active pane became another existing pane",
        tmux: "after-select-pane",
        notify: false,
    },
    EventSpec {
        name: "window-resized",
        fires: "a window changed size",
        tmux: "after-resize-window",
        notify: false,
    },
    EventSpec {
        name: "pane-resized",
        fires: "a window's panes changed size while the window did not (once per window)",
        tmux: "after-resize-pane",
        notify: false,
    },
    EventSpec {
        name: "client-resized",
        fires: "an attached client's terminal changed size",
        tmux: "client-resized",
        notify: true,
    },
    EventSpec {
        name: "client-session-changed",
        fires: "an attached client switched to another session",
        tmux: "client-session-changed",
        notify: true,
    },
    EventSpec {
        name: "pane-died",
        fires: "a pane's process exited and remain-on-exit kept the pane",
        tmux: "pane-died",
        notify: true,
    },
    EventSpec {
        name: "pane-exited",
        fires: "a pane's process exited and the pane was removed",
        tmux: "pane-exited",
        notify: true,
    },
    EventSpec {
        name: "pane-closed",
        fires: "a command removed a pane (its window stays, or it was the window's only pane)",
        tmux: "after-kill-pane",
        notify: false,
    },
    EventSpec {
        name: "session-closed",
        fires: "a session was removed",
        tmux: "session-closed",
        notify: true,
    },
];

/// The event named `name`.
pub fn event(name: &str) -> Option<&'static EventSpec> {
    EVENTS.iter().find(|e| e.name == name)
}

/// How often the model is checked for changes no command or pane event
/// reports (a client's resize, applied on a render tick), while any hook
/// is bound.
// ponytail: polling; the upgrade is a `run_pending` call after
// `clients::tick` applies sizes and after every `Job`.
const POLL: Duration = Duration::from_millis(200);

/// A hook's command string, split when it was set (so `$NAME` expanded
/// then, as tmux 3.7b expands it), in the dialect of the request that set
/// it.
#[derive(Clone)]
struct Stored {
    text: String,
    cmds: Vec<Vec<String>>,
    dialect: Dialect,
}

#[derive(Clone)]
struct Var {
    value: String,
    hidden: bool,
}

type Env = BTreeMap<String, Var>;

/// Bound hooks and the server environment (TC.5). Lives on `State.hooks`.
#[derive(Default)]
pub struct Hooks {
    bound: BTreeMap<&'static str, Stored>,
    /// The model as the last check saw it; `None` while no hook is bound.
    snapshot: Option<Snapshot>,
    /// Panes whose process exit the actor just handled ([`pane_event`]).
    exited: Vec<PaneId>,
    /// The last pane event changed nothing a hook watches (output, title).
    skip: bool,
    /// Hooks are running: their commands trigger none.
    running: bool,
    /// Running on the actor after a reply (a job): notify events run now.
    in_job: bool,
    polling: bool,
    global: Arc<RwLock<Env>>,
    /// Hidden session variables (`set-environment -h -t`), by session.
    session_hidden: BTreeMap<SessionId, BTreeMap<String, String>>,
}

impl Hooks {
    /// The server's hooks: the global environment starts as the process's
    /// own, and `$NAME` in command strings expands from it from now on.
    pub fn for_server() -> Hooks {
        let hooks = Hooks::default();
        {
            let mut env = hooks.global.write().unwrap_or_else(PoisonError::into_inner);
            for (name, value) in std::env::vars_os() {
                if let (Ok(name), Ok(value)) = (name.into_string(), value.into_string()) {
                    env.insert(
                        name,
                        Var {
                            value,
                            hidden: false,
                        },
                    );
                }
            }
        }
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

/// Applies the config's hooks (`[hooks]`), at start and on each `reload`,
/// after its options (TC.5).
// ponytail: the config schema has no `[hooks]` table yet (it is in
// `src/config`, outside TC.5's files); hooks are set at runtime only.
pub fn configure(_state: &mut State, _config: &Resolved) {}

// ---- hooks ----

/// Binds `event` to `command` (`set-hook -g`), replacing what was bound.
/// The string is split now and each command must parse, as tmux checks
/// at set time; formats in it are expanded when it runs.
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
        // Formats (`-t #{hook_pane}`) are not valid targets until expanded.
        let argv: Vec<String> = argv.iter().map(|w| cmd::expand(w, &NoVars)).collect();
        crate::compat::parse_split(ctx, &argv)?;
    }
    state.hooks.bound.insert(
        event.name,
        Stored {
            text: command.to_string(),
            cmds,
            dialect: ctx.dialect,
        },
    );
    start_polling(state);
    Ok(())
}

/// Removes `event`'s hook (`set-hook -gu`); nothing bound is fine.
pub fn unbind(state: &mut State, event: &'static EventSpec) {
    state.hooks.bound.remove(event.name);
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

/// Notes a pane event the actor is handling, before [`run_pending`] runs
/// for it: an exit (`pane-exited` rather than `pane-closed`), or an event
/// that changes nothing a hook watches.
pub fn pane_event(hooks: &mut Hooks, pane: PaneId, kind: &PaneEventKind) {
    if matches!(kind, PaneEventKind::Exited(_)) {
        hooks.exited.push(pane);
    } else {
        hooks.skip = true;
    }
}

/// Runs the hooks for what just happened. Called once after every command
/// sequence (`dispatch`, before its reply goes out) and after every pane
/// event, before their events are flushed to subscribers.
pub fn run_pending(state: &mut State) {
    let skip = std::mem::take(&mut state.hooks.skip);
    let exited = std::mem::take(&mut state.hooks.exited);
    if state.hooks.running {
        return;
    }
    if state.hooks.bound.is_empty() {
        state.hooks.snapshot = None;
        return;
    }
    if skip && exited.is_empty() && state.hooks.snapshot.is_some() {
        return;
    }
    let now = Snapshot::of(state);
    let fired = match &state.hooks.snapshot {
        Some(before) => diff(before, &now, &exited),
        None => Vec::new(),
    };
    state.hooks.snapshot = Some(now);
    let mut ran = false;
    for f in fired {
        if !state.hooks.bound.contains_key(f.event.name) {
            continue;
        }
        if f.event.notify && !state.hooks.in_job {
            defer(state, f);
        } else {
            run_hook(state, &f);
            ran = true;
        }
    }
    // What the hooks themselves changed fires nothing.
    if ran && state.hooks.snapshot.is_some() {
        state.hooks.snapshot = Some(Snapshot::of(state));
    }
}

/// Runs a notify-shaped hook after the reply, through the job queue; now,
/// if the queue is full.
fn defer(state: &mut State, f: Fired) {
    let job: Job = Box::new(move |state: &mut State| {
        run_hook(state, &f);
        if state.hooks.snapshot.is_some() {
            state.hooks.snapshot = Some(Snapshot::of(state));
        }
    });
    match state.jobs.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(job) | TrySendError::Closed(job)) => job(state),
    }
}

/// Starts the [`POLL`] task, once, when a hook is first bound on a
/// running server.
fn start_polling(state: &mut State) {
    if state.hooks.polling || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    state.hooks.polling = true;
    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            let job: Job = Box::new(|state: &mut State| {
                state.hooks.in_job = true;
                run_pending(state);
                state.hooks.in_job = false;
            });
            if jobs.send(job).await.is_err() {
                break;
            }
        }
    });
}

/// Runs the hook bound to `f`'s event: each command, its formats expanded
/// against the event, through the dispatcher with the event's pane as the
/// current one. The first error stops it, and goes to the server log.
// ponytail: a hook's `run-shell` (without `-b`) runs off the actor and
// does not hold the triggering reply, which tmux 3.7b's `after-*` hooks
// do; its output is dropped.
fn run_hook(state: &mut State, f: &Fired) {
    let Some(stored) = state.hooks.bound.get(f.event.name).cloned() else {
        return;
    };
    let pane = context_pane(state, f);
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
    let argvs: Vec<Vec<String>> = {
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
    state.hooks.running = true;
    // A hook's `run-shell` must not take over the triggering request's
    // deferred reply.
    let deferred = state.deferred_reply.take();
    for argv in argvs {
        if let Err(e) = dispatch::run_split(state, &ctx, &argv) {
            eprintln!("grofe hook {}: {e}", f.event.name);
            break;
        }
    }
    state.deferred_reply = deferred;
    state.hooks.running = false;
}

/// The pane a hook runs against: the event's own if it is still there,
/// else what tmux 3.7b gives (recorded: `after-kill-pane` and
/// `pane-exited` see the window's pane left active, `session-closed` the
/// most recent session's): the event window's active pane, its session's,
/// then the current one.
fn context_pane(state: &State, f: &Fired) -> Option<PaneId> {
    let model = &state.model;
    let live = |p: PaneId| model.panes.contains_key(&p).then_some(p);
    f.pane
        .and_then(live)
        .or_else(|| {
            f.window
                .and_then(|w| model.windows.get(&w))
                .map(|w| w.active)
        })
        .or_else(|| {
            let s = model.sessions.get(&f.session?)?;
            model.windows.get(&s.active).map(|w| w.active)
        })
        .or_else(|| {
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
/// `#{hook_session}`, `#{hook_session_name}`, `#{hook_client}`), over its
/// pane's formats.
struct HookVars<'a> {
    fired: &'a Fired,
    inner: Option<&'a dyn FormatVars>,
}

impl FormatVars for HookVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let f = self.fired;
        match name {
            "hook" => Some(f.event.name.to_string()),
            "hook_pane" => f.pane.map(|p| p.to_string()),
            "hook_window" => f.window.map(|w| w.to_string()),
            "hook_session" => f.session.map(|s| s.to_string()),
            "hook_session_name" => f.session_name.clone(),
            "hook_client" => f.client.map(|c| c.0.to_string()),
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

// ---- finding events ----

/// One event that happened, with what it happened to.
#[derive(Clone)]
struct Fired {
    event: &'static EventSpec,
    pane: Option<PaneId>,
    window: Option<WindowId>,
    session: Option<SessionId>,
    session_name: Option<String>,
    client: Option<ClientId>,
}

struct SessionSnap {
    name: String,
    active: WindowId,
}

struct WindowSnap {
    session: SessionId,
    active: PaneId,
    size: (u16, u16),
    panes: Vec<PaneId>,
}

struct PaneSnap {
    window: WindowId,
    size: (u16, u16),
    dead: bool,
}

struct ClientSnap {
    session: SessionId,
    size: (u16, u16),
}

/// What the hooks watch in the model and the clients.
#[derive(Default)]
struct Snapshot {
    sessions: BTreeMap<SessionId, SessionSnap>,
    windows: BTreeMap<WindowId, WindowSnap>,
    panes: BTreeMap<PaneId, PaneSnap>,
    clients: BTreeMap<ClientId, ClientSnap>,
}

impl Snapshot {
    fn of(state: &State) -> Snapshot {
        let m = &state.model;
        Snapshot {
            sessions: m
                .sessions
                .iter()
                .map(|(id, s)| {
                    let snap = SessionSnap {
                        name: s.name.clone(),
                        active: s.active,
                    };
                    (*id, snap)
                })
                .collect(),
            windows: m
                .windows
                .iter()
                .map(|(id, w)| {
                    let snap = WindowSnap {
                        session: w.session,
                        active: w.active,
                        size: (w.cols, w.rows),
                        panes: w.pane_ids(),
                    };
                    (*id, snap)
                })
                .collect(),
            panes: m
                .panes
                .iter()
                .map(|(id, p)| {
                    let snap = PaneSnap {
                        window: p.window,
                        size: (p.cols, p.rows),
                        dead: p.dead.is_some(),
                    };
                    (*id, snap)
                })
                .collect(),
            clients: state
                .clients
                .iter()
                .map(|(id, c)| {
                    let snap = ClientSnap {
                        session: c.session,
                        size: c.size,
                    };
                    (*id, snap)
                })
                .collect(),
        }
    }

    /// A session's active window's active pane.
    fn session_pane(&self, session: SessionId) -> Option<PaneId> {
        let s = self.sessions.get(&session)?;
        self.windows.get(&s.active).map(|w| w.active)
    }

    fn session_name(&self, session: SessionId) -> Option<String> {
        self.sessions.get(&session).map(|s| s.name.clone())
    }
}

/// The events between `old` and `new`, in [`EVENTS`] order. `exited`:
/// panes whose process exit caused this check.
// ponytail: found from the model, not the commands that ran. So a no-op
// `select-pane` fires nothing (tmux 3.7b fires nothing either); a
// `pane-resized` names the window's active pane when it changed size,
// though `resize-pane -t` named another; `window-resized` also fires when
// a client's size resizes the window; `pane-closed` also fires for
// `kill-window` on a one-pane window; `select-layout` fires
// `pane-resized`; `break-pane` fires `window-created`.
fn diff(old: &Snapshot, new: &Snapshot, exited: &[PaneId]) -> Vec<Fired> {
    let mut fired = Vec::new();
    let mut push = |name: &str, pane, window, session: Option<SessionId>, client| {
        let Some(spec) = event(name) else {
            return;
        };
        let session_name =
            session.and_then(|s| new.session_name(s).or_else(|| old.session_name(s)));
        fired.push(Fired {
            event: spec,
            pane,
            window,
            session,
            session_name,
            client,
        });
    };

    for id in new
        .sessions
        .keys()
        .filter(|s| !old.sessions.contains_key(s))
    {
        push(
            "session-created",
            new.session_pane(*id),
            None,
            Some(*id),
            None,
        );
    }
    for (id, w) in &new.windows {
        if !old.windows.contains_key(id) && old.sessions.contains_key(&w.session) {
            push(
                "window-created",
                Some(w.active),
                Some(*id),
                Some(w.session),
                None,
            );
        }
    }
    for (id, s) in &new.sessions {
        let Some(before) = old.sessions.get(id) else {
            continue;
        };
        let both = old.windows.contains_key(&s.active) && new.windows.contains_key(&before.active);
        if before.active != s.active && both {
            let pane = new.windows.get(&s.active).map(|w| w.active);
            push("window-focused", pane, Some(s.active), Some(*id), None);
        }
    }
    for (id, w) in &new.windows {
        let Some(before) = old.windows.get(id) else {
            continue;
        };
        let both = old.panes.contains_key(&w.active) && new.panes.contains_key(&before.active);
        if before.active != w.active && both {
            push(
                "pane-focused",
                Some(w.active),
                Some(*id),
                Some(w.session),
                None,
            );
        }
    }
    for (id, w) in &new.windows {
        let Some(before) = old.windows.get(id) else {
            continue;
        };
        if before.size != w.size {
            push(
                "window-resized",
                Some(w.active),
                Some(*id),
                Some(w.session),
                None,
            );
            continue;
        }
        let mut panes = w.panes.clone();
        let mut before_panes = before.panes.clone();
        panes.sort();
        before_panes.sort();
        if panes != before_panes {
            continue;
        }
        let changed: Vec<PaneId> = panes
            .iter()
            .filter(|p| {
                let size = |s: &Snapshot| s.panes.get(p).map(|p| p.size);
                size(old) != size(new)
            })
            .copied()
            .collect();
        if let Some(first) = changed.first() {
            let pane = if changed.contains(&w.active) {
                w.active
            } else {
                *first
            };
            push("pane-resized", Some(pane), Some(*id), Some(w.session), None);
        }
    }
    for (id, c) in &new.clients {
        let Some(before) = old.clients.get(id) else {
            continue;
        };
        let pane = new.session_pane(c.session);
        if before.size != c.size {
            push("client-resized", pane, None, Some(c.session), Some(*id));
        }
        if before.session != c.session {
            push(
                "client-session-changed",
                pane,
                None,
                Some(c.session),
                Some(*id),
            );
        }
    }
    for (id, p) in &new.panes {
        if p.dead && old.panes.get(id).is_some_and(|before| !before.dead) {
            let session = new.windows.get(&p.window).map(|w| w.session);
            push("pane-died", Some(*id), Some(p.window), session, None);
        }
    }
    for (id, p) in old
        .panes
        .iter()
        .filter(|(id, _)| !new.panes.contains_key(id))
    {
        let window = old.windows.get(&p.window);
        let session = window.map(|w| w.session);
        if exited.contains(id) {
            push("pane-exited", Some(*id), Some(p.window), session, None);
            continue;
        }
        let window_stays = new.windows.contains_key(&p.window);
        let only_pane = window.is_some_and(|w| w.panes.len() == 1)
            && session.is_some_and(|s| new.sessions.contains_key(&s));
        if window_stays || only_pane {
            push("pane-closed", Some(*id), Some(p.window), session, None);
        }
    }
    for id in old
        .sessions
        .keys()
        .filter(|id| !new.sessions.contains_key(id))
    {
        push("session-closed", None, None, Some(*id), None);
    }
    fired
}

// ---- environment ----

/// The server environment a new or respawned pane of `session` starts
/// with, under the session's own: every global variable but the hidden
/// ones.
// ponytail: a variable unset from the global environment that the server
// process itself inherited still reaches panes, which inherit the
// server's environment (`PaneSpawn.env` is applied over it and has no
// removals).
pub fn pane_env(state: &State, _session: SessionId) -> Vec<(String, String)> {
    visible(&state.hooks.global())
}

/// The environment a shell the server runs for `session` gets (`run-shell`,
/// popups, status sources): the global environment, then the session's,
/// both without hidden variables, as tmux 3.7b's `run-shell`.
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
        assert_eq!(
            bind(
                &mut state,
                &ctx,
                spec,
                "select-pane -t #{hook_pane} ; list-sessions"
            ),
            Ok(())
        );
        assert_eq!(
            show(&state, None),
            Ok("pane-focused select-pane -t #{hook_pane} ; list-sessions\n".to_string())
        );
    }

    #[test]
    fn hooks_do_nothing_while_none_is_bound() {
        let mut state = test_state();
        run_pending(&mut state);
        assert!(state.hooks.snapshot.is_none());
    }

    #[tokio::test]
    async fn a_session_hook_runs_once_and_its_commands_trigger_none() {
        let mut state = test_state();
        // The hook creates a session itself: that must not fire it again.
        fire(
            &mut state,
            &[
                "set-hook",
                "-g",
                "session-created",
                "new-session -d -s from-hook",
            ],
        )
        .unwrap();
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
        set_global(&mut state, "A", Some("1".into()), false);
        set_global(&mut state, "H", Some("2".into()), true);
        let env = shell_env(&state, None);
        assert_eq!(env, vec![("A".to_string(), "1".to_string())]);
        set_global(&mut state, "A", None, false);
        assert_eq!(pane_env(&state, SessionId(0)), Vec::new());
        assert_eq!(
            variables(&state, None),
            vec![("H".to_string(), "2".to_string(), true)]
        );
    }
}
