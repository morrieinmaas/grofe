//! The tmux golden recorder (task TC.0b, spec §9). `record_goldens` is an
//! ignored test driven by `scripts/record-compat-goldens.sh`, which runs it
//! inside the `containers/compat/golden.Containerfile` image, as an ordinary
//! user and without network. It refuses to run outside a container or
//! against any tmux but 3.7b.
//!
//! Each `*.scenario` under `tests/fixtures/compat/golden/` is replayed (see
//! `replay.rs` for the step grammar) against a fresh tmux server and its
//! output written, normalized, to the sibling `*.golden`. `tmux
//! list-commands` goes to `list-commands.txt`.
//!
//! Hermetic by construction: every tmux runs with a cleared environment
//! (`env -i`, which also drops `TMUX`) plus [`fixed_env`], from its absolute
//! path, on a private `-L` socket under a per-scenario `TMUX_TMPDIR`, with
//! `-f tmux-defaults.conf`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{Cli, Step, find_scenarios, next_pid, parse_scenario, replay};

/// The only tmux the recorder accepts.
pub const TMUX_VERSION: &str = "tmux 3.7b";

/// The private `-L` socket name, under each scenario's `TMUX_TMPDIR`.
pub const SOCKET_NAME: &str = "golden";

/// Pids at or above this are 5 digits, so the `pid` rule cannot hit a
/// width, a count or an index. See [`raise_pids`].
const PID_FLOOR: u32 = 20000;

/// The whole environment of every tmux the recorder runs, pane shells
/// included (before tmux adds its own variables).
pub fn fixed_env(tmpdir: &Path) -> Vec<(String, String)> {
    [
        ("SHELL", "/bin/sh".to_string()),
        ("TERM", "xterm-256color".into()),
        ("LANG", "C.UTF-8".into()),
        ("PATH", "/usr/local/bin:/usr/bin:/bin".into()),
        ("HOME", tmpdir.join("home").display().to_string()),
        ("TMUX_TMPDIR", tmpdir.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// How the recorder runs real tmux for a scenario whose tempdir is `tmpdir`.
pub fn tmux_cli(tmux: &Path, tmpdir: &Path) -> Cli {
    let uid = nix::unistd::getuid();
    let socket = tmpdir.join(format!("tmux-{uid}")).join(SOCKET_NAME);
    Cli {
        program: tmux.to_path_buf(),
        prefix: vec![
            "-L".into(),
            SOCKET_NAME.into(),
            "-f".into(),
            fixtures_dir()
                .join("tmux-defaults.conf")
                .display()
                .to_string(),
        ],
        env: fixed_env(tmpdir),
        cwd: tmpdir.join("home"),
        vars: vec![
            ("socket".into(), socket.display().to_string()),
            ("tmpdir".into(), tmpdir.display().to_string()),
        ],
    }
}

/// Refuse any tmux whose `-V` is not exactly [`TMUX_VERSION`], naming what
/// it printed instead.
pub fn check_version(tmux: &Path) -> Result<(), String> {
    if !tmux.is_absolute() {
        return Err(format!(
            "tmux must be an absolute path, got {}",
            tmux.display()
        ));
    }
    let out = Command::new(tmux)
        .env_clear()
        .arg("-V")
        .output()
        .map_err(|e| format!("running {} -V: {e}", tmux.display()))?;
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if printed == TMUX_VERSION {
        Ok(())
    } else {
        Err(format!(
            "refusing to record: {} -V printed {printed:?}, goldens need {TMUX_VERSION:?}",
            tmux.display()
        ))
    }
}

fn in_container() -> bool {
    Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists()
}

/// `GROFE_COMPAT_TMUX`, else `/usr/local/bin/tmux` (where the image puts it).
fn tmux_path() -> PathBuf {
    std::env::var_os("GROFE_COMPAT_TMUX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/tmux"))
}

#[test]
#[ignore = "records goldens from real tmux 3.7b; run scripts/record-compat-goldens.sh"]
fn record_goldens() {
    assert!(
        in_container(),
        "refusing to record outside a container; run scripts/record-compat-goldens.sh"
    );
    let tmux = tmux_path();
    if let Err(e) = check_version(&tmux) {
        panic!("{e}");
    }
    raise_pids();

    let src = fixtures_dir().join("golden");
    let out = std::env::var_os("GROFE_COMPAT_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| src.clone());
    let only: Vec<String> = std::env::var("GROFE_COMPAT_SCENARIOS")
        .unwrap_or_default()
        .split_whitespace()
        .map(String::from)
        .collect();
    let normalizer = Normalizer::load();

    let list = record(
        &tmux,
        &normalizer,
        &[Step::Tmux {
            line: "$ list-commands".into(),
            argv: vec!["list-commands".into()],
            background: false,
        }],
    );
    let list = list
        .strip_prefix("$ list-commands\n")
        .and_then(|s| s.strip_suffix("? 0\n"))
        .unwrap_or_else(|| panic!("list-commands failed:\n{list}"))
        .to_string();
    write(&out.join("list-commands.txt"), &list);

    let mut scenarios = Vec::new();
    find_scenarios(&src, &mut scenarios);
    scenarios.sort();
    for path in scenarios {
        let rel = path.strip_prefix(&src).unwrap_or(&path).to_path_buf();
        let rel_str = rel.display().to_string();
        if !only.is_empty() && !only.iter().any(|o| rel_str.contains(o.as_str())) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel_str}: {e}"));
        let steps = parse_scenario(&text).unwrap_or_else(|e| panic!("{rel_str}: {e}"));
        let golden = record(&tmux, &normalizer, &steps);
        write(&out.join(rel.with_extension("golden")), &golden);
        eprintln!("recorded {rel_str}");
    }
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{}: {e}", parent.display()));
    }
    fs::write(path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// Replay `steps` on a fresh tmux server and return the normalized golden.
fn record(tmux: &Path, normalizer: &Normalizer, steps: &[Step]) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("home")).expect("home dir");
    replay(&tmux_cli(tmux, dir.path()), steps).normalized(normalizer)
}

/// A fresh container's pid namespace starts at 1, so tmux's pids would be
/// small numbers the `pid` rule could confuse with any other number. Burn
/// pids until they are 5 digits.
fn raise_pids() {
    while next_pid() < u64::from(PID_FLOOR) {}
}
