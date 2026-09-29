//! Menus (TC.7, spec §4.5 "Overlays"): `display-menu` at a caller's
//! position, nested menus, `display-panes` and `confirm-before`, drawn
//! over one client's frame ([`MenuOverlay`]) and taking its keys while
//! open ([`MenuHook`]); and the grofe aliases config defines.
//!
//! The stub edit leaves it inert: no menu ever opens, no alias exists.

use std::time::Instant;

use crate::config::Resolved;
use crate::server::State;
use crate::server::input::{Input, InputHook, Routed};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::ClientId;

/// Open menus, per client, and the alias table (TC.7). Lives on
/// `State.menus`.
#[derive(Default)]
pub struct Menus {}

/// Takes a client's keys while its menu is open (TC.7). Answers `None`.
pub struct MenuHook;

impl InputHook for MenuHook {
    fn on_input(&self, _: &mut State, _: ClientId, _: &Input, _: Instant) -> Option<Routed> {
        None
    }
}

/// Draws a client's menu (TC.7). Draws nothing.
pub struct MenuOverlay;

impl Overlay for MenuOverlay {
    fn draw(&self, _: &State, _: ClientId, _: &OverlayCtx, _: &mut Screen) {}
}

/// Applies the config's aliases (`[aliases]`), at start and on each
/// `reload`, after its options (TC.7). Does nothing yet.
pub fn configure(_state: &mut State, _config: &Resolved) {}

/// Client `id` went away: its menu closes (TC.7). Does nothing yet.
pub fn client_gone(_state: &mut State, _id: ClientId) {}
