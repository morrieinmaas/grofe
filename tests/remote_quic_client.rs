//! The roaming transport from the client (spec ssh-remote §4): `grofe -r
//! box --transport quic` with a fake ssh that runs the remote command
//! locally (as in `remote_client.rs`). The fake ssh answers `ssh -G` with
//! `hostname 127.0.0.1`, so the client dials loopback the way it would dial
//! the host behind an ssh alias, and logs each `proxy --quic` it runs.
//! While a `silent` file exists it rewrites the bootstrap line's port to the
//! one in that file, where a socket that never answers is bound: UDP
//! blocked. Every detached proxy is killed by its pid file at the end.

mod common;
#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::fs;
use std::net::UdpSocket;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, run_bounded};
use harness::Harness;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tempfile::TempDir;

const BAR: &str = "reconnecting to box";

struct Remote {
    srv: TestServer,
    dir: TempDir,
}

impl Remote {
    fn new() -> Remote {
        let srv = TestServer::start();
        let dir = tempfile::Builder::new()
            .prefix("grofe-rq")
            .tempdir()
            .expect("tempdir");
        let root = dir.path();
        for d in ["home", "bin", "local-home", "local-config/grofe"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), root.join("bin/grofe")).unwrap();
        let script = format!(
            r#"#!/bin/sh
for a; do [ "$a" = -G ] && {{ printf 'hostname 127.0.0.1\nserveraliveinterval 0\n'; exit 0; }}; done
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
shift 2
case "$*" in *"proxy --quic"*) echo quic >> '{root}/bootstraps' ;; esac
export HOME='{root}/home' PATH='{root}/bin:/usr/bin:/bin' GROFE='{sock}'
export XDG_STATE_HOME='{state}' XDG_RUNTIME_DIR='{run}' XDG_CONFIG_HOME='{cfg}'
export GROFE_REMOTE_UDP_PORTS="$(cat '{root}/udp-ports' 2>/dev/null || echo 47100-47999)"
unset GROFE_PANE TMUX
case "$*" in *"proxy --quic"*) [ -e '{root}/silent' ] && {{
  /bin/sh -c "$*" | /usr/bin/sed "s/\"port\":[0-9]*/\"port\":$(cat '{root}/silent')/"
  exit 0; }} ;; esac
# `exec` inside too: dash (Linux /bin/sh) forks for a -c command's last
# step where bash execs, and the test must signal the proxy itself.
exec /bin/sh -c "exec $*"
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

    fn client(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.args(["-r", "box"])
            .args(args)
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

    fn bootstraps(&self) -> usize {
        fs::read_to_string(self.root().join("bootstraps"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// The detached proxies' pids, from their pid files.
    fn proxies(&self) -> Vec<Pid> {
        let log = self.srv.log_path();
        let stem = log.file_stem().unwrap().to_string_lossy().into_owned();
        let Ok(entries) = fs::read_dir(log.parent().unwrap()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(&format!("{stem}-quic-")) && n.ends_with(".pid")
            })
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|s| s.trim().parse().ok())
            .map(Pid::from_raw)
            .collect()
    }

    /// Authenticated QUIC clients the proxies logged.
    fn quic_clients(&self) -> usize {
        let log = self.srv.log_path();
        let stem = log.file_stem().unwrap().to_string_lossy().into_owned();
        fs::read_to_string(log.with_file_name(format!("{stem}-quic.log")))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(": client ") && !l.contains("client gone"))
            .count()
    }
}

impl Remote {
    /// SIGTERM ends every detached proxy, also one a reconnect started
    /// from the attach client (whose threads block SIGTERM).
    fn assert_proxies_stop(&self) {
        let pids = self.proxies();
        assert!(!pids.is_empty());
        for pid in pids {
            kill(pid, Signal::SIGTERM).unwrap();
            common::wait_until("the proxy to stop on SIGTERM", CMD_TIMEOUT, || {
                common::process_gone(pid)
            });
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        for pid in self.proxies() {
            let _ = kill(pid, Signal::SIGTERM);
        }
    }
}

fn quic_attach(r: &Remote, env: &[(&str, &str)]) -> Harness {
    let mut c = r.client(&["--transport", "quic"]);
    for (k, v) in env {
        c.env(k, v);
    }
    Harness::spawn(c, 80, 24)
}

#[test]
fn a_one_shot_under_quic_uses_ssh_and_leaves_a_quic_attach_alone() {
    let r = Remote::new();
    let mut h = quic_attach(&r, &[]);
    h.wait_for(common::PROMPT.trim());
    assert_eq!(r.bootstraps(), 1);
    let proxy = r.proxies();
    assert_eq!(proxy.len(), 1);
    assert_eq!(r.quic_clients(), 1);

    let out = run_bounded(
        r.client(&["--transport", "quic", "list-sessions"]),
        CMD_TIMEOUT,
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, r.srv.run(&["list-sessions"]).stdout);
    assert_eq!(r.bootstraps(), 1, "the one-shot started a QUIC proxy");
    assert_eq!(r.proxies(), proxy);
    assert!(
        !common::process_gone(proxy[0]),
        "the attach's proxy stopped"
    );

    h.send("echo still-here\r");
    h.wait_for("still-here");
    h.send("\x02d");
    h.wait_exit();
    r.assert_proxies_stop();
}

#[test]
fn silent_udp_falls_back_to_ssh_and_says_so() {
    let r = Remote::new();
    let quiet = UdpSocket::bind("127.0.0.1:0").unwrap();
    fs::write(
        r.root().join("silent"),
        quiet.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    let mut h = quic_attach(&r, &[]);
    h.wait_for_timeout(common::PROMPT.trim(), Duration::from_secs(40));
    let said = String::from_utf8_lossy(h.output()).into_owned();
    assert!(
        said.contains("over UDP") && said.contains("using ssh instead"),
        "{said}"
    );
    h.send("echo over-ssh\r");
    h.wait_for("over-ssh");
    assert_eq!(r.quic_clients(), 0);
    h.send("\x02d");
    h.wait_exit();
    r.assert_proxies_stop();
}

#[test]
fn a_dropped_link_comes_back_over_quic_without_a_new_bootstrap() {
    let r = Remote::new();
    fs::write(
        r.root().join("local-config/grofe/config.toml"),
        "remote-transport = \"quic\"\n",
    )
    .unwrap();
    let mut h = Harness::spawn(
        {
            let mut c = r.client(&[]);
            c.env("GROFE_TEST_PING_MS", "200");
            c
        },
        80,
        24,
    );
    h.wait_for(common::PROMPT.trim());
    h.send("echo before-roam\r");
    h.wait_for("before-roam");
    let proxy = r.proxies()[0];
    // The path goes quiet: pings go unanswered and the link drops.
    kill(proxy, Signal::SIGSTOP).unwrap();
    h.wait_for(BAR);
    kill(proxy, Signal::SIGCONT).unwrap();
    h.wait_until(
        "the bar gone after the reconnect",
        Duration::from_secs(30),
        |t| !t.contains(BAR),
    );
    h.send("echo after-roam\r");
    h.wait_for("after-roam");
    assert_eq!(r.bootstraps(), 1, "a reconnect bootstrapped again");
    assert_eq!(r.proxies(), vec![proxy]);
    assert!(r.quic_clients() >= 2, "no second QUIC connection");
    h.send("\x02d");
    h.wait_exit();
    r.assert_proxies_stop();
}

#[test]
fn a_gone_proxy_is_bootstrapped_again_over_ssh() {
    let r = Remote::new();
    let mut h = Harness::spawn(
        {
            let mut c = r.client(&["--transport", "quic"]);
            c.env("GROFE_TEST_PING_MS", "200");
            c
        },
        80,
        24,
    );
    h.wait_for(common::PROMPT.trim());
    let first = r.proxies()[0];
    kill(first, Signal::SIGKILL).unwrap();
    h.wait_for(BAR);
    h.wait_until(
        "the bar gone after the reconnect",
        Duration::from_secs(40),
        |t| !t.contains(BAR),
    );
    h.send("echo new-proxy\r");
    h.wait_for("new-proxy");
    assert_eq!(r.bootstraps(), 2);
    let now = r.proxies();
    assert_eq!(now.len(), 1, "{now:?}");
    assert_ne!(now[0], first);
    h.send("\x02d");
    h.wait_exit();
    r.assert_proxies_stop();
}

#[test]
fn a_failing_proxy_quic_falls_back_to_ssh_and_says_why() {
    let r = Remote::new();
    // A grofe that knows `proxy --quic` but cannot run it there.
    fs::write(r.root().join("udp-ports"), "0-0").unwrap();
    let mut h = quic_attach(&r, &[]);
    h.wait_for(common::PROMPT.trim());
    let said = String::from_utf8_lossy(h.output()).into_owned();
    assert!(
        said.contains("grofe proxy --quic failed there: bad port range")
            && said.contains("using ssh instead"),
        "{said}"
    );
    assert!(!said.contains("no usable proxy"), "{said}");
    assert!(
        !r.root().join("home/.local/share/grofe").exists(),
        "it installed"
    );
    h.send("echo over-ssh\r");
    h.wait_for("over-ssh");
    assert_eq!(r.bootstraps(), 1);
    assert!(r.proxies().is_empty());
    h.send("\x02d");
    h.wait_exit();
}
