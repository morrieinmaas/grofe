//! Issue 4: the format variables grofe answers from state it keeps,
//! against `golden/fv/`, recorded from tmux 3.7b.

use crate::tp::check;

#[test]
fn format_variables_match_tmux() {
    check("fv", "variables", &[]);
}
