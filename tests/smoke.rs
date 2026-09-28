//! `grofe` binary builds and runs.

use std::process::Command;

#[test]
fn binary_runs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_grofe"))
        .env("HOME", tmp.path())
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .output()
        .expect("failed to run grofe binary");
    assert!(output.status.success());
}
