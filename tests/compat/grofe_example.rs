//! The pattern for a compat test (FE tasks copy this): replay a scenario
//! against grofe, reached through a `tmux` symlink as the owner's scripts
//! will reach it, and require the normalized transcript to equal the golden
//! recorded from real tmux 3.7b. Ignored until grofe has the tmux front end
//! (FE.1); an FE task copies it, points it at its own scenario, and drops
//! the `#[ignore]` once its rows land.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use grofe::paths::{self, Env};

use crate::normalize::{Normalizer, fixtures_dir};
use crate::record::{SOCKET_NAME, fixed_env};
use crate::replay::{Cli, parse_scenario, replay};

/// grofe as `tmux`, with the recorder's fixed environment plus a private
/// runtime, state and config dir, so it never meets the user's servers.
fn grofe_cli(tmpdir: &Path) -> Cli {
    let bin = tmpdir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(tmpdir.join("home")).unwrap();
    let tmux = bin.join("tmux");
    symlink(env!("CARGO_BIN_EXE_grofe"), &tmux).unwrap();

    let dir = |name: &str| tmpdir.join(name).display().to_string();
    let mut env = fixed_env(tmpdir);
    for (key, value) in &mut env {
        if key == "PATH" {
            *value = format!("{}:{value}", bin.display());
        }
    }
    env.extend([
        ("XDG_RUNTIME_DIR".to_string(), dir("run")),
        ("XDG_STATE_HOME".to_string(), dir("state")),
        ("XDG_CONFIG_HOME".to_string(), dir("config")),
        ("GROFE_RESTORE_ON_START".to_string(), "0".to_string()),
    ]);
    let socket = paths::socket_path(
        &Env {
            xdg_runtime_dir: Some(dir("run")),
            tmpdir: None,
            xdg_state_home: Some(dir("state")),
            home: Some(dir("home")),
            uid: nix::unistd::getuid().as_raw(),
        },
        None,
        Some(SOCKET_NAME),
    );
    Cli {
        program: tmux,
        prefix: vec![
            "-L".into(),
            SOCKET_NAME.into(),
            "-f".into(),
            fixtures_dir()
                .join("tmux-defaults.conf")
                .display()
                .to_string(),
        ],
        env,
        cwd: tmpdir.join("home"),
        vars: vec![
            ("socket".into(), socket.display().to_string()),
            ("tmpdir".into(), tmpdir.display().to_string()),
        ],
    }
}

#[test]
#[ignore = "grofe has no tmux front end yet (FE.1); the pattern FE tasks copy"]
fn grofe_errors_match_tmux_golden() {
    let scenario = fixtures_dir().join("golden/tc_0b/errors.scenario");
    let steps = parse_scenario(&fs::read_to_string(&scenario).unwrap()).unwrap();
    let golden = fs::read_to_string(scenario.with_extension("golden")).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let got = replay(&grofe_cli(dir.path()), &steps).normalized(&Normalizer::load());
    assert_eq!(got, golden);
}
