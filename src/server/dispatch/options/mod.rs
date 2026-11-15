//! set-option/show-options scopes, persisted and user options, and the
//! model's format variables (spec §4.2, §4.5).
//!
//! [`Options`] is the server's one option store. A value lives at one
//! [`Owner`] (global, a session, a window or a pane); a lookup walks pane,
//! window, session, then global and takes the first value it finds. User
//! options (`@name`) are free-form strings; built-in options come from a
//! small typed table ([`BUILTINS`]) and always have a global value.
//!
//! [`FormatCtx`] implements [`FormatVars`] over the model for one pane, so
//! `-F`, `display-message -p` and `-c` expand against that pane, its
//! window and its session.

use crate::cmd::NoVars;
use crate::util::quote;
use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::num::IntErrorKind;
use std::path::{Path, PathBuf};

use crate::cmd::{
    BindKey, Command, DisplayMessage, FormatVars, Inner, ListKeys, ListPanes, ListSessions,
    ListWindows, LoopItem, LoopKind, LoopSort, NameKind, OptionScope, Reload, Resolve, SessionRef,
    SetOption, ShowOptions, SortOrder, SourceFile, Target, UnbindKey, expand, expand_time,
    split_command_string,
};
use crate::config::{self, Resolved, provenance::tilde};
use crate::event::Event;
use crate::protocol::{ClientCtx, Dialect};
use crate::server::borders::PaneStatus;
use crate::server::dispatch::sorting::{Order, Sort, by_name};
use crate::server::events::Events;
use crate::server::input::{Binding, BindingSource, Key, KeyTables, parse_key};
use crate::server::model::Model;
use crate::server::pane::procinfo::{self, ProcSample};
use crate::server::{State, clients};
use crate::types::{PaneId, SessionId, WindowId};

mod bindings;
mod formats;
mod lists;
mod reload;
mod source_file;
mod store;
mod table;

pub use self::bindings::*;
pub use self::formats::*;
pub use self::lists::*;
pub use self::reload::*;
pub use self::source_file::*;
pub use self::store::*;
pub use self::table::*;

#[cfg(test)]
mod tests;
