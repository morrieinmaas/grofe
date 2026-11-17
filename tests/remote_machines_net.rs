//! Saved machines over the network (spec ssh-remote §5), with no real
//! ssh: `GROFE_SSH` names a fake ssh that sends each host to its own
//! private-socket server (`alpha`, `beta`) and runs the remote command
//! there, and fails like ssh (255) for a host whose `down-<host>` file
//! exists or that it does not know. The local server is a third.

mod common;
#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, run_bounded};
use harness::Harness;
use tempfile::TempDir;

/// The picker box's top edge.
const BOX: &str = "─ machines ─";

/// The local server, two remote ones, and the fake ssh.
struct Fleet {
    local: TestServer,
    alpha: TestServer,
    beta: TestServer,
    dir: TempDir,
}

impl Fleet {
    /// The machines are saved as `a` (target `alpha`) and `b` (target
    /// `beta`, session `work`), so a name only works through the file.
    fn new() -> Fleet {
        let (local, alpha, beta) = (
            TestServer::start(),
            TestServer::start(),
            TestServer::start(),
        );
        let dir = tempfile::Builder::new()
            .prefix("grofe-mn")
            .tempdir()
            .expect("tempdir");
        let root = dir.path();
        for d in ["home", "bin", "local-home", "local-config", "local-state"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), root.join("bin/grofe")).unwrap();
        let env = |s: &TestServer| {
            format!(
                "export GROFE='{}' XDG_STATE_HOME='{}' XDG_RUNTIME_DIR='{}' XDG_CONFIG_HOME='{}'",
                s.socket.display(),
                s.state_home.display(),
                s.runtime_dir.display(),
                s.config_home.display()
            )
        };
        let script = format!(
            r#"#!/bin/sh
for a; do [ "$a" = -G ] && {{ echo "serveraliveinterval 0"; exit 0; }}; done
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
host="$2"
shift 2
[ -e '{root}/down-'"$host" ] && {{ echo "ssh: connect to host $host: Connection refused" >&2; exit 255; }}
case "$host" in
  alpha) {alpha} ;;
  beta) {beta} ;;
  *) echo "ssh: Could not resolve hostname $host" >&2; exit 255 ;;
esac
export HOME='{root}/home' PATH='{root}/bin:/usr/bin:/bin'
unset GROFE_PANE TMUX
# `exec` inside too: dash (Linux /bin/sh) forks for a -c command's last
# step where bash execs, and the test must signal the proxy itself.
exec /bin/sh -c "exec $*"
"#,
            root = root.display(),
            alpha = env(&alpha),
            beta = env(&beta),
        );
        let ssh = root.join("ssh");
        fs::write(&ssh, script).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let f = Fleet {
            local,
            alpha,
            beta,
            dir,
        };
        for args in [
            &["machine", "add", "a", "alpha"][..],
            &["machine", "add", "b", "beta", "--session", "work"][..],
        ] {
            let out = run_bounded(f.grofe(args), CMD_TIMEOUT);
            assert!(out.status.success(), "{out:?}");
        }
        f
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A local `grofe -S <local> <args>` with the fake ssh and private dirs.
    fn grofe(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.arg("-S")
            .arg(&self.local.socket)
            .args(args)
            .current_dir(self.root())
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env_remove("TMUX")
            .env("GROFE_SSH", self.root().join("ssh"))
            .env("HOME", self.root().join("local-home"))
            .env("XDG_CONFIG_HOME", self.root().join("local-config"))
            .env("XDG_STATE_HOME", self.root().join("local-state"))
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        cmd
    }

    fn set_down(&self, host: &str, down: bool) {
        let f = self.root().join(format!("down-{host}"));
        if down {
            fs::write(f, "").unwrap();
        } else {
            let _ = fs::remove_file(f);
        }
    }
}

/// A session `s` on `srv` with one agent reporting `state` as `kind`.
fn agent(srv: &TestServer, s: &str, kind: &str, state: &str) {
    srv.cmd(&["new-session", "-d", "-s", s]).unwrap();
    let pane = srv
        .cmd(&["display-message", "-p", "-t", s, "#{pane_id}"])
        .unwrap();
    srv.cmd(&["agent", "report", state, "-t", pane.trim(), "--kind", kind])
        .unwrap();
}

#[test]
fn dash_r_and_machine_resolve_a_saved_name() {
    let f = Fleet::new();
    f.alpha
        .cmd(&["new-session", "-d", "-s", "on-alpha"])
        .unwrap();
    for args in [
        &["-r", "a", "list-sessions"][..],
        &["--machine", "a", "list-sessions"],
    ] {
        let out = run_bounded(f.grofe(args), CMD_TIMEOUT);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(out.stdout.contains("on-alpha:"), "{args:?}: {out:?}");
    }
    // Not saved: `-r` takes it as an ssh target, `--machine` refuses it.
    let out = run_bounded(f.grofe(&["-r", "alpha", "list-sessions"]), CMD_TIMEOUT);
    assert!(out.stdout.contains("on-alpha:"), "{out:?}");
    let out = run_bounded(
        f.grofe(&["--machine", "nope", "list-sessions"]),
        CMD_TIMEOUT,
    );
    assert!(!out.status.success());
    assert!(out.stderr.contains("no machine named 'nope'"), "{out:?}");
    // `--machine local` is this machine.
    f.local.cmd(&["new-session", "-d", "-s", "here"]).unwrap();
    let out = run_bounded(
        f.grofe(&["--machine", "local", "list-sessions"]),
        CMD_TIMEOUT,
    );
    assert!(
        out.stdout.contains("here:") && !out.stdout.contains("on-alpha"),
        "{out:?}"
    );
}

#[test]
fn agent_list_all_machines_has_a_machine_column_and_offline_rows() {
    let f = Fleet::new();
    agent(&f.local, "l", "claude", "working");
    agent(&f.alpha, "x", "codex", "blocked");
    agent(&f.beta, "y", "claude", "done");
    let out = run_bounded(f.grofe(&["agent", "list", "--all-machines"]), CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    let rows: Vec<Vec<&str>> = out
        .stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    let shape: Vec<(&str, &str, &str)> = rows.iter().map(|r| (r[0], r[2], r[3])).collect();
    assert_eq!(
        shape,
        vec![
            ("local", "claude", "working"),
            ("a", "codex", "blocked"),
            ("b", "claude", "done"),
        ],
        "{}",
        out.stdout
    );

    // beta goes down: its last known agent stays, marked offline.
    f.set_down("beta", true);
    let out = run_bounded(f.grofe(&["agent", "list", "--all-machines"]), CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    let b: Vec<&str> = out.stdout.lines().filter(|l| l.starts_with("b ")).collect();
    assert_eq!(b.len(), 1, "{}", out.stdout);
    assert!(b[0].contains("claude done offline"), "{}", out.stdout);
    assert!(out.stdout.contains("codex blocked\n"), "alpha still online");
}

#[test]
fn the_picker_switches_the_attach_and_dims_an_offline_machine() {
    let f = Fleet::new();
    f.set_down("beta", true);
    agent(&f.alpha, "main", "codex", "blocked");
    let mut h = Harness::spawn(f.grofe(&["--machines"]), 100, 30);
    h.wait_for(common::PROMPT.trim());
    h.send("echo on-local\r");
    h.wait_for("on-local");

    // M-m opens the picker: alpha with its blocked agent, beta offline.
    h.send("\x1bm");
    h.wait_until("the picker", Duration::from_secs(20), |t| {
        t.contains("machines") && t.contains("codex blocked") && t.contains("offline")
    });
    // Escape closes it and the screen comes back.
    h.send("\x1b");
    h.wait_until("the picker gone", Duration::from_secs(10), |t| {
        !t.contains("codex blocked")
    });

    // M-2 in the picker attaches to alpha's `main`.
    h.send("\x1bm");
    h.wait_for("codex blocked");
    h.send("\x1b2");
    h.wait_until("alpha attached", Duration::from_secs(20), |_| {
        f.alpha
            .cmd(&["list-clients", "-F", "#{session_name}"])
            .is_ok_and(|o| o.trim() == "main")
    });
    h.send("echo on-alpha\r");
    h.wait_for("on-alpha");
    assert!(f.alpha.capture("main").contains("on-alpha"));
    // The local session keeps running, with no client left on it.
    assert!(f.local.cmd(&["has-session", "-t", "main"]).is_ok());
    h.wait_until("the local client gone", Duration::from_secs(10), |_| {
        f.local
            .cmd(&["list-clients"])
            .is_ok_and(|o| o.trim().is_empty())
    });

    // An offline machine cannot be chosen: the picker stays up.
    // Each key the picker keeps itself up for draws the box again, so a
    // new box after `M-3` shows the key was taken and the picker stayed.
    h.send("\x1bm");
    h.wait_for("offline");
    let from = h.output().len();
    h.send("\x1b3");
    h.wait_for_output(BOX.as_bytes(), from);
    h.send("\x1b");
    h.wait_until("the picker closed", Duration::from_secs(10), |t| {
        !t.contains(BOX)
    });
    assert!(
        f.alpha
            .cmd(&["list-clients", "-F", "#{session_name}"])
            .is_ok_and(|o| o.trim() == "main"),
        "still on alpha"
    );
    h.send("\x02d");
    h.wait_exit();
}

/// The picker follows its machines while it is up: a machine that comes
/// back shows as connected with no key pressed and no pane output.
#[test]
fn an_open_picker_redraws_when_a_machine_comes_back() {
    let f = Fleet::new();
    f.set_down("beta", true);
    // No status line refresh either: only the picker may draw.
    f.local
        .cmd(&["set-option", "-g", "status-interval", "0"])
        .unwrap();
    let mut h = Harness::spawn(f.grofe(&["--machines"]), 100, 30);
    h.wait_for(common::PROMPT.trim());
    h.send("\x1bm");
    h.wait_until("beta offline in the picker", Duration::from_secs(20), |t| {
        t.contains(BOX) && t.contains("offline")
    });
    f.set_down("beta", false);
    h.wait_until("beta back in the picker", Duration::from_secs(30), |t| {
        t.contains(BOX) && !t.contains("offline")
    });
    h.send("\x1b");
    h.wait_until("the picker closed", Duration::from_secs(10), |t| {
        !t.contains(BOX)
    });
    h.send("\x02d");
    h.wait_exit();
}
