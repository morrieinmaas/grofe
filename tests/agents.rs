//! Agents step 1 (spec §13): reports through the CLI, hook mode and OSC 7501,
//! recognition by program, resume from a reported session id, ordering, and
//! the `agent-changed` event, all on a private `-S` socket.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use common::{PROMPT, TestServer};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(10);

const LIST_FORMAT: &str = "#{pane_id} #{agent_kind} #{agent_state}";

struct Fixture {
    srv: TestServer,
    dir: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let srv = TestServer::start();
        srv.cmd(&["new-session", "-d", "-s", "main", "-x", "120", "-y", "30"])
            .expect("new-session");
        let f = Fixture {
            srv,
            dir: tempfile::tempdir().expect("tempdir"),
        };
        f.wait_prompt();
        f
    }

    fn wait_prompt(&self) {
        self.srv
            .wait_for(
                || {
                    self.srv
                        .capture("main")
                        .lines()
                        .any(|l| l.trim_end().ends_with(PROMPT.trim_end()))
                },
                WAIT,
            )
            .expect("shell prompt");
    }

    fn pane_id(&self) -> String {
        self.srv
            .cmd(&["display-message", "-p", "-t", "main", "#{pane_id}"])
            .expect("display-message")
            .trim()
            .to_string()
    }

    fn type_line(&self, line: &str) {
        self.srv
            .cmd(&["send-keys", "-t", "main", "-l", line])
            .expect("send-keys -l");
        self.srv
            .cmd(&["send-keys", "-t", "main", "Enter"])
            .expect("send-keys Enter");
    }

    /// Runs the grofe binary from the pane's shell, so it has `GROFE_PANE`.
    fn grofe_in_pane(&self, args: &str) {
        self.type_line(&format!(
            "{} -S {} {args}",
            env!("CARGO_BIN_EXE_grofe"),
            self.srv.socket.display()
        ));
    }

    fn list(&self) -> String {
        self.srv
            .cmd(&["agent", "list", "-F", LIST_FORMAT])
            .unwrap_or_default()
    }

    fn wait_list(&self, want: &str) {
        self.srv
            .wait_for(|| self.list().lines().any(|l| l == want), WAIT)
            .unwrap_or_else(|e| panic!("{e}: wanted {want:?} in {:?}", self.list()));
    }

    /// A temp `bin` dir holding a `claude` that runs `secs` seconds under
    /// that name (`exec -a`, so argv[0] is `claude` whatever the shebang), printing
    /// each second so the output-driven scan sees it.
    fn fake_claude(&self, secs: u32) -> PathBuf {
        let bin = self.dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let script = bin.join("claude");
        fs::write(
            &script,
            format!(
                "#!/bin/bash\nexec -a claude /bin/sh -c 'i=0; while [ $i -lt {secs} ]; do echo tick; sleep 1; i=$((i+1)); done'\n"
            ),
        )
        .expect("write claude");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        bin
    }
}

#[test]
fn report_from_pane_env_listed() {
    let f = Fixture::new();
    f.grofe_in_pane("agent report working --msg hello");
    let id = f.pane_id();
    f.wait_list(&format!("{id}  working"));
    let out = f
        .srv
        .cmd(&["agent", "list", "-F", "#{agent_msg}"])
        .expect("list");
    assert_eq!(out.trim(), "hello");
}

#[test]
fn hook_exit_0_garbage_stdin() {
    let f = Fixture::new();
    let id = f.pane_id();
    let hook = |srv: &TestServer, pane: Option<&str>| {
        let mut cmd = srv.command();
        cmd.args(["agent", "report", "done", "--hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(p) = pane {
            cmd.env("GROFE_PANE", p);
        }
        let mut child = cmd.spawn().expect("spawn hook");
        let _ = child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"\x00{not json\xff");
        child.wait().expect("wait hook")
    };
    assert!(hook(&f.srv, Some(&id)).success(), "garbage stdin");
    f.wait_list(&format!("{id} claude done"));
    assert!(hook(&f.srv, None).success(), "no GROFE_PANE");

    let none = TestServer::builder().unstarted();
    assert!(hook(&none, Some("%0")).success(), "no server");
    assert!(!none.is_running(), "a hook never starts a server");
}

#[test]
fn osc7501_pane_becomes_agent() {
    let f = Fixture::new();
    f.type_line(r"printf '\033]7501;state=blocked\033\\'");
    let id = f.pane_id();
    f.wait_list(&format!("{id}  blocked"));
}

#[test]
fn fake_claude_on_path_recognised_and_exit_removes() {
    let f = Fixture::new();
    let bin = f.fake_claude(4);
    f.type_line(&format!("PATH={}:$PATH claude", bin.display()));
    let id = f.pane_id();
    f.wait_list(&format!("{id} claude idle"));
    f.srv
        .wait_for(|| f.list().trim().is_empty(), WAIT)
        .unwrap_or_else(|e| panic!("{e}: still listed: {:?}", f.list()));
}

#[test]
fn resume_set_from_session_id() {
    let f = Fixture::new();
    let bin = f.fake_claude(1);
    // The CLI resolves argv[0] against its own PATH.
    f.type_line(&format!(
        "PATH={}:$PATH {} -S {} agent report idle --kind claude --session-id s1",
        bin.display(),
        env!("CARGO_BIN_EXE_grofe"),
        f.srv.socket.display()
    ));
    f.srv
        .wait_for(
            || {
                f.srv
                    .cmd(&["display-message", "-p", "-t", "main", "#{pane_resume}"])
                    .is_ok_and(|r| r.trim().ends_with("claude --resume s1"))
            },
            WAIT,
        )
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn late_report_dropped() {
    let f = Fixture::new();
    let id = f.pane_id();
    let report = |state: &str, ts: &str| {
        f.srv
            .cmd(&["agent", "report", state, "-t", &id, "--ts", ts])
            .expect("agent report");
    };
    report("working", "200");
    f.wait_list(&format!("{id}  working"));
    report("done", "100");
    common::quiet_period(Duration::from_millis(300));
    assert_eq!(f.list().trim(), format!("{id}  working"), "older dropped");
    report("done", "300");
    f.wait_list(&format!("{id}  done"));
}

#[test]
fn agent_changed_subscribed() {
    let f = Fixture::new();
    let id = f.pane_id();
    let mut sub = f.srv.subscribe(&["agent-changed"]).expect("subscribe");
    f.srv
        .cmd(&["agent", "report", "blocked", "-t", &id, "--ts", "1"])
        .expect("agent report");
    let ev = sub.next(WAIT).expect("an agent-changed event");
    let text = format!("{ev:?}");
    assert!(text.contains("AgentChanged"), "{text}");
    assert!(text.contains("blocked"), "{text}");
}

#[test]
fn agent_changed_hook_fires() {
    let f = Fixture::new();
    let id = f.pane_id();
    f.srv
        .cmd(&["set-hook", "-g", "agent-changed", "set -g -a @ac x"])
        .expect("set-hook");
    f.srv
        .cmd(&["agent", "report", "blocked", "-t", &id, "--ts", "1"])
        .expect("agent report");
    f.srv
        .wait_for(
            || {
                f.srv
                    .cmd(&["show-options", "-gqv", "@ac"])
                    .is_ok_and(|r| r.trim() == "x")
            },
            WAIT,
        )
        .unwrap_or_else(|e| panic!("{e}: agent-changed hook never ran"));
}

#[test]
fn osc7501_app_does_not_replace_a_known_kind() {
    let f = Fixture::new();
    let id = f.pane_id();
    f.srv
        .cmd(&[
            "agent", "report", "idle", "-t", &id, "--kind", "claude", "--ts", "1",
        ])
        .expect("agent report");
    f.wait_list(&format!("{id} claude idle"));
    f.type_line(r"printf '\033]7501;state=working:app=other\033\\'");
    f.wait_list(&format!("{id} claude working"));
}
