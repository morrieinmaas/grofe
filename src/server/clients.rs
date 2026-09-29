//! Attached clients: the registry, per-client output with one pending
//! frame, the render scheduler, the window-size policy, attach and detach
//! (spec §4.4, §4.6, D6).
//!
//! Everything here runs on the actor. A client's connection task (in
//! `server/mod.rs`) owns the socket: it sends the client's frames to the
//! actor and writes what the actor leaves in the client's one-slot outbox.
//!
//! - **Output.** [`Outbox`] holds at most one pending frame. The actor only
//!   `try_send`s into it: when the slot is still full the new diff is
//!   dropped, the client's previous screen is forgotten, and the next frame
//!   it gets is a full redraw (spec §4.4, slow clients). A client that stops
//!   reading therefore never stalls the actor or another client.
//! - **Scheduling.** Pane output sets the pane's dirty flag and wakes the
//!   actor; [`wake`] marks the clients showing a dirty pane, commands mark
//!   every client, and [`tick`] composes dirty clients at most once per
//!   [`FRAME_INTERVAL`].
//! - **Size (D6).** Each session's windows take the size of its latest
//!   active client ([`SizeOwner`]): attaching makes a client the latest at
//!   once, input makes it the latest after [`SIZE_DEBOUNCE`], so two clients
//!   typing in turn do not make the window flap. A larger client
//!   letterboxes, a smaller one crops with a viewport that follows the
//!   active pane's cursor ([`follow`]).
//! - **Input.** Bytes are decoded per client, each input goes to the
//!   registered input hooks first, then through the client's key table;
//!   bound commands run with the client's context, other keys go to the
//!   active pane encoded for its modes.
//! - **Raw output.** [`queue_raw`] puts bytes that are not a screen change
//!   (mouse modes, passthrough, graphics) ahead of the client's next frame,
//!   in the same `Output` frame.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use tokio::sync::{mpsc, oneshot};

use crate::cmd::{self, AttachSession, DetachClient, FormatVars, ListClients, Resolve, Target};
use crate::protocol::{Caps, ClientCtx, Dialect, Frame};
use crate::server::dispatch::options::{self, FormatCtx, Owner};
use crate::server::dispatch::window::automatic_rename;
use crate::server::input::{
    Decoder, Input, InputHook, KeySettings, KeyState, KeyTables, ROOT_TABLE, Routed,
    default_bindings, parse_prefix,
};
use crate::server::model::{Model, Session};
use crate::server::pane::CursorState;
use crate::server::render::{self, FrameInputs, Overlay, OverlayCtx, Screen};
use crate::server::status::{self, Span, StatusInputs, StyledLine, WindowStatus};
use crate::server::{State, dispatch, graphics};
use crate::types::{ClientId, PaneId, Rect, SessionId, cap_size};

/// At most one frame per client per this long (spec §4.4: ~16 ms).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How long a status-line message stays up (tmux's `display-time`).
// ponytail: `display-time` and `message-style` are fixed at tmux's
// defaults until they are options.
pub const DISPLAY_TIME: Duration = Duration::from_millis(750);
const MESSAGE_STYLE: &str = "bg=yellow,fg=black";

/// How often window names are checked for `automatic-rename` while clients
/// are attached (tmux's `NAME_INTERVAL`).
pub const NAME_INTERVAL: Duration = Duration::from_millis(500);

/// How long a client must stay the latest active one before the window
/// takes its size (spec D6: ~500 ms).
pub const SIZE_DEBOUNCE: Duration = Duration::from_millis(500);

/// What an attach hello asks for, as the connection hands it to the actor.
#[derive(Debug)]
pub struct AttachRequest {
    pub ctx: ClientCtx,
    /// The client terminal, `(cols, rows)`.
    pub size: (u16, u16),
    pub term: String,
    pub caps: Caps,
    /// `attach -t`, raw.
    pub target: Option<String>,
    pub detach_others: bool,
    pub nest: bool,
}

/// An accepted attach: the connection writes `output` (one frame at a
/// time) and, once the actor lets go of the sender, the reason from `exit`.
#[derive(Debug)]
pub struct Accepted {
    pub id: ClientId,
    pub output: mpsc::Receiver<Vec<u8>>,
    /// Dropped without a value when the server goes away mid-write.
    pub exit: oneshot::Receiver<String>,
}

/// A client's one-slot output: the actor offers each new screen, and the
/// diff goes out only if the slot is free.
pub struct Outbox {
    tx: mpsc::Sender<Vec<u8>>,
    /// The screen the client shows once everything sent so far is written;
    /// `None` forces a full redraw.
    prev: Option<Screen>,
    /// Set when a diff was dropped: nothing is composed for this client
    /// until its slot is free again.
    blocked: bool,
    /// Bytes for ahead of the next frame ([`queue_raw`]); kept across a
    /// dropped frame.
    raw: Vec<u8>,
}

/// Most raw bytes held for a client whose frames are being dropped.
// ponytail: past this, newer raw bytes are dropped until the client reads
// again; a full redraw re-sends what must survive (graphics, T3.8).
const RAW_CAP: usize = 16 * 1024 * 1024;

impl Outbox {
    /// An outbox and the receiver its connection writes from.
    pub fn new() -> (Outbox, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(1);
        let outbox = Outbox {
            tx,
            prev: None,
            blocked: false,
            raw: Vec::new(),
        };
        (outbox, rx)
    }

    /// Whether offering a frame now is worth composing one.
    pub fn ready(&self) -> bool {
        !self.blocked || self.tx.capacity() > 0
    }

    /// Whether the next offer is a full redraw.
    pub fn needs_full_redraw(&self) -> bool {
        self.prev.is_none()
    }

    /// Holds `bytes` for ahead of the next frame.
    pub fn queue_raw(&mut self, bytes: &[u8]) {
        if self.raw.len() + bytes.len() <= RAW_CAP {
            self.raw.extend_from_slice(bytes);
        }
    }

    /// Sends the queued raw bytes, then the diff that turns the client's
    /// screen into `next`, as ONE frame (so a synchronized-output begin/end
    /// pair is never split, and raw bytes land before it). If the slot is
    /// still full the frame is dropped, the raw bytes stay queued, and the
    /// next offer is a full redraw. Returns false for a dropped frame: the
    /// client still needs one, and must be offered it again once
    /// [`Outbox::ready`].
    pub fn offer(&mut self, next: Screen, caps: &Caps) -> bool {
        let diff = render::diff(self.prev.as_ref(), &next, caps);
        if diff.is_empty() && self.raw.is_empty() {
            return true;
        }
        let mut bytes = std::mem::take(&mut self.raw);
        let raw_len = bytes.len();
        bytes.extend_from_slice(&diff);
        match self.tx.try_send(bytes) {
            Ok(()) => {
                self.prev = Some(next);
                self.blocked = false;
                true
            }
            Err(mpsc::error::TrySendError::Full(mut bytes)) => {
                bytes.truncate(raw_len);
                self.raw = bytes;
                self.prev = None;
                self.blocked = true;
                false
            }
            // The connection is gone; its `ClientGone` removes the client.
            Err(mpsc::error::TrySendError::Closed(_)) => true,
        }
    }
}

/// One attached client, keyed by `ClientId` in `State::clients`.
pub struct ClientEntry {
    /// `client_name`, what `detach-client -t` takes.
    pub name: String,
    pub session: SessionId,
    /// The attaching CLI's context, with `client` set to this client.
    pub ctx: ClientCtx,
    /// The client terminal, `(cols, rows)`.
    pub size: (u16, u16),
    /// The latest Resize frame's size, applied on the next tick so a
    /// burst of resizes costs one reflow.
    pending_size: Option<(u16, u16)>,
    /// `TERM` of the client's terminal.
    pub term: String,
    pub caps: Caps,
    /// Last input (or the attach itself).
    pub activity: Instant,
    /// Wall-clock last input or attach (`client_activity`; unlike
    /// `activity`, a switch does not count, as in tmux).
    pub activity_time: SystemTime,
    pub decoder: Decoder,
    pub keys: KeyState,
    /// Whether the outer terminal has focus, from its focus reports (the
    /// client asks for them); a client starts focused, as in tmux.
    pub focused: bool,
    /// A message shown instead of the status line until it expires or a
    /// key is pressed (a failed binding's error, as tmux shows it).
    message: Option<(String, Instant)>,
    /// The window cell drawn at the client's top-left (crop, D6), as last
    /// drawn.
    pub viewport: (u16, u16),
    /// The outer terminal's mouse modes as last written (R2); `None`
    /// before the first write.
    mouse_modes: Option<bool>,
    /// Needs a new frame.
    dirty: bool,
    outbox: Outbox,
    exit: Option<oneshot::Sender<String>>,
}

/// Which client a session's windows are sized for (spec D6), and the
/// client about to take over once its debounce runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeOwner {
    pub client: ClientId,
    pending: Option<(ClientId, Instant)>,
}

impl SizeOwner {
    pub fn new(client: ClientId) -> SizeOwner {
        SizeOwner {
            client,
            pending: None,
        }
    }

    /// `client` sent input at `now`. The owner typing again cancels a
    /// pending switch; another client becomes the owner [`SIZE_DEBOUNCE`]
    /// after its first input unless the owner types meanwhile.
    pub fn activity(&mut self, client: ClientId, now: Instant) {
        if client == self.client {
            self.pending = None;
        } else if self.pending.is_none_or(|(c, _)| c != client) {
            self.pending = Some((client, now + SIZE_DEBOUNCE));
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at)
    }

    /// Completes a due switch; whether the owner changed.
    pub fn settle(&mut self, now: Instant) -> bool {
        match self.pending {
            Some((client, at)) if now >= at => {
                self.client = client;
                self.pending = None;
                true
            }
            _ => false,
        }
    }
}

/// Client state that is not per client.
pub struct Registry {
    next_id: u32,
    /// Bindings by table, starting with the default key table.
    pub key_tables: KeyTables,
    /// See every input before the key tables, in registration order.
    pub input_hooks: Vec<Arc<dyn InputHook + Send + Sync>>,
    /// Drawn over every client's frame, in reverse registration order.
    pub overlays: Vec<Arc<dyn Overlay + Send + Sync>>,
    owners: BTreeMap<SessionId, SizeOwner>,
    render_at: Option<Instant>,
    last_render: Option<Instant>,
    /// When the status line is next redrawn for its clock.
    status_at: Option<Instant>,
    /// When window names are next checked (`automatic-rename`).
    names_at: Option<Instant>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            next_id: 1,
            key_tables: default_bindings(),
            input_hooks: Vec::new(),
            overlays: Vec::new(),
            owners: BTreeMap::new(),
            render_at: None,
            last_render: None,
            status_at: None,
            names_at: None,
        }
    }
}

/// `client_name` for a client.
// ponytail: tmux names a client by its tty; the attach hello carries no
// tty path, so grofe numbers them.
pub fn client_name(id: ClientId) -> String {
    format!("client-{}", id.0)
}

/// Accepts an attach hello: resolves the target session (a window or pane
/// target also selects it), refuses self-nesting without `--nest`, and
/// registers the client as the session's latest (spec D6: attaching is
/// activity).
pub fn attach(state: &mut State, req: AttachRequest) -> Result<Accepted, String> {
    let now = Instant::now();
    let target = match &req.target {
        Some(raw) => cmd::target::parse_target(raw).map_err(|e| e.to_string())?,
        None => Target::Current,
    };
    check_target(&state.model, &target, &req.ctx, req.nest)?;
    let session = select_target(&mut state.model, &target, &req.ctx)?;
    if req.detach_others {
        detach_session(state, session, None);
    }

    let id = ClientId(state.registry.next_id);
    state.registry.next_id += 1;
    let (outbox, output) = Outbox::new();
    let (exit_tx, exit) = oneshot::channel();
    let ctx = ClientCtx {
        client: Some(id),
        ..req.ctx
    };
    state.clients.insert(
        id,
        ClientEntry {
            name: client_name(id),
            session,
            ctx,
            size: clamp_size(req.size),
            pending_size: None,
            term: req.term,
            caps: req.caps,
            activity: now,
            activity_time: SystemTime::now(),
            decoder: Decoder::new(),
            keys: KeyState::new(),
            focused: true,
            message: None,
            viewport: (0, 0),
            mouse_modes: None,
            dirty: true,
            outbox,
            exit: Some(exit_tx),
        },
    );
    state.registry.owners.insert(session, SizeOwner::new(id));
    touch_session(&mut state.model, session);
    sync_attached(state);
    changed(state);
    Ok(Accepted { id, output, exit })
}

/// Everything attach refuses before it changes anything, in tmux's order:
/// no sessions, then the target (session, window, pane), then self-nesting
/// without `--nest`.
fn check_target(
    model: &Model,
    target: &Target,
    ctx: &ClientCtx,
    nest: bool,
) -> Result<SessionId, String> {
    if model.sessions.is_empty() {
        return Err("no sessions".to_string());
    }
    let session = model.resolve_session(target, ctx)?;
    if let Target::Window(..) | Target::Pane(_) = target {
        model.resolve_window(target, ctx)?;
    }
    if let Target::Pane(_) = target {
        model.resolve_pane(target, ctx)?;
    }
    if !nest
        && let Some(pane) = ctx.pane
        && let Some(p) = model.panes.get(&pane)
        && model.windows.get(&p.window).map(|w| w.session) == Some(session)
    {
        if ctx.dialect == crate::protocol::Dialect::Tmux {
            return Err(crate::compat::TMUX_NESTED.to_string());
        }
        return Err(format!(
            "refusing to attach session {} from inside itself (use --nest to force)",
            model.sessions[&session].name
        ));
    }
    Ok(session)
}

/// The attach check (`protocol::ATTACH_CHECK`): `args` are
/// `attach-session`'s, checked as the attach hello will be, changing
/// nothing.
pub fn attach_check(state: &State, ctx: &ClientCtx, args: &[String]) -> Result<String, String> {
    let argv: Vec<String> = std::iter::once("attach-session".to_string())
        .chain(args.iter().cloned())
        .collect();
    match cmd::parse(&argv).map_err(|e| e.to_string())? {
        cmd::Command::AttachSession(c) => {
            check_target(&state.model, &c.target, ctx, c.nest).map(|_| String::new())
        }
        _ => Err("attach-session: unexpected command".to_string()),
    }
}

/// Resolves an attach or switch target to its session, making a window or
/// pane target that session's current window (and pane), as tmux does.
pub(crate) fn select_target(
    model: &mut Model,
    target: &Target,
    ctx: &ClientCtx,
) -> Result<SessionId, String> {
    let session = model.resolve_session(target, ctx)?;
    if let Target::Window(..) | Target::Pane(_) = target {
        let window = model.resolve_window(target, ctx)?;
        if let Target::Pane(_) = target {
            let pane = model.resolve_pane(target, ctx)?;
            if let Some(w) = model.windows.get_mut(&window) {
                w.set_active(pane);
            }
        }
        model.select_window(window);
    }
    Ok(session)
}

/// Client `id` now shows `session` (attach-session and switch-client):
/// the session it leaves becomes its last session, and it takes the new
/// session's size at once (switching is activity, spec D6).
pub(crate) fn show_session(state: &mut State, id: ClientId, session: SessionId) {
    let Some(entry) = state.clients.get_mut(&id) else {
        return;
    };
    let old = entry.session;
    entry.activity = Instant::now();
    if old != session {
        entry.session = session;
        entry.viewport = (0, 0);
        state.model.client_last_session.insert(id, old);
        leave_session(state, id, old);
    }
    state.registry.owners.insert(session, SizeOwner::new(id));
    touch_session(&mut state.model, session);
    sync_attached(state);
}

/// Session activity (tmux's `session_update_activity`): what makes it the
/// session a command without a pane or target runs against.
fn touch_session(model: &mut Model, session: SessionId) {
    if let Some(s) = model.sessions.get_mut(&session) {
        s.activity = Instant::now();
    }
}

/// Recounts `session_attached` for every session.
fn sync_attached(state: &mut State) {
    for s in state.model.sessions.values_mut() {
        s.attached = 0;
    }
    for c in state.clients.values() {
        if let Some(s) = state.model.sessions.get_mut(&c.session) {
            s.attached += 1;
        }
    }
}

/// `switch-client -T`: client `id`'s key table.
pub(crate) fn set_key_table(state: &mut State, id: ClientId, table: &str) {
    if let Some(c) = state.clients.get_mut(&id) {
        c.keys.set_table(table);
        c.dirty = true;
    }
}

/// Shows `text` on client `id`'s status row for [`DISPLAY_TIME`].
pub fn show_message(state: &mut State, id: ClientId, text: &str) {
    let now = Instant::now();
    if let Some(c) = state.clients.get_mut(&id) {
        c.message = Some((text.to_string(), now + DISPLAY_TIME));
        c.dirty = true;
        request_render(state, now);
    }
}

/// A client size within tmux's bounds and grofe's cell cap
/// ([`crate::types::cap_size`]).
fn clamp_size((cols, rows): (u16, u16)) -> (u16, u16) {
    cap_size(cols, rows)
}

/// A frame from client `id`.
pub fn frame(state: &mut State, id: ClientId, frame: Frame) {
    match frame {
        Frame::Input(bytes) => input(state, id, &bytes, Instant::now()),
        Frame::Resize(cols, rows) => {
            let Some(c) = state.clients.get_mut(&id) else {
                return;
            };
            c.pending_size = Some(clamp_size((cols, rows)));
            request_render(state, Instant::now());
        }
        Frame::Detach => detach(state, id),
        // Server-to-client kinds: ignored if a client sends them.
        Frame::Output(_) | Frame::Exit(_) => {}
    }
}

/// Client `id`'s connection ended.
pub fn gone(state: &mut State, id: ClientId) {
    remove(state, id, None);
}

/// Detaches client `id` with tmux's message.
pub fn detach(state: &mut State, id: ClientId) {
    let reason = state
        .clients
        .get(&id)
        .map(|c| detached_from(&state.model, c.session));
    remove(state, id, reason);
}

fn detached_from(model: &Model, session: SessionId) -> String {
    match model.sessions.get(&session) {
        Some(s) => format!("detached (from session {})", s.name),
        None => "detached".to_string(),
    }
}

/// Detaches every client on `session` except `keep`.
fn detach_session(state: &mut State, session: SessionId, keep: Option<ClientId>) {
    let ids: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(id, c)| c.session == session && Some(**id) != keep)
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        detach(state, id);
    }
}

/// Removes a client; with a reason, it is told why (its connection writes
/// what is still pending, then the `exit` frame).
fn remove(state: &mut State, id: ClientId, reason: Option<String>) {
    crate::server::passthrough::on_client_gone(state, id);
    let Some(mut c) = state.clients.remove(&id) else {
        return;
    };
    crate::server::prompt::client_gone(state, id);
    crate::server::menu::client_gone(state, id);
    crate::server::popup::client_gone(state, id);
    crate::server::tree::client_gone(state, id);
    if let (Some(tx), Some(reason)) = (c.exit.take(), reason) {
        let _ = tx.send(reason);
    }
    state.model.client_last_session.remove(&id);
    sync_attached(state);
    leave_session(state, id, c.session);
    graphics::on_clients_changed(state);
}

/// `id` no longer shows `session`: if it owned the size, the session's
/// most recently active remaining client takes over at once.
fn leave_session(state: &mut State, id: ClientId, session: SessionId) {
    let Some(owner) = state.registry.owners.get_mut(&session) else {
        return;
    };
    if owner.pending.is_some_and(|(c, _)| c == id) {
        owner.pending = None;
    }
    if owner.client != id {
        return;
    }
    let next = state
        .clients
        .iter()
        .filter(|(cid, c)| c.session == session && **cid != id)
        .max_by_key(|(_, c)| c.activity)
        .map(|(cid, _)| *cid);
    match next {
        Some(next) => *owner = SizeOwner::new(next),
        None => {
            state.registry.owners.remove(&session);
        }
    }
    changed(state);
}

/// Tells every client why it is going away and forgets them (server exit).
pub fn shutdown(state: &mut State, reason: &str) {
    for (_, mut c) in std::mem::take(&mut state.clients) {
        if let Some(tx) = c.exit.take() {
            let _ = tx.send(reason.to_string());
        }
    }
}

/// After anything that may have changed what clients show (a command, a
/// pane exiting): clients of a session that is gone leave (as tmux's
/// `[exited]`), window sizes follow their owners, and every client redraws.
pub fn changed(state: &mut State) {
    let orphans: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(_, c)| !state.model.sessions.contains_key(&c.session))
        .map(|(id, _)| *id)
        .collect();
    for id in orphans {
        remove(state, id, Some("exited".to_string()));
    }
    let live = &state.model.sessions;
    state.registry.owners.retain(|s, _| live.contains_key(s));
    apply_sizes(state);
    graphics::on_clients_changed(state);
    sync_mouse_modes(state);
    mark_all_dirty(state);
}

/// The outer terminal's mouse reporting on and off.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";

/// Writes each client's mouse modes where its session's `mouse` option
/// differs from what it was last sent (R2): so on attach, and after any
/// `set`, `reload` or switch that changes it. The client's exit reset
/// turns them off again.
fn sync_mouse_modes(state: &mut State) {
    let model = &state.model;
    for c in state.clients.values_mut() {
        let on = model
            .options
            .lookup("mouse", &[Owner::Session(c.session)])
            .is_some_and(|v| v == "on");
        if c.mouse_modes != Some(on) {
            c.mouse_modes = Some(on);
            c.outbox.queue_raw(if on { MOUSE_ON } else { MOUSE_OFF });
        }
    }
}

/// Queues `bytes` for client `id` ahead of its next frame (inside the same
/// `Output` frame, before the synchronized-output begin), and schedules
/// that frame.
pub fn queue_raw(state: &mut State, id: ClientId, bytes: &[u8]) {
    if let Some(c) = state.clients.get_mut(&id) {
        c.outbox.queue_raw(bytes);
    }
    mark_dirty(state, id);
}

/// Client `id` needs a new frame.
pub(crate) fn mark_dirty(state: &mut State, id: ClientId) {
    if let Some(c) = state.clients.get_mut(&id) {
        c.dirty = true;
        request_render(state, Instant::now());
    }
}

/// Every client needs a new frame.
pub(crate) fn mark_all_dirty(state: &mut State) {
    for c in state.clients.values_mut() {
        c.dirty = true;
    }
    request_render(state, Instant::now());
}

/// Pane output woke the actor: clients showing a pane that changed redraw.
pub fn wake(state: &mut State) {
    let dirty: BTreeSet<PaneId> = state
        .model
        .panes
        .values()
        .filter(|p| p.handle.take_dirty())
        .map(|p| p.id)
        .collect();
    if dirty.is_empty() || state.clients.is_empty() {
        return;
    }
    let model = &state.model;
    let mut any = false;
    for c in state.clients.values_mut() {
        let shown = model.sessions.get(&c.session).map(|s| s.active);
        if dirty
            .iter()
            .any(|p| model.panes.get(p).map(|p| p.window) == shown)
        {
            c.dirty = true;
            any = true;
        }
    }
    if any {
        request_render(state, Instant::now());
    }
}

/// Schedules a render of the dirty clients, at most once per
/// [`FRAME_INTERVAL`].
pub(crate) fn request_render(state: &mut State, now: Instant) {
    let r = &mut state.registry;
    if r.render_at.is_none() && !state.clients.is_empty() {
        let earliest = r.last_render.map_or(now, |last| last + FRAME_INTERVAL);
        r.render_at = Some(earliest.max(now));
    }
}

/// The next time [`tick`] has work: a frame, the status clock, a size
/// owner's debounce, or a client's escape-time or repeat-time.
pub fn deadline(state: &State) -> Option<Instant> {
    let escape = escape_time(&state.model);
    let r = &state.registry;
    let clients = state.clients.values().flat_map(|c| {
        [
            c.decoder.deadline(escape),
            c.keys.deadline(),
            c.message.as_ref().map(|(_, until)| *until),
        ]
    });
    [r.render_at, r.status_at, r.names_at]
        .into_iter()
        .chain(r.owners.values().map(SizeOwner::deadline))
        .chain(clients)
        .flatten()
        .min()
}

/// Runs whatever is due at `now` (see [`deadline`]).
pub fn tick(state: &mut State, now: Instant) {
    let mut resized = false;
    for c in state.clients.values_mut() {
        if let Some(size) = c.pending_size.take() {
            resized |= c.size != size;
            c.size = size;
        }
    }
    if resized {
        changed(state);
    }
    let escape = escape_time(&state.model);
    let ids: Vec<ClientId> = state.clients.keys().copied().collect();
    for id in ids {
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        if c.keys.expire(now) {
            c.dirty = true;
            request_render(state, now);
        }
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        if c.message.as_ref().is_some_and(|(_, until)| *until <= now) {
            c.message = None;
            c.dirty = true;
            request_render(state, now);
        }
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        if c.decoder.deadline(escape).is_some_and(|d| d <= now) {
            for input in c.decoder.flush(now, escape) {
                route(state, id, input, now);
            }
        }
    }

    let mut switched = false;
    for owner in state.registry.owners.values_mut() {
        switched |= owner.settle(now);
    }
    if state.registry.names_at.is_some_and(|at| at <= now) {
        state.registry.names_at = None;
        switched |= automatic_rename(&mut state.model);
    }
    if switched {
        changed(state);
    }

    if state.registry.status_at.is_some_and(|at| at <= now) {
        state.registry.status_at = None;
        for c in state.clients.values_mut() {
            c.dirty = true;
        }
        request_render(state, now);
    }

    if state.registry.render_at.is_some_and(|at| at <= now) {
        state.registry.render_at = None;
        state.registry.last_render = Some(now);
        render_dirty(state);
        // A client whose outbox is still full keeps its dirty flag: look
        // again next frame.
        if state.clients.values().any(|c| c.dirty) {
            request_render(state, now);
        }
    }
    arm_status_clock(state, now);
    if state.registry.names_at.is_none() && !state.clients.is_empty() {
        state.registry.names_at = Some(now + NAME_INTERVAL);
    }
}

/// Schedules the next status redraw for its clock, every `status-interval`
/// seconds (the shortest among the attached sessions; 0 turns it off).
fn arm_status_clock(state: &mut State, now: Instant) {
    if state.registry.status_at.is_some() {
        return;
    }
    let model = &state.model;
    state.registry.status_at = state
        .clients
        .values()
        .filter(|c| status_on(model, c.session))
        .filter_map(|c| {
            model
                .options
                .lookup("status-interval", &[Owner::Session(c.session)])?
                .parse::<u64>()
                .ok()
                .filter(|s| *s > 0)
        })
        .min()
        .map(|secs| now + Duration::from_secs(secs));
}

fn render_dirty(state: &mut State) {
    let ids: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(_, c)| c.dirty && c.outbox.ready())
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        if state
            .clients
            .get(&id)
            .is_some_and(|c| c.outbox.needs_full_redraw())
        {
            graphics::on_full_redraw(state, id);
        }
        crate::server::passthrough::on_active_changed(state, id);
        let Some(c) = state.clients.get(&id) else {
            continue;
        };
        let composed = compose_client(state, id, c);
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        c.dirty = false;
        if let Some((screen, viewport)) = composed {
            c.viewport = viewport;
            let caps = c.caps;
            // A dropped frame keeps the client dirty, so it is redrawn once
            // its outbox has room even if nothing else changes.
            c.dirty = !c.outbox.offer(screen, &caps);
        }
    }
}

/// Whether `session` shows a status row.
fn status_on(model: &Model, session: SessionId) -> bool {
    model
        .options
        .lookup("status", &[Owner::Session(session)])
        .is_some_and(|v| v != "off")
}

/// The window size a client of `session` with `size` wants: its terminal
/// less the status row, never below 1x1.
fn window_size(model: &Model, session: SessionId, (cols, rows): (u16, u16)) -> (u16, u16) {
    let status = u16::from(status_on(model, session));
    (cols.max(1), rows.saturating_sub(status).max(1))
}

/// One cell's size in pixels on the client `session`'s windows are sized
/// for, if it said; what a new pane there starts with.
pub(crate) fn cell_px(state: &State, session: SessionId) -> Option<(u16, u16)> {
    let owner = state.registry.owners.get(&session)?;
    state.clients.get(&owner.client)?.caps.cell_px
}

/// Sizes every session that has attached clients to its owner (spec D6),
/// in cells and, where the owner said its cell size, in pixels. A session
/// no client shows keeps what it had (0 pixels until a client says).
fn apply_sizes(state: &mut State) {
    let sizes: Vec<(SessionId, (u16, u16))> = state
        .registry
        .owners
        .iter()
        .filter_map(|(s, o)| {
            let c = state.clients.get(&o.client)?;
            Some((*s, window_size(&state.model, *s, c.size)))
        })
        .collect();
    for (session, size) in sizes {
        resize_session(&mut state.model, session, size);
        let cell_px = cell_px(state, session);
        let model = &state.model;
        let windows = model.sessions.get(&session).map(|s| &s.windows);
        for w in windows.into_iter().flatten() {
            let panes = model.panes.values().filter(|p| p.window == *w);
            for p in panes {
                if let Err(e) = p.handle.set_cell_px(cell_px) {
                    eprintln!("grofe server: resizing {}: {e}", p.id);
                }
            }
        }
    }
}

/// Gives `session` and all its windows `(cols, rows)`, laying out again
/// (a zoomed window's active pane keeps filling it).
fn resize_session(model: &mut Model, session: SessionId, (cols, rows): (u16, u16)) {
    let Some(s) = model.sessions.get_mut(&session) else {
        return;
    };
    s.cols = cols;
    s.rows = rows;
    for id in s.windows.clone() {
        // A window `resize-window` sized (`window-size manual`) keeps it.
        let manual = model.windows.get(&id).is_some_and(|w| {
            model
                .options
                .lookup("window-size", &options::chain(model, w.active))
                == Some("manual")
        });
        let Some(w) = model.windows.get_mut(&id) else {
            continue;
        };
        if manual || (w.cols, w.rows) == (cols, rows) {
            continue;
        }
        w.cols = cols;
        w.rows = rows;
        let (zoomed, active) = (w.zoomed, w.active);
        model.relayout(id);
        if zoomed && let Some(p) = model.panes.get_mut(&active) {
            p.cols = cols;
            p.rows = rows;
            if let Err(e) = p.handle.resize(cols, rows) {
                eprintln!("grofe server: resizing {active}: {e}");
            }
        }
    }
}

/// Moves a viewport of `visible` cells over a `window`, along one axis at
/// a time, as little as needed to keep `cursor` inside (spec D6 crop). A
/// window that fits has viewport 0.
pub fn follow(
    viewport: (u16, u16),
    cursor: Option<(u16, u16)>,
    window: (u16, u16),
    visible: (u16, u16),
) -> (u16, u16) {
    let axis = |v: u16, c: Option<u16>, win: u16, vis: u16| {
        if win <= vis {
            return 0;
        }
        let max = win - vis;
        let mut v = v.min(max);
        if let Some(c) = c {
            if c < v {
                v = c;
            } else if c >= v + vis {
                v = c + 1 - vis;
            }
        }
        v.min(max)
    };
    (
        axis(viewport.0, cursor.map(|c| c.0), window.0, visible.0),
        axis(viewport.1, cursor.map(|c| c.1), window.1, visible.1),
    )
}

/// The visible panes of `w` and their rects in window coordinates (only
/// the active pane when zoomed).
fn visible_panes(w: &crate::server::model::Window) -> Vec<(PaneId, Rect)> {
    if w.zoomed {
        vec![(w.active, w.area())]
    } else {
        w.layout.compute(w.area())
    }
}

/// Where things are on client `c`'s screen when drawn with `viewport`.
fn overlay_ctx_at(
    model: &Model,
    id: ClientId,
    c: &ClientEntry,
    viewport: (u16, u16),
) -> OverlayCtx {
    let status = status_on(model, c.session);
    let (cols, rows) = c.size;
    let win_rows = rows.saturating_sub(u16::from(status));
    let window = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active));
    let panes = window
        .map(visible_panes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(pane, r)| {
            let (vx, vy) = (u32::from(viewport.0), u32::from(viewport.1));
            let left = u32::from(r.x).max(vx);
            let top = u32::from(r.y).max(vy);
            let right = (u32::from(r.x) + u32::from(r.cols)).min(vx + u32::from(cols));
            let bottom = (u32::from(r.y) + u32::from(r.rows)).min(vy + u32::from(win_rows));
            (left < right && top < bottom).then(|| {
                let rect = Rect {
                    x: (left - vx) as u16,
                    y: (top - vy) as u16,
                    cols: (right - left) as u16,
                    rows: (bottom - top) as u16,
                };
                (pane, rect)
            })
        })
        .collect();
    OverlayCtx {
        client: id,
        viewport,
        status_row: (status && rows > 0).then(|| rows - 1),
        panes,
    }
}

/// Where things are on client `id`'s screen as last drawn, for an input
/// hook mapping a pointer to a pane.
pub fn overlay_ctx(state: &State, id: ClientId) -> Option<OverlayCtx> {
    let c = state.clients.get(&id)?;
    Some(overlay_ctx_at(&state.model, id, c, c.viewport))
}

/// A window option's style, looked up from the window's active pane.
fn window_style(model: &Model, w: &crate::server::model::Window, name: &str) -> status::Style {
    let chain = options::chain(model, w.active);
    status::parse_style(model.options.lookup(name, &chain).unwrap_or(""))
}

/// Composes client `c`'s next screen, and the viewport it was drawn with:
/// the frame, then every overlay (lowest priority first), then the
/// wide-pair fix-up.
fn compose_client(state: &State, id: ClientId, c: &ClientEntry) -> Option<(Screen, (u16, u16))> {
    let model = &state.model;
    let s = model.sessions.get(&c.session)?;
    let w = model.windows.get(&s.active)?;
    let status = status_on(model, s.id);
    let visible = (c.size.0, c.size.1.saturating_sub(u16::from(status)));
    let area = w.area();
    let rects = visible_panes(w);
    let borders = if w.zoomed {
        Vec::new()
    } else {
        w.layout.borders(area)
    };
    let mut cursor = None;
    let panes = rects
        .into_iter()
        .filter_map(|(id, rect)| {
            let snap = model.panes.get(&id)?.handle.snapshot_rows(0..rect.rows);
            if id == w.active {
                cursor = Some(CursorState {
                    row: snap.cursor.row.saturating_add(rect.y),
                    col: snap.cursor.col.saturating_add(rect.x),
                    ..snap.cursor
                });
            }
            Some((id, rect, snap))
        })
        .collect();
    let viewport = follow(
        c.viewport,
        cursor.map(|p| (p.col, p.row)),
        (w.cols, w.rows),
        visible,
    );
    let status_line = match &c.message {
        Some((text, _)) => Some(message_line(text, c.size.0)),
        None => status.then(|| status_line(model, c, s)),
    };
    let mut screen = render::compose(&FrameInputs {
        client: c.size,
        viewport,
        area,
        panes,
        borders,
        border_style: window_style(model, w, "pane-border-style"),
        active_border_style: window_style(model, w, "pane-active-border-style"),
        active: w.active,
        status: status_line,
        cursor,
        graphics: state.graphics.remap(),
    });
    let cx = overlay_ctx_at(model, id, c, viewport);
    for overlay in state.registry.overlays.iter().rev() {
        overlay.draw(state, id, &cx, &mut screen);
    }
    render::fix_wide_pairs(&mut screen);
    Some((screen, viewport))
}

/// The status row for client `c` on session `s` (spec §4.10): options
/// looked up from the current window's active pane, the window list with
/// each window's own format and variables, the local time.
fn status_line(model: &Model, c: &ClientEntry, s: &Session) -> StyledLine {
    let active = model.windows.get(&s.active).map(|w| w.active);
    let chain = active.map(|p| options::chain(model, p)).unwrap_or_default();
    let chain = if chain.is_empty() {
        vec![Owner::Session(s.id)]
    } else {
        chain
    };
    let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
    let length = |name: &str| opt(name).parse::<u16>().unwrap_or(0);
    let vars = ClientVars::new(model, c, active.map(|p| FormatCtx::new(model, p)));
    let windows: Vec<(String, ClientVars)> = s
        .windows
        .iter()
        .filter_map(|id| {
            let w = model.windows.get(id)?;
            let name = if *id == s.active {
                "window-status-current-format"
            } else {
                "window-status-format"
            };
            let format = model
                .options
                .lookup(name, &options::chain(model, w.active))
                .unwrap_or("")
                .to_string();
            Some((
                format,
                ClientVars::new(model, c, Some(FormatCtx::new(model, w.active))),
            ))
        })
        .collect();
    let entries: Vec<WindowStatus> = windows
        .iter()
        .map(|(format, vars)| WindowStatus {
            format,
            vars: vars as &dyn FormatVars,
        })
        .collect();
    status::render(&StatusInputs {
        left: opt("status-left"),
        right: opt("status-right"),
        left_length: length("status-left-length"),
        right_length: length("status-right-length"),
        style: opt("status-style"),
        vars: &vars,
        windows: &entries,
        width: c.size.0,
        now: local_now(),
    })
}

/// A message across the whole status row, in `message-style`.
// ponytail: with `status off` the message still takes the last row,
// drawn over the window's last row while it shows.
fn message_line(text: &str, width: u16) -> StyledLine {
    let used = unicode_width::UnicodeWidthStr::width(text);
    let pad = usize::from(width).saturating_sub(used);
    StyledLine {
        spans: vec![Span {
            text: format!("{text}{}", " ".repeat(pad)),
            style: status::parse_style(MESSAGE_STYLE),
        }],
    }
}

/// The local broken-down time, for strftime sequences in the status line.
fn local_now() -> libc::tm {
    // SAFETY: `time` with a null pointer only returns the time;
    // `localtime_r` writes into the zeroed `tm` we own (all-zero is a
    // valid `tm`).
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// Format variables for a client: `client_*` (M2 addendum: real
/// `client_width`/`client_height`), falling through to the pane's
/// [`FormatCtx`].
pub struct ClientVars<'a> {
    model: &'a Model,
    client: &'a ClientEntry,
    pane: Option<FormatCtx<'a>>,
}

impl<'a> ClientVars<'a> {
    pub fn new(
        model: &'a Model,
        client: &'a ClientEntry,
        pane: Option<FormatCtx<'a>>,
    ) -> ClientVars<'a> {
        ClientVars {
            model,
            client,
            pane,
        }
    }

    fn session_name(&self, session: SessionId) -> String {
        self.model
            .sessions
            .get(&session)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }
}

impl FormatVars for ClientVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let c = self.client;
        match name {
            "client_width" => Some(c.size.0.to_string()),
            "client_height" => Some(c.size.1.to_string()),
            "client_name" => Some(c.name.clone()),
            "client_termname" => Some(c.term.clone()),
            "client_key_table" => Some(c.keys.table().to_string()),
            // ponytail: grofe always speaks UTF-8 to the client.
            "client_flags" => Some(if c.focused {
                "attached,focused,UTF-8".to_string()
            } else {
                "attached,UTF-8".to_string()
            }),
            // tmux: 1 while the client is in any table but its session's
            // `key-table` (always `root` here).
            "client_prefix" => Some(
                if c.keys.table() == ROOT_TABLE {
                    "0"
                } else {
                    "1"
                }
                .to_string(),
            ),
            "client_activity" => Some(
                c.activity_time
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    .to_string(),
            ),
            "client_session" => Some(self.session_name(c.session)),
            "client_last_session" => Some(
                self.model
                    .client_last_session
                    .get(&c.ctx.client?)
                    .map(|s| self.session_name(*s))
                    .unwrap_or_default(),
            ),
            _ => self.pane.as_ref()?.get(name),
        }
    }
}

/// The global `escape-time`, in milliseconds.
fn escape_time(model: &Model) -> Duration {
    let ms = model
        .options
        .lookup("escape-time", &[])
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(25);
    Duration::from_millis(ms)
}

/// The session's `prefix` and `repeat-time`.
fn key_settings(model: &Model, session: SessionId) -> KeySettings {
    let chain = [Owner::Session(session)];
    let default = KeySettings::default();
    KeySettings {
        prefix: model
            .options
            .lookup("prefix", &chain)
            .map_or(default.prefix, |v| {
                parse_prefix(v).unwrap_or(default.prefix)
            }),
        repeat_time: model
            .options
            .lookup("repeat-time", &chain)
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(default.repeat_time, Duration::from_millis),
    }
}

fn active_pane(model: &Model, session: SessionId) -> Option<PaneId> {
    let s = model.sessions.get(&session)?;
    Some(model.windows.get(&s.active)?.active)
}

/// Input bytes from client `id` at `now`: activity for D6, then each
/// decoded input is routed in turn.
fn input(state: &mut State, id: ClientId, bytes: &[u8], now: Instant) {
    let escape = escape_time(&state.model);
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    c.activity = now;
    c.activity_time = SystemTime::now();
    let session = c.session;
    let inputs = c.decoder.feed(bytes, now, escape);
    if let Some(owner) = state.registry.owners.get_mut(&session) {
        owner.activity(id, now);
    }
    touch_session(&mut state.model, session);
    for input in inputs {
        // As tmux: a key press takes a message down, and still counts.
        if let Input::Key(_) = input
            && let Some(c) = state.clients.get_mut(&id)
            && c.message.take().is_some()
        {
            c.dirty = true;
            request_render(state, now);
        }
        route(state, id, input, now);
    }
}

/// Routes one input: input hooks first (every input), then the client's
/// key table, reading the active pane's modes now, since a command routed
/// just before may have changed the active pane. Focus reports go no
/// further than the client's own focus flag while `focus-events` is off
/// (R28).
fn route(state: &mut State, id: ClientId, input: Input, now: Instant) {
    if let Input::Focus(focused) = input {
        let Some(c) = state.clients.get_mut(&id) else {
            return;
        };
        c.focused = focused;
        if state.model.options.lookup("focus-events", &[]) != Some("on") {
            return;
        }
    }
    let hooks = state.registry.input_hooks.clone();
    let hooked = hooks
        .iter()
        .find_map(|hook| hook.on_input(state, id, &input, now));
    let Some(session) = state.clients.get(&id).map(|c| c.session) else {
        return;
    };
    let pane = active_pane(&state.model, session);
    let routed = match hooked {
        Some(routed) => routed,
        None => {
            let Some(modes) = pane
                .and_then(|p| state.model.panes.get(&p))
                .map(|p| p.handle.modes())
            else {
                return;
            };
            let settings = key_settings(&state.model, session);
            let State {
                clients, registry, ..
            } = &mut *state;
            let Some(c) = clients.get_mut(&id) else {
                return;
            };
            let table = c.keys.table().to_string();
            let routed = c
                .keys
                .route(&input, now, &registry.key_tables, &settings, &modes);
            if c.keys.table() != table {
                c.dirty = true;
                request_render(state, now);
            }
            routed
        }
    };
    match routed {
        Routed::ToPane(bytes) => {
            if let Some(p) = pane.and_then(|p| state.model.panes.get(&p))
                && p.handle.enqueue_input(bytes).is_err()
            {
                eprintln!("grofe server: input queue full for {}", p.id);
            }
        }
        Routed::Command(cmds, dialect) => run_binding(state, id, &cmds, dialect),
        Routed::Consumed => {}
    }
}

/// Runs bound commands for client `id` in order, each with the client's
/// context and its active pane (as it is then) as the current pane, parsed
/// in `dialect`, the binding's own (spec §4.5), never the attaching
/// client's: a native binding stays native under `tmux attach`. The first
/// error stops the rest and shows on the client's status row, as tmux shows
/// it. Output, and any deferred answer, is dropped.
pub fn run_binding(state: &mut State, id: ClientId, cmds: &[Vec<String>], dialect: Dialect) {
    for argv in cmds {
        let Some(c) = state.clients.get(&id) else {
            break;
        };
        let ctx = ClientCtx {
            pane: active_pane(&state.model, c.session),
            dialect,
            ..c.ctx.clone()
        };
        let result = dispatch::run_split(state, &ctx, argv);
        state.deferred_reply = None;
        if let Err(e) = result {
            show_message(state, id, e.trim_end());
            break;
        }
    }
    state.events.flush();
    changed(state);
}

/// The client a command acts on: the requesting client, else the most
/// recently active client on the request's pane's session, else the most
/// recently active client.
pub(crate) fn current_client(state: &State, ctx: &ClientCtx) -> Option<ClientId> {
    if let Some(id) = ctx.client.filter(|id| state.clients.contains_key(id)) {
        return Some(id);
    }
    let session = ctx
        .pane
        .and_then(|p| state.model.panes.get(&p))
        .and_then(|p| state.model.windows.get(&p.window))
        .map(|w| w.session);
    state
        .clients
        .iter()
        .filter(|(_, c)| session.is_none_or(|s| c.session == s))
        .max_by_key(|(_, c)| c.activity)
        .map(|(id, _)| *id)
}

fn client_by_name(state: &State, name: &str) -> Result<ClientId, String> {
    state
        .clients
        .iter()
        .find(|(_, c)| c.name == name)
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("can't find client: {name}"))
}

/// `attach-session` run as a command: only an attached client can (its
/// bindings, for example); it switches that client to the target session.
/// The CLI's `attach` is the attach client itself (spec §4.6).
pub fn attach_session(
    state: &mut State,
    ctx: &ClientCtx,
    c: AttachSession,
) -> Result<String, String> {
    let Some(id) = ctx.client.filter(|id| state.clients.contains_key(id)) else {
        // tmux resolves the target before it finds there is no terminal.
        check_target(&state.model, &c.target, ctx, c.nest)?;
        return Err("open terminal failed: not a terminal".to_string());
    };
    let session = select_target(&mut state.model, &c.target, ctx)?;
    if c.detach_others {
        detach_session(state, session, Some(id));
    }
    show_session(state, id, session);
    Ok(String::new())
}

/// `detach-client`: `-s` detaches every client on a session, `-t` one
/// client by name, neither the current client; `-a` detaches every client
/// but that one instead.
pub fn detach_client(
    state: &mut State,
    ctx: &ClientCtx,
    c: DetachClient,
) -> Result<String, String> {
    if let Some(target) = &c.session {
        let session = state.model.resolve_session(target, ctx)?;
        detach_session(state, session, None);
        return Ok(String::new());
    }
    let id = match &c.client {
        Some(name) => client_by_name(state, name)?,
        None => current_client(state, ctx).ok_or_else(|| "no current client".to_string())?,
    };
    if c.all_others {
        let others: Vec<ClientId> = state.clients.keys().copied().filter(|o| *o != id).collect();
        for other in others {
            detach(state, other);
        }
    } else {
        detach(state, id);
    }
    Ok(String::new())
}

/// `list-clients`, in client order; `-F` expands per client, with
/// `client_*` variables over the client's current pane's. The default line
/// is tmux 3.7b's (its `[user ...]` part only shows for another user's
/// client, which grofe never has).
pub fn list_clients(state: &State, c: ListClients) -> Result<String, String> {
    let model = &state.model;
    let format = c.format.unwrap_or_else(|| {
        "#{client_name}: #{session_name} [#{client_width}x#{client_height} #{client_termname}] \
         (#{client_flags})"
            .to_string()
    });
    Ok(state
        .clients
        .values()
        .map(|client| {
            let pane = active_pane(model, client.session).map(|p| FormatCtx::new(model, p));
            let vars = ClientVars::new(model, client, pane);
            format!("{}\n", cmd::expand(&format, &vars))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;

    /// A server state with no sessions and one client, as the actor
    /// would hold it.
    fn state_with_client() -> (State, ClientId) {
        let mut state = crate::server::test_state();
        let id = ClientId(1);
        let (outbox, _) = Outbox::new();
        state.clients.insert(
            id,
            ClientEntry {
                name: client_name(id),
                session: SessionId(0),
                ctx: ClientCtx {
                    cwd: PathBuf::from("/"),
                    pane: None,
                    env: Vec::new(),
                    client: Some(id),
                    dialect: crate::protocol::Dialect::Grofe,
                },
                size: (80, 24),
                pending_size: None,
                term: "xterm-256color".to_string(),
                caps: Caps::default(),
                activity: Instant::now(),
                activity_time: SystemTime::now(),
                decoder: Decoder::new(),
                keys: KeyState::new(),
                focused: true,
                message: None,
                viewport: (0, 0),
                mouse_modes: None,
                dirty: false,
                outbox,
                exit: None,
            },
        );
        (state, id)
    }

    #[test]
    fn a_key_press_takes_a_message_down_before_display_time() {
        let (mut state, id) = state_with_client();
        show_message(&mut state, id, "no last window");
        let shown = state.clients[&id].message.clone();
        let (text, until) = shown.expect("message shown");
        assert_eq!(text, "no last window");
        // A focus report is not a key: the message stays.
        let now = Instant::now();
        input(&mut state, id, b"\x1b[O", now);
        assert!(state.clients[&id].message.is_some());
        // A key press well before the message expires takes it down.
        assert!(now < until);
        input(&mut state, id, b"x", now);
        assert!(state.clients[&id].message.is_none());
        assert!(state.clients[&id].dirty);
    }

    /// R1/R2 and item 3: the M3 and M3.6 hooks and overlays (prompt, menu,
    /// popup, chooser, mouse) are registered in priority order, and with
    /// nothing open every hook passes every kind of input on and every
    /// overlay leaves the screen alone.
    #[test]
    fn the_builtin_hooks_and_overlays_are_inert() {
        let (mut state, id) = state_with_client();
        crate::server::register_builtins(&mut state);
        assert_eq!(state.registry.input_hooks.len(), 5);
        assert_eq!(state.registry.overlays.len(), 5);
        let now = Instant::now();
        let inputs = [
            Input::Key(crate::server::input::parse_key("C-b").expect("key")),
            Input::Paste {
                data: b"x".to_vec(),
                start: true,
                end: true,
            },
            Input::Focus(false),
            Input::Mouse(crate::server::input::Mouse {
                kind: crate::server::input::MouseKind::WheelUp,
                col: 3,
                row: 4,
                mods: Default::default(),
            }),
        ];
        for hook in state.registry.input_hooks.clone() {
            for input in &inputs {
                assert_eq!(hook.on_input(&mut state, id, input, now), None, "{input:?}");
            }
        }
        let mut screen = screen_with('a');
        let before = screen.clone();
        let cx = overlay_ctx(&state, id).expect("ctx");
        for overlay in state.registry.overlays.iter().rev() {
            overlay.draw(&state, id, &cx, &mut screen);
        }
        assert_eq!(screen, before);
    }

    /// Item 13: the config-load hook changes nothing until T3.5 fills it.
    #[test]
    fn apply_config_at_start_is_a_no_op() {
        let (mut state, _) = state_with_client();
        let options = |s: &State| {
            s.model
                .options
                .at(Owner::Global)
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect::<Vec<_>>()
        };
        let (before, keys) = (options(&state), format!("{:?}", state.registry.key_tables));
        crate::server::dispatch::options::apply_config_at_start(&mut state);
        assert_eq!(options(&state), before);
        assert_eq!(format!("{:?}", state.registry.key_tables), keys);
    }

    /// Item 11: raw bytes go out ahead of the next frame, in the same
    /// frame; with no screen change they go out alone; a dropped frame
    /// keeps them for the next one.
    #[test]
    fn raw_bytes_lead_the_next_frame_and_survive_a_drop() {
        let caps = Caps::default();
        let (mut outbox, mut rx) = Outbox::new();
        outbox.queue_raw(b"RAW1");
        assert!(outbox.offer(screen_with('a'), &caps));
        let first = rx.try_recv().expect("first frame");
        assert!(first.starts_with(b"RAW1\x1b[r"), "{first:?}");
        // Nothing changed on screen: the raw bytes alone.
        outbox.queue_raw(b"RAW2");
        assert!(outbox.offer(screen_with('a'), &caps));
        assert_eq!(rx.try_recv().expect("raw only"), b"RAW2");
        // The slot is full: the frame is dropped, the raw bytes are not.
        assert!(outbox.offer(screen_with('b'), &caps));
        outbox.queue_raw(b"RAW3");
        assert!(!outbox.offer(screen_with('c'), &caps));
        assert!(outbox.needs_full_redraw());
        let _ = rx.try_recv().expect("the 'b' frame");
        assert!(outbox.offer(screen_with('c'), &caps));
        let next = rx.try_recv().expect("redraw");
        assert!(next.starts_with(b"RAW3\x1b[r"), "{next:?}");
    }

    /// Item 5 (R2): mouse modes follow the client's session's `mouse`
    /// option, written once per change, ahead of the frame.
    #[test]
    fn mouse_modes_follow_the_option() {
        let (mut state, id) = state_with_client();
        let raw =
            |s: &mut State| std::mem::take(&mut s.clients.get_mut(&id).expect("c").outbox.raw);
        sync_mouse_modes(&mut state);
        assert_eq!(raw(&mut state), MOUSE_ON, "grofe's default is on");
        sync_mouse_modes(&mut state);
        assert_eq!(raw(&mut state), b"", "unchanged: nothing written");
        state
            .model
            .options
            .set(Owner::Global, "mouse", "off", false)
            .expect("set");
        sync_mouse_modes(&mut state);
        assert_eq!(raw(&mut state), MOUSE_OFF);
    }

    /// R28: focus reports update the client's focus flag either way, but
    /// reach hooks and panes only while `focus-events` is on.
    #[test]
    fn focus_events_off_stops_focus_reports() {
        struct Seen;
        impl InputHook for Seen {
            fn on_input(
                &self,
                s: &mut State,
                _: ClientId,
                i: &Input,
                _: Instant,
            ) -> Option<Routed> {
                if let Input::Focus(_) = i {
                    s.exit = true;
                }
                None
            }
        }
        let (mut state, id) = state_with_client();
        state.registry.input_hooks.push(Arc::new(Seen));
        state
            .model
            .options
            .set(Owner::Global, "focus-events", "off", false)
            .expect("set");
        route(&mut state, id, Input::Focus(false), Instant::now());
        assert!(!state.clients[&id].focused);
        assert!(!state.exit, "no hook saw it");
        state
            .model
            .options
            .set(Owner::Global, "focus-events", "on", false)
            .expect("set");
        route(&mut state, id, Input::Focus(true), Instant::now());
        assert!(state.clients[&id].focused);
        assert!(state.exit, "a hook saw it");
    }

    /// Fix round 1: every pane's graphics flag follows whether an
    /// attached client shows kitty graphics (as `GROFE_TEST_CAPS` makes a
    /// client claim it in its hello): on when one attaches, set on a pane
    /// spawned meanwhile, off once it leaves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graphics_ok_follows_the_attached_clients() {
        let (mut state, id) = state_with_client();
        let ctx = ClientCtx {
            cwd: std::env::temp_dir(),
            pane: None,
            env: Vec::new(),
            client: None,
            dialect: crate::protocol::Dialect::Grofe,
        };
        let run = |state: &mut State, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            dispatch::run(state, &ctx, &argv).expect("command");
        };
        let flags = |state: &State| {
            state
                .model
                .panes
                .values()
                .map(|p| p.handle.graphics_ok())
                .collect::<Vec<_>>()
        };
        run(&mut state, &["new-session", "-d", "-s", "g", "sleep 30"]);
        changed(&mut state);
        assert_eq!(flags(&state), [false], "a client without kitty graphics");
        if let Some(c) = state.clients.get_mut(&id) {
            c.caps.kitty_graphics = true;
        }
        changed(&mut state);
        assert_eq!(flags(&state), [true]);
        run(&mut state, &["split-window", "-t", "g", "sleep 30"]);
        assert_eq!(flags(&state), [true, true], "a new pane starts on");
        gone(&mut state, id);
        assert_eq!(flags(&state), [false, false], "the client left");
    }

    #[test]
    fn a_new_client_takes_the_size_only_after_the_debounce() {
        let (a, b) = (ClientId(1), ClientId(2));
        let t0 = Instant::now();
        let mut owner = SizeOwner::new(a);
        owner.activity(b, t0);
        assert_eq!(owner.deadline(), Some(t0 + SIZE_DEBOUNCE));
        // Typing on does not push the switch back.
        owner.activity(b, t0 + Duration::from_millis(300));
        assert_eq!(owner.deadline(), Some(t0 + SIZE_DEBOUNCE));
        assert!(!owner.settle(t0 + Duration::from_millis(499)));
        assert_eq!(owner.client, a);
        assert!(owner.settle(t0 + SIZE_DEBOUNCE));
        assert_eq!(owner.client, b);
        assert_eq!(owner.deadline(), None);
    }

    #[test]
    fn the_owner_typing_cancels_a_pending_switch() {
        let (a, b) = (ClientId(1), ClientId(2));
        let t0 = Instant::now();
        let mut owner = SizeOwner::new(a);
        owner.activity(b, t0);
        owner.activity(a, t0 + Duration::from_millis(100));
        assert_eq!(owner.deadline(), None);
        assert!(!owner.settle(t0 + Duration::from_secs(1)));
        assert_eq!(owner.client, a);
    }

    #[test]
    fn viewport_is_zero_when_the_window_fits() {
        assert_eq!(follow((5, 5), Some((70, 20)), (80, 23), (80, 23)), (0, 0));
        assert_eq!(follow((5, 5), Some((10, 5)), (40, 11), (80, 23)), (0, 0));
    }

    #[test]
    fn viewport_moves_just_enough_to_show_the_cursor() {
        // Cursor right and below the visible area.
        assert_eq!(follow((0, 0), Some((64, 19)), (80, 23), (40, 11)), (25, 9));
        // Inside already: no movement.
        assert_eq!(follow((25, 9), Some((30, 12)), (80, 23), (40, 11)), (25, 9));
        // Left of and above it: the viewport starts at the cursor.
        assert_eq!(follow((25, 9), Some((3, 2)), (80, 23), (40, 11)), (3, 2));
        // Never past the window's far edge, even after a shrink.
        assert_eq!(follow((70, 20), None, (80, 23), (40, 11)), (40, 12));
    }

    fn screen_with(c: char) -> Screen {
        let mut s = Screen::new(4, 2);
        s.cells[0].c = c;
        s
    }

    #[test]
    fn a_full_outbox_drops_the_diff_and_redraws_in_full_once_free() {
        let caps = Caps::default();
        let (mut outbox, mut rx) = Outbox::new();
        assert!(outbox.offer(screen_with('a'), &caps));
        assert!(outbox.ready());
        // The writer has not taken the first frame: the second is dropped.
        assert!(!outbox.offer(screen_with('b'), &caps));
        assert!(!outbox.ready());
        assert!(outbox.prev.is_none());
        let first = rx.try_recv().expect("first frame");
        assert!(
            first.windows(4).any(|w| w == b"\x1b[2J"),
            "full redraw first"
        );
        assert!(outbox.ready());
        // The next offer is a full redraw, not a diff against 'a'.
        assert!(outbox.offer(screen_with('b'), &caps));
        let next = rx.try_recv().expect("redraw");
        assert!(
            next.windows(4).any(|w| w == b"\x1b[2J"),
            "full redraw after a drop"
        );
        assert!(rx.try_recv().is_err());
        // Unchanged: nothing is sent.
        assert!(outbox.offer(screen_with('b'), &caps));
        assert!(rx.try_recv().is_err());
    }
}
