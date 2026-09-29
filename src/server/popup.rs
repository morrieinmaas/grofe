//! Popups (TC.6, spec §4.5 "Overlays"): `display-popup` and `new-pane`, a
//! command in its own pty drawn over one client's frame ([`PopupOverlay`])
//! and taking that client's input while open ([`PopupHook`]). A popup is
//! not a pane in the model. Its pty's events reach the actor as
//! [`super::Job`]s.
//!
//! The stub edit leaves it inert: no popup ever opens.

use std::time::Instant;

use crate::server::State;
use crate::server::input::{Input, InputHook, Routed};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::ClientId;

/// Open popups, per client (TC.6). Lives on `State.popups`.
#[derive(Default)]
pub struct Popups {}

/// Takes a client's input while its popup is open (TC.6). Answers `None`.
pub struct PopupHook;

impl InputHook for PopupHook {
    fn on_input(&self, _: &mut State, _: ClientId, _: &Input, _: Instant) -> Option<Routed> {
        None
    }
}

/// Draws a client's popup (TC.6). Draws nothing.
pub struct PopupOverlay;

impl Overlay for PopupOverlay {
    fn draw(&self, _: &State, _: ClientId, _: &OverlayCtx, _: &mut Screen) {}
}

/// Client `id` went away: its popup closes (TC.6). Does nothing yet.
pub fn client_gone(_state: &mut State, _id: ClientId) {}
