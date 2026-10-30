//! Event hooks (TC.5, spec §4.5 "Hooks and environment"): commands bound
//! to grofe events, run through the dispatcher with the event's target as
//! their context, plus the server environment `set-environment` keeps
//! (spec §4.2).
//!
//! **Hooks are arrays, as in tmux.** Each event holds, at each [`Scope`]
//! (global, a session's, a window's, a pane's), commands by index
//! (`set-hook -g/-t/-p`, `-a`, `name[N]`, `-u`, `-R`). A firing hook runs
//! the nearest scope that has any (`commands_for`); [`EVENTS`] holds all 68
//! of tmux's hook names.
//!
//! **Events.** [`EVENTS`] lists grofe's event names, what fires each, and
//! the tmux hook FE.3 maps onto it. Each is emitted where it happens, with
//! the context tmux 3.7b gives its hook (recorded on a private socket);
//! the notify hooks about sessions, windows and panes that change in many
//! places are found by comparing the model with its last look ([`watch`]):
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
//! `#{hook*}` variables). Commands an `after-*` hook runs fire notify
//! hooks but no `after-*` ones; commands a notify hook runs fire neither
//! (`Hooks::quiet`), as in tmux (recorded), so a hook's own commands
//! cannot fire it again (a `run-shell` that calls `tmux` is a new client,
//! which can, in tmux too).
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

use crate::cmd::{
    self, Command, FormatVars, Inner, LoopItem, LoopKind, LoopSort, NameKind, Resolve, Target,
};
use crate::config::Resolved;
use crate::event::Event;
use crate::protocol::{ClientCtx, Dialect, Failure};
use crate::server::dispatch;
use crate::server::{Job, State};
pub use watch::{track, track_focus};

use crate::types::{ClientId, PaneId, SessionId, WindowId};

/// One grofe event a hook can be bound to.
pub struct EventSpec {
    pub name: &'static str,
    /// What fires the event, for readers and FE.3.
    pub fires: &'static str,
    /// The tmux 3.7b hook FE.3's table maps onto this event.
    pub tmux: &'static str,
    /// Which tmux option table holds the hook.
    pub kind: HookKind,
}

/// The tmux option table a hook lives in, which decides the scopes it can
/// be set at and how a firing hook is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// A session hook: the session's own commands, else the global ones.
    Session,
    /// A window or pane hook (`pane-*`): the pane's, else the window's,
    /// else the global ones.
    WindowPane,
    /// A window hook: the window's, else the global ones.
    Window,
}

/// Every grofe event.
pub const EVENTS: &[EventSpec] = &[
    EventSpec {
        name: "pane-focused",
        fires: "select-pane made another pane active",
        tmux: "after-select-pane",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "window-focused",
        fires: "select-window succeeded, even on the current window",
        tmux: "after-select-window",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "window-created",
        fires: "new-window made a window",
        tmux: "after-new-window",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "pane-closed",
        fires: "kill-pane removed a pane",
        tmux: "after-kill-pane",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "pane-resized",
        fires: "resize-pane succeeded",
        tmux: "after-resize-pane",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "window-resized",
        fires: "resize-window succeeded",
        tmux: "after-resize-window",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-resized",
        fires: "a client attached, or its terminal reported a size",
        tmux: "client-resized",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-session-changed",
        fires: "a client attached, or switch-client or attach-session ran for it",
        tmux: "client-session-changed",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "session-created",
        fires: "new-session made a session",
        tmux: "session-created",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "session-closed",
        fires: "a session was removed",
        tmux: "session-closed",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "pane-exited",
        fires: "a pane's process exited and the pane was removed",
        tmux: "pane-exited",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "pane-died",
        fires: "a pane's process exited and remain-on-exit kept the pane",
        tmux: "pane-died",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "alert-bell",
        fires: "a bell in a monitored window, when bell-action applies (alerts)",
        tmux: "alert-bell",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "alert-activity",
        fires: "output in a monitored window, when activity-action applies (alerts)",
        tmux: "alert-activity",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "alert-silence",
        fires: "no output in a monitored window for monitor-silence seconds (alerts)",
        tmux: "alert-silence",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-bind-key",
        fires: "bind-key ran",
        tmux: "after-bind-key",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-capture-pane",
        fires: "capture-pane ran",
        tmux: "after-capture-pane",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-copy-mode",
        fires: "copy-mode ran",
        tmux: "after-copy-mode",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-display-message",
        fires: "display-message ran",
        tmux: "after-display-message",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-display-panes",
        fires: "display-panes ran",
        tmux: "after-display-panes",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-list-buffers",
        fires: "list-buffers ran",
        tmux: "after-list-buffers",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-list-clients",
        fires: "list-clients ran",
        tmux: "after-list-clients",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-list-keys",
        fires: "list-keys ran",
        tmux: "after-list-keys",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-list-panes",
        fires: "list-panes ran",
        tmux: "after-list-panes",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-list-sessions",
        fires: "list-sessions ran",
        tmux: "after-list-sessions",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-list-windows",
        fires: "list-windows ran",
        tmux: "after-list-windows",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-load-buffer",
        fires: "load-buffer ran",
        tmux: "after-load-buffer",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-lock-server",
        fires: "never: grofe has no lock-server (accepted so a tmux config loads)",
        tmux: "after-lock-server",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-new-session",
        fires: "new-session made a session",
        tmux: "after-new-session",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-paste-buffer",
        fires: "paste-buffer ran",
        tmux: "after-paste-buffer",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-pipe-pane",
        fires: "pipe-pane ran",
        tmux: "after-pipe-pane",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-queue",
        fires: "never: tmux 3.7b lists the hook but nothing runs it",
        tmux: "after-queue",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-refresh-client",
        fires: "refresh-client ran",
        tmux: "after-refresh-client",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-rename-session",
        fires: "rename-session ran",
        tmux: "after-rename-session",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-rename-window",
        fires: "rename-window ran",
        tmux: "after-rename-window",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-save-buffer",
        fires: "save-buffer ran",
        tmux: "after-save-buffer",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-select-layout",
        fires: "select-layout ran",
        tmux: "after-select-layout",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-send-keys",
        fires: "send-keys ran",
        tmux: "after-send-keys",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-set-buffer",
        fires: "set-buffer ran",
        tmux: "after-set-buffer",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-set-environment",
        fires: "set-environment ran",
        tmux: "after-set-environment",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-set-hook",
        fires: "set-hook ran",
        tmux: "after-set-hook",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-set-option",
        fires: "set-option ran",
        tmux: "after-set-option",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-show-environment",
        fires: "show-environment ran",
        tmux: "after-show-environment",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-show-messages",
        fires: "show-messages ran",
        tmux: "after-show-messages",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-show-options",
        fires: "show-options ran",
        tmux: "after-show-options",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-split-window",
        fires: "split-window made a pane",
        tmux: "after-split-window",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "after-unbind-key",
        fires: "unbind-key ran",
        tmux: "after-unbind-key",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-active",
        fires: "never: grofe does not tell a client's first input apart (accepted as tmux does)",
        tmux: "client-active",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-attached",
        fires: "a client attached",
        tmux: "client-attached",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-detached",
        fires: "an attached client went away",
        tmux: "client-detached",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-focus-in",
        fires: "a client's terminal reported focus (while focus-events is on)",
        tmux: "client-focus-in",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-focus-out",
        fires: "a client's terminal lost focus (while focus-events is on)",
        tmux: "client-focus-out",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-light-theme",
        fires: "never: grofe does not read the terminal's colour scheme reports",
        tmux: "client-light-theme",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "client-dark-theme",
        fires: "never: grofe does not read the terminal's colour scheme reports",
        tmux: "client-dark-theme",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "command-error",
        fires: "a command failed",
        tmux: "command-error",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "pane-focus-in",
        fires: "a pane became the active pane of a window an attached, focused client shows",
        tmux: "pane-focus-in",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "pane-focus-out",
        fires: "a pane stopped being that pane",
        tmux: "pane-focus-out",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "pane-mode-changed",
        fires: "a pane entered or left a mode (copy mode, the clock, a chooser)",
        tmux: "pane-mode-changed",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "pane-set-clipboard",
        fires: "a pane set the clipboard (OSC 52)",
        tmux: "pane-set-clipboard",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "pane-title-changed",
        fires: "a pane's title changed",
        tmux: "pane-title-changed",
        kind: HookKind::WindowPane,
    },
    EventSpec {
        name: "session-renamed",
        fires: "a session's name changed",
        tmux: "session-renamed",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "session-window-changed",
        fires: "a session's current window changed",
        tmux: "session-window-changed",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "window-layout-changed",
        fires: "a window's layout changed",
        tmux: "window-layout-changed",
        kind: HookKind::Window,
    },
    EventSpec {
        name: "window-linked",
        fires: "a window was linked into a session",
        tmux: "window-linked",
        kind: HookKind::Session,
    },
    EventSpec {
        name: "window-pane-changed",
        fires: "a window's active pane changed",
        tmux: "window-pane-changed",
        kind: HookKind::Window,
    },
    EventSpec {
        name: "window-renamed",
        fires: "a window's name changed",
        tmux: "window-renamed",
        kind: HookKind::Window,
    },
    EventSpec {
        name: "window-unlinked",
        fires: "a window was unlinked from a session",
        tmux: "window-unlinked",
        kind: HookKind::Session,
    },
    // grofe's own `window-resized` is tmux's `after-resize-window`; tmux's
    // `window-resized` is a different hook.
    EventSpec {
        name: "window-size-changed",
        fires: "a window's size changed",
        tmux: "window-resized",
        kind: HookKind::Window,
    },
];

/// The event named `name`.
pub fn event(name: &str) -> Option<&'static EventSpec> {
    EVENTS.iter().find(|e| e.name == name)
}

/// Where a hook is set: tmux's global options, or a session's, window's
/// or pane's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Global,
    Session(SessionId),
    Window(WindowId),
    Pane(PaneId),
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
    /// `None`: marked removed (`set-environment -r`), shown as `-NAME`.
    value: Option<String>,
    hidden: bool,
}

type Env = BTreeMap<String, Var>;

mod watch;

/// Bound hooks and the server environment (TC.5). Lives on `State.hooks`.
pub struct Hooks {
    /// The hooks, as tmux keeps them (options): each event at a scope holds
    /// an array of commands by index. An entry that is there but empty
    /// still hides the wider scope's commands, as in tmux.
    bound: BTreeMap<(Scope, &'static str), BTreeMap<u32, Stored>>,
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
    /// A notify hook's commands are running: they fire no hooks at all, as
    /// tmux's `notify_add` drops events while a `CMDQ_STATE_NOHOOKS` item
    /// of the global queue runs. This is what keeps a `window-renamed`
    /// hook that renames from firing itself forever.
    quiet: bool,
    /// How deep `run_commands` is nested ([`enter`]); 1 is a request's own
    /// commands.
    depth: u32,
    /// The running hook's variables.
    current: Option<HookFormats>,
    /// What the model looked like at the last look, for the notify hooks
    /// that are found by comparing ([`watch`]).
    watch: watch::Watch,
    /// See [`take_reply_hold`].
    reply_hold: Option<oneshot::Receiver<String>>,
    global: Arc<RwLock<Env>>,
    /// Hidden session variables (`set-environment -h -t`), by session.
    session_hidden: BTreeMap<SessionId, BTreeMap<String, Option<String>>>,
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
                    value: Some(value),
                    hidden: false,
                };
                (k, var)
            })
            .collect();
        if let Some(cwd) = start_cwd() {
            let var = Var {
                value: Some(cwd),
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
            quiet: false,
            depth: 0,
            current: None,
            watch: watch::Watch::default(),
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
            env.get(name).and_then(|v| v.value.clone())
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
    hooks.bound.retain(|(scope, event), elems| {
        if *scope == Scope::Global && !runtime.contains(event) {
            elems.retain(|_, s| !s.from_config);
        }
        !elems.is_empty()
    });
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
        hooks
            .bound
            .insert((Scope::Global, spec.name), BTreeMap::from([(0, stored)]));
    }
}

/// Which of tmux's three option tables a `set-hook` or `show-hooks` flag
/// set reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Session,
    Window,
    Pane,
}

/// The scope `-g`, `-t` and the table pick: `-g` the global one, else the
/// target's own session, window or pane; a pane table ignores `-g`. A
/// target that is not there is an error naming it, as tmux's.
pub fn scope_in(
    state: &State,
    ctx: &ClientCtx,
    table: Table,
    global: bool,
    target: &Target,
    text: Option<&str>,
) -> Result<Scope, String> {
    let model = &state.model;
    let missing = |what: &str| match text {
        Some(t) => format!("no such {what}: {t}"),
        None => format!("no current {what}"),
    };
    match table {
        Table::Pane => model
            .resolve_pane(target, ctx)
            .map(Scope::Pane)
            .map_err(|_| missing("pane")),
        _ if global => Ok(Scope::Global),
        Table::Session => model
            .resolve_session(target, ctx)
            .map(Scope::Session)
            .map_err(|_| missing("session")),
        Table::Window => model
            .resolve_window(target, ctx)
            .map(Scope::Window)
            .map_err(|_| missing("window")),
    }
}

/// The table `event`'s hook lives in, as `set-hook` picks it: a pane hook
/// is a pane's only with `-p`, a window's otherwise.
pub fn table_of(spec: &EventSpec, pane: bool) -> Table {
    match spec.kind {
        HookKind::Session => Table::Session,
        HookKind::WindowPane if pane => Table::Pane,
        HookKind::WindowPane | HookKind::Window => Table::Window,
    }
}

/// Parses a command string for a hook: split now, and each command must
/// parse (in the request's dialect), as tmux checks at set time; formats in
/// it expand when it runs. An empty or blank string is no commands.
fn parse_command(ctx: &ClientCtx, command: &str) -> Result<Vec<Vec<String>>, String> {
    let cmds = cmd::split_command_string(command).map_err(|e| e.to_string())?;
    for argv in &cmds {
        crate::compat::parse_split(ctx, &unformatted(argv))?;
    }
    Ok(cmds)
}

/// `set-hook [-a] [-g|-p|-t] hook[idx] command` (tmux's array assignment):
/// without an index the array is emptied first unless `append`, and the
/// command takes the first free index (an empty string adds none); with
/// an index that element is replaced. A command that does not parse
/// leaves the array emptied, as tmux does.
pub fn assign(
    state: &mut State,
    ctx: &ClientCtx,
    scope: Scope,
    spec: &'static EventSpec,
    index: Option<u32>,
    append: bool,
    command: &str,
) -> Result<(), String> {
    let key = (scope, spec.name);
    if scope == Scope::Global {
        state.hooks.runtime.insert(spec.name);
    }
    if index.is_none() && !append {
        state.hooks.bound.entry(key).or_default().clear();
    }
    if index.is_none() && command.is_empty() {
        state.hooks.bound.entry(key).or_default();
        return Ok(());
    }
    let cmds = parse_command(ctx, command)?;
    let elems = state.hooks.bound.entry(key).or_default();
    let at = index.unwrap_or_else(|| (0..).find(|i| !elems.contains_key(i)).unwrap_or(0));
    elems.insert(
        at,
        Stored {
            text: command.to_string(),
            cmds,
            dialect: ctx.dialect,
            from_config: false,
        },
    );
    watch::start(state);
    Ok(())
}

/// `set-hook -u hook[idx]`: one element, or the whole array, which falls
/// back to the wider scope (the global array is emptied instead, as tmux
/// resets a global option to its default).
pub fn unbind(state: &mut State, scope: Scope, spec: &'static EventSpec, index: Option<u32>) {
    let key = (scope, spec.name);
    if scope == Scope::Global {
        state.hooks.runtime.insert(spec.name);
    }
    match index {
        Some(i) => {
            if let Some(elems) = state.hooks.bound.get_mut(&key) {
                elems.remove(&i);
            }
        }
        None => {
            state.hooks.bound.remove(&key);
        }
    }
}

/// Drops the hooks of sessions, windows and panes that are gone.
fn prune(state: &mut State) {
    let m = &state.model;
    state.hooks.bound.retain(|(scope, _), _| match scope {
        Scope::Global => true,
        Scope::Session(s) => m.sessions.contains_key(s),
        Scope::Window(w) => m.windows.contains_key(w),
        Scope::Pane(p) => m.panes.contains_key(p),
    });
}

/// `show-hooks` and its flags: `name` alone, or the hooks of the table the
/// flags pick. In the tmux dialect tmux 3.7b's listing ([`show_tmux`]).
pub fn show(
    state: &State,
    ctx: &ClientCtx,
    c: &crate::cmd::ShowHooks,
    index: Option<u32>,
    spec: Option<&'static EventSpec>,
) -> Result<String, String> {
    let tmux = ctx.dialect == Dialect::Tmux;
    let text = c.target_text.as_deref();
    // `named`: the hook was asked for by name, so a global one with
    // nothing in it still shows as its bare name.
    let line = |spec: &EventSpec, scope: Scope, only: Option<u32>, named: bool| -> String {
        let elems = state.hooks.bound.get(&(scope, spec.name));
        let shown = if tmux { spec.tmux } else { spec.name };
        let Some(elems) = elems else {
            return if scope == Scope::Global && (tmux || named) {
                format!("{shown}\n")
            } else {
                String::new()
            };
        };
        let mut out = String::new();
        for (i, s) in elems.iter().filter(|(i, _)| only.is_none_or(|o| o == **i)) {
            let body = if tmux {
                tmux_commands(&s.cmds)
            } else {
                s.text.clone()
            };
            match (tmux, i) {
                (false, 0) => out.push_str(&format!("{shown} {body}\n")),
                _ => out.push_str(&format!("{shown}[{i}] {body}\n")),
            }
        }
        if out.is_empty() && only.is_none() && (tmux || named) {
            out = format!("{shown}\n");
        }
        out
    };
    if let Some(spec) = spec {
        let table = table_of(spec, c.pane);
        let scope = scope_in(state, ctx, table, c.global, &c.target, text)?;
        return Ok(line(spec, scope, index, true));
    }
    let table = if c.pane {
        Table::Pane
    } else if c.window {
        Table::Window
    } else {
        Table::Session
    };
    let scope = scope_in(state, ctx, table, c.global, &c.target, text)?;
    // grofe's own listing of the global hooks has no tables to keep apart.
    let every = !tmux && scope == Scope::Global && !c.window && !c.pane;
    let in_table = |e: &EventSpec| match table {
        _ if every => true,
        Table::Session => e.kind == HookKind::Session,
        Table::Window => e.kind != HookKind::Session,
        Table::Pane => e.kind == HookKind::WindowPane,
    };
    if tmux {
        use crate::compat::tmux::formats::tmux_hook_names;
        let names = tmux_hook_names(table != Table::Session);
        return Ok(names
            .iter()
            .filter_map(|n| EVENTS.iter().find(|e| e.tmux == *n))
            .filter(|e| in_table(e))
            .map(|e| line(e, scope, None, false))
            .collect());
    }
    let mut events: Vec<&EventSpec> = EVENTS.iter().filter(|e| in_table(e)).collect();
    events.sort_by_key(|e| e.name);
    Ok(events
        .into_iter()
        .map(|e| line(e, scope, None, false))
        .collect())
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
    /// Where to look the hook up when it is not the pane's own: the
    /// session or window the event is about (a window just unlinked has no
    /// pane to read it from).
    session: Option<SessionId>,
    window: Option<WindowId>,
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
            session: None,
            window: None,
            vars: HookFormats {
                hook: event,
                ..HookFormats::default()
            },
        }
    }
}

/// Whether a hook is bound to `name` anywhere (the scopes are looked up
/// when it fires).
fn wanted(state: &State, name: &str) -> bool {
    state
        .hooks
        .bound
        .iter()
        .any(|((_, e), elems)| *e == name && elems.values().any(|s| !s.cmds.is_empty()))
}

/// The commands a firing `f` runs: the nearest scope that has the hook
/// set, as tmux's options inherit (a session hook: the session's, else the
/// global; the others: the pane's, the window's, the global). A scope that
/// has the hook set but empty hides the wider ones.
fn commands_for(state: &State, spec: &EventSpec, f: &Fired) -> Vec<Stored> {
    let m = &state.model;
    let pane = f.pane.filter(|p| m.panes.contains_key(p));
    let window = f
        .window
        .or_else(|| pane.and_then(|p| m.panes.get(&p)).map(|p| p.window))
        .filter(|w| m.windows.contains_key(w));
    let session = f
        .session
        .or_else(|| window.and_then(|w| m.windows.get(&w)).map(|w| w.session));
    let get = |scope| state.hooks.bound.get(&(scope, spec.name));
    let own = match spec.kind {
        HookKind::Session => session.and_then(|s| get(Scope::Session(s))),
        HookKind::WindowPane | HookKind::Window => pane
            .and_then(|p| get(Scope::Pane(p)))
            .or_else(|| window.and_then(|w| get(Scope::Window(w)))),
    };
    own.or_else(|| get(Scope::Global))
        .map(|e| e.values().cloned().collect())
        .unwrap_or_default()
}

/// A session's current window's active pane.
fn session_pane(state: &State, session: SessionId) -> Option<PaneId> {
    let s = state.model.sessions.get(&session)?;
    state.model.windows.get(&s.active).map(|w| w.active)
}

/// Queues a notify-shaped hook on the global queue, after the current
/// reply; a full job queue keeps it for the next [`run_pending`].
fn notify(state: &mut State, f: Fired) {
    if state.hooks.quiet {
        return;
    }
    let job: Job = Box::new(move |state: &mut State| enqueue(state, Origin::Global, &f));
    if let Err(TrySendError::Full(job) | TrySendError::Closed(job)) = state.jobs.try_send(job) {
        state.hooks.overflow.push(job);
    }
}

/// `session-created`: new-session made `session`. Its pane is the new
/// session's.
pub fn session_created(state: &mut State, session: SessionId) {
    if let Some(s) = state.model.sessions.get(&session) {
        let name = s.name.clone();
        state.events.emit(Event::SessionCreated { session, name });
    }
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
    if let Some(c) = state.clients.get(&id) {
        let (client, session) = (c.name.clone(), c.session);
        let event = match name {
            "client-resized" => {
                // The size the terminal reported, which applies on the next tick.
                let (cols, rows) = c.pending_size.unwrap_or(c.size);
                Some(Event::ClientResized {
                    client,
                    session,
                    cols,
                    rows,
                })
            }
            "client-session-changed" => Some(Event::ClientSessionChanged { client, session }),
            _ => None,
        };
        if let Some(event) = event {
            state.events.emit(event);
        }
    }
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

/// `pane-set-clipboard`: `pane` set the clipboard (OSC 52).
pub fn pane_set_clipboard(state: &mut State, pane: PaneId) {
    if !wanted(state, "pane-set-clipboard") {
        return;
    }
    if let Some(f) = watch::pane_fired(state, "pane-set-clipboard", pane) {
        notify(state, f);
    }
}

/// `client-attached`: client `id` attached.
pub fn client_attached(state: &mut State, id: ClientId) {
    client_event(state, id, "client-attached");
}

/// `client-detached`: the attached client `name`, which showed `session`,
/// is gone. The hook runs in that session.
pub fn client_detached(state: &mut State, name: &str, session: SessionId) {
    if !wanted(state, "client-detached") {
        return;
    }
    let mut f = Fired::new("client-detached", session_pane(state, session));
    f.vars.hook_client = Some(name.to_string());
    notify(state, f);
}

/// `client-focus-in` and `client-focus-out`: client `id`'s terminal
/// reported that it gained or lost focus (while `focus-events` is on). The
/// panes' focus hooks come before the client's on losing it, after it on
/// gaining it, as tmux orders them.
pub fn client_focus(state: &mut State, id: ClientId, focused: bool) {
    if focused {
        client_event(state, id, "client-focus-in");
        track_focus(state);
    } else {
        track_focus(state);
        client_event(state, id, "client-focus-out");
    }
}

/// `alert-bell`, `alert-activity` or `alert-silence` in `window`
/// (`server::alerts`): run in the window, its active pane current.
pub fn window_alert(state: &mut State, name: &'static str, window: WindowId) {
    let Some(w) = state.model.windows.get(&window) else {
        return;
    };
    let (session, pane) = (w.session, w.active);
    let event = match name {
        "alert-bell" => Some(Event::AlertBell {
            window,
            session,
            pane,
        }),
        "alert-activity" => Some(Event::AlertActivity {
            window,
            session,
            pane,
        }),
        "alert-silence" => Some(Event::AlertSilence {
            window,
            session,
            pane,
        }),
        _ => None,
    };
    if let Some(event) = event {
        state.events.emit(event);
    }
    if !wanted(state, name) {
        return;
    }
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
    if let Some(p) = state.model.panes.get(&pane) {
        let died = Event::PaneDied {
            pane,
            uid: p.uid.clone(),
            window,
            status: p.dead.flatten(),
        };
        state.events.emit(died);
    }
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
    prune(state);
    crate::server::alerts::drain(state);
    for (session, name) in std::mem::take(&mut state.model.closed_sessions) {
        let closed = Event::SessionClosed {
            session,
            name: name.clone(),
        };
        state.events.emit(closed);
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
#[derive(Clone, Copy)]
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
    /// split-window: the highest pane id so far.
    SplitWindow(Option<PaneId>),
    /// new-session: the highest session id so far.
    NewSession(Option<SessionId>),
    /// Any other command with a hook: its target pane, found before it
    /// ran (rename-session changes what its target names).
    Other(Option<PaneId>),
}

/// The `after-<verb>` hook of a command that has no special context, and
/// the target the hook's context follows.
fn after_verb(cmd: &Command) -> Option<(&'static str, Option<&Target>)> {
    Some(match cmd {
        Command::BindKey(_) => ("after-bind-key", None),
        Command::UnbindKey(_) => ("after-unbind-key", None),
        Command::CapturePane(c) => ("after-capture-pane", Some(&c.target)),
        Command::CopyMode(c) => ("after-copy-mode", Some(&c.target)),
        Command::DisplayMessage(c) => ("after-display-message", Some(&c.target)),
        Command::DisplayPanes(_) => ("after-display-panes", None),
        Command::ListBuffers(_) => ("after-list-buffers", None),
        Command::ListClients(_) => ("after-list-clients", None),
        Command::ListKeys(_) => ("after-list-keys", None),
        Command::ListPanes(c) => ("after-list-panes", Some(&c.target)),
        Command::ListSessions(_) => ("after-list-sessions", None),
        Command::ListWindows(c) => ("after-list-windows", Some(&c.target)),
        Command::LoadBuffer(_) => ("after-load-buffer", None),
        Command::PasteBuffer(c) => ("after-paste-buffer", Some(&c.target)),
        Command::RefreshClient(_) => ("after-refresh-client", None),
        Command::RenameSession(c) => ("after-rename-session", Some(&c.target)),
        Command::RenameWindow(c) => ("after-rename-window", Some(&c.target)),
        Command::SaveBuffer(_) => ("after-save-buffer", None),
        Command::SelectLayout(c) => ("after-select-layout", Some(&c.target)),
        Command::SendKeys(c) => ("after-send-keys", Some(&c.target)),
        Command::SetBuffer(_) => ("after-set-buffer", None),
        Command::SetEnvironment(c) => ("after-set-environment", Some(&c.target)),
        Command::SetHook(c) => ("after-set-hook", Some(&c.target)),
        Command::SetOption(c) => ("after-set-option", Some(&c.target)),
        Command::ShowEnvironment(c) => ("after-show-environment", Some(&c.target)),
        Command::ShowMessages(_) => ("after-show-messages", None),
        Command::ShowOptions(c) => ("after-show-options", Some(&c.target)),
        _ => return None,
    })
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
                let (session, _) = m.resolve_winlink(&c.target, ctx).ok()?;
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
            Command::SplitWindow(_) => (
                "after-split-window",
                Before::SplitWindow(m.panes.keys().next_back().copied()),
            ),
            Command::NewSession(_) => (
                "after-new-session",
                Before::NewSession(m.sessions.keys().next_back().copied()),
            ),
            other => {
                let (name, target) = after_verb(other)?;
                if !wanted(state, name) && !sets_hooks(name) {
                    return None;
                }
                let pane = target
                    .and_then(|t| m.resolve_pane(t, ctx).ok())
                    .or_else(|| context_pane(state, ctx.pane));
                (name, Before::Other(pane))
            }
        };
        Some(before)
    }

    /// The stream event once the command succeeded, for the four verbs the
    /// catalog covers.
    fn event(self, state: &State) -> Option<Event> {
        let m = &state.model;
        match self {
            Before::SelectWindow(session) => {
                let window = m.sessions.get(&session)?.active;
                let pane = m.windows.get(&window)?.active;
                Some(Event::WindowFocused {
                    window,
                    session,
                    pane,
                })
            }
            Before::NewWindow(last) => {
                let window = m.windows.keys().copied().find(|w| Some(*w) > last)?;
                let w = m.windows.get(&window)?;
                Some(Event::WindowCreated {
                    window,
                    session: w.session,
                    pane: w.active,
                })
            }
            Before::ResizePane(pane) => Some(Event::PaneResized {
                pane,
                window: m.panes.get(&pane)?.window,
            }),
            Before::ResizeWindow(window) => Some(Event::WindowResized {
                window,
                session: m.windows.get(&window)?.session,
            }),
            Before::SelectPane(..)
            | Before::KillPane
            | Before::SplitWindow(_)
            | Before::NewSession(_)
            | Before::Other(_) => None,
        }
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
            Before::SplitWindow(last) => m.panes.keys().copied().find(|p| Some(*p) > last),
            Before::NewSession(last) => {
                let new = m.sessions.keys().copied().find(|s| Some(*s) > last)?;
                session_pane(state, new)
            }
            Before::Other(pane) => pane,
        })
    }
}

/// Whether a command's hook can be bound by the command itself, which so
/// has to be looked for after it ran (`set-hook -g after-set-hook`).
fn sets_hooks(name: &str) -> bool {
    matches!(name, "after-set-hook" | "after-set-option")
}

/// Whether the command-shaped hook `name` has an event on the stream.
fn streamed(name: &str) -> bool {
    matches!(
        name,
        "window-focused" | "window-created" | "pane-resized" | "window-resized"
    )
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
    // The stream events fire for hook commands too; the hooks do not.
    let before = Before::of(state, ctx, &cmd).filter(|(name, _)| {
        streamed(name) || (!state.hooks.running && (wanted(state, name) || sets_hooks(name)))
    });
    let reported = reported_window(state, ctx, &cmd);
    let result = route(state, ctx, cmd);
    if result.is_ok()
        && let Some((event, window, times)) = reported
    {
        watch::window_event(state, event, window, times);
    }
    watch::after_command(state);
    // A failed command runs `command-error` instead, in the current
    // context: what the hook prints goes out with the error, and its own
    // errors after the command's, as tmux prints them.
    if let Err(e) = &result
        && !state.hooks.running
        && wanted(state, "command-error")
    {
        let mut f = Fired::new("command-error", context_pane(state, ctx.pane));
        f.client = ctx.client;
        let (printed, errors) = fire_now(state, ctx, &f);
        state.failed_out.push_str(&printed);
        if !errors.is_empty() {
            return Err(std::iter::once(e.clone())
                .chain(errors)
                .collect::<Vec<_>>()
                .join("\n"));
        }
    }
    let (Ok(out), Some((name, before))) = (&result, before) else {
        return result;
    };
    if let Some(event) = before.event(state) {
        state.events.emit(event);
    }
    if state.hooks.running || !wanted(state, name) {
        return result;
    }
    let Some(pane) = before.pane(state, ctx) else {
        return result;
    };
    let mut f = Fired::new(name, pane);
    f.client = ctx.client;
    let mut out = out.clone();
    // ponytail: an after-* hook's errors are dropped; tmux prints them and
    // exits 1 while the request's later commands still run.
    out.push_str(&fire_now(state, ctx, &f).0);
    Ok(out)
}

/// The window a command that tmux always reports on is about, before it
/// runs (a rename changes what `-t` names), with the hook and how many
/// times tmux fires it ([`watch::window_event`]).
fn reported_window(
    state: &State,
    ctx: &ClientCtx,
    cmd: &Command,
) -> Option<(&'static str, WindowId, usize)> {
    if !watch::watched(state) {
        return None;
    }
    let m = &state.model;
    let as_pane = |t: &Target| {
        m.pane_type(t, ctx)
            .ok()
            .and_then(|t| m.resolve_window(&t, ctx).ok())
    };
    match cmd {
        Command::RenameWindow(c) => {
            Some(("window-renamed", m.resolve_window(&c.target, ctx).ok()?, 1))
        }
        // `-E` spreads the cells without a preset's own notify: once.
        Command::SelectLayout(c) if c.spread => {
            let pane = m.resolve_pane(&c.target, ctx).ok()?;
            Some(("window-layout-changed", m.panes.get(&pane)?.window, 1))
        }
        Command::SelectLayout(c) => Some(("window-layout-changed", as_pane(&c.target)?, 2)),
        Command::NextLayout(c) => Some(("window-layout-changed", as_pane(&c.target)?, 2)),
        _ => None,
    }
}

/// Runs `f`'s hook on its origin's queue: the attached client's for a
/// binding, else the CLI request's own. What it printed and the errors
/// its commands failed with so far, for the command's reply.
fn fire_now(state: &mut State, ctx: &ClientCtx, f: &Fired) -> (String, Vec<String>) {
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
    enqueue(state, origin, f);
    let mut out = String::new();
    let mut errors = Vec::new();
    if let Some(q) = state.hooks.queues.get_mut(&origin) {
        out.push_str(&std::mem::take(&mut q.out));
        errors = std::mem::take(&mut q.errors);
        if !q.waiting && q.items.is_empty() {
            state.hooks.queues.remove(&origin);
        }
    }
    (out, errors)
}

/// `set-hook -R`: runs `spec`'s hook now as if it had fired for `pane`,
/// with only `#{hook}` set. What the hook printed is the reply.
pub fn run_now(
    state: &mut State,
    ctx: &ClientCtx,
    spec: &'static EventSpec,
    pane: Option<PaneId>,
) -> String {
    let mut f = Fired::new(spec.name, pane);
    f.client = ctx.client;
    fire_now(state, ctx, &f).0
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
    /// The errors its hook commands failed with, for a CLI request's
    /// reply when `command-error` ran them.
    errors: Vec<String>,
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
    let Some(spec) = event(f.event) else {
        return;
    };
    let stored = commands_for(state, spec, f);
    for stored in stored.into_iter().filter(|s| !s.cmds.is_empty()) {
        let mut fired = f.clone();
        // A hook set in the tmux dialect sees tmux's name in `#{hook}`.
        if stored.dialect == Dialect::Tmux {
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
    }
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
        // A notify hook's commands fire nothing (`quiet`); what changed
        // before them fires first.
        let quiet = origin == Origin::Global && item.fired.event != "load-commands";
        if quiet {
            watch::track(state);
        }
        state.hooks.running = true;
        state.hooks.quiet = quiet;
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
                    if let Some(q) = state.hooks.queues.get_mut(&origin) {
                        q.errors.push(e);
                    }
                    item.cmds.clear();
                }
            }
            shell = state.deferred_reply.take();
        }
        state.hooks.current = None;
        state.deferred_reply = deferred;
        state.hooks.running = false;
        state.hooks.quiet = false;
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

/// When client `id`'s hook queue waits on a shell (a binding's command
/// fired an `after-*` hook that runs `run-shell`), a receiver told once
/// that queue is idle: the binding's later commands and the client's
/// input wait for it, as tmux runs the hook on the client's own queue.
pub fn take_client_wait(state: &mut State, id: ClientId) -> Option<oneshot::Receiver<String>> {
    let hooks = &mut state.hooks;
    if hooks.running {
        return None;
    }
    let q = hooks
        .queues
        .get_mut(&Origin::Client(id))
        .filter(|q| q.waiting)?;
    let (tx, rx) = oneshot::channel();
    q.idle.push(tx);
    Some(rx)
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
    pub inner: Inner<'a>,
}

impl FormatVars for HookVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.formats
            .get(name)
            .or_else(|| self.inner.vars().get(name))
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let items = self.inner.vars().loop_items(kind, sort)?;
        Some(
            items
                .into_iter()
                .map(|it| LoopItem {
                    vars: Box::new(HookVars {
                        formats: self.formats,
                        inner: Inner::Own(it.vars),
                    }),
                    ..it
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.inner.vars().name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.inner.vars().pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        self.inner.vars().reexpand(text)
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
        .filter_map(|(k, v)| Some((k.clone(), v.value.clone()?)))
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
            env.insert(
                name.to_string(),
                Var {
                    value: Some(value),
                    hidden,
                },
            );
        }
        None => {
            env.remove(name);
        }
    }
}

/// `set-environment -gr name`: the global variable is marked removed
/// (`-NAME`), so a new pane lacks it, as tmux's `environ_clear`: an entry
/// that is there keeps its hidden mark and loses its value, else a new one
/// is made.
pub fn clear_global(state: &mut State, name: &str) {
    state
        .hooks
        .global
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(name.to_string())
        .and_modify(|v| v.value = None)
        .or_insert(Var {
            value: None,
            hidden: false,
        });
}

/// `set-environment -t session -r name`: as [`clear_global`] for a session.
pub fn clear_session(state: &mut State, session: SessionId, name: &str) {
    let Some(s) = state.model.sessions.get_mut(&session) else {
        return;
    };
    s.env.retain(|(k, _)| k != name);
    let hidden = state.hooks.session_hidden.entry(session).or_default();
    if let Some(v) = hidden.get_mut(name) {
        *v = None;
        return;
    }
    state
        .hooks
        .session_removed
        .entry(session)
        .or_default()
        .insert(name.to_string());
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
            hidden_vars.insert(name.to_string(), Some(value));
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
                value: v.value.clone(),
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
            vars.insert(k, (v.as_deref(), true));
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
        assert_eq!(EVENTS.len(), 68);
        let mut names: Vec<&str> = EVENTS.iter().map(|e| e.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), EVENTS.len());
    }

    #[test]
    fn a_hook_command_must_parse_and_formats_may_be_targets() {
        let mut state = test_state();
        let ctx = test_ctx();
        let spec = event("pane-focused").unwrap();
        let set = |state: &mut State, command: &str| {
            assign(state, &ctx, Scope::Global, spec, None, false, command)
        };
        assert_eq!(
            set(&mut state, "no-such-verb"),
            Err("unknown command: no-such-verb".to_string())
        );
        let cmd = "select-pane -t #{hook_pane} ; list-sessions";
        assert_eq!(set(&mut state, cmd), Ok(()));
        let shown = crate::cmd::ShowHooks {
            global: true,
            ..Default::default()
        };
        assert_eq!(
            show(&state, &ctx, &shown, None, None),
            Ok(format!("pane-focused {cmd}\n"))
        );
        assert_eq!(check_command(cmd).map(|c| c.len()), Ok(2));
        assert_eq!(check_command("  "), Ok(Vec::new()));
    }

    #[test]
    fn an_array_takes_the_first_free_index_and_appends() {
        let mut state = test_state();
        let ctx = test_ctx();
        let spec = event("pane-focused").unwrap();
        let global = Scope::Global;
        assign(&mut state, &ctx, global, spec, None, false, "list-sessions").unwrap();
        assign(&mut state, &ctx, global, spec, None, true, "list-clients").unwrap();
        assign(
            &mut state,
            &ctx,
            global,
            spec,
            Some(5),
            true,
            "list-windows",
        )
        .unwrap();
        assign(&mut state, &ctx, global, spec, None, true, "list-keys").unwrap();
        let shown = crate::cmd::ShowHooks {
            global: true,
            ..Default::default()
        };
        assert_eq!(
            show(&state, &ctx, &shown, None, None),
            Ok("pane-focused list-sessions\npane-focused[1] list-clients\n\
                pane-focused[2] list-keys\npane-focused[5] list-windows\n"
                .to_string())
        );
        unbind(&mut state, global, spec, Some(1));
        assign(&mut state, &ctx, global, spec, None, false, "list-panes").unwrap();
        assert_eq!(
            show(&state, &ctx, &shown, None, None),
            Ok("pane-focused list-panes\n".to_string())
        );
    }

    #[tokio::test]
    async fn a_session_hook_does_not_fire_for_the_session_it_makes() {
        let mut state = test_state();
        // The hook's own new-session fires no session-created, as in tmux
        // (recorded, `golden/b2a/hook-quiet`): one session more, not one
        // per request from now on.
        let hook = "new-session -d";
        fire(&mut state, &["set-hook", "-g", "session-created", hook]).unwrap();
        fire(&mut state, &["new-session", "-d", "-s", "a"]).unwrap();
        fire(&mut state, &["list-sessions"]).unwrap();
        fire(&mut state, &["list-sessions"]).unwrap();
        let names: Vec<&str> = state
            .model
            .sessions
            .values()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
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
