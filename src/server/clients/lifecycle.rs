use super::*;

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
    pub(super) tx: mpsc::Sender<Vec<u8>>,
    /// The screen the client shows once everything sent so far is written;
    /// `None` forces a full redraw.
    pub(super) prev: Option<Screen>,
    /// Set when a diff was dropped: nothing is composed for this client
    /// until its slot is free again.
    pub(super) blocked: bool,
    /// Bytes for ahead of the next frame ([`queue_raw`]); kept across a
    /// dropped frame.
    pub(super) raw: Vec<u8>,
    /// Bytes of dropped diffs, ever (`client_discarded`).
    pub(super) discarded: u64,
    /// The images the client's terminal shows once everything sent so far
    /// is written (G3); unlike `prev`, kept across a dropped frame.
    pub(super) placed: Vec<ScreenImage>,
}

/// Most raw bytes held for a client whose frames are being dropped.
// ponytail: past this, newer raw bytes are dropped until the client reads
// again; a full redraw re-sends what must survive (graphics, T3.8).
pub(super) const RAW_CAP: usize = 16 * 1024 * 1024;

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
    pub(super) mouse_last: Option<(u16, u16, u32)>,
    /// A message shown on the status line's first row until it expires or
    /// a key is pressed (a failed binding's error, as tmux shows it), in
    /// the language [`status::draw`] reads, and when it expires (`None`
    /// with `display-time 0`: only a key takes it down, as in tmux), and
    /// its level, for `message-pill`.
    pub(super) message: Option<(String, Option<Instant>, render::Level)>,
    /// `display-message -N`: a key does not take the message down.
    pub(super) message_ignores_keys: bool,
    /// The status rows as last drawn, top first, for their click ranges,
    /// and the client row the first of them was drawn on.
    pub(super) status_rows: Vec<StatusRow>,
    pub(super) status_at: u16,
    /// The window cell drawn at the client's top-left (crop, D6), as last
    /// drawn.
    pub viewport: (u16, u16),
    /// Set by `refresh-client -L/-R/-U/-D` (TC.9): the window whose
    /// visible part the client moved by hand, and where it is; the view
    /// then stops following the cursor until `-c`, another window, or the
    /// window fitting the client, as tmux's `pan_window`.
    pub(super) pan: Option<(WindowId, u16, u16)>,
    /// `suspend-client` stopped the client (TC.9): nothing is drawn for it
    /// until it resumes and sends its size, which redraws it in full.
    pub(super) suspended: bool,
    pub(super) suspend_tx: mpsc::Sender<()>,
    /// `read-only`, `ignore-size` and the like.
    pub flags: ClientFlags,
    /// The outer terminal's mouse modes as last written (R2): 0 off, 1 on,
    /// 2 on with all motion; `None` before the first write.
    pub(super) mouse_modes: Option<u8>,
    /// Input held while a binding's shell condition runs ([`hold_input`]),
    /// in order, and how many conditions hold it.
    pub(super) held: VecDeque<Held>,
    pub(super) held_bytes: usize,
    pub(super) holds: u32,
    /// The hold passed its cap: all input is dropped until it is released.
    pub(super) held_full: bool,
    /// Needs a new frame.
    pub(super) dirty: bool,
    /// When the toast last shown here expires, as [`deadline`] last saw
    /// it: the toast is gone by then, so [`tick`] cannot work it out.
    pub(super) toast_until: std::cell::Cell<Option<Instant>>,
    /// What a remote client said about itself (`set-client-info`).
    pub(crate) info: ClientInfo,
    pub(super) outbox: Outbox,
    pub(super) exit: Option<oneshot::Sender<String>>,
}

/// `set-client-info`'s values: `#{client_rtt}` and `#{client_machines}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientInfo {
    /// The smoothed round trip time, in milliseconds.
    pub rtt_ms: Option<u64>,
    /// The client's machine list, as it wants it shown.
    pub machines: Option<String>,
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
    pub(super) fn names(self) -> Vec<&'static str> {
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
    pub(super) pending: Option<(ClientId, Instant)>,
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
    pub(super) next_id: u32,
    /// Bindings by table, starting with the default key table.
    pub key_tables: KeyTables,
    /// See every input before the key tables, in registration order.
    pub input_hooks: Vec<Arc<dyn InputHook + Send + Sync>>,
    /// Drawn over every client's frame, in reverse registration order.
    pub overlays: Vec<Arc<dyn Overlay + Send + Sync>>,
    pub(super) owners: BTreeMap<SessionId, SizeOwner>,
    pub(super) render_at: Option<Instant>,
    pub(super) last_render: Option<Instant>,
    /// When the status line is next redrawn for its clock.
    pub(super) status_at: Option<Instant>,
    /// When window names are next checked (`automatic-rename`).
    pub(super) names_at: Option<Instant>,
    /// The status-line messages `show-messages` prints (TC.9), oldest
    /// first, with when each was shown.
    pub(super) messages: VecDeque<(SystemTime, String)>,
    /// The options a `set-option` has already warned about: stored, but
    /// with no effect in grofe.
    pub(super) inert_warned: BTreeSet<String>,
    /// `wait-for` channels (TC.9).
    pub(crate) waits: BTreeMap<String, crate::server::dispatch::client::WaitChannel>,
    /// The clients now locked, their `lock-command` running in a popup
    /// (`dispatch::lock`).
    pub(crate) locked: BTreeSet<ClientId>,
    /// Per session, the client activity its `lock-after-time` last fired
    /// for: tmux's lock timer is one-shot, so it waits for newer activity.
    pub(crate) lock_fired: BTreeMap<SessionId, Instant>,
    /// `server-access`: the users allowed besides the owner, and whether
    /// each may only read (uid, read-only). Root and the server's own user
    /// are always in it, as tmux's `server_acl_init`.
    pub(super) acl: BTreeMap<u32, bool>,
    /// When each pane was last scanned for an agent program.
    // ponytail: never pruned except by `scan_agents` dropping gone panes,
    // so it holds at most one instant per pane that has produced output.
    pub(super) agent_scans: BTreeMap<PaneId, Instant>,
    /// `agents.scan-interval`; the config sets it with `set_scan_interval`.
    pub(super) scan_interval: Duration,
}

impl Registry {
    #[cfg(test)]
    pub(crate) fn scanned(&self, pane: PaneId) -> bool {
        self.agent_scans.contains_key(&pane)
    }

    /// The next scan of `pane` is not throttled.
    pub(crate) fn forget_scan(&mut self, pane: PaneId) {
        self.agent_scans.remove(&pane);
    }

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
            inert_warned: BTreeSet::new(),
            waits: BTreeMap::new(),
            locked: BTreeSet::new(),
            lock_fired: BTreeMap::new(),
            acl: [0, nix::unistd::getuid().as_raw()]
                .into_iter()
                .map(|uid| (uid, false))
                .collect(),
            agent_scans: BTreeMap::new(),
            scan_interval: Duration::from_secs(1),
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
    if req.caps.colours.is_some() {
        state.model.terminal_colours = req.caps.colours;
    }
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
            info: ClientInfo::default(),
            toast_until: std::cell::Cell::new(None),
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
    if req.extras.open_settings {
        let _ = crate::server::settings::overlay::open(state, id);
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
pub(super) fn usable_tty(tty: &str) -> bool {
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
pub(super) fn check_target(
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
pub(super) fn touch_session(model: &mut Model, session: SessionId) {
    if let Some(s) = model.sessions.get_mut(&session) {
        s.activity = Instant::now();
    }
}

/// Recounts `session_attached` for every session.
pub(super) fn sync_attached(state: &mut State) {
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
pub(super) const MESSAGE_LIMIT: usize = 1000;

/// Adds a status-line message for client `id` to the server's message
/// log, worded as tmux's `status_message_set` logs it.
pub(super) fn log_message(state: &mut State, id: ClientId, text: &str) {
    let Some(name) = state.clients.get(&id).map(|c| c.name.clone()) else {
        return;
    };
    push_message(state, format!("{name} message: {text}"));
}

/// Logs to `show-messages`, once per option and server, that setting `name`
/// does nothing: grofe stores it and never reads it.
pub(crate) fn warn_inert_option(state: &mut State, name: &str) {
    if state.registry.inert_warned.insert(name.to_string()) {
        push_message(state, format!("{name}: stored, but has no effect in grofe"));
    }
}

pub(super) fn push_message(state: &mut State, line: String) {
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
/// The log lines of the request now running: how many native commands each
/// covers, and its line. Logged as the commands fire ([`log_fired`]).
pub(crate) type CommandLog = VecDeque<(usize, Option<String>)>;

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
) -> CommandLog {
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
            let words: Vec<String> = cmd.iter().map(|w| quote::tmux_arg(w)).collect();
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
pub(super) fn print_for_log(dialect: crate::protocol::Dialect, cmd: &[String]) -> String {
    let printed = match dialect {
        crate::protocol::Dialect::Tmux => tmux_print(cmd),
        crate::protocol::Dialect::Grofe => None,
    };
    printed.unwrap_or_else(|| {
        let words: Vec<String> = cmd.iter().map(|w| quote::tmux_arg(w)).collect();
        words.join(" ")
    })
}

/// Logs `cmd`, a command a key's binding runs in a branch of its own
/// (`if-shell`), as run by that key.
pub(crate) fn log_key_command(state: &mut State, ctx: &ClientCtx, cmd: &[String]) {
    let Some(key) = ctx.request.event_key.clone() else {
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
pub(crate) fn log_fired(state: &mut State, log: &mut CommandLog) {
    while let Some((left, line)) = log.front_mut() {
        let (left, line) = (std::mem::take(left), line.take());
        if let Some(line) = line {
            push_message(state, line);
        }
        if left > 1 {
            if let Some(front) = log.front_mut() {
                front.0 = left - 1;
            }
            return;
        }
        log.pop_front();
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
pub(super) fn tmux_print(argv: &[String]) -> Option<String> {
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
            out.push_str(&format!(" -{flag} {}", quote::tmux_arg(v)));
        }
    }
    for word in args.get(i..).unwrap_or_default() {
        out.push(' ');
        out.push_str(&quote::tmux_arg(word));
    }
    Some(out)
}

/// The message log (TC.9's `show-messages`), oldest first.
pub(crate) fn messages(state: &State) -> impl DoubleEndedIterator<Item = &(SystemTime, String)> {
    state.registry.messages.iter()
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

pub(super) fn set_message(
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

/// Nerd Font round caps, a pill's ends when neither `message-pill-ends`
/// nor the theme's `@pill_l` and `@pill_r` give them.
const ROUND_ENDS: &str = "\u{e0b6}\u{e0b4}";

/// How a `level` message or prompt pill is drawn while `message-pill` is on
/// for `chain`; `None` draws tmux's full-width bar. The colour is the
/// theme's (`@theme_blue`, `@theme_yellow`, `@theme_red`, text in
/// `@theme_bg`), else a named one; the ends are `message-pill-ends`, else
/// the theme's `@pill_l` and `@pill_r`, else round caps.
pub(crate) fn pill_look(
    model: &Model,
    chain: &[Owner],
    level: render::Level,
) -> Option<render::PillLook> {
    let get = |name: &str| model.options.lookup(name, chain).filter(|v| !v.is_empty());
    if get("message-pill") == Some("off") {
        return None;
    }
    let ends = get("message-pill-ends")
        .map(str::to_string)
        .or_else(|| Some(format!("{}{}", get("@pill_l")?, get("@pill_r")?)))
        .unwrap_or_else(|| ROUND_ENDS.to_string());
    let (theme, _, _, _) = level.look();
    let named = render::PillLook::named(level, &ends);
    Some(match (get(theme), get("@theme_bg")) {
        (Some(colour), Some(text)) => render::PillLook {
            colour: colour.to_string(),
            text: text.to_string(),
            ends,
        },
        _ => named,
    })
}

/// How many columns of status line `line` a pill covers: up to where
/// `status-right` starts, so the right side stays; the whole row when the
/// line has no `status-right`.
pub(crate) fn pill_span(rows: &[StatusRow], line: usize, cols: u16) -> usize {
    let right = rows.get(line).and_then(|row| {
        row.ranges
            .iter()
            .find(|r| r.kind == RangeKind::Right && r.end > r.start)
            .map(|r| r.start)
    });
    usize::from(right.unwrap_or(cols).min(cols))
}

/// The session's `display-time`, or tmux's default; 0 means until a key.
pub(super) fn display_time(model: &Model, session: SessionId) -> Duration {
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
pub(super) fn clamp_size((cols, rows): (u16, u16)) -> (u16, u16) {
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

pub(super) fn detached_from(model: &Model, session: SessionId) -> String {
    match model.sessions.get(&session) {
        Some(s) => format!("detached (from session {})", s.name),
        None => "detached".to_string(),
    }
}

/// Detaches every client on `session` except `keep`, hanging up their
/// parents with `kill_parent`.
pub(super) fn detach_session_with(
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
pub(super) fn remove(state: &mut State, id: ClientId, reason: Option<String>) {
    crate::server::passthrough::on_client_gone(state, id);
    let Some(mut c) = state.clients.remove(&id) else {
        return;
    };
    crate::server::prompt::client_gone(state, id);
    crate::server::menu::client_gone(state, id);
    crate::server::popup::client_gone(state, id);
    crate::server::tree::client_gone(state, id);
    crate::server::picker::client_gone(state, id);
    crate::server::sidebar::client_gone(state, id);
    crate::server::grid::client_gone(state, id);
    crate::server::keymenu::client_gone(state, id);
    crate::server::settings::overlay::client_gone(state, id);
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
pub(super) fn leave_session(state: &mut State, id: ClientId, session: SessionId) {
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
pub(super) fn successor(state: &State, name: &str, mode: &str) -> Option<SessionId> {
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
    // A killed pane's copy mode goes with it.
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
