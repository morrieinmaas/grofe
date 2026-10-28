//! tmux compatibility: the golden recorder (TC.0b), the shared scenario
//! replayer and normalization list, and the compat tests that compare grofe
//! with the goldens. Nothing here runs real tmux except the ignored
//! `record::record_goldens`, which `scripts/record-compat-goldens.sh` runs
//! inside a container.
//!
//! FE tasks write their golden tests in their own modules here (`fe_1` to
//! `fe_4`, stubbed by the M3.6 stub edit), next to `grofe_example`, which
//! shows the pattern; TC.13's scenarios live in `scenarios`.

mod b1c;
mod b1d;
mod b1e;
mod b2b;
mod fe_1;
mod fe_2;
mod fe_3;
mod fe_4;
mod g3;
mod grofe_example;
mod normalize;
mod record;
mod replay;
mod scenarios;
mod tp;
mod tp_1;
mod tp_10;
mod tp_4;
mod tp_5;
mod tp_6;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use normalize::fixtures_dir;
use record::check_version;
use replay::{Step, find_scenarios, parse_scenario, split_words};

fn fake_tmux(dir: &Path, prints: &str) -> PathBuf {
    let path = dir.join("tmux");
    fs::write(&path, format!("#!/bin/sh\necho '{prints}'\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn recorder_refuses_another_tmux_version_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let err = check_version(&fake_tmux(dir.path(), "tmux 3.4")).unwrap_err();
    assert!(err.contains("tmux 3.4"), "{err}");
    assert!(err.contains("tmux 3.7b"), "{err}");
    assert!(check_version(&fake_tmux(dir.path(), "tmux 3.7b")).is_ok());
    assert!(
        check_version(Path::new("tmux")).is_err(),
        "relative path accepted"
    );
}

#[test]
fn words_split_like_a_shell() {
    let words = split_words(r#"display -p '#{a} b' "c \"d\" \$e" f\;g \; h"#).unwrap();
    assert_eq!(
        words,
        ["display", "-p", "#{a} b", r#"c "d" $e"#, "f;g", ";", "h"]
    );
    assert!(split_words("'open").is_err());
}

#[test]
fn scenarios_parse() {
    let text = "# c\n\n$ ls -F '#S'\nclient 80x24 attach -t m\n$& popup\nscreen\nscreen a b\n";
    let steps = parse_scenario(text).unwrap();
    assert_eq!(steps.len(), 5);
    let tmux = |i: usize| match &steps[i] {
        Step::Tmux { background, .. } => Some(*background),
        _ => None,
    };
    assert_eq!((tmux(0), tmux(2)), (Some(false), Some(true)));
    assert!(matches!(
        &steps[1],
        Step::Client {
            cols: 80,
            rows: 24,
            ..
        }
    ));
    assert_eq!(steps[3], Step::Screen { until: None });
    assert_eq!(
        steps[4],
        Step::Screen {
            until: Some("a b".into())
        }
    );
    assert!(parse_scenario("frob\n").is_err());
    assert!(parse_scenario("client 80 attach\n").is_err());
}

#[test]
fn every_scenario_parses_and_has_its_golden() {
    let mut found = Vec::new();
    find_scenarios(&fixtures_dir().join("golden"), &mut found);
    assert!(!found.is_empty());
    for path in found {
        let text = fs::read_to_string(&path).unwrap();
        parse_scenario(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let golden = path.with_extension("golden");
        assert!(
            golden.exists(),
            "{} has no golden; record it",
            path.display()
        );
    }
    assert!(fixtures_dir().join("golden/list-commands.txt").exists());
}

/// Spec §9: fixtures use no strftime sequences.
#[test]
fn fixtures_hold_no_strftime() {
    let mut files = vec![fixtures_dir().join("tmux-defaults.conf")];
    find_scenarios(&fixtures_dir().join("golden"), &mut files);
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        let bytes = text.as_bytes();
        for w in bytes.windows(2) {
            assert!(
                !(w[0] == b'%' && w[1].is_ascii_alphabetic()),
                "{} holds a strftime sequence",
                path.display()
            );
        }
    }
}

/// A recorded pane's `env` holds only the recorder's fixed variables plus
/// what tmux and the pane's shell add.
#[test]
fn recorded_pane_env_is_hermetic() {
    let path = fixtures_dir().join("golden/tc_0b/pane-env.golden");
    let golden = fs::read_to_string(&path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let fixed: BTreeSet<String> = record::fixed_env(dir.path())
        .into_iter()
        .map(|(k, _)| k.to_string())
        .collect();
    let tmux_own = [
        "TMUX",
        "TMUX_PANE",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "COLORTERM",
        "PWD",
    ];
    let captured = golden
        .split("$ capture-pane -pJ -t env\n")
        .nth(1)
        .expect("golden has the capture-pane step");
    let mut keys = BTreeSet::new();
    for line in captured.lines().take_while(|l| !l.starts_with("? ")) {
        if let Some((key, _)) = line.split_once('=') {
            keys.insert(key.to_string());
        }
    }
    assert!(keys.contains("TMUX") && keys.contains("SHELL"), "{keys:?}");
    for key in &keys {
        assert!(
            fixed.contains(key) || tmux_own.contains(&key.as_str()),
            "pane env has {key}, which is neither fixed nor tmux's own"
        );
    }
}

/// Recorded as the user `golden`, so tmux's `root` key table reaches the
/// goldens intact instead of as `<USER>`.
#[test]
fn root_key_table_survives_normalization_in_goldens() {
    let golden =
        |name: &str| fs::read_to_string(fixtures_dir().join("golden/tc_0b").join(name)).unwrap();
    assert!(golden("display-formats.golden").contains("bind-key  -T root MouseDown1Pane"));
    assert!(golden("status-line.golden").contains("'#{client_key_table}'\nroot\n? 0\n"));
}
