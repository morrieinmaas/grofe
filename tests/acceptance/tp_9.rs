//! TP.9: opensessions, unchanged, in a `plugin_rig` holding only it, with
//! its pinned release binaries (v0.2.0-alpha.12, the ones canopy
//! provisions) in place.
//!
//! opensessions' server installs its twelve global hooks when it starts.
//! Every hook call goes out through `curl ... -X POST
//! http://<host>:<port>/<path> -d <context>`; a `curl` first on the rig's
//! PATH records each call's argv and then runs the real one, so the server
//! still gets it.
//!
//! - Case 1: with the server started by the plugin's own
//!   `ensure-sidebar.sh` (the sidebar starts hidden, so none opens), a
//!   scripted session (a session created, two windows, a split, a pane
//!   killed, a session closed) fires the same hook calls, step by step, as
//!   tmux 3.7b does. Calls a step starts run in the background, so within
//!   a step they are compared as a set.
//! - Case 2: prefix o t (the plugin's toggle) opens the sidebar at
//!   `@opensessions_width`.
//! - Case 3: `resize-window` leaves the sidebar at that width.
//!
//! Each case matches a golden recorded from tmux 3.7b by the same code
//! (`record_tp9_goldens`, ignored: `GROFE_TP9_TMUX=<absolute path to tmux
//! 3.7b> cargo test --test acceptance record_tp9 -- --ignored`).
//!
//! The binaries are not vendored, and fetching them means the network, so
//! the test needs a directory holding them at `$GROFE_TEST_OPENSESSIONS`:
//! without it it skips, and under `CI` it fails. Each rig's server gets a
//! port of its own, a server key and a pid file in the rig, so nothing can
//! reach another opensessions server; the plugin's server log and start
//! lock under /tmp, named by that key, are removed afterwards.
//!
//! The sidebar's own launcher appends to `/tmp/opensessions-err.log`, a
//! fixed path every opensessions on the machine shares, so cases 2 and 3
//! run only where `/tmp` is the test's own: with `GROFE_TEST_PRIVATE_TMP=1`
//! (a container). Elsewhere they skip.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::harness::Harness;
use crate::plugin_rig::{PREFIX, Rig, WAIT, read_golden, recording_tmux, wait_until, write_golden};

const TASK: &str = "tp9";
const GOLDEN: &str = "tp_9";

/// Records every call's argv, then runs the real curl.
const CURL: &str = "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'<root>/curl.log'\nexec <curl> \"$@\"\n";

/// The provisioned binaries, if this machine has them.
fn binaries() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("GROFE_TEST_OPENSESSIONS")?);
    let ok = [
        "opensessions-server",
        "opensessions-sidebar",
        "lazydiff",
        ".opensessions-version",
    ]
    .iter()
    .all(|f| dir.join(f).is_file());
    ok.then_some(dir)
}

fn tools() -> Option<PathBuf> {
    match binaries() {
        Some(dir) => Some(dir),
        None if std::env::var_os("CI").is_some() => {
            panic!("no opensessions binaries at $GROFE_TEST_OPENSESSIONS (required under CI)")
        }
        None => {
            eprintln!("skipping: no opensessions binaries at $GROFE_TEST_OPENSESSIONS");
            None
        }
    }
}

/// A rig and what it must stop and remove after: the server's pid file and
/// the plugin's /tmp files for this rig's key.
struct Opensessions {
    rig: Rig,
    key: String,
    port: u16,
}

impl Drop for Opensessions {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.rig.path("opensessions.pid"))
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        for suffix in ["server.log", "version", "pid"] {
            let _ = fs::remove_file(format!("/tmp/opensessions.{}.{suffix}", self.key));
        }
        let _ = fs::remove_dir_all(format!("/tmp/opensessions.{}.start.lock", self.key));
    }
}

fn rig(real: Option<&Path>, binaries: &Path) -> Opensessions {
    let mut rig = Rig::new(TASK, real, &["opensessions"], Vec::new());
    let bin = rig.store().join("plugins/opensessions/bin");
    fs::create_dir_all(&bin).unwrap();
    for f in [
        "opensessions-server",
        "opensessions-sidebar",
        "lazydiff",
        ".opensessions-version",
    ] {
        fs::copy(binaries.join(f), bin.join(f)).unwrap();
    }
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let key = format!("grofe{TASK}{}{port}", std::process::id());
    rig.server_env(&[
        ("OPENSESSIONS_PORT", port.to_string()),
        ("OPENSESSIONS_SERVER_KEY", key.clone()),
        (
            "OPENSESSIONS_PID_FILE",
            rig.path("opensessions.pid").display().to_string(),
        ),
    ]);
    let curl = ["/usr/bin/curl", "/bin/curl"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .expect("a curl");
    let root = rig.root.display().to_string();
    rig.install(
        "curl",
        &CURL.replace("<root>", &root).replace("<curl>", curl),
    );
    Opensessions { rig, key, port }
}

/// The hook calls `curl.log` holds from line `from` on: each POST's path
/// and body, the port and client tty replaced.
fn posts(os: &Opensessions, from: usize) -> Vec<String> {
    let log = fs::read_to_string(os.rig.path("curl.log")).unwrap_or_default();
    log.lines()
        .skip(from)
        .filter(|l| l.contains("-X POST"))
        .map(|l| {
            let l = l.replace(&os.port.to_string(), "<port>");
            let l = l
                .split(' ')
                .map(|w| {
                    if w.starts_with("'/dev/") || w.starts_with("/dev/") {
                        w.split('|')
                            .enumerate()
                            .map(|(i, part)| if i == 0 { "<tty>" } else { part })
                            .collect::<Vec<_>>()
                            .join("|")
                    } else {
                        w.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            l.split_once("http://")
                .map_or(l.clone(), |(_, rest)| rest.to_string())
        })
        .collect()
}

/// Waits until `curl.log` has not grown for half a second.
fn settle(os: &Opensessions) {
    let lines = || os.rig.lines("curl.log");
    let mut last = lines();
    let mut quiet = Instant::now();
    let deadline = Instant::now() + WAIT;
    while quiet.elapsed() < Duration::from_millis(500) {
        assert!(Instant::now() < deadline, "curl.log never settled");
        std::thread::sleep(Duration::from_millis(50));
        let now = lines();
        if now != last {
            last = now;
            quiet = Instant::now();
        }
    }
}

/// The sidebar's pane in `window`, as `<width> <pane id>`.
fn sidebar(rig: &Rig, window: &str) -> Option<String> {
    rig.try_t(&[
        "list-panes",
        "-t",
        window,
        "-F",
        "#{pane_title}|#{pane_width}",
    ])
    .ok()?
    .lines()
    .find_map(|l| l.strip_prefix("opensessions-sidebar|").map(str::to_string))
}

/// The plugin's scripts directory in the rig's store.
fn scripts(rig: &Rig) -> String {
    rig.store()
        .join("plugins/opensessions/integrations/tmux-plugin/scripts")
        .display()
        .to_string()
}

/// Waits until the server has installed its hooks and gone quiet.
fn wait_hooks(os: &Opensessions) {
    let up = wait_until(
        || {
            os.rig
                .try_t(&["show-hooks", "-g"])
                .is_ok_and(|h| h.contains("/pane-exited"))
        },
        WAIT,
    );
    assert!(
        up.is_ok(),
        "opensessions never installed its hooks; curl:\n{}",
        fs::read_to_string(os.rig.path("curl.log")).unwrap_or_default()
    );
    settle(os);
}

/// Toggles the sidebar open from an attached client: prefix o t.
fn open_sidebar(os: &Opensessions) -> Harness {
    let rig = &os.rig;
    rig.start("main");
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}ot"));
    let open = wait_until(|| sidebar(rig, "main:0").is_some(), WAIT);
    assert!(
        open.is_ok(),
        "the sidebar never opened; panes:\n{}\ncurl:\n{}",
        rig.t(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id} #{pane_title} #{pane_width}"
        ]),
        fs::read_to_string(rig.path("curl.log")).unwrap_or_default()
    );
    wait_hooks(os);
    a
}

/// Case 1's transcript: each step and the hook calls it fired, sorted.
fn hook_calls(os: &Opensessions) -> String {
    let rig = &os.rig;
    rig.start("main");
    let mut a = rig.attach("main");
    rig.t(&[
        "run-shell",
        &format!("sh '{}/ensure-sidebar.sh'", scripts(rig)),
    ]);
    wait_hooks(os);
    assert!(sidebar(rig, "main:0").is_none(), "a hidden sidebar opened");
    let home = rig.path("home").display().to_string();
    let tty = rig.t(&["list-clients", "-F", "#{client_tty}"]);
    let tty = tty.trim();
    let steps: &[(&str, &[&str])] = &[
        (
            "new-session two",
            &["new-session", "-d", "-s", "two", "-c", &home],
        ),
        ("new-window one", &["new-window", "-t", "main", "-n", "one"]),
        ("new-window two", &["new-window", "-t", "main", "-n", "two"]),
        ("split-window", &["split-window", "-t", "main:two"]),
        ("select-pane", &["select-pane", "-t", "main:two.0"]),
        ("select-window", &["select-window", "-t", "main:one"]),
        (
            "resize-pane",
            &["resize-pane", "-t", "main:two.0", "-x", "50"],
        ),
        (
            "resize-window",
            &["resize-window", "-t", "main:one", "-x", "100", "-y", "30"],
        ),
        (
            "switch-client two",
            &["switch-client", "-c", tty, "-t", "two"],
        ),
        ("client resized", &[]),
        (
            "pane exits",
            &["send-keys", "-t", "main:two.1", "exit", "Enter"],
        ),
        ("kill-pane", &["kill-pane", "-t", "main:two.0"]),
        (
            "switch-client main",
            &["switch-client", "-c", tty, "-t", "main"],
        ),
        ("kill-session two", &["kill-session", "-t", "two"]),
    ];
    let mut out = String::new();
    for (name, argv) in steps {
        let from = rig.lines("curl.log");
        if argv.is_empty() {
            a.resize(110, 35);
        } else {
            rig.t(argv);
        }
        settle(os);
        let mut calls = posts(os, from);
        calls.sort();
        out.push_str(&format!("## {name}\n"));
        for c in calls {
            out.push_str(&format!("{c}\n"));
        }
    }
    out
}

/// Whether the sidebar cases may run here: only with a private /tmp.
fn private_tmp() -> bool {
    let ok = std::env::var_os("GROFE_TEST_PRIVATE_TMP").is_some_and(|v| v == "1");
    if !ok {
        eprintln!(
            "skipping: the sidebar writes /tmp/opensessions-err.log; set GROFE_TEST_PRIVATE_TMP=1 where /tmp is private"
        );
    }
    ok
}

/// Cases 2 and 3: the sidebar's width and the option, then after a
/// `resize-window`.
fn widths(os: &Opensessions) -> String {
    let rig = &os.rig;
    let _a = open_sidebar(os);
    let option = rig.t(&["show-options", "-gv", "@opensessions_width"]);
    let before = sidebar(rig, "main:0").unwrap();
    rig.t(&["resize-window", "-t", "main:0", "-x", "100", "-y", "30"]);
    settle(os);
    let after = sidebar(rig, "main:0").unwrap();
    format!("option {option}open {before}\nresized {after}\n")
}

#[test]
fn case_1_hooks_fire_as_on_tmux() {
    let Some(bin) = tools() else { return };
    let got = hook_calls(&rig(None, &bin));
    assert_eq!(got, read_golden(GOLDEN, "hook-calls"));
}

#[test]
fn case_2_and_3_the_sidebar_keeps_its_width() {
    let Some(bin) = tools() else { return };
    if !private_tmp() {
        return;
    }
    let got = widths(&rig(None, &bin));
    assert_eq!(got, read_golden(GOLDEN, "widths"));
}

/// Records the goldens from the real tmux 3.7b at `$GROFE_TP9_TMUX`, each
/// case on its own private socket, directories and opensessions port.
#[test]
#[ignore]
fn record_tp9_goldens() {
    let tmux = recording_tmux("GROFE_TP9_TMUX");
    let bin = binaries().expect("opensessions binaries at $GROFE_TEST_OPENSESSIONS");
    write_golden(GOLDEN, "hook-calls", &hook_calls(&rig(Some(&tmux), &bin)));
    if private_tmp() {
        write_golden(GOLDEN, "widths", &widths(&rig(Some(&tmux), &bin)));
    }
}
