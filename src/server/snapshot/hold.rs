//! The loss hold: the state from before a pane died of a signal, kept
//! for a termination signal the server has not handled yet.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::writer::{Io, lock};
use super::{PaneSnap, Snapshot};

/// How long the state from before a pane went is kept for a termination
/// signal. The signal reaches the server before the panes it kills die,
/// but the thread running its handler can lose the CPU to the one that
/// hands the actor their exits.
// ponytail: a handler later than this after the exits misses them.
pub(super) const LOSS_HOLD: Duration = Duration::from_millis(500);

impl Io {
    /// Keeps `snap` as the last state built. When a pane of the one before
    /// is missing from it and a pane died of a signal (at `killed_at`)
    /// within [`LOSS_HOLD`], that one is held for [`LOSS_HOLD`] too: the
    /// pane may have died of a signal the server has not handled yet. A
    /// pane closed on purpose (kill-pane, `exit`) is not held.
    pub(super) fn keep(&self, snap: Option<Snapshot>, killed_at: Option<Instant>) {
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
    pub(super) fn pre_signal(&self) -> Option<Snapshot> {
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

#[cfg(test)]
mod tests {
    use super::super::writer::Disk;
    use super::super::{SessionSnap, WindowSnap, sample};
    use super::*;
    use crate::server::layout::{GeometryCell, LayoutGeometry};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

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
