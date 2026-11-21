//! The session snapshot (spec §6): the schema `state.json` holds, and its
//! reader and atomic writer. T4.1 builds snapshots from the model and
//! writes them on change; T4.3 and T4.4 restore from them.
//!
//! The types are the contract (names binding).
//!
//! **The writer.** Registered by one call in the actor loop ([`track`]),
//! which starts it the first time. It never depends on a client:
//!
//! - every second a job on the actor samples each pane's cwd (the
//!   foreground process's, as `#{pane_current_path}`), builds the snapshot
//!   from the model and hands it to a writer thread, which writes it only
//!   when its bytes (all but `written_at`) differ from the last write, so
//!   an idle server leaves the file alone;
//! - after every other actor message [`track`] rebuilds the snapshot in
//!   memory (no I/O), so a termination signal finds the state as it was;
//! - `kill-server` (any exit with sessions left) writes the model as it
//!   stands, synchronously, and SIGTERM, SIGHUP and SIGINT write the last
//!   state built before the signal; each then **freezes** the writer, so
//!   the panes dying and sessions closing after the signal never reach the
//!   file. The signal is seen in its handler
//!   ([`terminating`](signals::terminating)), before the runtime delivers
//!   it. A pane exit it caused can still reach the actor before the
//!   handler runs (another thread runs first), so the state from before
//!   a pane died of a signal is kept for [`LOSS_HOLD`](hold::LOSS_HOLD),
//!   and a signal in that time writes it;
//! - an empty state is written only once the model had sessions and lost
//!   them with no termination signal seen (the last pane exiting, or the
//!   last session killed): an empty server that never had a session never
//!   overwrites a snapshot.
//!
//! `--persist` is a sticky mark on an option (`Options::set`); options
//! with it are saved at every scope.

mod build;
mod file;
mod hold;
mod signals;
mod writer;

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::server::layout::LayoutGeometry;

pub use build::build;
pub(crate) use file::rfc3339;
pub use file::{read, write_atomic};
pub(crate) use writer::Writer;
pub use writer::{pane_killed, pending_panes, track};

/// The `version` this grofe writes and understands.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Everything a restore needs, and nothing secret: no environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// RFC 3339.
    pub written_at: String,
    pub grofe_version: String,
    /// Global options, `--persist` only.
    pub options: Vec<OptionSnap>,
    pub sessions: Vec<SessionSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnap {
    pub name: String,
    /// No environment: spec §6.
    pub cwd: PathBuf,
    /// The active window's index.
    pub active_window: u32,
    pub options: Vec<OptionSnap>,
    pub windows: Vec<WindowSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSnap {
    pub index: u32,
    pub name: String,
    pub automatic_rename: bool,
    pub cols: u16,
    pub rows: u16,
    /// TC.8's geometry, cells keyed by pane `uid`.
    pub layout: LayoutGeometry<String>,
    /// The active pane's `uid`.
    pub active_pane: String,
    pub zoomed: bool,
    pub options: Vec<OptionSnap>,
    pub panes: Vec<PaneSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnap {
    pub uid: String,
    pub cwd: PathBuf,
    pub argv: Option<Vec<String>>,
    pub resume: Option<Vec<String>>,
    /// `--persist` only.
    pub options: Vec<OptionSnap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionSnap {
    pub name: String,
    pub value: String,
}

/// Why a snapshot could not be read.
#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    /// A `version` this grofe does not know.
    UnknownVersion(u32),
    /// The file does not parse as a snapshot.
    Corrupt(String),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "{e}"),
            SnapshotError::UnknownVersion(v) => write!(f, "unknown snapshot version {v}"),
            SnapshotError::Corrupt(e) => write!(f, "unreadable snapshot: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

#[cfg(test)]
fn sample() -> Snapshot {
    Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: rfc3339(std::time::SystemTime::now()),
        grofe_version: "0.0.0".to_string(),
        options: vec![OptionSnap {
            name: "@g".to_string(),
            value: "1".to_string(),
        }],
        sessions: Vec::new(),
    }
}
