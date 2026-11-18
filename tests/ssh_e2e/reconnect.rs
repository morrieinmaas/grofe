//! Drops of a real ssh link and the reconnect (spec ssh-remote §2 "Drop
//! and reconnect"): the session's sshd killed in the container, and the
//! container's traffic lost with `tc netem loss 100%`.

use std::time::{Duration, Instant};

use crate::harness::{E2e, REMOTE_TIMEOUT};

fn bar(e: &E2e) -> String {
    format!("reconnecting to {}", e.host)
}

#[test]
fn a_killed_sshd_reconnects_with_the_output_made_meanwhile() {
    let e = e2e_or_skip!();
    let bar = bar(&e);
    let local = e.local();
    let _cleanup = local.server_cleanup(&e, false);
    let mut h = local.attach(local.grofe(&["-r", &e.host]));
    local.wait_prompt(&e, &mut h);
    h.send("echo e2e-$((6*7))-before\r");
    h.wait_for_timeout("e2e-42-before", REMOTE_TIMEOUT);

    // Hold the reconnect off until the pane has printed while dropped.
    local.set_down(true);
    let out = e.helper("e2e_kill_sshd_children", &[]);
    assert!(out.status.success(), "{out:?}");
    h.wait_for_timeout(&bar, REMOTE_TIMEOUT);
    assert!(h.text().contains("e2e-42-before"), "the last frame stays");
    // Typed while offline: dropped.
    h.send("lost-keys");
    let out = e.ssh(&local.on("grofe send-keys -t main 'echo e2e-$((6*7))-away' Enter"));
    assert!(out.status.success(), "{out:?}");
    local.set_down(false);
    h.wait_until("the redraw after the reconnect", REMOTE_TIMEOUT, |t| {
        t.contains("e2e-42-away") && !t.contains(&bar)
    });
    assert!(!h.text().contains("lost-keys"));
    h.send("echo e2e-$((6*7))-after\r");
    h.wait_for_timeout("e2e-42-after", REMOTE_TIMEOUT);
    h.send("\x02d");
    h.wait_exit();
}

#[test]
fn netem_total_loss_is_a_drop_and_clearing_it_reconnects() {
    let e = e2e_or_skip!();
    let bar = bar(&e);
    let local = e.local();
    let _cleanup = local.server_cleanup(&e, true);
    let mut h = local.attach(local.grofe(&["-r", &e.host]));
    local.wait_prompt(&e, &mut h);
    h.send("echo e2e-$((6*7))-before\r");
    h.wait_for_timeout("e2e-42-before", REMOTE_TIMEOUT);

    let out = e.helper("e2e_netem", &["loss", "100%"]);
    assert!(out.status.success(), "{out:?}");
    let lost = Instant::now();
    // Two missed 5 s pings make the drop: 10 to 15 s after the loss.
    h.wait_for_timeout(&bar, REMOTE_TIMEOUT);
    let detected = lost.elapsed();
    assert!(
        detected >= Duration::from_secs(8),
        "a drop before two ping intervals: {detected:?}"
    );
    assert!(h.text().contains("e2e-42-before"), "the last frame stays");
    let out = e.helper("e2e_netem", &["clear"]);
    assert!(out.status.success(), "{out:?}");
    h.wait_until(
        "the bar gone after the reconnect",
        Duration::from_secs(60),
        |t| !t.contains(&bar),
    );
    h.send("echo e2e-$((6*7))-after\r");
    h.wait_for_timeout("e2e-42-after", REMOTE_TIMEOUT);
    h.send("\x02d");
    h.wait_exit();
}
