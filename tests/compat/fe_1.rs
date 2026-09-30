//! FE.1: the front end's core rows (the invocation, aliases and prefixes,
//! targets, options by tmux name, `\;` chains, error texts) against
//! goldens in `golden/fe_1/`, and a replay of every corpus command whose
//! verb is from M1 to M3 through the front end.
//!
//! [`grofe_cli`] is the grofe side of a golden test: grofe reached through
//! a `tmux` symlink, as the owner's scripts will reach it. FE.2 to FE.4
//! reuse it. It passes `-f fe_1/grofe-defaults.toml`, grofe's spelling of
//! the spec §5.3 values `tmux-defaults.conf` gives the recorded tmux: run
//! as `tmux`, grofe's built-in defaults are tmux's own, and `-f` loads a
//! grofe TOML file, not a tmux.conf.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use grofe::compat::tmux;
use grofe::paths::{self, Env};
use grofe::protocol::{ClientCtx, Dialect};

use crate::normalize::{Normalizer, fixtures_dir};
use crate::record::{SOCKET_NAME, fixed_env};
use crate::replay::{Cli, parse_scenario, replay, split_words};

/// grofe as `tmux`, with the recorder's fixed environment plus a private
/// runtime, state and config dir, so it never meets the user's servers.
pub fn grofe_cli(tmpdir: &Path) -> Cli {
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
                .join("golden/fe_1/grofe-defaults.toml")
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

/// Replays `fe_1/<name>.scenario` against grofe and compares with its
/// golden.
fn check_golden(name: &str) {
    let scenario = fixtures_dir().join(format!("golden/fe_1/{name}.scenario"));
    let steps = parse_scenario(&fs::read_to_string(&scenario).unwrap()).unwrap();
    let golden = fs::read_to_string(scenario.with_extension("golden")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let run = crate::fe_3::exact_pids(replay(&grofe_cli(dir.path()), &steps));
    let got = run.normalized(&Normalizer::load());
    assert_eq!(got, golden, "fe_1/{name} differs from its golden");
}

/// Case 1, 2: `-V` and the no-effect `-2 -u`, aliases and prefixes, the
/// ambiguous-prefix error, `list-commands` (with a name, with `-F`, bare).
#[test]
fn invocation_matches_its_golden() {
    check_golden("invocation");
}

/// Case 3, 4: `=x` never matches `xy`, `=x:2` is window 2 of exactly `x`,
/// `-t =` errors naming the mouse target, `:=N`, and offsets.
#[test]
fn targets_match_their_golden() {
    check_golden("targets");
}

/// Case 5: `set -gw`, `show -gqv`, the chained `set -p`, `setw`/`showw`,
/// options grofe stores without acting on them.
#[test]
fn options_match_their_golden() {
    check_golden("options");
}

/// Case 6: `display -p` in the middle of a `\;` chain.
#[test]
fn chains_match_their_golden() {
    check_golden("chain");
}

#[test]
fn output_before_a_failing_command_matches_its_golden() {
    check_golden("chain-error");
}

#[test]
fn errors_match_their_golden() {
    check_golden("errors");
}

/// Options through tmux's server and global tables, a second `-f`, and
/// `kill-window -a`.
#[test]
fn defaults_match_their_golden() {
    check_golden("defaults");
}

/// The FE.1 ruling: run as `tmux` with no config at all, grofe's built-in
/// defaults are tmux 3.7b's where they differ (recorded on `-L grofe-fe1
/// -f /dev/null` with a clean environment); run as `grofe`, its own.
#[test]
fn a_tmux_server_has_tmux_s_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let mut cli = grofe_cli(dir.path());
    cli.prefix.truncate(2);
    let args = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (status, _, err) = cli.run(&args(&["new-session", "-d", "-s", "d"]));
    assert!(status.success(), "{err}");
    for (name, tmux) in [
        ("base-index", "0"),
        ("pane-base-index", "0"),
        ("mouse", "off"),
        ("set-clipboard", "external"),
        ("focus-events", "off"),
        ("escape-time", "10"),
        ("history-limit", "2000"),
        ("extended-keys", "off"),
        ("mode-keys", "emacs"),
    ] {
        let (_, out, _) = cli.run(&args(&["show", "-gv", name]));
        assert_eq!(out.trim_end(), tmux, "{name}");
    }
    let (_, out, _) = cli.run(&args(&["list-windows", "-t", "d", "-F", "#{window_index}"]));
    assert_eq!(out, "0\n", "the first window at tmux's base-index");
    let _ = cli.run(&args(&["kill-server"]));
}

/// `tmux attach -t =name` reads its target the tmux way (exact, and a
/// bare `=` the mouse target).
#[test]
fn attach_matches_its_golden() {
    check_golden("attach");
}

/// Native flags the corpus sends (`new-session -n -P -F`, `split-window -l
/// N%`, `display -c`, `run-shell -d`) and flags tmux lacks.
#[test]
fn flags_match_their_golden() {
    check_golden("flags");
}

/// `-f` with a missing file or `/dev/null`, and `start-server` running the
/// rest of its chain.
#[test]
fn startup_matches_its_golden() {
    check_golden("startup");
}

/// `tmux -f <file>` is the only config of the server it starts: the
/// user's `config.toml` is not read, and a later `-f` does not change a
/// running server, as in tmux 3.7b.
#[test]
fn tmux_f_is_the_only_config_of_the_server_it_starts() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let config = dir.path().join("config/grofe");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "[status]\nleft = \"HOME\"\n").unwrap();
    let file = dir.path().join("only.toml");
    fs::write(&file, "[status]\nleft = \"FILE\"\n").unwrap();
    let other = dir.path().join("other.toml");
    fs::write(&other, "[status]\nleft = \"OTHER\"\n").unwrap();
    let args = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let f = file.display().to_string();
    let (status, _, err) = cli.run(&args(&["-f", &f, "new-session", "-d", "-s", "main"]));
    assert!(status.success(), "{err}");
    let o = other.display().to_string();
    let (_, out, _) = cli.run(&args(&["-f", &o, "show", "-gv", "status-left"]));
    assert_eq!(out, "FILE\n");
    let _ = cli.run(&args(&["kill-server"]));

    let (status, _, err) = cli.run(&args(&["-f", "/dev/null", "new-session", "-d"]));
    assert!(status.success(), "{err}");
    let (_, out, _) = cli.run(&args(&["show", "-gv", "status-left"]));
    assert_eq!(
        out, "[#{session_name}] \n",
        "built-in default, not config.toml"
    );
    let _ = cli.run(&args(&["kill-server"]));
}

/// Case 1: `-V` is decided by `argv[0]`, with no server: the `tmux` name
/// prints tmux's compat level, `grofe` grofe's own version.
#[test]
fn tmux_v_prints_the_compat_level_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let (status, out, err) = cli.run(&["-V".to_string()]);
    assert!(status.success(), "{err}");
    assert_eq!(out, "tmux 3.7b\n");
    let grofe = Process::new(env!("CARGO_BIN_EXE_grofe"))
        .arg("-V")
        .env_clear()
        .envs(cli.env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    assert!(grofe.status.success());
    assert_eq!(
        String::from_utf8_lossy(&grofe.stdout),
        format!("grofe {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Case 2: `tmux a` reaches the attach path as `attach-session`.
#[test]
fn tmux_a_attaches() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let args = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (status, _, err) = cli.run(&args(&["new-session", "-d", "-s", "main"]));
    assert!(status.success(), "{err}");
    let (status, _, err) = cli.run(&args(&["a", "-t", "nope"]));
    assert!(!status.success());
    assert_eq!(err, "can't find session: nope\n");
    let _ = cli.run(&args(&["kill-server"]));
}

// ---- Case 7: every M1 to M3 corpus command translates ----------------------

/// The verbs whose rows are FE.1's: M1 to M3, plus `setw`, `showw` and
/// `list-commands`.
const FE1_VERBS: &[&str] = &[
    "new-session",
    "kill-session",
    "has-session",
    "list-sessions",
    "split-window",
    "kill-pane",
    "select-pane",
    "resize-pane",
    "respawn-pane",
    "list-panes",
    "new-window",
    "kill-window",
    "select-window",
    "list-windows",
    "send-keys",
    "capture-pane",
    "set-option",
    "show-options",
    "set-window-option",
    "show-window-options",
    "display-message",
    "kill-server",
    "start-server",
    "attach-session",
    "detach-client",
    "next-window",
    "previous-window",
    "last-window",
    "rename-session",
    "rename-window",
    "switch-client",
    "send-prefix",
    "list-clients",
    "bind-key",
    "unbind-key",
    "list-keys",
    "source-file",
    "command-prompt",
    "run-shell",
    "select-layout",
    "list-commands",
];

/// M1 to M3 verb forms that do not translate yet, each with the task that
/// owns the missing piece. Every other one must translate.
const PENDING: &[(&str, &str)] = &[(
    "send-keys -N",
    "M5: a repeat count in copy-mode tables (M36-R8)",
)];

/// Corpus lines whose placeholders stand for more than one word, or that
/// are a shell alias the caller completes, so they cannot be made
/// concrete: each with what the placeholder hides.
const ARTIFACTS: &[(&str, &str)] = &[
    (
        "<$window_filter>",
        "`-a` or `-t <session>`, chosen at run time",
    ),
    ("new-session -A -s", "an alias; the caller appends the name"),
];

/// `line` with its placeholders (`<...>`, `%N`, `@N`) made concrete.
fn concrete(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('<') {
        let end = rest[start..].find('>').map(|e| start + e);
        match end {
            Some(end) if !rest[start + 1..end].contains([' ', '<']) && end > start + 1 => {
                out.push_str(&rest[..start]);
                out.push('1');
                rest = &rest[end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.replace("%N", "%1").replace("@N", "@1")
}

fn ctx() -> ClientCtx {
    ClientCtx {
        cwd: PathBuf::from("/"),
        pane: None,
        env: Vec::new(),
        client: None,
        dialect: Dialect::Tmux,
    }
}

/// The commands a corpus line sends: its top-level commands (a CLI line
/// split as a shell and tmux's CLI split it, after the global flags; a
/// config line as tmux's config language splits it) and, for `bind-key`,
/// the commands it stores.
fn commands_of(line: &str, conf: bool) -> Vec<Vec<String>> {
    let line = concrete(line);
    let cmds = if conf {
        grofe::cmd::split_command_string(&line).unwrap_or_default()
    } else {
        let Ok(words) = split_words(&line) else {
            return Vec::new();
        };
        match tmux::cli(&words) {
            Ok(cli) if cli.version => return Vec::new(),
            Ok(cli) => grofe::cmd::parse::split_argv(&cli.argv),
            Err(_) => return Vec::new(),
        }
    };
    let mut out = Vec::new();
    for cmd in cmds {
        out.extend(stored(&cmd));
        out.push(cmd);
    }
    out
}

/// The commands a `bind-key` stores: after its flags and key, one command
/// string, or one command's argv split at `;`.
fn stored(cmd: &[String]) -> Vec<Vec<String>> {
    if tmux::form_of(cmd).is_none_or(|f| !f.starts_with("bind-key")) {
        return Vec::new();
    }
    let mut i = 1;
    while let Some(w) = cmd.get(i) {
        match w.as_str() {
            "-T" | "-N" => i += 2,
            w if w.starts_with('-') && w.len() > 1 => i += 1,
            _ => break,
        }
    }
    match cmd.get(i + 1..).unwrap_or_default() {
        [] => Vec::new(),
        [one] => grofe::cmd::split_command_string(one).unwrap_or_default(),
        many => grofe::cmd::parse::split_argv(many),
    }
}

/// Every command in the six corpus files, placeholders made concrete,
/// the [`ARTIFACTS`] lines left out.
fn corpus_commands() -> Vec<Vec<String>> {
    let corpus = fixtures_dir().join("../../compat/corpus");
    let mut out = Vec::new();
    for file in [
        "static-cli",
        "static-conf",
        "container-cli",
        "container-pane",
        "container-job",
        "container-stored",
    ] {
        let text = fs::read_to_string(corpus.join(format!("{file}.argv"))).unwrap();
        let conf = matches!(file, "static-conf" | "container-stored");
        for line in text.lines() {
            let artifact = ARTIFACTS
                .iter()
                .any(|(a, _)| line.contains(a) && (a.starts_with('<') || line == *a));
            if !artifact {
                out.extend(commands_of(line, conf));
            }
        }
    }
    out
}

/// Whether the table refuses the command on purpose: its row refuses it,
/// for good or until a later milestone.
fn unsupported(cmd: &[String]) -> bool {
    tmux::row_for(cmd).is_some_and(|r| {
        matches!(
            r.target,
            tmux::GrofeTarget::Unsupported(_) | tmux::GrofeTarget::Refuse(_)
        )
    })
}

#[test]
fn every_m1_to_m3_corpus_command_translates() {
    let pending: BTreeMap<&str, &str> = PENDING.iter().copied().collect();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    let mut seen_pending = BTreeSet::new();
    let mut translated = BTreeSet::new();
    let mut count = 0;
    for cmd in corpus_commands() {
        let Some(form) = tmux::form_of(&cmd) else {
            continue;
        };
        let verb = form.split(' ').next().unwrap_or_default();
        if !FE1_VERBS.contains(&verb) {
            continue;
        }
        count += 1;
        match tmux::dispatch_command(&cmd, &ctx()) {
            Ok(_) => {
                translated.insert(form);
            }
            Err(_) if unsupported(&cmd) => {}
            Err(e) if pending.contains_key(form.as_str()) => {
                if seen_pending.insert(form.clone()) {
                    println!("pending {form}: {e}");
                }
            }
            Err(e) => {
                failed.entry(form).or_insert(format!("{cmd:?}: {e}"));
            }
        }
    }
    assert!(count > 500, "only {count} commands replayed");
    assert!(failed.is_empty(), "untranslated M1-M3 forms: {failed:#?}");
    println!(
        "FE.1 corpus replay: {count} commands, {} forms translated, {} pending",
        translated.len(),
        seen_pending.len()
    );
}

/// Every option the corpus sets or shows is in the one option-name table.
#[test]
fn every_corpus_option_is_in_the_option_table() {
    let doc = fs::read_to_string(fixtures_dir().join("../../../docs/compat-coverage.md")).unwrap();
    let table = doc
        .split("### Options by tmux name")
        .nth(1)
        .and_then(|t| t.split("\n### ").next())
        .expect("the coverage doc has its options table");
    let mut names = 0;
    for row in table.lines().filter(|l| l.starts_with("| `")) {
        let name = row.split('`').nth(1).unwrap().trim_end_matches("[N]");
        assert!(
            tmux::option_name(name).is_some(),
            "{name} is not in OPTION_NAMES"
        );
        names += 1;
    }
    assert!(names >= 42, "only {names} options in the coverage table");
}

/// Where a coverage-table form stands in the front end today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    Mapped,
    /// An `Unsupported` row, or out of scope in the coverage doc.
    Unsupported,
    /// Not translated yet: a later FE task's row, or a flag natively
    /// missing.
    Pending,
}

/// A command built from a coverage-table form: the verb, each flag (a
/// value flag with `1`, `-t[=]` with `=`, `-x[M]` with `M`), then as many
/// `1`s as the verb needs positionals.
fn form_argv(form: &str) -> Option<Vec<String>> {
    let mut words = form.split(' ');
    let verb = words.next()?;
    let spec = tmux::resolve(verb).ok()?;
    let mut argv = vec![verb.to_string()];
    for w in words {
        let w = w.trim_end_matches('!');
        let flag = w.strip_prefix('-')?.chars().next()?;
        argv.push(format!("-{flag}"));
        match w.find('[') {
            Some(at) => argv.push(w[at + 1..w.len() - 1].to_string()),
            None if spec_takes_value(spec, flag) => argv.push("1".to_string()),
            None => {}
        }
    }
    argv.extend(std::iter::repeat_n("1".to_string(), spec.min_args()));
    Some(argv)
}

fn spec_takes_value(spec: &tmux::Spec, flag: char) -> bool {
    spec.usage.contains(&format!("[-{flag} ")) || (spec.name == "select-pane" && flag == 'P')
}

/// Every form in `docs/compat-coverage.md` classified: mapped (the form
/// translates), unsupported (an `Unsupported` row or an out-of-scope row),
/// or pending. FE.1's own verbs are never pending except for `PENDING`'s
/// forms. The counts go to the FE.1 report (`--nocapture`).
#[test]
fn every_coverage_form_is_classified() {
    let doc = fs::read_to_string(fixtures_dir().join("../../../docs/compat-coverage.md")).unwrap();
    let forms = doc
        .split("\n### Forms\n")
        .nth(1)
        .and_then(|t| t.split("\n### ").next())
        .unwrap();
    // Each form's result from the real corpus commands, a success first;
    // a form only a stored string holds is built from its flags.
    let mut replayed: BTreeMap<String, Result<(), tmux::TmuxError>> = BTreeMap::new();
    for cmd in corpus_commands() {
        let Some(form) = tmux::form_of(&cmd) else {
            continue;
        };
        let got = tmux::dispatch_command(&cmd, &ctx()).map(|_| ());
        let entry = replayed.entry(form).or_insert_with(|| got.clone());
        if got.is_ok() {
            *entry = got;
        }
    }
    let mut counts: BTreeMap<Standing, usize> = BTreeMap::new();
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    for row in forms.lines().filter(|l| l.starts_with("| `")) {
        let form = row.split('`').nth(1).unwrap();
        let spec_row = row.split(" | ").nth(4).unwrap_or_default();
        let result = replayed.get(form).cloned().or_else(|| {
            form_argv(form).map(|argv| tmux::dispatch_command(&argv, &ctx()).map(|_| ()))
        });
        let standing = match result {
            Some(Ok(_)) => Standing::Mapped,
            _ if spec_row.starts_with("oos:") => Standing::Unsupported,
            Some(Err(_)) if form_argv(form).is_some_and(|argv| unsupported(&argv)) => {
                Standing::Unsupported
            }
            _ => Standing::Pending,
        };
        let verb = form.split(' ').next().unwrap_or_default();
        let listed = PENDING.iter().any(|(f, _)| *f == form);
        if standing == Standing::Pending && FE1_VERBS.contains(&verb) && !listed {
            unexpected.push(form.to_string());
        }
        if listed && standing != Standing::Pending {
            stale.push(form.to_string());
        }
        *counts.entry(standing).or_default() += 1;
    }
    println!("FE.1 coverage forms: {counts:?}");
    assert_eq!(counts.values().sum::<usize>(), 287);
    assert!(
        unexpected.is_empty(),
        "FE.1 verbs' forms that neither map nor are PENDING: {unexpected:#?}"
    );
    assert!(stale.is_empty(), "PENDING forms that map now: {stale:?}");
}
