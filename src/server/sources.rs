//! Cached status sources and `#()` (TC.4, spec D8, §4.10): named and
//! inline commands run off the actor on their interval, one run in flight,
//! their first output line stored as a user option. A run's result comes
//! back to the actor as a [`super::Job`].
//!
//! The stub edit leaves it inert: nothing is registered, nothing runs.

use crate::config::Resolved;
use crate::server::State;

/// The registered sources, their schedules and last values (TC.4). Lives on
/// `State.sources`.
#[derive(Default)]
pub struct Sources {}

/// Applies the config's sources (`[status.sources]`), at start and on each
/// `reload`, after its options (TC.4). Does nothing yet.
pub fn configure(_state: &mut State, _config: &Resolved) {}
