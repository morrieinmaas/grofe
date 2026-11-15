//! Agents step 2 (spec §5.2, §5.5): the picker and the key menu, driven by
//! keys typed into an attached client's pty on a private `-S` socket.

mod common;
#[allow(dead_code, clippy::duplicate_mod)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::time::Duration;

use common::{PROMPT, TestServer, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";

fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "100", "-y", "30"])
        .expect("new-session");
    wait_prompt(&srv, "main");
    srv
}

fn wait_prompt(srv: &TestServer, target: &str) {
    srv.wait_for(
        || {
            srv.capture(target)
                .lines()
                .any(|l| l.trim_end().ends_with(PROMPT.trim_end()))
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: prompt in {target}"));
}

/// A client attached to `main`, past its first full redraw.
fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 100, 30);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

fn display(srv: &TestServer, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", "main", format])
        .unwrap_or_else(|e| panic!("display {format}: {e}"))
        .trim_end()
        .to_string()
}

fn window_count(srv: &TestServer) -> usize {
    srv.cmd(&["list-windows", "-t", "main"])
        .expect("list-windows")
        .lines()
        .count()
}

/// Runs `grofe agent report idle --msg <msg>` from the shell of `target`, so
/// it has `GROFE_PANE`. `msg_shell` is shell text, so a test can keep the
/// message itself off the pane's echoed command line.
fn report_idle(srv: &TestServer, target: &str, msg_shell: &str) {
    let line = format!(
        "{} -S {} agent report idle --msg {msg_shell}",
        env!("CARGO_BIN_EXE_grofe"),
        srv.socket.display()
    );
    srv.cmd(&["send-keys", "-t", target, "-l", &line])
        .expect("send-keys -l");
    srv.cmd(&["send-keys", "-t", target, "Enter"])
        .expect("send-keys Enter");
}

fn agent_count(srv: &TestServer) -> usize {
    srv.cmd(&["agent", "list", "-F", "#{agent_state}"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

fn wait_agents(srv: &TestServer, n: usize) {
    wait_until(|| agent_count(srv) == n, WAIT).unwrap_or_else(|e| panic!("{e}: want {n} agents"));
}

#[test]
fn prefix_space_opens_key_menu_and_runs_c() {
    let srv = start();
    let mut a = attach(&srv);
    assert_eq!(window_count(&srv), 1);
    a.send(PREFIX);
    a.send(" ");
    a.wait_for("key-menu prefix");
    a.send("c");
    wait_until(|| window_count(&srv) == 2, WAIT).unwrap_or_else(|e| panic!("{e}: new window"));
    a.wait_until("menu closed", WAIT, |t| !t.contains("key-menu prefix"));
}

#[test]
fn prefix_u_upper_opens_picker_enter_jumps() {
    let srv = start();
    let first = display(&srv, "#{window_id}");
    report_idle(&srv, &first, "'ze'$(echo bra)'fish'");
    wait_agents(&srv, 1);
    srv.cmd(&["new-window", "-t", "main"]).expect("new-window");
    let second = display(&srv, "#{window_id}");
    wait_prompt(&srv, &second);
    assert_ne!(first, second);
    let mut a = attach(&srv);
    a.send(PREFIX);
    a.send("U");
    a.wait_for("choose-agent");
    a.wait_for("zebrafish");
    a.send("\r");
    wait_until(|| display(&srv, "#{window_id}") == first, WAIT)
        .unwrap_or_else(|e| panic!("{e}: jumped to window 0"));
    a.wait_until("picker closed", WAIT, |t| !t.contains("choose-agent"));
}

#[test]
fn picker_filters_by_typing() {
    let srv = start();
    let first = display(&srv, "#{window_id}");
    report_idle(&srv, &first, "'ze'$(echo bra)'fish'");
    srv.cmd(&["new-window", "-t", "main"]).expect("new-window");
    let second = display(&srv, "#{window_id}");
    wait_prompt(&srv, &second);
    report_idle(&srv, &second, "'wal'$(echo ru)'s'");
    wait_agents(&srv, 2);
    let mut a = attach(&srv);
    a.send(PREFIX);
    a.send("U");
    a.wait_for("zebrafish");
    a.wait_for("walrus");
    a.send("walr");
    a.wait_until("zebrafish filtered out", WAIT, |t| {
        t.contains("walrus") && !t.contains("zebrafish")
    });
}

#[test]
fn key_menu_c_f_filter() {
    let srv = start();
    let mut a = attach(&srv);
    a.send(PREFIX);
    a.send(" ");
    a.wait_for("key-menu prefix");
    a.send("\x06");
    a.send("create a new");
    a.wait_until("rows filtered", WAIT, |t| {
        t.contains("Create a new window") && !t.contains("Break pane")
    });
    a.send("\r");
    wait_until(|| window_count(&srv) == 2, WAIT).unwrap_or_else(|e| panic!("{e}: new window"));
    a.wait_until("menu closed", WAIT, |t| !t.contains("key-menu prefix"));
}
