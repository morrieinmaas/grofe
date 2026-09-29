//! TC.5: grofe's event hooks and the server environment (spec §4.2, §4.5
//! "Hooks and environment"). Each event FE.3's twelve tmux hook names map
//! onto runs its hook once per triggering action, in tmux 3.7b's order and
//! with tmux 3.7b's context (recorded on a private socket, see
//! [`TMUX_ORDER`]); `set-hook -gu` stops it; `[hooks]` in the config binds
//! them too, and a reload keeps runtime hooks; a hook's `run-shell -b`
//! never delays the next command; `set-environment` reaches new panes and
//! `run-shell`, `-u` removes a variable (even one the server started
//! with), and a hidden one reaches `$NAME` in command strings but no
//! process.
//!
//! Every server runs in the `TestServer` harness's own tempdirs.

mod common;
#[allow(dead_code)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TestServer, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(5);
/// How long a test waits to be sure a hook did not run (again).
const QUIET: Duration = Duration::from_millis(400);

/// Every event, and the tmux 3.7b hook it stands for.
const EVENTS: &[(&str, &str)] = &[
    ("pane-focused", "after-select-pane"),
    ("window-focused", "after-select-window"),
    ("window-created", "after-new-window"),
    ("pane-closed", "after-kill-pane"),
    ("pane-resized", "after-resize-pane"),
    ("window-resized", "after-resize-window"),
    ("client-resized", "client-resized"),
    ("client-session-changed", "client-session-changed"),
    ("session-created", "session-created"),
    ("session-closed", "session-closed"),
    ("pane-exited", "pane-exited"),
    ("pane-died", "pane-died"),
];

/// What the hooks logged on tmux 3.7b (`-L grofe-tc5 -f /dev/null`, every
/// hook bound to the same `LINE`) for the steps of
/// [`the_twelve_events_fire_in_tmuxs_order_with_tmuxs_context`], with
/// tmux's ids mapped to grofe's (tmux numbers windows and sessions the
/// same way; only the hook names differ).
const TMUX_ORDER: &[(&str, &[&str])] = &[
    ("split", &[]),
    ("select-pane-change", &["pane-focused p=%0 s=a hp= hs= hw="]),
    ("select-pane-noop", &[]),
    ("select-pane-other", &["pane-focused p=%1 s=a hp= hs= hw="]),
    ("resize-pane", &["pane-resized p=%1 s=a hp= hs= hw="]),
    (
        "resize-pane-zoom-twice",
        &[
            "pane-resized p=%1 s=a hp= hs= hw=",
            "pane-resized p=%1 s=a hp= hs= hw=",
        ],
    ),
    ("new-window-d", &["window-created p=%2 s=a hp= hs= hw="]),
    ("new-window", &["window-created p=%3 s=a hp= hs= hw="]),
    (
        "select-window-change",
        &["window-focused p=%2 s=a hp= hs= hw="],
    ),
    (
        "select-window-noop",
        &["window-focused p=%2 s=a hp= hs= hw="],
    ),
    (
        "kill-pane-other-window",
        &["pane-closed p=%2 s=a hp= hs= hw="],
    ),
    (
        "kill-window-last-pane",
        &["pane-closed p=%2 s=a hp= hs= hw="],
    ),
    ("new-session", &["session-created p=%4 s=b hp= hs=b hw="]),
    ("kill-session", &["session-closed p=%2 s=a hp= hs=b hw="]),
    ("new-session-2", &["session-created p=%5 s=c hp= hs=c hw="]),
    (
        "kill-last-pane-of-session",
        &[
            "pane-closed p=%2 s=a hp= hs= hw=",
            "session-closed p=%2 s=a hp= hs=c hw=",
        ],
    ),
    ("exit-remain-on", &["pane-died p=%6 s=a hp=%6 hs= hw=@1"]),
    ("exit-remain-off", &["pane-exited p=%6 s=a hp=%7 hs= hw=@1"]),
    (
        "short-lived-session",
        &[
            "session-created p=%8 s=d hp= hs=d hw=",
            "pane-exited p=%6 s=a hp=%8 hs= hw=@5",
            "session-closed p=%6 s=a hp= hs=d hw=",
        ],
    ),
];

const LINE: &str = "#{hook} p=#{pane_id} s=#{session_name} hp=#{hook_pane} hs=#{hook_session_name} hw=#{hook_window}";

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn display(srv: &TestServer, target: &str, fmt: &str) -> String {
    ok(srv, &["display-message", "-p", "-t", target, fmt])
        .trim_end()
        .to_string()
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Binds `event` to a command that appends `fmt`, expanded, to `log`.
fn bind_line(srv: &TestServer, event: &str, fmt: &str, log: &Path) {
    let cmd = format!("run-shell \"echo '{fmt}' >> {}\"", log.display());
    ok(srv, &["set-hook", "-g", event, &cmd]);
}

/// Waits until `log` has `want` more lines than `seen`, then makes sure
/// no more come, and returns the new ones.
fn new_lines(log: &Path, seen: &mut usize, want: usize) -> Vec<String> {
    let _ = wait_until(|| log_lines(log).len() >= *seen + want, WAIT);
    std::thread::sleep(QUIET);
    let lines = log_lines(log);
    let new = lines[(*seen).min(lines.len())..].to_vec();
    *seen = lines.len();
    new
}

fn pane_of(srv: &TestServer, target: &str) -> String {
    display(srv, target, "#{pane_id}")
}

fn start() -> (TestServer, PathBuf) {
    let srv = TestServer::start();
    ok(
        &srv,
        &["new-session", "-d", "-s", "a", "-x", "80", "-y", "24"],
    );
    let log = srv.home.join("hooks.log");
    (srv, log)
}

/// Case 1: with all twelve bound, each action runs exactly the hooks tmux
/// 3.7b runs for it, in the same order and with the same context, the
/// steps probe5 recorded (`after-resize-window` needs `resize-window`,
/// TC.8's; the client events have their own test).
#[test]
fn the_twelve_events_fire_in_tmuxs_order_with_tmuxs_context() {
    let (srv, log) = start();
    for (event, _) in EVENTS {
        bind_line(&srv, event, LINE, &log);
    }
    let mut seen = 0;
    for (step, want) in TMUX_ORDER {
        let run = |args: &[&str]| ok(&srv, args);
        match *step {
            "split" => drop(run(&["split-window", "-t", "a"])),
            "select-pane-change" | "select-pane-noop" => drop(run(&["select-pane", "-t", "%0"])),
            "select-pane-other" => drop(run(&["select-pane", "-t", "%1"])),
            "resize-pane" => drop(run(&["resize-pane", "-t", "%1", "-D", "2"])),
            "resize-pane-zoom-twice" => {
                run(&["resize-pane", "-Z", "-t", "%1"]);
                run(&["resize-pane", "-Z", "-t", "%1"]);
            }
            "new-window-d" => drop(run(&["new-window", "-d", "-t", "a"])),
            "new-window" => drop(run(&["new-window", "-t", "a"])),
            "select-window-change" | "select-window-noop" => {
                drop(run(&["select-window", "-t", "@1"]))
            }
            "kill-pane-other-window" => drop(run(&["kill-pane", "-t", "%0"])),
            "kill-window-last-pane" => drop(run(&["kill-pane", "-t", "%3"])),
            "new-session" => drop(run(&["new-session", "-d", "-s", "b"])),
            "kill-session" => drop(run(&["kill-session", "-t", "b"])),
            "new-session-2" => drop(run(&["new-session", "-d", "-s", "c"])),
            "kill-last-pane-of-session" => drop(run(&["kill-pane", "-t", "%5"])),
            "exit-remain-on" => drop(run(&["split-window", "-t", "a", "exit 3"])),
            "exit-remain-off" => {
                run(&["set", "-g", "remain-on-exit", "off"]);
                run(&["split-window", "-t", "a", "sleep 0.2"]);
            }
            "short-lived-session" => drop(run(&["new-session", "-d", "-s", "d", "sleep 0.5"])),
            other => panic!("no step {other}"),
        }
        let got = new_lines(&log, &mut seen, want.len());
        assert_eq!(got, *want, "step {step}");
    }
}

/// Case 1, the client events: attaching is a session change and a resize,
/// every size report is a resize, and every switch a session change (even
/// to the same session), as tmux 3.7b (recorded with a pty client).
#[test]
fn client_events_fire_as_tmux() {
    let (srv, log) = start();
    ok(&srv, &["new-session", "-d", "-s", "b"]);
    let fmt = "#{hook} p=#{pane_id} s=#{session_name} hc=#{hook_client}";
    bind_line(&srv, "client-resized", fmt, &log);
    bind_line(&srv, "client-session-changed", fmt, &log);
    let (a, b) = (pane_of(&srv, "a"), pane_of(&srv, "b"));
    let mut seen = 0;
    let mut client = Harness::spawn_attach_with_env(srv.command(), "a", &[], 80, 24);
    let got = new_lines(&log, &mut seen, 2);
    let name = ok(&srv, &["list-clients", "-F", "#{client_name}"])
        .trim_end()
        .to_string();
    assert_eq!(
        got,
        [
            format!("client-session-changed p={a} s=a hc={name}"),
            format!("client-resized p={a} s=a hc={name}"),
        ]
    );
    client.resize(100, 30);
    // One SIGWINCH, one report, one hook.
    assert_eq!(
        new_lines(&log, &mut seen, 1),
        [format!("client-resized p={a} s=a hc={name}")]
    );
    ok(&srv, &["switch-client", "-t", "b"]);
    let want = vec![format!("client-session-changed p={b} s=b hc={name}")];
    assert_eq!(new_lines(&log, &mut seen, 1), want);
    ok(&srv, &["switch-client", "-t", "b"]);
    assert_eq!(new_lines(&log, &mut seen, 1), want);
}

/// A command-shaped hook runs before its command's reply (tmux 3.7b's
/// `after-*` hooks hold the CLI), so the next command sees what it did.
#[test]
fn a_command_hook_has_run_when_its_command_returns() {
    let (srv, _) = start();
    let first = pane_of(&srv, "a");
    ok(&srv, &["split-window", "-t", "a"]);
    ok(
        &srv,
        &[
            "set-hook",
            "-g",
            "pane-focused",
            "set -g @focused #{pane_id}",
        ],
    );
    ok(&srv, &["select-pane", "-t", &first]);
    assert_eq!(display(&srv, "a", "#{@focused}"), first);
}

/// Binds `event` to a command that appends `<event> <pane_id>` to `log`.
fn bind(srv: &TestServer, event: &str, log: &Path) {
    bind_line(srv, event, "#{hook} #{pane_id}", log);
}

/// Waits for exactly one line in `log`, then makes sure no second one
/// comes, and returns it.
fn once(log: &Path) -> String {
    let mut seen = 0;
    let lines = new_lines(log, &mut seen, 1);
    assert_eq!(
        lines.len(),
        1,
        "the hook ran {} times: {lines:?}",
        lines.len()
    );
    let _ = fs::remove_file(log);
    lines[0].clone()
}

fn quiet(log: &Path) {
    std::thread::sleep(QUIET);
    assert_eq!(log_lines(log), Vec::<String>::new(), "the hook ran");
}

fn unbind(srv: &TestServer, event: &str) {
    ok(srv, &["set-hook", "-gu", event]);
}

/// A command-shaped hook's `run-shell` without `-b` holds the CLI's reply
/// until it exits (tmux 3.7b's waited about 1.04 s on a `sleep 1`).
#[test]
fn a_blocking_run_shell_in_a_command_hook_holds_the_reply() {
    let (srv, _) = start();
    let first = pane_of(&srv, "a");
    ok(&srv, &["split-window", "-t", "a"]);
    ok(
        &srv,
        &["set-hook", "-g", "pane-focused", "run-shell 'sleep 1'"],
    );
    let started = Instant::now();
    ok(&srv, &["select-pane", "-t", &first]);
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(900), "took {took:?}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
}

/// Case 2: a hook removed with `set-hook -gu` does not run again, and
/// `show-hooks` lists what is bound.
#[test]
fn an_unset_hook_stops_running() {
    let (srv, log) = start();
    let first = pane_of(&srv, "a");
    ok(&srv, &["split-window", "-t", "a"]);
    let second = pane_of(&srv, "a");
    bind(&srv, "pane-focused", &log);
    let shown = ok(&srv, &["show-hooks", "-g"]);
    assert!(
        shown.starts_with("pane-focused run-shell \"echo '#{hook} #{pane_id}'"),
        "{shown}"
    );
    ok(&srv, &["select-pane", "-t", &first]);
    once(&log);

    unbind(&srv, "pane-focused");
    assert_eq!(ok(&srv, &["show-hooks", "-g"]), "");
    ok(&srv, &["select-pane", "-t", &second]);
    ok(&srv, &["select-pane", "-t", &first]);
    quiet(&log);
}

/// Case 3: a hook running `run-shell -b 'sleep 5'` delays neither its own
/// command nor the next one.
#[test]
fn a_background_run_shell_hook_does_not_delay_the_next_command() {
    let (srv, _) = start();
    let first = pane_of(&srv, "a");
    ok(&srv, &["split-window", "-t", "a"]);
    ok(
        &srv,
        &["set-hook", "-g", "pane-focused", "run-shell -b 'sleep 5'"],
    );
    let started = Instant::now();
    ok(&srv, &["select-pane", "-t", &first]);
    ok(&srv, &["list-sessions"]);
    let took = started.elapsed();
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    // And `run-shell -b` from the CLI answers at once, with no output.
    let started = Instant::now();
    assert_eq!(ok(&srv, &["run-shell", "-b", "sleep 5"]), "");
    assert!(started.elapsed() < Duration::from_millis(1500));
}

/// The environment a new pane starts with, written to a file by the pane
/// itself.
fn new_pane_env(srv: &TestServer, name: &str) -> Vec<String> {
    let out = srv.home.join(format!("{name}.env"));
    let script = format!("env > {}.tmp; mv {0}.tmp {0}; sleep 30", out.display());
    ok(srv, &["new-window", "-d", "-t", "a", &script]);
    wait_until(|| out.exists(), WAIT).expect("the pane wrote its env");
    fs::read_to_string(&out)
        .expect("read env")
        .lines()
        .map(str::to_string)
        .collect()
}

/// Cases 4 and 5: a global variable reaches a new pane and `run-shell`;
/// `-gu` removes it from both.
#[test]
fn a_global_variable_reaches_new_panes_and_run_shell_until_unset() {
    let (srv, _) = start();
    ok(&srv, &["set-environment", "-g", "GROFE_TC5_FOO", "bar"]);
    assert_eq!(
        ok(&srv, &["show-environment", "-g", "GROFE_TC5_FOO"]),
        "GROFE_TC5_FOO=bar\n"
    );
    assert!(new_pane_env(&srv, "set").contains(&"GROFE_TC5_FOO=bar".to_string()));
    assert_eq!(ok(&srv, &["run-shell", "echo $GROFE_TC5_FOO"]), "bar\n");

    ok(&srv, &["set-environment", "-gu", "GROFE_TC5_FOO"]);
    let env = new_pane_env(&srv, "unset");
    assert!(
        !env.iter().any(|l| l.starts_with("GROFE_TC5_FOO=")),
        "{env:?}"
    );
    assert_eq!(ok(&srv, &["run-shell", "echo x$GROFE_TC5_FOO"]), "x\n");
    assert_eq!(
        srv.cmd(&["show-environment", "-g", "GROFE_TC5_FOO"]),
        Err("unknown variable: GROFE_TC5_FOO\n".to_string())
    );
}

/// A session variable reaches that session's new panes and `run-shell`.
#[test]
fn a_session_variable_reaches_its_panes_and_run_shell() {
    let (srv, _) = start();
    ok(
        &srv,
        &["set-environment", "-t", "a", "GROFE_TC5_SESS", "s1"],
    );
    assert!(ok(&srv, &["show-environment", "-t", "a"]).contains("GROFE_TC5_SESS=s1\n"));
    assert!(new_pane_env(&srv, "session").contains(&"GROFE_TC5_SESS=s1".to_string()));
    // From inside the session (a CLI without a pane picks the most recent
    // session, as tmux).
    assert_eq!(ok(&srv, &["run-shell", "echo $GROFE_TC5_SESS"]), "s1\n");
}

/// Case 6: a hidden variable reaches `$NAME` in a command string (here a
/// hook's, split when it is set, as tmux 3.7b does) but no process: not a
/// new pane, and not `run-shell`'s shell either (recorded on tmux 3.7b:
/// `set-environment -gh SECRET x; run-shell 'echo $SECRET'` prints an
/// empty line).
#[test]
fn a_hidden_variable_reaches_command_strings_and_not_a_new_pane() {
    let (srv, log) = start();
    let first = pane_of(&srv, "a");
    ok(&srv, &["split-window", "-t", "a"]);
    ok(&srv, &["set-environment", "-gh", "GROFE_TC5_SECRET", "x"]);
    assert_eq!(
        ok(&srv, &["show-environment", "-gh"]),
        "GROFE_TC5_SECRET=x\n"
    );
    assert!(!ok(&srv, &["show-environment", "-g"]).contains("GROFE_TC5_SECRET"));

    let cmd = format!(
        "run-shell \"echo secret:$GROFE_TC5_SECRET >> {}\"",
        log.display()
    );
    ok(&srv, &["set-hook", "-g", "pane-focused", &cmd]);
    ok(&srv, &["select-pane", "-t", &first]);
    assert_eq!(once(&log), "secret:x");

    let env = new_pane_env(&srv, "hidden");
    assert!(
        !env.iter().any(|l| l.starts_with("GROFE_TC5_SECRET=")),
        "{env:?}"
    );
    assert_eq!(ok(&srv, &["run-shell", "echo x$GROFE_TC5_SECRET"]), "x\n");
}

/// TC.3's ponytail, closed: `$NAME` in a stored command string expands
/// from the global environment, not the server process's.
#[test]
fn command_strings_expand_variables_from_the_global_environment() {
    let (srv, _) = start();
    ok(&srv, &["set-environment", "-g", "GROFE_TC5_WORD", "hello"]);
    ok(
        &srv,
        &[
            "bind-key",
            "-T",
            "root",
            "F9",
            "display-message \"$GROFE_TC5_WORD\"",
        ],
    );
    let keys = ok(&srv, &["list-keys", "-T", "root"]);
    assert!(
        keys.lines()
            .any(|l| l.contains("F9") && l.contains("display-message hello")),
        "{keys}"
    );
}

/// Case 5 for a variable the server started with (and the CLI did not):
/// `-gu` removes it from new panes and `run-shell` too, as tmux 3.7b seeds
/// its global environment from its own.
#[test]
fn unsetting_a_variable_the_server_started_with_removes_it() {
    let name = "GROFE_TC5_SERVER_ONLY";
    let live = spawn_server("", &[(name, "1")]);
    let srv = &live.srv;
    assert!(new_pane_env(srv, "before").contains(&format!("{name}=1")));
    assert_eq!(ok(srv, &["run-shell", &format!("echo x${name}")]), "x1\n");
    ok(srv, &["set-environment", "-gu", name]);
    let env = new_pane_env(srv, "after");
    assert!(
        !env.iter().any(|l| l.starts_with(&format!("{name}="))),
        "{env:?}"
    );
    assert_eq!(ok(srv, &["run-shell", &format!("echo x${name}")]), "x\n");
}

/// A server whose config was written before it started. Killed on drop.
struct Live {
    child: Option<std::process::Child>,
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

fn config_file(srv: &TestServer) -> PathBuf {
    srv.config_home.join("grofe").join("config.toml")
}

fn write_config(srv: &TestServer, text: &str) {
    let path = config_file(srv);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, text).expect("write config");
}

fn start_with_config(text: &str) -> Live {
    spawn_server(text, &[])
}

/// A server started over config `text`, with `env` added to its own
/// environment only, and one detached session `a`.
fn spawn_server(text: &str, env: &[(&str, &str)]) -> Live {
    let srv = TestServer::unstarted();
    write_config(&srv, text);
    let mut cmd = srv.command();
    cmd.envs(env.iter().copied());
    cmd.arg("__server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let live = Live {
        child: Some(cmd.spawn().expect("spawn server")),
        srv,
    };
    if wait_until(|| live.srv.is_running(), WAIT).is_err() {
        panic!("server did not start; log:\n{}", live.srv.log());
    }
    ok(
        &live.srv,
        &["new-session", "-d", "-s", "a", "-x", "80", "-y", "24"],
    );
    live
}

/// `[hooks]` binds events from the config, with provenance in
/// `show-config --explain`; `reload` replaces them all or nothing, and
/// keeps a hook set at runtime.
#[test]
fn config_hooks_bind_and_reload_all_or_nothing() {
    let live = start_with_config("[hooks]\npane-focused = \"set -g @cfg one:#{pane_id}\"\n");
    let srv = &live.srv;
    let first = pane_of(srv, "a");
    ok(srv, &["split-window", "-t", "a"]);
    ok(srv, &["select-pane", "-t", &first]);
    assert_eq!(display(srv, "a", "#{@cfg}"), format!("one:{first}"));
    assert_eq!(
        ok(srv, &["show-hooks", "-g"]),
        "pane-focused set -g @cfg one:#{pane_id}\n"
    );
    let explain = ok(srv, &["show-config", "--explain", "hooks.pane-focused"]);
    assert!(
        explain.starts_with("hooks.pane-focused = \"set -g @cfg one:#{pane_id}\"\n  set by "),
        "{explain}"
    );

    // An error anywhere keeps every hook as it was.
    write_config(
        srv,
        "[hooks]\npane-focused = \"set -g @cfg two\"\nno-such-event = \"list-sessions\"\n",
    );
    let out = srv.run(&["reload"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        out.stderr.contains("hooks.no-such-event: unknown hook"),
        "{}",
        out.stderr
    );
    let bad = srv.run(&["reload"]);
    assert!(!bad.status.success());
    write_config(srv, "[hooks]\npane-focused = \"nosuchverb\"\n");
    let out = srv.run(&["reload"]);
    assert!(
        out.stderr
            .contains("hooks.pane-focused: unknown command: nosuchverb"),
        "{}",
        out.stderr
    );
    assert_eq!(
        ok(srv, &["show-hooks", "-g"]),
        "pane-focused set -g @cfg one:#{pane_id}\n"
    );

    // A good reload replaces it; a runtime hook survives the next one.
    write_config(
        srv,
        "[hooks]\npane-focused = \"set -g @cfg two\"\npane-died = \"list-sessions\"\n",
    );
    ok(srv, &["reload"]);
    assert_eq!(
        ok(srv, &["show-hooks", "-g"]),
        "pane-died list-sessions\npane-focused set -g @cfg two\n"
    );
    ok(srv, &["set-hook", "-g", "pane-died", "list-clients"]);
    write_config(srv, "");
    ok(srv, &["reload"]);
    assert_eq!(ok(srv, &["show-hooks", "-g"]), "pane-died list-clients\n");
}
