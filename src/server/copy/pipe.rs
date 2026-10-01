//! Copy targets (TM.5): `copy-pipe` and its forms. Until TM.5 fills this
//! hook they do nothing.

use super::{Action, Ctx, Mode};

/// A copy-target command, or `None` if `name` is not one.
pub fn command(_mode: &mut Mode, _name: &str, _args: &[String], _ctx: &Ctx<'_>) -> Option<Action> {
    None
}
