//! PaneHandle, spawn, pre_exec.

pub mod io;
pub mod procinfo;

use std::path::PathBuf;

use crate::types::PaneId;

/// Events a pane reports to the server actor. Contract stub landed before M1
/// Wave A so the actor (T1.1b) and the pane (T1.4) can be built in parallel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// Child exited and the master drained (EOF/EIO or grace period).
    Exited(PaneId, Option<i32>),
    Title(PaneId, String),
    Bell(PaneId),
    /// OSC 52 clipboard store, already decoded.
    Clipboard(PaneId, String),
    /// Emitted from M3 (OSC 7 scanner).
    Cwd(PaneId, PathBuf),
    /// M3: one framed APC `_G` command from the scanner.
    Graphics(PaneId, Vec<u8>),
}
