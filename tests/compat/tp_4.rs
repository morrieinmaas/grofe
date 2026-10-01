//! TP.4: tmux-sensible, its own script unchanged, on grofe run as `tmux`
//! (plan M3.7).
//!
//! 1. `golden/tp_4/sensible`: sensible loaded alone, as canopy's loader
//!    loads it, each option it tests first put back at tmux's own default,
//!    then every option and key it sets read back; recorded from tmux
//!    3.7b, and grofe's replay matches it step by step.
//!    `golden/tp_4/sensible-prefix` is the same with `prefix C-a`, which
//!    takes its other branches (C-b unbound, C-a and a bound).
//! 2. On the owner's whole ported config (`tests/fixtures/canopy-port`,
//!    canopy's fragments between athome's `config.toml` and `user.toml`),
//!    sensible run the same way leaves every value it reads either as the
//!    port set it or at sensible's own value: never a third one.
//! 3. Neither run leaves a `compat-miss.log` line.

use std::fs;
use std::path::Path;

use crate::fe_1::grofe_cli;
use crate::normalize::fixtures_dir;
use crate::replay::Cli;
use crate::scenarios::port_shell;
use crate::tp::{check, misses};

#[test]
fn sensible_alone_matches_its_golden() {
    let got = check("tp_4", "sensible", &[]);
    assert!(got.misses.is_empty(), "{:?}", got.misses);
}

#[test]
fn sensible_with_another_prefix_matches_its_golden() {
    let got = check("tp_4", "sensible-prefix", &[]);
    assert!(got.misses.is_empty(), "{:?}", got.misses);
}

/// Every option sensible reads, with the `show` flags that read it and
/// the value it sets, from `golden/tp_4/sensible` (`default-command` it
/// sets only on macOS with reattach-to-user-namespace, which the golden
/// does not).
const OPTIONS: &[(&str, &str, &str)] = &[
    ("-sv", "escape-time", "0"),
    ("-gv", "history-limit", "50000"),
    ("-gv", "display-time", "4000"),
    ("-gv", "status-interval", "5"),
    ("-gv", "default-terminal", "screen-256color"),
    ("-sv", "default-terminal", "screen-256color"),
    ("-gv", "status-keys", "emacs"),
    ("-sv", "focus-events", "on"),
    ("-gwv", "aggressive-resize", "on"),
];

/// The port's own values for the options above, where it sets one: what
/// sensible must leave alone. canopy's 00-core (`user.conf.tmpl`'s "What
/// LEFT this file" list names it for history-limit and the prefix) sets
/// these; the status interval is 20-status's and athome's user.toml's.
const PORT_SETS: &[(&str, &str)] = &[
    ("history-limit", "100000"),
    ("default-terminal", "tmux-256color"),
    ("focus-events", "on"),
    ("status-interval", "5"),
];

/// grofe as `tmux` with the whole port as its config, from
/// `XDG_CONFIG_HOME`, as TC.13 runs it.
fn port_cli(dir: &Path) -> Cli {
    let mut cli = grofe_cli(dir);
    let f = cli.prefix.iter().position(|a| a == "-f").unwrap();
    cli.prefix.drain(f..f + 2);
    let port = fixtures_dir().parent().unwrap().join("canopy-port");
    let conf = dir.join("config/grofe");
    fs::create_dir_all(conf.join("conf.d")).unwrap();
    fs::copy(port.join("config.toml"), conf.join("config.toml")).unwrap();
    for entry in fs::read_dir(port.join("conf.d")).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, conf.join("conf.d").join(path.file_name().unwrap())).unwrap();
    }
    let user = fs::read_to_string(port.join("user.toml")).unwrap().replace(
        "default-shell = \"/bin/zsh\"",
        &format!("default-shell = \"{}\"", port_shell()),
    );
    fs::write(conf.join("user.toml"), user).unwrap();
    cli
}

fn ok(cli: &Cli, args: &[&str]) -> String {
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (status, out, err) = cli.run(&argv);
    assert!(status.success(), "{args:?}: {err}");
    out.trim_end().to_string()
}

/// The prefix table's lines for the keys sensible binds, padding squeezed.
fn sensible_keys(cli: &Cli) -> Vec<String> {
    ok(cli, &["list-keys", "-T", "prefix"])
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| {
            let key = l.split(' ').nth(3).unwrap_or_default();
            ["C-b", "C-s", "s", "C-n", "C-p", "R"].contains(&key)
        })
        .collect()
}

#[test]
fn on_the_whole_port_sensible_sets_its_value_or_keeps_the_ports() {
    let dir = tempfile::tempdir().unwrap();
    let cli = port_cli(dir.path());
    ok(
        &cli,
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
        ],
    );
    let read = |flags: &str, name: &str| ok(&cli, &["show", flags, name]);
    let before: Vec<String> = OPTIONS.iter().map(|(f, n, _)| read(f, n)).collect();
    for (name, value) in PORT_SETS {
        let (i, _) = OPTIONS
            .iter()
            .enumerate()
            .find(|(_, (_, n, _))| n == name)
            .unwrap();
        assert_eq!(&before[i], value, "the port's {name}");
    }
    let keys_before = sensible_keys(&cli);
    let misses_before = misses(dir.path());

    let entry = fixtures_dir().join("../canopy-port/plugins/tmux-sensible");
    ok(
        &cli,
        &[
            "run-shell",
            &format!("cp -R '{}' \"$HOME/sens\"", entry.display()),
        ],
    );
    ok(
        &cli,
        &["run-shell", "\"$HOME/sens/sensible.tmux\" >/dev/null 2>&1"],
    );

    for ((flags, name, sensible), before) in OPTIONS.iter().zip(&before) {
        let after = read(flags, name);
        assert!(
            after == *before || after == *sensible,
            "{name}: {after:?}, neither the port's {before:?} nor sensible's {sensible:?}"
        );
        if PORT_SETS.iter().any(|(n, _)| n == name) {
            assert_eq!(after, *before, "sensible changed the port's {name}");
        }
    }
    // It ran: the port leaves display-time at tmux's 750.
    assert_eq!(
        (before[2].as_str(), read("-gv", "display-time").as_str()),
        ("750", "4000")
    );
    // The port's prefix is C-s: sensible binds the keys it finds unset and
    // leaves the port's own.
    assert_eq!(read("-gv", "prefix"), "C-s");
    let keys_after = sensible_keys(&cli);
    for line in &keys_before {
        assert!(keys_after.contains(line), "{line} went: {keys_after:#?}");
    }
    for line in keys_after.iter().filter(|l| !keys_before.contains(l)) {
        let cmd = line.split(' ').skip(4).collect::<Vec<_>>().join(" ");
        assert!(
            [
                "send-prefix",
                "last-window",
                "previous-window",
                "next-window"
            ]
            .contains(&&*cmd)
                || cmd.starts_with("run-shell "),
            "a binding sensible would not make: {line}"
        );
    }
    let _ = cli.run(&["kill-server".to_string()]);
    let new: Vec<String> = misses(dir.path())
        .into_iter()
        .skip(misses_before.len())
        .collect();
    assert!(new.is_empty(), "sensible's calls missed: {new:?}");
}
