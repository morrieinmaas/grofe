//! B1d: tmux's stock bindings that need the marked pane, menu positions,
//! copy-mode flags and the clock, each against a golden recorded from tmux
//! 3.7b (`golden/b1d/`).

use crate::tp::check;

#[test]
fn the_marked_pane_matches_its_golden() {
    check("b1d", "marked", &[]);
}

#[test]
fn the_stock_menus_open_where_tmux_opens_them() {
    check("b1d", "stock-menus", &[]);
}

#[test]
fn menu_and_popup_positions_match_their_golden() {
    check("b1d", "menu-positions", &[]);
}

#[test]
fn copy_mode_flags_match_their_golden() {
    check("b1d", "copy-flags", &[]);
}

#[test]
fn clock_mode_matches_its_golden() {
    check("b1d", "clock", &[]);
}

/// What the clock draws changes with the time, so no golden holds it: the
/// screen has the big digits' blocks (and nothing of the pane's own text)
/// until a key is typed, then the pane is back.
#[test]
fn the_clock_draws_digits_until_a_key_is_typed() {
    use crate::fe_1::grofe_cli;
    use crate::normalize::Normalizer;
    use crate::replay::{parse_scenario, replay};

    let scenario = "\
$ new-session -d -s main -x 80 -y 24 'echo pane-text; exec cat'
$ set -g status off
client 80x24 attach -t main
screen pane-text
$ clock-mode
screen ####
keys z
screen pane-text
";
    let dir = tempfile::tempdir().unwrap();
    let steps = parse_scenario(scenario).unwrap();
    let got = replay(&grofe_cli(dir.path()), &steps).normalized(&Normalizer::load());
    let screens: Vec<&str> = got.split("screen ").skip(1).collect();
    assert_eq!(screens.len(), 3, "{got}");
    assert!(screens[1].matches('#').count() >= 20, "{got}");
    assert!(!screens[1].contains("pane-text"), "{got}");
    assert!(!screens[2].contains('#'), "{got}");
}
