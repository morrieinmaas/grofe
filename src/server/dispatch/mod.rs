//! Command dispatcher match skeleton.
//!
//! [`run`] parses argv into a [`Command`] and routes it: session commands to
//! [`session`], window commands (and the default `list-panes`) to
//! [`window`], pane commands to [`pane`], and option and format commands
//! (`set-option`, `show-options`, `display-message`, `-F` on the list
//! commands) to [`options`]. M3's commands route the same way: key
//! bindings and config (`bind-key`, `unbind-key`, `list-keys`,
//! `source-file`, `reload`) to [`options`], `command-prompt` and
//! `run-shell` to `server::prompt`, `select-layout` to [`window`]. M3.6's
//! verbs route to one file per owning task: [`structure`] (TC.8),
//! [`client`] (TC.9), [`popup`] (TC.6), [`menu`] (TC.7), [`bindings`]
//! (TC.10) and [`hooks`] (TC.5); M5's buffer verbs to [`buffers`] (TM.4).
//! After every command it runs the pending
//! event hooks (`server::hooks`) and checks the lifecycle cascade: once the
//! last session is gone the server exits.
//!
//! Parsing goes through `compat` (the request's dialect decides between
//! the native parser and the tmux front end), after grofe aliases are
//! expanded ([`menu::expand_aliases`]). No M3.6 task edits this file.
//!
//! Shared helpers for handlers that spawn panes live here too:
//! [`spawn_pane`] and [`resolve_cwd`].

pub mod bindings;
pub mod buffers;
pub mod client;
pub mod copy;
pub mod hooks;
pub mod lock;
pub mod menu;
pub mod options;
pub mod pane;
pub mod persist;
pub mod plugins;
pub mod popup;
pub mod scratch;
pub mod session;
pub mod sorting;
pub mod structure;
pub mod window;

use std::path::{Path, PathBuf};

use tokio::sync::oneshot;
use ulid::Ulid;

use crate::cmd::{self, Command};
use crate::protocol::{ATTACH_CHECK, ClientCtx};
use crate::server::model::{Model, Pane};
use crate::server::pane::{PaneHandle, PaneSpawn, pane_shell, usable_cwd};
use crate::server::{State, clients, graphics, prompt};
use crate::types::{PaneId, SessionId, WindowId};
use crate::{compat, server};

/// A request, parsed once ([`parse`]) and then both classified
/// ([`read_only`]) and run ([`run_parsed`]), so a front-end miss is logged
/// once per request.
#[derive(Debug)]
pub enum Parsed {
    /// [`ATTACH_CHECK`] with `attach-session`'s arguments.
    AttachCheck(Vec<String>),
    Commands(Vec<Command>),
}

/// Parses a request's argv (`argv[0]` is the verb): grofe aliases
/// expanded ([`menu::expand_aliases`]), then split and parsed in the
/// request's dialect (`compat::parse_request`).
pub fn parse(state: &State, ctx: &ClientCtx, argv: &[String]) -> Result<Parsed, String> {
    if argv.first().is_some_and(|verb| verb == ATTACH_CHECK) {
        return Ok(Parsed::AttachCheck(argv[1..].to_vec()));
    }
    let expanded = menu::expand_aliases(state, argv);
    compat::parse_request(ctx, expanded.as_deref().unwrap_or(argv)).map(Parsed::Commands)
}

/// Runs a request's argv against the server state, returning its stdout
/// or its error message: [`parse`], then [`run_parsed`].
pub fn run(state: &mut State, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    let parsed = parse(state, ctx, argv);
    run_parsed(state, ctx, parsed)
}

/// Runs a parsed request. argv may have held several commands, split at
/// `;` arguments as tmux's CLI splits them ([`cmd::parse_sequence`]): they
/// run in order and the first error stops the rest; what the commands
/// before it printed goes out with the error (`State::failed_out`).
pub fn run_parsed(
    state: &mut State,
    ctx: &ClientCtx,
    parsed: Result<Parsed, String>,
) -> Result<String, String> {
    match parsed? {
        Parsed::AttachCheck(args) => clients::attach_check(state, ctx, &args),
        Parsed::Commands(cmds) => run_commands(state, ctx, cmds),
    }
}

/// Runs one command whose argv is already split (a binding, the prompt,
/// the mouse): a `;` argument in it is literal, never a separator. An
/// alias verb may stand for several commands ([`menu::expand_alias_command`]).
pub fn run_split(state: &mut State, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    let cmds = parse_split(state, ctx, argv)?;
    run_commands(state, ctx, cmds)
}

/// [`run_split`]'s parse: one split command, an alias verb expanded.
pub(crate) fn parse_split(
    state: &State,
    ctx: &ClientCtx,
    argv: &[String],
) -> Result<Vec<Command>, String> {
    let Some(expanded) = menu::expand_alias_command(state, argv) else {
        return compat::parse_split(ctx, argv);
    };
    let mut cmds = Vec::new();
    for argv in &expanded {
        cmds.extend(compat::parse_split(ctx, argv)?);
    }
    Ok(cmds)
}

/// [`parse_split`] for a sourced tmux file's command, a failure marked
/// `true` when tmux itself refuses the command at parse time
/// ([`compat::parse_split_checked`]).
pub(crate) fn parse_split_checked(
    state: &State,
    ctx: &ClientCtx,
    argv: &[String],
) -> Result<Vec<Command>, (bool, String)> {
    let Some(expanded) = menu::expand_alias_command(state, argv) else {
        return compat::parse_split_checked(ctx, argv);
    };
    let mut cmds = Vec::new();
    for argv in &expanded {
        cmds.extend(compat::parse_split_checked(ctx, argv)?);
    }
    Ok(cmds)
}

/// Runs one command of a key binding: as [`run_split`], without
/// expanding an alias, since `bind-key` expanded it when the key was
/// bound, as tmux 3.7b does (a chain `a=b`, `b=...` expands once).
pub fn run_bound(state: &mut State, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    let cmds = compat::parse_split(ctx, argv)?;
    run_commands(state, ctx, cmds)
}

pub(crate) fn run_commands(
    state: &mut State,
    ctx: &ClientCtx,
    cmds: Vec<Command>,
) -> Result<String, String> {
    // Names follow their panes (automatic-rename) before a command reads
    // or targets them. A new name is drawn even if the command only reads.
    if window::automatic_rename(&mut state.model) {
        clients::mark_all_dirty(state);
    }
    let mut out = String::new();
    let mut result = Ok(());
    let mut cmds = cmds.into_iter();
    server::hooks::enter(state);
    while let Some(cmd) = cmds.next() {
        let answered = state.deferred_reply.is_none();
        let shell = matches!(cmd, Command::RunShell(_));
        if server::hooks::depth(state) == 1 && !server::hooks::in_hook(state) {
            clients::log_fired(state);
        }
        match server::hooks::run_command(state, ctx, cmd, route) {
            Ok(text) => out.push_str(&text),
            Err(e) => {
                // What ran before the failing command still prints, before
                // the error (and before what a nested request that failed
                // printed), as in tmux 3.7b.
                let nested = std::mem::take(&mut state.failed_out);
                state.failed_out = std::mem::take(&mut out) + &nested;
                result = Err(e);
                break;
            }
        }
        crate::server::clock::sweep(state);
        // A command that answers later (a blocked `wait-for`, `run-shell`)
        // holds the rest of the request until it answers, as tmux 3.7b's
        // command queue waits (TC.9), and so does a hook waiting on its
        // shell. Inside another command (`if -F`) the rest of that one is
        // held too, after this rest.
        // A hook's own commands wait on its queue instead (`hooks::drain`).
        if server::hooks::in_hook(state) {
            continue;
        }
        let nested = server::hooks::depth(state) > 1;
        if answered && state.deferred_reply.is_some() && (cmds.len() > 0 || nested) {
            let held = state.held.get_or_insert_default();
            held.wait = state.deferred_reply.take();
            held.shell = shell;
        }
        let hook_waits = cmds.len() > 0
            && state.deferred_reply.is_none()
            && server::hooks::request_waiting(state);
        if state.held.is_some() || hook_waits {
            let rest: Vec<Command> = cmds.collect();
            let held = state.held.get_or_insert_default();
            if !rest.is_empty() {
                held.frames.push(client::HeldFrame {
                    ctx: ctx.clone(),
                    cmds: rest,
                    keep_going: false,
                    depth: options::source_depth(),
                    verbose: options::source_verbose(),
                });
            }
            break;
        }
    }
    server::hooks::leave(state);
    if server::hooks::depth(state) == 0
        && let Some(held) = state.held.take()
    {
        client::hold_rest(state, ctx, held, std::mem::take(&mut out), None, None);
    }
    server::hooks::run_pending(state);
    if state.model.closed() {
        state.exit = true;
    }
    result.map(|()| out)
}

/// Whether a request only reads: nothing an attached client draws can
/// change, so its clients need no redraw (a redraw of a large client is
/// actor time every other request waits behind). Unknown or unparsable
/// requests count as changing something.
// ponytail: an explicit list. The upgrade path is a per-command "changed"
// result from dispatch, so each handler says what it touched.
pub fn read_only(parsed: &Result<Parsed, String>) -> bool {
    let cmds = match parsed {
        Ok(Parsed::AttachCheck(_)) => return true,
        Ok(Parsed::Commands(cmds)) => cmds,
        Err(_) => return false,
    };
    cmds.iter().all(|c| {
        matches!(
            c,
            Command::ListSessions(_)
                | Command::ListWindows(_)
                | Command::ListPanes(_)
                | Command::ListClients(_)
                | Command::ListKeys(_)
                | Command::ShowOptions(_)
                | Command::PromptHistory(cmd::PromptHistory { clear: false, .. })
                | Command::HasSession(_)
                | Command::CapturePane(_)
                | Command::ShowBuffer(_)
                | Command::SaveBuffer(_)
                | Command::ListBuffers(_)
                | Command::DisplayMessage(cmd::DisplayMessage { print: true, .. })
        )
    })
}

/// For a handler whose answer comes later (`run-shell` from the CLI): the
/// actor does not wait. The request's reply is whatever is sent on the
/// returned sender, which the handler moves into its own task; the value
/// the handler returns is ignored. From a binding the answer is dropped.
// ponytail: one deferred answer per request; a second call replaces the
// first, whose sender then fails.
pub fn defer_reply(state: &mut State) -> oneshot::Sender<Result<String, crate::protocol::Failure>> {
    let (tx, rx) = oneshot::channel();
    state.deferred_reply = Some(rx);
    tx
}

fn route(state: &mut State, ctx: &ClientCtx, cmd: Command) -> Result<String, String> {
    match cmd {
        Command::NewSession(c) => session::new_session(state, ctx, c),
        Command::KillSession(c) => session::kill_session(state, ctx, c),
        Command::HasSession(c) => session::has_session(state, ctx, c),
        // With no `-F`, grofe's own columns natively, tmux's default
        // templates (`options`) in the tmux dialect (FE.2).
        Command::ListSessions(c) if c.format.is_none() && !tmux(ctx) => {
            session::list_sessions(state, c)
        }

        Command::NewWindow(c) => window::new_window(state, ctx, c),
        Command::KillWindow(c) => window::kill_window(state, ctx, c),
        Command::SelectWindow(c) => window::select_window(state, ctx, c),
        Command::ListWindows(c) if c.format.is_none() && !tmux(ctx) => {
            window::list_windows(state, ctx, c)
        }
        Command::ListPanes(c) if c.format.is_none() && !tmux(ctx) => {
            window::list_panes(state, ctx, c)
        }

        // The mouse event of a mouse key's commands goes to its pane.
        Command::SendKeys(c) if c.mouse => server::mouse::send_current(state),

        Command::SplitWindow(_)
        | Command::KillPane(_)
        | Command::SelectPane(_)
        | Command::ResizePane(_)
        | Command::RespawnPane(_)
        | Command::SendKeys(_)
        | Command::ClearHistory(_)
        | Command::CapturePane(_) => pane::run(state, ctx, cmd),

        // A status source a `#(cmd)` registered stops once no option
        // reads it any more (FE.3).
        Command::SetOption(c) => {
            let silence = c.name == "monitor-silence";
            let unattached = c.name == "destroy-unattached";
            let out = options::run(state, ctx, Command::SetOption(c));
            server::sources::prune_unreferenced(state);
            if unattached && out.is_ok() {
                clients::check_unattached(state);
            }
            if silence && out.is_ok() {
                server::alerts::silence_option_set(state);
            }
            out
        }
        Command::ShowOptions(_)
        | Command::DisplayMessage(_)
        | Command::ListSessions(_)
        | Command::ListWindows(_)
        | Command::ListPanes(_) => options::run(state, ctx, cmd),

        Command::KillServer(_) => {
            // Who stopped the server, since nothing else in the log says.
            eprintln!(
                "grofe server: kill-server at {} from {} in {} ({:?})",
                crate::server::snapshot::rfc3339(std::time::SystemTime::now()),
                ctx.pane
                    .map_or("outside any pane".to_string(), |p| format!("pane {p}")),
                ctx.cwd.display(),
                ctx.dialect,
            );
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
        Command::ListClients(c) => clients::list_clients(state, ctx, c),
        Command::NextWindow(c) => window::step_window(state, ctx, c, window::Step::Next),
        Command::PreviousWindow(c) => window::step_window(state, ctx, c, window::Step::Previous),
        Command::LastWindow(c) => window::last_window(state, ctx, c),
        Command::RenameSession(c) => session::rename_session(state, ctx, c),
        Command::RenameWindow(c) => window::rename_window(state, ctx, c),
        Command::SwitchClient(c) => session::switch_client(state, ctx, c),
        Command::SendPrefix(c) => pane::send_prefix(state, ctx, c),

        // M3: bindings and config (T3.5), the prompt and run-shell (T3.4),
        // layout presets (T3.7).
        Command::BindKey(c) => options::bind_key(state, ctx, c),
        Command::UnbindKey(c) => options::unbind_key(state, ctx, c),
        Command::ListKeys(c) => options::list_keys(state, ctx, c),
        Command::SourceFile(c) => {
            let out = options::source_file(state, ctx, c);
            server::sources::prune_unreferenced(state);
            out
        }
        Command::Refused(text) => Err(text),
        Command::ListPlugins(c) => plugins::list_plugins(state, ctx, c),
        Command::PluginRestart(c) => plugins::plugin_restart(state, ctx, c),
        Command::PaneSetResume(c) => persist::pane_set_resume(state, ctx, c),
        Command::RebootCheck(c) => persist::reboot_check(state, ctx, c),
        Command::Reload(c) => {
            let out = options::reload(state, ctx, c);
            server::sources::prune_unreferenced(state);
            out
        }
        Command::CommandPrompt(c) => prompt::command_prompt(state, ctx, c),
        Command::RunShell(c) => prompt::run_shell(state, ctx, c),
        Command::SelectLayout(c) => window::select_layout(state, ctx, c),

        // M3.6: stubs until each owning task lands (the stub edit).
        Command::JoinPane(c) => structure::join_pane(state, ctx, c),
        Command::ClockMode(c) => crate::server::clock::clock_mode(state, ctx, c),
        Command::PipePane(c) => pane::pipe_pane(state, ctx, c),
        Command::SwapPane(c) => structure::swap_pane(state, ctx, c),
        Command::SwapWindow(c) => structure::swap_window(state, ctx, c),
        Command::BreakPane(c) => structure::break_pane(state, ctx, c),
        Command::ResizeWindow(c) => structure::resize_window(state, ctx, c),
        Command::LastPane(c) => structure::last_pane(state, ctx, c),
        Command::RotateWindow(c) => structure::rotate_window(state, ctx, c),
        Command::MoveWindow(c) => structure::move_window(state, ctx, c),
        Command::LinkWindow(c) => structure::link_window(state, ctx, c),
        Command::UnlinkWindow(c) => structure::unlink_window(state, ctx, c),
        Command::MovePane(c) => structure::move_pane(state, ctx, c),
        Command::NextLayout(c) => structure::next_layout(state, ctx, c),
        Command::PreviousLayout(c) => structure::previous_layout(state, ctx, c),
        Command::Lock(c) => lock::lock(state, ctx, c),
        Command::ServerAccess(c) => lock::server_access(state, ctx, c),
        Command::PromptHistory(c) => lock::prompt_history(state, ctx, c),
        Command::RespawnWindow(c) => structure::respawn_window(state, ctx, c),
        Command::RefreshClient(c) => client::refresh_client(state, ctx, c),
        Command::ShowMessages(c) => client::show_messages(state, ctx, c),
        Command::ChooseClient(c) => client::choose_client(state, ctx, c),
        Command::SuspendClient(c) => client::suspend_client(state, ctx, c),
        Command::FindWindow(c) => client::find_window(state, ctx, c),
        Command::ChooseTree(c) => client::choose_tree(state, ctx, c),
        Command::WaitFor(c) => client::wait_for(state, ctx, c),
        Command::DisplayPopup(c) => popup::display_popup(state, ctx, c),
        Command::NewPane(c) => popup::new_pane(state, ctx, c),
        Command::FloatPane(c) => popup::float_pane(state, ctx, c),
        Command::UnfloatPane(c) => popup::unfloat_pane(state, ctx, c),
        Command::DragPane(_) => server::mouse::drag_pane(state),
        Command::PlacePane(c) => popup::place_pane(state, ctx, c),
        Command::NewScratchpad(c) => scratch::new(state, ctx, c),
        Command::ToggleScratchpad(c) => scratch::toggle(state, ctx, c),
        Command::ShowScratchpad(c) => scratch::show(state, ctx, c),
        Command::ListScratchpads => scratch::list(state, ctx),
        Command::ChooseScratchpad => scratch::choose(state, ctx),
        Command::DisplayMenu(c) => menu::display_menu(state, ctx, c),
        Command::DisplayPanes(c) => menu::display_panes(state, ctx, c),
        Command::ConfirmBefore(c) => menu::confirm_before(state, ctx, c),
        Command::If(c) => bindings::if_shell(state, ctx, c),
        Command::SetHook(c) => hooks::set_hook(state, ctx, c),
        Command::ShowHooks(c) => hooks::show_hooks(state, ctx, c),
        Command::SetEnvironment(c) => hooks::set_environment(state, ctx, c),
        Command::ShowEnvironment(c) => hooks::show_environment(state, ctx, c),
        // FE.1: a status source a tmux `#(cmd)` rewrite registers first.
        Command::RegisterSource(c) => {
            server::sources::register(state, c.name.as_deref(), &c.command, None, None);
            Ok(String::new())
        }
        // M5: buffers (TM.4).
        Command::SetBuffer(c) => buffers::set_buffer(state, ctx, c),
        Command::ShowBuffer(c) => buffers::show_buffer(state, ctx, c),
        Command::LoadBuffer(c) => buffers::load_buffer(state, ctx, c),
        Command::SaveBuffer(c) => buffers::save_buffer(state, ctx, c),
        Command::ListBuffers(c) => buffers::list_buffers(state, ctx, c),
        Command::DeleteBuffer(c) => buffers::delete_buffer(state, ctx, c),
        Command::PasteBuffer(c) => buffers::paste_buffer(state, ctx, c),
        Command::ChooseBuffer(c) => buffers::choose_buffer(state, ctx, c),
        Command::CustomizeMode(c) => client::customize_mode(state, ctx, c),
        // M5: copy mode (TM.1).
        Command::CopyMode(c) => copy::copy_mode(state, ctx, c),
    }
}

fn tmux(ctx: &ClientCtx) -> bool {
    ctx.dialect == crate::protocol::Dialect::Tmux
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

/// A new or respawned pane's environment: the server environment
/// (`hooks::pane_env`, TC.5) under the session's `env`, then what the
/// server's identity adds (`compat::pane_env`, TC.1). Later entries win.
pub(crate) fn pane_env(
    state: &State,
    session: SessionId,
    pane: PaneId,
    env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut all = server::hooks::pane_env(state, session);
    all.extend(env);
    all.extend(compat::pane_env(state, session, pane));
    compat::with_tmux_link(&mut all);
    all
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
/// [`clients::cell_px`] for the window's session. The pane's environment
/// is [`pane_env`]'s.
pub(crate) fn spawn_pane(
    state: &mut State,
    // The window's session, passed in: a new window is not in the model yet.
    place: (SessionId, WindowId),
    env: Vec<(String, String)>,
    argv: Option<Vec<String>>,
    cwd: PathBuf,
    size: (u16, u16),
    cell_px: Option<(u16, u16)>,
) -> Result<PaneId, String> {
    let zero = std::time::Duration::ZERO;
    spawn_pane_delayed(state, place, env, argv, cwd, size, cell_px, zero)
}

/// [`spawn_pane`] with the child spawned `delay` after its pty is
/// allocated ([`PaneSpawn::delay`]): the restore stagger (T4.4).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_pane_delayed(
    state: &mut State,
    (session, window): (SessionId, WindowId),
    env: Vec<(String, String)>,
    argv: Option<Vec<String>>,
    cwd: PathBuf,
    (cols, rows): (u16, u16),
    cell_px: Option<(u16, u16)>,
    delay: std::time::Duration,
) -> Result<PaneId, String> {
    let id = state.model.alloc_pane_id();
    let cwd = usable_cwd(cwd);
    let mut env = pane_env(state, session, id, env);
    // As tmux's `spawn_pane`: no command means `default-command`, and the
    // pane's shell is `default-shell` (else `/bin/sh`), exported as `SHELL`.
    let argv = argv
        .filter(|a| !a.is_empty())
        .or_else(|| state.model.default_command(session).map(|c| vec![c]));
    let shell = pane_shell(state.model.default_shell(session).as_deref());
    env.push(("SHELL".to_string(), shell.clone()));
    let spec = PaneSpawn {
        id,
        session,
        argv: argv.clone(),
        shell: shell.clone(),
        login: true,
        cwd: cwd.clone(),
        env,
        cols,
        rows,
        history_limit: state.model.history_limit(session),
        socket: state.socket.clone(),
        term: state.pane_term(),
        cell_px,
        graphics_ok: graphics::panes_graphics_ok(state),
        images: graphics::images_enabled(&state.model),
        identity: state.model.identity,
        delay,
    };
    let handle = PaneHandle::spawn(spec, state.pane_events.clone(), state.wake.clone())
        .map_err(|e| format!("can't start pane in {}: {e}", cwd.display()))?;
    clients::seed_defaults(&state.model, (session, window), &handle);
    state.model.panes.insert(
        id,
        Pane {
            id,
            uid: Ulid::generate().to_string(),
            window,
            resume: argv.clone(),
            resume_binding: None,
            expect: None,
            restored: false,
            argv,
            shell,
            cwd,
            dead: None,
            dead_time: None,
            cols,
            rows,
            input_off: false,
            handle,
        },
    );
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn only_listed_commands_are_read_only() {
        let (state, ctx) = (crate::server::test_state(), crate::server::test_ctx());
        let read_only = |argv: &[String]| read_only(&parse(&state, &ctx, argv));
        for words in [
            &["list-sessions"][..],
            &["lsw"],
            &["show-options", "-g"],
            &["has-session", "-t", "main"],
            &["display-message", "-p", "#{window_name}"],
            &["capture-pane", "-p"],
            &[ATTACH_CHECK, "", "-t", "main"],
        ] {
            assert!(read_only(&argv(words)), "{words:?}");
        }
        for words in [
            &["display-message", "hello"][..],
            &["rename-window", "x"],
            &["set", "-g", "status", "off"],
            &["send-keys", "a"],
            &["no-such-command"],
            &[],
        ] {
            assert!(!read_only(&argv(words)), "{words:?}");
        }
    }
}
