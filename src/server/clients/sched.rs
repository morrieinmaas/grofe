use super::*;

/// The outer terminal's mouse reporting on and off, and on with every
/// motion (mode 1003) for `focus-follows-mouse` and a menu that takes the
/// mouse (tmux's `tty_update_mode`).
pub(super) const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(super) const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";
pub(super) const MOUSE_MOTION_ON: &[u8] = b"\x1b[?1003h";
pub(super) const MOUSE_MOTION_OFF: &[u8] = b"\x1b[?1003l";

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
    scan_agents(state, &dirty);
    if !dirty.is_empty() {
        arm_names(state, Instant::now());
    }
    if dirty.is_empty() || state.clients.is_empty() {
        return;
    }
    let model = &state.model;
    let mut any = false;
    let ids: Vec<ClientId> = state.clients.keys().copied().collect();
    for id in ids {
        let preview = crate::server::picker::selected_pane(state, id);
        let grid = crate::server::grid::visible_panes(state, id);
        let Some(c) = state.clients.get_mut(&id) else {
            continue;
        };
        let shown = model.sessions.get(&c.session).map(|s| s.active);
        if dirty
            .iter()
            .any(|p| model.panes.get(p).map(|p| p.window) == shown)
            || preview.is_some_and(|p| dirty.contains(&p))
            || grid.iter().any(|p| dirty.contains(p))
        {
            c.dirty = true;
            any = true;
        }
    }
    if any {
        request_render(state, Instant::now());
    }
}

/// Sets how often a busy pane is checked for an agent program.
pub fn set_scan_interval(state: &mut State, interval: Duration) {
    state.registry.scan_interval = interval;
}

/// A foreground leader's pid and argv.
pub(super) type Leader = (i32, Vec<String>);

/// The foreground leader of `pane`: its pid and argv, read now.
pub(super) fn leader_argv(p: &crate::server::model::Pane) -> Option<Leader> {
    let pgrp = p.handle.foreground_group()?;
    let argv = crate::server::pane::procinfo::leader(pgrp).argv?;
    Some((pgrp, argv))
}

/// Checks each of `panes` for an agent program (agents spec 3.1), at most
/// once per `agents.scan-interval` per pane; a match with no record makes
/// one, bound to the leader process.
pub fn scan_agents(state: &mut State, panes: &BTreeSet<PaneId>) {
    scan_agents_with(state, panes, Instant::now(), &mut leader_argv);
}

pub(super) fn scan_agents_with(
    state: &mut State,
    panes: &BTreeSet<PaneId>,
    now: Instant,
    read: &mut dyn FnMut(&crate::server::model::Pane) -> Option<Leader>,
) {
    if !state.agents_enabled {
        return;
    }
    let interval = state.registry.scan_interval;
    let mut created = Vec::new();
    for &id in panes {
        let Some(p) = state.model.panes.get(&id).filter(|p| p.dead.is_none()) else {
            continue;
        };
        let last = state.registry.agent_scans.get(&id);
        if last.is_some_and(|&t| now.saturating_duration_since(t) < interval) {
            continue;
        }
        state.registry.agent_scans.insert(id, now);
        let Some((leader, argv)) = read(p) else {
            continue;
        };
        let Some(kind) = crate::server::agents::kinds::match_argv(&state.agent_kinds, &argv) else {
            continue;
        };
        let binding = crate::server::resume::Binding::of(leader);
        if state.agents.ensure(id, kind, binding, now) {
            created.push(id);
        }
    }
    let model = &state.model;
    state
        .registry
        .agent_scans
        .retain(|id, _| model.panes.contains_key(id));
    if !created.is_empty() {
        crate::server::resume::watch(state);
    }
    for id in created {
        let kind = state.agents.get(id).and_then(|a| a.kind.clone());
        let change = crate::server::agents::Change {
            old: None,
            new: Some(crate::server::agents::AgentState::Idle),
            kind,
            session_changed: false,
        };
        crate::server::events::emit_agent_changed(state, id, &change);
        redraw_all(state, now);
    }
}

/// Every client redraws: an agent record appeared or went.
pub(crate) fn redraw_all(state: &mut State, now: Instant) {
    for c in state.clients.values_mut() {
        c.dirty = true;
    }
    request_render(state, now);
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
        crate::server::picker::deadline(state),
        crate::server::grid::deadline(state),
        state.notify.mute_deadline(),
    ]
    .into_iter()
    .chain(state.clients.iter().map(|(id, c)| {
        let at = crate::server::toast::deadline(state, *id);
        // Remembered: once it passes the toast is gone, and `tick` must
        // still know to redraw.
        if at.is_some() {
            c.toast_until.set(at);
        }
        at
    }))
    .chain(r.owners.values().map(SizeOwner::deadline))
    .chain(clients)
    .flatten()
    .min()
}

/// Reads and clears client `id`'s redraw flag, for tests in other modules.
#[cfg(test)]
pub(crate) fn take_dirty(state: &mut State, id: ClientId) -> bool {
    state
        .clients
        .get_mut(&id)
        .is_some_and(|c| std::mem::take(&mut c.dirty))
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
        if c.toast_until.get().is_some_and(|d| d <= now) {
            c.toast_until.set(None);
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

    if state.notify.expire_mute(now) {
        for c in state.clients.values_mut() {
            c.dirty = true;
        }
        request_render(state, now);
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
    crate::server::picker::tick(state, now);
    crate::server::grid::tick(state, now);
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
pub(super) fn arm_names(state: &mut State, now: Instant) {
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
pub(super) fn arm_status_clock(state: &mut State, now: Instant) {
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
pub(super) const STATIC_STATUS_VARS: &[&str] = &[
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
    // Agent state changes arrive as events and redraw. The plain words are
    // the default formats' glyph option names and state names.
    "window_agent_state",
    "agents_badge",
    "agent",
    "glyph",
    "blocked",
    "error",
    "unseen",
    "working",
];

/// Whether format `text` expands to the same thing at any time: no
/// strftime `%`, no `#()`, and every `#{}` word a [`STATIC_STATUS_VARS`]
/// name or a number. Anything else (`#{t:...}`, `#{@user}`, loops, other
/// variables) may depend on time or on a process, so it is not.
pub(super) fn format_is_static(text: &str) -> bool {
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
pub(super) fn status_is_static(model: &Model, session: SessionId) -> bool {
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

pub(super) fn render_dirty(state: &mut State) {
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
        let fits = window_fits(state, id, c);
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
