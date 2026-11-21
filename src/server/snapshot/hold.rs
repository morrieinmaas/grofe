//! The loss hold: the state from before a pane died of a signal, kept for
//! a termination signal the server has not handled yet.

use std::time::{Duration, Instant};

use super::Snapshot;
use super::writer::{Io, lock};

/// How long the state from before a signal death is kept for a
/// termination signal. The signal reaches the server before the panes it
/// kills die, but the thread running its handler can lose the CPU to the
/// one that hands the actor their exits.
// ponytail: a handler later than this after the exits misses them.
pub(super) const LOSS_HOLD: Duration = Duration::from_millis(500);

/// The last build from before the first signal death in a run of them,
/// and when the most recent one was.
pub(super) struct Hold {
    pre_loss: Option<Snapshot>,
    last_death: Instant,
}

impl Hold {
    fn running(&self) -> bool {
        self.last_death.elapsed() < LOSS_HOLD
    }
}

impl Io {
    /// Keeps `snap` as the last state built. When a pane's process died of
    /// a signal since the build before (`signal_death`), that build is held
    /// for [`LOSS_HOLD`] from this death; a pane closed on purpose
    /// (kill-pane, `exit`) never arms it.
    pub(super) fn keep(&self, snap: Option<Snapshot>, signal_death: bool) {
        let mut latest = lock(&self.latest);
        let mut hold = lock(&self.hold);
        if hold.as_ref().is_some_and(|h| !h.running()) {
            *hold = None;
        }
        if signal_death {
            let last_death = Instant::now();
            match hold.as_mut() {
                Some(h) => h.last_death = last_death,
                None => {
                    let pre_loss = latest.clone();
                    *hold = Some(Hold {
                        pre_loss,
                        last_death,
                    });
                }
            }
        }
        *latest = snap;
    }

    /// What a termination signal writes: the state from before the first
    /// signal death when one was in the last [`LOSS_HOLD`], else the last
    /// state built.
    // ponytail: changes made between a pane's signal death and the
    // server's own signal (a close, a new window, a rename) are not
    // written; that matters only for a layout edited in a reboot's half
    // second.
    pub(super) fn pre_signal(&self) -> Option<Snapshot> {
        match lock(&self.hold).as_ref() {
            Some(h) if h.running() => h.pre_loss.clone(),
            _ => lock(&self.latest).clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::writer::Disk;
    use super::super::{PaneSnap, SessionSnap, WindowSnap, sample};
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
            hold: Mutex::new(None),
        }
    }

    /// Moves the most recent signal death back by `by`.
    fn age(io: &Io, by: Duration) {
        if let Some(h) = lock(&io.hold).as_mut() {
            h.last_death = h.last_death.checked_sub(by).expect("an instant");
        }
    }

    /// The race a reboot runs: the panes the signal killed reach the actor
    /// before the signal's handler runs. The signal writes the state from
    /// before the first of them died.
    #[test]
    fn a_signal_just_after_panes_died_of_it_writes_the_state_before_them() {
        let io = unwritten_io();
        let two = with_panes(&["a", "b"]);
        io.keep(Some(two.clone()), false);
        io.keep(Some(with_panes(&["a"])), true);
        io.keep(Some(with_panes(&[])), true);
        assert_eq!(io.pre_signal(), Some(two));
    }

    /// A pane closed on purpose (kill-pane, `exit`) does not arm the hold.
    #[test]
    fn a_pane_closed_on_purpose_is_not_held() {
        let io = unwritten_io();
        let one = with_panes(&["a"]);
        io.keep(Some(with_panes(&["a", "b"])), false);
        io.keep(Some(one.clone()), false);
        assert_eq!(io.pre_signal(), Some(one));
    }

    /// Past [`LOSS_HOLD`] with no further signal death, the hold clears.
    #[test]
    fn the_hold_clears_after_loss_hold() {
        let io = unwritten_io();
        let one = with_panes(&["a"]);
        io.keep(Some(with_panes(&["a", "b"])), false);
        io.keep(Some(one.clone()), true);
        age(&io, LOSS_HOLD);
        assert_eq!(io.pre_signal(), Some(one.clone()));
        io.keep(Some(one.clone()), false);
        assert!(lock(&io.hold).is_none());
    }

    /// Each signal death restarts the hold, which keeps the state from
    /// before the first.
    #[test]
    fn the_hold_runs_from_the_last_signal_death() {
        let io = unwritten_io();
        let two = with_panes(&["a", "b"]);
        io.keep(Some(two.clone()), false);
        io.keep(Some(with_panes(&["a"])), true);
        age(&io, LOSS_HOLD - Duration::from_millis(50));
        io.keep(Some(with_panes(&[])), true);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(io.pre_signal(), Some(two));
    }
}
