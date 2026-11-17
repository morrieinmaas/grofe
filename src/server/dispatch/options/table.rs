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

/// Every built-in option, and the only list of their names, kinds, defaults
/// and TOML keys. Rows are in the order the config lists its keys (`show-config`
/// and `import-tmux` write them so): top-level keys above the sectioned ones.
pub(super) const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "prefix",
        doc: "The prefix key.",
        kind: Kind::Key,
        scope: OptionScope::Session,
        default: "C-b",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "base-index",
        doc: "The number the first window of a new session gets.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1",
        tmux_default: Some("0"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-base-index",
        doc: "The number the first pane of a window gets.",
        kind: Kind::Number {
            max: u16::MAX as i64,
        },
        scope: OptionScope::Window,
        default: "1",
        tmux_default: Some("0"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "escape-time",
        doc: "Milliseconds to wait after an Escape for the rest of a key sequence.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "25",
        toml: Some("escape-time-ms"),
        tmux_default: Some("10"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "repeat-time",
        doc: "Milliseconds during which a repeatable key (bound with -r) can be pressed again without the prefix.",
        kind: Kind::Number { max: 2_000_000 },
        scope: OptionScope::Session,
        default: "500",
        toml: Some("repeat-time-ms"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "history-limit",
        doc: "How many lines of scrollback each new pane keeps.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "50000",
        tmux_default: Some("2000"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "mouse",
        doc: "Mouse support: select, resize, drag and scroll panes and click the status line.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: REMAIN_ON_EXIT,
        doc: "Keep a pane open after its program exits: always (on), only when it failed (failed), or until a key is pressed (key).",
        // A choice in tmux 3.7b, so `yes` is refused and a bad value is
        // `unknown value`; `failed` keeps a pane only when its command
        // failed (Claude Code's agent teams set it); `key` keeps it until
        // a key is pressed (`split-window -k`).
        kind: Kind::Choice(&["off", "on", "failed", "key"]),
        scope: OptionScope::Window,
        default: "off",
        toml_ty: Some(Ty::Bool),
        ..Builtin::PLAIN
    },
    // Spec §6: whether a starting server restores the snapshot, and the
    // spread of resumed panes' start delays. grofe's own names.
    Builtin {
        name: "restore-on-start",
        doc: "Bring back the last snapshot's sessions when the server starts.",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "restore-stagger-ms",
        doc: "Spread restored panes' resume commands over up to this many milliseconds; 0 starts them all at once.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "1000",
        ..Builtin::PLAIN
    },
    // grofe's own names: a server that starts empty takes its sessions
    // from a running tmux, else from tmux-resurrect's newest save, found
    // in `resurrect-dir` or resurrect's default directories
    // (`server::adopt`).
    Builtin {
        name: "start-from-tmux",
        doc: "When the default server starts with no sessions, take them from a running tmux server or tmux-resurrect's newest save.",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "resurrect-dir",
        doc: "Where to look for tmux-resurrect saves for start-from-tmux; empty uses resurrect's own default.",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "automatic-rename",
        doc: "Rename a window after automatic-rename-format as its pane changes.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "automatic-rename-format",
        doc: "Format for a window's name while automatic-rename is on; grofe's default is the current folder.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{b:pane_current_path}",
        tmux_default: Some("#{?pane_in_mode,[tmux],#{pane_current_command}}#{?pane_dead,[dead],}"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "allow-rename",
        doc: "Let a program rename its window with an escape sequence.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "set-titles",
        doc: "Set the outer terminal's title from set-titles-string.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "set-titles-string",
        doc: "Format for the outer terminal's title when set-titles is on.",
        kind: Kind::String,
        scope: OptionScope::Session,
        // tmux's default, `#S:#I:#W - "#T" #{session_alerts}`, translated
        // into grofe's `#{...}`-only expander (R15); `session_alerts`
        // isn't implemented and expands empty, matching tmux with no
        // alerts set.
        default: "#{session_name}:#{window_index}:#{window_name} - \"#{pane_title}\" #{session_alerts}",
        ..Builtin::PLAIN
    },
    // Remote sessions (spec 2026-11-15-ssh-remote §3).
    Builtin {
        name: "stable-ssh-agent",
        doc: "Give a session a stable SSH_AUTH_SOCK link that each attach repoints at the attaching client's agent, so old panes keep a working agent.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "forward-browser",
        doc: "Set BROWSER to grofe open in new panes, so a remote client opens the URLs a pane asks for.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "machine-key",
        doc: "The key that opens the machine picker; the client takes it before the server sees it.",
        kind: Kind::Key,
        scope: OptionScope::Global,
        default: "M-m",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "remote-upload-paste",
        doc: "What a remote client does with a bracketed paste of local file paths: ask uploads them to the pane's directory after a prompt, on uploads without asking, off pastes the paths as text.",
        kind: Kind::Choice(&["ask", "on", "off"]),
        scope: OptionScope::Session,
        default: "ask",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "remote-upload-max",
        doc: "The largest total size, in MiB, of the files one paste uploads to a remote session; a bigger paste is refused.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "100",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "remote-forward-ports",
        doc: "What a remote client does when a pane starts listening on a port: ask forwards it to localhost (the same port, or the next free one) after a prompt, auto forwards without asking, off never forwards.",
        kind: Kind::Choice(&["ask", "auto", "off"]),
        scope: OptionScope::Session,
        default: "ask",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "set-clipboard",
        doc: "Where clipboard text goes: on makes a program's OSC 52 a paste buffer and passes it to the terminal, external sends only copy-mode copies to the terminal and ignores programs' OSC 52, off does neither.",
        kind: Kind::Choice(&["off", "external", "on"]),
        scope: OptionScope::Global,
        default: "external",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "focus-events",
        doc: "Pass focus in and out events to programs that ask for them.",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
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
    Builtin {
        name: "aggressive-resize",
        doc: "Resize a window to the clients that show it rather than to every client attached to the session.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-command",
        doc: "Command new panes run; empty runs default-shell.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-shell",
        doc: "Shell new panes run when default-command is empty; set from $SHELL when the server starts.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "/bin/sh",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-terminal",
        doc: "The TERM value programs in panes see.",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "tmux-256color",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "extended-keys",
        doc: "Whether keys are sent to programs in the extended (CSI u) encoding: off, on (when a program asks) or always.",
        kind: Kind::Choice(&["off", "on", "always"]),
        scope: OptionScope::Global,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "extended-keys-format",
        doc: "Encoding for extended keys sent to programs: xterm or csi-u.",
        kind: Kind::Choice(&["csi-u", "xterm"]),
        scope: OptionScope::Global,
        default: "xterm",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "mode-keys",
        doc: "Key table copy mode and the choose modes use: vi or emacs.",
        kind: Kind::Choice(&["emacs", "vi"]),
        scope: OptionScope::Window,
        default: "vi",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "renumber-windows",
        doc: "Renumber a session's windows when one is closed, so there are no gaps.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "synchronize-panes",
        doc: "Send input typed into one pane to every pane in the window.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    // FE.1: what `select-pane -P` sets on a pane, as tmux 3.7b does
    // (recorded; free text, unchecked). B1c draws them: the colours of the
    // cells in the default colours.
    Builtin {
        name: "window-style",
        doc: "Default style of inactive panes' content.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-active-style",
        doc: "Default style of the active pane's content.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    // B1a: options plugins and themes set, tmux 3.7b's kinds, scopes and
    // defaults (options-table.c). `prefix2`, `status-bg`, `status-fg`,
    // `default-size`, `exit-empty` and `detach-on-destroy` are read (input,
    // the status line, `new-session`, the server loop, a destroyed
    // session's clients). `window-status-activity-style` and
    // `focus-follows-mouse` are stored (`NO_EFFECT`): grofe has no activity
    // flag styling and no hover focus.
    Builtin {
        name: "prefix2",
        doc: "A second prefix key; None for none.",
        kind: Kind::Key,
        scope: OptionScope::Session,
        default: "None",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-bg",
        doc: "Background colour of the status line (an older spelling of status-style's bg).",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-fg",
        doc: "Foreground colour of the status line (an older spelling of status-style's fg).",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-activity-style",
        doc: "Style of a window's entry in the window list when it has an activity or silence alert.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "reverse",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "focus-follows-mouse",
        doc: "Make the pane under the mouse the active pane as the pointer moves.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-size",
        doc: "Size of new windows when a session starts detached or window-size is manual, as columns x rows.",
        kind: Kind::Size,
        scope: OptionScope::Session,
        default: "80x24",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "exit-empty",
        doc: "Stop the server when its last session closes.",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "detach-on-destroy",
        doc: "What an attached client does when its session closes: detach (on), switch to the most recent other session (off), switch unless none is detached (no-detached), or go to the previous or next session.",
        kind: Kind::Choice(&["off", "on", "no-detached", "previous", "next"]),
        scope: OptionScope::Session,
        default: "on",
        ..Builtin::PLAIN
    },
    // TC.2b: tmux's name, scope, default and free text (tmux 3.7b stores
    // any string; `select-layout` reads cells or `N%`, TC.8).
    Builtin {
        name: "main-pane-width",
        doc: "Width of the main pane in the main-vertical layout, in cells or as a percentage.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "80",
        ..Builtin::PLAIN
    },
    // TC.8: as `main-pane-width`, for the `main-horizontal` presets.
    Builtin {
        name: "main-pane-height",
        doc: "Height of the main pane in the main-horizontal layout, in cells or as a percentage.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "24",
        ..Builtin::PLAIN
    },
    // TC.8: tmux's `window-size`. `resize-window` sets `manual`, which
    // stops clients resizing the window; grofe sizes every other value as
    // `latest` (spec D6).
    Builtin {
        name: "window-size",
        doc: "How a window's size follows the clients attached to it: the largest, the smallest, the latest used, or manual.",
        kind: Kind::Choice(&["largest", "smallest", "manual", "latest"]),
        scope: OptionScope::Window,
        default: "latest",
        ..Builtin::PLAIN
    },
    // MF (spec 2026-11-05-frames.md): grofe's own names ([`GROFE_ONLY`]).
    // Each tiled pane gets its own frame with `pane-gap` cells between
    // frames and around the outer ones; the title is drawn in the top
    // edge unless `pane-frame-title` is off. Off and 0 under tmux
    // identity (`tmux_default`).
    Builtin {
        name: "pane-frames",
        doc: "Give each tiled pane its own rounded frame with its title, gaps between them; off gives tmux's shared borders.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-gap",
        doc: "Cells of gap between pane frames and around the outside.",
        kind: Kind::Number { max: 8 },
        scope: OptionScope::Window,
        default: "1",
        tmux_default: Some("0"),
        ..Builtin::PLAIN
    },
    // grofe's own: how many hidden scratchpads the server keeps.
    Builtin {
        name: "scratchpad-history",
        doc: "How many hidden scratchpads to keep; the oldest closes past this, 0 closes one as soon as it is hidden.",
        kind: Kind::Number { max: 100 },
        scope: OptionScope::Global,
        default: "10",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-frame-title",
        doc: "Draw the pane's number, title and controls in its frame's top edge.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    // The two end glyphs of a framed title's control tab and pane index
    // under grofe identity: brackets, which any terminal font has; a Nerd
    // font's thin rounded `\u{e0b7}\u{e0b5}` are the usual choice there,
    // and anything but two one-cell glyphs gives blank ends.
    Builtin {
        name: "pane-frame-tab-ends",
        doc: "Two glyphs drawn around the frame's number and controls; empty draws none.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "()",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-blocked",
        doc: "Glyph for an agent waiting on you, in the window list and the agents badge.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "⏸",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-error",
        doc: "Glyph for an agent that stopped with an error, in the window list and the agents badge.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "✗",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-unseen",
        doc: "Glyph for an agent that finished and has not been looked at, in the window list and the agents badge.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "✓",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-working",
        doc: "Glyph for a working agent, in the window list.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "●",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "notify-muted-glyph",
        doc: "Glyph the agents badge shows while notifications are muted.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "🔕",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-blocked-border-style",
        doc: "Border style of a pane whose agent is waiting on you, active or not.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "fg=yellow",
        ..Builtin::PLAIN
    },
    // Where the command prompt opens: on the status line, as tmux's, or as
    // a box in the middle of the window.
    Builtin {
        name: "prompt-position",
        doc: "Where the command prompt opens: on the status line (status) or as a box a third down the window (centre).",
        kind: Kind::Choice(&["status", "centre", "center"]),
        scope: OptionScope::Session,
        default: "status",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status",
        doc: "Show the status line, and how many rows it has (on is one row).",
        kind: Kind::Choice(&["off", "on", "2", "3", "4", "5"]),
        scope: OptionScope::Session,
        default: "on",
        toml: Some("status.lines"),
        toml_ty: Some(Ty::Int),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-position",
        doc: "Whether the status line is at the top or bottom of the window.",
        kind: Kind::Choice(&["top", "bottom"]),
        scope: OptionScope::Session,
        default: "bottom",
        toml: Some("status.position"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-style",
        doc: "Style of the status line.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=green,fg=black",
        toml: Some("status.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-left",
        doc: "Format for the left part of the status line.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "[#{session_name}] ",
        toml: Some("status.left"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-right",
        doc: "Format for the right part of the status line; grofe's default ends with the agents badge.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "\"#{pane_title}\" %H:%M %d-%b-%y#[range=user|agents]#{agents_badge}#[norange]",
        toml: Some("status.right"),
        tmux_default: Some("\"#{pane_title}\" %H:%M %d-%b-%y"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-left-length",
        doc: "The widest the left part of the status line may be, in cells.",
        kind: Kind::Number {
            max: i16::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "10",
        toml: Some("status.left-length"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-right-length",
        doc: "The widest the right part of the status line may be, in cells.",
        kind: Kind::Number {
            max: i16::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "40",
        toml: Some("status.right-length"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-interval",
        doc: "Seconds between status line refreshes and runs of its #() commands; 0 turns the timer off.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "15",
        toml: Some("status.interval"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-format",
        doc: "Format of a window's entry in the status line's window list; grofe's default adds the window's agent glyph.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}",
        toml: Some("status.window.format"),
        tmux_default: Some("#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-current-format",
        doc: "Format of the current window's entry in the window list; grofe's default adds the window's agent glyph.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}",
        toml: Some("status.window.current-format"),
        tmux_default: Some("#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }"),
        ..Builtin::PLAIN
    },
    // TC.11: tmux 3.7b's names, scopes, choices and defaults for the rest
    // of the status line (spec §4.5's status-line row).
    Builtin {
        name: "status-justify",
        doc: "Where the window list sits in the status line.",
        kind: Kind::Choice(&["left", "centre", "right", "absolute-centre"]),
        scope: OptionScope::Session,
        default: "left",
        toml: Some("status.justify"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-left-style",
        doc: "Style of the left part of the status line.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "default",
        toml: Some("status.left-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-right-style",
        doc: "Style of the right part of the status line.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "default",
        toml: Some("status.right-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-separator",
        doc: "Text drawn between windows in the window list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: " ",
        toml: Some("status.window.separator"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-style",
        doc: "Style of a window's entry in the window list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("status.window.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-current-style",
        doc: "Style of the current window's entry in the window list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("status.window.current-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-last-style",
        doc: "Style of the previously selected window's entry in the window list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("status.window.last-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-bell-style",
        doc: "Style of a window's entry in the window list when it has a bell alert.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "reverse",
        toml: Some("status.window.bell-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-style",
        doc: "Style of messages and of the command prompt.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=yellow,fg=black,fill=yellow",
        toml: Some("message.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-time",
        doc: "How long a message stays, in milliseconds.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "750",
        toml: Some("message.display-time-ms"),
        ..Builtin::PLAIN
    },
    // A message as a pill in the status line, coloured by its level, and
    // the prompt in the same style (grofe identity); `off` is tmux's
    // full-width bar in `message-style` (`tmux_default`).
    Builtin {
        name: "message-pill",
        doc: "Draw messages and the status-line prompt as a pill in the middle of the status line; off gives tmux's full-width bar.",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        toml: Some("message.pill"),
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    // The message and prompt pill's two end glyphs. Empty (the default)
    // gives square ends, which any font has; a Nerd font's rounded
    // `\u{e0b6}\u{e0b4}` match a pill status line.
    Builtin {
        name: "message-pill-ends",
        doc: "Two glyphs drawn at the ends of the message and prompt pill, in its colour; empty takes the theme's @pill_l and @pill_r, else Nerd Font round caps.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "",
        toml: Some("message.pill-ends"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-border-style",
        doc: "Style of the borders or frames of inactive panes.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("pane-border.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-active-border-style",
        doc: "Style of the active pane's border or frame.",
        kind: Kind::String,
        scope: OptionScope::Window,
        // tmux 3.7b's: yellow in a mode, red while panes are synchronized.
        default: "#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}",
        toml: Some("pane-border.active-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-border-format",
        doc: "Format of the pane status line shown in a border, and of a frame's title when it is changed from the default.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: PANE_BORDER_FORMAT,
        toml: Some("pane-border.format"),
        ..Builtin::PLAIN
    },
    // B1c: tmux 3.7b's names, values, window scope and defaults
    // (options-table.c). `pane-border-status` makes the panes along the
    // top or bottom edge give up a row to a title line.
    Builtin {
        name: "pane-border-status",
        doc: "Show a status line in the top or bottom border of each pane.",
        kind: Kind::Choice(&["off", "top", "bottom"]),
        scope: OptionScope::Window,
        default: "off",
        toml: Some("pane-border.status"),
        ..Builtin::PLAIN
    },
    // MF adds `rounded` (spec 2026-11-05-frames.md), grofe's default: a
    // frame's corners, drawn as `single` with frames off. tmux identity
    // keeps `single` (`tmux_default`).
    Builtin {
        name: "pane-border-lines",
        doc: "Characters borders and frames are drawn with.",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "number", "spaces", "rounded",
        ]),
        scope: OptionScope::Window,
        default: "rounded",
        toml: Some("pane-border.lines"),
        tmux_default: Some("single"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-border-indicators",
        doc: "How shared borders mark the active pane: by colour, with arrows, both or not at all.",
        kind: Kind::Choice(&["off", "colour", "arrows", "both"]),
        scope: OptionScope::Window,
        default: "colour",
        toml: Some("pane-border.indicators"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-keys",
        doc: "Key table the command prompt uses: emacs or vi.",
        kind: Kind::Choice(&["emacs", "vi"]),
        scope: OptionScope::Session,
        default: "emacs",
        toml: Some("status.keys"),
        ..Builtin::PLAIN
    },
    // The server's identity override (spec D12), read once at start.
    Builtin {
        name: "compat-identity",
        doc: "Whether the server behaves as grofe or as tmux; auto decides by the name it was started under. Fixed when the server starts.",
        kind: Kind::Choice(&["auto", "tmux", "grofe"]),
        scope: OptionScope::Global,
        default: "auto",
        ..Builtin::PLAIN
    },
    // G3: kitty direct placements, animation and sixel (grofe's own name,
    // like `compat-identity`; `docs/graphics-placement.md`). Off, the
    // clients get what T3.8 sent.
    Builtin {
        name: "graphics",
        doc: "Enable kitty graphics direct placements, animation and sixel (Unicode placeholders work without it).",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
        ..Builtin::PLAIN
    },
    // TC.12: tmux 3.7b's name, values, window scope and default, so the
    // `set -p allow-passthrough on` that icat, snacks.nvim and canopy send
    // works and `show` answers.
    // ponytail: a no-op. grofe always unwraps `DCS tmux;` and lets out only
    // what the same bytes sent bare would (passthrough.rs), so there is
    // nothing for `off` to guard; canopy turns it on anyway.
    Builtin {
        name: "allow-passthrough",
        doc: "Let programs pass escape sequences through to the outer terminal: off, on (visible panes only) or all.",
        kind: Kind::Choice(&["off", "on", "all"]),
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    // Window alerts (`server::alerts`, TP.10), with tmux's defaults.
    Builtin {
        name: "monitor-bell",
        doc: "Flag a window that rings the bell.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "monitor-activity",
        doc: "Flag a window that has output while you are not looking at it.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "monitor-silence",
        doc: "Flag a window that has had no output for this many seconds; 0 turns it off.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "bell-action",
        doc: "Which windows' bells are acted on: any, none, the current one or the others.",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "any",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "activity-action",
        doc: "Which windows' activity alerts are acted on: any, none, the current one or the others.",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "other",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "silence-action",
        doc: "Which windows' silence alerts are acted on: any, none, the current one or the others.",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "other",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "visual-bell",
        doc: "Show a message for a bell (on), pass the bell to the terminal (off), or both.",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "visual-activity",
        doc: "Show a message for an activity alert (on), ring the bell (off), or both.",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "visual-silence",
        doc: "Show a message for a silence alert (on), ring the bell (off), or both.",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    // TM.4: tmux's server option, at grofe's global scope (as
    // `escape-time`); at least 1.
    Builtin {
        name: "buffer-limit",
        doc: "How many automatically named paste buffers to keep; the oldest is dropped past this.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "50",
        ..Builtin::PLAIN
    },
    // B1d: clock mode's colour and time format, tmux 3.7b's names, choices
    // and defaults (`4` is blue; `show` prints it so).
    Builtin {
        name: "clock-mode-colour",
        doc: "Colour of the clock in clock mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "blue",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "clock-mode-style",
        doc: "Whether clock mode shows a 12 or 24 hour clock, with or without seconds.",
        kind: Kind::Choice(&["12", "24", "12-with-seconds", "24-with-seconds"]),
        scope: OptionScope::Window,
        default: "24",
        ..Builtin::PLAIN
    },
    // Copy mode's options, with tmux 3.7b's scopes and defaults
    // (`copy-command` is a server option there, global here).
    Builtin {
        name: "word-separators",
        doc: "Characters that end a word for copy mode's word motions and double-click selection.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "wrap-search",
        doc: "Whether a copy-mode search wraps around at the top or bottom of the history.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-command",
        doc: "A shell command that copy mode's copy-pipe commands send the copied text to when they are given none.",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "mode-style",
        doc: "Style of copy mode and of the choose and tree modes, including the selection.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "noattr,bg=yellow,fg=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-match-style",
        doc: "Style of search matches in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=cyan,fg=black",
        toml: Some("copy-mode.match-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-current-match-style",
        doc: "Style of the current search match in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=magenta,fg=black",
        toml: Some("copy-mode.current-match-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prefix-timeout",
        doc: "Milliseconds after the prefix before it is cancelled; 0 waits forever.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "0",
        ..Builtin::PLAIN
    },
    // B2a: the options tmux 3.7b has that grofe rejected, with tmux's kinds,
    // scopes and defaults (options-table.c). Those in `NO_EFFECT` are stored
    // only; the rest are read where the comment on `NO_EFFECT` says.
    Builtin {
        name: "assume-paste-time",
        doc: "Keys arriving faster than this many milliseconds apart are taken as a paste, not bindings.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "destroy-unattached",
        doc: "Close a session when its last client detaches: off, on, keep-last or keep-group.",
        kind: Kind::Choice(&["off", "on", "keep-last", "keep-group"]),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-panes-active-colour",
        doc: "Colour of the active pane's number in display-panes.",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "red",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-panes-colour",
        doc: "Colour of the other panes' numbers in display-panes.",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "blue",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-panes-time",
        doc: "Milliseconds display-panes shows the pane numbers for.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1000",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "initial-repeat-time",
        doc: "Repeat window, in milliseconds, after the first press of a repeatable key; 0 uses repeat-time.",
        kind: Kind::Number { max: 2_000_000 },
        scope: OptionScope::Session,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "key-table",
        doc: "The key table a client starts in.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "root",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "lock-after-time",
        doc: "Lock a session after this many seconds without input; 0 never locks.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "lock-command",
        doc: "Command run to lock a client.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "lock -np",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-command-style",
        doc: "Style of the command prompt while it is in vi command mode.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=black,fg=yellow,fill=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-format",
        doc: "Format of messages and the prompt line.",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "#[#{?#{command_prompt},#{E:message-command-style},#{E:message-style}}]#{message}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-line",
        doc: "Which status row messages and the prompt use.",
        kind: Kind::Choice(&["0", "1", "2", "3", "4"]),
        scope: OptionScope::Session,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-cursor-colour",
        doc: "Cursor colour in the command prompt.",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "none",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-cursor-style",
        doc: "Cursor shape in the command prompt.",
        kind: Kind::Choice(&[
            "default",
            "blinking-block",
            "block",
            "blinking-underline",
            "underline",
            "blinking-bar",
            "bar",
        ]),
        scope: OptionScope::Session,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-command-cursor-style",
        doc: "Cursor shape in the command prompt's vi command mode.",
        kind: Kind::Choice(&[
            "default",
            "blinking-block",
            "block",
            "blinking-underline",
            "underline",
            "blinking-bar",
            "bar",
        ]),
        scope: OptionScope::Session,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "cursor-colour",
        doc: "Cursor colour in panes.",
        kind: Kind::Colour,
        scope: OptionScope::Window,
        default: "none",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "cursor-style",
        doc: "Cursor shape in panes.",
        kind: Kind::Choice(&[
            "default",
            "blinking-block",
            "block",
            "blinking-underline",
            "underline",
            "blinking-bar",
            "bar",
        ]),
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-style",
        doc: "Style of menus.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-selected-style",
        doc: "Style of a menu's selected item.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=yellow,fg=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-border-style",
        doc: "Style of a menu's border.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-border-lines",
        doc: "Characters a menu's border is drawn with.",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "rounded", "padded", "none",
        ]),
        scope: OptionScope::Window,
        default: "single",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-status-current-style",
        doc: "Style of the current pane's entry in a status-format pane list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-status-style",
        doc: "Style of a pane's entry in a status-format pane list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "session-status-current-style",
        doc: "Style of the current session's entry in a status-format session list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "session-status-style",
        doc: "Style of a session's entry in a status-format session list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "allow-set-title",
        doc: "Let a program set its pane's title with an escape sequence.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "alternate-screen",
        doc: "Let programs use the alternate screen.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-mark-style",
        doc: "Style of the marked line in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=red,fg=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-position-format",
        doc: "Format of the position indicator in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#[align=right]#{t/p:top_line_time}#{?#{e|>:#{top_line_time},0}, ,}[#{copy_position}/#{copy_position_limit}]#{?search_timed_out, (timed out),#{?search_count, (#{search_count}#{?search_count_partial,+,} results),}}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-position-style",
        doc: "Style of the position indicator in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{E:mode-style}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-selection-style",
        doc: "Style of the selection in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{E:mode-style}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-current-line-number-style",
        doc: "Style of the current line's number in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=yellow",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-line-number-style",
        doc: "Style of line numbers in copy mode.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=white,dim",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-line-numbers",
        doc: "Show line numbers in copy mode, and which kind.",
        kind: Kind::Choice(&["off", "default", "absolute", "relative", "hybrid"]),
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "fill-character",
        doc: "Character to fill unused parts of a window with; empty is a space.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "other-pane-height",
        doc: "Height of the other panes in the main-horizontal layout; 0 shares the space.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "other-pane-width",
        doc: "Width of the other panes in the main-vertical layout; 0 shares the space.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-scrollbars",
        doc: "Show scrollbars in panes: off, in copy mode only (modal) or always.",
        kind: Kind::Choice(&["off", "modal", "on"]),
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-scrollbars-style",
        doc: "Style and width of pane scrollbars.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=black,fg=white,width=1,pad=0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-scrollbars-position",
        doc: "Which side of the pane its scrollbar is on.",
        kind: Kind::Choice(&["right", "left"]),
        scope: OptionScope::Window,
        default: "right",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "popup-style",
        doc: "Style of popups.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "popup-border-style",
        doc: "Style of a popup's border.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "popup-border-lines",
        doc: "Characters a popup's border is drawn with.",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "rounded", "padded", "none",
        ]),
        scope: OptionScope::Window,
        default: "single",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "remain-on-exit-format",
        doc: "Text shown at the bottom of a pane kept open by remain-on-exit.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "Pane is dead (#{?#{!=:#{pane_dead_status},},status #{pane_dead_status},}#{?#{!=:#{pane_dead_signal},},signal #{pane_dead_signal},}, #{t:pane_dead_time})",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "scroll-on-clear",
        doc: "Push the screen into the history when a program clears it.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "tiled-layout-max-columns",
        doc: "The most columns the tiled layout uses; 0 has no limit.",
        kind: Kind::Number { max: 65_535 },
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "tree-mode-preview-format",
        doc: "Format of the labels in choose-tree's preview.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{?pane_format,#{pane_index}:#{pane_title},#{window_index}:#{window_name}}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "tree-mode-preview-style",
        doc: "Style of the labels in choose-tree's preview.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=#{?#{||:#{&&:#{pane_format},#{pane_active}},#{&&:#{window_format},#{window_active}}},#{display-panes-active-colour},#{display-panes-colour}}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-pane-current-status-format",
        doc: "Format of the current pane's entry in a status-format pane list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#P:[#T]#{?pane_flags,#{pane_flags}, }",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-pane-status-format",
        doc: "Format of a pane's entry in a status-format pane list.",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#P:[#T]#{?pane_flags,#{pane_flags}, }",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "xterm-keys",
        doc: "Send function keys with modifiers in xterm's encoding.",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "backspace",
        doc: "The key sent for Backspace.",
        kind: Kind::Key,
        scope: OptionScope::Global,
        default: "C-?",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-client-command",
        doc: "Command run when grofe is started with no command.",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "new-session",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "editor",
        doc: "Editor run by commands that open one.",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "/usr/bin/vi",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "exit-unattached",
        doc: "Stop the server when no client is attached.",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "get-clipboard",
        doc: "Whether a program's OSC 52 clipboard query is answered. Only buffer answers it, from the newest paste buffer, and only when set-clipboard is on; off, request and both are never answered, and the terminal's clipboard is never asked.",
        kind: Kind::Choice(&["off", "buffer", "request", "both"]),
        scope: OptionScope::Global,
        default: "buffer",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "history-file",
        doc: "File the command prompt's history is saved to; empty saves none.",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "input-buffer-size",
        doc: "Most bytes one escape sequence from a program may hold.",
        kind: Kind::Number { max: 4_294_967_295 },
        scope: OptionScope::Global,
        default: "1048576",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-limit",
        doc: "How many messages to keep in the message log.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "1000",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-history-limit",
        doc: "How many command prompt entries to keep in its history.",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "100",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "variation-selector-always-wide",
        doc: "Treat a character followed by variation selector 16 as two cells wide.",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
        ..Builtin::PLAIN
    },
];

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
