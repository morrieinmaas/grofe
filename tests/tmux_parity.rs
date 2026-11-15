//! tmux parity: target forms and the `display-message -a` variable list.
//!
//! Target forms `{last}`, `!`, `{next}`, `{previous}`, `+N` and `-N` in
//! the window and pane parts, and as a bare word. Every expected value was
//! recorded on tmux 3.7b (private `-L` server, `-f /dev/null`) in the same
//! state: session `a` with windows 0, 1, 2 (current 2, last 1); window 2
//! holds panes 0, 1, 2 (active 1, last 0).

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Command;

mod common;

use common::{CMD_TIMEOUT, CmdOutput, TestServer, run_bounded};

fn tmux(srv: &TestServer, args: &[&str]) -> CmdOutput {
    let root = srv.home.parent().expect("root").to_path_buf();
    let bin: PathBuf = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join("tmux");
    if !link.exists() {
        symlink(env!("CARGO_BIN_EXE_grofe"), &link).unwrap();
    }
    let tmpdir = root.join("tmux-tmpdir");
    fs::create_dir_all(&tmpdir).unwrap();
    let mut cmd = Command::new(link);
    cmd.current_dir(&srv.home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", &srv.home)
        .env("XDG_CONFIG_HOME", &srv.config_home)
        .env("XDG_STATE_HOME", &srv.state_home)
        .env("XDG_RUNTIME_DIR", &srv.runtime_dir)
        .env("TMUX_TMPDIR", tmpdir)
        .env("GROFE_RESTORE_ON_START", "0")
        .arg("-S")
        .arg(&srv.socket)
        .args(args);
    run_bounded(cmd, CMD_TIMEOUT)
}

fn ok(srv: &TestServer, args: &[&str]) -> String {
    let out = tmux(srv, args);
    assert!(out.status.success(), "{args:?}: {}", out.stderr);
    out.stdout
}

/// The state the module docs describe.
fn fixture() -> TestServer {
    let srv = TestServer::unstarted();
    ok(&srv, &["start-server"]);
    ok(
        &srv,
        &["new-session", "-d", "-s", "a", "-n", "w0", "sleep 600"],
    );
    ok(&srv, &["new-window", "-t", "a", "-n", "w1", "sleep 600"]);
    ok(&srv, &["new-window", "-t", "a", "-n", "w2", "sleep 600"]);
    ok(&srv, &["split-window", "-t", "a:2", "sleep 600"]);
    ok(&srv, &["split-window", "-t", "a:2", "sleep 600"]);
    for w in ["a:0", "a:1", "a:2"] {
        ok(&srv, &["select-window", "-t", w]);
    }
    ok(&srv, &["select-pane", "-t", "a:2.0"]);
    ok(&srv, &["select-pane", "-t", "a:2.1"]);
    srv
}

/// `window.pane` that `display-message -p -t target` reports.
fn at(srv: &TestServer, target: &str) -> String {
    let out = ok(
        srv,
        &[
            "display-message",
            "-p",
            "-t",
            target,
            "#{window_index}.#{pane_index}",
        ],
    );
    out.trim().to_string()
}

#[test]
fn window_and_pane_targets_resolve_as_tmux_does() {
    let srv = fixture();
    let cases: &[(&str, &str)] = &[
        (":{last}", "1.0"),
        (":!", "1.0"),
        (":{next}", "0.0"),
        (":{previous}", "1.0"),
        (":+", "0.0"),
        (":-", "1.0"),
        (":+1", "0.0"),
        (":+2", "1.0"),
        (":-1", "1.0"),
        (":-2", "0.0"),
        (":+5", "1.0"),
        (":-5", "0.0"),
        ("a:{last}", "1.0"),
        ("a:!", "1.0"),
        ("a:+", "0.0"),
        // A bare word is a pane for a pane verb.
        ("{last}", "2.0"),
        ("!", "2.0"),
        ("{next}", "2.2"),
        ("+", "2.2"),
        ("-", "2.0"),
        ("{previous}", "2.0"),
        (".{last}", "2.0"),
        (".!", "2.0"),
        (".{next}", "2.2"),
        (".{previous}", "2.0"),
        (".+", "2.2"),
        (".-", "2.0"),
        (".+1", "2.2"),
        (".+2", "2.0"),
        (".-1", "2.0"),
        (".-2", "2.2"),
        (".+5", "2.0"),
        (":1.+", "1.0"),
        (":0.{next}", "0.0"),
        (":!.0", "1.0"),
        (":+1.+1", "0.0"),
    ];
    for (target, want) in cases {
        assert_eq!(&at(&srv, target), want, "{target}");
    }
}

#[test]
fn a_bare_word_is_a_window_for_a_window_verb() {
    let srv = fixture();
    // (target, window selected), each from "current 2, last 1" with three
    // windows, so offsets wrap at 3.
    let cases: &[(&str, &str)] = &[
        ("!", "1"),
        ("{last}", "1"),
        ("{next}", "0"),
        ("{previous}", "1"),
        ("+", "0"),
        ("-", "1"),
        ("+1", "0"),
        ("-1", "1"),
        ("+2", "1"),
        ("^", "0"),
        ("{start}", "0"),
        ("{end}", "2"),
    ];
    for (target, want) in cases {
        for w in ["a:0", "a:1", "a:2"] {
            ok(&srv, &["select-window", "-t", w]);
        }
        ok(&srv, &["select-window", "-t", target]);
        assert_eq!(
            ok(
                &srv,
                &["display-message", "-p", "-t", "a", "#{window_index}"]
            )
            .trim(),
            *want,
            "select-window -t {target}"
        );
    }
}

#[test]
fn no_last_window_or_pane_says_so_in_tmuxs_words() {
    let srv = TestServer::unstarted();
    ok(&srv, &["start-server"]);
    ok(
        &srv,
        &["new-session", "-d", "-s", "a", "-n", "w0", "sleep 600"],
    );
    for (verb, target, want) in [
        ("select-window", ":{last}", "can't find window: !"),
        ("select-window", ":!", "can't find window: !"),
        ("select-window", "a:{last}", "can't find window: !"),
        ("select-pane", ".{last}", "can't find pane: !"),
        ("select-pane", ".!", "can't find pane: !"),
        ("select-pane", "{last}", "can't find pane: !"),
        ("select-pane", ":.{last}", "can't find pane: !"),
        ("select-pane", ":!.0", "can't find window: !"),
    ] {
        let out = tmux(&srv, &[verb, "-t", target]);
        assert!(!out.status.success(), "{verb} -t {target}");
        assert_eq!(out.stderr.trim(), want, "{verb} -t {target}");
    }
    // `{next}`, `+` and `-N` wrap onto the only window or pane.
    for target in [":{next}", ":+", ":+3", ":{previous}"] {
        assert_eq!(at(&srv, target), "0.0", "{target}");
    }
}

/// Variables of tmux 3.7b's `display-message -a` (one detached session, no
/// client) that grofe has no value for, and why. `docs/compat-coverage.md`
/// lists them too. A name missing from grofe and not here fails the test.
const NOT_PROVIDED: &[&str] = &[
    // The command being run: grofe's `display-message -a` has no command.
    "command",
    "config_files",
    // Emulator state alacritty keeps private or does not track.
    "history_all_bytes",
    "pane_bg",
    "pane_fg",
    "pane_key_mode",
    "pane_pb_progress",
    "pane_pb_state",
    "pane_tabs",
    "pane_unseen_changes",
    "scroll_region_lower",
    "scroll_region_upper",
    "synchronized_output_flag",
    // Data grofe does not keep.
    "session_last_attached",
    "sixel_support",
    "window_active_clients",
    "window_cell_height",
    "window_cell_width",
];

fn names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_once('=').map(|(n, _)| n.to_string()))
        .collect();
    names.sort();
    names
}

#[test]
fn display_message_a_lists_every_variable_tmux_does_but_the_documented_ones() {
    let srv = TestServer::unstarted();
    ok(&srv, &["start-server"]);
    ok(&srv, &["new-session", "-d", "-s", "a", "sleep 600"]);
    let got = names(&ok(&srv, &["display-message", "-a", "-t", "a"]));
    let pinned = include_str!("fixtures/tmux-display-message-a.names");
    let missing: Vec<&str> = pinned
        .lines()
        .filter(|n| !got.iter().any(|g| g == n) && !NOT_PROVIDED.contains(n))
        .collect();
    assert!(
        missing.is_empty(),
        "tmux lists these, grofe does not: {missing:?}"
    );
    let stale: Vec<&&str> = NOT_PROVIDED
        .iter()
        .filter(|n| got.iter().any(|g| g == **n))
        .collect();
    assert!(
        stale.is_empty(),
        "provided now, drop from NOT_PROVIDED: {stale:?}"
    );
}

#[test]
fn setting_an_inert_option_logs_once_that_it_has_no_effect() {
    let srv = TestServer::unstarted();
    ok(&srv, &["start-server"]);
    ok(&srv, &["new-session", "-d", "-s", "a", "sleep 600"]);
    let before = ok(&srv, &["show-messages"]);
    assert!(!before.contains("has no effect"), "{before}");
    ok(&srv, &["set-option", "-g", "cursor-style", "bar"]);
    ok(&srv, &["set-option", "-g", "cursor-style", "block"]);
    ok(&srv, &["set-option", "-g", "extended-keys", "on"]);
    // An option that does something logs nothing.
    ok(&srv, &["set-option", "-g", "escape-time", "5"]);
    let log = ok(&srv, &["show-messages"]);
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("has no effect"))
        .collect();
    assert_eq!(lines.len(), 2, "{log}");
    for name in ["cursor-style", "extended-keys"] {
        assert_eq!(
            lines.iter().filter(|l| l.contains(name)).count(),
            1,
            "{log}"
        );
    }
    // show-options stays as tmux prints it.
    assert_eq!(
        ok(&srv, &["show-options", "-gv", "cursor-style"]).trim(),
        "block"
    );
}
