//! Write-back to settings.toml (spec 2026-11-20-settings-menu, PR 2):
//! T12 to T15, T16 (library level), T18. Needs `toml_edit` as a dependency.

use std::fs;
use std::path::Path;

use grofe::config::{self, Env, write};
use toml_edit::value;

fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("grofe-write")
        .tempdir()
        .expect("tempdir")
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).expect("read")
}

const FIXTURE: &str = "# my settings\n\
\n\
history-limit = 5000   # lots\n\
\n\
# mouse on\n\
mouse = true\n\
\n\
base-index = 1\n";

/// T12: setting one key changes one line; every other byte is equal.
#[test]
fn set_changes_one_line_and_keeps_every_other_byte() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    fs::write(&file, FIXTURE).expect("fixture");
    write::set(tmp.path(), "history-limit", value(9000)).expect("set");
    let got = read(&file);
    let old: Vec<&str> = FIXTURE.lines().collect();
    let new: Vec<&str> = got.lines().collect();
    assert_eq!(old.len(), new.len(), "{got}");
    let changed: Vec<usize> = (0..old.len()).filter(|&i| old[i] != new[i]).collect();
    assert_eq!(changed.len(), 1, "{got}");
    assert!(new[changed[0]].starts_with("history-limit = 9000"), "{got}");
    assert!(got.ends_with('\n'));
}

/// T12: a new key is appended and the old text is a byte prefix.
#[test]
fn set_of_a_new_key_appends_and_keeps_the_old_bytes() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    fs::write(&file, FIXTURE).expect("fixture");
    write::set(tmp.path(), "pane-base-index", value(1)).expect("set");
    let got = read(&file);
    assert!(got.starts_with(FIXTURE), "{got}");
    assert!(got.contains("pane-base-index = 1"), "{got}");
}

/// T12: a hand-written 200-line config keeps every byte (edge case "Max").
#[test]
fn a_long_hand_written_file_keeps_every_byte() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    let mut text = String::new();
    for i in 0..100 {
        text.push_str(&format!("# note {i}  世界\n@user-{i} = \"v{i}\"\n"));
    }
    fs::write(&file, &text).expect("fixture");
    write::set(tmp.path(), "mouse", value(false)).expect("set");
    assert!(read(&file).starts_with(&text));
}

/// T12: a dotted key lands where TOML puts it and loads back.
#[test]
fn set_of_a_dotted_key_loads_back() {
    let tmp = dir();
    write::set(tmp.path(), "status.left", value("世界 [#S]")).expect("set");
    let got = read(&tmp.path().join("settings.toml"));
    let v: toml::Table = got.parse().expect("valid toml");
    assert_eq!(v["status"]["left"].as_str(), Some("世界 [#S]"), "{got}");
}

/// T13: a binding and an unbind both land in `[keys.prefix]`.
#[test]
fn bindings_and_unbinds_land_in_the_keys_table() {
    let tmp = dir();
    write::set_binding(tmp.path(), "prefix", "C-t", "new-window").expect("bind");
    write::set_binding(tmp.path(), "prefix", "%", "").expect("unbind");
    let got = read(&tmp.path().join("settings.toml"));
    let v: toml::Table = got.parse().expect("valid toml");
    let prefix = v["keys"]["prefix"].as_table().expect("[keys.prefix]");
    assert_eq!(prefix["C-t"]["cmd"].as_str(), Some("new-window"), "{got}");
    assert_eq!(prefix["%"].as_str(), Some(""), "{got}");
    assert!(got.contains("[keys.prefix]"), "{got}");
}

/// T13: rebinding the same key replaces it; no duplicate key is written.
#[test]
fn rebinding_a_key_replaces_the_line() {
    let tmp = dir();
    write::set_binding(tmp.path(), "prefix", "C-t", "new-window").expect("bind");
    write::set_binding(tmp.path(), "prefix", "C-t", "split-window").expect("rebind");
    let got = read(&tmp.path().join("settings.toml"));
    let v: toml::Table = got.parse().expect("valid toml (no duplicate key)");
    assert_eq!(v["keys"]["prefix"]["C-t"]["cmd"].as_str(), Some("split-window"));
}

/// T14: a missing config dir and file are created.
#[test]
fn a_missing_dir_and_file_are_created() {
    let tmp = dir();
    let cfg = tmp.path().join("nope").join("deeper").join("grofe");
    assert!(!cfg.exists());
    write::set(&cfg, "mouse", value(false)).expect("set");
    let got = read(&cfg.join("settings.toml"));
    assert!(got.contains("mouse = false"), "{got}");
}

/// T14: the write leaves no temp file behind (atomic temp plus rename).
#[test]
fn the_write_leaves_only_settings_toml() {
    let tmp = dir();
    write::set(tmp.path(), "mouse", value(false)).expect("set");
    let names: Vec<String> = fs::read_dir(tmp.path())
        .expect("ls")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["settings.toml".to_string()]);
}

/// T15: rolling back restores the old bytes.
#[test]
fn rollback_restores_the_old_bytes() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    fs::write(&file, FIXTURE).expect("fixture");
    let saved = write::set(tmp.path(), "history-limit", value(1)).expect("set");
    assert_ne!(read(&file), FIXTURE);
    saved.rollback().expect("rollback");
    assert_eq!(read(&file), FIXTURE);
}

/// T15: rolling back a first save removes the file that did not exist.
#[test]
fn rollback_of_a_first_save_removes_the_file() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    let saved = write::set(tmp.path(), "mouse", value(false)).expect("set");
    assert!(file.exists());
    saved.rollback().expect("rollback");
    assert!(!file.exists());
}

/// T15: a set whose result no longer loads fails the load, and the
/// rollback leaves a loadable file.
#[test]
fn rollback_after_a_bad_value_gives_a_loadable_file() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    fs::write(&file, "history-limit = 10\n").expect("fixture");
    let saved = write::set(tmp.path(), "history-limit", value("lots")).expect("set");
    let env = |d: &Path| Env {
        config_dir: Some(d.to_path_buf()),
        home: None,
    };
    assert!(config::load(&env(tmp.path())).is_err(), "bad value must not load");
    saved.rollback().expect("rollback");
    assert_eq!(read(&file), "history-limit = 10\n");
    assert!(config::load(&env(tmp.path())).is_ok());
}

/// T16 (library level): set over user.toml, then unset; the key is gone
/// from settings.toml and the user.toml value loads again.
#[test]
fn unset_removes_the_key_and_the_lower_layer_applies_again() {
    let tmp = dir();
    let env = Env {
        config_dir: Some(tmp.path().to_path_buf()),
        home: None,
    };
    fs::write(tmp.path().join("user.toml"), "history-limit = 111\n").expect("user");
    write::set(tmp.path(), "history-limit", value(222)).expect("set");
    write::set(tmp.path(), "mouse", value(false)).expect("set");
    let r = config::load(&env).expect("load");
    assert_eq!(r.get("history-limit"), Some("222"));
    write::unset(tmp.path(), "history-limit").expect("unset");
    let got = read(&tmp.path().join("settings.toml"));
    assert!(!got.contains("history-limit"), "{got}");
    assert!(got.contains("mouse = false"), "{got}");
    let r = config::load(&env).expect("load");
    assert_eq!(r.get("history-limit"), Some("111"));
}

/// T16: unsetting a key that is not there is not an error.
#[test]
fn unset_of_an_absent_key_is_ok() {
    let tmp = dir();
    write::unset(tmp.path(), "history-limit").expect("unset on missing file");
    fs::write(tmp.path().join("settings.toml"), "mouse = true\n").expect("fixture");
    write::unset(tmp.path(), "history-limit").expect("unset absent");
    assert_eq!(read(&tmp.path().join("settings.toml")), "mouse = true\n");
}

/// T18: an option with no TOML key (a user option) is not saveable.
#[test]
fn an_option_without_a_toml_key_is_not_saveable() {
    let tmp = dir();
    let err = write::set(tmp.path(), "@some-user-option", value("x")).expect_err("must refuse");
    assert!(err.contains("not saveable"), "{err}");
    assert!(!tmp.path().join("settings.toml").exists());
}

/// Edge: an empty settings.toml and a file with a syntax error.
#[test]
fn an_empty_file_takes_a_set_and_a_broken_file_is_refused() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    fs::write(&file, "").expect("empty");
    write::set(tmp.path(), "mouse", value(false)).expect("set into empty");
    assert!(read(&file).contains("mouse = false"));
    fs::write(&file, "mouse = = oops\n").expect("broken");
    assert!(write::set(tmp.path(), "history-limit", value(1)).is_err());
    assert_eq!(read(&file), "mouse = = oops\n", "a refused write keeps the bytes");
}

/// Edge: the write re-reads the file, so a hand edit between two saves survives.
#[test]
fn a_hand_edit_between_saves_survives() {
    let tmp = dir();
    let file = tmp.path().join("settings.toml");
    write::set(tmp.path(), "mouse", value(false)).expect("set");
    let mut text = read(&file);
    text.push_str("# hand edit\nbase-index = 1\n");
    fs::write(&file, &text).expect("edit");
    write::set(tmp.path(), "history-limit", value(7)).expect("set");
    let got = read(&file);
    assert!(got.contains("# hand edit\nbase-index = 1\n"), "{got}");
}
