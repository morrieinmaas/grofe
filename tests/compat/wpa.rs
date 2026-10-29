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
