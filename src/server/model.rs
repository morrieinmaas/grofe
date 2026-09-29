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
use crate::protocol::ClientCtx;
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
pub type Removed = Vec<(PaneId, String)>;

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
    pub session: SessionId,
    pub index: u32,
    pub name: String,
    pub layout: LayoutTree,
    pub active: PaneId,
    pub zoomed: bool,
    pub cols: u16,
    pub rows: u16,
    /// tmux's `window_bell_flag` (R13): set by a bell (T3.3), cleared when
    /// the window becomes its session's current window.
    pub bell_flag: bool,
}

impl Window {
    /// The area the layout is computed against.
    pub fn area(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: self.cols,
            rows: self.rows,
        }
    }

    /// Pane ids in layout order: position `i` is pane index
    /// `i + pane-base-index`.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.layout
            .compute(self.area())
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }
}

pub struct Pane {
    pub id: PaneId,
    /// Durable identity (ULID); survives `respawn-pane`.
    pub uid: String,
    pub window: WindowId,
    /// What was spawned; `None` is the user's shell.
    pub argv: Option<Vec<String>>,
    pub cwd: PathBuf,
    /// The argv that brings this pane's process back; `None` starts a shell.
    pub resume: Option<Vec<String>>,
    /// `Some(status)` once the child exited and the master drained;
    /// `status` is `None` when a signal killed it.
    pub dead: Option<Option<i32>>,
    /// The size last given to the pty.
    pub cols: u16,
    pub rows: u16,
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
    next_session: u32,
    next_window: u32,
    next_pane: u32,
    had_session: bool,
}

impl Model {
    pub fn new(remain_on_exit: bool) -> Model {
        Model {
            options: Options::new(remain_on_exit),
            ..Model::default()
        }
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

    /// Inserts a window and adds it to its session's ordered window list.
    pub fn insert_window(&mut self, window: Window) {
        if let Some(session) = self.sessions.get_mut(&window.session) {
            let pos = session
                .windows
                .iter()
                .position(|w| self.windows.get(w).is_some_and(|w| w.index > window.index))
                .unwrap_or(session.windows.len());
            session.windows.insert(pos, window.id);
        }
        self.windows.insert(window.id, window);
    }

    /// Makes `window` its session's active window; the one it replaces
    /// goes on top of the session's last-window stack, as in tmux. A window
    /// that becomes current loses its bell flag (tmux's
    /// `winlink_clear_flags`); reselecting the current one changes nothing.
    pub fn select_window(&mut self, window: WindowId) {
        let Some(session) = self.windows.get(&window).map(|w| w.session) else {
            return;
        };
        if let Some(s) = self.sessions.get_mut(&session)
            && s.active != window
        {
            s.last_windows.retain(|w| *w != window);
            s.last_windows.push(s.active);
            s.active = window;
            if let Some(w) = self.windows.get_mut(&window) {
                w.bell_flag = false;
            }
        }
    }

    /// True once every session is gone, after at least one existed: the
    /// server should exit.
    pub fn closed(&self) -> bool {
        self.had_session && self.sessions.is_empty()
    }

    /// Whether `pane` stays after its process exits: `remain-on-exit`
    /// looked up pane over window over session over global.
    pub fn remain_on_exit(&self, pane: PaneId) -> bool {
        self.options
            .lookup(REMAIN_ON_EXIT, &options::chain(self, pane))
            == Some("on")
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
                    .filter_map(|w| self.windows.get(w).map(|w| w.index))
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
            .find(|w| self.windows.get(w).is_some_and(|w| w.index == index))
    }

    /// Records that `pane`'s child exited and the master drained. Removes
    /// the pane (with the cascade) unless `remain-on-exit` keeps it.
    pub fn pane_exited(&mut self, pane: PaneId, status: Option<i32>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        if self.remain_on_exit(pane) {
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
            return self.take_panes(&[pane]);
        };
        let order = window.pane_ids();
        if order.len() <= 1 {
            return self.remove_window(window_id);
        }
        let pos = order.iter().position(|p| *p == pane).unwrap_or(0);
        window.zoomed = false;
        window.layout.remove(pane);
        if window.active == pane {
            let rest: Vec<PaneId> = order.into_iter().filter(|p| *p != pane).collect();
            window.active = rest[pos.min(rest.len() - 1)];
        }
        let removed = self.take_panes(&[pane]);
        self.relayout(window_id);
        removed
    }

    /// Removes a window and its panes. Its session closes if this was its
    /// last window. Returns the removed panes still live.
    pub fn remove_window(&mut self, window: WindowId) -> Removed {
        let Some(w) = self.windows.remove(&window) else {
            return Vec::new();
        };
        let ids: Vec<PaneId> = self.panes_in(|p| p.window == window);
        let removed = self.take_panes(&ids);
        let Some(session) = self.sessions.get_mut(&w.session) else {
            return removed;
        };
        let pos = session.windows.iter().position(|id| *id == window);
        session.windows.retain(|id| *id != window);
        if session.windows.is_empty() {
            self.sessions.remove(&w.session);
            self.client_last_session
                .retain(|_, last| *last != w.session);
            return removed;
        }
        session.last_windows.retain(|w| *w != window);
        if session.active == window {
            // As tmux: the last window takes over, else the one at the
            // same position.
            session.active = match session.last_windows.pop() {
                Some(l) => l,
                None => session.windows[pos.unwrap_or(0).min(session.windows.len() - 1)],
            };
        }
        removed
    }

    /// Removes a session with all its windows and panes. Returns the
    /// removed panes still live.
    pub fn remove_session(&mut self, session: SessionId) -> Removed {
        let Some(s) = self.sessions.remove(&session) else {
            return Vec::new();
        };
        self.client_last_session.retain(|_, last| *last != session);
        for w in &s.windows {
            self.windows.remove(w);
        }
        let ids = self.panes_in(|p| s.windows.contains(&p.window));
        self.take_panes(&ids)
    }

    /// The live panes, as removing them all (server exit) would return.
    pub fn live_panes(&self) -> Removed {
        self.panes
            .values()
            .filter(|p| p.dead.is_none())
            .map(|p| (p.id, p.uid.clone()))
            .collect()
    }

    fn panes_in(&self, keep: impl Fn(&Pane) -> bool) -> Vec<PaneId> {
        self.panes
            .values()
            .filter(|p| keep(p))
            .map(|p| p.id)
            .collect()
    }

    /// Drops `ids` from the pane map, returning those whose exit was not
    /// reported yet (a dead pane kept by `remain-on-exit` already was).
    fn take_panes(&mut self, ids: &[PaneId]) -> Removed {
        ids.iter()
            .filter_map(|id| self.panes.remove(id))
            .filter(|p| p.dead.is_none())
            .map(|p| (p.id, p.uid.clone()))
            .collect()
    }

    /// Gives every pane in `window` its layout size, resizing the ptys whose
    /// size changed. A window too small for its panes squeezes some to
    /// nothing in the layout; the pane itself keeps at least one cell each
    /// way (tmux's `PANE_MINIMUM`).
    pub fn relayout(&mut self, window: WindowId) {
        let Some(w) = self.windows.get(&window) else {
            return;
        };
        for (id, rect) in w.layout.compute(w.area()) {
            let Some(p) = self.panes.get_mut(&id) else {
                continue;
            };
            let (cols, rows) = (rect.cols.max(1), rect.rows.max(1));
            if (p.cols, p.rows) != (cols, rows) {
                p.cols = cols;
                p.rows = rows;
                if let Err(e) = p.handle.resize(cols, rows) {
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
        self.windows[&self.panes[&pane].window].session
    }

    fn session_ref(&self, r: &SessionRef, ctx: &ClientCtx) -> Result<SessionId, String> {
        match r {
            SessionRef::Current => self.current_session(ctx),
            SessionRef::Id(id) => self
                .sessions
                .contains_key(id)
                .then_some(*id)
                .ok_or_else(|| format!("can't find session: {id}")),
            SessionRef::Name(name) => {
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
        }
    }

    fn window_ref(
        &self,
        s: &SessionRef,
        w: &WindowRef,
        ctx: &ClientCtx,
    ) -> Result<WindowId, String> {
        match (s, w) {
            (_, WindowRef::Id(id)) => {
                let window = self
                    .windows
                    .get(id)
                    .ok_or_else(|| format!("can't find window: {id}"))?;
                if *s != SessionRef::Current && window.session != self.session_ref(s, ctx)? {
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

impl Resolve for Model {
    fn resolve_pane(&self, target: &Target, ctx: &ClientCtx) -> Result<PaneId, String> {
        match target {
            Target::Current => self.current_pane(ctx),
            Target::Pane(r) => self.pane_ref(r, ctx),
            Target::Session(_) | Target::Window(..) => {
                let w = self.resolve_window(target, ctx)?;
                Ok(self.windows[&w].active)
            }
        }
    }

    fn resolve_window(&self, target: &Target, ctx: &ClientCtx) -> Result<WindowId, String> {
        match target {
            Target::Current => self.current_window(ctx),
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
            Target::Window(..) => {
                let w = self.resolve_window(target, ctx)?;
                Ok(self.windows[&w].session)
            }
            Target::Pane(r) => {
                let p = self.pane_ref(r, ctx)?;
                Ok(self.session_of_pane(p))
            }
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
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            history_limit: 1000,
            socket: std::env::temp_dir().join("grofe-model-test.sock"),
            term: crate::server::pane::inner_term(),
            cell_px: None,
            graphics_ok: false,
        };
        let handle = PaneHandle::spawn(spec, tx, Arc::new(Notify::new())).expect("spawn");
        model.panes.insert(
            id,
            Pane {
                id,
                uid: id.to_string(),
                window,
                argv: None,
                cwd: std::env::temp_dir(),
                resume: None,
                dead: None,
                cols: 80,
                rows: 24,
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
}
