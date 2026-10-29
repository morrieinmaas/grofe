//! WP-A: view mode, the choosers, the command prompt and customize-mode,
//! against `golden/wpa/`, recorded from tmux 3.7b.

use crate::tp::check;

#[test]
fn command_output_shows_in_view_mode() {
    check("wpa", "view", &[]);
}

#[test]
fn the_command_prompt_matches_tmux() {
    check("wpa", "prompt", &[]);
}

#[test]
fn choose_tree_keys_match_tmux() {
    check("wpa", "choose-keys", &[]);
}

#[test]
fn choose_tree_templates_filters_prompts_and_tags_match_tmux() {
    check("wpa", "choose-cmds", &[]);
}

#[test]
fn choose_buffer_matches_tmux() {
    check("wpa", "choose-buffer", &[]);
}

#[test]
fn choose_tree_marks_swaps_and_folds_match_tmux() {
    check("wpa", "choose-more", &[]);
}

#[test]
fn customize_mode_matches_tmux() {
    check("wpa", "customize", &[]);
}

#[test]
fn more_binding_output_matches_tmux() {
    check("wpa", "view-more", &[]);
}

#[test]
fn customize_mode_resets_keys_and_ignores_w_on_them() {
    check("wpa", "customize-keys", &[]);
}
