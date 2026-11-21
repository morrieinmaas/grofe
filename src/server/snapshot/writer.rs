//! The writer: the actor's side, the writer thread, and the final write.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime};

use super::hold::LOSS_HOLD;
use super::signals::{spawn_signals, terminating, wait_terminating, watch_termination};
use super::{PaneSnap, Snapshot, build, read, rfc3339, write_atomic};
use crate::paths::{self, Env};
use crate::server::model::Model;
use crate::server::{Job, State};
use crate::types::PaneId;

/// How often the writer samples and writes at most.
const DEBOUNCE: Duration = Duration::from_secs(1);

/// What the actor, the writer thread and the signal task share.
pub(super) struct Io {
    pub(super) path: PathBuf,
    /// Set once the pre-shutdown state is written: nothing more is.
    pub(super) frozen: AtomicBool,
    pub(super) disk: Mutex<Disk>,
    /// The last state built that may be written, for a signal.
    pub(super) latest: Mutex<Option<Snapshot>>,
    /// When the first pane in the last [`LOSS_HOLD`] went, and the state
    /// built before it.
    pub(super) before_loss: Mutex<Option<(Instant, Snapshot)>>,
}

#[derive(Default)]
pub(super) struct Disk {
    /// The last write's bytes with `written_at` empty.
    written: Option<Vec<u8>>,
    /// The existing file's version was looked at (before the first write).
    checked: bool,
    /// The last write failed and was logged.
    failing: bool,
}

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
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
    pub(super) fn write_final(&self, snap: Option<&Snapshot>) {
        let mut disk = lock(&self.disk);
        if let Some(snap) = snap {
            self.write(&mut disk, snap);
        }
        self.frozen.store(true, Ordering::SeqCst);
    }
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
    use super::super::sample;
    use super::*;
    use std::fs;
    use std::path::Path;

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
}
