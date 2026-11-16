//! Client input: decode the bytes an attached client sends, route keys
//! through the key tables, and encode keys for a pane's `InputModes`
//! (spec §4.7).
//!
//! The pieces, in the order T2.5a wires them:
//!
//! - [`Decoder`] turns client bytes into [`Input`]s: keys (legacy CSI and
//!   SS3 forms, kitty `CSI u`, xterm modifyOtherKeys, UTF-8, C0 controls),
//!   SGR mouse reports, focus reports, and bracketed paste as one opaque
//!   blob. A lone ESC waits `escape-time` for the rest of a sequence; the
//!   clock is passed in, never read here.
//! - [`KeyTables`] holds bindings by table name (`root`, `prefix`, custom).
//! - [`KeyState`] is one client's position in the tables (current table,
//!   repeat deadline) and routes an [`Input`] to a [`Routed`], following
//!   tmux 3.7b's `server_client_key_callback`: the prefix switches to the
//!   `prefix` table, a binding runs and returns to `root` unless it repeats,
//!   an unbound key after the prefix is dropped, and an unbound key in
//!   `root` goes to the pane.
//! - [`encode`] writes a key the way a terminal would for the pane's modes:
//!   legacy xterm/screen bytes (the pane's `TERM` is tmux-256color, whose
//!   unmodified keys are screen-256color's), or
//!   the kitty keyboard protocol for the flags the pane has pushed.
//!
//! [`InputHook`]s (the prompt, the mouse) see every input before the
//! tables; running them is the caller's job (`clients::route`), since the
//! registry lives in T2.5a's clients.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::Dialect;
use crate::server::State;
use crate::types::{ClientId, InputModes};

mod decoder;
mod defaults;
mod encoder;
mod keys;
mod state;
mod tables;

pub use self::decoder::*;
pub use self::defaults::*;
pub use self::encoder::*;
pub use self::keys::*;
pub use self::state::*;
pub use self::tables::*;

#[cfg(test)]
mod tests;
