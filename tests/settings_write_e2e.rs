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
