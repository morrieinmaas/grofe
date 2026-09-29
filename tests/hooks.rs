//! TC.5: grofe's event hooks and the server environment (spec §4.2, §4.5
//! "Hooks and environment"). Each event FE.3's tmux hook names map onto
//! runs its hook once per triggering action, with the event's pane and
//! session; `set-hook -gu` stops it; a hook's `run-shell -b` never delays
//! the next command; `set-environment` reaches new panes and `run-shell`,
//! `-u` removes a variable, and a hidden one reaches command strings (`$X`)
//! but no process.
//!
//! Every server runs in the `TestServer` harness's own tempdirs.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(5);
/// How long a test waits to be sure a hook did not run (again).
const QUIET: Duration = Duration::from_millis(600);

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    ok(srv, &["display-message", "-p", "-t", target, fmt])
        .trim_end()
        .to_string()
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Binds `event` to a command that appends `<event> <pane_id>
/// <session_name> <hook_pane>` to `log`.
fn bind(srv: &TestServer, event: &str, log: &Path) {
    let cmd = format!(
        "run-shell 'echo {event} #{{pane_id}} #{{session_name}} #{{hook_pane}} >> {}'",
        log.display()
    );
    ok(srv, &["set-hook", "-g", event, &cmd]);
}

/// Waits for exactly one line in `log`, then makes sure no second one
/// comes, and returns it.
fn once(log: &Path) -> String {
    wait_until(|| !log_lines(log).is_empty(), WAIT)
        .unwrap_or_else(|e| panic!("the hook never ran: {e}"));
    std::thread::sleep(QUIET);
    let lines = log_lines(log);
    assert_eq!(lines.len(), 1, "the hook ran more than once: {lines:?}");
    let _ = fs::remove_file(log);
    lines[0].clone()
}

fn quiet(log: &Path) {
    std::thread::sleep(QUIET);
    assert_eq!(log_lines(log), Vec::<String>::new(), "the hook ran");
}

fn unbind(srv: &TestServer, event: &str) {
    ok(srv, &["set-hook", "-gu", event]);
}

fn pane_of(srv: &TestServer, target: &str) -> String {
    display(srv, target, "#{pane_id}")
}

fn start() -> (TestServer, PathBuf) {
    let srv = TestServer::start();
    ok(
        &srv,
        &["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"],
    );
    let log = srv.home.join("hooks.log");
    (srv, log)
}

/// Case 1, the command-shaped events (tmux's `after-*` names): each runs
/// its hook once per action, with the pane it acted on and its session.
#[test]
fn command_events_run_their_hook_once_with_the_events_pane() {
    let (srv, log) = start();
    let first = pane_of(&srv, "main");
    ok(&srv, &["split-window", "-t", "main"]);
    let second = pane_of(&srv, "main");

    // after-select-pane.
    bind(&srv, "pane-focused", &log);
    ok(&srv, &["select-pane", "-t", &first]);
    assert_eq!(once(&log), format!("pane-focused {first} main {first}"));
    // Selecting the pane that is already active changes nothing (tmux
    // 3.7b fires nothing either).
    ok(&srv, &["select-pane", "-t", &first]);
    quiet(&log);
    unbind(&srv, "pane-focused");

    // after-resize-pane: once, though both panes change size.
    bind(&srv, "pane-resized", &log);
    ok(&srv, &["resize-pane", "-t", &first, "-D", "2"]);
    assert_eq!(once(&log), format!("pane-resized {first} main {first}"));
    unbind(&srv, "pane-resized");

    // after-new-window.
    bind(&srv, "window-created", &log);
    ok(&srv, &["new-window", "-d", "-t", "main"]);
    let created = pane_of(&srv, "main:2");
    assert_eq!(
        once(&log),
        format!("window-created {created} main {created}")
    );
    unbind(&srv, "window-created");

    // after-select-window.
    bind(&srv, "window-focused", &log);
    ok(&srv, &["select-window", "-t", "main:2"]);
    assert_eq!(
        once(&log),
        format!("window-focused {created} main {created}")
    );
    unbind(&srv, "window-focused");

    // after-kill-pane: `#{hook_pane}` is the pane that went, `#{pane_id}`
    // the one left active in its window (as tmux 3.7b).
    bind(&srv, "pane-closed", &log);
    ok(&srv, &["kill-pane", "-t", &second]);
    assert_eq!(once(&log), format!("pane-closed {first} main {second}"));
    unbind(&srv, "pane-closed");
}

/// Case 1, the notify-shaped events: sessions coming and going and panes
/// exiting.
#[test]
fn session_and_pane_exit_events_run_their_hook_once() {
    let (srv, log) = start();

    // session-created.
    bind(&srv, "session-created", &log);
    ok(&srv, &["new-session", "-d", "-s", "other"]);
    let other = pane_of(&srv, "other");
    assert_eq!(once(&log), format!("session-created {other} other {other}"));
    unbind(&srv, "session-created");

    // session-closed: the session is gone, so its name is
    // `#{hook_session_name}`.
    let cmd = format!(
        "run-shell 'echo closed #{{hook_session_name}} >> {}'",
        log.display()
    );
    ok(&srv, &["set-hook", "-g", "session-closed", &cmd]);
    ok(&srv, &["kill-session", "-t", "other"]);
    assert_eq!(once(&log), "closed other");
    unbind(&srv, "session-closed");

    // pane-died: the harness runs with remain-on-exit on.
    bind(&srv, "pane-died", &log);
    ok(&srv, &["split-window", "-t", "main", "exit 3"]);
    let line = once(&log);
    let died = ok(
        &srv,
        &["list-panes", "-t", "main", "-F", "#{pane_id} #{pane_dead}"],
    )
    .lines()
    .find(|l| l.ends_with(" 1"))
    .and_then(|l| l.split(' ').next().map(str::to_string))
    .expect("a dead pane");
    assert_eq!(line, format!("pane-died {died} main {died}"));
    unbind(&srv, "pane-died");

    // pane-exited: with remain-on-exit off the pane goes.
    ok(&srv, &["set", "-g", "remain-on-exit", "off"]);
    let cmd = format!(
        "run-shell 'echo exited #{{hook_pane}} #{{session_name}} >> {}'",
        log.display()
    );
    ok(&srv, &["set-hook", "-g", "pane-exited", &cmd]);
    ok(&srv, &["split-window", "-t", "main", "sleep 0.5"]);
    let exiting = pane_of(&srv, "main");
    assert_eq!(once(&log), format!("exited {exiting} main"));
}

/// Case 1, the client events: an attached client resizing (and its
/// window with it) and switching sessions.
#[test]
fn client_events_run_their_hook_once() {
    let (srv, log) = start();
    ok(&srv, &["new-session", "-d", "-s", "other"]);
    let mut client = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    wait_until(|| ok(&srv, &["list-clients"]).lines().count() == 1, WAIT).expect("attached");
    let main_pane = pane_of(&srv, "main");
    // The window follows the client's size first.
    std::thread::sleep(QUIET);

    // client-resized and after-resize-window.
    bind(&srv, "client-resized", &log);
    let window_log = srv.home.join("window.log");
    bind(&srv, "window-resized", &window_log);
    client.resize(100, 30);
    assert_eq!(
        once(&log),
        format!("client-resized {main_pane} main {main_pane}")
    );
    assert_eq!(
        once(&window_log),
        format!("window-resized {main_pane} main {main_pane}")
    );
    unbind(&srv, "client-resized");
    unbind(&srv, "window-resized");

    // client-session-changed.
    bind(&srv, "client-session-changed", &log);
    ok(&srv, &["switch-client", "-t", "other"]);
    let other = pane_of(&srv, "other");
    assert_eq!(
        once(&log),
        format!("client-session-changed {other} other {other}")
    );
}

/// A command-shaped hook runs before its command's reply (tmux 3.7b's
/// `after-*` hooks hold the CLI), so the next command sees what it did.
#[test]
fn a_command_hook_has_run_when_its_command_returns() {
    let (srv, _) = start();
    let first = pane_of(&srv, "main");
    ok(&srv, &["split-window", "-t", "main"]);
    ok(
        &srv,
        &[
            "set-hook",
            "-g",
            "pane-focused",
            "set -g @focused #{hook_pane}",
        ],
    );
    ok(&srv, &["select-pane", "-t", &first]);
    assert_eq!(display(&srv, "main", "#{@focused}"), first);
}

/// Case 2: a hook removed with `set-hook -gu` does not run again, and
/// `show-hooks` lists what is bound.
#[test]
fn an_unset_hook_stops_running() {
    let (srv, log) = start();
    let first = pane_of(&srv, "main");
    ok(&srv, &["split-window", "-t", "main"]);
    let second = pane_of(&srv, "main");
    bind(&srv, "pane-focused", &log);
    let shown = ok(&srv, &["show-hooks", "-g"]);
    assert!(
        shown.starts_with("pane-focused run-shell 'echo pane-focused"),
        "{shown}"
    );
    ok(&srv, &["select-pane", "-t", &first]);
    once(&log);

    unbind(&srv, "pane-focused");
    assert_eq!(ok(&srv, &["show-hooks", "-g"]), "");
    ok(&srv, &["select-pane", "-t", &second]);
    ok(&srv, &["select-pane", "-t", &first]);
    quiet(&log);
}

/// Case 3: a hook running `run-shell -b 'sleep 5'` delays neither its own
/// command nor the next one.
#[test]
fn a_background_run_shell_hook_does_not_delay_the_next_command() {
    let (srv, _) = start();
    let first = pane_of(&srv, "main");
    ok(&srv, &["split-window", "-t", "main"]);
    ok(
        &srv,
        &["set-hook", "-g", "pane-focused", "run-shell -b 'sleep 5'"],
    );
    let started = Instant::now();
    ok(&srv, &["select-pane", "-t", &first]);
    ok(&srv, &["list-sessions"]);
    let took = started.elapsed();
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    // And `run-shell -b` from the CLI answers at once, with no output.
    let started = Instant::now();
    assert_eq!(ok(&srv, &["run-shell", "-b", "sleep 5"]), "");
    assert!(started.elapsed() < Duration::from_millis(1500));
}

/// The environment a new pane starts with, written to a file by the pane
/// itself.
fn new_pane_env(srv: &TestServer, name: &str) -> Vec<String> {
    let out = srv.home.join(format!("{name}.env"));
    let script = format!("env > {}.tmp; mv {0}.tmp {0}; sleep 30", out.display());
    ok(srv, &["new-window", "-d", "-t", "main", &script]);
    wait_until(|| out.exists(), WAIT).expect("the pane wrote its env");
    fs::read_to_string(&out)
        .expect("read env")
        .lines()
        .map(str::to_string)
        .collect()
}

/// Cases 4 and 5: a global variable reaches a new pane and `run-shell`;
/// `-gu` removes it from both.
#[test]
fn a_global_variable_reaches_new_panes_and_run_shell_until_unset() {
    let (srv, _) = start();
    ok(&srv, &["set-environment", "-g", "GROFE_TC5_FOO", "bar"]);
    assert_eq!(
        ok(&srv, &["show-environment", "-g", "GROFE_TC5_FOO"]),
        "GROFE_TC5_FOO=bar\n"
    );
    assert!(new_pane_env(&srv, "set").contains(&"GROFE_TC5_FOO=bar".to_string()));
    assert_eq!(ok(&srv, &["run-shell", "echo $GROFE_TC5_FOO"]), "bar\n");

    ok(&srv, &["set-environment", "-gu", "GROFE_TC5_FOO"]);
    let env = new_pane_env(&srv, "unset");
    assert!(
        !env.iter().any(|l| l.starts_with("GROFE_TC5_FOO=")),
        "{env:?}"
    );
    assert_eq!(ok(&srv, &["run-shell", "echo x$GROFE_TC5_FOO"]), "x\n");
    assert_eq!(
        srv.cmd(&["show-environment", "-g", "GROFE_TC5_FOO"]),
        Err("unknown variable: GROFE_TC5_FOO\n".to_string())
    );
}

/// A session variable reaches that session's new panes and `run-shell`.
#[test]
fn a_session_variable_reaches_its_panes_and_run_shell() {
    let (srv, _) = start();
    ok(
        &srv,
        &["set-environment", "-t", "main", "GROFE_TC5_SESS", "s1"],
    );
    assert!(ok(&srv, &["show-environment", "-t", "main"]).contains("GROFE_TC5_SESS=s1\n"));
    assert!(new_pane_env(&srv, "session").contains(&"GROFE_TC5_SESS=s1".to_string()));
    // From inside the session (a CLI without a pane picks the most recent
    // session, as tmux).
    assert_eq!(ok(&srv, &["run-shell", "echo $GROFE_TC5_SESS"]), "s1\n");
}

/// Case 6: a hidden variable reaches `$NAME` in a command string (here a
/// hook's, split when it is set, as tmux 3.7b does) but no process: not a
/// new pane, and not `run-shell`'s shell either (recorded on tmux 3.7b:
/// `set-environment -gh SECRET x; run-shell 'echo $SECRET'` prints an
/// empty line).
#[test]
fn a_hidden_variable_reaches_command_strings_and_not_a_new_pane() {
    let (srv, log) = start();
    let first = pane_of(&srv, "main");
    ok(&srv, &["split-window", "-t", "main"]);
    ok(&srv, &["set-environment", "-gh", "GROFE_TC5_SECRET", "x"]);
    assert_eq!(
        ok(&srv, &["show-environment", "-gh"]),
        "GROFE_TC5_SECRET=x\n"
    );
    assert!(!ok(&srv, &["show-environment", "-g"]).contains("GROFE_TC5_SECRET"));

    let cmd = format!(
        "run-shell \"echo secret:$GROFE_TC5_SECRET >> {}\"",
        log.display()
    );
    ok(&srv, &["set-hook", "-g", "pane-focused", &cmd]);
    ok(&srv, &["select-pane", "-t", &first]);
    assert_eq!(once(&log), "secret:x");

    let env = new_pane_env(&srv, "hidden");
    assert!(
        !env.iter().any(|l| l.starts_with("GROFE_TC5_SECRET=")),
        "{env:?}"
    );
    assert_eq!(ok(&srv, &["run-shell", "echo x$GROFE_TC5_SECRET"]), "x\n");
}

/// TC.3's ponytail, closed: `$NAME` in a stored command string expands
/// from the global environment, not the server process's.
#[test]
fn command_strings_expand_variables_from_the_global_environment() {
    let (srv, _) = start();
    ok(&srv, &["set-environment", "-g", "GROFE_TC5_WORD", "hello"]);
    ok(
        &srv,
        &[
            "bind-key",
            "-T",
            "root",
            "F9",
            "display-message \"$GROFE_TC5_WORD\"",
        ],
    );
    let keys = ok(&srv, &["list-keys", "-T", "root"]);
    assert!(
        keys.lines()
            .any(|l| l.contains("F9") && l.contains("display-message hello")),
        "{keys}"
    );
}
