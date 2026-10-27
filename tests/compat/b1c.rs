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

#[test]
fn layouts_resizes_and_splits_count_the_row_a_border_status_takes() {
    check("b1c", "border-status-layout", &[]);
}

#[test]
fn the_default_active_border_is_yellow_in_a_mode_and_red_when_synchronized() {
    check("b1c", "border-copy-mode", &[]);
}

#[test]
fn a_floating_pane_border_follows_pane_border_lines() {
    check("b1c", "border-floating", &[]);
}

#[test]
fn a_style_colour_answers_the_osc_default_colour_queries() {
    check("b1c", "osc-colours", &[]);
}

#[test]
fn a_grid_of_panes_joins_borders_and_marks_the_active_pane() {
    check("b1c", "border-grid", &[]);
}

#[test]
fn copy_mode_shows_the_window_style() {
    check("b1c", "window-style-copy-mode", &[]);
}
