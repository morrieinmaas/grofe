//! Command dispatcher match skeleton.
//!
//! [`run`] parses argv into a [`Command`] and routes it: session commands to
//! [`session`], window commands (and the default `list-panes`) to
//! [`window`], pane commands to [`pane`], and option and format commands
//! (`set-option`, `show-options`, `display-message`, `-F` on the list
//! commands) to [`options`]. After every command it checks the lifecycle
//! cascade: once the last session is gone the server exits.
//!
//! Shared helpers for handlers that spawn panes live here too:
//! [`spawn_pane`], [`command_argv`] and [`resolve_cwd`].

pub mod options;
pub mod pane;
pub mod session;
pub mod window;

use std::path::{Path, PathBuf};

use ulid::Ulid;

use crate::cmd::{self, Command};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::model::{HISTORY_LIMIT, Model, Pane};
use crate::server::pane::{PaneHandle, PaneSpawn};
use crate::types::{PaneId, WindowId};

/// Runs one command (`argv[0]` is the verb) against the server state,
/// returning its stdout or its error message.
pub fn run(state: &mut State, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    let cmd = cmd::parse(argv).map_err(|e| e.to_string())?;
    let result = route(state, ctx, cmd);
    if state.model.closed() {
        state.exit = true;
    }
    result
}

fn route(state: &mut State, ctx: &ClientCtx, cmd: Command) -> Result<String, String> {
    match cmd {
        Command::NewSession(c) => session::new_session(state, ctx, c),
        Command::KillSession(c) => session::kill_session(state, ctx, c),
        Command::HasSession(c) => session::has_session(state, ctx, c),
        Command::ListSessions(c) if c.format.is_none() => session::list_sessions(state, c),

        Command::NewWindow(c) => window::new_window(state, ctx, c),
        Command::KillWindow(c) => window::kill_window(state, ctx, c),
        Command::SelectWindow(c) => window::select_window(state, ctx, c),
        Command::ListWindows(c) if c.format.is_none() => window::list_windows(state, ctx, c),
        Command::ListPanes(c) if c.format.is_none() => window::list_panes(state, ctx, c),

        Command::SplitWindow(_)
        | Command::KillPane(_)
        | Command::SelectPane(_)
        | Command::ResizePane(_)
        | Command::RespawnPane(_)
        | Command::SendKeys(_)
        | Command::CapturePane(_) => pane::run(state, ctx, cmd),

        Command::SetOption(_)
        | Command::ShowOptions(_)
        | Command::DisplayMessage(_)
        | Command::ListSessions(_)
        | Command::ListWindows(_)
        | Command::ListPanes(_) => options::run(state, ctx, cmd),

        Command::KillServer(_) => {
            let live = state.model.live_panes();
            state.events.panes_killed(live);
            state.exit = true;
            Ok(String::new())
        }
        // The actor handles `subscribe` itself (`ServerMsg::Subscribe`),
        // since it needs the connection's event queue.
        Command::Subscribe(_) => Err("subscribe needs a control connection".to_string()),

        // M2: T2.5a (attach, detach, list-clients) and T2.5b (the rest)
        // replace these stubs.
        Command::AttachSession(_) => not_yet("attach-session"),
        Command::DetachClient(_) => not_yet("detach-client"),
        Command::ListClients(_) => not_yet("list-clients"),
        Command::NextWindow(_) => not_yet("next-window"),
        Command::PreviousWindow(_) => not_yet("previous-window"),
        Command::LastWindow(_) => not_yet("last-window"),
        Command::RenameSession(_) => not_yet("rename-session"),
        Command::RenameWindow(_) => not_yet("rename-window"),
        Command::SwitchClient(_) => not_yet("switch-client"),
        Command::SendPrefix(_) => not_yet("send-prefix"),
    }
}

fn not_yet(verb: &str) -> Result<String, String> {
    Err(format!("{verb}: not implemented yet"))
}

/// The argv a trailing shell command runs as: a single argument goes
/// through `/bin/sh -c` (as tmux does), several are executed directly, none
/// means the user's shell.
pub(crate) fn command_argv(command: Option<Vec<String>>) -> Option<Vec<String>> {
    match command {
        Some(mut args) if args.len() == 1 => {
            let script = args.remove(0);
            Some(vec!["/bin/sh".to_string(), "-c".to_string(), script])
        }
        other => other,
    }
}

/// A `-c` value, its formats expanded against the `target` pane (spec
/// §4.5: at execution time, so `#{pane_current_path}` is the target's cwd
/// now) and made absolute against the request's cwd; `default` when no
/// `-c` was given. With no target pane, variables expand to nothing.
pub(crate) fn resolve_cwd(
    model: &Model,
    ctx: &ClientCtx,
    target: Option<PaneId>,
    dir: Option<&str>,
    default: &Path,
) -> PathBuf {
    let Some(dir) = dir else {
        return default.to_path_buf();
    };
    let dir = match target {
        Some(pane) => cmd::expand(dir, &options::FormatCtx::new(model, pane)),
        None => cmd::expand(dir, &NoVars),
    };
    ctx.cwd.join(dir)
}

struct NoVars;

impl cmd::FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

/// Spawns a pane for `window` (which may not be in the model yet) with
/// `env` (its session's), inserts it into `state.model.panes`, and returns
/// its id. The caller puts it in the window's layout.
pub(crate) fn spawn_pane(
    state: &mut State,
    window: WindowId,
    env: Vec<(String, String)>,
    argv: Option<Vec<String>>,
    cwd: PathBuf,
    (cols, rows): (u16, u16),
) -> Result<PaneId, String> {
    let id = state.model.alloc_pane_id();
    let spec = PaneSpawn {
        id,
        argv: argv.clone(),
        cwd: cwd.clone(),
        env,
        cols,
        rows,
        history_limit: HISTORY_LIMIT,
        socket: state.socket.clone(),
        term: state.inner_term,
    };
    // Deliberately unlike tmux, a bad cwd fails here rather than falling back to $HOME.
    let handle = PaneHandle::spawn(spec, state.pane_events.clone(), state.wake.clone())
        .map_err(|e| format!("can't start pane in {}: {e}", cwd.display()))?;
    state.model.panes.insert(
        id,
        Pane {
            id,
            uid: Ulid::generate().to_string(),
            window,
            resume: argv.clone(),
            argv,
            cwd,
            dead: None,
            cols,
            rows,
            handle,
        },
    );
    Ok(id)
}
