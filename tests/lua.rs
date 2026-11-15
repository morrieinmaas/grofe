//! Luau scripting end to end: `init.luau` against a real server, with keys
//! pressed through an attached client. Each test has a private socket and
//! config dir (`tests/common`).

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::time::{Duration, Instant};

use common::TestServer;
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
/// What an attached client sees once the server has turned the mouse on.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

/// A server whose config dir holds `init`, with session `main` running `cat`.
fn with_init(init: &str) -> TestServer {
    let srv = TestServer::builder().unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("mkdir config");
    fs::write(dir.join("init.luau"), init).expect("write init.luau");
    ok(&srv, &["start-server"]);
    ok(
        &srv,
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "80",
            "-y",
            "23",
            "cat",
        ],
    );
    srv
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    a.wait_for_output(MOUSE_ON, 0);
    a.wait_for("[main]");
    a
}

fn panes(srv: &TestServer) -> usize {
    ok(srv, &["list-panes", "-t", "main"]).lines().count()
}

#[test]
fn init_binds_a_key_that_runs() {
    let srv = with_init("grofe.bind('prefix', 'g', function() grofe.cmd{'split-window'} end)\n");
    let mut a = attach(&srv);
    assert_eq!(panes(&srv), 1);
    a.send("\x02g");
    srv.wait_for(|| panes(&srv) == 2, WAIT)
        .unwrap_or_else(|e| panic!("{e}; log:\n{}", srv.log()));
}

#[test]
fn status_widget_updates_option() {
    let srv = with_init(
        "grofe.every(100, function() grofe.system({'echo','hi'}, function(r) \
         grofe.cmd{'set-option','-g','@w', (r.stdout:gsub('%s+$',''))} end) end)\n",
    );
    srv.wait_for(
        || {
            srv.cmd(&["display-message", "-p", "#{@w}"])
                .is_ok_and(|s| s.trim() == "hi")
        },
        Duration::from_secs(1),
    )
    .unwrap_or_else(|e| panic!("{e}; log:\n{}", srv.log()));
}

#[test]
fn spinning_key_does_not_freeze() {
    let srv = with_init("grofe.bind('prefix', 'x', function() while true do end end)\n");
    let mut a = attach(&srv);
    a.send("\x02x");
    let t = Instant::now();
    srv.wait_for(
        || {
            srv.cmd(&["show-messages"])
                .is_ok_and(|m| m.contains("budget"))
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}; log:\n{}", srv.log()));
    ok(&srv, &["list-sessions"]);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "took {:?}",
        t.elapsed()
    );
    assert!(ok(&srv, &["show-messages"]).contains("budget"));
}

#[test]
fn reload_replaces_bindings() {
    let srv = with_init("grofe.bind('prefix', 'g', function() grofe.cmd{'split-window'} end)\n");
    let mut a = attach(&srv);
    let init = srv.config_home.join("grofe/init.luau");
    fs::write(
        &init,
        "grofe.bind('prefix', 'h', function() grofe.cmd{'split-window'} end)\n",
    )
    .expect("rewrite init.luau");
    ok(&srv, &["reload"]);
    a.send("\x02g");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(panes(&srv), 1, "the old key still fires");
    a.send("\x02h");
    srv.wait_for(|| panes(&srv) == 2, WAIT)
        .unwrap_or_else(|e| panic!("{e}; log:\n{}", srv.log()));
}

#[test]
fn lua_verb_returns_values() {
    let srv = TestServer::start();
    assert_eq!(ok(&srv, &["lua", "return 1 + 1"]).trim(), "2");
}
