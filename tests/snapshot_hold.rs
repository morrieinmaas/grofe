//! The loss hold (#57): a pane that dies of a termination signal can
//! reach the server before the signal does. The server keeps such panes
//! for `LOSS_HOLD` (500 ms) and writes them back if it is signalled in
//! that time; nothing closed on purpose comes back, and every other change
//! stays current.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::time::{Duration, Instant};

use common::{Signal, TestServer};
use grofe::server::snapshot::Snapshot;
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// The snapshot on disk, if there is one and it parses.
fn snapshot(srv: &TestServer) -> Option<Snapshot> {
    let text = fs::read_to_string(srv.snapshot_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn wait_snapshot(srv: &TestServer, what: &str, mut cond: impl FnMut(&Snapshot) -> bool) {
    srv.wait_for(
        || snapshot(srv).as_ref().is_some_and(&mut cond),
        Duration::from_secs(4),
    )
    .unwrap_or_else(|_| panic!("{what}: the snapshot never matched"));
}

fn display(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .expect("display-message")
        .trim_end()
        .to_string()
}

fn uids(srv: &TestServer) -> BTreeSet<String> {
    srv.cmd(&["list-panes", "-a", "-F", "#{pane_uid}"])
        .expect("list-panes")
        .lines()
        .map(str::to_string)
        .collect()
}

fn snap_uids(snap: &Snapshot) -> BTreeSet<String> {
    snap.sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
        .map(|p| p.uid.clone())
        .collect()
}

/// A pane as `list-panes -a` shows it.
struct Pane {
    uid: String,
    pid: i32,
    /// The `%N` target.
    id: String,
}

fn pane_list(srv: &TestServer) -> Vec<Pane> {
    srv.cmd(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_uid} #{pane_pid} #{pane_id}",
    ])
    .expect("list-panes")
    .lines()
    .map(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        Pane {
            uid: f[0].to_string(),
            pid: f[1].parse().expect("pane pid"),
            id: f[2].to_string(),
        }
    })
    .collect()
}

/// Kills `pane` with SIGTERM and waits until the server has dropped it.
fn signal_death(srv: &TestServer, pane: &Pane) {
    kill(Pid::from_raw(pane.pid), Signal::SIGTERM).expect("signal the pane");
    srv.wait_for(|| !uids(srv).contains(&pane.uid), Duration::from_secs(5))
        .expect("the server drops the signalled pane");
}

/// SIGTERM to the server alone; waits until it has gone.
fn terminate(srv: &TestServer, server: i32) {
    kill(Pid::from_raw(server), Signal::SIGTERM).expect("signal the server");
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(10))
        .expect("the server goes after SIGTERM");
    // Negative wait: nothing may land after the server exits.
    common::quiet_period(Duration::from_millis(500));
}

/// Whether `pid` is gone and reaped (no zombie left).
fn reaped(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_err()
}

/// A server with `sessions` sessions of `per` `sleep` panes each, its
/// snapshot written. Returns the live uids, the pane pids and the
/// server's pid.
fn sleeping_panes(
    srv: &TestServer,
    sessions: &[&str],
    per: usize,
) -> (BTreeSet<String>, Vec<i32>, i32) {
    srv.cmd(&["set-option", "-g", "remain-on-exit", "off"])
        .expect("remain-on-exit off");
    for name in sessions {
        // `sleep` dies of SIGTERM, where an interactive shell ignores it.
        srv.cmd(&["new-session", "-d", "-s", name, "sleep 600"])
            .expect("new-session");
        for _ in 1..per {
            srv.cmd(&["split-window", "-d", "-t", name, "sleep 600"])
                .expect("split-window");
        }
    }
    let live = uids(srv);
    assert_eq!(live.len(), sessions.len() * per);
    let pids = srv
        .cmd(&["list-panes", "-a", "-F", "#{pane_pid}"])
        .expect("list-panes")
        .lines()
        .map(|l| l.parse().expect("pane pid"))
        .collect();
    let server = display(srv, sessions[0], "#{pid}").parse().expect("pid");
    wait_snapshot(srv, "every pane", |s| snap_uids(s) == live);
    (live, pids, server)
}

/// The reboot race forced in order: every pane dies of `signal` and the
/// server reaps them all before its own signal arrives, 100 ms later.
/// Every pane must still be in the file.
fn signalled_after_its_panes_died_of_it(signal: Signal) {
    let srv = TestServer::start();
    let (live, pids, server) = sleeping_panes(&srv, &["main", "other"], 2);
    for &p in &pids {
        kill(Pid::from_raw(p), signal).expect("signal a pane");
    }
    srv.wait_for(|| pids.iter().all(|&p| reaped(p)), Duration::from_secs(5))
        .expect("the panes die and are reaped");
    std::thread::sleep(Duration::from_millis(100));
    // The server may already be going; the signal can then miss it.
    let _ = kill(Pid::from_raw(server), signal);
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("the server goes after {signal:?}"));
    // Negative wait: nothing may land after the server exits.
    common::quiet_period(Duration::from_millis(500));
    let snap = snapshot(&srv).expect("a snapshot");
    assert_eq!(snap_uids(&snap), live, "{signal:?}");
}

#[test]
fn sigterm_after_the_panes_died_of_it_leaves_every_pane() {
    signalled_after_its_panes_died_of_it(Signal::SIGTERM);
}

#[test]
fn sighup_after_the_panes_died_of_it_leaves_every_pane() {
    signalled_after_its_panes_died_of_it(Signal::SIGHUP);
}

/// A pane closed with kill-pane stays closed, even when the server is
/// signalled straight after (well inside the hold for signal deaths).
fn signalled_just_after_a_kill_pane(signal: Signal) {
    let srv = TestServer::start();
    let (live, _, server) = sleeping_panes(&srv, &["main"], 3);
    let gone = display(&srv, "main", "#{pane_uid}");
    assert!(live.contains(&gone), "{gone} in {live:?}");
    srv.cmd(&["kill-pane", "-t", "main"]).expect("kill-pane");
    std::thread::sleep(Duration::from_millis(100));
    kill(Pid::from_raw(server), signal).expect("signal the server");
    // SIGHUP writes the state but, as in tmux, does not stop the server.
    if signal == Signal::SIGTERM {
        srv.wait_for(|| !srv.is_running(), Duration::from_secs(10))
            .expect("the server goes after SIGTERM");
    }
    // Negative wait: the signal's write has landed by now.
    common::quiet_period(Duration::from_millis(500));
    let snap = snapshot(&srv).expect("a snapshot");
    let mut want = live;
    want.remove(&gone);
    assert_eq!(snap_uids(&snap), want, "{signal:?}");
}

#[test]
fn sigterm_just_after_a_kill_pane_leaves_it_closed() {
    signalled_just_after_a_kill_pane(Signal::SIGTERM);
}

#[test]
fn sighup_just_after_a_kill_pane_leaves_it_closed() {
    signalled_just_after_a_kill_pane(Signal::SIGHUP);
}

/// One session `main` of `n` `sleep` panes in one window, saved. Returns
/// its panes and the server's pid.
fn one_window(srv: &TestServer, n: usize) -> (Vec<Pane>, i32) {
    let (live, _, server) = sleeping_panes(srv, &["main"], n);
    let panes = pane_list(srv);
    assert_eq!(
        panes.iter().map(|p| p.uid.clone()).collect::<BTreeSet<_>>(),
        live
    );
    (panes, server)
}

/// A pane dies of a signal, then another is closed with kill-pane inside
/// the hold: the signal writes the one that died of it, not the closed one.
#[test]
fn a_kill_pane_inside_the_hold_stays_closed() {
    let srv = TestServer::start();
    let (panes, server) = one_window(&srv, 3);
    let (died, closed, kept) = (&panes[0], &panes[1], &panes[2]);
    signal_death(&srv, died);
    srv.cmd(&["kill-pane", "-t", &closed.id])
        .expect("kill-pane");
    terminate(&srv, server);
    let snap = snapshot(&srv).expect("a snapshot");
    let want: BTreeSet<String> = [died.uid.clone(), kept.uid.clone()].into();
    assert_eq!(snap_uids(&snap), want);
}

/// kill-session of the last session just after a signal death is the
/// user's choice: the server goes at once and saves the empty state.
#[test]
fn killing_the_last_session_inside_the_hold_does_not_wait() {
    let srv = TestServer::start();
    let (panes, _) = one_window(&srv, 2);
    signal_death(&srv, &panes[0]);
    let start = Instant::now();
    srv.cmd(&["kill-session", "-t", "main"])
        .expect("kill-session");
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(5))
        .expect("the server goes with its last session");
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(400),
        "the exit waited {took:?}"
    );
    common::quiet_period(Duration::from_millis(700));
    let snap = snapshot(&srv).expect("a snapshot");
    assert!(snap.sessions.is_empty(), "{snap:#?}");
}

/// A window made inside the hold is current state: the signal writes it,
/// along with the pane that died of the signal.
#[test]
fn a_window_made_inside_the_hold_is_written() {
    let srv = TestServer::start();
    let (panes, server) = one_window(&srv, 2);
    signal_death(&srv, &panes[0]);
    let new = srv
        .cmd(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_uid}",
            "-t",
            "main",
            "sleep 600",
        ])
        .expect("new-window")
        .trim_end()
        .to_string();
    terminate(&srv, server);
    let snap = snapshot(&srv).expect("a snapshot");
    let want: BTreeSet<String> = [panes[0].uid.clone(), panes[1].uid.clone(), new].into();
    assert_eq!(snap_uids(&snap), want);
    assert_eq!(snap.sessions[0].windows.len(), 2);
}
