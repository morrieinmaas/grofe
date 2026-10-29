//! `grofe layout up` attaching, switching and picking through an attached
//! client (docs/layouts.md).

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::time::Duration;

use common::TestServer;

use crate::harness::Harness;

const WAIT: Duration = Duration::from_secs(15);

fn write_layout(srv: &TestServer, name: &str, text: &str) {
    let dir = srv.config_home.join("grofe").join("layouts");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.toml")), text).unwrap();
}

fn status_has(text: &str, session: &str) -> bool {
    text.lines()
        .last()
        .is_some_and(|l| l.starts_with(&format!("[{session}]")))
}

/// Without `-d` on a terminal, `up` builds the session and attaches to it.
#[test]
fn up_attaches_this_terminal() {
    let srv = TestServer::unstarted();
    write_layout(&srv, "dev", "name = \"dev\"\n");
    let mut cmd = srv.command();
    cmd.args(["layout", "up", "dev"])
        .env("TERM", "xterm-256color");
    let mut a = Harness::spawn(cmd, 80, 24);
    a.wait_until("the status row of dev", WAIT, |t| status_has(t, "dev"));
}

/// From a pane of an attached client, `up` switches that client.
#[test]
fn up_inside_a_client_switches_instead_of_nesting() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "23"])
        .unwrap();
    write_layout(&srv, "other", "name = \"other\"\n");
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_until("the status row of main", WAIT, |t| status_has(t, "main"));
    let exe = env!("CARGO_BIN_EXE_grofe");
    a.send(format!(
        "{exe} -S {} layout up other\n",
        srv.socket.display()
    ));
    a.wait_until("the status row of other", WAIT, |t| status_has(t, "other"));
    let names = srv
        .cmd(&["list-sessions", "-F", "#{session_name}"])
        .unwrap();
    assert_eq!(names.lines().count(), 2, "{names}");
}

/// `layout pick` bound by the user shows a menu; its key runs `up`.
#[test]
fn pick_shows_a_menu_and_the_choice_is_built_and_entered() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "23"])
        .unwrap();
    write_layout(&srv, "alpha", "name = \"alpha-s\"\n");
    write_layout(&srv, "beta", "name = \"beta-s\"\n");
    let bind = format!(
        "run-shell '{} -S {} layout pick'",
        env!("CARGO_BIN_EXE_grofe"),
        srv.socket.display()
    );
    srv.cmd(&["bind-key", "g", &bind]).unwrap();
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_until("the status row of main", WAIT, |t| status_has(t, "main"));
    a.send("\x02g");
    a.wait_until("the menu", WAIT, |t| {
        t.contains("alpha (alpha-s)") && t.contains("beta (beta-s)")
    });
    a.send("2");
    a.wait_until("the status row of beta-s", WAIT, |t| {
        status_has(t, "beta-s")
    });
}
