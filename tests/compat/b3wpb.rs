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

#[test]
fn send_prefix_paste_buffer_and_send_keys_reset_reach_panes_as_in_tmux() {
    check("b3wpb", "input-paths", &[]);
}

#[test]
fn a_client_paste_under_synchronize_panes_brackets_each_pane_by_its_own_mode() {
    check("b3wpb", "paste-sync", &[]);
}

/// A key bound to `send-keys -K` of itself would recurse in grofe, which
/// runs a typed key's binding at once, until the stack overflowed and the
/// server died with every session. tmux queues the keys (and spins), so no
/// golden holds this: the server stays up and says why it stopped.
#[test]
fn a_binding_that_types_itself_does_not_bring_the_server_down() {
    use crate::fe_1::grofe_cli;
    use crate::normalize::Normalizer;
    use crate::replay::{parse_scenario, replay};

    let scenario = "\
$ new-session -d -s main -x 60 -y 10 'stty -echo; cat -v'
$ bind-key -n z send-keys -K z
client 60x10 attach -t main
keys z
$ run-shell 'sleep 0.5'
$ display-message -p alive
$ show-messages
";
    let dir = tempfile::tempdir().unwrap();
    let steps = parse_scenario(scenario).unwrap();
    let got = replay(&grofe_cli(dir.path()), &steps).normalized(&Normalizer::load());
    assert!(got.contains("\nalive\n"), "{got}");
    assert!(got.contains("-K nested too deeply"), "{got}");
}
