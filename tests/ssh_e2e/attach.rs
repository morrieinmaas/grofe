//! `grofe -r <host>` against the container that has grofe: one-shot
//! commands print like the local CLI, and an attach shows the pane.

use crate::harness::{REMOTE_TIMEOUT, run, text};

#[test]
fn a_remote_list_sessions_lists_the_container_sessions() {
    let e = e2e_or_skip!();
    let local = e.local();
    let _cleanup = local.server_cleanup(&e, false);
    let out = run(local.grofe(&["-r", &e.host, "new-session", "-d", "-s", "e2e-ls"]));
    assert!(out.status.success(), "{out:?}");
    let out = run(local.grofe(&["-r", &e.host, "list-sessions"]));
    assert!(out.status.success(), "{out:?}");
    // The same listing the container's own CLI prints.
    let there = e.ssh(&local.on("grofe list-sessions"));
    assert!(text(&out).contains("e2e-ls:"), "{out:?}");
    assert_eq!(text(&out), text(&there));
}

#[test]
fn dash_r_attaches_and_a_command_s_output_shows() {
    let e = e2e_or_skip!();
    let local = e.local();
    let _cleanup = local.server_cleanup(&e, false);
    let mut h = local.attach(local.grofe(&["-r", &e.host]));
    local.wait_prompt(&e, &mut h);
    // The output is not in the typed line: 6*7 is computed there.
    h.send("echo e2e-$((6*7))-attach\r");
    h.wait_for_timeout("e2e-42-attach", REMOTE_TIMEOUT);
    // `-r host` with no command is `new-session -A -s main` there.
    let out = e.ssh(&local.on("grofe has-session -t main"));
    assert!(out.status.success(), "{out:?}");
    h.send("\x02d");
    h.wait_exit();
    assert!(String::from_utf8_lossy(h.output()).contains("[detached"));
}
