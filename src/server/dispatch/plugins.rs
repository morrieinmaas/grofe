//! `list-plugins` and `plugin-restart` (T6.3, spec §7.4).

use crate::cmd::{ListPlugins, PluginRestart};
use crate::protocol::ClientCtx;
use crate::server::{State, plugins};

pub fn list_plugins(
    state: &mut State,
    _ctx: &ClientCtx,
    _c: ListPlugins,
) -> Result<String, String> {
    Ok(plugins::list(state))
}

pub fn plugin_restart(
    state: &mut State,
    _ctx: &ClientCtx,
    c: PluginRestart,
) -> Result<String, String> {
    plugins::restart(state, &c.name)?;
    Ok(String::new())
}
