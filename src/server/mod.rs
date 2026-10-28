//! Actor, lifecycle, accept, hook registration.
//!
//! Process start (spec §4.1): the CLI re-execs itself as `grofe __server -S
//! <sock> --ready-fd <n>`, detached. [`run_daemon`] is that process's whole
//! life: it closes every inherited fd above 2 (except the readiness pipe),
//! puts `/dev/null` on stdin and the log on stdout/stderr, and only then
//! builds the tokio runtime, binds, and writes `ok` to the readiness pipe.
//!
//! One actor task owns all server state ([`State`]); connection tasks turn
//! control-mode requests into [`ServerMsg::Control`] and write the reply
//! back. Commands run through the single [`dispatch`] seam.

pub mod alerts;
pub mod buffers;
pub mod clients;
pub mod clock;
pub mod copy;
pub mod dispatch;
pub mod events;
pub mod graphics;
pub mod hooks;
pub mod imgstore;
pub mod input;
pub mod layout;
pub mod menu;
pub mod model;
pub mod mouse;
pub mod pane;
pub mod passthrough;
pub mod plugins;
pub mod popup;
pub mod prompt;
pub mod render;
pub mod screenimg;
pub mod sixel;
pub mod sources;
pub mod status;
pub mod tree;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::{SinkExt, StreamExt};
use nix::fcntl::{Flock, FlockArg};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::codec::{Framed, FramedParts, FramedWrite, LinesCodec};

use crate::cmd::{self, Command};
use crate::event::Event;
use crate::paths::{self, Env};
use crate::protocol::{self, ClientCtx, Frame, FrameCodec, HELLO_REPLY_ID, Hello, Reply, Request};
use crate::server::input::InputHook;
use crate::server::render::Overlay;
use crate::types::ClientId;
use pane::{PaneEvent, PaneEventKind};

/// Logged once per server process, first thing after the log is open, as
/// `grofe server starting: pid <pid>, socket <path>`. The test harness counts
/// these lines to prove one server per socket.
pub const START_MARKER: &str = "grofe server starting";

/// What a CLI request brings besides its argv (TM.7).
#[derive(Debug, Clone, Default)]
pub(crate) struct CliIo {
    /// Its stdin, decoded; `None` when it sent none.
    pub stdin: Option<Vec<u8>>,
    pub umask: Option<u32>,
}

impl CliIo {
    fn of(req: &Request) -> CliIo {
        use base64::Engine as _;
        CliIo {
            stdin: req
                .stdin
                .as_deref()
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()),
            umask: req.umask,
        }
    }
}

/// A message into the server actor.
#[derive(Debug)]
pub enum ServerMsg {
    /// One control-mode request; the actor answers on `reply`.
    Control {
        ctx: ClientCtx,
        req: Request,
        reply: oneshot::Sender<Reply>,
        /// The requesting process, for the message log's `client-<pid>`.
        pid: Option<i32>,
    },
    /// A `subscribe` request, with the connection's event queue, which the
    /// actor keeps as the subscriber.
    Subscribe {
        ctx: ClientCtx,
        req: Request,
        reply: oneshot::Sender<Reply>,
        tx: mpsc::Sender<Event>,
    },
    Pane(PaneEvent),
    /// A client connection ended. `observer`: it had already stopped
    /// counting (see [`ServerMsg::Observer`]).
    Closed {
        observer: bool,
    },
    /// A connection turned out to be a plugin's own `grofe control`: it
    /// stops counting as an open connection, since the server supervises it
    /// and it must not keep an empty auto-started server up (tmux's server
    /// exits when no client is attached to a session). Any other
    /// connection, a subscriber included, still counts.
    Observer,
    /// An attach hello; the actor accepts it (the connection then sends
    /// the ok line and switches to frames) or refuses it.
    Attach {
        req: clients::AttachRequest,
        reply: oneshot::Sender<Result<clients::Accepted, String>>,
    },
    /// A frame from an attached client.
    ClientFrame {
        client: ClientId,
        frame: Frame,
    },
    /// An attached client's connection ended.
    ClientGone(ClientId),
}

/// Settings read from the server's environment at start. They stand in for
/// config until it exists, and the test harness sets both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartOptions {
    /// `GROFE_RESTORE_ON_START=0` turns it off. Read now, used from M4, so
    /// a test server can never restore the user's real sessions.
    pub restore_on_start: bool,
    /// `GROFE_REMAIN_ON_EXIT=1` turns it on: the initial global value of
    /// the `remain-on-exit` option (spec default off; tests run with it on).
    pub remain_on_exit: bool,
}

impl StartOptions {
    pub const RESTORE_ON_START_ENV: &str = "GROFE_RESTORE_ON_START";
    pub const REMAIN_ON_EXIT_ENV: &str = "GROFE_REMAIN_ON_EXIT";

    pub fn from_env() -> StartOptions {
        let var = |name| std::env::var(name).ok();
        StartOptions {
            restore_on_start: var(Self::RESTORE_ON_START_ENV).as_deref() != Some("0"),
            remain_on_exit: var(Self::REMAIN_ON_EXIT_ENV).as_deref() == Some("1"),
        }
    }
}

/// Work for the actor, sent from a task off it (a status source's result,
/// TC.4; a popup's pty, TC.6; a shell condition's answer, TC.10; a notify
/// hook deferred past its command's reply, TC.5). It runs between messages
/// with the whole state, then the events it caused are flushed; what it
/// changed on screen it marks itself (`clients::mark_dirty`,
/// `clients::changed`).
pub type Job = Box<dyn FnOnce(&mut State) + Send>;

/// How many [`Job`]s may wait for the actor. A task off the actor that
/// finds the queue full waits (`send().await`, or `blocking_send` on a
/// plain thread); code on the actor uses `try_send` and handles `Full`
/// itself, since the actor waiting on its own queue would never wake.
pub const JOB_QUEUE: usize = 256;

/// Everything the actor owns.
pub struct State {
    /// This server's socket; panes get it as `GROFE`.
    pub socket: PathBuf,
    pub options: StartOptions,
    /// Sessions, windows and panes.
    pub model: model::Model,
    /// The `TERM` panes get, resolved once at start.
    pub inner_term: &'static str,
    /// Handed to `PaneHandle::spawn`: pane events reach the actor through it.
    pub pane_events: mpsc::Sender<PaneEvent>,
    /// Handed to `PaneHandle::spawn`: pane output wakes the renderer (M2).
    pub wake: Arc<Notify>,
    /// Subscribers and the events commands recorded for them.
    pub events: events::Events,
    /// Attached clients.
    pub clients: BTreeMap<ClientId, clients::ClientEntry>,
    /// Key tables, input hooks, overlays, window-size owners and the render
    /// schedule: client state that is not per client.
    pub registry: clients::Registry,
    /// Open prompts and the prompt history (T3.4).
    pub prompts: prompt::Prompts,
    /// Scrollback views (T3.2).
    pub mouse: mouse::MouseState,
    /// Passthrough state (T3.3).
    pub passthrough: passthrough::Passthrough,
    /// Kitty graphics: id remap and image cache (T3.8).
    pub graphics: graphics::Graphics,
    /// Status sources (TC.4).
    pub sources: sources::Sources,
    /// Supervised plugins (T6.3).
    pub plugins: plugins::Plugins,
    /// Event hooks and the server environment (TC.5).
    pub hooks: hooks::Hooks,
    /// Open popups (TC.6).
    pub popups: popup::Popups,
    /// Open menus and the aliases (TC.7).
    pub menus: menu::Menus,
    /// Open choosers (TC.9).
    pub trees: tree::Trees,
    /// The paste buffers (TM.4).
    pub buffers: buffers::Buffers,
    /// The panes in copy mode (TM.1).
    pub copy: copy::Copies,
    /// The panes showing the clock (`clock-mode`).
    pub clocks: clock::Clocks,
    /// Each window's silence wait (`alerts`).
    pub alerts: alerts::Alerts,
    /// Sends a [`Job`] to the actor, bounded by [`JOB_QUEUE`] (see there
    /// for who may wait); any task may hold a clone. The actor keeps this
    /// one, so the channel never closes while it runs.
    pub jobs: mpsc::Sender<Job>,
    /// Set by a handler that answers later ([`dispatch::defer_reply`]);
    /// taken by whoever ran the command.
    pub deferred_reply: Option<oneshot::Receiver<Result<String, protocol::Failure>>>,
    /// The rest of the request now running, held behind a wait
    /// ([`dispatch::client::Held`]).
    pub(crate) held: Option<dispatch::client::Held>,
    /// What a request printed before the command that failed it: the
    /// reply carries it next to the error, as tmux prints both (FE.2).
    pub(crate) failed_out: String,
    /// The running CLI request's stdin and umask, for `load-buffer -` and
    /// the files `save-buffer` creates (TM.7); `None` outside one.
    pub(crate) cli_io: Option<CliIo>,
    /// The log lines of the request now running, logged as its commands
    /// fire (`clients::log_fired`).
    pub(crate) command_log: std::collections::VecDeque<(usize, Option<String>)>,
    /// Set by a command (`kill-server`, the last session closing): the
    /// actor removes the socket, sends the reply, and stops.
    pub exit: bool,
    /// Started implicitly by a session-creating command (`__server
    /// --exit-when-empty`): exits once it has no sessions and no client
    /// connections, even if it never had a session. `start-server` leaves
    /// it off, so that server stays up empty.
    exit_when_empty: bool,
    /// Open client connections, counted by the actor as it accepts them.
    /// A plugin's connection leaves the count, see [`ServerMsg::Observer`].
    connections: usize,
    /// (dev, ino) of the socket we bound, so we only ever remove our own.
    socket_id: Option<(u64, u64)>,
}

impl State {
    fn remove_socket(&self) {
        let Ok(meta) = fs::symlink_metadata(&self.socket) else {
            return;
        };
        if Some((meta.dev(), meta.ino())) == self.socket_id {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

/// The dispatch seam: runs one command (`argv[0]` is the verb) against the
/// actor's state, returning the command's stdout or its error message.
///
/// The work happens in [`dispatch::run`].
pub fn dispatch(state: &mut State, ctx: &ClientCtx, argv: &[String]) -> Result<String, String> {
    dispatch::run(state, ctx, argv)
}

/// Adds an overlay drawn over every client's frame. Overlays draw in
/// reverse registration order, so one registered earlier ends on top.
pub fn register_overlay(state: &mut State, overlay: Arc<dyn Overlay + Send + Sync>) {
    state.registry.overlays.push(overlay);
}

/// Adds a hook that sees every client's input before the key tables,
/// after those registered before it.
pub fn register_input_hook(state: &mut State, hook: Arc<dyn InputHook + Send + Sync>) {
    state.registry.input_hooks.push(hook);
}

/// The server's own overlays and input hooks, every M3 (T3.0) and M3.6
/// (stub edit) one, in priority order, highest first. The rule: input
/// hooks run in registration order and the first to answer `Some` wins;
/// overlays draw in reverse order, so the highest-priority one ends on top.
/// So an open prompt sees keys first and draws over everything; a menu
/// (TC.7) comes next, then a popup (TC.6), so a prompt or menu a popup's
/// command opens takes the keys and draws on top of it; then a chooser
/// (TC.9) over its pane; then the mouse, and copy mode (TM.1).
fn register_builtins(state: &mut State) {
    register_input_hook(state, Arc::new(prompt::PromptHook));
    register_overlay(state, Arc::new(prompt::PromptOverlay));
    register_input_hook(state, Arc::new(menu::MenuHook));
    register_overlay(state, Arc::new(menu::MenuOverlay));
    register_input_hook(state, Arc::new(popup::PopupHook));
    register_overlay(state, Arc::new(popup::PopupOverlay));
    register_input_hook(state, Arc::new(tree::TreeHook));
    register_overlay(state, Arc::new(tree::TreeOverlay));
    register_input_hook(state, Arc::new(mouse::MouseHook));
    clock::register(state);
    copy::register(state);
}

/// Set to `1` to make the server panic on its first command: the test
/// trigger for the panic report (T3.6, R22).
pub const TEST_PANIC_ENV: &str = "GROFE_TEST_SERVER_PANIC";

/// Handles one pane event. Events from a process `respawn-pane` already
/// replaced (an older generation) or from a pane that is gone are ignored.
/// A dead pane is reported (`pane-exited`, with its status) and removed
/// unless `remain-on-exit` keeps it, and the removal cascades (spec §4.2).
/// Output read while the tap is on becomes `pane-output`.
fn handle_pane_event(state: &mut State, event: PaneEvent) {
    let PaneEvent {
        pane,
        generation,
        kind,
    } = event;
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    if p.handle.generation() != generation {
        return;
    }
    match kind {
        PaneEventKind::Exited(status) => {
            let (uid, window) = (p.uid.clone(), p.window);
            // The pane may be gone from the model by the time this is sent.
            let place = state.model.place_of(p);
            let exited = Event::PaneExited { pane, uid, status };
            state.events.emit_at(exited, place);
            state.model.pane_exited(pane, status);
            hooks::pane_exited(state, pane, window);
            if state.model.closed() {
                state.exit = true;
            }
            clients::changed(state);
        }
        PaneEventKind::Output(bytes) => state.events.emit(Event::pane_output(pane, &bytes)),
        kind @ (PaneEventKind::Title(_)
        | PaneEventKind::Bell
        | PaneEventKind::Clipboard { .. }
        | PaneEventKind::Path(_)
        | PaneEventKind::Osc(_)) => passthrough::on_pane_event(state, pane, kind),
        PaneEventKind::Graphics(bytes) => graphics::on_graphics(state, pane, bytes),
        // The reader handles sixel images; they never reach the actor.
        PaneEventKind::Sixel { .. } => {}
    }
    hooks::run_pending(state);
    state.events.flush(&state.model);
}

/// Runs one [`Job`] on the actor, then sends the events it caused.
fn run_job(state: &mut State, job: Job) {
    job(state);
    // A job is no CLI request: nothing waits for its hooks.
    drop(hooks::take_reply_hold(state));
    state.events.flush(&state.model);
}

/// Entry point of `grofe __server`: runs the server until it exits and
/// returns the process exit code. Must run before any thread or runtime
/// exists (it closes inherited fds wholesale).
///
/// `exit_when_empty`: see [`State`]; the CLI's auto-start path sets it.
pub fn run_daemon(socket: &Path, ready_fd: Option<RawFd>, exit_when_empty: bool) -> i32 {
    close_inherited_fds(ready_fd);
    // SAFETY: the CLI passed us this fd as the write end of the readiness
    // pipe, and close_inherited_fds kept it open; nothing else owns it.
    let mut ready = ready_fd.map(|fd| unsafe { File::from_raw_fd(fd) });

    let log = paths::log_path(&Env::from_process(), socket);
    if let Err(e) = redirect_stdio(&log) {
        signal_ready(
            &mut ready,
            &format!("cannot open log {}: {e}", log.display()),
        );
        return 1;
    }
    eprintln!(
        "{START_MARKER}: pid {}, socket {}",
        std::process::id(),
        socket.display()
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("grofe server: cannot build runtime: {e}");
            signal_ready(&mut ready, &format!("cannot build runtime: {e}"));
            return 1;
        }
    };
    let options = StartOptions::from_env();
    match runtime.block_on(serve(socket.to_path_buf(), ready, options, exit_when_empty)) {
        Ok(()) => {
            eprintln!("grofe server exiting");
            0
        }
        Err(e) => {
            eprintln!("grofe server error: {e:#}");
            1
        }
    }
}

/// Closes every fd above 2 except `keep`: a leaked pipe from whoever
/// started the CLI would otherwise keep its reader waiting for an EOF that
/// never comes (spec §4.1).
fn close_inherited_fds(keep: Option<RawFd>) {
    let fds: Vec<RawFd> = match fs::read_dir("/dev/fd") {
        Ok(dir) => dir
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
        // ponytail: without /dev/fd, only fds below 1024 are closed.
        Err(_) => (3..1024).collect(),
    };
    for fd in fds {
        if fd > 2 && Some(fd) != keep {
            // EBADF (e.g. the read_dir handle, already closed) is fine.
            let _ = nix::unistd::close(fd);
        }
    }
}

/// `/dev/null` on fd 0 and the log on fds 1 and 2, so every std stream is a
/// valid fd before any pane spawns: a pty slave landing on 0-2 would keep
/// its CLOEXEC-less number and be closed at exec.
fn redirect_stdio(log: &Path) -> io::Result<()> {
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)?;
    nix::unistd::dup2_stdin(&null)?;
    nix::unistd::dup2_stdout(&log)?;
    nix::unistd::dup2_stderr(&log)?;
    // If either opened onto 0-2 (because that slot was closed), it now is
    // one of the std streams: keep it open rather than dropping it.
    for file in [null, log] {
        let fd = file.into_raw_fd();
        if fd > 2 {
            let _ = nix::unistd::close(fd);
        }
    }
    Ok(())
}

/// Tells the waiting CLI how start-up went (`ok` or an error), then closes
/// the pipe. A no-op after the first call.
fn signal_ready(ready: &mut Option<File>, msg: &str) {
    if let Some(mut f) = ready.take() {
        let _ = writeln!(f, "{msg}");
    }
}

async fn serve(
    socket: PathBuf,
    mut ready: Option<File>,
    options: StartOptions,
    exit_when_empty: bool,
) -> anyhow::Result<()> {
    let signals = (
        signal(SignalKind::terminate()).context("installing SIGTERM handler")?,
        signal(SignalKind::interrupt()).context("installing SIGINT handler")?,
        // A handler, not SIG_IGN, which panes would inherit across exec.
        signal(SignalKind::hangup()).context("installing SIGHUP handler")?,
    );
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            signal_ready(
                &mut ready,
                &format!("cannot bind {}: {e}", socket.display()),
            );
            return Err(e).context(format!("binding {}", socket.display()));
        }
    };
    let socket_id = fs::symlink_metadata(&socket)
        .ok()
        .map(|m| (m.dev(), m.ino()));
    eprintln!("grofe server listening on {}", socket.display());

    let (pane_tx, pane_rx) = mpsc::channel(256);
    let (jobs_tx, jobs_rx) = mpsc::channel(JOB_QUEUE);
    let mut state = State {
        socket,
        model: model::Model::new(options.remain_on_exit),
        inner_term: pane::inner_term(),
        options,
        pane_events: pane_tx,
        wake: Arc::new(Notify::new()),
        events: events::Events::default(),
        clients: BTreeMap::new(),
        registry: clients::Registry::default(),
        prompts: prompt::Prompts::default(),
        mouse: mouse::MouseState::default(),
        passthrough: passthrough::Passthrough::default(),
        graphics: graphics::Graphics::default(),
        sources: sources::Sources::default(),
        plugins: plugins::Plugins::default(),
        hooks: hooks::Hooks::for_server(),
        popups: popup::Popups::default(),
        menus: menu::Menus::default(),
        trees: tree::Trees::default(),
        buffers: buffers::Buffers::default(),
        copy: copy::Copies::default(),
        clocks: clock::Clocks::default(),
        alerts: alerts::Alerts::default(),
        jobs: jobs_tx,
        deferred_reply: None,
        held: None,
        failed_out: String::new(),
        cli_io: None,
        command_log: Default::default(),
        exit: false,
        exit_when_empty,
        connections: 0,
        socket_id,
    };

    let (msg_tx, msg_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (alive_tx, mut alive_rx) = mpsc::channel::<()>(1);
    let conns = Connections {
        msgs: msg_tx,
        shutdown: shutdown_rx,
        alive: alive_tx,
    };
    register_builtins(&mut state);
    dispatch::options::apply_config_at_start(&mut state);
    crate::compat::server_started(&mut state);
    plugins::start(&mut state);
    signal_ready(&mut ready, "ok");

    // The actor owns the listener (dropped when it returns: no more
    // accepts) and `conns`, the last `alive` sender outside connections.
    let mut state = actor(state, listener, conns, msg_rx, pane_rx, jobs_rx, signals).await;
    state.remove_socket();
    clients::shutdown(&mut state, "server exited");
    plugins::shutdown(&mut state).await;
    // Hang up the panes and let go of the subscribers' queues before the
    // connections drain them.
    drop(state);

    // Let connections finish writing in-flight replies (the `kill-server`
    // reply among them), then return.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), alive_rx.recv()).await;
    Ok(())
}

/// What a connection task needs, handed out by the actor as it accepts.
struct Connections {
    msgs: mpsc::Sender<ServerMsg>,
    shutdown: watch::Receiver<bool>,
    alive: mpsc::Sender<()>,
}

impl Connections {
    /// Runs one connection, then tells the actor it closed.
    fn spawn(&self, stream: UnixStream) {
        let msgs = self.msgs.clone();
        let (shutdown, alive) = (self.shutdown.clone(), self.alive.clone());
        tokio::spawn(async move {
            let observer = connection(stream, msgs.clone(), shutdown, alive).await;
            let _ = msgs.send(ServerMsg::Closed { observer }).await;
        });
    }
}

/// How often an empty auto-started server with no connections re-checks
/// whether to exit: its starter may never connect (it timed out, was
/// interrupted, or died), so no connection close would trigger the check.
const EMPTY_RECHECK: Duration = Duration::from_secs(2);

/// How long accepting pauses after an accept error (e.g. EMFILE).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Accepting happens here, not in a task of its own, so the connection
/// count and the listener's backlog are consistent whenever
/// [`exit_if_empty`] looks at them.
async fn actor(
    mut state: State,
    listener: UnixListener,
    conns: Connections,
    mut msgs: mpsc::Receiver<ServerMsg>,
    mut pane_events: mpsc::Receiver<PaneEvent>,
    mut jobs: mpsc::Receiver<Job>,
    (mut sigterm, mut sigint, mut sighup): (
        tokio::signal::unix::Signal,
        tokio::signal::unix::Signal,
        tokio::signal::unix::Signal,
    ),
) -> State {
    // Set after an accept error: accepting pauses, commands and pane events
    // keep flowing.
    let mut accept_paused_until: Option<Instant> = None;
    let wake = Arc::clone(&state.wake);
    loop {
        let idle =
            state.exit_when_empty && state.connections == 0 && state.model.sessions.is_empty();
        let due = clients::deadline(&state).map(Instant::from_std);
        tokio::select! {
            msg = msgs.recv() => match msg {
                Some(msg) => handle(&mut state, msg),
                None => break,
            },
            Some(event) = pane_events.recv() => handle(&mut state, ServerMsg::Pane(event)),
            Some(job) = jobs.recv() => run_job(&mut state, job),
            accepted = listener.accept(), if accept_paused_until.is_none() => match accepted {
                Ok((stream, _)) => {
                    state.connections += 1;
                    conns.spawn(stream);
                }
                Err(e) => {
                    // e.g. EMFILE: back off instead of spinning.
                    eprintln!("grofe server: accept: {e}");
                    accept_paused_until = Some(Instant::now() + ACCEPT_BACKOFF);
                }
            },
            _ = sleep_until(accept_paused_until.unwrap_or_else(Instant::now)),
                if accept_paused_until.is_some() => accept_paused_until = None,
            // Falls through to the exit_if_empty below.
            _ = sleep(EMPTY_RECHECK), if idle => {}
            // Pane output: redraw the clients showing it.
            _ = wake.notified() => clients::wake(&mut state),
            _ = sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                clients::tick(&mut state, std::time::Instant::now());
            }
            _ = sigterm.recv() => {
                eprintln!("grofe server: SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                eprintln!("grofe server: SIGINT");
                break;
            }
            // As tmux's server: SIGHUP (its terminal going away) is ignored.
            _ = sighup.recv() => eprintln!("grofe server: SIGHUP ignored"),
        }
        events::track_focus(&mut state);
        state.events.flush(&state.model);
        exit_if_empty(&mut state, &listener);
        if state.exit {
            break;
        }
    }
    state
}

/// For an auto-started server: exit once there are no sessions and no
/// client connections, after the first request completes (success or not).
///
/// A sibling CLI may be connecting right now, so this must not strand it:
/// - it takes the start lock (without waiting); a CLI holding it is about
///   to connect, and that connection's close checks again;
/// - it moves the socket aside, so no new connection can arrive, then looks
///   at the listener's backlog: one already queued means stay, and the
///   socket goes back;
/// - with the lock held no other server can bind the path meanwhile, and a
///   CLI that found no socket waits on the lock, then connects or starts
///   a fresh server.
///
/// While the socket is aside, a plain `connect` (a non-starting command
/// such as `has-session`) gets ENOENT even if the server then stays up; the
/// CLI retries such a connect once after a short wait to cover this window.
fn exit_if_empty(state: &mut State, listener: &UnixListener) {
    // `set -s exit-empty off` keeps the server up with no sessions.
    let keep = state.model.options.lookup("exit-empty", &[]) == Some("off");
    if !state.exit_when_empty
        || keep
        || state.exit
        || state.connections > 0
        || !state.model.sessions.is_empty()
    {
        return;
    }
    let Ok(lock_file) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(paths::lock_path(&state.socket))
    else {
        return;
    };
    let Ok(_lock) = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) else {
        return;
    };
    let ours = fs::symlink_metadata(&state.socket)
        .is_ok_and(|m| Some((m.dev(), m.ino())) == state.socket_id);
    if !ours {
        // Gone or replaced: nobody can reach us through it any more.
        state.exit = true;
        return;
    }
    let mut aside = state.socket.clone().into_os_string();
    aside.push(".exiting");
    let aside = PathBuf::from(aside);
    if fs::rename(&state.socket, &aside).is_err() {
        return;
    }
    let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
    if poll(&mut fds, PollTimeout::ZERO).is_ok_and(|n| n > 0) {
        if fs::rename(&aside, &state.socket).is_ok() {
            return;
        }
        eprintln!("grofe server: cannot restore {}", state.socket.display());
    }
    let _ = fs::remove_file(&aside);
    state.exit = true;
}

fn handle(state: &mut State, msg: ServerMsg) {
    match msg {
        ServerMsg::Control {
            ctx,
            req,
            reply,
            pid,
        } => {
            if std::env::var_os(TEST_PANIC_ENV).is_some_and(|v| v == "1") {
                panic!("{TEST_PANIC_ENV}=1: test panic on a command");
            }
            // Parsed once: classified, then run.
            state.failed_out.clear();
            state.cli_io = Some(CliIo::of(&req));
            let parsed = dispatch::parse(state, &ctx, &req.argv);
            let read_only = dispatch::read_only(&parsed);
            // Logged as tmux 3.7b logs a client's commands and errors
            // (`show-messages`).
            let who = pid.map_or_else(|| "client".to_string(), |p| format!("client-{p}"));
            state.command_log = match &parsed {
                Ok(dispatch::Parsed::Commands(cmds)) => {
                    clients::plan_command_log(state, &who, &ctx, &req.argv, cmds.len())
                }
                _ => Default::default(),
            };
            let result = dispatch::run_parsed(state, &ctx, parsed);
            state.cli_io = None;
            state.command_log.clear();
            if let Err(e) = &result {
                clients::log_error(state, &who, e);
            }
            finish(state, req.id, result, reply);
            if !read_only {
                clients::changed(state);
            }
        }
        ServerMsg::Subscribe {
            ctx,
            req,
            reply,
            tx,
        } => {
            let result = match cmd::parse(&req.argv) {
                Ok(Command::Subscribe(c)) => c
                    .target
                    .map(|t| events::Scope::resolve(&state.model, &ctx, &t))
                    .transpose()
                    .and_then(|scope| state.events.subscribe(c.events, scope, tx))
                    .map(|()| String::new()),
                // The connection sends only a `subscribe` verb here.
                Ok(_) => Err("not a subscribe command".to_string()),
                Err(e) => Err(e.to_string()),
            };
            state.deferred_reply = None;
            finish(state, req.id, result, reply);
        }
        ServerMsg::Pane(event) => handle_pane_event(state, event),
        ServerMsg::Closed { observer } => {
            if !observer {
                state.connections = state.connections.saturating_sub(1);
            }
        }
        ServerMsg::Observer => state.connections = state.connections.saturating_sub(1),
        ServerMsg::Attach { req, reply } => {
            let _ = reply.send(clients::attach(state, req));
        }
        ServerMsg::ClientFrame { client, frame } => clients::frame(state, client, frame),
        ServerMsg::ClientGone(client) => clients::gone(state, client),
    }
}

/// Sends a command's reply, then the events it caused, so a connection
/// writes its reply before those events. A command that succeeded after
/// deferring its answer ([`dispatch::defer_reply`]) is answered when its
/// handler sends it, without the actor waiting; one that failed is
/// answered with its error now, the deferred answer dropped.
fn finish(
    state: &mut State,
    id: u64,
    result: Result<String, String>,
    reply: oneshot::Sender<Reply>,
) {
    if state.exit {
        // Before replying, so a CLI that saw the reply finds no socket.
        state.remove_socket();
    }
    // Waits for the request's `after-*` hooks, as tmux 3.7b's CLI does.
    let hold = hooks::take_reply_hold(state);
    match (state.deferred_reply.take(), result, hold) {
        (Some(later), Ok(_), hold) => reply_later(id, later, reply, hold),
        (_, Ok(out), None) => {
            let _ = reply.send(Reply::Ok { id, out });
        }
        (_, Err(err), None) => {
            let failure = protocol::Failure {
                err,
                out: std::mem::take(&mut state.failed_out),
                status: None,
            };
            let _ = reply.send(Reply::failed(id, failure));
        }
        (_, result, Some(hold)) => {
            let (tx, later) = oneshot::channel();
            let out = std::mem::take(&mut state.failed_out);
            let _ = tx.send(result.map_err(|err| protocol::Failure {
                err,
                out,
                status: None,
            }));
            reply_later(id, later, reply, Some(hold));
        }
    }
    state.events.flush(&state.model);
}

/// Sends a deferred reply once its handler answers, without the actor
/// waiting; a handler that drops its sender answers with an error.
fn reply_later(
    id: u64,
    later: oneshot::Receiver<Result<String, protocol::Failure>>,
    reply: oneshot::Sender<Reply>,
    hold: Option<oneshot::Receiver<String>>,
) {
    tokio::spawn(async move {
        let mut reply = reply;
        let answer = async {
            let answer = later.await;
            // The rest of what the request's hooks printed follows its output.
            let more = match hold {
                Some(hold) => hold.await.unwrap_or_default(),
                None => String::new(),
            };
            match answer {
                Ok(Ok(out)) => Reply::Ok {
                    id,
                    out: out + &more,
                },
                Ok(Err(failure)) => Reply::failed(id, failure),
                Err(_) => error_reply(id, "command ended without an answer"),
            }
        };
        // A caller that hung up (a killed `wait-for`) drops the pending
        // answer, so its handler sees the sender closed.
        let reply_msg = tokio::select! {
            msg = answer => msg,
            () = reply.closed() => return,
        };
        let _ = reply.send(reply_msg);
    });
}

/// How long a new connection may take to send its hello before it is
/// closed. Clients send it at once (attach after at most its probe), so
/// this only bounds connections that never will.
const HELLO_DEADLINE: Duration = Duration::from_secs(5);

/// One client connection. `_alive` is held until the task ends, so shutdown
/// can wait for in-flight replies to be written. Returns whether the
/// connection became an observer ([`ServerMsg::Observer`]).
async fn connection(
    stream: UnixStream,
    msgs: mpsc::Sender<ServerMsg>,
    shutdown: watch::Receiver<bool>,
    alive: mpsc::Sender<()>,
) -> bool {
    let mut observer = false;
    connection_loop(stream, &msgs, shutdown, alive, &mut observer).await;
    observer
}

async fn connection_loop(
    stream: UnixStream,
    msgs: &mpsc::Sender<ServerMsg>,
    mut shutdown: watch::Receiver<bool>,
    _alive: mpsc::Sender<()>,
    observer: &mut bool,
) {
    let codec = LinesCodec::new_with_max_length(protocol::HELLO_MAX_LEN);
    let mut lines = Framed::new(stream, codec);
    let hello = tokio::select! {
        line = tokio::time::timeout(HELLO_DEADLINE, lines.next()) => line,
        _ = shutdown.changed() => return,
    };
    let Ok(Some(Ok(hello))) = hello else { return };
    // Past the hello, requests may be as large as any message.
    *lines.codec_mut() = protocol::lines_codec();
    let ctx = match protocol::decode_hello(&hello) {
        Ok(Hello::Control(ctx)) => {
            // A plugin's own `grofe control` (its env carries GROFE_PLUGIN,
            // which a plugin's `env` cannot override) lives as long as the
            // plugin does.
            if ctx.env.iter().any(|(k, _)| k == "GROFE_PLUGIN") {
                *observer = true;
                if msgs.send(ServerMsg::Observer).await.is_err() {
                    return;
                }
            }
            ctx
        }
        Ok(Hello::Attach {
            ctx,
            cols,
            rows,
            term,
            caps,
            target,
            detach_others,
            nest,
            tty,
        }) => {
            let req = clients::AttachRequest {
                pid: lines.get_ref().peer_cred().ok().and_then(|c| c.pid()),
                ctx,
                size: (cols, rows),
                term,
                caps,
                target,
                detach_others,
                nest,
                tty,
            };
            attach(lines, req, msgs.clone()).await;
            return;
        }
        Err(e) => {
            refuse(lines, &e.to_string()).await;
            return;
        }
    };

    // This connection's event queue; the actor holds a sender once it
    // subscribes.
    let (events_tx, mut events_rx) = mpsc::channel(events::QUEUE);
    // A request that arrived while an earlier one waited for its answer.
    let mut next: Option<String> = None;
    loop {
        let line = if let Some(line) = next.take() {
            Some(Ok(line))
        } else {
            tokio::select! {
            biased;
            _ = shutdown.changed() => {
                // Events the actor queued before it stopped (a
                // `kill-server`'s `pane-exited`) still go out.
                while let Ok(event) = events_rx.try_recv() {
                    if send(&mut lines, &Reply::Event(event)).await.is_err() {
                        break;
                    }
                }
                break;
            }
            Some(event) = events_rx.recv() => {
                if send(&mut lines, &Reply::Event(event)).await.is_err() {
                    break;
                }
                continue;
            }
            line = lines.next() => line,
            }
        };
        let Some(Ok(line)) = line else { break };
        let req: Request = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                let reply = error_reply(0, &format!("malformed request: {e}"));
                if send(&mut lines, &reply).await.is_err() {
                    break;
                }
                continue;
            }
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = if req.argv.first().is_some_and(|verb| verb == "subscribe") {
            ServerMsg::Subscribe {
                ctx: ctx.clone(),
                req,
                reply: reply_tx,
                tx: events_tx.clone(),
            }
        } else {
            ServerMsg::Control {
                ctx: ctx.clone(),
                req,
                reply: reply_tx,
                pid: lines.get_ref().peer_cred().ok().and_then(|c| c.pid()),
            }
        };
        if msgs.send(msg).await.is_err() {
            break;
        }
        // The socket is read while the answer is pending, so a caller that
        // hangs up during a long wait (`wait-for`) ends the connection.
        let mut reply_rx = reply_rx;
        let reply = loop {
            tokio::select! {
                reply = &mut reply_rx => break reply,
                line = lines.next(), if next.is_none() => match line {
                    Some(Ok(line)) => next = Some(line),
                    _ => return,
                },
            }
        };
        let Ok(reply) = reply else { break };
        if send(&mut lines, &reply).await.is_err() {
            break;
        }
    }
}

/// An attach connection: the actor accepts or refuses the hello; on
/// accept the ok line goes out and the connection switches to binary
/// frames, keeping any bytes already read past the hello (typeahead).
///
/// The stream is split so a client that stops reading can block only its
/// own writes: input and `detach` keep flowing to the actor, and the actor
/// only ever leaves one pending frame for the writer ([`clients::Outbox`]).
async fn attach(
    mut lines: Framed<UnixStream, LinesCodec>,
    req: clients::AttachRequest,
    msgs: mpsc::Sender<ServerMsg>,
) {
    let (reply_tx, reply_rx) = oneshot::channel();
    let msg = ServerMsg::Attach {
        req,
        reply: reply_tx,
    };
    if msgs.send(msg).await.is_err() {
        return;
    }
    let accepted = match reply_rx.await {
        Ok(Ok(accepted)) => accepted,
        Ok(Err(e)) => return refuse(lines, &e).await,
        Err(_) => return,
    };
    let id = accepted.id;
    let ok = Reply::Ok {
        id: HELLO_REPLY_ID,
        out: String::new(),
    };
    if send(&mut lines, &ok).await.is_err() {
        let _ = msgs.send(ServerMsg::ClientGone(id)).await;
        return;
    }
    let parts = lines.into_parts();
    let (read, write) = parts.io.into_split();
    // `from_parts` marks a non-empty buffer readable, so frames that came in
    // with the hello are decoded at once. (`FramedRead::new` with the bytes
    // pushed into its buffer would wait for the next read first.)
    let mut read_parts = FramedParts::new::<Frame>(read, FrameCodec);
    read_parts.read_buf = parts.read_buf;
    let mut frames = Framed::from_parts(read_parts);
    let reader = async {
        while let Some(Ok(frame)) = frames.next().await {
            let msg = ServerMsg::ClientFrame { client: id, frame };
            if msgs.send(msg).await.is_err() {
                return;
            }
        }
    };
    let writer = write_frames(FramedWrite::new(write, FrameCodec), accepted);
    tokio::select! {
        () = writer => {}
        () = reader => {}
    }
    // Whichever side ended (the client hung up, a write failed, or the
    // exit frame went out), the actor forgets the client; after a detach
    // it already has, and this is a no-op.
    let _ = msgs.send(ServerMsg::ClientGone(id)).await;
}

/// Writes an attached client's frames, one at a time as the actor leaves
/// them, then the `exit` frame once the actor let go of the client.
async fn write_frames(
    mut out: FramedWrite<tokio::net::unix::OwnedWriteHalf, FrameCodec>,
    mut accepted: clients::Accepted,
) {
    loop {
        // Output first, so a suspend goes out after what was drawn before it.
        let bytes = tokio::select! {
            biased;
            bytes = accepted.output.recv() => match bytes {
                Some(bytes) => bytes,
                None => break,
            },
            Some(()) = accepted.suspend.recv() => {
                if out.send(Frame::Suspend).await.is_err() {
                    return;
                }
                continue;
            }
        };
        // A redraw of a huge client can pass the frame cap: it goes out as
        // consecutive frames, which the client just writes in order.
        for chunk in bytes.chunks(protocol::MAX_MESSAGE_LEN) {
            if out.send(Frame::Output(chunk.to_vec())).await.is_err() {
                return;
            }
        }
    }
    let reason = (&mut accepted.exit)
        .await
        .unwrap_or_else(|_| "server exited".to_string());
    let _ = out.send(Frame::Exit(reason)).await;
}

/// Sends a hello's error line; the connection then closes.
async fn refuse(mut lines: Framed<UnixStream, LinesCodec>, err: &str) {
    let _ = send(&mut lines, &error_reply(HELLO_REPLY_ID, err)).await;
}

fn error_reply(id: u64, err: &str) -> Reply {
    Reply::err(id, err)
}

async fn send(lines: &mut Framed<UnixStream, LinesCodec>, reply: &Reply) -> anyhow::Result<()> {
    lines.send(serde_json::to_string(reply)?).await?;
    Ok(())
}

/// A server state with nothing in it, as the actor starts, on a socket
/// path nothing listens on, for unit tests.
#[cfg(test)]
pub(crate) fn test_state() -> State {
    test_state_at(PathBuf::from("/nonexistent/grofe-test.sock")).0
}

/// A request context from the CLI, run in `/`, for unit tests.
#[cfg(test)]
pub(crate) fn test_ctx() -> ClientCtx {
    ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: protocol::Dialect::Grofe,
    }
}

/// [`test_state`] on `socket`, with the receiving end of its job channel.
#[cfg(test)]
fn test_state_at(socket: PathBuf) -> (State, mpsc::Receiver<Job>) {
    let (pane_events, _) = mpsc::channel(1);
    let (jobs, jobs_rx) = mpsc::channel(JOB_QUEUE);
    let socket_id = fs::symlink_metadata(&socket)
        .ok()
        .map(|m| (m.dev(), m.ino()));
    let state = State {
        socket,
        options: StartOptions {
            restore_on_start: false,
            remain_on_exit: false,
        },
        model: model::Model::new(false),
        inner_term: pane::inner_term(),
        pane_events,
        wake: Arc::new(Notify::new()),
        events: events::Events::default(),
        clients: BTreeMap::new(),
        registry: clients::Registry::default(),
        prompts: prompt::Prompts::default(),
        mouse: mouse::MouseState::default(),
        passthrough: passthrough::Passthrough::default(),
        graphics: graphics::Graphics::default(),
        sources: sources::Sources::default(),
        plugins: plugins::Plugins::default(),
        hooks: hooks::Hooks::default(),
        popups: popup::Popups::default(),
        menus: menu::Menus::default(),
        trees: tree::Trees::default(),
        buffers: buffers::Buffers::default(),
        copy: copy::Copies::default(),
        clocks: clock::Clocks::default(),
        alerts: alerts::Alerts::default(),
        jobs,
        deferred_reply: None,
        held: None,
        failed_out: String::new(),
        cli_io: None,
        command_log: Default::default(),
        exit: false,
        exit_when_empty: false,
        connections: 0,
        socket_id,
    };
    (state, jobs_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_at(socket: PathBuf) -> State {
        test_state_at(socket).0
    }

    /// The stub edit's job channel: a job sent from another thread runs
    /// on the actor with the whole state.
    #[test]
    fn a_job_from_another_thread_runs_with_the_state() {
        let (mut state, mut jobs) = test_state_at(PathBuf::from("/nonexistent/grofe-job.sock"));
        let tx = state.jobs.clone();
        std::thread::spawn(move || {
            let job: Job = Box::new(|state: &mut State| state.exit = true);
            tx.blocking_send(job).map_err(|_| "closed").expect("send");
        })
        .join()
        .expect("sender thread");
        let job = jobs.try_recv().expect("a job");
        assert!(!state.exit);
        run_job(&mut state, job);
        assert!(state.exit);
    }

    /// Fix round 1: the queue is bounded. On the actor, `try_send` on a
    /// full queue answers `Full` at once instead of waiting; off it, a
    /// sender waits until the actor has run a job, then goes through.
    #[tokio::test]
    async fn a_full_job_queue_refuses_on_the_actor_and_waits_off_it() {
        use tokio::sync::mpsc::error::TrySendError;
        let (mut state, mut jobs) = test_state_at(PathBuf::from("/nonexistent/grofe-full.sock"));
        let noop = || -> Job { Box::new(|_: &mut State| {}) };
        for _ in 0..JOB_QUEUE {
            assert!(state.jobs.try_send(noop()).is_ok());
        }
        assert!(matches!(
            state.jobs.try_send(noop()),
            Err(TrySendError::Full(_))
        ));
        let tx = state.jobs.clone();
        let waiting = tokio::spawn(async move {
            let job: Job = Box::new(|state: &mut State| state.exit = true);
            tx.send(job).await.map_err(|_| "closed")
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "sent into a full queue");
        let job = jobs.recv().await.expect("a job");
        run_job(&mut state, job);
        waiting
            .await
            .expect("task")
            .expect("sent once there was room");
        for _ in 0..JOB_QUEUE {
            let job = jobs.recv().await.expect("a job");
            run_job(&mut state, job);
        }
        assert!(state.exit, "the waiting job ran last");
    }

    /// Fix round 1: a handler that deferred its answer and then failed is
    /// answered with its own error, at once.
    #[tokio::test]
    async fn a_handler_that_defers_then_fails_answers_with_its_error() {
        let mut state = state_at(PathBuf::from("/nonexistent/grofe-t30.sock"));
        let answer = dispatch::defer_reply(&mut state);
        let (reply, mut reply_rx) = oneshot::channel();
        finish(&mut state, 3, Err("spawn failed: boom".to_string()), reply);
        let got = reply_rx.try_recv().expect("answered now");
        assert!(
            matches!(got, Reply::Err { id: 3, ref err, .. } if err == "spawn failed: boom"),
            "{got:?}"
        );
        assert!(state.deferred_reply.is_none());
        assert!(answer.is_closed(), "the deferred answer was dropped");
    }

    /// Fix round 1: a deferred answer still goes through `finish`, so a
    /// command that ends the server removes the socket before any reply.
    #[tokio::test]
    async fn a_deferred_answer_still_removes_the_socket_on_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("s.sock");
        fs::write(&socket, b"").expect("stand-in socket");
        let mut state = state_at(socket.clone());
        state.exit = true;
        let answer = dispatch::defer_reply(&mut state);
        let (reply, reply_rx) = oneshot::channel();
        finish(&mut state, 4, Ok(String::new()), reply);
        assert!(!socket.exists(), "socket removed before the reply");
        let _ = answer.send(Ok("later\n".to_string()));
        let got = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("in time")
            .expect("a reply");
        assert!(
            matches!(got, Reply::Ok { id: 4, ref out } if out == "later\n"),
            "{got:?}"
        );
    }

    /// Item 8: a deferred answer reaches the request's reply once the
    /// handler sends it; the actor never waits for it.
    #[tokio::test]
    async fn a_deferred_reply_is_sent_when_the_handler_answers() {
        let (answer, later) = oneshot::channel();
        let (reply, mut reply_rx) = oneshot::channel();
        reply_later(7, later, reply, None);
        tokio::task::yield_now().await;
        assert!(reply_rx.try_recv().is_err(), "nothing before the answer");
        let _ = answer.send(Ok("hi\n".to_string()));
        let got = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("in time")
            .expect("a reply");
        assert!(
            matches!(got, Reply::Ok { id: 7, ref out } if out == "hi\n"),
            "{got:?}"
        );

        // A handler that gives up still answers the request.
        let (answer, later) = oneshot::channel::<Result<String, protocol::Failure>>();
        let (reply, reply_rx) = oneshot::channel();
        reply_later(8, later, reply, None);
        drop(answer);
        let got = reply_rx.await.expect("a reply");
        assert!(matches!(got, Reply::Err { id: 8, .. }), "{got:?}");
    }
}
