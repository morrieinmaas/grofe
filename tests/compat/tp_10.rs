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
//! The brief's last case, a nested attach to the popup's own session
//! refused by TC.1's nesting rule, is not tmux 3.7b's behaviour: tmux
//! attaches, and shows main nested in the popup. grofe refuses it, so
//! those two steps are listed below as a gap (TC.1, TC.6).

use std::path::Path;

use crate::tp::check;

/// The golden's last two steps: a popup attaching to main from main's own
/// client.
const NESTED_OWN_SESSION: &str = "grofe refuses a nested attach to the popup's own session (TC.1's \
     nesting rule); tmux 3.7b attaches and shows main in the popup";

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
    check(
        "tp_10",
        "claude",
        &[
            ("screen", NESTED_OWN_SESSION),
            ("$ list-clients -F '#{session_name}'", NESTED_OWN_SESSION),
        ],
    );
}
