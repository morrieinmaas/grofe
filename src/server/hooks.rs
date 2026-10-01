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
//! **Queues.** As tmux, each CLI request and each attached client has its
//! own hook queue, and notify hooks share a global one. A queue runs in
//! order, and a hook's `run-shell` without `-b` holds its queue (only) until
//! the shell exits; a CLI request's later commands and its reply wait for
//! its own queue ([`request_waiting`]). Commands
//! run as they are, each expanding its own formats (`run-shell` sees the
//! `#{hook*}` variables). Commands a hook runs fire notify hooks but no
//! `after-*` ones, as in tmux (recorded).
//!
//! **Environment.** The global environment starts as the server process's
//! own, and is all a pane or `run-shell` gets from the server's. Hidden
//! (`-h`) variables never reach a process (recorded on tmux 3.7b); they,
//! like every global variable, expand in `$NAME` in a command string
//! (`cmd::split_command_string`, through [`Hooks::for_server`]). A
//! session's variables are the `update-environment` ones from its client
//! (`Session.env`); here are its hidden ones and the names it marks
//! removed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;

use crate::cmd::{self, Command, FormatVars, Resolve, Target};
use crate::config::Resolved;
use crate::protocol::{ClientCtx, Dialect, Failure};
use crate::server::dispatch;
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
    EventSpec {
        name: "alert-bell",
        fires: "a bell in a monitored window, when bell-action applies (alerts)",
        tmux: "alert-bell",
    },
    EventSpec {
        name: "alert-activity",
        fires: "output in a monitored window, when activity-action applies (alerts)",
        tmux: "alert-activity",
    },
    EventSpec {
        name: "alert-silence",
        fires: "no output in a monitored window for monitor-silence seconds (alerts)",
        tmux: "alert-silence",
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
    /// Each origin's hook queue.
    queues: BTreeMap<Origin, HookQueue>,
    /// The CLI request now running, once one of its commands fired a hook.
    request: Option<u64>,
    next_request: u64,
    /// Hook commands are running: they fire no command-shaped hooks.
    running: bool,
    /// How deep `run_commands` is nested ([`enter`]); 1 is a request's own
    /// commands.
    depth: u32,
    /// The running hook's variables.
    current: Option<HookFormats>,
    /// See [`take_reply_hold`].
    reply_hold: Option<oneshot::Receiver<String>>,
    global: Arc<RwLock<Env>>,
    /// Hidden session variables (`set-environment -h -t`), by session.
    session_hidden: BTreeMap<SessionId, BTreeMap<String, String>>,
    /// Session variables marked removed (`-NAME`), by session.
    session_removed: BTreeMap<SessionId, BTreeSet<String>>,
}

/// The server's start directory as tmux's `find_cwd` gives it: `$PWD`
/// when it names the same directory, else the real path.
fn start_cwd() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let pwd = std::env::var("PWD")
        .ok()
        .filter(|p| std::fs::canonicalize(p).ok() == std::fs::canonicalize(&cwd).ok());
    pwd.or_else(|| cwd.to_str().map(str::to_string))
}

impl Default for Hooks {
    /// No hooks, and the global environment seeded from the process's,
    /// with `PWD` the server's start directory, as tmux 3.7b sets it
    /// (recorded: `show-environment -g` holds `PWD`).
    fn default() -> Hooks {
        let mut env: Env = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .map(|(k, value)| {
                let var = Var {
                    value,
                    hidden: false,
                };
                (k, var)
            })
            .collect();
        if let Some(cwd) = start_cwd() {
            let var = Var {
                value: cwd,
                hidden: false,
            };
            env.insert("PWD".to_string(), var);
        }
        Hooks {
            bound: BTreeMap::new(),
            runtime: BTreeSet::new(),
            overflow: Vec::new(),
            queues: BTreeMap::new(),
            request: None,
            next_request: 0,
            running: false,
            depth: 0,
            current: None,
            reply_hold: None,
            global: Arc::new(RwLock::new(env)),
            session_hidden: BTreeMap::new(),
            session_removed: BTreeMap::new(),
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
/// `name` alone (a bare `<event>` when nothing is bound to it). In the
/// tmux dialect, tmux 3.7b's listing ([`show_tmux`]).
pub fn show(
    state: &State,
    name: Option<&str>,
    dialect: Dialect,
    window: bool,
) -> Result<String, String> {
    if dialect == Dialect::Tmux {
        return show_tmux(state, name, window);
    }
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

/// tmux 3.7b's `show-hooks -g[w] [hook]` (recorded): the hooks of its
/// session table (`-w`: its window and pane table) in tmux's order, a
/// bound one as `<hook>[0] <commands>`, the rest by name alone; the
/// commands printed from their parse, with full verb names, each argument
/// quoted as `show-options` quotes a value, and ` ; ` between them. A
/// named hook shows its binding from either table.
fn show_tmux(state: &State, name: Option<&str>, window: bool) -> Result<String, String> {
    use crate::compat::tmux::formats::tmux_hook_names;
    let line = |hook: &str| -> String {
        let bound = EVENTS
            .iter()
            .find(|e| e.tmux == hook)
            .and_then(|e| state.hooks.bound.get(e.name));
        match bound {
            Some(s) => format!("{hook}[0] {}\n", tmux_commands(&s.cmds)),
            None => format!("{hook}\n"),
        }
    };
    match name {
        None => Ok(tmux_hook_names(window).iter().map(|h| line(h)).collect()),
        Some(hook)
            if !tmux_hook_names(false).contains(&hook)
                && !tmux_hook_names(true).contains(&hook) =>
        {
            Err(format!("invalid option: {hook}"))
        }
        Some(hook) => Ok(line(hook)),
    }
}

/// A hook's commands as tmux prints them.
fn tmux_commands(cmds: &[Vec<String>]) -> String {
    let one = |argv: &Vec<String>| -> String {
        let mut words = Vec::with_capacity(argv.len());
        if let Some((verb, args)) = argv.split_first() {
            let verb = crate::compat::tmux::resolve(verb).map_or(verb.as_str(), |s| s.name);
            words.push(verb.to_string());
            words.extend(
                args.iter()
                    .map(|a| crate::server::dispatch::options::quote(a)),
            );
        }
        words.join(" ")
    };
    cmds.iter().map(one).collect::<Vec<_>>().join(" ; ")
}

// ---- emission ----

/// One event that happened, with the context its hook runs in.
#[derive(Clone)]
struct Fired {
    event: &'static str,
    /// The pane the hook's commands see as current; `None`, or a pane
    /// gone by the time it runs, means the current pane then.
    pane: Option<PaneId>,
    /// The client the hook's commands see as current (a binding's, or a
    /// client event's).
    client: Option<ClientId>,
    vars: HookFormats,
}

/// `#{hook}` and the `#{hook_*}` variables, set only where tmux 3.7b
/// sets them (recorded), else empty.
#[derive(Clone, Default)]
pub struct HookFormats {
    hook: &'static str,
    hook_pane: Option<PaneId>,
    hook_window: Option<WindowId>,
    hook_session_name: Option<String>,
    hook_client: Option<String>,
    /// Set by the window alerts only, as tmux's `notify_winlink`.
    hook_session: Option<SessionId>,
    hook_window_name: Option<String>,
}

impl Fired {
    fn new(event: &'static str, pane: Option<PaneId>) -> Fired {
        Fired {
            event,
            pane,
            client: None,
            vars: HookFormats {
                hook: event,
                ..HookFormats::default()
            },
        }
    }
}

/// Whether a notify-shaped hook is bound. Commands a hook runs fire these
/// too, as tmux 3.7b's do (recorded).
fn wanted(state: &State, name: &str) -> bool {
    state.hooks.bound.contains_key(name)
}

/// A session's current window's active pane.
fn session_pane(state: &State, session: SessionId) -> Option<PaneId> {
    let s = state.model.sessions.get(&session)?;
    state.model.windows.get(&s.active).map(|w| w.active)
}

/// Queues a notify-shaped hook on the global queue, after the current
/// reply; a full job queue keeps it for the next [`run_pending`].
fn notify(state: &mut State, f: Fired) {
    let job: Job = Box::new(move |state: &mut State| enqueue(state, Origin::Global, &f));
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
    f.vars.hook_session_name = state.model.sessions.get(&session).map(|s| s.name.clone());
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
    f.client = Some(id);
    f.vars.hook_client = Some(client.name.clone());
    notify(state, f);
}

/// `alert-bell`, `alert-activity` or `alert-silence` in `window`
/// (`server::alerts`): run in the window, its active pane current.
pub fn window_alert(state: &mut State, name: &'static str, window: WindowId) {
    if !wanted(state, name) {
        return;
    }
    let Some(w) = state.model.windows.get(&window) else {
        return;
    };
    let mut f = Fired::new(name, Some(w.active));
    f.vars.hook_window = Some(window);
    f.vars.hook_window_name = Some(w.name.clone());
    f.vars.hook_session = Some(w.session);
    f.vars.hook_session_name = state.model.sessions.get(&w.session).map(|s| s.name.clone());
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
    f.vars.hook_pane = Some(pane);
    f.vars.hook_window = Some(window);
    notify(state, f);
}

/// Runs what is pending after a command sequence (before its reply) or a
/// pane event: `session-closed` for each session the model removed (the
/// current pane as context), and hooks a full job queue refused. At the
/// end of a CLI request (not inside a hook, nor at the end of a command
/// the request runs inside another, as `if-shell -F` runs its branch), a
/// request queue still waiting on a shell holds the reply
/// ([`take_reply_hold`]). A nested command's hook is on the request's one
/// queue, so the reply waits for every hook the request fired.
pub fn run_pending(state: &mut State) {
    crate::server::alerts::drain(state);
    for (_, name) in std::mem::take(&mut state.model.closed_sessions) {
        if wanted(state, "session-closed") {
            let mut f = Fired::new("session-closed", None);
            f.vars.hook_session_name = Some(name);
            notify(state, f);
        }
    }
    if state.hooks.running || state.hooks.depth > 0 {
        return;
    }
    for job in std::mem::take(&mut state.hooks.overflow) {
        job(state);
    }
    if let Some(request) = state.hooks.request.take() {
        let origin = Origin::Request(request);
        if let Some(q) = state.hooks.queues.get_mut(&origin) {
            let (tx, rx) = oneshot::channel();
            q.idle.push(tx);
            state.hooks.reply_hold = Some(rx);
        }
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
/// command-shaped hook (tmux's `after-<verb>`) on its origin's queue: the
/// attached client's for a binding, else the CLI request's own. What the
/// hook's commands print joins the command's output, as in tmux. Commands
/// a hook runs fire no command-shaped hooks.
pub fn run_command(
    state: &mut State,
    ctx: &ClientCtx,
    cmd: Command,
    route: fn(&mut State, &ClientCtx, Command) -> Result<String, String>,
) -> Result<String, String> {
    let before = Before::of(state, ctx, &cmd)
        .filter(|(name, _)| !state.hooks.running && state.hooks.bound.contains_key(name));
    let result = route(state, ctx, cmd);
    let (Ok(out), Some((name, before))) = (&result, before) else {
        return result;
    };
    let Some(pane) = before.pane(state, ctx) else {
        return result;
    };
    let origin = match ctx.client {
        Some(id) => Origin::Client(id),
        None => {
            let hooks = &mut state.hooks;
            let request = *hooks.request.get_or_insert_with(|| {
                hooks.next_request += 1;
                hooks.next_request
            });
            Origin::Request(request)
        }
    };
    let mut f = Fired::new(name, pane);
    f.client = ctx.client;
    let mut out = out.clone();
    enqueue(state, origin, &f);
    if let Some(q) = state.hooks.queues.get_mut(&origin) {
        out.push_str(&std::mem::take(&mut q.out));
        if !q.waiting && q.items.is_empty() {
            state.hooks.queues.remove(&origin);
        }
    }
    Ok(out)
}

/// Where a hook runs: tmux gives each CLI request and each attached
/// client its own command queue, and runs notify hooks on a global one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Global,
    Client(ClientId),
    Request(u64),
}

/// One origin's hook commands, in order.
#[derive(Default)]
struct HookQueue {
    items: VecDeque<Queued>,
    /// Waiting for a hook's `run-shell` to exit.
    waiting: bool,
    /// What the hooks printed, for a CLI request's reply.
    out: String,
    /// Told, with the rest of the output, when the queue is next idle.
    idle: Vec<oneshot::Sender<String>>,
}

/// A hook's commands waiting to run in order, with the context they run
/// in. Formats are not expanded here: each command expands its own when
/// it runs, as in tmux (`set -g @x '#{pane_id}'` stores the literal).
struct Queued {
    fired: Fired,
    dialect: Dialect,
    cmds: VecDeque<Vec<String>>,
}

/// Appends the hook bound to `f`'s event to `origin`'s queue, then runs
/// that queue.
fn enqueue(state: &mut State, origin: Origin, f: &Fired) {
    let Some(stored) = state.hooks.bound.get(f.event).cloned() else {
        return;
    };
    let mut fired = f.clone();
    // A hook set in the tmux dialect sees tmux's name in `#{hook}`.
    if stored.dialect == Dialect::Tmux
        && let Some(spec) = event(f.event)
    {
        fired.vars.hook = spec.tmux;
    }
    let queued = Queued {
        fired,
        dialect: stored.dialect,
        cmds: stored.cmds.into_iter().collect(),
    };
    state
        .hooks
        .queues
        .entry(origin)
        .or_default()
        .items
        .push_back(queued);
    drain(state, origin);
}

/// The context a queued hook's next command runs in: the event's pane if
/// it is still there, else the current one, and its session's cwd.
fn hook_ctx(state: &State, q: &Queued) -> ClientCtx {
    let pane = context_pane(state, q.fired.pane);
    let cwd = pane
        .and_then(|p| state.model.panes.get(&p))
        .and_then(|p| state.model.windows.get(&p.window))
        .and_then(|w| state.model.sessions.get(&w.session))
        .map_or_else(|| PathBuf::from("/"), |s| s.cwd.clone());
    ClientCtx {
        cwd,
        pane,
        env: Vec::new(),
        client: q.fired.client.filter(|c| state.clients.contains_key(c)),
        dialect: q.dialect,
    }
}

/// Runs `origin`'s queued hook commands in order, as tmux's command queue
/// does: a `run-shell` without `-b` holds everything after it on that
/// queue (later commands and later hooks) until its shell exits; other
/// queues go on. The first error ends a hook's commands, and goes to the
/// server log.
fn drain(state: &mut State, origin: Origin) {
    loop {
        let Some(q) = state.hooks.queues.get_mut(&origin) else {
            return;
        };
        if q.waiting || state.hooks.running {
            return;
        }
        let Some(mut item) = q.items.pop_front() else {
            // Idle: a held reply gets the output; a request's queue that
            // holds nothing keeps its output for `run_command` to take;
            // the others' output goes nowhere.
            if !q.idle.is_empty() {
                let out = std::mem::take(&mut q.out);
                for idle in std::mem::take(&mut q.idle) {
                    let _ = idle.send(out.clone());
                }
                state.hooks.queues.remove(&origin);
            } else if !matches!(origin, Origin::Request(_)) {
                state.hooks.queues.remove(&origin);
            }
            return;
        };
        let mut shell = None;
        let mut printed = String::new();
        state.hooks.running = true;
        // A hook's `run-shell` answers here, not to the request that
        // triggered it.
        let deferred = state.deferred_reply.take();
        while shell.is_none()
            && let Some(argv) = item.cmds.pop_front()
        {
            let ctx = hook_ctx(state, &item);
            state.hooks.current = Some(item.fired.vars.clone());
            match dispatch::run_split(state, &ctx, &argv) {
                Ok(text) => printed.push_str(&text),
                Err(e) => {
                    eprintln!("grofe hook {}: {e}", item.fired.event);
                    item.cmds.clear();
                }
            }
            shell = state.deferred_reply.take();
        }
        state.hooks.current = None;
        state.deferred_reply = deferred;
        state.hooks.running = false;
        let Some(q) = state.hooks.queues.get_mut(&origin) else {
            return;
        };
        q.out.push_str(&printed);
        let Some(rx) = shell else {
            continue;
        };
        if !item.cmds.is_empty() {
            q.items.push_front(item);
        }
        q.waiting = true;
        let jobs = state.jobs.clone();
        tokio::spawn(async move {
            let answer = rx.await;
            let resume: Job = Box::new(move |state: &mut State| {
                if let Some(q) = state.hooks.queues.get_mut(&origin) {
                    q.waiting = false;
                    match answer {
                        Ok(Ok(text)) => q.out.push_str(&text),
                        Ok(Err(f)) => q.out.push_str(&f.out),
                        Err(_) => {}
                    }
                }
                drain(state, origin);
            });
            let _ = jobs.send(resume).await;
        });
        return;
    }
}

/// The config's `load-commands` (TP.0), queued in order on the global
/// queue, where a `run-shell` holds the rest until its shell exits, as
/// tmux's queue runs a config file's commands. They run in grofe's own
/// grammar with no current pane; an error ends them and goes to the log.
pub fn run_at_load(state: &mut State, cmds: Vec<Vec<String>>) {
    if cmds.is_empty() {
        return;
    }
    let queued = Queued {
        fired: Fired::new("load-commands", None),
        dialect: Dialect::Grofe,
        cmds: cmds.into_iter().collect(),
    };
    state
        .hooks
        .queues
        .entry(Origin::Global)
        .or_default()
        .items
        .push_back(queued);
    drain(state, Origin::Global);
}

/// `run_commands` starts: a request's commands nest (`if -F`), and only
/// the outermost ends its request ([`run_pending`]).
pub fn enter(state: &mut State) {
    state.hooks.depth += 1;
}

/// How deep `run_commands` is nested now.
pub fn depth(state: &State) -> u32 {
    state.hooks.depth
}

/// `run_commands` ends.
pub fn leave(state: &mut State) {
    state.hooks.depth = state.hooks.depth.saturating_sub(1);
}

/// Whether the CLI request now running (outside a hook) has a hook
/// waiting on a shell: its next command waits, at any depth, as tmux
/// 3.7b runs an `after-*` hook before the next command of its queue
/// (recorded: `select-pane ; run-shell`, inside `if -F` too).
pub fn request_waiting(state: &State) -> bool {
    let hooks = &state.hooks;
    !hooks.running
        && hooks.request.is_some_and(|r| {
            hooks
                .queues
                .get(&Origin::Request(r))
                .is_some_and(|q| q.waiting)
        })
}

/// When [`request_waiting`], a receiver that answers with the hooks'
/// output once their queue is idle; the held rest of the request waits
/// for it. The reply no longer waits on this queue: the rest's answer
/// carries its output.
pub fn take_request_wait(state: &mut State) -> Option<oneshot::Receiver<Result<String, Failure>>> {
    let hooks = &mut state.hooks;
    if hooks.running {
        return None;
    }
    let origin = Origin::Request(hooks.request?);
    let q = hooks.queues.get_mut(&origin).filter(|q| q.waiting)?;
    let (tx, rx) = oneshot::channel();
    q.idle.push(tx);
    hooks.request = None;
    let (answer, wait) = oneshot::channel();
    tokio::spawn(async move {
        if let Ok(out) = rx.await {
            let _ = answer.send(Ok(out));
        }
    });
    Some(wait)
}

/// For the reply to the request that just ran (`finish`): when its own
/// hook queue still waits on a shell (tmux 3.7b holds the CLI until its
/// `after-*` hooks finish), a receiver that answers, with the rest of
/// the hooks' output, once that queue is idle. Only this request's hooks
/// hold it.
pub fn take_reply_hold(state: &mut State) -> Option<oneshot::Receiver<String>> {
    state.hooks.reply_hold.take()
}

/// Inside a hook's command: a `run-shell` there waits for its shell even
/// from a binding, so the hook's queue can hold what comes after it.
pub fn in_hook(state: &State) -> bool {
    state.hooks.running
}

/// The running hook's variables, for commands that expand formats
/// (`run-shell`).
pub fn current_formats(state: &State) -> Option<&HookFormats> {
    state.hooks.current.as_ref()
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

/// A hook's own variables over `inner`.
pub struct HookVars<'a> {
    pub formats: &'a HookFormats,
    pub inner: &'a dyn FormatVars,
}

impl FormatVars for HookVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.formats.get(name).or_else(|| self.inner.get(name))
    }
}

impl HookFormats {
    /// `#{hook}` or a `#{hook_*}` variable; `None` for any other name.
    pub fn get(&self, name: &str) -> Option<String> {
        let text = |v: Option<String>| Some(v.unwrap_or_default());
        match name {
            "hook" => Some(self.hook.to_string()),
            "hook_pane" => text(self.hook_pane.map(|p| p.to_string())),
            "hook_window" => text(self.hook_window.map(|w| w.to_string())),
            "hook_session_name" => text(self.hook_session_name.clone()),
            "hook_client" => text(self.hook_client.clone()),
            "hook_session" => text(self.hook_session.map(|s| s.to_string())),
            "hook_window_name" => text(self.hook_window_name.clone()),
            _ => None,
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
/// ones and those the session marks removed (`-NAME`). A pane gets nothing
/// else from the server's own environment, so `set-environment -gu`
/// removes even a variable the server started with.
pub fn pane_env(state: &State, session: SessionId) -> Vec<(String, String)> {
    let removed = state.hooks.session_removed.get(&session);
    visible(&state.hooks.global())
        .into_iter()
        .filter(|(k, _)| !removed.is_some_and(|r| r.contains(k)))
        .collect()
}

/// The whole environment a shell the server runs for `session` gets
/// (`run-shell`, popups, status sources): as a pane's, the global
/// environment under the session's, without hidden variables, as tmux
/// 3.7b's `run-shell`.
/// Over it, the variables tmux sets for every job (`compat::job_env`).
pub fn shell_env(state: &State, session: Option<SessionId>) -> Vec<(String, String)> {
    let session = session.filter(|s| state.model.sessions.contains_key(s));
    let mut env = match session {
        Some(s) => {
            let mut env = pane_env(state, s);
            env.extend(state.model.sessions[&s].env.iter().cloned());
            env
        }
        None => visible(&state.hooks.global()),
    };
    let set = crate::compat::job_env(state, session);
    env.retain(|(k, _)| !set.iter().any(|(name, _)| name == k));
    env.extend(set);
    env
}

fn visible(env: &Env) -> Vec<(String, String)> {
    env.iter()
        .filter(|(_, v)| !v.hidden)
        .map(|(k, v)| (k.clone(), v.value.clone()))
        .collect()
}

/// The names `update-environment` lists for `session` (a session not yet
/// made gets the global list).
fn update_names(state: &State, session: SessionId) -> Vec<String> {
    use crate::server::dispatch::options::Owner;
    state
        .model
        .options
        .lookup_list("update-environment", &[Owner::Session(session)])
        .map(|l| l.values().cloned().collect())
        .unwrap_or_default()
}

/// The environment new-session gives `session` from the creating client's
/// `client_env`, as tmux 3.7b: only the variables `update-environment`
/// names; one the client lacks is marked removed (`-NAME`), which hides
/// a global value of it.
pub fn session_environment(
    state: &mut State,
    session: SessionId,
    client_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let mut removed = BTreeSet::new();
    for name in update_names(state, session) {
        match client_env.iter().rev().find(|(k, _)| *k == name) {
            Some((_, v)) => env.push((name, v.clone())),
            None => {
                removed.insert(name);
            }
        }
    }
    state.hooks.session_removed.insert(session, removed);
    state.hooks.session_hidden.remove(&session);
    env
}

/// Attach and switch-client: `session` takes the `update-environment`
/// variables from the client's `client_env` again, as tmux 3.7b (recorded):
/// each it has is set, each it lacks marked removed.
pub fn update_environment(state: &mut State, session: SessionId, client_env: &[(String, String)]) {
    for name in update_names(state, session) {
        let value = client_env
            .iter()
            .rev()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone());
        set_session(state, session, &name, value.clone(), false);
        if value.is_none() {
            state
                .hooks
                .session_removed
                .entry(session)
                .or_default()
                .insert(name);
        }
    }
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

/// `set-environment -t session [-h] name value`, or `-u` (`value` `None`),
/// which removes the variable from the session altogether (a removed
/// mark included).
pub fn set_session(
    state: &mut State,
    session: SessionId,
    name: &str,
    value: Option<String>,
    hidden: bool,
) {
    let live = &state.model.sessions;
    let hooks = &mut state.hooks;
    hooks.session_hidden.retain(|s, _| live.contains_key(s));
    hooks.session_removed.retain(|s, _| live.contains_key(s));
    let Some(s) = state.model.sessions.get_mut(&session) else {
        return;
    };
    s.env.retain(|(k, _)| k != name);
    if let Some(removed) = hooks.session_removed.get_mut(&session) {
        removed.remove(name);
    }
    let hidden_vars = hooks.session_hidden.entry(session).or_default();
    hidden_vars.remove(name);
    match value {
        Some(value) if hidden => {
            hidden_vars.insert(name.to_string(), value);
        }
        Some(value) => s.env.push((name.to_string(), value)),
        None => {}
    }
}

/// One variable as `show-environment` sees it.
pub struct Variable {
    pub name: String,
    /// `None`: marked removed (`-NAME`).
    pub value: Option<String>,
    pub hidden: bool,
}

/// Every variable of the global environment (`session` `None`) or of a
/// session, by name. A session variable set twice shows its last value.
pub fn variables(state: &State, session: Option<SessionId>) -> Vec<Variable> {
    let Some(session) = session else {
        return state
            .hooks
            .global()
            .iter()
            .map(|(k, v)| Variable {
                name: k.clone(),
                value: Some(v.value.clone()),
                hidden: v.hidden,
            })
            .collect();
    };
    let mut vars: BTreeMap<&str, (Option<&str>, bool)> = BTreeMap::new();
    if let Some(removed) = state.hooks.session_removed.get(&session) {
        for k in removed {
            vars.insert(k, (None, false));
        }
    }
    if let Some(s) = state.model.sessions.get(&session) {
        for (k, v) in &s.env {
            vars.insert(k, (Some(v), false));
        }
    }
    if let Some(hidden) = state.hooks.session_hidden.get(&session) {
        for (k, v) in hidden {
            vars.insert(k, (Some(v), true));
        }
    }
    vars.into_iter()
        .map(|(k, (v, hidden))| Variable {
            name: k.to_string(),
            value: v.map(str::to_string),
            hidden,
        })
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
        assert_eq!(EVENTS.len(), 15);
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
        assert_eq!(
            show(&state, None, Dialect::Grofe, false),
            Ok(format!("pane-focused {cmd}\n"))
        );
        assert_eq!(check_command(cmd).map(|c| c.len()), Ok(2));
        assert_eq!(check_command("  "), Ok(Vec::new()));
    }

    #[tokio::test]
    async fn a_session_hook_fires_again_for_the_session_it_makes() {
        let mut state = test_state();
        // The hook's own new-session fires session-created again, as in
        // tmux; that second run fails (the name is taken) and stops.
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
        assert!(vars.iter().any(|v| v.name == "GROFE_TC5_H" && v.hidden));
    }
}
