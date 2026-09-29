//! Menus and confirmations (TC.7): `display-menu`, `display-panes`,
//! `confirm-before`, drawn by `server::menu`, and grofe aliases, expanded
//! before a request is parsed ([`expand_aliases`]). TC.7 fills the
//! handlers; each answers `not implemented: <verb>` until then.

use crate::cmd::{ConfirmBefore, DisplayMenu, DisplayPanes};
use crate::protocol::ClientCtx;
use crate::server::State;

use super::not_implemented;

/// `display-menu`.
pub fn display_menu(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: DisplayMenu,
) -> Result<String, String> {
    not_implemented("display-menu")
}

/// `display-panes`.
pub fn display_panes(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: DisplayPanes,
) -> Result<String, String> {
    not_implemented("display-panes")
}

/// `confirm-before`.
pub fn confirm_before(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ConfirmBefore,
) -> Result<String, String> {
    not_implemented("confirm-before")
}

/// The request's argv with grofe aliases expanded (TC.7), or `None` to
/// run it as it is. [`super::run`] and [`super::run_split`] call this
/// before parsing, for every dialect.
pub fn expand_aliases(_state: &State, _argv: &[String]) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    /// The stub edit's contract, replaced as each verb lands.
    #[test]
    fn stubs_answer_not_implemented() {
        let mut state = test_state();
        let ctx = test_ctx();
        for verb in ["display-menu", "display-panes", "confirm-before"] {
            let got = dispatch::run(&mut state, &ctx, &[verb.to_string()]);
            assert_eq!(got, Err(format!("not implemented: {verb}")));
        }
    }
}
