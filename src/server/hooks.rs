//! Event hooks (TC.5, spec §4.5 "Hooks and environment"): commands bound
//! to grofe events, run through the dispatcher with the event's target as
//! their context, plus the server environment `set-environment` keeps.
//!
//! The stub edit leaves it inert: no hook is ever bound or run.

use crate::config::Resolved;
use crate::server::State;

/// Bound hooks and the server environment (TC.5). Lives on `State.hooks`.
#[derive(Default)]
pub struct Hooks {}

/// Applies the config's hooks (`[hooks]`), at start and on each `reload`,
/// after its options (TC.5). Does nothing yet.
pub fn configure(_state: &mut State, _config: &Resolved) {}

/// Runs the hooks for what just happened. Called once after every command
/// sequence (`dispatch`, before its reply goes out) and after every pane
/// event, before their events are flushed to subscribers (TC.5). Does
/// nothing yet.
pub fn run_pending(_state: &mut State) {}
