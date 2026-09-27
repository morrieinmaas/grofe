mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::TestServer;

const WAIT: Duration = Duration::from_secs(5);

fn new_session(srv: &TestServer, name: &str) {
    srv.cmd(&["new-session", "-d", "-s", name, "sleep", "30"])
        .expect("new-session");
}

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, fmt])
        .unwrap_or_else(|e| panic!("display-message {fmt}: {e}"))
}

fn wait_for_file(path: &Path) -> String {
    common::wait_until(
        || fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{} never written: {e}", path.display()));
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn scope_precedence_is_pane_over_window_over_session_over_global() {
    let srv = TestServer::start();
    new_session(&srv, "s");
    srv.cmd(&["new-window", "-d", "-t", "s", "sleep", "30"])
        .unwrap();
    let x = |target: &str| display(&srv, target, "#{@x}");

    assert_eq!(x("s:1.1"), "\n");
    srv.cmd(&["set", "-g", "@x", "global"]).unwrap();
    assert_eq!(x("s:1.1"), "global\n");
    srv.cmd(&["set", "-s", "-t", "s", "@x", "session"]).unwrap();
    assert_eq!(x("s:1.1"), "session\n");
    srv.cmd(&["set", "-w", "-t", "s:1", "@x", "window"])
        .unwrap();
    assert_eq!(x("s:1.1"), "window\n");
    srv.cmd(&["set", "-p", "-t", "s:1.1", "@x", "pane"])
        .unwrap();
    assert_eq!(x("s:1.1"), "pane\n");
    // The other window of the session sees only the session value.
    assert_eq!(x("s:2.1"), "session\n");

    srv.cmd(&["set", "-u", "-p", "-t", "s:1.1", "@x"]).unwrap();
    assert_eq!(x("s:1.1"), "window\n");
    srv.cmd(&["set", "-u", "-w", "-t", "s:1", "@x"]).unwrap();
    assert_eq!(x("s:1.1"), "session\n");
    srv.cmd(&["set", "-u", "-s", "-t", "s", "@x"]).unwrap();
    assert_eq!(x("s:1.1"), "global\n");
}

#[test]
fn user_options_default_to_session_scope() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    new_session(&srv, "b");
    srv.cmd(&["set", "-t", "a", "@u", "one two"]).unwrap();

    assert_eq!(
        srv.cmd(&["show", "-s", "-t", "a"]).unwrap(),
        "@u \"one two\"\n"
    );
    assert_eq!(
        srv.cmd(&["show", "-t", "a", "@u"]).unwrap(),
        "@u \"one two\"\n"
    );
    assert_eq!(srv.cmd(&["show", "-g", "@u"]).unwrap(), "");
    assert_eq!(display(&srv, "a", "#{@u}"), "one two\n");
    assert_eq!(display(&srv, "b", "#{@u}"), "\n");
}

#[test]
fn show_options_lists_the_builtin_globals() {
    let srv = TestServer::start();
    let out = srv.cmd(&["show-options", "-g"]).unwrap();
    // The harness starts servers with remain-on-exit on.
    for line in ["remain-on-exit on", "history-limit 50000", "base-index 1"] {
        assert!(out.lines().any(|l| l == line), "{line:?} in {out:?}");
    }
    srv.cmd(&["set", "-g", "history-limit", "2000"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "history-limit"]).unwrap(),
        "history-limit 2000\n"
    );
    srv.cmd(&["set", "-gu", "history-limit"]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "history-limit"]).unwrap(),
        "history-limit 50000\n"
    );
}

#[test]
fn unknown_builtin_option_errors() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    let err = srv.cmd(&["set", "-g", "no-such-option", "1"]).unwrap_err();
    assert!(err.contains("invalid option: no-such-option"), "{err}");
    let err = srv.cmd(&["show", "-g", "no-such-option"]).unwrap_err();
    assert!(err.contains("invalid option: no-such-option"), "{err}");
    let err = srv
        .cmd(&["set", "-g", "remain-on-exit", "maybe"])
        .unwrap_err();
    assert!(err.contains("invalid value for remain-on-exit"), "{err}");
    let err = srv.cmd(&["set", "-g", "@x"]).unwrap_err();
    assert!(err.contains("empty value"), "{err}");
    // A user option needs no declaration.
    srv.cmd(&["set", "-g", "@anything", "1"]).unwrap();
}

#[test]
fn remain_on_exit_is_looked_up_per_pane() {
    let srv = TestServer::start();
    new_session(&srv, "r");
    for _ in 0..3 {
        srv.cmd(&["new-window", "-d", "-t", "r", "sleep", "2"])
            .unwrap();
    }
    srv.cmd(&["set", "-p", "-t", "r:2.1", "remain-on-exit", "on"])
        .unwrap();
    srv.cmd(&["set", "-w", "-t", "r:4", "remain-on-exit", "on"])
        .unwrap();
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();

    let windows = || srv.cmd(&["list-windows", "-t", "r", "-F", "#{window_index}"]);
    srv.wait_for(|| windows().is_ok_and(|w| w == "1\n2\n4\n"), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", windows()));
    for w in ["r:2", "r:4"] {
        srv.wait_for(
            || srv.cmd(&["list-panes", "-t", w, "-F", "#{pane_dead}"]) == Ok("1\n".into()),
            WAIT,
        )
        .unwrap_or_else(|e| panic!("{w} not dead: {e}"));
    }
}

#[test]
fn list_panes_format_matches_the_layout() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "l",
        "-x",
        "100",
        "-y",
        "30",
        "sleep",
        "30",
    ])
    .unwrap();
    let default = srv.cmd(&["list-panes", "-t", "l"]).unwrap();
    let id = default
        .split_whitespace()
        .find(|w| w.starts_with('%'))
        .unwrap()
        .to_string();
    assert!(default.contains("[100x30]"), "{default}");

    let out = srv
        .cmd(&[
            "list-panes",
            "-t",
            "l",
            "-F",
            "#{pane_id} #{pane_width} #{pane_height} #{pane_index} #{pane_active}",
        ])
        .unwrap();
    assert_eq!(out, format!("{id} 100 30 1 1\n"));
}

#[test]
fn list_windows_and_sessions_take_formats() {
    let srv = TestServer::start();
    new_session(&srv, "one");
    new_session(&srv, "two");
    srv.cmd(&["new-window", "-d", "-t", "two", "-n", "side", "sleep", "30"])
        .unwrap();

    let windows = srv
        .cmd(&[
            "list-windows",
            "-t",
            "two",
            "-F",
            "#{window_index}:#{window_name} #{window_active} #{window_zoomed_flag} #{session_name}",
        ])
        .unwrap();
    let lines: Vec<&str> = windows.lines().collect();
    assert_eq!(lines.len(), 2, "{windows}");
    assert!(
        lines[0].starts_with("1:") && lines[0].ends_with(" 1 0 two"),
        "{windows}"
    );
    assert_eq!(lines[1], "2:side 0 0 two");

    let sessions = srv
        .cmd(&["list-sessions", "-F", "#{session_name} #{session_id}"])
        .unwrap();
    assert_eq!(sessions, "one $0\ntwo $1\n");
}

#[test]
fn display_message_prints_pane_variables() {
    let srv = TestServer::start();
    new_session(&srv, "d");
    let out = display(
        &srv,
        "d",
        "#{session_name}:#{window_index}.#{pane_index} #{pane_active} #{pane_dead} #{?pane_active,yes,no}",
    );
    assert_eq!(out, "d:1.1 1 0 yes\n");

    let uid = display(&srv, "d", "#{pane_uid}");
    assert_eq!(uid.trim().len(), 26, "{uid:?}");
    let pid = display(&srv, "d", "#{pane_pid}");
    assert!(pid.trim().parse::<u32>().is_ok(), "{pid:?}");

    let default = srv.cmd(&["display-message", "-p", "-t", "d"]).unwrap();
    assert!(default.starts_with("[d] 1:"), "{default}");
    assert!(default.trim_end().ends_with("current pane 1"), "{default}");
}

#[test]
fn pane_current_argv_shows_the_foreground_argv() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    let argv = || display(&srv, "a", "#{pane_current_argv}");
    srv.wait_for(|| argv() == "sleep 30\n", WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", argv()));
    // The command is the process name, which need not match argv[0] (a
    // coreutils `sleep` can be `gsleep`).
    let command = display(&srv, "a", "#{pane_current_command}");
    assert!(command.ends_with("sleep\n"), "{command:?}");
}

/// `-c '#{pane_current_path}'` expands at execution time against the
/// target pane. `split-window` is T1.5b's, so this goes through
/// `new-window`, which shares `resolve_cwd`.
#[test]
fn dash_c_expands_pane_current_path_of_the_target() {
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
        "cd sub && exec sleep 30",
    ])
    .unwrap();
    let path = || display(&srv, "c", "#{pane_current_path}");
    srv.wait_for(|| path().trim_end().ends_with("/sub"), WAIT)
        .unwrap_or_else(|e| panic!("{e}: {:?}", path()));

    let out = srv.home.join("pwd.out");
    let script = format!("pwd -P > '{}'; sleep 30", out.display());
    srv.cmd(&[
        "new-window",
        "-d",
        "-t",
        "c",
        "-c",
        "#{pane_current_path}",
        &script,
    ])
    .unwrap();
    let got = wait_for_file(&out);
    assert_eq!(got, format!("{}\n", sub.canonicalize().unwrap().display()));
}
