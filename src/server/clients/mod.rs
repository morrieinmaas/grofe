//! Attached clients: the registry, per-client output with one pending
//! frame, the render scheduler, the window-size policy, attach and detach
//! (spec §4.4, §4.6, D6).
//!
//! Everything here runs on the actor. A client's connection task (in
//! `server/mod.rs`) owns the socket: it sends the client's frames to the
//! actor and writes what the actor leaves in the client's one-slot outbox.
//!
//! - **Output.** [`Outbox`] holds at most one pending frame. The actor only
//!   `try_send`s into it: when the slot is still full the new diff is
//!   dropped, the client's previous screen is forgotten, and the next frame
//!   it gets is a full redraw (spec §4.4, slow clients). A client that stops
//!   reading therefore never stalls the actor or another client.
//! - **Scheduling.** Pane output sets the pane's dirty flag and wakes the
//!   actor; [`wake`] marks the clients showing a dirty pane, commands mark
//!   every client, and [`tick`] composes dirty clients at most once per
//!   [`FRAME_INTERVAL`].
//! - **Size (D6).** Each session's windows take the size of its latest
//!   active client ([`SizeOwner`]): attaching makes a client the latest at
//!   once, input makes it the latest after [`SIZE_DEBOUNCE`], so two clients
//!   typing in turn do not make the window flap. A larger client
//!   letterboxes, a smaller one crops with a viewport that follows the
//!   active pane's cursor ([`follow`]).
//! - **Input.** Bytes are decoded per client, each input goes to the
//!   registered input hooks first, then through the client's key table;
//!   bound commands run with the client's context, other keys go to the
//!   active pane encoded for its modes.
//! - **Raw output.** [`queue_raw`] puts bytes that are not a screen change
//!   (mouse modes, passthrough, graphics) ahead of the client's next frame,
//!   in the same `Output` frame.

use crate::cmd::NoVars;
use crate::cmd::format::fnmatch;
use crate::util::quote;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use nix::libc;
use tokio::sync::{mpsc, oneshot};

use crate::cmd::{
    self, AttachSession, DetachClient, FormatVars, ListClients, LoopItem, LoopKind, LoopSort,
    NameKind, Resolve, Target,
};
use crate::protocol::{Caps, ClientCtx, Dialect, Frame};
use crate::server::borders::{Indicators, PaneLines, PaneStatus};
use crate::server::dispatch::options::{self, FormatCtx, Owner};
use crate::server::dispatch::sorting;
use crate::server::dispatch::window::automatic_rename;
use crate::server::input::{
    Decoder, Input, InputHook, KeySettings, KeyState, KeyTables, ROOT_TABLE, Routed,
    default_bindings, parse_prefix,
};
use crate::server::model::{Model, Session};
use crate::server::pane::CursorState;
use crate::server::render::{self, FrameInputs, Overlay, OverlayCtx, Screen};
use crate::server::screenimg::{self, ImageCaps, ScreenImage};
use crate::server::status::{
    self, RangeKind, StatusInputs, StatusRow, StyledLine, WindowOptions, WindowStatus,
};
use crate::server::{State, dispatch, graphics, hooks};
use crate::types::{ClientId, PaneId, Rect, SessionId, WindowId, cap_size};

mod compose;
mod geometry;
mod input;
mod lifecycle;
mod sched;

pub use self::compose::*;
pub use self::geometry::*;
pub use self::input::*;
pub use self::lifecycle::*;
pub use self::sched::*;

#[cfg(test)]
mod tests;
