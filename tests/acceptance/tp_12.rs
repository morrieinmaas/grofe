//! TP.12: tmux-resurrect and tmux-continuum, unchanged, on grofe run as
//! `tmux` with canopy's ported plugin fragment and TP.3's loader, in a
//! `plugin_rig` holding the two plugins. resurrect saves under the rig's
//! `$CANOPY_STATE/resurrect`.
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

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::plugin_rig::{
    PREFIX, Rig, WAIT, geometry, read_golden, recording_tmux, wait_until, write_golden,
};

const TASK: &str = "tp12";
const GOLDEN: &str = "tp_12";
const PLUGINS: &[&str] = &["tmux-resurrect", "tmux-continuum"];

/// Wraps resurrect's save script for case 2, logging each save's start and
/// end with its pid. continuum runs the script path as one word.
const SAVE_PROBE: &str = r#"#!/bin/sh
echo "start $$" >>'<root>/probe.log'
'<root>/store/plugins/tmux-resurrect/scripts/save.sh' "$@"
echo "end $$" >>'<root>/probe.log'
"#;

/// A rig for resurrect and continuum: pane contents saved, and a line
/// logged after each save and each restore.
fn rig(real: Option<&Path>) -> Rig {
    let log = |name: &str| format!("echo done >> '<root>/{name}'");
    let rig = Rig::new(
        TASK,
        real,
        PLUGINS,
        vec![
            ("@resurrect-capture-pane-contents".into(), "on".into()),
            ("@resurrect-hook-post-save-all".into(), log("saves.log")),
            (
                "@resurrect-hook-post-restore-all".into(),
                log("restores.log"),
            ),
        ],
    );
    for d in ["home/w1", "home/w2"] {
        fs::create_dir_all(rig.path(d)).unwrap();
    }
    let root = rig.root.display().to_string();
    rig.install("save-probe", &SAVE_PROBE.replace("<root>", &root));
    rig
}

/// Two sessions: `main` with a window holding a line of output and a
/// three-pane window, and `two` with two windows; the logs window and its
/// first pane active.
fn build(rig: &Rig) {
    let p = |rel: &str| rig.path(rel).display().to_string();
    rig.t(&["rename-window", "-t", "main:0", "editor"]);
    rig.t(&[
        "send-keys",
        "-t",
        "main:editor",
        &format!("cd '{}' && clear && echo hello-from-w$((1))", p("home/w1")),
        "Enter",
    ]);
    rig.t(&[
        "new-window",
        "-t",
        "main",
        "-n",
        "logs",
        "-c",
        &p("home/w2"),
    ]);
    rig.t(&["split-window", "-h", "-t", "main:logs", "-c", &p("home/w1")]);
    rig.t(&["split-window", "-v", "-t", "main:logs.1", "-c", &p("home")]);
    rig.t(&[
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
    rig.t(&["rename-window", "-t", "two:0", "first"]);
    rig.t(&[
        "new-window",
        "-t",
        "two",
        "-n",
        "second",
        "-c",
        &p("home/w2"),
    ]);
    rig.t(&["select-window", "-t", "main:logs"]);
    rig.t(&["select-pane", "-t", "main:logs.0"]);
    let shown = wait_until(
        || {
            rig.t(&["capture-pane", "-p", "-t", "main:editor"])
                .lines()
                .any(|l| l == "hello-from-w1")
                && rig
                    .t(&["display", "-p", "-t", "main:editor", "#{pane_current_path}"])
                    .trim_end()
                    .ends_with("/w1")
        },
        WAIT,
    );
    assert!(shown.is_ok(), "the editor pane never printed its line");
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
    build(rig);
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
    build(rig);
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

#[test]
fn case_1_save_now_and_restore_now_bring_the_sessions_back() {
    let rig = rig(None);
    let (before, after) = save_and_restore(&rig);
    // Everything saved is back; the session the fresh server started with
    // is left, as tmux 3.7b leaves it.
    assert_eq!(
        geometry(&Rig::without(&after, "scratch")),
        geometry(&before)
    );
    assert_eq!(
        format!("--- before\n{before}--- after\n{after}"),
        read_golden(GOLDEN, "save-restore")
    );
}

#[test]
fn case_1_pane_contents_come_back() {
    let rig = rig(None);
    save_and_restore_contents(&rig);
}

/// The editor pane's line of output, after a restore of a save made with
/// `@resurrect-capture-pane-contents on`.
fn save_and_restore_contents(rig: &Rig) {
    rig.start("main");
    build(rig);
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
    let rig = rig(None);
    rig.start("main");
    rig.t(&["set-option", "-g", "@continuum-save-interval", "1"]);
    rig.t(&["set-option", "-g", "status-interval", "1"]);
    // As shipped, continuum's saves go through canopy's one-at-a-time
    // wrapper; the probe stands in for it here.
    // (Set by the load command after the loader, which may still be
    // running when continuum, the last plugin, has loaded.)
    let shipped = wait_until(
        || {
            rig.t(&["show-options", "-gv", "@resurrect-save-script-path"])
                .trim_end()
                .ends_with("/plugins/strategies/canopy_save.sh")
        },
        WAIT,
    );
    assert!(
        shipped.is_ok(),
        "continuum's saves do not go through canopy's wrapper"
    );
    let probe = rig.path("bin/save-probe").display().to_string();
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
    let rig = rig(None);
    let after = auto_restore(&rig);
    assert!(after.contains("main:1 logs"), "{after}");
    assert_eq!(after, read_golden(GOLDEN, "auto-restore"));
}

/// Records cases 1 and 3's goldens from the real tmux 3.7b at
/// `$GROFE_TP12_TMUX`, on its own private socket and directories.
#[test]
#[ignore]
fn record_tp12_goldens() {
    let tmux = recording_tmux("GROFE_TP12_TMUX");
    let rig = rig(Some(&tmux));
    let (before, after) = save_and_restore(&rig);
    assert_eq!(
        geometry(&Rig::without(&after, "scratch")),
        geometry(&before)
    );
    write_golden(
        GOLDEN,
        "save-restore",
        &format!("--- before\n{before}--- after\n{after}"),
    );
    save_and_restore_contents(&self::rig(Some(&tmux)));
    write_golden(
        GOLDEN,
        "auto-restore",
        &auto_restore(&self::rig(Some(&tmux))),
    );
}
