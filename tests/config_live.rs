//! T3.5: the running server is driven by its config (spec §5.1 to §5.4).
//! Options and key tables load at start, `reload` is all or nothing and
//! reports every error, runtime `bind-key` and `set` survive it (R10),
//! `source-file` resolves against the including file, `list-keys` shows
//! each binding's source, and `;` sequences run in bindings and on the CLI.
//!
//! Every server here runs with `XDG_CONFIG_HOME` and `XDG_STATE_HOME` in
//! its own tempdir (the `TestServer` harness), never the real ones.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, wait_until};
use harness::Harness;

const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1006l";

/// A server whose config was written before it started, spawned with `cwd`
/// as its working directory. Killed on drop, before the harness cleans up.
struct Live {
    child: Option<Child>,
    srv: TestServer,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl std::ops::Deref for Live {
    type Target = TestServer;
    fn deref(&self) -> &TestServer {
        &self.srv
    }
}

/// The grofe config dir inside a harness's `XDG_CONFIG_HOME`.
fn config_dir(srv: &TestServer) -> PathBuf {
    srv.config_home.join("grofe")
}

/// Writes `text` to `rel` under the config dir, creating directories.
fn write(srv: &TestServer, rel: &str, text: &str) -> PathBuf {
    let path = config_dir(srv).join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, text).expect("write config");
    path
}

/// Starts a server over `files` (path under the config dir, contents),
/// with the server's cwd at `cwd`, and one detached session `main`.
fn start_in(files: &[(&str, &str)], cwd: &Path) -> Live {
    let srv = TestServer::unstarted();
    for (rel, text) in files {
        write(&srv, rel, text);
    }
    let mut cmd = srv.command();
    cmd.arg("__server")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn server");
    let live = Live {
        child: Some(child),
        srv,
    };
    if wait_until(|| live.is_running(), CMD_TIMEOUT).is_err() {
        panic!("server did not start; log:\n{}", live.log());
    }
    live.cmd(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "80",
        "-y",
        "23",
        "cat",
    ])
    .expect("new-session");
    live
}

fn start(files: &[(&str, &str)]) -> Live {
    let srv_home = std::env::temp_dir();
    start_in(files, &srv_home)
}

fn keys(srv: &TestServer, table: &str) -> String {
    srv.cmd(&["list-keys", "-T", table]).expect("list-keys")
}

/// The `list-keys` line for `key`, or an empty string.
fn key_line(srv: &TestServer, table: &str, key: &str) -> String {
    let prefix = format!("-T {table} {key} ");
    keys(srv, table)
        .lines()
        .find(|l| l.contains(&prefix))
        .unwrap_or_default()
        .to_string()
}

/// A global option's value as `show -g` prints it, without the name and
/// without surrounding double quotes.
// ponytail: `show -v` is not parsed yet.
fn show(srv: &TestServer, name: &str) -> String {
    let out = srv.cmd(&["show", "-g", name]).expect("show");
    let value = out
        .trim_end()
        .strip_prefix(&format!("{name} "))
        .unwrap_or("");
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

fn panes(srv: &TestServer) -> Vec<String> {
    srv.cmd(&[
        "list-panes",
        "-t",
        "main",
        "-F",
        "#{pane_left} #{pane_active}",
    ])
    .expect("list-panes")
    .lines()
    .map(str::to_string)
    .collect()
}

/// Two panes side by side, the left one active.
fn assert_left_active(srv: &TestServer) {
    let p = panes(srv);
    assert_eq!(p.len(), 2, "{p:?}");
    assert!(p.contains(&"0 1".to_string()), "{p:?}");
}

fn attach(srv: &TestServer) -> Harness {
    Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24)
}

/// Case 1: editing a conf.d file then `reload` changes a binding, and
/// `list-keys` names the file and line it came from.
#[test]
fn reload_picks_up_an_edited_conf_d_binding() {
    let srv = start(&[(
        "conf.d/10-a.toml",
        "[keys.prefix]\nx = \"split-window -h\"\n",
    )]);
    assert!(
        key_line(&srv, "prefix", "x").contains("split-window -h"),
        "{}",
        keys(&srv, "prefix")
    );
    let path = write(
        &srv,
        "conf.d/10-a.toml",
        "# edited\n\n[keys.prefix]\nx = \"split-window -v\"\n",
    );
    srv.cmd(&["reload"]).expect("reload");
    let line = key_line(&srv, "prefix", "x");
    assert!(
        line.starts_with("bind-key -T prefix x split-window -v  # "),
        "{line}"
    );
    assert!(line.ends_with(&format!("# {}:4", path.display())), "{line}");
}

/// Case 2: errors in two files are all reported, each with file and line,
/// and nothing is applied.
#[test]
fn reload_with_errors_in_two_files_reports_both_and_changes_nothing() {
    let srv = start(&[
        (
            "conf.d/10-a.toml",
            "status.right = \"before\"\n[keys.prefix]\nx = \"split-window -h\"\n",
        ),
        ("user.toml", "history-limit = 100\n"),
    ]);
    let before_keys = keys(&srv, "prefix");
    let a = write(
        &srv,
        "conf.d/10-a.toml",
        "status.right = \"after\"\nhistory-limit = \"lots\"\n[keys.prefix]\nx = \"split-window -v\"\n",
    );
    let user = write(&srv, "user.toml", "stauts.left = \"x\"\n");
    let out = srv.run(&["reload"]);
    assert!(!out.status.success(), "reload succeeded: {out:?}");
    assert!(
        out.stderr
            .contains(&format!("{}:2: history-limit", a.display())),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains(&format!("{}:1: stauts.left", user.display())),
        "{}",
        out.stderr
    );
    assert_eq!(show(&srv, "status-right"), "before");
    assert_eq!(show(&srv, "history-limit"), "100");
    assert_eq!(keys(&srv, "prefix"), before_keys);
}

/// Case 3: `source-file` in a conf.d file loads the file beside it (kept
/// out of conf.d's glob), whatever the server's cwd; a decoy of the same
/// name in that cwd is ignored.
#[test]
fn source_file_in_conf_d_resolves_beside_the_including_file() {
    let decoy = tempfile::tempdir().expect("decoy dir");
    fs::create_dir_all(decoy.path().join("parts")).expect("mkdir");
    fs::write(
        decoy.path().join("parts/sub.toml"),
        "status.left = \"decoy\"\n",
    )
    .expect("decoy");
    for cwd in [Path::new("/"), decoy.path()] {
        let srv = start_in(
            &[
                ("conf.d/10-a.toml", "source-file = [\"parts/sub.toml\"]\n"),
                ("conf.d/parts/sub.toml", "status.left = \"from sub\"\n"),
            ],
            cwd,
        );
        assert_eq!(show(&srv, "status-left"), "from sub", "cwd {cwd:?}");
    }
}

/// Runtime `source-file`: a relative path is the caller's cwd's, the file's
/// own includes are its directory's, and what it sets survives `reload`.
#[test]
fn runtime_source_file_loads_and_survives_reload() {
    let srv = start(&[("config.toml", "status.left = \"config\"\n")]);
    let dir = tempfile::tempdir().expect("dir");
    fs::create_dir_all(dir.path().join("inc")).expect("mkdir");
    fs::write(
        dir.path().join("extra.toml"),
        "source-file = \"inc/more.toml\"\n[keys.prefix]\nq = { cmd = \"split-window\", desc = \"split it\", name = \"split-q\" }\n",
    )
    .expect("write");
    fs::write(
        dir.path().join("inc/more.toml"),
        "status.left = \"sourced\"\n",
    )
    .expect("write");
    let mut cmd = srv.command();
    cmd.current_dir(dir.path())
        .args(["source-file", "extra.toml"]);
    let out = common::run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(show(&srv, "status-left"), "sourced");
    let line = key_line(&srv, "prefix", "q");
    assert!(line.ends_with("  # runtime: split it [split-q]"), "{line}");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(show(&srv, "status-left"), "sourced");
    assert!(key_line(&srv, "prefix", "q").contains("split-window"));

    // tmux's wording for a missing file; `-q` hides only that.
    let err = srv
        .cmd(&["source-file", "/nonexistent/x.toml"])
        .expect_err("missing");
    assert!(
        err.contains("No such file or directory: /nonexistent/x.toml"),
        "{err}"
    );
    srv.cmd(&["source-file", "-q", "/nonexistent/x.toml"])
        .expect("-q");
    // A broken file applies nothing and names the file and line.
    let bad = dir.path().join("bad.toml");
    fs::write(&bad, "status.left = \"bad\"\nmouse = 3\n").expect("write");
    let err = srv
        .cmd(&["source-file", &bad.display().to_string()])
        .expect_err("bad");
    assert!(
        err.contains(&format!("{}:2: mouse", bad.display())),
        "{err}"
    );
    assert_eq!(show(&srv, "status-left"), "sourced");
}

/// A `name` a sourced file uses is checked against the live tables.
#[test]
fn source_file_rejects_a_name_the_live_tables_use() {
    let srv = start(&[(
        "config.toml",
        "[keys.root]\nM-h = { cmd = \"select-pane -L\", name = \"focus-left\" }\n",
    )]);
    let dir = tempfile::tempdir().expect("dir");
    let f = dir.path().join("dup.toml");
    fs::write(
        &f,
        "[keys.root]\nM-j = { cmd = \"select-pane -D\", name = \"focus-left\" }\n",
    )
    .expect("write");
    let err = srv
        .cmd(&["source-file", &f.display().to_string()])
        .expect_err("duplicate name");
    assert!(err.contains("focus-left"), "{err}");
    assert!(key_line(&srv, "root", "M-j").is_empty());
}

/// Case 4: a runtime `bind-key` is marked `runtime` and survives
/// `reload`, as does a runtime `unbind-key`.
#[test]
fn runtime_bind_key_is_marked_and_survives_reload() {
    let srv = start(&[("config.toml", "[keys.prefix]\nw = \"new-window\"\n")]);
    srv.cmd(&["bind-key", "y", "split-window"]).expect("bind");
    srv.cmd(&["unbind-key", "w"]).expect("unbind");
    let line = key_line(&srv, "prefix", "y");
    assert_eq!(line, "bind-key -T prefix y split-window  # runtime");
    assert_eq!(
        key_line(&srv, "prefix", "c"),
        "bind-key -T prefix c new-window -c \"#{pane_current_path}\"  # default: Create a new window"
    );
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(key_line(&srv, "prefix", "y"), line);
    assert!(key_line(&srv, "prefix", "w").is_empty());
}

/// `bind-key -N` is the description; `unbind-key` words errors as tmux.
#[test]
fn bind_and_unbind_key_edges() {
    let srv = start(&[]);
    srv.cmd(&[
        "bind-key", "-N", "say hi", "-T", "mine", "h", "display", "a b",
    ])
    .expect("bind");
    assert_eq!(
        key_line(&srv, "mine", "h"),
        "bind-key -T mine h display \"a b\"  # runtime: say hi"
    );
    // Recorded on tmux 3.7b.
    srv.cmd(&["unbind-key", "-T", "prefix", "F12"])
        .expect("unbound is fine");
    let err = srv
        .cmd(&["unbind-key", "-T", "prefix", "NotAKey"])
        .expect_err("bad key");
    assert!(err.contains("unknown key: NotAKey"), "{err}");
    let err = srv
        .cmd(&["unbind-key", "-T", "nosuch", "x"])
        .expect_err("bad table");
    assert!(err.contains("table nosuch doesn't exist"), "{err}");
    let err = srv
        .cmd(&["list-keys", "-T", "nosuch"])
        .expect_err("no table");
    assert!(err.contains("table nosuch doesn't exist"), "{err}");
    let err = srv
        .cmd(&["bind-key", "x", "splat-window"])
        .expect_err("bad");
    assert!(err.contains("unknown command: splat-window"), "{err}");
    srv.cmd(&["unbind-key", "-a", "-T", "mine"]).expect("-a");
    assert_eq!(keys(&srv, "mine"), "");
}

/// Case 5: a `split-window ; select-pane -L` binding runs both.
#[test]
fn a_sequence_binding_runs_every_command() {
    let srv = start(&[(
        "config.toml",
        "[keys.prefix]\nz = \"split-window -h ; select-pane -L\"\n",
    )]);
    assert!(
        key_line(&srv, "prefix", "z").contains("split-window -h \\; select-pane -L"),
        "{}",
        keys(&srv, "prefix")
    );
    let mut a = attach(&srv);
    a.wait_for_output(b"\x1b[2J", 0);
    a.send("\x02z");
    srv.wait_for(|| panes(&srv).len() == 2, Duration::from_secs(10))
        .expect("two panes");
    assert_left_active(&srv);
}

/// A config key bound to a `command-alias` name runs the alias's command:
/// the alias expands when the config's bindings load, as `bind-key`
/// expands it, since a key press runs its commands as bound.
#[test]
fn a_config_binding_to_an_alias_runs_its_command() {
    let srv = start(&[(
        "config.toml",
        "command-alias = [\"twopanes=split-window -h\"]\n[keys.prefix]\nz = \"twopanes\"\n",
    )]);
    eprintln!("KEY {:?}", key_line(&srv, "prefix", "z"));
    let mut a = attach(&srv);
    a.wait_for_output(b"\x1b[2J", 0);
    a.send("\x02z");
    srv.wait_for(|| panes(&srv).len() == 2, Duration::from_secs(10))
        .expect("two panes");
}

/// R12: a quoted `';'` in a binding string is one literal argument.
#[test]
fn a_quoted_semicolon_in_a_binding_stays_literal() {
    let srv = start(&[(
        "config.toml",
        "[keys.prefix]\nk = \"send-keys ';' Enter\"\n",
    )]);
    assert!(
        key_line(&srv, "prefix", "k").contains("send-keys \\; Enter"),
        "{}",
        keys(&srv, "prefix")
    );
    let mut a = attach(&srv);
    a.wait_for_output(b"\x1b[2J", 0);
    a.send("\x02k");
    srv.wait_for(
        || {
            srv.capture("main")
                .lines()
                .filter(|l| l.trim() == ";")
                .count()
                == 2
        },
        Duration::from_secs(10),
    )
    .expect("`;` typed and echoed by cat");
}

/// Case 6: `;` on the CLI runs both commands; a failing first command
/// stops the sequence.
#[test]
fn cli_sequences_run_in_order_and_stop_at_the_first_error() {
    let srv = start(&[]);
    srv.cmd(&["split-window", "-h", ";", "select-pane", "-L"])
        .expect("sequence");
    assert_left_active(&srv);
    let out = srv.run(&["kill-session", "-t", "nope", ";", "new-window"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stderr.contains("nope"), "{}", out.stderr);
    let windows = srv.cmd(&["list-windows", "-t", "main"]).expect("lsw");
    assert_eq!(windows.lines().count(), 1, "{windows}");
    // tmux's `a;` form: the trailing `;` ends the command.
    let out = srv
        .cmd(&["display", "-p", "a;", "display", "-p", "b"])
        .expect("display");
    assert_eq!(out, "a\nb\n");
}

/// Case 7: `mouse = false` loads; a runtime `set -g mouse on` survives
/// `reload`.
#[test]
fn a_runtime_set_survives_reload() {
    let srv = start(&[("config.toml", "mouse = false\n")]);
    assert_eq!(show(&srv, "mouse"), "off");
    srv.cmd(&["set", "-g", "mouse", "on"]).expect("set");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(show(&srv, "mouse"), "on");
}

/// Case 7, second half, and the orchestrator's note: a `reload` that
/// turns `mouse` off reaches an attached client's outer stream, through
/// the same path as `set`.
#[test]
fn a_reload_that_turns_mouse_off_reaches_the_outer_terminal() {
    let srv = start(&[]);
    let mut a = attach(&srv);
    let at = a.wait_for_output(MOUSE_ON, 0);
    write(&srv, "conf.d/50-mouse.toml", "mouse = false\n");
    srv.cmd(&["reload"]).expect("reload");
    let at = a.wait_for_output(MOUSE_OFF, at);
    assert_eq!(show(&srv, "mouse"), "off");
    // Dropping the file puts back the built-in default.
    fs::remove_file(config_dir(&srv).join("conf.d/50-mouse.toml")).expect("rm");
    srv.cmd(&["reload"]).expect("reload");
    a.wait_for_output(MOUSE_ON, at);
}

/// Case 8: a repeating binding lists with `-r`, and its name and
/// description show.
#[test]
fn a_repeat_binding_lists_with_r_and_its_name() {
    let srv = start(&[(
        "config.toml",
        "[keys.root]\n\"M-H\" = { cmd = \"resize-pane -L 5\", desc = \"grow left\", name = \"grow-left\", repeat = true }\n",
    )]);
    let line = key_line(&srv, "root", "M-H");
    assert!(
        line.starts_with("bind-key -r -T root M-H resize-pane -L 5  # "),
        "{line}"
    );
    assert!(line.ends_with(":2: grow left [grow-left]"), "{line}");
}

/// R9: a broken config at start leaves the built-in defaults and logs
/// every error; start-time seeds (`remain-on-exit` from the harness)
/// survive a config that does not set them.
#[test]
fn a_broken_config_at_start_keeps_the_defaults_and_logs_errors() {
    let srv = start(&[
        ("config.toml", "status.left = \"mine\"\n"),
        ("user.toml", "nonsense = 1\n"),
    ]);
    assert_eq!(show(&srv, "status-left"), "[#{session_name}] ");
    assert!(srv.log().contains("user.toml:1: nonsense"), "{}", srv.log());
    fs::remove_file(config_dir(&srv).join("user.toml")).expect("rm");
    srv.cmd(&["reload"]).expect("reload");
    assert_eq!(show(&srv, "status-left"), "mine");
    assert_eq!(show(&srv, "remain-on-exit"), "on");
}

/// R11: `"key" = ""` unbinds a stock binding; `reload` without it brings
/// the stock one back.
#[test]
fn an_empty_command_unbinds_a_stock_key() {
    let srv = start(&[("config.toml", "[keys.prefix]\n\"%\" = \"\"\n")]);
    assert!(key_line(&srv, "prefix", "\\%").is_empty());
    fs::remove_file(config_dir(&srv).join("config.toml")).expect("rm");
    srv.cmd(&["reload"]).expect("reload");
    assert!(
        key_line(&srv, "prefix", "\\%").ends_with("  # default: Split window horizontally"),
        "{}",
        keys(&srv, "prefix")
    );
}

/// Fix round 1: a binding's argv is already split, so a literal `;`
/// argument stays literal. `list-keys` prints a literal `;` and a command
/// separator both as `\;`, as tmux does, so the printed text is ambiguous:
/// this checks only that rebinding from it prints the same text again.
#[test]
fn list_keys_output_prints_the_same_text_after_rebinding() {
    let srv = start(&[(
        "config.toml",
        "[keys.prefix]\nk = \"send-keys ';' Enter ; display -p 'q;'\"\n",
    )]);
    let cmd_of = |key: &str| {
        let line = key_line(&srv, "prefix", key);
        let rest = line
            .split_once(&format!("-T prefix {key} "))
            .map(|(_, r)| r.to_string())
            .unwrap_or_default();
        rest.split_once("  # ")
            .map(|(c, _)| c.to_string())
            .unwrap_or_default()
    };
    let printed = cmd_of("k");
    assert_eq!(printed, "send-keys \\; Enter \\; display -p \"q;\"");
    srv.cmd(&["bind-key", "K", &printed]).expect("rebind");
    assert_eq!(cmd_of("K"), printed);
}

/// Fix round 1: `bind-key` with several arguments splits them at `;`
/// arguments, as tmux 3.7b does (recorded on `-L grofe-t35`:
/// `bind y split-window '\\;' select-pane -L` binds both commands, and
/// `bind V send-keys 'a\\;'` binds `send-keys a`).
#[test]
fn multi_argument_bind_key_splits_like_tmux() {
    let srv = start(&[]);
    srv.cmd(&[
        "bind-key",
        "y",
        "split-window",
        "-h",
        "\\;",
        "select-pane",
        "-L",
    ])
    .expect("bind y");
    assert_eq!(
        key_line(&srv, "prefix", "y"),
        "bind-key -T prefix y split-window -h \\; select-pane -L  # runtime"
    );
    srv.cmd(&["bind-key", "V", "send-keys", "a\\;"])
        .expect("bind V");
    assert_eq!(
        key_line(&srv, "prefix", "V"),
        "bind-key -T prefix V send-keys a  # runtime"
    );
    let mut a = attach(&srv);
    a.wait_for_output(b"\x1b[2J", 0);
    a.send("\x02y");
    srv.wait_for(|| panes(&srv).len() == 2, Duration::from_secs(10))
        .expect("two panes");
    assert_left_active(&srv);
}

/// Fix round 1: commands typed at the prompt are already split, so a
/// quoted `';'` is one literal argument.
#[test]
fn a_quoted_semicolon_at_the_prompt_stays_literal() {
    let srv = start(&[]);
    let mut a = attach(&srv);
    a.wait_for_output(b"\x1b[2J", 0);
    a.send("\x02:");
    a.wait_for(":");
    a.send("set -g @x 'q;' ; send-keys ';' Enter\r");
    srv.wait_for(
        || {
            srv.capture("main")
                .lines()
                .filter(|l| l.trim() == ";")
                .count()
                == 2
        },
        Duration::from_secs(10),
    )
    .expect("`;` typed and echoed by cat");
    assert_eq!(show(&srv, "@x"), "q;");
}

/// Fix round 1: a runtime `unbind-key` of a key that was not bound is not
/// recorded, so a config binding of it added later still applies.
#[test]
fn unbinding_an_unbound_key_does_not_block_a_later_config_binding() {
    let srv = start(&[]);
    srv.cmd(&["unbind-key", "-T", "prefix", "F12"])
        .expect("unbind");
    write(&srv, "config.toml", "[keys.prefix]\nF12 = \"new-window\"\n");
    srv.cmd(&["reload"]).expect("reload");
    assert!(
        key_line(&srv, "prefix", "F12").contains("new-window"),
        "{}",
        keys(&srv, "prefix")
    );
}

/// Fix round 1: `base-index` numbers new windows (tmux 3.7b: with
/// `base-index 5`, the first window is 5 and the next 6).
#[test]
fn base_index_numbers_new_windows() {
    let srv = start(&[("config.toml", "base-index = 5\n")]);
    srv.cmd(&["new-window", "-d", "-t", "main"])
        .expect("new-window");
    let idx = srv
        .cmd(&["list-windows", "-t", "main", "-F", "#{window_index}"])
        .expect("lsw");
    assert_eq!(idx, "5\n6\n");
    srv.cmd(&["set", "-g", "base-index", "1"]).expect("set");
    srv.cmd(&["new-window", "-d", "-t", "main"])
        .expect("new-window");
    let idx = srv
        .cmd(&["list-windows", "-t", "main", "-F", "#{window_index}"])
        .expect("lsw");
    assert_eq!(idx, "1\n5\n6\n");
}

/// Fix round 1: `pane-base-index` sets `#{pane_index}` and what `.N`
/// targets.
#[test]
fn pane_base_index_numbers_panes_in_formats_and_targets() {
    let srv = start(&[]);
    srv.cmd(&["split-window", "-t", "main"]).expect("split");
    let fmt = ["list-panes", "-t", "main", "-F", "#{pane_index} #{pane_id}"];
    let with_one = srv.cmd(&fmt).expect("list-panes");
    let ids: Vec<&str> = with_one
        .lines()
        .filter_map(|l| l.split(' ').nth(1))
        .collect();
    assert!(with_one.starts_with("1 "), "{with_one}");
    let second = srv
        .cmd(&["display", "-p", "-t", ":.2", "#{pane_id}"])
        .expect(":.2");
    assert_eq!(second.trim(), ids[1]);

    srv.cmd(&["set", "-g", "pane-base-index", "0"])
        .expect("set");
    let with_zero = srv.cmd(&fmt).expect("list-panes");
    assert_eq!(with_zero, format!("0 {}\n1 {}\n", ids[0], ids[1]));
    let second = srv
        .cmd(&["display", "-p", "-t", ":.1", "#{pane_id}"])
        .expect(":.1");
    assert_eq!(second.trim(), ids[1]);
}

/// Fix round 1: `history-limit` is the scrollback a new pane keeps.
#[test]
fn history_limit_sizes_a_new_panes_scrollback() {
    let srv = start(&[("config.toml", "history-limit = 7\n")]);
    srv.cmd(&["new-window", "-d", "-t", "main:9", "seq 100; exec cat"])
        .expect("new-window");
    let capture = || {
        srv.cmd(&["capture-pane", "-p", "-S", "-", "-t", "main:9"])
            .unwrap_or_default()
    };
    srv.wait_for(|| capture().contains("100"), Duration::from_secs(10))
        .expect("seq ran");
    let text = capture();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    // 7 lines of history plus at most one screen of 23 rows.
    assert!(lines.len() <= 30, "{} lines:\n{text}", lines.len());
    assert_ne!(lines.first().copied(), Some("1"), "{text}");
}

/// Lua was dropped (spec §5.1): `source-file` of a `.lua` file is not a
/// special case, it is read as any other non-tmux file, as TOML, and fails
/// there. No error promises a Lua loader to come.
#[test]
fn source_file_of_a_lua_file_is_not_a_special_case() {
    let srv = start(&[]);
    let dir = tempfile::tempdir().expect("dir");
    let f = dir.path().join("init.lua");
    fs::write(&f, "vim.opt.mouse = true\n").expect("write");
    let err = srv
        .cmd(&["source-file", &f.display().to_string()])
        .expect_err("lua");
    assert!(err.contains("init.lua"), "{err}");
    assert!(
        !err.contains("M6") && !err.contains("not implemented"),
        "{err}"
    );
}
