//! Forwarding in the remote client (spec 2026-11-15-ssh-remote §3): a
//! `grofe -r box` client in a pty against a real server through a fake ssh
//! (as `tests/remote_client.rs`), so the remote and the local machine are
//! this one: pasted files upload, a pane's port reaches localhost, and a
//! pane's `grofe open` runs the local opener.

mod common;
#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{CMD_TIMEOUT, TestServer, run_bounded, wait_until};
use harness::Harness;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(10);
/// Long enough that a remote path never wraps on screen.
const COLS: u16 = 240;

/// The remote side: a server, a remote `HOME`, the fake ssh, and the local
/// client's private dirs.
struct Remote {
    srv: TestServer,
    dir: TempDir,
}

impl Remote {
    fn new() -> Remote {
        let srv = TestServer::start();
        let dir = tempfile::Builder::new()
            .prefix("grofe-fw")
            .tempdir()
            .expect("tempdir");
        let root = dir.path();
        for d in [
            "home",
            "bin",
            "local-home",
            "local-config",
            "local-run",
            "files",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_grofe"), root.join("bin/grofe")).unwrap();
        let script = format!(
            r#"#!/bin/sh
for a; do [ "$a" = -G ] && {{ echo "serveraliveinterval 0"; exit 0; }}; done
while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done
shift 2
export HOME='{root}/home' PATH='{root}/bin:/usr/bin:/bin' GROFE='{sock}'
export XDG_STATE_HOME='{state}' XDG_RUNTIME_DIR='{run}' XDG_CONFIG_HOME='{cfg}'
export SSH_AUTH_SOCK='{root}/agent.sock'
unset GROFE_PANE TMUX
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

    /// A local `grofe -r box <args>` with the fake ssh and private dirs.
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
            .env("XDG_RUNTIME_DIR", self.root().join("local-run"))
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        cmd
    }

    /// An attached client, its shell prompt up and the remote's options read.
    fn attach(&self, env: &[(&str, &str)]) -> Harness {
        let mut cmd = self.client(&[]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut h = Harness::spawn(cmd, COLS, 24);
        h.wait_for(common::PROMPT.trim());
        // ponytail: the options are read on the first tick after the attach;
        // there is no signal for it, so give it a moment.
        std::thread::sleep(Duration::from_millis(800));
        h
    }

    /// The directory the `main` pane's shell is in.
    fn pane_dir(&self) -> PathBuf {
        let out = self
            .srv
            .cmd(&[
                "display-message",
                "-p",
                "-t",
                "main",
                "#{pane_current_path}",
            ])
            .unwrap();
        PathBuf::from(out.trim())
    }

    /// A local file to paste.
    fn file(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root().join("files").join(name);
        fs::write(&path, body).unwrap();
        path
    }
}

fn pasted(path: &Path) -> String {
    format!("\x1b[200~{}\x1b[201~", path.display())
}

fn detach(mut h: Harness) {
    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn a_pasted_file_uploads_and_its_remote_path_is_typed_in_mode_on() {
    let r = Remote::new();
    r.srv
        .cmd(&["set-option", "-g", "remote-upload-paste", "on"])
        .unwrap();
    let local = r.file("notes.txt", "from the local side");
    let mut h = r.attach(&[]);
    h.send("echo ");
    h.send(pasted(&local));
    let remote = r.pane_dir().join("notes.txt");
    h.wait_for(&format!("echo '{}'", remote.display()));
    h.send("\r");
    h.wait_for(&format!("\n{}", remote.display()));
    assert_eq!(fs::read_to_string(&remote).unwrap(), "from the local side");
    detach(h);
}

#[test]
fn in_mode_ask_the_prompt_comes_first_and_y_uploads() {
    let r = Remote::new();
    let local = r.file("ask.txt", "12345");
    let mut h = r.attach(&[]);
    h.send("echo ");
    h.send(pasted(&local));
    h.wait_for("upload 1 file (5 B) to box:");
    h.wait_for("? [y/N]");
    let remote = r.pane_dir().join("ask.txt");
    assert!(!remote.exists(), "nothing before the answer");
    h.send("y\r");
    h.wait_for(&format!("echo '{}'", remote.display()));
    h.send("\r");
    h.wait_for(&format!("\n{}", remote.display()));
    detach(h);
}

/// Not a test of its own: the pane process of the ports test. It listens,
/// writes its port to `GROFE_T_PORTFILE`, and answers each connection with
/// `hi-from-remote` until `GROFE_T_STOPFILE` exists.
#[test]
#[ignore = "the pane process of the ports test"]
fn listener_child() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    fs::write(std::env::var("GROFE_T_PORTFILE").unwrap(), port.to_string()).unwrap();
    let stop = PathBuf::from(std::env::var("GROFE_T_STOPFILE").unwrap());
    listener.set_nonblocking(true).unwrap();
    for _ in 0..1200 {
        if stop.exists() {
            return;
        }
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.set_nonblocking(false);
            let _ = s.write_all(b"hi-from-remote");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_port_a_pane_listens_on_reaches_localhost_in_mode_auto() {
    let r = Remote::new();
    r.srv
        .cmd(&["set-option", "-g", "remote-forward-ports", "auto"])
        .unwrap();
    let none = run_bounded(r.client(&["ports"]), CMD_TIMEOUT);
    assert_eq!(none.stdout, "no ports forwarded from box\n");
    let h = r.attach(&[]);
    let port_file = r.root().join("port");
    let stop_file = r.root().join("stop");
    let command = format!(
        "GROFE_T_PORTFILE={} GROFE_T_STOPFILE={} {} --ignored --exact --quiet listener_child",
        port_file.display(),
        stop_file.display(),
        std::env::current_exe().unwrap().display()
    );
    r.srv
        .cmd(&["send-keys", "-t", "main", &command, "Enter"])
        .unwrap();
    wait_until("the listener's port", WAIT, || {
        fs::read_to_string(&port_file).is_ok_and(|p| p.trim().parse::<u16>().is_ok())
    });
    let port: u16 = fs::read_to_string(&port_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // `grofe -r box ports` lists it once the client forwards it.
    let mut listed = String::new();
    wait_until("the forward", WAIT, || {
        listed = run_bounded(r.client(&["ports"]), CMD_TIMEOUT).stdout;
        listed.starts_with(&format!("box:{port} -> localhost:"))
    });
    let local: u16 = listed
        .trim()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap();
    assert_ne!(local, port, "the remote's own port is taken on this host");
    let mut s = TcpStream::connect(("127.0.0.1", local)).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let mut got = String::new();
    s.read_to_string(&mut got).unwrap();
    assert_eq!(got, "hi-from-remote");
    fs::write(&stop_file, "").unwrap();
    detach(h);
}

#[test]
fn grofe_open_in_a_remote_pane_runs_the_local_opener() {
    let r = Remote::new();
    let out = r.root().join("opened");
    let opener = r.root().join("opener");
    fs::write(
        &opener,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", out.display()),
    )
    .unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o755)).unwrap();
    let h = r.attach(&[(grofe::open::OPENER_ENV, opener.to_str().unwrap())]);
    let command = format!(
        "{} open https://example.com/z?q=1",
        env!("CARGO_BIN_EXE_grofe")
    );
    r.srv
        .cmd(&["send-keys", "-t", "main", &command, "Enter"])
        .unwrap();
    wait_until("the local opener", WAIT, || out.exists());
    wait_until("the url", WAIT, || {
        fs::read_to_string(&out).is_ok_and(|u| u == "https://example.com/z?q=1")
    });
    detach(h);
}

#[test]
fn the_sessions_agent_link_follows_the_agent_ssh_gave_the_proxy() {
    let r = Remote::new();
    let h = r.attach(&[("SSH_AUTH_SOCK", "/local/agent/not/sent")]);
    let id = r
        .srv
        .cmd(&["display-message", "-p", "-t", "main", "#{session_id}"])
        .unwrap();
    let n: u32 = id.trim().trim_start_matches('$').parse().unwrap();
    let link = grofe::paths::agent_link(&r.srv.socket, n);
    assert_eq!(fs::read_link(&link).unwrap(), r.root().join("agent.sock"));
    detach(h);
}
