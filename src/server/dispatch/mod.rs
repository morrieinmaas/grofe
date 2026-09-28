//! Command dispatcher match skeleton.
//!
//! [`run`] parses argv into a [`Command`] and routes it: session commands to
//! [`session`], window commands (and the default `list-panes`) to
//! [`window`], pane commands to [`pane`], and option and format commands
//! (`set-option`, `show-options`, `display-message`, `-F` on the list
//! commands) to [`options`]. M3's commands route the same way: key
//! bindings and config (`bind-key`, `unbind-key`, `list-keys`,
//! `source-file`, `reload`) to [`options`], `command-prompt` and
//! `run-shell` to `server::prompt`, `select-layout` to [`window`]. After
//! every command it checks the lifecycle cascade: once the last session is
//! gone the server exits.
//!
//! Shared helpers for handlers that spawn panes live here too:
//! [`spawn_pane`], [`command_argv`] and [`resolve_cwd`].

pub mod options;
pub mod pane;
pub mod session;
pub mod window;

use std::path::{Path, PathBuf};

use tokio::sync::oneshot;
use ulid::Ulid;

use crate::cmd::{self, Command};
use crate::protocol::{ATTACH_CHECK, ClientCtx};
use crate::server::model::{HISTORY_LIMIT, Model, Pane};
use crate::server::pane::{PaneHandle, PaneSpawn, usable_cwd};
use crate::server::{State, clients, graphics, prompt};
use crate::types::{PaneId, WindowId};

/// Runs one command (`argv[0]` is the verb) against the server state,
/// returning its stdout or its error message.
///
/// argv may hold several commands ([`cmd::parse_sequence`]): they run in
/// order and the first error stops the rest.
// ponytail: the reply is the output of every command, or the first error
// alone; output of commands before a failing one is dropped.
pub fn run(state: &mut State, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    if argv.first().is_some_and(|verb| verb == ATTACH_CHECK) {
        return clients::attach_check(state, ctx, &argv[1..]);
    }
    let cmds = cmd::parse_sequence(argv).map_err(|e| e.to_string())?;
    // Names follow their panes (automatic-rename) before a command reads
    // or targets them.
    window::automatic_rename(&mut state.model);
    let mut out = String::new();
    let mut result = Ok(());
    for cmd in cmds {
        match route(state, ctx, cmd) {
            Ok(text) => out.push_str(&text),
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if state.model.closed() {
        state.exit = true;
    }
    result.map(|()| out)
}

/// For a handler whose answer comes later (`run-shell` from the CLI): the
/// actor does not wait. The request's reply is whatever is sent on the
/// returned sender, which the handler moves into its own task; the value
/// the handler returns is ignored. From a binding the answer is dropped.
// ponytail: one deferred answer per request; a second call replaces the
// first, whose sender then fails.
pub fn defer_reply(state: &mut State) -> oneshot::Sender<Result<String, String>> {
    let (tx, rx) = oneshot::channel();
    state.deferred_reply = Some(rx);
    tx
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

        // M2: T2.5a (attach, detach, list-clients) and T2.5b (the rest).
        Command::AttachSession(c) => clients::attach_session(state, ctx, c),
        Command::DetachClient(c) => clients::detach_client(state, ctx, c),
        Command::ListClients(c) => clients::list_clients(state, c),
        Command::NextWindow(c) => window::step_window(state, ctx, c, window::Step::Next),
        Command::PreviousWindow(c) => window::step_window(state, ctx, c, window::Step::Previous),
        Command::LastWindow(c) => window::last_window(state, ctx, c),
        Command::RenameSession(c) => session::rename_session(state, ctx, c),
        Command::RenameWindow(c) => window::rename_window(state, ctx, c),
        Command::SwitchClient(c) => session::switch_client(state, ctx, c),
        Command::SendPrefix(c) => pane::send_prefix(state, ctx, c),

        // M3: bindings and config (T3.5), the prompt and run-shell (T3.4),
        // layout presets (T3.7).
        Command::BindKey(c) => options::bind_key(state, c),
        Command::UnbindKey(c) => options::unbind_key(state, ctx, c),
        Command::ListKeys(c) => options::list_keys(state, ctx, c),
        Command::SourceFile(c) => options::source_file(state, ctx, c),
        Command::Reload(c) => options::reload(state, ctx, c),
        Command::CommandPrompt(c) => prompt::command_prompt(state, ctx, c),
        Command::RunShell(c) => prompt::run_shell(state, ctx, c),
        Command::SelectLayout(c) => window::select_layout(state, ctx, c),
    }
}

/// The error a command's stub answers with until its task lands.
pub(crate) fn not_implemented(verb: &str) -> Result<String, String> {
    Err(format!("not implemented: {verb}"))
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
/// its id. The caller puts it in the window's layout. `cell_px` is
/// [`clients::cell_px`] for the window's session.
pub(crate) fn spawn_pane(
    state: &mut State,
    window: WindowId,
    env: Vec<(String, String)>,
    argv: Option<Vec<String>>,
    cwd: PathBuf,
    (cols, rows): (u16, u16),
    cell_px: Option<(u16, u16)>,
) -> Result<PaneId, String> {
    let id = state.model.alloc_pane_id();
    let cwd = usable_cwd(cwd);
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
        cell_px,
        graphics_ok: graphics::panes_graphics_ok(state),
    };
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
