//! The session snapshot (spec §6): the schema `state.json` holds, and its
//! reader and atomic writer. T4.1 builds snapshots from the model and
//! writes them on change; T4.3 and T4.4 restore from them.
//!
//! The types are the contract (names binding).
//!
//! **The writer.** Registered by one call in the actor loop ([`track`]),
//! which starts it the first time. It never depends on a client:
//!
//! - every second a job on the actor samples each pane's cwd (the
//!   foreground process's, as `#{pane_current_path}`), builds the snapshot
//!   from the model and hands it to a writer thread, which writes it only
//!   when its bytes (all but `written_at`) differ from the last write, so
//!   an idle server leaves the file alone;
//! - after every other actor message [`track`] rebuilds the snapshot in
//!   memory (no I/O), so a termination signal finds the state as it was;
//! - `kill-server` (any exit with sessions left) writes the model as it
//!   stands, synchronously, and SIGTERM, SIGHUP and SIGINT write the last
//!   state built before the signal; each then **freezes** the writer, so
//!   the panes dying and sessions closing after the signal never reach the
//!   file. The signal is seen in its handler ([`terminating`]), before the
//!   runtime delivers it. A pane exit it caused can still reach the actor
//!   before the handler runs (another thread runs first), so the state
//!   from before a pane went is kept for [`LOSS_HOLD`], and a signal in
//!   that time writes it;
//! - an empty state is written only once the model had sessions and lost
//!   them with no termination signal seen (the last pane exiting, or the
//!   last session killed): an empty server that never had a session never
//!   overwrites a snapshot.
//!
//! `--persist` is a sticky mark on an option (`Options::set`); options
//! with it are saved at every scope.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use serde::{Deserialize, Serialize};

use crate::paths::{self, Env};
use crate::server::dispatch::options::Owner;
use crate::server::layout::{GeometryCell, LayoutGeometry};
use crate::server::model::{Model, Window};
use crate::server::{Job, State};
use crate::types::{PaneId, SessionId};

/// The `version` this grofe writes and understands.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Everything a restore needs, and nothing secret: no environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// RFC 3339.
    pub written_at: String,
    pub grofe_version: String,
    /// Global options, `--persist` only.
    pub options: Vec<OptionSnap>,
    pub sessions: Vec<SessionSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnap {
    pub name: String,
    /// No environment: spec §6.
    pub cwd: PathBuf,
    /// The active window's index.
    pub active_window: u32,
    pub options: Vec<OptionSnap>,
    pub windows: Vec<WindowSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSnap {
    pub index: u32,
    pub name: String,
    pub automatic_rename: bool,
    pub cols: u16,
    pub rows: u16,
    /// TC.8's geometry, cells keyed by pane `uid`.
    pub layout: LayoutGeometry<String>,
    /// The active pane's `uid`.
    pub active_pane: String,
    pub zoomed: bool,
    pub options: Vec<OptionSnap>,
    pub panes: Vec<PaneSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnap {
    pub uid: String,
    pub cwd: PathBuf,
    pub argv: Option<Vec<String>>,
    pub resume: Option<Vec<String>>,
    /// `--persist` only.
    pub options: Vec<OptionSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionSnap {
    pub name: String,
    pub value: String,
}

/// Why a snapshot could not be read.
#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    /// A `version` this grofe does not know.
    UnknownVersion(u32),
    /// The file does not parse as a snapshot.
    Corrupt(String),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "{e}"),
            SnapshotError::UnknownVersion(v) => write!(f, "unknown snapshot version {v}"),
            SnapshotError::Corrupt(e) => write!(f, "unreadable snapshot: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Reads the snapshot at `path`. A `version` this grofe does not know
/// moves the file to `state.json.v<N>`, and one that does not parse to
/// `state.json.corrupt-<unix time>`, each with one log line, so the next
/// write never overwrites another grofe's state.
pub fn read(path: &Path) -> Result<Snapshot, SnapshotError> {
    let bytes = fs::read(path).map_err(SnapshotError::Io)?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(e) => return Err(corrupt(path, &e.to_string())),
    };
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(SNAPSHOT_VERSION) => {
            serde_json::from_value(value).map_err(|e| corrupt(path, &e.to_string()))
        }
        Some(v) => match u32::try_from(v) {
            Ok(v) => {
                let aside = beside(path, &format!("v{v}"));
                move_aside(path, &aside, &format!("has unknown version {v}"));
                Err(SnapshotError::UnknownVersion(v))
            }
            Err(_) => Err(corrupt(path, &format!("version {v} is out of range"))),
        },
        None => Err(corrupt(path, "no version")),
    }
}

/// `path` with `.<suffix>` appended to its file name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

fn corrupt(path: &Path, why: &str) -> SnapshotError {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = beside(path, &format!("corrupt-{secs}"));
    move_aside(path, &aside, &format!("is unreadable ({why})"));
    SnapshotError::Corrupt(why.to_string())
}

/// Renames `path` to `aside`, with one log line either way.
fn move_aside(path: &Path, aside: &Path, why: &str) {
    match fs::rename(path, aside) {
        Ok(()) => eprintln!(
            "grofe server: snapshot {} {why}; moved to {}",
            path.display(),
            aside.display()
        ),
        Err(e) => eprintln!(
            "grofe server: snapshot {} {why}; cannot move it to {}: {e}",
            path.display(),
            aside.display()
        ),
    }
}

/// Writes `snap` to `path` atomically: a temp file in the same directory
/// (mode `0600`), `fsync`, `rename` over `path`, `fsync` of the directory.
/// A crash at any point leaves either the old file or the new one.
pub fn write_atomic(path: &Path, snap: &Snapshot) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("snapshot path has no directory"))?;
    paths::ensure_private_dir(dir)?;
    let mut bytes = serde_json::to_vec_pretty(snap).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(name);
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        // A leftover temp file keeps its old mode through `open`.
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `t` as RFC 3339, UTC, whole seconds.
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---- building ----

/// The options at exactly `owner` that carry the `--persist` mark.
fn persisted(model: &Model, owner: Owner) -> Vec<OptionSnap> {
    // ponytail: scalar options only; list options (`status-format[1]`)
    // keep their mark but are not saved.
    model
        .options
        .at(owner)
        .filter(|(_, e)| e.persist)
        .map(|(name, e)| OptionSnap {
            name: name.clone(),
            value: e.value.clone(),
        })
        .collect()
}

/// The snapshot of `model` now, `written_at` left empty (set at write).
/// `cwds` holds each pane's last sampled cwd; a pane not sampled yet has
/// the cwd it was spawned in.
pub fn build(model: &Model, cwds: &HashMap<PaneId, PathBuf>) -> Snapshot {
    // Least recently used first: the restore makes them in this order, each
    // more recent than the one before, so the session used last is the one
    // a bare `grofe` attaches to again.
    let mut by_use: Vec<_> = model.sessions.values().collect();
    by_use.sort_by_key(|s| s.activity);
    let sessions = by_use
        .into_iter()
        .map(|s| SessionSnap {
            name: s.name.clone(),
            cwd: s.cwd.clone(),
            active_window: model.index_in(s.id, s.active).unwrap_or_default(),
            options: persisted(model, Owner::Session(s.id)),
            // ponytail: a window linked into several sessions is saved
            // only in its first one, so no uid is saved twice.
            windows: s
                .windows
                .iter()
                .filter_map(|id| model.windows.get(id))
                .filter(|w| w.session == s.id)
                .map(|w| window(model, s.id, w, cwds))
                .collect(),
        })
        .collect();
    Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: String::new(),
        grofe_version: env!("CARGO_PKG_VERSION").to_string(),
        options: persisted(model, Owner::Global),
        sessions,
    }
}

fn window(
    model: &Model,
    session: SessionId,
    w: &Window,
    cwds: &HashMap<PaneId, PathBuf>,
) -> WindowSnap {
    let uid = |id: PaneId| {
        model
            .panes
            .get(&id)
            .map(|p| p.uid.clone())
            .unwrap_or_default()
    };
    let chain = crate::server::dispatch::options::chain(model, w.active);
    // ponytail: floating panes (outside the layout) are not saved.
    let panes = w
        .tiled_ids()
        .into_iter()
        .filter_map(|id| model.panes.get(&id))
        .map(|p| PaneSnap {
            uid: p.uid.clone(),
            cwd: cwds.get(&p.id).unwrap_or(&p.cwd).clone(),
            argv: p.argv.clone(),
            resume: p.resume.clone(),
            options: persisted(model, Owner::Pane(p.id)),
        })
        .collect();
    WindowSnap {
        index: model.index_in(session, w.id).unwrap_or(w.index),
        name: w.name.clone(),
        automatic_rename: model.options.lookup("automatic-rename", &chain) == Some("on"),
        cols: w.cols,
        rows: w.rows,
        layout: by_uid(w.layout.export(w.area()), &uid),
        active_pane: uid(w.active),
        zoomed: w.zoomed,
        options: persisted(model, Owner::Window(w.id)),
        panes,
    }
}

/// `g` with its cells keyed by pane `uid` instead of runtime id.
fn by_uid(g: LayoutGeometry, uid: &impl Fn(PaneId) -> String) -> LayoutGeometry<String> {
    let cell = match g.cell {
        GeometryCell::Pane(id) => GeometryCell::Pane(uid(id)),
        GeometryCell::Split { dir, children } => GeometryCell::Split {
            dir,
            children: children.into_iter().map(|c| by_uid(c, uid)).collect(),
        },
    };
    LayoutGeometry {
        x: g.x,
        y: g.y,
        cols: g.cols,
        rows: g.rows,
        cell,
    }
}

// ---- the writer ----

/// How often the writer samples and writes at most.
const DEBOUNCE: Duration = Duration::from_secs(1);

/// How long the state from before a pane went is kept for a termination
/// signal. The signal reaches the server before the panes it kills die,
/// but the thread running its handler can lose the CPU to the one that
/// hands the actor their exits.
// ponytail: a handler later than this after the exits misses them.
const LOSS_HOLD: Duration = Duration::from_millis(500);

/// What the actor, the writer thread and the signal task share.
struct Io {
    path: PathBuf,
    /// Set once the pre-shutdown state is written: nothing more is.
    frozen: AtomicBool,
    disk: Mutex<Disk>,
    /// The last state built that may be written, for a signal.
    latest: Mutex<Option<Snapshot>>,
    /// When the first pane in the last [`LOSS_HOLD`] went, and the state
    /// built before it.
    before_loss: Mutex<Option<(Instant, Snapshot)>>,
}

#[derive(Default)]
struct Disk {
    /// The last write's bytes with `written_at` empty.
    written: Option<Vec<u8>>,
    /// The existing file's version was looked at (before the first write).
    checked: bool,
    /// The last write failed and was logged.
    failing: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Io {
    /// Writes `snap` unless the writer is frozen or the bytes are those of
    /// the last write.
    fn write(&self, disk: &mut Disk, snap: &Snapshot) {
        if self.frozen.load(Ordering::SeqCst) {
            return;
        }
        let Ok(key) = serde_json::to_vec(snap) else {
            return;
        };
        if disk.written.as_ref() == Some(&key) {
            return;
        }
        if !disk.checked {
            disk.checked = true;
            // An unknown or unreadable file is moved aside by `read`.
            let _ = read(&self.path);
        }
        let stamped = Snapshot {
            written_at: rfc3339(SystemTime::now()),
            ..snap.clone()
        };
        match write_atomic(&self.path, &stamped) {
            Ok(()) => {
                disk.written = Some(key);
                disk.failing = false;
            }
            Err(e) => {
                if !disk.failing {
                    eprintln!(
                        "grofe server: cannot write snapshot {}: {e}",
                        self.path.display()
                    );
                }
                disk.failing = true;
            }
        }
    }

    /// The last write, then the freeze. `snap` `None` freezes only.
    fn write_final(&self, snap: Option<&Snapshot>) {
        let mut disk = lock(&self.disk);
        if let Some(snap) = snap {
            self.write(&mut disk, snap);
        }
        self.frozen.store(true, Ordering::SeqCst);
    }

    /// Keeps `snap` as the last state built. When a pane of the one before
    /// is missing from it and a pane died of a signal (at `killed_at`)
    /// within [`LOSS_HOLD`], that one is held for [`LOSS_HOLD`] too: the
    /// pane may have died of a signal the server has not handled yet. A
    /// pane closed on purpose (kill-pane, `exit`) is not held.
    fn keep(&self, snap: Option<Snapshot>, killed_at: Option<Instant>) {
        let mut latest = lock(&self.latest);
        let killed = killed_at.is_some_and(|at| at.elapsed() < LOSS_HOLD);
        if let (true, Some(old), Some(new)) = (killed, latest.as_ref(), snap.as_ref())
            && lost_pane(old, new)
        {
            let mut held = lock(&self.before_loss);
            let now = Instant::now();
            match held.as_mut() {
                // A later death in the hold restarts it, keeping the
                // state from before the first.
                Some((at, _)) if at.elapsed() < LOSS_HOLD => *at = now,
                _ => *held = Some((now, old.clone())),
            }
        }
        *latest = snap;
    }

    /// What a termination signal writes: the state from before the first
    /// pane that died of a signal in a running hold, else the last one
    /// built.
    fn pre_signal(&self) -> Option<Snapshot> {
        match lock(&self.before_loss).as_ref() {
            Some((at, snap)) if at.elapsed() < LOSS_HOLD => return Some(snap.clone()),
            _ => {}
        }
        lock(&self.latest).clone()
    }
}

/// Whether a pane of `old` is missing from `new`.
fn lost_pane(old: &Snapshot, new: &Snapshot) -> bool {
    let uids = |s: &Snapshot| -> HashSet<String> { panes(s).map(|p| p.uid.clone()).collect() };
    !uids(old).is_subset(&uids(new))
}

fn panes(snap: &Snapshot) -> impl Iterator<Item = &PaneSnap> {
    snap.sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
}

/// The actor's side of the writer, kept in `State::snapshot_writer`.
pub(crate) struct Writer {
    io: Arc<Io>,
    /// Each pane's cwd at the last sample.
    cwds: HashMap<PaneId, PathBuf>,
    /// The model had a session at some look: from then on an empty model
    /// is a state the user chose, and is written.
    had_sessions: bool,
    /// When a pane's process last died of a signal ([`pane_killed`]).
    killed_at: Option<Instant>,
    thread: mpsc::Sender<Snapshot>,
}

impl Writer {
    fn start(state: &State) -> Writer {
        let path = paths::snapshot_path(&Env::from_process(), &state.socket);
        let io = Arc::new(Io {
            path,
            frozen: AtomicBool::new(false),
            disk: Mutex::new(Disk::default()),
            latest: Mutex::new(None),
            before_loss: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel::<Snapshot>();
        let thread_io = Arc::clone(&io);
        let spawned = std::thread::Builder::new()
            .name("grofe-snapshot".to_string())
            .spawn(move || {
                while let Ok(mut snap) = rx.recv() {
                    // Only the newest of what queued up matters.
                    while let Ok(newer) = rx.try_recv() {
                        snap = newer;
                    }
                    let mut disk = lock(&thread_io.disk);
                    thread_io.write(&mut disk, &snap);
                }
            });
        if let Err(e) = spawned {
            eprintln!("grofe server: cannot start the snapshot writer: {e}");
        }
        spawn_ticks(state.jobs.clone());
        spawn_signals(Arc::clone(&io));
        watch_termination();
        Writer {
            io,
            cwds: HashMap::new(),
            had_sessions: false,
            killed_at: None,
            thread: tx,
        }
    }

    /// Builds the snapshot; keeps it for a signal when it may be written.
    fn build(&mut self, model: &Model) -> Option<Snapshot> {
        self.had_sessions |= !model.sessions.is_empty();
        let snap = build(model, &self.cwds);
        let writable = !snap.sessions.is_empty() || self.had_sessions;
        let snap = writable.then_some(snap);
        self.io.keep(snap.clone(), self.killed_at);
        snap
    }
}

/// Set by the handler of SIGTERM, SIGHUP or SIGINT as the signal arrives.
static TERMINATING: AtomicBool = AtomicBool::new(false);

/// The handlers [`on_termination`] replaced (the runtime's), by signal.
static PREVIOUS: [OnceLock<SigAction>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];

/// The signals [`on_termination`] watches, in [`PREVIOUS`] order.
const WATCHED: [Signal; 3] = [Signal::SIGTERM, Signal::SIGHUP, Signal::SIGINT];

/// Whether a termination signal has reached this process. It is set in
/// the signal handler itself: the runtime hands a signal to its listeners
/// only on a later poll, and a pane the same signal killed can reach the
/// actor first. A signal handled before the pane exit it caused is seen
/// here by the time the actor gets that exit.
fn terminating() -> bool {
    TERMINATING.load(Ordering::SeqCst)
}

/// Sets [`TERMINATING`], then runs the handler it replaced.
extern "C" fn on_termination(
    sig: nix::libc::c_int,
    info: *mut nix::libc::siginfo_t,
    ctx: *mut std::ffi::c_void,
) {
    TERMINATING.store(true, Ordering::SeqCst);
    for (s, prev) in WATCHED.iter().zip(&PREVIOUS) {
        if *s as nix::libc::c_int == sig
            && let Some(act) = prev.get()
        {
            chain(act, sig, info, ctx);
        }
    }
}

/// Calls the handler `act` holds, the kind its `SA_SIGINFO` flag names
/// (`SigAction::handler` reads the flag); the default and ignore actions
/// are never stored.
fn chain(
    act: &SigAction,
    sig: nix::libc::c_int,
    info: *mut nix::libc::siginfo_t,
    ctx: *mut std::ffi::c_void,
) {
    match act.handler() {
        SigHandler::Handler(f) => f(sig),
        SigHandler::SigAction(f) => f(sig, info, ctx),
        SigHandler::SigDfl | SigHandler::SigIgn => {}
    }
}

/// Puts [`on_termination`] in front of the runtime's handlers of SIGTERM,
/// SIGHUP and SIGINT (installed by `serve` and [`spawn_signals`]).
fn watch_termination() {
    for (sig, slot) in WATCHED.into_iter().zip(&PREVIOUS) {
        let new = SigAction::new(
            SigHandler::SigAction(on_termination),
            SaFlags::SA_SIGINFO | SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        // SAFETY: `on_termination` is async-signal safe: an atomic store
        // and the handler it replaced.
        let Ok(old) = (unsafe { sigaction(sig, &new) }) else {
            eprintln!("grofe server: snapshot writer: cannot watch signal {sig}");
            continue;
        };
        if matches!(old.handler(), SigHandler::SigDfl | SigHandler::SigIgn) {
            // Nothing to chain to; the default is not ours to replace.
            // SAFETY: restoring the action just replaced.
            let _ = unsafe { sigaction(sig, &old) };
            continue;
        }
        // ponytail: a signal between the install and this `set` sets the
        // flag but does not run the runtime's handler (its stream sees the
        // signal only on a later poll, as before).
        let _ = slot.set(old);
    }
}

/// A job on the actor every [`DEBOUNCE`]: [`tick`].
fn spawn_ticks(jobs: tokio::sync::mpsc::Sender<Job>) {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(DEBOUNCE);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        every.tick().await;
        loop {
            every.tick().await;
            let job: Job = Box::new(tick);
            if jobs.send(job).await.is_err() {
                return;
            }
        }
    });
}

/// SIGTERM, SIGHUP and SIGINT: the last state built is written, and the
/// writer freezes. The actor handles the signal itself as before.
fn spawn_signals(io: Arc<Io>) {
    use tokio::signal::unix::{SignalKind, signal};
    let kinds = [
        SignalKind::terminate(),
        SignalKind::hangup(),
        SignalKind::interrupt(),
    ];
    let mut signals = Vec::new();
    for kind in kinds {
        match signal(kind) {
            Ok(s) => signals.push(s),
            Err(e) => eprintln!("grofe server: snapshot writer: signal handler: {e}"),
        }
    }
    if signals.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let any = futures_util::future::select_all(signals.iter_mut().map(|s| Box::pin(s.recv())));
        let _ = any.await;
        io.write_final(io.pre_signal().as_ref());
    });
}

/// Every [`DEBOUNCE`]: samples the panes' cwds, builds, and hands the
/// snapshot to the writer thread, which writes it if it changed.
fn tick(state: &mut State) {
    let Some(w) = state.snapshot_writer.as_mut() else {
        return;
    };
    if w.io.frozen.load(Ordering::SeqCst) || terminating() {
        return;
    }
    let model = &state.model;
    w.cwds.retain(|id, _| model.panes.contains_key(id));
    for p in model.panes.values() {
        if let Some(cwd) = p.handle.current_path() {
            w.cwds.insert(p.id, cwd);
        }
    }
    if let Some(snap) = w.build(model) {
        let _ = w.thread.send(snap);
    }
}

/// The writer's registration: run by the actor after every message
/// (`output` when it was pane output, which changes nothing saved). The
/// first call starts the writer. While the server runs it keeps the
/// state a signal would write; once the server is exiting it writes the
/// final state synchronously and freezes.
pub fn track(state: &mut State, output: bool) {
    if state.snapshot_writer.is_none() {
        state.snapshot_writer = Some(Writer::start(state));
    }
    let Some(w) = state.snapshot_writer.as_mut() else {
        return;
    };
    if w.io.frozen.load(Ordering::SeqCst) {
        return;
    }
    if terminating() {
        // What this message changed came after the signal: the state
        // built before it is the one to keep.
        w.io.write_final(w.io.pre_signal().as_ref());
        return;
    }
    if state.exit {
        // `kill-server` (or any exit that leaves sessions) writes them;
        // the last session going writes the empty state. When its panes
        // died of a signal, the server may have been sent it too (a
        // reboot) with its handler not run yet: it gets the rest of the
        // hold to show.
        // ponytail: a pane killed by a reboot's signal before the server
        // gets its own (the panes signalled first) is an ordinary exit
        // here, and the last one writes the empty state.
        if state.model.sessions.is_empty()
            && let Some(at) = w.killed_at
            && wait_terminating(LOSS_HOLD.saturating_sub(at.elapsed()))
        {
            w.io.write_final(w.io.pre_signal().as_ref());
            return;
        }
        let snap = w.build(&state.model);
        w.io.write_final(snap.as_ref());
        return;
    }
    if !output {
        w.build(&state.model);
    }
}

/// A pane's process died of a signal (its exit has no code): kept for the
/// last session going, see [`track`].
pub fn pane_killed(state: &mut State) {
    if let Some(w) = state.snapshot_writer.as_mut() {
        w.killed_at = Some(Instant::now());
    }
}

/// Whether a termination signal is seen within `within`.
fn wait_terminating(within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !terminating() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    terminating()
}

/// The `uid` of each pane whose entry the writer would change at its next
/// write: its entry in a snapshot of `model` now differs from the last
/// write's, or the last write lacks it. A change the writer holds but has
/// not written yet; `reboot-check` (T4.5) tells "not saved yet" from a
/// disk that disagrees with it. Empty once the writer is frozen or the
/// server is terminating (nothing more is written); every pane before the
/// first write.
pub fn pending_panes(state: &State) -> HashSet<String> {
    let model = &state.model;
    let all = || model.panes.values().map(|p| p.uid.clone()).collect();
    let Some(w) = state.snapshot_writer.as_ref() else {
        return all();
    };
    if w.io.frozen.load(Ordering::SeqCst) || terminating() {
        return HashSet::new();
    }
    let written: Option<Snapshot> = lock(&w.io.disk)
        .written
        .as_deref()
        .and_then(|bytes| serde_json::from_slice(bytes).ok());
    let Some(written) = written else {
        return all();
    };
    let panes = |s: &Snapshot| -> HashMap<String, PaneSnap> {
        s.sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.panes)
            .map(|p| (p.uid.clone(), p.clone()))
            .collect()
    };
    let before = panes(&written);
    panes(&build(model, &w.cwds))
        .into_iter()
        .filter(|(uid, p)| before.get(uid) != Some(p))
        .map(|(uid, _)| uid)
        .collect()
}

#[cfg(test)]
mod tests {

    static PLAIN: AtomicBool = AtomicBool::new(false);
    static INFO: AtomicBool = AtomicBool::new(false);

    extern "C" fn plain(_: nix::libc::c_int) {
        PLAIN.store(true, Ordering::SeqCst);
    }

    extern "C" fn info(
        _: nix::libc::c_int,
        _: *mut nix::libc::siginfo_t,
        _: *mut std::ffi::c_void,
    ) {
        INFO.store(true, Ordering::SeqCst);
    }

    /// The previous handler runs, of the kind its `SA_SIGINFO` flag names.
    /// Calls `chain` directly: raising a real signal would set the
    /// process-wide termination flag the other tests read.
    #[test]
    fn chain_calls_the_previous_handler_of_either_kind() {
        let none = SigSet::empty();
        let a = SigAction::new(SigHandler::Handler(plain), SaFlags::empty(), none);
        chain(&a, 15, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(PLAIN.load(Ordering::SeqCst) && !INFO.load(Ordering::SeqCst));
        let b = SigAction::new(SigHandler::SigAction(info), SaFlags::SA_SIGINFO, none);
        chain(&b, 15, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(INFO.load(Ordering::SeqCst));
    }
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            written_at: rfc3339(SystemTime::now()),
            grofe_version: "0.0.0".to_string(),
            options: vec![OptionSnap {
                name: "@g".to_string(),
                value: "1".to_string(),
            }],
            sessions: Vec::new(),
        }
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(951_782_400 + 3_661);
        assert_eq!(rfc3339(t), "2000-02-29T01:01:01Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_798_761_599);
        assert_eq!(rfc3339(t), "2026-12-31T23:59:59Z");
    }

    #[test]
    fn write_then_read_round_trips_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inst").join("state.json");
        let snap = sample();
        write_atomic(&path, &snap).unwrap();
        assert_eq!(read(&path).unwrap(), snap);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
    }

    #[test]
    fn an_unknown_version_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"version": 9}"#).unwrap();
        assert!(matches!(read(&path), Err(SnapshotError::UnknownVersion(9))));
        assert!(!path.exists());
        assert!(dir.path().join("state.json.v9").exists());
    }

    #[test]
    fn garbage_and_a_bad_shape_are_moved_aside_as_corrupt() {
        for text in ["nope", r#"{"version": 1}"#, r#"{"sessions": []}"#] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            fs::write(&path, text).unwrap();
            assert!(
                matches!(read(&path), Err(SnapshotError::Corrupt(_))),
                "{text}"
            );
            let moved: Vec<String> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(moved.len(), 1);
            assert!(moved[0].starts_with("state.json.corrupt-"), "{moved:?}");
        }
    }

    #[test]
    fn a_missing_file_is_an_io_error_and_nothing_moves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(matches!(read(&path), Err(SnapshotError::Io(_))));
    }

    #[test]
    fn a_frozen_writer_writes_nothing_and_equal_bytes_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let io = Io {
            path: dir.path().join("inst").join("state.json"),
            frozen: AtomicBool::new(false),
            disk: Mutex::new(Disk::default()),
            latest: Mutex::new(None),
            before_loss: Mutex::new(None),
        };
        let mut snap = sample();
        snap.written_at.clear();
        io.write(&mut lock(&io.disk), &snap);
        let ino = |p: &Path| std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap());
        let first = ino(&io.path);
        io.write(&mut lock(&io.disk), &snap);
        assert_eq!(ino(&io.path), first, "equal bytes rewrote the file");
        io.write_final(None);
        snap.options.clear();
        io.write(&mut lock(&io.disk), &snap);
        assert_eq!(read(&io.path).unwrap().options.len(), 1);
    }

    /// One session with a window for each pane `uid`.
    fn with_panes(uids: &[&str]) -> Snapshot {
        let window = |(i, uid): (usize, &&str)| WindowSnap {
            index: i as u32,
            name: format!("w{i}"),
            automatic_rename: false,
            cols: 80,
            rows: 24,
            layout: LayoutGeometry {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
                cell: GeometryCell::Pane(uid.to_string()),
            },
            active_pane: uid.to_string(),
            zoomed: false,
            options: Vec::new(),
            panes: vec![PaneSnap {
                uid: uid.to_string(),
                cwd: PathBuf::from("/"),
                argv: None,
                resume: None,
                options: Vec::new(),
            }],
        };
        Snapshot {
            sessions: vec![SessionSnap {
                name: "main".to_string(),
                cwd: PathBuf::from("/"),
                active_window: 0,
                options: Vec::new(),
                windows: uids.iter().enumerate().map(window).collect(),
            }],
            ..sample()
        }
    }

    fn unwritten_io() -> Io {
        Io {
            path: PathBuf::from("/nonexistent/state.json"),
            frozen: AtomicBool::new(false),
            disk: Mutex::new(Disk::default()),
            latest: Mutex::new(None),
            before_loss: Mutex::new(None),
        }
    }

    /// The race a reboot runs: the panes the signal killed reach the actor
    /// before the signal's handler runs. The signal writes the state from
    /// before the first of them went; a pane added is no loss.
    #[test]
    fn a_signal_just_after_panes_went_writes_the_state_before_them() {
        let io = unwritten_io();
        let two = with_panes(&["a", "b"]);
        let killed = Some(Instant::now());
        io.keep(Some(with_panes(&["a"])), None);
        io.keep(Some(two.clone()), None);
        assert_eq!(io.pre_signal(), Some(two.clone()));
        io.keep(Some(with_panes(&["a"])), killed);
        io.keep(Some(with_panes(&[])), killed);
        assert_eq!(io.pre_signal(), Some(two));
    }

    /// Moves the hold's start back by `by`.
    fn age(io: &Io, by: Duration) {
        if let Some((at, _)) = lock(&io.before_loss).as_mut() {
            *at = at.checked_sub(by).expect("an instant in the past");
        }
    }

    /// A pane gone longer than [`LOSS_HOLD`] did not die of the signal.
    #[test]
    fn a_pane_gone_before_the_hold_stays_gone() {
        let io = unwritten_io();
        let one = with_panes(&["a"]);
        let killed = Some(Instant::now());
        io.keep(Some(with_panes(&["a", "b"])), None);
        io.keep(Some(one.clone()), killed);
        age(&io, LOSS_HOLD);
        assert_eq!(io.pre_signal(), Some(one.clone()));
        // The next pane going is held anew.
        io.keep(Some(with_panes(&[])), killed);
        assert_eq!(io.pre_signal(), Some(one));
    }

    /// A pane closed on purpose (kill-pane, `exit`) is no signal death: a
    /// signal right after it writes the state without it.
    #[test]
    fn a_pane_closed_on_purpose_is_not_held() {
        let io = unwritten_io();
        let one = with_panes(&["a"]);
        io.keep(Some(with_panes(&["a", "b"])), None);
        io.keep(Some(one.clone()), None);
        assert_eq!(io.pre_signal(), Some(one));
        // Nor does a signal death longer than the hold ago arm it.
        let long_ago = Instant::now().checked_sub(LOSS_HOLD * 2);
        io.keep(Some(with_panes(&[])), long_ago);
        assert_eq!(io.pre_signal(), Some(with_panes(&[])));
    }

    /// Each signal death in the hold restarts it, keeping the state from
    /// before the first.
    #[test]
    fn the_hold_runs_from_the_last_signal_death() {
        let io = unwritten_io();
        let two = with_panes(&["a", "b"]);
        let killed = Some(Instant::now());
        io.keep(Some(two.clone()), None);
        io.keep(Some(with_panes(&["a"])), killed);
        age(&io, LOSS_HOLD - Duration::from_millis(50));
        io.keep(Some(with_panes(&[])), killed);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(io.pre_signal(), Some(two));
    }
}
