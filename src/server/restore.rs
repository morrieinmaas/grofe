//! Restore from a snapshot (spec §6): T4.3 rebuilds sessions, windows and
//! panes into an empty server; T4.4 runs resumes and keeps a restored pane
//! whose resumed process exits.
//!
//! Stub edit for M4: the contract signatures, failing or doing nothing
//! until those tasks fill them.

use crate::server::State;
use crate::server::snapshot::Snapshot;
use crate::types::PaneId;

/// What a restore brought back, for the log and `restore-done`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
}

/// Why a restore did not happen; the text is what the caller reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreError(pub String);

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RestoreError {}

/// Recreates everything in `snap` (T4.3, T4.4).
pub fn restore(_state: &mut State, _snap: Snapshot) -> Result<RestoreReport, RestoreError> {
    Err(RestoreError("restore is not implemented yet".to_string()))
}

/// Called by the actor when `pane`'s process exits, before the pane is
/// reported dead or removed. `true` means this module kept the pane (T4.4
/// respawns a shell in a restored pane whose resume exited) and the usual
/// exit handling is skipped.
pub fn on_pane_exited(_state: &mut State, _pane: PaneId) -> bool {
    false
}
