//! Structure verbs (TC.8, M36-R6): moving, swapping, breaking and
//! rotating panes and windows, resizing windows, layout cycling and
//! `respawn-window`. TC.8 fills the handlers; each answers
//! `not implemented: <verb>` until then.

use crate::cmd::{
    BreakPane, JoinPane, LastPane, MovePane, MoveWindow, NextLayout, ResizeWindow, RespawnPane,
    RotateWindow, SwapPane, SwapWindow,
};
use crate::protocol::ClientCtx;
use crate::server::State;

use super::not_implemented;

/// `join-pane`.
pub fn join_pane(_state: &mut State, _ctx: &ClientCtx, _c: JoinPane) -> Result<String, String> {
    not_implemented("join-pane")
}

/// `swap-pane`.
pub fn swap_pane(_state: &mut State, _ctx: &ClientCtx, _c: SwapPane) -> Result<String, String> {
    not_implemented("swap-pane")
}

/// `swap-window`.
pub fn swap_window(_state: &mut State, _ctx: &ClientCtx, _c: SwapWindow) -> Result<String, String> {
    not_implemented("swap-window")
}

/// `break-pane`.
pub fn break_pane(_state: &mut State, _ctx: &ClientCtx, _c: BreakPane) -> Result<String, String> {
    not_implemented("break-pane")
}

/// `resize-window`.
pub fn resize_window(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ResizeWindow,
) -> Result<String, String> {
    not_implemented("resize-window")
}

/// `last-pane`.
pub fn last_pane(_state: &mut State, _ctx: &ClientCtx, _c: LastPane) -> Result<String, String> {
    not_implemented("last-pane")
}

/// `rotate-window`.
pub fn rotate_window(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: RotateWindow,
) -> Result<String, String> {
    not_implemented("rotate-window")
}

/// `move-window`.
pub fn move_window(_state: &mut State, _ctx: &ClientCtx, _c: MoveWindow) -> Result<String, String> {
    not_implemented("move-window")
}

/// `move-pane`.
pub fn move_pane(_state: &mut State, _ctx: &ClientCtx, _c: MovePane) -> Result<String, String> {
    not_implemented("move-pane")
}

/// `next-layout`.
pub fn next_layout(_state: &mut State, _ctx: &ClientCtx, _c: NextLayout) -> Result<String, String> {
    not_implemented("next-layout")
}

/// `respawn-window`.
pub fn respawn_window(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: RespawnPane,
) -> Result<String, String> {
    not_implemented("respawn-window")
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
        ] {
            let got = dispatch::run(&mut state, &ctx, &[verb.to_string()]);
            assert_eq!(got, Err(format!("not implemented: {verb}")));
        }
    }
}
