//! What a scenario needs from `scripts/ssh-e2e.sh`'s environment.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tempfile::TempDir;

use crate::pty::Harness;

/// The bound on a scenario's waits for the remote: a container round trip
/// plus a reconnect backoff step or two.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct E2e {
    pub config: String,
    pub host: String,
    pub bare_host: String,
    pub release_url: String,
    /// `LOW-HIGH`: UDP ports published as themselves on 127.0.0.1.
    pub udp_ports: Option<String>,
    script: String,
}

impl E2e {
    /// The environment of a `scripts/ssh-e2e.sh` run, or None (skip).
    pub fn get() -> Option<E2e> {
        let config = std::env::var("GROFE_E2E_SSH_CONFIG").ok()?;
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        Some(E2e {
            config,
            host: var("GROFE_E2E_HOST"),
            bare_host: var("GROFE_E2E_BARE_HOST"),
            release_url: var("GROFE_E2E_RELEASE_URL"),
            udp_ports: std::env::var("GROFE_E2E_UDP_PORTS")
                .ok()
                .filter(|s| !s.is_empty()),
            script: var("GROFE_E2E_SCRIPT"),
        })
    }

    /// `ssh -F config <host> <command>` on the container with grofe.
    pub fn ssh(&self, command: &str) -> Output {
        self.ssh_to(&self.host, command)
    }

    pub fn ssh_to(&self, host: &str, command: &str) -> Output {
        Command::new("ssh")
            .args(["-F", &self.config, host, command])
            .output()
            .expect("run ssh")
    }

    /// The script's helpers: `e2e_exec`, `e2e_netem`, `e2e_kill_sshd_children`.
    pub fn helper(&self, helper: &str, args: &[&str]) -> Output {
        Command::new(&self.script)
            .arg(helper)
            .args(args)
            .output()
            .expect("run the e2e script")
    }

    /// A local client side: private dirs and an ssh that reads the run's
    /// config.
    pub fn local(&self) -> Local {
        Local::new(&self.config)
    }
}

pub fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The local machine of a scenario: `HOME` and the XDG dirs in a temp dir,
/// and `GROFE_SSH` naming a wrapper that runs `ssh -F <config>` (so `ssh -G`
/// reads the same config). The wrapper fails like ssh (255) while a `down`
/// file exists, so a scenario can hold a reconnect off.
///
/// Each `Local` has its own grofe server in the container: the wrapper
/// sends `GROFE=<socket>` (sshd accepts it), which the remote proxy and
/// CLI use as the server socket, so no scenario reaches or kills another's.
pub struct Local {
    dir: TempDir,
    /// The container-side server socket.
    pub sock: String,
}

impl Local {
    fn new(config: &str) -> Local {
        static N: AtomicUsize = AtomicUsize::new(0);
        let sock = format!(
            "/tmp/grofe-e2e-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let dir = tempfile::Builder::new()
            .prefix("grofe-e2e")
            .tempdir()
            .expect("tempdir");
        let root = dir.path();
        for d in ["home", "config", "state", "run"] {
            fs::create_dir_all(root.join(d)).expect("create a local dir");
        }
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700))
            .expect("chmod the runtime dir");
        let script = format!(
            "#!/bin/sh\n\
             [ -e '{root}/down' ] && {{ echo 'ssh: connect to host: Connection refused (e2e down)' >&2; exit 255; }}\n\
             exec ssh -F '{config}' -o SetEnv=GROFE={sock} \"$@\"\n",
            root = root.display(),
        );
        let ssh = root.join("ssh");
        fs::write(&ssh, script).expect("write the ssh wrapper");
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).expect("chmod the wrapper");
        Local { dir, sock }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A local `grofe <args>`: the worktree's build, private dirs, the
    /// wrapper ssh, no inherited grofe or tmux session.
    pub fn grofe(&self, args: &[&str]) -> Command {
        let root = self.root();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_grofe"));
        cmd.args(args)
            .current_dir(root)
            .env_remove("GROFE")
            .env_remove("GROFE_PANE")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("GROFE_REMOTE_RELEASE_URL")
            .env("GROFE_SSH", root.join("ssh"))
            .env("GROFE_NOTIFY_DESKTOP", "off")
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM");
        cmd
    }

    /// `grofe <args>` in an 80x24 pty.
    pub fn attach(&self, cmd: Command) -> Harness {
        Harness::spawn(cmd, 80, 24)
    }

    /// `command` for a container shell, run against this scenario's server.
    pub fn on(&self, command: &str) -> String {
        format!("export GROFE={}; {command}", self.sock)
    }

    /// Kills this scenario's server in the container when dropped, after
    /// `e2e_netem clear` when `netem`.
    pub fn server_cleanup<'a>(&self, e: &'a E2e, netem: bool) -> Cleanup<'a> {
        Cleanup {
            e,
            host: e.host.clone(),
            command: self.on("grofe kill-server 2>/dev/null; rm -f \"$GROFE\"; true"),
            netem,
        }
    }

    /// Waits for the pane's first shell prompt. If none comes, panics with
    /// what explains it: the client's screen and output, and in the
    /// container this scenario's socket, the grofe logs, the processes and
    /// sshd's log.
    pub fn wait_prompt(&self, e: &E2e, h: &mut Harness) {
        let deadline = std::time::Instant::now() + REMOTE_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if h.text().contains("$ ") {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let screen = h.text();
        let raw = h.output();
        let raw = String::from_utf8_lossy(&raw[raw.len().saturating_sub(2000)..]).into_owned();
        let there = e.ssh(&self.on("echo \"== GROFE=$GROFE\"; ls -la /tmp; \
             for f in ~/.local/state/grofe/*.log; do echo \"== $f\"; tail -n 40 \"$f\"; done; \
             echo '== ps'; ps -eo pid,ppid,etime,stat,args"));
        let logs = e.helper("e2e_logs", &["80"]);
        panic!(
            "no shell prompt within {REMOTE_TIMEOUT:?}\n== screen\n{screen}\n== client output (tail)\n{raw:?}\n\
             {}\n{}\n== container log\n{}",
            text(&there),
            String::from_utf8_lossy(&there.stderr),
            text(&logs),
        );
    }

    /// While down, the wrapper ssh refuses every connection.
    pub fn set_down(&self, down: bool) {
        let f: PathBuf = self.root().join("down");
        if down {
            fs::write(f, "").expect("write the down file");
        } else {
            let _ = fs::remove_file(f);
        }
    }
}

/// Runs `command` on `host` over ssh when dropped, pass or fail: a
/// scenario's cleanup in the container (kill the grofe server it started,
/// clear netem, remove an install).
pub struct Cleanup<'a> {
    pub e: &'a E2e,
    pub host: String,
    pub command: String,
    /// Also `e2e_netem clear` first.
    pub netem: bool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if self.netem {
            let _ = self.e.helper("e2e_netem", &["clear"]);
        }
        let _ = Command::new("ssh")
            .args(["-F", &self.e.config, "-o", "ConnectTimeout=10"])
            .arg(&self.host)
            .arg(&self.command)
            .output();
    }
}

/// Runs a local client one-shot, killed if it takes over [`REMOTE_TIMEOUT`].
pub fn run(mut cmd: Command) -> Output {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grofe");
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let deadline = std::time::Instant::now() + REMOTE_TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait for grofe") {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{cmd:?} did not finish within {REMOTE_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: out_t.join().unwrap(),
        stderr: err_t.join().unwrap(),
    }
}
