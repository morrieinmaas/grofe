//! The rig the M3.7 plugin checks (`tp_*.rs`) run a plugin in, unchanged:
//! grofe answering to `tmux`, with canopy's ported plugin fragment and
//! TP.3's loader, or the real tmux 3.7b their goldens are recorded from,
//! sourcing canopy's tmux layer and running the same loader.
//!
//! Each rig has its own home, XDG, `TMPDIR`, socket and `CANOPY_*`
//! directories, and its own canopy store: a copy of the loader, the
//! strategies and only the plugins under test, out of
//! `tests/fixtures/canopy-port/store`, which `check-canopy-port.sh` keeps
//! byte for byte equal to canopy's. The plugins canopy loads that the copy
//! leaves out stay dormant.
//!
//! continuum counts every process of this user whose command starts with
//! `tmux`, and arms nothing if it finds another server. A `ps` first on the
//! rig's PATH shows that one listing only this server's own process tree,
//! as on a machine running no other tmux.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::run_bounded;
pub use common::wait_until;

use crate::harness::Harness;

pub const WAIT: Duration = Duration::from_secs(20);
/// How long one command may take. A `run-shell` of a plugin's script makes
/// a few hundred calls, each a fresh debug-build client, and has taken over
/// 10 s on a loaded machine; a hang still fails, only later.
const CMD: Duration = Duration::from_secs(30);
pub const PREFIX: &str = "\x02";
pub const COLS: u16 = 120;
pub const ROWS: u16 = 41;

/// canopy's plugin fragment, the copy `check-canopy-port.sh` keeps current.
const FRAGMENT: &str = include_str!("../fixtures/canopy-port/conf.d/30-canopy-plugins.toml");

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A golden recorded from tmux 3.7b, `golden/<task>/<name>.golden`.
pub fn golden(task: &str, name: &str) -> PathBuf {
    fixtures().join(format!("compat/golden/{task}/{name}.golden"))
}

pub fn read_golden(task: &str, name: &str) -> String {
    fs::read_to_string(golden(task, name))
        .unwrap_or_else(|e| panic!("{task}/{name}: no golden ({e}); record it"))
}

/// Writes a golden; for the ignored recorders.
pub fn write_golden(task: &str, name: &str, text: &str) {
    let path = golden(task, name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// The real tmux 3.7b a recorder runs: the absolute path in `var`.
pub fn recording_tmux(var: &str) -> PathBuf {
    let tmux = PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var} is unset")));
    assert!(tmux.is_absolute(), "{var} must be an absolute path");
    let version = Command::new(&tmux).arg("-V").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "tmux 3.7b");
    tmux
}

/// Whether a case whose plugin writes a fixed shared `/tmp` path may run:
/// only where `/tmp` is the test's own, as `GROFE_TEST_PRIVATE_TMP=1` (a
/// container) says. Elsewhere it skips, saying why; under `CI` a missing
/// flag fails, so CI cannot skip it quietly.
pub fn private_tmp(path: &str) -> bool {
    if std::env::var_os("GROFE_TEST_PRIVATE_TMP").is_some_and(|v| v == "1") {
        return true;
    }
    let why =
        format!("the plugin writes {path}; set GROFE_TEST_PRIVATE_TMP=1 where /tmp is private");
    if std::env::var_os("CI").is_some() {
        panic!("{why} (required under CI)");
    }
    eprintln!("SKIPPING (not run): {why}");
    false
}

/// The ps every plugin call finds first: the `-u <uid> -o "command pid"`
/// listing continuum counts servers with, cut to this server's tree.
const PS_SHIM: &str = r#"#!/bin/sh
real=@REAL@
if [ "$#" -eq 4 ] && [ "$1 $3 $4" = "-u -o command pid" ] && [ -n "${TMUX:-}" ]; then
  root=$(printf '%s\n' "$TMUX" | cut -d, -f2)
  tree=$("$real" -A -o pid=,ppid= | awk -v r="$root" '{ p[$1] = $2 }
    END { for (k in p) { x = k; while (x != r && (x in p) && x != p[x]) x = p[x]; if (x == r) print k } }')
  "$real" "$@" | awk -v t="$tree" 'BEGIN { n = split(t, a, "\n"); for (i = 1; i <= n; i++) keep[a[i]] = 1 }
    NR == 1 || ($NF in keep)'
  exit 0
fi
exec "$real" "$@"
"#;

/// The first `name` on this process's PATH.
/// A version manager's shim is skipped: it needs its own environment.
pub fn host_tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|d| !d.ends_with("shims"))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// One run's directories and the binary answering to `tmux`.
pub struct Rig {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    real_tmux: bool,
    socket: String,
    plugins: Vec<String>,
    options: Vec<(String, String)>,
    /// What a plugin reads from the server's environment, set with
    /// [`Rig::server_env`].
    extra_env: Vec<(String, String)>,
}

impl Rig {
    /// A rig on socket `grofe-<task>` loading `plugins` from its own store,
    /// with `options` (`@name`, value, where `<root>` is the rig's root)
    /// set before they load. `real` is the real tmux to run instead of
    /// grofe.
    pub fn new(
        task: &str,
        real: Option<&Path>,
        plugins: &[&str],
        options: Vec<(String, String)>,
    ) -> Rig {
        let dir = tempfile::Builder::new().prefix(task).tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for d in [
            "bin",
            "home",
            "tmp",
            "config/grofe/conf.d",
            "state",
            "cstate",
            "cconf",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::create_dir(root.join("run")).unwrap();
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = real.map_or_else(
            || PathBuf::from(env!("CARGO_BIN_EXE_grofe")),
            Path::to_path_buf,
        );
        symlink(tmux, root.join("bin/tmux")).unwrap();
        // The plugins' interpreter and picker, as on the machine running
        // the tests: bash 3.2 at /bin/bash is not what a plugin user has.
        for tool in ["bash", "fzf"] {
            if let Some(path) = host_tool(tool) {
                symlink(path, root.join("bin").join(tool)).unwrap();
            }
        }
        let ps = ["/bin/ps", "/usr/bin/ps"]
            .into_iter()
            .find(|p| Path::new(p).exists());
        let rig = Rig {
            _dir: dir,
            real_tmux: real.is_some(),
            socket: format!("grofe-{task}"),
            plugins: plugins.iter().map(|p| p.to_string()).collect(),
            options: options
                .into_iter()
                .map(|(k, v)| (k, v.replace("<root>", &root.display().to_string())))
                .collect(),
            root,
            extra_env: Vec::new(),
        };
        // A machine with no ps (a slim container) runs no other tmux either.
        if let Some(ps) = ps {
            rig.install("ps", &PS_SHIM.replace("@REAL@", ps));
        }

        let fixture = fixtures().join("canopy-port/store");
        let store = rig.store();
        copy_tree(&fixture.join("lib"), &store.join("lib"));
        copy_tree(
            &fixture.join("plugins/strategies"),
            &store.join("plugins/strategies"),
        );
        fs::copy(
            fixture.join("plugins/VERSIONS"),
            store.join("plugins/VERSIONS"),
        )
        .unwrap();
        for p in plugins {
            copy_tree(
                &fixture.join("plugins").join(p),
                &store.join("plugins").join(p),
            );
        }

        let conf = rig.path("config/grofe");
        fs::write(
            conf.join("config.toml"),
            format!(
                "[environment]\nCANOPY_STORE = \"{}\"\nCANOPY_STATE = \"{}\"\nCANOPY_CONFIG = \"{}\"\n",
                store.display(),
                rig.path("cstate").display(),
                rig.path("cconf").display()
            ),
        )
        .unwrap();
        fs::write(conf.join("conf.d/30-canopy-plugins.toml"), FRAGMENT).unwrap();
        let mut user = String::from("[user-options]\n");
        for (k, v) in &rig.options {
            user.push_str(&format!("{} = {v:?}\n", &k[1..]));
        }
        fs::write(conf.join("user.toml"), user).unwrap();
        // continuum's own switch for "do not restore at start".
        fs::write(rig.path("home/tmux_no_auto_restore"), "").unwrap();
        rig
    }

    /// Adds `env` to every command's environment, so the server a command
    /// starts holds it in its global environment, on grofe and on tmux.
    pub fn server_env(&mut self, env: &[(&str, String)]) {
        self.extra_env
            .extend(env.iter().map(|(k, v)| (k.to_string(), v.clone())));
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn store(&self) -> PathBuf {
        self.path("store")
    }

    /// An executable `name` first on the rig's PATH.
    pub fn install(&self, name: &str, script: &str) {
        let path = self.path("bin").join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn env(&self) -> Vec<(String, String)> {
        let p = |rel: &str| self.path(rel).display().to_string();
        vec![
            ("HOME".into(), p("home")),
            ("XDG_CONFIG_HOME".into(), p("config")),
            ("XDG_STATE_HOME".into(), p("state")),
            ("XDG_RUNTIME_DIR".into(), p("run")),
            ("TMUX_TMPDIR".into(), p("run")),
            ("TMPDIR".into(), p("tmp")),
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", p("bin")),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            ("ENV".into(), String::new()),
            ("PS1".into(), "$ ".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("LANG".into(), "en_US.UTF-8".into()),
            ("GROFE_RESTORE_ON_START".into(), "0".into()),
            ("OPENSESSIONS_SKIP_BINARY_DOWNLOAD".into(), "1".into()),
            ("CANOPY_STORE".into(), self.store().display().to_string()),
            ("CANOPY_STATE".into(), p("cstate")),
            ("CANOPY_CONFIG".into(), p("cconf")),
        ]
        .into_iter()
        .chain(self.extra_env.iter().cloned())
        .collect()
    }

    fn base(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .envs(self.env())
            .current_dir(self.path("home"));
        cmd
    }

    /// `tmux -L <socket> [-f /dev/null] <args>` in this rig's environment
    /// and nothing else.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.base(&self.path("bin/tmux"));
        cmd.args(["-L", &self.socket]);
        if self.real_tmux {
            cmd.args(["-f", "/dev/null"]);
        }
        cmd.args(args);
        cmd
    }

    pub fn try_t(&self, args: &[&str]) -> Result<String, String> {
        let out = run_bounded(self.command(args), CMD);
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(out.stderr)
        }
    }

    pub fn t(&self, args: &[&str]) -> String {
        self.try_t(args)
            .unwrap_or_else(|e| panic!("tmux {args:?}: {e}"))
    }

    /// A fresh server with session `name`, the plugins loaded: grofe reads
    /// the ported config itself; tmux gets what canopy's tmux.conf does.
    pub fn start(&self, name: &str) {
        let home = self.path("home").display().to_string();
        self.t(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "120",
            "-y",
            "40",
            "-c",
            &home,
        ]);
        if self.real_tmux {
            for (k, v) in self
                .env()
                .into_iter()
                .filter(|(k, _)| k.starts_with("CANOPY_"))
            {
                self.t(&["set-environment", "-g", &k, &v]);
            }
            for (k, v) in &self.options {
                self.t(&["set-option", "-g", k, v]);
            }
            let conf = fixtures().join("canopy-port/30-plugins.conf");
            self.t(&["source-file", &conf.display().to_string()]);
            let load = format!(
                ". \"$CANOPY_STORE/lib/env.sh\"; . \"$CANOPY_STORE/lib/plugins.sh\"; \
                 canopy_load_plugins {}",
                self.plugins.join(" ")
            );
            self.t(&["run-shell", &load]);
            // What tmux.conf sets after the loader, as the grofe fragment does.
            if self.plugins.iter().any(|p| p == "tmux-resurrect") {
                let wrapper = self.store().join("plugins/strategies/canopy_save.sh");
                let wrapper = wrapper.display().to_string();
                self.t(&["set-option", "-g", "@resurrect-save-script-path", &wrapper]);
                self.t(&["bind-key", "M-s", "run-shell", &wrapper]);
            }
        }
        let loaded = wait_until(|| self.plugins.iter().all(|p| self.loaded(p)), WAIT);
        assert!(
            loaded.is_ok(),
            "{:?} never loaded; plugins.log:\n{}\nlogs:\n{}",
            self.plugins,
            fs::read_to_string(self.path("cstate/plugins.log")).unwrap_or_default(),
            self.logs()
        );
    }

    /// Whether plugin `name` has finished loading: continuum's last step is
    /// its status-right autosave; every other plugin binds a key to a
    /// script of its own last, except tmux-yank finding no clipboard
    /// command, which binds only its error message.
    fn loaded(&self, name: &str) -> bool {
        let mark = format!("/plugins/{name}/");
        if name == "tmux-continuum" {
            return self
                .try_t(&["show-options", "-gv", "status-right"])
                .is_ok_and(|s| s.contains(&mark));
        }
        self.try_t(&["list-keys"]).is_ok_and(|k| {
            k.contains(&mark)
                || name == "tmux-yank" && k.contains("tmux-yank dependencies not installed")
        })
    }

    /// A client on `session`, attached through a pty.
    pub fn attach(&self, session: &str) -> Harness {
        let mut cmd = Command::new("/usr/bin/env");
        cmd.arg("-i");
        for (k, v) in self.env() {
            cmd.arg(format!("{k}={v}"));
        }
        cmd.arg(self.path("bin/tmux")).args(["-L", &self.socket]);
        if self.real_tmux {
            cmd.args(["-f", "/dev/null"]);
        }
        cmd.args(["attach", "-t", session]);
        let a = Harness::spawn(cmd, COLS, ROWS);
        let attached = wait_until(
            || {
                self.try_t(&["list-clients", "-F", "#{client_session}"])
                    .is_ok_and(|c| c.lines().any(|l| l == session))
            },
            WAIT,
        );
        assert!(attached.is_ok(), "the client never attached to {session}");
        a
    }

    pub fn kill_server(&self) {
        let _ = self.try_t(&["kill-server"]);
        let gone = wait_until(|| self.try_t(&["list-sessions"]).is_err(), WAIT);
        assert!(gone.is_ok(), "the server outlived kill-server");
    }

    /// grofe's server and compat-miss logs, for a failure message.
    pub fn logs(&self) -> String {
        let mut out = String::new();
        for e in fs::read_dir(self.path("state/grofe"))
            .into_iter()
            .flatten()
            .flatten()
        {
            out.push_str(&fs::read_to_string(e.path()).unwrap_or_default());
        }
        out
    }

    pub fn lines(&self, name: &str) -> usize {
        fs::read_to_string(self.path(name)).map_or(0, |s| s.lines().count())
    }

    pub fn wait_lines(&self, name: &str, n: usize, what: &str) {
        let got = wait_until(|| self.lines(name) >= n, WAIT);
        assert!(got.is_ok(), "{what}: {name} never reached {n} lines");
    }

    /// Sessions, windows and panes as `list-sessions`, `list-windows -a`
    /// and `list-panes -a` report them, the rig's root replaced.
    pub fn structure(&self) -> String {
        let sessions = self.t(&[
            "list-sessions",
            "-F",
            "#{session_name} windows=#{session_windows}",
        ]);
        let windows = self.t(&[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index} #{window_name} active=#{window_active} #{window_layout}",
        ]);
        let panes = self.t(&[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} active=#{pane_active} #{pane_current_path}",
        ]);
        // A window named after its shell is `<shell>`: /bin/sh is bash on
        // macOS and dash on Debian, on tmux and grofe alike.
        let windows: String = windows
            .lines()
            .map(|l| match l.split_once(' ') {
                Some((at, rest)) if rest.starts_with("bash ") || rest.starts_with("sh ") => {
                    let (_, tail) = rest.split_once(' ').unwrap_or_default();
                    format!("{at} <shell> {tail}\n")
                }
                _ => format!("{l}\n"),
            })
            .collect();
        format!("{sessions}{windows}{panes}").replace(&self.root.display().to_string(), "<root>")
    }

    /// `structure` without one session's lines.
    pub fn without(structure: &str, session: &str) -> String {
        structure
            .lines()
            .filter(|l| {
                !l.starts_with(&format!("{session}:")) && !l.starts_with(&format!("{session} "))
            })
            .map(|l| format!("{l}\n"))
            .collect()
    }
}

/// A case that fails part way still stops its server.
impl Drop for Rig {
    fn drop(&mut self) {
        // Bounded: a server that stops answering must not hang the test.
        let kill = self
            .command(&["kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(kill) = kill {
            let _ = common::wait_child(kill, CMD);
        }
    }
}

/// `structure` with each layout's checksum and pane ids left out: a
/// restore creates new panes, so only the geometry can come back.
pub fn geometry(structure: &str) -> String {
    let mut out = String::new();
    for line in structure.lines() {
        let words: Vec<String> = line
            .split(' ')
            .map(|w| match w.split_once(',') {
                Some((sum, rest)) if sum.len() == 4 && rest.contains('x') => layout_geometry(rest),
                _ => w.to_string(),
            })
            .collect();
        out.push_str(&words.join(" "));
        out.push('\n');
    }
    out
}

/// A layout body (`WxH,X,Y,ID` leaves, `{...}` and `[...]` cells) without
/// the leaves' pane ids: of each cell only its first three fields.
fn layout_geometry(body: &str) -> String {
    fn emit(tok: &mut String, out: &mut String, n: &mut usize) {
        if tok.is_empty() {
            return;
        }
        if tok.contains('x') {
            *n = 0;
        }
        if *n < 3 {
            if !out.is_empty() && !out.ends_with(['{', '[']) {
                out.push(',');
            }
            out.push_str(tok);
        }
        *n += 1;
        tok.clear();
    }
    let (mut out, mut tok, mut n) = (String::new(), String::new(), 0);
    for c in body.chars() {
        match c {
            ',' => emit(&mut tok, &mut out, &mut n),
            '{' | '}' | '[' | ']' => {
                emit(&mut tok, &mut out, &mut n);
                out.push(c);
            }
            _ => tok.push(c),
        }
    }
    emit(&mut tok, &mut out, &mut n);
    out
}

#[test]
fn layout_geometry_drops_checksum_and_pane_ids() {
    let line =
        "main:1 logs active=1 d6e5,120x40,0,0{60x40,0,0,1,59x40,61,0[59x20,61,0,2,59x19,61,21,3]}";
    assert_eq!(
        geometry(line),
        "main:1 logs active=1 120x40,0,0{60x40,0,0,59x40,61,0[59x20,61,0,59x19,61,21]}\n"
    );
}
