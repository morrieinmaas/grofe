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
//! `golden/tc_13/PINS`; with either unset the suite is skipped, loudly,
//! unless `GROFE_COMPAT_REQUIRED=1` (the gate sets it), which makes a
//! missing checkout a failure.
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

/// The `tmux` `script` would run: the first on the `PATH` its own `PATH=`
/// lines set, read in order with `$HOME` and `$PATH` from `cli`'s
/// environment, else on `cli`'s `PATH`. athome's scripts put
/// `/opt/nanobrew/prefix/bin` first, where a machine may have a real tmux,
/// which the script would call instead of grofe.
pub fn script_tmux(script: &Path, cli: &Cli) -> Option<PathBuf> {
    let var = |name: &str| {
        cli.env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let home = var("HOME");
    let mut path = var("PATH");
    for line in fs::read_to_string(script).unwrap_or_default().lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some(value) = line.strip_prefix("PATH=") {
            path = value
                .trim_matches(['"', '\''])
                .replace("${HOME}", &home)
                .replace("$HOME", &home)
                .replace("${PATH}", &path)
                .replace("$PATH", &path);
        }
    }
    path.split(':')
        .map(|dir| Path::new(dir).join("tmux"))
        .find(|t| fs::metadata(t).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0))
}

/// Why `scenario` must not run here, if it must not: a canopy or athome
/// script it runs would find a `tmux` other than the grofe symlink.
fn foreign_tmux(scenario: &str, cli: &Cli, co: &Checkouts) -> Option<String> {
    let text = scenario
        .replace(CANOPY_TREE, &co.canopy.display().to_string())
        .replace(ATHOME_TREE, &co.athome.display().to_string());
    let roots = [&co.canopy, &co.athome];
    text.split(|c: char| c.is_whitespace() || c == '\'' || c == '"')
        .map(Path::new)
        .filter(|p| roots.iter().any(|r| p.starts_with(r)) && p.is_file())
        .find_map(|script| match script_tmux(script, cli) {
            Some(t) if t == cli.program => None,
            found => Some(format!(
                "{} would run {}, not the grofe symlink",
                script.display(),
                found.map_or("no tmux".into(), |t| t.display().to_string())
            )),
        })
}

/// The tripwire: only a real tmux makes `tmux-$UID` in the private
/// `TMUX_TMPDIR` the suite gives it; grofe keeps its sockets elsewhere.
pub fn tripwire(cli: &Cli) -> Result<(), String> {
    let Some((_, tmpdir)) = cli.env.iter().find(|(k, _)| k == "TMUX_TMPDIR") else {
        return Err("the suite's tmux has no private TMUX_TMPDIR".into());
    };
    let dir = Path::new(tmpdir).join(format!("tmux-{}", nix::unistd::getuid()));
    match dir.exists() {
        true => Err(format!("a real tmux ran: {} exists", dir.display())),
        false => Ok(()),
    }
}

/// Scenario steps (by scenario and first line) where grofe is known to
/// differ from tmux 3.7b, each with the gap and its owner. A listed step
/// must still differ, so a fix shows up as a stale entry.
const KNOWN: &[(&str, &str, &str)] = &[];

/// The canopy and athome checkouts under test.
pub struct Checkouts {
    pub canopy: PathBuf,
    pub athome: PathBuf,
}

/// Both checkouts, or `None` (said loudly) when either variable is unset;
/// with `GROFE_COMPAT_REQUIRED=1` an unset one fails the run instead. A
/// checkout at another commit than `PINS` is an error: the goldens were
/// recorded against those.
fn checkouts() -> Option<Checkouts> {
    let get = |key: &str| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let (Some(canopy), Some(athome)) = (get("GROFE_COMPAT_CANOPY"), get("GROFE_COMPAT_ATHOME"))
    else {
        let why = "set GROFE_COMPAT_CANOPY and GROFE_COMPAT_ATHOME to canopy and athome \
                   checkouts at the commits in tests/fixtures/compat/golden/tc_13/PINS";
        assert!(
            std::env::var_os("GROFE_COMPAT_REQUIRED").is_none_or(|v| v != "1"),
            "tc_13 is required (GROFE_COMPAT_REQUIRED=1): {why}"
        );
        eprintln!("SKIPPED tc_13: {why}");
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

/// The shell athome's `lookPath "zsh"` block stands for in a test:
/// `/bin/zsh` where it can run, as the fixture copy says, else the first of
/// zsh and sh on this machine's `PATH` (a Linux CI container has no zsh).
/// grofe refuses a `default-shell` it cannot run, as tmux does, so the
/// test must not name one this machine lacks.
pub fn port_shell() -> String {
    use std::os::unix::fs::PermissionsExt;
    let runs = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if runs(Path::new("/bin/zsh")) {
        return "/bin/zsh".to_string();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    ["zsh", "sh"]
        .iter()
        .flat_map(|name| std::env::split_paths(&path).map(move |dir| dir.join(name)))
        .find(|p| p.is_absolute() && runs(p))
        .map_or_else(|| "/bin/sh".to_string(), |p| p.display().to_string())
}

/// athome's `user.toml.tmpl` rendered as `check-canopy-port.sh` renders it
/// without chezmoi: the lookPath block becomes [`port_shell`], the home
/// dir `~`.
fn render_user_toml(template: &str) -> String {
    let block = "{{ with lookPath \"zsh\" -}}\ndefault-shell = \"{{ . }}\"\n{{ end -}}\n";
    assert!(
        template.contains(block),
        "user.toml.tmpl's lookPath block moved"
    );
    let text = template
        .replace(block, &format!("default-shell = \"{}\"\n", port_shell()))
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

/// Replays one tc_13 scenario on grofe: the normalized transcript, or
/// `Err(Ok(reason))` when it is skipped because a script would find a
/// real tmux ([`foreign_tmux`]).
fn run_scenario(path: &Path, co: &Checkouts) -> Result<String, Result<String, String>> {
    let text = fs::read_to_string(path).map_err(|e| Err(e.to_string()))?;
    let steps = parse_scenario(&text).map_err(Err)?;
    let dir = tempfile::tempdir().map_err(|e| Err(e.to_string()))?;
    let cli = tc13_cli(dir.path(), co);
    if let Some(why) = foreign_tmux(&text, &cli, co) {
        return Err(Ok(why));
    }
    let got = exact_pids(replay(&cli, &grofe_steps(steps, co)));
    tripwire(&cli).map_err(Err)?;
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
    tripwire(cli)?;
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
            Err(Ok(skip)) => {
                eprintln!("SKIPPED tc_13 scenario {name}: {skip}");
                continue;
            }
            Err(Err(e)) => failures.push(format!("{name}: {e}")),
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
        format!("a\ndefault-shell = \"{}\"\nb = \"~/x\"\n", port_shell())
    );
}

#[test]
fn a_known_divergence_must_still_differ() {
    let golden = "$ a\n? 0\n";
    assert!(compare("none", golden, golden).is_ok());
    assert!(compare("none", golden, "$ a\n? 1\n").is_err());
    assert!(compare("none", golden, "$ a\n? 0\n$ b\n? 0\n").is_err());
}

/// A script's own `PATH=` line decides which `tmux` it reaches, and the
/// tripwire fires once a real tmux's socket directory appears.
#[test]
fn a_script_s_path_and_the_tripwire_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let cli = grofe_cli(dir.path());
    let other = dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    let fake = other.join("tmux");
    fs::write(&fake, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let plain = dir.path().join("plain");
    fs::write(&plain, "#!/bin/sh\ntmux ls\n").unwrap();
    assert_eq!(script_tmux(&plain, &cli), Some(cli.program.clone()));
    let hardened = dir.path().join("hardened");
    let line = format!("export PATH=\"{}:$HOME/x:$PATH\"\n", other.display());
    fs::write(&hardened, format!("#!/bin/sh\n{line}tmux ls\n")).unwrap();
    assert_eq!(script_tmux(&hardened, &cli), Some(fake));

    assert_eq!(tripwire(&cli), Ok(()));
    let real = dir.path().join(format!("tmux-{}", nix::unistd::getuid()));
    fs::create_dir_all(&real).unwrap();
    assert!(tripwire(&cli).unwrap_err().contains("a real tmux ran"));
}
