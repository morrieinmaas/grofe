//! The choosers (TC.9): `choose-tree`, `choose-client` and `find-window`'s
//! list, drawn over a client's pane ([`TreeOverlay`]) and taking its keys
//! while open ([`TreeHook`]).
//!
//! The stub edit leaves it inert: no chooser ever opens.

use std::time::Instant;

use crate::server::State;
use crate::server::input::{Input, InputHook, Routed};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::ClientId;

/// Open choosers, per client (TC.9). Lives on `State.trees`.
#[derive(Default)]
pub struct Trees {}

/// Takes a client's keys while its chooser is open (TC.9). Answers `None`.
pub struct TreeHook;

impl InputHook for TreeHook {
    fn on_input(&self, _: &mut State, _: ClientId, _: &Input, _: Instant) -> Option<Routed> {
        None
    }
}

/// Draws a client's chooser (TC.9). Draws nothing.
pub struct TreeOverlay;

impl Overlay for TreeOverlay {
    fn draw(&self, _: &State, _: ClientId, _: &OverlayCtx, _: &mut Screen) {}
}

/// Client `id` went away: its chooser closes (TC.9). Does nothing yet.
pub fn client_gone(_state: &mut State, _id: ClientId) {}
