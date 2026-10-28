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
    PREFIX, Rig, WAIT, geometry, private_tmp, read_golden, recording_tmux, wait_until, write_golden,
};

const TASK: &str = "tp12";
const GOLDEN: &str = "tp_12";
const PLUGINS: &[&str] = &["tmux-resurrect", "tmux-continuum"];

/// Wraps canopy's save wrapper for case 2, logging each save's start and
/// end with its pid. continuum runs the script path as one word.
const SAVE_PROBE: &str = r#"#!/bin/sh
echo "start $$" >>'<root>/probe.log'
'<root>/store/plugins/strategies/canopy_save.sh' "$@"
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

/// The wrapper canopy's config points continuum's saves and M-s at.
fn wrapper(rig: &Rig) -> String {
    rig.store()
        .join("plugins/strategies/canopy_save.sh")
        .display()
        .to_string()
}

/// Waits until the load is over: the config's last step after the loader
/// points continuum's saves and M-s at canopy's wrapper, expanded (a
/// literal `$CANOPY_STORE` would not run).
fn wait_loaded(rig: &Rig) {
    let want = wrapper(rig);
    let done = wait_until(
        || {
            rig.t(&["show-options", "-gv", "@resurrect-save-script-path"])
                .trim_end()
                == want
                && rig
                    .t(&["list-keys", "-T", "prefix"])
                    .lines()
                    .any(|l| l.contains(" M-s ") && l.contains("canopy_save.sh"))
        },
        WAIT,
    );
    assert!(
        done.is_ok(),
        "continuum's saves and M-s do not run {want}: {}",
        rig.t(&["show-options", "-gv", "@resurrect-save-script-path"])
    );
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
/// armed. Returns the structure once the restore ran.
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
    // continuum restores from one background job, started once per load:
    // with the load over, nothing can start another.
    wait_loaded(rig);
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
    // continuum's launcher locks with directories under the shared /tmp.
    if !private_tmp("/tmp/tmux-continuum-<server pid>-lock-*") {
        return;
    }
    let rig = rig(None);
    rig.start("main");
    rig.t(&["set-option", "-g", "@continuum-save-interval", "1"]);
    rig.t(&["set-option", "-g", "status-interval", "1"]);
    wait_loaded(&rig);
    // The probe logs each save around canopy's wrapper, which continuum
    // runs as shipped.
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

/// Case 4: canopy's lock. An autosave made slow (a `sleep` in
/// resurrect's post-save-layout hook, mid-save) is running when M-s and a
/// second autosave arrive: both are skipped, each with one log line, and
/// only the first save writes.
#[test]
fn case_4_a_save_during_a_save_is_skipped() {
    let rig = rig(None);
    rig.start("main");
    wait_loaded(&rig);
    rig.t(&[
        "set-option",
        "-g",
        "@resurrect-hook-post-save-layout",
        "sleep 3",
    ]);
    let mut a = rig.attach("main");
    let wrapper = wrapper(&rig);
    rig.t(&["run-shell", "-b", &format!("'{wrapper}' quiet")]);
    let lock = rig.path("cstate").join(format!(
        "resurrect-save.{}.lock",
        rig.t(&["display-message", "-p", "#{pid}"]).trim()
    ));
    let held = wait_until(|| lock.join("pid").exists(), WAIT);
    assert!(held.is_ok(), "the first save never took {}", lock.display());
    a.send(format!("{PREFIX}\x1bs"));
    rig.wait_lines("cstate/resurrect-save.log", 1, "M-s during a save");
    rig.t(&["run-shell", "-b", &format!("'{wrapper}' quiet")]);
    rig.wait_lines("cstate/resurrect-save.log", 2, "an autosave during a save");
    rig.wait_lines("saves.log", 1, "the first save");
    let gone = wait_until(|| !lock.exists(), WAIT);
    assert!(gone.is_ok(), "the first save kept its lock");
    assert_eq!(rig.lines("saves.log"), 1, "a skipped save wrote");
    let log = fs::read_to_string(rig.path("cstate/resurrect-save.log")).unwrap();
    assert!(
        log.lines()
            .all(|l| l.contains("skipped: a save is already running")),
        "{log}"
    );
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

/// Case 6 (B1e): a grouped session is saved as a line pointing at its
/// original and restored by resurrect's `new-session -d -s <name> -t
/// <original>`: back in the same group, with the original's windows.
#[test]
fn case_6_a_grouped_session_comes_back_in_its_group() {
    let rig = rig(None);
    rig.start("main");
    build(&rig);
    rig.t(&["new-session", "-d", "-s", "mirror", "-t", "main"]);
    let list = |rig: &Rig| {
        rig.t(&[
            "list-sessions",
            "-F",
            "#{session_name} group=#{session_group} [#{session_group_list}] windows=#{session_windows}",
        ])
    };
    let before = list(&rig);
    assert!(
        before.contains("mirror group=main [main,mirror] windows=2"),
        "{before}"
    );
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}\x1bs"));
    rig.wait_lines("saves.log", 1, "M-s");
    rig.kill_server();
    a.wait_exit();

    rig.start("scratch");
    let mut b = rig.attach("scratch");
    b.send(format!("{PREFIX}\x1br"));
    rig.wait_lines("restores.log", 1, "M-r");
    let after = list(&rig);
    assert!(
        after.contains("main group=main [main,mirror] windows=2")
            && after.contains("mirror group=main [main,mirror] windows=2"),
        "{after}"
    );
    // One window list: the windows are the original's, not copies.
    let ids = rig.t(&[
        "list-windows",
        "-a",
        "-F",
        "#{session_name}:#{window_index}:#{window_id}",
    ]);
    let id_of = |s: &str| {
        ids.lines()
            .filter(|l| l.starts_with(s))
            .map(|l| l.rsplit(':').next().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(id_of("main:"), id_of("mirror:"), "{ids}");
    rig.kill_server();
    b.wait_exit();
}
