//! The notify hooks grofe finds by looking at the model: tmux raises
//! `window-renamed`, `session-window-changed`, `pane-focus-in` and the like
//! from inside whatever changed the thing, grofe has many places that
//! change them, so after each message [`track`] compares sessions, windows,
//! panes and clients with what they were when it last looked and fires a
//! hook for each difference.
//!
//! Nothing is kept while no such hook is bound; binding the first one
//! takes the snapshot ([`start`]), so what happened before is not reported.
//! A change that is undone before [`track`] looks (a rename to the same
//! name) is not seen, which tmux would report.

use std::collections::{BTreeMap, BTreeSet};

use super::{Fired, State, notify, wanted};
use crate::types::{PaneId, SessionId, WindowId};

/// The hooks [`track`] fires.
const WATCHED: &[&str] = &[
    "session-renamed",
    "session-window-changed",
    "window-renamed",
    "window-pane-changed",
    "window-layout-changed",
    "window-size-changed",
    "window-linked",
    "window-unlinked",
    "pane-title-changed",
    "pane-mode-changed",
    "pane-focus-in",
    "pane-focus-out",
];

#[derive(Default)]
pub(super) struct Watch {
    /// The last look, once there is one.
    last: Option<Snap>,
}

#[derive(Clone, Default)]
struct Snap {
    sessions: BTreeMap<SessionId, SessionSnap>,
    windows: BTreeMap<WindowId, WindowSnap>,
    titles: BTreeMap<PaneId, String>,
    /// The modes each pane is in, oldest first (only while
    /// `pane-mode-changed` is bound).
    modes: BTreeMap<PaneId, Vec<String>>,
    /// Each window an attached, focused client shows, and the pane tmux
    /// counts as focused in it.
    focused: BTreeMap<WindowId, PaneId>,
}

#[derive(Clone)]
struct SessionSnap {
    name: String,
    current: WindowId,
}

#[derive(Clone)]
struct WindowSnap {
    name: String,
    active: PaneId,
    /// Only while `window-layout-changed` is bound: building it is the
    /// costly part of a look.
    layout: Option<String>,
    size: (u16, u16),
    sessions: BTreeSet<SessionId>,
}

pub(super) fn watched(state: &State) -> bool {
    WATCHED.iter().any(|name| wanted(state, name))
}

fn snapshot(state: &State, before: Option<&Snap>) -> Snap {
    let m = &state.model;
    let layouts = wanted(state, "window-layout-changed");
    let focus_events = m.options.lookup("focus-events", &[]) == Some("on");
    let mut snap = Snap::default();
    for s in m.sessions.values() {
        let session = SessionSnap {
            name: s.name.clone(),
            current: s.active,
        };
        snap.sessions.insert(s.id, session);
    }
    for w in m.windows.values() {
        let window = WindowSnap {
            name: w.name.clone(),
            active: w.active,
            layout: layouts.then(|| crate::server::dispatch::options::window_layout(w)),
            size: (w.cols, w.rows),
            sessions: w.links.iter().map(|(s, _)| *s).collect(),
        };
        snap.windows.insert(w.id, window);
    }
    if wanted(state, "pane-title-changed") {
        for pane in m.panes.keys() {
            snap.titles.insert(*pane, state.passthrough.title(*pane));
        }
    }
    if wanted(state, "pane-mode-changed") {
        for pane in m.panes.keys() {
            snap.modes.insert(*pane, pane_modes(state, *pane));
        }
    }
    for c in state.clients.values() {
        // A terminal that does not report focus is always focused.
        if focus_events && !c.focused {
            continue;
        }
        let Some(window) = m.sessions.get(&c.session).map(|s| s.active) else {
            continue;
        };
        let Some(active) = m.windows.get(&window).map(|w| w.active) else {
            continue;
        };
        // Without focus-events tmux only looks again when a client comes,
        // goes or changes window, not when the window's pane changes.
        let kept = before
            .and_then(|b| b.focused.get(&window))
            .filter(|p| !focus_events && m.panes.get(p).is_some_and(|p| p.window == window));
        snap.focused.insert(window, kept.copied().unwrap_or(active));
    }
    snap
}

/// The modes `pane` is in, the oldest first: the clock, copy mode over it,
/// a chooser over both.
fn pane_modes(state: &State, pane: PaneId) -> Vec<String> {
    let mut out = Vec::new();
    if state.clocks.is_active(pane) {
        out.push("clock-mode".to_string());
    }
    if state.copy.is_active(pane) {
        out.push(
            crate::server::copy::format_var(state, pane, "pane_mode")
                .unwrap_or_else(|| "copy-mode".to_string()),
        );
    }
    if let Some(name) = crate::server::tree::format_var(state, pane, "pane_mode") {
        out.push(name);
    }
    out
}

/// Takes the first snapshot if a watched hook has just been bound and
/// there is none, so changes from now on are reported.
pub(super) fn start(state: &mut State) {
    if state.hooks.watch.last.is_none() && watched(state) {
        state.hooks.watch.last = Some(snapshot(state, None));
    }
}

/// Fires the hooks for what changed since the last look.
pub fn track(state: &mut State) {
    if !watched(state) {
        state.hooks.watch.last = None;
        return;
    }
    let before = state.hooks.watch.last.take();
    let now = snapshot(state, before.as_ref());
    if let Some(before) = &before {
        for f in diff(state, before, &now) {
            if wanted(state, f.event) {
                notify(state, f);
            }
        }
    }
    state.hooks.watch.last = Some(now);
}

/// Takes a new look without firing anything: what commands a notify hook
/// runs changes fires no notify hooks, as in tmux (`notify_add` skips them
/// while a `CMDQ_STATE_NOHOOKS` item runs).
pub(super) fn absorb(state: &mut State) {
    if !watched(state) {
        state.hooks.watch.last = None;
        return;
    }
    let before = state.hooks.watch.last.take();
    state.hooks.watch.last = Some(snapshot(state, before.as_ref()));
}

/// After each command: what it changed fires now, so two changes one
/// request makes (a rename and an `after-*` hook's rename) fire twice, as
/// tmux fires each as it happens; inside a notify hook nothing fires.
pub(super) fn after_command(state: &mut State) {
    if state.hooks.quiet {
        absorb(state);
    } else {
        track(state);
    }
}

/// A command tmux reports on whether or not anything changed:
/// `rename-window` fires `window-renamed` even for the same name, and
/// `select-layout` and `next-layout` fire `window-layout-changed` twice
/// (`layout_set_select` and the command each notify; `select-layout -E`
/// once, as `layout_spread_out` does not). The change is taken
/// into the last look so the next comparison does not fire it again.
pub(super) fn window_event(state: &mut State, event: &'static str, window: WindowId, times: usize) {
    if !watched(state) {
        return;
    }
    if let (Some(last), Some(w)) = (
        state.hooks.watch.last.as_mut(),
        state.model.windows.get(&window),
    ) && let Some(snap) = last.windows.get_mut(&window)
    {
        match event {
            "window-renamed" => snap.name = w.name.clone(),
            _ => {
                if snap.layout.is_some() {
                    snap.layout = Some(crate::server::dispatch::options::window_layout(w));
                }
            }
        }
    }
    if !wanted(state, event) {
        return;
    }
    for _ in 0..times {
        if let Some(f) = window_fired(state, event, window) {
            notify(state, f);
        }
    }
}

/// Fires only the focus hooks, for the moments where tmux orders them
/// ahead of the client's own (an attach, a switch, a detach): whatever
/// else changed is left for the next [`track`].
pub fn track_focus(state: &mut State) {
    if !watched(state) {
        return;
    }
    let Some(before) = state.hooks.watch.last.take() else {
        return;
    };
    let now = snapshot(state, Some(&before));
    let mut merged = before.clone();
    merged.focused = now.focused;
    for f in diff(state, &before, &merged) {
        if f.event.starts_with("pane-focus") && wanted(state, f.event) {
            notify(state, f);
        }
    }
    state.hooks.watch.last = Some(merged);
}

/// A hook about a window: its active pane current, `hook_window` set.
fn window_fired(state: &State, event: &'static str, window: WindowId) -> Option<Fired> {
    let w = state.model.windows.get(&window)?;
    let mut f = Fired::new(event, Some(w.active));
    f.window = Some(window);
    f.vars.hook_window = Some(window);
    f.vars.hook_window_name = Some(w.name.clone());
    Some(f)
}

/// A hook about a session: the pane of its current window current.
fn session_fired(state: &State, event: &'static str, session: SessionId) -> Option<Fired> {
    let s = state.model.sessions.get(&session)?;
    let pane = state.model.windows.get(&s.active).map(|w| w.active);
    let mut f = Fired::new(event, pane);
    f.session = Some(session);
    f.vars.hook_session = Some(session);
    f.vars.hook_session_name = Some(s.name.clone());
    Some(f)
}

/// A hook about a pane: the pane current, its window set.
pub(super) fn pane_fired(state: &State, event: &'static str, pane: PaneId) -> Option<Fired> {
    let window = state.model.panes.get(&pane)?.window;
    let w = state.model.windows.get(&window)?;
    let mut f = Fired::new(event, Some(pane));
    f.vars.hook_pane = Some(pane);
    f.vars.hook_window = Some(window);
    f.vars.hook_window_name = Some(w.name.clone());
    Some(f)
}

fn diff(state: &State, before: &Snap, now: &Snap) -> Vec<Fired> {
    let mut out = Vec::new();
    // Focus first, as tmux updates it before it tells anyone of the change
    // that caused it.
    for (window, pane) in &before.focused {
        if now.focused.get(window) != Some(pane) {
            out.extend(pane_fired(state, "pane-focus-out", *pane));
        }
    }
    for (window, pane) in &now.focused {
        if before.focused.get(window) != Some(pane) {
            out.extend(pane_fired(state, "pane-focus-in", *pane));
        }
    }
    for (id, w) in &now.windows {
        let Some(was) = before.windows.get(id) else {
            // A new window is linked into the sessions that hold it.
            for s in &w.sessions {
                out.extend(linked(state, *s, *id));
            }
            continue;
        };
        for s in w.sessions.difference(&was.sessions) {
            out.extend(linked(state, *s, *id));
        }
        if w.active != was.active {
            out.extend(window_fired(state, "window-pane-changed", *id));
        }
        if w.layout.is_some() && was.layout.is_some() && w.layout != was.layout {
            out.extend(window_fired(state, "window-layout-changed", *id));
        }
        if w.size != was.size {
            out.extend(window_fired(state, "window-size-changed", *id));
        }
        if w.name != was.name {
            out.extend(window_fired(state, "window-renamed", *id));
        }
    }
    for (id, s) in &now.sessions {
        if before
            .sessions
            .get(id)
            .is_some_and(|b| b.current != s.current)
        {
            out.extend(session_fired(state, "session-window-changed", *id));
        }
    }
    for (id, was) in &before.windows {
        let kept = now.windows.get(id).map(|w| &w.sessions);
        for s in &was.sessions {
            if kept.is_none_or(|k| !k.contains(s)) {
                out.push(unlinked(state, before, *s, *id, was));
            }
        }
    }
    for (id, s) in &now.sessions {
        if before.sessions.get(id).is_some_and(|b| b.name != s.name) {
            out.extend(session_fired(state, "session-renamed", *id));
        }
    }
    for (pane, title) in &now.titles {
        if before.titles.get(pane).is_some_and(|t| t != title) {
            out.extend(pane_fired(state, "pane-title-changed", *pane));
        }
    }
    // One hook for each mode a pane left and each it entered, as tmux fires
    // one per `window_pane_set_mode` and `window_pane_reset_mode`.
    for (pane, now_modes) in &now.modes {
        let Some(was) = before.modes.get(pane) else {
            continue;
        };
        let kept = was
            .iter()
            .zip(now_modes)
            .take_while(|(a, b)| a == b)
            .count();
        let changes = (was.len() - kept) + (now_modes.len() - kept);
        for _ in 0..changes {
            out.extend(pane_fired(state, "pane-mode-changed", *pane));
        }
    }
    out
}

/// `window-linked`: run in the session and window, as tmux's
/// `notify_session_window`.
fn linked(state: &State, session: SessionId, window: WindowId) -> Option<Fired> {
    let mut f = window_fired(state, "window-linked", window)?;
    let s = state.model.sessions.get(&session)?;
    f.session = Some(session);
    f.vars.hook_session = Some(session);
    f.vars.hook_session_name = Some(s.name.clone());
    Some(f)
}

/// `window-unlinked`: the session or window may be gone, so the hook runs
/// in the session's current window if it is still there (else wherever
/// the server is), and names the window and session as they were.
fn unlinked(
    state: &State,
    before: &Snap,
    session: SessionId,
    window: WindowId,
    was: &WindowSnap,
) -> Fired {
    let pane = state
        .model
        .sessions
        .get(&session)
        .and_then(|s| state.model.windows.get(&s.active))
        .map(|w| w.active);
    let mut f = Fired::new("window-unlinked", pane);
    f.session = Some(session);
    f.vars.hook_session = Some(session);
    f.vars.hook_session_name = before.sessions.get(&session).map(|s| s.name.clone());
    f.vars.hook_window = Some(window);
    f.vars.hook_window_name = Some(was.name.clone());
    f
}
