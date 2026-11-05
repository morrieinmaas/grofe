//! Restore from a snapshot (spec §6): T4.3 rebuilds sessions, windows and
//! panes into an empty server; T4.4 runs resumes and keeps a restored pane
//! whose resumed process exits.
//!
//! A restore happens once, at server start, before the server reports
//! ready ([`at_start`]): when `start-server --restore` asked for it (the
//! CLI passes [`RESTORE_ENV`] to the server it starts), or when the
//! config's `restore-on-start` is on and `GROFE_RESTORE_ON_START=0` does
//! not force it off.
//!
//! Every pane gets a shell in its `cwd` (the home directory, with one log
//! line, when it is gone), its `uid` and `#{pane_restored}`; runtime ids
//! are fresh. A restored session takes the server's environment: the
//! snapshot holds none, its `update-environment` variables start unset,
//! and the first attach fills them, as every attach does
//! (`hooks::update_environment`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::paths::{self, Env};
use crate::server::State;
use crate::server::dispatch::options::Owner;
use crate::server::dispatch::{pane::toggle_zoom, spawn_pane};
use crate::server::layout::{GeometryCell, LayoutGeometry, LayoutTree, Preset};
use crate::server::model::{Session, Window};
use crate::server::pane::usable_cwd;
use crate::server::snapshot::{self, OptionSnap, SessionSnap, Snapshot, SnapshotError, WindowSnap};
use crate::types::{PaneId, Rect, SessionId, WindowId, cap_size};

/// Set to `1` in the environment of a server `start-server --restore`
/// starts: restore whatever the config says. The server drops it from its
/// environment at once ([`take_request`]).
pub const RESTORE_ENV: &str = "GROFE_RESTORE";

/// What a restore brought back, for the log and `restore-done`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
}

/// Why a restore did not happen; the text is what the caller reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreError(pub String);

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RestoreError {}

/// Whether the server's environment held [`RESTORE_ENV`] set to `1`.
static FORCED: AtomicBool = AtomicBool::new(false);

/// Takes [`RESTORE_ENV`] out of the process environment, remembering it
/// for [`at_start`], so nothing the server runs inherits it: not a pane,
/// a hook, a `pipe-pane` command (which takes the process environment)
/// nor a server one of them starts. Called by `run_daemon` before any
/// thread or runtime exists.
pub fn take_request() {
    if let Some(v) = std::env::var_os(RESTORE_ENV) {
        FORCED.store(v == "1", Ordering::SeqCst);
        // SAFETY: called before the server starts any thread, so nothing
        // reads the environment concurrently.
        unsafe { std::env::remove_var(RESTORE_ENV) };
    }
}

/// The error a restore into a server that has sessions gives.
pub const NOT_EMPTY: &str = "server not empty: restore needs an empty server";

/// Called once at server start, after the config is applied and before
/// the server reports ready: restores the snapshot when asked to.
pub fn at_start(state: &mut State) {
    let forced = FORCED.load(Ordering::SeqCst);
    let configured = state.options.restore_on_start
        && state.model.options.lookup("restore-on-start", &[]) == Some("on");
    if !forced && !configured {
        return;
    }
    let path = paths::snapshot_path(&Env::from_process(), &state.socket);
    // `read` moves an unknown version or an unreadable file aside itself.
    let snap = match snapshot::read(&path) {
        Ok(snap) => snap,
        Err(SnapshotError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("grofe server: restore: no snapshot at {}", path.display());
            return;
        }
        Err(e) => {
            eprintln!("grofe server: restore: {}: {e}", path.display());
            return;
        }
    };
    match restore(state, snap) {
        Ok(r) => {
            eprintln!(
                "grofe server: restore-done: {} sessions, {} windows, {} panes from {}",
                r.sessions,
                r.windows,
                r.panes,
                path.display()
            );
            crate::server::hooks::restore_done(state);
        }
        Err(e) => eprintln!("grofe server: restore: {e}"),
    }
}

/// Recreates everything in `snap` (T4.3, T4.4), only into a server with
/// no session. A session, window or pane the snapshot repeats is skipped
/// with a log line; a window whose layout does not hold its panes gets
/// them side by side.
pub fn restore(state: &mut State, snap: Snapshot) -> Result<RestoreReport, RestoreError> {
    if !state.model.sessions.is_empty() {
        return Err(RestoreError(NOT_EMPTY.to_string()));
    }
    let sessions = dedupe(snap.sessions);
    for o in &snap.options {
        set_option(state, Owner::Global, o);
    }
    let mut report = RestoreReport::default();
    let mut made = Vec::new();
    for s in sessions {
        match restore_session(state, s, &mut report) {
            Ok(Some(id)) => made.push(id),
            Ok(None) => {}
            Err(e) => {
                // Nothing half-made stays.
                for id in made {
                    let _ = state.model.remove_session(id);
                }
                return Err(RestoreError(e));
            }
        }
    }
    report.sessions = made.len();
    for id in made {
        crate::server::hooks::session_created(state, id);
    }
    Ok(report)
}

/// The snapshot's sessions without a repeated session name, window index
/// or pane `uid` (each repeat skipped, logged), nor an empty window or
/// session.
fn dedupe(sessions: Vec<SessionSnap>) -> Vec<SessionSnap> {
    let mut names = BTreeSet::new();
    let mut uids = BTreeSet::new();
    let mut out = Vec::new();
    for mut s in sessions {
        if !names.insert(s.name.clone()) {
            eprintln!(
                "grofe server: restore: session {} repeated, skipped",
                s.name
            );
            continue;
        }
        let mut indexes = BTreeSet::new();
        s.windows.retain_mut(|w| {
            if !indexes.insert(w.index) {
                eprintln!(
                    "grofe server: restore: window {}:{} repeated, skipped",
                    s.name, w.index
                );
                return false;
            }
            w.panes.retain(|p| {
                let fresh = uids.insert(p.uid.clone());
                if !fresh {
                    eprintln!("grofe server: restore: pane {} repeated, skipped", p.uid);
                }
                fresh
            });
            !w.panes.is_empty()
        });
        if !s.windows.is_empty() {
            out.push(s);
        }
    }
    out
}

fn set_option(state: &mut State, owner: Owner, o: &OptionSnap) {
    if let Err(e) = state.model.options.set(owner, &o.name, &o.value, true) {
        eprintln!("grofe server: restore: option {}: {e}", o.name);
    }
}

/// One session, its windows and panes; `None` when the snapshot's has a
/// name a session already took. Every pane made is removed again on
/// error.
fn restore_session(
    state: &mut State,
    s: SessionSnap,
    report: &mut RestoreReport,
) -> Result<Option<SessionId>, String> {
    let id = state.model.alloc_session_id();
    let env = crate::server::hooks::blank_session_environment(state, id);
    for o in &s.options {
        set_option(state, Owner::Session(id), o);
    }
    let mut windows = Vec::new();
    let mut spawned = Vec::new();
    for w in &s.windows {
        match restore_window(state, id, w, &env, &mut spawned) {
            Ok(window) => windows.push((window, w)),
            Err(e) => {
                for p in spawned {
                    state.model.panes.remove(&p);
                }
                return Err(e);
            }
        }
    }
    let active = windows
        .iter()
        .find(|(_, w)| w.index == s.active_window)
        .or(windows.first())
        .map(|(window, _)| window.id);
    let Some(active) = active else {
        return Ok(None);
    };
    let size = windows
        .iter()
        .find(|(window, _)| window.id == active)
        .map(|(window, _)| (window.cols, window.rows))
        .unwrap_or_default();
    state.model.insert_session(Session {
        id,
        name: s.name.clone(),
        windows: Vec::new(),
        active,
        env,
        cwd: usable_cwd(s.cwd.clone()),
        cols: size.0,
        rows: size.1,
        last_windows: Vec::new(),
        activity: Instant::now(),
        attached: 0,
        created: std::time::SystemTime::now(),
    });
    for (window, snap) in windows {
        let (wid, active_pane) = (window.id, window.active);
        state.model.insert_window(window);
        state.model.relayout(wid);
        if snap.zoomed {
            toggle_zoom(&mut state.model, wid, active_pane);
        }
        automatic_rename(state, wid, snap.automatic_rename);
        report.windows += 1;
    }
    report.panes += spawned.len();
    Ok(Some(id))
}

/// Gives the window the automatic-rename state it had: its own value when
/// the inherited one differs.
fn automatic_rename(state: &mut State, window: WindowId, on: bool) {
    let options = &mut state.model.options;
    let now = options.lookup("automatic-rename", &[Owner::Window(window)]) == Some("on");
    if now != on {
        let value = if on { "on" } else { "off" };
        let _ = options.set(Owner::Window(window), "automatic-rename", value, false);
    }
}

/// One window of session `session`, with its panes spawned (their ids
/// pushed to `spawned`) and laid out; not yet in the model.
fn restore_window(
    state: &mut State,
    session: SessionId,
    w: &WindowSnap,
    env: &[(String, String)],
    spawned: &mut Vec<PaneId>,
) -> Result<Window, String> {
    let id = state.model.alloc_window_id();
    for o in &w.options {
        set_option(state, Owner::Window(id), o);
    }
    let (cols, rows) = cap_size(w.cols, w.rows);
    let mut panes = Vec::new();
    for p in &w.panes {
        let cwd = pane_cwd(&p.uid, &p.cwd);
        let pane = spawn_pane(
            state,
            (session, id),
            env.to_vec(),
            None,
            cwd,
            (cols, rows),
            None,
        )?;
        spawned.push(pane);
        if let Some(live) = state.model.panes.get_mut(&pane) {
            live.uid = p.uid.clone();
            live.restored = true;
            // As every spawn: a `default-command` the pane runs is its
            // resume, bound to its root process.
            crate::server::resume::bind_root(live);
        }
        crate::server::resume::spawned(state, pane, Default::default());
        for o in &p.options {
            set_option(state, Owner::Pane(pane), o);
        }
        panes.push((p.uid.as_str(), pane));
    }
    let area = Rect {
        x: 0,
        y: 0,
        cols,
        rows,
    };
    let first = panes
        .first()
        .map(|(_, p)| *p)
        .ok_or("a window with no pane")?;
    let layout = layout(&w.layout, &panes, first, area);
    let active = panes
        .iter()
        .find(|(uid, _)| *uid == w.active_pane)
        .map_or(first, |(_, p)| *p);
    Ok(Window {
        id,
        session,
        index: w.index,
        name: w.name.clone(),
        layout,
        active,
        zoomed: false,
        cols,
        rows,
        bell_flag: false,
        activity_flag: false,
        silence_flag: false,
        activity: std::time::SystemTime::now(),
        last_panes: Vec::new(),
        active_points: Default::default(),
        floating: Vec::new(),
        pane_status: Default::default(),
        links: Vec::new(),
    })
}

/// Where a restored pane's shell starts: its `cwd`, or the home directory
/// with one log line when that is gone.
fn pane_cwd(uid: &str, cwd: &Path) -> PathBuf {
    let usable = usable_cwd(cwd.to_path_buf());
    if usable != cwd {
        eprintln!(
            "grofe server: restore: pane {uid}: {} is gone, starting in {}",
            cwd.display(),
            usable.display()
        );
    }
    usable
}

/// The window's layout: `geometry` with its cells' `uid`s made the panes'
/// ids, the panes in the snapshot's order. A geometry that does not hold
/// exactly these panes, or does not tile, gives them side by side.
fn layout(
    geometry: &LayoutGeometry<String>,
    panes: &[(&str, PaneId)],
    first: PaneId,
    area: Rect,
) -> LayoutTree {
    let ids: Vec<PaneId> = panes.iter().map(|(_, p)| *p).collect();
    let mut tree = LayoutTree::new(first);
    if ids.len() < 2 {
        return tree;
    }
    // A tree holding every pane in index order, for `import` to rearrange.
    // ponytail: the preset leaves `next-layout` going on from
    // even-horizontal; the snapshot does not hold where it was.
    let wide = Rect {
        x: 0,
        y: 0,
        cols: u16::MAX,
        rows: u16::MAX,
    };
    tree.preset(wide, Preset::EvenHorizontal, &ids);
    let by_uid: BTreeMap<&str, PaneId> = panes.iter().copied().collect();
    let imported = keyed(geometry, &by_uid).is_some_and(|g| tree.import(&g).is_ok());
    if !imported {
        eprintln!("grofe server: restore: a window's layout does not fit its panes");
        tree.preset(area, Preset::EvenHorizontal, &ids);
    }
    tree
}

/// `g` with every cell's `uid` replaced by its pane's id; `None` when a
/// cell names a pane the window lacks.
fn keyed(g: &LayoutGeometry<String>, ids: &BTreeMap<&str, PaneId>) -> Option<LayoutGeometry> {
    let cell = match &g.cell {
        GeometryCell::Pane(uid) => GeometryCell::Pane(*ids.get(uid.as_str())?),
        GeometryCell::Split { dir, children } => GeometryCell::Split {
            dir: *dir,
            children: children
                .iter()
                .map(|c| keyed(c, ids))
                .collect::<Option<_>>()?,
        },
    };
    Some(LayoutGeometry {
        x: g.x,
        y: g.y,
        cols: g.cols,
        rows: g.rows,
        cell,
    })
}

/// Called by the actor when `pane`'s process exits, before the pane is
/// reported dead or removed. `true` means this module kept the pane (T4.4
/// respawns a shell in a restored pane whose resume exited) and the usual
/// exit handling is skipped.
pub fn on_pane_exited(_state: &mut State, _pane: PaneId) -> bool {
    false
}
