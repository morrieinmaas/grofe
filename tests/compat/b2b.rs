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

#[test]
fn pipe_pane_sends_the_panes_output_to_a_command() {
    check("b2b", "pipe-pane", &[]);
}

#[test]
fn pipe_pane_input_types_into_the_pane() {
    check("b2b", "pipe-pane-in", &[]);
}

#[test]
fn pipe_pane_refuses_a_dead_pane_and_survives_a_respawn() {
    check("b2b", "pipe-pane-dead", &[]);
}

#[test]
fn unbind_key_q_stays_quiet_and_a_drops_the_table() {
    check("b2b", "unbind-q", &[]);
}

#[test]
fn confirm_before_y_c_and_b_match() {
    check("b2b", "confirm-y", &[]);
}

#[test]
fn capture_pane_n_and_t_keep_and_drop_blanks_as_tmux_does() {
    check("b2b", "capture-flags", &[]);
}

#[test]
fn display_message_v_traces_the_expansion_and_l_shows_the_text_as_it_is() {
    check("b2b", "display-v", &[]);
}

#[test]
fn display_message_v_traces_modifiers_too() {
    check("b2b", "display-v2", &[]);
}

#[test]
fn display_message_v_traces_tmux_s_own_format_language() {
    check("b2b", "display-v3", &[]);
}

#[test]
fn an_incremental_prompt_searches_again_on_the_keys_tmux_does() {
    check("b2b", "emacs-isearch-keys", &[]);
}

#[test]
fn pipe_commands_expand_tmux_s_short_forms() {
    check("b2b", "pipe-forms", &[]);
}
