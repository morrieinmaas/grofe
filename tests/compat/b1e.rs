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
