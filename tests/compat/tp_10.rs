//! TP.10: tmux-claude-session-manager, its own scripts unchanged, on grofe
//! run as `tmux` (plan M3.7), against `golden/tp_10/claude`, recorded from
//! tmux 3.7b with canopy's vendored copy loaded as canopy's loader loads
//! it:
//!
//! 1. prefix a makes the agent session (`claude-<hash>`, running a
//!    stand-in agent) and shows it in a `display-popup -E` running `tmux
//!    attach-session`;
//! 2. prefix d in the popup detaches the nested client: the popup closes
//!    and main stays attached;
//! 3. the plugin's own guard: from a session whose name has the session
//!    prefix, prefix a makes nothing;
//! 4. prefix A opens the picker popup, which without fzf shows the
//!    plugin's own message (the golden container has no fzf, and neither
//!    may the replay's `PATH`);
//! 5. a bell in the agent session runs the plugin's `alert-bell` hook,
//!    which forwards it to the origin window: `window_flags` shows `!`.
//!
//! The brief's last case assumed a nested attach to the popup's own
//! session is refused. tmux 3.7b attaches instead (its nesting check
//! refuses only a client on one of its panes' terminals), and by the
//! coordinator's ruling grofe does the same in the tmux dialect:
//! `golden/tp_10/claude`'s last steps, and `popup-own-session`, which also
//! detaches the nested client again (no hang, no recursion: the window
//! shrinks to the popup's size and back). `smaller-window` pins the edge
//! tmux draws round a window smaller than its client, which that shows.

use std::path::Path;

use crate::tp::check;

#[test]
fn claude_session_manager_matches_its_golden() {
    // The replay's PATH, `fe_1::grofe_cli`'s: with an fzf on it the picker
    // would run for real (and write a cache under $TMPDIR).
    if let Some(dir) = ["/usr/local/bin", "/usr/bin", "/bin"]
        .iter()
        .find(|d| Path::new(d).join("fzf").exists())
    {
        eprintln!("SKIPPED tp_10: {dir}/fzf would run the picker the golden recorded without fzf");
        return;
    }
    check("tp_10", "claude", &[]);
}

#[test]
fn a_popup_attaching_to_its_own_session_matches_tmux() {
    check("tp_10", "popup-own-session", &[]);
}

#[test]
fn a_window_smaller_than_its_client_has_tmux_s_edge() {
    check("tp_10", "smaller-window", &[]);
}
