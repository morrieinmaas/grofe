//! TP.6: tmux-sessionx, its own scripts unchanged, on grofe run as `tmux`
//! (plan M3.7), loaded as canopy's loader loads it, with the owner's
//! options set first (`@sessionx-bind s`, zoxide and window modes on).
//!
//! - `golden/tp_6/no-fzf`: without fzf, prefix s runs the plugin's no-fzf
//!   path, recorded from tmux 3.7b in TC.0b's container (which has no
//!   fzf): nothing shows and the client stays where it was. grofe's
//!   replay matches it, and no `compat-miss.log` line is written.
//! - The cases a golden cannot record, since the container has no fzf,
//!   run on grofe with the fzf, fzf-tmux and zoxide on this machine's
//!   `PATH`, and assert what the plugin does: prefix s opens fzf-tmux's
//!   popup listing both sessions' windows (window mode); choosing the
//!   other session's window switches the client to it; in zoxide mode
//!   ctrl-f lists the directories of a scratch zoxide database (never the
//!   runner's own). Each is skipped, said loudly, when a tool is missing.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{Cli, parse_scenario, replay};
use crate::tp::{check, misses};

/// The replay's own `PATH` (`fe_1::grofe_cli`), where an fzf-tmux would
/// make the no-fzf golden take the other path.
const REPLAY_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

#[test]
fn without_fzf_prefix_s_matches_its_golden() {
    if let Some(dir) = REPLAY_PATH
        .iter()
        .find(|d| Path::new(d).join("fzf-tmux").exists() || Path::new(d).join("fzf").exists())
    {
        eprintln!("SKIPPED tp_6 no-fzf: {dir} has an fzf, which the golden recorded without");
        return;
    }
    let got = check("tp_6", "no-fzf", &[]);
    assert!(got.misses.is_empty(), "{:?}", got.misses);
}

/// `bin` on this test's `PATH`.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// grofe as `tmux` with fzf, fzf-tmux and zoxide first on its `PATH`, and
/// a scratch zoxide database holding `dir/projects/zoxide-seed`. `None`
/// (said) when a tool is missing.
fn live_cli(dir: &Path) -> Option<Cli> {
    let mut cli = grofe_cli(dir);
    let tools = dir.join("tools");
    fs::create_dir_all(&tools).unwrap();
    for tool in ["fzf", "fzf-tmux", "zoxide"] {
        let Some(found) = which(tool) else {
            eprintln!("SKIPPED tp_6 live case: no {tool} on PATH");
            return None;
        };
        symlink(found, tools.join(tool)).unwrap();
    }
    let zo = dir.join("zoxide");
    let seed = dir.join("projects/zoxide-seed");
    fs::create_dir_all(&zo).unwrap();
    fs::create_dir_all(&seed).unwrap();
    let added = std::process::Command::new(tools.join("zoxide"))
        .arg("add")
        .arg(&seed)
        .env_clear()
        .env("_ZO_DATA_DIR", &zo)
        .env("HOME", dir.join("home"))
        .status()
        .unwrap();
    assert!(added.success(), "zoxide add");
    for (k, v) in &mut cli.env {
        if k == "PATH" {
            *v = format!("{}:{v}", tools.display());
        }
    }
    cli.env
        .push(("_ZO_DATA_DIR".to_string(), zo.display().to_string()));
    Some(cli)
}

/// The scenario every live case starts from: two sessions, the plugin
/// loaded, a client on `main`.
const SETUP: &str = "\
$ new-session -d -s main -n first -x 100 -y 30 cat
$ new-session -d -s other -n second cat
$ set -g status off
$ set -g @sessionx-bind s
$ set -g @sessionx-zoxide-mode on
$ set -g @sessionx-window-mode on
$ run-shell 'cp -R {fixtures}/../canopy-port/plugins/tmux-sessionx \"$HOME/sx\"'
$ run-shell '\"$HOME/sx/sessionx.tmux\" >/dev/null 2>&1'
client 100x30 attach -t main
";

/// Replays `SETUP` then `steps` on [`live_cli`]: the normalized transcript
/// and the miss log, or `None` when a tool is missing.
fn live(steps: &str) -> Option<(String, Vec<String>)> {
    let dir = tempfile::tempdir().unwrap();
    let cli = live_cli(dir.path())?;
    let parsed = parse_scenario(&format!("{SETUP}{steps}")).unwrap();
    let got = replay(&cli, &parsed).normalized(&Normalizer::load());
    assert!(fixtures_dir().exists());
    Some((got, misses(dir.path())))
}

/// The text a transcript recorded after its step `head`, up to the next
/// step.
fn after<'t>(transcript: &'t str, head: &str) -> &'t str {
    let start = transcript
        .find(&format!("{head}\n"))
        .unwrap_or_else(|| panic!("no step {head:?} in:\n{transcript}"));
    let rest = &transcript[start + head.len() + 1..];
    let end = ["\n$ ", "\nkeys ", "\nscreen", "\nclient "]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Cases 1 and 2: prefix s opens the popup over both sessions' windows,
/// and choosing the other session's switches the client.
#[test]
fn prefix_s_lists_the_sessions_and_choosing_one_switches_to_it() {
    let Some((got, misses)) = live(
        "keys C-b s
screen other:1 second
keys other
screen 1/2
keys Enter
$ run-shell 'sleep 1'
$ display -p '#{client_session}'
",
    ) else {
        return;
    };
    let popup = after(&got, "screen other:1 second");
    assert!(popup.contains("main:1 first"), "{popup}");
    // The label is the session current when the plugin loaded, as on
    // tmux (`sessionx.tmux` reads `#S` once).
    assert!(popup.contains("Current session: \""), "{popup}");
    assert_eq!(
        after(&got, "$ display -p '#{client_session}'").trim(),
        "other\n? 0",
        "{got}"
    );
    assert!(misses.is_empty(), "{misses:?}");
}

/// Case 3: zoxide mode, ctrl-f lists the scratch database's directory.
#[test]
fn in_zoxide_mode_ctrl_f_lists_the_zoxide_directories() {
    let Some((got, _)) = live(
        "keys C-b s
screen other:1 second
keys C-f
screen
keys Escape
$ run-shell 'sleep 1'
$ display -p '#{client_session}'
",
    ) else {
        return;
    };
    let listed = after(&got, "screen");
    // The scratch database's one directory, cut by fzf to its column.
    assert!(listed.contains("▶ <TMPDIR>/proj"), "{listed}");
    assert!(listed.contains(" 1/1 "), "{listed}");
    assert!(!listed.contains("main:1 first"), "{listed}");
    assert_eq!(
        after(&got, "$ display -p '#{client_session}'").trim(),
        "main\n? 0",
        "{got}"
    );
}
