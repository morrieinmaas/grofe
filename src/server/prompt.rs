//! The command prompt (`prefix :`, `command-prompt`) and `run-shell`
//! (T3.4). T3.0 lands the seams: the state on [`crate::server::State`], the
//! hook and overlay registered first in `register_builtins`, and the
//! handlers the dispatch arms call. Until T3.4 each is inert: the hook
//! passes every input on, the overlay draws nothing, the commands answer
//! `not implemented`.

use std::time::Instant;

use crate::cmd::{CommandPrompt, RunShell};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::dispatch::not_implemented;
use crate::server::input::{Input, InputHook, Routed};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::ClientId;

/// Every client's open prompt and the server-wide history (T3.4).
#[derive(Debug, Default)]
pub struct Prompts {}

/// Eats a client's input while its prompt is open (T3.4).
pub struct PromptHook;

impl InputHook for PromptHook {
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

/// Draws a client's open prompt on its status row (T3.4).
pub struct PromptOverlay;

impl Overlay for PromptOverlay {
    fn draw(&self, _state: &State, _client: ClientId, _cx: &OverlayCtx, _screen: &mut Screen) {}
}

/// `command-prompt` (R24: `-p`, `-I`, `%%`).
pub fn command_prompt(
    _state: &mut State,
    _ctx: &ClientCtx,
    _c: CommandPrompt,
) -> Result<String, String> {
    not_implemented("command-prompt")
}

/// `run-shell` (R23): off the actor, the CLI's reply deferred through
/// [`crate::server::dispatch::defer_reply`].
pub fn run_shell(_state: &mut State, _ctx: &ClientCtx, _c: RunShell) -> Result<String, String> {
    not_implemented("run-shell")
}
