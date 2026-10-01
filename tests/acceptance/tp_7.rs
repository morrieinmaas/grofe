//! TP.7: tmux-fzf, unchanged, in a `plugin_rig` holding only it. prefix F
//! opens its action menu in a `display-popup` (through its own
//! `.fzf-tmux`), and each step is picked by typing into the popup's fzf
//! from an attached client, as a user would:
//!
//! - Case 1: window, switch, the other window: it becomes active.
//! - Case 2: pane, kill, the other pane: it is gone, the current one stays.
//! - Case 3: session, switch, the other session: the client shows it.
//!
//! Each case's end state matches a golden recorded from tmux 3.7b by the
//! same code (`record_tp7_goldens`, ignored: `GROFE_TP7_TMUX=<absolute path
//! to tmux 3.7b> cargo test --test acceptance record_tp7 -- --ignored`).

use std::path::Path;

use crate::harness::{Harness, require_tool};
use crate::plugin_rig::{PREFIX, Rig, WAIT, read_golden, recording_tmux, wait_until, write_golden};

const TASK: &str = "tp7";
const GOLDEN: &str = "tp_7";

fn rig(real: Option<&Path>) -> Rig {
    Rig::new(TASK, real, &["tmux-fzf"], Vec::new())
}

/// Types `item` into the popup once its list shows `shown`, then Enter
/// once fzf shows the query. (The plugin's `--header` lines reach fzf only
/// through an exported `FZF_DEFAULT_OPTS`, so they are not waited on.)
fn pick(a: &mut Harness, shown: &str, item: &str) {
    a.wait_for(shown);
    a.send(item);
    a.wait_for(&format!("> {item}"));
    a.send("\r");
}

/// What a case ends with: the structure and the client's session.
fn end_state(rig: &Rig) -> String {
    let client = rig.t(&["list-clients", "-F", "client session=#{client_session}"]);
    format!("{}{client}", rig.structure())
}

fn wait_state(rig: &Rig, what: &str, done: impl Fn(&str) -> bool) {
    let got = wait_until(|| done(&end_state(rig)), WAIT);
    assert!(got.is_ok(), "{what}; state:\n{}", end_state(rig));
}

/// Case 1: two windows, the first current; window, switch, `main:1`.
fn switch_window(rig: &Rig) -> String {
    rig.start("main");
    rig.t(&["new-window", "-d", "-t", "main", "-n", "second"]);
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}F"));
    pick(&mut a, "keybinding", "window");
    pick(&mut a, "rename", "switch");
    pick(&mut a, "main:1: second", "main:1:");
    wait_state(rig, "main:1 never became active", |s| {
        s.contains("main:1 second active=1")
    });
    end_state(rig)
}

/// Case 2: a window split in two, the new pane current; pane, kill,
/// `main:0.0`.
fn kill_pane(rig: &Rig) -> String {
    rig.start("main");
    rig.t(&["split-window", "-h", "-t", "main:0"]);
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}F"));
    pick(&mut a, "keybinding", "pane");
    pick(&mut a, "resize", "kill");
    pick(&mut a, "main:0.0: [", "main:0.0:");
    wait_state(rig, "main:0.0 was never killed", |s| {
        !s.contains("main:0.1")
    });
    end_state(rig)
}

/// Case 3: sessions `main` (attached) and `two`; session, switch, `two`.
fn switch_session(rig: &Rig) -> String {
    rig.start("main");
    let home = rig.path("home").display().to_string();
    rig.t(&["new-session", "-d", "-s", "two", "-c", &home]);
    let mut a = rig.attach("main");
    a.send(format!("{PREFIX}F"));
    pick(&mut a, "keybinding", "session");
    pick(&mut a, "detach", "switch");
    pick(&mut a, "two: 1 windows", "two:");
    wait_state(rig, "the client never switched to two", |s| {
        s.contains("client session=two")
    });
    end_state(rig)
}

#[test]
fn case_1_switch_window() {
    if !require_tool("fzf") {
        return;
    }
    assert_eq!(
        switch_window(&rig(None)),
        read_golden(GOLDEN, "switch-window")
    );
}

#[test]
fn case_2_kill_pane() {
    if !require_tool("fzf") {
        return;
    }
    assert_eq!(kill_pane(&rig(None)), read_golden(GOLDEN, "kill-pane"));
}

#[test]
fn case_3_switch_session() {
    if !require_tool("fzf") {
        return;
    }
    assert_eq!(
        switch_session(&rig(None)),
        read_golden(GOLDEN, "switch-session")
    );
}

/// Records the three cases' goldens from the real tmux 3.7b at
/// `$GROFE_TP7_TMUX`, each on its own private socket and directories.
#[test]
#[ignore]
fn record_tp7_goldens() {
    let tmux = recording_tmux("GROFE_TP7_TMUX");
    write_golden(GOLDEN, "switch-window", &switch_window(&rig(Some(&tmux))));
    write_golden(GOLDEN, "kill-pane", &kill_pane(&rig(Some(&tmux))));
    write_golden(GOLDEN, "switch-session", &switch_session(&rig(Some(&tmux))));
}
