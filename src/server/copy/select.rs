//! Selection in copy mode (TM.2): `begin-selection`, `select-line`,
//! `rectangle-toggle`, `other-end` and the copies. Copy mode's motions
//! (`super`) call these hooks where tmux 3.7b's `window-copy.c` calls its
//! own; until TM.2 fills them there is no selection.

use super::{Action, Ctx, Mode};

/// tmux's `window_copy_update_selection`: the selection's moving end
/// follows the cursor.
pub fn update_selection(_mode: &mut Mode) {}

/// tmux's `window_copy_other_end`.
pub fn other_end(_mode: &mut Mode) {}

/// tmux's `window_copy_clear_selection`.
pub fn clear_selection(mode: &mut Mode) {
    mode.selecting = false;
}

/// A selection command, or `None` if `name` is not one.
pub fn command(_mode: &mut Mode, _name: &str, _args: &[String], _ctx: &Ctx<'_>) -> Option<Action> {
    None
}

/// The selection's format variables (`selection_present` and the rest).
pub fn format_var(_mode: &Mode, _name: &str) -> Option<String> {
    None
}
