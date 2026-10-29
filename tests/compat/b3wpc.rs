//! B3 WP-C: mouse, menus, popups and alerts, against goldens recorded from
//! tmux 3.7b (`golden/b3wpc/`).

use crate::tp::check;

/// The stock `root` mouse rows are bindings: listed as tmux lists them,
/// unbound one by one or all at once, and bound again.
#[test]
fn the_stock_root_mouse_rows_are_ordinary_bindings() {
    check("b3wpc", "stock-root", &[]);
}

/// The menus the stock rows open: the pane menu at the pointer, the window
/// menu on the status line, the session menu on its left end. They live
/// while the button is held.
#[test]
fn the_stock_menus_follow_the_mouse_as_tmux_does() {
    check("b3wpc", "menu-bound-mouse", &[]);
}

/// A click, a drag, the wheel and motion in a menu, with and without
/// `-O`, `-M` and a mouse event behind it.
#[test]
fn a_menu_takes_the_mouse_as_tmux_does() {
    check("b3wpc", "menu-mouse", &[]);
}

/// `menu-style`, `menu-selected-style`, `menu-border-style`,
/// `menu-border-lines` and `display-menu -s -H -S -b`.
#[test]
fn menu_styles_and_border_lines_match() {
    check("b3wpc", "menu-styles", &[]);
}

/// `display-menu -C -t -c`.
#[test]
fn display_menu_flags_match() {
    check("b3wpc", "menu-flags", &[]);
}

/// `display-panes -d -b -N -t` and the template.
#[test]
fn display_panes_flags_and_template_match() {
    check("b3wpc", "display-panes", &[]);
}

/// Activity, bell and silence alerts: window-status styles and the
/// `visual-*` messages.
#[test]
fn activity_alerts_are_styled() {
    check("b3wpc", "alerts-activity", &[]);
}

#[test]
fn bell_alerts_are_styled_and_say_so() {
    check("b3wpc", "alerts-bell", &[]);
}

#[test]
fn silence_alerts_are_styled_and_say_so() {
    check("b3wpc", "alerts-silence", &[]);
}

/// `display-popup -s -S -k -N -C -t`, `popup-style` and a popup changed
/// while it is open.
#[test]
fn popup_styles_flags_and_changes_match() {
    check("b3wpc", "popup-style", &[]);
}

/// `focus-follows-mouse`.
#[test]
fn focus_follows_mouse_matches() {
    check("b3wpc", "focus-follows-mouse", &[]);
}
