//! `grofe` binary builds and runs.

use std::process::Command;

#[test]
fn binary_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_grofe"))
        .output()
        .expect("failed to run grofe binary");
    assert!(output.status.success());
}
