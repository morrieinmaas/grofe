use super::*;
use crate::config::Ty;

pub const REMAIN_ON_EXIT: &str = "remain-on-exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    /// A whole number from 0 to `max` (tmux's bounds).
    Number {
        max: i64,
    },
    /// Free text, stored as given. Formats and styles in it are checked
    /// where they are used (the status line), not here.
    String,
    /// One of a fixed set of words (tmux's choice options).
    Choice(&'static [&'static str]),
    /// A key name or `None`, stored in tmux's canonical spelling.
    Key,
    /// A colour word, stored in tmux's spelling of it ([`colour_text`]).
    Colour,
    /// `<columns>x<rows>`, tmux's pattern `[0-9]*x[0-9]*`.
    Size,
}

/// A built-in option: its type, the scope `set`/`show` use when no scope
/// flag is given (tmux's), and its global default.
pub(super) struct Builtin {
    pub(super) name: &'static str,
    /// One line for the generated options reference.
    pub(super) doc: &'static str,
    pub(super) kind: Kind,
    pub(super) scope: OptionScope,
    pub(super) default: &'static str,
    /// The TOML key, when it is not `name` (`status.lines` for `status`).
    pub(super) toml: Option<&'static str>,
    /// The TOML type, when it is not the kind's: a choice option the TOML
    /// spells as a boolean or an integer. A bool is `on`/`off`.
    pub(super) toml_ty: Option<Ty>,
    /// tmux 3.7b's built-in default where grofe's differs (recorded: `show
    /// -g`/`-s`/`-gw` on `-f /dev/null` with a clean environment), used under
    /// tmux identity ([`Options::use_tmux_defaults`]); `status-keys` and
    /// `mode-keys` follow `$EDITOR` there. Formats are tmux's where the native
    /// language can say the same thing.
    // ponytail: `status-right`'s `#{=21:pane_title}` and the format tmux uses
    // for `pane-active-border-style` have no native spelling, so grofe's stay.
    pub(super) tmux_default: Option<&'static str>,
}

impl Builtin {
    /// A row's unset columns: `..Builtin::PLAIN` after the ones it sets.
    const PLAIN: Builtin = Builtin {
        name: "",
        doc: "",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "",
        toml: None,
        toml_ty: None,
        tmux_default: None,
    };
}

// ponytail: only the options grofe has a use for. `history-limit`,
// `base-index` and `pane-base-index` take effect through the model.
//
// The window, status and key rows use tmux's names and scopes (tmux 3.7b
// bounds). Their defaults are tmux's except where grofe differs on purpose or
// the native formats cannot say the same thing: `escape-time` 25 (spec §5.3), `automatic-rename-format`
// `#{b:pane_current_path}` (spec §4.5), and `status-right`'s pane title
// untrimmed (tmux's is `#{=21:pane_title}`; the native language has no `=`
// trim, so a title over 21 columns shows whole). The window formats are
// tmux's, `#I:#W` spelled out (TC.11, D8). tmux's escape-time is a server
// option; grofe's server scope is global.
//
// The mouse, focus and border rows use tmux's names and scopes with spec
// §5.3's defaults where grofe differs: `mouse` on (tmux off), `focus-events` on (tmux
// off), `pane-active-border-style`
// `fg=green` (tmux's is a format that also colours copy mode and
// synchronized panes). `status-position` takes `top` from TC.11.
// `set-titles` (off) and `set-titles-string` match tmux's own defaults,
// the string translated into grofe's `#{...}`-only expander (R15).
/// grofe's own options that tmux's CLI does not know (spec
/// 2026-11-05-frames.md): through the tmux dialect `set-option` and
/// `show-options` call them `invalid option`, as tmux would, while the
/// native CLI and the TOML config take them under either identity.
pub(super) const GROFE_ONLY: &[&str] = &[
    "pane-frames",
    "pane-gap",
    "pane-frame-title",
    "pane-frame-tab-ends",
    "agent-glyph-blocked",
    "agent-glyph-error",
    "agent-glyph-unseen",
    "agent-glyph-working",
    "notify-muted-glyph",
    "agent-blocked-border-style",
    "scratchpad-history",
    "message-pill",
    "message-pill-ends",
    "prompt-position",
    "stable-ssh-agent",
    "forward-browser",
    "machine-key",
    "remote-upload-paste",
    "remote-upload-max",
    "remote-forward-ports",
    "remote-reconnect",
    "remote-release-url",
    "remote-transport",
    "remote-udp-ports",
    "remote-quic-idle",
    "predictive-echo",
    "default-layout",
];

/// grofe's own list options, kept apart from [`GROFE_ONLY`] (which names
/// `BUILTINS` rows) and treated the same: tmux's CLI calls them invalid.
pub(super) const GROFE_ONLY_LISTS: &[&str] = &["copy-hint-patterns"];

/// `copy-hint-patterns`' default: regular expressions, tried in order, for
/// copy mode's hints (`copy-mode -h`). A pattern with a capture group
/// hints that group, so a leading boundary can stay out of the match.
pub(super) const HINT_PATTERNS: &[&str] = &[
    // URLs, less trailing punctuation.
    r#"(?:https?|ftp|file|ssh|git)://[^\s<>"'`()\[\]{}]*[^\s<>"'`()\[\]{}.,;:!?]"#,
    // Absolute, home and dot paths.
    r#"(?:^|[\s"'(=:])((?:~|\.{1,2})?(?:/[\w.@%+~-]+)+/?)"#,
    // Relative paths with a slash.
    r#"(?:^|[\s"'(=])((?:[\w.@%+~-]+/)+[\w.@%+~-]+/?)"#,
    // IPv6.
    r"(?:^|[\s\[(])((?:(?:[0-9a-fA-F]{1,4}:){1,6}|:):[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4}){0,5}|(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4})\b",
    // IPv4.
    r"\b\d{1,3}(?:\.\d{1,3}){3}\b",
    // Git hashes.
    r"\b[0-9a-f]{7,40}\b",
    // Numbers of four or more digits.
    r"\b\d{4,}\b",
];

/// [`HINT_PATTERNS`], for the tests of copy mode's hints.
#[cfg(test)]
pub fn default_hint_patterns() -> &'static [&'static str] {
    HINT_PATTERNS
}

/// `pane-border-format`'s default, tmux 3.7b's: its `[z]` and `[x]`
/// buttons are `control|8` and `control|9`.
pub const PANE_BORDER_FORMAT: &str = "#{?pane_active,#[reverse],}#{pane_index}#[default] \
     \"#{pane_title}\"#{?#{mouse},#[align=right]#[range=control|8]\
     [#{?#{window_zoomed_flag},u,z}]#[norange]#[range=control|9][x]#[norange],}";

/// What a framed title under grofe identity draws for the default
/// `pane-border-format`: the same less its buttons, which the frame's
/// control tab takes over (`render::frame_tab`), and with the pane index
/// between the tab's `ends` (`render::tab_ends`) in place of reversed, so
/// the two match. A pane whose title no program has set (empty, or the
/// hostname tmux gives it) shows its command and folder instead.
pub fn frame_title_format(ends: &str) -> String {
    let (l, r) = crate::server::render::tab_ends(ends);
    format!(
        "{l} #{{pane_index}} {r} \
         #{{?#{{||:#{{==:#{{pane_title}},}},#{{||:#{{==:#{{pane_title}},#{{host}}}},\
         #{{==:#{{pane_title}},#{{host_short}}}}}}}},\
         #{{pane_current_command}} · #{{b:pane_current_path}},\"#{{pane_title}}\"}}"
    )
}

/// Whether `name` is a built-in tmux's CLI must not see ([`GROFE_ONLY`]).
pub fn is_grofe_only(name: &str) -> bool {
    GROFE_ONLY.contains(&name) || GROFE_ONLY_LISTS.contains(&name)
}

/// `bell-action`, `activity-action` and `silence-action`'s choices.
pub(super) const ALERT_ACTIONS: &[&str] = &["none", "any", "current", "other"];

/// The `visual-*` options' choices.
pub(super) const VISUAL: &[&str] = &["off", "on", "both"];

/// One `Builtin` per row: `{ name, kind, scope, default, doc }`, then
/// `; toml = .., toml_ty = .., tmux_default = ..` for the columns a row sets.
macro_rules! builtins {
    ($({
        $name:expr, $kind:expr, $scope:ident, $default:expr, $doc:expr
        $(; $($field:ident = $value:expr),+)?
    })*) => {
        &[$(Builtin {
            name: $name,
            doc: $doc,
            kind: $kind,
            scope: OptionScope::$scope,
            default: $default,
            $($($field: Some($value),)+)?
            ..Builtin::PLAIN
        }),*]
    };
}

/// Every built-in option, and the only list of their names, kinds, defaults
/// and TOML keys. Rows are in the order the config lists its keys (`show-config`
/// and `import-tmux` write them so): top-level keys above the sectioned ones.
pub(super) const BUILTINS: &[Builtin] = builtins! {
    { "prefix", Kind::Key, Session, "C-b", "The prefix key." }
    { "base-index", Kind::Number { max: i32::MAX as i64 }, Session, "1", "The number the first window of a new session gets."; tmux_default = "0" }
    { "pane-base-index", Kind::Number { max: u16::MAX as i64 }, Window, "1", "The number the first pane of a window gets."; tmux_default = "0" }
    { "escape-time", Kind::Number { max: i32::MAX as i64 }, Global, "25", "Milliseconds to wait after an Escape for the rest of a key sequence."; toml = "escape-time-ms", tmux_default = "10" }
    { "repeat-time", Kind::Number { max: 2_000_000 }, Session, "500", "Milliseconds during which a repeatable key (bound with -r) can be pressed again without the prefix."; toml = "repeat-time-ms" }
    { "history-limit", Kind::Number { max: i32::MAX as i64 }, Session, "2000", "How many lines of scrollback each new pane keeps." }
    { "mouse", Kind::Bool, Session, "on", "Mouse support: select, resize, drag and scroll panes and click the status line."; tmux_default = "off" }
    // A choice in tmux 3.7b, so `yes` is refused and a bad value is
    // `unknown value`; `failed` keeps a pane only when its command
    // failed (Claude Code's agent teams set it); `key` keeps it until
    // a key is pressed (`split-window -k`).
    { REMAIN_ON_EXIT, Kind::Choice(&["off", "on", "failed", "key"]), Window, "off", "Keep a pane open after its program exits: always (on), only when it failed (failed), or until a key is pressed (key)."; toml_ty = Ty::Bool }
    // Spec §6: whether a starting server restores the snapshot, and the
    // spread of resumed panes' start delays. grofe's own names.
    { "restore-on-start", Kind::Bool, Global, "on", "Bring back the last snapshot's sessions when the server starts." }
    { "restore-stagger-ms", Kind::Number { max: i32::MAX as i64 }, Global, "1000", "Spread restored panes' resume commands over up to this many milliseconds; 0 starts them all at once." }
    // grofe's own names: a server that starts empty takes its sessions
    // from a running tmux, else from tmux-resurrect's newest save, found
    // in `resurrect-dir` or resurrect's default directories
    // (`server::adopt`).
    { "start-from-tmux", Kind::Bool, Global, "off", "When the default server starts with no sessions, take them from a running tmux server or tmux-resurrect's newest save." }
    { "resurrect-dir", Kind::String, Global, "", "Where to look for tmux-resurrect saves for start-from-tmux; empty uses resurrect's own default." }
    { "default-layout", Kind::String, Global, "", "Name of the layout file in the layouts directory that a new-session without a command builds; empty makes a plain session." }
    { "automatic-rename", Kind::Bool, Window, "on", "Rename a window after automatic-rename-format as its pane changes." }
    { "automatic-rename-format", Kind::String, Window, "#{b:pane_current_path}", "Format for a window's name while automatic-rename is on; grofe's default is the current folder."; tmux_default = "#{?pane_in_mode,[tmux],#{pane_current_command}}#{?pane_dead,[dead],}" }
    { "allow-rename", Kind::Bool, Window, "off", "Let a program rename its window with an escape sequence." }
    { "set-titles", Kind::Bool, Session, "off", "Set the outer terminal's title from set-titles-string." }
    // tmux's default, `#S:#I:#W - "#T" #{session_alerts}`, translated
    // into grofe's `#{...}`-only expander (R15); `session_alerts`
    // isn't implemented and expands empty, matching tmux with no
    // alerts set.
    { "set-titles-string", Kind::String, Session, "#{session_name}:#{window_index}:#{window_name} - \"#{pane_title}\" #{session_alerts}", "Format for the outer terminal's title when set-titles is on." }
    // Remote sessions (spec 2026-11-15-ssh-remote §3).
    { "stable-ssh-agent", Kind::Bool, Session, "on", "Give a session a stable SSH_AUTH_SOCK link that each attach repoints at the attaching client's agent, so old panes keep a working agent." }
    { "forward-browser", Kind::Bool, Session, "on", "Set BROWSER in new panes to a grofe-open shim (grofe open), so a remote client opens the URLs a pane asks for." }
    { "machine-key", Kind::Key, Global, "M-m", "The key that opens the machine picker; the client takes it before the server sees it." }
    { "remote-upload-paste", Kind::Choice(&["ask", "on", "off"]), Global, "ask", "What a remote client (grofe -r) does with a bracketed paste of local file paths: ask uploads them to the pane's directory after a prompt, on uploads without asking, off pastes the paths as text. Read from this machine's config; a remote server's value is ignored." }
    { "remote-upload-max", Kind::Number { max: i32::MAX as i64 }, Global, "100", "The largest total size, in MiB, of the files one paste uploads to a remote session; a bigger paste is refused. Read from this machine's config; a remote server's value is ignored." }
    { "remote-forward-ports", Kind::Choice(&["ask", "auto", "off"]), Global, "ask", "What a remote client (grofe -r) does when a remote pane starts listening on a port: ask forwards it to localhost (the same port, or the next free one) after a prompt, auto forwards without asking, off never forwards. Read from this machine's config; a remote server's value is ignored." }
    { "remote-reconnect", Kind::Bool, Global, "on", "Keep a remote attach (grofe -r) alive across a dropped connection: show a reconnect bar and attach again, instead of exiting." }
    { "remote-release-url", Kind::String, Global, "https://github.com/termage/grofe/releases/download", "Where grofe -r downloads the release it installs on a remote without grofe: an https or file URL, or a directory, holding one v-prefixed directory per version with the archives and SHA256SUMS." }
    { "remote-transport", Kind::Choice(&["ssh", "quic"]), Global, "ssh", "How grofe -r reaches the remote proxy: ssh, or quic (a roaming UDP link set up over ssh)." }
    { "remote-udp-ports", Kind::String, Global, "60001-61000", "UDP ports grofe proxy --quic picks a free one from, as LOW-HIGH or one port; read on the remote when the roaming transport starts." }
    { "predictive-echo", Kind::Choice(&["auto", "on", "off"]), Global, "auto", "Whether grofe -r draws typed characters at once, underlined, before the remote echoes them: auto does so when the round trip is 50 ms or more, on always, off never. Read from this machine's config." }
    { "remote-quic-idle", Kind::Number { max: i32::MAX as i64 }, Global, "604800", "Seconds grofe proxy --quic waits with no client connected before it exits; the default is 7 days; 0 is refused." }
    { "set-clipboard", Kind::Choice(&["off", "external", "on"]), Global, "external", "Where clipboard text goes: on makes a program's OSC 52 a paste buffer and passes it to the terminal, external sends only copy-mode copies to the terminal and ignores programs' OSC 52, off does neither." }
    { "focus-events", Kind::Bool, Global, "on", "Pass focus in and out events to programs that ask for them."; tmux_default = "off" }
    // FE.1: the corpus's tmux options grofe stores and never reads
    // (`NO_EFFECT`), with tmux 3.7b's kinds and scopes (recorded) and spec
    // §5.3's defaults where grofe's differ (`extended-keys`, `mode-keys`,
    // `renumber-windows`). Spec §4.5: the probe and D7 decide the terminal
    // ones; D6 sizes windows; grofe's prompt keys are emacs's.
    // `renumber-windows` itself is read (`Model::remove_window`).
    // ponytail: `synchronize-panes` does something in tmux; grofe stores it
    // only, until a ported script needs one.
    // `default-command` and `default-shell` are read when a pane starts
    // (`Model::default_command`, `Model::default_shell`);
    // its default is the server's shell, as tmux's `getshell` picks it
    // (`compat::server_started`).
    // Copy mode reads `mode-keys`.
    { "aggressive-resize", Kind::Bool, Window, "off", "Resize a window to the clients that show it rather than to every client attached to the session." }
    { "default-command", Kind::String, Session, "", "Command new panes run; empty runs default-shell." }
    { "default-shell", Kind::String, Session, "/bin/sh", "Shell new panes run when default-command is empty; set from $SHELL when the server starts." }
    { "default-terminal", Kind::String, Global, "tmux-256color", "The TERM value programs in panes see." }
    { "extended-keys", Kind::Choice(&["off", "on", "always"]), Global, "on", "Whether keys are sent to programs in the extended (CSI u) encoding: off, on (when a program asks) or always."; tmux_default = "off" }
    { "extended-keys-format", Kind::Choice(&["csi-u", "xterm"]), Global, "xterm", "Encoding for extended keys sent to programs: xterm or csi-u." }
    { "mode-keys", Kind::Choice(&["emacs", "vi"]), Window, "vi", "Key table copy mode and the choose modes use: vi or emacs." }
    { "renumber-windows", Kind::Bool, Session, "on", "Renumber a session's windows when one is closed, so there are no gaps."; tmux_default = "off" }
    { "synchronize-panes", Kind::Bool, Window, "off", "Send input typed into one pane to every pane in the window." }
    // FE.1: what `select-pane -P` sets on a pane, as tmux 3.7b does
    // (recorded; free text, unchecked). B1c draws them: the colours of the
    // cells in the default colours.
    { "window-style", Kind::String, Window, "default", "Default style of inactive panes' content." }
    { "window-active-style", Kind::String, Window, "default", "Default style of the active pane's content." }
    // B1a: options plugins and themes set, tmux 3.7b's kinds, scopes and
    // defaults (options-table.c). `prefix2`, `status-bg`, `status-fg`,
    // `default-size`, `exit-empty` and `detach-on-destroy` are read (input,
    // the status line, `new-session`, the server loop, a destroyed
    // session's clients). `window-status-activity-style` and
    // `focus-follows-mouse` are stored (`NO_EFFECT`): grofe has no activity
    // flag styling and no hover focus.
    { "prefix2", Kind::Key, Session, "None", "A second prefix key; None for none." }
    { "status-bg", Kind::Colour, Session, "default", "Background colour of the status line (an older spelling of status-style's bg)." }
    { "status-fg", Kind::Colour, Session, "default", "Foreground colour of the status line (an older spelling of status-style's fg)." }
    { "window-status-activity-style", Kind::String, Window, "reverse", "Style of a window's entry in the window list when it has an activity or silence alert." }
    { "focus-follows-mouse", Kind::Bool, Session, "off", "Make the pane under the mouse the active pane as the pointer moves." }
    { "default-size", Kind::Size, Session, "80x24", "Size of new windows when a session starts detached or window-size is manual, as columns x rows." }
    { "exit-empty", Kind::Bool, Global, "on", "Stop the server when its last session closes." }
    { "detach-on-destroy", Kind::Choice(&["off", "on", "no-detached", "previous", "next"]), Session, "on", "What an attached client does when its session closes: detach (on), switch to the most recent other session (off), switch unless none is detached (no-detached), or go to the previous or next session." }
    // TC.2b: tmux's name, scope, default and free text (tmux 3.7b stores
    // any string; `select-layout` reads cells or `N%`, TC.8).
    { "main-pane-width", Kind::String, Window, "80", "Width of the main pane in the main-vertical layout, in cells or as a percentage." }
    // TC.8: as `main-pane-width`, for the `main-horizontal` presets.
    { "main-pane-height", Kind::String, Window, "24", "Height of the main pane in the main-horizontal layout, in cells or as a percentage." }
    // TC.8: tmux's `window-size`. `resize-window` sets `manual`, which
    // stops clients resizing the window; grofe sizes every other value as
    // `latest` (spec D6).
    { "window-size", Kind::Choice(&["largest", "smallest", "manual", "latest"]), Window, "latest", "How a window's size follows the clients attached to it: the largest, the smallest, the latest used, or manual." }
    // MF (spec 2026-11-05-frames.md): grofe's own names ([`GROFE_ONLY`]).
    // Each tiled pane gets its own frame with `pane-gap` cells between
    // frames and around the outer ones; the title is drawn in the top
    // edge unless `pane-frame-title` is off. Off and 0 under tmux
    // identity (`tmux_default`).
    { "pane-frames", Kind::Bool, Window, "on", "Give each tiled pane its own rounded frame with its title, gaps between them; off gives tmux's shared borders."; tmux_default = "off" }
    { "pane-gap", Kind::Number { max: 8 }, Window, "1", "Cells of gap between pane frames and around the outside."; tmux_default = "0" }
    // grofe's own: how many hidden scratchpads the server keeps.
    { "scratchpad-history", Kind::Number { max: 100 }, Global, "10", "How many hidden scratchpads to keep; the oldest closes past this, 0 closes one as soon as it is hidden." }
    { "pane-frame-title", Kind::Bool, Window, "on", "Draw the pane's number, title and controls in its frame's top edge." }
    // The two end glyphs of a framed title's control tab and pane index
    // under grofe identity: brackets, which any terminal font has; a Nerd
    // font's thin rounded `\u{e0b7}\u{e0b5}` are the usual choice there,
    // and anything but two one-cell glyphs gives blank ends.
    { "pane-frame-tab-ends", Kind::String, Window, "()", "Two glyphs drawn around the frame's number and controls; empty draws none." }
    { "agent-glyph-blocked", Kind::String, Session, "⏸", "Glyph for an agent waiting on you, in the window list and the agents badge." }
    { "agent-glyph-error", Kind::String, Session, "✗", "Glyph for an agent that stopped with an error, in the window list and the agents badge." }
    { "agent-glyph-unseen", Kind::String, Session, "✓", "Glyph for an agent that finished and has not been looked at, in the window list and the agents badge." }
    { "agent-glyph-working", Kind::String, Session, "●", "Glyph for a working agent, in the window list." }
    { "notify-muted-glyph", Kind::String, Session, "🔕", "Glyph the agents badge shows while notifications are muted." }
    { "agent-blocked-border-style", Kind::String, Session, "fg=yellow", "Border style of a pane whose agent is waiting on you, active or not." }
    // Where the command prompt opens: on the status line, as tmux's, or as
    // a box in the middle of the window.
    { "prompt-position", Kind::Choice(&["status", "centre", "center"]), Session, "status", "Where the command prompt opens: on the status line (status) or as a box a third down the window (centre)." }
    { "status", Kind::Choice(&["off", "on", "2", "3", "4", "5"]), Session, "on", "Show the status line, and how many rows it has (on is one row)."; toml = "status.lines", toml_ty = Ty::Int }
    { "status-position", Kind::Choice(&["top", "bottom"]), Session, "bottom", "Whether the status line is at the top or bottom of the window."; toml = "status.position" }
    { "status-style", Kind::String, Session, "bg=green,fg=black", "Style of the status line."; toml = "status.style" }
    { "status-left", Kind::String, Session, "[#{session_name}] ", "Format for the left part of the status line."; toml = "status.left" }
    { "status-right", Kind::String, Session, "\"#{pane_title}\" %H:%M %d-%b-%y#[range=user|agents]#{agents_badge}#[norange]", "Format for the right part of the status line; grofe's default ends with the agents badge."; toml = "status.right", tmux_default = "\"#{pane_title}\" %H:%M %d-%b-%y" }
    { "status-left-length", Kind::Number { max: i16::MAX as i64 }, Session, "10", "The widest the left part of the status line may be, in cells."; toml = "status.left-length" }
    { "status-right-length", Kind::Number { max: i16::MAX as i64 }, Session, "40", "The widest the right part of the status line may be, in cells."; toml = "status.right-length" }
    { "status-interval", Kind::Number { max: i32::MAX as i64 }, Session, "15", "Seconds between status line refreshes and runs of its #() commands; 0 turns the timer off."; toml = "status.interval" }
    { "window-status-format", Kind::String, Window, "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}", "Format of a window's entry in the status line's window list; grofe's default adds the window's agent glyph."; toml = "status.window.format", tmux_default = "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }" }
    { "window-status-current-format", Kind::String, Window, "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}", "Format of the current window's entry in the window list; grofe's default adds the window's agent glyph."; toml = "status.window.current-format", tmux_default = "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }" }
    // TC.11: tmux 3.7b's names, scopes, choices and defaults for the rest
    // of the status line (spec §4.5's status-line row).
    { "status-justify", Kind::Choice(&["left", "centre", "right", "absolute-centre"]), Session, "left", "Where the window list sits in the status line."; toml = "status.justify" }
    { "status-left-style", Kind::String, Session, "default", "Style of the left part of the status line."; toml = "status.left-style" }
    { "status-right-style", Kind::String, Session, "default", "Style of the right part of the status line."; toml = "status.right-style" }
    { "window-status-separator", Kind::String, Window, " ", "Text drawn between windows in the window list."; toml = "status.window.separator" }
    { "window-status-style", Kind::String, Window, "default", "Style of a window's entry in the window list."; toml = "status.window.style" }
    { "window-status-current-style", Kind::String, Window, "default", "Style of the current window's entry in the window list."; toml = "status.window.current-style" }
    { "window-status-last-style", Kind::String, Window, "default", "Style of the previously selected window's entry in the window list."; toml = "status.window.last-style" }
    { "window-status-bell-style", Kind::String, Window, "reverse", "Style of a window's entry in the window list when it has a bell alert."; toml = "status.window.bell-style" }
    { "message-style", Kind::String, Session, "bg=yellow,fg=black,fill=yellow", "Style of messages and of the command prompt."; toml = "message.style" }
    { "display-time", Kind::Number { max: i32::MAX as i64 }, Session, "750", "How long a message stays, in milliseconds."; toml = "message.display-time-ms" }
    // A message as a pill in the status line, coloured by its level, and
    // the prompt in the same style (grofe identity); `off` is tmux's
    // full-width bar in `message-style` (`tmux_default`).
    { "message-pill", Kind::Bool, Session, "on", "Draw messages and the status-line prompt as a pill in the middle of the status line; off gives tmux's full-width bar."; toml = "message.pill", tmux_default = "off" }
    // The message and prompt pill's two end glyphs. Empty (the default)
    // gives square ends, which any font has; a Nerd font's rounded
    // `\u{e0b6}\u{e0b4}` match a pill status line.
    { "message-pill-ends", Kind::String, Session, "", "Two glyphs drawn at the ends of the message and prompt pill, in its colour; empty takes the theme's @pill_l and @pill_r, else Nerd Font round caps."; toml = "message.pill-ends" }
    { "pane-border-style", Kind::String, Window, "default", "Style of the borders or frames of inactive panes."; toml = "pane-border.style" }
    // tmux 3.7b's: yellow in a mode, red while panes are synchronized.
    { "pane-active-border-style", Kind::String, Window, "#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}", "Style of the active pane's border or frame."; toml = "pane-border.active-style" }
    { "pane-border-format", Kind::String, Window, PANE_BORDER_FORMAT, "Format of the pane status line shown in a border, and of a frame's title when it is changed from the default."; toml = "pane-border.format" }
    // B1c: tmux 3.7b's names, values, window scope and defaults
    // (options-table.c). `pane-border-status` makes the panes along the
    // top or bottom edge give up a row to a title line.
    { "pane-border-status", Kind::Choice(&["off", "top", "bottom"]), Window, "off", "Show a status line in the top or bottom border of each pane."; toml = "pane-border.status" }
    // MF adds `rounded` (spec 2026-11-05-frames.md), grofe's default: a
    // frame's corners, drawn as `single` with frames off. tmux identity
    // keeps `single` (`tmux_default`).
    { "pane-border-lines", Kind::Choice(&["single", "double", "heavy", "simple", "number", "spaces", "rounded"]), Window, "rounded", "Characters borders and frames are drawn with."; toml = "pane-border.lines", tmux_default = "single" }
    { "pane-border-indicators", Kind::Choice(&["off", "colour", "arrows", "both"]), Window, "colour", "How shared borders mark the active pane: by colour, with arrows, both or not at all."; toml = "pane-border.indicators" }
    { "status-keys", Kind::Choice(&["emacs", "vi"]), Session, "emacs", "Key table the command prompt uses: emacs or vi."; toml = "status.keys" }
    // The server's identity override (spec D12), read once at start.
    { "compat-identity", Kind::Choice(&["auto", "tmux", "grofe"]), Global, "auto", "Whether the server behaves as grofe or as tmux; auto decides by the name it was started under. Fixed when the server starts." }
    // G3: kitty direct placements, animation and sixel (grofe's own name,
    // like `compat-identity`; `docs/graphics-placement.md`). Off, the
    // clients get what T3.8 sent.
    { "graphics", Kind::Bool, Global, "off", "Enable kitty graphics direct placements, animation and sixel (Unicode placeholders work without it)." }
    // TC.12: tmux 3.7b's name, values, window scope and default, so the
    // `set -p allow-passthrough on` that icat, snacks.nvim and canopy send
    // works and `show` answers.
    // ponytail: a no-op. grofe always unwraps `DCS tmux;` and lets out only
    // what the same bytes sent bare would (passthrough.rs), so there is
    // nothing for `off` to guard; canopy turns it on anyway.
    { "allow-passthrough", Kind::Choice(&["off", "on", "all"]), Window, "off", "Let programs pass escape sequences through to the outer terminal: off, on (visible panes only) or all." }
    // Window alerts (`server::alerts`, TP.10), with tmux's defaults.
    { "monitor-bell", Kind::Bool, Window, "on", "Flag a window that rings the bell." }
    { "monitor-activity", Kind::Bool, Window, "off", "Flag a window that has output while you are not looking at it." }
    { "monitor-silence", Kind::Number { max: i32::MAX as i64 }, Window, "0", "Flag a window that has had no output for this many seconds; 0 turns it off." }
    { "bell-action", Kind::Choice(ALERT_ACTIONS), Session, "any", "Which windows' bells are acted on: any, none, the current one or the others." }
    { "activity-action", Kind::Choice(ALERT_ACTIONS), Session, "other", "Which windows' activity alerts are acted on: any, none, the current one or the others." }
    { "silence-action", Kind::Choice(ALERT_ACTIONS), Session, "other", "Which windows' silence alerts are acted on: any, none, the current one or the others." }
    { "visual-bell", Kind::Choice(VISUAL), Session, "off", "Show a message for a bell (on), pass the bell to the terminal (off), or both." }
    { "visual-activity", Kind::Choice(VISUAL), Session, "off", "Show a message for an activity alert (on), ring the bell (off), or both." }
    { "visual-silence", Kind::Choice(VISUAL), Session, "off", "Show a message for a silence alert (on), ring the bell (off), or both." }
    // TM.4: tmux's server option, at grofe's global scope (as
    // `escape-time`); at least 1.
    { "buffer-limit", Kind::Number { max: i32::MAX as i64 }, Global, "50", "How many automatically named paste buffers to keep; the oldest is dropped past this." }
    // B1d: clock mode's colour and time format, tmux 3.7b's names, choices
    // and defaults (`4` is blue; `show` prints it so).
    { "clock-mode-colour", Kind::String, Window, "blue", "Colour of the clock in clock mode." }
    { "clock-mode-style", Kind::Choice(&["12", "24", "12-with-seconds", "24-with-seconds"]), Window, "24", "Whether clock mode shows a 12 or 24 hour clock, with or without seconds." }
    // Copy mode's options, with tmux 3.7b's scopes and defaults
    // (`copy-command` is a server option there, global here).
    { "word-separators", Kind::String, Session, "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~", "Characters that end a word for copy mode's word motions and double-click selection." }
    { "wrap-search", Kind::Bool, Window, "on", "Whether a copy-mode search wraps around at the top or bottom of the history." }
    { "copy-command", Kind::String, Global, "", "A shell command that copy mode's copy-pipe commands send the copied text to when they are given none." }
    { "mode-style", Kind::String, Window, "noattr,bg=yellow,fg=black", "Style of copy mode and of the choose and tree modes, including the selection." }
    { "copy-mode-match-style", Kind::String, Window, "bg=cyan,fg=black", "Style of search matches in copy mode."; toml = "copy-mode.match-style" }
    { "copy-mode-current-match-style", Kind::String, Window, "bg=magenta,fg=black", "Style of the current search match in copy mode."; toml = "copy-mode.current-match-style" }
    { "prefix-timeout", Kind::Number { max: i32::MAX as i64 }, Global, "0", "Milliseconds after the prefix before it is cancelled; 0 waits forever." }
    // B2a: the options tmux 3.7b has that grofe rejected, with tmux's kinds,
    // scopes and defaults (options-table.c). Those in `NO_EFFECT` are stored
    // only; the rest are read where the comment on `NO_EFFECT` says.
    { "assume-paste-time", Kind::Number { max: i32::MAX as i64 }, Session, "1", "Keys arriving faster than this many milliseconds apart are taken as a paste, not bindings." }
    { "destroy-unattached", Kind::Choice(&["off", "on", "keep-last", "keep-group"]), Session, "off", "Close a session when its last client detaches: off, on, keep-last or keep-group." }
    { "display-panes-active-colour", Kind::Colour, Session, "red", "Colour of the active pane's number in display-panes." }
    { "display-panes-colour", Kind::Colour, Session, "blue", "Colour of the other panes' numbers in display-panes." }
    { "display-panes-time", Kind::Number { max: i32::MAX as i64 }, Session, "1000", "Milliseconds display-panes shows the pane numbers for." }
    { "initial-repeat-time", Kind::Number { max: 2_000_000 }, Session, "0", "Repeat window, in milliseconds, after the first press of a repeatable key; 0 uses repeat-time." }
    { "key-table", Kind::String, Session, "root", "The key table a client starts in." }
    { "lock-after-time", Kind::Number { max: i32::MAX as i64 }, Session, "0", "Lock a session after this many seconds without input; 0 never locks." }
    { "lock-command", Kind::String, Session, "lock -np", "Command run to lock a client." }
    { "message-command-style", Kind::String, Session, "bg=black,fg=yellow,fill=black", "Style of the command prompt while it is in vi command mode." }
    { "message-format", Kind::String, Session, "#[#{?#{command_prompt},#{E:message-command-style},#{E:message-style}}]#{message}", "Format of messages and the prompt line." }
    { "message-line", Kind::Choice(&["0", "1", "2", "3", "4"]), Session, "0", "Which status row messages and the prompt use." }
    { "prompt-cursor-colour", Kind::Colour, Session, "none", "Cursor colour in the command prompt." }
    { "prompt-cursor-style", Kind::Choice(&["default", "blinking-block", "block", "blinking-underline", "underline", "blinking-bar", "bar"]), Session, "default", "Cursor shape in the command prompt." }
    { "prompt-command-cursor-style", Kind::Choice(&["default", "blinking-block", "block", "blinking-underline", "underline", "blinking-bar", "bar"]), Session, "default", "Cursor shape in the command prompt's vi command mode." }
    { "cursor-colour", Kind::Colour, Window, "none", "Cursor colour in panes." }
    { "cursor-style", Kind::Choice(&["default", "blinking-block", "block", "blinking-underline", "underline", "blinking-bar", "bar"]), Window, "default", "Cursor shape in panes." }
    { "menu-style", Kind::String, Window, "default", "Style of menus." }
    { "menu-selected-style", Kind::String, Window, "bg=yellow,fg=black", "Style of a menu's selected item." }
    { "menu-border-style", Kind::String, Window, "default", "Style of a menu's border." }
    { "menu-border-lines", Kind::Choice(&["single", "double", "heavy", "simple", "rounded", "padded", "none"]), Window, "single", "Characters a menu's border is drawn with." }
    { "pane-status-current-style", Kind::String, Window, "default", "Style of the current pane's entry in a status-format pane list." }
    { "pane-status-style", Kind::String, Window, "default", "Style of a pane's entry in a status-format pane list." }
    { "session-status-current-style", Kind::String, Window, "default", "Style of the current session's entry in a status-format session list." }
    { "session-status-style", Kind::String, Window, "default", "Style of a session's entry in a status-format session list." }
    { "allow-set-title", Kind::Bool, Window, "on", "Let a program set its pane's title with an escape sequence." }
    { "alternate-screen", Kind::Bool, Window, "on", "Let programs use the alternate screen." }
    { "copy-mode-mark-style", Kind::String, Window, "bg=red,fg=black", "Style of the marked line in copy mode." }
    { "copy-mode-position-format", Kind::String, Window, "#[align=right]#{t/p:top_line_time}#{?#{e|>:#{top_line_time},0}, ,}[#{copy_position}/#{copy_position_limit}]#{?search_timed_out, (timed out),#{?search_count, (#{search_count}#{?search_count_partial,+,} results),}}", "Format of the position indicator in copy mode." }
    { "copy-mode-position-style", Kind::String, Window, "#{E:mode-style}", "Style of the position indicator in copy mode." }
    { "copy-mode-selection-style", Kind::String, Window, "#{E:mode-style}", "Style of the selection in copy mode." }
    { "copy-mode-current-line-number-style", Kind::String, Window, "fg=yellow", "Style of the current line's number in copy mode." }
    { "copy-mode-line-number-style", Kind::String, Window, "fg=white,dim", "Style of line numbers in copy mode." }
    { "copy-mode-line-numbers", Kind::Choice(&["off", "default", "absolute", "relative", "hybrid"]), Window, "off", "Show line numbers in copy mode, and which kind." }
    { "fill-character", Kind::String, Window, "", "Character to fill unused parts of a window with; empty is a space." }
    { "other-pane-height", Kind::String, Window, "0", "Height of the other panes in the main-horizontal layout; 0 shares the space." }
    { "other-pane-width", Kind::String, Window, "0", "Width of the other panes in the main-vertical layout; 0 shares the space." }
    { "pane-scrollbars", Kind::Choice(&["off", "modal", "on"]), Window, "off", "Show scrollbars in panes: off, in copy mode only (modal) or always." }
    { "pane-scrollbars-style", Kind::String, Window, "bg=black,fg=white,width=1,pad=0", "Style and width of pane scrollbars." }
    { "pane-scrollbars-position", Kind::Choice(&["right", "left"]), Window, "right", "Which side of the pane its scrollbar is on." }
    { "popup-style", Kind::String, Window, "default", "Style of popups." }
    { "popup-border-style", Kind::String, Window, "default", "Style of a popup's border." }
    { "popup-border-lines", Kind::Choice(&["single", "double", "heavy", "simple", "rounded", "padded", "none"]), Window, "single", "Characters a popup's border is drawn with." }
    { "remain-on-exit-format", Kind::String, Window, "Pane is dead (#{?#{!=:#{pane_dead_status},},status #{pane_dead_status},}#{?#{!=:#{pane_dead_signal},},signal #{pane_dead_signal},}, #{t:pane_dead_time})", "Text shown at the bottom of a pane kept open by remain-on-exit." }
    { "scroll-on-clear", Kind::Bool, Window, "on", "Push the screen into the history when a program clears it." }
    { "tiled-layout-max-columns", Kind::Number { max: 65_535 }, Window, "0", "The most columns the tiled layout uses; 0 has no limit." }
    { "tree-mode-preview-format", Kind::String, Window, "#{?pane_format,#{pane_index}:#{pane_title},#{window_index}:#{window_name}}", "Format of the labels in choose-tree's preview." }
    { "tree-mode-preview-style", Kind::String, Window, "fg=#{?#{||:#{&&:#{pane_format},#{pane_active}},#{&&:#{window_format},#{window_active}}},#{display-panes-active-colour},#{display-panes-colour}}", "Style of the labels in choose-tree's preview." }
    { "window-pane-current-status-format", Kind::String, Window, "#P:[#T]#{?pane_flags,#{pane_flags}, }", "Format of the current pane's entry in a status-format pane list." }
    { "window-pane-status-format", Kind::String, Window, "#P:[#T]#{?pane_flags,#{pane_flags}, }", "Format of a pane's entry in a status-format pane list." }
    { "xterm-keys", Kind::Bool, Window, "on", "Send function keys with modifiers in xterm's encoding." }
    { "backspace", Kind::Key, Global, "C-?", "The key sent for Backspace." }
    { "default-client-command", Kind::String, Global, "new-session", "Command run when grofe is started with no command." }
    { "editor", Kind::String, Global, "/usr/bin/vi", "Editor run by commands that open one." }
    { "exit-unattached", Kind::Bool, Global, "off", "Stop the server when no client is attached." }
    { "get-clipboard", Kind::Choice(&["off", "buffer", "request", "both"]), Global, "buffer", "Whether a program's OSC 52 clipboard query is answered. Only buffer answers it, from the newest paste buffer, and only when set-clipboard is on; off, request and both are never answered, and the terminal's clipboard is never asked." }
    { "history-file", Kind::String, Global, "", "File the command prompt's history is saved to; empty saves none." }
    { "input-buffer-size", Kind::Number { max: 4_294_967_295 }, Global, "1048576", "Most bytes one escape sequence from a program may hold." }
    { "message-limit", Kind::Number { max: i32::MAX as i64 }, Global, "1000", "How many messages to keep in the message log." }
    { "prompt-history-limit", Kind::Number { max: i32::MAX as i64 }, Global, "100", "How many command prompt entries to keep in its history." }
    { "variation-selector-always-wide", Kind::Bool, Global, "on", "Treat a character followed by variation selector 16 as two cells wide." }
};

/// A built-in option's scope (a list's too), `None` for anything else.
pub fn builtin_scope(name: &str) -> Option<OptionScope> {
    builtin(name)
        .map(|b| b.scope)
        .or_else(|| list_builtin(name).map(|l| l.scope))
}

/// Every built-in option's name tmux's CLI knows, in table order: all but
/// [`grofe_only_names`].
pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    BUILTINS
        .iter()
        .map(|b| b.name)
        .filter(|n| !is_grofe_only(n))
}

/// The built-ins only grofe's own CLI and config know ([`GROFE_ONLY`]).
pub fn grofe_only_names() -> impl Iterator<Item = &'static str> {
    GROFE_ONLY.iter().copied()
}

/// The kind of built-in option `name`, for the config's TOML-type test.
#[cfg(test)]
pub(crate) fn kind_of(name: &str) -> Option<Kind> {
    builtin(name).map(|b| b.kind)
}

pub(super) fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// Every built-in option as `(TOML key, option name)`, in table order.
pub fn toml_names() -> impl Iterator<Item = (&'static str, &'static str)> {
    BUILTINS.iter().map(|b| (b.toml.unwrap_or(b.name), b.name))
}

/// The TOML type of built-in option `name`: its override, else from its kind.
pub(crate) fn toml_ty_of(name: &str) -> Option<Ty> {
    builtin(name).map(|b| {
        b.toml_ty.unwrap_or(match b.kind {
            Kind::Bool => Ty::Bool,
            Kind::Number { .. } => Ty::Int,
            _ => Ty::Str,
        })
    })
}

#[cfg(test)]
mod default_tests {
    use super::super::Options;

    #[test]
    fn show_g_history_limit_is_2000() {
        let o = Options::new(false);
        assert_eq!(o.lookup("history-limit", &[]), Some("2000"));
    }
}
