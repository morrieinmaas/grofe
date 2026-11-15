//! `show-config --explain` output, pinned byte for byte (WS9): the option
//! table is refactored under it, and its order, spelling and provenance
//! lines must not move. Set `UPDATE_PINS=1` to rewrite the pin.

mod common;

use std::fs;
use std::process::Command;
use std::time::Duration;

use common::run_bounded;

const PIN: &str = "tests/fixtures/config/show-config-explain.txt";

#[test]
fn show_config_explain_is_pinned() {
    let tmp = tempfile::Builder::new()
        .prefix("grofe-pin")
        .tempdir()
        .expect("tempdir");
    let home = tmp.path().join("home");
    let dir = home.join(".config").join("grofe");
    let run = tmp.path().join("run");
    let state = tmp.path().join("state");
    for d in [&dir, &run, &state] {
        fs::create_dir_all(d).expect("mkdir");
    }
    fs::write(
        dir.join("config.toml"),
        "mouse = false\nhistory-limit = 1234\nprefix = \"C-a\"\n\
         [status]\nlines = 2\nright = \"hi\"\n\
         [message]\npill = true\n[pane-border]\nstyle = \"fg=red\"\n",
    )
    .expect("config");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
    cmd.arg("-S")
        .arg(run.join("none.sock"))
        .args(["show-config", "--explain"])
        .current_dir(&home)
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &run);
    let out = run_bounded(cmd, Duration::from_secs(10));
    assert!(out.status.success(), "{}", out.stderr);

    let got = out
        .stdout
        .replace(home.to_str().expect("utf8 path"), "<home>");
    let pin = format!("{}/{PIN}", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("UPDATE_PINS").is_some() {
        fs::write(&pin, &got).expect("write pin");
    }
    let want = fs::read_to_string(&pin).expect("pin file");
    assert!(got == want, "show-config --explain drifted from {PIN}");
}
