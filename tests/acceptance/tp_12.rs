//! TP.12: tmux-resurrect and tmux-continuum, unchanged, on grofe run as
//! `tmux` with canopy's ported plugin fragment and TP.3's loader.
//!
//! The canopy store is `tests/fixtures/canopy-port/store`, the loader, the
//! save and restore strategies and the two vendored plugins, which
//! `check-canopy-port.sh` keeps byte for byte equal to canopy's. Every run
//! has its own home, XDG, socket and `CANOPY_*` directories, so resurrect
//! saves under the test's `$CANOPY_STATE/resurrect`.
//!
//! - Case 1: prefix M-s saves; after `kill-server`, a fresh server and
//!   prefix M-r, the sessions, windows, cwds, layouts, pane contents and
//!   the active window and pane are back.
//! - Case 2: continuum's timer saves, and a second save starts only after
//!   the first has ended.
//! - Case 3: with `@continuum-restore on`, a fresh server restores the last
//!   save once.
//! - Cases 1 and 3 match goldens recorded from tmux 3.7b by the same code
//!   (`record_tp12_goldens`, ignored: `GROFE_TP12_TMUX=<absolute path to
//!   tmux 3.7b> cargo test --test acceptance record_tp12 -- --ignored`).
//!
//! continuum counts every process of this user whose command starts with
//! `tmux`, and arms nothing if it finds another server. A `ps` first on the
//! rig's PATH shows it only this server's own process tree, as on a machine
//! running no other tmux.

#[allow(clippy::duplicate_mod)]
#[path = "../common/mod.rs"]
mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{run_bounded, wait_until};

use crate::harness::Harness;

const SOCKET: &str = "grofe-tp12";
const WAIT: Duration = Duration::from_secs(20);
const CMD: Duration = Duration::from_secs(10);
const PREFIX: &str = "\x02";
const COLS: u16 = 120;
const ROWS: u16 = 41;

/// canopy's plugin fragment, the copy `check-canopy-port.sh` keeps current.
const FRAGMENT: &str = include_str!("../fixtures/canopy-port/conf.d/30-canopy-plugins.toml");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
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

/// Wraps resurrect's save script for case 2, logging each save's start and
/// end with its pid. continuum runs the script path as one word.
const SAVE_PROBE: &str = r#"#!/bin/sh
echo "start $$" >>'@LOG@'
'@SAVE@' "$@"
echo "end $$" >>'@LOG@'
"#;

/// One run's directories and the binary answering to `tmux`: grofe, or the
/// real tmux 3.7b the goldens are recorded from.
struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    real_tmux: bool,
}

impl Rig {
    fn new(real: Option<&Path>) -> Rig {
        let dir = tempfile::Builder::new().prefix("tp12").tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for d in [
            "bin",
            "home/w1",
            "home/w2",
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
        let ps = ["/bin/ps", "/usr/bin/ps"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .expect("a ps");
        let shim = root.join("bin/ps");
        fs::write(&shim, PS_SHIM.replace("@REAL@", ps)).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let probe = root.join("save-probe");
        let save = fixtures().join("canopy-port/store/plugins/tmux-resurrect/scripts/save.sh");
        let probe_text = SAVE_PROBE
            .replace("@LOG@", &root.join("probe.log").display().to_string())
            .replace("@SAVE@", &save.display().to_string());
        fs::write(&probe, probe_text).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        let rig = Rig {
            _dir: dir,
            root,
            real_tmux: real.is_some(),
        };
        let conf = rig.root.join("config/grofe");
        fs::write(
            conf.join("config.toml"),
            format!(
                "[environment]\nCANOPY_STORE = \"{}\"\nCANOPY_STATE = \"{}\"\nCANOPY_CONFIG = \"{}\"\n",
                rig.store().display(),
                rig.path("cstate").display(),
                rig.path("cconf").display()
            ),
        )
        .unwrap();
        fs::write(conf.join("conf.d/30-canopy-plugins.toml"), FRAGMENT).unwrap();
        let mut user = String::from("[user-options]\n");
        for (k, v) in rig.options() {
            user.push_str(&format!("{} = {v:?}\n", &k[1..]));
        }
        fs::write(conf.join("user.toml"), user).unwrap();
        // continuum's own switch for "do not restore at start", lifted by
        // case 3.
        fs::write(rig.path("home/tmux_no_auto_restore"), "").unwrap();
        rig
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn store(&self) -> PathBuf {
        fixtures().join("canopy-port/store")
    }

    /// What the test sets beyond canopy's config: pane contents saved, and
    /// a line logged after each save and each restore.
    fn options(&self) -> Vec<(&'static str, String)> {
        let log = |name: &str| format!("echo done >> '{}'", self.path(name).display());
        vec![
            ("@resurrect-capture-pane-contents", "on".to_string()),
            ("@resurrect-hook-post-save-all", log("saves.log")),
            ("@resurrect-hook-post-restore-all", log("restores.log")),
        ]
    }

    fn env(&self) -> Vec<(String, String)> {
        let p = |rel: &str| self.path(rel).display().to_string();
        vec![
            ("HOME".into(), p("home")),
            ("XDG_CONFIG_HOME".into(), p("config")),
            ("XDG_STATE_HOME".into(), p("state")),
            ("XDG_RUNTIME_DIR".into(), p("run")),
            ("TMUX_TMPDIR".into(), p("run")),
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", p("bin")),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            ("ENV".into(), String::new()),
            ("PS1".into(), "$ ".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("GROFE_RESTORE_ON_START".into(), "0".into()),
            ("CANOPY_STORE".into(), self.store().display().to_string()),
            ("CANOPY_STATE".into(), p("cstate")),
            ("CANOPY_CONFIG".into(), p("cconf")),
        ]
    }

    /// `tmux -L grofe-tp12 [-f /dev/null] <args>` in this rig's environment
    /// and nothing else.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(self.path("bin/tmux"));
        cmd.env_clear()
            .envs(self.env())
            .current_dir(self.path("home"));
        cmd.args(["-L", SOCKET]);
        if self.real_tmux {
            cmd.args(["-f", "/dev/null"]);
        }
        cmd.args(args);
        cmd
    }

    fn try_t(&self, args: &[&str]) -> Result<String, String> {
        let out = run_bounded(self.command(args), CMD);
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(out.stderr)
        }
    }

    fn t(&self, args: &[&str]) -> String {
        self.try_t(args)
            .unwrap_or_else(|e| panic!("tmux {args:?}: {e}"))
    }

    /// A fresh server with session `name`, the plugins loaded: grofe reads
    /// the ported config itself; tmux gets what canopy's tmux.conf does.
    fn start(&self, name: &str) {
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
            for (k, v) in self.options() {
                self.t(&["set-option", "-g", k, &v]);
            }
            let conf = fixtures().join("canopy-port/30-plugins.conf");
            self.t(&["source-file", &conf.display().to_string()]);
            self.t(&[
                "run-shell",
                ". \"$CANOPY_STORE/lib/env.sh\"; . \"$CANOPY_STORE/lib/plugins.sh\"; \
                 canopy_load_plugins tmux-resurrect tmux-continuum",
            ]);
        }
        // continuum loads last, and arms its autosave as its last step.
        let loaded = wait_until(
            || {
                self.try_t(&["list-keys", "-T", "prefix"]).is_ok_and(|k| {
                    k.lines()
                        .any(|l| l.contains(" M-s ") && l.contains("save.sh"))
                }) && self
                    .try_t(&["show-options", "-gv", "status-right"])
                    .is_ok_and(|s| s.contains("continuum_save.sh"))
            },
            WAIT,
        );
        assert!(
            loaded.is_ok(),
            "resurrect and continuum never loaded; plugins.log:\n{}\nlogs:\n{}",
            fs::read_to_string(self.path("cstate/plugins.log")).unwrap_or_default(),
            self.logs()
        );
    }

    /// A client on `session`, attached through a pty.
    fn attach(&self, session: &str) -> Harness {
        let mut cmd = Command::new("/usr/bin/env");
        cmd.arg("-i");
        for (k, v) in self.env() {
            cmd.arg(format!("{k}={v}"));
        }
        cmd.arg(self.path("bin/tmux")).args(["-L", SOCKET]);
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

    /// grofe's server and compat-miss logs, for a failure message.
    fn logs(&self) -> String {
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

    fn kill_server(&self) {
        let _ = self.try_t(&["kill-server"]);
        let gone = wait_until(|| self.try_t(&["list-sessions"]).is_err(), WAIT);
        assert!(gone.is_ok(), "the server outlived kill-server");
    }

    fn lines(&self, name: &str) -> usize {
        fs::read_to_string(self.path(name)).map_or(0, |s| s.lines().count())
    }

    fn wait_lines(&self, name: &str, n: usize, what: &str) {
        let got = wait_until(|| self.lines(name) >= n, WAIT);
        assert!(got.is_ok(), "{what}: {name} never reached {n} lines");
    }

    /// Sessions, windows and panes as `list-windows -a` and `list-panes -a`
    /// report them, the rig's root replaced.
    fn structure(&self) -> String {
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
        let sessions = self.t(&[
            "list-sessions",
            "-F",
            "#{session_name} windows=#{session_windows}",
        ]);
        format!("{sessions}{windows}{panes}").replace(&self.root.display().to_string(), "<root>")
    }

    /// `structure` without one session's lines.
    fn without(structure: &str, session: &str) -> String {
        structure
            .lines()
            .filter(|l| {
                !l.starts_with(&format!("{session}:")) && !l.starts_with(&format!("{session} "))
            })
            .map(|l| format!("{l}\n"))
            .collect()
    }

    /// Two sessions: `main` with a window holding a line of output and a
    /// three-pane window, and `two` with two windows; the logs window and
    /// its first pane active.
    fn build(&self) {
        let p = |rel: &str| self.path(rel).display().to_string();
        self.t(&["rename-window", "-t", "main:0", "editor"]);
        self.t(&[
            "send-keys",
            "-t",
            "main:editor",
            &format!("cd '{}' && clear && echo hello-from-w$((1))", p("home/w1")),
            "Enter",
        ]);
        self.t(&[
            "new-window",
            "-t",
            "main",
            "-n",
            "logs",
            "-c",
            &p("home/w2"),
        ]);
        self.t(&["split-window", "-h", "-t", "main:logs", "-c", &p("home/w1")]);
        self.t(&["split-window", "-v", "-t", "main:logs.1", "-c", &p("home")]);
        self.t(&[
            "new-session",
            "-d",
            "-s",
            "two",
            "-x",
            "120",
            "-y",
            "40",
            "-c",
            &p("home/w2"),
        ]);
        // Named, because an automatic name races the shell's start.
        self.t(&["rename-window", "-t", "two:0", "first"]);
        self.t(&[
            "new-window",
            "-t",
            "two",
            "-n",
            "second",
            "-c",
            &p("home/w2"),
        ]);
        self.t(&["select-window", "-t", "main:logs"]);
        self.t(&["select-pane", "-t", "main:logs.0"]);
        let shown = wait_until(
            || {
                self.t(&["capture-pane", "-p", "-t", "main:editor"])
                    .lines()
                    .any(|l| l == "hello-from-w1")
                    && self
                        .t(&["display", "-p", "-t", "main:editor", "#{pane_current_path}"])
                        .trim_end()
                        .ends_with("/w1")
            },
            WAIT,
        );
        assert!(shown.is_ok(), "the editor pane never printed its line");
    }
}

/// `structure` with each layout's checksum and pane ids left out: a
/// restore creates new panes, so only the geometry can come back.
fn geometry(structure: &str) -> String {
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

/// A case that fails part way still stops its server.
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self
            .command(&["kill-server"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Case 1's run: M-s, kill-server, a fresh server, M-r. Returns the
/// structure before the save and after the restore.
fn save_and_restore(rig: &Rig) -> (String, String) {
    rig.start("main");
    rig.build();
    let mut a = rig.attach("main");
    let before = rig.structure();
    a.send(format!("{PREFIX}\x1bs"));
    rig.wait_lines("saves.log", 1, "M-s");
    assert!(
        rig.path("cstate/resurrect/last").exists(),
        "no save under $CANOPY_STATE/resurrect"
    );
    rig.kill_server();
    a.wait_exit();

    rig.start("scratch");
    let mut b = rig.attach("scratch");
    b.send(format!("{PREFIX}\x1br"));
    rig.wait_lines("restores.log", 1, "M-r");
    let after = rig.structure();
    rig.kill_server();
    b.wait_exit();
    (before, after)
}

/// Case 3's run: a save, then a fresh server with continuum's restore
/// armed. Returns the structure once the restore ran, a few seconds later.
fn auto_restore(rig: &Rig) -> String {
    rig.start("main");
    rig.build();
    rig.t(&[
        "run-shell",
        &format!(
            "{}/plugins/tmux-resurrect/scripts/save.sh quiet",
            rig.store().display()
        ),
    ]);
    rig.wait_lines("saves.log", 1, "the save");
    rig.kill_server();
    fs::remove_file(rig.path("home/tmux_no_auto_restore")).unwrap();
    rig.start("scratch");
    rig.wait_lines("restores.log", 1, "continuum's restore");
    // continuum restores from one background job, started once per load.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.lines("restores.log"), 1, "restored more than once");
    let after = rig.structure();
    rig.kill_server();
    after
}

fn golden(name: &str) -> PathBuf {
    fixtures().join(format!("compat/golden/tp_12/{name}.golden"))
}

fn read_golden(name: &str) -> String {
    fs::read_to_string(golden(name))
        .unwrap_or_else(|e| panic!("tp_12/{name}: no golden ({e}); record it"))
}

#[test]
fn case_1_save_now_and_restore_now_bring_the_sessions_back() {
    let rig = Rig::new(None);
    let (before, after) = save_and_restore(&rig);
    // Everything saved is back; the session the fresh server started with
    // is left, as tmux 3.7b leaves it.
    assert_eq!(
        geometry(&Rig::without(&after, "scratch")),
        geometry(&before)
    );
    assert_eq!(
        format!("--- before\n{before}--- after\n{after}"),
        read_golden("save-restore")
    );
}

#[test]
fn case_1_pane_contents_come_back() {
    let rig = Rig::new(None);
    save_and_restore_contents(&rig);
}

/// The editor pane's line of output, after a restore of a save made with
/// `@resurrect-capture-pane-contents on`.
fn save_and_restore_contents(rig: &Rig) {
    rig.start("main");
    rig.build();
    rig.t(&[
        "run-shell",
        &format!(
            "{}/plugins/tmux-resurrect/scripts/save.sh quiet",
            rig.store().display()
        ),
    ]);
    rig.wait_lines("saves.log", 1, "the save");
    assert!(
        rig.path("cstate/resurrect/pane_contents.tar.gz").exists(),
        "no pane contents saved"
    );
    rig.kill_server();
    rig.start("scratch");
    rig.t(&[
        "run-shell",
        &format!(
            "{}/plugins/tmux-resurrect/scripts/restore.sh",
            rig.store().display()
        ),
    ]);
    rig.wait_lines("restores.log", 1, "the restore");
    let back = wait_until(
        || {
            rig.t(&["capture-pane", "-p", "-t", "main:editor"])
                .contains("hello-from-w1")
        },
        WAIT,
    );
    assert!(
        back.is_ok(),
        "pane contents not restored:\n{}",
        rig.t(&["capture-pane", "-p", "-t", "main:editor"])
    );
    rig.kill_server();
}

#[test]
fn case_2_continuum_autosaves_one_save_at_a_time() {
    let rig = Rig::new(None);
    rig.start("main");
    rig.t(&["set-option", "-g", "@continuum-save-interval", "1"]);
    rig.t(&["set-option", "-g", "status-interval", "1"]);
    let probe = rig.path("save-probe").display().to_string();
    rig.t(&["set-option", "-g", "@resurrect-save-script-path", &probe]);
    let mut a = rig.attach("main");
    for n in 1..=2 {
        // An interval and a second ago: the next status refresh saves.
        let due = (now() - 61).to_string();
        rig.t(&["set-option", "-g", "@continuum-save-last-timestamp", &due]);
        let started = Instant::now();
        rig.wait_lines("saves.log", n, "continuum's autosave");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "a save outlasted the interval"
        );
        let ended = wait_until(|| rig.lines("probe.log") >= 2 * n, WAIT);
        assert!(
            ended.is_ok(),
            "save {n} never ended: {}",
            fs::read_to_string(rig.path("probe.log")).unwrap_or_default()
        );
    }
    std::thread::sleep(Duration::from_secs(3));
    let log = fs::read_to_string(rig.path("probe.log")).unwrap();
    let kinds: Vec<&str> = log.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(
        kinds,
        ["start", "end", "start", "end"],
        "saves overlapped:\n{log}"
    );
    assert_eq!(rig.lines("saves.log"), 2);
    rig.kill_server();
    a.wait_exit();
}

#[test]
fn case_3_continuum_restores_the_last_save_once_at_start() {
    let rig = Rig::new(None);
    let after = auto_restore(&rig);
    assert!(after.contains("main:1 logs"), "{after}");
    assert_eq!(after, read_golden("auto-restore"));
}

/// Records cases 1 and 3's goldens from the real tmux 3.7b at
/// `$GROFE_TP12_TMUX`, on its own private socket and directories.
#[test]
#[ignore]
fn record_tp12_goldens() {
    let tmux = PathBuf::from(std::env::var("GROFE_TP12_TMUX").expect("GROFE_TP12_TMUX"));
    assert!(
        tmux.is_absolute(),
        "GROFE_TP12_TMUX must be an absolute path"
    );
    let version = Command::new(&tmux).arg("-V").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "tmux 3.7b");
    fs::create_dir_all(golden("x").parent().unwrap()).unwrap();
    let rig = Rig::new(Some(&tmux));
    let (before, after) = save_and_restore(&rig);
    assert_eq!(
        geometry(&Rig::without(&after, "scratch")),
        geometry(&before)
    );
    fs::write(
        golden("save-restore"),
        format!("--- before\n{before}--- after\n{after}"),
    )
    .unwrap();
    let rig = Rig::new(Some(&tmux));
    save_and_restore_contents(&rig);
    let rig = Rig::new(Some(&tmux));
    fs::write(golden("auto-restore"), auto_restore(&rig)).unwrap();
}
