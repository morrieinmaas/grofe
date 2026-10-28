//! B2b: the stock emacs `copy-mode` table, incremental search, the
//! remaining copy-mode commands, `pipe-pane` and the small flags, each
//! against a golden recorded from tmux 3.7b (`golden/b2b/`).

use crate::tp::check;

#[test]
fn list_keys_prints_the_stock_emacs_table() {
    check("b2b", "emacs-keys", &[]);
}

#[test]
fn emacs_motions_move_the_cursor_as_tmux_does() {
    check("b2b", "emacs-move", &[]);
}

#[test]
fn emacs_paging_and_recentring_match() {
    check("b2b", "emacs-scroll", &[]);
}

#[test]
fn emacs_arrow_and_page_keys_match() {
    check("b2b", "emacs-keypad", &[]);
}

#[test]
fn emacs_selection_and_copy_keys_match() {
    check("b2b", "emacs-select", &[]);
}

#[test]
fn emacs_marks_and_repeats_match() {
    check("b2b", "emacs-marks", &[]);
}

#[test]
fn emacs_prompts_and_counts_match() {
    check("b2b", "emacs-prompts", &[]);
}

#[test]
fn incremental_search_through_the_prompt_matches() {
    check("b2b", "emacs-isearch", &[]);
}

#[test]
fn the_incremental_search_commands_match() {
    check("b2b", "emacs-isearch-cmd", &[]);
}

#[test]
fn the_and_cancel_variants_leave_only_at_the_bottom() {
    check("b2b", "copy-cancels", &[]);
}

#[test]
fn centring_and_recentring_match() {
    check("b2b", "copy-centre", &[]);
}

#[test]
fn scroll_exit_commands_match() {
    check("b2b", "copy-exit", &[]);
}

#[test]
fn selection_mode_matches() {
    check("b2b", "copy-selmode", &[]);
}

#[test]
fn pipe_commands_feed_the_selection_to_a_command() {
    check("b2b", "copy-pipe", &[]);
}
