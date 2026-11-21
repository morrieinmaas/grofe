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

/// `key` in the prefix table runs `cmd`.
fn bound_to(srv: &TestServer, key: &str, cmd: &str) -> bool {
    srv.cmd(&["list-keys", "-T", "prefix"])
        .unwrap_or_default()
        .lines()
        .any(|l| {
            // list-keys backslash-escapes a lone `%`.
            let shown = l.contains(&format!(" {key} ")) || l.contains(&format!(" \\{key} "));
            shown && l.contains(cmd)
        })
}

/// In Keys, moves `new-window` from `c` to `C-t`.
fn rebind_new_window(srv: &TestServer, h: &mut Harness) {
    h.send("\t\t");
    type_query(h, "new-window");
    h.send("\r");
    std::thread::sleep(Duration::from_millis(400));
    h.send("\x14");
    try_wait_until(|| bound_to(srv, "C-t", "new-window"), WAIT)
        .unwrap_or_else(|e| panic!("C-t not bound: {e}\n{}", h.text()));
}

/// A config that binds a key, so `reload` rebuilds the key tables.
const KEYS: &str = "[keys.prefix]\nF9 = \"display-message hi\"\n";

/// One more client on session `s`, with the menu open.
fn second_menu(srv: &TestServer) -> Harness {
    let mut h = Harness::spawn_attach_with_env(srv.command(), "s", &[], 100, 30);
    h.send(format!("{PREFIX}C"));
    h.wait_until("menu open", WAIT, |t| t.contains("Appearance"));
    h
}

/// T25: Esc after two option changes, a rebind and an unbind asks, and `y`
/// restores all of them. They are the config's again: a reload applies a
/// config edit and keeps the keys.
#[test]
fn esc_discards_every_change_after_confirming() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", KEYS);
    let mut h = open_menu(&srv);
    let (status0, mouse0) = (opt(&srv, "status"), opt(&srv, "mouse"));
    assert!(bound_to(&srv, "c", "new-window"));
    type_query(&mut h, "status");
    h.send("\r");
    clear_query(&mut h, 6);
    type_query(&mut h, "mouse");
    h.send("\r");
    try_wait_until(|| opt(&srv, "mouse") != mouse0, WAIT).expect("mouse applied");
    assert_ne!(opt(&srv, "status"), status0);
    clear_query(&mut h, 5);
    rebind_new_window(&srv, &mut h);
    h.send("\x04");
    try_wait_until(|| !bound_to(&srv, "C-t", "new-window"), WAIT)
        .unwrap_or_else(|e| panic!("C-t still bound: {e}\n{}", h.text()));
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| t.contains("discard 4 changes? (y/n)"));
    h.send("y");
    try_wait_until(
        || {
            opt(&srv, "status") == status0
                && opt(&srv, "mouse") == mouse0
                && bound_to(&srv, "c", "new-window")
                && !bound_to(&srv, "C-t", "new-window")
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("not restored: {e}\n{}", h.text()));
    write(&srv, "config.toml", &format!("mouse = false\n{KEYS}"));
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "mouse"), "off", "mouse kept its runtime mark");
    assert!(bound_to(&srv, "c", "new-window"), "the reload unbound c");
}

/// A key the menu moved or unbound stays so over a `reload`, and its rows
/// are marked as changed.
#[test]
fn a_reload_keeps_the_menus_key_edits() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", KEYS);
    let mut h = open_menu(&srv);
    rebind_new_window(&srv, &mut h);
    h.wait_until("C-t row marked", WAIT, |t| {
        t.lines()
            .any(|l| l.contains("* prefix") && l.contains("C-t"))
    });
    srv.cmd(&["reload"]).expect("reload");
    assert!(
        !bound_to(&srv, "c", "new-window"),
        "the reload bound c again"
    );
    assert!(bound_to(&srv, "C-t", "new-window"));
    h.send("\x04");
    try_wait_until(|| !bound_to(&srv, "C-t", "new-window"), WAIT)
        .unwrap_or_else(|e| panic!("C-t still bound: {e}\n{}", h.text()));
    srv.cmd(&["reload"]).expect("reload");
    assert!(!bound_to(&srv, "C-t", "new-window"));
    assert!(!bound_to(&srv, "c", "new-window"));
}

/// Edge concurrent: each menu undoes only what it set, so B's Esc `y`
/// leaves A's change.
#[test]
fn two_menus_each_discard_only_their_own_changes() {
    let srv = TestServer::start();
    let (status0, mouse0) = (opt(&srv, "status"), opt(&srv, "mouse"));
    let mut a = open_menu(&srv);
    let mut b = second_menu(&srv);
    type_query(&mut a, "mouse");
    a.send("\r");
    try_wait_until(|| opt(&srv, "mouse") != mouse0, WAIT).expect("mouse applied");
    type_query(&mut b, "status");
    b.send("\r");
    try_wait_until(|| opt(&srv, "status") != status0, WAIT).expect("status applied");
    b.send(ESC);
    b.wait_until("asks", WAIT, |t| t.contains("discard 1 changes? (y/n)"));
    b.send("y");
    try_wait_until(|| opt(&srv, "status") == status0, WAIT)
        .unwrap_or_else(|e| panic!("status not restored: {e}\n{}", b.text()));
    assert_ne!(opt(&srv, "mouse"), mouse0, "B undid A's change");
}

/// Edge concurrent: a reload while the menu is open is not the menu's
/// change, and Esc `y` does not undo it.
#[test]
fn esc_y_keeps_a_reload_made_while_open() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    write(&srv, "config.toml", "status.left = \"R\"\n");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "status-left"), "R");
    type_query(&mut h, "mouse");
    h.send("\r");
    try_wait_until(|| opt(&srv, "mouse") == "off", WAIT).expect("mouse applied");
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| t.contains("discard 1 changes? (y/n)"));
    h.send("y");
    try_wait_until(|| opt(&srv, "mouse") == "on", WAIT)
        .unwrap_or_else(|e| panic!("mouse not restored: {e}\n{}", h.text()));
    assert_eq!(opt(&srv, "status-left"), "R", "Esc y undid the reload");
}

/// Edge concurrent: after a reload while the menu is open, C-s writes only
/// what the menu set.
#[test]
fn ctrl_s_after_a_reload_writes_only_the_menus_changes() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    write(&srv, "config.toml", "status.left = \"R\"\n");
    srv.cmd(&["reload"]).expect("reload");
    save_mouse(&srv, &mut h, |t| t.contains("saved settings.toml"));
    let text = fs::read_to_string(settings_file(&srv)).expect("settings.toml");
    assert!(text.contains("mouse = false"), "{text}");
    assert!(!text.contains("status"), "{text}");
}

/// A client that goes with its menu open leaves nothing behind: the next
/// client's menu is fresh, and its Esc closes without asking.
#[test]
fn a_client_closing_with_the_menu_open_leaves_no_menu_behind() {
    let srv = TestServer::start();
    let mut a = open_menu(&srv);
    type_query(&mut a, "mouse");
    a.send("\r");
    a.wait_until("mouse marked", WAIT, |t| t.contains("* mouse"));
    srv.cmd(&["detach-client", "-s", "s"]).expect("detach");
    a.wait_exit();
    let mut b = second_menu(&srv);
    type_query(&mut b, "mouse");
    b.wait_until("mouse row", WAIT, |t| t.contains("mouse"));
    assert!(!b.text().contains("* mouse"), "{}", b.text());
    b.send(ESC);
    b.wait_until("closed", WAIT, |t| !t.contains("Appearance"));
    assert!(!b.text().contains("discard"), "{}", b.text());
}

/// `settings` on a client whose menu is open keeps that menu and what it
/// changed.
#[test]
fn settings_again_keeps_the_open_menu() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "mouse");
    h.send("\r");
    try_wait_until(|| opt(&srv, "mouse") == "off", WAIT).expect("mouse applied");
    // A resize runs `settings` again, on this client.
    srv.cmd(&["set-hook", "-g", "client-resized", "settings"])
        .expect("set-hook");
    h.resize(99, 30);
    std::thread::sleep(Duration::from_millis(400));
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| t.contains("discard 1 changes? (y/n)"));
}

/// Edge small window: a client resized below the menu's size closes it and
/// says so, and keys reach the pane again.
#[test]
fn resizing_below_the_menu_closes_it() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    h.resize(20, 5);
    h.wait_until("too small", WAIT, |t| t.contains("window too small"));
    h.send("hello\r");
    try_wait_until(|| srv.capture("s").contains("hello"), WAIT)
        .unwrap_or_else(|e| panic!("input did not reach the pane: {e}\n{}", h.text()));
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
        || fs::read_to_string(&file).is_ok_and(|t| t.contains("mouse") && t.contains("false")),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", h.text()));
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "mouse"), "off");
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
/// `split-window -h` and needs `y`; `C-d` unbinds it.
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
            .any(|l| {
                // list-keys backslash-escapes a lone `%`.
                let shown =
                    |k: &str| l.contains(&format!(" {k} ")) || l.contains(&format!(" \\{k} "));
                shown(k) && l.contains("new-window")
            })
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

    h.send("\x04");
    try_wait_until(|| !bound("%"), WAIT)
        .unwrap_or_else(|e| panic!("% still bound: {e}\n{}", h.text()));
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

fn settings_file(srv: &TestServer) -> PathBuf {
    grofe_dir(srv).join("settings.toml")
}

/// Toggles `mouse` in the menu, then C-s and `y`, and waits for the menu to
/// close or for `done` to show.
fn save_mouse(srv: &TestServer, h: &mut Harness, done: impl Fn(&str) -> bool) {
    h.send("mouse");
    h.send("\r");
    try_wait_until(|| opt(srv, "mouse") == "off", WAIT)
        .unwrap_or_else(|e| panic!("mouse did not change: {e}\n{}", h.text()));
    h.send(CTRL_S);
    h.wait_until("diff", WAIT, |t| t.contains("write settings.toml"));
    h.send("y");
    h.wait_until("saved or failed", WAIT, |t| done(t));
}

/// C-s `y` hands the option to settings.toml: after the key is deleted by
/// hand, `reload` brings the default back instead of keeping the menu's
/// runtime value.
#[test]
fn a_saved_option_follows_a_hand_edit_of_settings_toml() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    save_mouse(&srv, &mut h, |t| t.contains("saved settings.toml"));
    let text = fs::read_to_string(settings_file(&srv)).expect("settings.toml");
    assert!(text.contains("mouse = false"), "{text}");
    fs::write(settings_file(&srv), "# emptied by hand\n").expect("edit");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "mouse"), "on");
}

/// A reload that fails after C-s `y` leaves settings.toml and the live
/// values as they were, and the option keeps its runtime mark.
#[test]
fn a_failed_reload_after_ctrl_s_changes_nothing() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    let before = "# mine\nmouse = true\n";
    fs::write(settings_file(&srv), before).expect("settings.toml");
    let user = grofe_dir(&srv).join("user.toml");
    fs::write(&user, "history-limit = = oops\n").expect("user.toml");
    save_mouse(&srv, &mut h, |t| t.contains("user.toml"));
    assert_eq!(fs::read_to_string(settings_file(&srv)).unwrap(), before);
    assert_eq!(opt(&srv, "mouse"), "off");
    assert!(
        h.text().contains("Appearance"),
        "menu closed:\n{}",
        h.text()
    );
    fs::remove_file(&user).expect("remove user.toml");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "mouse"), "off", "the runtime mark was lost");
}

/// A rebound key is written with its real command and handed to
/// settings.toml.
#[test]
fn a_rebound_key_is_saved_with_its_command() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    h.send("\t\t");
    type_query(&mut h, "new-window");
    h.send("\r");
    std::thread::sleep(Duration::from_millis(400));
    h.send("\x14");
    std::thread::sleep(Duration::from_millis(400));
    h.send(CTRL_S);
    h.wait_until("diff", WAIT, |t| t.contains("write settings.toml"));
    h.send("y");
    let file = settings_file(&srv);
    try_wait_until(
        || fs::read_to_string(&file).is_ok_and(|t| t.contains("C-t")),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", h.text()));
    let text = fs::read_to_string(&file).unwrap();
    let t: toml::Table = text.parse().expect("toml");
    let entry = &t["keys"]["prefix"]["C-t"];
    let cmd = entry["cmd"].as_str().unwrap_or_default();
    assert!(cmd.starts_with("new-window"), "{text}");
    assert_eq!(
        entry["desc"].as_str(),
        Some("Create a new window"),
        "{text}"
    );
}

/// In Keys, moves the binding that matches `query` to `key`.
fn move_binding(h: &mut Harness, query: &str, key: &str) {
    type_query(h, query);
    h.send("\r");
    std::thread::sleep(Duration::from_millis(400));
    h.send(key);
    std::thread::sleep(Duration::from_millis(400));
    clear_query(h, query.len());
}

/// Esc then `y` puts a key back as a runtime `unbind-key` had left it: a
/// reload leaves it unbound.
#[test]
fn esc_y_restores_a_runtime_unbind() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", KEYS);
    let mut h = attached(&srv, 100, 30);
    srv.cmd(&["unbind-key", "c"]).expect("unbind-key");
    h.send(format!("{PREFIX}C"));
    h.wait_until("menu open", WAIT, |t| t.contains("Appearance"));
    h.send("\t\t");
    move_binding(&mut h, "split-window -h", "c");
    try_wait_until(|| bound_to(&srv, "c", "split-window"), WAIT)
        .unwrap_or_else(|e| panic!("c not bound: {e}\n{}", h.text()));
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| t.contains("discard 2 changes? (y/n)"));
    h.send("y");
    try_wait_until(|| bound_to(&srv, "%", "split-window"), WAIT)
        .unwrap_or_else(|e| panic!("% not back: {e}\n{}", h.text()));
    srv.cmd(&["reload"]).expect("reload");
    assert!(!bound_to(&srv, "c", ""), "the reload bound c again");
}

/// A plugin's binding moved onto a key the menu freed is refused at C-s,
/// though the key was noted before the move.
#[test]
fn a_plugin_binding_moved_onto_a_freed_key_is_refused() {
    let srv = TestServer::unstarted();
    write(
        &srv,
        "init.luau",
        "grofe.bind('prefix', 'g', function() grofe.cmd{'split-window'} end)\n",
    );
    let mut h = open_menu(&srv);
    h.send("\t\t");
    type_query(&mut h, "new-window");
    h.send("\x04");
    try_wait_until(|| !bound_to(&srv, "c", "new-window"), WAIT)
        .unwrap_or_else(|e| panic!("c still bound: {e}\n{}", h.text()));
    clear_query(&mut h, "new-window".len());
    move_binding(&mut h, "lua", "c");
    h.send(CTRL_S);
    h.wait_until("refused", WAIT, |t| t.contains("bound by a plugin"));
    assert!(!h.text().contains("write settings.toml"), "{}", h.text());
}

/// A key bound while the menu is open is listed at the next key.
#[test]
fn a_binding_made_while_open_is_listed() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    srv.cmd(&["bind-key", "-T", "prefix", "F8", "display-message", "hello"])
        .expect("bind-key");
    h.send("\t\t");
    type_query(&mut h, "F8");
    h.wait_until("F8 row", WAIT, |t| t.contains("display-message hello"));
}

/// A string option can be set to nothing.
#[test]
fn a_string_option_can_be_emptied() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "default-layout");
    h.send("\r\x15dev\r");
    try_wait_until(|| opt(&srv, "default-layout") == "dev", WAIT)
        .unwrap_or_else(|e| panic!("{e}\n{}", h.text()));
    h.send("\r\x15\r");
    try_wait_until(|| opt(&srv, "default-layout").is_empty(), WAIT)
        .unwrap_or_else(|e| panic!("{e}\n{}", h.text()));
}

/// A menu closed by a resize below the minimum discards like Esc then `y`.
#[test]
fn shrinking_the_client_restores_what_the_menu_changed() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    let mouse0 = opt(&srv, "mouse");
    type_query(&mut h, "mouse");
    h.send("\r");
    try_wait_until(|| opt(&srv, "mouse") != mouse0, WAIT).expect("mouse applied");
    h.resize(20, 5);
    h.wait_until("too small", WAIT, |t| t.contains("window too small"));
    try_wait_until(|| opt(&srv, "mouse") == mouse0, WAIT)
        .unwrap_or_else(|e| panic!("not restored: {e}\n{}", h.text()));
}

/// Sets `history-limit` to `n` through the menu's value prompt.
fn set_history(srv: &TestServer, h: &mut Harness, n: &str) {
    type_query(h, "history-limit");
    h.send(format!("\r\x15{n}\r"));
    try_wait_until(|| opt(srv, "history-limit") == n, WAIT)
        .unwrap_or_else(|e| panic!("history-limit not {n}: {e}\n{}", h.text()));
    clear_query(h, "history-limit".len());
}

/// Esc then `y` on a menu with changes.
fn esc_y(h: &mut Harness, n: usize) {
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| {
        t.contains(&format!("discard {n} changes? (y/n)"))
    });
    h.send("y");
    h.wait_until("closed", WAIT, |t| !t.contains("Appearance"));
}

/// Two menus change one option, the first discards, then the second: the
/// option is the config's again, so a config edit applies at the reload.
#[test]
fn two_menus_discarding_one_option_hand_it_back_to_the_config() {
    let srv = TestServer::start();
    let mut a = open_menu(&srv);
    let mut b = second_menu(&srv);
    set_history(&srv, &mut a, "3000");
    set_history(&srv, &mut b, "4000");
    esc_y(&mut a, 1);
    assert_eq!(opt(&srv, "history-limit"), "4000", "A undid B's change");
    esc_y(&mut b, 1);
    try_wait_until(|| opt(&srv, "history-limit") == "2000", WAIT)
        .unwrap_or_else(|e| panic!("not the config value: {e}"));
    write(&srv, "config.toml", "history-limit = 5000\n");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "history-limit"), "5000", "still runtime");
}

/// A key moved by one menu and unbound by another is the config's again
/// after both discard: a reload binds both keys from the config.
#[test]
fn two_menus_discarding_one_key_hand_it_back_to_the_config() {
    let srv = TestServer::unstarted();
    write(&srv, "config.toml", KEYS);
    let mut a = open_menu(&srv);
    let mut b = second_menu(&srv);
    rebind_new_window(&srv, &mut a);
    b.send("\t\t");
    type_query(&mut b, "C-t");
    b.wait_until("C-t row", WAIT, |t| t.contains("new-window"));
    b.send("\x04");
    try_wait_until(|| !bound_to(&srv, "C-t", "new-window"), WAIT)
        .unwrap_or_else(|e| panic!("C-t still bound: {e}\n{}", b.text()));
    esc_y(&mut a, 2);
    esc_y(&mut b, 1);
    try_wait_until(
        || bound_to(&srv, "c", "new-window") && !bound_to(&srv, "C-t", "new-window"),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("not the config's: {e}"));
    write(
        &srv,
        "config.toml",
        &format!("{KEYS}c = \"display-message cee\"\nC-t = \"display-message tee\"\n"),
    );
    srv.cmd(&["reload"]).expect("reload");
    assert!(
        bound_to(&srv, "c", "display-message cee"),
        "c still runtime"
    );
    assert!(
        bound_to(&srv, "C-t", "display-message tee"),
        "C-t still runtime unbound"
    );
}

/// A config edit reloaded while the menu holds its own value of the option
/// shows once the menu discards.
#[test]
fn esc_y_shows_a_config_value_reloaded_under_the_menus_edit() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    set_history(&srv, &mut h, "3000");
    write(&srv, "config.toml", "history-limit = 5000\n");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(opt(&srv, "history-limit"), "3000");
    esc_y(&mut h, 1);
    try_wait_until(|| opt(&srv, "history-limit") == "5000", WAIT)
        .unwrap_or_else(|e| panic!("not the new config value: {e}"));
}

/// A value prompt the menu opened does nothing once the menu has closed.
#[test]
fn a_value_prompt_answered_after_the_menu_closed_changes_nothing() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "history-limit");
    h.send("\r");
    h.wait_until("prompt", WAIT, |t| t.contains("(history-limit)"));
    // The prompt covers the message line, so there is no `window too small`.
    h.resize(20, 5);
    std::thread::sleep(Duration::from_millis(500));
    h.send("\x153000\r");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(opt(&srv, "history-limit"), "2000");
}

/// `default-layout` is looked up in the server's config folder: a client
/// without `--config` builds the layout next to the server's file.
#[test]
fn default_layout_is_found_in_the_servers_config_folder() {
    let srv = TestServer::unstarted();
    let dir = srv.home.join("a");
    fs::create_dir_all(dir.join("layouts")).unwrap();
    let file = dir.join("x.toml");
    fs::write(&file, "default-layout = \"dev\"\n").unwrap();
    fs::write(
        dir.join("layouts/dev.toml"),
        "[[window]]\nname = \"dev\"\nsplit = \"h\"\n[[window.pane]]\n[[window.pane]]\n",
    )
    .unwrap();
    let out = srv.run(&[
        "--config",
        file.to_str().expect("utf8"),
        "new-session",
        "-d",
        "-s",
        "first",
    ]);
    assert!(out.status.success(), "{out:?}");
    srv.cmd(&["new-session", "-d", "-s", "d"])
        .expect("new-session");
    let panes = srv.cmd(&["list-panes", "-t", "d"]).unwrap();
    assert_eq!(panes.lines().count(), 2, "{panes}");
}

/// Esc `y` with a config that no longer loads keeps the menu open with its
/// edits and shows the error; after the fix, Esc `y` restores the config.
#[test]
fn a_failed_reload_on_discard_keeps_the_menu_open() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    type_query(&mut h, "mouse");
    h.send("\r");
    try_wait_until(|| opt(&srv, "mouse") == "off", WAIT)
        .unwrap_or_else(|e| panic!("mouse did not change: {e}\n{}", h.text()));
    let user = grofe_dir(&srv).join("user.toml");
    fs::write(&user, "history-limit = = oops\n").expect("user.toml");
    h.send(ESC);
    h.wait_until("asks", WAIT, |t| t.contains("discard 1 changes? (y/n)"));
    h.send("y");
    h.wait_until("error", WAIT, |t| t.contains("user.toml"));
    assert!(
        h.text().contains("Appearance"),
        "menu closed:\n{}",
        h.text()
    );
    assert_eq!(opt(&srv, "mouse"), "off");
    fs::remove_file(&user).expect("remove user.toml");
    esc_y(&mut h, 1);
    try_wait_until(|| opt(&srv, "mouse") == "on", WAIT)
        .unwrap_or_else(|e| panic!("config value not restored: {e}"));
}

/// C-s is the menu's even when it is the prefix.
#[test]
fn ctrl_s_saves_in_the_menu_when_it_is_the_prefix() {
    let srv = TestServer::start();
    let mut h = open_menu(&srv);
    srv.cmd(&["set-option", "-g", "prefix", "C-s"])
        .expect("prefix");
    assert_eq!(opt(&srv, "prefix"), "C-s");
    type_query(&mut h, "mouse");
    h.send("\r");
    h.send(CTRL_S);
    h.wait_until("diff", WAIT, |t| t.contains("write settings.toml"));
}
