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

use crate::cmd::{PanePart, PaneRef, Resolve, SessionRef, Target, WindowRef};
use crate::protocol::ClientCtx;
use crate::server::dispatch::options::{self, Options, REMAIN_ON_EXIT};
use crate::server::layout::LayoutTree;
use crate::server::pane::PaneHandle;
use crate::types::{PaneId, Rect, SessionId, WindowId};

// ponytail: `base-index`, `pane-base-index` and `history-limit` are fixed at
// their spec defaults until the options store (T1.5c) and config (M3) exist.
pub const BASE_INDEX: u32 = 1;
pub const PANE_BASE_INDEX: u32 = 1;
pub const HISTORY_LIMIT: usize = 50_000;
/// Size of a session created with no `-x`/`-y` and no client (tmux's
/// `default-size`).
pub const DEFAULT_SIZE: (u16, u16) = (80, 24);

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
    /// `i + PANE_BASE_INDEX`.
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

    /// Inserts a new session. Its windows (and their panes) must already
    /// be in the model.
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

    /// The lowest free window index at or above `BASE_INDEX` in `session`.
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
        (BASE_INDEX..)
            .find(|i| !used.contains(i))
            .unwrap_or(BASE_INDEX)
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
    /// otherwise the remaining panes are laid out again.
    pub fn remove_pane(&mut self, pane: PaneId) {
        let Some(window_id) = self.panes.get(&pane).map(|p| p.window) else {
            return;
        };
        let Some(window) = self.windows.get_mut(&window_id) else {
            self.panes.remove(&pane);
            return;
        };
        let order = window.pane_ids();
        if order.len() <= 1 {
            self.remove_window(window_id);
            return;
        }
        let pos = order.iter().position(|p| *p == pane).unwrap_or(0);
        window.layout.remove(pane);
        if window.active == pane {
            let rest: Vec<PaneId> = order.into_iter().filter(|p| *p != pane).collect();
            window.active = rest[pos.min(rest.len() - 1)];
        }
        self.panes.remove(&pane);
        self.relayout(window_id);
    }

    /// Removes a window and its panes. Its session closes if this was its
    /// last window.
    pub fn remove_window(&mut self, window: WindowId) {
        let Some(w) = self.windows.remove(&window) else {
            return;
        };
        self.panes.retain(|_, p| p.window != window);
        let Some(session) = self.sessions.get_mut(&w.session) else {
            return;
        };
        let pos = session.windows.iter().position(|id| *id == window);
        session.windows.retain(|id| *id != window);
        if session.windows.is_empty() {
            self.sessions.remove(&w.session);
            return;
        }
        if session.active == window {
            let pos = pos.unwrap_or(0).min(session.windows.len() - 1);
            session.active = session.windows[pos];
        }
    }

    /// Removes a session with all its windows and panes.
    pub fn remove_session(&mut self, session: SessionId) {
        let Some(s) = self.sessions.remove(&session) else {
            return;
        };
        for w in &s.windows {
            self.windows.remove(w);
        }
        self.panes.retain(|_, p| !s.windows.contains(&p.window));
    }

    /// Gives every pane in `window` its layout size, resizing the ptys whose
    /// size changed.
    pub fn relayout(&mut self, window: WindowId) {
        let Some(w) = self.windows.get(&window) else {
            return;
        };
        for (id, rect) in w.layout.compute(w.area()) {
            let Some(p) = self.panes.get_mut(&id) else {
                continue;
            };
            if (p.cols, p.rows) != (rect.cols, rect.rows) {
                p.cols = rect.cols;
                p.rows = rect.rows;
                if let Err(e) = p.handle.resize(rect.cols, rect.rows) {
                    eprintln!("grofe server: resizing {id}: {e}");
                }
            }
        }
    }

    /// The session a request with no target acts on: the one holding the
    /// request's pane, else the newest session.
    fn current_session(&self, ctx: &ClientCtx) -> Result<SessionId, String> {
        if ctx.pane.is_some() {
            let pane = self.current_pane(ctx)?;
            return Ok(self.session_of_pane(pane));
        }
        // ponytail: tmux picks the most recently used session; with no
        // clients before M2, the newest one stands in.
        self.sessions
            .keys()
            .next_back()
            .copied()
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
                if !unqualified && pane.window != self.window_ref(&r.session, &r.window, ctx)? {
                    return Err(format!("can't find pane: {id}"));
                }
                Ok(id)
            }
            PanePart::Index(index) => {
                let w = self.window_ref(&r.session, &r.window, ctx)?;
                index
                    .checked_sub(PANE_BASE_INDEX)
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
