//! What the M3.7 plugin checks (TP.4 to TP.10) share: a scenario under
//! `golden/tp_<n>/`, recorded from tmux 3.7b in TC.0b's container with the
//! plugin's vendored copy (`tests/fixtures/canopy-port/plugins/`, which
//! `check-canopy-port.sh` keeps byte for byte equal to canopy's), replayed
//! on grofe run as `tmux` and compared step by step.

use std::collections::BTreeMap;
use std::fs;

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{parse_scenario, replay};

/// A transcript split into steps: each step's first line (`$ ...`,
/// `client ...`, `keys ...`, `screen ...`) with everything it recorded.
pub fn steps(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let starts = line.starts_with("$ ")
            || line.starts_with("$& ")
            || line.starts_with("client ")
            || line.starts_with("keys ")
            || line == "screen\n"
            || line.starts_with("screen ");
        match out.last_mut() {
            Some(step) if !starts => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// `golden/<task>/<name>` replayed on grofe against its golden: every step
/// equal, except the listed ones (by first line, each with its reason),
/// which must still differ.
pub fn check(task: &str, name: &str, diverges: &[(&str, &str)]) -> Replayed {
    check_with(task, name, diverges, |s| s.to_string(), |g| g.to_string())
}

/// What a [`check`] replayed: grofe's transcript, and the lines its
/// server wrote to `compat-miss.log` (without their timestamps).
pub struct Replayed {
    pub transcript: String,
    pub misses: Vec<String>,
}

/// The `compat-miss.log` lines of the grofe [`grofe_cli`] ran in `dir`,
/// each without its timestamp.
pub fn misses(dir: &std::path::Path) -> Vec<String> {
    fs::read_to_string(dir.join("state/grofe/compat-miss.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_once(' ').map_or(l, |(_, what)| what).to_string())
        .collect()
}

/// [`check`], with the scenario edited by `scenario` before grofe replays
/// it and the golden by `golden` before the comparison: for a step the
/// recording could not make (an Escape the container's tmux holds back).
pub fn check_with(
    task: &str,
    name: &str,
    diverges: &[(&str, &str)],
    scenario: impl Fn(&str) -> String,
    golden: impl Fn(&str) -> String,
) -> Replayed {
    let path = fixtures_dir().join(format!("golden/{task}/{name}.scenario"));
    let parsed = parse_scenario(&scenario(&fs::read_to_string(&path).unwrap())).unwrap();
    let golden = golden(
        &fs::read_to_string(path.with_extension("golden"))
            .unwrap_or_else(|e| panic!("{task}/{name}: no golden ({e}); record it")),
    );
    let dir = tempfile::tempdir().unwrap();
    // Exact pids, not the replayer's window, which can swallow a value such
    // as history-limit's 50000 on a busy machine.
    let got = crate::fe_3::exact_pids(replay(&grofe_cli(dir.path()), &parsed))
        .normalized(&Normalizer::load());
    let (want, steps) = (steps(&golden), steps(&got));
    assert_eq!(want.len(), steps.len(), "{task}/{name}: step count\n{got}");
    let listed: BTreeMap<&str, &str> = diverges.iter().copied().collect();
    let mut wrong = Vec::new();
    let mut stale = Vec::new();
    for (w, g) in want.iter().zip(&steps) {
        let head = w.lines().next().unwrap_or_default();
        match (listed.contains_key(head), w == g) {
            (false, false) => wrong.push(format!("--- tmux 3.7b\n{w}+++ grofe\n{g}")),
            (true, true) => stale.push(head.to_string()),
            _ => {}
        }
    }
    for head in listed.keys() {
        assert!(
            want.iter().any(|w| w.lines().next() == Some(head)),
            "{task}/{name}: listed step {head:?} is not in the scenario"
        );
    }
    assert!(
        wrong.is_empty(),
        "{task}/{name} differs:\n{}",
        wrong.join("\n")
    );
    assert!(
        stale.is_empty(),
        "{task}/{name}: listed steps now match: {stale:#?}"
    );
    Replayed {
        transcript: got,
        misses: misses(dir.path()),
    }
}
