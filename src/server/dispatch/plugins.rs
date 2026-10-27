//! `list-plugins` and `plugin-restart` (T6.3, spec §7.4). Stubs until T6.3
//! implements them.

use crate::cmd::{ListPlugins, PluginRestart};
use crate::protocol::ClientCtx;
use crate::server::State;

const NOT_YET: &str = "plugins are not yet supported";

pub fn list_plugins(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: ListPlugins,
) -> Result<String, String> {
    Err(NOT_YET.to_string())
}

pub fn plugin_restart(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: PluginRestart,
) -> Result<String, String> {
    Err(NOT_YET.to_string())
}
