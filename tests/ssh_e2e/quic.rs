//! The roaming transport against the container (spec ssh-remote §4):
//! `grofe -r host --transport quic` bootstraps over ssh and then talks
//! QUIC to the published UDP ports; a path that goes silent (`e2e_netem
//! loss 100%`) drops the link, and the client comes back over QUIC to the
//! same proxy, from a new client address, with no new bootstrap.
//!
//! The client's own address change is produced by the reconnect: it dials
//! from a new UDP socket, so the proxy sees a new source port. A change of
//! the client's IP address cannot be made with one loopback host.

use std::time::Duration;

use crate::harness::{Cleanup, E2e, REMOTE_TIMEOUT, run, text};
use crate::pty::Harness;

/// The detached proxies' pid files and the QUIC log, under the container
/// user's state dir.
const STATE: &str = "~/.local/state/grofe";

/// The pids the detached proxies wrote, sorted.
fn proxies(e: &E2e) -> Vec<String> {
    let out = e.ssh(&format!("cat {STATE}/*-quic-*.pid 2>/dev/null; true"));
    let mut pids: Vec<String> = text(&out).split_whitespace().map(String::from).collect();
    pids.sort();
    pids
}

/// The client addresses the proxies logged, one per QUIC connection.
fn quic_clients(e: &E2e) -> Vec<String> {
    let out = e.ssh(&format!("cat {STATE}/*-quic.log 2>/dev/null; true"));
    text(&out)
        .lines()
        .filter(|l| !l.contains("client gone"))
        .filter_map(|l| l.split_once(": client ").map(|(_, a)| a.trim().to_string()))
        .collect()
}

#[test]
fn quic_attaches_and_a_silent_path_reconnects_to_the_same_proxy() {
    let e = e2e_or_skip!();
    let Some(ports) = e.udp_ports.clone() else {
        eprintln!("skipped: GROFE_E2E_UDP_PORTS is not set");
        return;
    };
    // Runs last of the cleanups: the netem clear and the server go first.
    let _proxies = Cleanup {
        e: &e,
        host: e.host.clone(),
        command: format!(
            "pkill -f 'grofe proxy --quic'; rm -rf ~/.config/grofe {STATE}/*-quic*; true"
        ),
        netem: false,
    };
    let out = e.ssh(&format!(
        "mkdir -p ~/.config/grofe && echo 'remote-udp-ports = \"{ports}\"' > ~/.config/grofe/config.toml"
    ));
    assert!(out.status.success(), "{out:?}");
    let local = e.local();
    let _cleanup = local.server_cleanup(&e, true);

    let out = run(local.grofe(&[
        "-r",
        &e.host,
        "--transport",
        "quic",
        "new-session",
        "-d",
        "-s",
        "quic",
    ]));
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("using ssh"), "UDP did not get through: {err}");

    let mut cmd = local.grofe(&["-r", &e.host, "--transport", "quic", "attach", "-t", "quic"]);
    // Pings every second: the drop shows within about 2 s of the loss,
    // well inside the 10 s a QUIC reconnect waits for its handshake.
    cmd.env("GROFE_TEST_PING_MS", "1000");
    let mut h = Harness::spawn(cmd, 80, 24);
    h.wait_for_timeout("$ ", REMOTE_TIMEOUT);
    h.send("echo e2e-$((6*7))-before\r");
    h.wait_for_timeout("e2e-42-before", REMOTE_TIMEOUT);
    let before = proxies(&e);
    assert!(!before.is_empty(), "no detached proxy");
    let clients = quic_clients(&e);
    assert!(!clients.is_empty(), "the proxy logged no QUIC client");

    let out = e.helper("e2e_netem", &["loss", "100%"]);
    assert!(out.status.success(), "{out:?}");
    let bar = format!("reconnecting to {}", e.host);
    h.wait_for_timeout(&bar, REMOTE_TIMEOUT);
    let out = e.helper("e2e_netem", &["clear"]);
    assert!(out.status.success(), "{out:?}");
    h.wait_until(
        "the bar gone after the reconnect",
        Duration::from_secs(60),
        |t| !t.contains(&bar),
    );
    h.send("echo e2e-$((6*7))-after\r");
    h.wait_for_timeout("e2e-42-after", REMOTE_TIMEOUT);

    assert_eq!(
        proxies(&e),
        before,
        "the reconnect bootstrapped a new proxy"
    );
    let after = quic_clients(&e);
    assert!(
        after.len() > clients.len(),
        "no new QUIC connection: {after:?}"
    );
    let (old, new) = (&clients[clients.len() - 1], &after[after.len() - 1]);
    assert_ne!(old, new, "the reconnect came from the same client address");
    h.send("\x02d");
    h.wait_exit();
}
