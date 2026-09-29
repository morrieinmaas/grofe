//! The M3.6 stub edit (wave 0): every new native verb parses and is routed
//! to its owning task's handler, which answers `not implemented: <verb>`
//! until that task lands; and the `compat-identity` option exists with its
//! default. Written so no M3.6 task has to edit it: a verb that has landed
//! may answer anything except `unknown command`, and a stub must use the
//! exact `not implemented: <verb>` text.

mod common;

use common::TestServer;

/// Every native verb the stub edit adds (m36-addendum, "Stub edit" item 3).
const NEW_VERBS: &[&str] = &[
    // TC.8, dispatch/structure.rs
    "join-pane",
    "swap-pane",
    "swap-window",
    "break-pane",
    "resize-window",
    "last-pane",
    "rotate-window",
    "move-window",
    "move-pane",
    "next-layout",
    "respawn-window",
    // TC.9, dispatch/client.rs
    "refresh-client",
    "show-messages",
    "choose-client",
    "suspend-client",
    "find-window",
    "choose-tree",
    "wait-for",
    // TC.6, dispatch/popup.rs
    "display-popup",
    "new-pane",
    // TC.7, dispatch/menu.rs
    "display-menu",
    "display-panes",
    "confirm-before",
    // TC.10, dispatch/bindings.rs
    "if-shell",
    // TC.5, dispatch/hooks.rs
    "set-hook",
    "show-hooks",
    "set-environment",
    "show-environment",
];

#[test]
fn every_new_verb_is_routed_not_unknown() {
    let srv = TestServer::start();
    srv.cmd(&["new-session", "-d", "-s", "main", "sleep", "30"])
        .expect("new-session");
    for verb in NEW_VERBS {
        let out = srv.run(&[verb]);
        let err = out.stderr.trim_end();
        assert!(!err.contains("unknown command"), "{verb}: {err}");
        if err.contains("not implemented") {
            assert_eq!(err, format!("not implemented: {verb}"), "{verb}");
        }
    }
    // The stubs changed nothing: the session is still there, alone.
    let sessions = srv.cmd(&["list-sessions", "-F", "#{session_name}"]);
    assert_eq!(sessions.as_deref(), Ok("main\n"));
}

#[test]
fn list_commands_stays_front_end_only() {
    // M36-R1: no native `list-commands`; FE.1 answers it for tmux.
    let srv = TestServer::start();
    let err = srv.cmd(&["list-commands"]).unwrap_err();
    assert!(err.contains("unknown command: list-commands"), "{err}");
}

#[test]
fn compat_identity_option_defaults_to_auto() {
    let srv = TestServer::start();
    let out = srv.cmd(&["show-options", "-g", "compat-identity"]);
    assert_eq!(out.as_deref(), Ok("compat-identity auto\n"));
    for value in ["tmux", "grofe", "auto"] {
        srv.cmd(&["set-option", "-g", "compat-identity", value])
            .unwrap_or_else(|e| panic!("{value}: {e}"));
    }
    let err = srv
        .cmd(&["set-option", "-g", "compat-identity", "screen"])
        .unwrap_err();
    assert!(err.contains("screen"), "{err}");
}
