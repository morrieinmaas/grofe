//! B4 rest: the leftover gaps, against goldens recorded from tmux 3.7b
//! (`golden/b4rest/`).

use crate::tp::check;

/// A click, a drag or the wheel on a pane in clock mode ends it.
#[test]
fn the_mouse_ends_clock_mode() {
    check("b4rest", "clock-mouse", &[]);
}

/// `source-file -t`: the format and command context of the file.
#[test]
fn source_file_target_is_the_context() {
    check("b4rest", "source-file-target", &[]);
}
