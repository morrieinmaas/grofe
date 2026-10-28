//! Windows linked into several sessions, and session groups
//! (`docs/linked-windows.md`).
//!
//! A window keeps every link it has in [`Window::links`] (session and index,
//! in tmux's `window->winlinks` order); a session keeps its windows ordered
//! by its own index for them. `Window::session` / `Window::index` mirror the
//! first link, for the code that has no session of its own. Every change to
//! a link goes through the methods here, which keep the three together.
//!
//! A session group shares one window list: after a change to a member,
//! [`Model::sync_group`] gives the others the same links (tmux's
//! `session_group_synchronize_from`), keeping each one's own current and
//! last window.

use crate::server::dispatch::options::Owner;
use crate::server::model::{Model, Removed};
use crate::types::{SessionId, WindowId};

/// A session group: its name is fixed when it is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    /// In the order they joined.
    pub members: Vec<SessionId>,
}

impl Model {
    /// The index `session` knows `window` by.
    pub fn index_in(&self, session: SessionId, window: WindowId) -> Option<u32> {
        self.windows
            .get(&window)?
            .links
            .iter()
            .find(|(s, _)| *s == session)
            .map(|(_, i)| *i)
    }

    pub fn has_link(&self, session: SessionId, window: WindowId) -> bool {
        self.index_in(session, window).is_some()
    }

    /// The session to act in for `window` when nothing says which: `prefer`
    /// if it holds the window, else the most recently active one that does.
    pub fn link_session(&self, window: WindowId, prefer: Option<SessionId>) -> Option<SessionId> {
        let w = self.windows.get(&window)?;
        if let Some(p) = prefer.filter(|p| w.links.iter().any(|(s, _)| s == p)) {
            return Some(p);
        }
        w.links
            .iter()
            .filter_map(|(s, _)| self.sessions.get(s))
            .max_by_key(|s| s.activity)
            .map(|s| s.id)
            .or_else(|| w.links.first().map(|(s, _)| *s))
    }

    /// The group `session` is in.
    pub fn group_of(&self, session: SessionId) -> Option<&Group> {
        self.groups.iter().find(|g| g.members.contains(&session))
    }

    /// The other sessions in `session`'s group.
    pub fn group_peers(&self, session: SessionId) -> Vec<SessionId> {
        self.group_of(session)
            .map(|g| {
                g.members
                    .iter()
                    .copied()
                    .filter(|s| *s != session)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Makes `session` a member of the group named `name`, made if needed.
    pub fn join_group(&mut self, name: &str, session: SessionId) {
        match self.groups.iter_mut().find(|g| g.name == name) {
            Some(g) => g.members.push(session),
            None => self.groups.push(Group {
                name: name.to_string(),
                members: vec![session],
            }),
        }
    }

    fn leave_group(&mut self, session: SessionId) {
        for g in &mut self.groups {
            g.members.retain(|s| *s != session);
        }
        self.groups.retain(|g| !g.members.is_empty());
    }

    /// Distinct sessions-or-groups holding `window`: tmux's
    /// `window_linked_sessions`.
    pub fn linked_units(&self, window: WindowId) -> usize {
        let Some(w) = self.windows.get(&window) else {
            return 0;
        };
        let mut units: Vec<Result<&str, SessionId>> = Vec::new();
        for (s, _) in &w.links {
            let unit = match self.group_of(*s) {
                Some(g) => Ok(g.name.as_str()),
                None => Err(*s),
            };
            if !units.contains(&unit) {
                units.push(unit);
            }
        }
        units.len()
    }

    pub(crate) fn refresh_primary(&mut self, window: WindowId) {
        if let Some(w) = self.windows.get_mut(&window)
            && let Some((s, i)) = w.links.first().copied()
        {
            (w.session, w.index) = (s, i);
        }
    }

    /// Puts `window` in `session`'s list at `index`, which must be free
    /// there. The group is not synchronized.
    pub fn add_link(&mut self, session: SessionId, window: WindowId, index: u32) {
        let Some(w) = self.windows.get_mut(&window) else {
            return;
        };
        w.links.push((session, index));
        self.refresh_primary(window);
        let at = self.sessions.get(&session).map(|s| {
            s.windows
                .iter()
                .position(|id| self.index_in(session, *id).is_some_and(|i| i > index))
                .unwrap_or(s.windows.len())
        });
        if let (Some(at), Some(s)) = (at, self.sessions.get_mut(&session)) {
            s.windows.insert(at, window);
        }
    }

    /// Takes `window` out of `session`'s list and last-window stack, with no
    /// choice of a new current window and no group sync.
    pub fn remove_link(&mut self, session: SessionId, window: WindowId) {
        if let Some(w) = self.windows.get_mut(&window) {
            w.links.retain(|(s, _)| *s != session);
        }
        self.refresh_primary(window);
        if let Some(s) = self.sessions.get_mut(&session) {
            s.windows.retain(|id| *id != window);
            s.last_windows.retain(|id| *id != window);
        }
    }

    /// Gives `window` the index `index` in `session`'s list, which must be
    /// free there.
    pub fn set_index(&mut self, session: SessionId, window: WindowId, index: u32) {
        if let Some(w) = self.windows.get_mut(&window)
            && let Some(link) = w.links.iter_mut().find(|(s, _)| *s == session)
        {
            link.1 = index;
        }
        self.refresh_primary(window);
        self.sort_windows(session);
    }

    pub fn sort_windows(&mut self, session: SessionId) {
        let Some(mut list) = self.sessions.get(&session).map(|s| s.windows.clone()) else {
            return;
        };
        list.sort_by_key(|w| self.index_in(session, *w).unwrap_or(u32::MAX));
        if let Some(s) = self.sessions.get_mut(&session) {
            s.windows = list;
        }
    }

    /// Gives every other member of `from`'s group `from`'s windows at the
    /// same indexes (tmux's `session_group_synchronize1`): their links are
    /// dropped and added again in `from`'s order, and each keeps its current
    /// and last windows by index. A member whose current index is gone first
    /// moves, in its old list, to its last window, else the one before it
    /// (the last of all from the first), as `session_detach` does.
    pub fn sync_group(&mut self, from: SessionId) {
        let Some(source) = self.sessions.get(&from).map(|s| s.windows.clone()) else {
            return;
        };
        let source: Vec<(WindowId, u32)> = source
            .into_iter()
            .filter_map(|w| Some((w, self.index_in(from, w)?)))
            .collect();
        if source.is_empty() {
            return;
        }
        for member in self.group_peers(from) {
            let Some(s) = self.sessions.get(&member) else {
                continue;
            };
            let slot = |w: &WindowId| self.index_in(member, *w);
            let own: Vec<u32> = s.windows.iter().filter_map(slot).collect();
            let mut active = slot(&s.active);
            let mut last: Vec<u32> = s.last_windows.iter().filter_map(slot).collect();
            if let Some(cur) = active
                && !source.iter().any(|(_, i)| *i == cur)
            {
                let before = own.iter().rev().find(|i| **i < cur).or(own.last()).copied();
                let to = last.last().copied().or(before).filter(|i| *i != cur);
                if let Some(to) = to {
                    last.retain(|i| *i != to);
                    last.push(cur);
                    active = Some(to);
                }
            }
            for w in s.windows.clone() {
                if let Some(win) = self.windows.get_mut(&w) {
                    win.links.retain(|(s, _)| *s != member);
                }
                self.refresh_primary(w);
            }
            for (w, i) in &source {
                if let Some(win) = self.windows.get_mut(w) {
                    win.links.push((member, *i));
                }
                self.refresh_primary(*w);
            }
            let at = |i: u32| source.iter().find(|(_, x)| *x == i).map(|(w, _)| *w);
            let Some(s) = self.sessions.get_mut(&member) else {
                continue;
            };
            s.windows = source.iter().map(|(w, _)| *w).collect();
            s.last_windows = last.into_iter().filter_map(at).collect();
            if let Some(w) = active.and_then(at).or(source.first().map(|(w, _)| *w)) {
                s.active = w;
            }
        }
    }

    /// With `renumber-windows` on, `session`'s windows, and its group's, are
    /// numbered again from `base-index` in order, as tmux does when one of
    /// them goes (`server_renumber_session`).
    pub fn renumber_session(&mut self, session: SessionId) {
        let on = self
            .options
            .lookup("renumber-windows", &[Owner::Session(session)])
            == Some("on");
        if on {
            let mut all = self.group_peers(session);
            all.insert(0, session);
            for s in all {
                self.renumber_from_base(s);
            }
        }
    }

    /// [`Model::renumber_from_base`] if the session has `renumber-windows`
    /// on: `move-window` numbers the session the window left, and no group.
    pub fn renumber_alone(&mut self, session: SessionId) {
        if self.sessions.contains_key(&session)
            && self
                .options
                .lookup("renumber-windows", &[Owner::Session(session)])
                == Some("on")
        {
            self.renumber_from_base(session);
        }
    }

    /// `session`'s windows numbered from `base-index` in order. Like tmux's
    /// `session_renumber_windows`, it leaves the group alone: a member
    /// numbered by itself differs from the others until the next sync.
    pub fn renumber_from_base(&mut self, session: SessionId) {
        let base = self.base_index(session);
        let Some(list) = self.sessions.get(&session).map(|s| s.windows.clone()) else {
            return;
        };
        for (i, w) in list.into_iter().enumerate() {
            self.set_index(session, w, base + i as u32);
        }
    }

    /// Takes `window` out of `session` as tmux's `session_detach` does: if it
    /// was the current window, the last window takes over, else the one
    /// before (the last of all when it was first). The group follows. A
    /// session left with no windows goes, and its whole group with it. The
    /// window itself stays in the model, even with no link left: the caller
    /// closes it ([`Model::close_orphans`]).
    pub fn detach_window(&mut self, session: SessionId, window: WindowId) {
        let Some(s) = self.sessions.get_mut(&session) else {
            return;
        };
        // The window before it, the last one when it is first (tmux's
        // `session_previous` wraps).
        let n = s.windows.len();
        let pos = s.windows.iter().position(|id| *id == window).unwrap_or(0);
        let before = s.windows[(pos + n - 1) % n];
        let was_current = s.active == window;
        s.last_windows.retain(|id| *id != window);
        let last = if was_current {
            s.last_windows.pop()
        } else {
            None
        };
        self.remove_link(session, window);
        let Some(s) = self.sessions.get_mut(&session) else {
            return;
        };
        if s.windows.is_empty() {
            self.destroy_group_of(session);
            return;
        }
        if was_current {
            s.active = last.unwrap_or(before);
        }
        self.sync_group(session);
    }

    /// Destroys `session` and its group's other members: their window lists
    /// are shared, so they are empty too.
    fn destroy_group_of(&mut self, session: SessionId) {
        let mut doomed = self.group_peers(session);
        doomed.insert(0, session);
        for s in doomed {
            self.destroy_session(s);
        }
    }

    /// Removes the session entry and its links, and records it for
    /// `session-closed` and `detach-on-destroy`. Returns the windows left
    /// with no link, for [`Model::close_orphans`].
    pub(crate) fn destroy_session(&mut self, session: SessionId) -> Vec<WindowId> {
        let mode = self
            .options
            .lookup("detach-on-destroy", &[Owner::Session(session)])
            .unwrap_or("on")
            .to_string();
        let Some(s) = self.sessions.remove(&session) else {
            return Vec::new();
        };
        self.destroyed_sessions
            .insert(session, (s.name.clone(), mode));
        self.closed_sessions.push((session, s.name));
        self.client_last_session.retain(|_, last| *last != session);
        self.leave_group(session);
        let mut orphans = Vec::new();
        for w in s.windows {
            if let Some(win) = self.windows.get_mut(&w) {
                win.links.retain(|(l, _)| *l != session);
                if win.links.is_empty() {
                    orphans.push(w);
                }
            }
            self.refresh_primary(w);
        }
        orphans
    }

    /// Removes the windows `windows`, which have no link left, and their
    /// panes. Returns the panes still live.
    pub fn close_orphans(&mut self, windows: &[WindowId]) -> Removed {
        let mut removed = Vec::new();
        for window in windows {
            let Some(w) = self.windows.get(window).filter(|w| w.links.is_empty()) else {
                continue;
            };
            let session = w.session;
            let ids = self.panes_in(|p| p.window == *window);
            removed.extend(self.take_panes(&ids, Some(session)));
            self.windows.remove(window);
        }
        removed
    }

    /// Removes `window` from every session holding it, and its panes. A
    /// session left empty goes. With `renumber-windows` the sessions that
    /// held it are numbered again (tmux's `server_kill_window`; `unlink-window
    /// -k` does not ask for it, recorded).
    pub fn kill_window_everywhere(&mut self, window: WindowId, renumber: bool) -> Removed {
        let Some(w) = self.windows.get(&window) else {
            return Vec::new();
        };
        let sessions: Vec<SessionId> = w.links.iter().map(|(s, _)| *s).collect();
        let ids = self.panes_in(|p| p.window == window);
        let mut removed = self.take_panes(&ids, Some(w.session));
        for s in sessions {
            if self.has_link(s, window) {
                self.detach_window(s, window);
                if renumber {
                    self.renumber_session(s);
                }
            }
        }
        removed.extend(self.close_orphans(&[window]));
        removed
    }
}
