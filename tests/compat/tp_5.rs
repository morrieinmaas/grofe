//! TP.5: tmux-which-key, its own script unchanged, on grofe run as `tmux`
//! (plan M3.7). `golden/tp_5/which-key` loads canopy's vendored copy as
//! canopy's loader does (`@tmux-which-key-disable-autobuild`, then a
//! `run-shell` of `plugin.sh.tmux`, which ends in a `source-file` of its
//! tmux-syntax `init.tmux`) and walks the menu prefix Space opens,
//! recorded from tmux 3.7b; grofe's replay must match it step by step:
//!
//! 1. prefix Space opens the plugin's root menu, its items and order the
//!    golden's (the `@wk_menu_root` group layout the plugin ships, which
//!    canopy loads as built: no `@wk_menu_*` option of canopy's own);
//! 2. a group item (`+Windows`) opens that group's menu in its place;
//! 3. closing it closes everything: the keys after it reach the pane;
//! 4. a leaf item (`Last window`, Tab) runs its command, `last-window`.
//!
//! The golden closes the menus with `q`, since the container's tmux holds
//! a lone Escape back (see the scenario). tmux 3.7b on a private socket
//! outside it closes the which-key menu on Escape and the keys after reach
//! the pane, so the Escape test replays the same walk with Escape on grofe
//! against the same screens.

use crate::tp::{check, check_with};

#[test]
fn which_key_matches_its_golden() {
    check("tp_5", "which-key", &[]);
}

#[test]
fn escape_closes_the_menu_as_q_does() {
    let escape = |text: &str| text.replace("keys q\n", "keys Escape\n");
    let got = check_with("tp_5", "which-key", &[], escape, escape).transcript;
    assert_eq!(got.matches("keys Escape\n").count(), 2, "{got}");
}
