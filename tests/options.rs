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
    // As tmux: showing a user option not set at that scope is an error.
    let err = srv.cmd(&["show", "-g", "@u"]).unwrap_err();
    assert!(err.contains("invalid option: @u"), "{err}");
    let err = srv.cmd(&["show", "-t", "b", "@nope"]).unwrap_err();
    assert!(err.contains("invalid option: @nope"), "{err}");
    assert_eq!(display(&srv, "a", "#{@u}"), "one two\n");
    srv.cmd(&["set", "-g", "@e", ""]).unwrap();
    assert_eq!(srv.cmd(&["show", "-g", "@e"]).unwrap(), "@e ''\n");
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
    // Value errors are worded as tmux words them.
    for (name, value, want) in [
        ("remain-on-exit", "maybe", "unknown value: maybe"),
        ("history-limit", "abc", "value is invalid: abc"),
        ("history-limit", "3x", "value is invalid: 3x"),
        ("history-limit", "-3", "value is too small: -3"),
        (
            "history-limit",
            "2147483648",
            "value is too large: 2147483648",
        ),
        ("pane-base-index", "65536", "value is too large: 65536"),
    ] {
        let err = srv.cmd(&["set", "-g", name, value]).unwrap_err();
        assert!(err.contains(want), "{name} {value}: {err}");
    }
    srv.cmd(&["set", "-g", "history-limit", "2147483647"])
        .unwrap();
    let err = srv.cmd(&["set", "-g", "@x"]).unwrap_err();
    assert!(err.contains("empty value"), "{err}");
    // A user option needs no declaration.
    srv.cmd(&["set", "-g", "@anything", "1"]).unwrap();
}

/// tmux 3.7b checks the option name before the missing value: an unknown
/// name given no value says `invalid option: <name>`, not `empty value`.
#[test]
fn unknown_option_with_no_value_names_the_option() {
    let srv = TestServer::start();
    new_session(&srv, "a");
    let err = srv.cmd(&["set", "-g", "bogus-option"]).unwrap_err();
    assert!(err.contains("invalid option: bogus-option"), "{err}");
    assert!(!err.contains("empty value"), "{err}");
}

#[test]
fn remain_on_exit_is_looked_up_per_pane() {
    let srv = TestServer::start();
    new_session(&srv, "r");
    // The windows keep their indexes while the panes end.
    srv.cmd(&["set", "-g", "renumber-windows", "off"]).unwrap();
    for _ in 0..3 {
        srv.cmd(&["new-window", "-d", "-t", "r", "sleep", "30"])
            .unwrap();
    }
    srv.cmd(&["set", "-p", "-t", "r:2.1", "remain-on-exit", "on"])
        .unwrap();
    srv.cmd(&["set", "-w", "-t", "r:4", "remain-on-exit", "on"])
        .unwrap();
    srv.cmd(&["set", "-g", "remain-on-exit", "off"]).unwrap();
    // Only now end the panes in windows 2 to 4.
    for w in ["r:2", "r:3", "r:4"] {
        let pid: i32 = display(&srv, w, "#{pane_pid}").trim().parse().unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
    }

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

/// tmux silently falls back to the current pane when `-t` names nothing;
/// per the orchestrator's ruling, native grofe errors instead, the same
/// as every other command taking a pane target (that leniency is left to
/// the M3.6 compat front end).
#[test]
fn display_message_errors_on_an_unresolvable_target() {
    let srv = TestServer::start();
    new_session(&srv, "d");
    let err = srv
        .cmd(&["display-message", "-p", "-t", "no-such-session", "hi"])
        .unwrap_err();
    assert!(err.contains("can't find pane: no-such-session"), "{err}");
    let err = srv
        .cmd(&["display-message", "-p", "-t", "%999", "hi"])
        .unwrap_err();
    assert!(err.contains("can't find pane: %999"), "{err}");
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

/// As tmux: a flag option set with no value flips the value it has at
/// that scope, inherited or not.
#[test]
fn bool_option_with_no_value_toggles() {
    let srv = TestServer::start();
    new_session(&srv, "t");
    let show = |args: &[&str]| srv.cmd(args).unwrap();

    // The harness starts servers with remain-on-exit on.
    srv.cmd(&["set", "-g", "remain-on-exit"]).unwrap();
    assert_eq!(
        show(&["show", "-g", "remain-on-exit"]),
        "remain-on-exit off\n"
    );
    srv.cmd(&["set", "-g", "remain-on-exit"]).unwrap();
    assert_eq!(
        show(&["show", "-g", "remain-on-exit"]),
        "remain-on-exit on\n"
    );

    // No window value yet: the inherited `on` flips to a window `off`.
    srv.cmd(&["set", "-w", "-t", "t:1", "remain-on-exit"])
        .unwrap();
    assert_eq!(
        show(&["show", "-w", "-t", "t:1", "remain-on-exit"]),
        "remain-on-exit off\n"
    );
    // The pane inherits the window's `off` and flips it to `on`.
    srv.cmd(&["set", "-p", "-t", "t:1.1", "remain-on-exit"])
        .unwrap();
    assert_eq!(
        show(&["show", "-p", "-t", "t:1.1", "remain-on-exit"]),
        "remain-on-exit on\n"
    );

    // A number option still needs a value.
    let err = srv.cmd(&["set", "-g", "history-limit"]).unwrap_err();
    assert!(err.contains("empty value"), "{err}");
}

/// A missing `-c` directory falls back to `$HOME`, as in tmux 3.7b.
#[test]
fn dash_c_with_a_missing_directory_falls_back_to_home() {
    let srv = TestServer::start();
    new_session(&srv, "m");
    let missing = srv.home.join("no-such-dir");
    srv.cmd(&[
        "new-window",
        "-t",
        "m",
        "-c",
        missing.to_str().unwrap(),
        "sleep",
        "30",
    ])
    .unwrap();
    let windows = srv
        .cmd(&["list-windows", "-t", "m", "-F", "#{window_index}"])
        .unwrap();
    assert_eq!(windows, "1\n2\n");
    let home = format!("{}\n", fs::canonicalize(&srv.home).unwrap().display());
    srv.wait_for(
        || display(&srv, "m:2", "#{pane_current_path}") == home,
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: {}", display(&srv, "m:2", "#{pane_current_path}")));
}

/// The M2 options, by tmux name: each shows its default, takes a value at
/// its own scope and shows it back (tmux 3.7b bounds and choice words).
#[test]
fn m2_options_set_and_show_by_tmux_name() {
    let srv = TestServer::start();
    new_session(&srv, "m");
    let show = |args: &[&str]| srv.cmd(args).unwrap();
    // (name, default as `show` prints it, scope flag, new value)
    let rows: &[(&str, &str, &str, &str)] = &[
        ("status", "on", "-s", "off"),
        ("status-left", "\"[#{session_name}] \"", "-s", "L"),
        (
            "status-right",
            "\"\\\"#{pane_title}\\\" %H:%M %d-%b-%y\"",
            "-s",
            "R",
        ),
        ("status-left-length", "10", "-s", "32767"),
        ("status-right-length", "40", "-s", "0"),
        ("status-style", "bg=green,fg=black", "-s", "fg=red"),
        ("status-interval", "15", "-s", "2147483647"),
        (
            "window-status-format",
            "\"#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }\"",
            "-w",
            "#{window_index}",
        ),
        (
            "window-status-current-format",
            "\"#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }\"",
            "-w",
            "[#{window_index}]",
        ),
        ("prefix", "C-b", "-s", "C-a"),
        ("escape-time", "25", "-g", "0"),
        ("repeat-time", "500", "-s", "2000000"),
        ("automatic-rename", "on", "-w", "off"),
        (
            "automatic-rename-format",
            "\"#{b:pane_current_path}\"",
            "-w",
            "#{pane_current_command}",
        ),
        ("allow-rename", "off", "-w", "on"),
    ];
    for (name, default, flag, value) in rows {
        assert_eq!(
            show(&["show", "-g", name]),
            format!("{name} {default}\n"),
            "default of {name}"
        );
        let target = if *flag == "-w" { "m:1" } else { "m" };
        let mut set = vec!["set", *flag];
        if *flag != "-g" {
            set.extend(["-t", target]);
        }
        set.extend([*name, *value]);
        srv.cmd(&set).unwrap_or_else(|e| panic!("set {name}: {e}"));
        let mut get = vec!["show", *flag];
        if *flag != "-g" {
            get.extend(["-t", target]);
        }
        get.push(name);
        // As tmux 3.7b, a value holding `#` shows double-quoted.
        let shown = if value.contains('#') {
            format!("\"{value}\"")
        } else {
            value.to_string()
        };
        assert_eq!(show(&get), format!("{name} {shown}\n"), "{name}");
    }
    // Without a scope flag each lands at its own tmux scope.
    srv.cmd(&["set", "-t", "m", "status-left", "x"]).unwrap();
    assert_eq!(
        show(&["show", "-s", "-t", "m", "status-left"]),
        "status-left x\n"
    );
    srv.cmd(&["set", "-t", "m:1", "window-status-format", "y"])
        .unwrap();
    assert_eq!(
        show(&["show", "-w", "-t", "m:1", "window-status-format"]),
        "window-status-format y\n"
    );
    srv.cmd(&["set", "escape-time", "7"]).unwrap();
    assert_eq!(show(&["show", "-g", "escape-time"]), "escape-time 7\n");
}

#[test]
fn m2_option_values_are_checked_as_tmux_checks_them() {
    let srv = TestServer::start();
    for (name, value, want) in [
        ("status", "yes", "unknown value: yes"),
        ("status", "6", "unknown value: 6"),
        ("status-left-length", "32768", "value is too large: 32768"),
        (
            "status-interval",
            "2147483648",
            "value is too large: 2147483648",
        ),
        (
            "escape-time",
            "2147483648",
            "value is too large: 2147483648",
        ),
        ("repeat-time", "2000001", "value is too large: 2000001"),
        ("allow-rename", "maybe", "bad value: maybe"),
        ("remain-on-exit", "yes", "unknown value: yes"),
    ] {
        let err = srv.cmd(&["set", "-g", name, value]).unwrap_err();
        assert!(err.contains(want), "{name} {value}: {err}");
    }
    // A string option still needs a value.
    let err = srv.cmd(&["set", "-g", "status-left"]).unwrap_err();
    assert!(err.contains("empty value"), "{err}");
    // An empty string is a value.
    srv.cmd(&["set", "-g", "status-left", ""]).unwrap();
    assert_eq!(
        srv.cmd(&["show", "-g", "status-left"]).unwrap(),
        "status-left ''\n"
    );
}

/// As tmux: `set status` with no value flips off and on and leaves a row
/// count as it is.
#[test]
fn status_with_no_value_toggles_off_and_on() {
    let srv = TestServer::start();
    let status = || srv.cmd(&["show", "-g", "status"]).unwrap();
    srv.cmd(&["set", "-g", "status"]).unwrap();
    assert_eq!(status(), "status off\n");
    srv.cmd(&["set", "-g", "status"]).unwrap();
    assert_eq!(status(), "status on\n");
    srv.cmd(&["set", "-g", "status", "2"]).unwrap();
    srv.cmd(&["set", "-g", "status"]).unwrap();
    assert_eq!(status(), "status 2\n");
}
