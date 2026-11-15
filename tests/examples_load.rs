//! The example configs in `examples/` load: `show-config --file` exits 0 and
//! never starts a server.

use std::path::Path;
use std::process::Command;

fn check_loads(file: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_grofe"))
        .arg("-S")
        .arg(tmp.path().join("none.sock"))
        .args(["show-config", "--file"])
        .arg(root.join(file))
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("GROFE")
        .env_remove("GROFE_PANE")
        .output()
        .expect("run grofe");
    assert!(
        out.status.success(),
        "{file}: show-config --file failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn pills_example_loads() {
    check_loads("examples/pills.toml");
}

#[test]
fn pills_dark_example_loads() {
    check_loads("examples/pills-dark.toml");
}
