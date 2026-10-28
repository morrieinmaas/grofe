//! B1e: `link-window`, `unlink-window` and `new-session -t` (session
//! groups), against what tmux 3.7b did (`golden/b1e`).

use crate::tp::check;

#[test]
fn link_window_and_unlink_window_share_a_window_between_sessions() {
    check("b1e", "links", &[]);
}

#[test]
fn a_session_group_shares_one_window_list() {
    check("b1e", "groups", &[]);
}

#[test]
fn move_window_links_then_unlinks_and_swap_window_trades_places() {
    check("b1e", "moves", &[]);
}

#[test]
fn a_client_sees_a_linked_window_under_its_own_session_index() {
    check("b1e", "status", &[]);
}

#[test]
fn a_group_counts_the_clients_of_all_its_members() {
    check("b1e", "group-clients", &[]);
}

#[test]
fn kills_reach_every_session_holding_the_window() {
    check("b1e", "kills", &[]);
}

#[test]
fn link_and_move_allocate_indexes_as_tmux_does() {
    check("b1e", "index", &[]);
}

#[test]
fn a_pane_in_a_linked_window_answers_for_the_session_in_play() {
    check("b1e", "panes", &[]);
}

#[test]
fn swap_window_d_keeps_the_current_windows() {
    check("b1e", "swapd", &[]);
}

#[test]
fn group_members_follow_each_other_through_every_window_change() {
    check("b1e", "groups2", &[]);
}
