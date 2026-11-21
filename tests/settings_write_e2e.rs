//! customize-mode `P` saves the selected option to settings.toml
//! (spec 2026-11-20-settings-menu, T17, AC2.8). A private -S socket, a temp
//! HOME and XDG_CONFIG_HOME (the TestServer layout), a pty-attached client.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::time::Duration;

use common::{TestServer, try_wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);

/// T17: with `status` at 3 lines, customize-mode on `status` and `P` writes
/// settings.toml: the TOML key of the option is `status.lines`, so this
/// checks `status.lines = 3`.
#[test]
fn p_in_customize_mode_saves_the_selected_option() {
    let srv = TestServer::start();
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        "70",
        "-y",
        "20",
        "cat",
    ])
    .expect("new-session");
    srv.cmd(&["set", "-g", "status", "3"]).expect("set");
    let mut h = Harness::spawn_attach_with_env(srv.command(), "s", &[], 70, 20);
    // The filter keeps only the `status` option.
    srv.cmd(&[
        "bind-key",
        "C",
        "customize-mode",
        "-Z",
        "-f",
        "#{==:#{option_name},status}",
    ])
    .expect("bind-key");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("status listed", WAIT, |t| t.contains("status:"));
    h.send("j");
    h.send("P");
    let file = srv.config_home.join("grofe").join("settings.toml");
    let ok = try_wait_until(
        || {
            fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.parse::<toml::Table>().ok())
                .and_then(|t| t.get("status")?.get("lines").cloned())
                .is_some_and(|v| v.as_integer() == Some(3))
        },
        WAIT,
    );
    assert!(
        ok.is_ok(),
        "settings.toml: {:?}\nscreen: {}",
        fs::read_to_string(&file),
        h.text()
    );
    // Nothing else is saved.
    let t: toml::Table = fs::read_to_string(&file)
        .expect("read")
        .parse()
        .expect("toml");
    assert_eq!(t.len(), 1, "{t:?}");
}

/// A server with a session `s` and customize-mode on `C`, filtered by the
/// format `filter`; an attached client of `cols` columns.
fn attach(srv: &TestServer, filter: &str, cols: u16) -> Harness {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        &cols.to_string(),
        "-y",
        "20",
        "cat",
    ])
    .expect("new-session");
    let h = Harness::spawn_attach_with_env(srv.command(), "s", &[], cols, 20);
    srv.cmd(&["bind-key", "C", "customize-mode", "-Z", "-f", filter])
        .expect("bind-key");
    h
}

fn settings_file(srv: &TestServer) -> std::path::PathBuf {
    srv.config_home.join("grofe").join("settings.toml")
}

fn user_file(srv: &TestServer) -> std::path::PathBuf {
    srv.config_home.join("grofe").join("user.toml")
}

/// The saved table of settings.toml, once it parses and `has` holds for it.
fn wait_settings(srv: &TestServer, has: impl Fn(&toml::Table) -> bool) -> Result<(), String> {
    let file = settings_file(srv);
    try_wait_until(
        || {
            fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.parse::<toml::Table>().ok())
                .is_some_and(|t| has(&t))
        },
        WAIT,
    )
}

fn global(srv: &TestServer, name: &str) -> String {
    srv.cmd(&["show", "-gv", name])
        .expect("show")
        .trim()
        .to_string()
}

/// T16: `P` over user.toml, then `d`: the key is gone from settings.toml and
/// the user.toml value shows again (AC2.7).
#[test]
fn d_in_customize_mode_resets_to_the_lower_layer() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 80);
    fs::write(user_file(&srv), "history-limit = 111\n").expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(global(&srv, "history-limit"), "111");
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("history-limit listed", WAIT, |t| {
        t.contains("history-limit:")
    });
    h.send("j");
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        t.get("history-limit").and_then(|v| v.as_integer()) == Some(222)
    });
    assert!(
        saved.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    h.send("d");
    h.wait_until("reset asked", WAIT, |t| t.contains("Reset history-limit"));
    h.send("y");
    let gone = wait_settings(&srv, |t| !t.contains_key("history-limit"));
    assert!(
        gone.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    let shown = try_wait_until(|| global(&srv, "history-limit") == "111", WAIT);
    assert!(
        shown.is_ok(),
        "history-limit: {}\n{}",
        global(&srv, "history-limit"),
        h.text()
    );
}

/// After `P`, settings.toml owns the option: a hand edit of the file and a
/// `reload` show the new value, though the first one was set at runtime.
#[test]
fn a_hand_edit_after_p_shows_on_reload() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 80);
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("history-limit listed", WAIT, |t| {
        t.contains("history-limit:")
    });
    h.send("j");
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        t.get("history-limit").and_then(|v| v.as_integer()) == Some(222)
    });
    assert!(
        saved.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    fs::write(settings_file(&srv), "history-limit = 333\n").expect("hand edit");
    srv.cmd(&["reload"]).expect("reload");
    let shown = try_wait_until(|| global(&srv, "history-limit") == "333", WAIT);
    assert!(
        shown.is_ok(),
        "history-limit: {}\n{}",
        global(&srv, "history-limit"),
        h.text()
    );
}

/// `P` on a key binding writes its command in the config's spelling, its
/// note and its repeat flag.
#[test]
fn p_on_a_key_saves_the_binding() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{key},F5}", 100);
    srv.cmd(&[
        "bind-key",
        "-r",
        "-N",
        "my note",
        "F5",
        "display-message",
        "one",
        "\\;",
        "display-message",
        "two",
    ])
    .expect("bind-key");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Key Table - prefix")
    });
    // The groups are not filtered away; the prefix table is the sixth.
    for _ in 0..5 {
        h.send("j");
    }
    h.send("l");
    h.wait_until("key listed", WAIT, |t| t.contains("F5"));
    h.send("j");
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        t.get("keys")
            .and_then(|k| k.get("prefix")?.get("F5"))
            .is_some()
    });
    let text = fs::read_to_string(settings_file(&srv));
    assert!(saved.is_ok(), "{text:?}\n{}", h.text());
    let t: toml::Table = text.expect("read").parse().expect("toml");
    let e = &t["keys"]["prefix"]["F5"];
    assert_eq!(
        e["cmd"].as_str(),
        Some("display-message one ; display-message two"),
        "{e:?}"
    );
    assert_eq!(e["desc"].as_str(), Some("my note"), "{e:?}");
    assert_eq!(e["repeat"].as_bool(), Some(true), "{e:?}");
}

/// AC2.6: when the reload after `P` fails (a broken user.toml), settings.toml
/// keeps its bytes and the error is shown.
#[test]
fn a_failed_reload_after_p_keeps_the_file_and_shows_the_error() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},status}", 200);
    srv.cmd(&["set", "-g", "status", "3"]).expect("set");
    let before = "# mine\nmouse = true\n";
    fs::write(settings_file(&srv), before).expect("settings.toml");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("status listed", WAIT, |t| t.contains("status:"));
    h.send("j");
    fs::write(user_file(&srv), "history-limit = = oops\n").expect("user.toml");
    h.send("P");
    h.wait_until("error shown", WAIT, |t| t.contains("user.toml"));
    assert_eq!(
        fs::read_to_string(settings_file(&srv)).expect("read"),
        before
    );
    assert!(!h.text().contains("Saved status"), "{}", h.text());
}

/// The `list-keys` line of `key` in the prefix table, or an empty string.
fn key_line(srv: &TestServer, key: &str) -> String {
    srv.cmd(&["list-keys", "-T", "prefix"])
        .expect("list-keys")
        .lines()
        .find(|l| l.split_whitespace().any(|w| w == key))
        .unwrap_or("")
        .to_string()
}

/// Opens customize-mode on the prefix table's `F5` and puts the cursor on it.
fn open_f5(h: &mut Harness) {
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Key Table - prefix")
    });
    for _ in 0..5 {
        h.send("j");
    }
    h.send("l");
    h.wait_until("key listed", WAIT, |t| t.contains("F5"));
    h.send("j");
}

/// `P` on a key from user.toml with a `group` and a `name` keeps both, in the
/// file and after the reload.
#[test]
fn p_on_a_key_keeps_its_group_and_name() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{key},F5}", 100);
    fs::write(
        user_file(&srv),
        "[keys.prefix]\nF5 = { cmd = \"display-message hi\", group = \"g\", name = \"n\" }\n",
    )
    .expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    open_f5(&mut h);
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        let e = t.get("keys").and_then(|k| k.get("prefix")?.get("F5"));
        e.and_then(|e| e.get("group")?.as_str()) == Some("g")
            && e.and_then(|e| e.get("name")?.as_str()) == Some("n")
    });
    assert!(
        saved.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    assert!(key_line(&srv, "F5").contains("display-message hi"));
}

/// After `P` on a key, settings.toml owns the binding: a hand edit of the
/// file and a `reload` show the new command, though the first was bound at
/// runtime.
#[test]
fn a_hand_edit_of_a_key_after_p_shows_on_reload() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{key},F5}", 100);
    srv.cmd(&["bind-key", "F5", "display-message", "one"])
        .expect("bind-key");
    open_f5(&mut h);
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        t.get("keys")
            .and_then(|k| k.get("prefix")?.get("F5"))
            .is_some()
    });
    assert!(
        saved.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    fs::write(
        settings_file(&srv),
        "[keys.prefix]\nF5 = { cmd = \"display-message two\" }\n",
    )
    .expect("hand edit");
    srv.cmd(&["reload"]).expect("reload");
    let shown = try_wait_until(|| key_line(&srv, "F5").contains("two"), WAIT);
    assert!(shown.is_ok(), "F5: {}\n{}", key_line(&srv, "F5"), h.text());
}

/// `d` on a saved key takes its entry out of settings.toml and the binding
/// from user.toml shows again.
#[test]
fn d_on_a_saved_key_removes_its_entry() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{key},F5}", 100);
    fs::write(
        user_file(&srv),
        "[keys.prefix]\nF5 = \"display-message user\"\n",
    )
    .expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    srv.cmd(&["bind-key", "F5", "display-message", "mine"])
        .expect("bind-key");
    open_f5(&mut h);
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        t.get("keys")
            .and_then(|k| k.get("prefix")?.get("F5"))
            .is_some()
    });
    assert!(
        saved.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    h.send("d");
    h.wait_until("reset asked", WAIT, |t| t.contains("Reset F5"));
    h.send("y");
    let gone = wait_settings(&srv, |t| {
        t.get("keys")
            .and_then(|k| k.get("prefix")?.get("F5"))
            .is_none()
    });
    assert!(
        gone.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(&srv)),
        h.text()
    );
    let shown = try_wait_until(|| key_line(&srv, "F5").contains("user"), WAIT);
    assert!(shown.is_ok(), "F5: {}\n{}", key_line(&srv, "F5"), h.text());
}

/// When the reload after `P` fails, the option keeps its runtime mark: a
/// later good reload leaves its live value alone.
#[test]
fn a_failed_reload_after_p_keeps_the_runtime_value() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 200);
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("history-limit listed", WAIT, |t| {
        t.contains("history-limit:")
    });
    h.send("j");
    fs::write(user_file(&srv), "history-limit = = oops\n").expect("user.toml");
    h.send("P");
    h.wait_until("error shown", WAIT, |t| t.contains("user.toml"));
    fs::write(user_file(&srv), "history-limit = 111\n").expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(global(&srv, "history-limit"), "222", "{}", h.text());
}

/// `P` on `history-limit`, which was set to 222 at runtime.
fn save_history_limit(srv: &TestServer, h: &mut Harness) {
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("history-limit listed", WAIT, |t| {
        t.contains("history-limit:")
    });
    h.send("j");
    h.send("P");
    let saved = wait_settings(srv, |t| {
        t.get("history-limit").and_then(|v| v.as_integer()) == Some(222)
    });
    assert!(
        saved.is_ok(),
        "{:?}\n{}",
        fs::read_to_string(settings_file(srv)),
        h.text()
    );
}

/// After `P`, the config owns the option and has recorded its base: with the
/// key deleted from settings.toml by hand, a `reload` shows the built-in
/// default, not the value that was saved.
#[test]
fn deleting_a_saved_key_by_hand_restores_the_default_on_reload() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 80);
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    save_history_limit(&srv, &mut h);
    fs::write(settings_file(&srv), "").expect("hand edit");
    srv.cmd(&["reload"]).expect("reload");
    let shown = try_wait_until(|| global(&srv, "history-limit") == "2000", WAIT);
    assert!(
        shown.is_ok(),
        "history-limit: {}\n{}",
        global(&srv, "history-limit"),
        h.text()
    );
}

/// When the reload after `P` on a key fails, the runtime binding stays: a
/// later good reload leaves it alone.
#[test]
fn a_failed_reload_after_p_keeps_the_runtime_binding() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{key},F5}", 100);
    srv.cmd(&["bind-key", "F5", "display-message", "one"])
        .expect("bind-key");
    open_f5(&mut h);
    fs::write(user_file(&srv), "history-limit = = oops\n").expect("user.toml");
    h.send("P");
    h.wait_until("error shown", WAIT, |t| t.contains("user.toml"));
    assert!(
        key_line(&srv, "F5").contains("one"),
        "{}",
        key_line(&srv, "F5")
    );
    fs::write(
        user_file(&srv),
        "[keys.prefix]\nF5 = \"display-message user\"\n",
    )
    .expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    assert!(
        key_line(&srv, "F5").contains("one"),
        "F5: {}\n{}",
        key_line(&srv, "F5"),
        h.text()
    );
}

/// `d` on a session-local value leaves the saved global entry in settings.toml.
#[test]
fn d_on_a_session_local_value_keeps_the_global_entry() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 80);
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("history-limit listed", WAIT, |t| {
        t.contains("history-limit:")
    });
    h.send("j");
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        t.get("history-limit").and_then(|v| v.as_integer()) == Some(222)
    });
    assert!(saved.is_ok(), "{}", h.text());
    srv.cmd(&["set", "-t", "s", "history-limit", "333"])
        .expect("set local");
    h.send("q");
    h.send("\x02C");
    h.wait_until("customize-mode reopen", WAIT, |t| {
        t.contains("history-limit")
    });
    h.send("j");
    h.send("l");
    h.wait_until("local listed", WAIT, |t| t.contains("333"));
    // Move the cursor onto the session-local line.
    for _ in 0..3 {
        h.send("j");
        if h.text()
            .lines()
            .any(|l| l.contains("333") && l.contains('>'))
        {
            break;
        }
    }
    h.send("d");
    h.wait_until("reset asked", WAIT, |t| t.contains("Reset history-limit"));
    h.send("y");
    let local_gone = try_wait_until(
        || {
            srv.cmd(&["show", "-t", "s", "history-limit"])
                .is_ok_and(|o| !o.contains("333"))
        },
        WAIT,
    );
    assert!(local_gone.is_ok(), "{}", h.text());
    let t: toml::Table = fs::read_to_string(settings_file(&srv))
        .expect("read")
        .parse()
        .expect("toml");
    assert_eq!(
        t.get("history-limit").and_then(|v| v.as_integer()),
        Some(222),
        "{t:?}\n{}",
        h.text()
    );
}

/// user.toml sets `history-limit` to 111 and the runtime to 222, which `P`
/// saves: the option is open in customize-mode, the cursor on it.
fn save_over_user_toml(srv: &TestServer, h: &mut Harness) {
    fs::write(user_file(srv), "history-limit = 111\n").expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    save_history_limit(srv, h);
}

/// `d` on `history-limit`, answered `y`.
fn reset_history_limit(h: &mut Harness) {
    h.send("d");
    h.wait_until("reset asked", WAIT, |t| t.contains("Reset history-limit"));
    h.send("y");
}

/// AC2.7: after `d` the config owns the option again: user.toml's value
/// shows, and a later edit of user.toml applies on `reload`.
#[test]
fn d_hands_the_option_back_to_the_config() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 80);
    save_over_user_toml(&srv, &mut h);
    reset_history_limit(&mut h);
    let gone = wait_settings(&srv, |t| !t.contains_key("history-limit"));
    assert!(gone.is_ok(), "{}", h.text());
    let shown = try_wait_until(|| global(&srv, "history-limit") == "111", WAIT);
    assert!(shown.is_ok(), "{}", global(&srv, "history-limit"));
    fs::write(user_file(&srv), "history-limit = 444\n").expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(global(&srv, "history-limit"), "444", "{}", h.text());
}

/// A hook whose `run-shell` holds the client's commands does not let an
/// unset land after the reload: `D` on two tagged options still ends at
/// user.toml's values, which the config then owns.
#[test]
fn d_with_a_run_shell_hook_still_shows_the_lower_layer() {
    let srv = TestServer::start();
    let filter = "#{||:#{==:#{option_name},history-limit},#{==:#{option_name},display-time}}";
    let mut h = attach(&srv, filter, 80);
    srv.cmd(&[
        "set-hook",
        "-g",
        "after-set-option",
        "run-shell 'sleep 0.3'",
    ])
    .expect("set-hook");
    let user = |a: u32, b: u32| format!("history-limit = {a}\n[message]\ndisplay-time-ms = {b}\n");
    fs::write(user_file(&srv), user(111, 1111)).expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    srv.cmd(&["set", "-g", "display-time", "2222"])
        .expect("set");
    h.send("\x02C");
    h.wait_until("customize-mode open", WAIT, |t| {
        t.contains("Session Options")
    });
    h.send("j");
    h.send("l");
    h.wait_until("both listed", WAIT, |t| {
        t.contains("history-limit:") && t.contains("display-time:")
    });
    h.send("j");
    h.send("t");
    h.send("t");
    h.send("D");
    h.wait_until("reset asked", WAIT, |t| t.contains("Reset 2 tagged"));
    h.send("y");
    let both =
        |a: &str, b: &str| global(&srv, "history-limit") == a && global(&srv, "display-time") == b;
    assert!(
        try_wait_until(|| both("111", "1111"), WAIT).is_ok(),
        "{}",
        h.text()
    );
    common::quiet_period(Duration::from_millis(800));
    assert!(both("111", "1111"), "{}", h.text());
    fs::write(user_file(&srv), user(444, 4444)).expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    assert!(both("444", "4444"), "{}", h.text());
}

/// AC2.6 for `d`: when the reload fails, settings.toml keeps its bytes, the
/// value stays, and settings.toml still owns it (a hand edit applies).
#[test]
fn a_failed_reload_after_d_leaves_everything_as_before() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 200);
    save_over_user_toml(&srv, &mut h);
    let before = fs::read_to_string(settings_file(&srv)).expect("read");
    fs::write(user_file(&srv), "history-limit = = oops\n").expect("user.toml");
    reset_history_limit(&mut h);
    h.wait_until("error shown", WAIT, |t| t.contains("user.toml"));
    assert_eq!(
        fs::read_to_string(settings_file(&srv)).expect("read"),
        before
    );
    assert_eq!(global(&srv, "history-limit"), "222", "{}", h.text());
    fs::write(user_file(&srv), "history-limit = 111\n").expect("user.toml");
    fs::write(settings_file(&srv), "history-limit = 333\n").expect("hand edit");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(global(&srv, "history-limit"), "333", "{}", h.text());
}

/// `option-changed` comes once per real change: none for `P`, which keeps
/// the value, and one for `d`, which brings back user.toml's.
#[test]
fn p_and_d_announce_only_real_changes() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{option_name},history-limit}", 80);
    fs::write(user_file(&srv), "history-limit = 111\n").expect("user.toml");
    srv.cmd(&["reload"]).expect("reload");
    srv.cmd(&["set", "-g", "history-limit", "222"])
        .expect("set");
    let mut sub = srv.subscribe(&["option-changed"]).expect("subscribe");
    save_history_limit(&srv, &mut h);
    reset_history_limit(&mut h);
    let gone = wait_settings(&srv, |t| !t.contains_key("history-limit"));
    assert!(gone.is_ok(), "{}", h.text());
    srv.cmd(&["set", "-g", "@end", "1"]).expect("set @end");
    let mut seen = Vec::new();
    loop {
        match sub.next(WAIT).expect("event") {
            grofe::event::Event::OptionChanged { name, .. } if name == "@end" => break,
            grofe::event::Event::OptionChanged { name, value, .. } if name == "history-limit" => {
                seen.push(value);
            }
            _ => {}
        }
    }
    assert_eq!(seen, vec![Some("111".to_string())], "{}", h.text());
}

/// `P` replaces the whole entry: a key saved with a note, then bound again
/// at runtime without one, is saved with no `desc`.
#[test]
fn p_drops_a_field_the_live_binding_lacks() {
    let srv = TestServer::start();
    let mut h = attach(&srv, "#{==:#{key},F5}", 100);
    srv.cmd(&["bind-key", "-N", "my note", "F5", "display-message", "one"])
        .expect("bind-key");
    open_f5(&mut h);
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        let e = t.get("keys").and_then(|k| k.get("prefix")?.get("F5"));
        e.and_then(|e| e.get("desc")?.as_str()) == Some("my note")
    });
    assert!(saved.is_ok(), "{}", h.text());
    srv.cmd(&["bind-key", "F5", "display-message", "two"])
        .expect("rebind");
    h.send("P");
    let saved = wait_settings(&srv, |t| {
        let e = t.get("keys").and_then(|k| k.get("prefix")?.get("F5"));
        e.and_then(|e| e.get("cmd")?.as_str()) == Some("display-message two")
    });
    let text = fs::read_to_string(settings_file(&srv));
    assert!(saved.is_ok(), "{text:?}\n{}", h.text());
    let t: toml::Table = text.expect("read").parse().expect("toml");
    let e = &t["keys"]["prefix"]["F5"];
    assert!(e.get("desc").is_none(), "{e:?}");
}
