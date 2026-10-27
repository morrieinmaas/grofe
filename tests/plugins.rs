//! T6.3: plugin supervision (spec §7.4). Each test runs a real server on a
//! private socket, with `HOME` and the state dir in a tempdir, so a plugin's
//! log and files never touch the user's.
//!
//! The restart policy runs on real time. Tests that look at the 1, 2, 4 s
//! spacing or the five-crash window set `GROFE_PLUGIN_TIME_SCALE` so the
//! same policy takes a fraction of the time; the one at scale 1 is the
//! "back within 1.5 s" case.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{CMD_TIMEOUT, TestServer, run_bounded, wait_until};
use grofe::config::{self, Env, Restart};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const WAIT: Duration = Duration::from_secs(10);

fn write_config(srv: &TestServer, text: &str) {
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), text).expect("write config");
}

/// A script in the test's home, executable; its absolute path.
fn script(srv: &TestServer, name: &str, body: &str) -> PathBuf {
    let path = srv.home.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    let mut perms = fs::metadata(&path).expect("stat").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&path, perms).expect("chmod");
    path
}

fn toml_str(p: &Path) -> String {
    format!("{:?}", p.display().to_string())
}

/// `[plugins.<name>]` running `sh <script>`.
fn plugin_table(name: &str, script: &Path, extra: &str) -> String {
    format!(
        "[plugins.{name}]\ncmd = [\"sh\", {}]\n{extra}\n",
        toml_str(script)
    )
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    pid: Option<i32>,
    state: String,
    restarts: u32,
    since: String,
}

fn rows(srv: &TestServer) -> Vec<Row> {
    let out = srv.cmd(&["list-plugins"]).expect("list-plugins");
    out.lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 5, "list-plugins line {l:?}");
            Row {
                name: f[0].to_string(),
                pid: f[1].parse().ok(),
                state: f[2].to_string(),
                restarts: f[3].parse().expect("restarts"),
                since: f[4].to_string(),
            }
        })
        .collect()
}

fn row(srv: &TestServer, name: &str) -> Row {
    rows(srv)
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no plugin {name} in list-plugins"))
}

/// Waits for `name` to be running and returns its pid.
fn running(srv: &TestServer, name: &str) -> i32 {
    let mut pid = None;
    let ok = wait_until(
        || match rows(srv).into_iter().find(|r| r.name == name) {
            Some(Row {
                state,
                pid: Some(p),
                ..
            }) if state == "running" => {
                pid = Some(p);
                true
            }
            _ => false,
        },
        WAIT,
    );
    ok.unwrap_or_else(|e| panic!("{name} never ran: {e}; rows {:?}", rows(srv)));
    pid.expect("pid")
}

fn wait_state(srv: &TestServer, name: &str, state: &str) {
    wait_until(|| row(srv, name).state == state, WAIT).unwrap_or_else(|e| {
        panic!(
            "{name} never reached {state}: {e}; row {:?}",
            row(srv, name)
        )
    });
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn sigkill(pid: i32) {
    kill(Pid::from_raw(pid), Signal::SIGKILL).expect("kill plugin");
}

fn lines(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |t| t.lines().count())
}

fn read(path: &Path) -> String {
    // The shell creates the file before it writes the line.
    let mut text = String::new();
    wait_until(
        || {
            text = fs::read_to_string(path).unwrap_or_default();
            text.ends_with('\n')
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text
}

fn session(srv: &TestServer) {
    srv.cmd(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-x",
        "80",
        "-y",
        "24",
        "cat",
    ])
    .expect("new-session");
}

fn server_pid(srv: &TestServer) -> i32 {
    session_if_none(srv);
    srv.cmd(&["display-message", "-p", "#{pid}"])
        .expect("pid")
        .trim()
        .parse()
        .expect("server pid")
}

fn session_if_none(srv: &TestServer) {
    if srv.cmd(&["has-session", "-t", "s"]).is_err() {
        session(srv);
    }
}

#[test]
fn a_plugin_starts_with_the_server_and_sees_grofe() {
    let srv = TestServer::unstarted();
    let out = srv.home.join("env.txt");
    let p = script(
        &srv,
        "p.sh",
        &format!(
            "echo \"$GROFE|$GROFE_PLUGIN|$GROFE_PANE|$TMUX_PANE|$TMUX\" > {}\nexec sleep 600",
            out.display()
        ),
    );
    let srv = start_with(srv, &plugin_table("probe", &p, ""), None);
    let pid = running(&srv, "probe");
    assert!(alive(pid));
    let text = read(&out);
    let fields: Vec<&str> = text.trim_end().split('|').collect();
    assert!(
        grofe::paths::same_socket(Path::new(fields[0]), &srv.socket),
        "GROFE names the socket: {text}"
    );
    assert_eq!(fields[1], "probe");
    assert_eq!(
        &fields[2..],
        ["", "", ""],
        "no pane variables, no TMUX: {text}"
    );
}

#[test]
fn a_killed_plugin_is_back_within_a_second_and_a_half() {
    let srv = TestServer::unstarted();
    let p = script(&srv, "p.sh", "exec sleep 600");
    let srv = start_with(srv, &plugin_table("p", &p, ""), None);
    let pid = running(&srv, "p");
    sigkill(pid);
    let t = Instant::now();
    let new = loop {
        let r = row(&srv, "p");
        match r.pid {
            Some(p) if p != pid && r.state == "running" => break p,
            _ => {}
        }
        assert!(t.elapsed() < Duration::from_millis(1500), "not back: {r:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    let back = t.elapsed();
    assert!(
        back >= Duration::from_millis(900),
        "backoff skipped: {back:?}"
    );
    assert!(alive(new));
    assert_eq!(row(&srv, "p").restarts, 1);
}

/// Starts a server on `config` with `start-server`, so a test can choose its
/// environment: `scale` is `GROFE_PLUGIN_TIME_SCALE`. The harness is made
/// first so a script can live in its home.
fn start_with(srv: TestServer, config: &str, scale: Option<&str>) -> TestServer {
    write_config(&srv, config);
    let mut cmd = srv.command();
    if let Some(scale) = scale {
        cmd.env("GROFE_PLUGIN_TIME_SCALE", scale);
    }
    cmd.arg("start-server");
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "start-server: {}", out.stderr);
    srv
}

#[test]
fn repeated_kills_space_restarts_one_two_and_four_units_apart() {
    // Scale 0.2: the policy's 1, 2 and 4 s are 0.2, 0.4 and 0.8 s.
    let srv = TestServer::unstarted();
    let p = script(&srv, "p.sh", "exec sleep 600");
    let srv = start_with(srv, &plugin_table("p", &p, ""), Some("0.2"));
    let mut pid = running(&srv, "p");
    for nominal in [0.2, 0.4, 0.8] {
        sigkill(pid);
        let t = Instant::now();
        pid = loop {
            let r = row(&srv, "p");
            match r.pid {
                Some(p) if p != pid && r.state == "running" => break p,
                _ => {}
            }
            assert!(t.elapsed() < Duration::from_secs(5), "never back: {r:?}");
            std::thread::sleep(Duration::from_millis(5));
        };
        let gap = t.elapsed().as_secs_f64();
        assert!(
            gap >= nominal * 0.9 && gap < nominal + 1.0,
            "restart after {gap:.3}s, wanted about {nominal}s"
        );
    }
    assert_eq!(row(&srv, "p").restarts, 3);
}

#[test]
fn the_fifth_crash_within_the_window_disables_the_plugin() {
    // Scale 0.05: the 60 s window is 3 s; delays 50, 100, 200, 400 ms.
    let srv = TestServer::unstarted();
    let starts = srv.home.join("starts.txt");
    let p = script(
        &srv,
        "p.sh",
        &format!("echo start >> {}\nexec sleep 600", starts.display()),
    );
    let srv = start_with(srv, &plugin_table("p", &p, ""), Some("0.05"));
    let mut pid = running(&srv, "p");
    for crash in 1..=5 {
        sigkill(pid);
        if crash < 5 {
            let prev = pid;
            wait_until(
                || {
                    let r = row(&srv, "p");
                    r.state == "running" && r.pid.is_some_and(|p| p != prev)
                },
                WAIT,
            )
            .expect("restarted");
            pid = row(&srv, "p").pid.expect("pid");
        }
    }
    wait_state(&srv, "p", "disabled");
    assert_eq!(row(&srv, "p").pid, None);
    // Longer than the next backoff (16 x 50 ms) would have been.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(row(&srv, "p").state, "disabled");
    assert_eq!(lines(&starts), 5, "nothing spawned after the fifth crash");
    let log = fs::read_to_string(srv.log_path()).unwrap_or_default();
    assert!(
        log.contains("grofe plugin p: disabled after 5 crashes"),
        "{log}"
    );

    // plugin-restart starts a disabled plugin again, with a clean count.
    srv.cmd(&["plugin-restart", "p"]).expect("plugin-restart");
    let pid = running(&srv, "p");
    assert!(alive(pid));
    assert_eq!(row(&srv, "p").restarts, 0);
}

#[test]
fn kill_server_leaves_no_plugin_process_and_no_child() {
    let srv = TestServer::unstarted();
    let child = srv.home.join("child.pid");
    // A leader with a child of its own, in the plugin's process group.
    let p = script(
        &srv,
        "p.sh",
        &format!("sleep 600 &\necho $! > {}\nwait", child.display()),
    );
    let srv = start_with(srv, &plugin_table("p", &p, ""), None);
    let pid = running(&srv, "p");
    let child_pid: i32 = read(&child).trim().parse().expect("child pid");
    assert!(alive(pid) && alive(child_pid));
    srv.cmd(&["kill-server"]).expect("kill-server");
    wait_until(|| !alive(pid) && !alive(child_pid), WAIT)
        .unwrap_or_else(|e| panic!("plugin {pid} or child {child_pid} left: {e}"));
}

#[test]
fn a_plugin_that_ignores_sigterm_is_killed_after_the_grace() {
    let srv = TestServer::unstarted();
    let ready = srv.home.join("ready");
    let p = script(
        &srv,
        "p.sh",
        &format!(
            "trap '' TERM\ntouch {}\nwhile :; do sleep 1; done",
            ready.display()
        ),
    );
    // Scale 0.25: the 2 s grace is half a second.
    let srv = start_with(srv, &plugin_table("p", &p, ""), Some("0.25"));
    let pid = running(&srv, "p");
    wait_until(|| ready.exists(), WAIT).expect("ready");
    let t = Instant::now();
    srv.cmd(&["kill-server"]).expect("kill-server");
    wait_until(|| !alive(pid), WAIT).expect("killed");
    assert!(
        t.elapsed() >= Duration::from_millis(400),
        "no grace: {:?}",
        t.elapsed()
    );
}

#[test]
fn a_plugin_does_not_delay_new_session() {
    let srv = TestServer::unstarted();
    // Never connects, never reads: holds the socket path and sleeps.
    let p = script(&srv, "p.sh", "exec sleep 600");
    let mut config = String::new();
    for name in ["a", "b", "c"] {
        config.push_str(&plugin_table(name, &p, ""));
    }
    let srv = start_with(srv, &config, None);
    let t = Instant::now();
    session(&srv);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "new-session took {:?}",
        t.elapsed()
    );
}

#[test]
fn stderr_and_stdout_land_in_the_plugin_log() {
    let srv = TestServer::unstarted();
    let p = script(&srv, "p.sh", "echo out-line\necho oops >&2\nexec sleep 600");
    let srv = start_with(srv, &plugin_table("noisy", &p, ""), None);
    running(&srv, "noisy");
    let log = srv.state_home.join("grofe/plugins/noisy.log");
    wait_until(
        || fs::read_to_string(&log).is_ok_and(|t| t.contains("oops") && t.contains("out-line")),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: {:?}", fs::read_to_string(&log)));
}

#[test]
fn the_log_rotates_at_one_mebibyte() {
    let srv = TestServer::unstarted();
    let p = script(
        &srv,
        "p.sh",
        "head -c 1300000 /dev/zero | tr '\\0' x\necho tail-marker >&2\nexec sleep 600",
    );
    let srv = start_with(srv, &plugin_table("big", &p, ""), None);
    running(&srv, "big");
    let dir = srv.state_home.join("grofe/plugins");
    let (log, old) = (dir.join("big.log"), dir.join("big.log.1"));
    wait_until(
        || fs::read_to_string(&log).is_ok_and(|t| t.contains("tail-marker")),
        WAIT,
    )
    .expect("tail written");
    let (new_len, old_len) = (
        fs::metadata(&log).expect("log").len(),
        fs::metadata(&old).expect("rotated log").len(),
    );
    assert!(
        old_len <= 1024 * 1024 && old_len > 900 * 1024,
        "old {old_len}"
    );
    assert!(new_len < 1024 * 1024, "new {new_len}");
}

#[test]
fn reload_starts_stops_and_restarts_plugins_whose_config_changed() {
    let srv = TestServer::unstarted();
    let p = script(&srv, "p.sh", "exec sleep 600");
    let q = script(&srv, "q.sh", "exec sleep 600");
    let both = format!(
        "{}{}",
        plugin_table("keep", &p, ""),
        plugin_table("gone", &q, "")
    );
    let srv = start_with(srv, &both, None);
    let keep = running(&srv, "keep");
    let gone = running(&srv, "gone");

    // Reload without `gone`, `keep` unchanged, `fresh` new, `env` changed.
    let config = format!(
        "{}{}{}",
        plugin_table("keep", &p, ""),
        plugin_table("fresh", &q, ""),
        plugin_table("changing", &q, "env = { A = \"1\" }")
    );
    write_config(&srv, &config);
    // `changing` starts under the first reload; then its env changes.
    srv.cmd(&["reload"]).expect("reload");
    wait_until(|| !alive(gone), WAIT).expect("gone is stopped");
    assert_eq!(rows(&srv).len(), 3, "{:?}", rows(&srv));
    assert_eq!(
        row(&srv, "keep").pid,
        Some(keep),
        "unchanged plugin left alone"
    );
    let fresh = running(&srv, "fresh");
    let changing = running(&srv, "changing");

    let config = format!(
        "{}{}{}",
        plugin_table("keep", &p, ""),
        plugin_table("fresh", &q, "enabled = false"),
        plugin_table("changing", &q, "env = { A = \"2\" }")
    );
    write_config(&srv, &config);
    srv.cmd(&["reload"]).expect("reload");
    wait_until(|| !alive(fresh) && !alive(changing), WAIT).expect("stopped");
    assert_eq!(row(&srv, "fresh").state, "off");
    let again = running(&srv, "changing");
    assert_ne!(again, changing, "a changed env restarts");
    assert_eq!(row(&srv, "keep").pid, Some(keep));

    // Enabling `fresh` starts it.
    write_config(
        &srv,
        &format!(
            "{}{}",
            plugin_table("keep", &p, ""),
            plugin_table("fresh", &q, "")
        ),
    );
    srv.cmd(&["reload"]).expect("reload");
    running(&srv, "fresh");
    wait_until(|| !alive(again), WAIT).expect("changing removed");
}

#[test]
fn reload_without_the_plugin_stops_it() {
    let srv = TestServer::unstarted();
    let p = script(&srv, "p.sh", "exec sleep 600");
    let srv = start_with(srv, &plugin_table("p", &p, ""), None);
    let pid = running(&srv, "p");
    write_config(&srv, "mouse = true\n");
    srv.cmd(&["reload"]).expect("reload");
    wait_until(|| !alive(pid), WAIT).expect("plugin stopped");
    assert!(rows(&srv).is_empty());
}

#[test]
fn a_disabled_plugin_is_off_and_plugin_restart_refuses_it() {
    let srv = TestServer::unstarted();
    let started = srv.home.join("started");
    let p = script(
        &srv,
        "p.sh",
        &format!("touch {}\nexec sleep 600", started.display()),
    );
    let srv = start_with(srv, &plugin_table("p", &p, "enabled = false"), None);
    let r = row(&srv, "p");
    assert_eq!((r.state.as_str(), r.pid, r.restarts), ("off", None, 0));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!started.exists(), "an off plugin is never started");
    let err = srv.cmd(&["plugin-restart", "p"]).expect_err("off");
    assert!(err.contains("off"), "{err}");
    let err = srv.cmd(&["plugin-restart", "nope"]).expect_err("unknown");
    assert!(err.contains("unknown plugin"), "{err}");
}

#[test]
fn restart_policies_decide_what_an_exit_leads_to() {
    let srv = TestServer::unstarted();
    let ok = script(&srv, "ok.sh", "exit 0");
    let bad = script(&srv, "bad.sh", "exit 3");
    let config = [
        plugin_table("never-ok", &ok, "restart = \"never\""),
        plugin_table("never-bad", &bad, "restart = \"never\""),
        plugin_table("fail-ok", &ok, "restart = \"on-failure\""),
        plugin_table("fail-bad", &bad, "restart = \"on-failure\""),
    ]
    .concat();
    let srv = start_with(srv, &config, None);
    for name in ["never-ok", "never-bad", "fail-ok"] {
        wait_state(&srv, name, "exited");
        assert_eq!(row(&srv, name).pid, None);
    }
    // on-failure with a non-zero exit goes on to restart.
    wait_until(|| row(&srv, "fail-bad").restarts >= 1, WAIT).expect("restarted");
    // plugin-restart starts an exited plugin again.
    srv.cmd(&["plugin-restart", "never-ok"]).expect("restart");
    wait_state(&srv, "never-ok", "exited");
}

#[test]
fn plugin_restart_replaces_the_process() {
    let srv = TestServer::unstarted();
    let p = script(&srv, "p.sh", "exec sleep 600");
    let srv = start_with(srv, &plugin_table("p", &p, ""), None);
    let pid = running(&srv, "p");
    srv.cmd(&["plugin-restart", "p"]).expect("plugin-restart");
    wait_until(
        || {
            let r = row(&srv, "p");
            r.state == "running" && r.pid.is_some_and(|n| n != pid)
        },
        WAIT,
    )
    .expect("a new process");
    wait_until(|| !alive(pid), WAIT).expect("the old one is gone");
}

#[test]
fn a_tmux_identity_server_gives_plugins_tmux_pointing_at_it() {
    let srv = TestServer::unstarted();
    let out = srv.home.join("tmux.txt");
    let p = script(
        &srv,
        "p.sh",
        &format!("echo \"$TMUX\" > {}\nexec sleep 600", out.display()),
    );
    let config = format!("compat-identity = \"tmux\"\n{}", plugin_table("p", &p, ""));
    let srv = start_with(srv, &config, None);
    running(&srv, "p");
    let value = read(&out);
    let pid = server_pid(&srv);
    let (path, rest) = value.trim_end().split_once(',').expect("TMUX shape");
    assert!(
        grofe::paths::same_socket(Path::new(path), &srv.socket),
        "{value}"
    );
    assert_eq!(rest, format!("{pid},-1"), "{value}");
}

#[test]
fn the_plugin_env_is_added_and_cannot_override_grofe() {
    let srv = TestServer::unstarted();
    let out = srv.home.join("env.txt");
    let p = script(
        &srv,
        "p.sh",
        &format!(
            "echo \"$MINE|$GROFE_PLUGIN|$GROFE\" > {}\nexec sleep 600",
            out.display()
        ),
    );
    let extra = "env = { MINE = \"yes\", GROFE_PLUGIN = \"x\", GROFE = \"/nope\" }";
    let srv = start_with(srv, &plugin_table("p", &p, extra), None);
    running(&srv, "p");
    let text = read(&out);
    let f: Vec<&str> = text.trim_end().split('|').collect();
    assert_eq!((f[0], f[1]), ("yes", "p"));
    assert_ne!(f[2], "/nope");
}

#[test]
fn a_relative_cmd_runs_from_the_config_file_s_directory() {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(dir.join("bin")).expect("bin dir");
    let marker = srv.home.join("marker");
    fs::write(
        dir.join("bin/p.sh"),
        format!("touch {}\nexec sleep 600\n", marker.display()),
    )
    .expect("script");
    let srv = start_with(srv, "[plugins.p]\ncmd = [\"sh\", \"bin/p.sh\"]\n", None);
    running(&srv, "p");
    wait_until(|| marker.exists(), WAIT).expect("the script ran");
}

// ---- config: schema, merge, resolution ----

fn config_env(dir: &Path) -> Env {
    Env {
        config_dir: Some(dir.to_path_buf()),
        home: None,
    }
}

fn load(dir: &Path) -> config::Resolved {
    config::load(&config_env(dir)).unwrap_or_else(|e| {
        panic!(
            "{}",
            e.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

fn errors(dir: &Path) -> String {
    config::load(&config_env(dir))
        .map(|_| String::new())
        .unwrap_or_else(|e| {
            e.iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        })
}

#[test]
fn plugin_tables_merge_key_by_key_across_layers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    fs::create_dir_all(dir.join("conf.d")).expect("conf.d");
    fs::write(
        dir.join("config.toml"),
        "[plugins.p]\ncmd = [\"a\", \"1\"]\nenv = { X = \"x\", Y = \"y\" }\nrestart = \"never\"\n",
    )
    .expect("config");
    fs::write(
        dir.join("conf.d/10.toml"),
        "[plugins.p]\nenv = { Y = \"y2\", Z = \"z\" }\n",
    )
    .expect("fragment");
    fs::write(dir.join("user.toml"), "[plugins.p]\nenabled = false\n").expect("user");

    let r = load(dir);
    let p = &r.plugins["p"];
    assert_eq!(p.cmd, ["a", "1"]);
    assert!(!p.enabled);
    assert_eq!(p.restart, Restart::Never);
    let env: Vec<(&str, &str)> = p
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(env, [("X", "x"), ("Y", "y2"), ("Z", "z")]);
    // Provenance per key.
    let file = |k: &str| {
        p.sources[k]
            .file
            .as_ref()
            .and_then(|f| f.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .expect("a file")
    };
    assert_eq!(file("cmd"), "config.toml");
    assert_eq!(file("env.Y"), "10.toml");
    assert_eq!(file("env.X"), "config.toml");
    assert_eq!(file("enabled"), "user.toml");
}

#[test]
fn a_plugin_needs_a_cmd_once_merged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(
        tmp.path().join("config.toml"),
        "[plugins.p]\nenabled = false\n",
    )
    .expect("config");
    let e = errors(tmp.path());
    assert!(
        e.contains("config.toml:1") && e.contains("plugins.p.cmd") && e.contains("missing cmd"),
        "{e}"
    );
}

#[test]
fn plugin_schema_errors_name_the_file_and_key() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("config.toml");
    for (text, want) in [
        (
            "[plugins.p]\ncmd = []\n",
            "plugins.p.cmd: cmd must not be empty",
        ),
        (
            "[plugins.p]\ncmd = \"sh\"\n",
            "plugins.p.cmd: expected an array of strings",
        ),
        (
            "[plugins.p]\ncmd = [\"x\"]\nrestart = \"sometimes\"\n",
            "plugins.p.restart",
        ),
        (
            "[plugins.p]\ncmd = [\"x\"]\nenabled = 1\n",
            "plugins.p.enabled: expected a boolean",
        ),
        (
            "[plugins.p]\ncmd = [\"x\"]\nenv = { A = 1 }\n",
            "plugins.p.env.A: expected a string",
        ),
        (
            "[plugins.p]\ncmd = [\"x\"]\nenable = true\n",
            "did you mean enabled?",
        ),
        ("[plugins.\"a b\"]\ncmd = [\"x\"]\n", "a plugin name is"),
        ("plugins = 1\n", "expected a table of plugins"),
    ] {
        fs::write(&path, text).expect("config");
        let e = errors(tmp.path());
        assert!(
            e.contains("config.toml:") && e.contains(want),
            "{text:?} gave {e:?}, wanted {want:?}"
        );
    }
}

#[test]
fn a_relative_cmd_resolves_against_the_file_that_set_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    fs::create_dir_all(dir.join("conf.d/sub")).expect("dirs");
    fs::write(dir.join("conf.d/sub/run.sh"), "").expect("script");
    fs::write(
        dir.join("conf.d/a.toml"),
        "[plugins.a]\ncmd = [\"sub/run.sh\", \"x/none\", \"-c\"]\n\
         [plugins.b]\ncmd = [\"sh\", \"sub/run.sh\", \"/abs/p\", \"--f=a/b\"]\n\
         [plugins.c]\ncmd = [\"sh\"]\n",
    )
    .expect("fragment");
    let r = load(dir);
    let here = |p: &str| dir.join("conf.d").join(p).display().to_string();
    assert_eq!(
        r.plugins["a"].cmd,
        [here("sub/run.sh"), "x/none".into(), "-c".into()]
    );
    assert_eq!(
        r.plugins["b"].cmd,
        [
            "sh".to_string(),
            here("sub/run.sh"),
            "/abs/p".into(),
            "--f=a/b".into()
        ]
    );
    assert_eq!(
        r.plugins["c"].cmd,
        ["sh"],
        "a bare name is looked up on PATH"
    );
}

#[test]
fn show_config_lists_a_plugin_and_who_set_each_key() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    fs::write(
        dir.join("config.toml"),
        "[plugins.event-log]\ncmd = [\"sh\", \"x\"]\nenv = { LC_ALL = \"C\" }\n",
    )
    .expect("config");
    fs::write(
        dir.join("user.toml"),
        "[plugins.event-log]\nrestart = \"never\"\n",
    )
    .expect("user");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
    cmd.args(["show-config", "--explain", "plugins.event-log"])
        .env("XDG_CONFIG_HOME", dir.parent().expect("parent"))
        .env("HOME", dir);
    // The loader reads `<XDG_CONFIG_HOME>/grofe`; lay it out that way.
    let home = tempfile::tempdir().expect("tempdir");
    let cfg = home.path().join("grofe");
    fs::create_dir_all(&cfg).expect("grofe dir");
    for f in ["config.toml", "user.toml"] {
        fs::copy(dir.join(f), cfg.join(f)).expect("copy");
    }
    cmd.env("XDG_CONFIG_HOME", home.path());
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.contains("plugins.event-log = { cmd = [\"sh\", \"x\"], env = { LC_ALL = \"C\" }, enabled = true, restart = \"never\" }"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("restart set by") && out.stdout.contains("user.toml"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("env.LC_ALL set by") && out.stdout.contains("config.toml"),
        "{}",
        out.stdout
    );
}
