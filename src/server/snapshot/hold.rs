//! The loss hold: panes that died of a signal, kept for a termination
//! signal the server has not handled yet. Only those panes are held, each
//! with its window and session as the last build that had it saw them;
//! the signal writes the latest state with them merged back, so nothing
//! closed on purpose returns and every other change stays current.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::writer::{Io, lock};
use super::{PaneSnap, SessionSnap, Snapshot, WindowSnap};
use crate::server::layout::{GeometryCell, LayoutGeometry};

/// How long a pane that died of a signal is kept for a termination
/// signal. The signal reaches the server before the panes it kills die,
/// but the thread running its handler can lose the CPU to the one that
/// hands the actor their exits.
// ponytail: a handler later than this after the exits misses them.
pub(super) const LOSS_HOLD: Duration = Duration::from_millis(500);

/// A pane that died of a signal, as the last build that had it saw it.
pub(super) struct Lost {
    at: Instant,
    /// Its session, `windows` left empty.
    session: SessionSnap,
    /// The session's place in the snapshot.
    place: usize,
    window: WindowSnap,
}

impl Io {
    /// Keeps `snap` as the last state built. Each pane of the one before
    /// that is missing from it and in `killed` (its process died of a
    /// signal) is held for [`LOSS_HOLD`]; a pane closed on purpose
    /// (kill-pane, `exit`) never is.
    pub(super) fn keep(&self, snap: Option<Snapshot>, killed: &HashSet<String>) {
        let mut latest = lock(&self.latest);
        let mut lost = lock(&self.lost);
        lost.retain(|_, l| l.at.elapsed() < LOSS_HOLD);
        if let (Some(old), Some(new)) = (latest.as_ref(), snap.as_ref())
            && !killed.is_empty()
        {
            let now: HashSet<&str> = panes(new).map(|p| p.uid.as_str()).collect();
            for (place, s) in old.sessions.iter().enumerate() {
                for w in &s.windows {
                    for p in &w.panes {
                        if killed.contains(&p.uid) && !now.contains(p.uid.as_str()) {
                            let session = SessionSnap {
                                windows: Vec::new(),
                                ..s.clone()
                            };
                            let at = Instant::now();
                            let window = w.clone();
                            lost.insert(
                                p.uid.clone(),
                                Lost {
                                    at,
                                    session,
                                    place,
                                    window,
                                },
                            );
                        }
                    }
                }
            }
        }
        *latest = snap;
    }

    /// What a termination signal writes: the last state built, with the
    /// panes that died of a signal in the last [`LOSS_HOLD`] put back.
    pub(super) fn pre_signal(&self) -> Option<Snapshot> {
        let mut snap = lock(&self.latest).clone()?;
        let lost = lock(&self.lost);
        let present: HashSet<String> = panes(&snap).map(|p| p.uid.clone()).collect();
        let mut fresh: Vec<(&String, &Lost)> = lost
            .iter()
            .filter(|(uid, l)| l.at.elapsed() < LOSS_HOLD && !present.contains(*uid))
            .collect();
        // The earliest loss of a window holds the most of it.
        fresh.sort_by_key(|(_, l)| l.at);
        let mut by_window: HashMap<(&str, u32), (&Lost, HashSet<String>)> = HashMap::new();
        for (uid, l) in fresh {
            by_window
                .entry((l.session.name.as_str(), l.window.index))
                .or_insert_with(|| (l, HashSet::new()))
                .1
                .insert(uid.clone());
        }
        for (l, back) in by_window.into_values() {
            put_back(&mut snap, l, &back);
        }
        Some(snap)
    }
}

/// Puts the panes `back` into `snap`: into their window, as `l` saw it,
/// keeping its panes still in `snap`; the window and session are made
/// again when they went too.
// ponytail: a window split or renumbered during the hold keeps its
// current shape, and its panes that died of a signal are not put back.
fn put_back(snap: &mut Snapshot, l: &Lost, back: &HashSet<String>) {
    let place = match snap.sessions.iter().position(|s| s.name == l.session.name) {
        Some(i) => i,
        None => {
            let i = l.place.min(snap.sessions.len());
            snap.sessions.insert(i, l.session.clone());
            i
        }
    };
    let session = &mut snap.sessions[place];
    let at = session
        .windows
        .iter()
        .position(|w| w.index == l.window.index);
    let mut keep = back.clone();
    if let Some(i) = at {
        let now: HashSet<&String> = session.windows[i].panes.iter().map(|p| &p.uid).collect();
        if !now
            .iter()
            .all(|u| l.window.panes.iter().any(|p| &p.uid == *u))
        {
            return;
        }
        keep.extend(now.into_iter().cloned());
    }
    let Some(window) = pruned(&l.window, &keep) else {
        return;
    };
    match at {
        Some(i) => session.windows[i] = window,
        None => {
            let i = session.windows.partition_point(|w| w.index < window.index);
            session.windows.insert(i, window);
        }
    }
    if !session
        .windows
        .iter()
        .any(|w| w.index == session.active_window)
        && let Some(w) = session.windows.first()
    {
        session.active_window = w.index;
    }
}

/// `w` with only the panes in `keep`; `None` when none is left.
fn pruned(w: &WindowSnap, keep: &HashSet<String>) -> Option<WindowSnap> {
    let panes: Vec<PaneSnap> = w
        .panes
        .iter()
        .filter(|p| keep.contains(&p.uid))
        .cloned()
        .collect();
    let first = panes.first()?.uid.clone();
    let active_pane = if keep.contains(&w.active_pane) {
        w.active_pane.clone()
    } else {
        first
    };
    // A layout that no longer tiles is laid out afresh by the restore.
    let layout = prune(w.layout.clone(), keep)?;
    Some(WindowSnap {
        layout,
        active_pane,
        panes,
        ..w.clone()
    })
}

/// `g` with the cells of panes not in `keep` taken out, a split left with
/// one child replaced by it.
fn prune(g: LayoutGeometry<String>, keep: &HashSet<String>) -> Option<LayoutGeometry<String>> {
    let cell = match g.cell {
        GeometryCell::Pane(uid) => GeometryCell::Pane(keep.contains(&uid).then_some(uid)?),
        GeometryCell::Split { dir, children } => {
            let mut children: Vec<_> = children
                .into_iter()
                .filter_map(|c| prune(c, keep))
                .collect();
            match children.len() {
                0 => return None,
                1 => children.pop()?.cell,
                _ => GeometryCell::Split { dir, children },
            }
        }
    };
    Some(LayoutGeometry { cell, ..g })
}

fn panes(snap: &Snapshot) -> impl Iterator<Item = &PaneSnap> {
    snap.sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
}

#[cfg(test)]
mod tests {
    use super::super::sample;
    use super::super::writer::Disk;
    use super::*;
    use crate::types::Dir;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    fn pane(uid: &str) -> PaneSnap {
        PaneSnap {
            uid: uid.to_string(),
            cwd: PathBuf::from("/"),
            argv: None,
            resume: None,
            options: Vec::new(),
        }
    }

    fn cell(uid: &str) -> LayoutGeometry<String> {
        LayoutGeometry {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
            cell: GeometryCell::Pane(uid.to_string()),
        }
    }

    /// A window `index` of the panes `uids`, side by side.
    fn window(index: u32, uids: &[&str]) -> WindowSnap {
        let layout = match uids {
            [one] => cell(one),
            _ => LayoutGeometry {
                cell: GeometryCell::Split {
                    dir: Dir::Horizontal,
                    children: uids.iter().map(|u| cell(u)).collect(),
                },
                ..cell("")
            },
        };
        WindowSnap {
            index,
            name: format!("w{index}"),
            automatic_rename: false,
            cols: 80,
            rows: 24,
            layout,
            active_pane: uids[0].to_string(),
            zoomed: false,
            options: Vec::new(),
            panes: uids.iter().map(|u| pane(u)).collect(),
        }
    }

    /// One session `main` of `windows`.
    fn with_windows(windows: Vec<WindowSnap>) -> Snapshot {
        Snapshot {
            sessions: vec![SessionSnap {
                name: "main".to_string(),
                cwd: PathBuf::from("/"),
                active_window: windows.first().map_or(0, |w| w.index),
                options: Vec::new(),
                windows,
            }],
            ..sample()
        }
    }

    /// One session with a window for each pane `uid`.
    fn with_panes(uids: &[&str]) -> Snapshot {
        let windows = uids
            .iter()
            .enumerate()
            .map(|(i, u)| window(i as u32, &[u]))
            .collect();
        with_windows(windows)
    }

    fn unwritten_io() -> Io {
        Io {
            path: PathBuf::from("/nonexistent/state.json"),
            frozen: AtomicBool::new(false),
            disk: Mutex::new(Disk::default()),
            latest: Mutex::new(None),
            lost: Mutex::new(HashMap::new()),
        }
    }

    fn set(uids: &[&str]) -> HashSet<String> {
        uids.iter().map(|u| u.to_string()).collect()
    }

    /// Moves every held pane's death back by `by`.
    fn age(io: &Io, by: Duration) {
        for l in lock(&io.lost).values_mut() {
            l.at = l.at.checked_sub(by).expect("an instant in the past");
        }
    }

    /// The race a reboot runs: the panes the signal killed reach the actor
    /// before the signal's handler runs. The signal writes them back, their
    /// windows and session too.
    #[test]
    fn a_signal_just_after_panes_died_of_it_writes_them_back() {
        let io = unwritten_io();
        let two = with_panes(&["a", "b"]);
        io.keep(Some(two.clone()), &set(&[]));
        io.keep(Some(with_panes(&["a"])), &set(&["b"]));
        let mut empty = with_panes(&[]);
        empty.sessions.clear();
        io.keep(Some(empty), &set(&["a"]));
        assert_eq!(io.pre_signal().expect("a state").sessions, two.sessions);
    }

    /// A pane closed on purpose (kill-pane, `exit`) is not held, even right
    /// after another pane died of a signal.
    #[test]
    fn a_pane_closed_on_purpose_is_not_held() {
        let io = unwritten_io();
        io.keep(Some(with_panes(&["a", "b", "c"])), &set(&[]));
        io.keep(Some(with_panes(&["a", "b"])), &set(&["c"]));
        io.keep(Some(with_panes(&["b"])), &set(&[]));
        let back = io.pre_signal().expect("a state");
        let uids: HashSet<String> = panes(&back).map(|p| p.uid.clone()).collect();
        assert_eq!(uids, set(&["b", "c"]));
    }

    /// A pane that died of a signal longer than [`LOSS_HOLD`] ago stays gone.
    #[test]
    fn a_pane_gone_before_the_hold_stays_gone() {
        let io = unwritten_io();
        let one = with_panes(&["a"]);
        io.keep(Some(with_panes(&["a", "b"])), &set(&[]));
        io.keep(Some(one.clone()), &set(&["b"]));
        age(&io, LOSS_HOLD);
        assert_eq!(io.pre_signal(), Some(one));
    }

    /// What changed during the hold stays: a window made after a pane died
    /// is written with the pane put back.
    #[test]
    fn a_change_during_the_hold_is_kept() {
        let io = unwritten_io();
        io.keep(Some(with_panes(&["a", "b"])), &set(&[]));
        io.keep(Some(with_panes(&["a"])), &set(&["b"]));
        let mut now = with_windows(vec![window(0, &["a"]), window(5, &["c"])]);
        now.sessions[0].windows[1].name = "new".to_string();
        io.keep(Some(now), &set(&[]));
        let back = io.pre_signal().expect("a state");
        let windows: Vec<(u32, &str)> = back.sessions[0]
            .windows
            .iter()
            .map(|w| (w.index, w.name.as_str()))
            .collect();
        assert_eq!(windows, [(0, "w0"), (1, "w1"), (5, "new")]);
    }

    /// A pane that died of a signal goes back into its window, beside the
    /// panes still there and without one closed on purpose since.
    #[test]
    fn a_pane_goes_back_into_its_window() {
        let io = unwritten_io();
        io.keep(
            Some(with_windows(vec![window(0, &["a", "b", "c"])])),
            &set(&[]),
        );
        io.keep(
            Some(with_windows(vec![window(0, &["b", "c"])])),
            &set(&["a"]),
        );
        io.keep(Some(with_windows(vec![window(0, &["c"])])), &set(&[]));
        let back = io.pre_signal().expect("a state");
        assert_eq!(
            back.sessions,
            with_windows(vec![window(0, &["a", "c"])]).sessions
        );
    }
}
