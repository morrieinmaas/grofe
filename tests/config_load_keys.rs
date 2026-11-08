//! TP.0's config keys: `[user-options]`, `[environment]` and
//! `load-commands`, which `import-tmux` converts tmux's user options,
//! `set-environment -g` and load-time `if-shell`/`run-shell` into. Every
//! server runs in the harness's own tempdirs.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, wait_until};

const WAIT: Duration = Duration::from_secs(10);

/// A server started over `config.toml`, killed on drop.
struct Live {
    child: Child,
    srv: TestServer,
}

impl Drop for Live {
    fn drop(&mut self) {
        common::stop_server(&mut self.child, &self.srv.socket, &self.srv.ctx());
    }
}

fn write_config(srv: &TestServer, text: &str) {
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), text).expect("write config");
}

fn start(config: &str) -> Live {
    let srv = TestServer::unstarted();
    write_config(&srv, config);
    let mut cmd = srv.command();
    cmd.arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn server");
    wait_until(|| srv.is_running(), CMD_TIMEOUT).expect("server up");
    Live { child, srv }
}

fn ok(srv: &TestServer, args: &[&str]) -> String {
    srv.cmd(args)
        .unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn user_options_are_set_at_load_and_a_runtime_set_wins() {
    let live = start("[user-options]\ntheme_blue = \"#458588\"\nmark = \"a\"\n");
    let srv = &live.srv;
    ok(srv, &["new-session", "-d", "-s", "s"]);
    assert_eq!(
        ok(srv, &["show-options", "-gv", "@theme_blue"]).trim(),
        "#458588"
    );
    assert_eq!(
        ok(srv, &["display-message", "-p", "#{@theme_blue}/#{@mark}"]).trim(),
        "#458588/a"
    );
    ok(srv, &["set-option", "-g", "@theme_blue", "red"]);
    write_config(srv, "[user-options]\ntheme_blue = \"#000000\"\n");
    ok(srv, &["reload"]);
    assert_eq!(
        ok(srv, &["show-options", "-gv", "@theme_blue"]).trim(),
        "red",
        "a runtime set stays over the config's value"
    );
    assert!(
        srv.cmd(&["show-options", "-gv", "@mark"])
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "a user option the config stops setting is unset again"
    );
}

#[test]
fn environment_entries_are_set_at_load_with_dollar_and_tilde_expanded() {
    let live = start(
        "[environment]\nSTORE = \"$HOME/store\"\nNOTES = \"~/notes\"\nNEXT = \"${STORE}/bin\"\n",
    );
    let srv = &live.srv;
    let home = srv.home.display().to_string();
    let env = ok(srv, &["show-environment", "-g"]);
    assert!(env.contains(&format!("STORE={home}/store\n")), "{env}");
    assert!(env.contains(&format!("NOTES={home}/notes\n")), "{env}");
    assert!(env.contains(&format!("NEXT={home}/store/bin\n")), "{env}");
}

#[test]
fn load_commands_run_in_order_at_start_and_on_reload() {
    let srv = TestServer::unstarted();
    let log = srv.home.join("ran");
    let config = format!(
        "load-commands = [\n  \"run-shell 'sleep 0.3; echo one >> {log}'\",\n  \"run-shell 'echo two >> {log}'\",\n  \"if-shell true 'run-shell \\\"echo three >> {log}\\\"'\",\n]\n",
        log = log.display()
    );
    write_config(&srv, &config);
    let mut cmd = srv.command();
    cmd.arg("__server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn server");
    let live = Live { child, srv };
    let srv = &live.srv;
    wait_until(|| srv.is_running(), CMD_TIMEOUT).expect("server up");
    wait_until(|| lines(&log).len() == 3, WAIT)
        .unwrap_or_else(|_| panic!("ran: {:?}", lines(&log)));
    assert_eq!(lines(&log), ["one", "two", "three"], "in config order");
    ok(srv, &["reload"]);
    wait_until(|| lines(&log).len() == 6, WAIT)
        .unwrap_or_else(|_| panic!("ran: {:?}", lines(&log)));
    assert_eq!(lines(&log)[3..], ["one", "two", "three"]);
}

/// What the config starts before the server is ready (a status source, a
/// background job) must not hold the readiness pipe: the CLI that
/// auto-started the server waits for its EOF.
#[test]
fn a_config_child_does_not_delay_the_cli_start() {
    let srv = TestServer::unstarted();
    write_config(
        &srv,
        "load-commands = [\"run-shell -b 'sleep 30'\", \"run-shell true\"]\n\
         [status.sources.slow]\ncmd = \"sleep 30\"\n",
    );
    let started = std::time::Instant::now();
    srv.cmd(&["new-session", "-d"]).expect("new-session");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "new-session took {:?}",
        started.elapsed()
    );
}

#[test]
fn the_new_keys_are_checked_when_the_config_loads() {
    for (config, want) in [
        ("[user-options]\nx = 1\n", "expected a string"),
        (
            "[environment]\n\"A=B\" = \"x\"\n",
            "a variable name has no =",
        ),
        ("load-commands = [\"splat-window\"]\n", "splat-window"),
        ("load-commands = \"run-shell true\"\n", "expected an array"),
    ] {
        let srv = TestServer::unstarted();
        write_config(&srv, config);
        let path = srv.config_home.join("grofe/config.toml");
        let out = srv.run(&["show-config", "--file", path.to_str().expect("utf8")]);
        assert!(!out.status.success(), "{config}: loaded");
        assert!(out.stderr.contains(want), "{config}: {}", out.stderr);
    }
}
