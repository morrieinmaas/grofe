//! FE.3: format, option, status and hook rows, against goldens in
//! `golden/fe_3/`. Each scenario is compared step by step: every step
//! must equal its golden except those a scenario's divergence list names,
//! each with its reason and owner, and a listed step must still differ
//! (so the list shrinks as the gaps close).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{Cli, Transcript, parse_scenario, replay};

/// A transcript split into steps: each step's first line (`$ ...`,
/// `client ...`, `screen ...`) with everything it recorded.
fn steps(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let starts = line.starts_with("$ ")
            || line.starts_with("$& ")
            || line.starts_with("client ")
            || line == "screen\n"
            || line.starts_with("screen ");
        match out.last_mut() {
            Some(step) if !starts => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// The grofe side of `fe_3/<name>` against its golden, after `fix`
/// (harness differences only), allowing the listed steps to differ.
fn check(name: &str, diverges: &[(&str, &str)], fix: fn(String) -> String) {
    let scenario = fixtures_dir().join(format!("golden/fe_3/{name}.scenario"));
    let parsed = parse_scenario(&fs::read_to_string(&scenario).unwrap()).unwrap();
    let golden = fs::read_to_string(scenario.with_extension("golden")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cli = with_client_ttys(grofe_cli(dir.path()), dir.path());
    let got = fix(exact_pids(replay(&cli, &parsed)).normalized(&Normalizer::load()));
    let (want, got) = (steps(&golden), steps(&got));
    assert_eq!(
        want.len(),
        got.len(),
        "fe_3/{name}: step count\n{}",
        got.join("")
    );
    let listed: BTreeMap<&str, &str> = diverges.iter().copied().collect();
    let mut wrong = Vec::new();
    let mut stale = Vec::new();
    for (w, g) in want.iter().zip(&got) {
        let head = w.lines().next().unwrap_or_default();
        match (listed.contains_key(head), w == g) {
            (false, false) => wrong.push(format!("--- golden\n{w}+++ grofe\n{g}")),
            (true, true) => stale.push(head.to_string()),
            _ => {}
        }
    }
    for head in listed.keys() {
        assert!(
            want.iter().any(|w| w.lines().next() == Some(head)),
            "fe_3/{name}: listed step {head:?} is not in the scenario"
        );
    }
    assert!(
        wrong.is_empty(),
        "fe_3/{name} differs:\n{}",
        wrong.join("\n")
    );
    assert!(
        stale.is_empty(),
        "fe_3/{name}: listed steps now match: {stale:#?}"
    );
}

/// `t` with its pids known exactly, in place of the replayer's pid window
/// (`normalize.txt`): the numbers that steps asking for a pid
/// (`#{pid}`, `#{pane_pid}`) printed. On a busy machine the window spans
/// thousands of pids, so a number such as `history-limit`'s `50000` can
/// fall inside it and read as a pid (the `fe_1/defaults` failure); and a
/// window whose pids wrapped past the maximum holds none at all.
pub fn exact_pids(mut t: Transcript) -> Transcript {
    let mut pids = Vec::new();
    let mut asks = false;
    for line in t.text.lines() {
        if line.starts_with("$ ") || line.starts_with("$& ") || line.starts_with("client ") {
            asks = line.contains("pid}");
            continue;
        }
        if asks && !line.starts_with("! ") && !line.starts_with("? ") {
            pids.extend(
                line.split(|c: char| !c.is_ascii_digit())
                    .filter_map(|n| n.parse::<u64>().ok()),
            );
        }
    }
    t.ctx.pids = None;
    t.ctx.exact_pids = pids;
    t
}

/// `cli` behind a wrapper that answers the replayer's attach check,
/// `list-clients -F '#{client_tty}'`, with the ttys of this run's attach
/// clients once the server lists a client, since grofe's `client_tty` is
/// still empty (TC.3 left it a stub for FE.2, which sends the tty in the
/// attach hello). Every other command reaches grofe as it is, without the
/// variables the wrapper's own shell adds (`SHLVL`, `OLDPWD`, `_`).
// ponytail: harness only; drop it once FE.2 fills client_tty.
fn with_client_ttys(mut cli: Cli, dir: &Path) -> Cli {
    let wrap = dir.join("wrap");
    fs::create_dir_all(&wrap).unwrap();
    let script = wrap.join("tmux");
    let grofe = cli.program.display().to_string();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             unset OLDPWD _\n\
             case \"$*\" in\n\
             *'list-clients -F #{{client_tty}}')\n\
             \x20 [ \"$('{grofe}' \"$@\" | wc -l)\" -gt 0 ] || exit 0\n\
             \x20 ps -A -o tty= -o args= | awk -v g='{grofe}' 'index($0, g \" \") && / attach/ {{ print \"/dev/\" $1 }}'\n\
             \x20 exit 0 ;;\n\
             esac\n\
             exec env -u SHLVL '{grofe}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    cli.program = script;
    cli
}

fn same(s: String) -> String {
    s
}

/// macOS gives a pane's cwd through `/private`, where the harness's
/// temporary directory really is; the container has no such link.
fn realpath(s: String) -> String {
    s.replace("/private<TMPDIR>", "<TMPDIR>")
}

/// Case 1, 6: the format vocabulary, short forms, escapes, `#()` and the
/// corpus's own format strings; `#{version}` is `3.7b`.
#[test]
fn formats_match_their_golden() {
    check("formats", FORMAT_GAPS, realpath);
}

const FORMAT_GAPS: &[(&str, &str)] = &[
    (
        "$ display -p -t main:1.1 '#{log_info}'",
        "native: tmux formats fall back to the session, then the global, \
         environment; grofe's FormatVars do not",
    ),
    (
        "$ display -p -t main:1.1 '#{pane_marked_set}'",
        "marked panes are out of scope (spec 4.5); tmux answers 0, grofe nothing",
    ),
    (
        "$ display -p -t main:1.1 '#{pane_marked}'",
        "marked panes are out of scope (spec 4.5); tmux answers 0, grofe nothing",
    ),
    (
        "$ display -p -t main:1 'X#{p5:session_name}X#{p-5:session_name}X'",
        "native: grofe's format language has no p: padding (TC.3); left for \
         the expander, which gives nothing",
    ),
    (
        "$ display -p -t main:1 '#{q:session_name}|#{E:@opt}|#{T:@opt}|#{=3:session_name}|#{s/a/A/:session_name}|#{n:session_name}'",
        "native: grofe's format language has no q:, E:, T:, =N:, s///: or n: \
         (TC.3); the front end leaves them, and notes them in compat-miss.log",
    ),
];

/// Case 2: canopy's two-row status theme, rendered on a pty-attached
/// client, cell for cell, with its `#()` aggregator fixed.
#[test]
fn the_canopy_status_matches_its_golden() {
    check("canopy", &[], same);
}

/// TC.13 regression: `#{pane_at_left}` and its siblings say which window
/// edges a pane touches, zoomed or not.
#[test]
fn pane_at_edges_match_their_golden() {
    check("pane-at", &[], same);
}

/// Case 3: `show` output, arrays and appends.
#[test]
fn show_options_match_their_golden() {
    check("show", SHOW_GAPS, same);
}

const SHOW_GAPS: &[(&str, &str)] = &[];

/// Case 4: each of the twelve hooks fires once (and `alert-bell`, since
/// TP.10, is set as tmux sets it), its output where tmux
/// puts it against its command's reply.
#[test]
fn hooks_match_their_golden() {
    check("hooks", HOOK_GAPS, same);
}

const HOOK_GAPS: &[(&str, &str)] = &[(
    "$ set-hook after-select-pane 'display x'",
    "refused on purpose: grofe's hooks are global only (TC.5)",
)];

/// A pane's one-word command runs through `default-shell` (new-window,
/// split-window, respawn-pane, new-session, `default-command`), several
/// words do not, and `run-shell` and `if-shell` use `/bin/sh` as `sh`.
#[test]
fn default_shell_matches_its_golden() {
    check("default-shell", &[], same);
}

/// With no command a pane starts `default-shell` as a login shell
/// (`argv[0]` `-sh`) in every verb; a command string does not.
#[test]
fn default_shell_login_matches_its_golden() {
    check("default-shell-login", &[], same);
}

/// Case 7: a hook whose command uses tmux-only forms runs them when it
/// fires.
#[test]
fn a_hook_s_tmux_forms_run_when_it_fires() {
    check("hook-forms", &[], same);
}

const ENV_GAPS: &[(&str, &str)] = &[];

/// Case 5: `show-environment -g` and the environment flags. The grofe
/// side's harness adds its own runtime directories and a `bin` dir on
/// `PATH` to the server's environment; those lines are dropped first.
#[test]
fn the_environment_matches_its_golden() {
    fn harness(s: String) -> String {
        s.lines()
            .filter(|l| !l.starts_with("XDG_") && !l.starts_with("GROFE_"))
            .map(|l| l.replace("PATH=<TMPDIR>/bin:", "PATH="))
            .map(|l| realpath(l.to_string()))
            .map(|l| format!("{l}\n"))
            .collect()
    }
    check("environment", ENV_GAPS, harness);
}

/// Every corpus form of FE.3's verbs (`docs/compat-coverage.md`'s Forms
/// table) translates, and each format-bearing argument is rewritten.
#[test]
fn every_fe3_corpus_form_translates() {
    use grofe::cmd::Command;
    use grofe::compat::tmux;
    use grofe::protocol::{ClientCtx, Dialect};
    let ctx = ClientCtx {
        cwd: std::path::PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    };
    let forms: &[&[&str]] = &[
        &["set-environment", "-g", "OPENSESSIONS_DIR", "/x"],
        &["set-environment", "-g", "-u", "OPENSESSIONS_WIDTH"],
        &["set-environment", "-h", "log_info", "x"],
        &["set-environment", "-t", "main", "SMUG_SESSION", "x"],
        &["set-hook", "-g", "after-select-pane", "display -p #S"],
        &["set-hook", "-g", "-u", "after-select-pane"],
        &[
            "set-option",
            "-F",
            "-g",
            "command-alias[200]",
            "wk=#{@wk_cmd_show}",
        ],
        &["set-option", "-a", "-g", "status-right", "#(x)"],
        &["set-option", "-a", "-s", "terminal-features", ",x:RGB"],
        &["set-option", "-g", "status-left", "#S"],
        &["set-option", "-g", "-q", "status-right", "#(a)#(b)"],
        &["set-option", "-g", "-u", "status-right"],
        &["set-option", "-p", "@a", "1"],
        &[
            "set-option",
            "-p",
            "-t",
            "%1",
            "pane-border-format",
            "#{@fzf-border-label}",
        ],
        &["set-option", "-s", "escape-time", "10"],
        &["set-option", "-t", "main", "@a", "1"],
        &["set-option", "-t", "main", "-u", "@a"],
        &[
            "set-window-option",
            "-g",
            "automatic-rename-format",
            "#{b:pane_current_path}",
        ],
        &[
            "set-window-option",
            "-t",
            "main:1",
            "automatic-rename",
            "on",
        ],
        &["show-environment", "-g", "OPENSESSIONS_DIR"],
        &["show-options", "-g", "status"],
        &["show-options", "-g", "-q", "@x"],
        &["show-options", "-g", "-q", "-v", "@tpm_plugins"],
        &["show-options", "-g", "-v", "status-right"],
        &["show-options", "-g", "-v", "-w", "main-pane-width"],
        &["show-options", "-g", "-w", "mode-keys"],
        &["show-options", "-p"],
        &["show-options", "-q", "-t", "%1", "-v", "@a"],
        &["show-options", "-s", "-v", "escape-time"],
        &["show-window-options"],
        &[
            "show-window-options",
            "-t",
            "main:1",
            "-v",
            "automatic-rename",
        ],
    ];
    for form in forms {
        let argv: Vec<String> = form.iter().map(|s| s.to_string()).collect();
        let got = tmux::dispatch_command(&argv, &ctx);
        let cmds = got.unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let text: String = cmds
            .iter()
            .map(|c| match c {
                Command::SetOption(set) => format!("{:?}", set.value),
                c => format!("{c:?}"),
            })
            .collect();
        // A hook keeps its command as sent, translated when it fires.
        let stored = form[0] == "set-hook";
        assert!(
            stored || (!text.contains("#S") && !text.contains("#(")),
            "{argv:?}: {text}"
        );
        if form.contains(&"#(a)#(b)") {
            let sources = cmds
                .iter()
                .filter(|c| matches!(c, Command::RegisterSource(_)))
                .count();
            assert_eq!(sources, 2, "{cmds:?}");
        }
    }
}

/// A pane title can never become a command: tmux 3.7b stores an OSC 2
/// `#(` as `_(` (recorded), so a title pulled into a status format
/// through `#{pane_title}` stays text. The reviewer's repro.
#[test]
fn a_pane_title_never_runs_as_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let marker = dir.path().join("X");
    let args = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let title = format!(
        "printf '\\033]2;#(touch {})\\033\\\\'; exec cat",
        marker.display()
    );
    let (status, _, err) = cli.run(&args(&["new-session", "-d", "-s", "t", &title]));
    assert!(status.success(), "{err}");
    let mut shown = String::new();
    for _ in 0..50 {
        shown = cli
            .run(&args(&["display", "-p", "-t", "t", "#{pane_title}"]))
            .1;
        if shown.contains("touch") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(shown.starts_with("_(touch "), "{shown:?}");
    let _ = cli.run(&args(&["set", "-g", "status-interval", "1"]));
    let _ = cli.run(&args(&["set", "-g", "status-right", shown.trim_end()]));
    let _ = cli.run(&args(&["set", "-gF", "status-left", "#{pane_title}"]));
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let _ = cli.run(&args(&["kill-server"]));
    assert!(!marker.exists(), "the title ran as a command");
}

/// The front end's copy of tmux's option tables
/// (`src/compat/tmux/tmux-options.txt`) is the recorded golden's.
#[test]
fn the_shipped_option_tables_are_the_golden_s() {
    let golden = fs::read_to_string(fixtures_dir().join("golden/fe_3/tmux-tables.golden")).unwrap();
    let data = fs::read_to_string(fixtures_dir().join("../../../src/compat/tmux/tmux-options.txt"))
        .unwrap();
    for (step, section) in [
        ("$ show -g\n", "[session]"),
        ("$ show -gw\n", "[window]"),
        ("$ show -s\n", "[server]"),
    ] {
        let at = golden.find(step).unwrap() + step.len();
        let want: Vec<&str> = golden[at..]
            .lines()
            .take_while(|l| !l.starts_with("? "))
            .collect();
        let got: Vec<&str> = data
            .lines()
            .skip_while(|l| *l != section)
            .skip(1)
            .take_while(|l| !l.starts_with('['))
            .collect();
        assert_eq!(got, want, "{section}");
    }
}

/// The root cause of the `fe_1/defaults` failure, pinned: with the pid
/// window spanning 50000, `history-limit`'s value reads as a pid; with
/// the pids known exactly it stays, and a real pid is still replaced.
#[test]
fn a_value_inside_the_pid_window_is_not_a_pid() {
    let text = "$ show -gqv history-limit\n50000\n? 0\n$ display -p '#{pid}'\n50123\n? 0\n";
    let ctx = crate::normalize::Context {
        pids: Some(49_000..=51_000),
        ..Default::default()
    };
    let t = Transcript {
        text: text.to_string(),
        ctx,
    };
    let normalizer = Normalizer::load();
    assert!(
        t.normalized(&normalizer)
            .starts_with("$ show -gqv history-limit\n<PID>\n")
    );
    let fixed = exact_pids(t).normalized(&normalizer);
    assert_eq!(
        fixed,
        "$ show -gqv history-limit\n50000\n? 0\n$ display -p '#{pid}'\n<PID>\n? 0\n"
    );
}
