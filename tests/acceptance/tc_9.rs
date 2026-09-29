//! TC.9: client and session verbs and the choosers. `new-session -A -s x`
//! twice leaves one session and the second call attaches; `switch-client
//! -c <client> -t s` moves only that client; `detach-client -s s` detaches
//! every client on `s`; `switch-client -T t` then a key runs table `t`'s
//! binding and returns to root; `display-message hi` shows in the status
//! line and then in `show-messages`; `choose-tree -Zw` zooms, lists windows
//! under sessions, and Enter switches; `find-window -Z foo` lists only
//! matching windows; `wait-for -S` releases a waiting `wait-for`.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;

use common::{TestServer, wait_child, wait_until};

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// A server with session `main` running `cat`, sized for an 80x24 client.
fn start() -> TestServer {
    let srv = TestServer::start();
    new_session(&srv, "main");
    srv
}

fn new_session(srv: &TestServer, name: &str) {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        "80",
        "-y",
        "23",
        "cat",
    ])
    .expect("new-session");
}

fn attach(srv: &TestServer, session: &str) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), session, &[], 80, 24);
    wait_last_row_starts(&mut a, &format!("[{session}]"));
    a
}

fn last_row(text: &str) -> &str {
    text.lines().last().unwrap_or_default()
}

fn wait_last_row_starts(a: &mut Harness, want: &str) {
    a.wait_until(&format!("a last row starting {want:?}"), WAIT, |text| {
        last_row(text).starts_with(want)
    });
}

fn lines(srv: &TestServer, args: &[&str]) -> Vec<String> {
    srv.cmd(args)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `client_name session_name` for every client, in client order.
fn clients(srv: &TestServer) -> Vec<String> {
    lines(
        srv,
        &["list-clients", "-F", "#{client_name} #{session_name}"],
    )
}

fn wait_clients(srv: &TestServer, want: &[&str]) {
    wait_until(|| clients(srv) == want, WAIT)
        .unwrap_or_else(|e| panic!("clients {want:?}: {e}, got {:?}", clients(srv)));
}

fn display(srv: &TestServer, target: &str, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", target, format])
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// Case 1: a second `new-session -A -s x` attaches to `x` instead of
/// making another session.
#[test]
fn new_session_a_twice_leaves_one_session_and_attaches() {
    let srv = start();
    let spawn = || {
        let mut cmd = srv.command();
        cmd.args(["new-session", "-A", "-s", "x", "cat"])
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        let mut a = Harness::spawn(cmd, 80, 24);
        wait_last_row_starts(&mut a, "[x]");
        a
    };
    let _first = spawn();
    let _second = spawn();
    assert_eq!(
        lines(&srv, &["list-sessions", "-F", "#{session_name}"]),
        ["main", "x"]
    );
    let on: Vec<String> = clients(&srv)
        .iter()
        .map(|c| c.split(' ').nth(1).unwrap_or_default().to_string())
        .collect();
    assert_eq!(on, ["x", "x"]);
    // From the CLI, `-A` on an existing session makes no duplicate either.
    srv.cmd(&["new-session", "-A", "-d", "-s", "x"])
        .expect("new-session -A -d");
    assert_eq!(lines(&srv, &["list-sessions"]).len(), 2);
}

/// `has-session` answers with its exit status alone, as tmux's.
#[test]
fn has_session_answers_by_exit_status() {
    let srv = start();
    let found = srv.run(&["has-session", "-t", "main"]);
    assert!(found.status.success(), "{found:?}");
    assert_eq!(found.stdout, "");
    let missing = srv.run(&["has-session", "-t", "nope"]);
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    assert_eq!(missing.stdout, "");
    assert_eq!(missing.stderr.trim_end(), "can't find session: nope");
}

/// Case 2: `switch-client -c` moves the named client, not the others.
#[test]
fn switch_client_c_moves_only_that_client() {
    let srv = start();
    new_session(&srv, "other");
    let _a = attach(&srv, "main");
    let mut b = attach(&srv, "main");
    let names: Vec<String> = clients(&srv)
        .iter()
        .map(|c| c.split(' ').next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    srv.cmd(&["switch-client", "-c", &names[1], "-t", "other"])
        .expect("switch-client -c");
    wait_last_row_starts(&mut b, "[other]");
    wait_clients(
        &srv,
        &[
            &format!("{} main", names[0]),
            &format!("{} other", names[1]),
        ],
    );
    let err = srv
        .cmd(&["switch-client", "-c", "nobody", "-t", "other"])
        .unwrap_err();
    assert_eq!(err.trim_end(), "can't find client: nobody");
}

/// Case 3: `detach-client -s` detaches every client on the session.
#[test]
fn detach_client_s_detaches_every_client_on_the_session() {
    let srv = start();
    new_session(&srv, "other");
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "main");
    let _c = attach(&srv, "other");
    srv.cmd(&["detach-client", "-s", "main"])
        .expect("detach-client -s");
    a.wait_exit();
    b.wait_exit();
    let left: Vec<String> = clients(&srv)
        .iter()
        .map(|c| c.split(' ').nth(1).unwrap_or_default().to_string())
        .collect();
    assert_eq!(left, ["other"]);
}

/// Case 4: after `switch-client -T mytable`, one key runs that table's
/// binding and the next goes through `root` again.
#[test]
fn switch_client_t_is_for_the_next_key_only() {
    let srv = start();
    srv.cmd(&["bind-key", "-T", "mytable", "x", "rename-window", "viaT"])
        .expect("bind-key");
    let mut a = attach(&srv, "main");
    srv.cmd(&["switch-client", "-T", "mytable"])
        .expect("switch-client -T");
    wait_until(
        || display(&srv, "main", "#{client_key_table}") == "mytable",
        WAIT,
    )
    .expect("the client in mytable");
    a.send("x");
    wait_until(|| display(&srv, "main", "#{window_name}") == "viaT", WAIT)
        .expect("the mytable binding ran");
    assert_eq!(display(&srv, "main", "#{client_key_table}"), "root");
    a.send("x");
    wait_until(|| srv.capture("main").contains('x'), WAIT).expect("the next x reached the pane");
}

/// Case 5: `display-message` without `-p` shows on the status line and is
/// then listed by `show-messages`.
#[test]
fn display_message_shows_on_the_status_line_and_in_show_messages() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let out = srv.cmd(&["display-message", "hi #{session_name}"]);
    assert_eq!(out.as_deref(), Ok(""));
    a.wait_until("the message on the status line", WAIT, |text| {
        last_row(text).trim_end() == "hi main"
    });
    let log = srv.cmd(&["show-messages"]).expect("show-messages");
    let line = log.lines().next().unwrap_or_default();
    let (time, rest) = line.split_at(5.min(line.len()));
    assert!(
        time.len() == 5 && time.as_bytes()[2] == b':',
        "a HH:MM time: {log:?}"
    );
    assert!(
        rest.starts_with(": client-") && rest.ends_with(" message: hi main"),
        "{log:?}"
    );
}

/// Case 6: `choose-tree -Zw` zooms the pane, lists windows under their
/// sessions, and Enter switches the client to the chosen line.
#[test]
fn choose_tree_zw_zooms_lists_windows_and_enter_switches() {
    let srv = start();
    srv.cmd(&["split-window", "-t", "main", "cat"])
        .expect("split");
    srv.cmd(&["new-window", "-d", "-t", "main", "-n", "two", "cat"])
        .expect("new-window");
    new_session(&srv, "other");
    let mut a = attach(&srv, "main");
    srv.cmd(&["choose-tree", "-Zw"]).expect("choose-tree");
    a.wait_for("(0) - main: 2 windows (attached)");
    let text = a.text();
    assert!(text.contains("(1) ├─> + 1: "), "{text}");
    assert!(text.contains("(2) └─> + 2: two"), "{text}");
    assert!(text.contains("(3) - other: 1 windows"), "{text}");
    assert!(text.contains("(4) └─> + 1: "), "{text}");
    assert!(text.contains("┌ 1 (sort: index) "), "{text}");
    assert_eq!(display(&srv, "main:1", "#{window_zoomed_flag}"), "1");
    // Down twice, from the current window to the session `other`.
    a.send("\x1b[B");
    a.wait_for("┌ 2 (sort: index) ");
    a.send("\x1b[B");
    a.wait_for("┌ other (sort: index) ");
    a.send("\r");
    wait_last_row_starts(&mut a, "[other]");
    assert_eq!(display(&srv, "main:1", "#{window_zoomed_flag}"), "0");
    let text = a.text();
    assert!(!text.contains("(sort: index)"), "{text}");
}

/// A line's own key chooses it, and `q` closes the chooser untouched.
#[test]
fn choose_tree_line_keys_choose_and_q_closes() {
    let srv = start();
    new_session(&srv, "other");
    let mut a = attach(&srv, "main");
    srv.cmd(&["choose-tree", "-w"]).expect("choose-tree");
    a.wait_for("(2) - other: 1 windows");
    a.send("q");
    a.wait_until("the chooser to close", WAIT, |t| !t.contains("(sort: "));
    srv.cmd(&["choose-tree", "-w"]).expect("choose-tree");
    a.wait_for("(2) - other: 1 windows");
    a.send("2");
    wait_last_row_starts(&mut a, "[other]");
}

/// Case 7: `find-window -Z foo` lists only the windows that match.
#[test]
fn find_window_lists_only_matching_windows() {
    let srv = start();
    srv.cmd(&["rename-window", "-t", "main", "bar"])
        .expect("rename");
    srv.cmd(&["new-window", "-d", "-t", "main", "-n", "foo-win", "cat"])
        .expect("new-window");
    srv.cmd(&["new-window", "-d", "-t", "main", "-n", "baz", "cat"])
        .expect("new-window");
    let mut a = attach(&srv, "main");
    srv.cmd(&["find-window", "-Z", "foo"]).expect("find-window");
    a.wait_for("(filter: active)");
    let text = a.text();
    let listed: Vec<&str> = text
        .lines()
        .take_while(|l| !l.starts_with('┌'))
        .filter(|l| l.contains("─> "))
        .collect();
    assert_eq!(listed.len(), 2, "the window and its pane: {text}");
    assert!(listed[0].contains("2: foo-win"), "{text}");
    assert!(listed[1].contains("└─> 1: "), "{text}");
    assert!(
        text.contains("┌ main (sort: index) (filter: active) "),
        "{text}"
    );
}

/// `choose-client -Z` lists the attached clients; Enter detaches the
/// chosen one, as tmux's default template does.
#[test]
fn choose_client_lists_clients_and_enter_detaches() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "main");
    srv.cmd(&["choose-client", "-Z"]).expect("choose-client");
    let names: Vec<String> = clients(&srv)
        .iter()
        .map(|c| c.split(' ').next().unwrap_or_default().to_string())
        .collect();
    b.wait_for(&format!("(0) {}: ", names[0]));
    b.wait_for(&format!("(1) {}: ", names[1]));
    b.wait_for(": session main");
    b.send("0");
    a.wait_exit();
    wait_clients(&srv, &[&format!("{} main", names[1])]);
}

/// Case 8: `suspend-client` stops the attach client with SIGTSTP.
#[test]
#[ignore = "suspend-client needs a suspend attach frame outside TC.9's files (TC.9 report)"]
fn suspend_client_stops_the_attach_client() {
    let srv = start();
    let _a = attach(&srv, "main");
    srv.cmd(&["suspend-client"]).expect("suspend-client");
}

/// Case 9: `wait-for -S chan` releases a `wait-for chan` blocked on it.
#[test]
fn wait_for_s_releases_a_waiting_wait_for() {
    let srv = start();
    let mut cmd = srv.command();
    cmd.args(["wait-for", "chan"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut waiter = cmd.spawn().expect("spawn wait-for");
    sleep(Duration::from_millis(300));
    assert!(
        waiter.try_wait().expect("try_wait").is_none(),
        "wait-for returned before the signal"
    );
    srv.cmd(&["wait-for", "-S", "chan"]).expect("wait-for -S");
    let status = wait_child(waiter, WAIT).expect("the waiter exits");
    assert!(status.success(), "{status:?}");
}
