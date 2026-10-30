//! Sessions, windows, panes, ids, env (spec §4.2).
//!
//! The actor owns one [`Model`]. Sessions, windows and panes live in flat
//! id-keyed maps and point at their parent by id: a window knows its
//! session, a pane its window. A session keeps its windows ordered by
//! index; a window's pane order is its layout order.
//!
//! Removal cascades (spec §4.2): removing a window's last pane removes the
//! window, removing a session's last window removes the session, and once
//! the last session is gone [`Model::closed`] turns true, which the
//! dispatcher and the pane-event handler turn into server exit. A server
//! that never had a session (`start-server`) is not closed.
//!
//! Dropping a [`Pane`] drops its `PaneHandle`, which stops the pane's tasks
//! and closes the master, hanging up the child.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::cmd::{PanePart, PaneRef, Resolve, SessionRef, Target, WindowRef};
use crate::event::Place;
use crate::protocol::ClientCtx;
use crate::server::borders::PaneStatus;
use crate::server::dispatch::options::{self, Options, REMAIN_ON_EXIT};
use crate::server::layout::LayoutTree;
use crate::server::pane::PaneHandle;
use crate::types::{ClientId, PaneId, Rect, SessionId, WindowId};

/// The built-in defaults of `base-index`, `pane-base-index` and
/// `history-limit` (spec §5.3). The live values are the options, read
/// through [`Model::base_index`], [`Model::pane_base_index`] and
/// [`Model::history_limit`].
pub const BASE_INDEX: u32 = 1;
pub const PANE_BASE_INDEX: u32 = 1;
pub const HISTORY_LIMIT: usize = 50_000;
/// Size of a session created with no `-x`/`-y` and no client (tmux's
/// `default-size`).
pub const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// Panes a removal took out whose exit was not reported yet, as
/// `(id, uid)`: the caller emits `pane-exited` for each.
pub type Removed = Vec<(PaneId, String, Place)>;

pub struct Session {
    pub id: SessionId,
    pub name: String,
    /// Ordered by window index.
    pub windows: Vec<WindowId>,
    pub active: WindowId,
    /// Captured from the client that created the session (spec §4.2).
    pub env: Vec<(String, String)>,
    /// Default cwd for new windows.
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    /// The windows that were active before `active`, most recent last
    /// (tmux's `lastw` stack, for `last-window`), kept by
    /// [`Model::select_window`] and [`Model::remove_window`].
    pub last_windows: Vec<WindowId>,
    /// When the session was created, attached, switched to or typed in:
    /// tmux's `activity_time`, which picks the session a command with no
    /// pane and no target runs against.
    pub activity: Instant,
    /// How many clients show the session (`session_attached`).
    pub attached: usize,
    /// When the session was created (`session_created`).
    pub created: std::time::SystemTime,
}

pub struct Window {
    pub id: WindowId,
    /// The first link's session and index (`links[0]`), for code with no
    /// session of its own; see `server::links`.
    pub session: SessionId,
    pub index: u32,
    /// Every link of the window, session and index, in tmux's order.
    /// `insert_window` fills it from `session` and `index` when empty.
    pub links: Vec<(SessionId, u32)>,
    pub name: String,
    pub layout: LayoutTree,
    pub active: PaneId,
    pub zoomed: bool,
    pub cols: u16,
    pub rows: u16,
    /// tmux's `window_bell_flag` (R13): set by a bell (T3.3), cleared when
    /// the window becomes its session's current window.
    pub bell_flag: bool,
    /// tmux's `window_activity_flag` and `window_silence_flag`
    /// (`server::alerts`), cleared as the bell flag is.
    pub activity_flag: bool,
    pub silence_flag: bool,
    /// When the window was made or last printed something or became its
    /// session's current window: tmux's `activity_time`
    /// (`window_activity`, and `#{W/t:}`'s order).
    pub activity: std::time::SystemTime,
    /// The panes that were active before `active`, most recent last
    /// (tmux's `last_panes`, for `last-pane`), kept by
    /// [`Window::set_active`] and [`Window::lose_pane`].
    pub last_panes: Vec<PaneId>,
    /// When each pane last became active (tmux's `active_point`), for
    /// `select-pane -L/-R/-U/-D`; a pane never made active has none (0).
    pub active_points: ActivePoints,
    /// Floating panes (tmux 3.7's `new-pane`, TC.6), in creation order:
    /// panes of the window outside its layout, drawn over it.
    pub floating: Vec<Floating>,
    /// `pane-border-status` as last applied (`clients::sync_pane_status`):
    /// the panes along the top or bottom edge give up a row to their title.
    pub pane_status: PaneStatus,
}

/// Panes' `active_point`s, drawn from one counter for the whole server as
/// in tmux, where it starts at 0 (so the first pane made active ties with
/// the panes never made active).
#[derive(Debug, Default, Clone)]
pub struct ActivePoints(std::collections::BTreeMap<PaneId, u64>);

static NEXT_ACTIVE_POINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ActivePoints {
    fn stamp(&mut self, pane: PaneId) {
        let point = NEXT_ACTIVE_POINT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.0.insert(pane, point);
    }

    pub fn get(&self, pane: PaneId) -> u64 {
        self.0.get(&pane).copied().unwrap_or(0)
    }
}

/// Where a floating pane is: the window cell of its top-left content
/// cell (its border is drawn around it, and either may be off the
/// window), its size, and its place in the stack, highest on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Floating {
    pub pane: PaneId,
    pub x: i32,
    pub y: i32,
    pub cols: u16,
    pub rows: u16,
    pub z: u32,
}

/// Stands in a window's layout once its last tiled pane has gone while
/// floating panes remain (tmux 3.7b keeps such a window, recorded): no
/// pane has this id, so nothing is drawn or listed for it.
pub const HOLE: PaneId = PaneId(u32::MAX);

impl Window {
    /// Makes `pane` the active pane; the one it replaces becomes the most
    /// recent last pane, as tmux's `window_set_active_pane` does.
    pub fn set_active(&mut self, pane: PaneId) {
        // A floating pane that becomes active comes to the top, as in
        // tmux 3.7b (recorded: select-pane raises it).
        let top = self.floating.iter().map(|f| f.z).max().unwrap_or(0);
        if let Some(f) = self.floating.iter_mut().find(|f| f.pane == pane)
            && f.z != top
        {
            f.z = top + 1;
        }
        if self.active == pane {
            return;
        }
        let old = self.active;
        self.last_panes.retain(|p| *p != pane && *p != old);
        self.last_panes.push(old);
        self.active = pane;
        self.active_points.stamp(pane);
    }

    /// The pane `last-pane` goes back to.
    pub fn last_pane(&self) -> Option<PaneId> {
        self.last_panes.last().copied()
    }

    /// After `pane` left the window, whose index order was `order`: it
    /// leaves the last-pane stack, and if it was active the last pane
    /// takes over, else the one before it, else the one after (tmux's
    /// `window_lost_pane`).
    pub fn lose_pane(&mut self, order: &[PaneId], pane: PaneId) {
        self.last_panes.retain(|p| *p != pane);
        if self.active != pane {
            return;
        }
        let pos = order.iter().position(|p| *p == pane).unwrap_or(0);
        let next = self
            .last_pane()
            .or_else(|| pos.checked_sub(1).map(|i| order[i]))
            .or_else(|| order.get(pos + 1).copied());
        if let Some(next) = next {
            self.active = next;
            self.last_panes.retain(|p| *p != next);
        }
    }

    /// `r`, a layout cell's rect, less the row a border status takes from
    /// the panes along the top (or bottom) edge, as tmux's
    /// `layout_fix_panes` does.
    fn edge_adjusted(&self, mut r: Rect) -> Rect {
        let area = self.area();
        match self.pane_status {
            PaneStatus::Top if r.y == 0 => {
                r.y = 1;
                r.rows = r.rows.saturating_sub(1);
            }
            PaneStatus::Bottom if r.y + r.rows == area.rows => {
                r.rows = r.rows.saturating_sub(1);
            }
            _ => {}
        }
        r
    }

    /// The tiled panes and their content rects in the layout.
    pub fn tiled_rects(&self) -> Vec<(PaneId, Rect)> {
        self.layout
            .compute(self.area())
            .into_iter()
            .map(|(id, r)| (id, self.edge_adjusted(r)))
            .collect()
    }

    /// The rect the zoomed pane fills.
    pub fn zoom_rect(&self) -> Rect {
        self.edge_adjusted(self.area())
    }

    /// The panes on show and their content rects: the layout's, or only
    /// the active pane over the whole window when zoomed.
    pub fn pane_rects(&self) -> Vec<(PaneId, Rect)> {
        if self.zoomed {
            vec![(self.active, self.zoom_rect())]
        } else {
            self.tiled_rects()
        }
    }

    /// The area the layout is computed against.
    pub fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: self.cols,
            rows: self.rows,
        }
    }

    /// Pane ids in layout order, then the floating panes in creation
    /// order, as tmux 3.7b indexes them: position `i` is pane index
    /// `i + pane-base-index`.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = self.tiled_ids();
        ids.extend(self.floating.iter().map(|f| f.pane));
        ids
    }

    /// The panes in the layout, in layout order: the only ones a layout
    /// operation (presets, rotation, splits) may touch.
    pub fn tiled_ids(&self) -> Vec<PaneId> {
        self.layout
            .compute(self.area())
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| *id != HOLE)
            .collect()
    }

    /// `pane`'s place if it is a floating pane of this window.
    pub fn floating(&self, pane: PaneId) -> Option<&Floating> {
        self.floating.iter().find(|f| f.pane == pane)
    }
}

pub struct Pane {
    pub id: PaneId,
    /// Durable identity (ULID); survives `respawn-pane`.
    pub uid: String,
    pub window: WindowId,
    /// What was spawned; `None` is the user's shell.
    pub argv: Option<Vec<String>>,
    /// The shell the pane started with, which a respawn keeps (tmux's
    /// `wp->shell`).
    pub shell: String,
    pub cwd: PathBuf,
    /// The argv that brings this pane's process back; `None` starts a shell.
    pub resume: Option<Vec<String>>,
    /// `Some(status)` once the child exited and the master drained;
    /// `status` is `None` when a signal killed it.
    pub dead: Option<Option<i32>>,
    /// The size last given to the pty.
    pub cols: u16,
    pub rows: u16,
    /// `select-pane -d` / `last-pane -d` (tmux's `PANE_INPUTOFF`): the pane
    /// takes no keys, pastes or `send-keys`.
    pub input_off: bool,
    pub handle: PaneHandle,
}

#[derive(Default)]
pub struct Model {
    pub sessions: BTreeMap<SessionId, Session>,
    pub windows: BTreeMap<WindowId, Window>,
    pub panes: BTreeMap<PaneId, Pane>,
    /// Every option value, scoped (spec §4.2). The global
    /// `remain-on-exit` is seeded from the server's start options.
    pub options: Options,
    /// Each attached client's previous session (MRU, for `switch-client
    /// -l`). Switching a client records it; removing a session drops
    /// entries naming it.
    pub client_last_session: BTreeMap<ClientId, SessionId>,
    /// Who the server says it is (spec D12), fixed at start by
    /// `compat::server_started` (TC.1); grofe until then.
    pub identity: crate::compat::Identity,
    /// Sessions removed since `hooks::run_pending` last took them, as
    /// `(id, name)`, for the `session-closed` hook (TC.5).
    pub closed_sessions: Vec<(SessionId, String)>,
    /// Sessions removed since `clients::changed` last looked, as
    /// `(name, detach-on-destroy)`: where their clients go
    /// (`server_destroy_session`).
    pub destroyed_sessions: BTreeMap<SessionId, (String, String)>,
    /// When the server started (`start_time`); `None` only for a model
    /// built with `Model::default()`.
    pub started: Option<std::time::SystemTime>,
    /// Windows that became their session's current one since
    /// `server::alerts::drain` last took them: tmux counts that as
    /// activity in the window (`session_set_current`).
    pub selected: Vec<WindowId>,
    /// Session groups (`new-session -t`), see `server::links`.
    pub groups: Vec<crate::server::links::Group>,
    /// The server's marked pane (tmux's `marked_pane`), with the window it
    /// was marked in. Read it through [`Model::marked_pane`], which checks
    /// it is still there.
    pub marked: Option<(SessionId, WindowId, PaneId)>,
    /// Each window's layout before its last `select-layout` (tmux's
    /// `old_layout`), as a layout string, for `select-layout -o`.
    pub old_layouts: BTreeMap<WindowId, String>,
    next_session: u32,
    next_window: u32,
    next_pane: u32,
    had_session: bool,
}

impl Model {
    pub fn new(remain_on_exit: bool) -> Model {
        Model {
            options: Options::new(remain_on_exit),
            started: Some(std::time::SystemTime::now()),
            ..Model::default()
        }
    }

    /// The marked pane while it is valid, as tmux's `server_check_marked`:
    /// the pane exists and is still in the window it was marked in (a pane
    /// that moved to another window is no longer marked, killed panes and
    /// windows are gone; ids are never reused). The mark also names the
    /// session it was made in, and holds only while that session still has
    /// the window (a winlink in tmux): unlinking the window from it clears it
    /// even if another session keeps the window.
    pub fn marked_pane(&self) -> Option<PaneId> {
        let (session, window, pane) = self.marked?;
        let p = self.panes.get(&pane)?;
        (p.window == window && self.has_link(session, window)).then_some(pane)
    }

    pub fn alloc_session_id(&mut self) -> SessionId {
        self.next_session += 1;
        SessionId(self.next_session - 1)
    }

    pub fn alloc_window_id(&mut self) -> WindowId {
        self.next_window += 1;
        WindowId(self.next_window - 1)
    }

    pub fn alloc_pane_id(&mut self) -> PaneId {
        self.next_pane += 1;
        PaneId(self.next_pane - 1)
    }

    /// Inserts a new session with an empty window list. `new_session`
    /// calls this before `insert_window` for the session's first window;
    /// `insert_window` is what adds it to `session.windows`.
    pub fn insert_session(&mut self, session: Session) {
        self.had_session = true;
        self.sessions.insert(session.id, session);
    }

    /// Inserts a window and adds it to its session's ordered window list
    /// (and, in a session group, to its peers').
    pub fn insert_window(&mut self, mut window: Window) {
        let (id, session, index) = (window.id, window.session, window.index);
        window.links.clear();
        self.windows.insert(id, window);
        self.add_link(session, id, index);
        self.sync_group(session);
    }

    /// Makes `window` its session's active window; the one it replaces
    /// goes on top of the session's last-window stack, as in tmux. A window
    /// that becomes current loses its alert flags (tmux's
    /// `winlink_clear_flags`); reselecting the current one changes nothing.
    pub fn select_window(&mut self, window: WindowId) {
        let Some(session) = self.windows.get(&window).map(|w| w.session) else {
            return;
        };
        self.select_window_in(session, window);
    }

    /// [`Model::select_window`] in `session`, which holds `window`.
    pub fn select_window_in(&mut self, session: SessionId, window: WindowId) {
        if !self.has_link(session, window) {
            return;
        }
        if let Some(s) = self.sessions.get_mut(&session)
            && s.active != window
        {
            s.last_windows.retain(|w| *w != window);
            s.last_windows.push(s.active);
            s.active = window;
            if let Some(w) = self.windows.get_mut(&window) {
                w.bell_flag = false;
                w.activity_flag = false;
                w.silence_flag = false;
            }
            self.selected.push(window);
        }
    }

    /// True once every session is gone, after at least one existed: the
    /// server should exit, unless `exit-empty` is off.
    pub fn closed(&self) -> bool {
        self.had_session
            && self.sessions.is_empty()
            && self.options.lookup("exit-empty", &[]) != Some("off")
    }

    /// Whether `pane` stays after its process exits with `status` (its
    /// exit code, `None` when a signal ended it): `remain-on-exit` looked
    /// up pane over window over session over global. `failed` keeps it
    /// unless it exited 0, as tmux 3.7b (recorded: a signal keeps it too).
    pub fn remain_on_exit(&self, pane: PaneId, status: Option<i32>) -> bool {
        match self
            .options
            .lookup(REMAIN_ON_EXIT, &options::chain(self, pane))
        {
            Some("on" | "key") => true,
            Some("failed") => status != Some(0),
            _ => false,
        }
    }

    pub fn session_by_name(&self, name: &str) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|s| s.name == name)
            .map(|s| s.id)
    }

    /// The lowest free window index at or above `base-index` in `session`.
    pub fn free_window_index(&self, session: SessionId) -> u32 {
        let used: Vec<u32> = self
            .sessions
            .get(&session)
            .map(|s| {
                s.windows
                    .iter()
                    .filter_map(|w| self.index_in(session, *w))
                    .collect()
            })
            .unwrap_or_default();
        let base = self.base_index(session);
        (base..).find(|i| !used.contains(i)).unwrap_or(base)
    }

    /// A number option's value along `chain` (then global), else `default`.
    fn number_option(&self, name: &str, chain: &[options::Owner], default: u32) -> u32 {
        self.options
            .lookup(name, chain)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    /// `default-shell` for `session`: the shell a new pane starts, as tmux
    /// starts it. Its default is the server's own shell, picked at start
    /// as tmux's `getshell` picks it (`compat::server_started`).
    pub fn default_shell(&self, session: SessionId) -> Option<String> {
        self.options
            .lookup("default-shell", &[options::Owner::Session(session)])
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    /// `default-command` for `session`: what a new pane runs when it is
    /// given no command, through its shell.
    pub fn default_command(&self, session: SessionId) -> Option<String> {
        self.options
            .lookup("default-command", &[options::Owner::Session(session)])
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    /// `base-index` for `session`: where new windows start numbering.
    pub fn base_index(&self, session: SessionId) -> u32 {
        self.number_option(
            "base-index",
            &[options::Owner::Session(session)],
            BASE_INDEX,
        )
    }

    /// `pane-base-index` for `window`: the index of its first pane.
    pub fn pane_base_index(&self, window: WindowId) -> u32 {
        let mut chain = vec![options::Owner::Window(window)];
        if let Some(w) = self.windows.get(&window) {
            chain.push(options::Owner::Session(w.session));
        }
        self.number_option("pane-base-index", &chain, PANE_BASE_INDEX)
    }

    /// `history-limit` for `session`: the scrollback a new pane gets.
    pub fn history_limit(&self, session: SessionId) -> usize {
        let chain = [options::Owner::Session(session)];
        self.number_option("history-limit", &chain, HISTORY_LIMIT as u32) as usize
    }

    pub fn window_at_index(&self, session: SessionId, index: u32) -> Option<WindowId> {
        let s = self.sessions.get(&session)?;
        s.windows
            .iter()
            .copied()
            .find(|w| self.index_in(session, *w) == Some(index))
    }

    /// Records that `pane`'s child exited and the master drained. Removes
    /// the pane (with the cascade) unless `remain-on-exit` keeps it.
    pub fn pane_exited(&mut self, pane: PaneId, status: Option<i32>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        if self.remain_on_exit(pane, status) {
            if let Some(p) = self.panes.get_mut(&pane) {
                p.dead = Some(status);
            }
        } else {
            self.remove_pane(pane);
        }
    }

    /// Removes a pane. Its window closes if this was its last pane;
    /// otherwise the window is unzoomed (as tmux 3.7b does whenever a pane
    /// leaves) and the remaining panes are laid out again. Returns the
    /// removed panes still live, for `pane-exited`.
    pub fn remove_pane(&mut self, pane: PaneId) -> Removed {
        let Some(window_id) = self.panes.get(&pane).map(|p| p.window) else {
            return Vec::new();
        };
        let Some(window) = self.windows.get_mut(&window_id) else {
            return self.take_panes(&[pane], None);
        };
        let order = window.pane_ids();
        if order.len() <= 1 {
            return self.remove_window(window_id);
        }
        if window.floating(pane).is_some() {
            window.floating.retain(|f| f.pane != pane);
            window.lose_pane(&order, pane);
            return self.take_panes(&[pane], None);
        }
        if window.tiled_ids().len() <= 1 {
            // Floating panes remain: the window stays, as tmux 3.7b's does.
            window.zoomed = false;
            window.layout.replace(pane, HOLE);
            window.lose_pane(&order, pane);
            return self.take_panes(&[pane], None);
        }
        window.zoomed = false;
        window.layout.remove(pane);
        window.lose_pane(&order, pane);
        let removed = self.take_panes(&[pane], None);
        self.relayout(window_id);
        removed
    }

    /// Removes a window and its panes, from every session it is linked
    /// into. A session closes if this was its last window. Returns the
    /// removed panes still live.
    pub fn remove_window(&mut self, window: WindowId) -> Removed {
        self.kill_window_everywhere(window, true)
    }

    /// Removes a session. Its windows go with it unless another session (a
    /// link elsewhere, a group member) holds them. Returns the removed panes
    /// still live.
    pub fn remove_session(&mut self, session: SessionId) -> Removed {
        let orphans = self.destroy_session(session);
        self.close_orphans(&orphans)
    }

    /// The live panes, as removing them all (server exit) would return.
    pub fn live_panes(&self) -> Removed {
        self.panes
            .values()
            .filter(|p| p.dead.is_none())
            .map(|p| (p.id, p.uid.clone(), self.place_of(p)))
            .collect()
    }

    /// Where `pane` is now.
    pub fn place_of(&self, pane: &Pane) -> Place {
        Place {
            window: Some(pane.window),
            session: self.windows.get(&pane.window).map(|w| w.session),
        }
    }

    pub(crate) fn panes_in(&self, keep: impl Fn(&Pane) -> bool) -> Vec<PaneId> {
        self.panes
            .values()
            .filter(|p| keep(p))
            .map(|p| p.id)
            .collect()
    }

    /// Drops `ids` from the pane map, returning those whose exit was not
    /// reported yet (a dead pane kept by `remain-on-exit` already was).
    /// `session` is the panes' session when their window is already gone.
    pub(crate) fn take_panes(&mut self, ids: &[PaneId], session: Option<SessionId>) -> Removed {
        let taken: Vec<Pane> = ids.iter().filter_map(|id| self.panes.remove(id)).collect();
        taken
            .into_iter()
            .filter(|p| p.dead.is_none())
            .map(|p| {
                let mut place = self.place_of(&p);
                place.session = place.session.or(session);
                (p.id, p.uid, place)
            })
            .collect()
    }

    /// Gives every pane in `window` its layout size, resizing the ptys whose
    /// size changed. A window too small for its panes squeezes some to
    /// nothing in the layout; the pane itself keeps at least one cell each
    /// way (tmux's `PANE_MINIMUM`).
    pub fn relayout(&mut self, window: WindowId) {
        self.relayout_with(window, false);
    }

    /// [`Model::relayout`] for a mouse drag's report: each pty learns its
    /// new size at most every 250 ms, as in tmux
    /// ([`crate::server::pane::PaneHandle::resize_held`]).
    pub fn relayout_held(&mut self, window: WindowId) {
        self.relayout_with(window, true);
    }

    fn relayout_with(&mut self, window: WindowId, held: bool) {
        let Some(w) = self.windows.get(&window) else {
            return;
        };
        let floating = w.floating.iter().map(|f| {
            let rect = Rect {
                x: 0,
                y: 0,
                cols: f.cols,
                rows: f.rows,
            };
            (f.pane, rect)
        });
        for (id, rect) in w.tiled_rects().into_iter().chain(floating) {
            let Some(p) = self.panes.get_mut(&id) else {
                continue;
            };
            let (cols, rows) = (rect.cols.max(1), rect.rows.max(1));
            if (p.cols, p.rows) != (cols, rows) {
                p.cols = cols;
                p.rows = rows;
                let resized = if held {
                    p.handle.resize_held(cols, rows)
                } else {
                    p.handle.resize(cols, rows)
                };
                if let Err(e) = resized {
                    eprintln!("grofe server: resizing {id}: {e}");
                }
            }
        }
    }

    /// The session a request with no target acts on: the one holding the
    /// request's pane, else the most recently active session (tmux's
    /// `cmd_find_best_session`; the first in name order on a tie).
    fn current_session(&self, ctx: &ClientCtx) -> Result<SessionId, String> {
        if ctx.pane.is_some() {
            let pane = self.current_pane(ctx)?;
            return Ok(self.session_of_pane(pane));
        }
        let mut best: Option<&Session> = None;
        for s in self.sessions.values() {
            let better = best.is_none_or(|b| {
                s.activity > b.activity || (s.activity == b.activity && s.name < b.name)
            });
            if better {
                best = Some(s);
            }
        }
        best.map(|s| s.id)
            .ok_or_else(|| "no current session".to_string())
    }

    fn current_window(&self, ctx: &ClientCtx) -> Result<WindowId, String> {
        if ctx.pane.is_some() {
            let pane = self.current_pane(ctx)?;
            return Ok(self.panes[&pane].window);
        }
        let s = self.current_session(ctx)?;
        Ok(self.sessions[&s].active)
    }

    fn current_pane(&self, ctx: &ClientCtx) -> Result<PaneId, String> {
        if let Some(pane) = ctx.pane {
            return if self.panes.contains_key(&pane) {
                Ok(pane)
            } else {
                Err(format!("can't find pane: {pane}"))
            };
        }
        let w = self.current_window(ctx)?;
        Ok(self.windows[&w].active)
    }

    /// Only for a pane known to be in the model (the maps are kept
    /// consistent by the insert and remove methods above).
    fn session_of_pane(&self, pane: PaneId) -> SessionId {
        let window = self.panes[&pane].window;
        self.link_session(window, None)
            .unwrap_or(self.windows[&window].session)
    }

    fn session_ref(&self, r: &SessionRef, ctx: &ClientCtx) -> Result<SessionId, String> {
        match r {
            SessionRef::Current => self.current_session(ctx),
            SessionRef::Id(id) => self
                .sessions
                .contains_key(id)
                .then_some(*id)
                .ok_or_else(|| format!("can't find session: {id}")),
            // tmux's `=name` as a session: never a prefix.
            SessionRef::Bare(word) if word.len() > 1 && word.starts_with('=') => {
                self.session_ref(&SessionRef::Exact(word[1..].to_string()), ctx)
            }
            SessionRef::Name(name) | SessionRef::Bare(name) => {
                if let Some(id) = self.session_by_name(name) {
                    return Ok(id);
                }
                let matches: Vec<SessionId> = self
                    .sessions
                    .values()
                    .filter(|s| s.name.starts_with(name.as_str()))
                    .map(|s| s.id)
                    .collect();
                match matches.as_slice() {
                    [id] => Ok(*id),
                    [] => Err(format!("can't find session: {name}")),
                    _ => Err(format!("ambiguous session: {name}")),
                }
            }
            // tmux's `=name` (FE.1): never a prefix.
            SessionRef::Exact(name) => self
                .session_by_name(name)
                .ok_or_else(|| format!("can't find session: {name}")),
            SessionRef::Mouse => Err(crate::cmd::target::NO_MOUSE_TARGET.to_string()),
            SessionRef::Marked => {
                let (session, ..) = self.marked_target()?;
                Ok(session)
            }
        }
    }

    /// tmux's `~` / `{marked}`: the marked pane and its window, or tmux's
    /// `no marked target`.
    fn marked_target(&self) -> Result<(SessionId, WindowId, PaneId), String> {
        let pane = self
            .marked_pane()
            .ok_or_else(|| "no marked target".to_string())?;
        let (session, window, _) = self.marked.ok_or_else(|| "no marked target".to_string())?;
        Ok((session, window, pane))
    }

    fn window_ref(
        &self,
        s: &SessionRef,
        w: &WindowRef,
        ctx: &ClientCtx,
    ) -> Result<WindowId, String> {
        match (s, w) {
            (_, WindowRef::Id(id)) => {
                if !self.windows.contains_key(id) {
                    return Err(format!("can't find window: {id}"));
                }
                if *s != SessionRef::Current && !self.has_link(self.session_ref(s, ctx)?, *id) {
                    return Err(format!("can't find window: {id}"));
                }
                Ok(*id)
            }
            (SessionRef::Current, WindowRef::Current) => self.current_window(ctx),
            (s, WindowRef::Current) => {
                let sid = self.session_ref(s, ctx)?;
                Ok(self.sessions[&sid].active)
            }
            (s, WindowRef::Index(index)) => {
                let sid = self.session_ref(s, ctx)?;
                self.window_at_index(sid, *index)
                    .ok_or_else(|| format!("can't find window: {index}"))
            }
            // tmux's `+N`/`-N` (FE.1): from the session's current window,
            // by index order, wrapping (recorded on tmux 3.7b).
            (s, WindowRef::Offset(n)) => {
                let sid = self.session_ref(s, ctx)?;
                let current = match s {
                    SessionRef::Current => self.current_window(ctx)?,
                    _ => self.sessions[&sid].active,
                };
                let windows = &self.sessions[&sid].windows;
                Ok(step(windows, current, *n))
            }
            (s, WindowRef::Name(name)) => {
                let sid = self.session_ref(s, ctx)?;
                let matches: Vec<WindowId> = self.sessions[&sid]
                    .windows
                    .iter()
                    .copied()
                    .filter(|w| self.windows.get(w).is_some_and(|w| &w.name == name))
                    .collect();
                match matches.as_slice() {
                    [id] => Ok(*id),
                    [] => Err(format!("can't find window: {name}")),
                    _ => Err(format!("ambiguous window: {name}")),
                }
            }
        }
    }

    fn pane_ref(&self, r: &PaneRef, ctx: &ClientCtx) -> Result<PaneId, String> {
        match r.pane {
            PanePart::Id(id) => {
                let pane = self
                    .panes
                    .get(&id)
                    .ok_or_else(|| format!("can't find pane: {id}"))?;
                let unqualified =
                    r.session == SessionRef::Current && r.window == WindowRef::Current;
                let mismatched = if unqualified {
                    false
                } else if r.window == WindowRef::Current {
                    // The window part is unspecified: the pane only needs
                    // to belong to the named session, not to that
                    // session's *active* window.
                    self.session_of_pane(id) != self.session_ref(&r.session, ctx)?
                } else {
                    pane.window != self.window_ref(&r.session, &r.window, ctx)?
                };
                if mismatched {
                    return Err(format!("can't find pane: {id}"));
                }
                Ok(id)
            }
            // tmux's `+N`/`-N` (FE.1): from the window's active pane, by
            // index order, wrapping (recorded on tmux 3.7b).
            PanePart::Offset(n) => {
                let w = self.window_ref(&r.session, &r.window, ctx)?;
                let window = &self.windows[&w];
                Ok(step(&window.pane_ids(), window.active, n))
            }
            PanePart::Index(index) => {
                let w = self.window_ref(&r.session, &r.window, ctx)?;
                index
                    .checked_sub(self.pane_base_index(w))
                    .and_then(|i| self.windows[&w].pane_ids().get(i as usize).copied())
                    .ok_or_else(|| format!("can't find pane: {index}"))
            }
        }
    }
}

impl Model {
    /// tmux's bare word as a window (cmd-find.c): `=word` is `word`, read
    /// the same way here since window names never match by prefix.
    fn bare_window(&self, word: &str, ctx: &ClientCtx) -> Result<WindowId, String> {
        let name = match word.strip_prefix('=') {
            Some(name) if !name.is_empty() => name,
            _ => word,
        };
        self.word_window(name, ctx)
            .map_err(|_| format!("can't find window: {name}"))
    }

    /// [`Model::word_window`] with the session it was found in: the current
    /// one for an index or a name, else the session the word names.
    fn word_winlink(&self, word: &str, ctx: &ClientCtx) -> Option<(SessionId, WindowId)> {
        let current = self.current_session(ctx).ok()?;
        let in_current = word
            .parse::<u32>()
            .ok()
            .map(WindowRef::Index)
            .into_iter()
            .chain([WindowRef::Name(word.to_string())])
            .find_map(|w| self.window_ref(&SessionRef::Current, &w, ctx).ok());
        match in_current {
            Some(w) => Some((current, w)),
            None => {
                let s = self
                    .session_ref(&SessionRef::Name(word.to_string()), ctx)
                    .ok()?;
                Some((s, self.sessions[&s].active))
            }
        }
    }

    /// A window of the current session by index, then by exact name, then
    /// a session's current window, as cmd-find.c tries them.
    fn word_window(&self, word: &str, ctx: &ClientCtx) -> Result<WindowId, String> {
        let by_index = word.parse::<u32>().ok().and_then(|index| {
            self.window_ref(&SessionRef::Current, &WindowRef::Index(index), ctx)
                .ok()
        });
        if let Some(w) = by_index {
            return Ok(w);
        }
        self.window_ref(
            &SessionRef::Current,
            &WindowRef::Name(word.to_string()),
            ctx,
        )
        .or_else(|_| {
            let sid = self.session_ref(&SessionRef::Name(word.to_string()), ctx)?;
            Ok(self.sessions[&sid].active)
        })
    }

    /// tmux's bare word as a pane (cmd-find.c): a pane index in the current
    /// window, then a window's active pane ([`Model::word_window`]). A
    /// leading `=` is part of the word here: tmux strips it only from a
    /// session or window part.
    fn bare_pane(&self, word: &str, ctx: &ClientCtx) -> Result<PaneId, String> {
        if let Ok(index) = word.parse::<u32>() {
            let pane = PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Index(index),
            };
            if let Ok(p) = self.pane_ref(&pane, ctx) {
                return Ok(p);
            }
        }
        self.word_window(word, ctx)
            .map(|w| self.windows[&w].active)
            .map_err(|_| format!("can't find pane: {word}"))
    }

    /// `target` as the window it names and the session it was found in
    /// (`b:3` and `a:1` can be one window): the session the target names,
    /// else the current one if it holds the window, else the most recently
    /// active session that does.
    pub fn resolve_winlink(
        &self,
        target: &Target,
        ctx: &ClientCtx,
    ) -> Result<(SessionId, WindowId), String> {
        // A bare word naming a session is that session's current window,
        // found in that session whatever the current one is.
        if let Target::Session(SessionRef::Bare(word)) = target {
            let word = word
                .strip_prefix('=')
                .filter(|n| !n.is_empty())
                .unwrap_or(word);
            if let Some(found) = self.word_winlink(word, ctx) {
                return Ok(found);
            }
        }
        let window = self.resolve_window(target, ctx)?;
        let named = match target {
            Target::Window(s, _) if *s != SessionRef::Current => Some(self.session_ref(s, ctx)?),
            Target::Session(s) if !matches!(s, SessionRef::Bare(_) | SessionRef::Current) => {
                Some(self.session_ref(s, ctx)?)
            }
            Target::Pane(r) if r.session != SessionRef::Current => {
                Some(self.session_ref(&r.session, ctx)?)
            }
            _ => None,
        };
        let prefer = named.or_else(|| self.current_session(ctx).ok());
        let session = self
            .link_session(window, prefer)
            .ok_or_else(|| format!("can't find window: {window}"))?;
        Ok((session, window))
    }

    /// `target` read as tmux reads a pane target, for a verb whose `-t` is
    /// a pane in tmux but whose grofe handler resolves it as a window or a
    /// session (`set-option`, `set-hook`, `select-layout`): a bare word
    /// becomes the pane it names. Every other target is returned as is.
    pub fn pane_type(&self, target: &Target, ctx: &ClientCtx) -> Result<Target, String> {
        match target {
            Target::Session(SessionRef::Bare(word)) => Ok(Target::Pane(PaneRef {
                session: SessionRef::Current,
                window: WindowRef::Current,
                pane: PanePart::Id(self.bare_pane(word, ctx)?),
            })),
            other => Ok(other.clone()),
        }
    }

    /// `target` read as tmux reads a window-index target (`new-window -t`,
    /// `move-window -t`): a bare number is that index in the current
    /// session, used or not; another bare word is a window of the current
    /// session by name, then a session. Every other target is returned as
    /// is.
    pub fn window_index_type(&self, target: &Target, ctx: &ClientCtx) -> Result<Target, String> {
        let Target::Session(SessionRef::Bare(word)) = target else {
            return Ok(target.clone());
        };
        let word = match word.strip_prefix('=') {
            Some(name) if !name.is_empty() => name,
            _ => word,
        };
        let current = self.current_session(ctx)?;
        let session = SessionRef::Id(current);
        if let Ok(index) = word.parse::<u32>() {
            return Ok(Target::Window(session, WindowRef::Index(index)));
        }
        let name = WindowRef::Name(word.to_string());
        if let Ok(w) = self.window_ref(&SessionRef::Current, &name, ctx)
            && let Some(index) = self.index_in(current, w)
        {
            return Ok(Target::Window(session, WindowRef::Index(index)));
        }
        match self.session_ref(&SessionRef::Name(word.to_string()), ctx) {
            Ok(sid) => Ok(Target::Session(SessionRef::Id(sid))),
            Err(_) => Err(format!("can't find window: {word}")),
        }
    }
}

/// The item `n` places after `from` in `items` (before it when `n` is
/// negative), wrapping round; `from` itself when it is not there.
fn step<T: Copy + PartialEq>(items: &[T], from: T, n: i32) -> T {
    match items.iter().position(|i| *i == from) {
        Some(at) => {
            let len = items.len() as i64;
            items[(at as i64 + n as i64).rem_euclid(len) as usize]
        }
        None => from,
    }
}

impl Resolve for Model {
    fn resolve_pane(&self, target: &Target, ctx: &ClientCtx) -> Result<PaneId, String> {
        match target {
            Target::Current => self.current_pane(ctx),
            Target::Pane(r) => self.pane_ref(r, ctx),
            Target::Session(SessionRef::Bare(word)) => self.bare_pane(word, ctx),
            Target::Session(SessionRef::Marked) => self.marked_target().map(|(.., pane)| pane),
            Target::Session(_) | Target::Window(..) => {
                let w = self.resolve_window(target, ctx)?;
                Ok(self.windows[&w].active)
            }
        }
    }

    fn resolve_window(&self, target: &Target, ctx: &ClientCtx) -> Result<WindowId, String> {
        match target {
            Target::Current => self.current_window(ctx),
            Target::Session(SessionRef::Bare(word)) => self.bare_window(word, ctx),
            Target::Session(SessionRef::Marked) => {
                self.marked_target().map(|(_, window, _)| window)
            }
            Target::Session(s) => {
                let sid = self.session_ref(s, ctx)?;
                Ok(self.sessions[&sid].active)
            }
            Target::Window(s, w) => self.window_ref(s, w, ctx),
            Target::Pane(r) => {
                let p = self.pane_ref(r, ctx)?;
                Ok(self.panes[&p].window)
            }
        }
    }

    fn resolve_session(&self, target: &Target, ctx: &ClientCtx) -> Result<SessionId, String> {
        match target {
            Target::Current => self.current_session(ctx),
            Target::Session(s) => self.session_ref(s, ctx),
            // The session the target names, which a window linked into
            // several sessions needs.
            Target::Window(..) => Ok(self.resolve_winlink(target, ctx)?.0),
            Target::Pane(_) => Ok(self.resolve_winlink(target, ctx)?.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::{Notify, mpsc};

    use super::*;
    use crate::server::pane::PaneSpawn;

    // `session:%pane` (an explicit session, no window part) has no string
    // form in today's target grammar (cmd/target.rs only builds
    // `PaneRef::window: Current` for a bare, fully-unqualified `%pane`), so
    // this drives `Resolve` directly with a hand-built `Target`, the same
    // way cmd/target.rs's own tests drive it through a `FakeModel`.

    fn spawn_pane(model: &mut Model, window: WindowId) -> PaneId {
        let id = model.alloc_pane_id();
        let (tx, _rx) = mpsc::channel(8);
        let spec = PaneSpawn {
            id,
            session: SessionId(0),
            argv: Some(vec!["sleep".to_string(), "30".to_string()]),
            shell: "/bin/sh".to_string(),
            login: true,
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            history_limit: 1000,
            socket: std::env::temp_dir().join("grofe-model-test.sock"),
            term: crate::server::pane::inner_term(),
            cell_px: None,
            graphics_ok: false,
            images: false,
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        model.panes.insert(
            id,
            Pane {
                id,
                uid: id.to_string(),
                window,
                argv: None,
                shell: "/bin/sh".to_string(),
                cwd: std::env::temp_dir(),
                resume: None,
                dead: None,
                cols: 80,
                rows: 24,
                input_off: false,
                handle,
            },
        );
        id
    }

    fn add_session(model: &mut Model, name: &str, active: WindowId) -> SessionId {
        let id = model.alloc_session_id();
        model.insert_session(Session {
            id,
            name: name.to_string(),
            windows: Vec::new(),
            active,
            env: Vec::new(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            last_windows: Vec::new(),
            activity: Instant::now(),
            attached: 0,
            created: std::time::SystemTime::now(),
        });
        id
    }

    fn ctx() -> ClientCtx {
        ClientCtx {
            cwd: std::env::temp_dir(),
            pane: None,
            env: Vec::new(),
            client: None,
            dialect: crate::protocol::Dialect::Grofe,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn qualified_pane_id_checks_the_session_not_the_active_window() {
        let mut model = Model::new(true);

        // Session "a" has two windows: 1 (active) and 2 (not).
        let w1 = model.alloc_window_id();
        let w2 = model.alloc_window_id();
        let p1 = spawn_pane(&mut model, w1);
        let p2 = spawn_pane(&mut model, w2);
        let a = add_session(&mut model, "a", w1);
        model.insert_window(Window {
            id: w1,
            session: a,
            index: 1,
            name: "one".to_string(),
            layout: LayoutTree::new(p1),
            active: p1,
            zoomed: false,
            cols: 80,
            rows: 24,
            bell_flag: false,
            activity_flag: false,
            silence_flag: false,
            activity: std::time::SystemTime::now(),
            last_panes: Vec::new(),
            active_points: Default::default(),
            floating: Vec::new(),
            pane_status: Default::default(),
            links: Vec::new(),
        });
        model.insert_window(Window {
            id: w2,
            session: a,
            index: 2,
            name: "two".to_string(),
            layout: LayoutTree::new(p2),
            active: p2,
            zoomed: false,
            cols: 80,
            rows: 24,
            bell_flag: false,
            activity_flag: false,
            silence_flag: false,
            activity: std::time::SystemTime::now(),
            last_panes: Vec::new(),
            active_points: Default::default(),
            floating: Vec::new(),
            pane_status: Default::default(),
            links: Vec::new(),
        });

        // A second session, owning nothing of "a"'s.
        let wb = model.alloc_window_id();
        add_session(&mut model, "b", wb);

        let ctx = ctx();

        // p2 lives in a's non-active window (w2). Qualified as session "a"
        // with the window part unspecified, it must still resolve: the
        // check is against the session, not against a's active window.
        let target = Target::Pane(PaneRef {
            session: SessionRef::Name("a".to_string()),
            window: WindowRef::Current,
            pane: PanePart::Id(p2),
        });
        assert_eq!(model.resolve_pane(&target, &ctx).unwrap(), p2);

        // The same pane qualified with a session that does not own it is
        // rejected.
        let target = Target::Pane(PaneRef {
            session: SessionRef::Name("b".to_string()),
            window: WindowRef::Current,
            pane: PanePart::Id(p2),
        });
        let err = model.resolve_pane(&target, &ctx).unwrap_err();
        assert!(err.contains(&format!("can't find pane: {p2}")), "{err}");
    }

    /// tmux's `last_panes`: `set_active` pushes the pane it replaces, and a
    /// pane that leaves drops out; the active one leaving hands over to
    /// the last pane, else the one before, else the one after.
    #[test]
    fn the_last_pane_stack_follows_tmux() {
        let (a, b, c) = (PaneId(1), PaneId(2), PaneId(3));
        let mut w = Window {
            id: WindowId(0),
            session: SessionId(0),
            index: 0,
            name: String::new(),
            layout: LayoutTree::new(a),
            active: a,
            zoomed: false,
            cols: 80,
            rows: 24,
            bell_flag: false,
            activity_flag: false,
            silence_flag: false,
            activity: std::time::SystemTime::now(),
            last_panes: Vec::new(),
            active_points: Default::default(),
            floating: Vec::new(),
            pane_status: Default::default(),
            links: Vec::new(),
        };
        assert_eq!(w.last_pane(), None);
        w.set_active(b);
        w.set_active(c);
        assert_eq!(w.last_pane(), Some(b));
        w.set_active(b);
        assert_eq!((w.last_pane(), w.last_panes.len()), (Some(c), 2));
        w.set_active(b);
        assert_eq!(w.last_pane(), Some(c));
        // The active pane leaves: the last pane takes over and leaves the
        // stack.
        w.lose_pane(&[a, b, c], b);
        assert_eq!((w.active, w.last_pane()), (c, Some(a)));
        // With no last pane: the one before, else the one after.
        w.last_panes.clear();
        w.lose_pane(&[a, c], c);
        assert_eq!(w.active, a);
        w.lose_pane(&[a, b], a);
        assert_eq!(w.active, b);
    }

    /// R13, as tmux 3.7b does it (recorded on a private socket): selecting
    /// a window clears its bell flag, reselecting the current window does
    /// not, and neither does anything to another window's flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn becoming_current_clears_the_bell_flag() {
        let mut model = Model::new(true);
        let (w1, w2) = (model.alloc_window_id(), model.alloc_window_id());
        let (p1, p2) = (spawn_pane(&mut model, w1), spawn_pane(&mut model, w2));
        let s = add_session(&mut model, "a", w1);
        for (id, index, pane) in [(w1, 1, p1), (w2, 2, p2)] {
            model.insert_window(Window {
                id,
                session: s,
                index,
                name: index.to_string(),
                layout: LayoutTree::new(pane),
                active: pane,
                zoomed: false,
                cols: 80,
                rows: 24,
                bell_flag: true,
                activity_flag: false,
                silence_flag: false,
                activity: std::time::SystemTime::now(),
                last_panes: Vec::new(),
                active_points: Default::default(),
                floating: Vec::new(),
                pane_status: Default::default(),
                links: Vec::new(),
            });
        }
        let flag = |m: &Model, w| m.windows[&w].bell_flag;
        model.select_window(w1);
        assert!(flag(&model, w1), "reselecting the current window keeps it");
        model.select_window(w2);
        assert!(!flag(&model, w2));
        assert!(flag(&model, w1), "the window left keeps its flag");
        model.select_window(w1);
        assert!(!flag(&model, w1));
    }
    /// tmux's `=name` (FE.1): only the exact name, where a plain name also
    /// takes a unique prefix.
    #[test]
    fn an_exact_session_never_matches_a_prefix() {
        let mut model = Model::new(true);
        let xy = add_session(&mut model, "xy", WindowId(0));
        let target = |r: SessionRef| Target::Session(r);
        let name = |s: &str| target(SessionRef::Name(s.to_string()));
        let exact = |s: &str| target(SessionRef::Exact(s.to_string()));
        assert_eq!(model.resolve_session(&name("x"), &ctx()), Ok(xy));
        assert_eq!(
            model.resolve_session(&exact("x"), &ctx()),
            Err("can't find session: x".to_string())
        );
        assert_eq!(model.resolve_session(&exact("xy"), &ctx()), Ok(xy));
        let x = add_session(&mut model, "x", WindowId(0));
        assert_eq!(model.resolve_session(&exact("x"), &ctx()), Ok(x));
        assert_eq!(
            model.resolve_session(&target(SessionRef::Mouse), &ctx()),
            Err("no mouse target".to_string())
        );
    }

    /// tmux's offsets wrap round in both directions.
    #[test]
    fn offsets_step_and_wrap() {
        let items = [1, 2, 3];
        assert_eq!(step(&items, 1, 1), 2);
        assert_eq!(step(&items, 3, 1), 1);
        assert_eq!(step(&items, 1, -1), 3);
        assert_eq!(step(&items, 2, 5), 1);
        assert_eq!(step(&items, 2, -7), 1);
        assert_eq!(step(&items, 9, 1), 9);
    }

    /// `#{W:}`, `#{S:}` and `#{P:}` visit in tmux's `sort.c` orders: windows
    /// by index (`n` by name, `t` by activity, newest first, `r` reversing
    /// whichever), sessions by id, panes by id. The recorded cases are
    /// `golden/fe_3/format-loops`; this one has the clocks the golden
    /// cannot fix.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loops_visit_in_the_orders_sort_c_gives() {
        use crate::cmd::{FormatVars, expand};
        use crate::server::dispatch::options::FormatCtx;
        use std::time::{Duration, SystemTime};

        let mut model = Model::new(true);
        let (w1, w2, w3) = (
            model.alloc_window_id(),
            model.alloc_window_id(),
            model.alloc_window_id(),
        );
        let (p1, p2, p3) = (
            spawn_pane(&mut model, w1),
            spawn_pane(&mut model, w2),
            spawn_pane(&mut model, w3),
        );
        let a = add_session(&mut model, "zed", w1);
        let t0 = SystemTime::now();
        // Index 1 "mid" is the oldest, 2 "zeta" the newest, 3 "alpha" between.
        for (id, index, name, pane, age) in [
            (w1, 1, "mid", p1, 30),
            (w2, 2, "zeta", p2, 0),
            (w3, 3, "alpha", p3, 10),
        ] {
            model.insert_window(Window {
                id,
                session: a,
                index,
                name: name.to_string(),
                layout: LayoutTree::new(pane),
                active: pane,
                zoomed: false,
                cols: 80,
                rows: 24,
                bell_flag: false,
                activity_flag: false,
                silence_flag: false,
                activity: t0 - Duration::from_secs(age),
                last_panes: Vec::new(),
                active_points: Default::default(),
                floating: Vec::new(),
                pane_status: Default::default(),
                links: Vec::new(),
            });
        }
        let wb = model.alloc_window_id();
        let pb = spawn_pane(&mut model, wb);
        let b = add_session(&mut model, "alpha", wb);
        model.insert_window(Window {
            id: wb,
            session: b,
            index: 1,
            name: "solo".to_string(),
            layout: LayoutTree::new(pb),
            active: pb,
            zoomed: false,
            cols: 80,
            rows: 24,
            bell_flag: false,
            activity_flag: false,
            silence_flag: false,
            activity: t0,
            last_panes: Vec::new(),
            active_points: Default::default(),
            floating: Vec::new(),
            pane_status: Default::default(),
            links: Vec::new(),
        });
        // `add_session` leaves the window list to the caller.
        model.sessions.get_mut(&a).unwrap().windows = vec![w1, w2, w3];
        model.sessions.get_mut(&a).unwrap().active = w2;

        let vars = FormatCtx::new(&model, p1);
        let names = |fmt: &str| expand(fmt, &vars);
        assert_eq!(names("#{W:#{window_name} }"), "mid zeta alpha ");
        assert_eq!(names("#{W/r:#{window_name} }"), "alpha zeta mid ");
        assert_eq!(names("#{W/n:#{window_name} }"), "alpha mid zeta ");
        assert_eq!(names("#{W/nr:#{window_name} }"), "zeta mid alpha ");
        assert_eq!(names("#{W/t:#{window_name} }"), "zeta alpha mid ");
        assert_eq!(names("#{W/tr:#{window_name} }"), "mid alpha zeta ");
        // The current window (2) takes the second branch.
        assert_eq!(names("#{W:#{window_index},[#{window_index}]}"), "1[2]3");
        assert_eq!(names("#{W:#{window_index}#{loop_last_flag}}"), "102031");
        // Sessions go by id (creation), whatever their names.
        assert_eq!(names("#{S:#{session_name} }"), "zed alpha ");
        assert_eq!(names("#{S/n:#{session_name} }"), "alpha zed ");
        assert_eq!(names("#{S/r:#{session_name} }"), "alpha zed ");
        // No client, so no session takes the second branch.
        assert_eq!(names("#{S:#{session_name},x}"), "zedalpha");
        assert_eq!(vars.get("loop_last_flag").as_deref(), Some("0"));
        // A window loop of a pane in a gone window fails the whole `#{}`.
        let gone = FormatCtx::new(&model, PaneId(999));
        assert_eq!(expand("a#{W:x}b", &gone), "a");
    }
}
