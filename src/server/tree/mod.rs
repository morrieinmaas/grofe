//! The choosers (TC.9): `choose-tree`, `find-window`, `choose-client` and
//! (TM.4) `choose-buffer`,
//! drawn over a pane of one client ([`TreeOverlay`]) and taking its keys
//! while open ([`TreeHook`]).
//!
//! Everything follows tmux 3.7b's `mode-tree.c`, `window-tree.c` and
//! `window-client.c`, read from its source and compared cell by cell in a
//! pty against a private server:
//!
//! - The list takes the top of the pane: each line is its key (`(0)` to
//!   `(9)`, then `(M-a)` to `(M-z)`), the tree lines (`├─> `, `└─> `,
//!   `│   `), `+ ` or `- ` for an item with children, the name (window and
//!   pane numbers right-aligned per depth), then `: ` and the item's text.
//!   The current line is drawn in `mode-style`.
//! - Below the list, a box titled with the item's name and the sort order
//!   holds a preview: a session's windows or a window's panes side by side,
//!   each with a small labelled box in its middle, or the pane itself.
//! - Enter (or a line's key) switches the client to the chosen session,
//!   window or pane (`choose-client` detaches the chosen client,
//!   `choose-buffer` pastes the chosen buffer into the pane, `paste-buffer
//!   -p`, and its `d` deletes the current buffer); `q`, Escape and C-g
//!   close it. `-Z` zooms the pane while it is open.
//!
//! grofe keeps a chooser per client, over one pane, where tmux puts the
//! pane itself in a mode that every client showing it sees.
// ponytail: tagging (`t`, `T`, C-t, `X`, `:`), kill (`x`), search (`/`,
// `n`, `N`), filter (`f`), sort (`O`, `r`), preview size (`v`), marks (`m`,
// `M`), swap (S-Up, S-Down), help and the mouse are not implemented; their
// keys do nothing. The choose-tree formats (`-F`, `-K`) and templates are
// tmux's defaults, and the styles are the defaults of `mode-style`,
// `tree-mode-preview-style`, `display-panes-colour` and
// `display-panes-active-colour`, until those are options and flags.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cmd::{self, ChooseOpts, Resolve, Target};
use crate::protocol::{ClientCtx, Dialect};
use crate::server::customize::{self, Entry, Field, Fs, Set};
use crate::server::dispatch::options::{self, FormatCtx, Level, ServerVars};
use crate::server::input::{Input, InputHook, Key, KeyCode, ROOT_TABLE, Routed};
use crate::server::model::Model;
use crate::server::pane::{CellFlags, SnapCell};
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::status::{self, Style};
use crate::server::{State, clients};
use crate::types::{ClientId, PaneId, Rect, SessionId, WindowId};

mod customize_keys;
mod drawing;
mod items;
mod keys;
mod model;
mod pointer;

use self::customize_keys::*;
pub use self::drawing::*;
pub(crate) use self::items::*;
pub use self::keys::*;
pub use self::model::*;
pub use self::pointer::*;

#[cfg(test)]
mod tests;
