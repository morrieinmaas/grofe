//! B1c: `window-style` and `window-active-style`, and the pane border
//! options (`pane-border-status`, `-format`, `-lines`, `-indicators`,
//! `-style`), against what tmux 3.7b drew (`golden/b1c`).

use crate::tp::check;

#[test]
fn window_style_colours_only_default_colour_cells() {
    check("b1c", "window-style", &[]);
}

#[test]
fn window_active_style_sets_what_it_names_and_styles_expand() {
    check("b1c", "window-style-default", &[]);
}

#[test]
fn a_border_status_takes_a_row_from_the_panes_at_the_edge() {
    check("b1c", "border-status", &[]);
}

#[test]
fn stacked_panes_share_the_border_line_with_a_title() {
    check("b1c", "border-status-stacked", &[]);
}

#[test]
fn pane_border_format_expands_per_pane_and_is_cut_to_the_pane() {
    check("b1c", "border-format", &[]);
}

#[test]
fn pane_border_lines_choose_the_glyphs() {
    check("b1c", "border-lines", &[]);
}

#[test]
fn pane_border_indicators_colour_half_a_border_or_draw_arrows() {
    check("b1c", "border-indicators", &[]);
}

#[test]
fn border_styles_follow_the_owning_pane_and_expand_formats() {
    check("b1c", "border-style", &[]);
}
