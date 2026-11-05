//! T4.3: restore core (spec §6). A server restores the snapshot into an
//! empty server only, on start when `restore-on-start` says so (the
//! harness forces it off) or always with `start-server --restore`:
//! sessions, windows by index, names, sizes, layouts keyed by pane `uid`,
//! active window and pane, zoom and `--persist` options, every pane a
//! shell in its `cwd` (resumes are T4.4).
//!
//! Most tests write the snapshot themselves. The round trip and its
//! mutation proof need T4.1's writer (`kill-server` saves the state).

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestServer, wait_until};
use grofe::server::layout::{GeometryCell, LayoutGeometry};
use grofe::server::snapshot::{
    OptionSnap, PaneSnap, SNAPSHOT_VERSION, SessionSnap, Snapshot, WindowSnap,
};
use grofe::types::Dir;
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

const PANES_FMT: &str = "#{session_name}:#{window_index}.#{pane_index} #{pane_uid} \
                         #{pane_current_path} #{pane_width}x#{pane_height}";
const WINDOWS_FMT: &str = "#{window_name} #{window_zoomed_flag} #{window_active}";

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn lines(out: String) -> Vec<String> {
    out.lines().map(str::to_string).collect()
}

fn list_panes(srv: &TestServer, fmt: &str) -> Vec<String> {
    lines(ok(srv, &["list-panes", "-a", "-F", fmt]))
}

fn list_windows(srv: &TestServer) -> Vec<String> {
    lines(ok(srv, &["list-windows", "-a", "-F", WINDOWS_FMT]))
}

fn opt(name: &str, value: &str) -> OptionSnap {
    OptionSnap {
        name: name.to_string(),
        value: value.to_string(),
    }
}

fn pane(uid: &str, cwd: &Path) -> PaneSnap {
    PaneSnap {
        uid: uid.to_string(),
        cwd: cwd.to_path_buf(),
        argv: None,
        resume: None,
        options: Vec::new(),
    }
}

fn cell(x: u16, y: u16, cols: u16, rows: u16, uid: &str) -> LayoutGeometry<String> {
    LayoutGeometry {
        x,
        y,
        cols,
        rows,
        cell: GeometryCell::Pane(uid.to_string()),
    }
}

fn split(dir: Dir, children: Vec<LayoutGeometry<String>>) -> LayoutGeometry<String> {
    LayoutGeometry {
        x: 0,
        y: 0,
        cols: 80,
        rows: 24,
        cell: GeometryCell::Split { dir, children },
    }
}

fn window(
    index: u32,
    name: &str,
    layout: LayoutGeometry<String>,
    panes: Vec<PaneSnap>,
) -> WindowSnap {
    WindowSnap {
        index,
        name: name.to_string(),
        automatic_rename: false,
        cols: 80,
        rows: 24,
        layout,
        active_pane: panes[0].uid.clone(),
        zoomed: false,
        options: Vec::new(),
        panes,
    }
}

/// Two sessions: `main` with one pane, and `work` with two windows, one of
/// two panes side by side (the second active, with a persisted pane
/// option) and one of two stacked panes, zoomed.
fn sample(home: &Path) -> Snapshot {
    let (a, b) = (home.join("a"), home.join("b"));
    for dir in [&a, &b] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let mut pane_b = pane("01UIDB", &b);
    pane_b.options.push(opt("@agent_session", "sess-b"));
    let mut editor = window(
        1,
        "editor",
        split(
            Dir::Horizontal,
            vec![cell(0, 0, 40, 24, "01UIDA"), cell(41, 0, 39, 24, "01UIDB")],
        ),
        vec![pane("01UIDA", &a), pane_b],
    );
    editor.active_pane = "01UIDB".to_string();
    editor.options.push(opt("@w_opt", "w"));
    let mut logs = window(
        2,
        "logs",
        split(
            Dir::Vertical,
            vec![cell(0, 0, 80, 12, "01UIDC"), cell(0, 13, 80, 11, "01UIDD")],
        ),
        vec![pane("01UIDC", home), pane("01UIDD", home)],
    );
    logs.zoomed = true;
    let main = window(
        0,
        "top",
        cell(0, 0, 80, 24, "01UIDE"),
        vec![pane("01UIDE", home)],
    );
    Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: "2026-11-05T10:00:00Z".to_string(),
        grofe_version: "0.0.0".to_string(),
        options: vec![opt("@theme", "dark")],
        sessions: vec![
            SessionSnap {
                name: "work".to_string(),
                cwd: home.to_path_buf(),
                active_window: 2,
                options: vec![opt("@s_opt", "s")],
                windows: vec![editor, logs],
            },
            SessionSnap {
                name: "main".to_string(),
                cwd: home.to_path_buf(),
                active_window: 0,
                options: Vec::new(),
                windows: vec![main],
            },
        ],
    }
}

/// What [`sample`] restores to, as `PANES_FMT` lines.
fn sample_panes(home: &Path) -> Vec<String> {
    let h = home.display();
    vec![
        format!("main:0.1 01UIDE {h} 80x24"),
        format!("work:1.1 01UIDA {h}/a 40x24"),
        format!("work:1.2 01UIDB {h}/b 39x24"),
        format!("work:2.1 01UIDC {h} 80x24"),
        format!("work:2.2 01UIDD {h} 80x11"),
    ]
}

fn write_snapshot(srv: &TestServer, snap: &Snapshot) {
    let path = srv.snapshot_path();
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir state");
    let text = serde_json::to_string_pretty(snap).expect("serialize");
    fs::write(&path, text).expect("write snapshot");
}

/// Stops the server and waits until its socket stops answering.
fn kill(srv: &TestServer) {
    ok(srv, &["kill-server"]);
    wait_until(|| !srv.is_running(), WAIT).expect("the server goes");
}

fn write_config(srv: &TestServer, text: &str) -> PathBuf {
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("mkdir config");
    let path = dir.join("config.toml");
    fs::write(&path, text).expect("write config");
    path
}

fn assert_sample_restored(srv: &TestServer) {
    assert_eq!(list_panes(srv, PANES_FMT), sample_panes(&srv.home));
    assert_eq!(
        list_windows(srv),
        ["top 0 1", "editor 0 0", "logs 1 1"].map(String::from)
    );
}

/// `start-server --restore` restores whatever the config says: here under
/// the harness, which forces restore-on-start off.
#[test]
fn start_server_restore_brings_back_the_snapshot() {
    let srv = TestServer::start();
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    ok(&srv, &["start-server", "--restore"]);
    assert_sample_restored(&srv);
    // The active pane of each window, and only restored panes are marked.
    assert_eq!(
        list_panes(&srv, "#{pane_uid} #{pane_active} #{pane_restored}"),
        [
            "01UIDE 1 1",
            "01UIDA 0 1",
            "01UIDB 1 1",
            "01UIDC 1 1",
            "01UIDD 0 1"
        ]
        .map(String::from)
    );
    // `--persist` options at every scope.
    assert_eq!(ok(&srv, &["show-options", "-gv", "@theme"]), "dark\n");
    assert_eq!(
        ok(&srv, &["show-options", "-t", "work", "-v", "@s_opt"]),
        "s\n"
    );
    assert_eq!(
        ok(
            &srv,
            &["show-options", "-w", "-t", "work:1", "-v", "@w_opt"]
        ),
        "w\n"
    );
    assert_eq!(
        ok(
            &srv,
            &[
                "show-options",
                "-p",
                "-t",
                "work:1.2",
                "-v",
                "@agent_session"
            ]
        ),
        "sess-b\n"
    );
    // Renamed windows keep their names: automatic-rename stays off.
    assert_eq!(
        ok(
            &srv,
            &[
                "show-options",
                "-w",
                "-t",
                "work:1",
                "-v",
                "automatic-rename"
            ]
        ),
        "off\n"
    );
    // A pane made after the restore is not marked restored.
    ok(&srv, &["split-window", "-t", "main:0"]);
    assert_eq!(
        list_panes(&srv, "#{session_name} #{pane_restored}")
            .iter()
            .filter(|l| l.starts_with("main "))
            .cloned()
            .collect::<Vec<_>>(),
        ["main 1", "main 0"].map(String::from)
    );
    // Restoring once more into the now busy server refuses.
    let again = srv.run(&["start-server", "--restore"]);
    assert!(!again.status.success());
    assert!(
        again
            .stderr
            .contains("server not empty: restore needs an empty server"),
        "{}",
        again.stderr
    );
}

/// Each restored pane runs a shell in its `cwd`.
#[test]
fn restored_panes_run_a_shell_in_their_cwd() {
    let srv = TestServer::start();
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    ok(&srv, &["start-server", "--restore"]);
    ok(&srv, &["send-keys", "-t", "work:1.1", "pwd", "Enter"]);
    let want = srv.home.join("a").display().to_string();
    srv.wait_for(|| srv.capture("work:1.1").lines().any(|l| l == want), WAIT)
        .unwrap_or_else(|_| panic!("pwd in the pane: {}", srv.capture("work:1.1")));
}

/// Into a server that has a session, `--restore` errors and changes
/// nothing.
#[test]
fn restore_into_a_server_with_a_session_errors() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "keep"]);
    let before = ok(&srv, &["list-sessions", "-F", "#{session_name}"]);
    write_snapshot(&srv, &sample(&srv.home));
    let out = srv.run(&["start-server", "--restore"]);
    assert_eq!(out.status.code(), Some(1), "{}", out.stderr);
    assert!(
        out.stderr
            .contains("server not empty: restore needs an empty server"),
        "{}",
        out.stderr
    );
    assert_eq!(
        ok(&srv, &["list-sessions", "-F", "#{session_name}"]),
        before
    );
}

/// `start-server --restore` with no snapshot says so and still starts an
/// empty server.
#[test]
fn restore_without_a_snapshot_reports_it() {
    let srv = TestServer::start();
    kill(&srv);
    let _ = fs::remove_file(srv.snapshot_path());
    let out = srv.run(&["start-server", "--restore"]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stderr.contains("no snapshot"), "{}", out.stderr);
    assert!(srv.is_running());
    assert_eq!(ok(&srv, &["list-sessions", "-F", "#{session_name}"]), "");
}

/// With restore-on-start on (the config default), a starting server
/// restores before it answers anything.
#[test]
fn restore_on_start_restores_before_the_first_command() {
    let srv = TestServer::builder().restore_on_start(true).start();
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    let srv = srv.restart();
    assert_sample_restored(&srv);
}

/// The CLI that starts the server runs its command against the restored
/// sessions: `new-session -A -s main` attaches the restored `main`.
#[test]
fn new_session_a_attaches_the_restored_session() {
    let srv = TestServer::builder().restore_on_start(true).start();
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    let mut cmd = srv.command();
    cmd.args(["new-session", "-A", "-s", "main"])
        .env("TERM", "xterm-256color");
    let _client = Harness::spawn(cmd, 80, 24);
    wait_until(
        || {
            srv.cmd(&["list-sessions", "-F", "#{session_name} #{session_attached}"])
                .is_ok_and(|out| out.lines().any(|l| l == "main 1"))
        },
        WAIT,
    )
    .expect("main attached");
    assert_eq!(
        ok(&srv, &["list-sessions", "-F", "#{session_name}"]),
        "main\nwork\n"
    );
    assert_eq!(
        ok(
            &srv,
            &["display-message", "-p", "-t", "main", "#{pane_uid}"]
        ),
        "01UIDE\n"
    );
}

/// `restore-on-start = false` in the config: a starting server restores
/// nothing, but `start-server --restore` still does.
#[test]
fn restore_on_start_off_in_config_is_read() {
    let srv = TestServer::builder().restore_on_start(true).start();
    write_config(&srv, "restore-on-start = false\n");
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    let srv = srv.restart();
    assert_eq!(ok(&srv, &["list-sessions", "-F", "#{session_name}"]), "");
    kill(&srv);
    ok(&srv, &["start-server", "--restore"]);
    assert_sample_restored(&srv);
}

/// Under the default harness (`GROFE_RESTORE_ON_START=0`) a present
/// snapshot restores nothing.
#[test]
fn the_harness_forces_restore_off() {
    let srv = TestServer::start();
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    let srv = srv.restart();
    assert_eq!(ok(&srv, &["list-sessions", "-F", "#{session_name}"]), "");
}

/// A pane whose `cwd` is gone gets the home directory, with one log line.
#[test]
fn a_missing_cwd_gives_the_home_directory() {
    let srv = TestServer::start();
    kill(&srv);
    let gone = srv.home.join("gone-away");
    let snap = Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: String::new(),
        grofe_version: String::new(),
        options: Vec::new(),
        sessions: vec![SessionSnap {
            name: "main".to_string(),
            cwd: gone.clone(),
            active_window: 0,
            options: Vec::new(),
            windows: vec![window(
                0,
                "w",
                cell(0, 0, 80, 24, "01GONE"),
                vec![pane("01GONE", &gone)],
            )],
        }],
    };
    write_snapshot(&srv, &snap);
    ok(&srv, &["start-server", "--restore"]);
    assert_eq!(
        ok(
            &srv,
            &[
                "display-message",
                "-p",
                "-t",
                "main",
                "#{pane_current_path}"
            ]
        ),
        format!("{}\n", srv.home.display())
    );
    let gone = gone.display().to_string();
    let log = srv.log();
    let mentions: Vec<_> = log.lines().filter(|l| l.contains(&gone)).collect();
    assert_eq!(mentions.len(), 1, "{log}");
}

/// A restored session takes no environment from the snapshot: its
/// `update-environment` variables start unset and the first attach fills
/// them, for the panes made after it.
#[test]
fn the_first_attach_fills_the_update_environment() {
    let srv = TestServer::builder()
        .restore_on_start(true)
        .path_env("/usr/bin:/bin")
        .start();
    kill(&srv);
    write_snapshot(&srv, &sample(&srv.home));
    let srv = srv.restart();
    assert_eq!(list_panes(&srv, PANES_FMT), sample_panes(&srv.home));
    let unset = srv.run(&["show-environment", "-t", "main", "SSH_AUTH_SOCK"]);
    assert!(
        !unset.stdout.contains("SSH_AUTH_SOCK="),
        "{} {}",
        unset.stdout,
        unset.stderr
    );
    let _client = Harness::spawn_attach_with_env(
        srv.command(),
        "main",
        &[("SSH_AUTH_SOCK", "/tmp/x")],
        80,
        24,
    );
    wait_until(
        || {
            srv.cmd(&["show-environment", "-t", "main", "SSH_AUTH_SOCK"])
                .is_ok_and(|out| out == "SSH_AUTH_SOCK=/tmp/x\n")
        },
        WAIT,
    )
    .expect("the attach fills SSH_AUTH_SOCK");
    let id = ok(
        &srv,
        &[
            "new-window",
            "-d",
            "-t",
            "main",
            "-P",
            "-F",
            "#{pane_id}",
            "echo sock=$SSH_AUTH_SOCK; sleep 60",
        ],
    );
    let id = id.trim();
    srv.wait_for(|| srv.capture(id).contains("sock=/tmp/x"), WAIT)
        .unwrap_or_else(|_| panic!("new pane env: {}", srv.capture(id)));
}

/// A window whose layout does not hold its panes still comes back, every
/// pane in it.
#[test]
fn a_bad_layout_keeps_every_pane() {
    let srv = TestServer::start();
    kill(&srv);
    let mut snap = sample(&srv.home);
    // The editor window's layout names a pane it does not have.
    snap.sessions[0].windows[0].layout = split(
        Dir::Horizontal,
        vec![cell(0, 0, 40, 24, "01UIDA"), cell(41, 0, 39, 24, "01NOPE")],
    );
    write_snapshot(&srv, &snap);
    ok(&srv, &["start-server", "--restore"]);
    assert_eq!(
        lines(ok(
            &srv,
            &["list-panes", "-t", "work:1", "-F", "#{pane_uid}"]
        )),
        ["01UIDA", "01UIDB"].map(String::from)
    );
}

// ---- needs T4.1's writer: `kill-server` saves the state ----

/// Two sessions, three windows and six panes with splits, a zoomed
/// window, renamed windows, persisted and unpersisted options and `cd`s.
fn build(srv: &TestServer) {
    for dir in ["one", "two"] {
        fs::create_dir_all(srv.home.join(dir)).expect("mkdir");
    }
    ok(
        srv,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    ok(srv, &["split-window", "-h", "-t", "main:1"]);
    ok(srv, &["split-window", "-v", "-t", "main:1.2"]);
    ok(srv, &["new-window", "-t", "main", "-n", "logs"]);
    ok(srv, &["split-window", "-t", "main:logs"]);
    ok(srv, &["resize-pane", "-Z", "-t", "main:logs.1"]);
    ok(
        srv,
        &["new-session", "-d", "-s", "work", "-x", "80", "-y", "24"],
    );
    ok(srv, &["rename-window", "-t", "work:1", "build"]);
    ok(srv, &["select-pane", "-t", "main:1.1"]);
    ok(srv, &["select-window", "-t", "main:1"]);
    ok(
        srv,
        &[
            "set-option",
            "-p",
            "-t",
            "main:1.2",
            "--persist",
            "@agent_session",
            "abc",
        ],
    );
    ok(
        srv,
        &[
            "set-option",
            "-p",
            "-t",
            "main:1.2",
            "@agent_state",
            "working",
        ],
    );
    for (pane, dir) in [("main:1.2", "one"), ("work:1.1", "two")] {
        ok(
            srv,
            &["send-keys", "-t", pane, &format!("cd {dir}"), "Enter"],
        );
        let want = format!("{}/{dir}", srv.home.display());
        srv.wait_for(
            || {
                srv.cmd(&["display-message", "-p", "-t", pane, "#{pane_current_path}"])
                    .is_ok_and(|p| p.trim() == want)
            },
            WAIT,
        )
        .unwrap_or_else(|_| panic!("cd in {pane}"));
    }
    // T4.1 writes within a second of a change and samples cwds.
    std::thread::sleep(Duration::from_secs(3));
}

/// `kill-server` then `start-server --restore` brings back what was there,
/// line for line. Needs T4.1.
#[test]
fn kill_server_then_restore_round_trips() {
    let srv = TestServer::start();
    build(&srv);
    let panes = list_panes(&srv, PANES_FMT);
    let windows = list_windows(&srv);
    assert_eq!(panes.len(), 6, "{panes:?}");
    kill(&srv);
    ok(&srv, &["start-server", "--restore"]);
    assert_eq!(list_panes(&srv, PANES_FMT), panes);
    assert_eq!(list_windows(&srv), windows);
    assert_eq!(
        ok(
            &srv,
            &[
                "show-options",
                "-p",
                "-t",
                "main:1.2",
                "-v",
                "@agent_session"
            ]
        ),
        "abc\n"
    );
    assert_eq!(
        ok(
            &srv,
            &[
                "show-options",
                "-p",
                "-t",
                "main:1.2",
                "-qv",
                "@agent_state"
            ]
        ),
        ""
    );
}

/// Mutation proof: a snapshot with one pane's `uid` changed turns the
/// `uid` comparison red. Needs T4.1.
#[test]
fn a_changed_uid_turns_the_comparison_red() {
    let srv = TestServer::start();
    build(&srv);
    let panes = list_panes(&srv, PANES_FMT);
    kill(&srv);
    srv.edit_snapshot(|v| {
        v["sessions"][0]["windows"][0]["panes"][0]["uid"] = "01MUTATED".into();
    });
    ok(&srv, &["start-server", "--restore"]);
    let after = list_panes(&srv, PANES_FMT);
    assert_ne!(after, panes, "the mutated uid must show");
    assert!(after.iter().any(|l| l.contains("01MUTATED")), "{after:?}");
}
