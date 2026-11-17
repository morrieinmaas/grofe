//! `set-client-info` (spec ssh-remote §4): what a remote client tells the
//! server about itself, shown by `#{client_rtt}` and `#{client_machines}`,
//! through the control socket.

mod common;
#[path = "acceptance/harness.rs"]
#[allow(dead_code)]
mod harness;

use common::TestServer;
use harness::Harness;

/// A server with session `main` and one attached client; its name.
fn attached() -> (TestServer, Harness, String) {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main"]).unwrap();
    let mut h = Harness::spawn_attach_with_env(srv.command(), "main", &[], 80, 24);
    h.wait_for(common::PROMPT.trim());
    let name = srv
        .cmd(&["list-clients", "-F", "#{client_name}"])
        .unwrap()
        .trim()
        .to_string();
    assert!(!name.is_empty());
    (srv, h, name)
}

fn info(srv: &TestServer) -> String {
    srv.cmd(&["list-clients", "-F", "[#{client_rtt}|#{client_machines}]"])
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn the_formats_are_empty_until_set() {
    let (srv, _h, _) = attached();
    assert_eq!(info(&srv), "[|]");
}

#[test]
fn set_client_info_sets_rtt_and_machines() {
    let (srv, _h, name) = attached();
    srv.cmd(&["set-client-info", "-t", &name, "rtt", "42"])
        .unwrap();
    assert_eq!(info(&srv), "[42ms|]");
    srv.cmd(&[
        "set-client-info",
        "-t",
        &name,
        "machines",
        "box ok, pi down",
    ])
    .unwrap();
    assert_eq!(info(&srv), "[42ms|box ok, pi down]");
    srv.cmd(&["set-client-info", "-t", &name, "rtt", "7"])
        .unwrap();
    assert_eq!(info(&srv), "[7ms|box ok, pi down]");
}

#[test]
fn bad_keys_values_and_clients_are_errors() {
    let (srv, _h, name) = attached();
    let err = srv
        .cmd(&["set-client-info", "-t", &name, "colour", "red"])
        .unwrap_err();
    assert!(err.contains("unknown key"), "{err}");
    let err = srv
        .cmd(&["set-client-info", "-t", &name, "rtt", "fast"])
        .unwrap_err();
    assert!(err.contains("rtt"), "{err}");
    assert!(
        srv.cmd(&["set-client-info", "-t", "nobody", "rtt", "1"])
            .is_err()
    );
    assert!(srv.cmd(&["set-client-info", "-t", &name, "rtt"]).is_err());
    assert_eq!(info(&srv), "[|]");
}

#[test]
fn the_status_line_redraws_with_the_new_value() {
    let (srv, mut h, name) = attached();
    srv.cmd(&["set-option", "-g", "status-right", "rtt=#{client_rtt}"])
        .unwrap();
    h.wait_for("rtt=");
    srv.cmd(&["set-client-info", "-t", &name, "rtt", "123"])
        .unwrap();
    h.wait_for("rtt=123ms");
    srv.cmd(&["set-option", "-g", "status-left", "[#{client_machines}]"])
        .unwrap();
    srv.cmd(&["set-client-info", "-t", &name, "machines", "pi*"])
        .unwrap();
    h.wait_for("[pi*]");
}
