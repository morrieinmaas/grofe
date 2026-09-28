//! Mouse: click to focus, the read-only scrollback view, forwarding to
//! panes that asked for the mouse (T3.2). T3.0 lands the seams: the state on
//! [`crate::server::State`], the hook and overlay registered after the
//! prompt's in `register_builtins`. Until T3.2 the hook passes every input
//! on (the key tables then drop mouse input, as in M2) and the overlay
//! draws nothing.

use std::time::Instant;

use crate::server::State;
use crate::server::input::{Input, InputHook, Routed};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::ClientId;

/// Per-pane scrollback view offsets (T3.2).
#[derive(Debug, Default)]
pub struct MouseState {}

/// Routes mouse input: focus, wheel, forwarding (T3.2).
pub struct MouseHook;

impl InputHook for MouseHook {
    fn on_input(
        &self,
        _state: &mut State,
        _client: ClientId,
        _input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        None
    }
}

/// Draws a scrolled-back pane's history and its `[offset/history]`
/// indicator (T3.2, R17).
pub struct ScrollbackOverlay;

impl Overlay for ScrollbackOverlay {
    fn draw(&self, _state: &State, _client: ClientId, _cx: &OverlayCtx, _screen: &mut Screen) {}
}
