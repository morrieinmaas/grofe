//! Popups (TC.6, M36-R1): `display-popup` and `new-pane`, both drawn by
//! `server::popup`. TC.6 fills the handlers; each answers
//! `not implemented: <verb>` until then.

use crate::cmd::{DisplayPopup, NewPane};
use crate::protocol::ClientCtx;
use crate::server::State;

use super::not_implemented;

/// `display-popup`.
pub fn display_popup(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: DisplayPopup,
) -> Result<String, String> {
    not_implemented("display-popup")
}

/// `new-pane`.
pub fn new_pane(_state: &mut State, _ctx: &ClientCtx, _c: NewPane) -> Result<String, String> {
    not_implemented("new-pane")
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    /// The stub edit's contract, replaced as each verb lands.
    #[test]
    fn stubs_answer_not_implemented() {
        let mut state = test_state();
        let ctx = test_ctx();
        for verb in ["display-popup", "new-pane"] {
            let got = dispatch::run(&mut state, &ctx, &[verb.to_string()]);
            assert_eq!(got, Err(format!("not implemented: {verb}")));
        }
    }
}
