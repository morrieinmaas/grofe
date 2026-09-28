//! Inner `TERM` resolution for spawned panes (spec D7).
//!
//! Default is `tmux-256color`. T0.4 kept `screen-256color` because not every
//! capability `tmux-256color`'s terminfo entry adds (italics, strikeout,
//! invisible text, the `Smulx` extended underline styles, OSC 52
//! clipboard, focus reporting, cursor colour and style, window-title relay
//! via `tsl`/`fsl`/`dsl`) survived the M0 spike's render path. T3.6 re-ran
//! every one of them through grofe's real render path (pane, server,
//! attach client, reconstructed outer terminal) on macOS and Linux, and
//! all pass (`tests/acceptance/t3_6.rs`, the `term_rerun_*` tests), so
//! D7's bar is met. See docs/spike-notes.md, "TERM decision" and "T3.6
//! re-run", for the evidence.

use std::path::PathBuf;

/// Panes get truecolor support regardless of the base terminfo entry.
pub const COLORTERM: &str = "truecolor";

/// D7's chosen default (T3.6).
const DEFAULT_TERM: &str = "tmux-256color";

/// The safe fallback when this host has no `tmux-256color` entry (an older
/// ncurses, or a minimal image): smaller, but everything it advertises
/// works.
const FALLBACK_TERM: &str = "screen-256color";

/// Used when neither entry exists on this host. It advertises more than
/// `screen-256color` does and exists only for hosts lacking both.
const LAST_RESORT_TERM: &str = "xterm-256color";

/// `TERM` grofe should export to a freshly spawned pane: the first of
/// `DEFAULT_TERM`, `FALLBACK_TERM` whose compiled terminfo entry exists on
/// this host, else `LAST_RESORT_TERM`.
/// It walks the filesystem, so callers should resolve once at server start
/// and reuse the result for every spawn.
pub fn resolve_term() -> &'static str {
    resolve_from(&search_dirs())
}

/// [`resolve_term`]'s logic over an explicit candidate dir list, so it is
/// testable without touching the real host's terminfo database.
pub fn resolve_from(dirs: &[PathBuf]) -> &'static str {
    [DEFAULT_TERM, FALLBACK_TERM]
        .into_iter()
        .find(|term| entry_exists(dirs, term))
        .unwrap_or(LAST_RESORT_TERM)
}

/// Whether a compiled terminfo entry for `term` exists under any of `dirs`,
/// checking both directory layouts terminfo databases use: a first-letter
/// subdirectory (`s/screen-256color`) and the two-digit hex subdirectory
/// macOS's ncurses uses (`73/screen-256color`). Pure filesystem check, no
/// process spawn.
pub fn entry_exists(dirs: &[PathBuf], term: &str) -> bool {
    let Some(first) = term.chars().next() else {
        return false;
    };
    let hex_dir = format!("{:02x}", first as u32);
    dirs.iter().any(|dir| {
        dir.join(first.to_string()).join(term).is_file() || dir.join(&hex_dir).join(term).is_file()
    })
}

/// Standard terminfo search locations, most to least specific: `$TERMINFO`,
/// `~/.terminfo`, each `$TERMINFO_DIRS` entry, then the usual compiled-in
/// system locations. Homebrew's keg-only ncurses is not searched: a pane
/// must get a `TERM` the system's own ncurses tools resolve.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("TERMINFO") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".terminfo"));
    }
    if let Some(list) = std::env::var_os("TERMINFO_DIRS") {
        dirs.extend(std::env::split_paths(&list).filter(|d| !d.as_os_str().is_empty()));
    }
    dirs.extend(
        [
            "/etc/terminfo",
            "/lib/terminfo",
            "/usr/share/terminfo",
            "/usr/lib/terminfo",
            "/usr/local/share/terminfo",
        ]
        .map(PathBuf::from),
    );
    dirs
}
