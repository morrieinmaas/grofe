//! Mouse: bindable mouse keys (TC.10, spec §4.5 bindings row), forwarding
//! to panes that asked for the mouse (T3.2), and the mouse in copy mode
//! (TM.6, spec §4.11 "Mouse").
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
//! A key bound in the client's table (else `root`; first the copy-mode
//! table when the pane under the pointer is in copy mode, as tmux's
//! `server_client_key_callback`) runs with the pointer's
//! target as its current pane ([`run_mouse_binding`]): the pane under it,
//! or on the status line the clicked window's active pane. While it runs,
//! `#{mouse_status_range}` and the other mouse formats ([`format_var`])
//! describe the event, and a bare `=` target (M36-R3) means the pointer's
//! target ([`resolve_targets`]).
//!
//! Everything a mouse key does is a binding. tmux 3.7b's stock `root` rows
//! (`input::TMUX_STOCK_ROOT`, listed by `list-keys -T root` as tmux lists
//! them) are ordinary default bindings, so `unbind -n MouseDown3Pane` and
//! `unbind -a -T root` take them away, and the keys they bound then do
//! what tmux does with an unbound key: the report goes to the pane it is
//! over, if that pane asked for the mouse (`forward_key`, [`send_event`]).
//! A server with grofe identity binds the pane, status line, border and
//! wheel rows; one with tmux identity all of them, the right-button menus,
//! the Meta and Control variants and the scrollbar rows too. The commands
//! the rows run are native: `send-keys -M` sends the event on to its pane
//! ([`send_current`]), `copy-mode -M`, `-e` and `-H` start copy mode as
//! the key asks, `select-pane -M` clears the mark (and focuses a floating
//! pane), and `display-menu` opens a menu that keeps the event, so `-t=` in
//! its items is still the pane clicked.
//!
//! The one thing the mouse does itself is the border drag: a binding of
//! `MouseDrag1Border` that is `resize-pane -M` (the front end has the
//! command do nothing outside a drag) moves the border under the pointer
//! with it until the release, or a floating pane by its edges
//! (`tc_6/float-border-mouse`, [`drag_border_start`]).
//!
//! `focus-follows-mouse` makes motion over a pane its window's active pane,
//! whether or not `mouse` is on ([`focus_follows`]), and has the client
//! report every motion (`clients::sync_mouse_modes`).
//!
//! With `pane-border-status` on, a pane's title line is its border, and
//! `MouseDown1Control8` and `Control9`, where the default
//! `pane-border-format` puts its `[z]` and `[x]` buttons, zoom the pane
//! and ask to kill it (tmux's stock bindings).
//!
//! In copy mode, as tmux 3.7b's `window_copy_command`: a `send-keys -X`
//! that a mouse key runs first moves the cursor to the pointer unless the
//! key is the wheel ([`copy_command`]), and `begin-selection` (or
//! `copy-mode -M`) starts a drag: until the release the pointer moves the
//! selection's end, and held on the pane's top or bottom row it scrolls
//! every 50 ms ([`start_drag`]).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::protocol::Dialect;
use crate::server::borders::PaneStatus;
use crate::server::clients;
use crate::server::copy::{self, select};
use crate::server::floating;
use crate::server::input::{
    Input, InputHook, Key, KeyCode, Mouse, MouseAction, MouseButton, MouseKey, MouseKind,
    MousePlace, ROOT_TABLE, Routed,
};
use crate::server::layout::BorderSegment;
use crate::server::model::Window;
use crate::server::render::OverlayCtx;
use crate::server::status::RangeKind;
use crate::server::{Job, State};
use crate::types::{
    ClientId, Edge, InputModes, MouseEncoding, MouseMode, PaneId, Rect, SessionId, WindowId,
};

/// How long a click waits for the next one (tmux's `KEYC_CLICK_TIMEOUT`).
pub const CLICK_TIMEOUT: Duration = Duration::from_millis(300);

/// How often a copy-mode drag held on a pane's top or bottom row scrolls
/// (tmux's `WINDOW_COPY_DRAG_REPEAT_TIME`).
pub const DRAG_REPEAT: Duration = Duration::from_millis(50);

/// Each client's place in the click and drag sequence, and the mouse event
/// a bound command is running for.
#[derive(Debug, Default)]
pub struct MouseState {
    pointers: BTreeMap<ClientId, Pointer>,
    /// Numbers each click wait and drag timer, so a stale one does nothing.
    generation: u64,
    /// Set while a mouse key's commands run ([`run_mouse_binding`]).
    current: Option<MouseTarget>,
}

/// One client's pointer, as tmux keeps it on the client.
#[derive(Debug, Default)]
struct Pointer {
    /// The last report's cell (tmux's `lx`, `ly`): where a drag's first
    /// report is placed, and where a border drag moves from.
    last: Option<(u16, u16)>,
    /// A drag in progress (tmux's `mouse_drag_flag`).
    drag: Option<Drag>,
    /// A copy-mode drag the pointer moves until the release (tmux's
    /// `mouse_drag_update`, set by `window_copy_start_drag`).
    copy_drag: Option<CopyDrag>,
    /// The floating pane a border drag moves (tmux's `mouse_last_pane`,
    /// fixed at the drag's start).
    float_drag: Option<PaneId>,
    /// Waiting for another click (tmux's double- and triple-click flags).
    click: Option<Click>,
}

#[derive(Debug, Clone, Copy)]
struct CopyDrag {
    /// The pane in copy mode the last report moved.
    pane: PaneId,
    /// The drag timer's number while the pointer holds the selection on
    /// the pane's top or bottom row (tmux's `dragtimer`).
    timer: Option<u64>,
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
    /// The client cell of the report itself (tmux's `m->x`, `m->y`), which
    /// `cell` is not for a drag's first report (placed where the press was).
    pub report: (u16, u16),
    /// The event is the wheel, which moves no copy-mode cursor.
    pub wheel: bool,
    /// The report the key is for (a double click's is its second press),
    /// which `send-keys -M` sends on to the pane.
    pub event: Mouse,
}

impl MouseState {
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

/// Routes mouse input (mouse keys, the built-in model, copy-mode drags,
/// forwarding).
pub struct MouseHook;

impl InputHook for MouseHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        let clients = &state.clients;
        state.mouse.pointers.retain(|c, _| clients.contains_key(c));
        match input {
            Input::Mouse(m) => on_mouse(state, client, m),
            Input::Key(_) | Input::Paste { .. } | Input::Focus(_) => None,
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
    w.pane_rects()
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
    /// A border, tiled or floating, and the pane it belongs to.
    Border(PaneId),
    /// A click range (`#[range=control|N]`) in a pane's border title.
    Control(PaneId, u8),
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
    // A floating pane is over the layout, the top one first, its border
    // included (tmux's `window_get_active_at`).
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
                return Hit::Border(f.pane);
            }
        }
    }
    let panes = pane_rects(w);
    if let Some((pane, x, y)) = pane_at(&panes, at) {
        return Hit::Pane(pane, x, y);
    }
    if let Some(hit) = title_hit(state, client, w, &panes, at) {
        return hit;
    }
    if !w.zoomed && on_border(&w.layout.borders(w.area()), at) {
        return border_pane(&panes, at).map_or(Hit::Nothing, Hit::Border);
    }
    Hit::Nothing
}

/// A click on a pane's border title line, which belongs to that pane as
/// tmux's `window_get_active_at` finds it: a border, or a control where
/// the title has a `#[range=control|N]` part (the default
/// `pane-border-format`'s zoom and close buttons).
fn title_hit(
    state: &State,
    client: ClientId,
    w: &Window,
    panes: &[(PaneId, Rect)],
    (x, y): (u32, u32),
) -> Option<Hit> {
    let top = match w.pane_status {
        PaneStatus::Off => return None,
        PaneStatus::Top => true,
        PaneStatus::Bottom => false,
    };
    let (pane, _) = panes.iter().find(|(_, r)| {
        let row = if top {
            i64::from(r.y) - 1
        } else {
            i64::from(r.y) + i64::from(r.rows)
        };
        let (left, right) = (u32::from(r.x), u32::from(r.x) + u32::from(r.cols));
        i64::from(y) == row && (left..=right).contains(&x)
    })?;
    let titles = clients::border_titles(state, client);
    let range = titles.iter().find(|t| t.pane == *pane).and_then(|t| {
        let col = u16::try_from(i64::from(x) - i64::from(t.x)).ok()?;
        t.drawn.range_at(col).cloned()
    });
    Some(match range.map(|r| r.kind) {
        Some(RangeKind::Control(n)) => Hit::Control(*pane, n as u8),
        _ => Hit::Border(*pane),
    })
}

fn place(hit: &Hit) -> Option<MousePlace> {
    Some(match hit {
        Hit::Pane(..) => MousePlace::Pane,
        Hit::Border(_) => MousePlace::Border,
        Hit::Control(_, n) => MousePlace::Control(*n),
        Hit::Status(_, None) => MousePlace::StatusDefault,
        Hit::Status(_, Some(RangeKind::Left)) => MousePlace::StatusLeft,
        Hit::Status(_, Some(RangeKind::Right)) => MousePlace::StatusRight,
        Hit::Status(_, Some(_)) => MousePlace::Status,
        Hit::Nothing => return None,
    })
}

/// The target of report `m`'s key, placed at client cell `cell` over `hit`.
fn target(
    state: &State,
    client: ClientId,
    m: &Mouse,
    cell: (u16, u16),
    hit: &Hit,
) -> Option<MouseTarget> {
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
        Hit::Border(p) | Hit::Control(p, _) => (*p, p.to_string(), None, None),
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
        report: (m.col, m.row),
        wheel: matches!(
            m.kind,
            MouseKind::WheelUp
                | MouseKind::WheelDown
                | MouseKind::WheelLeft
                | MouseKind::WheelRight
        ),
        event: *m,
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
    // Any report but a drag or the wheel ends a drag (tmux's "stop dragging
    // if needed"), the drag's release or not: a press after a release the
    // client lost is the drag's end, not a press.
    let ends = matches!(
        m.kind,
        MouseKind::Up(_) | MouseKind::Down(_) | MouseKind::Move
    );
    if ends && let Some(d) = p.drag.take() {
        // The drag's release ends a copy-mode drag first (tmux's
        // `mouse_drag_release`).
        p.copy_drag = None;
        p.float_drag = None;
        return Some((MouseAction::DragEnd, d.button, at));
    }
    let key = match m.kind {
        MouseKind::Move | MouseKind::WheelLeft | MouseKind::WheelRight => return None,
        MouseKind::WheelUp => (MouseAction::WheelUp, 0, at),
        MouseKind::WheelDown => (MouseAction::WheelDown, 0, at),
        MouseKind::Up(b) => (MouseAction::Up, button_number(b), at),
        // A copy-mode drag takes every report until the release (tmux's
        // `KEYC_DRAGGING`).
        MouseKind::Drag(b) if p.copy_drag.is_some() => {
            p.drag.get_or_insert(Drag {
                button: button_number(b),
                border: false,
            });
            return None;
        }
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

/// `focus-follows-mouse`: motion over a pane that is not its window's
/// active one makes it so (tmux's `server_client_check_mouse`, before the
/// `mouse` option is looked at).
fn focus_follows(state: &mut State, client: ClientId, m: &Mouse) {
    use crate::server::dispatch::options::Owner;
    let Some(session) = state.clients.get(&client).map(|c| c.session) else {
        return;
    };
    let options = &state.model.options;
    if options.lookup("focus-follows-mouse", &[Owner::Session(session)]) != Some("on") {
        return;
    }
    let Hit::Pane(pane, ..) = hit(state, client, (m.col, m.row)) else {
        return;
    };
    let Some(window) = client_window(state, client).filter(|w| w.active != pane) else {
        return;
    };
    let window = window.id;
    if let Some(w) = state.model.windows.get_mut(&window) {
        w.set_active(pane);
    }
    clients::changed(state);
}

fn on_mouse(state: &mut State, client: ClientId, m: &Mouse) -> Option<Routed> {
    if m.kind == MouseKind::Move {
        focus_follows(state, client, m);
    }
    if !mouse_on(state, client) {
        // The key tables drop it, as with no mouse at all.
        return None;
    }
    // A client showing no window has nothing to point at.
    client_window(state, client)?;
    let from = state.mouse.pointers.get(&client).and_then(|p| p.last);
    let Some(key) = classify(state, client, m) else {
        let pointer = state.mouse.pointers.get(&client);
        let border = pointer.and_then(|p| p.drag).is_some_and(|d| d.border);
        let floating = pointer.and_then(|p| p.float_drag);
        if border && let (Some(pane), Some(from)) = (floating, from) {
            drag_float(state, client, pane, from, (m.col, m.row));
            return Some(Routed::Consumed);
        }
        if border && let Some(from) = from {
            drag_border(state, client, from, (m.col, m.row));
            return Some(Routed::Consumed);
        }
        if matches!(m.kind, MouseKind::Drag(_)) && pointer.is_some_and(|p| p.copy_drag.is_some()) {
            drag_update(state, client, (m.col, m.row));
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
    let Some(target) = target(state, client, m, at, &hit) else {
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
    match binding(state, client, &key, &target) {
        // tmux's `resize-pane -M` is the border drag of the mouse itself.
        Some((cmds, _)) if is_border_drag_row(&key, &cmds) => {
            drag_border_start(state, client, m, &target);
        }
        Some((cmds, dialect)) => run_mouse_binding(state, &target, &cmds, dialect),
        // Bound nowhere: the report goes to the pane it is over, as
        // tmux's `forward_key`.
        None => {
            send_event(state, client, m, target.pane);
        }
    }
}

/// The binding of mouse key `key` in client `client`'s table, else in
/// `root`, as tmux's key walk finds it; from `root`, first in the
/// copy-mode table when the pane the event is for (`target`'s, else the
/// active one) is in copy mode. Any table but `root` ends with the event.
fn binding(
    state: &mut State,
    client: ClientId,
    key: &Key,
    target: &MouseTarget,
) -> Option<(Vec<Vec<String>>, Dialect)> {
    let table = state.clients.get(&client)?.keys.table().to_string();
    if table != ROOT_TABLE {
        clients::set_key_table(state, client, ROOT_TABLE);
    }
    let pane = if target.range.is_none() {
        Some(target.pane)
    } else {
        active_pane(state, client)
    };
    let mode_table = pane
        .filter(|p| table == ROOT_TABLE && state.copy.is_active(*p))
        .map(|p| copy::key_table(state, p));
    let tables = &state.registry.key_tables;
    // ponytail: a root `Any` also takes the presses the built-in model
    // handles, where tmux's stock MouseDown1Pane and the rest are bound
    // and win over it; no config the owner runs binds root `Any`.
    let b = mode_table
        .and_then(|t| tables.get(t, key))
        .or_else(|| tables.lookup(&table, key))
        .or_else(|| tables.lookup(ROOT_TABLE, key))?;
    Some((b.cmds.clone(), b.dialect))
}

/// Whether `cmds`, bound to `key` (with any modifiers), is tmux 3.7b's
/// `MouseDrag1Border` row, `resize-pane -M` (by name or alias), or no
/// command at all, which in tmux leaves the stock row bound. The drag is
/// the mouse's own ([`drag_border_start`]): `resize-pane -M` is no command
/// outside a drag, as the front end has it.
fn is_border_drag_row(key: &Key, cmds: &[Vec<String>]) -> bool {
    let KeyCode::Mouse(mk) = key.code else {
        return false;
    };
    if mk.place != MousePlace::Border || mk.button != 1 || mk.action != MouseAction::Drag {
        return false;
    }
    match cmds {
        [] => true,
        [c] => {
            matches!(c.as_slice(), [verb, flag] if ["resize-pane", "resizep"].contains(&verb.as_str()) && flag == "-M")
        }
        _ => false,
    }
}

/// A border drag starts (tmux's `resize-pane -M` on `MouseDrag1Border`):
/// the pointer moves the border under it until the release; a floating
/// pane is focused and raised first, and moved or resized by its edges.
fn drag_border_start(state: &mut State, client: ClientId, m: &Mouse, target: &MouseTarget) {
    let floating = client_window(state, client)
        .and_then(|w| w.floating(target.pane))
        .is_some()
        .then_some(target.pane);
    if let Some(p) = state.mouse.pointers.get_mut(&client) {
        p.drag = Some(Drag {
            button: 1,
            border: true,
        });
        p.float_drag = floating;
    }
    // As `resize-pane -M`: the first report already moves it.
    let from = target.cell;
    match floating {
        Some(pane) => {
            // tmux focuses (and so raises) a floating pane first.
            let window = state.model.panes.get(&pane).map(|p| p.window);
            if let Some(w) = window.and_then(|w| state.model.windows.get_mut(&w)) {
                w.set_active(pane);
            }
            clients::changed(state);
            drag_float(state, client, pane, from, (m.col, m.row));
        }
        None => drag_border(state, client, from, (m.col, m.row)),
    }
}

/// `send-keys -M`: the mouse event the running command is for goes to its
/// pane, as the pane's mouse mode asks (tmux's `window_pane_key`). Outside
/// a mouse key's commands it is tmux's own failure.
pub fn send_current(state: &mut State) -> Result<String, String> {
    let Some(t) = state.mouse.current.clone() else {
        return Err(crate::cmd::target::NO_MOUSE_TARGET.to_string());
    };
    send_event(state, t.client, &t.event, t.pane);
    Ok(String::new())
}

/// Report `m` goes to `pane` if the pointer is over it, the pane is in no
/// mode and takes input, and its mouse mode asks for this kind of
/// event.
// ponytail: a drag that leaves its pane is dropped rather than following
// the pane it started in.
fn send_event(state: &mut State, client: ClientId, m: &Mouse, pane: PaneId) {
    if let Hit::Pane(over, x, y) = hit(state, client, (m.col, m.row))
        && over == pane
    {
        send_to(state, pane, m, (x, y));
    }
}

/// Sends report `m` to the pane under it, if that pane asked for this
/// kind of event and is in no mode.
fn forward(state: &mut State, client: ClientId, m: &Mouse) -> Routed {
    if let Hit::Pane(pane, x, y) = hit(state, client, (m.col, m.row)) {
        send_to(state, pane, m, (x, y));
    }
    Routed::Consumed
}

/// Report `m`, at 0-based cell `(x, y)` of `pane`, goes to the pane's
/// process if its mouse mode asks for it.
fn send_to(state: &mut State, pane: PaneId, m: &Mouse, (x, y): (u16, u16)) {
    // A pane in a mode gives its app nothing (tmux's `window_pane_key`).
    if state.copy.is_active(pane)
        || state.clocks.is_active(pane)
        || crate::server::tree::is_over(state, pane)
    {
        return;
    }
    // A pane taking no input gets no mouse either (tmux's `window_pane_key`).
    let Some(p) = state.model.panes.get(&pane).filter(|p| !p.input_off) else {
        return;
    };
    if let Some(bytes) = encode(m, x, y, &p.handle.modes())
        && p.handle.enqueue_input(bytes).is_err()
    {
        eprintln!("grofe server: input queue full for {pane}");
    }
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
    let ((lx, ly), (x, y)) = (drag_cell(&cx, w, from), drag_cell(&cx, w, to));
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
    state.model.relayout_held(window);
    clients::changed(state);
}

/// Client cell `cell` of a drag in window `w`'s cells. As tmux: a pointer
/// off the window (on the status line) drags along its nearest row.
fn drag_cell(cx: &OverlayCtx, w: &Window, (col, row): (u16, u16)) -> (i32, i32) {
    let row = row.saturating_sub(cx.window_top).min(w.rows.max(1) - 1);
    (
        i32::from(col) + i32::from(cx.viewport.0),
        i32::from(row) + i32::from(cx.viewport.1),
    )
}

/// A drag report from client cell `from` (the last report) to `to` on the
/// border of floating pane `pane`, as tmux 3.7b's
/// `cmd_resize_pane_mouse_update_floating` ([`floating::dragged`]). A pane
/// gone since the press, or a window no longer shown, is left alone.
fn drag_float(state: &mut State, client: ClientId, pane: PaneId, from: (u16, u16), to: (u16, u16)) {
    let Some(cx) = clients::overlay_ctx(state, client) else {
        return;
    };
    let Some(w) = client_window(state, client) else {
        return;
    };
    let (window, last, now) = (w.id, drag_cell(&cx, w, from), drag_cell(&cx, w, to));
    let Some(now) = w
        .floating(pane)
        .and_then(|f| floating::dragged(f, last, now))
    else {
        return;
    };
    let Some(f) = state
        .model
        .windows
        .get_mut(&window)
        .and_then(|w| w.floating.iter_mut().find(|f| f.pane == pane))
    else {
        return;
    };
    *f = now;
    state.model.relayout_held(window);
    clients::changed(state);
}

/// Runs mouse-bound `cmds` in `dialect` for the event at `target`, as
/// [`clients::run_binding`] runs a key's (and holds as it does), but with
/// the pointer's pane as the current pane while it is there, the event as
/// the current mouse event, and a bare `=` resolved to the pointer's
/// target (M36-R3). The first error stops the rest and shows on the
/// client's status row.
pub fn run_mouse_binding(
    state: &mut State,
    target: &MouseTarget,
    cmds: &[Vec<String>],
    dialect: Dialect,
) {
    clients::run_commands_for(
        state,
        target.client,
        cmds,
        dialect,
        true,
        Some(target.clone()),
    );
}

/// `argv` with each bare `=` target (`-t =`, `-t=`, and the same for `-s`)
/// replaced by the current mouse event's target (M36-R3). Outside a mouse
/// key's commands a bare `=` is left as it is, the literal session name it
/// is natively, which no session can have.
pub fn resolve_targets(state: &State, argv: &[String]) -> Vec<String> {
    let Some(m) = &state.mouse.current else {
        return argv.to_vec();
    };
    // tmux's `@`: the client's own current pane, not the pointer's.
    let active = || {
        active_pane(state, m.client)
            .map(|p| p.to_string())
            .unwrap_or_default()
    };
    let mut out = Vec::with_capacity(argv.len());
    let mut after_flag = false;
    for word in argv {
        let replaced = match word.as_str() {
            "=" if after_flag => m.target.clone(),
            "@" | "{active}" | "{current}" if after_flag => active(),
            "-t=" | "-s=" => format!("{}{}", &word[..2], m.target),
            "-t@" | "-s@" => format!("{}{}", &word[..2], active()),
            _ => word.clone(),
        };
        after_flag = matches!(word.as_str(), "-t" | "-s");
        out.push(replaced);
    }
    out
}

/// A format variable the mouse answers while a mouse key's commands run:
/// `mouse_status_range` (tmux's words for the range clicked, empty off the
/// status line or outside a range), `mouse_pane`, and `mouse_x`/`mouse_y`
/// ([`MouseTarget::pane_cell`]). `None` lets the caller's own variables
/// answer.
pub fn format_var(state: &State, name: &str) -> Option<String> {
    let m = state.mouse.current.as_ref()?;
    match name {
        "mouse_status_range" => Some(match &m.range {
            Some(Some(range)) => range.mouse_status_range(),
            _ => String::new(),
        }),
        "mouse_pane" => Some(m.pane.to_string()),
        "mouse_x" => m.pane_cell.map(|(x, _)| x.to_string()),
        "mouse_y" => m.pane_cell.map(|(_, y)| y.to_string()),
        // Over a pane's text; the status line has no such cell.
        "mouse_word" | "mouse_line" | "mouse_hyperlink" if m.range.is_none() => {
            let (x, y) = m.pane_cell?;
            crate::server::copy::mouse_text(state, m.pane, x, y, name)
        }
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

// ---- Copy mode (TM.6) -------------------------------------------------------

/// The pane under client `client`'s cell `cell`, if it is in copy mode,
/// and the cell inside it (tmux's `cmd_mouse_pane` and `cmd_mouse_at`).
fn copy_pane_at(
    state: &State,
    client: ClientId,
    cell: (u16, u16),
) -> Option<(PaneId, usize, usize)> {
    let Hit::Pane(pane, x, y) = hit(state, client, cell) else {
        return None;
    };
    state
        .copy
        .is_active(pane)
        .then_some((pane, usize::from(x), usize::from(y)))
}

/// The pane the running mouse key is for, if it is over a pane or a
/// border (tmux's `cmd_mouse_pane`): `copy-mode -M`'s pane.
pub fn event_pane(state: &State) -> Option<PaneId> {
    let m = state.mouse.current.as_ref()?;
    m.range.is_none().then_some(m.pane)
}

/// What a `send-keys -X` run for a mouse event does first, as tmux's
/// `window_copy_command`: unless the event is the wheel, the cursor of the
/// copy mode under the pointer moves to it (`window_copy_move_mouse`);
/// and `begin-selection` for any mouse event starts a drag instead of a
/// plain selection ([`start_drag`]). Whether that was the whole command.
pub fn copy_command(state: &mut State, pane: PaneId, args: &[String]) -> bool {
    let Some(m) = state.mouse.current.clone() else {
        return false;
    };
    if !m.wheel
        && let Some((at, x, y)) = copy_pane_at(state, m.client, m.report)
    {
        copy::edit_now(state, at, |mode, _| select::move_to(mode, x, y));
        clients::mark_all_dirty(state);
    }
    if args != ["begin-selection"] {
        return false;
    }
    start_drag(state);
    copy::edit_now(state, pane, |mode, _| mode.prefix = 1);
    true
}

/// Starts a copy-mode drag for the running mouse key, as tmux's
/// `window_copy_start_drag`: in the copy mode under the place the key is
/// at (a drag's press), the selection starts there (or a word or line
/// selection the press is inside keeps its first word or line), and until
/// the release each report moves its end ([`drag_update`]).
pub fn start_drag(state: &mut State) {
    let Some(m) = state.mouse.current.clone() else {
        return;
    };
    let Some((pane, x, y)) = copy_pane_at(state, m.client, m.cell) else {
        return;
    };
    let Some(p) = state.mouse.pointers.get_mut(&m.client) else {
        return;
    };
    p.copy_drag = Some(CopyDrag { pane, timer: None });
    copy::edit_now(state, pane, |mode, _| select::start_drag(mode, x, y));
    drag_update(state, m.client, m.report);
}

/// A report during client `client`'s copy-mode drag, at client cell
/// `cell`, as tmux's `window_copy_drag_update`: the cursor and the
/// selection's end follow the pointer, and on the pane's top or bottom row
/// the view scrolls a line, then again every [`DRAG_REPEAT`] while the
/// cursor stays there.
fn drag_update(state: &mut State, client: ClientId, cell: (u16, u16)) {
    let Some(drag) = state
        .mouse
        .pointers
        .get_mut(&client)
        .and_then(|p| p.copy_drag.as_mut())
    else {
        return;
    };
    drag.timer = None;
    let Some((pane, x, y)) = copy_pane_at(state, client, cell) else {
        return;
    };
    let edge = copy::edit_now(state, pane, |mode, _| select::drag_update(mode, x, y));
    if let Some(drag) = state
        .mouse
        .pointers
        .get_mut(&client)
        .and_then(|p| p.copy_drag.as_mut())
    {
        drag.pane = pane;
    }
    if edge == Some(true) {
        start_drag_timer(state, client);
    }
    clients::mark_all_dirty(state);
}

/// Arms client `client`'s drag timer: after [`DRAG_REPEAT`], if the drag
/// is still on and the timer not replaced, the view scrolls another line.
fn start_drag_timer(state: &mut State, client: ClientId) {
    let generation = state.mouse.generation + 1;
    let Some(drag) = state
        .mouse
        .pointers
        .get_mut(&client)
        .and_then(|p| p.copy_drag.as_mut())
    else {
        return;
    };
    state.mouse.generation = generation;
    drag.timer = Some(generation);
    // Without a runtime (unit tests) the timer never fires.
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let jobs = state.jobs.clone();
    rt.spawn(async move {
        tokio::time::sleep(DRAG_REPEAT).await;
        let job: Job = Box::new(move |state: &mut State| drag_tick(state, client, generation));
        let _ = jobs.send(job).await;
    });
}

/// The drag timer (tmux's `window_copy_scroll_timer`): see
/// [`start_drag_timer`].
fn drag_tick(state: &mut State, client: ClientId, generation: u64) {
    if !state.clients.contains_key(&client) {
        client_gone(state, client);
        return;
    }
    let Some(drag) = state
        .mouse
        .pointers
        .get_mut(&client)
        .and_then(|p| p.copy_drag.as_mut())
        .filter(|d| d.timer == Some(generation))
    else {
        return;
    };
    drag.timer = None;
    let pane = drag.pane;
    let Some((edge, moved)) = copy::edit_now(state, pane, |mode, _| select::drag_scroll(mode))
    else {
        return;
    };
    if edge {
        start_drag_timer(state, client);
    }
    if moved {
        clients::mark_all_dirty(state);
    }
}

/// Client `id` is gone: its pointer, and with it any drag and its timer,
/// goes too (the client's removal calls this).
pub fn client_gone(state: &mut State, id: ClientId) {
    state.mouse.pointers.remove(&id);
    if state.mouse.current.as_ref().is_some_and(|m| m.client == id) {
        state.mouse.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::Mods;

    #[test]
    fn only_tmuxs_own_border_drag_row_is_the_mouses_drag() {
        let key = |name: &str| crate::server::input::parse_key(name).unwrap();
        let cmd = |words: &[&str]| vec![words.iter().map(|w| w.to_string()).collect::<Vec<_>>()];
        let (down, drag) = (key("MouseDown1Border"), key("MouseDrag1Border"));
        assert!(is_border_drag_row(&drag, &cmd(&["resize-pane", "-M"])));
        assert!(is_border_drag_row(&drag, &cmd(&["resizep", "-M"])));
        assert!(is_border_drag_row(&drag, &[]));
        // The click row is a command that runs as it is.
        assert!(!is_border_drag_row(&down, &cmd(&["select-pane", "-M"])));
        assert!(!is_border_drag_row(&down, &cmd(&["resize-pane", "-M"])));
        // Anything more, less or else is the user's own.
        assert!(!is_border_drag_row(
            &drag,
            &cmd(&["resize-pane", "-M", "-Z"])
        ));
        assert!(!is_border_drag_row(&drag, &cmd(&["resize-pane", "-Z"])));
        assert!(!is_border_drag_row(&drag, &cmd(&["display-message", "hi"])));
        let two = [cmd(&["resize-pane", "-M"]), cmd(&["kill-pane"])].concat();
        assert!(!is_border_drag_row(&drag, &two));
        // A modifier does not change what the row does, as in tmux; the
        // built-in drag has no other button and no other place.
        assert!(is_border_drag_row(
            &key("M-MouseDrag1Border"),
            &cmd(&["resize-pane", "-M"])
        ));
        assert!(!is_border_drag_row(
            &key("MouseDrag3Border"),
            &cmd(&["resize-pane", "-M"])
        ));
        assert!(!is_border_drag_row(
            &key("MouseDrag1Pane"),
            &cmd(&["resize-pane", "-M"])
        ));
    }

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

    /// TM.6: while a copy-mode drag is on, its reports are no keys (tmux's
    /// `KEYC_DRAGGING`); its release is the drag's end and stops it.
    #[test]
    fn a_copy_mode_drag_takes_the_reports_until_its_release() {
        let mut state = crate::server::test_state();
        let c = ClientId(1);
        let key = |state: &mut State, kind, x, y| classify(state, c, &report(kind, x, y));
        assert!(key(&mut state, MouseKind::Down(LEFT), 3, 3).is_some());
        assert_eq!(
            key(&mut state, MouseKind::Drag(LEFT), 4, 3).map(|k| k.0),
            Some(MouseAction::Drag)
        );
        state.mouse.pointers.get_mut(&c).unwrap().copy_drag = Some(CopyDrag {
            pane: PaneId(0),
            timer: None,
        });
        assert_eq!(key(&mut state, MouseKind::Drag(LEFT), 9, 5), None);
        assert_eq!(
            key(&mut state, MouseKind::Up(LEFT), 9, 5).map(|k| k.0),
            Some(MouseAction::DragEnd)
        );
        assert!(state.mouse.pointers[&c].copy_drag.is_none());
        // With no drag left, a stale timer does nothing.
        drag_tick(&mut state, c, 7);
    }

    /// A client that goes in the middle of a drag takes its pointer, drag
    /// and timer with it; its timer firing later does nothing.
    #[test]
    fn a_client_gone_mid_drag_leaves_no_drag_behind() {
        let mut state = crate::server::test_state();
        let c = ClientId(1);
        classify(&mut state, c, &report(MouseKind::Down(LEFT), 3, 3));
        let p = state.mouse.pointers.get_mut(&c).unwrap();
        p.copy_drag = Some(CopyDrag {
            pane: PaneId(0),
            timer: Some(9),
        });
        // No such client in `state`: the timer clears what it left.
        drag_tick(&mut state, c, 9);
        assert!(!state.mouse.pointers.contains_key(&c));
        let p = state.mouse.pointers.entry(c).or_default();
        p.copy_drag = Some(CopyDrag {
            pane: PaneId(0),
            timer: Some(10),
        });
        client_gone(&mut state, c);
        assert!(!state.mouse.pointers.contains_key(&c));
        drag_tick(&mut state, c, 10);
        assert!(!state.mouse.pointers.contains_key(&c));
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
            report: (5, 23),
            wheel: false,
            event: mouse(MouseKind::Move),
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
        assert_eq!(format_var(&state, "mouse_status_range"), None);
        // `pane_in_mode` is copy mode's now (TM.6): the view is gone.
        assert_eq!(format_var(&state, "pane_in_mode"), None);
        let net = Some(Some(RangeKind::User("net".to_string())));
        state.mouse.current = Some(mouse_target("@3", net));
        assert_eq!(
            format_var(&state, "mouse_status_range").as_deref(),
            Some("net")
        );
        assert_eq!(format_var(&state, "mouse_pane").as_deref(), Some("%4"));
        assert_eq!(format_var(&state, "mouse_x"), None);
        assert_eq!(format_var(&state, "session_name"), None);
        state.mouse.current = Some(mouse_target("%4", Some(Some(RangeKind::Left))));
        assert_eq!(
            format_var(&state, "mouse_status_range").as_deref(),
            Some("left")
        );
        state.mouse.current = Some(mouse_target("%4", None));
        assert_eq!(
            format_var(&state, "mouse_status_range").as_deref(),
            Some("")
        );
        assert_eq!(pointer(&state, ClientId(1)), Some((5, 23)));
        assert_eq!(pointer(&state, ClientId(2)), None);
    }
}
