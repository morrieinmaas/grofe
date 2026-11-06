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
    // With no terminal, `-A` on an existing session fails as tmux 3.7b's
    // does, `-d` or not, and makes no duplicate.
    let out = srv.run(&["new-session", "-A", "-d", "-s", "x"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        out.stderr.trim_end(),
        "open terminal failed: not a terminal"
    );
    assert_eq!(lines(&srv, &["list-sessions"]).len(), 2);
    // A new name with `-A -d` is made detached.
    srv.cmd(&["new-session", "-A", "-d", "-s", "z"])
        .expect("new-session -A -d");
    assert_eq!(lines(&srv, &["list-sessions"]).len(), 3);
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
    // An unknown session is no error, as in tmux 3.7b (recorded).
    let out = srv.run(&["detach-client", "-s", "nope"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stderr, "");
    assert_eq!(clients(&srv).len(), 1);
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
    // Newest first: `show-messages` itself, logged as a command (FE.2),
    // then the message.
    let mut lines = log.lines();
    let first = lines.next().unwrap_or_default();
    assert!(first.ends_with(" command: show-messages"), "{log:?}");
    let line = lines.next().unwrap_or_default();
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

/// `display-message -d` sets how long the message shows, 0 until a key,
/// and the cursor hides while it shows, as tmux 3.7b's (recorded).
#[test]
fn display_message_d_sets_the_delay_and_hides_the_cursor() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let from = a.output().len();
    srv.cmd(&["display-message", "-d", "0", "held"])
        .expect("display-message -d 0");
    a.wait_until("the message", WAIT, |t| last_row(t).trim_end() == "held");
    a.wait_for_output(b"\x1b[?25l", from);
    // Past `display-time` (750 ms by default) it still shows.
    sleep(Duration::from_millis(1500));
    assert_eq!(last_row(&a.text()).trim_end(), "held");
    a.send("x");
    wait_last_row_starts(&mut a, "[main]");
    for (delay, want) in [
        ("nope", "delay invalid"),
        ("-1", "delay too small"),
        ("4294967296", "delay too large"),
    ] {
        let err = srv.cmd(&["display-message", "-d", delay, "x"]).unwrap_err();
        assert_eq!(err.trim_end(), want);
    }
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

/// `find-window` matches pane text with a `*pattern*` glob per line, as
/// tmux's `#{C:}`, and takes the pattern as text: a `,` in it is no
/// format syntax.
#[test]
fn find_window_globs_the_text_and_escapes_the_pattern() {
    let srv = start();
    srv.cmd(&[
        "new-window",
        "-d",
        "-t",
        "main",
        "-n",
        "one",
        "printf 'hello fxo there\\n'; exec cat",
    ])
    .expect("new-window");
    srv.cmd(&["new-window", "-d", "-t", "main", "-n", "a,b", "cat"])
        .expect("new-window");
    let mut a = attach(&srv, "main");
    // Each entry, with the screen column it starts at.
    let listed = |a: &mut Harness, pattern: &str| -> Vec<(usize, String)> {
        srv.cmd(&["find-window", "-Z", pattern])
            .expect("find-window");
        a.wait_for("(filter: ");
        let text = a.text();
        a.send("q");
        a.wait_until("the chooser closed", WAIT, |t| !t.contains("(filter: "));
        text.lines()
            .take_while(|l| !l.starts_with('┌'))
            .filter(|l| l.contains("─> - "))
            .filter_map(|l| {
                let (before, entry) = l.split_once("─> - ")?;
                let at = before.chars().count() + "─> - ".chars().count();
                Some((at, entry.trim().to_string()))
            })
            .collect()
    };
    // A one-pane window shows its pane's title, which starts as the full
    // host name, unless that is `host_short`, as in tmux: so a dotted host
    // name (a container's `localhost.localdomain`) adds `: "<host>"`.
    let host = srv
        .cmd(&["display-message", "-p", "#{host}"])
        .expect("host");
    let host = host.trim_end();
    let short = host.split('.').next().unwrap_or(host);
    // The row ends at the screen's edge, so a long host name (a macOS
    // runner's) is cut there, as tmux cuts it.
    let titled = |name: &str, at: usize| {
        let full = if host.is_empty() || host == short {
            name.to_string()
        } else {
            format!("{name}: \"{host}\"")
        };
        let shown: String = full.chars().take(80usize.saturating_sub(at)).collect();
        (at, shown.trim_end().to_string())
    };
    let one = listed(&mut a, "f?o");
    assert_eq!(one.len(), 1, "{one:?}");
    assert_eq!(one, [titled("2: one", one[0].0)]);
    let ab = listed(&mut a, "a,b");
    assert_eq!(ab.len(), 1, "{ab:?}");
    assert_eq!(ab, [titled("3: a,b", ab[0].0)]);
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

/// Case 8: `suspend-client` stops the attach client with SIGTSTP, so the
/// interactive shell that ran it reports a stopped job; `fg` resumes it
/// and the screen is drawn again.
#[test]
fn suspend_client_stops_the_client_and_fg_resumes_it() {
    let srv = start();
    let grofe = srv.command();
    let mut line = format!("'{}'", grofe.get_program().to_string_lossy());
    for arg in grofe.get_args() {
        line.push_str(&format!(" '{}'", arg.to_string_lossy()));
    }
    line.push_str(" attach -t main; echo \"stopped=$?\"\n");
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-i")
        .env("PS1", "$ ")
        .env("ENV", "")
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    for (key, value) in grofe.get_envs() {
        match value {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
    if let Some(dir) = grofe.get_current_dir() {
        cmd.current_dir(dir);
    }
    let mut a = Harness::spawn(cmd, 80, 24);
    a.wait_for("$ ");
    a.send(line);
    wait_last_row_starts(&mut a, "[main]");
    srv.cmd(&["suspend-client"]).expect("suspend-client");
    // The shell's report of a job stopped by SIGTSTP: 128 plus its number.
    let stopped = format!("stopped={}", 128 + nix::libc::SIGTSTP);
    a.wait_for(&stopped);
    assert_eq!(clients(&srv).len(), 1, "a suspended client stays attached");
    a.send("fg\n");
    a.wait_until("the client drawn again", WAIT, |t| {
        last_row(t).starts_with("[main]") && !t.contains(&stopped)
    });
    srv.cmd(&["send-keys", "-t", "main", "back"])
        .expect("send-keys");
    a.wait_for("back");
    srv.cmd(&["detach-client"]).expect("detach-client");
    a.wait_for("[detached (from session main)]");
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

/// A blocked `wait-for` holds the rest of its request until the signal,
/// and the request prints everything once done, as tmux 3.7b (recorded).
#[test]
fn wait_for_holds_the_rest_of_the_request() {
    use std::io::Read;
    let srv = start();
    let mut cmd = srv.command();
    cmd.args([
        "display-message",
        "-p",
        "a",
        ";",
        "wait-for",
        "c",
        ";",
        "new-session",
        "-d",
        "-s",
        "after",
        ";",
        "display-message",
        "-p",
        "b",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let mut waiter = cmd.spawn().expect("spawn wait-for");
    let mut stdout = waiter.stdout.take().expect("stdout");
    sleep(Duration::from_millis(300));
    assert!(
        srv.run(&["has-session", "-t", "after"]).status.code() == Some(1),
        "the rest ran before the signal"
    );
    srv.cmd(&["wait-for", "-S", "c"]).expect("wait-for -S");
    let status = wait_child(waiter, WAIT).expect("the waiter exits");
    assert!(status.success(), "{status:?}");
    let mut out = String::new();
    stdout.read_to_string(&mut out).expect("read stdout");
    assert_eq!(out, "a\nb\n");
    assert!(srv.run(&["has-session", "-t", "after"]).status.success());

    // A caller that hangs up drops its rest, as tmux frees a dead client's
    // queue.
    let mut cmd = srv.command();
    cmd.args(["wait-for", "d", ";", "new-session", "-d", "-s", "gone"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut waiter = cmd.spawn().expect("spawn wait-for");
    sleep(Duration::from_millis(300));
    waiter.kill().expect("kill waiter");
    waiter.wait().expect("reap waiter");
    sleep(Duration::from_millis(200));
    srv.cmd(&["wait-for", "-S", "d"]).expect("wait-for -S");
    sleep(Duration::from_millis(300));
    assert_eq!(
        srv.run(&["has-session", "-t", "gone"]).status.code(),
        Some(1)
    );
}

/// How many file descriptors `pid` has open.
fn open_fds(pid: i32) -> usize {
    if let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        return dir.count();
    }
    let out = std::process::Command::new("lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()
        .expect("lsof");
    // Skip the header; count only numbered descriptors (not cwd, txt...).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|fd| fd.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count()
}

/// Killed waiters leave nothing behind: the server closes their
/// connections, as tmux does, and a later signal still works.
#[test]
fn killed_waiters_close_their_connections() {
    let srv = start();
    let pid: i32 = display(&srv, "main", "#{pid}").parse().expect("server pid");
    let baseline = open_fds(pid);
    let waiters: Vec<_> = (0..50)
        .map(|i| {
            let mut cmd = srv.command();
            cmd.args(["wait-for", &format!("chan{}", i % 5)])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            cmd.spawn().expect("spawn wait-for")
        })
        .collect();
    wait_until(|| open_fds(pid) >= baseline + 50, WAIT)
        .unwrap_or_else(|_| panic!("{} fds, baseline {baseline}", open_fds(pid)));
    for mut w in waiters {
        w.kill().expect("kill waiter");
        w.wait().expect("reap waiter");
    }
    wait_until(|| open_fds(pid) <= baseline, WAIT)
        .unwrap_or_else(|_| panic!("{} fds, baseline {baseline}", open_fds(pid)));

    let mut cmd = srv.command();
    cmd.args(["wait-for", "chan0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let waiter = cmd.spawn().expect("spawn wait-for");
    wait_until(|| open_fds(pid) > baseline, WAIT).expect("the waiter connects");
    srv.cmd(&["wait-for", "-S", "chan0"]).expect("wait-for -S");
    let status = wait_child(waiter, WAIT).expect("the waiter exits");
    assert!(status.success(), "{status:?}");
}

/// tmux's stock `prefix s`: `choose-tree -Zs` lists the sessions
/// collapsed, the current one selected.
#[test]
fn choose_tree_s_lists_sessions_collapsed() {
    let srv = start();
    new_session(&srv, "other");
    let mut a = attach(&srv, "main");
    srv.cmd(&["choose-tree", "-Zs"]).expect("choose-tree -Zs");
    a.wait_for("(0) + main: 1 windows (attached)");
    a.wait_for("(1) + other: 1 windows");
    a.wait_for("┌ main (sort: index) ");
    a.send("\x1b[C");
    a.wait_for("(1) └─> + 1: ");
    a.wait_for("(2) + other: 1 windows");
}

/// opensessions' `new-session -d -P -F` prints the new session; sessionx's
/// `-n` names its window for good.
#[test]
fn new_session_p_prints_it_and_n_names_the_window() {
    let srv = start();
    let out = srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "x",
        "-n",
        "w",
        "-P",
        "-F",
        "#{session_name}|#{window_name}",
    ]);
    assert_eq!(out.as_deref(), Ok("x|w\n"));
    let out = srv.cmd(&["new-session", "-d", "-s", "y", "-P"]);
    assert_eq!(out.as_deref(), Ok("y:\n"));
    assert_eq!(display(&srv, "x", "#{window_name}"), "w");
}

/// `new-session -P` without `-d` attaches, as tmux 3.7b's does.
#[test]
fn new_session_p_without_d_attaches() {
    let srv = start();
    let mut cmd = srv.command();
    cmd.args([
        "new-session",
        "-P",
        "-F",
        "#{session_name}",
        "-s",
        "x",
        "cat",
    ])
    .env("TERM", "xterm-256color")
    .env_remove("COLORTERM");
    let mut a = Harness::spawn(cmd, 80, 24);
    wait_last_row_starts(&mut a, "[x]");
    assert_eq!(lines(&srv, &["list-sessions"]).len(), 2);
}

/// `display-message -c` shows the message on that client only.
#[test]
fn display_message_c_goes_to_that_client() {
    let srv = start();
    let mut a = attach(&srv, "main");
    let mut b = attach(&srv, "main");
    let names: Vec<String> = clients(&srv)
        .iter()
        .map(|c| c.split(' ').next().unwrap_or_default().to_string())
        .collect();
    srv.cmd(&["display-message", "-c", &names[0], "for a"])
        .expect("display-message -c");
    a.wait_until("a's message", WAIT, |t| last_row(t).trim_end() == "for a");
    assert!(!last_row(&b.text()).contains("for a"));
    let out = srv.cmd(&[
        "display-message",
        "-p",
        "-c",
        &names[1],
        "-F",
        "#{client_name}",
    ]);
    assert_eq!(out.as_deref(), Ok(format!("{}\n", names[1]).as_str()));
}

/// tmux's stock pan keys: `refresh-client -L` moves the visible part of a
/// window larger than the client, and `-c` goes back to following the
/// cursor from offset 0.
#[test]
fn refresh_client_pans_a_window_larger_than_the_client() {
    let srv = start();
    srv.cmd(&[
        "new-window",
        "-t",
        "main",
        "printf '\\033[5;10HX'; exec cat",
    ])
    .expect("new-window");
    srv.cmd(&["resize-window", "-t", "main", "-x", "120", "-y", "40"])
        .expect("resize-window");
    let mut a = attach(&srv, "main");
    let x_at = |t: &str| t.lines().nth(4).and_then(|l| l.find('X'));
    a.wait_until("X at column 9", WAIT, |t| x_at(t) == Some(9));
    srv.cmd(&["refresh-client", "-R", "30"]).expect("pan right");
    a.wait_until("X panned off", WAIT, |t| x_at(t).is_none());
    srv.cmd(&["refresh-client", "-L", "25"]).expect("pan left");
    a.wait_until("X panned to column 4", WAIT, |t| x_at(t) == Some(4));
    // tmux 3.7b goes back to offset 0 while the cursor is on the first
    // screenful (recorded).
    srv.cmd(&["refresh-client", "-c"]).expect("reset");
    a.wait_until("X back at column 9", WAIT, |t| x_at(t) == Some(9));
    let err = srv.cmd(&["refresh-client", "-R", "nope"]).unwrap_err();
    assert_eq!(err.trim_end(), "adjustment invalid");
    let keys = srv.cmd(&["list-keys", "-T", "prefix"]).expect("list-keys");
    for want in [
        "refresh-client -U 10",
        "refresh-client -c",
        "suspend-client",
        "choose-tree -Zs",
    ] {
        assert!(keys.contains(want), "{want}: {keys}");
    }
}

/// `new-session -A -d` on an existing session attaches anyway when there
/// is a terminal, as tmux 3.7b does.
#[test]
fn new_session_a_d_attaches_from_a_terminal() {
    let srv = start();
    let mut cmd = srv.command();
    cmd.args(["new-session", "-A", "-d", "-s", "main"])
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM");
    let mut a = Harness::spawn(cmd, 80, 24);
    wait_last_row_starts(&mut a, "[main]");
    assert_eq!(lines(&srv, &["list-sessions"]).len(), 1);
}

/// As in tmux 3.7b, an open chooser puts its pane in a mode: while it is
/// open `#{pane_in_mode}` is 1 and `#{pane_mode}` names it (`tree-mode`,
/// `client-mode`, `buffer-mode`, from `window-tree.c`, `window-client.c`
/// and `window-buffer.c`; TP.9b's key parity read `buffer-mode` back from
/// tmux after the stock `prefix =`). Closed, both are back to 0 and empty.
#[test]
fn an_open_chooser_puts_its_pane_in_a_mode() {
    let srv = start();
    let mut a = attach(&srv, "main");
    srv.cmd(&["set-buffer", "one"]).expect("set-buffer");
    let mode = || display(&srv, "main", "#{pane_in_mode} #{pane_mode}");
    for (verb, name, shown) in [
        ("choose-tree", "tree-mode", "(sort: index)"),
        ("choose-client", "client-mode", "(sort: name)"),
        ("choose-buffer", "buffer-mode", "(sort: creation)"),
    ] {
        srv.cmd(&[verb]).expect(verb);
        a.wait_for(shown);
        assert_eq!(mode(), format!("1 {name}"), "{verb}");
        a.send("q");
        a.wait_until("the chooser to close", WAIT, |t| !t.contains("(sort: "));
        assert_eq!(mode(), "0", "{verb}, closed");
    }
}
