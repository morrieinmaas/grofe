//! Mouse: bindable mouse keys and grofe's built-in mouse model (TC.10,
//! spec §4.5 bindings row), the read-only scrollback view and forwarding to
//! panes that asked for the mouse (T3.2, spec §4.11 "Mouse"; M5 replaces
//! the view with copy mode).
//!
//! Every report becomes a mouse key, as tmux 3.7b's
//! `server_client_check_mouse` makes one (recorded on a private server):
//! - a press is `MouseDown<b>`; a press of the same button within 300 ms of
//!   the last is `SecondClick<b>`, then `TripleClick<b>`, and 300 ms after a
//!   second click with no third comes `DoubleClick<b>`, from a timer;
//! - motion with a button held is `MouseDrag<b>`, placed where the press
//!   was for its first report and where the pointer is after that; the
//!   release that ends a drag is `MouseDragEnd<b>`, not `MouseUp<b>`;
//! - the wheel is `WheelUp` or `WheelDown`;
//! - the place is the pane under the pointer, a border, or the status line:
//!   `Status` in a window, session or user range, `StatusLeft` and
//!   `StatusRight` in those ranges, `StatusDefault` in none.
//!
//! A key bound in the client's table (else `root`) runs with the pointer's
//! target as its current pane ([`run_mouse_binding`]): the pane under it,
//! or on the status line the clicked window's active pane. While it runs,
//! `#{mouse_status_range}` and the other mouse formats ([`format_var`])
//! describe the event, and a bare `=` target (M36-R3) means the pointer's
//! target ([`resolve_targets`]). An unbound key gets the built-in model,
//! which replaces tmux's stock mouse bindings:
//! - a press focuses the pane under the pointer (`select-pane -t =`) and,
//!   if that pane asked for the mouse, is forwarded too;
//! - the wheel goes to a pane that asked for the mouse; on the alternate
//!   screen without mouse mode it does nothing (recorded: tmux sends such a
//!   pane nothing, mode 1007 or not); otherwise wheel up opens the view;
//!   this is tmux's `WheelUpPane` condition,
//!   `#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}`;
//! - in the view the wheel scrolls it 5 lines a tick and selects the pane
//!   (copy mode's `select-pane \; send -X -N 5 scroll-up`), and scrolling
//!   back to the bottom closes it (`copy-mode -e`);
//! - `MouseDown1Status` switches to the window clicked (`switch-client -t
//!   =`), and the wheel on the status line goes to the previous or next
//!   window;
//! - `MouseDrag1Border` moves the border under the pointer with it until
//!   the release (`resize-pane -M`);
//! - `q` and Escape close the view and are eaten; any other key or a paste
//!   closes it and is then routed as usual (R17);
//! - every other mouse event reaches a pane that asked for it, filtered by
//!   the mode it asked for, and nothing else.
//!
//! One divergence, from the task's acceptance case: tmux's first wheel tick
//! only enters copy mode (`[0/N]`), grofe's already scrolls 5 lines.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::protocol::{ClientCtx, Dialect};
use crate::server::clients;
use crate::server::dispatch;
use crate::server::input::{
    Input, InputHook, Key, KeyCode, Mods, Mouse, MouseAction, MouseButton, MouseKey, MouseKind,
    MousePlace, ROOT_TABLE, Routed,
};
use crate::server::layout::BorderSegment;
use crate::server::model::Window;
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status::RangeKind;
use crate::server::{Job, State};
use crate::types::{
    ClientId, Edge, InputModes, MouseEncoding, MouseMode, PaneId, Rect, SessionId, WindowId,
};

/// Lines per wheel tick in the view (tmux's `-N 5`).
pub const WHEEL_LINES: usize = 5;

/// How long a click waits for the next one (tmux's `KEYC_CLICK_TIMEOUT`).
pub const CLICK_TIMEOUT: Duration = Duration::from_millis(300);

/// The panes whose scrollback view is open, by pane; each client's place
/// in the click and drag sequence; and the mouse event a bound command is
/// running for.
#[derive(Debug, Default)]
pub struct MouseState {
    views: BTreeMap<PaneId, View>,
    pointers: BTreeMap<ClientId, Pointer>,
    /// Numbers each click wait, so a stale timer does nothing.
    generation: u64,
    /// Set while a mouse key's commands run ([`run_mouse_binding`]).
    current: Option<MouseTarget>,
}

/// An open view: `offset` lines up from the live screen, as of when the
/// pane had `history` lines of scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct View {
    offset: usize,
    history: usize,
}

/// One client's pointer, as tmux keeps it on the client.
#[derive(Debug, Default)]
struct Pointer {
    /// The last report's cell (tmux's `lx`, `ly`): where a drag's first
    /// report is placed, and where a border drag moves from.
    last: Option<(u16, u16)>,
    /// A drag in progress (tmux's `mouse_drag_flag`).
    drag: Option<Drag>,
    /// Waiting for another click (tmux's double- and triple-click flags).
    click: Option<Click>,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    button: u8,
    /// The built-in border resize follows it (tmux's `mouse_drag_update`).
    border: bool,
}

#[derive(Debug, Clone, Copy)]
struct Click {
    button: u8,
    /// A second click came: the timer fires `DoubleClick`.
    second: bool,
    generation: u64,
    /// The last press, where `DoubleClick` is placed.
    event: Mouse,
}

/// The mouse event a bound command runs for (M36-R3's "the pane or status
/// range under the current mouse event").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseTarget {
    pub client: ClientId,
    /// The client cell of the event.
    pub cell: (u16, u16),
    /// The pane commands act on: the pane under the pointer, the pane
    /// before a border, or on the status line the clicked window's active
    /// pane.
    pub pane: PaneId,
    /// What a bare `=` target means: `%N` in a pane or on a border, `@N`
    /// for a window range or any other status click (the current window),
    /// `$N` for a session range.
    pub target: String,
    /// `Some` on the status line: the range under the pointer, if any.
    pub range: Option<Option<RangeKind>>,
    /// `mouse_x` and `mouse_y`: the 0-based cell inside `pane` when the
    /// pointer is in it; on the status line the client column and the
    /// status row's index (recorded on tmux 3.7b: `5,0`, `5,1` with two
    /// rows, top or bottom).
    pub pane_cell: Option<(u16, u16)>,
}

impl MouseState {
    /// How far `pane`'s view is scrolled back now that it has `history`
    /// lines, if it is open. Lines added since the last scroll push the
    /// offset up with them, so the view keeps showing the same lines (tmux
    /// copy mode works on a copy taken on entry).
    // ponytail: once history is at its limit, new lines evict old ones
    // without growing it, and the view drifts with the output.
    pub fn offset(&self, pane: PaneId, history: usize) -> Option<usize> {
        let v = self.views.get(&pane)?;
        let grown = history.saturating_sub(v.history);
        Some((v.offset + grown).min(history))
    }

    /// Scrolls `pane`'s view by `lines` (up is positive), opening it if
    /// needed; at or below the bottom it closes. Whether the view is open
    /// afterwards.
    pub fn scroll(&mut self, pane: PaneId, history: usize, lines: isize) -> bool {
        let now = self.offset(pane, history).unwrap_or(0);
        let offset = now.saturating_add_signed(lines).min(history);
        if offset == 0 {
            self.views.remove(&pane);
            return false;
        }
        self.views.insert(pane, View { offset, history });
        true
    }

    /// Closes `pane`'s view; whether one was open.
    pub fn close(&mut self, pane: PaneId) -> bool {
        self.views.remove(&pane).is_some()
    }

    pub fn is_open(&self, pane: PaneId) -> bool {
        self.views.contains_key(&pane)
    }

    /// The mouse event the running command was bound to, if any.
    pub fn current(&self) -> Option<&MouseTarget> {
        self.current.as_ref()
    }

    /// Runs `f` with `target` as the current mouse event (a shell-form
    /// conditional's branch, later), then restores the one before.
    pub fn with_current<R>(
        state: &mut State,
        target: Option<MouseTarget>,
        f: impl FnOnce(&mut State) -> R,
    ) -> R {
        let before = std::mem::replace(&mut state.mouse.current, target);
        let out = f(state);
        state.mouse.current = before;
        out
    }
}

/// Routes mouse input (mouse keys, the built-in model, forwarding) and the
/// keys that leave the view.
pub struct MouseHook;

impl InputHook for MouseHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        let model = &state.model;
        state.mouse.views.retain(|p, _| model.panes.contains_key(p));
        let clients = &state.clients;
        state.mouse.pointers.retain(|c, _| clients.contains_key(c));
        match input {
            Input::Mouse(m) => on_mouse(state, client, m),
            Input::Key(key) => {
                let pane = active_pane(state, client)?;
                if !state.mouse.close(pane) {
                    return None;
                }
                clients::mark_all_dirty(state);
                let leaves = key.mods == Mods::default()
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Escape);
                leaves.then_some(Routed::Consumed)
            }
            Input::Paste { .. } => {
                let pane = active_pane(state, client)?;
                if state.mouse.close(pane) {
                    clients::mark_all_dirty(state);
                }
                None
            }
            Input::Focus(_) => None,
        }
    }
}

/// The active window of client `id`'s session.
fn client_window(state: &State, id: ClientId) -> Option<&Window> {
    let c = state.clients.get(&id)?;
    let s = state.model.sessions.get(&c.session)?;
    state.model.windows.get(&s.active)
}

fn active_pane(state: &State, id: ClientId) -> Option<PaneId> {
    client_window(state, id).map(|w| w.active)
}

/// The visible panes of `w` and their full rects in window coordinates
/// (only the active pane when zoomed), as the renderer lays them out.
fn pane_rects(w: &Window) -> Vec<(PaneId, Rect)> {
    if w.zoomed {
        vec![(w.active, w.area())]
    } else {
        w.layout.compute(w.area())
    }
}

/// Client cell `(col, row)` in window coordinates: through the viewport
/// (D6 crop), below the status rows when they are on top. `None` off the
/// window (on a status row).
fn window_cell(cx: &OverlayCtx, (col, row): (u16, u16)) -> Option<(u32, u32)> {
    let row = row.checked_sub(cx.window_top)?;
    if cx.window_top == 0 && cx.status_row.is_some_and(|s| row >= s) {
        return None;
    }
    Some((
        u32::from(col) + u32::from(cx.viewport.0),
        u32::from(row) + u32::from(cx.viewport.1),
    ))
}

/// The pane under window cell `(x, y)` and the 0-based cell inside it;
/// borders and letterbox belong to no pane.
fn pane_at(panes: &[(PaneId, Rect)], (x, y): (u32, u32)) -> Option<(PaneId, u16, u16)> {
    panes.iter().find_map(|&(pane, r)| {
        let (left, top) = (u32::from(r.x), u32::from(r.y));
        let inside = (left..left + u32::from(r.cols)).contains(&x)
            && (top..top + u32::from(r.rows)).contains(&y);
        inside.then(|| (pane, (x - left) as u16, (y - top) as u16))
    })
}

/// Whether window cell `(x, y)` is on one of `borders`.
fn on_border(borders: &[BorderSegment], (x, y): (u32, u32)) -> bool {
    borders.iter().any(|b| match *b {
        BorderSegment::Vertical { col, row, len } => {
            x == u32::from(col) && (u32::from(row)..u32::from(row) + u32::from(len)).contains(&y)
        }
        BorderSegment::Horizontal { row, col, len } => {
            y == u32::from(row) && (u32::from(col)..u32::from(col) + u32::from(len)).contains(&x)
        }
    })
}

/// The pane a border cell belongs to, as tmux's `window_get_active_at`
/// finds it: the pane whose right or bottom edge the border runs along.
fn border_pane(panes: &[(PaneId, Rect)], (x, y): (u32, u32)) -> Option<PaneId> {
    panes.iter().find_map(|&(pane, r)| {
        let (left, top) = (u32::from(r.x), u32::from(r.y));
        let (right, bottom) = (left + u32::from(r.cols), top + u32::from(r.rows));
        let inside = (left..=right).contains(&x) && (top..=bottom).contains(&y);
        inside.then_some(pane)
    })
}

/// What the pointer is over.
enum Hit {
    Pane(PaneId, u16, u16),
    Border(PaneId),
    /// The status row's index among the status rows, and the range.
    Status(u16, Option<RangeKind>),
    Nothing,
}

fn hit(state: &State, client: ClientId, cell: (u16, u16)) -> Hit {
    if let Some((line, range)) = clients::status_at(state, client, cell.0, cell.1) {
        return Hit::Status(line, range);
    }
    let (Some(cx), Some(w)) = (
        clients::overlay_ctx(state, client),
        client_window(state, client),
    ) else {
        return Hit::Nothing;
    };
    let Some(at) = window_cell(&cx, cell) else {
        return Hit::Nothing;
    };
    // A floating pane is over the layout, the top one first; its border
    // belongs to no pane (TC.6).
    if !w.zoomed {
        let mut stack = w.floating.clone();
        stack.sort_by_key(|f| std::cmp::Reverse(f.z));
        let (x, y) = (i64::from(at.0), i64::from(at.1));
        for f in stack {
            let (dx, dy) = (x - i64::from(f.x), y - i64::from(f.y));
            if (0..i64::from(f.cols)).contains(&dx) && (0..i64::from(f.rows)).contains(&dy) {
                return Hit::Pane(f.pane, dx as u16, dy as u16);
            }
            if (-1..=i64::from(f.cols)).contains(&dx) && (-1..=i64::from(f.rows)).contains(&dy) {
                return Hit::Nothing;
            }
        }
    }
    let panes = pane_rects(w);
    if let Some((pane, x, y)) = pane_at(&panes, at) {
        return Hit::Pane(pane, x, y);
    }
    if !w.zoomed && on_border(&w.layout.borders(w.area()), at) {
        return border_pane(&panes, at).map_or(Hit::Nothing, Hit::Border);
    }
    Hit::Nothing
}

fn place(hit: &Hit) -> Option<MousePlace> {
    Some(match hit {
        Hit::Pane(..) => MousePlace::Pane,
        Hit::Border(_) => MousePlace::Border,
        Hit::Status(_, None) => MousePlace::StatusDefault,
        Hit::Status(_, Some(RangeKind::Left)) => MousePlace::StatusLeft,
        Hit::Status(_, Some(RangeKind::Right)) => MousePlace::StatusRight,
        Hit::Status(_, Some(_)) => MousePlace::Status,
        Hit::Nothing => return None,
    })
}

/// The target of an event at client cell `cell` over `hit`.
fn target(state: &State, client: ClientId, cell: (u16, u16), hit: &Hit) -> Option<MouseTarget> {
    let session = state.clients.get(&client)?.session;
    let current = state.model.sessions.get(&session)?.active;
    let window_target = |w: WindowId| {
        let w = state.model.windows.get(&w)?;
        Some((w.active, w.id.to_string()))
    };
    let session_target = |s: SessionId| {
        let active = state.model.sessions.get(&s)?.active;
        Some((window_target(active)?.0, s.to_string()))
    };
    let (pane, target, range, pane_cell) = match hit {
        Hit::Pane(p, x, y) => (*p, p.to_string(), None, Some((*x, *y))),
        Hit::Border(p) => (*p, p.to_string(), None, None),
        Hit::Status(line, range) => {
            let (pane, target) = match range {
                Some(RangeKind::Window(index)) => state
                    .model
                    .window_at_index(session, *index)
                    .and_then(window_target),
                Some(RangeKind::Session(id)) => session_target(SessionId(*id)),
                Some(RangeKind::Pane(id)) => state
                    .model
                    .panes
                    .contains_key(&PaneId(*id))
                    .then(|| (PaneId(*id), PaneId(*id).to_string())),
                _ => None,
            }
            .or_else(|| window_target(current))?;
            (pane, target, Some(range.clone()), Some((cell.0, *line)))
        }
        Hit::Nothing => return None,
    };
    Some(MouseTarget {
        client,
        cell,
        pane,
        target,
        range,
        pane_cell,
    })
}

/// tmux's button number for `b`.
fn button_number(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Other(n) => n,
    }
}

/// Turns report `m` into the mouse key it fires and the cell that key is
/// placed at, stepping client `client`'s click and drag state as tmux's
/// `server_client_check_mouse` does. `None` for a report that is no key
/// (motion with no button, horizontal wheel) or that a border drag takes.
fn classify(
    state: &mut State,
    client: ClientId,
    m: &Mouse,
) -> Option<(MouseAction, u8, (u16, u16))> {
    let at = (m.col, m.row);
    let generation = state.mouse.generation + 1;
    let p = state.mouse.pointers.entry(client).or_default();
    let last = p.last.replace(at);
    let mut waits = false;
    let key = match m.kind {
        MouseKind::Move | MouseKind::WheelLeft | MouseKind::WheelRight => return None,
        MouseKind::WheelUp => (MouseAction::WheelUp, 0, at),
        MouseKind::WheelDown => (MouseAction::WheelDown, 0, at),
        MouseKind::Up(b) => match p.drag.take() {
            Some(d) => (MouseAction::DragEnd, d.button, at),
            None => (MouseAction::Up, button_number(b), at),
        },
        MouseKind::Drag(b) => match p.drag {
            Some(Drag { border: true, .. }) => return None,
            Some(d) => (MouseAction::Drag, d.button, at),
            None => {
                let button = button_number(b);
                p.drag = Some(Drag {
                    button,
                    border: false,
                });
                (MouseAction::Drag, button, last.unwrap_or(at))
            }
        },
        MouseKind::Down(b) => {
            let button = button_number(b);
            let action = match p.click.take() {
                Some(c) if c.button == button && !c.second => MouseAction::SecondClick,
                Some(c) if c.button == button => MouseAction::TripleClick,
                _ => MouseAction::Down,
            };
            if action != MouseAction::TripleClick {
                p.click = Some(Click {
                    button,
                    second: action == MouseAction::SecondClick,
                    generation,
                    event: *m,
                });
                waits = true;
            }
            (action, button, at)
        }
    };
    if waits {
        state.mouse.generation = generation;
        start_click_timer(state, client, generation);
    }
    Some(key)
}

/// After [`CLICK_TIMEOUT`], ends client `client`'s click wait `generation`:
/// a second click with no third is a `DoubleClick`.
fn start_click_timer(state: &State, client: ClientId, generation: u64) {
    // Without a runtime (unit tests) the wait ends only with a click.
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let jobs = state.jobs.clone();
    rt.spawn(async move {
        tokio::time::sleep(CLICK_TIMEOUT).await;
        let job: Job = Box::new(move |state: &mut State| click_timeout(state, client, generation));
        let _ = jobs.send(job).await;
    });
}

/// The click timer: see [`start_click_timer`].
pub(crate) fn click_timeout(state: &mut State, client: ClientId, generation: u64) {
    let Some(p) = state.mouse.pointers.get_mut(&client) else {
        return;
    };
    let Some(c) = p.click.filter(|c| c.generation == generation) else {
        return;
    };
    p.click = None;
    if !c.second {
        return;
    }
    // Behind any input a shell condition holds, as tmux queues the key.
    let fire_double = move |state: &mut State| {
        if !mouse_on(state, client) {
            return;
        }
        let at = (c.event.col, c.event.row);
        fire(
            state,
            client,
            &c.event,
            (MouseAction::DoubleClick, c.button, at),
        );
    };
    clients::in_input_order(state, client, Box::new(fire_double));
}

fn mouse_on(state: &State, client: ClientId) -> bool {
    let Some(session) = state.clients.get(&client).map(|c| c.session) else {
        return false;
    };
    state.model.options.lookup(
        "mouse",
        &[crate::server::dispatch::options::Owner::Session(session)],
    ) == Some("on")
}

fn on_mouse(state: &mut State, client: ClientId, m: &Mouse) -> Option<Routed> {
    if !mouse_on(state, client) {
        // The key tables drop it, as with no mouse at all.
        return None;
    }
    // A client showing no window has nothing to point at.
    client_window(state, client)?;
    let from = state.mouse.pointers.get(&client).and_then(|p| p.last);
    let Some(key) = classify(state, client, m) else {
        let border = state
            .mouse
            .pointers
            .get(&client)
            .and_then(|p| p.drag)
            .is_some_and(|d| d.border);
        if border && let Some(from) = from {
            drag_border(state, client, from, (m.col, m.row));
            return Some(Routed::Consumed);
        }
        return Some(forward(state, client, m));
    };
    fire(state, client, m, key);
    Some(Routed::Consumed)
}

/// Runs mouse key `(action, button)` placed at `at` for report `m`: its
/// binding if it has one, else the built-in model.
fn fire(
    state: &mut State,
    client: ClientId,
    m: &Mouse,
    (action, button, at): (MouseAction, u8, (u16, u16)),
) {
    let hit = hit(state, client, at);
    let Some(place) = place(&hit) else {
        return;
    };
    let Some(target) = target(state, client, at, &hit) else {
        return;
    };
    let key = Key::new(
        KeyCode::Mouse(MouseKey {
            action,
            button,
            place,
        }),
        m.mods,
    );
    if let Some((cmds, dialect)) = binding(state, client, &key) {
        run_mouse_binding(state, &target, &cmds, dialect);
        return;
    }
    builtin(state, client, m, &key, &target);
}

/// The binding of mouse key `key` in client `client`'s table, else in
/// `root`, as tmux's key walk finds it. Any table but `root` ends with
/// the event.
fn binding(state: &mut State, client: ClientId, key: &Key) -> Option<(Vec<Vec<String>>, Dialect)> {
    let table = state.clients.get(&client)?.keys.table().to_string();
    if table != ROOT_TABLE {
        clients::set_key_table(state, client, ROOT_TABLE);
    }
    let tables = &state.registry.key_tables;
    // ponytail: a root `Any` also takes the presses the built-in model
    // handles, where tmux's stock MouseDown1Pane and the rest are bound
    // and win over it; no config the owner runs binds root `Any`.
    let b = tables
        .lookup(&table, key)
        .or_else(|| tables.lookup(ROOT_TABLE, key))?;
    Some((b.cmds.clone(), b.dialect))
}

/// grofe's built-in model for an unbound mouse key (the module docs).
fn builtin(state: &mut State, client: ClientId, m: &Mouse, key: &Key, target: &MouseTarget) {
    let KeyCode::Mouse(mk) = key.code else {
        return;
    };
    let run = |state: &mut State, argv: &[&str]| {
        let cmds = vec![argv.iter().map(|s| s.to_string()).collect()];
        run_mouse_binding(state, target, &cmds, Dialect::Grofe);
    };
    match (mk.place, mk.action, mk.button) {
        (MousePlace::Status, MouseAction::Down, 1) => run(state, &["switch-client", "-t", "="]),
        (MousePlace::Status, MouseAction::WheelUp, _) => run(state, &["previous-window"]),
        (MousePlace::Status, MouseAction::WheelDown, _) => run(state, &["next-window"]),
        (MousePlace::Border, MouseAction::Drag, 1) => {
            if let Some(p) = state.mouse.pointers.get_mut(&client) {
                p.drag = Some(Drag {
                    button: 1,
                    border: true,
                });
            }
            // As `resize-pane -M`: the first report already moves it.
            let from = target.cell;
            drag_border(state, client, from, (m.col, m.row));
        }
        (MousePlace::Pane, MouseAction::DoubleClick, _) => {}
        (MousePlace::Pane, _, _) => {
            let _ = in_pane(state, client, m, target.pane);
        }
        _ => {}
    }
}

/// The built-in model for a report in pane `pane` (T3.2): a press focuses
/// it; the wheel goes to a pane that asked for the mouse, scrolls an open
/// view, or opens one; anything else the pane asked for is forwarded.
fn in_pane(state: &mut State, client: ClientId, m: &Mouse, pane: PaneId) -> Option<()> {
    let p = state.model.panes.get(&pane)?;
    let modes = p.handle.modes();
    let history = p.handle.history_size();
    let in_view = state.mouse.is_open(pane);
    let wants = modes.mouse != MouseMode::Off && !in_view;
    let focus = vec![vec!["select-pane".into(), "-t".into(), pane.to_string()]];
    let wheel = match m.kind {
        MouseKind::WheelUp => Some(WHEEL_LINES as isize),
        MouseKind::WheelDown => Some(-(WHEEL_LINES as isize)),
        _ => None,
    };
    if let Some(lines) = wheel
        && !wants
    {
        if in_view {
            state.mouse.scroll(pane, history, lines);
            clients::mark_all_dirty(state);
            clients::run_binding(state, client, &focus, Dialect::Grofe);
        } else if lines > 0 && !modes.alt_screen && history > 0 {
            state.mouse.scroll(pane, history, lines);
            clients::mark_all_dirty(state);
        }
        return Some(());
    }
    // As tmux (`select-pane -t = \; send-keys -M`): the pane is active
    // before its click arrives.
    if matches!(m.kind, MouseKind::Down(_)) {
        clients::run_binding(state, client, &focus, Dialect::Grofe);
    }
    forward(state, client, m);
    Some(())
}

/// Sends report `m` to the pane under it, if that pane asked for this
/// kind of event and has no view open.
// ponytail: a drag that leaves its pane is dropped rather than following
// the pane it started in.
fn forward(state: &mut State, client: ClientId, m: &Mouse) -> Routed {
    let Hit::Pane(pane, x, y) = hit(state, client, (m.col, m.row)) else {
        return Routed::Consumed;
    };
    if state.mouse.is_open(pane) {
        return Routed::Consumed;
    }
    let Some(p) = state.model.panes.get(&pane) else {
        return Routed::Consumed;
    };
    if let Some(bytes) = encode(m, x, y, &p.handle.modes())
        && p.handle.enqueue_input(bytes).is_err()
    {
        eprintln!("grofe server: input queue full for {pane}");
    }
    Routed::Consumed
}

/// A border drag from client cell `from` to `to`, as tmux 3.7b's
/// `cmd_resize_pane_mouse_update`: a pane whose right edge is the border
/// under `from` grows by the columns moved, one whose bottom edge is grows
/// by the rows, each through [`crate::layout::LayoutTree::resize`]. A
/// border the clamp kept back no longer lies under the pointer, so it
/// waits until the pointer comes back to it (recorded).
fn drag_border(state: &mut State, client: ClientId, from: (u16, u16), to: (u16, u16)) {
    let Some(cx) = clients::overlay_ctx(state, client) else {
        return;
    };
    let Some(w) = client_window(state, client) else {
        return;
    };
    if w.zoomed {
        return;
    }
    let window = w.id;
    // As tmux: a pointer off the window (on the status line) drags along
    // its nearest row.
    let clamp = |(col, row): (u16, u16)| {
        let top = cx.window_top;
        let rows = w.rows.max(1);
        let row = row.saturating_sub(top).min(rows - 1);
        (
            i32::from(col) + i32::from(cx.viewport.0),
            i32::from(row) + i32::from(cx.viewport.1),
        )
    };
    let ((lx, ly), (x, y)) = (clamp(from), clamp(to));
    let area = w.area();
    let panes = pane_rects(w);
    let edge = |along_x: bool| {
        panes.iter().find_map(|&(pane, r)| {
            let (left, top) = (i32::from(r.x), i32::from(r.y));
            let (right, bottom) = (left + i32::from(r.cols), top + i32::from(r.rows));
            let hit = if along_x {
                right == lx && top <= ly + 1 && bottom >= ly
            } else {
                bottom == ly && left <= lx + 1 && right >= lx
            };
            hit.then_some(pane)
        })
    };
    let right = edge(true).filter(|_| x != lx);
    let down = edge(false).filter(|_| y != ly);
    if right.is_none() && down.is_none() {
        return;
    }
    if let Some(w) = state.model.windows.get_mut(&window) {
        if let Some(pane) = right {
            w.layout.resize(area, pane, Edge::Right, x - lx);
        }
        if let Some(pane) = down {
            w.layout.resize(area, pane, Edge::Down, y - ly);
        }
    }
    state.model.relayout(window);
    clients::changed(state);
}

/// Runs mouse-bound `cmds` in `dialect` for the event at `target`, as
/// [`clients::run_binding`] runs a key's, but with the pointer's pane as
/// the current pane while it is there, the event as the current mouse
/// event, and a bare `=` resolved to the pointer's target (M36-R3). The
/// first error stops the rest and shows on the client's status row.
pub fn run_mouse_binding(
    state: &mut State,
    target: &MouseTarget,
    cmds: &[Vec<String>],
    dialect: Dialect,
) {
    let id = target.client;
    MouseState::with_current(state, Some(target.clone()), |state| {
        for argv in cmds {
            let Some(c) = state.clients.get(&id) else {
                break;
            };
            let pane = Some(target.pane)
                .filter(|p| state.model.panes.contains_key(p))
                .or_else(|| active_pane(state, id));
            let ctx = ClientCtx {
                pane,
                dialect,
                ..c.ctx.clone()
            };
            let argv = resolve_targets(state, argv);
            // Bound commands: their aliases expanded at bind time.
            let result = dispatch::run_bound(state, &ctx, &argv);
            state.deferred_reply = None;
            if let Err(e) = result {
                clients::show_message(state, id, e.trim_end());
                break;
            }
        }
    });
    state.events.flush();
    clients::changed(state);
}

/// `argv` with each bare `=` target (`-t =`, `-t=`, and the same for `-s`)
/// replaced by the current mouse event's target (M36-R3). Outside a mouse
/// key's commands a bare `=` is left as it is, the literal session name it
/// is natively, which no session can have.
pub fn resolve_targets(state: &State, argv: &[String]) -> Vec<String> {
    let Some(m) = &state.mouse.current else {
        return argv.to_vec();
    };
    let mut out = Vec::with_capacity(argv.len());
    let mut after_flag = false;
    for word in argv {
        let replaced = match word.as_str() {
            "=" if after_flag => m.target.clone(),
            "-t=" | "-s=" => format!("{}{}", &word[..2], m.target),
            _ => word.clone(),
        };
        after_flag = matches!(word.as_str(), "-t" | "-s");
        out.push(replaced);
    }
    out
}

/// A format variable the mouse answers: while a mouse key's commands run,
/// `mouse_status_range` (tmux's words for the range clicked, empty off the
/// status line or outside a range), `mouse_pane`, and `mouse_x`/`mouse_y`
/// ([`MouseTarget::pane_cell`]); and always
/// `pane_in_mode` for `pane` (1 while its scrollback view is open). `None`
/// lets the caller's own variables answer.
pub fn format_var(state: &State, pane: Option<PaneId>, name: &str) -> Option<String> {
    if name == "pane_in_mode" {
        let open = pane.is_some_and(|p| state.mouse.is_open(p));
        return Some(if open { "1" } else { "0" }.to_string());
    }
    let m = state.mouse.current.as_ref()?;
    match name {
        "mouse_status_range" => Some(match &m.range {
            Some(Some(range)) => range.mouse_status_range(),
            _ => String::new(),
        }),
        "mouse_pane" => Some(m.pane.to_string()),
        "mouse_x" => m.pane_cell.map(|(x, _)| x.to_string()),
        "mouse_y" => m.pane_cell.map(|(_, y)| y.to_string()),
        _ => None,
    }
}

/// The client cell of the mouse event client `client`'s running command
/// was bound to: where a menu a mouse key opens goes (spec §4.5, TC.7's
/// `menu::open` takes it as its position).
pub fn pointer(state: &State, client: ClientId) -> Option<(u16, u16)> {
    state
        .mouse
        .current
        .as_ref()
        .filter(|m| m.client == client)
        .map(|m| m.cell)
}

/// Encodes `m` at 0-based pane cell `(x, y)` for a pane with `modes`, as
/// tmux's `input_key_mouse`: `None` when the pane's mouse mode does not
/// report this kind of event, or the encoding cannot carry it. X10 clamps
/// a position past its reach to 255 and drops only a button byte that
/// overflows; UTF-8 drops a value past U+07FF.
pub fn encode(m: &Mouse, x: u16, y: u16, modes: &InputModes) -> Option<Vec<u8>> {
    let reported = match m.kind {
        MouseKind::Drag(_) => matches!(modes.mouse, MouseMode::Drag | MouseMode::Motion),
        MouseKind::Move => modes.mouse == MouseMode::Motion,
        _ => modes.mouse != MouseMode::Off,
    };
    if !reported {
        return None;
    }
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::Other(n) => 128 + u32::from(n.saturating_sub(8) & 3),
    };
    let sgr = modes.mouse_encoding == MouseEncoding::Sgr;
    let mut b = match m.kind {
        MouseKind::Down(btn) => button(btn),
        // Legacy encodings cannot say which button went up.
        MouseKind::Up(btn) if sgr => button(btn),
        MouseKind::Up(_) => 3,
        MouseKind::Drag(btn) => 32 + button(btn),
        MouseKind::Move => 35,
        MouseKind::WheelUp => 64,
        MouseKind::WheelDown => 65,
        MouseKind::WheelLeft => 66,
        MouseKind::WheelRight => 67,
    };
    if m.mods.shift {
        b |= 4;
    }
    if m.mods.alt {
        b |= 8;
    }
    if m.mods.ctrl {
        b |= 16;
    }
    let (x, y) = (u32::from(x) + 1, u32::from(y) + 1);
    match modes.mouse_encoding {
        MouseEncoding::Sgr => {
            let end = if matches!(m.kind, MouseKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            Some(format!("\x1b[<{b};{x};{y}{end}").into_bytes())
        }
        MouseEncoding::X10 => {
            let clamp = |v: u32| u8::try_from(v + 32).unwrap_or(u8::MAX);
            let button = u8::try_from(b + 32).ok()?;
            Some(vec![0x1b, b'[', b'M', button, clamp(x), clamp(y)])
        }
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for v in [b, x, y] {
                let c = char::from_u32(v + 32).filter(|c| (*c as u32) < 0x800)?;
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
    }
}

/// Draws each visible pane's open view: its history rows over the pane,
/// and the `[offset/history]` indicator top right in tmux's `mode-style`
/// (black on yellow). The cursor hides while the client's active pane is
/// scrolled back.
// ponytail: tmux shows a copy-mode cursor it can move; the view has none.
pub struct ScrollbackOverlay;

impl Overlay for ScrollbackOverlay {
    fn draw(&self, state: &State, client: ClientId, cx: &OverlayCtx, screen: &mut Screen) {
        if state.mouse.views.is_empty() {
            return;
        }
        let Some(w) = client_window(state, client) else {
            return;
        };
        for (pane, rect) in pane_rects(w) {
            let Some(p) = state.model.panes.get(&pane) else {
                continue;
            };
            let history = p.handle.history_size();
            let offset = match state.mouse.offset(pane, history) {
                Some(n) if n > 0 => n,
                _ => continue,
            };
            let Some(&(_, clip)) = cx.panes.iter().find(|(id, _)| *id == pane) else {
                continue;
            };
            let mut put = |x: u16, y: u16, cell: &SnapCell| {
                let cx_ = i32::from(rect.x) + i32::from(x) - i32::from(cx.viewport.0);
                let cy_ = i32::from(rect.y) + i32::from(y) - i32::from(cx.viewport.1)
                    + i32::from(cx.window_top);
                let inside = |v: i32, lo: u16, len: u16| {
                    v >= i32::from(lo) && v < i32::from(lo) + i32::from(len)
                };
                if inside(cx_, clip.x, clip.cols) && inside(cy_, clip.y, clip.rows) {
                    let i = cy_ as usize * usize::from(screen.cols) + cx_ as usize;
                    if let Some(slot) = screen.cells.get_mut(i) {
                        *slot = cell.clone();
                    }
                }
            };
            for (y, row) in p
                .handle
                .snapshot_history(offset, rect.rows)
                .iter()
                .enumerate()
            {
                for (x, cell) in row.iter().take(usize::from(rect.cols)).enumerate() {
                    put(x as u16, y as u16, cell);
                }
            }
            let label = format!("[{offset}/{history}]");
            let start = rect.cols.saturating_sub(label.len() as u16);
            for (i, c) in label.chars().take(usize::from(rect.cols)).enumerate() {
                put(start + i as u16, 0, &indicator_cell(c));
            }
            if pane == w.active {
                screen.cursor = None;
            }
        }
    }
}

fn indicator_cell(c: char) -> SnapCell {
    SnapCell {
        c,
        fg: Color::Named(NamedColor::Black),
        bg: Color::Named(NamedColor::Yellow),
        flags: CellFlags::empty(),
        zerowidth: Vec::new(),
        underline_color: None,
        hyperlink: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(kind: MouseKind) -> Mouse {
        Mouse {
            kind,
            col: 0,
            row: 0,
            mods: Mods::default(),
        }
    }

    fn modes(mouse: MouseMode, mouse_encoding: MouseEncoding) -> InputModes {
        InputModes {
            mouse,
            mouse_encoding,
            ..InputModes::default()
        }
    }

    const LEFT: MouseButton = MouseButton::Left;

    /// Recorded on tmux 3.7b: what a pane gets for each mode and encoding.
    #[test]
    fn encodes_as_tmux() {
        let sgr = modes(MouseMode::Click, MouseEncoding::Sgr);
        let x10 = modes(MouseMode::Click, MouseEncoding::X10);
        let enc = |kind, x, y, m: &InputModes| encode(&mouse(kind), x, y, m);
        assert_eq!(
            enc(MouseKind::WheelUp, 9, 4, &sgr).as_deref(),
            Some(&b"\x1b[<64;10;5M"[..])
        );
        assert_eq!(
            enc(MouseKind::Down(LEFT), 2, 3, &sgr).as_deref(),
            Some(&b"\x1b[<0;3;4M"[..])
        );
        assert_eq!(
            enc(MouseKind::Up(LEFT), 2, 3, &sgr).as_deref(),
            Some(&b"\x1b[<0;3;4m"[..])
        );
        assert_eq!(
            enc(MouseKind::WheelUp, 9, 4, &x10).as_deref(),
            Some(&b"\x1b[M`*%"[..])
        );
        assert_eq!(
            enc(MouseKind::Down(LEFT), 2, 3, &x10).as_deref(),
            Some(&b"\x1b[M #$"[..])
        );
        assert_eq!(
            enc(MouseKind::Up(LEFT), 2, 3, &x10).as_deref(),
            Some(&b"\x1b[M##$"[..])
        );
        let utf8 = modes(MouseMode::Click, MouseEncoding::Utf8);
        assert_eq!(
            enc(MouseKind::Up(LEFT), 2, 3, &utf8).as_deref(),
            Some(&b"\x1b[M##$"[..])
        );
        // Past X10's reach the position clamps to 255 (recorded: a press
        // and release at client column 260); UTF-8 carries it in two bytes.
        assert_eq!(
            enc(MouseKind::Down(LEFT), 259, 4, &x10).as_deref(),
            Some(&b"\x1b[M \xff%"[..])
        );
        assert_eq!(
            enc(MouseKind::Up(LEFT), 259, 4, &x10).as_deref(),
            Some(&b"\x1b[M#\xff%"[..])
        );
        assert_eq!(enc(MouseKind::Down(LEFT), 2100, 3, &utf8), None);
        assert_eq!(
            enc(MouseKind::Down(LEFT), 200, 3, &utf8).as_deref(),
            Some(&b"\x1b[M \xc3\xa9$"[..])
        );
        let mut ctrl_shift = mouse(MouseKind::Down(LEFT));
        ctrl_shift.mods.ctrl = true;
        ctrl_shift.mods.shift = true;
        assert_eq!(
            encode(&ctrl_shift, 2, 3, &sgr).as_deref(),
            Some(&b"\x1b[<20;3;4M"[..])
        );
    }

    /// Mode 1000 reports presses, releases and the wheel; 1002 adds drags;
    /// 1003 adds motion with no button; off reports nothing.
    #[test]
    fn filters_by_mouse_mode() {
        let at = |mode, kind| encode(&mouse(kind), 4, 3, &modes(mode, MouseEncoding::Sgr));
        assert_eq!(at(MouseMode::Off, MouseKind::WheelUp), None);
        assert_eq!(at(MouseMode::Click, MouseKind::Drag(LEFT)), None);
        assert_eq!(at(MouseMode::Click, MouseKind::Move), None);
        assert_eq!(
            at(MouseMode::Drag, MouseKind::Drag(LEFT)).as_deref(),
            Some(&b"\x1b[<32;5;4M"[..])
        );
        assert_eq!(at(MouseMode::Drag, MouseKind::Move), None);
        assert_eq!(
            at(MouseMode::Motion, MouseKind::Move).as_deref(),
            Some(&b"\x1b[<35;5;4M"[..])
        );
    }

    fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
        Rect { x, y, cols, rows }
    }

    fn cx(viewport: (u16, u16), status_row: Option<u16>, window_top: u16) -> OverlayCtx {
        OverlayCtx {
            client: ClientId(1),
            viewport,
            status_row,
            window_top,
            panes: Vec::new(),
        }
    }

    fn at(
        panes: &[(PaneId, Rect)],
        cx: &OverlayCtx,
        cell: (u16, u16),
    ) -> Option<(PaneId, u16, u16)> {
        pane_at(panes, window_cell(cx, cell)?)
    }

    #[test]
    fn maps_a_pointer_through_the_viewport_to_a_pane() {
        let panes = [
            (PaneId(1), rect(0, 0, 59, 23)),
            (PaneId(2), rect(60, 0, 20, 23)),
        ];
        let bottom = cx((0, 0), Some(23), 0);
        assert_eq!(at(&panes, &bottom, (3, 2)), Some((PaneId(1), 3, 2)));
        assert_eq!(at(&panes, &bottom, (65, 2)), Some((PaneId(2), 5, 2)));
        // The border column, the status row, a letterbox cell.
        assert_eq!(at(&panes, &bottom, (59, 2)), None);
        assert_eq!(at(&panes, &bottom, (3, 23)), None);
        assert_eq!(at(&panes, &cx((0, 0), None, 0), (3, 30)), None);
        // Cropped at x = 40: client column 30 is window column 70.
        let cropped = cx((40, 1), Some(11), 0);
        assert_eq!(at(&panes, &cropped, (30, 2)), Some((PaneId(2), 10, 3)));
        assert_eq!(at(&panes, &cropped, (30, 11)), None);
    }

    /// TC.11's note: with the status line on top, the window starts below
    /// it, so client row 1 is window row 0.
    #[test]
    fn maps_a_pointer_below_a_status_line_on_top() {
        let panes = [(PaneId(1), rect(0, 0, 80, 23))];
        let top = cx((0, 0), Some(0), 1);
        assert_eq!(at(&panes, &top, (3, 0)), None);
        assert_eq!(at(&panes, &top, (3, 1)), Some((PaneId(1), 3, 0)));
        assert_eq!(at(&panes, &top, (3, 23)), Some((PaneId(1), 3, 22)));
        let two = cx((0, 0), Some(0), 2);
        assert_eq!(at(&panes, &two, (3, 1)), None);
        assert_eq!(at(&panes, &two, (3, 2)), Some((PaneId(1), 3, 0)));
    }

    #[test]
    fn finds_the_border_under_the_pointer_and_its_pane() {
        let panes = [
            (PaneId(1), rect(0, 0, 40, 23)),
            (PaneId(2), rect(41, 0, 39, 11)),
            (PaneId(3), rect(41, 12, 39, 11)),
        ];
        let borders = [
            BorderSegment::Vertical {
                col: 40,
                row: 0,
                len: 23,
            },
            BorderSegment::Horizontal {
                row: 11,
                col: 41,
                len: 39,
            },
        ];
        assert!(on_border(&borders, (40, 5)));
        assert!(on_border(&borders, (50, 11)));
        assert!(!on_border(&borders, (39, 5)));
        assert!(!on_border(&borders, (40, 23)));
        assert_eq!(border_pane(&panes, (40, 5)), Some(PaneId(1)));
        assert_eq!(border_pane(&panes, (50, 11)), Some(PaneId(2)));
    }

    fn report(kind: MouseKind, col: u16, row: u16) -> Mouse {
        Mouse {
            kind,
            col,
            row,
            mods: Mods::default(),
        }
    }

    /// The keys tmux 3.7b fired for each sequence (recorded: every mouse
    /// key bound to append its name to an option).
    #[test]
    fn reports_become_tmux_mouse_keys() {
        let mut state = crate::server::test_state();
        let c = ClientId(1);
        let mut keys = |kinds: &[(MouseKind, u16, u16)]| -> Vec<(MouseAction, u8, (u16, u16))> {
            kinds
                .iter()
                .filter_map(|&(k, x, y)| classify(&mut state, c, &report(k, x, y)))
                .collect()
        };
        let down = MouseKind::Down(LEFT);
        let up = MouseKind::Up(LEFT);
        use MouseAction::*;
        // Single, double (DoubleClick comes from the timer), triple, and a
        // fourth click starting over.
        assert_eq!(
            keys(&[(down, 9, 4), (up, 9, 4)]),
            vec![(Down, 1, (9, 4)), (Up, 1, (9, 4))]
        );
        assert_eq!(
            keys(&[(down, 9, 4), (up, 9, 4)]),
            vec![(SecondClick, 1, (9, 4)), (Up, 1, (9, 4))]
        );
        assert_eq!(
            keys(&[(down, 9, 4), (up, 9, 4), (down, 9, 4)]),
            vec![(TripleClick, 1, (9, 4)), (Up, 1, (9, 4)), (Down, 1, (9, 4))]
        );
        // Another button starts over.
        let right = MouseKind::Down(MouseButton::Right);
        assert_eq!(keys(&[(right, 9, 4)]), vec![(Down, 3, (9, 4))]);
        // A drag's first report is placed where the press was, later ones
        // where the pointer is; its release is the drag's end, not an up.
        assert_eq!(
            keys(&[
                (down, 40, 4),
                (MouseKind::Drag(LEFT), 42, 4),
                (MouseKind::Drag(LEFT), 45, 5),
                (up, 45, 5),
                (up, 45, 5),
            ]),
            vec![
                (Down, 1, (40, 4)),
                (Drag, 1, (40, 4)),
                (Drag, 1, (45, 5)),
                (DragEnd, 1, (45, 5)),
                (Up, 1, (45, 5)),
            ]
        );
        assert_eq!(
            keys(&[(MouseKind::WheelUp, 3, 3), (MouseKind::Move, 3, 3)]),
            vec![(WheelUp, 0, (3, 3))]
        );
    }

    /// The timer turns a second click with no third into `DoubleClick`; a
    /// stale timer, or one after a single click, ends the wait only.
    #[test]
    fn the_click_timer_ends_the_wait() {
        let mut state = crate::server::test_state();
        let c = ClientId(1);
        let down = report(MouseKind::Down(LEFT), 2, 2);
        classify(&mut state, c, &down);
        let first = state.mouse.generation;
        classify(&mut state, c, &down);
        let second = state.mouse.generation;
        click_timeout(&mut state, c, first);
        assert!(state.mouse.pointers[&c].click.is_some(), "stale timer");
        click_timeout(&mut state, c, second);
        assert!(state.mouse.pointers[&c].click.is_none());
        // The next press is a fresh click.
        assert_eq!(
            classify(&mut state, c, &down).map(|k| k.0),
            Some(MouseAction::Down)
        );
    }

    fn mouse_target(target: &str, range: Option<Option<RangeKind>>) -> MouseTarget {
        MouseTarget {
            client: ClientId(1),
            cell: (5, 23),
            pane: PaneId(4),
            target: target.to_string(),
            range,
            pane_cell: None,
        }
    }

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    /// M36-R3: a bare `=` means the mouse event's target only while a
    /// mouse key's commands run.
    #[test]
    fn a_bare_equals_is_the_mouse_target_only_in_a_mouse_binding() {
        let mut state = crate::server::test_state();
        let argv = words(&["switch-client", "-t", "="]);
        assert_eq!(resolve_targets(&state, &argv), argv);
        state.mouse.current = Some(mouse_target("@3", Some(None)));
        assert_eq!(
            resolve_targets(&state, &argv),
            words(&["switch-client", "-t", "@3"])
        );
        assert_eq!(
            resolve_targets(&state, &words(&["copy-mode", "-t="])),
            words(&["copy-mode", "-t@3"])
        );
        assert_eq!(
            resolve_targets(&state, &words(&["swap-pane", "-s", "=", "-t", "=x"])),
            words(&["swap-pane", "-s", "@3", "-t", "=x"])
        );
        // Only a flag's value: a literal `=` argument stays.
        assert_eq!(
            resolve_targets(&state, &words(&["send-keys", "-l", "="])),
            words(&["send-keys", "-l", "="])
        );
    }

    #[test]
    fn mouse_formats_describe_the_running_event() {
        let mut state = crate::server::test_state();
        assert_eq!(format_var(&state, None, "mouse_status_range"), None);
        assert_eq!(
            format_var(&state, Some(PaneId(4)), "pane_in_mode").as_deref(),
            Some("0")
        );
        state.mouse.scroll(PaneId(4), 10, 5);
        assert_eq!(
            format_var(&state, Some(PaneId(4)), "pane_in_mode").as_deref(),
            Some("1")
        );
        let net = Some(Some(RangeKind::User("net".to_string())));
        state.mouse.current = Some(mouse_target("@3", net));
        assert_eq!(
            format_var(&state, None, "mouse_status_range").as_deref(),
            Some("net")
        );
        assert_eq!(
            format_var(&state, None, "mouse_pane").as_deref(),
            Some("%4")
        );
        assert_eq!(format_var(&state, None, "mouse_x"), None);
        assert_eq!(format_var(&state, None, "session_name"), None);
        state.mouse.current = Some(mouse_target("%4", Some(Some(RangeKind::Left))));
        assert_eq!(
            format_var(&state, None, "mouse_status_range").as_deref(),
            Some("left")
        );
        state.mouse.current = Some(mouse_target("%4", None));
        assert_eq!(
            format_var(&state, None, "mouse_status_range").as_deref(),
            Some("")
        );
        assert_eq!(pointer(&state, ClientId(1)), Some((5, 23)));
        assert_eq!(pointer(&state, ClientId(2)), None);
    }

    #[test]
    fn the_view_scrolls_closes_at_the_bottom_and_stays_on_its_lines() {
        let mut s = MouseState::default();
        let p = PaneId(1);
        assert_eq!(s.offset(p, 100), None);
        assert!(s.scroll(p, 100, 5));
        assert_eq!(s.offset(p, 100), Some(5));
        assert!(s.scroll(p, 100, 5));
        assert_eq!(s.offset(p, 100), Some(10));
        // Three new lines: the same lines are now 13 up.
        assert_eq!(s.offset(p, 103), Some(13));
        assert!(s.scroll(p, 103, -5));
        assert_eq!(s.offset(p, 103), Some(8));
        assert!(!s.scroll(p, 103, -10));
        assert_eq!(s.offset(p, 103), None);
        // Capped at the history.
        assert!(s.scroll(p, 7, 5));
        assert!(s.scroll(p, 7, 5));
        assert_eq!(s.offset(p, 7), Some(7));
        assert!(s.close(p));
        assert!(!s.close(p));
    }
}
