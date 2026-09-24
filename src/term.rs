//! Inner `TERM` resolution for spawned panes (spec D7, decided in T0.4).
//!
//! Default is `screen-256color`, not `tmux-256color`. `tmux-256color`'s
//! terminfo entry adds real capabilities over `screen-256color` (italics,
//! strikeout, invisible text, the `Smulx` extended underline styles, OSC 52
//! clipboard, focus reporting, cursor colour, window-title relay via
//! `tsl`/`fsl`/`dsl`), and `alacritty_terminal` 0.26 parses every one of
//! them into `Term` state. But D7's bar for switching is that every extra
//! capability works *end to end*, through the M0 single-pane spike's own
//! render path, and it does not: the spike's `render()` collapses every
//! underline style (`Flags::ALL_UNDERLINES`) to plain SGR `4`, so a curly or
//! dashed underline drawn via `Smulx` reaches the outer terminal as a
//! regular underline. Several non-SGR extras (window title, OSC 52 paste,
//! cursor colour, focus events) are not forwarded to the outer terminal at
//! all yet: the M2 renderer (T2.2) emits underline styles and M3 passthrough
//! (T3.3) forwards title, OSC 52, cursor colour and focus.
//! Since the bar is not met, grofe keeps advertising the smaller, honest
//! `screen-256color`. See docs/spike-notes.md's "TERM decision" section for
//! the full capability-by-capability evidence.

use std::path::PathBuf;

/// Panes get truecolor support regardless of the base terminfo entry.
pub const COLORTERM: &str = "truecolor";

/// D7's chosen default. Also the "safe fallback" a three-tier scheme would
/// name separately; here they are the same entry, so resolution only has
/// two real tiers (see [`resolve_from`]).
const DEFAULT_TERM: &str = "screen-256color";

/// Used when even `DEFAULT_TERM`'s terminfo entry is missing on this host.
/// It advertises more than `screen-256color` does and exists only for hosts
/// lacking both `screen-256color` and `tmux-256color` (only the former is
/// checked; ncurses ships the two together).
const LAST_RESORT_TERM: &str = "xterm-256color";

/// `TERM` grofe should export to a freshly spawned pane: `DEFAULT_TERM` if
/// its compiled terminfo entry exists on this host, else `LAST_RESORT_TERM`.
/// It walks the filesystem, so callers should resolve once at server start
/// and reuse the result for every spawn.
pub fn resolve_term() -> &'static str {
    resolve_from(&search_dirs())
}

/// [`resolve_term`]'s logic over an explicit candidate dir list, so it is
/// testable without touching the real host's terminfo database.
pub fn resolve_from(dirs: &[PathBuf]) -> &'static str {
    if entry_exists(dirs, DEFAULT_TERM) {
        DEFAULT_TERM
    } else {
        LAST_RESORT_TERM
    }
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
/// system locations (including Homebrew's keg-only ncurses on macOS).
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
            "/opt/homebrew/opt/ncurses/share/terminfo",
        ]
        .map(PathBuf::from),
    );
    dirs
}
