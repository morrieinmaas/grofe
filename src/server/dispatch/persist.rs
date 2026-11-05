//! `pane-set-resume` (T4.2) and `reboot-check` (T4.5), spec §6.
//! `reboot-check` is a stub until T4.5 implements it.

use crate::cmd::{PaneSetResume, RebootCheck, Resolve};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::resume::{self, Set};

/// `pane-set-resume [-t pane] [-u] [--expect <reason>] [-- argv...]`: the
/// logic is `server::resume`'s.
pub fn pane_set_resume(
    state: &mut State,
    ctx: &ClientCtx,
    c: PaneSetResume,
) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    let what = match (c.unset, c.expect) {
        (true, _) => Set::Clear,
        (false, Some(reason)) => Set::Expect(reason),
        (false, None) => Set::Resume(c.argv),
    };
    resume::set(state, ctx, pane, what)?;
    Ok(String::new())
}

pub fn reboot_check(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: RebootCheck,
) -> Result<String, String> {
    Err("not implemented: reboot-check".to_string())
}
