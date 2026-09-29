//! grofe's conditional (TC.10): `if-shell`, a format form evaluated on
//! the actor and a shell form run off it. TC.10 fills the handler; it
//! answers `not implemented: if-shell` until then.

use crate::cmd::If;
use crate::protocol::ClientCtx;
use crate::server::State;

use super::not_implemented;

/// `if-shell`.
pub fn if_shell(_state: &mut State, _ctx: &ClientCtx, _c: If) -> Result<String, String> {
    not_implemented("if-shell")
}

#[cfg(test)]
mod tests {
    use crate::server::{dispatch, test_ctx, test_state};

    /// The stub edit's contract, replaced as each verb lands.
    #[test]
    fn stub_answers_not_implemented() {
        let mut state = test_state();
        let got = dispatch::run(&mut state, &test_ctx(), &["if-shell".to_string()]);
        assert_eq!(got, Err("not implemented: if-shell".to_string()));
    }
}
