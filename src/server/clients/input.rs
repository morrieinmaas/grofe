use super::*;

/// The global `escape-time`, in milliseconds.
pub(super) fn escape_time(model: &Model) -> Duration {
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

pub(super) fn active_pane(model: &Model, session: SessionId) -> Option<PaneId> {
    let s = model.sessions.get(&session)?;
    Some(model.windows.get(&s.active)?.active)
}

/// The terminal sequences the `user-keys` option names, by `UserN`: its
/// array's elements, as the strings they were set to.
pub(super) fn user_keys(model: &Model) -> Vec<(u16, Vec<u8>)> {
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
pub(super) fn input(state: &mut State, id: ClientId, bytes: &[u8], now: Instant) {
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
pub(super) const HELD_CAP: usize = 1 << 20;

/// One decoded input from client `id`: held behind a running shell
/// condition or binding ([`hold_input`]), else routed. An open prompt,
/// menu or popup takes it even then, as tmux hands an overlay's and the
/// prompt's keys over at once rather than queue them (a binding's
/// `run-shell` that opens a popup, as tmux-sessionx's does, gets its keys).
pub(super) fn deliver(state: &mut State, id: ClientId, input: Input, now: Instant) {
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
pub(super) enum Held {
    Input(Input),
    /// Work queued behind the input ([`in_input_order`]).
    Run(crate::server::Job),
}

impl Held {
    /// What it counts against [`HELD_CAP`].
    pub(super) fn size(&self) -> usize {
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
pub(super) fn hold_item(state: &mut State, id: ClientId, item: Held) {
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

pub(super) fn write_input(state: &State, pane: PaneId, bytes: Vec<u8>) {
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
pub(super) fn pane_in_mode(state: &State, pane: PaneId) -> bool {
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
pub(super) fn read_only_command(argv: &[String]) -> bool {
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
pub(super) fn route(state: &mut State, id: ClientId, input: Input, now: Instant) {
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
        Routed::Command(ref cmds, _) | Routed::Keyed(ref cmds, ..) | Routed::Bound(ref cmds, _)
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
        Routed::Command(cmds, dialect) => run_binding(state, id, &cmds, dialect, None),
        Routed::Keyed(cmds, dialect, key) => run_binding(state, id, &cmds, dialect, key),
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
pub fn run_binding(
    state: &mut State,
    id: ClientId,
    cmds: &[Vec<String>],
    dialect: Dialect,
    key: Option<String>,
) {
    run_commands_for(state, id, cmds, dialect, false, None, key);
}

/// One binding's commands still to run for a client, in order.
pub(crate) struct BindingRun {
    pub(super) id: ClientId,
    pub(super) cmds: VecDeque<Vec<String>>,
    pub(super) dialect: Dialect,
    /// Aliases were expanded when the key was bound.
    pub(super) bound: bool,
    /// The mouse event a mouse key's commands run for (TC.10): its pane is
    /// the current pane while it is there.
    pub(super) mouse: Option<crate::server::mouse::MouseTarget>,
    /// The pane the commands run for, if not the active one (a menu's `-t`).
    pub(super) pane: Option<PaneId>,
    /// The key whose binding runs these commands, if one does: each is
    /// logged for `show-messages` as run by it.
    pub(super) key: Option<String>,
    /// This run holds the client's input.
    pub(super) holding: bool,
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
pub(super) fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What a held [`BindingRun`] waits for; the error to show, if the held
/// commands failed.
pub(super) type BindingWait = std::pin::Pin<Box<dyn std::future::Future<Output = Waited> + Send>>;

/// How a held command ended: what it printed, and the error to show.
#[derive(Default)]
pub(super) struct Waited {
    pub(super) out: String,
    pub(super) err: Option<String>,
}

/// Runs `run`'s next commands until one makes it wait, then resumes it on
/// the actor once that is done.
pub(super) fn run_from(state: &mut State, mut run: BindingRun) {
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
            request: crate::protocol::RequestCtx {
                event_key: run.key.clone(),
                ..crate::protocol::RequestCtx::default()
            },
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
        let result = match &run.mouse {
            Some(target) => MouseState::with_current(state, Some(target.clone()), one),
            None => one(state),
        };
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
pub(super) fn binding_wait(state: &mut State, id: ClientId) -> Option<BindingWait> {
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
