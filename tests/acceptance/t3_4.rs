//! T3.4: the command prompt and `run-shell`. `prefix :` opens a prompt on
//! the client's status row with emacs line editing and a server-wide
//! history; Enter runs the line as a binding would; `command-prompt` takes
//! `-p`, `-I` and a `%%` template (R24); `run-shell` never blocks the
//! server, answers the CLI with its stdout and runs in the CLI's cwd from
//! the CLI and the session's cwd from a binding (R23).

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// A server with one session `main` running `cat`, sized for an 80x24
/// client with a status line.
fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "cat",
    ])
    .expect("new-session");
    bind_colon(&srv);
    srv
}

/// tmux's stock `prefix :`. The brief expected T2.5b's default table to
/// have it, but `input::default_bindings` does not (reported), so the
/// tests bind it the way a user would.
fn bind_colon(srv: &TestServer) {
    srv.cmd(&["bind-key", ":", "command-prompt"])
        .expect("bind-key :");
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_until("the status row", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
    a
}

fn last_row(text: &str) -> &str {
    text.lines().last().unwrap_or_default()
}

/// Waits until the client's last row is `want` (trailing blanks ignored).
fn wait_last_row(a: &mut Harness, want: &str) {
    a.wait_until(&format!("last row {want:?}"), WAIT, |text| {
        last_row(text).trim_end() == want
    });
}

/// `(pane_left, pane_top)` of each pane of `main`'s current window.
fn pane_origins(srv: &TestServer) -> Vec<(u16, u16)> {
    srv.cmd(&["list-panes", "-t", "main", "-F", "#{pane_left} #{pane_top}"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (x, y) = l.split_once(' ')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        })
        .collect()
}

fn windows(srv: &TestServer) -> usize {
    srv.cmd(&["list-windows", "-t", "main"])
        .unwrap_or_default()
        .lines()
        .count()
}

/// A directory made canonical, so it compares equal to what `pwd` prints
/// (macOS temp dirs sit behind the `/var` symlink).
fn canonical_dir(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("create dir");
    fs::canonicalize(path).expect("canonicalize")
}

#[test]
fn prefix_colon_runs_a_command_typed_in_the_prompt() {
    let srv = start();
    let mut a = attach(&srv);
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    a.send("split-window -h");
    wait_last_row(&mut a, ":split-window -h");
    a.send("\r");
    wait_until(|| pane_origins(&srv).len() == 2, WAIT).unwrap_or_else(|e| panic!("no split: {e}"));
    let origins = pane_origins(&srv);
    assert_eq!(origins[0], (0, 0), "{origins:?}");
    assert!(
        origins[1].0 > 0 && origins[1].1 == 0,
        "side by side: {origins:?}"
    );
    a.wait_until("the prompt to go", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
}

/// R1: a prompt belongs to one client. The other client's status row stays
/// and its keys still reach the pane.
#[test]
fn a_prompt_leaves_other_clients_alone() {
    let srv = start();
    let mut a = attach(&srv);
    let mut b = attach(&srv);
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    b.send("typed-in-b\r");
    wait_until(|| srv.capture("main").contains("typed-in-b"), WAIT)
        .unwrap_or_else(|e| panic!("b's keys: {e}\n{}", srv.capture("main")));
    assert!(last_row(&b.text()).starts_with("[main]"), "{}", b.text());
    // a's prompt is still open and empty.
    wait_last_row(&mut a, ":");
    a.send("x");
    wait_last_row(&mut a, ":x");
    assert!(last_row(&b.text()).starts_with("[main]"), "{}", b.text());
}

/// Emacs editing: C-w deletes back to a word separator as tmux does (its
/// default `word-separators` holds `-`, so `-v` takes two), and Up recalls
/// the last command run.
#[test]
fn line_editing_and_history() {
    let srv = start();
    let mut a = attach(&srv);
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    a.send("split-window -v");
    wait_last_row(&mut a, ":split-window -v");
    a.send("\x17");
    wait_last_row(&mut a, ":split-window -");
    a.send("\x17");
    wait_last_row(&mut a, ":split-window");
    // C-a, C-k, then retype, going back over `window -h` with Left.
    a.send("\x01\x0b");
    wait_last_row(&mut a, ":");
    a.send("splitwindow -h");
    a.send("\x1b[D".repeat(9) + "-");
    wait_last_row(&mut a, ":split-window -h");
    a.send("\r");
    wait_until(|| pane_origins(&srv).len() == 2, WAIT).unwrap_or_else(|e| panic!("no split: {e}"));
    let origins = pane_origins(&srv);
    assert!(origins[1].0 > 0 && origins[1].1 == 0, "{origins:?}");

    a.wait_until("the prompt to go", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    a.send("\x1b[A");
    wait_last_row(&mut a, ":split-window -h");
    a.send("\x1b[B");
    wait_last_row(&mut a, ":");
    a.send("\x03");
    a.wait_until("the prompt to go", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
}

/// With `status off` the prompt takes the last row, as tmux draws it.
#[test]
fn the_prompt_takes_the_last_row_with_status_off() {
    let srv = start();
    let mut a = attach(&srv);
    srv.cmd(&["set", "-g", "status", "off"])
        .expect("status off");
    a.wait_until("the status row gone", WAIT, |text| {
        !last_row(text).starts_with("[main]")
    });
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    a.send("\x1b");
    a.wait_until("the prompt gone", WAIT, |text| {
        !last_row(text).starts_with(':')
    });
}

#[test]
fn escape_cancels_the_prompt() {
    let srv = start();
    let mut a = attach(&srv);
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    a.send("new-window");
    wait_last_row(&mut a, ":new-window");
    a.send("\x1b");
    a.wait_until("the status row back", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
    // Escape was eaten, and nothing ran.
    assert_eq!(windows(&srv), 1);
    a.send("zz\r");
    wait_until(|| srv.capture("main").contains("zz"), WAIT)
        .unwrap_or_else(|e| panic!("keys after cancel: {e}"));
    assert_eq!(windows(&srv), 1);
}

#[test]
fn a_failing_prompt_command_shows_its_error() {
    let srv = start();
    let mut a = attach(&srv);
    a.send("\x02:");
    wait_last_row(&mut a, ":");
    a.send("no-such-command\r");
    a.wait_until("the error", WAIT, |text| {
        last_row(text).contains("no-such-command")
    });
    a.wait_until("the status row back", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
}

#[test]
fn a_binding_runs_a_shell_command() {
    let srv = start();
    let file = srv.home.join("x");
    let script = format!("touch '{}'", file.display());
    srv.cmd(&["bind-key", "X", "run-shell", &script])
        .expect("bind-key");
    let mut a = attach(&srv);
    a.send("\x02X");
    wait_until(|| file.exists(), Duration::from_secs(2)).unwrap_or_else(|e| panic!("C-b X: {e}"));
}

#[test]
fn run_shell_answers_with_stdout_and_fails_on_nonzero_exit() {
    let srv = start();
    let out = srv.run(&["run-shell", "echo hi; echo two"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, "hi\ntwo\n");

    let out = srv.run(&["run-shell", "false"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(out.stderr.trim_end(), "'false' returned 1");

    // stderr is not the reply.
    let out = srv.run(&["run-shell", "echo out; echo err >&2"]);
    assert_eq!(out.stdout, "out\n", "{out:?}");
}

/// D8: a long `run-shell` holds its own CLI, not the server.
#[test]
fn run_shell_does_not_block_other_commands() {
    let srv = start();
    let mut slow = srv.command();
    slow.args(["run-shell", "sleep 5"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut slow = slow.spawn().expect("spawn run-shell");
    // Let the slow request reach the server first.
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let out = srv.run(&["list-sessions"]);
    let took = started.elapsed();
    let still_running = slow.try_wait().ok().flatten().is_none();
    let _ = slow.kill();
    let _ = slow.wait();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.starts_with("main:"), "{out:?}");
    assert!(took < Duration::from_secs(1), "list-sessions took {took:?}");
    assert!(still_running, "run-shell answered before its command ended");
}

/// R24: `-I` prefills the prompt; the answer goes into the template at
/// `%%`.
#[test]
fn command_prompt_template_and_initial_text() {
    let srv = start();
    let mut a = attach(&srv);
    srv.cmd(&["command-prompt", "-I", "old", "rename-window \"%%\""])
        .expect("command-prompt");
    wait_last_row(&mut a, "(rename-window) old");
    a.send("\r");
    wait_until(
        || {
            srv.cmd(&["display-message", "-p", "-t", "main", "#{window_name}"])
                .is_ok_and(|n| n == "old\n")
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("rename: {e}"));

    // -p sets the prompt; -I is a format.
    srv.cmd(&["command-prompt", "-p", "name", "-I", "#{window_name}!"])
        .expect("command-prompt -p");
    wait_last_row(&mut a, "name old!");
    a.send("\x03");
    a.wait_until("the status row back", WAIT, |text| {
        last_row(text).starts_with("[main]")
    });
}

/// R23: from the CLI `run-shell` runs in the CLI's cwd; from a binding, in
/// the session's.
#[test]
fn run_shell_cwd_follows_the_caller() {
    let srv = TestServer::start();
    let session_dir = canonical_dir(&srv.home.join("session-dir"));
    let cli_dir = canonical_dir(&srv.home.join("cli-dir"));
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "-c",
        session_dir.to_str().expect("utf-8 path"),
        "cat",
    ])
    .expect("new-session");

    let mut cli = srv.command();
    cli.args(["run-shell", "pwd"]).current_dir(&cli_dir);
    let out = common::run_bounded(cli, common::CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout.trim_end(), cli_dir.to_str().expect("utf-8"));

    let file = srv.home.join("out");
    let script = format!("pwd > '{}'", file.display());
    srv.cmd(&["bind-key", "Y", "run-shell", &script])
        .expect("bind-key");
    let mut a = attach(&srv);
    a.send("\x02Y");
    wait_until(
        || fs::read_to_string(&file).is_ok_and(|s| s.ends_with('\n')),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("C-b Y: {e}"));
    let written = fs::read_to_string(&file).expect("out");
    assert_eq!(written.trim_end(), session_dir.to_str().expect("utf-8"));
}
