//! TP.1: canopy's grofe fragments and the owner's `user.toml`, ported from
//! canopy's tmux layers and athome's `user.conf`. The fixtures in
//! `tests/fixtures/canopy-port/` are copies of both repos' files, which
//! `scripts/check-canopy-port.sh` keeps current and loads from the repos
//! themselves.
//!
//! - Case 1: the whole tree loads with `show-config --file`, no errors.
//! - Case 2: canopy's two-row status, from the native fragments with the
//!   aggregator fixed, matches FE.3's tmux 3.7b golden cell for cell.
//! - Case 3: every `keys.tsv` row and `user.conf` binding has its grofe
//!   binding: the same table and key, and the commands `import-tmux` reads
//!   the tmux line as, with `keys.tsv`'s columns carried over.
//! - Cases 4 and 5: `canopy-reload` runs `reload`, and `save-now` and
//!   `restore-now` are not bound.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grofe::config::{self, Env, KeyBinding, Resolved};
use grofe::server::input::parse_key;

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{parse_scenario, replay};

fn port_dir() -> PathBuf {
    fixtures_dir()
        .parent()
        .expect("tests/fixtures")
        .join("canopy-port")
}

/// canopy's fragments, in the order grofe reads conf.d.
fn fragments() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(port_dir().join("conf.d"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        5,
        "00-core, 05-caps, 10-keys, 20-status, 30-plugins"
    );
    files
}

/// grofe's layer order: config.toml, conf.d, user.toml.
fn whole_tree() -> Vec<PathBuf> {
    let mut files = vec![port_dir().join("config.toml")];
    files.extend(fragments());
    files.push(port_dir().join("user.toml"));
    files
}

/// Only the named files: no `~/.config/grofe` of this machine.
fn no_dir() -> Env {
    Env {
        config_dir: None,
        home: None,
    }
}

fn load(files: &[PathBuf]) -> Resolved {
    config::load_with_file(&no_dir(), files).unwrap_or_else(|errs| {
        let errs: Vec<String> = errs.iter().map(ToString::to_string).collect();
        panic!("config errors:\n{}", errs.join("\n"))
    })
}

/// A command with none of the live tmux, canopy or grofe environment.
fn isolated(tmp: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", tmp)
        .env("XDG_CONFIG_HOME", tmp.join("xdg"));
    cmd
}

/// `keys.tsv`'s rows: name, key, flags, command, group, description.
fn key_rows() -> Vec<[String; 6]> {
    let text = fs::read_to_string(port_dir().join("keys.tsv")).unwrap();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let cols: Vec<String> = l.split('\t').map(str::to_string).collect();
            cols.try_into()
                .unwrap_or_else(|c| panic!("not 6 columns: {c:?}"))
        })
        .collect()
}

/// The names canopy's `grofe/keys.tsv` leaves unbound on grofe.
fn dropped() -> Vec<String> {
    let text = fs::read_to_string(port_dir().join("keys-grofe.tsv")).unwrap();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('\t').collect();
            (cols.get(1) == Some(&"-")).then(|| cols[0].to_string())
        })
        .collect()
}

/// A `keys.tsv` row as the tmux line `canopy keys` writes for it.
fn bind_line([_, key, flags, cmd, _, _]: &[String; 6]) -> String {
    let cmd = cmd.replace(" ; ", " \\; ");
    if flags == "-" {
        format!("bind {key} {cmd}")
    } else {
        format!("bind {flags} {key} {cmd}")
    }
}

/// What `import-tmux` makes of `conf`, a tmux config, loaded.
fn imported(conf: &str) -> Resolved {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("tmux.conf");
    let out = tmp.path().join("imported.toml");
    fs::write(&src, conf).unwrap();
    let args = [
        "import-tmux",
        src.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ];
    let run = isolated(tmp.path(), &args).output().unwrap();
    assert!(
        run.status.success(),
        "import-tmux: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    load(&[out])
}

fn by_key(r: &Resolved) -> BTreeMap<(String, String), &KeyBinding> {
    r.keys
        .iter()
        .map(|b| ((b.table.clone(), b.key.to_string()), b))
        .collect()
}

#[test]
fn case_1_the_ported_tree_loads_with_no_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let mut args = vec!["show-config".to_string()];
    for f in whole_tree() {
        args.push("--file".into());
        args.push(f.display().to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = isolated(tmp.path(), &args).output().unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success() && stderr.is_empty(), "{stderr}");
    // Each fragment loads on its own too: none leans on another to parse.
    for f in fragments() {
        load(&[f]);
    }
}

#[test]
fn case_3_every_key_table_row_has_its_grofe_binding_column_for_column() {
    let rows = key_rows();
    let dropped = dropped();
    assert_eq!(
        dropped,
        ["save-now", "restore-now"],
        "spec 7.1's dropped pair"
    );
    let lines: Vec<String> = rows.iter().map(bind_line).collect();
    let want = imported(&lines.join("\n"));
    let want = by_key(&want);
    let canopy = load(&fragments());
    let mut checked = 0;
    for row @ [name, key, flags, _, group, desc] in &rows {
        let key = parse_key(key).unwrap().to_string();
        let got = canopy.keys.iter().find(|b| b.name.as_deref() == Some(name));
        if dropped.contains(name) {
            assert!(got.is_none(), "{name} is bound");
            continue;
        }
        let got = got.unwrap_or_else(|| panic!("no binding named {name}"));
        assert_eq!(
            (got.table.as_str(), got.key.to_string()),
            ("prefix", key.clone())
        );
        assert_eq!(got.desc.as_deref(), Some(desc.as_str()), "{name}");
        assert_eq!(got.group.as_deref(), Some(group.as_str()), "{name}");
        assert_eq!(got.repeat, flags.split(' ').any(|f| f == "-r"), "{name}");
        let tmux = want[&("prefix".to_string(), key)];
        if name == "canopy-reload" {
            assert_eq!(tmux.cmds[0][0], "source-file", "{}", bind_line(row));
            assert_eq!(got.cmds[0], ["reload"]);
            assert_eq!(got.cmds[1..], tmux.cmds[1..]);
        } else {
            assert_eq!(got.cmds, tmux.cmds, "{name}: {}", bind_line(row));
        }
        checked += 1;
    }
    assert_eq!(checked, rows.len() - 2);
}

#[test]
fn case_3_every_user_conf_binding_has_its_grofe_binding() {
    let keys: Vec<String> = key_rows().iter().map(bind_line).collect();
    let user_conf = fs::read_to_string(port_dir().join("user.conf")).unwrap();
    // As tmux reads them: canopy's key layer, then user.conf last.
    let want = imported(&format!("{}\n{user_conf}", keys.join("\n")));
    let got = load(&whole_tree());
    let got = by_key(&got);
    let mut missing = Vec::new();
    for ((table, key), tmux) in by_key(&want) {
        if ["M-s", "M-r"].contains(&key.as_str()) {
            continue;
        }
        match got.get(&(table.clone(), key.clone())) {
            None => missing.push(format!("{table} {key}: unbound")),
            Some(b) if key == "r" => assert_eq!(b.cmds[0], ["reload"]),
            Some(b) if b.cmds != tmux.cmds => missing.push(format!(
                "{table} {key}: {:?}, tmux reads {:?}",
                b.command, tmux.command
            )),
            Some(_) => {}
        }
    }
    assert!(missing.is_empty(), "{missing:#?}");
    // The owner's own keys, by name: `B g G M N P T W K ?` and the
    // network click.
    for key in ["B", "g", "G", "M", "N", "P", "T", "W", "K", "?"] {
        let b = got[&("prefix".to_string(), key.to_string())];
        assert!(
            b.source
                .file
                .as_deref()
                .is_some_and(|f| f.ends_with("user.toml")),
            "{key}"
        );
    }
    let click = got[&("root".to_string(), "MouseDown1Status".to_string())];
    assert_eq!(click.cmds[0][0], "if-shell");
}

#[test]
fn case_4_canopy_reload_runs_reload_not_source_file() {
    let got = load(&whole_tree());
    let r = by_key(&got)[&("prefix".to_string(), "r".to_string())];
    assert_eq!(r.name.as_deref(), Some("canopy-reload"));
    assert_eq!(r.cmds[0], ["reload"]);
    for b in &got.keys {
        assert!(
            b.cmds.iter().all(|argv| argv[0] != "source-file"),
            "{} {}: {}",
            b.table,
            b.key,
            b.command
        );
    }
}

#[test]
fn case_5_save_now_and_restore_now_are_absent() {
    let keys = port_dir().join("conf.d/10-canopy-keys.toml");
    let text = fs::read_to_string(&keys).unwrap();
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("M-s") || l.starts_with("M-r"))
    );
    assert!(!text.contains("save.sh") && !text.contains("restore.sh"));
    for tree in [fragments(), whole_tree()] {
        let got = load(&tree);
        for key in ["M-s", "M-r"] {
            assert!(
                !by_key(&got).contains_key(&("prefix".to_string(), key.to_string())),
                "{key} is bound"
            );
        }
        let names: Vec<_> = got.keys.iter().filter_map(|b| b.name.as_deref()).collect();
        assert!(!names.contains(&"save-now") && !names.contains(&"restore-now"));
    }
}

/// A transcript split into steps, each headed by its `$`, `client` or
/// `screen` line.
fn steps(transcript: &str) -> BTreeMap<String, String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let head =
            line.starts_with("$ ") || line.starts_with("client ") || line.starts_with("screen");
        match out.last_mut() {
            Some(step) if !head => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out.into_iter()
        .map(|s| (s.lines().next().unwrap_or_default().to_string(), s))
        .collect()
}

/// Case 2: FE.3's `canopy` golden is tmux 3.7b rendering canopy's
/// 20-status.conf, set line by line, with its `#()` aggregator a fixed
/// script. Here grofe renders the same windows from the native fragments,
/// with the canopy-status source pointed at that script, and every step of
/// the golden that does not set an option must come out the same.
#[test]
fn case_2_the_native_status_matches_tmuxs_rendering_cell_for_cell() {
    let golden_dir = fixtures_dir().join("golden/fe_3");
    let golden = fs::read_to_string(golden_dir.join("canopy.golden")).unwrap();
    let golden = steps(&golden);

    // Run as `tmux -f <file>...`, which loads those files alone, in order:
    // the harness's defaults, canopy's fragments, then the fixed source.
    let dir = tempfile::tempdir().unwrap();
    let mut cli = grofe_cli(dir.path());
    let fixed = dir.path().join("99-tp1-fixed-source.toml");
    let script = golden_dir.join("canopy-status.sh");
    fs::write(
        &fixed,
        format!(
            "[status.sources.canopy-status]\ncmd = \"sh '{}'\"\n",
            script.display()
        ),
    )
    .unwrap();
    for f in fragments().into_iter().chain([fixed]) {
        cli.prefix
            .extend(["-f".to_string(), f.display().to_string()]);
    }

    let scenario: String = fs::read_to_string(golden_dir.join("canopy.scenario"))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("$ set "))
        .map(|l| format!("{l}\n"))
        .collect();
    let parsed = parse_scenario(&scenario).unwrap();
    let got = replay(&cli, &parsed).normalized(&Normalizer::load());
    let got = steps(&got);
    let heads: Vec<&str> = got.keys().map(String::as_str).collect();
    for want in ["client 80x24 attach -t main", "screen net up", "screen +"] {
        assert!(heads.contains(&want), "{heads:#?}");
    }
    for (head, step) in &got {
        assert_eq!(
            Some(step),
            golden.get(head),
            "step {head:?} differs from tmux 3.7b's"
        );
    }
}
