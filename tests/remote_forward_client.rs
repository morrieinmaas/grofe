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
case "$*" in *proxy*) echo $$ >> '{root}/proxies' ;; esac
export HOME='{root}/home' PATH='{root}/bin:/usr/bin:/bin' GROFE='{sock}'
export GROFE_NOTIFY_DESKTOP=off
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
        cmd.env("GROFE_NOTIFY_DESKTOP", "off");
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

    /// An attached client, its shell prompt up.
    fn attach(&self, env: &[(&str, &str)]) -> Harness {
        let mut cmd = self.client(&[]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut h = Harness::spawn(cmd, COLS, 24);
        h.wait_for(common::PROMPT.trim());
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

    /// This machine's config for the client.
    fn local_config(&self, toml: &str) {
        let dir = self.root().join("local-config/grofe");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), toml).unwrap();
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
    let local = r.file("notes.txt", "from the local side");
    r.local_config("remote-upload-paste = \"on\"\n");
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
    // The remote saying `on` grants it nothing: consent is this machine's.
    r.srv
        .cmd(&["set-option", "-g", "remote-upload-paste", "on"])
        .unwrap();
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

impl Remote {
    /// Starts [`listener_child`] in `session`'s pane; its port and the file
    /// that stops it.
    fn listener(&self, session: &str) -> (u16, PathBuf) {
        let port_file = self.root().join(format!("port-{session}"));
        let stop_file = self.root().join(format!("stop-{session}"));
        let command = format!(
            "GROFE_T_PORTFILE={} GROFE_T_STOPFILE={} {} --ignored --exact --quiet listener_child",
            port_file.display(),
            stop_file.display(),
            std::env::current_exe().unwrap().display()
        );
        self.srv
            .cmd(&["send-keys", "-t", session, &command, "Enter"])
            .unwrap();
        wait_until("the listener's port", WAIT, || {
            fs::read_to_string(&port_file).is_ok_and(|p| p.trim().parse::<u16>().is_ok())
        });
        let port = fs::read_to_string(&port_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        (port, stop_file)
    }
}

#[test]
fn a_port_a_pane_listens_on_reaches_localhost_in_mode_auto() {
    let r = Remote::new();
    r.local_config("remote-forward-ports = \"auto\"\n");
    let none = run_bounded(r.client(&["ports"]), CMD_TIMEOUT);
    assert_eq!(none.stdout, "no ports forwarded from box\n");
    // A port in a session this client is not attached to is not its own.
    r.srv.cmd(&["new-session", "-d", "-s", "other"]).unwrap();
    let (theirs, stop_theirs) = r.listener("other");
    let h = r.attach(&[]);
    let (port, stop_file) = r.listener("main");
    // `grofe -r box ports` lists it once the client forwards it.
    let mut listed = String::new();
    wait_until("the forward", WAIT, || {
        listed = run_bounded(r.client(&["ports"]), CMD_TIMEOUT).stdout;
        listed.starts_with(&format!("box:{port} -> localhost:"))
    });
    assert_eq!(
        listed.lines().count(),
        1,
        "only this session's port: {listed}"
    );
    assert!(!listed.contains(&format!("box:{theirs} ")), "{listed}");
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
    fs::write(&stop_theirs, "").unwrap();
    detach(h);
}

#[test]
fn grofe_open_in_a_remote_pane_runs_the_local_opener() {
    let r = Remote::new();
    let out = r.root().join("opened");
    let opener = r.root().join("opener");
    fs::write(
        &opener,
        format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", out.display()),
    )
    .unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o755)).unwrap();
    r.srv.cmd(&["new-session", "-d", "-s", "other"]).unwrap();
    let mut h = r.attach(&[(grofe::open::OPENER_ENV, opener.to_str().unwrap())]);
    // The remote's own opener, which `grofe open` falls back to when no
    // client opens the url.
    let theirs = r.root().join("opened-there");
    let remote_opener = r.root().join("remote-opener");
    fs::write(
        &remote_opener,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n",
            theirs.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&remote_opener, fs::Permissions::from_mode(0o755)).unwrap();
    let open = |session: &str, url: &str| {
        let command = format!(
            "GROFE_OPENER={} {} open '{url}'",
            remote_opener.display(),
            env!("CARGO_BIN_EXE_grofe")
        );
        r.srv
            .cmd(&["send-keys", "-t", session, &command, "Enter"])
            .unwrap();
    };
    // Another session's url is not this client's: the remote opens it
    // itself. This one's opens here.
    open("other", "https://example.com/theirs");
    wait_until("the remote's own opener", WAIT, || {
        fs::read_to_string(&theirs).is_ok_and(|u| u == "https://example.com/theirs\n")
    });
    open("main", "https://example.com/z?q=1");
    wait_until("the url", WAIT, || out.exists());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "https://example.com/z?q=1\n"
    );
    assert_eq!(
        fs::read_to_string(&theirs).unwrap(),
        "https://example.com/theirs\n",
        "the remote opened only the other session's url"
    );
    // A local file asks first.
    // The real path: the offer and the opener name the file a link (macOS
    // /tmp) leads to.
    let doc = fs::canonicalize(r.file("report.txt", "x")).unwrap();
    let url = format!("file://{}", doc.display());
    open("main", &url);
    h.wait_for(&format!("open {url}? [y/N]"));
    // The remote raised it: it takes a key only after the keyboard rests.
    std::thread::sleep(Duration::from_millis(700));
    h.send("y\r");
    wait_until("the file url", WAIT, || {
        fs::read_to_string(&out).is_ok_and(|u| u.ends_with(&format!("{url}\n")))
    });
    detach(h);
}

#[test]
fn the_sessions_agent_link_follows_the_agent_ssh_gave_the_proxy() {
    let r = Remote::new();
    // The agent ssh gave the proxy: a live socket.
    let _agent = std::os::unix::net::UnixListener::bind(r.root().join("agent.sock")).unwrap();
    let h = r.attach(&[("SSH_AUTH_SOCK", "/local/agent/not/sent")]);
    let id = r
        .srv
        .cmd(&["display-message", "-p", "-t", "main", "#{session_id}"])
        .unwrap();
    let n: u32 = id.trim().trim_start_matches('$').parse().unwrap();
    let link = grofe::paths::agent_link(&r.srv.socket, n);
    assert_eq!(fs::read_link(&link).unwrap(), r.root().join("agent.sock"));
    // The first pane, made over a control channel before the attach, has
    // the link, and through it the agent.
    let out = r.root().join("first-agent");
    let command = format!("printf %s \"$SSH_AUTH_SOCK\" > {}", out.display());
    r.srv
        .cmd(&["send-keys", "-t", "main", &command, "Enter"])
        .unwrap();
    wait_until("the pane's agent", WAIT, || {
        fs::read_to_string(&out).is_ok_and(|s| !s.is_empty())
    });
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        link.display().to_string()
    );
    std::os::unix::net::UnixStream::connect(&link).expect("the agent through the link");
    detach(h);
}

#[test]
fn a_port_offer_lets_typing_through_and_takes_a_key_after_a_pause() {
    let r = Remote::new();
    let mut h = r.attach(&[]);
    let (port, stop) = r.listener("main");
    h.wait_for(&format!(
        "forward box:{port} to localhost:{port}? [y/N/always]"
    ));
    // Typed at once, as mid-command: it reaches the pane, not the offer.
    h.send("y\r");
    std::thread::sleep(Duration::from_millis(300));
    let none = run_bounded(r.client(&["ports"]), CMD_TIMEOUT);
    assert_eq!(none.stdout, "no ports forwarded from box\n");
    // After a pause the offer takes the answer.
    std::thread::sleep(Duration::from_millis(700));
    h.send("y\r");
    wait_until("the forward", WAIT, || {
        run_bounded(r.client(&["ports"]), CMD_TIMEOUT)
            .stdout
            .starts_with(&format!("box:{port} -> localhost:"))
    });
    fs::write(&stop, "").unwrap();
    detach(h);
}

#[test]
fn a_file_dropped_while_a_port_offer_is_up_still_uploads() {
    let r = Remote::new();
    r.local_config("remote-upload-paste = \"on\"\n");
    let local = r.file("during-offer.txt", "dropped");
    let mut h = r.attach(&[]);
    // A listener in a window of its own, so the shell on screen is free.
    let port_file = r.root().join("port-offer");
    let command = format!(
        "GROFE_T_PORTFILE={} GROFE_T_STOPFILE={} {} --ignored --exact --quiet listener_child",
        port_file.display(),
        r.root().join("stop-offer").display(),
        std::env::current_exe().unwrap().display()
    );
    r.srv
        .cmd(&["new-window", "-d", "-t", "main", &command])
        .unwrap();
    wait_until("the listener's port", WAIT, || {
        fs::read_to_string(&port_file).is_ok_and(|p| p.trim().parse::<u16>().is_ok())
    });
    let port = fs::read_to_string(&port_file).unwrap().trim().to_string();
    h.wait_for(&format!("forward box:{port} to localhost:{port}?"));
    // At once, inside the offer's grace: the drop goes through forwarding.
    h.send("echo ");
    h.send(pasted(&local));
    let remote = r.pane_dir().join("during-offer.txt");
    h.wait_for(&format!("echo '{}'", remote.display()));
    assert_eq!(fs::read_to_string(&remote).unwrap(), "dropped");
    // The offer is still up (its port still listens), past its grace, as
    // on a slow runner: the detach key must still work.
    std::thread::sleep(Duration::from_millis(700));
    detach(h);
    fs::write(r.root().join("stop-offer"), "").unwrap();
}

#[test]
fn an_upload_prompt_lost_with_the_link_comes_back_and_typing_reaches_the_pane() {
    let r = Remote::new();
    let local = r.file("across-a-drop.txt", "x");
    let mut h = r.attach(&[]);
    h.send("echo ");
    h.send(pasted(&local));
    h.wait_for("upload 1 file (1 B) to box:");
    // The link drops with the prompt up; the client reconnects.
    let proxy = fs::read_to_string(r.root().join("proxies")).unwrap();
    let pid: i32 = proxy.lines().last().unwrap().trim().parse().unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    wait_until("the reconnect", WAIT, || {
        fs::read_to_string(r.root().join("proxies")).is_ok_and(|p| p.lines().count() >= 2)
    });
    // The question is still there (after the redraw), and answering it
    // frees the keyboard.
    std::thread::sleep(Duration::from_millis(800));
    h.wait_for("upload 1 file (1 B) to box:");
    h.send("n\r");
    std::thread::sleep(Duration::from_millis(300));
    h.send("\recho after-the-drop\r");
    h.wait_for("\nafter-the-drop");
    detach(h);
}
