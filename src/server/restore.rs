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
//! Every pane gets its `uid` and `#{pane_restored}`; runtime ids are
//! fresh. A pane with a `resume` runs it directly, no shell (T4.4), its
//! pty allocated at once and its child spawned after the pane's stagger
//! delay ([`stagger_delay`]), which the server applies and never writes
//! into the argv; input sent meanwhile waits in the pty. Every other pane
//! gets a shell in its `cwd` at once (the home directory, with one log
//! line, when it is gone). Each start is logged with its offset from the
//! restore's start on the monotonic clock. When a process a restore
//! started from a resume exits, or cannot start, the pane respawns as a
//! shell in its `cwd` with the same `uid` and its resume cleared
//! ([`on_pane_exited`]); a pane's later processes follow `remain-on-exit`. A restored session takes the server's environment: the
//! snapshot holds none, its `update-environment` variables start unset,
//! and the first attach fills them, as every attach does
//! (`hooks::update_environment`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::paths::{self, Env};
use crate::server::dispatch::options::Owner;
use crate::server::dispatch::pane::{respawn, toggle_zoom};
use crate::server::dispatch::{spawn_pane, spawn_pane_delayed};
use crate::server::layout::{GeometryCell, LayoutGeometry, LayoutTree, Preset};
use crate::server::model::{Session, Window};
use crate::server::pane::usable_cwd;
use crate::server::snapshot::{
    self, OptionSnap, PaneSnap, SessionSnap, Snapshot, SnapshotError, WindowSnap,
};
use crate::server::{Job, State, adopt, hooks, resume};
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
    if from_tmux(state) {
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

/// `start-from-tmux` (`server::adopt`): the sessions of a running tmux
/// server, else of tmux-resurrect's newest save, restored in place of
/// grofe's own snapshot. Whether it restored anything.
fn from_tmux(state: &mut State) -> bool {
    if state.model.options.lookup("start-from-tmux", &[]) != Some("on") {
        return false;
    }
    let (snap, from) = match adopt::from_live_tmux() {
        Ok(snap) => (snap, "the running tmux server".to_string()),
        Err(live) => {
            let vars = hooks::variables(state, None);
            let get = |name: &str| {
                vars.iter()
                    .find(|v| v.name == name)
                    .and_then(|v| v.value.clone())
                    .unwrap_or_default()
            };
            let dir = state
                .model
                .options
                .lookup("resurrect-dir", &[])
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(crate::config::expand_value(d, &get)));
            let home = PathBuf::from(Env::from_process().home.unwrap_or_default());
            let Some(save) = adopt::resurrect_last(dir.as_deref(), &home) else {
                eprintln!("grofe server: start-from-tmux: {live}, and no resurrect save");
                return false;
            };
            let parsed = std::fs::read_to_string(&save)
                .map_err(|e| e.to_string())
                .and_then(|text| adopt::from_resurrect(&text));
            match parsed {
                Ok(snap) => (snap, save.display().to_string()),
                Err(e) => {
                    eprintln!(
                        "grofe server: start-from-tmux: {live}; {}: {e}",
                        save.display()
                    );
                    return false;
                }
            }
        }
    };
    match restore(state, snap) {
        Ok(r) => {
            eprintln!(
                "grofe server: restore-done: {} sessions, {} windows, {} panes from {from}",
                r.sessions, r.windows, r.panes
            );
            crate::server::hooks::restore_done(state);
            true
        }
        Err(e) => {
            eprintln!("grofe server: start-from-tmux: {e}");
            false
        }
    }
}

/// A restored pane's delay before its resume is spawned: the FNV-1a hash
/// of its `uid` modulo `stagger_ms` (`restore-stagger-ms`); 0 is no delay
/// at all. Panes without a resume are never delayed.
pub fn stagger_delay(uid: &str, stagger_ms: u64) -> Duration {
    match stagger_ms {
        0 => Duration::ZERO,
        ms => Duration::from_millis(paths::fnv1a64(uid.as_bytes()) % ms),
    }
}

/// One restore's start, on the monotonic clock, and its stagger.
#[derive(Clone, Copy)]
struct Clock {
    t0: Instant,
    stagger_ms: u64,
}

impl Clock {
    /// Milliseconds since the restore began.
    fn offset(&self) -> u128 {
        self.t0.elapsed().as_millis()
    }
}

/// The panes running a process a restore started from their resume, with
/// that spawn's generation: their exit respawns a shell
/// ([`on_pane_exited`]); a later process in the pane (another generation)
/// does not count.
// ponytail: one server per process, so one set per process; `State` has
// no field for it (rule 4).
static FROM_RESUME: Mutex<Vec<(PaneId, u64)>> = Mutex::new(Vec::new());

fn from_resume() -> std::sync::MutexGuard<'static, Vec<(PaneId, u64)>> {
    FROM_RESUME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Recreates everything in `snap` (T4.3, T4.4), only into a server with
/// no session. A session, window or pane the snapshot repeats is skipped
/// with a log line; a window whose layout does not hold its panes gets
/// them side by side.
pub fn restore(state: &mut State, snap: Snapshot) -> Result<RestoreReport, RestoreError> {
    if !state.model.sessions.is_empty() {
        return Err(RestoreError(NOT_EMPTY.to_string()));
    }
    let t0 = Instant::now();
    let sessions = dedupe(snap.sessions);
    for o in &snap.options {
        set_option(state, Owner::Global, o);
    }
    let clock = Clock {
        t0,
        stagger_ms: state
            .model
            .options
            .lookup("restore-stagger-ms", &[])
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000),
    };
    let mut report = RestoreReport::default();
    let mut made = Vec::new();
    for s in sessions {
        match restore_session(state, s, clock, &mut report) {
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
    clock: Clock,
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
        match restore_window(state, id, w, &env, clock, &mut spawned) {
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
    clock: Clock,
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
        let pane = restore_pane(state, (session, id), p, env, cwd, (cols, rows), clock)?;
        spawned.push(pane);
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
        frames: false,
        pane_gap: 0,
        drop_preview: None,
        links: Vec::new(),
    })
}

/// One pane of `p`'s window `place`: its resume after its delay, else a
/// shell at once. A resume whose spawn fails outright (`fork` itself)
/// gets a shell too, so it never aborts the restore.
fn restore_pane(
    state: &mut State,
    place: (SessionId, WindowId),
    p: &PaneSnap,
    env: &[(String, String)],
    cwd: PathBuf,
    size: (u16, u16),
    clock: Clock,
) -> Result<PaneId, String> {
    let resume = p.resume.clone().filter(|r| !r.is_empty());
    let mut delay = Duration::ZERO;
    let mut pane = None;
    if let Some(argv) = &resume {
        delay = stagger_delay(&p.uid, clock.stagger_ms);
        let made = spawn_pane_delayed(
            state,
            place,
            env.to_vec(),
            Some(argv.clone()),
            cwd.clone(),
            size,
            None,
            delay,
        );
        match made {
            Ok(id) => pane = Some(id),
            Err(e) => eprintln!(
                "grofe server: restore: pane {}: resume {}: {e}, starting a shell",
                p.uid,
                argv.join(" ")
            ),
        }
    }
    let resumed = pane.is_some();
    let pane = match pane {
        Some(pane) => pane,
        None => spawn_pane(state, place, env.to_vec(), None, cwd, size, None)?,
    };
    let Some(live) = state.model.panes.get_mut(&pane) else {
        return Ok(pane);
    };
    live.uid = p.uid.clone();
    live.restored = true;
    if resumed {
        // Saved again as it was: the spawn argv and the resume (which
        // `spawn_pane` took from what it ran).
        live.argv = p.argv.clone();
        from_resume().push((pane, live.handle.generation()));
    }
    if !delay.is_zero() {
        when_started(state, pane, clock);
        return Ok(pane);
    }
    // As every spawn: the command the pane runs (its resume, or a
    // `default-command`) is its resume, bound to its root process.
    resume::bind_root(live);
    log_started(state, pane, clock.offset());
    resume::spawned(state, pane, Default::default());
    Ok(pane)
}

/// Once `pane`'s delayed child is up, the actor binds its resume and logs
/// the start, as [`restore_pane`] does at once for a pane with no delay.
/// A pane respawned or gone first is left alone.
fn when_started(state: &State, pane: PaneId, clock: Clock) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    let started = p.handle.started();
    let generation = p.handle.generation();
    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        if !started.await {
            return;
        }
        let at = clock.offset();
        let job: Job = Box::new(move |state: &mut State| {
            let Some(live) = state.model.panes.get_mut(&pane) else {
                return;
            };
            if live.handle.generation() != generation {
                return;
            }
            resume::bind_root(live);
            log_started(state, pane, at);
            resume::spawned(state, pane, Default::default());
        });
        let _ = jobs.send(job).await;
    });
}

/// The log line of a restored pane's start, `at` ms into the restore: its
/// resume, or its shell. Nothing for a resume that could not start
/// ([`on_pane_exited`] logs that).
fn log_started(state: &State, pane: PaneId, at: u128) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    if p.handle.pid().is_none() {
        return;
    }
    let resumed = from_resume().contains(&(pane, p.handle.generation()));
    match (&p.resume, resumed) {
        (Some(argv), true) => eprintln!(
            "grofe server: restore: pane {}: resume started at +{at} ms: {}",
            p.uid,
            argv.join(" ")
        ),
        _ => eprintln!(
            "grofe server: restore: pane {}: shell started at +{at} ms",
            p.uid
        ),
    }
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
/// reported dead or removed. `true` means this module kept the pane and
/// the usual exit handling is skipped: when the process was one a restore
/// started from the pane's resume (it exited, or could not start), the
/// pane respawns as a shell in its `cwd`, same `uid`, with its resume
/// cleared (`resume-changed`) and one log line. Not while the server is
/// exiting.
pub fn on_pane_exited(state: &mut State, pane: PaneId) -> bool {
    let Some(p) = state.model.panes.get(&pane) else {
        return false;
    };
    let key = (pane, p.handle.generation());
    {
        let mut set = from_resume();
        let Some(at) = set.iter().position(|k| *k == key) else {
            return false;
        };
        set.swap_remove(at);
    }
    if state.exit {
        return false;
    }
    let ran = p.handle.pid().is_some();
    let uid = p.uid.clone();
    let cwd = usable_cwd(p.cwd.clone());
    let Some(p) = state.model.panes.get_mut(&pane) else {
        return false;
    };
    let argv = p.resume.clone().unwrap_or_default().join(" ");
    // The resume goes with its process (T4.2); the shell is not a spawn
    // command to rerun either.
    let had = p.resume.take().is_some() | p.expect.take().is_some();
    p.resume_binding = None;
    p.argv = None;
    let what = if ran { "exited" } else { "could not start" };
    let kept = match respawn(state, pane, None, Some(cwd.clone()), Vec::new()) {
        Ok(()) => {
            eprintln!(
                "grofe server: restore: pane {uid}: resume {what}: {argv}; a shell in {}",
                cwd.display()
            );
            true
        }
        Err(e) => {
            eprintln!("grofe server: restore: pane {uid}: resume {what}: {argv}; no shell: {e}");
            false
        }
    };
    if had {
        hooks::resume_changed(state, pane);
    }
    kept
}
