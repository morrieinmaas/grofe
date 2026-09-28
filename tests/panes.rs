//! Pane commands (spec §4.5, §4.8): split-window, kill-pane, select-pane,
//! resize-pane, respawn-pane, send-keys and capture-pane, including
//! execution-time format expansion in `-c` and the M1 done-means scenario.

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::TestServer;

const WAIT: Duration = Duration::from_secs(5);

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap_or_else(|e| panic!("display-message {fmt}: {e}"))
        .trim_end()
        .to_string()
}

/// `list-panes -F` for `target`, one string per pane in layout order.
fn panes(srv: &TestServer, target: &str, fmt: &str) -> Vec<String> {
    srv.cmd(&["list-panes", "-t", target, "-F", fmt])
        .unwrap_or_else(|e| panic!("list-panes: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// Pane widths in layout order.
fn widths(srv: &TestServer, target: &str) -> Vec<u16> {
    panes(srv, target, "#{pane_width}")
        .iter()
        .map(|w| w.parse().unwrap())
        .collect()
}

fn heights(srv: &TestServer, target: &str) -> Vec<u16> {
    panes(srv, target, "#{pane_height}")
        .iter()
        .map(|h| h.parse().unwrap())
        .collect()
}

fn active(srv: &TestServer, target: &str) -> String {
    display(srv, target, "#{pane_index}")
}

fn wait_for_file(path: &Path) -> String {
    common::wait_until(
        || fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{} never written: {e}", path.display()));
    fs::read_to_string(path).unwrap_or_default()
}

/// Waits until the capture of `target` has a line exactly equal to `line`.
fn wait_for_line(srv: &TestServer, target: &str, line: &str) {
    srv.wait_for(|| srv.capture(target).lines().any(|l| l == line), WAIT)
        .unwrap_or_else(|e| panic!("{e}: no line {line:?} in {:?}", srv.capture(target)));
}

/// A session `name` of `cols`x`rows` whose first pane runs `sleep 30`.
fn session(srv: &TestServer, name: &str, cols: u16, rows: u16) {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        &cols.to_string(),
        "-y",
        &rows.to_string(),
        "sleep 30",
    ])
    .expect("new-session");
}

/// M1 done means: create a session, split it, run `echo hi` via
/// `send-keys`, and read `hi` back with `capture-pane`.
#[test]
fn m1_scenario_split_send_keys_capture() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "m", "/bin/sh"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "m", "/bin/sh"]).unwrap();
    assert_eq!(panes(&srv, "m", "#{pane_index}").len(), 2);
    assert_eq!(active(&srv, "m"), "2", "the new pane is active");

    srv.cmd(&["send-keys", "-t", "m:1.2", "echo hi", "Enter"])
        .unwrap();
    wait_for_line(&srv, "m:1.2", "hi");
    assert!(
        !srv.capture("m:1.1").lines().any(|l| l == "hi"),
        "the other pane got nothing"
    );
}

/// The sampler must never report the server's own (pre-exec) image: right
/// after `split-window 'sleep 30'`, `#{pane_current_argv}` should either be
/// empty (no sample yet) or already `sleep 30`, never the grofe server's
/// own argv.
#[test]
fn pane_current_argv_never_shows_the_server_before_exec() {
    let srv = TestServer::start();
    session(&srv, "a", 80, 24);
    srv.cmd(&["split-window", "-t", "a", "sleep", "30"])
        .unwrap();
    let target = "a:1.2";
    let deadline = std::time::Instant::now() + Duration::from_millis(2000);
    loop {
        let argv = display(&srv, target, "#{pane_current_argv}");
        assert!(
            !argv.contains("__server"),
            "pane_current_argv showed the server's own image: {argv:?}"
        );
        if argv == "sleep 30" || std::time::Instant::now() >= deadline {
            break;
        }
    }
}

#[test]
fn split_window_sizes_and_list_panes_format_match_the_layout() {
    let srv = TestServer::start();
    session(&srv, "l", 81, 25);
    srv.cmd(&["split-window", "-h", "-t", "l", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-v", "-t", "l:1.2", "sleep 30"])
        .unwrap();
    let ids = panes(&srv, "l", "#{pane_id}");
    assert_eq!(
        panes(
            &srv,
            "l",
            "#{pane_index} #{pane_id} #{pane_width}x#{pane_height} #{pane_active}"
        ),
        vec![
            format!("1 {} 40x25 0", ids[0]),
            format!("2 {} 40x12 0", ids[1]),
            format!("3 {} 40x12 1", ids[2]),
        ]
    );

    // -l takes cells, -p a percentage of the target's size.
    session(&srv, "c", 81, 25);
    srv.cmd(&["split-window", "-h", "-l", "20", "-t", "c", "sleep 30"])
        .unwrap();
    assert_eq!(widths(&srv, "c"), vec![60, 20]);

    // The pty got the layout's size.
    srv.cmd(&["split-window", "-h", "-t", "c:1.2", "stty size; sleep 30"])
        .unwrap();
    let size = display(&srv, "c:1.3", "#{pane_height} #{pane_width}");
    wait_for_line(&srv, "c:1.3", &size);
}

#[test]
fn split_window_percent_sizes_like_tmux() {
    let srv = TestServer::start();
    session(&srv, "c", 81, 25);
    srv.cmd(&["split-window", "-h", "-l", "20", "-t", "c", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-v", "-p", "20", "-t", "c:1.1", "sleep 30"])
        .unwrap();
    // tmux 3.7b: 20% of the 25-row pane gives a new 5-row pane below 19 rows.
    assert_eq!(heights(&srv, "c"), vec![19, 5, 25]);
}

#[test]
fn split_window_without_room_errors_like_tmux() {
    let srv = TestServer::start();
    session(&srv, "s", 4, 10);
    srv.cmd(&["split-window", "-h", "-t", "s", "sleep 30"])
        .unwrap();
    let ran = srv.home.join("ran");
    let script = format!("touch '{}'; sleep 30", ran.display());
    let err = srv
        .cmd(&["split-window", "-h", "-t", "s:1.1", &script])
        .unwrap_err();
    // tmux 3.7b's text, verbatim.
    assert_eq!(err.trim_end(), "size or position no space for a new pane");
    assert_eq!(panes(&srv, "s", "#{pane_id}").len(), 2);
    // The command never started: no pane was spawned for a failed split.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!ran.exists());
}

/// A missing `-c` directory falls back to `$HOME`, as in tmux 3.7b: the
/// split succeeds and the new pane starts there.
#[test]
fn split_window_with_a_missing_dash_c_starts_in_home() {
    let srv = TestServer::start();
    session(&srv, "f", 81, 20);
    let missing = srv.home.join("no-such-dir");
    srv.cmd(&[
        "split-window",
        "-h",
        "-t",
        "f",
        "-c",
        missing.to_str().unwrap(),
        "sleep 30",
    ])
    .unwrap();
    assert_eq!(panes(&srv, "f", "#{pane_id}").len(), 2);
    let home = fs::canonicalize(&srv.home).unwrap().display().to_string();
    srv.wait_for(
        || display(&srv, "f:1.2", "#{pane_current_path}") == home,
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: {}", display(&srv, "f:1.2", "#{pane_current_path}")));
}

/// A directly exec'd program that does not exist does not fail the
/// command (tmux 3.7b): the pane is born dead with status 1, and stays
/// dead under `remain-on-exit` (the harness default is on).
#[test]
fn a_program_that_fails_to_exec_gives_a_dead_pane() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "k", "grofe-no-such-cmd", "arg1"])
        .unwrap();
    srv.cmd(&["new-window", "-d", "-t", "k", "grofe-no-such-cmd", "x"])
        .unwrap();
    srv.cmd(&["split-window", "-t", "k:1", "grofe-no-such-cmd", "x"])
        .unwrap();
    for w in ["k:1", "k:2"] {
        srv.wait_for(
            || panes(&srv, w, "#{pane_dead}").iter().all(|d| d == "1"),
            WAIT,
        )
        .unwrap_or_else(|e| panic!("{w}: {e}: {:?}", panes(&srv, w, "#{pane_dead}")));
    }
    assert_eq!(panes(&srv, "k:1", "#{pane_id}").len(), 2);
}

/// With `remain-on-exit` off, a pane whose program failed to exec goes
/// away like any exited pane, taking its window with it.
#[test]
fn a_failed_exec_without_remain_on_exit_closes_the_window() {
    let srv = TestServer::start();
    session(&srv, "k", 80, 24);
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();
    srv.cmd(&["new-window", "-d", "-t", "k", "grofe-no-such-cmd", "x"])
        .unwrap();
    let windows = || srv.cmd(&["list-windows", "-t", "k", "-F", "#{window_index}"]);
    srv.wait_for(|| windows() == Ok("1\n".to_string()), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", windows()));
}

/// `-c` is expanded at execution time against the target pane: a pane
/// that `cd`s after the session started splits into its new cwd.
#[test]
fn split_window_dash_c_uses_the_target_pane_cwd() {
    let srv = TestServer::start();
    let dir = srv.home.join("proj");
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "c",
        "-c",
        dir.to_str().unwrap(),
        "sleep 30",
    ])
    .unwrap();
    // The target is not the active pane, so a fallback to the active pane
    // (or the session cwd) would give `proj`, not `proj/sub`.
    srv.cmd(&["split-window", "-t", "c", "cd sub && exec sleep 30"])
        .unwrap();
    srv.cmd(&["select-pane", "-t", "c:1.1"]).unwrap();
    let path = || display(&srv, "c:1.2", "#{pane_current_path}");
    srv.wait_for(|| path().ends_with("/sub"), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", path()));

    let out = srv.home.join("pwd.out");
    let script = format!("pwd -P > '{}'; sleep 30", out.display());
    srv.cmd(&[
        "split-window",
        "-t",
        "c:1.2",
        "-c",
        "#{pane_current_path}",
        &script,
    ])
    .unwrap();
    let got = wait_for_file(&out);
    assert_eq!(got, format!("{}\n", sub.canonicalize().unwrap().display()));
}

/// As tmux 3.7b: `-L n` moves the pane's right border left by n cells, or
/// its left border when it has no right neighbour; `-R`, `-U` and `-D`
/// likewise.
#[test]
fn resize_pane_moves_the_border_by_exact_cells() {
    let srv = TestServer::start();
    session(&srv, "r", 81, 21);
    srv.cmd(&["split-window", "-h", "-t", "r", "sleep 30"])
        .unwrap();
    assert_eq!(widths(&srv, "r"), vec![40, 40]);

    srv.cmd(&["resize-pane", "-t", "r:1.1", "-L", "5"]).unwrap();
    assert_eq!(widths(&srv, "r"), vec![35, 45]);
    srv.cmd(&["resize-pane", "-t", "r:1.2", "-L", "5"]).unwrap();
    assert_eq!(widths(&srv, "r"), vec![30, 50]);
    srv.cmd(&["resize-pane", "-t", "r:1.2", "-R", "3"]).unwrap();
    assert_eq!(widths(&srv, "r"), vec![33, 47]);
    // No amount means one cell.
    srv.cmd(&["resize-pane", "-t", "r:1.1", "-R"]).unwrap();
    assert_eq!(widths(&srv, "r"), vec![34, 46]);

    session(&srv, "v", 40, 21);
    srv.cmd(&["split-window", "-v", "-t", "v", "sleep 30"])
        .unwrap();
    assert_eq!(heights(&srv, "v"), vec![10, 10]);
    srv.cmd(&["resize-pane", "-t", "v:1.1", "-D", "4"]).unwrap();
    assert_eq!(heights(&srv, "v"), vec![14, 6]);
    srv.cmd(&["resize-pane", "-t", "v:1.2", "-U", "2"]).unwrap();
    assert_eq!(heights(&srv, "v"), vec![12, 8]);

    // The pty follows.
    srv.cmd(&["respawn-pane", "-k", "-t", "v:1.2", "stty size; sleep 30"])
        .unwrap();
    wait_for_line(&srv, "v:1.2", "8 40");
}

/// After a `kill-pane` in a three-column window the remaining border still
/// moves by exactly the requested cells.
#[test]
fn resize_pane_after_kill_pane_moves_the_border_exactly() {
    let srv = TestServer::start();
    session(&srv, "k", 92, 20);
    srv.cmd(&["split-window", "-h", "-t", "k", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-h", "-t", "k:1.2", "sleep 30"])
        .unwrap();
    srv.cmd(&["resize-pane", "-t", "k:1.1", "-R", "7"]).unwrap();
    srv.cmd(&["kill-pane", "-t", "k:1.2"]).unwrap();
    let before = widths(&srv, "k");
    assert_eq!(before.len(), 2);
    assert_eq!(before[0] + before[1] + 1, 92);

    srv.cmd(&["resize-pane", "-t", "k:1.1", "-L", "5"]).unwrap();
    assert_eq!(widths(&srv, "k"), vec![before[0] - 5, before[1] + 5]);
    srv.cmd(&["resize-pane", "-t", "k:1.2", "-R", "2"]).unwrap();
    assert_eq!(widths(&srv, "k"), vec![before[0] - 3, before[1] + 3]);
}

#[test]
fn resize_pane_zoom_toggles_the_active_pane_to_the_window_size() {
    let srv = TestServer::start();
    session(&srv, "z", 81, 20);
    srv.cmd(&["split-window", "-h", "-t", "z", "sleep 30"])
        .unwrap();
    srv.cmd(&["resize-pane", "-Z", "-t", "z"]).unwrap();
    assert_eq!(display(&srv, "z", "#{window_zoomed_flag}"), "1");
    assert_eq!(display(&srv, "z", "#{pane_width}x#{pane_height}"), "81x20");

    srv.cmd(&["resize-pane", "-Z", "-t", "z"]).unwrap();
    assert_eq!(display(&srv, "z", "#{window_zoomed_flag}"), "0");
    assert_eq!(widths(&srv, "z"), vec![40, 40]);

    // As tmux 3.7b: selecting the already-active pane keeps the zoom,
    // selecting another pane leaves it.
    srv.cmd(&["resize-pane", "-Z", "-t", "z:1.2"]).unwrap();
    srv.cmd(&["select-pane", "-t", "z:1.2"]).unwrap();
    assert_eq!(display(&srv, "z", "#{window_zoomed_flag}"), "1");
    assert_eq!(widths(&srv, "z"), vec![40, 81]);
    srv.cmd(&["select-pane", "-t", "z:1.1"]).unwrap();
    assert_eq!(
        panes(
            &srv,
            "z",
            "#{pane_width}x#{pane_height} #{pane_active} #{window_zoomed_flag}"
        ),
        vec!["40x20 1 0", "40x20 0 0"]
    );

    // A layout change unzooms, as in tmux.
    srv.cmd(&["resize-pane", "-Z", "-t", "z"]).unwrap();
    srv.cmd(&["split-window", "-v", "-t", "z", "sleep 30"])
        .unwrap();
    assert_eq!(display(&srv, "z", "#{window_zoomed_flag}"), "0");
    assert_eq!(widths(&srv, "z"), vec![40, 40, 40]);
}

#[test]
fn select_pane_by_target_and_direction() {
    let srv = TestServer::start();
    session(&srv, "p", 81, 20);
    srv.cmd(&["split-window", "-h", "-t", "p", "sleep 30"])
        .unwrap();
    srv.cmd(&["split-window", "-v", "-t", "p:1.2", "sleep 30"])
        .unwrap();
    // Layout: 1 on the left, 2 above 3 on the right; 3 is active.
    assert_eq!(active(&srv, "p"), "3");

    srv.cmd(&["select-pane", "-t", "p", "-U"]).unwrap();
    assert_eq!(active(&srv, "p"), "2");
    srv.cmd(&["select-pane", "-t", "p", "-L"]).unwrap();
    assert_eq!(active(&srv, "p"), "1");
    // Past the edge wraps around, as tmux does.
    srv.cmd(&["select-pane", "-t", "p", "-L"]).unwrap();
    assert_eq!(active(&srv, "p"), "2");
    srv.cmd(&["select-pane", "-t", "p", "-D"]).unwrap();
    assert_eq!(active(&srv, "p"), "3");
    srv.cmd(&["select-pane", "-t", "p", "-D"]).unwrap();
    assert_eq!(active(&srv, "p"), "2");

    // No direction: the target becomes active, in its own window, which
    // does not become the session's current window (as tmux).
    srv.cmd(&["new-window", "-t", "p", "sleep 30"]).unwrap();
    srv.cmd(&["select-pane", "-t", "p:1.1"]).unwrap();
    assert_eq!(active(&srv, "p:1"), "1");
    assert_eq!(display(&srv, "p", "#{window_index}"), "2");

    let err = srv.cmd(&["select-pane", "-t", "p:1.9"]).unwrap_err();
    assert_eq!(err.trim_end(), "can't find pane: 9");
}

#[test]
fn kill_pane_removes_the_pane_and_cascades() {
    let srv = TestServer::start();
    session(&srv, "k", 81, 20);
    srv.cmd(&["split-window", "-h", "-t", "k", "sleep 30"])
        .unwrap();
    let ids = panes(&srv, "k", "#{pane_id}");

    srv.cmd(&["kill-pane", "-t", &ids[1]]).unwrap();
    assert_eq!(panes(&srv, "k", "#{pane_id}"), vec![ids[0].clone()]);
    assert_eq!(widths(&srv, "k"), vec![81], "the survivor takes the space");
    assert_eq!(active(&srv, "k"), "1");

    // Killing a window's last pane closes the window; the last session's
    // last pane stops the server.
    srv.cmd(&["new-window", "-t", "k", "sleep 30"]).unwrap();
    srv.cmd(&["kill-pane", "-t", "k:2.1"]).unwrap();
    assert!(srv.cmd(&["list-panes", "-t", "k:2"]).is_err());
    srv.cmd(&["kill-pane", "-t", "k:1.1"]).unwrap();
    srv.wait_for(|| !srv.is_running(), WAIT)
        .expect("server stops after its last pane");
}

/// `respawn-pane` revives a dead pane under the same id and `pane_uid`,
/// refuses a live pane without `-k`, and with no command reruns the
/// pane's resume argv.
#[test]
fn respawn_pane_keeps_the_uid() {
    let srv = TestServer::start();
    session(&srv, "r", 80, 20);
    srv.cmd(&["split-window", "-t", "r", "echo first; exit 3"])
        .unwrap();
    srv.wait_for(|| display(&srv, "r:1.2", "#{pane_dead}") == "1", WAIT)
        .expect("pane dies and stays (remain-on-exit)");
    let before = display(&srv, "r:1.2", "#{pane_id} #{pane_uid}");

    srv.cmd(&["respawn-pane", "-t", "r:1.2", "echo second; sleep 30"])
        .unwrap();
    wait_for_line(&srv, "r:1.2", "second");
    assert_eq!(display(&srv, "r:1.2", "#{pane_dead}"), "0");
    assert_eq!(display(&srv, "r:1.2", "#{pane_id} #{pane_uid}"), before);

    let err = srv
        .cmd(&["respawn-pane", "-t", "r:1.2", "sleep 30"])
        .unwrap_err();
    assert_eq!(
        err.trim_end(),
        "respawn pane failed: pane r:1.2 still active"
    );

    // -k kills the live process; no command reruns the last one.
    let pid = display(&srv, "r:1.2", "#{pane_pid}");
    srv.cmd(&["respawn-pane", "-k", "-t", "r:1.2"]).unwrap();
    assert_ne!(display(&srv, "r:1.2", "#{pane_pid}"), pid);
    wait_for_line(&srv, "r:1.2", "second");
    assert_eq!(display(&srv, "r:1.2", "#{pane_id} #{pane_uid}"), before);
    // The respawned pane is still live after the old process's exit.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(display(&srv, "r:1.2", "#{pane_dead}"), "0");
}

#[test]
fn send_keys_encodes_key_names_literals_and_hex() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "k",
        "stty -echo -isig; exec cat -vt",
    ])
    .unwrap();
    srv.wait_for(
        || display(&srv, "k", "#{pane_current_command}").ends_with("cat"),
        WAIT,
    )
    .expect("cat is in the foreground");

    srv.cmd(&["send-keys", "-t", "k", "-l", "Enter"]).unwrap();
    srv.cmd(&["send-keys", "-t", "k", "Enter"]).unwrap();
    wait_for_line(&srv, "k", "Enter");

    srv.cmd(&["send-keys", "-t", "k", "-H", "41", "42", "0a"])
        .unwrap();
    wait_for_line(&srv, "k", "AB");

    srv.cmd(&["send-keys", "-t", "k", "Tab", "Up", "x y", "Enter"])
        .unwrap();
    wait_for_line(&srv, "k", "^I^[[Ax y");

    // Modifiers: C-c is 0x03 (isig is off, so it reaches cat as data) and
    // M-x is ESC x.
    srv.cmd(&["send-keys", "-t", "k", "C-c", "M-x", "Enter"])
        .unwrap();
    wait_for_line(&srv, "k", "^C^[x");
}

#[test]
fn capture_pane_honours_start_and_end() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "c",
        "-x",
        "20",
        "-y",
        "5",
        "for i in 1 2 3 4 5 6 7 8; do echo l$i; done; exec sleep 30",
    ])
    .unwrap();
    // Eight lines on a five-row screen: l5..l8 visible, l1..l4 in history.
    wait_for_line(&srv, "c", "l8");
    let cap = |s: &str, e: &str| {
        srv.cmd(&["capture-pane", "-p", "-t", "c", "-S", s, "-E", e])
            .unwrap()
    };
    assert_eq!(cap("0", "1"), "l5\nl6\n");
    assert_eq!(cap("-2", "0"), "l3\nl4\nl5\n");
    assert_eq!(cap("-", "-1"), "l1\nl2\nl3\nl4\n");
    assert_eq!(
        srv.cmd(&["capture-pane", "-p", "-t", "c"]).unwrap(),
        "l5\nl6\nl7\nl8\n\n"
    );
}
