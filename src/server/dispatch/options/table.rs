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
    "prompt-position",
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
        kind: Kind::Key,
        scope: OptionScope::Session,
        default: "C-b",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "base-index",
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
        kind: Kind::Number { max: 2_000_000 },
        scope: OptionScope::Session,
        default: "500",
        toml: Some("repeat-time-ms"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "history-limit",
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
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: REMAIN_ON_EXIT,
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
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "restore-stagger-ms",
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
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "resurrect-dir",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "automatic-rename",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "automatic-rename-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{b:pane_current_path}",
        tmux_default: Some("#{?pane_in_mode,[tmux],#{pane_current_command}}#{?pane_dead,[dead],}"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "allow-rename",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "set-titles",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "set-titles-string",
        kind: Kind::String,
        scope: OptionScope::Session,
        // tmux's default, `#S:#I:#W - "#T" #{session_alerts}`, translated
        // into grofe's `#{...}`-only expander (R15); `session_alerts`
        // isn't implemented and expands empty, matching tmux with no
        // alerts set.
        default: "#{session_name}:#{window_index}:#{window_name} - \"#{pane_title}\" #{session_alerts}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "set-clipboard",
        kind: Kind::Choice(&["off", "external", "on"]),
        scope: OptionScope::Global,
        default: "external",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "focus-events",
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
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-command",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-shell",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "/bin/sh",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-terminal",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "tmux-256color",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "extended-keys",
        kind: Kind::Choice(&["off", "on", "always"]),
        scope: OptionScope::Global,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "extended-keys-format",
        kind: Kind::Choice(&["csi-u", "xterm"]),
        scope: OptionScope::Global,
        default: "xterm",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "mode-keys",
        kind: Kind::Choice(&["emacs", "vi"]),
        scope: OptionScope::Window,
        default: "vi",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "renumber-windows",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "synchronize-panes",
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
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-active-style",
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
        kind: Kind::Key,
        scope: OptionScope::Session,
        default: "None",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-bg",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-fg",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-activity-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "reverse",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "focus-follows-mouse",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-size",
        kind: Kind::Size,
        scope: OptionScope::Session,
        default: "80x24",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "exit-empty",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "detach-on-destroy",
        kind: Kind::Choice(&["off", "on", "no-detached", "previous", "next"]),
        scope: OptionScope::Session,
        default: "on",
        ..Builtin::PLAIN
    },
    // TC.2b: tmux's name, scope, default and free text (tmux 3.7b stores
    // any string; `select-layout` reads cells or `N%`, TC.8).
    Builtin {
        name: "main-pane-width",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "80",
        ..Builtin::PLAIN
    },
    // TC.8: as `main-pane-width`, for the `main-horizontal` presets.
    Builtin {
        name: "main-pane-height",
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
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-gap",
        kind: Kind::Number { max: 8 },
        scope: OptionScope::Window,
        default: "1",
        tmux_default: Some("0"),
        ..Builtin::PLAIN
    },
    // grofe's own: how many hidden scratchpads the server keeps.
    Builtin {
        name: "scratchpad-history",
        kind: Kind::Number { max: 100 },
        scope: OptionScope::Global,
        default: "10",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-frame-title",
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
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "()",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-blocked",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "⏸",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-error",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "✗",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-unseen",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "✓",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-glyph-working",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "●",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "notify-muted-glyph",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "🔕",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "agent-blocked-border-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "fg=yellow",
        ..Builtin::PLAIN
    },
    // Where the command prompt opens: on the status line, as tmux's, or as
    // a box in the middle of the window.
    Builtin {
        name: "prompt-position",
        kind: Kind::Choice(&["status", "centre", "center"]),
        scope: OptionScope::Session,
        default: "status",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status",
        kind: Kind::Choice(&["off", "on", "2", "3", "4", "5"]),
        scope: OptionScope::Session,
        default: "on",
        toml: Some("status.lines"),
        toml_ty: Some(Ty::Int),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-position",
        kind: Kind::Choice(&["top", "bottom"]),
        scope: OptionScope::Session,
        default: "bottom",
        toml: Some("status.position"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=green,fg=black",
        toml: Some("status.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-left",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "[#{session_name}] ",
        toml: Some("status.left"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-right",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "\"#{pane_title}\" %H:%M %d-%b-%y#[range=user|agents]#{agents_badge}#[norange]",
        toml: Some("status.right"),
        tmux_default: Some("\"#{pane_title}\" %H:%M %d-%b-%y"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-left-length",
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
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }#{?#{==:#{window_agent_state},blocked},#{agent-glyph-blocked},#{?#{==:#{window_agent_state},error},#{agent-glyph-error},#{?#{==:#{window_agent_state},unseen},#{agent-glyph-unseen},#{?#{==:#{window_agent_state},working},#{agent-glyph-working},}}}}",
        toml: Some("status.window.format"),
        tmux_default: Some("#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-current-format",
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
        kind: Kind::Choice(&["left", "centre", "right", "absolute-centre"]),
        scope: OptionScope::Session,
        default: "left",
        toml: Some("status.justify"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-left-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "default",
        toml: Some("status.left-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-right-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "default",
        toml: Some("status.right-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-separator",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: " ",
        toml: Some("status.window.separator"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("status.window.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-current-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("status.window.current-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-last-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("status.window.last-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-status-bell-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "reverse",
        toml: Some("status.window.bell-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=yellow,fg=black,fill=yellow",
        toml: Some("message.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-time",
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
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
        toml: Some("message.pill"),
        tmux_default: Some("off"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        toml: Some("pane-border.style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-active-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        // tmux 3.7b's: yellow in a mode, red while panes are synchronized.
        default: "#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}",
        toml: Some("pane-border.active-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-border-format",
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
        kind: Kind::Choice(&["off", "colour", "arrows", "both"]),
        scope: OptionScope::Window,
        default: "colour",
        toml: Some("pane-border.indicators"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "status-keys",
        kind: Kind::Choice(&["emacs", "vi"]),
        scope: OptionScope::Session,
        default: "emacs",
        toml: Some("status.keys"),
        ..Builtin::PLAIN
    },
    // The server's identity override (spec D12), read once at start.
    Builtin {
        name: "compat-identity",
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
        kind: Kind::Choice(&["off", "on", "all"]),
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    // Window alerts (`server::alerts`, TP.10), with tmux's defaults.
    Builtin {
        name: "monitor-bell",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "monitor-activity",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "monitor-silence",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "bell-action",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "any",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "activity-action",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "other",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "silence-action",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "other",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "visual-bell",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "visual-activity",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "visual-silence",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    // TM.4: tmux's server option, at grofe's global scope (as
    // `escape-time`); at least 1.
    Builtin {
        name: "buffer-limit",
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
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "blue",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "clock-mode-style",
        kind: Kind::Choice(&["12", "24", "12-with-seconds", "24-with-seconds"]),
        scope: OptionScope::Window,
        default: "24",
        ..Builtin::PLAIN
    },
    // Copy mode's options, with tmux 3.7b's scopes and defaults
    // (`copy-command` is a server option there, global here).
    Builtin {
        name: "word-separators",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "wrap-search",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-command",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "mode-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "noattr,bg=yellow,fg=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-match-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=cyan,fg=black",
        toml: Some("copy-mode.match-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-current-match-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=magenta,fg=black",
        toml: Some("copy-mode.current-match-style"),
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prefix-timeout",
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
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "destroy-unattached",
        kind: Kind::Choice(&["off", "on", "keep-last", "keep-group"]),
        scope: OptionScope::Session,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-panes-active-colour",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "red",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-panes-colour",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "blue",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "display-panes-time",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1000",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "initial-repeat-time",
        kind: Kind::Number { max: 2_000_000 },
        scope: OptionScope::Session,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "key-table",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "root",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "lock-after-time",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "lock-command",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "lock -np",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-command-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=black,fg=yellow,fill=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-format",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "#[#{?#{command_prompt},#{E:message-command-style},#{E:message-style}}]#{message}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-line",
        kind: Kind::Choice(&["0", "1", "2", "3", "4"]),
        scope: OptionScope::Session,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-cursor-colour",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "none",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-cursor-style",
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
        kind: Kind::Colour,
        scope: OptionScope::Window,
        default: "none",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "cursor-style",
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
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-selected-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=yellow,fg=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "menu-border-lines",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "rounded", "padded", "none",
        ]),
        scope: OptionScope::Window,
        default: "single",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-status-current-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-status-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "session-status-current-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "session-status-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "allow-set-title",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "alternate-screen",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-mark-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=red,fg=black",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-position-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#[align=right]#{t/p:top_line_time}#{?#{e|>:#{top_line_time},0}, ,}[#{copy_position}/#{copy_position_limit}]#{?search_timed_out, (timed out),#{?search_count, (#{search_count}#{?search_count_partial,+,} results),}}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-position-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{E:mode-style}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-selection-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{E:mode-style}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-current-line-number-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=yellow",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-line-number-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=white,dim",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "copy-mode-line-numbers",
        kind: Kind::Choice(&["off", "default", "absolute", "relative", "hybrid"]),
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "fill-character",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "other-pane-height",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "other-pane-width",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-scrollbars",
        kind: Kind::Choice(&["off", "modal", "on"]),
        scope: OptionScope::Window,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-scrollbars-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=black,fg=white,width=1,pad=0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "pane-scrollbars-position",
        kind: Kind::Choice(&["right", "left"]),
        scope: OptionScope::Window,
        default: "right",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "popup-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "popup-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "popup-border-lines",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "rounded", "padded", "none",
        ]),
        scope: OptionScope::Window,
        default: "single",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "remain-on-exit-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "Pane is dead (#{?#{!=:#{pane_dead_status},},status #{pane_dead_status},}#{?#{!=:#{pane_dead_signal},},signal #{pane_dead_signal},}, #{t:pane_dead_time})",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "scroll-on-clear",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "tiled-layout-max-columns",
        kind: Kind::Number { max: 65_535 },
        scope: OptionScope::Window,
        default: "0",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "tree-mode-preview-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{?pane_format,#{pane_index}:#{pane_title},#{window_index}:#{window_name}}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "tree-mode-preview-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=#{?#{||:#{&&:#{pane_format},#{pane_active}},#{&&:#{window_format},#{window_active}}},#{display-panes-active-colour},#{display-panes-colour}}",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-pane-current-status-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#P:[#T]#{?pane_flags,#{pane_flags}, }",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "window-pane-status-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#P:[#T]#{?pane_flags,#{pane_flags}, }",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "xterm-keys",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "backspace",
        kind: Kind::Key,
        scope: OptionScope::Global,
        default: "C-?",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "default-client-command",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "new-session",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "editor",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "/usr/bin/vi",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "exit-unattached",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "get-clipboard",
        kind: Kind::Choice(&["off", "buffer", "request", "both"]),
        scope: OptionScope::Global,
        default: "buffer",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "history-file",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "input-buffer-size",
        kind: Kind::Number { max: 4_294_967_295 },
        scope: OptionScope::Global,
        default: "1048576",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "message-limit",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "1000",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "prompt-history-limit",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "100",
        ..Builtin::PLAIN
    },
    Builtin {
        name: "variation-selector-always-wide",
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
