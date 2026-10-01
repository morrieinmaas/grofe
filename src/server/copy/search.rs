//! Search and jumps in copy mode (TM.3): `search-forward`,
//! `search-backward`, `search-again` and the jumps
//! (`#{copy_cursor_word}` is `Mode::cursor_word`). Until TM.3 fills these hooks they do nothing.

use super::{Action, Ctx, Mode};

/// tmux's `window_copy_clear_marks`: the search matches are dropped.
pub fn clear_marks(_mode: &mut Mode) {}

/// A search or jump command, or `None` if `name` is not one.
pub fn command(_mode: &mut Mode, _name: &str, _args: &[String], _ctx: &Ctx<'_>) -> Option<Action> {
    None
}

/// The search's format variables (`search_present` and the rest).
pub fn format_var(_mode: &Mode, _name: &str) -> Option<String> {
    None
}
