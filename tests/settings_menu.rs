//! The settings menu, end to end (spec 2026-11-20-settings-menu, PR 3,
//! T19 to T31 and T36). The menu is an overlay on the attached client, so
//! the tests read the client's screen with `Harness::text`.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use common::{TestServer, try_wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";
const BACKSPACE: &str = "\x7f";
const CTRL_S: &str = "\x13";
const ESC: &str = "\x1b";

fn grofe_dir(srv: &TestServer) -> PathBuf {
    srv.config_home.join("grofe")
}

fn write(srv: &TestServer, rel: &str, text: &str) {
    let p = grofe_dir(srv).join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn opt(srv: &TestServer, name: &str) -> String {
    srv.cmd(&["show-options", "-gv", name])
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A server with a `cat` session and an attached client of `w` by `h`.
fn attached(srv: &TestServer, w: u16, h: u16) -> Harness {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        &w.to_string(),
        "-y",
        &h.to_string(),
        "cat",
    ])
    .expect("new-session");
    Harness::spawn_attach_with_env(srv.command(), "s", &[], w, h)
}

/// Client with the menu open (`prefix C`, grofe identity).
fn open_menu(srv: &TestServer) -> Harness {
    let mut h = attached(srv, 100, 30);
    h.send(format!("{PREFIX}C"));
    h.wait_until("menu open", WAIT, |t| t.contains("Appearance"));
    h
}

fn type_query(h: &mut Harness, q: &str) {
    h.send(q);
}

fn clear_query(h: &mut Harness, n: usize) {
    h.send(BACKSPACE.repeat(n));
}

/// T19: `prefix C` opens the menu in grofe identity.
#[test]
fn prefix_c_opens_the_menu_in_grofe_identity() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    let t = h.text();
    assert!(
        !t.contains("Session Options"),
        "that is customize-mode:\n{t}"
    );
}

/// T19: in tmux identity `C` is still customize-mode.
#[test]
fn prefix_c_is_customize_mode_in_tmux_identity() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", "compat-identity = \"tmux\"\n");
    let mut h = attached(&srv, 100, 30);
    h.send(format!("{PREFIX}C"));
    h.wait_until("customize-mode", WAIT, |t| t.contains("Session Options"));
    assert!(!h.text().contains("Appearance"));
}

/// T20: `grofe settings` with no server starts one and draws the menu.
#[test]
fn grofe_settings_starts_a_server_and_draws_the_menu() {
    let srv = TestServer::unstarted();
    assert!(!srv.is_running());
    let mut cmd = srv.command();
    cmd.arg("settings");
    let mut h = Harness::spawn(cmd, 100, 30);
    h.wait_until("menu drawn", WAIT, |t| t.contains("Appearance"));
    assert!(srv.is_running());
}

/// T22 (through the screen): `stl` ranks `status-left` above
/// `status-left-length`.
#[test]
fn query_stl_ranks_status_left_first() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "stl");
    h.wait_until("both rows", WAIT, |t| t.contains("status-left-length"));
    let t = h.text();
    let first = t
        .lines()
        .position(|l| l.contains("status-left"))
        .expect("a status-left row");
    let line = t.lines().nth(first).unwrap();
    assert!(
        !line.contains("status-left-length"),
        "first row is {line:?}\n{t}"
    );
}

/// T23 (through the screen): a settings.toml value over conf.d names the
/// file and line it overrides.
#[test]
fn a_row_shows_what_settings_toml_overrides() {
    let srv = TestServer::unstarted();
    write(&srv, "conf.d/10.toml", "# c\n\nstatus.left = \"A\"\n");
    write(&srv, "settings.toml", "status.left = \"S\"\n");
    let mut h = {
        srv.cmd(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "100",
            "-y",
            "30",
            "cat",
        ])
        .unwrap();
        Harness::spawn_attach_with_env(srv.command(), "s", &[], 100, 30)
    };
    h.send(format!("{PREFIX}C"));
    h.wait_until("menu", WAIT, |t| t.contains("Appearance"));
    type_query(&mut h, "status-left");
    h.wait_until("override note", WAIT, |t| {
        t.contains("overrides conf.d/10.toml:3")
    });
}

/// T24: Enter changes `status`; a bad `history-limit` shows the option's
/// own error and changes nothing.
#[test]
fn enter_changes_a_value_and_a_bad_number_is_refused() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "status");
    h.send("\r");
    try_wait_until(|| opt(&srv, "status") != "on", WAIT).unwrap_or_else(|e| {
        panic!("status did not change: {e}\n{}", h.text());
    });

    let error = srv
        .cmd(&["set", "-g", "history-limit", "abc"])
        .expect_err("abc is not a number");
    let error = error.trim().to_string();
    clear_query(&mut h, 6);
    type_query(&mut h, "history-limit");
    h.send("\r");
    h.send("\x15abc\r");
    h.wait_until("error shown", WAIT, |t| t.contains(error.as_str()));
    assert_eq!(opt(&srv, "history-limit"), "2000");
}

/// T25: Esc after two changes asks, and `y` restores both.
#[test]
fn esc_discards_every_change_after_confirming() {
    let srv = TestServer::start();
    let (status0, mouse0) = (opt(&srv, "status"), opt(&srv, "mouse"));
    let mut h = open_menu(&srv);
    type_query(&mut h, "status");
    h.send("\r");
    clear_query(&mut h, 6);
    type_query(&mut h, "mouse");
    h.send("\r");
    try_wait_until(|| opt(&srv, "mouse") != mouse0, WAIT).expect("mouse applied");
    assert_ne!(opt(&srv, "status"), status0);
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| t.contains("discard 2 changes? (y/n)"));
    h.send("y");
    try_wait_until(
        || opt(&srv, "status") == status0 && opt(&srv, "mouse") == mouse0,
        WAIT,
    )
    .unwrap_or_else(|e| panic!("not restored: {e}\n{}", h.text()));
}

/// T26: C-s shows the diff, `y` writes settings.toml and the value
/// survives `reload`.
#[test]
fn ctrl_s_shows_the_diff_and_y_writes_it() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "mouse");
    h.send("\r");
    h.send(CTRL_S);
    h.wait_until("diff", WAIT, |t| {
        t.contains("settings.toml") && t.contains("mouse")
    });
    h.send("y");
    let file = grofe_dir(&srv).join("settings.toml");
    try_wait_until(
        || fs::read_to_string(&file).is_ok_and(|t| t.contains("mouse") && t.contains("true")),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", h.text()));
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "mouse"), "on");
}

/// AC3.9: `n` after C-s returns to the menu and writes nothing.
#[test]
fn n_after_the_diff_returns_to_the_menu() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "mouse");
    h.send("\r");
    h.send(CTRL_S);
    h.wait_until("diff", WAIT, |t| t.contains("settings.toml"));
    h.send("n");
    h.wait_until("back", WAIT, |t| t.contains("Appearance"));
    assert!(!grofe_dir(&srv).join("settings.toml").exists());
}

/// T27: rebind `new-window` to `C-t`; then rebind to `%`, which is taken by
/// `split-window -h` and needs `y`.
#[test]
fn keys_rebind_and_conflict() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    h.send("\t\t");
    type_query(&mut h, "new-window");
    h.send("\r");
    std::thread::sleep(Duration::from_millis(400));
    h.send("\x14");
    let bound = |k: &str| {
        srv.cmd(&["list-keys", "-T", "prefix"])
            .unwrap_or_default()
            .lines()
            .any(|l| l.contains(&format!(" {k} ")) && l.contains("new-window"))
    };
    try_wait_until(|| bound("C-t"), WAIT)
        .unwrap_or_else(|e| panic!("C-t not bound: {e}\n{}", h.text()));

    h.send("\r");
    std::thread::sleep(Duration::from_millis(400));
    h.send("%");
    h.wait_until("conflict", WAIT, |t| t.contains("replaces split-window -h"));
    assert!(!bound("%"), "bound before the y");
    h.send("y");
    try_wait_until(|| bound("%"), WAIT)
        .unwrap_or_else(|e| panic!("% not bound: {e}\n{}", h.text()));
}

/// T29: `default-layout = "dev"` builds the layout for a plain
/// `new-session`.
#[test]
fn default_layout_builds_for_new_session() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", "default-layout = \"dev\"\n");
    write(
        &srv,
        "layouts/dev.toml",
        "[[window]]\nname = \"dev\"\nsplit = \"h\"\n[[window.pane]]\n[[window.pane]]\n",
    );
    srv.cmd(&["new-session", "-d", "-s", "d"])
        .expect("new-session");
    let panes = srv.cmd(&["list-panes", "-t", "d"]).unwrap();
    assert_eq!(panes.lines().count(), 2, "{panes}");
}

/// T29: an unknown layout name makes a plain session and says why.
#[test]
fn unknown_default_layout_makes_a_plain_session_and_a_message() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", "default-layout = \"nope\"\n");
    srv.cmd(&["new-session", "-d", "-s", "d"])
        .expect("new-session");
    let panes = srv.cmd(&["list-panes", "-t", "d"]).unwrap();
    assert_eq!(panes.lines().count(), 1, "{panes}");
    let msgs = srv.cmd(&["show-messages"]).unwrap_or_default();
    assert!(msgs.contains("default-layout: no layout nope"), "{msgs}");
}

/// AC3.11: Enter on a layout row sets `default-layout`.
#[test]
fn enter_on_a_layout_sets_default_layout() {
    let srv = TestServer::unstarted();
    write(
        &srv,
        "layouts/dev.toml",
        "[[window]]\nname = \"dev\"\nsplit = \"h\"\n[[window.pane]]\n[[window.pane]]\n",
    );
    let mut h = {
        srv.cmd(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "100",
            "-y",
            "30",
            "cat",
        ])
        .unwrap();
        Harness::spawn_attach_with_env(srv.command(), "s", &[], 100, 30)
    };
    h.send(format!("{PREFIX}C"));
    h.wait_until("menu", WAIT, |t| t.contains("Appearance"));
    h.send("\t\t\t");
    h.wait_until("layout row", WAIT, |t| t.contains("dev"));
    h.send("\r");
    try_wait_until(|| opt(&srv, "default-layout") == "dev", WAIT)
        .unwrap_or_else(|e| panic!("{e}\n{}", h.text()));
}

/// T31: a query matching nothing says `no matches`.
#[test]
fn a_query_with_no_match_says_so() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "zzqqxx");
    h.wait_until("no matches", WAIT, |t| t.contains("no matches"));
}

/// T35 (query half): `世界` matches an option whose value holds it.
#[test]
fn a_unicode_query_matches_a_value() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", "status.left = \"世界 hi\"\n");
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        "100",
        "-y",
        "30",
        "cat",
    ])
    .unwrap();
    let mut h = Harness::spawn_attach_with_env(srv.command(), "s", &[], 100, 30);
    h.send(format!("{PREFIX}C"));
    h.wait_until("menu", WAIT, |t| t.contains("Appearance"));
    type_query(&mut h, "世界");
    h.wait_until("row for the value", WAIT, |t| t.contains("status-left"));
}

/// T36: on a 20x5 client the menu says it cannot draw and closes, so the
/// next keys reach the pane.
#[test]
fn a_tiny_client_gets_window_too_small_and_the_menu_closes() {
    let srv = TestServer::start();
    let mut h = attached(&srv, 20, 5);
    h.send(format!("{PREFIX}C"));
    h.wait_until("too small", WAIT, |t| t.contains("window too small"));
    h.send("hello\r");
    try_wait_until(|| srv.capture("s").contains("hello"), WAIT)
        .unwrap_or_else(|e| panic!("input did not reach the pane: {e}\n{}", h.text()));
}
