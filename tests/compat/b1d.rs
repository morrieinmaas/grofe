//! B1d: tmux's stock bindings that need the marked pane, menu positions,
//! copy-mode flags and the clock, each against a golden recorded from tmux
//! 3.7b (`golden/b1d/`).

use crate::tp::check;

#[test]
fn the_marked_pane_matches_its_golden() {
    check("b1d", "marked", &[]);
}
