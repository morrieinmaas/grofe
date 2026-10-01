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
//!   `restore-now` run the vendored resurrect scripts (owner ruling
//!   2026-09-30: resurrect and continuum run as plugins from alias day).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grofe::config::{self, Env, KeyBinding, Resolved};
use grofe::server::input::parse_key;

use crate::fe_1::grofe_cli;
use crate::normalize::{Normalizer, fixtures_dir};
use crate::replay::{parse_scenario, replay};
use crate::scenarios::port_shell;

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
        6,
        "00-core, 05-caps, 10-keys, 20-status, 30-plugins, 40-nav"
    );
    files
}

/// grofe's layer order: config.toml, conf.d, user.toml.
fn whole_tree() -> Vec<PathBuf> {
    let mut files = vec![port_dir().join("config.toml")];
    files.extend(fragments());
    files.push(user_toml());
    files
}

/// The fixture's `user.toml`, its `/bin/zsh` swapped for
/// [`port_shell`] where this machine has no `/bin/zsh`; written once.
fn user_toml() -> PathBuf {
    static RENDERED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    RENDERED
        .get_or_init(|| {
            let copy = port_dir().join("user.toml");
            let shell = port_shell();
            if shell == "/bin/zsh" {
                return copy;
            }
            let text = fs::read_to_string(&copy).unwrap();
            let line = "default-shell = \"/bin/zsh\"";
            assert!(text.contains(line), "user.toml's default-shell moved");
            let dir =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("tp_1-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let out = dir.join("user.toml");
            fs::write(
                &out,
                text.replace(line, &format!("default-shell = \"{shell}\"")),
            )
            .unwrap();
            out
        })
        .clone()
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
    assert!(
        dropped.is_empty(),
        "every row is bound on grofe: {dropped:?}"
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
    assert_eq!(checked, rows.len());
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
        match got.get(&(table.clone(), key.clone())) {
            None => missing.push(format!("{table} {key}: unbound")),
            Some(b) if key == "r" => assert_eq!(b.cmds[0], ["reload"]),
            // `$CANOPY_STORE` expands at load: here from config.toml's
            // [environment], in the import from nothing. The text must match.
            Some(b) if tmux.command.contains("$CANOPY_STORE") => {
                if b.command != tmux.command {
                    missing.push(format!(
                        "{table} {key}: {:?}, tmux reads {:?}",
                        b.command, tmux.command
                    ));
                }
            }
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
fn case_5_save_now_and_restore_now_run_the_vendored_resurrect_scripts() {
    let script = |s: &str| format!("/plugins/tmux-resurrect/scripts/{s}.sh");
    for tree in [fragments(), whole_tree()] {
        let got = load(&tree);
        let got = by_key(&got);
        for (key, name, s) in [
            ("M-s", "save-now", "save"),
            ("M-r", "restore-now", "restore"),
        ] {
            let b = got
                .get(&("prefix".to_string(), key.to_string()))
                .unwrap_or_else(|| panic!("{key} is not bound"));
            assert_eq!(b.name.as_deref(), Some(name));
            assert_eq!(
                b.command,
                format!("run-shell \"$CANOPY_STORE{}\"", script(s))
            );
        }
    }
    // With the whole tree, `$CANOPY_STORE` is config.toml's canopy store.
    let got = load(&whole_tree());
    let home = std::env::var("HOME").unwrap_or_default();
    let b = by_key(&got)[&("prefix".to_string(), "M-s".to_string())];
    assert_eq!(
        b.cmds,
        [[
            "run-shell".to_string(),
            format!("{home}/.local/share/canopy{}", script("save"))
        ]]
    );
}

/// The options `30-plugins.conf` sets for resurrect and continuum, by
/// name without the `@`, their quotes stripped.
fn tmux_plugin_options() -> BTreeMap<String, String> {
    let text = fs::read_to_string(port_dir().join("30-plugins.conf")).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("set -g @"))
        .filter(|l| l.starts_with("resurrect-") || l.starts_with("continuum-"))
        .map(|l| {
            let (name, value) = l.split_once(' ').unwrap();
            let value = value.trim_matches(|c| c == '"' || c == '\'');
            (name.to_string(), value.to_string())
        })
        .collect()
}

/// A server running the whole ported tree as `tmux -f <file>...`, with a
/// session, and a way to wait for a value.
fn tree_server(dir: &Path, extra: &[PathBuf]) -> crate::replay::Cli {
    let mut cli = grofe_cli(dir);
    cli.prefix.truncate(2);
    for f in whole_tree().iter().chain(extra) {
        cli.prefix
            .extend(["-f".to_string(), f.display().to_string()]);
    }
    let (status, _, err) = cli.run(&[
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        "main".into(),
    ]);
    assert!(status.success(), "{err}");
    cli
}

fn run(cli: &crate::replay::Cli, args: &[&str]) -> String {
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (status, out, err) = cli.run(&argv);
    assert!(status.success(), "{args:?}: {err}");
    out.trim_end_matches('\n').to_string()
}

/// Every resurrect and continuum option canopy's tmux layer sets is set on
/// grofe, to the same value; `@resurrect-dir`'s `$CANOPY_STATE` expands
/// from config.toml's [environment], as tmux expands it from the server's.
#[test]
fn the_plugin_options_are_the_tmux_layers() {
    let want = tmux_plugin_options();
    assert_eq!(want.len(), 7, "{want:#?}");
    let dir = tempfile::tempdir().unwrap();
    let cli = tree_server(dir.path(), &[]);
    let home = cli.env.iter().find(|(k, _)| k == "HOME").unwrap().1.clone();
    for (name, value) in &want {
        let value = value.replace("$CANOPY_STATE", &format!("{home}/.local/state/canopy"));
        let got = || run(&cli, &["show-options", "-gqv", &format!("@{name}")]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got() != value && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(got(), value, "@{name}");
    }
    let _ = cli.run(&["kill-server".to_string()]);
}

/// A status segment shows whenever its source printed anything, `0`
/// included, as tmux's bar did with its segments switched on; only an
/// empty or unset value hides it. `#{?@pet,...}` would read `0` as false.
#[test]
fn a_segment_whose_source_prints_0_still_shows() {
    let tree = load(&whole_tree());
    let right = tree.get("status.right").unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    // The sources are this machine's scripts: switched off, and their
    // values set here instead.
    let off = dir.path().join("no-sources.toml");
    let tables: String = ["pet", "network", "battery"]
        .iter()
        .map(|n| format!("[status.sources.{n}]\ncmd = \"\"\n"))
        .collect();
    fs::write(&off, tables).unwrap();
    let cli = tree_server(dir.path(), &[off]);
    let shown = |cli: &crate::replay::Cli| run(cli, &["display-message", "-p", &right]);
    assert!(!shown(&cli).contains("#[bg=default] 0"), "{}", shown(&cli));
    run(&cli, &["set-option", "-g", "@pet", "0"]);
    assert!(
        shown(&cli).starts_with("#[bg=default] 0  "),
        "{}",
        shown(&cli)
    );
    run(&cli, &["set-option", "-g", "@battery", "0"]);
    assert!(shown(&cli).contains("#[bg=default]0  "), "{}", shown(&cli));
    // The old spelling hides it: why the port does not use it.
    assert_eq!(
        run(&cli, &["display-message", "-p", "x#{?@pet,shown,}"]),
        "x"
    );
    run(&cli, &["set-option", "-g", "@pet", ""]);
    assert!(
        !shown(&cli).starts_with("#[bg=default] "),
        "{}",
        shown(&cli)
    );
    let _ = cli.run(&["kill-server".to_string()]);
}

/// A transcript split into steps, each headed by its `$`, `client`,
/// `screen` or `styles` line.
fn steps(transcript: &str) -> BTreeMap<String, String> {
    let mut out: Vec<String> = Vec::new();
    for line in transcript.split_inclusive('\n') {
        let head = ["$ ", "client ", "screen", "styles "]
            .iter()
            .any(|p| line.starts_with(p));
        match out.last_mut() {
            Some(step) if !head => step.push_str(line),
            _ => out.push(line.to_string()),
        }
    }
    out.into_iter()
        .map(|s| (s.lines().next().unwrap_or_default().to_string(), s))
        .collect()
}

/// `golden/tp_1/canopy-status`: tmux 3.7b drawing canopy's own
/// 20-status.conf, its `#()` aggregator a fixed script.
fn status_golden() -> BTreeMap<String, String> {
    let path = fixtures_dir().join("golden/tp_1/canopy-status.golden");
    steps(&fs::read_to_string(path).unwrap())
}

/// The golden's scenario replayed on grofe run as `tmux -f <file>...`,
/// which loads those files alone, in order: the harness's defaults,
/// `fragments`, then the canopy-status source pointed at the fixed script.
/// The two steps that load the tmux config are left out.
fn grofe_status(fragments: &[PathBuf]) -> BTreeMap<String, String> {
    let dir = tempfile::tempdir().unwrap();
    let mut cli = grofe_cli(dir.path());
    // tmux draws 24-bit colour because the harness's terminal-features say
    // `*:RGB`; grofe learns it only from the client's $COLORTERM (or a
    // terminal it recognises), so its client is told the same way.
    cli.env.push(("COLORTERM".into(), "truecolor".into()));
    let fixed = dir.path().join("99-tp1-fixed-source.toml");
    let script = fixtures_dir().join("golden/fe_3/canopy-status.sh");
    fs::write(
        &fixed,
        format!(
            "[status.sources.canopy-status]\ncmd = \"sh '{}'\"\n",
            script.display()
        ),
    )
    .unwrap();
    for f in fragments.iter().cloned().chain([fixed]) {
        cli.prefix
            .extend(["-f".to_string(), f.display().to_string()]);
    }
    let scenario = fixtures_dir().join("golden/tp_1/canopy-status.scenario");
    let scenario: String = fs::read_to_string(scenario)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("$ set ") && !l.starts_with("$ source-file "))
        .map(|l| format!("{l}\n"))
        .collect();
    let parsed = parse_scenario(&scenario).unwrap();
    steps(&replay(&cli, &parsed).normalized(&Normalizer::load()))
}

/// Case 2: grofe draws canopy's native fragments exactly as tmux 3.7b draws
/// canopy's tmux layer: the text of every row, and the colours and
/// attributes of both status rows, cell for cell, before and after a zoom.
#[test]
fn case_2_the_native_status_matches_tmuxs_rendering_cell_for_cell() {
    let golden = status_golden();
    let got = grofe_status(&fragments());
    let heads: Vec<&str> = got.keys().map(String::as_str).collect();
    for want in [
        "client 80x24 attach -t main",
        "screen net up",
        "styles 0-1 net up",
        "screen +",
        "styles 0-1 +",
    ] {
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

/// Case 2's check sees colour: one theme colour changed in the status
/// fragment leaves the text alone and breaks the styles steps.
#[test]
fn case_2_a_changed_theme_colour_fails_the_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = fragments();
    let status = files
        .iter_mut()
        .find(|f| f.ends_with("20-canopy-status.toml"))
        .unwrap();
    let text = fs::read_to_string(&*status).unwrap();
    let broken = text.replace("theme_blue = \"#458588\"", "theme_blue = \"#458589\"");
    assert_ne!(text, broken);
    let copy = dir.path().join("20-canopy-status.toml");
    fs::write(&copy, broken).unwrap();
    *status = copy;
    let golden = status_golden();
    let got = grofe_status(&files);
    assert_eq!(got.get("screen net up"), golden.get("screen net up"));
    assert_ne!(
        got.get("styles 0-1 net up"),
        golden.get("styles 0-1 net up")
    );
}
