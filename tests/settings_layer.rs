//! settings.toml as the last config layer (spec 2026-11-20-settings-menu,
//! PR 2): T10 (AC2.1) and T11 (AC2.2). No toml_edit needed: the files are
//! written by hand.

mod common;

use std::fs;
use std::process::Command;
use std::time::Duration;

use common::run_bounded;
use grofe::config::{self, Env, Layer};

fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("grofe-settings")
        .tempdir()
        .expect("tempdir");
    let dir = tmp.path().join("home").join(".config").join("grofe");
    fs::create_dir_all(&dir).expect("mkdir");
    (tmp, dir)
}

/// T10: user.toml `history-limit = 111`, settings.toml `222`: 222 loads.
#[test]
fn settings_toml_wins_over_user_toml() {
    let (tmp, dir) = setup();
    fs::write(dir.join("user.toml"), "history-limit = 111\n").expect("user");
    fs::write(dir.join("settings.toml"), "history-limit = 222\n").expect("settings");
    let home = tmp.path().join("home");
    let env = Env::from_vars(None, home.to_str());
    let r = config::load(&env).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(r.get("history-limit"), Some("222"));
}

/// T10: a key only settings.toml sets still loads, and its provenance is the
/// Settings layer (not the defaults, not User).
#[test]
fn settings_toml_alone_sets_a_key() {
    let (tmp, dir) = setup();
    fs::write(dir.join("settings.toml"), "history-limit = 333\n").expect("settings");
    let env = Env::from_vars(None, tmp.path().join("home").to_str());
    let r = config::load(&env).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(r.get("history-limit"), Some("333"));
    let (_, _, source) = r
        .configured()
        .find(|(name, _, _)| *name == "history-limit")
        .expect("history-limit is configured");
    assert_eq!(source.layer, Layer::Settings);
    assert!(
        source
            .file
            .as_deref()
            .is_some_and(|f| f.ends_with("settings.toml"))
    );
}

/// T11: `show-config --explain` names settings.toml for the key it sets and
/// user.toml for one it does not.
#[test]
fn explain_names_settings_toml() {
    let (tmp, dir) = setup();
    let home = tmp.path().join("home");
    let run = tmp.path().join("run");
    let state = tmp.path().join("state");
    for d in [&run, &state] {
        fs::create_dir_all(d).expect("mkdir");
    }
    fs::write(
        dir.join("user.toml"),
        "history-limit = 111\nmouse = false\n",
    )
    .expect("user");
    fs::write(dir.join("settings.toml"), "history-limit = 222\n").expect("settings");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
    cmd.arg("-S")
        .arg(run.join("none.sock"))
        .args(["show-config", "--explain"])
        .current_dir(&home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env_remove("GROFE_CONFIG")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &run);
    let out = run_bounded(cmd, Duration::from_secs(10));
    assert!(out.status.success(), "{}", out.stderr);
    let lines: Vec<&str> = out.stdout.lines().collect();
    let after = |key: &str| {
        let i = lines
            .iter()
            .position(|l| l.starts_with(&format!("{key} = ")))
            .unwrap_or_else(|| panic!("no {key} in {}", out.stdout));
        lines[i].to_string() + "\n" + lines[i + 1]
    };
    let hl = after("history-limit");
    assert!(hl.contains("= 222"), "{hl}");
    assert!(hl.contains("settings.toml"), "{hl}");
    let mouse = after("mouse");
    assert!(mouse.contains("user.toml"), "{mouse}");
}
