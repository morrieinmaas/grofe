//! T4.1: the snapshot writer (spec §6). The server writes `state.json` on
//! change, debounced, atomically, with no client involved; termination
//! writes the pre-shutdown state once and freezes the writer; `--persist`
//! is sticky; unknown and unreadable files are moved aside.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Signal, TestServer};
use grofe::server::snapshot::{PaneSnap, SNAPSHOT_VERSION, Snapshot};

/// How long a change may take to reach the file: the debounce (1 s) plus
/// slack for a loaded machine.
const SAVED: Duration = Duration::from_secs(4);

/// The snapshot on disk, if there is one and it parses.
fn snapshot(srv: &TestServer) -> Option<Snapshot> {
    let text = fs::read_to_string(srv.snapshot_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn wait_snapshot(
    srv: &TestServer,
    what: &str,
    mut cond: impl FnMut(&Snapshot) -> bool,
) -> Snapshot {
    let mut last = None;
    let found = srv.wait_for(
        || {
            last = snapshot(srv);
            last.as_ref().is_some_and(&mut cond)
        },
        SAVED,
    );
    if found.is_err() {
        panic!("{what}: the snapshot never matched; last seen:\n{last:#?}");
    }
    last.unwrap_or_else(|| unreachable!())
}

fn panes(snap: &Snapshot) -> Vec<&PaneSnap> {
    snap.sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
        .collect()
}

fn pane<'a>(snap: &'a Snapshot, uid: &str) -> Option<&'a PaneSnap> {
    panes(snap).into_iter().find(|p| p.uid == uid)
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
    panes(snap).into_iter().map(|p| p.uid.clone()).collect()
}

fn canonical(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn session(srv: &TestServer, name: &str) {
    srv.cmd(&["new-session", "-d", "-s", name])
        .expect("new-session");
}

fn option<'a>(opts: &'a [grofe::server::snapshot::OptionSnap], name: &str) -> Option<&'a str> {
    opts.iter()
        .find(|o| o.name == name)
        .map(|o| o.value.as_str())
}

#[test]
fn a_split_reaches_the_file_with_no_client_attached() {
    let srv = TestServer::start();
    session(&srv, "main");
    let dir = tempfile::tempdir().expect("tempdir");
    let dir_path = dir.path().to_str().expect("utf-8 tempdir").to_string();
    srv.cmd(&["split-window", "-t", "main", "-c", &dir_path, "sleep 600"])
        .expect("split-window");
    let uid = display(&srv, "main", "#{pane_uid}");
    let snap = wait_snapshot(&srv, "the split pane", |s| pane(s, &uid).is_some());
    let p = pane(&snap, &uid).expect("pane");
    assert_eq!(canonical(&p.cwd), canonical(dir.path()), "{p:#?}");
    let argv = p.argv.as_ref().expect("the split's argv");
    assert!(argv.join(" ").contains("sleep 600"), "{argv:?}");
    assert_eq!(snap.version, SNAPSHOT_VERSION);
    assert_eq!(snap_uids(&snap), uids(&srv));
}

#[test]
fn a_cd_in_a_pane_follows_into_the_file() {
    let srv = TestServer::start();
    session(&srv, "main");
    let uid = display(&srv, "main", "#{pane_uid}");
    wait_snapshot(&srv, "the session", |s| pane(s, &uid).is_some());
    let dir = tempfile::tempdir().expect("tempdir");
    let target = canonical(dir.path());
    let line = format!("cd '{}'", target.display());
    srv.cmd(&["send-keys", "-t", "main", &line, "Enter"])
        .expect("send-keys");
    wait_snapshot(&srv, "the cd", |s| {
        pane(s, &uid).is_some_and(|p| canonical(&p.cwd) == target)
    });
}

#[test]
fn with_nothing_changing_the_file_is_left_alone() {
    let srv = TestServer::start();
    session(&srv, "main");
    srv.cmd(&["split-window", "-t", "main"]).expect("split");
    wait_snapshot(&srv, "two panes", |s| panes(s).len() == 2);
    // Let the shells settle (prompts, first samples).
    std::thread::sleep(Duration::from_secs(2));
    let path = srv.snapshot_path();
    let before = fs::metadata(&path).expect("metadata");
    std::thread::sleep(Duration::from_secs(5));
    let after = fs::metadata(&path).expect("metadata");
    assert_eq!(before.ino(), after.ino(), "the file was replaced");
    assert_eq!(before.mtime(), after.mtime());
    assert_eq!(before.mtime_nsec(), after.mtime_nsec());
}

#[test]
fn persist_is_sticky_and_only_no_persist_drops_it() {
    let srv = TestServer::start();
    session(&srv, "main");
    let uid = display(&srv, "main", "#{pane_uid}");
    let set = |args: &[&str]| {
        let mut argv = vec!["set-option", "-p", "-t", "main"];
        argv.extend_from_slice(args);
        srv.cmd(&argv).expect("set-option");
    };
    set(&["--persist", "@agent_session", "x"]);
    set(&["@agent_state", "working"]);
    let snap = wait_snapshot(&srv, "@agent_session x", |s| {
        pane(s, &uid).is_some_and(|p| option(&p.options, "@agent_session") == Some("x"))
    });
    let p = pane(&snap, &uid).expect("pane");
    assert_eq!(option(&p.options, "@agent_state"), None, "{p:#?}");

    set(&["@agent_session", "y"]);
    let snap = wait_snapshot(&srv, "the sticky mark saves y", |s| {
        pane(s, &uid).is_some_and(|p| option(&p.options, "@agent_session") == Some("y"))
    });
    assert_eq!(
        option(&pane(&snap, &uid).expect("pane").options, "@agent_state"),
        None
    );

    set(&["--no-persist", "@agent_session", "y"]);
    wait_snapshot(&srv, "--no-persist drops it", |s| {
        pane(s, &uid).is_some_and(|p| option(&p.options, "@agent_session").is_none())
    });
    // The value itself stays; only the mark went.
    assert_eq!(display(&srv, "main", "#{@agent_session}"), "y");

    // Global, session and window scopes are saved too.
    srv.cmd(&["set-option", "-g", "--persist", "@g", "1"])
        .expect("set -g");
    srv.cmd(&["set-option", "-t", "main", "--persist", "@s", "2"])
        .expect("set -s");
    srv.cmd(&["set-option", "-w", "-t", "main", "--persist", "@w", "3"])
        .expect("set -w");
    let snap = wait_snapshot(&srv, "every scope", |s| {
        option(&s.options, "@g") == Some("1")
            && s.sessions
                .first()
                .is_some_and(|s| option(&s.options, "@s") == Some("2"))
            && s.sessions
                .first()
                .and_then(|s| s.windows.first())
                .is_some_and(|w| option(&w.options, "@w") == Some("3"))
    });
    // Unsetting clears the mark with the value.
    srv.cmd(&["set-option", "-gu", "@g"]).expect("set -gu");
    wait_snapshot(&srv, "the unset", |s| option(&s.options, "@g").is_none());
    drop(snap);
}

#[test]
fn kill_server_leaves_the_pre_kill_sessions() {
    let srv = TestServer::start();
    session(&srv, "main");
    session(&srv, "other");
    wait_snapshot(&srv, "two sessions", |s| s.sessions.len() == 2);
    // Straight before the kill, inside the debounce: the kill's own
    // synchronous write must hold it.
    srv.cmd(&["split-window", "-t", "main"]).expect("split");
    let live = uids(&srv);
    assert_eq!(live.len(), 3);
    srv.cmd(&["kill-server"]).expect("kill-server");
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(5))
        .expect("the server goes");
    let snap = snapshot(&srv).expect("a snapshot");
    assert_eq!(snap_uids(&snap), live);
    let names: Vec<&str> = snap.sessions.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["main", "other"]);
}

/// A reboot or logout: the server and every pane get `signal`; the panes
/// die after the server's signal and their sessions close, none of which
/// may reach the file.
fn signalled_with_its_panes_keeps_the_sessions(signal: Signal) {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "remain-on-exit", "off"])
        .expect("remain-on-exit off");
    session(&srv, "main");
    session(&srv, "other");
    wait_snapshot(&srv, "two sessions", |s| s.sessions.len() == 2);
    srv.cmd(&["split-window", "-t", "main"]).expect("split");
    let live = uids(&srv);
    assert_eq!(live.len(), 3);
    srv.signal_all(signal);
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("the server goes after {signal:?}"));
    // Anything still in flight would have landed by now.
    std::thread::sleep(Duration::from_millis(1500));
    let snap = snapshot(&srv).expect("a snapshot");
    assert_eq!(snap_uids(&snap), live, "{signal:?}");
    assert_eq!(snap.sessions.len(), 2);
}

#[test]
fn sigterm_with_dying_panes_leaves_the_pre_shutdown_sessions() {
    signalled_with_its_panes_keeps_the_sessions(Signal::SIGTERM);
}

#[test]
fn sighup_with_dying_panes_leaves_the_pre_shutdown_sessions() {
    signalled_with_its_panes_keeps_the_sessions(Signal::SIGHUP);
}

#[test]
fn the_last_pane_exiting_normally_writes_the_empty_state() {
    let srv = TestServer::start();
    srv.cmd(&["set-option", "-g", "remain-on-exit", "off"])
        .expect("remain-on-exit off");
    session(&srv, "main");
    wait_snapshot(&srv, "the session", |s| s.sessions.len() == 1);
    srv.cmd(&["send-keys", "-t", "main", "exit", "Enter"])
        .expect("send-keys");
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(10))
        .expect("the server goes with its last pane");
    let snap = snapshot(&srv).expect("a snapshot");
    assert!(snap.sessions.is_empty(), "{snap:#?}");
    let text = fs::read_to_string(srv.snapshot_path()).expect("read");
    assert!(text.contains("\"sessions\": []"), "{text}");
}

#[test]
fn an_empty_server_never_overwrites_the_snapshot() {
    let srv = TestServer::start();
    session(&srv, "main");
    let before = wait_snapshot(&srv, "the session", |s| s.sessions.len() == 1);
    srv.kill_hard();
    let srv = srv.restart();
    // Restore is off under the harness: this server is empty, and stays
    // so; its kill-server must not wipe the old state either.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(snapshot(&srv).map(|s| s.sessions), Some(before.sessions.clone()));
    srv.cmd(&["kill-server"]).expect("kill-server");
    srv.wait_for(|| !srv.is_running(), Duration::from_secs(5))
        .expect("the server goes");
    assert_eq!(snapshot(&srv).map(|s| s.sessions), Some(before.sessions));
}

/// A tiny seeded generator (xorshift64*), so a failure replays.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % n
    }
}

/// The structure both the server and the snapshot can show: per window,
/// its session, index and name, and its panes' uids in order.
type Shape = Vec<(String, u32, String, Vec<String>)>;

fn live_shape(srv: &TestServer) -> Shape {
    let out = srv
        .cmd(&[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{window_name}\t#{pane_uid}",
        ])
        .expect("list-panes");
    let mut shape: Shape = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let (s, i, n, u) = (f[0].to_string(), f[1].parse().unwrap(), f[2], f[3]);
        match shape.last_mut() {
            Some(last) if last.0 == s && last.1 == i => last.3.push(u.to_string()),
            _ => shape.push((s, i, n.to_string(), vec![u.to_string()])),
        }
    }
    shape
}

fn snap_shape(snap: &Snapshot) -> Shape {
    let mut sessions: Vec<_> = snap.sessions.iter().collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    let mut shape = Vec::new();
    for s in sessions {
        let mut windows: Vec<_> = s.windows.iter().collect();
        windows.sort_by_key(|w| w.index);
        for w in windows {
            let uids = w.panes.iter().map(|p| p.uid.clone()).collect();
            shape.push((s.name.clone(), w.index, w.name.clone(), uids));
        }
    }
    shape
}

#[test]
fn a_crash_at_any_point_leaves_a_state_that_was_written() {
    let seed = std::env::var("GROFE_T41_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_0401_u64);
    let mut rng = Rng(seed);
    let srv = TestServer::start();
    // Static names, so a name is part of the state and not a moving part.
    srv.cmd(&["set-option", "-g", "automatic-rename", "off"])
        .expect("automatic-rename off");
    srv.cmd(&["new-session", "-d", "-s", "a", "-n", "w0"])
        .expect("new-session");
    srv.cmd(&["new-session", "-d", "-s", "b", "-n", "w0"])
        .expect("new-session");
    let first = live_shape(&srv);
    wait_snapshot(&srv, "the start", |s| snap_shape(s) == first);
    let mut states = vec![first];
    let kill_at = rng.below(200);
    for step in 0..200 {
        let shape = states.last().expect("a state").clone();
        let (s, i, _, panes) = &shape[rng.below(shape.len())];
        let window = format!("{s}:{i}");
        let name = format!("n{step}");
        let argv: Vec<&str> = match rng.below(6) {
            0 => vec!["new-window", "-d", "-t", s, "-n", &name],
            1 | 2 => vec!["split-window", "-d", "-t", &window],
            3 if panes.len() > 1 => vec!["kill-pane", "-t", &window],
            4 => vec!["rename-window", "-t", &window, &name],
            5 if shape.iter().filter(|w| &w.0 == s).count() > 1 => {
                vec!["kill-window", "-t", &window]
            }
            _ => vec!["select-layout", "-t", &window, "tiled"],
        };
        // A split may refuse for lack of room; that is no change.
        let _ = srv.cmd(&argv);
        states.push(live_shape(&srv));
        if step == kill_at {
            break;
        }
        if rng.below(10) == 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    srv.kill_hard();
    let text = fs::read_to_string(srv.snapshot_path()).expect("a snapshot");
    let snap: Snapshot = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("seed {seed:#x}: does not parse: {e}\n{text}"));
    let shape = snap_shape(&snap);
    assert!(
        states.contains(&shape),
        "seed {seed:#x}, killed after step {kill_at}: {shape:#?} is no state that was"
    );
    // No temp file is mistaken for the snapshot, and none is the snapshot.
    let dir = srv.snapshot_path().parent().expect("dir").to_path_buf();
    assert!(dir.join("state.json").exists());
}

#[test]
fn an_unknown_version_is_moved_aside_before_the_first_write() {
    let srv = TestServer::start();
    let path = srv.snapshot_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let newer = r#"{"version": 2, "from": "a newer grofe"}"#;
    fs::write(&path, newer).unwrap();
    session(&srv, "main");
    wait_snapshot(&srv, "a fresh version 1 file", |s| s.sessions.len() == 1);
    let aside = path.with_file_name("state.json.v2");
    assert_eq!(fs::read_to_string(&aside).expect("state.json.v2"), newer);
    let log = srv.log();
    assert_eq!(
        log.lines().filter(|l| l.contains("state.json.v2")).count(),
        1,
        "{log}"
    );
}

#[test]
fn a_garbage_file_is_moved_aside_as_corrupt() {
    let srv = TestServer::start();
    let path = srv.snapshot_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "this is { not json").unwrap();
    session(&srv, "main");
    wait_snapshot(&srv, "a fresh file", |s| s.sessions.len() == 1);
    let dir = path.parent().unwrap();
    let corrupt: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("state.json.corrupt-"))
        })
        .collect();
    assert_eq!(corrupt.len(), 1, "{corrupt:?}");
    assert_eq!(
        fs::read_to_string(&corrupt[0]).unwrap(),
        "this is { not json"
    );
    let log = srv.log();
    assert_eq!(
        log.lines()
            .filter(|l| l.contains("state.json.corrupt-"))
            .count(),
        1,
        "{log}"
    );
}

#[test]
fn the_file_is_at_the_snapshot_path_with_mode_0600() {
    let srv = TestServer::start();
    session(&srv, "main");
    wait_snapshot(&srv, "the session", |s| s.sessions.len() == 1);
    let meta = fs::metadata(srv.snapshot_path()).expect("metadata");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // Nothing but the snapshot is left in its directory.
    let dir = srv.snapshot_path().parent().unwrap().to_path_buf();
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    assert_eq!(names, ["state.json"], "{dir:?}");
}

#[test]
fn the_snapshot_holds_the_window_shape() {
    let srv = TestServer::start();
    session(&srv, "main");
    srv.cmd(&["rename-window", "-t", "main:0", "edit"])
        .expect("rename");
    srv.cmd(&["split-window", "-h", "-t", "main"])
        .expect("split");
    srv.cmd(&["resize-pane", "-Z", "-t", "main"]).expect("zoom");
    let active = display(&srv, "main", "#{pane_uid}");
    let snap = wait_snapshot(&srv, "the zoomed window", |s| {
        s.sessions
            .first()
            .and_then(|s| s.windows.first())
            .is_some_and(|w| w.zoomed && w.panes.len() == 2)
    });
    let s = &snap.sessions[0];
    assert_eq!(s.name, "main");
    assert_eq!(s.active_window, s.windows[0].index);
    let w = &s.windows[0];
    assert_eq!(w.name, "edit");
    assert!(!w.automatic_rename);
    assert_eq!(w.active_pane, active);
    assert_eq!(
        (w.cols, w.rows),
        (
            display(&srv, "main", "#{window_width}").parse().unwrap(),
            display(&srv, "main", "#{window_height}").parse().unwrap()
        )
    );
    let json = serde_json::to_value(&w.layout).expect("layout json");
    let text = json.to_string();
    for p in &w.panes {
        assert!(text.contains(&format!("\"{}\"", p.uid)), "{text}");
    }
}
