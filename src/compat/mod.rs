//! tmux compatibility, identity and entry (spec D1, D12, §4.6; TC.1): the
//! server's identity, the `argv[0]` check the CLI makes before its own
//! table, and the dialect decision that sends a request's argv to the
//! native parser or to the tmux front end ([`tmux`], FE.1).
//!
//! The stub edit leaves it inert: the identity (`Model.identity`) stays
//! grofe, only a request that says `dialect: tmux` (which no CLI sends
//! yet) reaches the front end, and the front end rejects every tmux form.

pub mod tmux;

use std::ffi::OsStr;
use std::path::Path;

use crate::cmd::{self, Command};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::State;
use crate::types::{PaneId, SessionId};

/// Who a server says it is (spec D12): grofe, or tmux when started through
/// a `tmux` name or with `compat-identity tmux`. Kept on `Model.identity`,
/// so formats (`#{version}`, TC.3) and tests read it from the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Identity {
    #[default]
    Grofe,
    Tmux,
}

/// Called once in the server, after its config is applied: where TC.1
/// fixes `state.model.identity` from the starting CLI's `argv[0]` and the
/// `compat-identity` option. Does nothing yet.
pub fn server_started(_state: &mut State) {}

/// Variables a new or respawned pane of `session` gets from the server's
/// identity (TC.1: `TMUX`, `TMUX_PANE`, `TERM_PROGRAM`), applied over the
/// session's environment. None yet.
pub fn pane_env(_state: &State, _session: SessionId, _pane: PaneId) -> Vec<(String, String)> {
    Vec::new()
}

/// Whether the CLI runs under the name `tmux`: `argv[0]`'s basename is
/// exactly `tmux` (a symlink on `PATH`, spec D1).
pub fn invoked_as_tmux(argv0: &OsStr) -> bool {
    Path::new(argv0)
        .file_name()
        .is_some_and(|name| name == "tmux")
}

/// The commands a request's argv means: split at `;` and parsed natively,
/// or, for a tmux-dialect request, translated by the front end.
pub fn parse_request(ctx: &ClientCtx, argv: &[String]) -> Result<Vec<Command>, String> {
    match ctx.dialect {
        Dialect::Grofe => cmd::parse_sequence(argv).map_err(|e| e.to_string()),
        Dialect::Tmux => tmux::dispatch(argv, ctx).map_err(|e| e.to_string()),
    }
}

/// One command whose argv is already split (a binding, the prompt, the
/// mouse), in the dialect it was stored in: `run_binding` sets
/// `ctx.dialect` from the binding (spec §4.5), never from the client that
/// pressed the key.
pub fn parse_split(ctx: &ClientCtx, argv: &[String]) -> Result<Vec<Command>, String> {
    match ctx.dialect {
        Dialect::Grofe => cmd::parse(argv).map(|c| vec![c]).map_err(|e| e.to_string()),
        Dialect::Tmux => tmux::dispatch(argv, ctx).map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{KillServer, ListSessions};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn ctx(dialect: Dialect) -> ClientCtx {
        ClientCtx {
            dialect,
            ..crate::server::test_ctx()
        }
    }

    /// Fix round 1: the identity lives on the model, grofe until TC.1
    /// fixes it, and a test can set it.
    #[test]
    fn identity_is_on_the_model_and_starts_as_grofe() {
        let mut state = crate::server::test_state();
        assert_eq!(state.model.identity, Identity::Grofe);
        server_started(&mut state);
        assert_eq!(state.model.identity, Identity::Grofe);
        state.model.identity = Identity::Tmux;
        assert_eq!(state.model.identity, Identity::Tmux);
    }

    #[test]
    fn only_the_basename_tmux_is_tmux() {
        for yes in ["tmux", "/usr/local/bin/tmux", "./tmux"] {
            assert!(invoked_as_tmux(OsStr::new(yes)), "{yes}");
        }
        for no in [
            "grofe",
            "/bin/grofe",
            "tmuxx",
            "tmux-3.7b",
            "/tmux/grofe",
            "",
        ] {
            assert!(!invoked_as_tmux(OsStr::new(no)), "{no}");
        }
    }

    #[test]
    fn grofe_requests_parse_natively_and_tmux_ones_are_refused() {
        let got = parse_request(&ctx(Dialect::Grofe), &argv(&["ls", ";", "kill-server"]));
        assert!(got.is_err(), "ls is not native: {got:?}");
        let got = parse_request(
            &ctx(Dialect::Grofe),
            &argv(&["list-sessions", ";", "kill-server"]),
        );
        assert_eq!(
            got,
            Ok(vec![
                Command::ListSessions(ListSessions::default()),
                Command::KillServer(KillServer),
            ])
        );
        let err = parse_request(&ctx(Dialect::Tmux), &argv(&["list-sessions"])).unwrap_err();
        assert_eq!(err, "tmux front end is not implemented: list-sessions");
        let got = parse_split(&ctx(Dialect::Grofe), &argv(&["kill-server"]));
        assert_eq!(got, Ok(vec![Command::KillServer(KillServer)]));
        let err = parse_split(&ctx(Dialect::Tmux), &argv(&["kill-server"])).unwrap_err();
        assert_eq!(err, "tmux front end is not implemented: kill-server");
    }
}
