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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use nix::libc;
use tokio::sync::{mpsc, oneshot};

use crate::cmd::{
    self, AttachSession, DetachClient, FormatVars, ListClients, LoopItem, LoopKind, LoopSort,
    NameKind, Resolve, Target,
};
use crate::protocol::{Caps, ClientCtx, Dialect, Failure, Frame};
use crate::server::borders::{Indicators, PaneLines, PaneStatus};
use crate::server::dispatch::options::{self, FormatCtx, Owner};
use crate::server::dispatch::sorting;
use crate::server::dispatch::window::automatic_rename;
use crate::server::input::{
    Decoder, Input, InputHook, KeySettings, KeyState, KeyTables, ROOT_TABLE, Routed,
    default_bindings, parse_prefix,
};
use crate::server::model::{Model, Session};
use crate::server::pane::CursorState;
use crate::server::render::{self, FrameInputs, Overlay, OverlayCtx, Screen};
use crate::server::screenimg::{self, ImageCaps, ScreenImage};
use crate::server::status::{
    self, RangeKind, StatusInputs, StatusRow, StyledLine, WindowOptions, WindowStatus,
};
use crate::server::{State, dispatch, graphics, hooks};
use crate::types::{ClientId, PaneId, Rect, SessionId, WindowId, cap_size};

/// At most one frame per client per this long (spec §4.4: ~16 ms).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How long a status-line message stays up (tmux's `display-time`), when
/// the session has no `display-time` option.
pub const DISPLAY_TIME: Duration = Duration::from_millis(750);
/// tmux 3.7b's default `message-format`.
pub(crate) const MESSAGE_FORMAT: &str =
    "#[#{?#{command_prompt},#{E:message-command-style},#{E:message-style}}]#{message}";

/// How soon after a pane changed its window's name is checked for
/// `automatic-rename` (tmux's `NAME_INTERVAL`).
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
    /// The attaching process, from the socket's peer credentials
    /// (`client_pid`).
    pub pid: Option<i32>,
    /// `attach -t`, raw.
    pub target: Option<String>,
    pub detach_others: bool,
    pub nest: bool,
    /// The client's terminal device, if it said (`client_tty`).
    pub tty: Option<String>,
    /// Its own `new-session` made the session: no `client-attached`.
    pub created: bool,
    /// `attach-session -c -E -f -r -x`.
    pub extras: crate::protocol::AttachExtras,
}

/// An accepted attach: the connection writes `output` (one frame at a
/// time) and, once the actor lets go of the sender, the reason from `exit`.
#[derive(Debug)]
pub struct Accepted {
    pub id: ClientId,
    pub output: mpsc::Receiver<Vec<u8>>,
    /// Dropped without a value when the server goes away mid-write.
    pub exit: oneshot::Receiver<String>,
    /// A value asks the client to suspend itself (`suspend-client`, TC.9):
    /// its connection sends a `suspend` frame after the output before it.
    pub suspend: mpsc::Receiver<()>,
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
    /// Bytes of dropped diffs, ever (`client_discarded`).
    discarded: u64,
    /// The images the client's terminal shows once everything sent so far
    /// is written (G3); unlike `prev`, kept across a dropped frame.
    placed: Vec<ScreenImage>,
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
            discarded: 0,
            placed: Vec::new(),
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

    /// Holds `bytes` for ahead of the next frame; false if they would pass
    /// [`RAW_CAP`] and were dropped.
    pub fn queue_raw(&mut self, bytes: &[u8]) -> bool {
        let fits = self.raw.len() + bytes.len() <= RAW_CAP;
        if fits {
            self.raw.extend_from_slice(bytes);
        }
        fits
    }

    /// Sends the queued raw bytes, then the diff that turns the client's
    /// screen into `next`, as ONE frame (so a synchronized-output begin/end
    /// pair is never split, and raw bytes land before it). If the slot is
    /// still full the frame is dropped, the raw bytes stay queued, and the
    /// next offer is a full redraw. Returns false for a dropped frame: the
    /// client still needs one, and must be offered it again once
    /// [`Outbox::ready`].
    pub fn offer(&mut self, next: Screen, caps: &Caps) -> bool {
        let diff = render::diff_placed(self.prev.as_ref(), &next, caps, &self.placed);
        if diff.is_empty() && self.raw.is_empty() {
            return true;
        }
        let mut bytes = std::mem::take(&mut self.raw);
        let raw_len = bytes.len();
        bytes.extend_from_slice(&diff);
        match self.tx.try_send(bytes) {
            Ok(()) => {
                self.placed.clone_from(&next.images);
                self.prev = Some(next);
                self.blocked = false;
                true
            }
            Err(mpsc::error::TrySendError::Full(mut bytes)) => {
                self.discarded += (bytes.len() - raw_len) as u64;
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
    /// `client_name`, what `detach-client -t` takes: its tty when it
    /// attached in the tmux dialect, as tmux names clients, else
    /// `client-N`.
    pub name: String,
    /// `client_tty`: the client's terminal device, if it said.
    pub tty: Option<String>,
    /// `client_pid`: the client process.
    pub pid: Option<i32>,
    pub session: SessionId,
    /// The attaching CLI's context, with `client` set to this client.
    pub ctx: ClientCtx,
    /// The client terminal, `(cols, rows)`.
    pub size: (u16, u16),
    /// The latest Resize frame's size, applied on the next tick so a
    /// burst of resizes costs one reflow.
    pub(crate) pending_size: Option<(u16, u16)>,
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
    /// The mouse report before the one being routed, as tmux's `m->lx`,
    /// `m->ly` and `m->lb` carry it: client cell and report code.
    pub mouse_prev: Option<(u16, u16, u32)>,
    mouse_last: Option<(u16, u16, u32)>,
    /// A message shown on the status line's first row until it expires or
    /// a key is pressed (a failed binding's error, as tmux shows it), in
    /// the language [`status::draw`] reads, and when it expires (`None`
    /// with `display-time 0`: only a key takes it down, as in tmux), and
    /// its level, for `message-pill`.
    message: Option<(String, Option<Instant>, render::Level)>,
    /// `display-message -N`: a key does not take the message down.
    message_ignores_keys: bool,
    /// The status rows as last drawn, top first, for their click ranges,
    /// and the client row the first of them was drawn on.
    status_rows: Vec<StatusRow>,
    status_at: u16,
    /// The window cell drawn at the client's top-left (crop, D6), as last
    /// drawn.
    pub viewport: (u16, u16),
    /// Set by `refresh-client -L/-R/-U/-D` (TC.9): the window whose
    /// visible part the client moved by hand, and where it is; the view
    /// then stops following the cursor until `-c`, another window, or the
    /// window fitting the client, as tmux's `pan_window`.
    pan: Option<(WindowId, u16, u16)>,
    /// `suspend-client` stopped the client (TC.9): nothing is drawn for it
    /// until it resumes and sends its size, which redraws it in full.
    suspended: bool,
    suspend_tx: mpsc::Sender<()>,
    /// `read-only`, `ignore-size` and the like.
    pub flags: ClientFlags,
    /// The outer terminal's mouse modes as last written (R2): 0 off, 1 on,
    /// 2 on with all motion; `None` before the first write.
    mouse_modes: Option<u8>,
    /// Input held while a binding's shell condition runs ([`hold_input`]),
    /// in order, and how many conditions hold it.
    held: VecDeque<Held>,
    held_bytes: usize,
    holds: u32,
    /// The hold passed its cap: all input is dropped until it is released.
    held_full: bool,
    /// Needs a new frame.
    dirty: bool,
    outbox: Outbox,
    exit: Option<oneshot::Sender<String>>,
}

/// What `refresh-client -f`, `attach-session -f -r` and `switch-client -r`
/// set on a client (tmux's `CLIENT_READONLY` and its kin).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientFlags {
    /// The client's keys, pastes and mouse reach no pane, and only the
    /// commands tmux marks read-only run from its bindings.
    pub read_only: bool,
    /// The client does not size the windows of its session.
    pub ignore_size: bool,
    /// The client stays attached when its session is destroyed.
    pub no_detach_on_destroy: bool,
    /// tmux gives such a client its own active pane per window; grofe keeps
    /// the flag (and shows it) but not that.
    pub active_pane: bool,
}

impl ClientFlags {
    /// `server_client_set_flags`: `flags` is a comma list of names, each
    /// switched off by a `!`; a name tmux does not know is skipped.
    pub fn apply(&mut self, flags: &str) {
        for word in flags.split(',') {
            let (on, name) = match word.strip_prefix('!') {
                Some(name) => (false, name),
                None => (true, word),
            };
            match name {
                // tmux masks `read-only` out of the flags it clears: a
                // client cannot be made writable this way.
                "read-only" => self.read_only |= on,
                "ignore-size" => self.ignore_size = on,
                "no-detach-on-destroy" => self.no_detach_on_destroy = on,
                "active-pane" => {
                    if on && !self.active_pane {
                        crate::compat::note_miss("client flag active-pane: cosmetic, ignored");
                    }
                    self.active_pane = on;
                }
                _ => {}
            }
        }
    }

    /// The names `#{client_flags}` lists for these, in tmux's order.
    fn names(self) -> Vec<&'static str> {
        [
            (self.ignore_size, "ignore-size"),
            (self.no_detach_on_destroy, "no-detach-on-destroy"),
            (self.read_only, "read-only"),
            (self.active_pane, "active-pane"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}

impl ClientEntry {
    /// `suspend-client` stopped the client and it has not resumed.
    pub(crate) fn is_suspended(&self) -> bool {
        self.suspended
    }
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
    /// The status-line messages `show-messages` prints (TC.9), oldest
    /// first, with when each was shown.
    messages: VecDeque<(SystemTime, String)>,
    /// `wait-for` channels (TC.9).
    waits: BTreeMap<String, WaitChannel>,
    /// The clients now locked, their `lock-command` running in a popup
    /// (`dispatch::lock`).
    pub(crate) locked: BTreeSet<ClientId>,
    /// Per session, the client activity its `lock-after-time` last fired
    /// for: tmux's lock timer is one-shot, so it waits for newer activity.
    pub(crate) lock_fired: BTreeMap<SessionId, Instant>,
    /// `server-access`: the users allowed besides the owner, and whether
    /// each may only read (uid, read-only). Root and the server's own user
    /// are always in it, as tmux's `server_acl_init`.
    acl: BTreeMap<u32, bool>,
}

impl Registry {
    pub(crate) fn acl_contains(&self, uid: u32) -> bool {
        self.acl.contains_key(&uid)
    }

    pub(crate) fn acl_entries(&self) -> impl Iterator<Item = (u32, bool)> + '_ {
        self.acl.iter().map(|(uid, ro)| (*uid, *ro))
    }

    pub(crate) fn acl_allow(&mut self, uid: u32) {
        self.acl.entry(uid).or_insert(false);
    }

    pub(crate) fn acl_deny(&mut self, uid: u32) {
        self.acl.remove(&uid);
    }

    pub(crate) fn acl_set_read_only(&mut self, uid: u32, read_only: bool) {
        if let Some(entry) = self.acl.get_mut(&uid) {
            *entry = read_only;
        }
    }
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
            messages: VecDeque::new(),
            waits: BTreeMap::new(),
            locked: BTreeSet::new(),
            lock_fired: BTreeMap::new(),
            acl: [0, nix::unistd::getuid().as_raw()]
                .into_iter()
                .map(|uid| (uid, false))
                .collect(),
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
    let req_created = req.created;
    let target = match &req.target {
        // A tmux attach reads `-t` the tmux way (`=name` exact, FE.1).
        Some(raw) if req.ctx.dialect == crate::protocol::Dialect::Tmux => {
            cmd::target::with_tmux_targets(|| cmd::target::parse_target(raw))
                .map_err(|e| e.to_string())?
        }
        Some(raw) => cmd::target::parse_target(raw).map_err(|e| e.to_string())?,
        None => Target::Current,
    };
    check_target(
        &state.model,
        &target,
        &req.ctx,
        req.nest,
        req.tty.as_deref(),
    )?;
    let session = select_target(&mut state.model, &target, &req.ctx, false)?;
    // `-c`: where the session's new windows start from now on.
    if let (Some(dir), Some(s)) = (&req.extras.cwd, state.model.sessions.get_mut(&session)) {
        s.cwd = req.ctx.cwd.join(dir);
    }
    if req.detach_others {
        detach_session_with(state, session, None, req.extras.kill_parent);
    }
    // `-r` makes the client read-only and have its size ignored; `-f` sets
    // flags as `refresh-client -f` does.
    let mut flags = ClientFlags::default();
    if let Some(list) = &req.extras.flags {
        flags.apply(list);
    }
    if req.extras.read_only {
        flags.read_only = true;
        flags.ignore_size = true;
    }

    let id = ClientId(state.registry.next_id);
    state.registry.next_id += 1;
    let (outbox, output) = Outbox::new();
    let (exit_tx, exit) = oneshot::channel();
    let (suspend_tx, suspend) = mpsc::channel(1);
    let ctx = ClientCtx {
        client: Some(id),
        ..req.ctx
    };
    // The tty comes from the client: one that is not a device path is
    // taken as none.
    let tty = req.tty.filter(|t| usable_tty(t));
    let name = match &tty {
        Some(tty) if ctx.dialect == crate::protocol::Dialect::Tmux => tty.clone(),
        _ => client_name(id),
    };
    state.clients.insert(
        id,
        ClientEntry {
            name,
            tty,
            pid: req.pid,
            session,
            ctx,
            size: clamp_size(req.size),
            pending_size: None,
            caps: terminal_features(&state.model, &req.term, req.caps),
            term: req.term,
            activity: now,
            activity_time: SystemTime::now(),
            decoder: Decoder::new(),
            keys: KeyState::new(),
            focused: true,
            mouse_prev: None,
            mouse_last: None,
            message: None,
            message_ignores_keys: false,
            status_rows: Vec::new(),
            status_at: 0,
            held: VecDeque::new(),
            held_bytes: 0,
            holds: 0,
            held_full: false,
            viewport: (0, 0),
            pan: None,
            suspended: false,
            suspend_tx,
            flags,
            mouse_modes: None,
            dirty: true,
            outbox,
            exit: Some(exit_tx),
        },
    );
    // A client that ignores size does not size the windows, unless it is
    // the session's only one.
    if !flags.ignore_size || !state.registry.owners.contains_key(&session) {
        state.registry.owners.insert(session, SizeOwner::new(id));
    }
    touch_session(&mut state.model, session);
    sync_attached(state);
    changed(state);
    if !req.extras.no_env
        && let Some(client_env) = state.clients.get(&id).map(|c| c.ctx.env.clone())
    {
        hooks::update_environment(state, session, &client_env);
    }
    // As tmux 3.7b (recorded): the panes it focuses, a session change, a
    // resize, then the attach.
    hooks::track_focus(state);
    hooks::client_session_changed(state, id);
    hooks::client_resized(state, id);
    // As tmux: only attach-session fires it, not new-session's attach.
    if !req_created {
        hooks::client_attached(state, id);
    }
    if state.binary.as_ref().is_some_and(|b| b.replaced()) {
        let text = status::escape_hashes(crate::server::build::STALE);
        display_message(
            state,
            id,
            &text,
            Some(Duration::from_secs(10)),
            false,
            render::Level::Warning,
        );
    }
    Ok(Accepted {
        id,
        output,
        exit,
        suspend,
    })
}

/// Whether an attach hello's tty can name its client: a path under
/// `/dev/`, no longer than `PATH_MAX`, with no control characters (it
/// reaches `list-clients`, the message log and scripts that parse them).
fn usable_tty(tty: &str) -> bool {
    tty.starts_with("/dev/")
        && tty.len() <= libc::PATH_MAX as usize
        && !tty.chars().any(char::is_control)
}

/// Everything attach refuses before it changes anything, in tmux's order:
/// no sessions, then the target (session, window, pane), then self-nesting
/// without `--nest`.
///
/// Self-nesting is two rules. In the tmux dialect it is tmux 3.7b's
/// `server_client_check_nested` (recorded): a client with `$TMUX` set
/// whose terminal `tty` is one of this server's panes' is refused, to any
/// session; any other client attaches, so a popup's `tmux attach -t` to
/// the popup's own session shows it nested in the popup (TP.10). grofe's
/// own rule stays TC.1's: from a pane, its own session is refused and
/// another is allowed.
fn check_target(
    model: &Model,
    target: &Target,
    ctx: &ClientCtx,
    nest: bool,
    tty: Option<&str>,
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
    if !nest && ctx.dialect == crate::protocol::Dialect::Tmux {
        let tmux_set = ctx.env.iter().any(|(k, v)| k == "TMUX" && !v.is_empty());
        let on_a_pane = tty.filter(|t| !t.is_empty()).is_some_and(|t| {
            model
                .panes
                .values()
                .any(|p| p.handle.tty_path() == Some(std::path::Path::new(t)))
        });
        if tmux_set && on_a_pane {
            return Err(crate::compat::TMUX_NESTED.to_string());
        }
        return Ok(session);
    }
    if !nest
        && let Some(pane) = ctx.pane
        && let Some(p) = model.panes.get(&pane)
        && model.has_link(session, p.window)
    {
        return Err(format!(
            "refusing to attach session {} from inside itself (use --nest to force)",
            model.sessions[&session].name
        ));
    }
    Ok(session)
}

/// The attach check (`protocol::ATTACH_CHECK`): `args` are the client's
/// terminal, then `attach-session`'s arguments, checked as the attach
/// hello will be, changing nothing.
pub fn attach_check(state: &State, ctx: &ClientCtx, args: &[String]) -> Result<String, String> {
    let (tty, args) = args
        .split_first()
        .map_or(("", args), |(t, a)| (t.as_str(), a));
    let argv: Vec<String> = std::iter::once("attach-session".to_string())
        .chain(args.iter().cloned())
        .collect();
    // In the request's dialect, so a tmux `attach -t =name` is exact (FE.1).
    let parsed = crate::compat::parse_split(ctx, &argv)?;
    match parsed.into_iter().next() {
        Some(cmd::Command::AttachSession(c)) => {
            check_target(&state.model, &c.target, ctx, c.nest, Some(tty)).map(|_| String::new())
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
    keep_zoom: bool,
) -> Result<SessionId, String> {
    let session = model.resolve_session(target, ctx)?;
    if let Target::Window(..) | Target::Pane(_) = target {
        let window = model.resolve_window(target, ctx)?;
        if let Target::Pane(_) = target {
            let pane = model.resolve_pane(target, ctx)?;
            crate::server::dispatch::pane::switch_active(model, window, pane, keep_zoom);
        }
        model.select_window_in(session, window);
    }
    Ok(session)
}

/// Client `id` now shows `session` (attach-session and switch-client):
/// the session it leaves becomes its last session, and it takes the new
/// session's size at once (switching is activity, spec D6).
pub(crate) fn show_session(state: &mut State, id: ClientId, session: SessionId) {
    show_session_with(state, id, session, true);
}

/// [`show_session`], leaving the session's environment as it is unless
/// `update_env` (`switch-client -E`, `attach-session -E`).
pub(crate) fn show_session_with(
    state: &mut State,
    id: ClientId,
    session: SessionId,
    update_env: bool,
) {
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
    let ignores_size = state.clients.get(&id).is_some_and(|c| c.flags.ignore_size);
    if !ignores_size || !state.registry.owners.contains_key(&session) {
        state.registry.owners.insert(session, SizeOwner::new(id));
    }
    touch_session(&mut state.model, session);
    sync_attached(state);
    if update_env && let Some(client_env) = state.clients.get(&id).map(|c| c.ctx.env.clone()) {
        hooks::update_environment(state, session, &client_env);
    }
    hooks::track_focus(state);
    hooks::client_session_changed(state, id);
    check_unattached(state);
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

/// Shows `text` as it is (styles in it are not drawn, as tmux shows a
/// command's error) on client `id`'s status line for `display-time`.
pub fn show_message(state: &mut State, id: ClientId, text: &str) {
    show_message_as(state, id, text, render::Level::Error);
}

/// [`show_message`] for a message that is not an error.
pub fn show_message_as(state: &mut State, id: ClientId, text: &str, level: render::Level) {
    log_message(state, id, text);
    set_message(state, id, status::escape_hashes(text), None, false, level);
}

/// The status-line message area (spec §4.10) for `display-message`
/// without `-p` (TC.9): `text` is already expanded, and `#[...]` styles in
/// it are drawn, as tmux draws a `display-message`. It replaces the first
/// status row (tmux's `message-line 0`) for `display-time`, or until a key
/// is pressed; the rows below stay.
/// `delay` overrides `display-time` (`display-message -d`).
pub fn display_message(
    state: &mut State,
    id: ClientId,
    text: &str,
    delay: Option<Duration>,
    ignore_keys: bool,
    level: render::Level,
) {
    log_message(state, id, text);
    set_message(state, id, text.to_string(), delay, ignore_keys, level);
}

/// How many messages `show-messages` keeps when `message-limit` is unset:
/// tmux's default.
const MESSAGE_LIMIT: usize = 1000;

/// Adds a status-line message for client `id` to the server's message
/// log, worded as tmux's `status_message_set` logs it.
fn log_message(state: &mut State, id: ClientId, text: &str) {
    let Some(name) = state.clients.get(&id).map(|c| c.name.clone()) else {
        return;
    };
    push_message(state, format!("{name} message: {text}"));
}

fn push_message(state: &mut State, line: String) {
    let limit = state
        .model
        .options
        .lookup("message-limit", &[])
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(MESSAGE_LIMIT);
    let log = &mut state.registry.messages;
    log.push_back((SystemTime::now(), line));
    while log.len() > limit {
        log.pop_front();
    }
}

/// Plans the log lines of a CLI request's commands, as tmux 3.7b's
/// `cmdq_add_message` logs each command as it fires (`<client> command:
/// <cmd>`, re-printed with its full name, recorded): one line per command
/// of `argv`, with how many native commands it parsed to, so
/// [`log_fired`] logs it as the first of those runs. `who` is the
/// requesting client. Empty when the request does not parse, as tmux
/// fires nothing then, or when its commands cannot be told apart.
// ponytail: only CLI requests are logged, and not the rest held behind a
// wait. tmux also logs the commands of bindings (`<client> key <key>:
// <cmd>`), hooks and sourced files, and names a CLI run from a terminal by
// its tty.
pub(crate) fn plan_command_log(
    state: &State,
    who: &str,
    ctx: &ClientCtx,
    argv: &[String],
    parsed: usize,
) -> VecDeque<(usize, Option<String>)> {
    let mut plan = VecDeque::new();
    if argv.first().is_some_and(|verb| verb.starts_with("__")) {
        return plan;
    }
    for cmd in cmd::parse::split_argv(argv) {
        // Parsed again only to count, so the front end notes nothing.
        let parsed_again =
            crate::compat::quietly(|| crate::server::dispatch::parse_split(state, ctx, &cmd));
        let Ok(count) = parsed_again.map(|c| c.len()) else {
            return VecDeque::new();
        };
        let printed = match ctx.dialect {
            crate::protocol::Dialect::Tmux => tmux_print(&cmd),
            crate::protocol::Dialect::Grofe => None,
        };
        let printed = printed.unwrap_or_else(|| {
            let words: Vec<String> = cmd
                .iter()
                .map(|w| crate::server::dispatch::options::escape_arg(w))
                .collect();
            words.join(" ")
        });
        plan.push_back((count, Some(format!("{who} command: {printed}"))));
    }
    if plan.iter().map(|(n, _)| n).sum::<usize>() != parsed {
        return VecDeque::new();
    }
    plan
}

/// `cmd` as `show-messages` prints a command it ran (tmux's `cmd_print`).
fn print_for_log(dialect: crate::protocol::Dialect, cmd: &[String]) -> String {
    let printed = match dialect {
        crate::protocol::Dialect::Tmux => tmux_print(cmd),
        crate::protocol::Dialect::Grofe => None,
    };
    printed.unwrap_or_else(|| {
        let words: Vec<String> = cmd
            .iter()
            .map(|w| crate::server::dispatch::options::escape_arg(w))
            .collect();
        words.join(" ")
    })
}

/// Logs `cmd`, a command a key's binding runs in a branch of its own
/// (`if-shell`), as run by that key.
pub(crate) fn log_key_command(state: &mut State, ctx: &ClientCtx, cmd: &[String]) {
    let Some(key) = state.event_key.clone() else {
        return;
    };
    let Some(who) = ctx
        .client
        .and_then(|id| state.clients.get(&id))
        .map(|c| c.name.clone())
    else {
        return;
    };
    let printed = print_for_log(ctx.dialect, cmd);
    push_message(state, format!("{who} key {key}: {printed}"));
}

/// Logs the planned line of the request command whose first native
/// command is about to run ([`plan_command_log`]), and any before it that
/// ran as no command at all.
pub(crate) fn log_fired(state: &mut State) {
    while let Some((left, line)) = state.command_log.front_mut() {
        let (left, line) = (std::mem::take(left), line.take());
        if let Some(line) = line {
            push_message(state, line);
        }
        if left > 1 {
            if let Some(front) = state.command_log.front_mut() {
                front.0 = left - 1;
            }
            return;
        }
        state.command_log.pop_front();
        if left == 1 {
            return;
        }
    }
}

/// A request's error, as tmux's `cmdq_error` logs it for a client.
pub(crate) fn log_error(state: &mut State, who: &str, err: &str) {
    push_message(state, format!("{who} message: {err}"));
}

/// A tmux command as tmux's `cmd_print` prints it: the full name, the
/// flags with no value together and sorted, then each flag with a value,
/// sorted, then the arguments, each quoted as `args_escape` quotes it.
/// `None` for a verb tmux does not have.
fn tmux_print(argv: &[String]) -> Option<String> {
    use crate::server::dispatch::options::escape_arg;
    let (name, args) = argv.split_first()?;
    let spec = crate::compat::tmux::resolve(name).ok()?;
    // `[-c working-directory]` takes a value, `[-dErx]` does not.
    let takes = |flag: char| {
        spec.usage.split("[-").skip(1).any(|part| {
            let mut chars = part.chars();
            chars.next() == Some(flag) && chars.next() == Some(' ')
        }) || (spec.name == "select-pane" && flag == 'P')
    };
    let mut bare: std::collections::BTreeMap<char, usize> = Default::default();
    let mut valued: std::collections::BTreeMap<char, Vec<&str>> = Default::default();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else {
            break;
        };
        i += 1;
        for (at, flag) in letters.char_indices() {
            if !takes(flag) {
                *bare.entry(flag).or_default() += 1;
                continue;
            }
            let rest = &letters[at + flag.len_utf8()..];
            let value = if rest.is_empty() {
                let next = args.get(i).map(String::as_str);
                i += 1;
                next.unwrap_or_default()
            } else {
                rest
            };
            valued.entry(flag).or_default().push(value);
            break;
        }
    }
    let mut out = spec.name.to_string();
    if !bare.is_empty() {
        out.push_str(" -");
        for (flag, n) in bare {
            out.extend(std::iter::repeat_n(flag, n));
        }
    }
    for (flag, values) in valued {
        for v in values {
            out.push_str(&format!(" -{flag} {}", escape_arg(v)));
        }
    }
    for word in args.get(i..).unwrap_or_default() {
        out.push(' ');
        out.push_str(&escape_arg(word));
    }
    Some(out)
}

/// The message log (TC.9's `show-messages`), oldest first.
pub(crate) fn messages(state: &State) -> impl DoubleEndedIterator<Item = &(SystemTime, String)> {
    state.registry.messages.iter()
}

/// tmux's `format_pretty_time` without seconds (`#{t/p:...}`): `HH:MM`
/// within a day, `%a%d` within the month or 28 days, `%d%b` within the
/// year, else `%b%y`.
pub(crate) fn pretty_time(t: SystemTime) -> String {
    let secs = |t: SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .map_or(0, |d| libc::time_t::try_from(d.as_secs()).unwrap_or(0))
    };
    let then = secs(t);
    let now = secs(SystemTime::now()).max(then);
    let age = now - then;
    let (tm, now_tm) = (local_tm(then), local_tm(now));
    let format = if age < 24 * 3600 {
        "%H:%M"
    } else if (tm.tm_year == now_tm.tm_year && tm.tm_mon == now_tm.tm_mon) || age < 28 * 24 * 3600 {
        "%a%d"
    } else if (tm.tm_year == now_tm.tm_year && tm.tm_mon < now_tm.tm_mon)
        || (tm.tm_year == now_tm.tm_year - 1 && tm.tm_mon > now_tm.tm_mon)
    {
        "%d%b"
    } else {
        "%b%y"
    };
    strftime(format, &tm)
}

fn local_tm(t: libc::time_t) -> libc::tm {
    // SAFETY: `localtime_r` writes into the zeroed `tm` we own (all-zero
    // is a valid `tm`).
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

fn strftime(format: &str, tm: &libc::tm) -> String {
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is writable for its length and `strftime` writes at
    // most that many bytes, returning how many (0 on overflow).
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Client `id` redraws (`refresh-client`): only its status line with
/// `status_only`, else everything, as if it had just attached.
pub(crate) fn refresh(state: &mut State, id: ClientId, status_only: bool) {
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    if !status_only {
        c.outbox.prev = None;
    }
    c.dirty = true;
    request_render(state, Instant::now());
}

/// Client `id`'s status rows as last drawn, and whether they are at the
/// top of its screen (TC.9's `choose-client` preview copies them).
pub(crate) fn drawn_status(state: &State, id: ClientId) -> Option<(&[StatusRow], bool)> {
    let c = state.clients.get(&id)?;
    Some((
        &c.status_rows,
        c.status_at == 0 && !c.status_rows.is_empty(),
    ))
}

/// `suspend-client` (TC.9): client `id` is asked to stop itself with
/// SIGTSTP, as tmux 3.7b's MSG_SUSPEND; nothing is drawn for it until it
/// resumes.
pub(crate) fn suspend(state: &mut State, id: ClientId) {
    if let Some(c) = state.clients.get_mut(&id)
        && !c.suspended
        && c.suspend_tx.try_send(()).is_ok()
    {
        c.suspended = true;
    }
}

/// A client named on the command line (`-c` or `-t` of the client
/// verbs), else the current client ([`current_client`]).
pub(crate) fn target_client(
    state: &State,
    ctx: &ClientCtx,
    name: Option<&str>,
) -> Result<ClientId, String> {
    match name {
        Some(name) => client_by_name(state, name),
        None => current_client(state, ctx).ok_or_else(|| "no current client".to_string()),
    }
}

/// One `wait-for` channel (TC.9, M36-R1): signalled with nobody waiting,
/// it stays woken until the next wait, as in tmux.
#[derive(Default)]
struct WaitChannel {
    woken: bool,
    waiters: Vec<oneshot::Sender<Result<String, Failure>>>,
    /// `wait-for -L` holds the channel.
    locked: bool,
    /// The `-L` callers behind the holder, oldest first.
    lockers: Vec<oneshot::Sender<Result<String, Failure>>>,
}

/// `wait-for -S`: every waiter on `channel` is released; with none, the
/// channel stays woken for the next wait.
pub(crate) fn signal_channel(state: &mut State, channel: &str) {
    prune_waits(state);
    let waits = &mut state.registry.waits;
    let wc = waits.entry(channel.to_string()).or_default();
    if wc.waiters.is_empty() {
        wc.woken = true;
        return;
    }
    for tx in wc.waiters.drain(..) {
        let _ = tx.send(Ok(String::new()));
    }
    if !wc.locked {
        waits.remove(channel);
    }
}

/// `wait-for -L`: takes `channel`'s lock, or waits behind whoever holds it
/// until `wait-for -U` hands it over.
pub(crate) fn lock_channel(
    state: &mut State,
    channel: &str,
    reply: impl FnOnce(&mut State) -> oneshot::Sender<Result<String, Failure>>,
) {
    prune_waits(state);
    if !state
        .registry
        .waits
        .get(channel)
        .is_some_and(|wc| wc.locked)
    {
        state
            .registry
            .waits
            .entry(channel.to_string())
            .or_default()
            .locked = true;
        return;
    }
    let tx = reply(state);
    if let Some(wc) = state.registry.waits.get_mut(channel) {
        wc.lockers.push(tx);
    }
}

/// `wait-for -U`: the lock passes to the first `-L` waiting for it, else
/// the channel is free. An unlocked channel is an error.
pub(crate) fn unlock_channel(state: &mut State, channel: &str) -> Result<(), String> {
    let waits = &mut state.registry.waits;
    let Some(wc) = waits.get_mut(channel).filter(|wc| wc.locked) else {
        return Err(format!("channel {channel} not locked"));
    };
    // A locker that hung up is skipped.
    while !wc.lockers.is_empty() {
        if wc.lockers.remove(0).send(Ok(String::new())).is_ok() {
            return Ok(());
        }
    }
    wc.locked = false;
    if !wc.woken && wc.waiters.is_empty() {
        waits.remove(channel);
    }
    Ok(())
}

/// `wait-for`: returns at once when `channel` was woken; otherwise
/// `reply` answers once it is signalled.
pub(crate) fn wait_channel(
    state: &mut State,
    channel: &str,
    reply: impl FnOnce(&mut State) -> oneshot::Sender<Result<String, Failure>>,
) {
    if let Some(wc) = state.registry.waits.get(channel)
        && wc.woken
    {
        // As tmux's `cmd_wait_for_remove`: a locked channel stays, woken.
        if !wc.locked {
            state.registry.waits.remove(channel);
        }
        return;
    }
    prune_waits(state);
    let tx = reply(state);
    let wc = state.registry.waits.entry(channel.to_string()).or_default();
    wc.waiters.push(tx);
}

/// Drops the waits of callers that hung up, and the channels they leave
/// empty and unwoken.
fn prune_waits(state: &mut State) {
    state.registry.waits.retain(|_, wc| {
        wc.waiters.retain(|tx| !tx.is_closed());
        wc.lockers.retain(|tx| !tx.is_closed());
        wc.woken || wc.locked || !wc.waiters.is_empty()
    });
}

fn set_message(
    state: &mut State,
    id: ClientId,
    text: String,
    delay: Option<Duration>,
    ignore_keys: bool,
    level: render::Level,
) {
    let now = Instant::now();
    let Some(session) = state.clients.get(&id).map(|c| c.session) else {
        return;
    };
    let time = delay.unwrap_or_else(|| display_time(&state.model, session));
    let until = (!time.is_zero()).then(|| now + time);
    if let Some(c) = state.clients.get_mut(&id) {
        c.message = Some((text, until, level));
        // As tmux: with a delay of 0 (until a key), `-N` does not change
        // whether keys take the message down.
        if until.is_some() {
            c.message_ignores_keys = ignore_keys;
        }
        c.dirty = true;
        request_render(state, now);
    }
}

/// The ends a message or prompt pill takes (`pane-frame-tab-ends`) while
/// `message-pill` is on for `chain`; `None` draws tmux's full-width bar.
pub(crate) fn pill_ends<'a>(model: &'a Model, chain: &[Owner]) -> Option<&'a str> {
    (model.options.lookup("message-pill", chain) != Some("off")).then(|| {
        model
            .options
            .lookup("pane-frame-tab-ends", chain)
            .unwrap_or("()")
    })
}

/// The session's `display-time`, or tmux's default; 0 means until a key.
fn display_time(model: &Model, session: SessionId) -> Duration {
    model
        .options
        .lookup("display-time", &[Owner::Session(session)])
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DISPLAY_TIME, Duration::from_millis)
}

/// What a click at client cell `(col, row)` hits on client `id`'s status
/// line, as last drawn: when it is on a status row, that row's index
/// among the status rows and the range under it, if any (TC.10 sets
/// `mouse_status_range` and `mouse_y` from them). Rows and ranges both
/// come from the last draw, so an option changed since then cannot move a
/// click onto another row's ranges.
pub fn status_at(
    state: &State,
    id: ClientId,
    col: u16,
    row: u16,
) -> Option<(u16, Option<RangeKind>)> {
    let c = state.clients.get(&id)?;
    let line = row.checked_sub(c.status_at)?;
    let drawn = c.status_rows.get(usize::from(line))?;
    Some((line, drawn.range_at(col).map(|r| r.kind.clone())))
}

/// Client `id`'s status line as last drawn, for a menu or popup placed by
/// `-x W` / `-y W` / `-y S` (tmux's `cmd_display_menu_get_pos`).
pub struct StatusSpot {
    /// How many rows the status line takes.
    pub lines: u16,
    /// `status-position top`.
    pub top: bool,
    /// Each window range: the status row it is on (0 is the first), its
    /// first column, and the window index it names, in drawing order.
    pub windows: Vec<(u16, u16, u32)>,
}

/// Client `id`'s [`StatusSpot`]; `None` with the status line off.
pub fn status_spot(state: &State, id: ClientId) -> Option<StatusSpot> {
    let c = state.clients.get(&id)?;
    let geom = status_geometry(&state.model, c.session, c.size.1);
    if geom.lines == 0 {
        return None;
    }
    let windows = c
        .status_rows
        .iter()
        .enumerate()
        .flat_map(|(line, row)| {
            row.ranges.iter().filter_map(move |r| match r.kind {
                RangeKind::Window(index) => Some((line as u16, r.start, index)),
                _ => None,
            })
        })
        .collect();
    Some(StatusSpot {
        lines: geom.lines,
        top: geom.top,
        windows,
    })
}

/// The range under a click at client cell `(col, row)` on client `id`'s
/// status line as last drawn ([`status_at`]).
pub fn status_range_at(state: &State, id: ClientId, col: u16, row: u16) -> Option<RangeKind> {
    status_at(state, id, col, row).and_then(|(_, range)| range)
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
            // A suspended client sends its size when it resumes: its
            // terminal is its own again, so everything is drawn anew.
            if c.suspended {
                c.suspended = false;
                c.outbox.prev = None;
                c.mouse_modes = None;
                c.dirty = true;
                sync_mouse_modes(state);
                let Some(c) = state.clients.get_mut(&id) else {
                    return;
                };
                c.pending_size = Some(clamp_size((cols, rows)));
                request_render(state, Instant::now());
                hooks::client_resized(state, id);
                return;
            }
            c.pending_size = Some(clamp_size((cols, rows)));
            request_render(state, Instant::now());
            hooks::client_resized(state, id);
        }
        Frame::Detach => detach(state, id),
        // Server-to-client kinds: ignored if a client sends them.
        Frame::Output(_) | Frame::Exit(_) | Frame::Suspend => {}
    }
}

/// Client `id`'s connection ended.
pub fn gone(state: &mut State, id: ClientId) {
    remove(state, id, None);
}

/// Detaches client `id` with tmux's message.
pub fn detach(state: &mut State, id: ClientId) {
    detach_with(state, id, false);
}

/// [`detach`]; with `kill_parent` (tmux's `MSG_DETACHKILL`) the parent of
/// the client's process, its shell, gets SIGHUP as the client goes, which
/// tmux's client does by itself.
pub fn detach_with(state: &mut State, id: ClientId, kill_parent: bool) {
    let reason = state
        .clients
        .get(&id)
        .map(|c| detached_from(&state.model, c.session));
    let parent = kill_parent
        .then(|| {
            let pid = state.clients.get(&id)?.pid?;
            crate::server::pane::procinfo::parent(pid)
        })
        .flatten();
    remove(state, id, reason);
    if let Some(parent) = parent.filter(|p| *p > 1) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(parent),
            nix::sys::signal::Signal::SIGHUP,
        );
    }
}

fn detached_from(model: &Model, session: SessionId) -> String {
    match model.sessions.get(&session) {
        Some(s) => format!("detached (from session {})", s.name),
        None => "detached".to_string(),
    }
}

/// Detaches every client on `session` except `keep`, hanging up their
/// parents with `kill_parent`.
fn detach_session_with(
    state: &mut State,
    session: SessionId,
    keep: Option<ClientId>,
    kill_parent: bool,
) {
    let ids: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(id, c)| c.session == session && Some(**id) != keep)
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        detach_with(state, id, kill_parent);
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
    crate::server::mouse::client_gone(state, id);
    if let (Some(tx), Some(reason)) = (c.exit.take(), reason) {
        let _ = tx.send(reason);
    }
    state.model.client_last_session.remove(&id);
    sync_attached(state);
    leave_session(state, id, c.session);
    graphics::on_clients_changed(state);
    hooks::track_focus(state);
    hooks::client_detached(state, &c.name, c.session);
    check_unattached(state);
}

/// tmux's `server_check_unattached`: a session no client shows goes when
/// its `destroy-unattached` says so (`on`; `keep-last` keeps the last of a
/// group and every ungrouped one; `keep-group` keeps the last of a group).
/// Run where a session loses or gains a client, and when the option is set.
pub fn check_unattached(state: &mut State) {
    use crate::server::dispatch::options::Owner;
    let unattached: Vec<SessionId> = state
        .model
        .sessions
        .values()
        .filter(|s| s.attached == 0)
        .map(|s| s.id)
        .collect();
    for id in unattached {
        let mode = state
            .model
            .options
            .lookup("destroy-unattached", &[Owner::Session(id)]);
        let grouped = state.model.group_of(id).map(|g| g.members.len());
        let destroy = match mode {
            Some("on") => true,
            Some("keep-last") => grouped.is_some_and(|n| n > 1),
            Some("keep-group") => grouped.is_none_or(|n| n > 1),
            _ => false,
        };
        if destroy {
            let removed = state.model.remove_session(id);
            state.events.panes_killed(removed);
        }
    }
    if state.model.closed() {
        state.exit = true;
    }
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
    crate::server::popup::shutdown(state);
    for (_, mut c) in std::mem::take(&mut state.clients) {
        if let Some(tx) = c.exit.take() {
            let _ = tx.send(reason.to_string());
        }
    }
}

/// The session a client of the destroyed session `name` moves to, by its
/// `detach-on-destroy` `mode` (tmux's `server_destroy_session`): `off`, the
/// most recently active other session; `no-detached`, the most recent one
/// no client shows; `previous` and `next`, the neighbour in name order
/// (tmux 3.7b's crash on a NULL sort order is not copied). `on` detaches.
fn successor(state: &State, name: &str, mode: &str) -> Option<SessionId> {
    let mut sessions: Vec<_> = state.model.sessions.values().collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    let newest = |keep: &dyn Fn(&crate::server::model::Session) -> bool| {
        sessions
            .iter()
            .filter(|s| keep(s))
            .fold(None::<&&crate::server::model::Session>, |best, s| {
                best.filter(|b| s.activity <= b.activity).or(Some(s))
            })
            .map(|s| s.id)
    };
    // Where the destroyed session sat in name order.
    let before = sessions.iter().filter(|s| s.name.as_str() < name).count();
    match mode {
        "off" => newest(&|_| true),
        "no-detached" => newest(&|s| !state.clients.values().any(|c| c.session == s.id)),
        "previous" => before
            .checked_sub(1)
            .or(sessions.len().checked_sub(1))
            .map(|i| sessions[i].id),
        "next" => sessions.get(before).or(sessions.first()).map(|s| s.id),
        _ => None,
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
    let destroyed = std::mem::take(&mut state.model.destroyed_sessions);
    // One successor per destroyed session, chosen before any client moves,
    // as tmux picks `s_new` once: a second client must not see the first
    // one's session as attached.
    let successors: BTreeMap<SessionId, Option<SessionId>> = destroyed
        .iter()
        .map(|(id, (name, mode))| (*id, successor(state, name, mode)))
        .collect();
    for id in orphans {
        let next = state
            .clients
            .get(&id)
            .and_then(|c| successors.get(&c.session).copied().flatten());
        // `no-detach-on-destroy`: where `on` and `no-detached` would detach
        // the client, it goes to the session most recently used instead.
        let next = next.or_else(|| {
            let c = state
                .clients
                .get(&id)
                .filter(|c| c.flags.no_detach_on_destroy)?;
            let (name, mode) = destroyed.get(&c.session)?;
            matches!(mode.as_str(), "on" | "no-detached")
                .then(|| successor(state, name, "off"))
                .flatten()
        });
        match next {
            Some(session) => {
                show_session(state, id, session);
                state.model.client_last_session.remove(&id);
            }
            None => remove(state, id, Some("exited".to_string())),
        }
    }
    let live = &state.model.sessions;
    state.registry.owners.retain(|s, _| live.contains_key(s));
    // A killed pane's copy mode goes with it (M5).
    let panes = &state.model.panes;
    state.copy.retain(|p| panes.contains_key(&p));
    sync_pane_status(&mut state.model);
    sync_pane_defaults(&state.model);
    apply_sizes(state);
    graphics::on_clients_changed(state);
    sync_mouse_modes(state);
    mark_all_dirty(state);
    arm_names(state, Instant::now());
}

/// The outer terminal's mouse reporting on and off, and on with every
/// motion (mode 1003) for `focus-follows-mouse` and a menu that takes the
/// mouse (tmux's `tty_update_mode`).
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";
const MOUSE_MOTION_ON: &[u8] = b"\x1b[?1003h";
const MOUSE_MOTION_OFF: &[u8] = b"\x1b[?1003l";

/// Writes each client's mouse modes where they differ from what they were
/// last sent (R2): so on attach, and after any `set`, `reload`, switch or
/// menu that changes them. The client's exit reset turns them off again.
pub(crate) fn sync_mouse_modes(state: &mut State) {
    let model = &state.model;
    let levels: Vec<(ClientId, u8)> = state
        .clients
        .iter()
        .map(|(id, c)| {
            let owners = [Owner::Session(c.session)];
            let on = |name: &str| {
                model
                    .options
                    .lookup(name, &owners)
                    .is_some_and(|v| v == "on")
            };
            let level = if crate::server::menu::wants_mouse(state, *id)
                || on("mouse") && on("focus-follows-mouse")
            {
                2
            } else {
                u8::from(on("mouse"))
            };
            (*id, level)
        })
        .collect();
    for (id, level) in levels {
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        if c.mouse_modes == Some(level) {
            continue;
        }
        let before = c.mouse_modes;
        c.mouse_modes = Some(level);
        if level == 2 {
            c.outbox.queue_raw(MOUSE_ON);
            c.outbox.queue_raw(MOUSE_MOTION_ON);
            continue;
        }
        if before == Some(2) {
            c.outbox.queue_raw(MOUSE_MOTION_OFF);
        }
        c.outbox
            .queue_raw(if level == 1 { MOUSE_ON } else { MOUSE_OFF });
    }
}

/// Queues `bytes` for client `id` ahead of its next frame (inside the same
/// `Output` frame, before the synchronized-output begin), and schedules
/// that frame.
pub fn queue_raw(state: &mut State, id: ClientId, bytes: &[u8]) {
    try_queue_raw(state, id, bytes);
}

/// [`queue_raw`], false if the client's held raw output was too full to
/// take `bytes`, which were dropped.
pub fn try_queue_raw(state: &mut State, id: ClientId, bytes: &[u8]) -> bool {
    let queued = state
        .clients
        .get_mut(&id)
        .is_some_and(|c| c.outbox.queue_raw(bytes));
    mark_dirty(state, id);
    queued
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
    crate::server::alerts::output(state, &dirty);
    if !dirty.is_empty() {
        arm_names(state, Instant::now());
    }
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
            c.message.as_ref().and_then(|(_, until, _)| *until),
        ]
    });
    [
        r.render_at,
        r.status_at,
        r.names_at,
        dispatch::lock::deadline(state),
    ]
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
        crate::server::popup::clients_resized(state);
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
        if c.message
            .as_ref()
            .is_some_and(|(_, until, _)| until.is_some_and(|u| u <= now))
        {
            c.message = None;
            c.dirty = true;
            request_render(state, now);
        }
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        if c.decoder.deadline(escape).is_some_and(|d| d <= now) {
            for input in c.decoder.flush(now, escape) {
                deliver(state, id, input, now);
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
    dispatch::lock::tick(state, now);
    arm_status_clock(state, now);
}

/// Client `id` has just been active: a client whose lock command exited.
pub(crate) fn touch(state: &mut State, id: ClientId, now: Instant) {
    if let Some(c) = state.clients.get_mut(&id) {
        c.activity = now;
        c.activity_time = SystemTime::now();
    }
}

/// Schedules one `automatic-rename` check. As tmux's `check_window_name`
/// (names.c), which only runs for a window whose active pane changed
/// (`PANE_CHANGED`: pane output, a pane or window switch, an option
/// change) and at most once per `NAME_INTERVAL`: the timer is armed by
/// those events and is not re-armed by the check, so an idle server never
/// wakes for it. tmux runs it from `server_client_loop` for every window,
/// attached or not, so a detached session is renamed too.
fn arm_names(state: &mut State, now: Instant) {
    if state.registry.names_at.is_none() {
        state.registry.names_at = Some(now + NAME_INTERVAL);
    }
}

/// Schedules the next status redraw for its clock, every `status-interval`
/// seconds (the shortest among the attached sessions; 0 turns it off). A
/// session whose status line provably cannot change with time (see
/// [`status_is_static`]) gets no timer: every other change redraws it
/// through its own event, and a `set` of the formats redraws and so
/// re-evaluates this at the end of the next tick.
fn arm_status_clock(state: &mut State, now: Instant) {
    if state.registry.status_at.is_some() {
        return;
    }
    let model = &state.model;
    state.registry.status_at = state
        .clients
        .values()
        .filter(|c| status_lines(model, c.session) > 0)
        .filter(|c| !status_is_static(model, c.session))
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

/// Variables a status format may use and still be redrawn only by events
/// (a rename, a window change, pane output), never by the passage of time.
const STATIC_STATUS_VARS: &[&str] = &[
    "session_name",
    "session_id",
    "session_windows",
    "window_index",
    "window_id",
    "window_name",
    "window_flags",
    "window_active",
    "window_panes",
    "host",
    "host_short",
];

/// Whether format `text` expands to the same thing at any time: no
/// strftime `%`, no `#()`, and every `#{}` word a [`STATIC_STATUS_VARS`]
/// name or a number. Anything else (`#{t:...}`, `#{@user}`, loops, other
/// variables) may depend on time or on a process, so it is not.
fn format_is_static(text: &str) -> bool {
    if text.contains('%') || text.contains("#(") {
        return false;
    }
    let mut rest = text;
    while let Some(at) = rest.find("#{") {
        let mut depth = 1;
        let body = &rest[at + 2..];
        let mut end = body.len();
        for (i, ch) in body.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = i;
                break;
            }
        }
        let words = body[..end]
            .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@')))
            .filter(|w| !w.is_empty());
        for w in words {
            if !STATIC_STATUS_VARS.contains(&w) && !w.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
        }
        rest = &body[end..];
    }
    true
}

/// Whether session `session`'s status line cannot change with time: every
/// format that draws it is [`format_is_static`]. When unsure, false (the
/// clock keeps ticking).
fn status_is_static(model: &Model, session: SessionId) -> bool {
    const SESSION: &[&str] = &[
        "status-left",
        "status-right",
        "status-style",
        "status-left-style",
        "status-right-style",
    ];
    const WINDOW: &[&str] = &[
        "window-status-format",
        "window-status-current-format",
        "window-status-separator",
        "window-status-style",
        "window-status-current-style",
        "window-status-last-style",
        "window-status-bell-style",
    ];
    let own = [Owner::Session(session)];
    let session_ok = SESSION
        .iter()
        .all(|n| model.options.lookup(n, &own).is_none_or(format_is_static));
    // Only the rows the status line draws; the defaults of the others
    // (the `P:`/`S:` rows) use loops and are not static.
    let rows = status_lines(model, session);
    let formats_ok = model
        .options
        .lookup_list("status-format", &own)
        .is_none_or(|f| {
            f.iter()
                .filter(|(i, _)| (**i as usize) < usize::from(rows))
                .all(|(_, t)| format_is_static(t))
        });
    let windows_ok = |chain: &[Owner]| {
        WINDOW
            .iter()
            .all(|n| model.options.lookup(n, chain).is_none_or(format_is_static))
    };
    let listed = model
        .sessions
        .get(&session)
        .map_or(&[][..], |s| &s.windows[..]);
    session_ok
        && formats_ok
        && windows_ok(&own)
        && listed
            .iter()
            .filter_map(|id| model.windows.get(id))
            .all(|w| windows_ok(&options::chain(model, w.active)))
}

fn render_dirty(state: &mut State) {
    let ids: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(_, c)| c.dirty && !c.suspended && c.outbox.ready())
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
        let fits = window_fits(&state.model, c);
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        c.dirty = false;
        if let Some((screen, viewport, rows, status_at)) = composed {
            c.viewport = viewport;
            c.pan = match (c.pan, fits) {
                (Some((window, ..)), Some((current, false))) if window == current => {
                    Some((window, viewport.0, viewport.1))
                }
                _ => None,
            };
            c.status_rows = rows;
            c.status_at = status_at;
            let caps = c.caps;
            // A dropped frame keeps the client dirty, so it is redrawn once
            // its outbox has room even if nothing else changes.
            c.dirty = !c.outbox.offer(screen, &caps);
        }
    }
}

/// `caps` with what the `terminal-features` option adds for a client whose
/// `TERM` is `term`, as tmux reads it: entries `pattern:feature:...`, the
/// pattern a `fnmatch` glob of the terminal name. Only `sixel` is read (G3);
/// the probe cannot ask for it on a terminal that lists it nowhere else.
fn terminal_features(model: &Model, term: &str, mut caps: Caps) -> Caps {
    let features = model.options.lookup_list("terminal-features", &[]);
    for entry in features.into_iter().flat_map(|f| f.values()) {
        let mut parts = entry.split(':');
        let matches = parts.next().is_some_and(|pattern| glob(pattern, term));
        if matches && parts.any(|feature| feature == "sixel") {
            caps.sixel = true;
        }
    }
    caps
}

/// `fnmatch` with `*` and `?` only.
fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some((b'?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// Client `c`'s current window, and whether it fits the client (then
/// there is nothing to pan).
fn window_fits(model: &Model, c: &ClientEntry) -> Option<(WindowId, bool)> {
    let w = model.windows.get(&model.sessions.get(&c.session)?.active)?;
    let (cols, rows) = visible_size(model, c);
    Some((w.id, w.cols <= cols && w.rows <= rows))
}

/// The part of client `c` that shows the window: its size less the
/// status rows.
fn visible_size(model: &Model, c: &ClientEntry) -> (u16, u16) {
    let lines = status_geometry(model, c.session, c.size.1).lines;
    (c.size.0, c.size.1.saturating_sub(lines))
}

/// `refresh-client -c/-L/-R/-U/-D` (TC.9): moves the visible part of a
/// window larger than client `id` by `by` cells, or with `Reset` goes
/// back to following the cursor, as tmux 3.7b's `cmd_refresh_client_exec`.
pub(crate) fn pan(state: &mut State, id: ClientId, pan: cmd::Pan, by: u16) {
    let model = &state.model;
    let Some(c) = state.clients.get(&id) else {
        return;
    };
    let Some(w) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
    else {
        return;
    };
    let (cols, rows) = visible_size(model, c);
    let (wid, wcols, wrows) = (w.id, w.cols, w.rows);
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    if pan == cmd::Pan::Reset {
        // tmux 3.7b recomputes the offset from the cursor, which is 0 while
        // the cursor is within the first screenful (recorded).
        // ponytail: past that tmux centres the cursor; `follow` scrolls
        // only as far as it must.
        c.pan = None;
        c.viewport = (0, 0);
    } else {
        let (_, mut x, mut y) = match c.pan {
            Some(p) if p.0 == wid => p,
            _ => (wid, c.viewport.0, c.viewport.1),
        };
        match pan {
            cmd::Pan::Left => x = x.saturating_sub(by),
            cmd::Pan::Right => x = x.saturating_add(by).min(wcols.saturating_sub(cols)),
            cmd::Pan::Up => y = y.saturating_sub(by),
            cmd::Pan::Down => y = y.saturating_add(by).min(wrows.saturating_sub(rows)),
            cmd::Pan::Reset => {}
        }
        c.pan = Some((wid, x, y));
    }
    c.dirty = true;
    request_render(state, Instant::now());
}

/// How many status rows `session` shows: `status` is `off`, `on` or 2 to
/// 5 (spec §4.10).
fn status_lines(model: &Model, session: SessionId) -> u16 {
    match model.options.lookup("status", &[Owner::Session(session)]) {
        Some("off") | None => 0,
        Some("on") => 1,
        Some(n) => n.parse().unwrap_or(1),
    }
}

/// Where a client's status rows are, for a client `rows` tall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusGeometry {
    /// How many rows the status line takes (never more than the client).
    pub lines: u16,
    /// `status-position top`: the rows are above the window.
    pub top: bool,
    /// The client row of the first status row, where a message or prompt
    /// goes (tmux's `message-line 0`).
    first_row: u16,
    /// The client row the window's top row is drawn on.
    window_top: u16,
}

pub(crate) fn status_geometry(model: &Model, session: SessionId, rows: u16) -> StatusGeometry {
    let lines = status_lines(model, session).min(rows);
    let top = model
        .options
        .lookup("status-position", &[Owner::Session(session)])
        == Some("top");
    StatusGeometry {
        lines,
        top,
        first_row: if top { 0 } else { rows - lines },
        window_top: if top { lines } else { 0 },
    }
}

/// The window size a client of `session` with `size` wants: its terminal
/// less the status rows, never below 1x1.
fn window_size(model: &Model, session: SessionId, (cols, rows): (u16, u16)) -> (u16, u16) {
    let status = status_lines(model, session);
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
    let sizes: Vec<(SessionId, (u16, u16), Instant)> = state
        .registry
        .owners
        .iter()
        .filter_map(|(s, o)| {
            let c = state.clients.get(&o.client)?;
            Some((*s, window_size(&state.model, *s, c.size), c.activity))
        })
        .collect();
    for &(session, size, activity) in &sizes {
        // A window shown to several sessions with a client follows the
        // client that was active last (`window-size latest`), so two
        // clients of different sizes do not make it flap.
        let newer =
            |other: &&(SessionId, (u16, u16), Instant)| (other.2, other.0) > (activity, session);
        let yielded: Vec<WindowId> = state
            .model
            .sessions
            .get(&session)
            .map(|s| s.windows.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|w| {
                sizes
                    .iter()
                    .filter(|o| newer(o))
                    .any(|o| state.model.has_link(o.0, *w))
            })
            .collect();
        resize_session(&mut state.model, session, size, &yielded);
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
fn resize_session(
    model: &mut Model,
    session: SessionId,
    (cols, rows): (u16, u16),
    yielded: &[WindowId],
) {
    let Some(s) = model.sessions.get_mut(&session) else {
        return;
    };
    s.cols = cols;
    s.rows = rows;
    for id in s.windows.clone() {
        if yielded.contains(&id) {
            continue;
        }
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
        model.relayout(id);
        fit_zoomed(model, id);
    }
}

/// A zoomed window's active pane fills the window, less the row a border
/// status takes.
fn fit_zoomed(model: &mut Model, window: WindowId) {
    let Some(w) = model.windows.get(&window).filter(|w| w.zoomed) else {
        return;
    };
    let (active, zoom) = (w.active, w.zoom_rect());
    if let Some(p) = model.panes.get_mut(&active) {
        p.cols = zoom.cols;
        p.rows = zoom.rows;
        if let Err(e) = p.handle.resize(zoom.cols, zoom.rows) {
            eprintln!("grofe server: resizing {active}: {e}");
        }
    }
}

/// Gives every window the `pane-border-status` its options now say, laying
/// it out again where that changed: the panes along its top or bottom edge
/// give up a row to their title.
fn sync_pane_status(model: &mut Model) {
    let wanted: Vec<(WindowId, PaneStatus)> = model
        .windows
        .values()
        .filter_map(|w| {
            let chain = options::chain(model, w.active);
            let value = model.options.lookup("pane-border-status", &chain)?;
            Some((w.id, PaneStatus::parse(value)))
        })
        .filter(|(id, status)| model.windows[id].pane_status != *status)
        .collect();
    for (id, status) in wanted {
        if let Some(w) = model.windows.get_mut(&id) {
            w.pane_status = status;
        }
        model.relayout(id);
        fit_zoomed(model, id);
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
pub(crate) fn visible_panes(w: &crate::server::model::Window) -> Vec<(PaneId, Rect)> {
    w.pane_rects()
}

/// Where things are on client `c`'s screen when drawn with `viewport`.
fn overlay_ctx_at(
    model: &Model,
    id: ClientId,
    c: &ClientEntry,
    viewport: (u16, u16),
) -> OverlayCtx {
    let (cols, rows) = c.size;
    let geom = status_geometry(model, c.session, rows);
    let win_rows = rows - geom.lines;
    let top = u32::from(geom.window_top);
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
            let upper = u32::from(r.y).max(vy);
            let right = (u32::from(r.x) + u32::from(r.cols)).min(vx + u32::from(cols));
            let bottom = (u32::from(r.y) + u32::from(r.rows)).min(vy + u32::from(win_rows));
            (left < right && upper < bottom).then(|| {
                let rect = Rect {
                    x: (left - vx) as u16,
                    y: (upper - vy + top) as u16,
                    cols: (right - left) as u16,
                    rows: (bottom - upper) as u16,
                };
                (pane, rect)
            })
        })
        .collect();
    OverlayCtx {
        client: id,
        viewport,
        status_row: (geom.lines > 0).then_some(geom.first_row),
        window_top: geom.window_top,
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

/// What a pane's default-colour cells show: `window-style`'s colours, and
/// for the window's active pane `window-active-style`'s where it sets one
/// (tmux's `tty_default_colours`).
fn window_style_colours<'a>(
    opt: &dyn Fn(&str) -> &'a str,
    vars: &dyn FormatVars,
    is_active: bool,
) -> (Color, Color) {
    let plain = status::base_style(opt("window-style"), vars);
    let active = status::base_style(opt("window-active-style"), vars);
    let fg = if is_active && active.fg != Color::Named(NamedColor::Foreground) {
        active.fg
    } else {
        plain.fg
    };
    let bg = if is_active && active.bg != Color::Named(NamedColor::Background) {
        active.bg
    } else {
        plain.bg
    };
    (fg, bg)
}

/// Tells every pane the colours its `window-style` gives it, which its
/// OSC 10 and OSC 11 queries are answered with.
fn sync_pane_defaults(model: &Model) {
    for w in model.windows.values() {
        for id in w.pane_ids() {
            let Some(pane) = model.panes.get(&id) else {
                continue;
            };
            let (fg, bg) = pane_defaults(model, id);
            pane.handle
                .set_default_colours(colour_rgb(fg), colour_rgb(bg));
        }
    }
}

struct NoVars;

impl FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

/// Hands a pane that was just spawned the colours its window and session
/// give it, before anything can be read from it: a program that asks for
/// OSC 10 or 11 at once is answered with the style, not the fixed palette.
/// The window may not be in the model yet, so the styles are read as the
/// window's active pane would see them, without formats.
pub(crate) fn seed_defaults(
    model: &Model,
    (session, window): (SessionId, WindowId),
    handle: &crate::server::pane::PaneHandle,
) {
    let chain = [Owner::Window(window), Owner::Session(session)];
    let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
    let (fg, bg) = window_style_colours(&opt, &NoVars, true);
    handle.set_default_colours(colour_rgb(fg), colour_rgb(bg));
}

/// The colours the default-colour cells of `pane` show (`window-style`,
/// `window-active-style`), the terminal's own where they set none.
pub(crate) fn pane_defaults(model: &Model, pane: PaneId) -> (Color, Color) {
    let chain = options::chain(model, pane);
    let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
    let active = model
        .panes
        .get(&pane)
        .and_then(|p| model.windows.get(&p.window))
        .is_some_and(|w| w.active == pane);
    window_style_colours(&opt, &FormatCtx::new(model, pane), active)
}

/// `colour` as RGB (tmux's `colour_force_rgb`); `None` for the terminal's
/// own default.
fn colour_rgb(colour: Color) -> Option<Rgb> {
    let index = match colour {
        Color::Spec(rgb) => return Some(rgb),
        Color::Indexed(n) => usize::from(n),
        Color::Named(name) => {
            let n = name as usize;
            if n >= 16 {
                return None;
            }
            n
        }
    };
    const ANSI: [u32; 16] = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let v = match index {
        0..16 => ANSI[index],
        16..232 => {
            const LEVELS: [u32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let i = index - 16;
            LEVELS[i / 36] << 16 | LEVELS[i / 6 % 6] << 8 | LEVELS[i % 6]
        }
        _ => {
            let g = 8 + 10 * (index as u32 - 232);
            g << 16 | g << 8 | g
        }
    };
    Some(Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    })
}

/// How `w`'s visible `panes` look to client `c`: the colours their
/// default-colour cells take (`window-style`, and `window-active-style` for
/// the window's active pane where it sets one), their border styles and
/// titles (`pane-border-format`), and the window's border options.
fn pane_looks(
    state: &State,
    c: &ClientEntry,
    w: &crate::server::model::Window,
    panes: &[PaneId],
) -> render::PaneLooks {
    let model = &state.model;
    let wchain = options::chain(model, w.active);
    let wopt = |name: &str| model.options.lookup(name, &wchain).unwrap_or("");
    let mut looks = render::PaneLooks {
        status: PaneStatus::parse(wopt("pane-border-status")),
        lines: PaneLines::parse(wopt("pane-border-lines")),
        indicators: Indicators::parse(wopt("pane-border-indicators")),
        // `fill-character`: its first character, if one cell wide.
        fill: wopt("fill-character")
            .chars()
            .next()
            .filter(|c| unicode_width::UnicodeWidthChar::width(*c) == Some(1)),
        ..Default::default()
    };
    // FR.2: the frames, their title and lines, and the drop preview.
    render::apply_frames(model, w, &mut looks);
    looks.lifted = crate::server::mouse::lifted(state, w.id).map(|(p, _)| p);
    let titled = looks.status != PaneStatus::Off || looks.frame_title;
    // grofe identity's framed titles end in a control tab while the mouse
    // is on; tmux identity keeps tmux's stock title, buttons and all.
    let tabbed = model.identity != crate::compat::Identity::Tmux
        && !looks.frames.is_empty()
        && looks.frame_title
        && wopt("mouse") == "on";
    let order = w.pane_ids();
    let base_index = model.pane_base_index(w.id) as usize;
    let now = local_now();
    for id in panes {
        let chain = options::chain(model, *id);
        let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
        let client_vars = ClientVars::new(model, c, Some(FormatCtx::new(model, *id)));
        let vars = options::ServerVars::new(state, *id, &client_vars);
        let (fg, bg) = window_style_colours(&opt, &vars, *id == w.active);
        let format = match opt("pane-border-format") {
            // The tab takes the place of the default format's buttons.
            f if tabbed && f == options::PANE_BORDER_FORMAT => {
                options::frame_title_format(opt("pane-frame-tab-ends"))
            }
            f => f.to_string(),
        };
        let title = if !titled {
            String::new()
        } else {
            status::expand_time(&format, &vars, &now)
        };
        let tab = tabbed.then(|| {
            render::frame_tab(
                opt("pane-frame-tab-ends"),
                w.zoomed,
                w.floating(*id).is_some(),
            )
        });
        looks.panes.insert(
            *id,
            render::PaneLook {
                fg,
                bg,
                border: status::base_style(opt("pane-border-style"), &vars),
                active_border: status::base_style(opt("pane-active-border-style"), &vars),
                title,
                tab,
                index: order.iter().position(|p| p == id).unwrap_or(0) + base_index,
            },
        );
    }
    looks
}

/// The `#[range=control|N]` at window column `x` of the top edge of
/// `pane`'s frame, which runs from column `x0` to `x1` (its corners), in
/// client `id`'s framed window: grofe identity's control tab, or a control
/// of the pane's `pane-border-format` (tmux's default `[z]` and `[x]`),
/// laid out as [`render::frame_title`] draws them.
pub(crate) fn frame_title_control(
    state: &State,
    id: ClientId,
    pane: PaneId,
    (x0, x1): (i32, i32),
    x: i32,
) -> Option<u8> {
    let c = state.clients.get(&id)?;
    let model = &state.model;
    let w = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))?;
    let look = pane_looks(state, c, w, &[pane]);
    if !look.frame_title {
        return None;
    }
    let l = look.panes.get(&pane)?;
    let plain = status::Style::default();
    let edge = render::frame_title(
        (x0, x1),
        i32::from(w.cols),
        &l.title,
        l.tab.as_deref(),
        plain,
        '─',
    );
    match edge.range_at(x)?.kind {
        status::RangeKind::Control(n) => Some(n as u8),
        _ => None,
    }
}

/// The border titles client `id`'s window shows, as drawn: where each
/// pane's title is and what its `#[range=...]` parts cover, for the mouse.
pub(crate) fn border_titles(state: &State, id: ClientId) -> Vec<render::Title> {
    let Some(c) = state.clients.get(&id) else {
        return Vec::new();
    };
    let model = &state.model;
    let Some(w) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
    else {
        return Vec::new();
    };
    if w.pane_status == PaneStatus::Off {
        return Vec::new();
    }
    let rects = visible_panes(w);
    let ids: Vec<PaneId> = rects.iter().map(|(id, _)| *id).collect();
    let look = pane_looks(state, c, w, &ids);
    let plain = status::Style::default();
    render::border_titles(
        &look,
        (i32::from(w.cols), i32::from(w.rows)),
        &rects,
        w.active,
        (plain, plain),
    )
}

/// A composed frame: the screen, its viewport, its status rows and the
/// client row the first of them is on.
type Composed = (Screen, (u16, u16), Vec<StatusRow>, u16);

/// Composes client `c`'s next screen, the viewport it was drawn with, its
/// status rows and the client row the first of them is on: the window,
/// the status rows above or below it (`status-position`), then every
/// overlay (lowest priority first), then the wide-pair fix-up.
fn compose_client(state: &State, id: ClientId, c: &ClientEntry) -> Option<Composed> {
    let model = &state.model;
    let s = model.sessions.get(&c.session)?;
    let w = model.windows.get(&s.active)?;
    let geom = status_geometry(model, s.id, c.size.1);
    let visible = (c.size.0, c.size.1 - geom.lines);
    let area = w.area();
    let rects = visible_panes(w);
    let borders = if w.zoomed {
        Vec::new()
    } else {
        w.layout.borders(area)
    };
    // A pane moved with the mouse is drawn under the pointer, above
    // everything else, and its own place shows empty (FR.3).
    let lifted = crate::server::mouse::lifted(state, w.id);
    let is_lifted = |id: PaneId| lifted.is_some_and(|(p, _)| p == id);
    let mut cursor = None;
    let panes: Vec<_> = rects
        .into_iter()
        .filter(|(id, _)| !is_lifted(*id))
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
    let viewport = match c.pan {
        Some((window, x, y)) if window == w.id => follow((x, y), None, (w.cols, w.rows), visible),
        _ => follow(
            c.viewport,
            cursor.map(|p| (p.col, p.row)),
            (w.cols, w.rows),
            visible,
        ),
    };
    let shown: Vec<PaneId> = panes
        .iter()
        .map(|(id, _, _)| *id)
        .chain(w.floating.iter().map(|f| f.pane).filter(|_| !w.zoomed))
        .chain(lifted.map(|(p, _)| p))
        .collect();
    let look = pane_looks(state, c, w, &shown);
    let mut window = render::compose(&FrameInputs {
        client: visible,
        viewport,
        area,
        panes,
        borders,
        border_style: window_style(model, w, "pane-border-style"),
        active_border_style: window_style(model, w, "pane-active-border-style"),
        active: w.active,
        // The border is marked in the session the mark was made in.
        marked: model
            .marked_pane()
            .filter(|_| model.marked.is_some_and(|(s, ..)| s == c.session))
            .filter(|p| model.panes.get(p).is_some_and(|p| p.window == w.id)),
        status: None,
        cursor,
        graphics: state.graphics.remap(),
        images: Some(ImageCaps::of(&c.caps)),
        look: look.clone(),
    });
    screenimg::mark_covered(&mut window, 0);
    if (!w.zoomed && !w.floating.is_empty()) || lifted.is_some() {
        let mut stack = w.floating.clone();
        stack.sort_by_key(|f| f.z);
        let floating_pane = |id: PaneId, at: (i32, i32)| {
            let p = model.panes.get(&id)?;
            Some(render::FloatingPane {
                id,
                at,
                size: (p.cols, p.rows),
                snap: p.handle.snapshot_rows(0..p.rows),
                active: id == w.active,
            })
        };
        let floating: Vec<render::FloatingPane> = stack
            .iter()
            .filter(|f| !w.zoomed && !is_lifted(f.pane))
            .map(|f| (f.pane, (f.x, f.y)))
            .chain(lifted)
            .filter_map(|(id, at)| floating_pane(id, at))
            .collect();
        render::draw_floating(
            &mut window,
            &floating,
            viewport,
            &window_style(model, w, "pane-border-style"),
            &window_style(model, w, "pane-active-border-style"),
            &look,
        );
    }
    // Floating panes cover what is under them, images included.
    screenimg::hide_covered(&mut window);
    let (rows, message) = status_rows(state, c, s, geom.lines);
    let pill = c.message.as_ref().and_then(|(text, _, level)| {
        Some((
            text,
            *level,
            pill_ends(model, &options::chain(model, w.active))?,
        ))
    });
    let mut lines: Vec<StyledLine> = rows.iter().map(|r| r.line.clone()).collect();
    let mut screen = if !lines.is_empty() {
        if let Some((line, m)) = &message
            && pill.is_none()
        {
            lines[*line] = m.line.clone();
        }
        render::with_status(window, &lines, geom.top)
    } else {
        // ponytail: with `status off` a message still takes the last row,
        // drawn over the window's last row while it shows.
        let mut window = window;
        if let Some((_, m)) = &message
            && pill.is_none()
            && let Some(last) = window.rows.checked_sub(1)
        {
            screenimg::mark_covered(&mut window, 0);
            render::draw_row(&mut window, last, &m.line);
            screenimg::hide_covered(&mut window);
        }
        window
    };
    let cx = overlay_ctx_at(model, id, c, viewport);
    screenimg::mark_covered(&mut screen, 0);
    if let Some((text, level, ends)) = pill {
        let row = match &message {
            Some((line, _)) if geom.lines > 0 => geom.first_row + *line as u16,
            _ => screen.rows.saturating_sub(1),
        };
        render::draw_message_pill(&mut screen, row, text, level, ends);
    }
    for overlay in state.registry.overlays.iter().rev() {
        overlay.draw(state, id, &cx, &mut screen);
    }
    // tmux 3.7b hides the cursor while a message shows (`TTY_NOCURSOR`).
    if message.is_some() {
        screen.cursor = None;
    }
    render::fix_wide_pairs(&mut screen);
    // Popups, menus, prompts and choosers cover images the same way.
    screenimg::hide_covered(&mut screen);
    Some((screen, viewport, rows, geom.first_row))
}

/// Client `c`'s `lines` status rows on session `s` (spec §4.10), and its
/// message row if one shows. Row N draws `status-format[N]`; an empty
/// `status-format[0]` (the default) is tmux's default row, spelled by
/// [`status::render_row`]; a row with no entry is blank in `status-style`.
/// Formats expand against the current window's active pane with the
/// server's and the client's variables over it (D9: `pane_title` and the
/// host), options come from that pane's chain, and every window of the
/// list expands against its own active pane.
fn status_rows(
    state: &State,
    c: &ClientEntry,
    s: &Session,
    lines: u16,
) -> (Vec<StatusRow>, Option<(usize, StatusRow)>) {
    let model = &state.model;
    let width = c.size.0;
    let active = model.windows.get(&s.active).map(|w| w.active);
    let chain = active.map(|p| options::chain(model, p)).unwrap_or_default();
    let chain = if chain.is_empty() {
        vec![Owner::Session(s.id)]
    } else {
        chain
    };
    let opt = |name: &str, default: &'static str| {
        model
            .options
            .lookup(name, &chain)
            .map_or(default, |v| v as &str)
    };
    let client_vars = ClientVars::new(model, c, active.map(|p| FormatCtx::new(model, p)));
    let server_vars = active.map(|p| options::ServerVars::new(state, p, &client_vars));
    let vars: &dyn FormatVars = match &server_vars {
        Some(v) => v,
        None => &client_vars,
    };
    let style = status_style(opt);
    let style = style.as_str();
    let now = local_now();
    // The message, formatted by `message-format` (`#{message}` and
    // `#{command_prompt}` set), drawn over the status line `message-line`
    // names (clamped to the last), whose cells it keeps where it writes none.
    let message_format = |under: Option<&StatusRow>| {
        let (text, _, _) = c.message.as_ref()?;
        let vars = MessageVars {
            inner: vars,
            message: text,
            command_prompt: false,
        };
        let expanded = status::expand_time(opt("message-format", MESSAGE_FORMAT), &vars, &now);
        Some(match under {
            Some(row) => {
                let cells: Vec<(char, status::Style)> = row
                    .line
                    .spans
                    .iter()
                    .flat_map(|span| span.text.chars().map(move |ch| (ch, span.style)))
                    .collect();
                status::draw_over(&expanded, status::Style::default(), &cells)
            }
            None => status::draw(&expanded, status::Style::default(), width),
        })
    };
    if lines == 0 {
        return (Vec::new(), message_format(None).map(|m| (0, m)));
    }
    let formats = model.options.lookup_list("status-format", &chain);
    let rows: Vec<StatusRow> = (0..u32::from(lines))
        .map(|i| match formats.and_then(|f| f.get(&i)) {
            Some(t) if t.is_empty() && i == 0 => default_row(state, c, s, &chain, vars, now),
            Some(t) => status::draw_row(t, style, vars, width, &now),
            None => status::blank_row(style, vars, width),
        })
        .collect();
    let line = opt("message-line", "0")
        .parse::<usize>()
        .unwrap_or(0)
        .min(usize::from(lines) - 1);
    let message = message_format(rows.get(line)).map(|m| (line, m));
    (rows, message)
}

/// The format variables a message or prompt label is drawn with.
pub(crate) struct MessageVars<'a> {
    pub inner: &'a dyn FormatVars,
    pub message: &'a str,
    /// `#{command_prompt}`: the prompt is in vi's command mode.
    pub command_prompt: bool,
}

impl FormatVars for MessageVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "message" => Some(self.message.to_string()),
            "command_prompt" => Some(u8::from(self.command_prompt).to_string()),
            _ => self.inner.get(name),
        }
    }
}

/// tmux's default status row for client `c` on session `s`: status-left,
/// the window list, status-right.
fn default_row(
    state: &State,
    c: &ClientEntry,
    s: &Session,
    chain: &[Owner],
    vars: &dyn FormatVars,
    now: libc::tm,
) -> StatusRow {
    let model = &state.model;
    let opt = |name: &str, default: &'static str| {
        model
            .options
            .lookup(name, chain)
            .map_or(default, |v| v as &str)
    };
    let length = |name: &str| opt(name, "0").parse::<u16>().unwrap_or(0);
    let listed: Vec<(&crate::server::model::Window, Vec<Owner>)> = s
        .windows
        .iter()
        .filter_map(|id| {
            let w = model.windows.get(id)?;
            Some((w, options::chain(model, w.active)))
        })
        .collect();
    let client_vars: Vec<ClientVars> = listed
        .iter()
        .map(|(w, _)| {
            let vars = FormatCtx::new(model, w.active).in_session(s.id);
            ClientVars::new(model, c, Some(vars))
        })
        .collect();
    let server_vars: Vec<options::ServerVars> = listed
        .iter()
        .zip(&client_vars)
        .map(|((w, _), v)| options::ServerVars::new(state, w.active, v))
        .collect();
    let entries: Vec<WindowStatus> = listed
        .iter()
        .zip(&server_vars)
        .map(|((w, chain), vars)| {
            let wopt = |name: &str, default: &'static str| {
                model
                    .options
                    .lookup(name, chain)
                    .map_or(default, |v| v as &str)
            };
            let defaults = WindowOptions::default();
            let current = w.id == s.active;
            WindowStatus {
                format: if current {
                    wopt("window-status-current-format", "")
                } else {
                    wopt("window-status-format", "")
                },
                vars,
                index: model.index_in(s.id, w.id).unwrap_or(w.index),
                current,
                last: s.last_windows.last() == Some(&w.id),
                bell: w.bell_flag,
                alert: w.activity_flag || w.silence_flag,
                options: WindowOptions {
                    separator: wopt("window-status-separator", defaults.separator),
                    style: wopt("window-status-style", defaults.style),
                    current_style: wopt("window-status-current-style", defaults.current_style),
                    last_style: wopt("window-status-last-style", defaults.last_style),
                    bell_style: wopt("window-status-bell-style", defaults.bell_style),
                    activity_style: wopt("window-status-activity-style", defaults.activity_style),
                },
            }
        })
        .collect();
    status::render_row(&StatusInputs {
        left: opt("status-left", ""),
        right: opt("status-right", ""),
        left_length: length("status-left-length"),
        right_length: length("status-right-length"),
        style: &status_style(opt),
        left_style: opt("status-left-style", "default"),
        right_style: opt("status-right-style", "default"),
        justify: opt("status-justify", "left"),
        vars,
        windows: &entries,
        width: c.size.0,
        now,
    })
}

/// `status-style` with `status-fg` and `status-bg` over it, as tmux draws
/// the status line (status.c): a colour other than `default` replaces the
/// style's.
fn status_style<'a>(opt: impl Fn(&str, &'static str) -> &'a str) -> String {
    let mut style = opt("status-style", "").to_string();
    for (key, name) in [("fg", "status-fg"), ("bg", "status-bg")] {
        let colour = opt(name, "default");
        if colour != "default" && colour != "terminal" {
            style.push_str(&format!(",{key}={colour}"));
        }
    }
    style
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
        // A client sees a linked window as its own session has it.
        let pane = pane.map(|p| p.or_session(client.session));
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
            "client_tty" => Some(c.tty.clone().unwrap_or_default()),
            "client_pid" => Some(c.pid.unwrap_or(0).to_string()),
            "client_termname" => Some(c.term.clone()),
            // tmux: bytes not sent because the client fell behind; here,
            // the dropped frames' diffs.
            "client_discarded" => Some(c.outbox.discarded.to_string()),
            "client_key_table" => Some(c.keys.table().to_string()),
            // ponytail: grofe always speaks UTF-8 to the client.
            "client_flags" => Some({
                let mut flags = vec!["attached"];
                if c.focused {
                    flags.push("focused");
                }
                flags.extend(c.flags.names());
                if c.suspended {
                    flags.push("suspended");
                }
                flags.push("UTF-8");
                flags.join(",")
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
            // The terminal's own answer to XDA is not asked for.
            "client_termtype" => Some(String::new()),
            "client_utf8" => Some("1".to_string()),
            // Only this user's processes can connect to the socket.
            "client_uid" => Some(nix::unistd::getuid().as_raw().to_string()),
            "client_user" => Some(
                nix::unistd::User::from_uid(nix::unistd::getuid())
                    .ok()
                    .flatten()
                    .map(|u| u.name)
                    .unwrap_or_default(),
            ),
            "client_readonly" | "client_control_mode" => Some("0".to_string()),
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

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let pane = self.pane.as_ref()?;
        let specs = options::loop_specs(
            self.model,
            pane.pane_id(),
            None,
            Some(self.client.session),
            kind,
            sort,
        )?;
        Some(
            specs
                .into_iter()
                .map(|s| LoopItem {
                    vars: Box::new(ClientVars::new(
                        self.model,
                        self.client,
                        Some(match s.session {
                            Some(session) => {
                                FormatCtx::at(self.model, s.pane, s.level).in_session(session)
                            }
                            None => FormatCtx::at(self.model, s.pane, s.level),
                        }),
                    )),
                    pane: Some(s.pane),
                    active: s.active,
                    extras: s.extras,
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.pane.as_ref()?.name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.pane.as_ref()?.pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        match &self.pane {
            Some(pane) => pane.reexpand(text),
            None => text.to_string(),
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
pub(crate) fn key_settings(model: &Model, session: SessionId) -> KeySettings {
    let chain = [Owner::Session(session)];
    let default = KeySettings::default();
    KeySettings {
        prefix: model
            .options
            .lookup("prefix", &chain)
            .map_or(default.prefix, |v| {
                parse_prefix(v).unwrap_or(default.prefix)
            }),
        prefix2: model
            .options
            .lookup("prefix2", &chain)
            .and_then(|v| parse_prefix(v).ok())
            .flatten(),
        repeat_time: model
            .options
            .lookup("repeat-time", &chain)
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(default.repeat_time, Duration::from_millis),
        initial_repeat_time: model
            .options
            .lookup("initial-repeat-time", &chain)
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(default.initial_repeat_time, Duration::from_millis),
        prefix_timeout: model
            .options
            .lookup("prefix-timeout", &[])
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(default.prefix_timeout, Duration::from_millis),
    }
}

fn active_pane(model: &Model, session: SessionId) -> Option<PaneId> {
    let s = model.sessions.get(&session)?;
    Some(model.windows.get(&s.active)?.active)
}

/// The terminal sequences the `user-keys` option names, by `UserN`: its
/// array's elements, as the strings they were set to.
fn user_keys(model: &Model) -> Vec<(u16, Vec<u8>)> {
    model
        .options
        .lookup_list("user-keys", &[])
        .into_iter()
        .flatten()
        .filter_map(|(n, v)| Some((u16::try_from(*n).ok()?, v.clone().into_bytes())))
        .collect()
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
    c.decoder.set_user_keys(user_keys(&state.model));
    let inputs = c.decoder.feed(bytes, now, escape);
    // A client that ignores size never takes the windows' size over.
    let sizes_windows = state.clients.get(&id).is_some_and(|c| !c.flags.ignore_size);
    if sizes_windows && let Some(owner) = state.registry.owners.get_mut(&session) {
        owner.activity(id, now);
    }
    touch_session(&mut state.model, session);
    // The client that was active last sizes the windows its session shares
    // with another ([`apply_sizes`]).
    let shared = |m: &Model| {
        m.sessions
            .get(&session)
            .is_some_and(|s| s.windows.iter().any(|w| m.windows[w].links.len() > 1))
    };
    if shared(&state.model) {
        apply_sizes(state);
        mark_all_dirty(state);
    }
    for input in inputs {
        deliver(state, id, input, now);
    }
}

/// How much input a client may have held ([`hold_input`]): paste bytes,
/// and one for anything else. tmux holds without limit; past this grofe
/// drops everything that arrives until the hold is released, and logs it.
const HELD_CAP: usize = 1 << 20;

/// One decoded input from client `id`: held behind a running shell
/// condition or binding ([`hold_input`]), else routed. An open prompt,
/// menu or popup takes it even then, as tmux hands an overlay's and the
/// prompt's keys over at once rather than queue them (a binding's
/// `run-shell` that opens a popup, as tmux-sessionx's does, gets its keys).
fn deliver(state: &mut State, id: ClientId, input: Input, now: Instant) {
    let overlay = state.prompts.is_open(id) || state.menus.is_open(id) || state.popups.is_open(id);
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    if c.holds > 0 && !overlay {
        hold_item(state, id, Held::Input(input));
        return;
    }
    // As tmux: a key press takes a message down, and still counts; a
    // message from `display-message -N` is not taken down by one (and the
    // key goes no further, as tmux's `server_client_key_callback` returns).
    if let Input::Key(_) = input
        && c.message.is_some()
        && c.message_ignores_keys
        && c.message
            .as_ref()
            .is_some_and(|(_, until, _)| until.is_some())
    {
        return;
    }
    if let Input::Key(_) = input
        && c.message.take().is_some()
    {
        c.dirty = true;
        request_render(state, now);
    }
    route(state, id, input, now);
}

/// One thing held for a client, in the order it came.
enum Held {
    Input(Input),
    /// Work queued behind the input ([`in_input_order`]).
    Run(crate::server::Job),
}

impl Held {
    /// What it counts against [`HELD_CAP`].
    fn size(&self) -> usize {
        match self {
            Held::Input(Input::Paste { data, .. }) => data.len().max(1),
            _ => 1,
        }
    }
}

/// Runs `job` for client `id` in input order: now, or after the input its
/// hold keeps, as tmux appends a key event (the click timer's
/// `DoubleClick`) to the client's queue behind what waits there.
pub(crate) fn in_input_order(state: &mut State, id: ClientId, job: crate::server::Job) {
    if state.clients.get(&id).is_some_and(|c| c.holds > 0) {
        hold_item(state, id, Held::Run(job));
    } else {
        job(state);
    }
}

/// Puts `input` back behind client `id`'s hold, if it has one: an overlay
/// that took a key while the client was held and passes it on. Whether it
/// was held.
pub(crate) fn hold_behind(state: &mut State, id: ClientId, input: &Input) -> bool {
    if !state.clients.get(&id).is_some_and(|c| c.holds > 0) {
        return false;
    }
    hold_item(state, id, Held::Input(input.clone()));
    true
}

/// Keeps `item` for held client `id`, unless the hold is full.
fn hold_item(state: &mut State, id: ClientId, item: Held) {
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    let size = item.size();
    if c.held_full || c.held_bytes + size > HELD_CAP {
        if !c.held_full {
            eprintln!("grofe server: input held for {} is full; dropped", c.name);
        }
        c.held_full = true;
        return;
    }
    c.held_bytes += size;
    c.held.push_back(item);
}

/// Holds client `id`'s input, in order, until [`release_input`]: a
/// binding's shell condition runs off the actor, and as tmux's per-client
/// command queue does, what the client types next waits for its branch
/// (TC.10). Other clients are not held. A condition that never exits
/// holds the client until it is killed or detaches, as in tmux.
pub(crate) fn hold_input(state: &mut State, id: ClientId) {
    if let Some(c) = state.clients.get_mut(&id) {
        c.holds += 1;
    }
}

/// Ends one [`hold_input`]; with none left, the held input is routed in
/// order, until an input starts another hold.
pub(crate) fn release_input(state: &mut State, id: ClientId) {
    let Some(c) = state.clients.get_mut(&id) else {
        return;
    };
    c.holds = c.holds.saturating_sub(1);
    if c.holds > 0 {
        return;
    }
    c.held_full = false;
    c.held_bytes = 0;
    let rest = std::mem::take(&mut c.held);
    let now = Instant::now();
    // Each item goes through `deliver` as if it came now: once one starts
    // another hold, those behind it are held again in order, except that a
    // prompt, menu or popup it opened takes the input behind it (`2` then
    // `{` read in one go: copy mode's count prompt gets the `{`).
    for item in rest {
        if !state.clients.contains_key(&id) {
            return;
        }
        match item {
            Held::Input(input) => deliver(state, id, input, now),
            Held::Run(job) => in_input_order(state, id, job),
        }
    }
}

/// A key typed on client `id` (`send-keys -K`), through its key tables.
pub(crate) fn type_key(state: &mut State, id: ClientId, key: crate::server::input::Key) {
    route(state, id, Input::Key(key), Instant::now());
}

fn write_input(state: &State, pane: PaneId, bytes: Vec<u8>) {
    if let Some(p) = state.model.panes.get(&pane)
        && p.handle.enqueue_input(bytes).is_err()
    {
        eprintln!("grofe server: input queue full for {}", p.id);
    }
}

/// Whether keys written to `pane` reach it: it is alive and was not
/// stopped by `select-pane -d` (tmux's `fd != -1` and `PANE_INPUTOFF`).
pub(crate) fn takes_input(state: &State, pane: PaneId) -> bool {
    state
        .model
        .panes
        .get(&pane)
        .is_some_and(|p| p.dead.is_none() && !p.input_off)
}

/// tmux's `window_pane_copy_key`: `key`, just written to `pane`, goes to
/// its [`sync_targets`] as well, encoded for each one's own modes.
pub(crate) fn copy_key(state: &State, pane: PaneId, key: &crate::server::input::Key) {
    for other in sync_targets(state, pane) {
        let bytes = crate::server::input::encode(key, &state.model.panes[&other].handle.modes());
        if !bytes.is_empty() {
            write_input(state, other, bytes);
        }
    }
}

/// Whether `pane` shows a mode (copy, clock, a chooser), which takes the
/// keys instead of the program.
fn pane_in_mode(state: &State, pane: PaneId) -> bool {
    state.clocks.is_active(pane)
        || state.copy.is_active(pane)
        || crate::server::tree::format_var(state, pane, "pane_in_mode").is_some()
}

/// The panes that get what is typed, pasted or sent into `pane` as well,
/// as tmux's `window_pane_copy_key` and `window_pane_copy_paste`: with
/// `synchronize-panes` on for `pane`, the other panes of its window that
/// have it on, are visible (not hidden by zoom), are alive, take input and
/// show no mode. In pane order.
pub(crate) fn sync_targets(state: &State, pane: PaneId) -> Vec<PaneId> {
    let model = &state.model;
    let on = |p: PaneId| {
        model
            .options
            .lookup("synchronize-panes", &options::chain(model, p))
            == Some("on")
    };
    let Some(w) = model
        .panes
        .get(&pane)
        .and_then(|p| model.windows.get(&p.window))
    else {
        return Vec::new();
    };
    if !on(pane) {
        return Vec::new();
    }
    w.pane_ids()
        .into_iter()
        .filter(|p| *p != pane)
        .filter(|p| !w.zoomed || *p == w.active)
        .filter(|p| takes_input(state, *p) && !pane_in_mode(state, *p) && on(*p))
        .collect()
}

/// `refresh-client -f`: `list` of client flags set or cleared on client `id`.
pub(crate) fn set_flags(state: &mut State, id: ClientId, list: &str) {
    if let Some(c) = state.clients.get_mut(&id) {
        c.flags.apply(list);
    }
}

/// `switch-client -r`: the client becomes read-only, with its size ignored,
/// or stops being.
pub(crate) fn toggle_read_only(state: &mut State, id: ClientId) {
    if let Some(c) = state.clients.get_mut(&id) {
        let on = !c.flags.read_only;
        c.flags.read_only = on;
        c.flags.ignore_size = on;
    }
}

/// Whether tmux lets a read-only client run the command `argv` (its
/// `CMD_READONLY` commands; `send-keys` only to drive a mode, with `-X`).
fn read_only_command(argv: &[String]) -> bool {
    let Some(verb) = argv.first() else {
        return true;
    };
    let name = crate::compat::tmux::resolve(verb).map_or(verb.as_str(), |spec| spec.name);
    match name {
        "attach-session" | "copy-mode" | "detach-client" | "list-clients" | "switch-client" => true,
        // send-keys' value flags are -c -N -t, so in `-tX` the X is a target.
        "send-keys" => argv.iter().skip(1).any(|a| {
            a.strip_prefix('-').is_some_and(|f| {
                f.chars()
                    .take_while(|c| !matches!(c, 'c' | 'N' | 't'))
                    .any(|c| c == 'X')
            })
        }),
        _ => false,
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
        hooks::client_focus(state, id, focused);
    }
    // A read-only client's pastes and mouse reach nothing.
    let read_only = state.clients.get(&id).is_some_and(|c| c.flags.read_only);
    if read_only && matches!(input, Input::Mouse(_) | Input::Paste { .. }) {
        return;
    }
    // A locked client's input is its lock command's alone: a prompt or menu
    // left open under the lock (their hooks come before the popup's) must
    // not take it, as tmux's suspended client reads no keys at all.
    if state.registry.locked.contains(&id) && state.popups.is_open(id) {
        let _ = crate::server::popup::PopupHook.on_input(state, id, &input, now);
        return;
    }
    if let Input::Mouse(m) = &input
        && let Some(c) = state.clients.get_mut(&id)
    {
        c.mouse_prev = c.mouse_last.replace((m.col, m.row, m.code()));
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
    // A read-only client's keys go to no pane, and its bindings run only
    // commands tmux marks read-only (`key_bindings_dispatch`).
    let routed = match routed {
        Routed::ToPane(_) if read_only && !matches!(input, Input::Focus(_)) => return,
        Routed::Command(ref cmds, _) | Routed::Bound(ref cmds, _)
            if read_only && !cmds.iter().all(|argv| read_only_command(argv)) =>
        {
            show_message(state, id, "client is read-only");
            return;
        }
        routed => routed,
    };
    match routed {
        Routed::ToPane(bytes) => {
            let Some(pane) = pane else {
                return;
            };
            // A key pressed over a pane kept with `remain-on-exit key` closes
            // it, and goes no further (`server_client_key_callback`).
            if let Input::Key(_) = &input
                && state
                    .model
                    .panes
                    .get(&pane)
                    .is_some_and(|p| p.dead.is_some())
                && state
                    .model
                    .options
                    .lookup("remain-on-exit", &options::chain(&state.model, pane))
                    == Some("key")
            {
                let removed = state.model.remove_pane(pane);
                state.events.panes_killed(removed);
                changed(state);
                return;
            }
            // A focus report goes in whatever the pane takes, as tmux's
            // `window_pane_update_focus`; keys and pastes stop at a pane
            // that takes no input or has exited (`window_pane_key`).
            if !matches!(input, Input::Focus(_)) && !takes_input(state, pane) {
                return;
            }
            write_input(state, pane, bytes);
            match &input {
                Input::Key(key) => copy_key(state, pane, key),
                Input::Paste { data, start, end } => {
                    // tmux's `window_pane_copy_paste` copies the text as
                    // is; the paste markers are keys, written to each pane
                    // as its own mode asks (`input_key_pane`).
                    for other in sync_targets(state, pane) {
                        let modes = state.model.panes[&other].handle.modes();
                        let bytes = crate::server::input::encode_paste(data, *start, *end, &modes);
                        write_input(state, other, bytes);
                    }
                }
                _ => {}
            }
        }
        Routed::Command(cmds, dialect) => run_binding(state, id, &cmds, dialect),
        Routed::Bound(cmds, dialect) => {
            let key = match &input {
                Input::Key(key) => Some(key.to_string()),
                _ => None,
            };
            run_commands_for(state, id, &cmds, dialect, true, None, key);
        }
        Routed::Consumed => {}
    }
}

/// Runs bound commands for client `id` in order, each with the client's
/// context and its active pane (as it is then) as the current pane, parsed
/// in `dialect`, the binding's own (spec §4.5), never the attaching
/// client's: a native binding stays native under `tmux attach`. The first
/// error stops the rest and shows on the client's status row, as tmux shows
/// it. Output is dropped. An alias in `cmds` expands as each runs; a key
/// binding's were expanded when it was bound ([`Routed::Bound`]).
///
/// As tmux 3.7b's per-client command queue (recorded, `tm_6/run-shell`):
/// a command that answers later (`run-shell` without `-b`, `-d` too, a
/// blocked `wait-for`, an `if-shell` branch) or an `after-*` hook of it
/// waiting on its shell holds the rest of the commands, and the client's
/// later input, until it is done ([`BindingRun`]).
pub fn run_binding(state: &mut State, id: ClientId, cmds: &[Vec<String>], dialect: Dialect) {
    let key = state.command_key.take();
    run_commands_for(state, id, cmds, dialect, false, None, key);
}

/// One binding's commands still to run for a client, in order.
pub(crate) struct BindingRun {
    id: ClientId,
    cmds: VecDeque<Vec<String>>,
    dialect: Dialect,
    /// Aliases were expanded when the key was bound.
    bound: bool,
    /// The mouse event a mouse key's commands run for (TC.10): its pane is
    /// the current pane while it is there.
    mouse: Option<crate::server::mouse::MouseTarget>,
    /// The pane the commands run for, if not the active one (a menu's `-t`).
    pane: Option<PaneId>,
    /// The key whose binding runs these commands, if one does: each is
    /// logged for `show-messages` as run by it.
    key: Option<String>,
    /// This run holds the client's input.
    holding: bool,
}

/// Starts a [`BindingRun`] (see [`run_binding`]).
pub(crate) fn run_commands_for(
    state: &mut State,
    id: ClientId,
    cmds: &[Vec<String>],
    dialect: Dialect,
    bound: bool,
    mouse: Option<crate::server::mouse::MouseTarget>,
    key: Option<String>,
) {
    let run = BindingRun {
        id,
        cmds: cmds.iter().cloned().collect(),
        dialect,
        bound,
        mouse,
        pane: None,
        key,
        holding: false,
    };
    run_from(state, run);
}

/// [`run_binding`], with the commands' current pane `pane` (the pane a
/// menu was opened for) rather than the active one.
pub(crate) fn run_binding_in(
    state: &mut State,
    id: ClientId,
    cmds: &[Vec<String>],
    dialect: Dialect,
    pane: Option<PaneId>,
    key: Option<String>,
) {
    let run = BindingRun {
        id,
        cmds: cmds.iter().cloned().collect(),
        dialect,
        bound: false,
        mouse: None,
        pane,
        key,
        holding: false,
    };
    run_from(state, run);
}

/// `text` with a capital first letter, as tmux's `cmdq_error` shows an error
/// on an attached client's status line.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What a held [`BindingRun`] waits for; the error to show, if the held
/// commands failed.
type BindingWait = std::pin::Pin<Box<dyn std::future::Future<Output = Waited> + Send>>;

/// How a held command ended: what it printed, and the error to show.
#[derive(Default)]
struct Waited {
    out: String,
    err: Option<String>,
}

/// Runs `run`'s next commands until one makes it wait, then resumes it on
/// the actor once that is done.
fn run_from(state: &mut State, mut run: BindingRun) {
    use crate::server::mouse::{self, MouseState};
    let id = run.id;
    while let Some(argv) = run.cmds.pop_front() {
        let Some(c) = state.clients.get(&id) else {
            run.cmds.clear();
            break;
        };
        let active = active_pane(&state.model, c.session);
        let pane = run
            .mouse
            .as_ref()
            .map(|t| t.pane)
            .filter(|p| state.model.panes.contains_key(p))
            .or(run.pane.filter(|p| state.model.panes.contains_key(p)))
            .or(active);
        let ctx = ClientCtx {
            pane,
            dialect: run.dialect,
            ..c.ctx.clone()
        };
        let bound = run.bound;
        let one = |state: &mut State| {
            // A bare `=` is the mouse event's target (M36-R3).
            let argv = mouse::resolve_targets(state, &argv);
            if bound {
                dispatch::run_bound(state, &ctx, &argv)
            } else {
                dispatch::run_split(state, &ctx, &argv)
            }
        };
        if let Some(key) = &run.key {
            let who = state.clients.get(&id).map(|c| c.name.clone());
            let printed = print_for_log(run.dialect, &argv);
            push_message(
                state,
                format!("{} key {key}: {printed}", who.unwrap_or_default()),
            );
        }
        let saved = std::mem::replace(&mut state.event_key, run.key.clone());
        let result = match &run.mouse {
            Some(target) => MouseState::with_current(state, Some(target.clone()), one),
            None => one(state),
        };
        state.event_key = saved;
        let wait = binding_wait(state, id);
        // What a command prints shows in the view mode of the client's
        // pane (`server_client_print`), the error on its status line.
        match result {
            Ok(text) => crate::server::copy::view::print_to_client(state, id, &text),
            Err(e) => {
                let printed = std::mem::take(&mut state.failed_out);
                crate::server::copy::view::print_to_client(state, id, &printed);
                show_message(state, id, &capitalized(e.trim_end()));
                run.cmds.clear();
            }
        }
        let Some(wait) = wait else {
            continue;
        };
        if !run.holding {
            hold_input(state, id);
            run.holding = true;
        }
        state.events.flush(&state.model);
        changed(state);
        let jobs = state.jobs.clone();
        tokio::spawn(async move {
            let waited = wait.await;
            let job: crate::server::Job = Box::new(move |state: &mut State| {
                crate::server::copy::view::print_to_client(state, run.id, &waited.out);
                if let Some(e) = waited.err {
                    show_message(state, run.id, &capitalized(e.trim_end()));
                    run.cmds.clear();
                }
                run_from(state, run);
            });
            let _ = jobs.send(job).await;
        });
        return;
    }
    if run.holding {
        release_input(state, id);
    }
    state.events.flush(&state.model);
    changed(state);
}

/// What the command a binding just ran for client `id` left it waiting
/// for: its deferred answer, else the client's hook queue (see
/// [`run_binding`]). A failed `run-shell` (an exit status) does not stop
/// the rest, as in tmux; a held command's own error does.
fn binding_wait(state: &mut State, id: ClientId) -> Option<BindingWait> {
    if let Some(rx) = state.deferred_reply.take() {
        return Some(Box::pin(async move {
            match rx.await {
                Ok(Ok(out)) => Waited { out, err: None },
                Ok(Err(f)) => Waited {
                    out: f.out,
                    err: (!f.err.is_empty()).then_some(f.err),
                },
                Err(_) => Waited::default(),
            }
        }));
    }
    let rx = hooks::take_client_wait(state, id)?;
    Some(Box::pin(async move {
        let _ = rx.await;
        Waited::default()
    }))
}

/// The client a command acts on: the requesting client, else the most
/// recently active client on the request's pane's session, else the most
/// recently active client.
pub(crate) fn current_client(state: &State, ctx: &ClientCtx) -> Option<ClientId> {
    if let Some(id) = ctx.client.filter(|id| state.clients.contains_key(id)) {
        return Some(id);
    }
    // The session tmux picks for a pane in a window linked into several.
    let session = ctx
        .pane
        .and_then(|p| state.model.panes.get(&p))
        .and_then(|p| state.model.link_session(p.window, None));
    state
        .clients
        .iter()
        .filter(|(_, c)| session.is_none_or(|s| c.session == s))
        .max_by_key(|(_, c)| c.activity)
        .map(|(id, _)| *id)
}

pub(crate) fn client_by_name(state: &State, name: &str) -> Result<ClientId, String> {
    client_named(state, name).ok_or_else(|| format!("can't find client: {name}"))
}

/// The client `name` names, as tmux's `cmd_find_client` finds it: by its
/// name or its tty, with or without `/dev/`, one trailing `:` ignored.
pub(crate) fn client_named(state: &State, name: &str) -> Option<ClientId> {
    let name = name.strip_suffix(':').unwrap_or(name);
    state
        .clients
        .iter()
        .find(|(_, c)| {
            c.name == name
                || c.tty
                    .as_deref()
                    .is_some_and(|t| t == name || t.strip_prefix("/dev/") == Some(name))
        })
        .map(|(id, _)| *id)
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
        check_target(&state.model, &c.target, ctx, c.nest, None)?;
        return Err("open terminal failed: not a terminal".to_string());
    };
    let session = select_target(&mut state.model, &c.target, ctx, false)?;
    if let (Some(dir), Some(s)) = (&c.cwd, state.model.sessions.get_mut(&session)) {
        s.cwd = ctx.cwd.join(dir);
    }
    if let Some(list) = &c.flags
        && let Some(client) = state.clients.get_mut(&id)
    {
        client.flags.apply(list);
    }
    if c.read_only
        && let Some(client) = state.clients.get_mut(&id)
    {
        client.flags.read_only = true;
        client.flags.ignore_size = true;
    }
    if c.detach_others {
        detach_session_with(state, session, Some(id), c.kill_parent);
    }
    show_session_with(state, id, session, !c.no_env);
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
        // tmux 3.7b finds `-s` with CMD_FIND_CANFAIL: no such session is
        // not an error (recorded).
        if let Ok(session) = state.model.resolve_session(target, ctx) {
            detach_session_with(state, session, None, c.kill_parent);
        }
        return Ok(String::new());
    }
    let id = match &c.client {
        Some(name) => client_by_name(state, name)?,
        None => current_client(state, ctx).ok_or_else(|| "no current client".to_string())?,
    };
    if c.all_others {
        let others: Vec<ClientId> = state.clients.keys().copied().filter(|o| *o != id).collect();
        for other in others {
            detach_with(state, other, c.kill_parent);
        }
    } else {
        detach_with(state, id, c.kill_parent);
    }
    Ok(String::new())
}

/// `list-clients`, in client order; `-F` expands per client, with
/// `client_*` variables over the client's current pane's. The default line
/// is tmux 3.7b's (its `[user ...]` part only shows for another user's
/// client, which grofe never has).
/// `list-clients`: every client, or with `-t` those showing that
/// session, that `-f` keeps.
pub fn list_clients(state: &State, ctx: &ClientCtx, c: ListClients) -> Result<String, String> {
    let model = &state.model;
    let session = match &c.target {
        Some(target) => Some(model.resolve_session(target, ctx)?),
        None => None,
    };
    let format = c.format.unwrap_or_else(|| match ctx.dialect {
        crate::protocol::Dialect::Tmux => options::tmux_template(options::TMUX_LIST_CLIENTS),
        crate::protocol::Dialect::Grofe => {
            "#{client_name}: #{session_name} [#{client_width}x#{client_height} \
             #{client_termname}] (#{client_flags})"
                .to_string()
        }
    });
    let sort = sorting::Sort::new(&c.sort)?;
    let mut listed: Vec<(&ClientId, &ClientEntry)> = state.clients.iter().collect();
    sort.apply(&mut listed, |order, (ia, a), (ib, b)| {
        let first = match order {
            sorting::Order::Name => sorting::by_name(&a.name, &b.name),
            sorting::Order::Size => a.size.0.cmp(&b.size.0).then(a.size.1.cmp(&b.size.1)),
            sorting::Order::Creation => ia.cmp(ib),
            sorting::Order::Activity => b.activity.cmp(&a.activity),
            _ => std::cmp::Ordering::Equal,
        };
        first.then_with(|| sorting::by_name(&a.name, &b.name))
    });
    let mut out = String::new();
    for (_, client) in listed {
        if session.is_some_and(|s| s != client.session) {
            continue;
        }
        let pane = active_pane(model, client.session).map(|p| FormatCtx::new(model, p));
        let vars = ClientVars::new(model, client, pane);
        if let Some(filter) = &c.filter {
            let got = cmd::expand(filter, &vars);
            if got.is_empty() || got == "0" {
                continue;
            }
        }
        out.push_str(&format!("{}\n", cmd::expand(&format, &vars)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn a_read_only_client_drives_a_mode_only_with_a_real_x_flag() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(read_only_command(&v(&["send-keys", "-X", "cancel"])));
        assert!(read_only_command(&v(&["send-keys", "-RX", "cancel"])));
        assert!(!read_only_command(&v(&["send-keys", "-tX", "ls"])));
        assert!(!read_only_command(&v(&["send-keys", "ls"])));
    }
    use std::sync::Arc;

    use super::*;

    /// A hello's tty names the client only if it is a device path: else
    /// it is taken as none and the client is `client-N`.
    #[tokio::test]
    async fn only_a_device_path_names_a_client() {
        assert!(usable_tty("/dev/ttys003"));
        assert!(usable_tty("/dev/pts/4"));
        for bad in [
            "ttys003",
            "/tmp/x",
            "/dev/pts/1\n/dev/pts/2",
            "/dev/\x1b[31mred",
            &format!("/dev/{}", "a".repeat(libc::PATH_MAX as usize)),
        ] {
            assert!(!usable_tty(bad), "{bad:?}");
        }
        let mut state = crate::server::test_state();
        crate::server::dispatch::run(
            &mut state,
            &crate::server::test_ctx(),
            &[
                "new-session".to_string(),
                "-d".to_string(),
                "-s".to_string(),
                "main".to_string(),
            ],
        )
        .expect("new-session");
        let ctx = ClientCtx {
            dialect: crate::protocol::Dialect::Tmux,
            ..crate::server::test_ctx()
        };
        for (tty, name, kept) in [
            ("/dev/ttys009", "/dev/ttys009", Some("/dev/ttys009")),
            ("/dev/tty\x07s", "client-", None),
        ] {
            let accepted = attach(
                &mut state,
                AttachRequest {
                    ctx: ctx.clone(),
                    size: (80, 24),
                    term: "xterm".to_string(),
                    caps: Caps::default(),
                    pid: None,
                    target: Some("main".to_string()),
                    detach_others: false,
                    nest: true,
                    tty: Some(tty.to_string()),
                    created: false,
                    extras: Default::default(),
                },
            )
            .expect("attach");
            let c = &state.clients[&accepted.id];
            assert!(c.name.starts_with(name), "{}", c.name);
            assert_eq!(c.tty.as_deref(), kept);
        }
    }

    /// As tmux 3.7b's `show-messages` printed them (recorded on `-L
    /// grofe-fe2 -f /dev/null`).
    #[test]
    fn commands_print_as_tmux_logs_them() {
        let print = |words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            tmux_print(&argv).unwrap()
        };
        assert_eq!(
            print(&["set", "-sg", "escape-time", "25"]),
            "set-option -gs escape-time 25"
        );
        assert_eq!(
            print(&["set", "-as", "terminal-features", "*:RGB"]),
            "set-option -as terminal-features *:RGB"
        );
        assert_eq!(
            print(&["ls", "-f", "#{==:#{session_name},b}"]),
            "list-sessions -f \"#{==:#{session_name},b}\""
        );
        assert_eq!(print(&["has", "-t", "=ma"]), "has-session -t =ma");
        assert_eq!(
            print(&["new", "-d", "-s", "b", "-x", "80", "-y", "24"]),
            "new-session -d -s b -x 80 -y 24"
        );
        assert_eq!(print(&["show-messages"]), "show-messages");
        assert_eq!(print(&["show-messages", "-JT"]), "show-messages -JT");
    }

    #[test]
    fn a_cli_request_is_logged_command_by_command_with_its_error() {
        let mut state = crate::server::test_state();
        let ctx = ClientCtx {
            dialect: crate::protocol::Dialect::Tmux,
            ..crate::server::test_ctx()
        };
        let argv: Vec<String> = ["splitw", "-h", ";", "has", "-t", "=ma", ";", "ls"]
            .map(String::from)
            .to_vec();
        state.command_log = plan_command_log(&state, "client-7", &ctx, &argv, 3);
        // Two commands fire; the second fails, so the third never does.
        log_fired(&mut state);
        log_fired(&mut state);
        log_error(&mut state, "client-7", "can't find session: ma");
        let lines: Vec<&str> = messages(&state).map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            lines,
            [
                "client-7 command: split-window -h",
                "client-7 command: has-session -t =ma",
                "client-7 message: can't find session: ma",
            ]
        );
        // A plan whose parts do not add up to the request logs nothing.
        assert!(plan_command_log(&state, "c", &ctx, &argv, 4).is_empty());
    }

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
                tty: None,
                pid: None,
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
                mouse_prev: None,
                mouse_last: None,
                message: None,
                message_ignores_keys: false,
                status_rows: Vec::new(),
                status_at: 0,
                held: VecDeque::new(),
                held_bytes: 0,
                holds: 0,
                held_full: false,
                viewport: (0, 0),
                pan: None,
                suspended: false,
                suspend_tx: mpsc::channel(1).0,
                flags: ClientFlags::default(),
                mouse_modes: None,
                dirty: false,
                outbox,
                exit: None,
            },
        );
        (state, id)
    }

    /// Timer wakes the actor would take over `window` of idle time with one
    /// attached client: each is a `deadline` that falls inside it.
    fn idle_wakes(state: &mut State, window: Duration) -> usize {
        let start = Instant::now();
        tick(state, start);
        let mut wakes = 0;
        while let Some(due) = deadline(state).filter(|d| *d <= start + window) {
            wakes += 1;
            tick(state, due);
            assert!(wakes < 100, "runaway timer");
        }
        wakes
    }

    fn set_session_option(state: &mut State, name: &str, value: &str) {
        let session = state.clients[&ClientId(1)].session;
        state
            .model
            .options
            .set(Owner::Session(session), name, value, false)
            .unwrap();
    }

    /// `lock-after-time` fires once per idle spell, as tmux's one-shot
    /// session timer: a client that cannot be locked (no `lock-command`,
    /// no session, another popup open, suspended) does not leave the
    /// deadline in the past for the actor to spin on.
    #[test]
    fn a_lock_that_cannot_happen_does_not_wake_the_server_again() {
        let (mut state, _) = state_with_client();
        set_session_option(&mut state, "status-right", "x");
        set_session_option(&mut state, "lock-after-time", "1");
        set_session_option(&mut state, "lock-command", "");
        assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 1);
    }

    /// G2: with a clock in the status line, an idle server wakes only for
    /// the clock (once per `status-interval`), not every 500 ms for names.
    #[test]
    fn an_idle_server_wakes_only_for_the_status_clock() {
        let (mut state, _) = state_with_client();
        set_session_option(&mut state, "status-interval", "1");
        let wakes = idle_wakes(&mut state, Duration::from_millis(3500));
        assert!((3..=4).contains(&wakes), "{wakes} wakes in 3.5 s");
    }

    /// G2: a status line with nothing time-dependent in it needs no timer,
    /// so an idle server does not wake at all.
    #[test]
    fn an_idle_server_with_a_static_status_line_never_wakes() {
        let (mut state, _) = state_with_client();
        set_session_option(&mut state, "status-interval", "1");
        set_session_option(&mut state, "status-right", "#{session_name}");
        assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 0);
    }

    /// G2: a clock put into a static status line starts ticking again, and
    /// a pane change arms exactly one name check that does not re-arm.
    #[test]
    fn the_status_clock_follows_the_formats_and_names_arm_once() {
        let (mut state, _) = state_with_client();
        set_session_option(&mut state, "status-interval", "1");
        set_session_option(&mut state, "status-right", "x");
        assert_eq!(idle_wakes(&mut state, Duration::from_secs(3)), 0);
        set_session_option(&mut state, "status-right", "%H:%M");
        let now = Instant::now();
        tick(&mut state, now);
        assert!(deadline(&state).is_some_and(|d| d <= now + Duration::from_secs(1)));

        let (mut state, _) = state_with_client();
        set_session_option(&mut state, "status-right", "#{session_name}");
        let now = Instant::now();
        arm_names(&mut state, now);
        assert_eq!(deadline(&state), Some(now + NAME_INTERVAL));
        tick(&mut state, now + NAME_INTERVAL);
        assert_eq!(deadline(&state), None);
    }

    /// F1: tmux checks window names whether or not a client is attached.
    #[test]
    fn names_are_checked_with_no_client_attached() {
        let (mut state, _) = state_with_client();
        state.clients.clear();
        let now = Instant::now();
        arm_names(&mut state, now);
        assert_eq!(deadline(&state), Some(now + NAME_INTERVAL));
        // One check, and no timer after it: a detached server idles at 0 wakes.
        tick(&mut state, now + NAME_INTERVAL);
        assert_eq!(deadline(&state), None);
    }

    #[test]
    fn only_formats_free_of_time_are_static() {
        for ok in [
            "",
            "plain",
            "[#{session_name}] ",
            "#I:#W#{?window_flags,#{window_flags}, }",
            "#[fg=red]#{=21:window_name}",
        ] {
            assert!(format_is_static(ok), "{ok}");
        }
        for bad in [
            "%H:%M",
            "100%",
            "#(date)",
            "#{t:session_created}",
            "#{@clock}",
            "#{pane_current_command}",
            "#{client_activity}",
            "#{S:#{window_name}}",
        ] {
            assert!(!format_is_static(bad), "{bad}");
        }
    }

    #[test]
    fn a_key_press_takes_a_message_down_before_display_time() {
        let (mut state, id) = state_with_client();
        show_message(&mut state, id, "no last window");
        let shown = state.clients[&id].message.clone();
        let (text, until, level) = shown.expect("message shown");
        assert_eq!(level, render::Level::Error);
        let until = until.expect("display-time 750 expires");
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

    /// As tmux: with `display-time 0` a message stays until a key is
    /// pressed, however long that is.
    #[test]
    fn display_time_zero_keeps_a_message_until_a_key() {
        let (mut state, id) = state_with_client();
        let session = state.clients[&id].session;
        state
            .model
            .options
            .set(Owner::Session(session), "display-time", "0", false)
            .unwrap();
        show_message(&mut state, id, "held");
        assert_eq!(
            state.clients[&id].message,
            Some(("held".to_string(), None, render::Level::Error))
        );
        let later = Instant::now() + Duration::from_secs(3600);
        tick(&mut state, later);
        assert!(state.clients[&id].message.is_some());
        assert!(deadline(&state).is_none_or(|d| d > Instant::now() + DISPLAY_TIME));
        input(&mut state, id, b"x", later);
        assert!(state.clients[&id].message.is_none());
    }

    /// TC.9: every status-line message is logged for `show-messages`, as
    /// tmux logs `<client> message: <text>`, and the log keeps tmux's
    /// `message-limit` newest.
    #[test]
    fn messages_are_logged_and_capped() {
        let (mut state, id) = state_with_client();
        display_message(&mut state, id, "hi", None, false, render::Level::Info);
        show_message(&mut state, id, "an #[error]");
        let log: Vec<&str> = messages(&state).map(|(_, m)| m.as_str()).collect();
        assert_eq!(
            log,
            ["client-1 message: hi", "client-1 message: an #[error]"]
        );
        for i in 0..MESSAGE_LIMIT {
            display_message(
                &mut state,
                id,
                &i.to_string(),
                None,
                false,
                render::Level::Info,
            );
        }
        assert_eq!(messages(&state).count(), MESSAGE_LIMIT);
        assert_eq!(
            messages(&state).next().map(|(_, m)| m.as_str()),
            Some("client-1 message: 0")
        );
        // `message-limit` is the cap once it is set.
        state
            .model
            .options
            .set(Owner::Global, "message-limit", "3", false)
            .unwrap();
        display_message(&mut state, id, "last", None, false, render::Level::Info);
        assert_eq!(messages(&state).count(), 3);
        assert_eq!(
            messages(&state).next_back().map(|(_, m)| m.as_str()),
            Some("client-1 message: last")
        );
    }

    /// tmux's `format_pretty_time`: the time of day within a day, else a
    /// day and date, a date and month, or a month and year.
    #[test]
    fn pretty_times_follow_tmux() {
        let now = SystemTime::now();
        let t = pretty_time(now);
        assert!(t.len() == 5 && t.as_bytes()[2] == b':', "{t}");
        let year_ago = now - Duration::from_secs(400 * 24 * 3600);
        let t = pretty_time(year_ago);
        assert_eq!(t.len(), 5, "{t}");
        assert!(t[3..].chars().all(|c| c.is_ascii_digit()), "{t}");
        let week_ago = pretty_time(now - Duration::from_secs(7 * 24 * 3600));
        assert!(week_ago.len() == 5 && week_ago[3..].chars().all(|c| c.is_ascii_digit()));
    }

    fn held_inputs(state: &State, id: ClientId) -> Vec<Input> {
        let held = &state.clients[&id].held;
        held.iter()
            .filter_map(|h| match h {
                Held::Input(input) => Some(input.clone()),
                Held::Run(_) => None,
            })
            .collect()
    }

    /// TC.10 fix round 1: a held client's input waits in order, holds
    /// nest, and past the cap new input is dropped.
    #[test]
    fn held_input_waits_in_order_until_the_last_release() {
        let (mut state, id) = state_with_client();
        let now = Instant::now();
        hold_input(&mut state, id);
        hold_input(&mut state, id);
        input(&mut state, id, b"ab", now);
        let held = held_inputs(&state, id);
        assert_eq!(
            held,
            vec![
                Input::Key(crate::server::input::parse_key("a").unwrap()),
                Input::Key(crate::server::input::parse_key("b").unwrap()),
            ]
        );
        release_input(&mut state, id);
        assert_eq!(state.clients[&id].held.len(), 2, "one hold left");
        release_input(&mut state, id);
        assert!(state.clients[&id].held.is_empty());
        assert_eq!(state.clients[&id].held_bytes, 0);

        hold_input(&mut state, id);
        state.clients.get_mut(&id).unwrap().held_bytes = HELD_CAP;
        input(&mut state, id, b"c", now);
        assert!(state.clients[&id].held.is_empty(), "dropped past the cap");
    }

    /// Past the cap, everything that arrives is dropped until the hold is
    /// released, not only the input that did not fit: a later small key
    /// would otherwise run with a gap before it.
    #[test]
    fn past_the_cap_held_input_is_dropped_until_release() {
        let (mut state, id) = state_with_client();
        let now = Instant::now();
        let key = |k: &str| Input::Key(crate::server::input::parse_key(k).unwrap());
        hold_input(&mut state, id);
        deliver(&mut state, id, key("a"), now);
        state.clients.get_mut(&id).unwrap().held_bytes = HELD_CAP - 1;
        let big = Input::Paste {
            data: vec![b'x'; 2],
            start: true,
            end: true,
        };
        deliver(&mut state, id, big, now);
        deliver(&mut state, id, key("b"), now);
        assert_eq!(
            held_inputs(&state, id),
            vec![key("a")],
            "b came after a dropped paste"
        );
        release_input(&mut state, id);
        assert!(state.clients[&id].held.is_empty());
        // A new hold holds again.
        hold_input(&mut state, id);
        deliver(&mut state, id, key("c"), now);
        assert_eq!(state.clients[&id].held.len(), 1);
    }

    /// A job queued in input order (the click timer's `DoubleClick`) waits
    /// behind the input a hold kept, as tmux appends it to the client's
    /// queue, and runs at once when nothing is held.
    #[test]
    fn a_job_in_input_order_runs_after_the_held_input() {
        let (mut state, id) = state_with_client();
        let now = Instant::now();
        let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job = |ran: &Arc<std::sync::Mutex<Vec<usize>>>| -> crate::server::Job {
            let ran = Arc::clone(ran);
            Box::new(move |state: &mut State| {
                let held = state.clients[&id].held.len();
                ran.lock().unwrap().push(held);
            })
        };
        hold_input(&mut state, id);
        input(&mut state, id, b"ab", now);
        in_input_order(&mut state, id, job(&ran));
        assert!(ran.lock().unwrap().is_empty(), "held behind a and b");
        release_input(&mut state, id);
        assert_eq!(*ran.lock().unwrap(), vec![0], "ran after both keys");
        in_input_order(&mut state, id, job(&ran));
        assert_eq!(*ran.lock().unwrap(), vec![0, 0]);
    }

    /// TM.1 (`keys_through_a_client`): a held key replayed on release that
    /// opens a prompt holds the client again, and the keys held behind it
    /// go to that prompt, as when they arrive after it opened. They were
    /// left held behind the prompt, which waited for them: copy mode's `2`
    /// then `{`, read in one go, lost the `{`.
    #[tokio::test]
    async fn held_keys_behind_a_replayed_prompt_go_to_it() {
        let (mut state, id) = state_with_client();
        crate::server::register_input_hook(&mut state, Arc::new(crate::server::prompt::PromptHook));
        let ctx = crate::server::test_ctx();
        let run = |state: &mut State, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(state, &ctx, &argv).expect("run")
        };
        run(&mut state, &["new-session", "-d", "-s", "a"]);
        let session = *state.model.sessions.keys().next().expect("a session");
        state.clients.get_mut(&id).expect("client").session = session;
        run(
            &mut state,
            &[
                "bind-key",
                "-n",
                "a",
                "command-prompt -I x { set -g @got '%%' }",
            ],
        );
        hold_input(&mut state, id);
        input(&mut state, id, b"ab\r", Instant::now());
        assert_eq!(state.clients[&id].held.len(), 3, "all held");
        release_input(&mut state, id);
        assert_eq!(state.model.options.lookup("@got", &[]), Some("xb"));
        assert!(!state.prompts.is_open(id));
    }

    /// tmux 3.7b (recorded): a binding keeps the alias expansion made when
    /// it was bound, and a key press does not expand it again. With
    /// `a=display-message` and `display-message=set @hit twice`, the key
    /// bound to `a` runs `display-message`.
    #[tokio::test]
    async fn a_bound_alias_chain_expands_once() {
        let (mut state, id) = state_with_client();
        let ctx = crate::server::test_ctx();
        let run = |state: &mut State, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(state, &ctx, &argv).expect("run")
        };
        run(&mut state, &["new-session", "-d", "-s", "a"]);
        let session = *state.model.sessions.keys().next().expect("a session");
        state.clients.get_mut(&id).expect("client").session = session;
        run(
            &mut state,
            &["set", "-ag", "command-alias", "a=display-message"],
        );
        run(
            &mut state,
            &[
                "set",
                "-ag",
                "command-alias",
                "display-message=set -g @hit twice",
            ],
        );
        run(&mut state, &["bind-key", "-n", "F5", "a"]);
        run(&mut state, &["bind-key", "-n", "F6", "set -g @pressed yes"]);
        input(&mut state, id, b"\x1b[15~\x1b[17~", Instant::now());
        assert_eq!(state.model.options.lookup("@pressed", &[]), Some("yes"));
        let hit = state.model.options.lookup("@hit", &[]);
        assert_eq!(hit, None, "expanded a second time");
    }

    /// TC.11: a click maps onto the status row it lands on, at the bottom
    /// by default and at the top with `status-position top`.
    #[test]
    fn a_status_click_finds_the_range_under_it() {
        let (mut state, id) = state_with_client();
        let net = status::draw(
            "#[range=user|net]NET#[norange]",
            status::Style::default(),
            80,
        );
        let user = Some(RangeKind::User("net".to_string()));
        let c = state.clients.get_mut(&id).unwrap();
        c.status_rows = vec![net.clone()];
        c.status_at = 23;
        assert_eq!(status_range_at(&state, id, 1, 23), user);
        assert_eq!(status_range_at(&state, id, 1, 22), None);
        assert_eq!(status_range_at(&state, id, 40, 23), None);

        let o = &mut state.model.options;
        o.set(Owner::Global, "status", "2", false).unwrap();
        o.set(Owner::Global, "status-position", "top", false)
            .unwrap();
        // TC.10: until the next draw, clicks still map onto the rows as
        // they were drawn, not as the options now place them.
        assert_eq!(status_range_at(&state, id, 1, 23), user);
        assert_eq!(status_at(&state, id, 1, 0), None);
        let c = state.clients.get_mut(&id).unwrap();
        c.status_rows = vec![StatusRow::default(), net];
        c.status_at = 0;
        assert_eq!(status_range_at(&state, id, 1, 1), user);
        assert_eq!(status_range_at(&state, id, 1, 0), None);
        assert_eq!(status_at(&state, id, 1, 0), Some((0, None)));
        assert_eq!(status_range_at(&state, id, 1, 23), None);
        assert_eq!(status_at(&state, id, 1, 2), None);
    }

    /// R1/R2 and item 3: the M3 and M3.6 hooks and overlays (prompt, menu,
    /// popup, chooser, mouse) and M5's copy mode are registered in priority order, and with
    /// nothing open every hook passes every kind of input on and every
    /// overlay leaves the screen alone.
    #[test]
    fn the_builtin_hooks_and_overlays_are_inert() {
        let (mut state, id) = state_with_client();
        crate::server::register_builtins(&mut state);
        assert_eq!(state.registry.input_hooks.len(), 7);
        // TM.6 removed the mouse's scrollback view overlay.
        assert_eq!(state.registry.overlays.len(), 6);
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
        assert_eq!(outbox.discarded, 0);
        assert!(!outbox.offer(screen_with('c'), &caps));
        assert!(outbox.needs_full_redraw());
        assert!(outbox.discarded > 0, "the dropped diff is counted");
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
    fn terminal_feature_patterns_are_globs() {
        assert!(glob("xterm*", "xterm-256color"));
        assert!(glob("*", "anything"));
        assert!(glob("screen?256color", "screen-256color"));
        assert!(glob("foot", "foot"));
        assert!(!glob("foot", "foot-extra"));
        assert!(!glob("xterm*", "screen"));
        assert!(!glob("a?", "a"));
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
