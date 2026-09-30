//! FE.2: structure, client and listing rows, against goldens in
//! `golden/fe_2/`: tmux's layout strings (written by `#{window_layout}`,
//! read by `select-layout`), the structure verbs, the `list-*` default
//! output with `-a -s -t -f`, `list-clients` by tty, `has-session`'s exit
//! statuses, `info`, and a chain's output before its error. Each scenario
//! is compared step by step, so a failure names the step.

use std::fs;

use grofe::compat::tmux;
use grofe::protocol::{ClientCtx, Dialect};

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{parse_scenario, replay};

/// A transcript split into steps, each from its `$`/`client`/`screen`
/// line.
fn steps(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let starts = line.starts_with("$ ")
            || line.starts_with("$& ")
            || line.starts_with("client ")
            || line.starts_with("screen");
        match out.last_mut() {
            Some(step) if !starts => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// The grofe side of `fe_2/<name>` against its golden, step by step.
fn check(name: &str) {
    let scenario = fixtures_dir().join(format!("golden/fe_2/{name}.scenario"));
    let parsed = parse_scenario(&fs::read_to_string(&scenario).unwrap()).unwrap();
    let golden = fs::read_to_string(scenario.with_extension("golden")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let run = crate::fe_3::exact_pids(replay(&grofe_cli(dir.path()), &parsed));
    let got = run.normalized(&Normalizer::load());
    let (want, got) = (steps(&golden), steps(&got));
    let wrong: Vec<String> = want
        .iter()
        .zip(&got)
        .filter(|(w, g)| w != g)
        .map(|(w, g)| format!("--- golden\n{w}+++ grofe\n{g}"))
        .collect();
    assert!(
        wrong.is_empty(),
        "fe_2/{name} differs:\n{}",
        wrong.join("\n")
    );
    assert_eq!(want.len(), got.len(), "fe_2/{name}: step count");
}

/// Cases 1 and 2: after the same splits at 200x50 `#{window_layout}` is
/// tmux's string byte for byte; `select-layout` with the golden's own
/// string gives the golden geometry back, with a string naming other panes
/// puts the window's panes in its cells, and fails as tmux does.
#[test]
fn layout_strings_match_their_golden() {
    check("layout");
}

/// Case 3: join, swap (pane and window), rotate, break, move and
/// `select-layout -E`, each against its golden `list-panes -a -F`.
#[test]
fn structure_verbs_match_their_golden() {
    check("structure");
}

/// Case 4: `list-sessions`, `list-windows -a` and `list-panes -a` with no
/// `-F`, the `-s -t -f` forms, and `list-clients` (D16, D26) with a client
/// attached, found by its tty.
#[test]
fn lists_match_their_golden() {
    check("lists");
}

/// Cases 5 and 6: `has-session -t =x` with and without `x`, and `info`
/// with and without a server.
#[test]
fn sessions_and_info_match_their_golden() {
    check("sessions");
}

/// A chain's output before the command that fails it, then the error.
#[test]
fn chain_output_before_an_error_matches_its_golden() {
    check("chain");
}

/// The verbs whose rows are FE.2's.
const FE2_VERBS: &[&str] = &[
    "join-pane",
    "move-pane",
    "swap-pane",
    "swap-window",
    "break-pane",
    "rotate-window",
    "move-window",
    "last-pane",
    "next-layout",
    "resize-window",
    "respawn-window",
    "show-messages",
    "refresh-client",
    "choose-client",
    "suspend-client",
    "find-window",
    "list-sessions",
    "list-windows",
    "list-panes",
    "list-clients",
    "server-info",
];

/// Every coverage-doc form of FE.2's verbs translates.
#[test]
fn every_fe2_coverage_form_translates() {
    let doc = fs::read_to_string(fixtures_dir().join("../../../docs/compat-coverage.md")).unwrap();
    let forms = doc
        .split("\n### Forms\n")
        .nth(1)
        .and_then(|t| t.split("\n### ").next())
        .unwrap();
    let ctx = ClientCtx {
        cwd: "/".into(),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    };
    let mut failed = Vec::new();
    let mut seen = 0;
    for row in forms.lines().filter(|l| l.starts_with("| `")) {
        let form = row.split('`').nth(1).unwrap();
        let mut words = form.split(' ');
        let verb = words.next().unwrap_or_default();
        let Ok(spec) = tmux::resolve(verb) else {
            continue;
        };
        if !FE2_VERBS.contains(&spec.name) {
            continue;
        }
        seen += 1;
        let mut argv = vec![verb.to_string()];
        for w in words {
            let flag = &w[..2];
            argv.push(flag.to_string());
            let takes = spec.usage.contains(&format!("[{flag} "));
            if takes {
                argv.push(
                    if flag == "-x" || flag == "-y" {
                        "10"
                    } else {
                        "1"
                    }
                    .to_string(),
                );
            }
        }
        argv.extend(std::iter::repeat_n("1".to_string(), spec.min_args()));
        if let Err(e) = tmux::dispatch_command(&argv, &ctx) {
            failed.push(format!("{form}: {e}"));
        }
    }
    assert!(seen > 30, "only {seen} FE.2 forms");
    assert!(
        failed.is_empty(),
        "FE.2 forms that do not translate: {failed:#?}"
    );
}
