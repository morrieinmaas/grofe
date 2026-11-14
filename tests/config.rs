//! Config core (T3.1): layering, provenance, errors with file and line,
//! `requires`, duplicate keys and names, includes, and `show-config`
//! without a server (spec §5.1, §5.2, §5.4; M3 addendum R5 to R8).
//!
//! Every test builds its own tempdir `HOME` and config dir; nothing reads
//! the real `~/.config/grofe` or state dir.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{exists, run_bounded};
use grofe::config::{self, ConfigError, Env, Layer, Resolved};
use tempfile::TempDir;

/// A tempdir with `home/` and `home/.config/grofe/`, and the [`Env`] that
/// points at them.
struct Setup {
    tmp: TempDir,
    env: Env,
}

impl Setup {
    fn new() -> Setup {
        let tmp = tempfile::Builder::new()
            .prefix("grofe-cfg")
            .tempdir()
            .expect("tempdir");
        let home = tmp.path().join("home");
        let dir = home.join(".config").join("grofe");
        fs::create_dir_all(dir.join("conf.d")).expect("config dir");
        let env = Env {
            config_dir: Some(dir),
            home: Some(home),
        };
        Setup { tmp, env }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn dir(&self) -> PathBuf {
        self.home().join(".config").join("grofe")
    }

    /// Writes `text` to `rel` under the config dir and returns its path.
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.dir().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, text).expect("write config file");
        path
    }

    fn load(&self) -> Result<Resolved, Vec<ConfigError>> {
        config::load(&self.env)
    }

    /// `grofe -S <tmp>/run/none.sock show-config <args>` with the tempdir
    /// environment, run from `cwd`.
    fn show_config(&self, args: &[&str], cwd: &Path) -> common::CmdOutput {
        let run = self.tmp.path().join("run");
        let state = self.tmp.path().join("state");
        fs::create_dir_all(&run).expect("run dir");
        fs::create_dir_all(&state).expect("state dir");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.arg("-S")
            .arg(self.socket())
            .arg("show-config")
            .args(args)
            .current_dir(cwd)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &run);
        run_bounded(cmd, Duration::from_secs(5))
    }

    fn socket(&self) -> PathBuf {
        self.tmp.path().join("run").join("none.sock")
    }
}

fn errors(r: Result<Resolved, Vec<ConfigError>>) -> Vec<ConfigError> {
    match r {
        Ok(_) => panic!("config loaded, expected errors"),
        Err(e) => e,
    }
}

fn resolved(r: Result<Resolved, Vec<ConfigError>>) -> Resolved {
    match r {
        Ok(r) => r,
        Err(e) => panic!("config failed: {e:?}"),
    }
}

fn joined(errs: &[ConfigError]) -> String {
    errs.iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- layering and provenance (case 1) ----

#[test]
fn user_toml_beats_conf_d_which_beats_config_toml() {
    let s = Setup::new();
    s.write("config.toml", "status.right = \"a\"\n");
    s.write("conf.d/10-x.toml", "[status]\nright = \"b\"\n");
    let user = s.write("user.toml", "status.right = \"c\"\n");

    let r = resolved(s.load());
    assert_eq!(r.get("status.right"), Some("c"));
    let src = r.provenance.get("status.right").expect("provenance");
    assert_eq!(src.layer, Layer::User);
    assert_eq!(src.file.as_deref(), Some(user.as_path()));
    assert_eq!(
        src.describe(s.env.home.as_deref()),
        "~/.config/grofe/user.toml"
    );

    fs::remove_file(&user).expect("rm user.toml");
    let r = resolved(s.load());
    assert_eq!(r.get("status.right"), Some("b"));
    let src = r.provenance.get("status.right").expect("provenance");
    assert_eq!(src.layer, Layer::ConfD);
    assert_eq!(
        src.describe(s.env.home.as_deref()),
        "~/.config/grofe/conf.d/10-x.toml"
    );
}

#[test]
fn conf_d_loads_in_lexical_order() {
    let s = Setup::new();
    s.write("conf.d/20-y.toml", "status.right = \"twenty\"\n");
    s.write("conf.d/10-x.toml", "status.right = \"ten\"\n");
    s.write("conf.d/README", "not toml, not loaded\n");
    let r = resolved(s.load());
    assert_eq!(r.get("status.right"), Some("twenty"));
}

#[test]
fn defaults_come_from_the_built_in_layer() {
    let s = Setup::new();
    let r = resolved(s.load());
    assert_eq!(r.get("history-limit"), Some("50000"));
    assert_eq!(r.get("escape-time-ms"), Some("25"));
    let src = r.provenance.get("history-limit").expect("provenance");
    assert_eq!(src.layer, Layer::Defaults);
    assert_eq!(src.describe(s.env.home.as_deref()), "built-in defaults");
}

#[test]
fn no_config_dir_loads_defaults_only() {
    let env = Env {
        config_dir: None,
        home: None,
    };
    let r = resolved(config::load(&env));
    assert_eq!(r.get("mouse"), Some("on"));
    assert!(r.keys.is_empty());
}

#[test]
fn values_map_to_tmux_names_and_forms() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "escape-time-ms = 10\nremain-on-exit = true\nmouse = false\nprefix = \"C-a\"\n\
         [pane-border]\nactive-style = \"fg=red\"\n[status]\nlines = 0\n",
    );
    let r = resolved(s.load());
    let tmux: std::collections::BTreeMap<&str, &str> = r.tmux_options().collect();
    assert_eq!(tmux.get("escape-time"), Some(&"10"));
    assert_eq!(tmux.get("remain-on-exit"), Some(&"on"));
    assert_eq!(tmux.get("mouse"), Some(&"off"));
    assert_eq!(tmux.get("prefix"), Some(&"C-a"));
    assert_eq!(tmux.get("pane-active-border-style"), Some(&"fg=red"));
    assert_eq!(tmux.get("status"), Some(&"off"));
}

#[test]
fn pane_border_titles_and_pane_styles_have_native_names() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "window-style = \"bg=colour17\"\nwindow-active-style = \"bg=colour52\"\n\
         [pane-border]\nstatus = \"top\"\nlines = \"heavy\"\nindicators = \"both\"\n\
         format = \" #{pane_index} \"\n",
    );
    let r = resolved(s.load());
    let tmux: std::collections::BTreeMap<&str, &str> = r.tmux_options().collect();
    assert_eq!(tmux.get("window-style"), Some(&"bg=colour17"));
    assert_eq!(tmux.get("window-active-style"), Some(&"bg=colour52"));
    assert_eq!(tmux.get("pane-border-status"), Some(&"top"));
    assert_eq!(tmux.get("pane-border-lines"), Some(&"heavy"));
    assert_eq!(tmux.get("pane-border-indicators"), Some(&"both"));
    assert_eq!(tmux.get("pane-border-format"), Some(&" #{pane_index} "));

    let s = Setup::new();
    s.write("config.toml", "[pane-border]\nstatus = \"left\"\n");
    assert!(
        !errors(s.load()).is_empty(),
        "a status the option store refuses is an error"
    );
}

// ---- errors (cases 2 and 3) ----

#[test]
fn a_type_error_names_file_line_and_key() {
    let s = Setup::new();
    let path = s.write(
        "config.toml",
        "# grofe\nmouse = true\nhistory-limit = \"lots\"\n",
    );
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    let e = &errs[0];
    assert_eq!(e.file, path);
    assert_eq!(e.line, 3);
    assert_eq!(e.key, "history-limit");
    let text = e.to_string();
    assert!(
        text.starts_with(&format!("{}:3: history-limit: ", path.display())),
        "{text}"
    );
    assert!(text.contains("integer"), "{text}");
}

#[test]
fn a_value_the_option_store_refuses_is_an_error() {
    let s = Setup::new();
    s.write("config.toml", "history-limit = -5\n");
    let errs = errors(s.load());
    assert!(joined(&errs).contains("too small"), "{}", joined(&errs));
}

#[test]
fn an_unknown_key_suggests_the_closest_valid_one() {
    let s = Setup::new();
    s.write("config.toml", "\nstauts.left = \"x\"\n");
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    assert_eq!(errs[0].line, 2);
    assert_eq!(errs[0].key, "stauts.left");
    assert!(errs[0].message.contains("status.left"), "{}", errs[0]);
}

#[test]
fn every_error_is_reported_not_just_the_first() {
    let s = Setup::new();
    s.write("config.toml", "mouse = 1\n");
    s.write("user.toml", "bogus = 1\n");
    let errs = errors(s.load());
    assert_eq!(errs.len(), 2, "{}", joined(&errs));
}

#[test]
fn a_toml_syntax_error_has_a_line() {
    let s = Setup::new();
    s.write("config.toml", "mouse = true\nstatus.left = \n");
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    assert_eq!(errs[0].line, 2, "{}", errs[0]);
}

// ---- requires (case 4, R7) ----

#[test]
fn an_unmet_requires_skips_the_file_with_a_warning() {
    let s = Setup::new();
    let path = s.write(
        "conf.d/50-new.toml",
        "requires = \"grofe >= 99.0\"\nbogus = 1\nstatus.right = \"new\"\n",
    );
    let r = resolved(s.load());
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(
        r.warnings[0].contains(&path.display().to_string()),
        "{:?}",
        r.warnings
    );
    assert_ne!(r.get("status.right"), Some("new"));
}

#[test]
fn an_unknown_key_without_requires_is_an_error() {
    let s = Setup::new();
    s.write("conf.d/50-new.toml", "bogus = 1\n");
    let errs = errors(s.load());
    assert_eq!(errs[0].key, "bogus");
}

#[test]
fn a_met_requires_still_rejects_unknown_keys() {
    let s = Setup::new();
    s.write("config.toml", "requires = \"grofe >= 0.1\"\nbogus = 1\n");
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    assert_eq!(errs[0].key, "bogus");
}

#[test]
fn a_malformed_requires_is_an_error() {
    for bad in [
        "grofe >> 1",
        "grofe >= x.y",
        "tmux >= 3.4",
        "grofe 1.0",
        "grofe >= 1.2.3.4",
    ] {
        let s = Setup::new();
        s.write("config.toml", &format!("requires = \"{bad}\"\n"));
        let errs = errors(s.load());
        assert_eq!(errs.len(), 1, "{bad}: {}", joined(&errs));
        assert_eq!(errs[0].key, "requires", "{bad}");
        assert_eq!(errs[0].line, 1, "{bad}");
    }
}

#[test]
fn requires_operators_compare_with_this_version() {
    let s = Setup::new();
    for (req, met) in [
        ("grofe >= 0.1", true),
        ("grofe > 0.1.0", false),
        ("grofe <= 0.1", true),
        ("grofe < 0.1", false),
        ("grofe = 0.1.0", true),
        ("grofe=0.1", true),
        ("grofe < 1.0", true),
    ] {
        s.write(
            "config.toml",
            &format!("requires = \"{req}\"\nstatus.right = \"x\"\n"),
        );
        let r = resolved(s.load());
        assert_eq!(r.get("status.right") == Some("x"), met, "{req}");
        assert_eq!(r.warnings.is_empty(), met, "{req}");
    }
}

// ---- duplicates (case 5, R6) ----

#[test]
fn a_key_bound_twice_in_one_table_is_rejected() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[keys.root]\n\"M-h\" = \"select-pane -L\"\n\"m-h\" = \"select-pane -R\"\n",
    );
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    assert!(errs[0].message.contains("twice"), "{}", errs[0]);
}

#[test]
fn the_same_key_in_two_tables_is_fine() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[keys.root]\n\"M-h\" = \"select-pane -L\"\n[keys.prefix]\n\"M-h\" = \"select-pane -R\"\n",
    );
    let r = resolved(s.load());
    assert_eq!(r.keys.len(), 2);
}

#[test]
fn a_name_used_twice_across_files_is_rejected_naming_both() {
    let s = Setup::new();
    let a = s.write(
        "config.toml",
        "[keys.root]\n\"M-h\" = { cmd = \"select-pane -L\", name = \"focus-left\" }\n",
    );
    let b = s.write(
        "conf.d/10-x.toml",
        "[keys.root]\n\"M-j\" = { cmd = \"select-pane -D\", name = \"focus-left\" }\n",
    );
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    let text = errs[0].to_string();
    assert!(text.contains(&a.display().to_string()), "{text}");
    assert!(text.contains(&b.display().to_string()), "{text}");
    assert!(text.contains("focus-left"), "{text}");
}

#[test]
fn a_later_layer_rebinding_a_key_overrides_it() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[keys.root]\n\"M-h\" = { cmd = \"select-pane -L\", name = \"focus-left\" }\n",
    );
    let user = s.write(
        "user.toml",
        "[keys.root]\n\"M-h\" = { cmd = \"select-pane -R\", name = \"focus-left\", repeat = true }\n",
    );
    let r = resolved(s.load());
    assert_eq!(r.keys.len(), 1);
    let b = &r.keys[0];
    assert_eq!(b.table, "root");
    assert_eq!(b.key.to_string(), "M-h");
    assert_eq!(
        b.cmds,
        vec![vec!["select-pane".to_string(), "-R".to_string()]]
    );
    assert!(b.repeat);
    assert_eq!(b.name.as_deref(), Some("focus-left"));
    assert_eq!(b.source.file.as_deref(), Some(user.as_path()));
    assert_eq!(b.source.line, 2);
}

// ---- bindings ----

#[test]
fn binding_strings_split_into_sequences() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[keys.prefix]\n\"Z\" = \"split-window ; select-pane -L\"\n\
         \"|\" = { cmd = \"split-window -h -c '#{pane_current_path}'\", desc = \"split\", group = \"panes\" }\n",
    );
    let r = resolved(s.load());
    let z = r
        .keys
        .iter()
        .find(|b| b.key.to_string() == "Z")
        .expect("Z bound");
    assert_eq!(z.cmds.len(), 2);
    let bar = r
        .keys
        .iter()
        .find(|b| b.key.to_string() == "|")
        .expect("| bound");
    assert_eq!(
        bar.cmds,
        vec![vec![
            "split-window".to_string(),
            "-h".to_string(),
            "-c".to_string(),
            "#{pane_current_path}".to_string()
        ]]
    );
    assert_eq!(bar.desc.as_deref(), Some("split"));
    assert_eq!(bar.group.as_deref(), Some("panes"));
}

#[test]
fn an_empty_command_unbinds() {
    let s = Setup::new();
    s.write("user.toml", "[keys.prefix]\n\"%\" = \"\"\n");
    let r = resolved(s.load());
    assert_eq!(r.keys.len(), 1);
    assert!(r.keys[0].cmds.is_empty());
}

#[test]
fn a_bad_key_name_or_command_is_an_error_with_a_line() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[keys.root]\n\"C-Nope\" = \"select-pane -L\"\n\"M-x\" = \"splat-window\"\n\
         \"M-y\" = { cmd = \"select-pane\", nmae = \"x\" }\n\"M-z\" = { desc = \"no cmd\" }\n",
    );
    let errs = errors(s.load());
    assert_eq!(errs.len(), 4, "{}", joined(&errs));
    let lines: Vec<usize> = errs.iter().map(|e| e.line).collect();
    assert_eq!(lines, vec![2, 3, 4, 5], "{}", joined(&errs));
    assert!(errs[2].message.contains("name"), "{}", errs[2]);
}

// ---- includes (case 8) ----

#[test]
fn source_file_resolves_beside_the_including_file() {
    let s = Setup::new();
    s.write(
        "conf.d/10-a.toml",
        "source-file = [\"parts/sub.toml\"]\nstatus.right = \"own\"\nstatus.left = \"own\"\n",
    );
    // In a subdirectory, so the conf.d glob cannot load it by itself.
    let sub = s.write("conf.d/parts/sub.toml", "status.right = \"sub\"\n");
    let r = resolved(s.load());
    // Includes apply after the including file's own keys.
    assert_eq!(r.get("status.right"), Some("sub"));
    assert_eq!(r.get("status.left"), Some("own"));
    let src = r.provenance.get("status.right").expect("provenance");
    assert_eq!(src.file.as_deref(), Some(sub.as_path()));
    assert_eq!(src.layer, Layer::ConfD);
}

#[test]
fn an_unreferenced_file_in_a_conf_d_subdirectory_is_not_loaded() {
    let s = Setup::new();
    s.write("conf.d/parts/sub.toml", "status.right = \"sub\"\n");
    let r = resolved(s.load());
    assert_ne!(r.get("status.right"), Some("sub"));
}

#[test]
fn an_included_file_loads_exactly_once() {
    let s = Setup::new();
    s.write("conf.d/10-a.toml", "source-file = \"parts/new.toml\"\n");
    s.write(
        "conf.d/parts/new.toml",
        "requires = \"grofe >= 99.0\"\nbogus = 1\n",
    );
    let r = resolved(s.load());
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(r.warnings[0].contains("new.toml"), "{:?}", r.warnings);
}

#[test]
fn an_include_cycle_is_an_error_naming_the_chain() {
    let s = Setup::new();
    s.write("config.toml", "source-file = \"a.toml\"\n");
    s.write("a.toml", "source-file = [\"b.toml\"]\n");
    s.write("b.toml", "source-file = [\"a.toml\"]\n");
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    let text = errs[0].to_string();
    assert!(text.contains("cycle"), "{text}");
    assert!(text.contains("a.toml") && text.contains("b.toml"), "{text}");
}

#[test]
fn a_missing_include_is_an_error() {
    let s = Setup::new();
    s.write("config.toml", "source-file = [\"gone.toml\"]\n");
    let errs = errors(s.load());
    assert_eq!(errs[0].key, "source-file");
    assert_eq!(errs[0].line, 1);
}

// ---- --file (R5) ----

#[test]
fn file_layers_replace_config_toml_in_order() {
    let s = Setup::new();
    s.write("config.toml", "status.left = \"config\"\n");
    s.write("user.toml", "status.right = \"user\"\n");
    let one = s.write("../x.toml", "status.left = \"one\"\nhistory-limit = 1\n");
    let two = s.write("../y.toml", "status.left = \"two\"\n");
    let r = resolved(config::load_with_file(&s.env, &[one, two]));
    assert_eq!(r.get("status.left"), Some("two"));
    assert_eq!(r.get("history-limit"), Some("1"));
    assert_eq!(r.get("status.right"), Some("user"));
}

#[test]
fn a_lua_file_is_not_a_config_format_and_other_extensions_fail() {
    let s = Setup::new();
    let lua = s.write("init.lua", "-- lua\n");
    let errs = errors(config::load_with_file(&s.env, &[lua]));
    assert!(
        joined(&errs).contains("not a config file: --file takes a .toml file"),
        "{}",
        joined(&errs)
    );
    assert!(!joined(&errs).contains("M6"), "{}", joined(&errs));
    let conf = s.write("x.conf", "set -g mouse on\n");
    let errs = errors(config::load_with_file(&s.env, &[conf]));
    assert_eq!(errs.len(), 1);
}

// ---- show-config (cases 6 and 7) ----

#[test]
fn show_config_explain_names_the_file() {
    let s = Setup::new();
    s.write("config.toml", "status.right = \"a\"\n");
    s.write("user.toml", "status.right = \"c\"\n");
    let out = s.show_config(&["status.right", "--explain"], &s.home());
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "status.right = \"c\"\n  set by ~/.config/grofe/user.toml\n"
    );
    let out = s.show_config(&["history-limit", "--explain"], &s.home());
    assert_eq!(
        out.stdout,
        "history-limit = 50000\n  set by built-in defaults\n"
    );
}

#[test]
fn show_config_file_needs_no_server() {
    let s = Setup::new();
    s.write("user.toml", "status.right = \"from-user\"\n");
    let x = s.write("../x.toml", "status.left = \"x\"\n");
    let x = x.to_string_lossy().into_owned();
    let out = s.show_config(&["--file", &x], &s.home());
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.contains("status.right = \"from-user\""),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("status.left = \"x\""), "{}", out.stdout);
    assert!(!exists(&s.socket()), "show-config created a socket");
}

#[test]
fn show_config_two_files_load_in_order() {
    let s = Setup::new();
    let x = s.write("../x.toml", "status.left = \"x\"\n");
    let y = s.write("../y.toml", "status.left = \"y\"\n");
    let (x, y) = (x.to_string_lossy(), y.to_string_lossy());
    let out = s.show_config(&["--file", &x, "--file", &y, "status.left"], &s.home());
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(out.stdout, "status.left = \"y\"\n");
}

#[test]
fn show_config_rejects_lua_and_unknown_extensions() {
    let s = Setup::new();
    let lua = s.write("init.lua", "-- lua\n");
    let out = s.show_config(&["--file", &lua.to_string_lossy()], &s.home());
    assert!(!out.status.success());
    assert!(
        out.stderr.contains("takes a .toml file") && !out.stderr.contains("M6"),
        "{}",
        out.stderr
    );
    let conf = s.write("x.conf", "\n");
    let out = s.show_config(&["--file", &conf.to_string_lossy()], &s.home());
    assert!(!out.status.success());
}

#[test]
fn show_config_reports_every_error_and_fails() {
    let s = Setup::new();
    s.write("config.toml", "mouse = 1\n");
    s.write("user.toml", "stauts.left = \"x\"\n");
    let out = s.show_config(&[], &s.home());
    assert!(!out.status.success());
    assert!(
        out.stderr.contains("config.toml:1: mouse:"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("user.toml:1: stauts.left:"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{}", out.stdout);
}

#[test]
fn show_config_prints_warnings_on_stderr() {
    let s = Setup::new();
    s.write(
        "conf.d/50-new.toml",
        "requires = \"grofe >= 99.0\"\nbogus = 1\n",
    );
    let out = s.show_config(&["mouse"], &s.home());
    assert!(out.status.success(), "{}", out.stderr);
    assert!(out.stderr.contains("50-new.toml"), "{}", out.stderr);
    assert_eq!(out.stdout, "mouse = true\n");
}

#[test]
fn show_config_lists_bindings_and_looks_one_up() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[keys.root]\n\"M-h\" = { cmd = \"select-pane -L\", name = \"focus-left\" }\n\
         [keys.prefix]\n\"|\" = \"split-window -h\"\n",
    );
    let out = s.show_config(&[], &s.home());
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout
            .contains("keys.prefix.\"|\" = \"split-window -h\"\n"),
        "{}",
        out.stdout
    );
    let out = s.show_config(&["keys.root.M-h", "--explain"], &s.home());
    assert!(out.status.success(), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "keys.root.M-h = { cmd = \"select-pane -L\", name = \"focus-left\" }\n  set by ~/.config/grofe/config.toml\n"
    );
}

#[test]
fn show_config_unknown_key_suggests() {
    let s = Setup::new();
    let out = s.show_config(&["stauts.left"], &s.home());
    assert!(!out.status.success());
    assert!(out.stderr.contains("status.left"), "{}", out.stderr);
}

#[test]
fn show_config_include_ignores_the_process_cwd() {
    let s = Setup::new();
    s.write("conf.d/10-a.toml", "source-file = [\"parts/sub.toml\"]\n");
    s.write("conf.d/parts/sub.toml", "status.right = \"sub\"\n");
    // A decoy at the same relative path from the cwd must not be loaded.
    let elsewhere = s.tmp.path().join("elsewhere");
    fs::create_dir_all(elsewhere.join("parts")).expect("mkdir");
    fs::write(
        elsewhere.join("parts").join("sub.toml"),
        "status.right = \"decoy\"\n",
    )
    .expect("write");
    for cwd in [elsewhere.clone(), s.dir(), s.home()] {
        let out = s.show_config(&["status.right", "--explain"], &cwd);
        assert!(out.status.success(), "{}", out.stderr);
        assert_eq!(
            out.stdout,
            "status.right = \"sub\"\n  set by ~/.config/grofe/conf.d/parts/sub.toml\n",
            "cwd {}",
            cwd.display()
        );
    }
}

// ---- fix round 1: requires placement and skew, names, configured, load_one ----

#[test]
fn requires_after_another_key_is_an_error() {
    let s = Setup::new();
    let path = s.write("config.toml", "mouse = true\nrequires = \"grofe >= 0.1\"\n");
    let errs = errors(s.load());
    assert_eq!(errs.len(), 1, "{}", joined(&errs));
    assert_eq!(errs[0].file, path);
    assert_eq!(errs[0].line, 2);
    assert_eq!(errs[0].key, "requires");
    assert!(errs[0].message.contains("top"), "{}", errs[0]);
}

#[test]
fn an_unmet_leading_requires_skips_a_file_that_does_not_parse() {
    let s = Setup::new();
    let path = s.write(
        "conf.d/50-new.toml",
        "# from a newer grofe\n\nrequires = \"grofe >= 99.0\" # skew\nnew syntax {{ here\n",
    );
    let r = resolved(s.load());
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(
        r.warnings[0].contains(&path.display().to_string()),
        "{:?}",
        r.warnings
    );
}

#[test]
fn a_parse_error_under_a_met_or_missing_requires_is_still_an_error() {
    for text in [
        "requires = \"grofe >= 0.1\"\nnew syntax {{ here\n",
        "new syntax {{ here\n",
        "mouse = true\nrequires = \"grofe >= 99.0\"\nnew syntax {{ here\n",
    ] {
        let s = Setup::new();
        s.write("config.toml", text);
        let errs = errors(s.load());
        assert_eq!(errs.len(), 1, "{text}: {}", joined(&errs));
    }
}

#[test]
fn window_status_formats_live_under_status_window() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "[status.window]\nformat = \"#I\"\ncurrent-format = \"#I*\"\n",
    );
    let r = resolved(s.load());
    let tmux: std::collections::BTreeMap<&str, &str> = r.tmux_options().collect();
    assert_eq!(tmux.get("window-status-format"), Some(&"#I"));
    assert_eq!(tmux.get("window-status-current-format"), Some(&"#I*"));
}

#[test]
fn configured_skips_the_built_in_defaults() {
    let s = Setup::new();
    let user = s.write("user.toml", "history-limit = 7\n");
    let r = resolved(s.load());
    let configured: Vec<_> = r.configured().collect();
    assert_eq!(configured.len(), 1, "{configured:?}");
    let (name, value, src) = configured[0];
    assert_eq!((name, value), ("history-limit", "7"));
    assert_eq!(src.file.as_deref(), Some(user.as_path()));
}

#[test]
fn load_one_returns_only_that_file_and_its_includes() {
    let s = Setup::new();
    s.write("config.toml", "status.left = \"config\"\n");
    s.write("user.toml", "status.right = \"user\"\n");
    let one = s.write(
        "../elsewhere/one.toml",
        "source-file = \"parts/sub.toml\"\nhistory-limit = 9\n[keys.root]\n\"M-h\" = \"select-pane -L\"\n",
    );
    let sub = s.write("../elsewhere/parts/sub.toml", "mouse = false\n");
    let r = resolved(config::load_one(&one, &s.env));
    let configured: std::collections::BTreeMap<&str, (&str, Option<PathBuf>)> = r
        .configured()
        .map(|(n, v, src)| (n, (v, src.file.clone())))
        .collect();
    assert_eq!(configured.len(), 2, "{configured:?}");
    assert_eq!(
        configured.get("history-limit"),
        Some(&("9", Some(one.clone())))
    );
    assert_eq!(configured.get("mouse"), Some(&("off", Some(sub))));
    assert_eq!(r.get("status.left"), None);
    assert_eq!(r.keys.len(), 1);
}

#[test]
fn load_one_reports_errors_and_a_missing_file() {
    let s = Setup::new();
    let bad = s.write("../x.toml", "history-limit = \"lots\"\n");
    let errs = errors(config::load_one(&bad, &s.env));
    assert_eq!(errs[0].line, 1);
    let gone = s.dir().join("gone.toml");
    let errs = errors(config::load_one(&gone, &s.env));
    assert_eq!(errs[0].file, gone);
}

#[test]
fn the_message_pill_and_prompt_position_are_typed() {
    let s = Setup::new();
    s.write(
        "config.toml",
        "prompt-position = \"centre\"\n[message]\npill = false\n",
    );
    let r = resolved(s.load());
    let tmux: std::collections::BTreeMap<&str, &str> = r.tmux_options().collect();
    assert_eq!(tmux.get("prompt-position"), Some(&"centre"));
    assert_eq!(tmux.get("message-pill"), Some(&"off"));

    let s = Setup::new();
    s.write(
        "config.toml",
        "prompt-position = \"middle\"\n[message]\npill = \"yes\"\npil = true\n",
    );
    let errs = errors(s.load());
    let keys: Vec<&str> = errs.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        keys,
        ["prompt-position", "message.pill", "message.pil"],
        "{}",
        joined(&errs)
    );
}
