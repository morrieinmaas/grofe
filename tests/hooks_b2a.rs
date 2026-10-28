//! B2a: hooks beyond the twelve of TC.5: the scopes `set-hook` can set at,
//! and the notify hooks grofe finds by looking at the model (window
//! renames, links, titles, the clipboard). tmux's own behaviour for the
//! common cases is in `golden/b2a`; these cover what the goldens cannot
//! show (native names, panes that write escape sequences).

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::{TestServer, wait_until};

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(400);

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn logging(fmt: &str, log: &Path) -> String {
    format!("run-shell \"echo '{fmt}' >> {}\"", log.display())
}

/// A hook set with `-w` belongs to one window; the global one is only a
/// fallback for windows that set none.
#[test]
fn a_window_hook_runs_for_its_own_window_and_hides_the_global_one() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "a", "-n", "one"]);
    ok(&srv, &["new-window", "-d", "-t", "a", "-n", "two"]);
    let log = srv.home.join("w.log");
    let global = logging("global #{hook_window_name}", &log);
    let local = logging("local #{hook_window_name}", &log);
    ok(&srv, &["set-hook", "-g", "window-renamed", &global]);
    ok(
        &srv,
        &["set-hook", "-w", "-t", "a:1", "window-renamed", &local],
    );

    ok(&srv, &["rename-window", "-t", "a:2", "other"]);
    assert!(wait_until(|| log_lines(&log).len() == 1, WAIT).is_ok());
    ok(&srv, &["rename-window", "-t", "a:1", "mine"]);
    assert!(wait_until(|| log_lines(&log).len() == 2, WAIT).is_ok());
    std::thread::sleep(QUIET);
    assert_eq!(log_lines(&log), ["global other", "local mine"]);

    // Unsetting the window's hook lets the global one back in.
    ok(
        &srv,
        &["set-hook", "-u", "-w", "-t", "a:1", "window-renamed"],
    );
    ok(&srv, &["rename-window", "-t", "a:1", "again"]);
    assert!(wait_until(|| log_lines(&log).len() == 3, WAIT).is_ok());
    assert_eq!(log_lines(&log)[2], "global again");
}

/// A session's hooks do not reach another session.
#[test]
fn a_session_hook_stays_with_its_session() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "a"]);
    ok(&srv, &["new-session", "-d", "-s", "b"]);
    let log = srv.home.join("s.log");
    let cmd = logging("renamed #{hook_session_name}", &log);
    ok(&srv, &["set-hook", "-t", "a", "session-renamed", &cmd]);
    ok(&srv, &["rename-session", "-t", "b", "bb"]);
    ok(&srv, &["rename-session", "-t", "a", "aa"]);
    assert!(wait_until(|| !log_lines(&log).is_empty(), WAIT).is_ok());
    std::thread::sleep(QUIET);
    assert_eq!(log_lines(&log), ["renamed aa"]);
    assert_eq!(
        ok(&srv, &["show-hooks", "-t", "aa", "session-renamed"])
            .lines()
            .count(),
        1
    );
}

/// `pane-set-clipboard` follows `set-clipboard on`, as tmux's OSC 52
/// handler, and names the pane.
#[test]
fn pane_set_clipboard_runs_when_a_pane_stores_to_the_clipboard() {
    let srv = TestServer::start();
    ok(&srv, &["set", "-g", "set-clipboard", "on"]);
    let log = srv.home.join("c.log");
    let cmd = logging("#{hook} #{hook_pane}", &log);
    ok(&srv, &["new-session", "-d", "-s", "a", "sleep 30"]);
    ok(&srv, &["set-hook", "-g", "pane-set-clipboard", &cmd]);
    ok(
        &srv,
        &[
            "split-window",
            "-d",
            "-t",
            "a",
            "sh -c \"printf '\\033]52;c;aGk=\\007'; sleep 30\"",
        ],
    );
    assert!(wait_until(|| !log_lines(&log).is_empty(), WAIT).is_ok());
    assert_eq!(log_lines(&log), ["pane-set-clipboard %1"]);
}

/// `command-error` runs when a command fails, and only then.
#[test]
fn command_error_runs_for_a_failed_command() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "a"]);
    let log = srv.home.join("e.log");
    ok(
        &srv,
        &["set-hook", "-g", "command-error", &logging("#{hook}", &log)],
    );
    ok(&srv, &["list-sessions"]);
    assert!(srv.cmd(&["rename-window", "-t", "nope", "x"]).is_err());
    assert!(wait_until(|| !log_lines(&log).is_empty(), WAIT).is_ok());
    std::thread::sleep(QUIET);
    assert_eq!(log_lines(&log), ["command-error"]);
}

/// A hook that is accepted but never fires still lists, so a tmux config
/// that sets it loads.
#[test]
fn every_hook_tmux_names_can_be_set_and_listed() {
    let srv = TestServer::start();
    ok(&srv, &["new-session", "-d", "-s", "a"]);
    for name in [
        "after-queue",
        "client-light-theme",
        "pane-mode-changed",
        "after-lock-server",
    ] {
        ok(&srv, &["set-hook", "-g", name, "list-sessions"]);
        let shown = ok(&srv, &["show-hooks", "-g", name]);
        assert_eq!(shown, format!("{name} list-sessions\n"), "{name}");
    }
}
