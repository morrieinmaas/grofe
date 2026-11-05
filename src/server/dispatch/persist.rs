//! `pane-set-resume` (T4.2) and `reboot-check` (T4.5), spec §6. Stubs
//! until those tasks implement them.

use crate::cmd::{PaneSetResume, RebootCheck};
use crate::protocol::ClientCtx;
use crate::server::State;

pub fn pane_set_resume(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: PaneSetResume,
) -> Result<String, String> {
    Err("not implemented: pane-set-resume".to_string())
}

pub fn reboot_check(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: RebootCheck,
) -> Result<String, String> {
    Err("not implemented: reboot-check".to_string())
}
