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

pub mod clients;
pub mod dispatch;
pub mod events;
pub mod input;
pub mod layout;
pub mod model;
pub mod mouse;
pub mod pane;
pub mod passthrough;
pub mod prompt;
pub mod render;
pub mod status;

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
use tokio_util::codec::{Framed, LinesCodec};

use crate::cmd::{self, Command};
use crate::event::Event;
use crate::paths::{self, Env};
use crate::protocol::{self, ClientCtx, Hello, Reply, Request};
use pane::{PaneEvent, PaneEventKind};

/// Logged once per server process, first thing after the log is open, as
/// `grofe server starting: pid <pid>, socket <path>`. The test harness counts
/// these lines to prove one server per socket.
pub const START_MARKER: &str = "grofe server starting";

/// A message into the server actor.
#[derive(Debug)]
pub enum ServerMsg {
    /// One control-mode request; the actor answers on `reply`.
    Control {
        ctx: ClientCtx,
        req: Request,
        reply: oneshot::Sender<Reply>,
    },
    /// A `subscribe` request, with the connection's event queue, which the
    /// actor keeps as the subscriber.
    Subscribe {
        req: Request,
        reply: oneshot::Sender<Reply>,
        tx: mpsc::Sender<Event>,
    },
    Pane(PaneEvent),
    /// A client connection ended.
    Closed,
    // M2: client frames.
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
    /// Set by a command (`kill-server`, the last session closing): the
    /// actor removes the socket, sends the reply, and stops.
    pub exit: bool,
    /// Started implicitly by a session-creating command (`__server
    /// --exit-when-empty`): exits once it has no sessions and no client
    /// connections, even if it never had a session. `start-server` leaves
    /// it off, so that server stays up empty.
    exit_when_empty: bool,
    /// Open client connections, counted by the actor as it accepts them.
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
            let uid = p.uid.clone();
            state.events.emit(Event::PaneExited { pane, uid, status });
            state.model.pane_exited(pane, status);
            if state.model.closed() {
                state.exit = true;
            }
        }
        PaneEventKind::Output(bytes) => state.events.emit(Event::pane_output(pane, &bytes)),
        _ => {}
    }
    state.events.flush();
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
    let state = State {
        socket,
        model: model::Model::new(options.remain_on_exit),
        inner_term: pane::inner_term(),
        options,
        pane_events: pane_tx,
        wake: Arc::new(Notify::new()),
        events: events::Events::default(),
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
    signal_ready(&mut ready, "ok");

    // The actor owns the listener (dropped when it returns: no more
    // accepts) and `conns`, the last `alive` sender outside connections.
    let state = actor(state, listener, conns, msg_rx, pane_rx, signals).await;
    state.remove_socket();
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
            connection(stream, msgs.clone(), shutdown, alive).await;
            let _ = msgs.send(ServerMsg::Closed).await;
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
    (mut sigterm, mut sigint): (tokio::signal::unix::Signal, tokio::signal::unix::Signal),
) -> State {
    // Set after an accept error: accepting pauses, commands and pane events
    // keep flowing.
    let mut accept_paused_until: Option<Instant> = None;
    loop {
        let idle =
            state.exit_when_empty && state.connections == 0 && state.model.sessions.is_empty();
        tokio::select! {
            msg = msgs.recv() => match msg {
                Some(msg) => handle(&mut state, msg),
                None => break,
            },
            Some(event) = pane_events.recv() => handle(&mut state, ServerMsg::Pane(event)),
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
            _ = sigterm.recv() => {
                eprintln!("grofe server: SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                eprintln!("grofe server: SIGINT");
                break;
            }
        }
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
    if !state.exit_when_empty
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
        ServerMsg::Control { ctx, req, reply } => {
            let result = dispatch(state, &ctx, &req.argv);
            finish(state, req.id, result, reply);
        }
        ServerMsg::Subscribe { req, reply, tx } => {
            let result = match cmd::parse(&req.argv) {
                Ok(Command::Subscribe(c)) => {
                    state.events.subscribe(c.events, tx).map(|()| String::new())
                }
                // The connection sends only a `subscribe` verb here.
                Ok(_) => Err("not a subscribe command".to_string()),
                Err(e) => Err(e.to_string()),
            };
            finish(state, req.id, result, reply);
        }
        ServerMsg::Pane(event) => handle_pane_event(state, event),
        ServerMsg::Closed => state.connections = state.connections.saturating_sub(1),
    }
}

/// Sends a command's reply, then the events it caused, so a connection
/// writes its reply before those events.
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
    let reply_msg = match result {
        Ok(out) => Reply::Ok { id, out },
        Err(err) => Reply::Err { id, err },
    };
    let _ = reply.send(reply_msg);
    state.events.flush();
}

/// One client connection. `_alive` is held until the task ends, so shutdown
/// can wait for in-flight replies to be written.
async fn connection(
    stream: UnixStream,
    msgs: mpsc::Sender<ServerMsg>,
    mut shutdown: watch::Receiver<bool>,
    _alive: mpsc::Sender<()>,
) {
    let mut lines = Framed::new(stream, protocol::lines_codec());
    let hello = tokio::select! {
        line = lines.next() => line,
        _ = shutdown.changed() => return,
    };
    let Some(Ok(hello)) = hello else { return };
    let ctx = match protocol::decode_hello(&hello) {
        Ok(Hello::Control(ctx)) => ctx,
        Ok(Hello::Attach { .. }) => {
            let _ = send(&mut lines, &error_reply(0, "attach is not supported yet")).await;
            return;
        }
        Err(e) => {
            let _ = send(&mut lines, &error_reply(0, &e.to_string())).await;
            return;
        }
    };

    // This connection's event queue; the actor holds a sender once it
    // subscribes.
    let (events_tx, mut events_rx) = mpsc::channel(events::QUEUE);
    loop {
        let line = tokio::select! {
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
                req,
                reply: reply_tx,
                tx: events_tx.clone(),
            }
        } else {
            ServerMsg::Control {
                ctx: ctx.clone(),
                req,
                reply: reply_tx,
            }
        };
        if msgs.send(msg).await.is_err() {
            break;
        }
        let Ok(reply) = reply_rx.await else { break };
        if send(&mut lines, &reply).await.is_err() {
            break;
        }
    }
}

fn error_reply(id: u64, err: &str) -> Reply {
    Reply::Err {
        id,
        err: err.to_string(),
    }
}

async fn send(lines: &mut Framed<UnixStream, LinesCodec>, reply: &Reply) -> anyhow::Result<()> {
    lines.send(serde_json::to_string(reply)?).await?;
    Ok(())
}
