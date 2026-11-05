//! Pane resumes (spec §6, T4.2): the argv that brings a pane's process
//! back, bound to the process it was set for, and cleared when that
//! process is gone.
//!
//! Stub edit for M4: the binding type the model holds and the hook every
//! spawn and respawn calls, a no-op until T4.2 fills it.

use crate::server::model::Pane;

/// The process a resume or expectation belongs to: pid plus start time, so
/// a reused pid does not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub pid: i32,
    pub start_time: u64,
}

/// Called after every spawn and respawn: a resume from the spawn command
/// binds to the pane's root process (T4.2).
pub fn bind_root(_pane: &mut Pane) {}
