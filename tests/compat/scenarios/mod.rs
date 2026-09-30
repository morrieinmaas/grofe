//! TC.13: the compat acceptance suite (plan M3.6, Wave D; spec §9's compat
//! bullet). It proves the tmux front end against the owner's own scripts
//! and every recorded corpus line, with `tmux` on a temp `PATH` pointing at
//! grofe and the Wave C port as its config: athome's `config.toml`,
//! canopy's `grofe/conf.d` and athome's rendered `user.toml`, in the temp
//! `XDG_CONFIG_HOME`. On the tmux side each scenario sources canopy's own
//! tmux entry point over the recorder's `tmux-defaults.conf`.
//!
//! canopy and athome are read from the checkouts `GROFE_COMPAT_CANOPY` and
//! `GROFE_COMPAT_ATHOME` name, which must sit at the commits in
//! `golden/tc_13/PINS`; with either unset the suite is skipped, loudly.
//!
//! [`tc_13_suite`] runs, in order, and reports every failure at the end:
//!
//! 1. the smoke check: `command -v tmux` through the constructed `PATH`
//!    (and from a run-shell job) is the grofe symlink, `tmux -V` prints
//!    the compat level;
//! 2. each `golden/tc_13/*.scenario` (canopy-agent, canopy-status, tsess,
//!    cockpit, nav, smug) against its golden, recorded from tmux 3.7b in
//!    TC.0b's container: byte for byte per step, except the steps in
//!    [`KNOWN`], each a grofe gap named for its owner;
//! 3. the checks that cannot have a golden because the container has no
//!    fzf or Neovim ([`live`]): tsess's picker through a real `fzf --tmux`
//!    on an attached pty client, and vim-tmux-navigator's call from a
//!    headless Neovim. tmux is not in the loop there, so they assert
//!    structure (the selection lands, the focus moves), as each says;
//! 4. the corpus replay ([`corpus`]): every line of the six `*.argv` files
//!    through the front end, with no `unknown command` or `not
//!    implemented`, and `compat-miss.log` holding only the documented
//!    [`corpus::MISS_ALLOWED`] lines;
//! 5. a changed golden byte turns a scenario red;
//!
//! and compares the user's real tmux and grofe socket directories before
//! and after the whole run.
//!
//! The scenarios reach canopy and athome as
//! `{fixtures}/../../compat/scenarios/trees/<repo>`: for recording, those
//! trees are the pinned commits exported into `tests/compat/scenarios/trees`
//! (git-ignored); here they are rewritten to the checkouts. The path keeps
//! off the word `golden`, the recorder's user name, which normalization
//! would replace.

mod corpus;
mod live;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use grofe::paths::{self, Env};

use crate::fe_1::grofe_cli;
use crate::fe_3::exact_pids;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{Cli, Step, find_scenarios, parse_scenario, replay};

/// Where the scenarios name the canopy and athome trees.
const CANOPY_TREE: &str = "{fixtures}/../../compat/scenarios/trees/canopy";
const ATHOME_TREE: &str = "{fixtures}/../../compat/scenarios/trees/athome";

/// The one step that runs on tmux only: canopy's tmux entry point. On
/// grofe the port is already loaded from `XDG_CONFIG_HOME`, so the step is
/// `start-server`, which prints nothing; its golden must be `? 0` alone,
/// so a load error on the tmux side cannot hide behind the swap.
const TMUX_ONLY: &str =
    "$ source-file {fixtures}/../../compat/scenarios/trees/canopy/tmux/tmux.conf";

/// athome's tsess puts this directory first on its own `PATH`. A `tmux`
/// there is a real tmux, which the script would call instead of grofe, so
/// where one exists the tsess scenario is skipped, loudly, and the fzf
/// check runs tsess's picker call without the script: real tmux runs only
/// in the golden container.
pub const TSESS_PATH_TMUX: &str = "/opt/nanobrew/prefix/bin/tmux";

/// Scenario steps (by scenario and first line) where grofe is known to
/// differ from tmux 3.7b, each with the gap and its owner. A listed step
/// must still differ, so a fix shows up as a stale entry.
const KNOWN: &[(&str, &str, &str)] = &[
    (
        "canopy-agent",
        r#"$ run-shell 'echo "TMUX ${TMUX:+set} TMUX_PANE ${TMUX_PANE:-unset}"'"#,
        JOB_TMUX,
    ),
    (
        "cockpit",
        r##"$ new-window -c "#{pane_current_path}" \; split-window -h -c "#{pane_current_path}" \; split-window -v -c "#{pane_current_path}" \; select-layout main-vertical \; select-pane -t 1"##,
        BARE_PANE_INDEX,
    ),
    (
        "cockpit",
        "$ display-message -p '#{session_name}:#{window_index}.#{pane_index} #{window_panes} panes'",
        BARE_PANE_INDEX,
    ),
    (
        "cockpit",
        "$ list-panes -t main:2 -F '#{pane_index} #{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height} active=#{pane_active}'",
        BARE_PANE_INDEX,
    ),
    (
        "nav",
        "$ display-message -p '#{pane_id} #{pane_at_left} #{pane_at_right}'",
        PANE_AT,
    ),
    (
        "nav",
        "$ display-message -p '#{pane_id} #{pane_at_left}'",
        PANE_AT,
    ),
];

/// tmux 3.7b gives every job `$TMUX` (`environ_for_session`); grofe's
/// run-shell jobs get only `$GROFE`. Scripts still reach grofe through its
/// own variable, but anything that tests `$TMUX` sees no tmux: fzf's
/// `--tmux` decides from `$TMUX`, so `bind K run-shell tsess` would open
/// no floating pane. Owner: run-shell's job env (TC.5; TC.1 set it in panes).
const JOB_TMUX: &str = "run-shell jobs have no $TMUX (TC.5, run-shell's environment)";

/// canopy's `prefix c` ends with `select-pane -t 1`; tmux reads a bare
/// number as a pane index in the current window, grofe as a session name,
/// so the chain stops at `can't find session: 1` with the last pane
/// focused. Owner: FE.1's target grammar.
const BARE_PANE_INDEX: &str =
    "`select-pane -t 1` resolves 1 as a session, not the pane index (FE.1 targets)";

/// `#{pane_at_left}` and its siblings are empty: no inventory row names
/// them (docs/compat-coverage.md, "Variable named: no"). The navigator's
/// no-wrap mode reads them; the owner does not set it. Owner: TC.3.
const PANE_AT: &str = "#{pane_at_left} and #{pane_at_right} are empty (TC.3 inventory)";

/// The canopy and athome checkouts under test.
pub struct Checkouts {
    pub canopy: PathBuf,
    pub athome: PathBuf,
}

/// Both checkouts, or `None` (said loudly) when either variable is unset.
/// A checkout at another commit than `PINS` is an error: the goldens were
/// recorded against those.
fn checkouts() -> Option<Checkouts> {
    let get = |key: &str| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let (Some(canopy), Some(athome)) = (get("GROFE_COMPAT_CANOPY"), get("GROFE_COMPAT_ATHOME"))
    else {
        eprintln!(
            "SKIPPED tc_13: set GROFE_COMPAT_CANOPY and GROFE_COMPAT_ATHOME to canopy and \
             athome checkouts at the commits in tests/fixtures/compat/golden/tc_13/PINS"
        );
        return None;
    };
    let pins = fs::read_to_string(fixtures_dir().join("golden/tc_13/PINS")).unwrap();
    for (name, dir) in [("canopy", &canopy), ("athome", &athome)] {
        let want = pins
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name} ")))
            .unwrap_or_else(|| panic!("PINS has no {name} line"));
        // An exported tree (the recording's, in the golden container) has
        // no history to check; it is the pinned commit by construction.
        if !dir.join(".git").exists() {
            eprintln!(
                "tc_13: {} is not a git checkout, so its commit is not checked against PINS",
                dir.display()
            );
            continue;
        }
        let head = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap_or_else(|e| panic!("git rev-parse in {}: {e}", dir.display()));
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_eq!(
            head,
            want,
            "{name} checkout {} is not at the pinned commit; re-record tc_13 to move the pin",
            dir.display()
        );
    }
    Some(Checkouts { canopy, athome })
}

/// athome's `user.toml.tmpl` rendered as `check-canopy-port.sh` renders it
/// without chezmoi: the lookPath block becomes `/bin/zsh`, the home dir
/// `~`.
fn render_user_toml(template: &str) -> String {
    let block = "{{ with lookPath \"zsh\" -}}\ndefault-shell = \"{{ . }}\"\n{{ end -}}\n";
    assert!(
        template.contains(block),
        "user.toml.tmpl's lookPath block moved"
    );
    let text = template
        .replace(block, "default-shell = \"/bin/zsh\"\n")
        .replace("{{ .chezmoi.homeDir }}", "~");
    assert!(
        !text.contains("{{"),
        "a chezmoi directive is left unrendered"
    );
    text
}

/// grofe as `tmux` (FE.1's [`grofe_cli`]: the recorder's fixed env, private
/// runtime, state and config dirs), with the Wave C port as its config:
/// athome's `config.toml`, canopy's fragments and athome's `user.toml` in
/// the temp config dir, and canopy
/// where `config.toml`'s `[environment]` says it is installed. Commands
/// the corpus's hooks and bindings would reach the network or root with
/// (`curl`, `git`, `sudo` and the like) are stubs that fail.
pub fn tc13_cli(dir: &Path, co: &Checkouts) -> Cli {
    let mut cli = grofe_cli(dir);
    // tmux's `-f` names the one config a server loads, and grofe as `tmux`
    // keeps that meaning, so FE.1's `-f grofe-defaults.toml` goes: the
    // port is the config, loaded from XDG_CONFIG_HOME as on the owner's
    // machine.
    let f = cli.prefix.iter().position(|a| a == "-f").unwrap();
    cli.prefix.drain(f..f + 2);
    let dots = co.athome.join("home/dot_config/grofe");

    let conf = dir.join("config/grofe");
    fs::create_dir_all(conf.join("conf.d")).unwrap();
    fs::copy(dots.join("config.toml"), conf.join("config.toml")).unwrap();
    for entry in fs::read_dir(co.canopy.join("grofe/conf.d")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "toml") {
            fs::copy(&path, conf.join("conf.d").join(path.file_name().unwrap())).unwrap();
        }
    }
    let template = fs::read_to_string(dots.join("user.toml.tmpl")).unwrap();
    fs::write(conf.join("user.toml"), render_user_toml(&template)).unwrap();

    let share = dir.join("home/.local/share");
    fs::create_dir_all(&share).unwrap();
    symlink(&co.canopy, share.join("canopy")).unwrap();

    let bin = dir.join("bin");
    for stub in ["curl", "wget", "git", "sudo", "doas", "open", "osascript"] {
        let path = bin.join(stub);
        fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    cli
}

/// A scenario's steps as grofe runs them: the trees are the checkouts, and
/// the [`TMUX_ONLY`] step is `start-server`.
fn grofe_steps(steps: Vec<Step>, co: &Checkouts) -> Vec<Step> {
    let canopy = co.canopy.display().to_string();
    let athome = co.athome.display().to_string();
    steps
        .into_iter()
        .map(|step| match step {
            Step::Tmux {
                line, background, ..
            } if line == TMUX_ONLY => Step::Tmux {
                line,
                argv: vec!["start-server".into()],
                background,
            },
            Step::Tmux {
                line,
                argv,
                background,
            } => Step::Tmux {
                line,
                argv: argv
                    .iter()
                    .map(|a| {
                        a.replace(CANOPY_TREE, &canopy)
                            .replace(ATHOME_TREE, &athome)
                    })
                    .collect(),
                background,
            },
            other => other,
        })
        .collect()
}

/// A transcript split into steps: each step's first line with what it
/// recorded.
fn steps_of(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let starts = line.starts_with("$ ")
            || line.starts_with("$& ")
            || line.starts_with("client ")
            || line.starts_with("styles ")
            || line == "screen\n"
            || line.starts_with("screen ");
        match out.last_mut() {
            Some(step) if !starts => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// `got` against `golden`, step by step: equal, except the [`KNOWN`] steps
/// of `name`, which must differ.
fn compare(name: &str, golden: &str, got: &str) -> Result<(), String> {
    let (want, got) = (steps_of(golden), steps_of(got));
    if want.len() != got.len() {
        return Err(format!(
            "{name}: {} steps recorded, {} replayed:\n{}",
            want.len(),
            got.len(),
            got.join("")
        ));
    }
    let known: BTreeMap<&str, &str> = KNOWN
        .iter()
        .filter(|(s, _, _)| *s == name)
        .map(|(_, head, why)| (*head, *why))
        .collect();
    let mut wrong = Vec::new();
    for (w, g) in want.iter().zip(&got) {
        let head = w.lines().next().unwrap_or_default();
        match (known.contains_key(head), w == g) {
            (false, false) => wrong.push(format!("--- tmux 3.7b\n{w}+++ grofe\n{g}")),
            (true, true) => wrong.push(format!("KNOWN entry now matches, remove it: {head}")),
            _ => {}
        }
    }
    for head in known.keys() {
        if !want.iter().any(|w| w.lines().next() == Some(head)) {
            wrong.push(format!("KNOWN entry names no step: {head}"));
        }
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!("{name} differs:\n{}", wrong.join("\n")))
    }
}

/// Replays one tc_13 scenario on grofe; the normalized transcript.
fn run_scenario(path: &Path, co: &Checkouts) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let steps = parse_scenario(&text)?;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let cli = tc13_cli(dir.path(), co);
    let got = exact_pids(replay(&cli, &grofe_steps(steps, co)));
    Ok(got.normalized(&Normalizer::load()))
}

/// Every tc_13 scenario, sorted.
fn scenarios() -> Vec<PathBuf> {
    let mut found = Vec::new();
    find_scenarios(&fixtures_dir().join("golden/tc_13"), &mut found);
    found.sort();
    found
}

/// Case 7: `tmux` resolves through the suite's `PATH` to the grofe
/// symlink, from a shell and from a server job, and prints the compat
/// level.
fn smoke(cli: &Cli) -> Result<(), String> {
    let symlink = cli.program.display().to_string();
    let found = Command::new("/bin/sh")
        .args(["-c", "command -v tmux"])
        .env_clear()
        .envs(cli.env.iter().map(|(k, v)| (k, v)))
        .output()
        .map_err(|e| e.to_string())?;
    let found = String::from_utf8_lossy(&found.stdout).trim().to_string();
    if found != symlink {
        return Err(format!(
            "command -v tmux is {found:?}, not the grofe symlink {symlink}"
        ));
    }
    let version = Command::new(&cli.program)
        .arg("-V")
        .env_clear()
        .envs(cli.env.iter().map(|(k, v)| (k, v)))
        .output()
        .map_err(|e| e.to_string())?;
    let want = format!("tmux {}\n", grofe::compat::compat_level());
    if String::from_utf8_lossy(&version.stdout) != want {
        return Err(format!("tmux -V printed {version:?}, want {want:?}"));
    }
    let arg = |s: &str| s.to_string();
    let (status, _, err) = cli.run(&[arg("new-session"), arg("-d"), arg("-s"), arg("smoke")]);
    if !status.success() {
        return Err(format!("new-session: {err}"));
    }
    let (_, job, _) = cli.run(&[arg("run-shell"), arg("command -v tmux")]);
    let _ = cli.run(&[arg("kill-server")]);
    if job.trim() != symlink {
        return Err(format!(
            "a run-shell job's tmux is {:?}, not the grofe symlink",
            job.trim()
        ));
    }
    Ok(())
}

/// Every name and inode in the user's real tmux and grofe socket and state
/// directories, TC.0b's check: `grofe-*` entries are other agents' private
/// scratch sockets and are left out.
fn socket_dirs() -> String {
    let env = Env::from_process();
    let uid = nix::unistd::getuid().as_raw();
    let tmux_tmpdir = std::env::var("TMUX_TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let mut dirs = vec![PathBuf::from(tmux_tmpdir).join(format!("tmux-{uid}"))];
    if let Some(dir) = paths::socket_path(&env, None, None).parent() {
        dirs.push(dir.to_path_buf());
    }
    dirs.push(paths::state_dir(&env));
    let mut out = String::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with("grofe-"))
            .map(|e| {
                let ino = e.metadata().map(|m| m.ino()).unwrap_or(0);
                format!("{ino} {}", e.file_name().to_string_lossy())
            })
            .collect();
        names.sort();
        out.push_str(&format!("{}\n{}\n", dir.display(), names.join("\n")));
    }
    out
}

/// The whole suite, in order; one failure does not stop the rest. The
/// [`KNOWN`] divergences are printed, so a green run still shows them.
#[test]
fn tc_13_suite() {
    let before = socket_dirs();
    let Some(co) = checkouts() else {
        return;
    };
    for (name, head, why) in KNOWN {
        eprintln!("KNOWN tc_13 divergence in {name} ({why}): {head}");
    }
    let mut failures: Vec<String> = Vec::new();

    let dir = tempfile::tempdir().unwrap();
    if let Err(e) = smoke(&tc13_cli(dir.path(), &co)) {
        panic!("smoke check (run first): {e}");
    }

    let found = scenarios();
    assert!(found.len() >= 6, "tc_13 has {} scenarios", found.len());
    let mut first: Option<(String, String, String)> = None;
    for path in &found {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        if name == "tsess" && Path::new(TSESS_PATH_TMUX).exists() {
            eprintln!("SKIPPED tc_13 scenario tsess: {TSESS_PATH_TMUX} would shadow grofe");
            continue;
        }
        let golden = fs::read_to_string(path.with_extension("golden"))
            .unwrap_or_else(|e| panic!("{name}: no golden ({e}); record it"));
        if let Some(swapped) = golden.split(&format!("{TMUX_ONLY}\n")).nth(1)
            && !swapped.starts_with("? 0\n$")
        {
            failures.push(format!(
                "{name}: canopy's tmux entry point did not load cleanly on tmux"
            ));
        }
        match run_scenario(path, &co) {
            Ok(got) => {
                if let Err(e) = compare(&name, &golden, &got) {
                    failures.push(e);
                } else if first.is_none() {
                    first = Some((name.clone(), golden, got));
                }
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
        eprintln!("tc_13 scenario {name}: done");
    }

    // Case 8: one changed byte in a golden makes its comparison fail.
    match first {
        Some((name, golden, got)) => {
            let mut bytes = golden.into_bytes();
            let at = bytes.len() / 2;
            bytes[at] = if bytes[at] == b'x' { b'y' } else { b'x' };
            let edited = String::from_utf8_lossy(&bytes).into_owned();
            if compare(&name, &edited, &got).is_ok() {
                failures.push(format!("{name}: an edited golden still compares equal"));
            }
        }
        None => failures.push("no scenario matched, so the edited-golden check had none".into()),
    }

    failures.extend(live::run(&co));
    failures.extend(corpus::run(&co));

    let after = socket_dirs();
    if before != after {
        failures.push(format!(
            "the real tmux or grofe socket directories changed:\n--- before\n{before}+++ after\n{after}"
        ));
    }
    assert!(
        failures.is_empty(),
        "tc_13: {} failure(s)\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn user_toml_renders_like_check_canopy_port() {
    let template = "a\n{{ with lookPath \"zsh\" -}}\ndefault-shell = \"{{ . }}\"\n{{ end -}}\nb = \"{{ .chezmoi.homeDir }}/x\"\n";
    assert_eq!(
        render_user_toml(template),
        "a\ndefault-shell = \"/bin/zsh\"\nb = \"~/x\"\n"
    );
}

#[test]
fn a_known_divergence_must_still_differ() {
    let golden = "$ a\n? 0\n";
    assert!(compare("none", golden, golden).is_ok());
    assert!(compare("none", golden, "$ a\n? 1\n").is_err());
    assert!(compare("none", golden, "$ a\n? 0\n$ b\n? 0\n").is_err());
}
