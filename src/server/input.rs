//! decode client bytes, key tables, encode for a pane's InputModes.
//!
//! T2.0 lands the contract types; T2.3 fills in decoding, key tables and
//! encoding (spec §4.7).

use crate::types::ClientId;

/// A decoded key press. T2.3 may add [`KeyCode`] variants; the field names
/// are the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

/// The key itself, without modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// `F(1)` is F1.
    F(u8),
}

/// Modifiers held with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_: bool,
}

/// What a key turned into after routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// A bound command's argv, for dispatch.
    Command(Vec<String>),
    /// Bytes for the active pane, encoded for its `InputModes`.
    ToPane(Vec<u8>),
    /// Eaten (a prefix key, an overlay's key).
    Consumed,
}

/// A hook that sees keys before the key tables (M3 overlays and prompts
/// register one through T2.5a's API). `None` passes the key on.
pub trait InputHook {
    fn on_key(&mut self, key: &Key, client: ClientId) -> Option<Routed>;
}
