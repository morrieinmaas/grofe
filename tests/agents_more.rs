//! Agents sidebar, grid and `agent new` (plan T9): the real surfaces driven
//! through an attached client's pty on a private `-S` socket.

mod common;
#[allow(dead_code, clippy::duplicate_mod)]
#[path = "acceptance/harness.rs"]
mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{PROMPT, TestServer, wait_until};
use harness::Harness;

const WAIT: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";

fn start() -> TestServer {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "-x", "160", "-y", "40"])
        .expect("new-session");
    wait_prompt(&srv, "main");
    srv
}

fn wait_prompt(srv: &TestServer, target: &str) {
    srv.wait_for(
        || {
            srv.capture(target)
                .lines()
                .any(|l| l.trim_end().ends_with(PROMPT.trim_end()))
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: prompt in {target}"));
}

fn attach(srv: &TestServer) -> Harness {
    let mut a = Harness::spawn_attach_with_env(srv.command(), "main", &[], 160, 40);
    a.wait_for_output(b"\x1b[2J", 0);
    a
}

fn display(srv: &TestServer, format: &str) -> String {
    srv.cmd(&["display-message", "-p", "-t", "main", format])
        .unwrap_or_else(|e| panic!("display {format}: {e}"))
        .trim_end()
        .to_string()
}

fn window_count(srv: &TestServer) -> usize {
    srv.cmd(&["list-windows", "-t", "main"])
        .expect("list-windows")
        .lines()
        .count()
}

/// `grofe agent report idle` typed into `target`'s shell,
/// then a long-lived foreground process so the record's binding stays alive
/// past the sweep (it binds to the pane's foreground process).
fn report_idle(srv: &TestServer, target: &str) {
    let line = format!(
        "sh -c '{} -S {} agent report idle; exec sleep 60'",
        env!("CARGO_BIN_EXE_grofe"),
        srv.socket.display()
    );
    srv.cmd(&["send-keys", "-t", target, "-l", &line])
        .expect("send-keys -l");
    srv.cmd(&["send-keys", "-t", target, "Enter"])
        .expect("send-keys Enter");
}

fn agent_list(srv: &TestServer) -> String {
    srv.cmd(&["agent", "list", "-F", "#{agent_state} #{agent_name}"])
        .unwrap_or_default()
}

fn wait_agents(srv: &TestServer, n: usize) {
    wait_until(
        || {
            agent_list(srv)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
                == n
        },
        WAIT,
    )
    .unwrap_or_else(|e| panic!("{e}: want {n} agents"));
}

/// A kind `claude` whose command is a harmless script that sleeps.
fn write_claude_kind(srv: &TestServer) {
    let script = srv.home.join("fake-claude.sh");
    fs::write(&script, "#!/bin/sh\nexec sleep 30\n").expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    let conf = srv.config_home.join("grofe").join("conf.d");
    fs::create_dir_all(&conf).expect("conf.d");
    fs::write(
        conf.join("50-agents.toml"),
        format!(
            "[agents.kinds.claude]\ncommand = \"{}\"\n",
            script.display()
        ),
    )
    .expect("write config");
    srv.cmd(&["reload"]).expect("reload");
}

#[test]
fn sidebar_toggle_resizes_window() {
    let srv = start();
    let mut a = attach(&srv);
    assert_eq!(display(&srv, "#{window_width}"), "160");
    a.send(PREFIX);
    a.send("b");
    wait_until(|| display(&srv, "#{window_width}") == "127", WAIT)
        .unwrap_or_else(|e| panic!("{e}: window shrinks to 127"));
    a.send(PREFIX);
    a.send("b");
    wait_until(|| display(&srv, "#{window_width}") == "160", WAIT)
        .unwrap_or_else(|e| panic!("{e}: window back to 160"));
}

#[test]
fn grid_enter_jumps() {
    let srv = start();
    let first = display(&srv, "#{window_id}");
    report_idle(&srv, &first);
    srv.cmd(&["new-window", "-t", "main"]).expect("new-window");
    let second = display(&srv, "#{window_id}");
    wait_prompt(&srv, &second);
    report_idle(&srv, &second);
    wait_agents(&srv, 2);
    assert_eq!(display(&srv, "#{window_id}"), second);
    let mut a = attach(&srv);
    a.send(PREFIX);
    a.send("O");
    // Tile titles carry the target label, not the message.
    a.wait_for("main:1.1");
    a.wait_for("main:2.1");
    a.send("main:1.");
    a.wait_until("filtered to window 1", WAIT, |t| {
        t.contains("main:1.1") && !t.contains("main:2.1")
    });
    a.send("\r");
    wait_until(|| display(&srv, "#{window_id}") == first, WAIT)
        .unwrap_or_else(|e| panic!("{e}: jumped to the first window"));
}

#[test]
fn agent_new_opens_window_with_record() {
    let srv = start();
    write_claude_kind(&srv);
    assert_eq!(window_count(&srv), 1);
    srv.cmd(&["agent", "new", "claude"]).expect("agent new");
    wait_until(|| window_count(&srv) == 2, WAIT).unwrap_or_else(|e| panic!("{e}: new window"));
    wait_agents(&srv, 1);
    let list = agent_list(&srv);
    assert!(list.starts_with("idle"), "got {list:?}");
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgsign=false", "-c", "user.name=t", "-c"])
        .arg("user.email=t@example.com")
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn agent_new_worktree_with_git_repo() {
    let srv = start();
    write_claude_kind(&srv);
    let root = tempfile::tempdir().expect("tempdir");
    let base = fs::canonicalize(root.path()).expect("canonicalize");
    let repo = base.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "--no-verify", "-m", "init"],
    );
    let repo_arg = repo.to_str().expect("utf8 path");
    srv.cmd(&[
        "agent", "new", "claude", "-c", repo_arg, "-w", "-b", "feat/x",
    ])
    .expect("agent new -w");
    let wt: PathBuf = base.join("worktrees").join("feat-x");
    assert!(wt.is_dir(), "worktree {wt:?} missing");
    wait_until(|| window_count(&srv) == 2, WAIT).unwrap_or_else(|e| panic!("{e}: new window"));
    let paths = srv
        .cmd(&["list-windows", "-t", "main", "-F", "#{pane_current_path}"])
        .expect("list-windows");
    let want = wt.to_str().expect("utf8 path");
    assert!(
        paths.lines().any(|l| l.trim_end() == want),
        "want {want} in {paths:?}"
    );
}
