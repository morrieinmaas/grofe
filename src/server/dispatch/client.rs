//! Client and session verbs (TC.9, M36-R1): `refresh-client`,
//! `show-messages`, the choosers (their UI in `server::tree`),
//! `suspend-client`, `find-window` and `wait-for`. TC.9 fills the
//! handlers; each answers `not implemented: <verb>` until then.

use crate::cmd::{
    ChooseClient, ChooseTree, FindWindow, RefreshClient, ShowMessages, SuspendClient, WaitFor,
};
use crate::protocol::ClientCtx;
use crate::server::State;

use super::not_implemented;

/// `refresh-client`.
pub fn refresh_client(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: RefreshClient,
) -> Result<String, String> {
    not_implemented("refresh-client")
}

/// `show-messages`.
pub fn show_messages(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ShowMessages,
) -> Result<String, String> {
    not_implemented("show-messages")
}

/// `choose-client`.
pub fn choose_client(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ChooseClient,
) -> Result<String, String> {
    not_implemented("choose-client")
}

/// `suspend-client`.
pub fn suspend_client(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: SuspendClient,
) -> Result<String, String> {
    not_implemented("suspend-client")
}

/// `find-window`.
pub fn find_window(_state: &mut State, _ctx: &ClientCtx, _c: FindWindow) -> Result<String, String> {
    not_implemented("find-window")
}

/// `choose-tree`.
pub fn choose_tree(_state: &mut State, _ctx: &ClientCtx, _c: ChooseTree) -> Result<String, String> {
    not_implemented("choose-tree")
}

/// `wait-for`.
pub fn wait_for(_state: &mut State, _ctx: &ClientCtx, _c: WaitFor) -> Result<String, String> {
    not_implemented("wait-for")
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    /// The stub edit's contract, replaced as each verb lands.
    #[test]
    fn stubs_answer_not_implemented() {
        let mut state = test_state();
        let ctx = test_ctx();
        for verb in [
            "refresh-client",
            "show-messages",
            "choose-client",
            "suspend-client",
            "find-window",
            "choose-tree",
            "wait-for",
        ] {
            let got = dispatch::run(&mut state, &ctx, &[verb.to_string()]);
            assert_eq!(got, Err(format!("not implemented: {verb}")));
        }
    }
}
