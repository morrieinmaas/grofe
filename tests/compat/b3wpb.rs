//! B3 WP-B: `synchronize-panes`, the most recently active neighbour for
//! `select-pane -L/-R/-U/-D`, `capture-pane` into a buffer, the window and
//! pane flags grofe refused, and `default-terminal`, each against a golden
//! recorded from tmux 3.7b (`golden/b3wpb/`).

use crate::tp::check;

#[test]
fn synchronize_panes_copies_keys_to_the_panes_tmux_does() {
    check("b3wpb", "sync", &[]);
}

#[test]
fn select_pane_directions_pick_the_most_recently_active_pane() {
    check("b3wpb", "neighbour", &[]);
}

#[test]
fn capture_pane_without_p_stores_a_buffer() {
    check("b3wpb", "capture-buffer", &[]);
}

#[test]
fn select_window_last_pane_and_send_keys_flags_match() {
    check("b3wpb", "flags", &[]);
}

#[test]
fn default_terminal_sets_the_term_of_new_panes() {
    check("b3wpb", "term", &[]);
}
