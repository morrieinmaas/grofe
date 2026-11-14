//! T4.5: `grofe reboot-check [--json]` (spec §6). It reads the snapshot on
//! disk, never the writer's memory, and gives each pane a verdict; the
//! exit status says whether every pane that expects a resume will get it
//! (0), some will not (1), or persistence is misconfigured (2).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::thread;
use std::time::Duration;

use common::{CmdOutput, PROMPT, TestServer};

const WAIT: Duration = Duration::from_secs(10);

/// The config every test starts from: indexes from 0, so targets read
/// `main:0.0`.
const ZERO: &str = "base-index = 0\npane-base-index = 0\n";

/// A server with a detached session `main` of one pane.
fn server() -> TestServer {
    server_with_config("")
}

/// A server started over a temp `config.toml` holding `config`.
fn server_with_config(config: &str) -> TestServer {
    let srv = TestServer::unstarted();
    let dir = srv.config_home.join("grofe");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("config.toml"), format!("{ZERO}{config}")).expect("write config");
    let srv = srv.restart();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        .expect("new-session");
    // The login shell's startup is done: until then its foreground process
    // can be a startup command that exits before pane-set-resume binds it.
    srv.wait_for(
        || {
            srv.capture("main:0.0")
                .lines()
                .any(|l| l.trim_end().ends_with(PROMPT.trim_end()))
        },
        Duration::from_secs(10),
    )
    .expect("the shell's prompt");
    srv
}

fn uid(srv: &TestServer, pane: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", pane, "#{pane_uid}"])
        .expect("pane_uid")
        .trim()
        .to_string()
}

fn check(srv: &TestServer) -> CmdOutput {
    srv.run(&["reboot-check"])
}

fn code(out: &CmdOutput) -> Option<i32> {
    out.status.code()
}

/// The snapshot's pane `uid`, if the file has it.
fn saved_pane(srv: &TestServer, uid: &str) -> Option<serde_json::Value> {
    let text = fs::read_to_string(srv.snapshot_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    for s in v["sessions"].as_array()? {
        for w in s["windows"].as_array()? {
            for p in w["panes"].as_array()? {
                if p["uid"] == uid {
                    return Some(p.clone());
                }
            }
        }
    }
    None
}

fn wait_saved(srv: &TestServer, uid: &str, resume: serde_json::Value) {
    srv.wait_for(
        || saved_pane(srv, uid).is_some_and(|p| p["resume"] == resume),
        WAIT,
    )
    .unwrap_or_else(|e| panic!("pane {uid} saved with {resume}: {e}"));
}

/// The line `reboot-check` printed for pane `uid`.
fn line_for<'a>(out: &'a CmdOutput, uid: &str) -> &'a str {
    out.stdout
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(uid))
        .unwrap_or_else(|| panic!("no line for {uid} in {:?}", out.stdout))
}

#[test]
fn every_resumed_pane_saved_exits_0() {
    let srv = server();
    srv.cmd(&["split-window", "-d", "-t", "main"])
        .expect("split");
    let a = uid(&srv, "main:0.0");
    let b = uid(&srv, "main:0.1");
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat", "-u"])
        .expect("set resume");
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat", "-u"]));
    wait_saved(&srv, &b, serde_json::Value::Null);
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(line_for(&out, &a), format!("main:0.0 {a} will resume"));
    assert_eq!(
        line_for(&out, &b),
        format!("main:0.1 {b} restores as a shell")
    );
    assert_eq!(out.stdout.lines().count(), 2, "{out:?}");
}

#[test]
fn a_resume_set_less_than_a_second_ago_is_not_saved_yet() {
    let srv = server();
    let a = uid(&srv, "main:0.0");
    // The pane is in the file already, without a resume: only the new
    // resume is missing.
    wait_saved(&srv, &a, serde_json::Value::Null);
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat"])
        .expect("set resume");
    let out = check(&srv);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(
        line_for(&out, &a).starts_with(&format!("main:0.0 {a} not saved yet")),
        "{out:?}"
    );
    thread::sleep(Duration::from_secs(2));
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(line_for(&out, &a), format!("main:0.0 {a} will resume"));
}

#[test]
fn a_respawn_with_a_command_is_not_saved_yet_until_written() {
    // Not through pane-set-resume: the writer's own pending state says so.
    let srv = server();
    let a = uid(&srv, "main:0.0");
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat"])
        .expect("set resume");
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat"]));
    srv.cmd(&["respawn-pane", "-k", "-t", "main:0.0", "/bin/cat", "-u"])
        .expect("respawn with a command");
    let out = check(&srv);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(
        line_for(&out, &a).starts_with(&format!("main:0.0 {a} not saved yet")),
        "{out:?}"
    );
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat", "-u"]));
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(line_for(&out, &a), format!("main:0.0 {a} will resume"));
}

#[test]
fn it_reads_the_disk_not_the_writers_memory() {
    let srv = server();
    let a = uid(&srv, "main:0.0");
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat"])
        .expect("set resume");
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat"]));
    srv.edit_snapshot(|v| {
        for s in v["sessions"].as_array_mut().into_iter().flatten() {
            for w in s["windows"].as_array_mut().into_iter().flatten() {
                for p in w["panes"].as_array_mut().into_iter().flatten() {
                    if p["uid"] == a.as_str() {
                        p["resume"] = serde_json::json!(["/bin/cat", "old"]);
                    }
                }
            }
        }
    });
    let out = check(&srv);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(
        line_for(&out, &a),
        format!(
            "main:0.0 {a} will restart without its conversation (the snapshot holds an older resume)"
        )
    );
}

#[test]
fn an_expectation_gives_its_reason() {
    let srv = server();
    let a = uid(&srv, "main:0.0");
    srv.cmd(&[
        "pane-set-resume",
        "-t",
        "main:0.0",
        "--expect",
        "no session id reported",
    ])
    .expect("expect");
    wait_saved(&srv, &a, serde_json::Value::Null);
    let out = check(&srv);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(
        line_for(&out, &a),
        format!("main:0.0 {a} will restart without its conversation (no session id reported)")
    );
}

#[test]
fn only_plain_shells_exit_0() {
    let srv = server();
    srv.cmd(&["split-window", "-d", "-t", "main"])
        .expect("split");
    srv.cmd(&["new-window", "-d", "-t", "main"])
        .expect("window");
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(out.stdout.lines().count(), 3, "{out:?}");
    for line in out.stdout.lines() {
        assert!(line.ends_with(" restores as a shell"), "{line}");
    }
}

#[test]
fn restore_off_and_no_unit_is_misconfigured() {
    let srv = server_with_config("restore-on-start = false\n");
    srv.cmd(&["split-window", "-d", "-t", "main"])
        .expect("split");
    let a = uid(&srv, "main:0.0");
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat"])
        .expect("set resume");
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat"]));
    let out = check(&srv);
    assert_eq!(code(&out), Some(2), "{out:?}");
    assert_eq!(out.stdout.lines().count(), 2, "{out:?}");
    for line in out.stdout.lines() {
        assert!(line.contains(" will not be restored"), "{line}");
    }
    assert!(out.stderr.contains("restore-on-start"), "{out:?}");
}

#[test]
fn a_systemd_unit_that_restores_makes_restore_off_fine() {
    let srv = server_with_config("restore-on-start = false\n");
    let units = srv.home.join(".config/systemd/user");
    fs::create_dir_all(&units).expect("unit dir");
    fs::write(
        units.join("grofe.service"),
        "[Unit]\nDescription=grofe\n\n[Service]\nType=forking\n\
         ExecStart=/usr/bin/tmux start-server --restore\n\n\
         [Install]\nWantedBy=default.target\n",
    )
    .expect("write unit");
    let a = uid(&srv, "main:0.0");
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(
        line_for(&out, &a),
        format!("main:0.0 {a} restores as a shell")
    );
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat"])
        .expect("set resume");
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat"]));
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(line_for(&out, &a), format!("main:0.0 {a} will resume"));
}

#[test]
fn a_unit_that_does_not_restore_does_not_count() {
    let srv = server_with_config("restore-on-start = false\n");
    let units = srv.home.join(".config/systemd/user");
    fs::create_dir_all(&units).expect("unit dir");
    fs::write(
        units.join("grofe.service"),
        "[Service]\nExecStart=/usr/bin/tmux start-server\n",
    )
    .expect("write unit");
    assert_eq!(code(&check(&srv)), Some(2));
}

#[test]
fn no_server_exits_0_with_a_message() {
    let srv = TestServer::unstarted();
    let out = check(&srv);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert!(out.stdout.contains("no server"), "{out:?}");
    assert!(!srv.is_running(), "reboot-check started a server");
}

#[test]
fn a_read_only_state_directory_is_misconfigured() {
    if nix::unistd::geteuid().is_root() {
        // Root writes through any mode bits: nothing to prove.
        return;
    }
    let srv = server();
    let a = uid(&srv, "main:0.0");
    wait_saved(&srv, &a, serde_json::Value::Null);
    let snap = srv.snapshot_path();
    let dir = snap.parent().expect("instance dir");
    let restore = |mode| fs::set_permissions(dir, fs::Permissions::from_mode(mode));
    restore(0o500).expect("chmod 0500");
    let out = check(&srv);
    restore(0o700).expect("chmod back");
    assert_eq!(code(&out), Some(2), "{out:?}");
    assert!(out.stderr.contains("not writable"), "{out:?}");
    assert!(out.stderr.contains(&dir.display().to_string()), "{out:?}");
}

#[test]
fn json_has_one_entry_per_pane() {
    let srv = server();
    srv.cmd(&["split-window", "-d", "-t", "main"])
        .expect("split");
    srv.cmd(&["new-session", "-d", "-s", "other"])
        .expect("other");
    let a = uid(&srv, "main:0.0");
    srv.cmd(&["pane-set-resume", "-t", "main:0.0", "--", "/bin/cat"])
        .expect("set resume");
    wait_saved(&srv, &a, serde_json::json!(["/bin/cat"]));
    let out = srv.run(&["reboot-check", "--json"]);
    assert_eq!(code(&out), Some(0), "{out:?}");
    let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("JSON");
    let entries = v.as_array().expect("an array");
    assert_eq!(entries.len(), 3, "{v}");
    let mine = entries
        .iter()
        .find(|e| e["uid"] == a.as_str())
        .expect("entry for a");
    assert_eq!(mine["target"], "main:0.0");
    assert_eq!(mine["verdict"], "will resume");
    for e in entries {
        assert!(e["verdict"].is_string() && e["target"].is_string(), "{e}");
    }
}

#[test]
fn a_new_pane_is_not_saved_yet_until_the_writer_has_it() {
    // A pane spawned with a command has that command as its resume.
    let srv = server();
    srv.cmd(&["split-window", "-d", "-t", "main", "/bin/cat"])
        .expect("split with a command");
    let b = uid(&srv, "main:0.1");
    let out = check(&srv);
    let line = line_for(&out, &b).to_string();
    if line.contains("not saved yet") {
        assert_eq!(code(&out), Some(1), "{out:?}");
    }
    srv.wait_for(|| code(&check(&srv)) == Some(0), WAIT)
        .expect("saved within the debounce");
    assert_eq!(
        line_for(&check(&srv), &b),
        format!("main:0.1 {b} will resume")
    );
}
