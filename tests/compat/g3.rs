//! G3: sixel images, replayed on grofe run as `tmux` with its `graphics`
//! option on, against `golden/g3/`, recorded from a tmux 3.7b built with
//! `--enable-sixel` (the container's `tmux-sixel`; the default build has no
//! sixel and ignores them). The scenarios' client reports no cell pixels,
//! so tmux draws its text stand-in for each image, which is what the
//! goldens show: where the image is, what freed it, where the cursor went
//! and what scrolled.

use crate::tp::check;

#[test]
fn an_image_is_stored_at_the_cursor_and_the_cursor_goes_below_it() {
    check("g3", "fit", &[]);
}

#[test]
fn a_clear_frees_the_image() {
    check("g3", "clear", &[]);
}

#[test]
fn text_over_the_image_frees_it() {
    check("g3", "overwrite", &[]);
}

#[test]
fn scrolling_crops_the_image_and_it_leaves_at_the_top() {
    check("g3", "scroll", &[]);
}

#[test]
fn an_image_that_does_not_fit_below_the_cursor_scrolls_the_screen() {
    check("g3", "bottom", &[]);
}

#[test]
fn an_image_is_cut_to_the_pane() {
    check("g3", "wide", &[]);
    check("g3", "tall", &[]);
}
