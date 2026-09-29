//! Event hooks and the server environment (TC.5): `set-hook`,
//! `show-hooks`, `set-environment`, `show-environment`. The hooks
//! themselves run from `server::hooks`. TC.5 fills the handlers; each
//! answers `not implemented: <verb>` until then.

use crate::cmd::{SetEnvironment, SetHook, ShowEnvironment, ShowHooks};
use crate::protocol::ClientCtx;
use crate::server::State;

use super::not_implemented;

/// `set-hook`.
pub fn set_hook(_state: &mut State, _ctx: &ClientCtx, _c: SetHook) -> Result<String, String> {
    not_implemented("set-hook")
}

/// `show-hooks`.
pub fn show_hooks(_state: &mut State, _ctx: &ClientCtx, _c: ShowHooks) -> Result<String, String> {
    not_implemented("show-hooks")
}

/// `set-environment`.
pub fn set_environment(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: SetEnvironment,
) -> Result<String, String> {
    not_implemented("set-environment")
}

/// `show-environment`.
pub fn show_environment(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ShowEnvironment,
) -> Result<String, String> {
    not_implemented("show-environment")
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
            "set-hook",
            "show-hooks",
            "set-environment",
            "show-environment",
        ] {
            let got = dispatch::run(&mut state, &ctx, &[verb.to_string()]);
            assert_eq!(got, Err(format!("not implemented: {verb}")));
        }
    }
}
