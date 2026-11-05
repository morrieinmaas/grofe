//! The session snapshot (spec §6): the schema `state.json` holds, and its
//! reader and atomic writer. T4.1 builds snapshots from the model and
//! writes them on change; T4.3 and T4.4 restore from them.
//!
//! Stub edit for M4: the types are the contract (names binding), with
//! serde derives and no logic; [`read`] and [`write_atomic`] fail until
//! T4.1 implements them.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::server::layout::LayoutGeometry;

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

fn not_yet(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{what} is not implemented yet"),
    )
}

/// Reads the snapshot at `path` (T4.1).
pub fn read(_path: &Path) -> Result<Snapshot, SnapshotError> {
    Err(SnapshotError::Io(not_yet("reading a snapshot")))
}

/// Writes `snap` to `path` atomically, mode `0600` (T4.1).
pub fn write_atomic(_path: &Path, _snap: &Snapshot) -> io::Result<()> {
    Err(not_yet("writing a snapshot"))
}
