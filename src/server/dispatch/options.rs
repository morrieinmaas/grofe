//! set-option/show-options scopes, persisted and user options, and the
//! model's format variables (spec §4.2, §4.5).
//!
//! [`Options`] is the server's one option store. A value lives at one
//! [`Owner`] (global, a session, a window or a pane); a lookup walks pane,
//! window, session, then global and takes the first value it finds. User
//! options (`@name`) are free-form strings; built-in options come from a
//! small typed table ([`BUILTINS`]) and always have a global value.
//!
//! [`FormatCtx`] implements [`FormatVars`] over the model for one pane, so
//! `-F`, `display-message -p` and `-c` expand against that pane, its
//! window and its session.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::num::IntErrorKind;
use std::path::{Path, PathBuf};

use crate::cmd::{
    BindKey, Command, DisplayMessage, FormatVars, Inner, ListKeys, ListPanes, ListSessions,
    ListWindows, LoopItem, LoopKind, LoopSort, NameKind, OptionScope, Reload, Resolve, SessionRef,
    SetOption, ShowOptions, SortOrder, SourceFile, Target, UnbindKey, expand, expand_time,
    split_command_string,
};
use crate::config::{self, Resolved, provenance::tilde};
use crate::event::Event;
use crate::protocol::{ClientCtx, Dialect};
use crate::server::borders::PaneStatus;
use crate::server::dispatch::sorting::{Order, Sort, by_name};
use crate::server::events::Events;
use crate::server::input::{Binding, BindingSource, Key, KeyTables, parse_key};
use crate::server::model::Model;
use crate::server::pane::procinfo::{self, ProcSample};
use crate::server::{State, clients};
use crate::types::{PaneId, SessionId, WindowId};

pub const REMAIN_ON_EXIT: &str = "remain-on-exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
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
struct Builtin {
    name: &'static str,
    kind: Kind,
    scope: OptionScope,
    default: &'static str,
}

// ponytail: only the options M1 to M3 need. `history-limit`, `base-index`
// and `pane-base-index` take effect through the model (T3.5).
//
// M2 rows use tmux's names and scopes (tmux 3.7b bounds). Their defaults are
// tmux's except where grofe differs on purpose or M2 formats cannot yet say
// the same thing: `escape-time` 25 (spec §5.3), `automatic-rename-format`
// `#{b:pane_current_path}` (spec §4.5), and `status-right`'s pane title
// untrimmed (tmux's is `#{=21:pane_title}`; the native language has no `=`
// trim, so a title over 21 columns shows whole). The window formats are
// tmux's, `#I:#W` spelled out (TC.11, D8). tmux's escape-time is a server
// option; grofe's server scope is global.
//
// M3 rows (T3.0, R4) use tmux's names and scopes with spec §5.3's defaults
// where grofe differs: `mouse` on (tmux off), `focus-events` on (tmux
// off), `pane-active-border-style`
// `fg=green` (tmux's is a format that also colours copy mode and
// synchronized panes). `status-position` takes `top` from TC.11.
// `set-titles` (off) and `set-titles-string` match tmux's own defaults,
// the string translated into grofe's `#{...}`-only expander (R15).
/// tmux 3.7b's built-in defaults where grofe's differ (recorded: `show
/// -g`/`-s`/`-gw` on `-f /dev/null` with a clean environment), used under
/// tmux identity ([`Options::use_tmux_defaults`]); `status-keys` and
/// `mode-keys` follow `$EDITOR` there. Formats are tmux's where the native
/// language can say the same thing.
// ponytail: `status-right`'s `#{=21:pane_title}` and the format tmux uses
// for `pane-active-border-style` have no native spelling, so grofe's stay.
const TMUX_DEFAULTS: &[(&str, &str)] = &[
    ("base-index", "0"),
    ("pane-base-index", "0"),
    ("history-limit", "2000"),
    ("escape-time", "10"),
    ("mouse", "off"),
    ("focus-events", "off"),
    ("extended-keys", "off"),
    ("renumber-windows", "off"),
    (
        "automatic-rename-format",
        "#{?pane_in_mode,[tmux],#{pane_current_command}}#{?pane_dead,[dead],}",
    ),
];

/// `bell-action`, `activity-action` and `silence-action`'s choices.
const ALERT_ACTIONS: &[&str] = &["none", "any", "current", "other"];

/// The `visual-*` options' choices.
const VISUAL: &[&str] = &["off", "on", "both"];

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "base-index",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1",
    },
    Builtin {
        name: "history-limit",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "50000",
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
    },
    // M5 stub: copy mode's options, with tmux 3.7b's scopes and defaults
    // (`copy-command` is a server option there, global here).
    Builtin {
        name: "word-separators",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~",
    },
    Builtin {
        name: "wrap-search",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "copy-command",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
    },
    Builtin {
        name: "mode-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "noattr,bg=yellow,fg=black",
    },
    Builtin {
        name: "copy-mode-match-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=cyan,fg=black",
    },
    Builtin {
        name: "copy-mode-current-match-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=magenta,fg=black",
    },
    Builtin {
        name: "pane-base-index",
        kind: Kind::Number {
            max: u16::MAX as i64,
        },
        scope: OptionScope::Window,
        default: "1",
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
    },
    Builtin {
        name: "status",
        kind: Kind::Choice(&["off", "on", "2", "3", "4", "5"]),
        scope: OptionScope::Session,
        default: "on",
    },
    Builtin {
        name: "status-left",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "[#{session_name}] ",
    },
    Builtin {
        name: "status-right",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "\"#{pane_title}\" %H:%M %d-%b-%y",
    },
    Builtin {
        name: "status-left-length",
        kind: Kind::Number {
            max: i16::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "10",
    },
    Builtin {
        name: "status-right-length",
        kind: Kind::Number {
            max: i16::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "40",
    },
    Builtin {
        name: "status-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=green,fg=black",
    },
    Builtin {
        name: "status-interval",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "15",
    },
    Builtin {
        name: "window-status-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }",
    },
    Builtin {
        name: "window-status-current-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{window_index}:#{window_name}#{?window_flags,#{window_flags}, }",
    },
    // TC.11: tmux 3.7b's names, scopes, choices and defaults for the rest
    // of the status line (spec §4.5's status-line row).
    Builtin {
        name: "status-justify",
        kind: Kind::Choice(&["left", "centre", "right", "absolute-centre"]),
        scope: OptionScope::Session,
        default: "left",
    },
    Builtin {
        name: "status-left-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "default",
    },
    Builtin {
        name: "status-right-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "default",
    },
    Builtin {
        name: "window-status-separator",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: " ",
    },
    Builtin {
        name: "window-status-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "window-status-current-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "window-status-last-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "window-status-bell-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "reverse",
    },
    // Window alerts (`server::alerts`, TP.10), with tmux's defaults.
    Builtin {
        name: "monitor-bell",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "monitor-activity",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
    },
    Builtin {
        name: "monitor-silence",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Window,
        default: "0",
    },
    Builtin {
        name: "bell-action",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "any",
    },
    Builtin {
        name: "activity-action",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "other",
    },
    Builtin {
        name: "silence-action",
        kind: Kind::Choice(ALERT_ACTIONS),
        scope: OptionScope::Session,
        default: "other",
    },
    Builtin {
        name: "visual-bell",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
    },
    Builtin {
        name: "visual-activity",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
    },
    Builtin {
        name: "visual-silence",
        kind: Kind::Choice(VISUAL),
        scope: OptionScope::Session,
        default: "off",
    },
    Builtin {
        name: "message-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=yellow,fg=black,fill=yellow",
    },
    Builtin {
        name: "display-time",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "750",
    },
    Builtin {
        name: "prefix",
        kind: Kind::Key,
        scope: OptionScope::Session,
        default: "C-b",
    },
    Builtin {
        name: "escape-time",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "25",
    },
    Builtin {
        name: "repeat-time",
        kind: Kind::Number { max: 2_000_000 },
        scope: OptionScope::Session,
        default: "500",
    },
    Builtin {
        name: "automatic-rename",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "automatic-rename-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{b:pane_current_path}",
    },
    Builtin {
        name: "allow-rename",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
    },
    Builtin {
        name: "set-titles",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "off",
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
    },
    Builtin {
        name: "mouse",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
    },
    Builtin {
        name: "set-clipboard",
        kind: Kind::Choice(&["off", "external", "on"]),
        scope: OptionScope::Global,
        default: "external",
    },
    Builtin {
        name: "focus-events",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
    },
    // B1d: clock mode's colour and time format, tmux 3.7b's names, choices
    // and defaults (`4` is blue; `show` prints it so).
    Builtin {
        name: "clock-mode-colour",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "blue",
    },
    Builtin {
        name: "clock-mode-style",
        kind: Kind::Choice(&["12", "24", "12-with-seconds", "24-with-seconds"]),
        scope: OptionScope::Window,
        default: "24",
    },
    Builtin {
        name: "pane-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "pane-active-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        // tmux 3.7b's: yellow in a mode, red while panes are synchronized.
        default: "#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}",
    },
    // B1c: tmux 3.7b's names, values, window scope and defaults
    // (options-table.c). `pane-border-status` makes the panes along the
    // top or bottom edge give up a row to a title line.
    Builtin {
        name: "pane-border-status",
        kind: Kind::Choice(&["off", "top", "bottom"]),
        scope: OptionScope::Window,
        default: "off",
    },
    Builtin {
        name: "pane-border-lines",
        kind: Kind::Choice(&["single", "double", "heavy", "simple", "number", "spaces"]),
        scope: OptionScope::Window,
        default: "single",
    },
    Builtin {
        name: "pane-border-indicators",
        kind: Kind::Choice(&["off", "colour", "arrows", "both"]),
        scope: OptionScope::Window,
        default: "colour",
    },
    Builtin {
        name: "status-position",
        kind: Kind::Choice(&["top", "bottom"]),
        scope: OptionScope::Session,
        default: "bottom",
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
    },
    // M3.6 (the stub edit): the server's identity override (spec D12).
    // TC.1 reads it once at start; until then nothing does.
    Builtin {
        name: "compat-identity",
        kind: Kind::Choice(&["auto", "tmux", "grofe"]),
        scope: OptionScope::Global,
        default: "auto",
    },
    // G3: kitty direct placements, animation and sixel (grofe's own name,
    // like `compat-identity`; `docs/graphics-placement.md`). Off, the
    // clients get what T3.8 sent.
    Builtin {
        name: "graphics",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
    },
    // TC.2b: tmux's name, scope, default and free text (tmux 3.7b stores
    // any string; `select-layout` reads cells or `N%`, TC.8).
    Builtin {
        name: "main-pane-width",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "80",
    },
    // TC.8: as `main-pane-width`, for the `main-horizontal` presets.
    Builtin {
        name: "main-pane-height",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "24",
    },
    // TC.8: tmux's `window-size`. `resize-window` sets `manual`, which
    // stops clients resizing the window; grofe sizes every other value as
    // `latest` (spec D6).
    Builtin {
        name: "window-size",
        kind: Kind::Choice(&["largest", "smallest", "manual", "latest"]),
        scope: OptionScope::Window,
        default: "latest",
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
    // Copy mode reads `mode-keys` (M5).
    Builtin {
        name: "aggressive-resize",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
    },
    Builtin {
        name: "default-command",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "",
    },
    Builtin {
        name: "default-shell",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "/bin/sh",
    },
    Builtin {
        name: "default-terminal",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "tmux-256color",
    },
    Builtin {
        name: "extended-keys",
        kind: Kind::Choice(&["off", "on", "always"]),
        scope: OptionScope::Global,
        default: "on",
    },
    Builtin {
        name: "extended-keys-format",
        kind: Kind::Choice(&["csi-u", "xterm"]),
        scope: OptionScope::Global,
        default: "xterm",
    },
    Builtin {
        name: "mode-keys",
        kind: Kind::Choice(&["emacs", "vi"]),
        scope: OptionScope::Window,
        default: "vi",
    },
    Builtin {
        name: "pane-border-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{?pane_active,#[reverse],}#{pane_index}#[default] \"#{pane_title}\"\
                  #{?#{mouse},#[align=right]#[range=control|8][#{?#{window_zoomed_flag},u,z}]\
                  #[norange]#[range=control|9][x]#[norange],}",
    },
    Builtin {
        name: "renumber-windows",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "on",
    },
    Builtin {
        name: "status-keys",
        kind: Kind::Choice(&["emacs", "vi"]),
        scope: OptionScope::Session,
        default: "emacs",
    },
    Builtin {
        name: "synchronize-panes",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "off",
    },
    // FE.1: what `select-pane -P` sets on a pane, as tmux 3.7b does
    // (recorded; free text, unchecked). B1c draws them: the colours of the
    // cells in the default colours.
    Builtin {
        name: "window-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "window-active-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
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
    },
    Builtin {
        name: "status-bg",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "default",
    },
    Builtin {
        name: "status-fg",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "default",
    },
    Builtin {
        name: "window-status-activity-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "reverse",
    },
    Builtin {
        name: "focus-follows-mouse",
        kind: Kind::Bool,
        scope: OptionScope::Session,
        default: "off",
    },
    Builtin {
        name: "default-size",
        kind: Kind::Size,
        scope: OptionScope::Session,
        default: "80x24",
    },
    Builtin {
        name: "exit-empty",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
    },
    Builtin {
        name: "detach-on-destroy",
        kind: Kind::Choice(&["off", "on", "no-detached", "previous", "next"]),
        scope: OptionScope::Session,
        default: "on",
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
    },
    Builtin {
        name: "destroy-unattached",
        kind: Kind::Choice(&["off", "on", "keep-last", "keep-group"]),
        scope: OptionScope::Session,
        default: "off",
    },
    Builtin {
        name: "display-panes-active-colour",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "red",
    },
    Builtin {
        name: "display-panes-colour",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "blue",
    },
    Builtin {
        name: "display-panes-time",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "1000",
    },
    Builtin {
        name: "initial-repeat-time",
        kind: Kind::Number { max: 2_000_000 },
        scope: OptionScope::Session,
        default: "0",
    },
    Builtin {
        name: "key-table",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "root",
    },
    Builtin {
        name: "lock-after-time",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Session,
        default: "0",
    },
    Builtin {
        name: "lock-command",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "lock -np",
    },
    Builtin {
        name: "message-command-style",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "bg=black,fg=yellow,fill=black",
    },
    Builtin {
        name: "message-format",
        kind: Kind::String,
        scope: OptionScope::Session,
        default: "#[#{?#{command_prompt},#{E:message-command-style},#{E:message-style}}]#{message}",
    },
    Builtin {
        name: "message-line",
        kind: Kind::Choice(&["0", "1", "2", "3", "4"]),
        scope: OptionScope::Session,
        default: "0",
    },
    Builtin {
        name: "prompt-cursor-colour",
        kind: Kind::Colour,
        scope: OptionScope::Session,
        default: "none",
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
    },
    Builtin {
        name: "cursor-colour",
        kind: Kind::Colour,
        scope: OptionScope::Window,
        default: "none",
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
    },
    Builtin {
        name: "menu-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "menu-selected-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=yellow,fg=black",
    },
    Builtin {
        name: "menu-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "menu-border-lines",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "rounded", "padded", "none",
        ]),
        scope: OptionScope::Window,
        default: "single",
    },
    Builtin {
        name: "pane-status-current-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "pane-status-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "session-status-current-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "session-status-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "allow-set-title",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "alternate-screen",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "copy-mode-mark-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=red,fg=black",
    },
    Builtin {
        name: "copy-mode-position-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#[align=right]#{t/p:top_line_time}#{?#{e|>:#{top_line_time},0}, ,}[#{copy_position}/#{copy_position_limit}]#{?search_timed_out, (timed out),#{?search_count, (#{search_count}#{?search_count_partial,+,} results),}}",
    },
    Builtin {
        name: "copy-mode-position-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{E:mode-style}",
    },
    Builtin {
        name: "copy-mode-selection-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{E:mode-style}",
    },
    Builtin {
        name: "copy-mode-current-line-number-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=yellow",
    },
    Builtin {
        name: "copy-mode-line-number-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=white,dim",
    },
    Builtin {
        name: "copy-mode-line-numbers",
        kind: Kind::Choice(&["off", "default", "absolute", "relative", "hybrid"]),
        scope: OptionScope::Window,
        default: "off",
    },
    Builtin {
        name: "fill-character",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "",
    },
    Builtin {
        name: "other-pane-height",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "0",
    },
    Builtin {
        name: "other-pane-width",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "0",
    },
    Builtin {
        name: "pane-scrollbars",
        kind: Kind::Choice(&["off", "modal", "on"]),
        scope: OptionScope::Window,
        default: "off",
    },
    Builtin {
        name: "pane-scrollbars-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "bg=black,fg=white,width=1,pad=0",
    },
    Builtin {
        name: "pane-scrollbars-position",
        kind: Kind::Choice(&["right", "left"]),
        scope: OptionScope::Window,
        default: "right",
    },
    Builtin {
        name: "popup-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "popup-border-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "default",
    },
    Builtin {
        name: "popup-border-lines",
        kind: Kind::Choice(&[
            "single", "double", "heavy", "simple", "rounded", "padded", "none",
        ]),
        scope: OptionScope::Window,
        default: "single",
    },
    Builtin {
        name: "remain-on-exit-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "Pane is dead (#{?#{!=:#{pane_dead_status},},status #{pane_dead_status},}#{?#{!=:#{pane_dead_signal},},signal #{pane_dead_signal},}, #{t:pane_dead_time})",
    },
    Builtin {
        name: "scroll-on-clear",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "tiled-layout-max-columns",
        kind: Kind::Number { max: 65_535 },
        scope: OptionScope::Window,
        default: "0",
    },
    Builtin {
        name: "tree-mode-preview-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{?pane_format,#{pane_index}:#{pane_title},#{window_index}:#{window_name}}",
    },
    Builtin {
        name: "tree-mode-preview-style",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "fg=#{?#{||:#{&&:#{pane_format},#{pane_active}},#{&&:#{window_format},#{window_active}}},#{display-panes-active-colour},#{display-panes-colour}}",
    },
    Builtin {
        name: "window-pane-current-status-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#P:[#T]#{?pane_flags,#{pane_flags}, }",
    },
    Builtin {
        name: "window-pane-status-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#P:[#T]#{?pane_flags,#{pane_flags}, }",
    },
    Builtin {
        name: "xterm-keys",
        kind: Kind::Bool,
        scope: OptionScope::Window,
        default: "on",
    },
    Builtin {
        name: "backspace",
        kind: Kind::Key,
        scope: OptionScope::Global,
        default: "C-?",
    },
    Builtin {
        name: "default-client-command",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "new-session",
    },
    Builtin {
        name: "editor",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "/usr/bin/vi",
    },
    Builtin {
        name: "exit-unattached",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "off",
    },
    Builtin {
        name: "get-clipboard",
        kind: Kind::Choice(&["off", "buffer", "request", "both"]),
        scope: OptionScope::Global,
        default: "buffer",
    },
    Builtin {
        name: "history-file",
        kind: Kind::String,
        scope: OptionScope::Global,
        default: "",
    },
    Builtin {
        name: "input-buffer-size",
        kind: Kind::Number { max: 4_294_967_295 },
        scope: OptionScope::Global,
        default: "1048576",
    },
    Builtin {
        name: "message-limit",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "1000",
    },
    Builtin {
        name: "prompt-history-limit",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "100",
    },
    Builtin {
        name: "prefix-timeout",
        kind: Kind::Number {
            max: i32::MAX as i64,
        },
        scope: OptionScope::Global,
        default: "0",
    },
    Builtin {
        name: "variation-selector-always-wide",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
    },
];

/// A built-in option's scope (a list's too), `None` for anything else.
pub fn builtin_scope(name: &str) -> Option<OptionScope> {
    builtin(name)
        .map(|b| b.scope)
        .or_else(|| list_builtin(name).map(|l| l.scope))
}

/// Every built-in option's name, in table order (T3.1 walks it to check
/// its TOML name table is complete).
pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    BUILTINS.iter().map(|b| b.name)
}

fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// Where an option value is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    Global,
    Session(SessionId),
    Window(WindowId),
    Pane(PaneId),
    /// tmux's server and global window scopes, for user options set
    /// through the tmux front end (FE.1): looked up after global.
    Server,
    GlobalWindow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub value: String,
    /// Set with `--persist`: M4 includes it in the snapshot.
    pub persist: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    values: BTreeMap<Owner, BTreeMap<String, Entry>>,
    /// Global options the config set, each with the value it had before
    /// (a built-in default or a start-time seed), so `reload` can put back
    /// a value the config no longer sets (R10).
    config_base: BTreeMap<String, String>,
    /// Global options set at runtime (`set -g`, `source-file`): the config
    /// no longer touches them (spec §5.1 layer 6, R10).
    runtime: BTreeSet<String>,
    /// List options (TC.2b), beside the scalar store so [`Entry`] keeps
    /// its shape.
    lists: BTreeMap<Owner, BTreeMap<String, ListEntry>>,
    /// Global lists the config set, each with the elements it had before,
    /// as `config_base` for scalars.
    config_list_base: BTreeMap<String, BTreeMap<u32, String>>,
    /// Built-in defaults that differ under the server's identity
    /// ([`Options::use_tmux_defaults`]): what `set -gu` restores.
    identity_defaults: BTreeMap<&'static str, String>,
    /// The same for list options.
    identity_list_defaults: BTreeMap<&'static str, Vec<String>>,
    /// What the tmux front end was sent for a value it rewrote into
    /// grofe's format spelling (FE.3), by owner and name (`status-format[1]`
    /// for an element): the stored value it stands for, and the text as
    /// sent. A tmux-dialect `show` prints the text while the value is
    /// still that one (continuum and canopy read values back).
    shown: BTreeMap<(Owner, String), (String, String)>,
}

/// tmux 3.7b's default `command-alias` entries (recorded: `show -gv
/// command-alias` on a `-f /dev/null` server), grofe's under tmux identity.
const TMUX_COMMAND_ALIASES: &[&str] = &[
    "split-pane=split-window",
    "splitp=split-window",
    "server-info=show-messages -JT",
    "info=show-messages -JT",
    "choose-window=choose-tree -w",
    "choose-session=choose-tree -s",
];

impl Options {
    /// The built-in defaults, with the global `remain-on-exit` seeded from
    /// the server's start options.
    pub fn new(remain_on_exit: bool) -> Options {
        let mut global: BTreeMap<String, Entry> = BUILTINS
            .iter()
            .map(|b| {
                let entry = Entry {
                    value: b.default.to_string(),
                    persist: false,
                };
                (b.name.to_string(), entry)
            })
            .collect();
        if let Some(e) = global.get_mut(REMAIN_ON_EXIT) {
            e.value = bool_str(remain_on_exit).to_string();
        }
        Options {
            values: BTreeMap::from([(Owner::Global, global)]),
            lists: BTreeMap::from([(Owner::Global, default_lists())]),
            ..Options::default()
        }
    }

    /// Under tmux identity (spec D12 ruling, FE.1): the built-in defaults
    /// where grofe's differ from tmux 3.7b's become tmux's
    /// ([`TMUX_DEFAULTS`]), so a script sees what tmux would. `editor` is
    /// the server's `$VISUAL`, else `$EDITOR`: tmux makes `status-keys`
    /// and `mode-keys` `vi` when its basename holds `vi` (recorded).
    /// A value the config set keeps winning, and falls back to tmux's on
    /// `reload` once the config stops setting it; a runtime value is left
    /// alone. Called once, at start.
    /// Makes `value` this server's built-in default for `name`: a value
    /// the config sets still wins, with this as its reload base, a runtime
    /// value is left alone, and `set -gu` restores it.
    pub fn set_default(&mut self, name: &'static str, value: String) {
        if let Some(base) = self.config_base.get_mut(name) {
            *base = value.clone();
        } else if !self.runtime.contains(name)
            && let Some(global) = self.values.get_mut(&Owner::Global)
        {
            global.insert(
                name.to_string(),
                Entry {
                    value: value.clone(),
                    persist: false,
                },
            );
        }
        self.identity_defaults.insert(name, value);
    }

    pub fn use_tmux_defaults(&mut self, editor: Option<&str>) {
        let keys = match editor.and_then(|e| e.rsplit('/').next()) {
            Some(e) if e.contains("vi") => "vi",
            _ => "emacs",
        };
        let defaults = TMUX_DEFAULTS
            .iter()
            .map(|(n, v)| (*n, v.to_string()))
            .chain([
                ("mode-keys", keys.to_string()),
                ("status-keys", keys.to_string()),
            ]);
        for (name, value) in defaults {
            self.set_default(name, value);
        }
        let aliases: BTreeMap<u32, String> = (0..)
            .zip(TMUX_COMMAND_ALIASES.iter().map(|a| a.to_string()))
            .collect();
        if let Some(base) = self.config_list_base.get_mut("command-alias") {
            *base = aliases.clone();
        } else if !self.runtime.contains("command-alias") {
            self.lists.entry(Owner::Global).or_default().insert(
                "command-alias".to_string(),
                ListEntry {
                    items: aliases,
                    persist: false,
                },
            );
        }
        self.identity_list_defaults.insert(
            "command-alias",
            TMUX_COMMAND_ALIASES.iter().map(|a| a.to_string()).collect(),
        );
    }

    /// Whether any value, at any owner, scalar or list element, holds
    /// `needle`.
    pub fn mentions(&self, needle: &str) -> bool {
        let scalars = self
            .values
            .values()
            .flat_map(|m| m.values().map(|e| &e.value));
        let lists = self
            .lists
            .values()
            .flat_map(|m| m.values().flat_map(|l| l.items.values()));
        scalars.chain(lists).any(|v| v.contains(needle))
    }

    /// Records the text a tmux-dialect request sent for `name`'s value
    /// at `owner` (see `shown`); `None`, or the value itself, forgets it.
    pub fn set_shown(&mut self, owner: Owner, name: &str, value: &str, sent: Option<String>) {
        let key = (owner, name.to_string());
        match sent {
            Some(sent) if sent != value => {
                self.shown.insert(key, (value.to_string(), sent));
            }
            _ => {
                self.shown.remove(&key);
            }
        }
    }

    /// The text sent for `name` at `owner`, while its value is `value`.
    pub fn shown(&self, owner: Owner, name: &str, value: &str) -> Option<&str> {
        self.shown
            .get(&(owner, name.to_string()))
            .filter(|(v, _)| v == value)
            .map(|(_, sent)| sent.as_str())
    }

    /// `name`'s built-in default under this server's identity.
    fn default_of(&self, b: &'static Builtin) -> &str {
        self.identity_defaults
            .get(b.name)
            .map_or(b.default, String::as_str)
    }

    /// Marks a global option as set at runtime, which the config then
    /// leaves alone.
    fn mark_runtime(&mut self, name: &str) {
        self.config_base.remove(name);
        self.config_list_base.remove(name);
        self.runtime.insert(name.to_string());
    }

    /// Sets `name` at `owner`. Returns whether the value changed: setting
    /// an option to its current value is a no-op (spec §4.2), so the
    /// caller emits `option-changed` only on `true`. The `--persist` flag
    /// is stored either way.
    pub fn set(
        &mut self,
        owner: Owner,
        name: &str,
        value: &str,
        persist: bool,
    ) -> Result<bool, String> {
        let value = validate(name, value)?;
        let at = self.values.entry(owner).or_default();
        if let Some(e) = at.get_mut(name) {
            e.persist = persist;
            if e.value == value {
                return Ok(false);
            }
            e.value = value;
            return Ok(true);
        }
        at.insert(name.to_string(), Entry { value, persist });
        Ok(true)
    }

    /// Removes `name` at `owner`; a built-in's global value goes back to
    /// its default instead. Returns whether anything changed.
    pub fn unset(&mut self, owner: Owner, name: &str) -> Result<bool, String> {
        check_name(name)?;
        if owner == Owner::Global
            && let Some(b) = builtin(name)
        {
            let default = self.default_of(b).to_string();
            return self.set(owner, name, &default, false);
        }
        let Some(at) = self.values.get_mut(&owner) else {
            return Ok(false);
        };
        let removed = at.remove(name).is_some();
        if at.is_empty() {
            self.values.remove(&owner);
        }
        Ok(removed)
    }

    /// The entry set at exactly `owner`.
    pub fn get(&self, owner: Owner, name: &str) -> Option<&Entry> {
        self.values.get(&owner)?.get(name)
    }

    /// Every entry set at exactly `owner`, by name.
    pub fn at(&self, owner: Owner) -> impl Iterator<Item = (&String, &Entry)> {
        self.values.get(&owner).into_iter().flatten()
    }

    /// The first value found walking `chain`, then global.
    pub fn lookup(&self, name: &str, chain: &[Owner]) -> Option<&str> {
        chain
            .iter()
            .chain([&Owner::GlobalWindow, &Owner::Global, &Owner::Server])
            .find_map(|o| self.get(*o, name))
            .map(|e| e.value.as_str())
    }

    /// Drops values set on sessions, windows or panes that no longer exist.
    /// Runtime ids are never reused within a server, so stale values never
    /// apply to anything; this only frees them.
    pub fn retain_live(&mut self, model: &Model) {
        self.values.retain(|owner, _| match owner {
            Owner::Global | Owner::Server | Owner::GlobalWindow => true,
            Owner::Session(id) => model.sessions.contains_key(id),
            Owner::Window(id) => model.windows.contains_key(id),
            Owner::Pane(id) => model.panes.contains_key(id),
        });
        self.lists.retain(|owner, _| match owner {
            Owner::Global | Owner::Server | Owner::GlobalWindow => true,
            Owner::Session(id) => model.sessions.contains_key(id),
            Owner::Window(id) => model.windows.contains_key(id),
            Owner::Pane(id) => model.panes.contains_key(id),
        });
    }
}

fn bool_str(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// An error unless `name` is a user option or a known built-in.
fn check_name(name: &str) -> Result<(), String> {
    if name.starts_with('@') || builtin(name).is_some() {
        Ok(())
    } else {
        Err(format!("invalid option: {name}"))
    }
}

/// The normalized value to store for `name`, or an error for an unknown
/// option or a value of the wrong type, worded as tmux words it.
fn validate(name: &str, value: &str) -> Result<String, String> {
    check_name(name)?;
    let Some(b) = builtin(name) else {
        return Ok(value.to_string());
    };
    match b.kind {
        // tmux reads on/yes/off/no in any case; `true` and `false` are
        // grofe's own, which `set_option` refuses in the tmux dialect.
        Kind::Bool => match value.to_ascii_lowercase().as_str() {
            "on" | "yes" | "true" | "1" => Ok("on".to_string()),
            "off" | "no" | "false" | "0" => Ok("off".to_string()),
            // tmux's wording for a flag option; choices say `unknown value`.
            _ => Err(format!("bad value: {value}")),
        },
        // tmux's `checkshell`: an absolute path it may execute, not tmux.
        Kind::String if name == "default-shell" => crate::server::pane::check_shell(value)
            .then(|| value.to_string())
            .ok_or_else(|| format!("not a suitable shell: {value}")),
        // tmux checks a style when it is set, unless it holds a format.
        Kind::String if STYLE_OPTIONS.contains(&name) && !value.contains("#{") => {
            if crate::server::status::style_valid(value) {
                Ok(value.to_string())
            } else {
                Err(format!("invalid style: {value}"))
            }
        }
        // A command option: the first word of each command must be one.
        Kind::String if name == "default-client-command" => {
            for argv in split_command_string(value).map_err(|e| e.to_string())? {
                if let Some(verb) = argv.first() {
                    crate::compat::tmux::resolve(verb).map_err(|e| e.0)?;
                }
            }
            Ok(value.to_string())
        }
        Kind::String => Ok(value.to_string()),
        Kind::Key => crate::server::input::parse_prefix(value)
            .map(|key| key.map_or_else(|| "None".to_string(), |key| key.to_string())),
        // The cursor colours default to "no colour" (-1), which tmux shows
        // as `none` but cannot be set to; `show-config` has to read its own
        // output back.
        Kind::Colour
            if value == "none" && matches!(name, "cursor-colour" | "prompt-cursor-colour") =>
        {
            Ok("none".to_string())
        }
        Kind::Colour => colour_text(value).ok_or_else(|| format!("bad colour: {value}")),
        Kind::Size => size_text(value)
            .then(|| value.to_string())
            .ok_or_else(|| format!("value is invalid: {value}")),
        Kind::Choice(words) => words
            .contains(&value)
            .then(|| value.to_string())
            .ok_or_else(|| format!("unknown value: {value}")),
        Kind::Number { max } => {
            let too_small = || format!("value is too small: {value}");
            let too_large = || format!("value is too large: {value}");
            let n = value.parse::<i64>().map_err(|e| match e.kind() {
                IntErrorKind::PosOverflow => too_large(),
                IntErrorKind::NegOverflow => too_small(),
                _ => format!("value is invalid: {value}"),
            })?;
            if n < number_minimum(name) {
                Err(too_small())
            } else if n > max {
                Err(too_large())
            } else {
                Ok(n.to_string())
            }
        }
    }
}

/// The options tmux checks as styles when they are set (`IS_STYLE`).
const STYLE_OPTIONS: &[&str] = &[
    "menu-style",
    "menu-selected-style",
    "menu-border-style",
    "message-command-style",
    "message-style",
    "status-left-style",
    "status-right-style",
    "status-style",
    "pane-status-current-style",
    "pane-status-style",
    "session-status-current-style",
    "session-status-style",
    "copy-mode-match-style",
    "copy-mode-current-match-style",
    "copy-mode-mark-style",
    "copy-mode-position-style",
    "copy-mode-selection-style",
    "copy-mode-current-line-number-style",
    "copy-mode-line-number-style",
    "mode-style",
    "pane-active-border-style",
    "pane-border-style",
    "pane-scrollbars-style",
    "popup-style",
    "popup-border-style",
    "tree-mode-preview-style",
    "window-active-style",
    "window-style",
    "window-status-activity-style",
    "window-status-bell-style",
    "window-status-current-style",
    "window-status-last-style",
    "window-status-style",
];

/// The least a number option takes (tmux's `minimum`; the rest take 0).
fn number_minimum(name: &str) -> i64 {
    match name {
        "buffer-limit" | "display-panes-time" => 1,
        "input-buffer-size" => 1_048_576,
        _ => 0,
    }
}

/// A colour option's value as tmux shows it: `default` and `terminal`, the
/// sixteen names in lower case, `colourN` for the 256 palette, and
/// `#rrggbb` in lower case. `None` for anything else (tmux also takes the
/// X11 colour names; grofe does not).
fn colour_text(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    const NAMES: &[&str] = &[
        "default",
        "terminal",
        "black",
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "white",
        "brightblack",
        "brightred",
        "brightgreen",
        "brightyellow",
        "brightblue",
        "brightmagenta",
        "brightcyan",
        "brightwhite",
    ];
    if NAMES.contains(&lower.as_str()) {
        return Some(lower);
    }
    if let Some(hex) = lower.strip_prefix('#') {
        let ok = hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit());
        return ok.then_some(lower);
    }
    let n = lower
        .strip_prefix("colour")
        .or_else(|| lower.strip_prefix("color"))?;
    n.parse::<u8>().ok().map(|n| format!("colour{n}"))
}

/// tmux's `[0-9]*x[0-9]*` (an fnmatch pattern): a digit first, then an `x`
/// followed by a digit somewhere after it.
fn size_text(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_digit)
        && bytes
            .windows(2)
            .skip(1)
            .any(|w| w[0] == b'x' && w[1].is_ascii_digit())
}

/// `default-size` as `(columns, rows)`, read as tmux's `%ux%u`.
pub(crate) fn parse_size(value: &str) -> Option<(u16, u16)> {
    let (cols, rows) = value.split_once('x')?;
    let digits = |s: &str| {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse::<u32>().ok()
    };
    let clamp = |n: u32| u16::try_from(n.clamp(1, 10_000)).unwrap_or(u16::MAX);
    Some((clamp(digits(cols)?), clamp(digits(rows)?)))
}

/// The scope a command acts on: the explicit flag, else the option's own
/// (user options default to session scope, as in tmux).
fn default_scope(scope: Option<OptionScope>, name: Option<&str>) -> OptionScope {
    scope.unwrap_or_else(|| {
        name.and_then(|n| {
            builtin(n)
                .map(|b| b.scope)
                .or_else(|| list_builtin(n).map(|l| l.scope))
        })
        .unwrap_or(OptionScope::Session)
    })
}

fn owner(
    model: &Model,
    scope: OptionScope,
    target: &Target,
    ctx: &ClientCtx,
) -> Result<Owner, String> {
    Ok(match scope {
        OptionScope::Global => Owner::Global,
        // tmux finds `-t` as a pane for every scope (cmd-set-option.c).
        OptionScope::Session => {
            Owner::Session(model.resolve_session(&model.pane_type(target, ctx)?, ctx)?)
        }
        OptionScope::Window => {
            Owner::Window(model.resolve_window(&model.pane_type(target, ctx)?, ctx)?)
        }
        OptionScope::Pane => Owner::Pane(model.resolve_pane(target, ctx)?),
        OptionScope::Server => Owner::Server,
        OptionScope::GlobalWindow => Owner::GlobalWindow,
    })
}

/// The owners a lookup for `pane` walks before global: pane, window,
/// session. Empty if the pane is gone.
pub fn chain(model: &Model, pane: PaneId) -> Vec<Owner> {
    let Some(p) = model.panes.get(&pane) else {
        return Vec::new();
    };
    let mut chain = vec![Owner::Pane(pane), Owner::Window(p.window)];
    if let Some(w) = model.windows.get(&p.window) {
        chain.push(Owner::Session(w.session));
    }
    chain
}

/// Runs an option or format command routed here by `dispatch::route`:
/// `set-option`, `show-options`, `display-message`, and `list-sessions`,
/// `list-windows` or `list-panes` given `-F`.
pub fn run(state: &mut State, ctx: &ClientCtx, cmd: Command) -> Result<String, String> {
    let model = &mut state.model;
    let mut options = std::mem::take(&mut model.options);
    options.retain_live(model);
    model.options = options;
    match cmd {
        Command::SetOption(mut c) => {
            if c.expand
                && let Some(value) = &c.value
            {
                c.value = Some(expand_value(state, ctx, &c.target, value));
            }
            set_option(&mut state.model, &mut state.events, ctx, c)
        }
        // A hook is an option in tmux: asked for by name, its elements.
        Command::ShowOptions(c)
            if c.name.as_deref().is_some_and(|n| {
                split_index(n).is_ok_and(|(base, _)| {
                    builtin(base).is_none() && crate::server::hooks::event(base).is_some()
                })
            }) =>
        {
            crate::server::dispatch::hooks::show_hooks(
                state,
                ctx,
                crate::cmd::ShowHooks {
                    global: c.scope == Some(OptionScope::Global),
                    window: c.scope == Some(OptionScope::Window),
                    pane: c.scope == Some(OptionScope::Pane),
                    target: c.target.clone(),
                    hook: c.name.clone(),
                    ..Default::default()
                },
            )
        }
        Command::ShowOptions(c) => {
            // `-H`: the hooks a whole listing of the scope has, after the
            // rest, as tmux's table keeps them last.
            let hooks = (c.hooks && c.name.is_none()).then(|| crate::cmd::ShowHooks {
                global: c.scope == Some(OptionScope::Global),
                window: c.scope == Some(OptionScope::Window),
                pane: c.scope == Some(OptionScope::Pane),
                target: c.target.clone(),
                ..Default::default()
            });
            let mut out = show_options(&state.model, ctx, c)?;
            if let Some(hooks) = hooks
                && let Ok(more) = crate::server::dispatch::hooks::show_hooks(state, ctx, hooks)
            {
                out.push_str(&more);
            }
            Ok(out)
        }
        Command::DisplayMessage(c) => display_message(state, ctx, c),
        Command::ListSessions(c) => list_sessions(state, c),
        Command::ListWindows(c) => list_windows(state, ctx, c),
        Command::ListPanes(c) => list_panes(state, ctx, c),
        _ => Err("not an option command".to_string()),
    }
}

/// `set -F`'s value, expanded as `display-message` expands a format:
/// against the target pane, with the current client's and the server's
/// variables (the mouse event's and a running hook's among them) over it.
fn expand_value(state: &State, ctx: &ClientCtx, target: &Target, value: &str) -> String {
    let model = &state.model;
    let Ok(pane) = model.resolve_pane(target, ctx) else {
        return expand(value, &NoVars);
    };
    let vars = FormatCtx::new(model, pane);
    match clients::current_client(state, ctx).and_then(|id| state.clients.get(&id)) {
        Some(client) => {
            let client = clients::ClientVars::new(model, client, Some(vars));
            expand(value, &ServerVars::fresh(state, pane, &client))
        }
        None => expand(value, &ServerVars::fresh(state, pane, &vars)),
    }
}

fn set_option(
    model: &mut Model,
    events: &mut Events,
    ctx: &ClientCtx,
    c: SetOption,
) -> Result<String, String> {
    // TC.11, as tmux 3.7b (recorded): `-q` hides an unknown or ambiguous
    // name and a target that does not resolve, never a bad value.
    let quiet = |r: Result<String, String>| if c.quiet { Ok(String::new()) } else { r };
    let (base, index) = match split_index(&c.name) {
        Ok(split) => split,
        Err(e) => return quiet(Err(e)),
    };
    if index.is_none() && list_builtin(base).is_none() && check_name(base).is_err() {
        return quiet(check_name(base).map(|_| String::new()));
    }
    if index.is_some() || list_builtin(base).is_some() {
        let base = base.to_string();
        return set_list(model, events, ctx, &c, &base, index);
    }
    let scope = default_scope(c.scope, Some(&c.name));
    let owner = match owner(model, scope, &c.target, ctx) {
        Ok(owner) => owner,
        Err(e) => return quiet(Err(e)),
    };
    if c.only_if_unset && !c.unset && model.options.get(owner, &c.name).is_some() {
        return quiet(Err(format!("already set: {}", c.name)));
    }
    // A runtime value at global scope takes the option out of the
    // config's hands (R10).
    let changed = |model: &mut Model, events: &mut Events, changed: bool| {
        if owner == Owner::Global {
            model.options.mark_runtime(&c.name);
        }
        if changed {
            option_changed(model, events, owner, &c.name);
        }
    };
    if c.unset {
        // `-U`: a window option goes from its panes too.
        if c.unset_panes
            && let Owner::Window(window) = owner
            && let Some(w) = model.windows.get(&window)
        {
            for pane in w.pane_ids() {
                let _ = model.options.unset(Owner::Pane(pane), &c.name);
                model
                    .options
                    .set_shown(Owner::Pane(pane), &c.name, "", None);
            }
        }
        let did = model.options.unset(owner, &c.name)?;
        model.options.set_shown(owner, &c.name, "", None);
        changed(model, events, did);
        return Ok(String::new());
    }
    let value = match c.value {
        Some(value) => value,
        // As tmux: a flag option given no value flips the value it has at
        // this scope, inherited or not.
        None if builtin(&c.name).is_some_and(|b| b.kind == Kind::Bool) => {
            let current = model.options.lookup(&c.name, &owner_chain(model, owner));
            bool_str(current != Some("on")).to_string()
        }
        // As tmux: a choice option given no value flips `off` and `on`
        // and leaves any other word as it is.
        None if builtin(&c.name).is_some_and(|b| matches!(b.kind, Kind::Choice(_))) => {
            let current = model.options.lookup(&c.name, &owner_chain(model, owner));
            match current {
                Some("off") => "on".to_string(),
                Some("on") => "off".to_string(),
                other => other.unwrap_or_default().to_string(),
            }
        }
        // An unknown name is reported before an absent value (tmux checks
        // the name first).
        None => {
            check_name(&c.name)?;
            return Err("empty value".to_string());
        }
    };
    // grofe's own dialect also reads `true` and `false` for a flag option;
    // tmux 3.7b refuses them (`options_from_string_flag`).
    let tmux = ctx.dialect == Dialect::Tmux;
    if tmux
        && builtin(&c.name).is_some_and(|b| b.kind == Kind::Bool)
        && matches!(value.to_ascii_lowercase().as_str(), "true" | "false")
    {
        return Err(format!("bad value: {value}"));
    }
    // `-a` appends to a string's value at this scope (tmux: style options
    // join with a comma); any other kind takes the value as given.
    let separator = if c.name.ends_with("-style") { "," } else { "" };
    // In the tmux dialect, the text as sent, for `show` (FE.3), kept only
    // when the front end rewrote the value, or when an append extends
    // text `show` already prints in place of the value. Any other value
    // shows as stored, normalized as tmux normalizes it (`mouse yes` is
    // `on`).
    let (value, sent) = match model.options.get(owner, &c.name) {
        Some(old) if c.append && is_text(&c.name) => {
            let before = tmux_text(&model.options, owner, &c.name, &old.value);
            let recorded = before != old.value;
            let piece = c.shown.clone().unwrap_or_else(|| value.clone());
            let sent = (tmux && (recorded || c.shown.is_some()))
                .then(|| format!("{before}{separator}{piece}"));
            (format!("{}{separator}{value}", old.value), sent)
        }
        _ => (value, c.shown.clone().filter(|_| tmux)),
    };
    let did = model.options.set(owner, &c.name, &value, c.persist)?;
    let stored = model
        .options
        .get(owner, &c.name)
        .map(|e| e.value.clone())
        .unwrap_or_default();
    model.options.set_shown(owner, &c.name, &stored, sent);
    changed(model, events, did);
    Ok(String::new())
}

/// Whether `name` holds free text (a user option or a string built-in),
/// which `-a` appends to.
fn is_text(name: &str) -> bool {
    name.starts_with('@') || builtin(name).is_some_and(|b| b.kind == Kind::String)
}

/// No format variables: `-F` with no pane to expand against.
struct NoVars;

impl FormatVars for NoVars {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

/// `option-changed`, emitted only for a real change (spec §4.2).
fn option_changed(model: &Model, events: &mut Events, owner: Owner, name: &str) {
    if name == "graphics" {
        crate::server::graphics::sync_option(model);
    }
    events.emit(Event::OptionChanged {
        scope: match owner {
            Owner::Global | Owner::Server | Owner::GlobalWindow => "global".to_string(),
            Owner::Session(id) => id.to_string(),
            Owner::Window(id) => id.to_string(),
            Owner::Pane(id) => id.to_string(),
        },
        name: name.to_string(),
        value: model.options.get(owner, name).map(|e| e.value.clone()),
    });
}

/// The owners a lookup at `owner` walks before global: `owner` and the
/// objects above it.
fn owner_chain(model: &Model, owner: Owner) -> Vec<Owner> {
    match owner {
        Owner::Global | Owner::Server | Owner::GlobalWindow => Vec::new(),
        Owner::Session(_) => vec![owner],
        Owner::Window(w) => {
            let mut chain = vec![owner];
            if let Some(w) = model.windows.get(&w) {
                chain.push(Owner::Session(w.session));
            }
            chain
        }
        Owner::Pane(p) => chain(model, p),
    }
}

fn show_options(model: &Model, ctx: &ClientCtx, c: ShowOptions) -> Result<String, String> {
    if let Some(name) = &c.name {
        let (base, index) = split_index(name)?;
        if index.is_some() || list_builtin(base).is_some() {
            return show_list(model, ctx, &c, name, base, index);
        }
    }
    let scope = default_scope(c.scope, c.name.as_deref());
    let owner = owner(model, scope, &c.target, ctx)?;
    if let Some(name) = &c.name {
        // As tmux: a user option not set at this scope is an error. `-q`
        // (recorded on tmux 3.7b) makes that, and an unknown option, an
        // empty answer.
        let known = check_name(name).and_then(|()| {
            if name.starts_with('@') && model.options.get(owner, name).is_none() {
                Err(format!("invalid option: {name}"))
            } else {
                Ok(())
            }
        });
        match known {
            Err(_) if c.quiet => return Ok(String::new()),
            known => known?,
        }
    }
    let tmux = ctx.dialect == Dialect::Tmux;
    if tmux
        && c.name.is_none()
        && matches!(owner, Owner::Global | Owner::Server | Owner::GlobalWindow)
    {
        return Ok(tmux_listing(model, owner, c.value_only));
    }
    // `-A` on a whole session or window table, in tmux's order.
    if tmux
        && c.inherited
        && c.name.is_none()
        && matches!(owner, Owner::Session(_) | Owner::Window(_))
    {
        return Ok(tmux_inherited_listing(model, owner, c.value_only));
    }
    // `-v` (tmux 3.7b): each value alone and unquoted; in the tmux
    // dialect, a value as it was sent. `inherited` is `-A`'s `name*`.
    let line = |name: &str, value: &str, inherited: bool| {
        let value = if tmux {
            tmux_text(&model.options, owner, name, value)
        } else {
            value.to_string()
        };
        let value = value.as_str();
        if c.value_only {
            format!("{value}\n")
        } else {
            // tmux prints a colour as colour_tostring does, unquoted.
            let colour = builtin(name).is_some_and(|b| b.kind == Kind::Colour);
            let shown = if colour {
                value.to_string()
            } else {
                quote(value)
            };
            let star = if inherited { "*" } else { "" };
            format!("{name}{star} {shown}\n")
        }
    };
    // The server table listed whole (tmux's bare `show -s`) is the
    // server's own options, which grofe keeps at global scope, and the
    // user options set with `-s`; what `-s` stored for any other option
    // shows only by name, as in tmux 3.7b.
    let (listed, only_server) = match (owner, &c.name) {
        (Owner::Server, None) => (Owner::Global, true),
        _ => (owner, false),
    };
    let wanted = |name: &str| !only_server || builtin_scope(name) == Some(OptionScope::Global);
    let mut lines: Vec<(&str, String)> = model
        .options
        .at(listed)
        .filter(|(name, _)| c.name.as_ref().is_none_or(|n| n == *name) && wanted(name))
        .map(|(name, e)| (name.as_str(), line(name, &e.value, false)))
        .collect();
    if only_server {
        lines.extend(
            model
                .options
                .at(Owner::Server)
                .filter(|(name, _)| name.starts_with('@'))
                .map(|(name, e)| (name.as_str(), line(name, &e.value, false))),
        );
    }
    // `-A`: what the scope does not set and inherits, from its parents, at
    // the scope's own table (tmux's table scopes for a pane or window).
    if c.inherited {
        let mine = match owner {
            Owner::Session(_) => Some(OptionScope::Session),
            Owner::Window(_) => Some(OptionScope::Window),
            Owner::Pane(_) => Some(OptionScope::Pane),
            _ => None,
        };
        if let Some(mine) = mine {
            let parents = &owner_chain(model, owner)[1..];
            let names: Vec<&str> = match &c.name {
                Some(n) => vec![n.as_str()],
                None => builtin_names()
                    .filter(|n| builtin_scope(n) == Some(mine))
                    .collect(),
            };
            for name in names {
                if model.options.get(owner, name).is_some() {
                    continue;
                }
                if let Some(value) = model.options.lookup(name, parents) {
                    lines.push((name, line(name, value, true)));
                }
            }
        }
    }
    if c.name.is_none() {
        lines.extend(
            model
                .options
                .lists_at(listed)
                .filter(|(name, _)| wanted(name))
                .map(|(name, l)| (name.as_str(), list_lines(name, l, c.value_only))),
        );
        lines.sort_by(|a, b| a.0.cmp(b.0));
    }
    Ok(lines.into_iter().map(|(_, l)| l).collect())
}

// ---- The tmux dialect's show (FE.3) -------------------------------------

/// Options whose default the tmux dialect shows as tmux's own text: grofe
/// spells the same default another way (`#{window_index}` for `#I`), or
/// has none of that shape (`status-format`'s rows).
const SPELLED_DEFAULTS: &[&str] = &[
    "pane-active-border-style",
    "set-titles-string",
    "status-format",
    "status-right",
    "window-status-current-format",
    "window-status-format",
];

/// tmux's default text for `name` (`base[N]` for an element) when the
/// global `value` (`None`: no such element) is grofe's default for it.
fn spelled_default(options: &Options, name: &str, value: Option<&str>) -> Option<String> {
    let (base, index) = split_index(name).ok()?;
    if !SPELLED_DEFAULTS.contains(&base) {
        return None;
    }
    let default = match index {
        None => builtin(base).map(|b| options.default_of(b)),
        Some(i) => list_builtin(base)?
            .default
            .get(usize::try_from(i).ok()?)
            .copied(),
    };
    if index.is_none() && default.is_none() {
        return None;
    }
    (default == value)
        .then(|| crate::compat::tmux::formats::tmux_default(name))
        .flatten()
}

/// `value`, `name`'s at `owner`, as the tmux dialect shows it: the text a
/// tmux request sent for it, tmux's own default text for a default grofe
/// spells another way, else the value.
pub(crate) fn tmux_text(options: &Options, owner: Owner, name: &str, value: &str) -> String {
    if let Some(sent) = options.shown(owner, name, value) {
        return sent.to_string();
    }
    if owner == Owner::Global
        && let Some(text) = spelled_default(options, name, Some(value))
    {
        return text;
    }
    value.to_string()
}

/// One `show` line for a value already in the tmux dialect's text.
fn line_raw(name: &str, value: &str, value_only: bool) -> String {
    if value_only {
        format!("{value}\n")
    } else {
        format!("{name} {}\n", quote(value))
    }
}

/// A global list's lines in the tmux dialect: grofe's elements as sent,
/// plus tmux's default rows grofe's list lacks, by index. `tmux_rows` are
/// tmux's own indices for the list, when a table lists them.
fn tmux_list_lines(
    model: &Model,
    base: &str,
    tmux_rows: Option<&[u32]>,
    value_only: bool,
) -> String {
    let o = &model.options;
    let items = o
        .list(Owner::Global, base)
        .map(|l| l.items.clone())
        .unwrap_or_default();
    let mut indices: BTreeSet<u32> = items.keys().copied().collect();
    if SPELLED_DEFAULTS.contains(&base) {
        indices.extend(tmux_rows.unwrap_or_default());
        if tmux_rows.is_none() {
            let table = crate::compat::tmux::formats::Table::Session;
            indices.extend(list_rows(table, base));
        }
    }
    if indices.is_empty() {
        return if value_only {
            String::new()
        } else {
            format!("{base}\n")
        };
    }
    indices
        .into_iter()
        .filter_map(|i| {
            let full = format!("{base}[{i}]");
            let text = match items.get(&i) {
                Some(v) => tmux_text(o, Owner::Global, &full, v),
                None => spelled_default(o, &full, None)?,
            };
            Some(line_raw(&full, &text, value_only))
        })
        .collect()
}

/// The indices tmux's `table` lists for the list `base`.
fn list_rows(table: crate::compat::tmux::formats::Table, base: &str) -> Vec<u32> {
    crate::compat::tmux::formats::tmux_table(table)
        .into_iter()
        .filter_map(|(name, _)| {
            let (b, i) = split_index(name).ok()?;
            (b == base).then_some(i).flatten()
        })
        .collect()
}

/// tmux 3.7b's `show -A` of a session or a window (`owner`), with no name:
/// its user options first, then tmux's table in its order, each option the
/// scope sets with its value and each it inherits as `name* value`.
fn tmux_inherited_listing(model: &Model, owner: Owner, value_only: bool) -> String {
    use crate::compat::tmux::formats::{Table, tmux_table, unquote};
    let table = match owner {
        Owner::Window(_) => Table::Window,
        _ => Table::Session,
    };
    let o = &model.options;
    let parents = &owner_chain(model, owner)[1..];
    let star = |name: &str, text: &str| match value_only {
        true => format!("{text}\n"),
        false => format!("{name}* {}\n", quote(text)),
    };
    let mut out = String::new();
    for (name, e) in o.at(owner) {
        if name.starts_with('@') && !name.starts_with("@src-") {
            out.push_str(&line_raw(
                name,
                &tmux_text(o, owner, name, &e.value),
                value_only,
            ));
        }
    }
    let mut lists_done: BTreeSet<&str> = BTreeSet::new();
    for (name, text) in tmux_table(table) {
        let Ok((base, index)) = split_index(name) else {
            continue;
        };
        if list_builtin(base).is_some() {
            // An array nobody set here shows by name, starred.
            if lists_done.insert(base) && o.list(owner, base).is_none() && !value_only {
                out.push_str(&format!("{base}*\n"));
            }
            continue;
        }
        if index.is_some() {
            continue;
        }
        match (o.get(owner, base), o.lookup(base, parents)) {
            (Some(own), _) => out.push_str(&line_raw(
                base,
                &tmux_text(o, owner, base, &own.value),
                value_only,
            )),
            (None, Some(inherited)) => {
                out.push_str(&star(base, &tmux_text(o, Owner::Global, base, inherited)));
            }
            (None, None) => out.push_str(&star(base, &unquote(text))),
        }
    }
    out
}

/// tmux 3.7b's `show -g`, `show -gw` or `show -s` (`owner` Global,
/// GlobalWindow or Server) with no name: the user options set there
/// first, by name (the status sources' `@src-` options left out); then
/// tmux's table in its order, each option grofe keeps with grofe's value
/// as the tmux dialect shows it, each one grofe lacks with tmux's default.
fn tmux_listing(model: &Model, owner: Owner, value_only: bool) -> String {
    use crate::compat::tmux::formats::{Table, tmux_table, unquote};
    let table = match owner {
        Owner::Server => Table::Server,
        Owner::GlobalWindow => Table::Window,
        _ => Table::Session,
    };
    let o = &model.options;
    let mut out = String::new();
    for (name, e) in o.at(owner) {
        if name.starts_with('@') && !name.starts_with("@src-") {
            out.push_str(&line_raw(
                name,
                &tmux_text(o, owner, name, &e.value),
                value_only,
            ));
        }
    }
    let mut lists_done: BTreeSet<&str> = BTreeSet::new();
    for (name, text) in tmux_table(table) {
        let Ok((base, index)) = split_index(name) else {
            continue;
        };
        if list_builtin(base).is_some() {
            if lists_done.insert(base) {
                let rows = list_rows(table, base);
                out.push_str(&tmux_list_lines(model, base, Some(&rows), value_only));
            }
            continue;
        }
        match o.get(Owner::Global, base) {
            Some(e) if index.is_none() && builtin(base).is_some() => {
                let text = tmux_text(o, Owner::Global, base, &e.value);
                out.push_str(&line_raw(base, &text, value_only));
            }
            // An empty list shows by name alone, and `-v` prints nothing.
            _ if text.is_empty() && value_only => {}
            _ if text.is_empty() => out.push_str(&format!("{name}\n")),
            _ if value_only => out.push_str(&format!("{}\n", unquote(text))),
            _ => out.push_str(&format!("{name} {text}\n")),
        }
    }
    out
}

/// A value as `show-options` prints it, as tmux 3.7b's `args_escape` does:
/// `''` when empty; double quotes when it holds any of `` #';${}%`` (with
/// `"`, `$` and `\` escaped inside); else single quotes when it holds `"`;
/// a lone special character or `~` backslash-escaped; a leading `~`
/// escaped; control characters C-style or octal.
pub(crate) fn quote(value: &str) -> String {
    const DOUBLE: &str = " #';${}%";
    if value.is_empty() {
        return "''".to_string();
    }
    let quotes = if value.contains(|c| DOUBLE.contains(c)) {
        Some('"')
    } else if value.contains('"') {
        Some('\'')
    } else {
        None
    };
    let mut chars = value.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c != ' '
        && (quotes.is_some() || c == '~')
    {
        return format!("\\{c}");
    }
    let mut escaped = String::new();
    let mut it = value.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            // As tmux's `utf8_stravis`: `$` only before a name (M5: the
            // `word-separators` default holds `$%`).
            '$' if quotes == Some('"')
                && !it
                    .peek()
                    .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_' || *n == '{') =>
            {
                escaped.push('$');
            }
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            '"' | '$' if quotes == Some('"') => {
                escaped.push('\\');
                escaped.push(c);
            }
            c if c.is_ascii_control() => escaped.push_str(&format!("\\{:03o}", c as u32)),
            c => escaped.push(c),
        }
    }
    let tilde = if escaped.starts_with('~') { "\\" } else { "" };
    match quotes {
        Some('"') => format!("\"{tilde}{escaped}\""),
        Some(_) => format!("'{escaped}'"),
        None => format!("{tilde}{escaped}"),
    }
}

/// Where the server reads its config: the process environment (R5). Unit
/// tests never read the real config dir.
fn config_env() -> config::Env {
    if cfg!(test) {
        return config::Env {
            config_dir: None,
            home: None,
        };
    }
    config::Env::from_process()
}

/// The config chain (spec §5.1), or with `tmux -f <file>...` those files
/// alone, in order (`compat::start_config`): a missing file or
/// `/dev/null` is no file, so none at all leaves the built-in defaults.
fn load_config() -> Result<Resolved, Vec<config::ConfigError>> {
    let env = config_env();
    match crate::compat::start_config() {
        None => config::load(&env),
        Some(files) => {
            let only = config::Env {
                config_dir: None,
                ..env
            };
            let files: Vec<PathBuf> = files
                .into_iter()
                .filter(|f| f != Path::new("/dev/null") && f.exists())
                .collect();
            config::load_with_file(&only, &files)
        }
    }
}

/// Applies the config chain once at server start. On any error the
/// server keeps the built-in defaults and logs every error (R9).
pub fn apply_config_at_start(state: &mut State) {
    match load_config() {
        Ok(resolved) => {
            log_warnings(&resolved);
            apply_config(state, &resolved);
        }
        Err(errors) => {
            eprintln!("grofe config: not applied, using built-in defaults");
            for e in errors {
                eprintln!("grofe config: {e}");
            }
        }
    }
}

fn log_warnings(resolved: &Resolved) {
    for w in &resolved.warnings {
        eprintln!("grofe config: {w}");
    }
}

/// `reload` (spec §5.4): resolves layers 1 to 4 afresh, then applies all
/// of it or, on any error, none of it and answers every error, one per
/// line.
pub fn reload(state: &mut State, _ctx: &ClientCtx, _c: Reload) -> Result<String, String> {
    let resolved = load_config().map_err(|errors| join_errors(&errors))?;
    log_warnings(&resolved);
    apply_config(state, &resolved);
    Ok(String::new())
}

fn join_errors(errors: &[config::ConfigError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Swaps in a resolved config's options and bindings, replacing only what
/// the config set before (R10): runtime values and bindings stay. Options
/// change through the store as `set` changes them, emitting
/// `option-changed`; the caller's `clients::changed` then re-syncs mouse
/// modes and repaints. The config's sources (TC.4), hooks (TC.5) and
/// aliases (TC.7) are applied last, by their own modules.
fn apply_config(state: &mut State, resolved: &Resolved) {
    let model = &mut state.model;
    let events = &mut state.events;
    // `[user-options]` (TP.0) are global options the config sets like any
    // other: a runtime `set` of one takes it out of the config's hands.
    let configured: Vec<(&str, &str)> = resolved
        .configured()
        .map(|(n, v, _)| (n, v))
        .chain(
            resolved
                .user_options
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str())),
        )
        .collect();
    let dropped: Vec<(String, String)> = model
        .options
        .config_base
        .iter()
        .filter(|(name, _)| !configured.iter().any(|(n, _)| n == name))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    for (name, base) in dropped {
        model.options.config_base.remove(&name);
        // A user option the config no longer sets was unset before it.
        if name.starts_with('@') && base.is_empty() {
            if let Ok(true) = model.options.unset(Owner::Global, &name) {
                option_changed(model, events, Owner::Global, &name);
            }
            continue;
        }
        set_global(model, events, &name, &base);
    }
    for (name, value) in configured {
        if model.options.runtime.contains(name) {
            continue;
        }
        if !model.options.config_base.contains_key(name) {
            let base = model
                .options
                .get(Owner::Global, name)
                .map(|e| e.value.clone())
                .unwrap_or_default();
            model.options.config_base.insert(name.to_string(), base);
        }
        set_global(model, events, name, value);
    }
    let tables = &mut state.registry.key_tables;
    // Nothing configured before or now: leave the tables as they are.
    if !resolved.keys.is_empty() || tables.is_configured() {
        *tables = tables.with_config_from(KeyTables::from_config(&resolved.keys));
    }
    apply_config_lists(model, events, resolved);
    crate::server::sources::configure(state, resolved);
    crate::server::plugins::configure(state, resolved);
    crate::server::hooks::configure(state, resolved);
    crate::server::menu::configure(state, resolved);
    // `[environment]`, then `load-commands` (TP.0), as tmux runs a config
    // file's `set-environment` and other commands each time it reads it.
    for (name, value) in &resolved.environment {
        let value = expand_env(state, value);
        crate::server::hooks::set_global(state, name, Some(value), false);
    }
    let cmds: Vec<Vec<String>> = resolved
        .load_commands
        .iter()
        .flat_map(|c| c.cmds.iter().cloned())
        .collect();
    crate::server::hooks::run_at_load(state, cmds);
}

/// `value` with `$NAME`, `${NAME}` and a leading `~` expanded from the
/// global environment, as tmux expands a config line's words; an unset
/// name is empty.
fn expand_env(state: &State, value: &str) -> String {
    let vars = crate::server::hooks::variables(state, None);
    let get = |name: &str| {
        vars.iter()
            .find(|v| v.name == name)
            .and_then(|v| v.value.clone())
            .unwrap_or_default()
    };
    config::expand_value(value, &get)
}

/// Sets a global option, emitting `option-changed` on a change. The
/// loader validated the value with the same store, so an error is a bug,
/// logged rather than fatal.
fn set_global(model: &mut Model, events: &mut Events, name: &str, value: &str) {
    match model.options.set(Owner::Global, name, value, false) {
        Ok(true) => option_changed(model, events, Owner::Global, name),
        Ok(false) => {}
        Err(e) => eprintln!("grofe config: {name}: {e}"),
    }
}

/// `bind-key` (R25 grown up): binds in `Registry.key_tables` as a runtime
/// binding, which `list-keys` marks and `reload` keeps (R10). A single
/// command argument is a command string, split like a stored one (R12);
/// several are split at `;` arguments as the CLI splits argv (tmux 3.7b:
/// `bind y split-window '\;' select-pane -L` binds both). Each command
/// must parse, as tmux checks at bind time; no command at all changes
/// nothing, as in tmux. The binding keeps the request's dialect (spec
/// §4.5), so a key bound through the tmux front end runs through it.
pub fn bind_key(state: &mut State, ctx: &ClientCtx, c: BindKey) -> Result<String, String> {
    let key = parse_key(&c.key).map_err(|_| format!("unknown key: {}", c.key))?;
    let cmds = match c.command.as_slice() {
        [one] => split_command_string(one).map_err(|e| e.to_string())?,
        [] => return Ok(String::new()),
        argv => crate::cmd::parse::split_argv(argv),
    };
    if cmds.is_empty() {
        return Ok(String::new());
    }
    // An alias (TC.7) is expanded now, as tmux expands it when the binding
    // is made: the binding keeps what it ran then.
    let cmds: Vec<Vec<String>> = cmds
        .into_iter()
        .flat_map(|argv| {
            crate::server::dispatch::menu::expand_alias_command(state, &argv)
                .unwrap_or_else(|| vec![argv])
        })
        .collect();
    for argv in &cmds {
        crate::compat::parse_split(ctx, argv)?;
    }
    let mut b = Binding::new(cmds, c.repeat, BindingSource::Runtime);
    b.desc = c.note;
    b.dialect = ctx.dialect;
    state.registry.key_tables.insert(&c.table, key, b);
    Ok(String::new())
}

/// `unbind-key [-a] [-T table] key`, worded as tmux 3.7b words it: an
/// unknown key or table is an error, an unbound key is not.
pub fn unbind_key(state: &mut State, _ctx: &ClientCtx, c: UnbindKey) -> Result<String, String> {
    let tables = &mut state.registry.key_tables;
    // `-q`: a failure with nothing said, which tmux's exit status shows as
    // success.
    if c.quiet && !c.all && c.key.is_none() {
        return Ok(String::new());
    }
    let key = match &c.key {
        Some(name) => match parse_key(name) {
            Ok(key) => Some(key),
            Err(_) if c.quiet => return Ok(String::new()),
            Err(_) => return Err(format!("unknown key: {name}")),
        },
        None => None,
    };
    if !tables.exists(&c.table) {
        if c.quiet {
            return Ok(String::new());
        }
        return Err(format!("table {} doesn't exist", c.table));
    }
    let keys = match key {
        Some(key) => vec![key],
        None => tables.keys(&c.table),
    };
    for key in keys {
        tables.unbind_runtime(&c.table, &key);
    }
    Ok(String::new())
}

/// `list-keys [-1N] [-T table] [key]` in grofe's native form, one line per
/// binding: `bind-key [-r] -T <table> <key> <command>` as tmux spells it,
/// then `  # <source>`, with `: <desc>` and `[<name>]` when set. The source
/// is `default`, `<file>:<line>` (`~` for `$HOME`), `runtime` or
/// `plugin <name>`, so a binding from outside the config table shows. A
/// key lists only its binding; `-1` only the first line; `-N` the notes
/// ([`list_notes`]).
// ponytail: tmux's padded `list-keys` layout is FE.4's.
pub fn list_keys(state: &mut State, ctx: &ClientCtx, c: ListKeys) -> Result<String, String> {
    let only = match &c.key {
        Some(name) => Some(parse_key(name).map_err(|_| format!("invalid key: {name}"))?),
        None => None,
    };
    let sort = Sort::new(&c.sort)?;
    let tables = &state.registry.key_tables;
    if let Some(t) = &c.table
        && !tables.exists(t)
    {
        return Err(format!("table {t} doesn't exist"));
    }
    // The flags only the tmux listing has choose it in either dialect.
    let tmux_listing = c.format.is_some() || c.prefix.is_some() || c.all || c.sort.order.is_some();
    if ctx.dialect == crate::protocol::Dialect::Tmux || tmux_listing {
        return list_keys_tmux(state, ctx, &c, only, sort);
    }
    if c.notes {
        return list_notes(state, ctx, &c, only);
    }
    let names: Vec<&str> = match &c.table {
        Some(t) => vec![t.as_str()],
        None => tables.table_names().collect(),
    };
    let home = config_env().home;
    let mut out = String::new();
    for table in names {
        for key in tables.keys(table) {
            if only.is_some_and(|k| !k.matches(&key)) {
                continue;
            }
            let Some(b) = tables.exact(table, &key) else {
                continue;
            };
            out.push_str(&binding_line(table, &key, b, home.as_deref()));
            out.push('\n');
            if c.first {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// `list-keys -N`: each binding with a note as `<prefix><key> <note>`, keys
/// padded to the widest, as tmux 3.7b prints them (recorded: without `-T`,
/// the prefix table then the root table, every line after the prefix
/// key, `C-b `; with `-T`, that table alone and no prefix). With `-1` only
/// the first, and when a client is current it goes to that client's status
/// line instead, unpadded, as tmux's stock `prefix /` shows it.
// ponytail: tmux 3.7b's CLI prints nothing when a key is given, with or
// without `-N` or `-1` (recorded: `list-keys -T prefix c`, `list-keys -1
// -T prefix c` and `list-keys -N c` all print nothing), and `-1N` of a key
// without a note shows the first noted binding; grofe lists what the key
// names. Matching tmux's output is FE.4's.
fn list_notes(
    state: &mut State,
    ctx: &ClientCtx,
    c: &ListKeys,
    only: Option<Key>,
) -> Result<String, String> {
    let tables = &state.registry.key_tables;
    let prefix = match state.model.options.lookup("prefix", &[]) {
        Some(p) if !p.eq_ignore_ascii_case("None") => format!("{p} "),
        _ => String::new(),
    };
    let names: Vec<(&str, &str)> = match &c.table {
        Some(t) => vec![(t.as_str(), "")],
        None => vec![("prefix", prefix.as_str()), ("root", prefix.as_str())],
    };
    let mut notes: Vec<(String, String)> = Vec::new();
    for (table, start) in names {
        for key in tables.keys(table) {
            if only.is_some_and(|k| !k.matches(&key)) {
                continue;
            }
            let Some(note) = tables.exact(table, &key).and_then(|b| b.desc.as_deref()) else {
                continue;
            };
            if note.is_empty() {
                continue;
            }
            notes.push((format!("{start}{key}"), note.to_string()));
        }
    }
    if c.first {
        notes.truncate(1);
        if let (Some((key, note)), Some(id)) = (notes.first(), clients::current_client(state, ctx))
        {
            clients::show_message(state, id, &format!("{key} {note}"));
            return Ok(String::new());
        }
    }
    let width = notes
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    Ok(notes
        .iter()
        .map(|(key, note)| format!("{key:<width$} {note}\n"))
        .collect())
}

/// tmux 3.7b's `LIST_KEYS_TEMPLATE`, a line of `list-keys`.
const TMUX_LIST_KEYS: &str = "#{?notes_only,\
#{key_prefix} #{p|#{key_string_width}:key_string} #{?key_note,#{key_note},#{key_command}},\
bind-key #{?key_has_repeat,#{?key_repeat,-r,  },} \
-T #{p|#{key_table_width}:key_table} #{p|#{key_string_width}:#{q|a:key_string}} #{key_command}}";

/// The variables of one `list-keys` line (`cmd_list_keys_format_add_key_binding`).
struct KeyVars(BTreeMap<&'static str, String>);

impl FormatVars for KeyVars {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// `list-keys` as tmux 3.7b prints it (`cmd-list-keys.c`, recorded):
///
/// - which bindings: `-T`'s table, or with `-N` the prefix then the root
///   table, or every table by name; each table in tmux's key order
///   ([`tmux_key_order`]) or the order `-O` asks for (`-r` reversing it); a
///   key given keeps only its binding, `-N` only bindings with a note
///   (`-a`: every one), `-1` only the first;
/// - a line is `-F`'s format, else tmux's template, over the line's key
///   variables: by default `bind-key [-r] -T <table> <key> <commands>`, the
///   table and the key (escaped as tmux escapes an argument) padded to the
///   widest, `-r` or two blanks only when some binding repeats; with `-N`,
///   `<prefix> <key> <note>` (`-P`'s prefix, else the `prefix` key);
/// - one binding (a key given, or `-1`) goes to the current client's
///   status line, not to the output.
///
/// The commands are tmux's stock text for a stock binding, else each one
/// printed as tmux prints a parsed command ([`tmux_command_line`]).
// ponytail: tmux compares key codes as `int`s, so its `-O index` and
// `-O modifier` order the keys it cannot tell apart by the low bits of the
// difference of their codes; here they order by the key's place in tmux's
// key table (index) or its modifiers (modifier), and `-O name` is by table.
fn list_keys_tmux(
    state: &mut State,
    ctx: &ClientCtx,
    c: &ListKeys,
    only: Option<Key>,
    sort: Sort,
) -> Result<String, String> {
    let tables = &state.registry.key_tables;
    let names: Vec<String> = match &c.table {
        Some(t) => vec![t.clone()],
        None if c.notes => vec!["prefix".to_string(), "root".to_string()],
        None => tables.table_names().map(str::to_string).collect(),
    };
    // Each table, in key order; sorted as a whole unless a table or `-N`
    // names the tables to sort one by one.
    let mut groups: Vec<Vec<(String, Key, Binding)>> = Vec::new();
    for table in &names {
        let mut keys = tables.keys(table);
        keys.sort_by_key(tmux_key_order);
        groups.push(
            keys.into_iter()
                .filter_map(|key| Some((table.clone(), key, tables.exact(table, &key)?.clone())))
                .collect(),
        );
    }
    // tmux's `sort_key_binding_cmp` is no total order (it takes the
    // difference of two 64 bit key codes as an `int`, and a tie as "greater"
    // when the tables are the same), so its order is the one its `qsort`
    // makes of it: the same merge sort here.
    type Row = (String, Key, Binding);
    let compare = |order: Order, a: &Row, b: &Row| -> i32 {
        let same_table = a.0.eq_ignore_ascii_case(&b.0);
        let first = match order {
            Order::Index => tmux_key_low32(&a.1).wrapping_sub(tmux_key_low32(&b.1)) as i32,
            Order::Name => i32::from(same_table),
            _ => 0,
        };
        let result = if first == 0 {
            i32::from(same_table)
        } else {
            first
        };
        if sort.reversed() { -result } else { result }
    };
    let sort_rows = |rows: &mut [Row]| match sort.order() {
        None => {}
        Some(Order::Natural) => {
            if sort.reversed() {
                rows.reverse();
            }
        }
        Some(order) => {
            crate::server::dispatch::sorting::merge_sort(rows, &|a, b| compare(order, a, b))
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    if c.table.is_some() || c.notes {
        for group in &mut groups {
            sort_rows(group);
        }
        rows.extend(groups.into_iter().flatten());
    } else {
        rows.extend(groups.into_iter().flatten());
        sort_rows(&mut rows);
    }
    let filter_notes = c.notes && !c.all;
    rows.retain(|(_, key, b)| {
        only.is_none_or(|k| k.matches(key)) && (!filter_notes || b.desc.is_some())
    });
    if c.first {
        rows.truncate(1);
    }
    let prefix = match &c.prefix {
        Some(p) => p.clone(),
        None => match state.model.options.lookup("prefix", &[]) {
            Some(p) if !p.eq_ignore_ascii_case("None") => p.to_string(),
            _ => String::new(),
        },
    };
    let width = |s: &str| unicode_width::UnicodeWidthStr::width(s);
    let key_width = rows
        .iter()
        .map(|(_, k, _)| width(&k.to_string()))
        .max()
        .unwrap_or(0);
    let table_width = rows.iter().map(|(t, _, _)| width(t)).max().unwrap_or(0);
    let has_repeat = rows.iter().any(|(_, _, b)| b.repeat);
    let template = match &c.format {
        Some(f) if ctx.dialect == crate::protocol::Dialect::Tmux => tmux_template(f),
        Some(f) => f.clone(),
        None => tmux_template(TMUX_LIST_KEYS),
    };
    let flag = |on: bool| if on { "1" } else { "0" }.to_string();
    let lines: Vec<String> = rows
        .iter()
        .map(|(table, key, b)| {
            let command = b
                .shown
                .clone()
                .unwrap_or_else(|| tmux_command_line(&b.cmds));
            let vars = KeyVars(BTreeMap::from([
                ("notes_only", flag(c.notes)),
                ("key_has_repeat", flag(has_repeat)),
                ("key_string_width", key_width.to_string()),
                ("key_table_width", table_width.to_string()),
                ("key_repeat", flag(b.repeat)),
                ("key_note", b.desc.clone().unwrap_or_default()),
                ("key_prefix", prefix.clone()),
                ("key_table", table.clone()),
                ("key_string", key.to_string()),
                ("key_command", command),
            ]));
            expand(&template, &vars)
        })
        .collect();
    if lines.len() == 1 || c.first {
        if let (Some(line), Some(id)) = (lines.first(), clients::current_client(state, ctx)) {
            clients::show_message(state, id, line);
        }
        return Ok(String::new());
    }
    Ok(lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| format!("{l}\n"))
        .collect())
}

/// The low 32 bits of tmux's key code for `key` (`key_code`): what is left
/// of a key code once its type and modifier bits are out of the way, and
/// all `-O index` sees of it (it compares the difference as an `int`).
// ponytail: a character of four UTF-8 bytes is an index into tmux's table of
// them, which is not reproduced; its bytes stand in.
fn tmux_key_low32(key: &Key) -> u32 {
    use crate::server::input::{KeyCode, MousePlace};
    use unicode_width::UnicodeWidthChar;
    let packed = |c: char| {
        let mut buf = [0u8; 4];
        let bytes = c.encode_utf8(&mut buf).as_bytes();
        let width = c.width().unwrap_or(0) as u32;
        let data = bytes
            .iter()
            .take(3)
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (8 * i));
        (bytes.len() as u32) << 24 | (width + 1) << 29 | data
    };
    // Past KEYC_NONE: the functional keys, in tmux's order.
    let function = |n: u32| n;
    match key.code {
        KeyCode::Char(c) if c.is_ascii() => c as u32,
        KeyCode::Char(c) => packed(c),
        KeyCode::Literal(c) => packed(c),
        KeyCode::Control(c) => u32::from(c),
        KeyCode::Tab => 9,
        KeyCode::Enter => 13,
        KeyCode::Escape => 27,
        KeyCode::User(n) => u32::from(n),
        KeyCode::Any => function(4),
        KeyCode::Backspace => function(7),
        KeyCode::F(n) => function(7 + u32::from(n)),
        KeyCode::Insert => function(20),
        KeyCode::Delete => function(21),
        KeyCode::Home => function(22),
        KeyCode::End => function(23),
        KeyCode::PageDown => function(24),
        KeyCode::PageUp => function(25),
        KeyCode::BackTab => function(26),
        KeyCode::Up => function(27),
        KeyCode::Down => function(28),
        KeyCode::Left => function(29),
        KeyCode::Right => function(30),
        KeyCode::Keypad(k) => function(31 + k.order()),
        KeyCode::Mouse(m) => {
            let place = match m.place {
                MousePlace::Pane => 0,
                MousePlace::Status => 1,
                MousePlace::StatusLeft => 2,
                MousePlace::StatusRight => 3,
                MousePlace::StatusDefault => 4,
                MousePlace::Border => 5,
                MousePlace::ScrollbarUp => 6,
                MousePlace::ScrollbarSlider => 7,
                MousePlace::ScrollbarDown => 8,
                MousePlace::Control(n) => 9 + u32::from(n),
            };
            u32::from(m.button) << 8 | place
        }
    }
}

/// Where tmux 3.7b's key codes put `key` (its key tables are ordered by
/// code): no modifier, then Meta, Ctrl and Shift as the high bits (`M-`
/// 1, `C-` 2, `S-` 4); within one, characters by code point (Enter, Tab
/// and Escape are C0 characters), then tmux's special keys in its own
/// order, then the mouse keys.
pub(crate) fn tmux_key_order(key: &Key) -> (u8, u8, u32, String) {
    use crate::server::input::KeyCode;
    let m = &key.mods;
    let mods = u8::from(m.alt) | u8::from(m.ctrl) << 1 | u8::from(m.shift) << 2;
    const SPECIAL: &[KeyCode] = &[
        KeyCode::Backspace,
        KeyCode::Insert,
        KeyCode::Delete,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageDown,
        KeyCode::PageUp,
        KeyCode::BackTab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
    ];
    // tmux orders by the key code: the characters and C0 codes, the user
    // keys, the functional keys, then the mouse keys.
    match key.code {
        // The characters tmux packs as UTF-8 (those past ASCII, and those a
        // `0xNN` made) come after the C0 codes and ASCII, in that order.
        KeyCode::Char(c) if c.is_ascii() => (mods, 0, c as u32, String::new()),
        KeyCode::Literal(c) => (mods, 0, 0x80 + c as u32, String::new()),
        KeyCode::Char(c) => (mods, 0, 0x1000 + c as u32, String::new()),
        KeyCode::Enter => (mods, 0, 13, String::new()),
        KeyCode::Tab => (mods, 0, 9, String::new()),
        KeyCode::Escape => (mods, 0, 27, String::new()),
        KeyCode::Control(c) => (mods, 0, u32::from(c), String::new()),
        KeyCode::User(n) => (mods, 1, u32::from(n), String::new()),
        // F1 to F12 sit between BSpace and IC.
        KeyCode::F(n) => (mods, 2, u32::from(n), String::new()),
        // The keypad follows the arrows.
        KeyCode::Keypad(k) => (mods, 2, 25 + k.order(), String::new()),
        KeyCode::Mouse(m) => (mods, 3, mouse_order(m), String::new()),
        code => {
            let at = SPECIAL.iter().position(|s| *s == code).unwrap_or(0) as u32;
            let at = if at == 0 { 0 } else { at + 12 };
            (mods, 2, at, String::new())
        }
    }
}

/// Where tmux 3.7b's key codes put a mouse key: by action (`KEYC_MOUSE_KEY`
/// order, the wheel down before up, as recorded), then button, then where
/// the pointer was (pane, the status line's ranges, border, scrollbar,
/// border controls).
fn mouse_order(m: crate::server::input::MouseKey) -> u32 {
    use crate::server::input::{MouseAction as A, MousePlace as P};
    let action = match m.action {
        A::Down => 0,
        A::Up => 1,
        A::Drag => 2,
        A::DragEnd => 3,
        A::WheelDown => 4,
        A::WheelUp => 5,
        A::SecondClick => 6,
        A::DoubleClick => 7,
        A::TripleClick => 8,
    };
    let place = match m.place {
        P::Pane => 0,
        P::Status => 1,
        P::StatusLeft => 2,
        P::StatusRight => 3,
        P::StatusDefault => 4,
        P::Border => 5,
        P::ScrollbarUp => 6,
        P::ScrollbarSlider => 7,
        P::ScrollbarDown => 8,
        P::Control(n) => 9 + u32::from(n),
    };
    action * 10_000 + u32::from(m.button) * 100 + place
}

/// A binding's commands as tmux 3.7b prints a parsed command list in
/// `list-keys` (`cmd_list_print` escaped): each command by its full tmux
/// name, its flags without values first in one word, then each flag with
/// a value, both in tmux's flag order (ASCII: digits, upper, lower case),
/// then the arguments, every value escaped as tmux escapes one; commands
/// joined by ` \; `.
pub(crate) fn tmux_command_line(cmds: &[Vec<String>]) -> String {
    cmds.iter()
        .map(|argv| tmux_command(argv))
        .collect::<Vec<_>>()
        .join(" \\; ")
}

fn tmux_command(argv: &[String]) -> String {
    use crate::cmd::parse::escape_arg;
    let Some((name, args)) = argv.split_first() else {
        return String::new();
    };
    let Ok(spec) = crate::compat::tmux::resolve(name) else {
        // A native-only verb: as it is.
        let mut out = vec![name.clone()];
        out.extend(args.iter().map(|a| escape_arg(a)));
        return out.join(" ");
    };
    // Flags taking a value: `[-c word]` in the usage (and `select-pane
    // -P`, which tmux hides).
    let mut takes_value: Vec<char> = spec
        .usage
        .split("[-")
        .skip(1)
        .filter_map(|part| {
            let mut chars = part.chars();
            let flag = chars.next()?;
            (chars.next() == Some(' ')).then_some(flag)
        })
        .collect();
    if spec.name == "select-pane" {
        takes_value.push('P');
    }
    let mut bools: BTreeMap<char, usize> = BTreeMap::new();
    let mut values: BTreeMap<char, Vec<String>> = BTreeMap::new();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else {
            break;
        };
        i += 1;
        for (at, flag) in letters.char_indices() {
            if !takes_value.contains(&flag) {
                *bools.entry(flag).or_default() += 1;
                continue;
            }
            let attached = &letters[at + flag.len_utf8()..];
            let value = if attached.is_empty() {
                i += 1;
                args.get(i - 1).cloned().unwrap_or_default()
            } else {
                attached.to_string()
            };
            values.entry(flag).or_default().push(value);
            break;
        }
    }
    let mut out = spec.name.to_string();
    if !bools.is_empty() {
        out.push_str(" -");
        for (flag, n) in &bools {
            out.extend(std::iter::repeat_n(*flag, *n));
        }
    }
    for (flag, vals) in &values {
        for v in vals {
            out.push_str(&format!(" -{flag} {}", escape_arg(v)));
        }
    }
    for a in &args[i.min(args.len())..] {
        out.push(' ');
        out.push_str(&escape_arg(a));
    }
    out
}

fn binding_line(table: &str, key: &Key, b: &Binding, home: Option<&Path>) -> String {
    let repeat = if b.repeat { "-r " } else { "" };
    let cmds: Vec<String> = b
        .cmds
        .iter()
        .map(|argv| argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "))
        .collect();
    let source = match &b.source {
        BindingSource::Default => "default".to_string(),
        BindingSource::File { path, line } => format!("{}:{line}", tilde(path, home)),
        BindingSource::Runtime => "runtime".to_string(),
        BindingSource::Plugin(name) => format!("plugin {name}"),
    };
    let mut line = format!(
        "bind-key {repeat}-T {table} {} {}  # {source}",
        quote(&key.to_string()),
        cmds.join(" \\; ")
    );
    if let Some(desc) = &b.desc {
        line.push_str(&format!(": {desc}"));
    }
    if let Some(name) = &b.name {
        line.push_str(&format!(" [{name}]"));
    }
    line
}

/// `source-file [-Fnqv] path ...`: loads each file at runtime, read by its
/// name. A path is a glob, as tmux's `glob(3)`, and one that matches
/// nothing is `No such file or directory: path` unless `-q`; the others
/// still load. A relative path is the requester's cwd's, even in a nested
/// file (recorded).
///
/// - `*.toml` is grofe's config (spec §5.1), with its includes resolved
///   against it. Its values and bindings count as runtime (R10): `reload`
///   keeps them. All errors or nothing, as `reload`.
/// - Any other file is a tmux config when the request is tmux's (its
///   dialect, or a tmux-identity server): tmux's plugins source their own
///   `*.tmux` and `*.conf` files through `tmux` ([`source_tmux_file`]). A
///   native request on a grofe server reads it as TOML, as before.
///
/// Files sourced from files nest at most [`MAX_SOURCE_DEPTH`] deep, as in
/// tmux 3.7b; a deeper one is `too many nested files`. A TOML file cannot
/// run `source-file` (its includes are `config::load_one`'s, which refuse
/// a cycle), so only a tmux file nests this way.
pub fn source_file(state: &mut State, ctx: &ClientCtx, c: SourceFile) -> Result<String, String> {
    // `-t` can fail quietly: the file then runs for the current pane.
    let run = match c.target.as_ref().map(|t| state.model.resolve_pane(t, ctx)) {
        Some(Ok(pane)) => ClientCtx {
            pane: Some(pane),
            ..ctx.clone()
        },
        _ => ctx.clone(),
    };
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for given in &c.paths {
        // `-F`: the path is a format, expanded for the requester's pane.
        let given = if c.format {
            let pane = state.model.resolve_pane(&Target::Current, &run).ok();
            super::bindings::expand(state, &run, pane, given)
        } else {
            given.clone()
        };
        if given == "-" {
            files.push(PathBuf::from("-"));
            continue;
        }
        match glob_files(&source_pattern(&ctx.cwd, &given)) {
            Ok(found) => files.extend(found),
            Err(GlobError::NoMatch) if c.quiet => {}
            Err(e) => errors.push(format!("{}: {given}", e.text())),
        }
    }
    let mut out = String::new();
    // What tmux refused while parsing is printed when every file is done.
    let mut refused = String::new();
    // Every file is read (and traced) before any of them runs.
    let mut loaded = Vec::new();
    for path in files {
        match load_one(state, ctx, &run, &path, &c) {
            Ok(file) => loaded.push((path, file)),
            Err(e) if e.is_empty() => refused.push_str(&std::mem::take(&mut state.failed_out)),
            Err(e) => {
                out.push_str(&std::mem::take(&mut state.failed_out));
                errors.push(e);
            }
        }
    }
    for (_, file) in &mut loaded {
        if let Loaded::Tmux(parsed) = file {
            out.push_str(&std::mem::take(&mut parsed.shown));
        }
    }
    let mut loaded = loaded.into_iter();
    while let Some((_, file)) = loaded.next() {
        let result = match file {
            Loaded::Tmux(parsed) => run_parsed(state, parsed),
            Loaded::Toml(path) => apply_toml(state, &path),
        };
        match result {
            Ok(text) => out.push_str(&text),
            Err(e) if e.is_empty() => refused.push_str(&std::mem::take(&mut state.failed_out)),
            Err(e) => {
                out.push_str(&std::mem::take(&mut state.failed_out));
                errors.push(e);
            }
        }
        // A command that held the request holds the files after it too.
        if let Some(held) = &mut state.held {
            held.errors.append(&mut errors);
            for (path, file) in loaded.by_ref() {
                match file {
                    Loaded::Tmux(parsed) => {
                        let Parsed {
                            groups,
                            run,
                            depth,
                            verbose,
                            ..
                        } = parsed;
                        held.frames.extend(groups.into_iter().map(|cmds| {
                            super::client::HeldFrame {
                                ctx: run.clone(),
                                cmds,
                                keep_going: true,
                                depth,
                                verbose,
                            }
                        }));
                    }
                    Loaded::Toml(_) => {
                        let rest = SourceFile {
                            format: false,
                            paths: vec![glob_escape(&path)],
                            ..c.clone()
                        };
                        held.frames.push(super::client::HeldFrame {
                            ctx: ctx.clone(),
                            cmds: vec![Command::SourceFile(rest)],
                            keep_going: true,
                            depth: source_depth(),
                            verbose: source_verbose(),
                        });
                    }
                }
            }
            out.push_str(&refused);
            return Ok(out);
        }
    }
    out.push_str(&refused);
    if errors.is_empty() && refused.is_empty() {
        return Ok(out);
    }
    state.failed_out = out;
    Err(join_nonempty(&errors))
}

/// Why a path gave no file: `glob(3)`'s own failures, with the `strerror`
/// text tmux prints for them.
enum GlobError {
    NoMatch,
    Other(i32),
}

impl GlobError {
    fn text(&self) -> &'static str {
        match self {
            GlobError::NoMatch => "No such file or directory",
            GlobError::Other(rc) if *rc == nix::libc::GLOB_NOSPACE => "Cannot allocate memory",
            GlobError::Other(_) => "Invalid argument",
        }
    }
}

/// The files `pattern` matches, in `glob(3)`'s order (sorted).
fn glob_files(pattern: &Path) -> Result<Vec<PathBuf>, GlobError> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let pattern = std::ffi::CString::new(pattern.as_os_str().as_bytes())
        .map_err(|_| GlobError::Other(nix::libc::GLOB_ABORTED))?;
    // SAFETY: `found` is zeroed storage for `glob` to fill; the paths it
    // holds are read before `globfree` releases them.
    unsafe {
        let mut found: nix::libc::glob_t = std::mem::zeroed();
        let rc = nix::libc::glob(pattern.as_ptr(), 0, None, &mut found);
        if rc != 0 {
            nix::libc::globfree(&mut found);
            return Err(if rc == nix::libc::GLOB_NOMATCH {
                GlobError::NoMatch
            } else {
                GlobError::Other(rc)
            });
        }
        let files = (0..found.gl_pathc)
            .map(|i| {
                let path = std::ffi::CStr::from_ptr(*found.gl_pathv.add(i));
                PathBuf::from(std::ffi::OsString::from_vec(path.to_bytes().to_vec()))
            })
            .collect();
        nix::libc::globfree(&mut found);
        Ok(files)
    }
}

/// The pattern `source-file` globs for `given`: an absolute path as it
/// is, else under the requester's `cwd`, which is quoted so only `given`
/// is a pattern, as tmux's `cmd_source_file_quote_for_glob`.
fn source_pattern(cwd: &Path, given: &str) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    if given.starts_with('/') {
        return PathBuf::from(given);
    }
    let mut pattern = glob_quote(cwd.as_os_str().as_bytes());
    pattern.push(b'/');
    pattern.extend_from_slice(given.as_bytes());
    PathBuf::from(std::ffi::OsString::from_vec(pattern))
}

/// `bytes` with `glob(3)`'s special characters quoted, so the pattern
/// matches only itself.
fn glob_quote(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if matches!(b, b'*' | b'?' | b'[' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// `path` as a pattern that matches only itself.
fn glob_escape(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    String::from_utf8_lossy(&glob_quote(path.as_os_str().as_bytes())).into_owned()
}

/// A file `source-file` found, read: a tmux file is parsed (and traced)
/// before any file runs, as tmux reads every file when its read is done,
/// a TOML one is applied when its turn comes.
enum Loaded {
    Tmux(Parsed),
    Toml(PathBuf),
}

/// A tmux file read and parsed.
struct Parsed {
    /// What `-v` traced.
    shown: String,
    /// The commands of each line, to run in turn (none with `-n`).
    groups: Vec<Vec<Command>>,
    /// The context they run in.
    run: ClientCtx,
    /// The source-file depth and `-v` they run under.
    depth: u32,
    verbose: bool,
}

/// Reads one file `source-file` found.
fn load_one(
    state: &mut State,
    ctx: &ClientCtx,
    run: &ClientCtx,
    path: &Path,
    c: &SourceFile,
) -> Result<Loaded, String> {
    let tmux =
        ctx.dialect == Dialect::Tmux || state.model.identity == crate::compat::Identity::Tmux;
    if path == Path::new("-") || tmux && path.extension().is_none_or(|e| e != "toml") {
        let depth = source_depth();
        if depth >= MAX_SOURCE_DEPTH {
            return Err("too many nested files".to_string());
        }
        let verbose = c.verbose || source_verbose();
        let parsed = with_source_depth(depth + 1, verbose, || {
            parse_tmux_file(state, ctx, run, path, c.parse_only, verbose)
        })?;
        return Ok(Loaded::Tmux(Parsed {
            depth: depth + 1,
            verbose,
            ..parsed
        }));
    }
    Ok(Loaded::Toml(path.to_path_buf()))
}

/// Applies a TOML file's settings and bindings.
fn apply_toml(state: &mut State, path: &Path) -> Result<String, String> {
    let resolved = config::load_one(path, &config_env()).map_err(|e| join_errors(&e))?;
    check_live_names(&state.registry.key_tables, &resolved)?;
    log_warnings(&resolved);
    let model = &mut state.model;
    for (name, value, _) in resolved.configured() {
        set_global(model, &mut state.events, name, value);
        model.options.mark_runtime(name);
    }
    for (name, items, _) in resolved.configured_lists() {
        set_global_list(model, &mut state.events, name, list_items(items));
        model.options.mark_runtime(name);
    }
    let tables = &mut state.registry.key_tables;
    for k in &resolved.keys {
        if k.cmds.is_empty() {
            tables.unbind_runtime(&k.table, &k.key);
            continue;
        }
        let mut b = Binding::new(k.cmds.clone(), k.repeat, BindingSource::Runtime);
        b.desc = k.desc.clone();
        b.name = k.name.clone();
        tables.insert(&k.table, k.key, b);
    }
    Ok(String::new())
}

/// How deep `source-file` of a tmux file nests, as tmux 3.7b's (recorded:
/// a file that sources itself loads 50 times, then the 51st is `too many
/// nested files`, exit 1, the server up).
const MAX_SOURCE_DEPTH: u32 = 50;

thread_local! {
    /// The tmux files being sourced now, one inside another (the actor's
    /// stack). A held file's later lines run outside it, so a
    /// [`super::client::HeldFrame`] carries the depth it was held at and
    /// runs under it again ([`with_source_depth`]).
    static SOURCE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// How many tmux files are being sourced, one inside another, right now.
pub(crate) fn source_depth() -> u32 {
    SOURCE_DEPTH.get()
}

/// Runs `f` at source-file depth `depth`, then puts the old depth back.
pub(crate) fn with_source_depth<T>(depth: u32, verbose: bool, f: impl FnOnce() -> T) -> T {
    let old = SOURCE_DEPTH.replace(depth);
    let was = SOURCE_VERBOSE.replace(verbose);
    let out = f();
    SOURCE_DEPTH.set(old);
    SOURCE_VERBOSE.set(was);
    out
}

/// Whether `source-file -v` is on for the file being sourced now.
pub(crate) fn source_verbose() -> bool {
    SOURCE_VERBOSE.get()
}

/// Reads a config file for [`source_tmux_file`]: `%if` conditions are
/// formats expanded against the requester's pane, and `NAME=value` sets the
/// global environment (not for `source-file -n`).
struct FileHost<'a> {
    state: &'a mut State,
    ctx: &'a ClientCtx,
    parse_only: bool,
}

impl crate::cmd::ConfigHost for FileHost<'_> {
    fn condition(&mut self, format: &str) -> bool {
        let pane = self
            .state
            .model
            .resolve_pane(&Target::Current, self.ctx)
            .ok();
        let text = super::bindings::expand(self.state, self.ctx, pane, format);
        !text.is_empty() && text != "0"
    }

    fn assign(&mut self, assignment: &str, hidden: bool) {
        let Some((name, value)) = assignment.split_once('=') else {
            return;
        };
        if !self.parse_only {
            crate::server::hooks::set_global(self.state, name, Some(value.to_string()), hidden);
        }
    }
}

thread_local! {
    /// `source-file -v` is on for the files it sources in turn.
    static SOURCE_VERBOSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A command as `source-file -v` prints it (tmux's `cmd_print`, recorded):
/// the full name, a cluster of flags in letter order, flags with a value
/// after them, and every argument as `args_escape` writes it.
fn print_command(argv: &[String]) -> String {
    tmux_command(argv)
}

/// A config file's text. An open that fails says why, as tmux (`strerror`
/// first, then the path); a read that fails is always `Input/output error`
/// (recorded: a directory), as tmux's file reader reports it.
fn read_config(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| {
        let text = e.to_string();
        let text = text.split(" (os error").next().unwrap_or_default();
        format!("{text}: {}", path.display())
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| format!("Input/output error: {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A tmux config file, read as tmux 3.7b's `source-file` reads one
/// (recorded): split into lines ([`cmd::split_config_file_with`], which
/// also settles `%if` and `NAME=value`) and every command parsed through
/// the tmux front end before any runs. What tmux refuses while parsing (a
/// syntax error, an unknown command, a flag its usage lacks) runs nothing
/// and is `<path>:<line>: <error>`. A command that is valid tmux but that
/// grofe cannot translate fails only itself, with the same text: it stops
/// the rest of its line, the lines around it run, and it is noted in
/// `compat-miss.log`. Then each line's commands run as one group: an error
/// stops the rest of its line, the next line still runs, and the request
/// fails at the end with every error, after what the commands printed. A
/// command that holds the request (`run-shell` without `-b`, `wait-for`)
/// holds the lines after it too, as tmux's queue waits; those lines still
/// run on after an error, and the errors from before the hold and after it
/// fail the request at the end. With `parse_only` (`-n`) nothing runs.
fn parse_tmux_file(
    state: &mut State,
    ctx: &ClientCtx,
    run: &ClientCtx,
    path: &Path,
    parse_only: bool,
    verbose: bool,
) -> Result<Parsed, String> {
    let text = if path == Path::new("-") {
        // The CLI client's stdin; an attached client has none (tmux's EBADF).
        match &state.cli_io {
            Some(crate::server::CliIo {
                stdin: Some(data), ..
            }) => String::from_utf8_lossy(data).into_owned(),
            _ => return Err("Bad file descriptor: -".to_string()),
        }
    } else {
        read_config(path)?
    };
    let at = |line: usize, e: &dyn std::fmt::Display| format!("{}:{line}: {e}", path.display());
    // tmux collects what it refuses while parsing and prints it on stdout
    // when the file is done (`cfg_print_causes`), and the request fails.
    let refused = |state: &mut State, message: String| {
        state.failed_out = format!("{message}\n");
        Err(String::new())
    };
    // `%if` reads the client's current pane; the commands run for `-t`'s.
    let ctx = ClientCtx {
        dialect: Dialect::Tmux,
        ..ctx.clone()
    };
    let run = ClientCtx {
        dialect: Dialect::Tmux,
        ..run.clone()
    };
    let split = {
        let mut host = FileHost {
            state: &mut *state,
            ctx: &ctx,
            parse_only,
        };
        crate::cmd::split_config_file_with(&text, &mut host)
    };
    let mut groups = Vec::new();
    let split = match split {
        Ok(split) => split,
        Err((line, e)) => return refused(state, at(line, &e)),
    };
    let mut shown = String::new();
    for (line, cmds) in &split {
        if verbose {
            let cmds: Vec<String> = cmds.iter().map(|argv| print_command(argv)).collect();
            shown.push_str(&format!(
                "{}:{line}: {}\n",
                path.display(),
                cmds.join(" ; ")
            ));
        }
    }
    for (line, cmds) in split {
        let mut group = Vec::new();
        for argv in cmds {
            match super::parse_split_checked(state, &run, &argv) {
                Ok(parsed) => group.extend(parsed),
                Err((true, e)) => return refused(state, at(line, &e)),
                Err((false, e)) => {
                    // Fails when its turn comes; the line stops there.
                    group.push(Command::Refused(at(line, &e)));
                    break;
                }
            }
        }
        groups.push(group);
    }
    Ok(Parsed {
        shown,
        groups: if parse_only { Vec::new() } else { groups },
        run,
        depth: 0,
        verbose,
    })
}

/// Runs a parsed tmux file's lines (see [`parse_tmux_file`]).
fn run_parsed(state: &mut State, parsed: Parsed) -> Result<String, String> {
    let Parsed {
        groups,
        run,
        depth,
        verbose,
        ..
    } = parsed;
    with_source_depth(depth, verbose, || {
        let mut out = String::new();
        let mut errors = Vec::new();
        let mut groups = groups.into_iter();
        while let Some(group) = groups.next() {
            match super::run_commands(state, &run, group) {
                Ok(text) => out.push_str(&text),
                Err(e) => {
                    out.push_str(&std::mem::take(&mut state.failed_out));
                    errors.push(e);
                }
            }
            if state.held.is_some() {
                let held = state.held.get_or_insert_default();
                held.errors.append(&mut errors);
                held.frames
                    .extend(groups.by_ref().map(|cmds| super::client::HeldFrame {
                        ctx: run.clone(),
                        cmds,
                        keep_going: true,
                        depth,
                        verbose,
                    }));
                return Ok(out);
            }
        }
        if errors.is_empty() {
            return Ok(out);
        }
        state.failed_out = out;
        Err(join_nonempty(&errors))
    })
}

/// `errors` one to a line, leaving out those whose text already went to
/// stdout (a nested file's parse errors).
pub(crate) fn join_nonempty(errors: &[String]) -> String {
    let shown: Vec<&str> = errors
        .iter()
        .map(String::as_str)
        .filter(|e| !e.is_empty())
        .collect();
    shown.join("\n")
}

/// A `name` is unique across the live tables too (R6): `load_one` checks
/// only inside the file it loads.
fn check_live_names(tables: &KeyTables, resolved: &Resolved) -> Result<(), String> {
    let mut errors = Vec::new();
    for k in &resolved.keys {
        let Some(name) = &k.name else { continue };
        let taken = tables.table_names().find_map(|table| {
            tables
                .keys(table)
                .into_iter()
                .find(|key| {
                    !(table == k.table && *key == k.key)
                        && tables
                            .exact(table, key)
                            .is_some_and(|b| b.name.as_deref() == Some(name))
                })
                .map(|key| format!("{table} {key}"))
        });
        if let Some(taken) = taken {
            let file = k.source.file.clone().unwrap_or_default();
            errors.push(config::ConfigError {
                file,
                line: k.source.line,
                key: format!("keys.{}.{}", k.table, k.key),
                message: format!("name {name} already used by {taken}"),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(join_errors(&errors))
    }
}

/// tmux's default `display-message` text.
const DEFAULT_MESSAGE: &str = "[#{session_name}] #{window_index}:#{window_name}, current pane #{pane_index} - (%H:%M %d-%b-%y)";

/// `display-message`: the text expanded against the target pane and the
/// current client (the one whose binding ran it, else the most recently
/// active one, as tmux's `cmd_find_best_client`), so `#{client_width}`
/// and the other `client_*` variables work. With `-p` it is printed;
/// without, it goes on that client's status line (`clients::
/// display_message`, which logs it for `show-messages`), and with no
/// client attached nothing happens, as in tmux 3.7b.
fn display_message(
    state: &mut State,
    ctx: &ClientCtx,
    c: DisplayMessage,
) -> Result<String, String> {
    // `-d` is checked first, as tmux 3.7b's `args_strtonum` (recorded).
    let delay = match c.delay.as_deref().map(str::parse::<i128>) {
        None => None,
        Some(Ok(n)) if n < 0 => return Err("delay too small".to_string()),
        Some(Ok(n)) if n > i128::from(u32::MAX) => return Err("delay too large".to_string()),
        Some(Ok(n)) => Some(std::time::Duration::from_millis(
            u64::try_from(n).unwrap_or(0),
        )),
        Some(Err(_)) => return Err("delay invalid".to_string()),
    };
    let model = &state.model;
    let text = c.text.as_deref().unwrap_or(DEFAULT_MESSAGE);
    // A tmux client's `-v` text is left in tmux's language by the front end.
    let tmux = ctx.dialect == crate::protocol::Dialect::Tmux;
    // tmux's target here can fail (`CMD_FIND_CANFAIL`): a `~` with nothing
    // marked is quietly no pane, and every variable expands to nothing.
    let pane = match model.resolve_pane(&c.target, ctx) {
        Ok(pane) => pane,
        Err(_) if c.target == Target::Session(SessionRef::Marked) => {
            let (out, trace) = render_message(&c, tmux, text, &NoVars);
            return Ok(if c.all {
                out
            } else if c.print {
                format!("{trace}{out}\n")
            } else {
                trace
            });
        }
        Err(e) => return Err(e),
    };
    // The session the target names, for a window linked into several.
    let vars = match model.resolve_winlink(&c.target, ctx) {
        Ok((session, _)) => FormatCtx::new(model, pane).in_session(session),
        Err(_) => FormatCtx::new(model, pane),
    };
    // `-c` names the client for the message; one that is not there shows
    // nothing, as tmux's `CMD_CLIENT_CANFAIL`. Formats see that client if
    // it shows the pane's session, else the current one.
    let current = match &c.client {
        Some(name) => clients::client_named(state, name),
        None => clients::current_client(state, ctx),
    };
    let session = model
        .resolve_winlink(&c.target, ctx)
        .ok()
        .map(|(s, _)| s)
        .or_else(|| {
            let w = model.windows.get(&model.panes.get(&pane)?.window)?;
            Some(w.session)
        });
    let for_formats = current
        .filter(|id| state.clients.get(id).map(|e| e.session) == session)
        .or_else(|| clients::current_client(state, ctx));
    let (out, trace) = match for_formats.and_then(|id| state.clients.get(&id)) {
        Some(client) => {
            let client = clients::ClientVars::new(model, client, Some(vars));
            render_message(&c, tmux, text, &ServerVars::fresh(state, pane, &client))
        }
        None => render_message(&c, tmux, text, &ServerVars::fresh(state, pane, &vars)),
    };
    // `-a` lists the variables and shows no message.
    if c.all {
        return Ok(out);
    }
    if c.print {
        return Ok(format!("{trace}{out}\n"));
    }
    if let Some(id) = current {
        clients::display_message(state, id, &out, delay, c.ignore_keys);
    }
    Ok(trace)
}

/// What `display-message` shows for `text`, and the lines `-v` prints
/// before it: `-a` lists every variable instead, and `-l` shows the text
/// as it is.
fn render_message(
    c: &DisplayMessage,
    tmux: bool,
    text: &str,
    vars: &dyn FormatVars,
) -> (String, String) {
    if c.all {
        return (crate::cmd::all_variables(vars), String::new());
    }
    if c.literal {
        return (text.to_string(), String::new());
    }
    if c.verbose {
        let (out, trace) = crate::cmd::expand_traced(text, vars, true, tmux);
        let lines: String = trace.iter().map(|l| format!("{l}\n")).collect();
        return (out, lines);
    }
    (expand_time(text, vars), String::new())
}

fn lines(
    state: &State,
    fmt: &str,
    level: Level,
    panes: impl Iterator<Item = (Option<SessionId>, PaneId)>,
) -> String {
    panes
        .map(|(s, p)| {
            let vars = in_session(FormatCtx::at(&state.model, p, level), s);
            format!("{}\n", expand(fmt, &ServerVars::fresh(state, p, &vars)))
        })
        .collect()
}

fn in_session(vars: FormatCtx<'_>, session: Option<SessionId>) -> FormatCtx<'_> {
    match session {
        Some(s) => vars.in_session(s),
        None => vars,
    }
}

/// The pane a window's formats expand against: its active pane.
fn active_pane(model: &Model, window: WindowId) -> Option<PaneId> {
    model.windows.get(&window).map(|w| w.active)
}

// tmux 3.7b's default `list-*` templates (`cmd-list-*.c`), in tmux's
// format language (`compat::tmux::formats::to_native` rewrites them as a
// tmux `-F`): the output a tmux-dialect `list-*` with no `-F` gives (FE.2).

pub(crate) const TMUX_LIST_SESSIONS: &str = "#{session_name}: #{session_windows} windows \
     (created #{t:session_created})#{?session_grouped, (group ,}\
     #{session_group}#{?session_grouped,),}#{?session_attached, (attached),}";

const TMUX_LIST_WINDOWS: &str = "#{window_index}: #{window_name}#{window_raw_flags} \
     (#{window_panes} panes) [#{window_width}x#{window_height}] \
     [layout #{window_layout}] #{window_id}#{?window_active, (active),}";

const TMUX_LIST_WINDOWS_ALL: &str = "#{session_name}:#{window_index}: \
     #{window_name}#{window_raw_flags} (#{window_panes} panes) \
     [#{window_width}x#{window_height}] ";

/// `list-panes`' template after its line's own prefix.
const TMUX_LIST_PANES: &str = "[#{pane_width}x#{pane_height}\
     #{?pane_floating_flag, #{pane_x}#,#{pane_y}#,#{pane_z}}] [history \
     #{history_size}/#{history_limit}, #{history_bytes} bytes] #{pane_id}\
     #{?pane_active, (active),}#{?pane_dead, (dead),}";

pub(crate) const TMUX_LIST_CLIENTS: &str = "#{client_name}: #{session_name} \
     [#{client_width}x#{client_height} #{client_termname}] \
     #{?#{!=:#{client_uid},#{uid}},[user #{?client_user,#{client_user},#{client_uid},}] ,}\
     #{?client_flags,(,}#{client_flags}#{?client_flags,),}";

/// A tmux template in grofe's format language.
pub(crate) fn tmux_template(template: &str) -> String {
    crate::compat::tmux::formats::to_native(template, crate::compat::tmux::formats::Jobs::Empty)
        .value
}

/// A `list-*` command's format: its `-F`, else tmux's default template
/// (only a tmux-dialect request with no `-F` gets here, [`super::route`]).
fn list_format(format: Option<String>, template: impl FnOnce() -> String) -> String {
    format.unwrap_or_else(|| tmux_template(&template()))
}

fn list_sessions(state: &State, c: ListSessions) -> Result<String, String> {
    let fmt = list_format(c.format.clone(), || TMUX_LIST_SESSIONS.to_string());
    let sessions = listed_sessions(state, &c)?;
    let panes = sessions
        .into_iter()
        .filter_map(|s| Some((Some(s), session_pane(&state.model, s)?)));
    Ok(lines(state, &fmt, Level::Session, panes))
}

fn list_windows(state: &State, ctx: &ClientCtx, c: ListWindows) -> Result<String, String> {
    let fmt = list_format(c.format.clone(), || match c.all {
        true => TMUX_LIST_WINDOWS_ALL.to_string(),
        false => TMUX_LIST_WINDOWS.to_string(),
    });
    let windows = listed_windows(state, ctx, &c)?;
    let panes = windows
        .into_iter()
        .filter_map(|(s, w)| Some((Some(s), active_pane(&state.model, w)?)));
    Ok(lines(state, &fmt, Level::Window, panes))
}

fn list_panes(state: &State, ctx: &ClientCtx, c: ListPanes) -> Result<String, String> {
    // `-a` lines start with the session and window, `-s` lines with the
    // window, others with the pane index alone.
    let fmt = list_format(c.format.clone(), || {
        let prefix = if c.all {
            "#{session_name}:#{window_index}.#{pane_index}: "
        } else if c.session {
            "#{window_index}.#{pane_index}: "
        } else {
            "#{pane_index}: "
        };
        format!("{prefix}{TMUX_LIST_PANES}")
    });
    let panes = listed_panes(state, ctx, &c)?;
    let panes = panes.into_iter().map(|(s, p)| (Some(s), p));
    Ok(lines(state, &fmt, Level::Pane, panes))
}

/// Whether a list's `-f` filter keeps the line of `pane` (tmux's
/// `format_true`: not empty and not `0`); no filter keeps every line.
fn kept(
    state: &State,
    filter: Option<&str>,
    level: Level,
    session: Option<SessionId>,
    pane: PaneId,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let vars = in_session(FormatCtx::at(&state.model, pane, level), session);
    let got = expand(filter, &ServerVars::new(state, pane, &vars));
    !got.is_empty() && got != "0"
}

/// The pane a session's formats expand against: its current window's
/// active pane.
fn session_pane(model: &Model, session: SessionId) -> Option<PaneId> {
    model
        .sessions
        .get(&session)
        .and_then(|s| active_pane(model, s.active))
}

/// Every session, by name as tmux keeps them.
fn sessions_by_name(model: &Model) -> Vec<SessionId> {
    let mut sessions: Vec<_> = model.sessions.values().map(|s| (&s.name, s.id)).collect();
    sessions.sort();
    sessions.into_iter().map(|(_, id)| id).collect()
}

/// The sessions `list-sessions` lists: all of them, by name or in the order
/// `-O` asks for, that `-f` keeps.
pub(crate) fn listed_sessions(state: &State, c: &ListSessions) -> Result<Vec<SessionId>, String> {
    let sort = Sort::new(&c.sort)?;
    Ok(sessions_sorted(&state.model, &sort)
        .into_iter()
        .filter(|s| {
            session_pane(&state.model, *s)
                .is_some_and(|p| kept(state, c.filter.as_deref(), Level::Session, Some(*s), p))
        })
        .collect())
}

/// Every session, by name, or in the order `sort` asks for.
pub(crate) fn sessions_sorted(model: &Model, sort: &Sort) -> Vec<SessionId> {
    let mut all = sessions_by_name(model);
    sort.apply(&mut all, |order, a, b| {
        let (sa, sb) = (&model.sessions[a], &model.sessions[b]);
        let first = match order {
            Order::Index => sa.id.cmp(&sb.id),
            Order::Creation => sa.created.cmp(&sb.created),
            Order::Activity => sb.activity.cmp(&sa.activity),
            Order::Name => by_name(&sa.name, &sb.name),
            _ => Ordering::Equal,
        };
        first.then_with(|| by_name(&sa.name, &sb.name))
    });
    all
}

/// How tmux's `sort_winlink_cmp` compares two windows of `model` as the
/// sessions `a` and `b` have them.
fn compare_windows(
    model: &Model,
    order: Order,
    a: (SessionId, WindowId),
    b: (SessionId, WindowId),
) -> Ordering {
    let (wa, wb) = (&model.windows[&a.1], &model.windows[&b.1]);
    let first = match order {
        Order::Index => {
            let index = |(s, w): (SessionId, WindowId)| {
                model.index_in(s, w).unwrap_or(model.windows[&w].index)
            };
            index(a).cmp(&index(b))
        }
        Order::Creation => a.1.cmp(&b.1),
        Order::Activity => wb.activity.cmp(&wa.activity),
        Order::Name => by_name(&wa.name, &wb.name),
        Order::Size => (u32::from(wa.cols) * u32::from(wa.rows))
            .cmp(&(u32::from(wb.cols) * u32::from(wb.rows))),
        _ => Ordering::Equal,
    };
    first.then_with(|| by_name(&wa.name, &wb.name))
}

/// The windows `list-windows` lists: the target session's, or with `-a`
/// every session's (by name), in index order or the order `-O` asks for,
/// that `-f` keeps. Each comes with the session it is listed in.
pub(crate) fn listed_windows(
    state: &State,
    ctx: &ClientCtx,
    c: &ListWindows,
) -> Result<Vec<(SessionId, WindowId)>, String> {
    let sort = Sort::new(&c.sort)?;
    let model = &state.model;
    let sessions = match c.all {
        true => sessions_by_name(model),
        false => vec![model.resolve_session(&c.target, ctx)?],
    };
    let mut all: Vec<(SessionId, WindowId)> = sessions
        .iter()
        .flat_map(|s| model.sessions[s].windows.iter().map(|w| (*s, *w)))
        .collect();
    sort.apply(&mut all, |order, a, b| {
        compare_windows(model, order, *a, *b)
    });
    Ok(all
        .into_iter()
        .filter(|(s, w)| {
            active_pane(model, *w)
                .is_some_and(|p| kept(state, c.filter.as_deref(), Level::Window, Some(*s), p))
        })
        .collect())
}

/// How tmux's `sort_pane_cmp` compares two panes of one window.
fn compare_panes(state: &State, order: Order, a: PaneId, b: PaneId) -> Ordering {
    let model = &state.model;
    let title = |p: PaneId| state.passthrough.title(p);
    let Some(window) = model
        .panes
        .get(&a)
        .and_then(|p| model.windows.get(&p.window))
    else {
        return Ordering::Equal;
    };
    let size = |p: PaneId| {
        model
            .panes
            .get(&p)
            .map_or(0, |p| u32::from(p.cols) * u32::from(p.rows))
    };
    let ids = window.pane_ids();
    let index = |p: PaneId| ids.iter().position(|i| *i == p).unwrap_or(0);
    let first = match order {
        Order::Activity => window
            .active_points
            .get(a)
            .cmp(&window.active_points.get(b)),
        Order::Creation => a.cmp(&b),
        Order::Size => size(a).cmp(&size(b)),
        Order::Index => index(a).cmp(&index(b)),
        Order::Name => by_name(&title(a), &title(b)),
        // tmux's `z_index` list, the top pane first: the floating panes by
        // height, then the tiled ones, the last in pane order first.
        Order::Z => {
            let z = |p: PaneId| match window.floating(p) {
                Some(f) => (0u8, -i64::from(f.z)),
                None => (1u8, -(index(p) as i64)),
            };
            z(a).cmp(&z(b))
        }
        _ => Ordering::Equal,
    };
    first.then_with(|| by_name(&title(a), &title(b)))
}

/// The panes `list-panes` lists: the target window's, with `-s` its
/// session's, with `-a` every session's, that `-f` keeps; each window's
/// panes in pane order or the order `-O` asks for. Each comes with the
/// session it is listed in.
pub(crate) fn listed_panes(
    state: &State,
    ctx: &ClientCtx,
    c: &ListPanes,
) -> Result<Vec<(SessionId, PaneId)>, String> {
    // The order is checked once; each window is sorted on its own.
    Sort::new(&c.sort)?;
    let sort = Sort::lenient(&c.sort);
    let model = &state.model;
    let windows: Vec<(SessionId, WindowId)> = if c.all {
        sessions_by_name(model)
            .iter()
            .flat_map(|s| model.sessions[s].windows.iter().map(|w| (*s, *w)))
            .collect()
    } else if c.session {
        // tmux finds `-t` as a window, then takes its session.
        let (s, _) = model.resolve_winlink(&c.target, ctx)?;
        model.sessions[&s].windows.iter().map(|w| (s, *w)).collect()
    } else {
        let (s, w) = model.resolve_winlink(&c.target, ctx)?;
        vec![(s, w)]
    };
    let mut out = Vec::new();
    for (s, w) in windows {
        let Some(window) = model.windows.get(&w) else {
            continue;
        };
        let mut panes = window.pane_ids();
        sort.apply(&mut panes, |order, a, b| {
            compare_panes(state, order, *a, *b)
        });
        out.extend(
            panes
                .into_iter()
                .filter(|p| kept(state, c.filter.as_deref(), Level::Pane, Some(s), *p))
                .map(|p| (s, p)),
        );
    }
    Ok(out)
}

/// Format variables (spec §4.5) for one pane, its window and its session.
pub struct FormatCtx<'a> {
    model: &'a Model,
    pane: PaneId,
    level: Level,
    /// The session the window is seen in, when it is linked into several
    /// (`link-window`): its index, current-ness and session variables are
    /// that session's. `None`: the window's first link.
    session: Option<SessionId>,
}

/// What a format is expanded for, as tmux's `format_tree` type: the
/// `pane_format`, `window_format` and `session_format` variables say which
/// (a `list-windows -F` is for windows, even though its variables read the
/// window's active pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Session,
    Window,
    Pane,
}

impl<'a> FormatCtx<'a> {
    pub fn new(model: &'a Model, pane: PaneId) -> FormatCtx<'a> {
        FormatCtx::at(model, pane, Level::Pane)
    }

    pub fn at(model: &'a Model, pane: PaneId, level: Level) -> FormatCtx<'a> {
        FormatCtx {
            model,
            pane,
            level,
            session: None,
        }
    }

    /// These variables for the window as `session` sees it.
    pub fn in_session(mut self, session: SessionId) -> FormatCtx<'a> {
        self.session = Some(session);
        self
    }

    /// Names of the sessions whose current window is `window`, a group
    /// once (by its first member), as tmux's `window_active_sessions_list`.
    fn active_sessions(&self, window: WindowId) -> Vec<String> {
        let m = self.model;
        let mut seen: Vec<SessionId> = Vec::new();
        let mut names = Vec::new();
        for s in m.sessions.values().filter(|s| s.active == window) {
            if seen.contains(&s.id) {
                continue;
            }
            seen.push(s.id);
            seen.extend(m.group_peers(s.id));
            names.push(s.name.clone());
        }
        names
    }

    /// These variables for `session`, unless one is set already.
    pub fn or_session(mut self, session: SessionId) -> FormatCtx<'a> {
        self.session = self.session.or(Some(session));
        self
    }

    /// The session the window of `w` is seen in.
    fn session_for(&self, w: &crate::server::model::Window) -> SessionId {
        self.session
            .filter(|s| self.model.has_link(*s, w.id))
            .unwrap_or(w.session)
    }

    pub fn pane_id(&self) -> PaneId {
        self.pane
    }

    /// The options a variable reads: the pane's chain, with the session the
    /// window is seen in (`status-left` of `b:7`, not of the window's first
    /// link).
    fn option_chain(&self) -> Vec<Owner> {
        let mut chain = chain(self.model, self.pane);
        if let Some(w) = self
            .model
            .panes
            .get(&self.pane)
            .and_then(|p| self.model.windows.get(&p.window))
            && let Some(Owner::Session(s)) = chain.last_mut()
        {
            *s = self.session_for(w);
        }
        chain
    }
}

/// One thing a session, window or pane loop visits.
pub(crate) struct LoopSpec {
    /// The pane its formats read: the active pane of a session's current
    /// window or of a window, else the pane itself.
    pub pane: PaneId,
    pub level: Level,
    pub active: bool,
    pub extras: Vec<(String, String)>,
    /// The session the item is seen in.
    pub session: Option<SessionId>,
}

/// What `#{S:}`, `#{W:}` and `#{P:}` visit from `pane`'s point of view,
/// sorted as tmux sorts them (`sort.c`): every session; the windows of
/// `pane`'s session; the panes of its window. `client_session` is the
/// session of the client the formats are for, which `#{S:}`'s second
/// branch is for. `None` for the clients loop (the server's) and a pane
/// that is gone.
pub(crate) fn loop_specs(
    model: &Model,
    pane: PaneId,
    in_session: Option<SessionId>,
    client_session: Option<SessionId>,
    kind: LoopKind,
    sort: LoopSort,
) -> Option<Vec<LoopSpec>> {
    let p = model.panes.get(&pane)?;
    let w = model.windows.get(&p.window)?;
    let s = in_session
        .filter(|s| model.has_link(*s, w.id))
        .unwrap_or(w.session);
    let s = model.sessions.get(&s)?;
    let by_name = |a: &str, b: &str| a.as_bytes().cmp(b.as_bytes());
    match kind {
        LoopKind::Sessions => {
            // tmux keeps its sessions in a tree by name.
            let mut all: Vec<&crate::server::model::Session> = model.sessions.values().collect();
            all.sort_by(|a, b| by_name(&a.name, &b.name));
            sort.apply(&mut all, |order, a, b| {
                let first = match order {
                    SortOrder::Index => a.id.cmp(&b.id),
                    SortOrder::Activity => b.activity.cmp(&a.activity),
                    SortOrder::Name => by_name(&a.name, &b.name),
                    _ => std::cmp::Ordering::Equal,
                };
                first.then_with(|| by_name(&a.name, &b.name))
            });
            Some(
                all.into_iter()
                    .filter_map(|o| {
                        let pane = model.windows.get(&o.active)?.active;
                        Some(LoopSpec {
                            pane,
                            level: Level::Session,
                            active: client_session == Some(o.id),
                            extras: Vec::new(),
                            session: Some(o.id),
                        })
                    })
                    .collect(),
            )
        }
        LoopKind::Windows => {
            let mut all: Vec<&crate::server::model::Window> = s
                .windows
                .iter()
                .filter_map(|id| model.windows.get(id))
                .collect();
            sort.apply(&mut all, |order, a, b| {
                let first = match order {
                    SortOrder::Name => by_name(&a.name, &b.name),
                    SortOrder::Activity => b.activity.cmp(&a.activity),
                    _ => std::cmp::Ordering::Equal,
                };
                first.then_with(|| by_name(&a.name, &b.name))
            });
            let n = all.len();
            let flag = |on: bool| if on { "1" } else { "0" }.to_string();
            Some(
                (0..n)
                    .map(|i| {
                        let this = all[i];
                        let mut extras = vec![
                            (
                                "window_after_active".to_string(),
                                flag(i > 0 && all[i - 1].id == s.active),
                            ),
                            (
                                "window_before_active".to_string(),
                                flag(i + 1 < n && all[i + 1].id == s.active),
                            ),
                        ];
                        for (prefix, other) in [
                            ("next", all.get(i + 1)),
                            ("prev", i.checked_sub(1).and_then(|j| all.get(j))),
                        ] {
                            let Some(other) = other else { continue };
                            let index = model.index_in(s.id, other.id).unwrap_or(other.index);
                            extras.push((format!("{prefix}_window_index"), index.to_string()));
                            extras.push((
                                format!("{prefix}_window_active"),
                                flag(other.id == s.active),
                            ));
                            for (name, entry) in model.options.at(Owner::Window(other.id)) {
                                if name.starts_with('@') {
                                    extras.push((format!("{prefix}_{name}"), entry.value.clone()));
                                }
                            }
                        }
                        LoopSpec {
                            pane: this.active,
                            level: Level::Window,
                            active: this.id == s.active,
                            extras,
                            session: Some(s.id),
                        }
                    })
                    .collect(),
            )
        }
        LoopKind::Panes => {
            // The sort is by pane id, whatever flags a `P` modifier has;
            // `r` reverses it.
            let mut all = w.pane_ids();
            sort.apply(&mut all, |_, a, b| a.cmp(b));
            Some(
                all.into_iter()
                    .map(|id| LoopSpec {
                        pane: id,
                        level: Level::Pane,
                        active: id == w.active,
                        extras: Vec::new(),
                        session: Some(s.id),
                    })
                    .collect(),
            )
        }
        LoopKind::Clients => None,
    }
}

fn flag(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// Format variables that need the server state, not only the model
/// (`socket_path`, `pane_title`, `pane_tty`, TC.11's `host` and
/// `host_short`, the mouse's, and a running hook's `#{hook*}`), over
/// `inner` for the rest.
/// Built where dispatch holds `&State`, the way `ClientVars` layers the
/// client's variables.
pub struct ServerVars<'a> {
    state: &'a State,
    pane: PaneId,
    inner: Inner<'a>,
    /// Read the foreground process now, not from the last sample.
    fresh: bool,
    /// What a fresh expansion has read so far: each is read at most once
    /// per expansion, and the members only when a format asks for them.
    group: OnceCell<Option<i32>>,
    leader: OnceCell<ProcSample>,
    members: OnceCell<Vec<Vec<String>>>,
}

impl<'a> ServerVars<'a> {
    /// The status line's and hooks' form: the foreground process comes
    /// from the pane's 1 s sample, so a redraw costs no syscalls for it.
    pub fn new(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars {
            state,
            pane,
            inner: Inner::Ref(inner),
            fresh: false,
            group: OnceCell::new(),
            leader: OnceCell::new(),
            members: OnceCell::new(),
        }
    }

    /// A command's form (a binding's conditional, `display-message`, `set
    /// -F`, `list-* -F`): `pane_current_command`, `pane_current_argv` and
    /// `pane_foreground_argv` read the pane's foreground process group at
    /// this moment, with syscalls and no fork, so a key pressed just after
    /// a program starts or exits sees the one in front.
    pub fn fresh(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars {
            fresh: true,
            ..ServerVars::new(state, pane, inner)
        }
    }

    fn foreground(&self, name: &str) -> Option<String> {
        let group = *self.group.get_or_init(|| {
            let p = self.state.model.panes.get(&self.pane)?;
            p.handle.foreground_group()
        });
        let group = group?;
        let leader = || self.leader.get_or_init(|| procinfo::leader(group));
        match name {
            "pane_current_command" => leader().command.clone(),
            "pane_current_argv" => leader().argv.as_ref().map(|a| a.join(" ")),
            _ => {
                let members = self.members.get_or_init(|| procinfo::members(group));
                (!members.is_empty()).then(|| procinfo::argv_lines(members))
            }
        }
    }
}

impl FormatVars for ServerVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        // The clock is the newest mode of a pane showing it.
        if let Some(v) = crate::server::clock::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // Copy mode's variables while the pane is in it (TM.1).
        if let Some(v) = crate::server::copy::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // A chooser open over the pane is a mode too (TP.9b).
        if let Some(v) = crate::server::tree::format_var(self.state, self.pane, name) {
            return Some(v);
        }
        // tmux 3.7b reads it from the screen on view, so a pane in any mode
        // (whatever answers `pane_in_mode`) is 0 (TM.7).
        if name == "bracket_paste_flag"
            && (crate::server::clock::format_var(self.state, self.pane, "pane_in_mode").is_some()
                || crate::server::copy::format_var(self.state, self.pane, "pane_in_mode")
                    .as_deref()
                    == Some("1")
                || crate::server::tree::format_var(self.state, self.pane, "pane_in_mode").is_some())
        {
            return Some("0".to_string());
        }
        // The mouse event a mouse key's commands run for, and the view (TC.10).
        if let Some(v) = crate::server::mouse::format_var(self.state, name) {
            return Some(v);
        }
        // The newest automatic buffer's `#{buffer_*}` (TM.4).
        if let Some(v) = crate::server::buffers::format_var(self.state, name) {
            return Some(v);
        }
        // Inside a hook, its `#{hook*}` variables (TC.5).
        let hook = crate::server::hooks::current_formats(self.state);
        if let Some(v) = hook.and_then(|f| f.get(name)) {
            return Some(v);
        }
        match name {
            "socket_path" => Some(self.state.socket.display().to_string()),
            "pane_title" => Some(self.state.passthrough.title(self.pane)),
            "host" => Some(crate::server::passthrough::hostname().to_string()),
            // tmux: the host name up to its first dot.
            "host_short" => {
                let host = crate::server::passthrough::hostname();
                Some(host.split('.').next().unwrap_or(host).to_string())
            }
            "pane_tty" => {
                let p = self.state.model.panes.get(&self.pane)?;
                Some(p.handle.tty_path()?.display().to_string())
            }
            "pane_current_command" | "pane_current_argv" | "pane_foreground_argv" if self.fresh => {
                self.foreground(name)
                    .or_else(|| self.inner.vars().get(name))
            }
            "session_attached_list" => self.attached_list(),
            "session_group_attached"
            | "session_group_attached_list"
            | "session_group_many_attached" => {
                let session = self.session_id()?;
                self.state.model.group_of(session)?;
                let names = self.group_attached()?;
                Some(match name {
                    "session_group_attached" => names.len().to_string(),
                    "session_group_many_attached" => flag(names.len() > 1),
                    _ => names.join(","),
                })
            }
            _ => self.inner.vars().get(name),
        }
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        if kind == LoopKind::Clients {
            return self.client_items(sort);
        }
        let items = self.inner.vars().loop_items(kind, sort)?;
        Some(
            items
                .into_iter()
                .map(|it| {
                    let pane = it.pane.unwrap_or(self.pane);
                    LoopItem {
                        vars: Box::new(self.rebuild(Inner::Own(it.vars), pane)),
                        pane: Some(pane),
                        active: it.active,
                        extras: it.extras,
                    }
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        self.inner.vars().name_exists(kind, name)
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        self.inner.vars().pane_search(term, regex, ignore)
    }

    fn reexpand(&self, text: &str) -> String {
        self.inner.vars().reexpand(text)
    }
}

impl<'a> ServerVars<'a> {
    /// This layer over other variables and the pane they read: what a
    /// loop's item sees.
    fn rebuild<'x>(&'x self, inner: Inner<'x>, pane: PaneId) -> ServerVars<'x> {
        ServerVars {
            state: self.state,
            pane,
            inner,
            fresh: self.fresh,
            group: OnceCell::new(),
            leader: OnceCell::new(),
            members: OnceCell::new(),
        }
    }

    /// `#{session_attached_list}`: the names of the clients showing the
    /// pane's session, comma-joined.
    fn attached_list(&self) -> Option<String> {
        let session = self.session_id()?;
        let names: Vec<&str> = self
            .state
            .clients
            .values()
            .filter(|c| c.session == session)
            .map(|c| c.name.as_str())
            .collect();
        Some(names.join(","))
    }

    /// The session the formats are for (`#{session_id}`).
    fn session_id(&self) -> Option<SessionId> {
        let id = self.inner.vars().get("session_id")?;
        Some(SessionId(id.strip_prefix('$')?.parse().ok()?))
    }

    /// The clients showing any session of the session's group, else of the
    /// session: names, in client order.
    fn group_attached(&self) -> Option<Vec<&str>> {
        let session = self.session_id()?;
        let m = &self.state.model;
        let mut sessions = m.group_peers(session);
        sessions.insert(0, session);
        Some(
            self.state
                .clients
                .values()
                .filter(|c| sessions.contains(&c.session))
                .map(|c| c.name.as_str())
                .collect(),
        )
    }

    /// `#{L:}`: every client, each over the pane this layer reads.
    fn client_items(&self, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let model = &self.state.model;
        let mut all: Vec<&clients::ClientEntry> = self.state.clients.values().collect();
        sort.apply(&mut all, |order, a, b| {
            let first = match order {
                SortOrder::Name => a.name.as_bytes().cmp(b.name.as_bytes()),
                SortOrder::Activity => b.activity_time.cmp(&a.activity_time),
                _ => std::cmp::Ordering::Equal,
            };
            first.then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
        });
        Some(
            all.into_iter()
                .map(|c| {
                    let client =
                        clients::ClientVars::new(model, c, Some(FormatCtx::new(model, self.pane)));
                    LoopItem {
                        vars: Box::new(self.rebuild(Inner::Own(Box::new(client)), self.pane)),
                        pane: Some(self.pane),
                        active: false,
                        extras: Vec::new(),
                    }
                })
                .collect(),
        )
    }
}

/// The tmux version a server with tmux identity reports (`#{version}`,
/// `tmux -V`'s number): the release the compat suite is recorded against.
const TMUX_COMPAT_VERSION: &str = "3.7b";

impl FormatVars for FormatCtx<'_> {
    fn get(&self, name: &str) -> Option<String> {
        use crate::compat::Identity;
        use crate::types::MouseMode;
        let m = self.model;
        if name.starts_with('@') || builtin(name).is_some() {
            let value = m.options.lookup(name, &self.option_chain())?;
            // A flag is 1 or 0 in a format (tmux's `options_to_string`
            // with `numeric`), so `#{?synchronize-panes,...}` is false off.
            return Some(match (builtin(name).map(|b| b.kind), value) {
                (Some(Kind::Bool), "on") => "1".to_string(),
                (Some(Kind::Bool), "off") => "0".to_string(),
                _ => value.to_string(),
            });
        }
        match name {
            "version" => {
                return Some(
                    match m.identity {
                        Identity::Grofe => env!("CARGO_PKG_VERSION"),
                        Identity::Tmux => TMUX_COMPAT_VERSION,
                    }
                    .to_string(),
                );
            }
            "grofe_version" => return Some(env!("CARGO_PKG_VERSION").to_string()),
            // Outside a loop; the loop's items say otherwise.
            "loop_last_flag" => return Some("0".to_string()),
            "server_sessions" => return Some(m.sessions.len().to_string()),
            "uid" => return Some(nix::unistd::getuid().as_raw().to_string()),
            "tree_mode_format" => return Some(TREE_MODE_FORMAT.to_string()),
            "buffer_mode_format" => return Some(BUFFER_MODE_FORMAT.to_string()),
            "client_mode_format" => return Some(CLIENT_MODE_FORMAT.to_string()),
            "pid" => return Some(std::process::id().to_string()),
            "start_time" => {
                let since = m.started?.duration_since(std::time::UNIX_EPOCH).ok()?;
                return Some(since.as_secs().to_string());
            }
            // ponytail: stubs until the features exist: copy mode (M5),
            // and the status range under
            // the mouse, which only a mouse-bound command sees (TC.10).
            // `client_tty` is a client's (`ClientVars`), empty with none.
            "copy_cursor_word" | "pane_mode" | "mouse_status_range" | "client_tty"
            | "client_pid" => return Some(String::new()),
            "pane_in_mode" => return Some("0".to_string()),
            "pane_marked" => return Some(flag(m.marked_pane() == Some(self.pane))),
            "pane_marked_set" => return Some(flag(m.marked_pane().is_some())),
            "window_marked_flag" | "session_marked" => {
                let here = m.panes.get(&self.pane)?.window;
                let in_session = self.session_for(m.windows.get(&here)?);
                // The mark names the session and window link it was made in.
                let same = match m.marked_pane().and(m.marked) {
                    Some((session, _, _)) if name == "session_marked" => session == in_session,
                    Some((session, window, _)) => session == in_session && window == here,
                    None => false,
                };
                return Some(flag(same));
            }
            // The search string of a copy mode, which answers first.
            "pane_search_string" => return Some(String::new()),
            _ => {}
        }
        let p = m.panes.get(&self.pane)?;
        let w = m.windows.get(&p.window)?;
        let s = m.sessions.get(&self.session_for(w))?;
        let index_of = |id: &WindowId| {
            m.index_in(s.id, *id)
                .or_else(|| Some(m.windows.get(id)?.index))
        };
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "window_index" => index_of(&w.id)?.to_string(),
            "window_name" => w.name.clone(),
            "window_id" => w.id.to_string(),
            "window_active" => flag(s.active == w.id),
            "window_zoomed_flag" => flag(w.zoomed),
            "window_width" => w.cols.to_string(),
            "window_bell_flag" => flag(w.bell_flag),
            "window_activity_flag" => flag(w.activity_flag),
            "window_silence_flag" => flag(w.silence_flag),
            "window_height" => w.rows.to_string(),
            "window_panes" => w.pane_ids().len().to_string(),
            "pane_floating_flag" => flag(w.floating(p.id).is_some()),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s
                .created
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string(),
            "session_attached" => s.attached.to_string(),
            "session_path" => s.cwd.display().to_string(),
            "window_layout" => window_layout(w),
            "window_raw_flags" => window_flags(m, s, w),
            "pane_id" => p.id.to_string(),
            "pane_uid" => p.uid.clone(),
            "pane_index" => {
                let pos = w.pane_ids().iter().position(|id| *id == p.id)?;
                (pos as u32 + m.pane_base_index(w.id)).to_string()
            }
            "pane_active" => flag(w.active == p.id),
            "pane_pid" => p.handle.pid()?.to_string(),
            "pane_current_command" => p.handle.current_command()?,
            "pane_current_argv" => p.handle.current_argv()?.join(" "),
            // Every member of the foreground group, one argv per line; the
            // sample holds only the leader's (a command reads them all,
            // `ServerVars::fresh`).
            "pane_foreground_argv" => procinfo::argv_lines(&[p.handle.current_argv()?]),
            "pane_current_path" => p
                .handle
                .current_path()
                .unwrap_or_else(|| p.cwd.clone())
                .display()
                .to_string(),
            // What the pane reported with OSC 7, as tmux shows it.
            "pane_path" => p.handle.reported_path().unwrap_or_default(),
            "pane_left" => match w.floating(p.id) {
                Some(f) => f.x.to_string(),
                None => pane_origin(w, p.id)?.0.to_string(),
            },
            "pane_top" => match w.floating(p.id) {
                Some(f) => f.y.to_string(),
                None => pane_origin(w, p.id)?.1.to_string(),
            },
            // tmux's names for the same corner, and the far one.
            "pane_x" => self.get("pane_left")?,
            "pane_y" => self.get("pane_top")?,
            "pane_right" => {
                let left: u32 = self.get("pane_left")?.parse().ok()?;
                (left + u32::from(p.cols)).saturating_sub(1).to_string()
            }
            "pane_bottom" => {
                let top: u32 = self.get("pane_top")?.parse().ok()?;
                (top + u32::from(p.rows)).saturating_sub(1).to_string()
            }
            // Whether the pane touches that edge of the window, as tmux's
            // format.c decides: a border status moves the top and bottom
            // edges in by the row it takes.
            "pane_at_left" => flag(self.get("pane_left")? == "0"),
            "pane_at_top" => {
                let top = if w.pane_status == PaneStatus::Top {
                    "1"
                } else {
                    "0"
                };
                flag(self.get("pane_top")? == top)
            }
            "pane_at_right" => {
                let left: u32 = self.get("pane_left")?.parse().ok()?;
                flag(left + u32::from(p.cols) >= u32::from(w.cols))
            }
            "pane_at_bottom" => {
                let top: u32 = self.get("pane_top")?.parse().ok()?;
                let edge = u32::from(w.rows) - u32::from(w.pane_status == PaneStatus::Bottom);
                flag(top + u32::from(p.rows) >= edge)
            }
            // The place in the stack, 0 on top: the floating panes, then
            // every tiled pane one below them all (tmux's z_index).
            "pane_z" => match w.floating(p.id) {
                Some(f) => w.floating.iter().filter(|o| o.z > f.z).count(),
                None => w.floating.len() + 1,
            }
            .to_string(),
            "pane_width" => p.cols.to_string(),
            "pane_height" => p.rows.to_string(),
            "pane_dead" => flag(p.dead.is_some()),
            "pane_pipe" => flag(p.handle.pipe_open()),
            "pane_pipe_pid" => p.handle.pipe_pid()?.to_string(),
            // Empty for a live pane and for one a signal killed, as tmux.
            "pane_dead_status" => p.dead.flatten()?.to_string(),
            "pane_start_command" => p
                .argv
                .as_deref()
                .map(|argv| {
                    argv.iter()
                        .map(|a| escape_arg(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            "alternate_on" => flag(p.handle.modes().alt_screen),
            "mouse_any_flag" => flag(p.handle.modes().mouse != MouseMode::Off),
            // 0 in a mode: `ServerVars::get`.
            "bracket_paste_flag" => flag(p.handle.modes().bracketed_paste),
            "history_size" => p.handle.history_size().to_string(),
            "history_limit" => m.history_limit(s.id).to_string(),
            // tmux's grid: 40 bytes a line, history and screen.
            // ponytail: tmux adds 5 bytes for each cell a line has
            // allocated, which depends on how its grid grew; left out.
            "history_bytes" => ((p.handle.history_size() + usize::from(p.rows)) * 40).to_string(),
            "window_flags" => window_flags(m, s, w),
            "pane_format" => flag(self.level == Level::Pane),
            "window_format" => flag(self.level == Level::Window),
            "session_format" => flag(self.level == Level::Session),
            "window_last_flag" => flag(s.last_windows.last() == Some(&w.id)),
            "pane_last" => flag(w.last_pane() == Some(p.id)),
            // tmux's `window_pane_printable_flags`.
            "pane_flags" => [
                (w.active == p.id, '*'),
                (w.last_pane() == Some(p.id), '-'),
                (w.zoomed && w.active == p.id, 'Z'),
                (w.floating(p.id).is_some(), 'F'),
            ]
            .into_iter()
            .filter_map(|(on, c)| on.then_some(c))
            .collect(),
            "window_start_flag" => flag(s.windows.first() == Some(&w.id)),
            "window_end_flag" => flag(s.windows.last() == Some(&w.id)),
            "window_activity" => epoch_secs(w.activity),
            "session_activity" => epoch_secs(std::time::SystemTime::now() - s.activity.elapsed()),
            "active_window_index" => index_of(&s.active)?.to_string(),
            "last_window_index" => s.windows.iter().filter_map(index_of).max()?.to_string(),
            "session_many_attached" => flag(s.attached > 1),
            "pane_zoomed_flag" => flag(w.zoomed && w.active == p.id),
            // The sessions (or groups, once each) whose current window it is.
            "window_active_sessions" => self.active_sessions(w.id).len().to_string(),
            "window_active_sessions_list" => self.active_sessions(w.id).join(","),
            "window_linked_sessions_list" => w
                .links
                .iter()
                .filter_map(|(l, _)| m.sessions.get(l).map(|s| s.name.as_str()))
                .collect::<Vec<_>>()
                .join(","),
            "session_grouped" => flag(m.group_of(s.id).is_some()),
            "session_group" => m.group_of(s.id)?.name.clone(),
            "session_group_size" => m.group_of(s.id)?.members.len().to_string(),
            "session_group_list" => m
                .group_of(s.id)?
                .members
                .iter()
                .filter_map(|l| m.sessions.get(l).map(|s| s.name.as_str()))
                .collect::<Vec<_>>()
                .join(","),
            // Each kind of alert once, windows in index order.
            "session_alert" => {
                let windows: Vec<_> = s
                    .windows
                    .iter()
                    .filter_map(|id| m.windows.get(id))
                    .collect();
                [
                    ('#', windows.iter().any(|w| w.activity_flag)),
                    ('!', windows.iter().any(|w| w.bell_flag)),
                    ('~', windows.iter().any(|w| w.silence_flag)),
                ]
                .into_iter()
                .filter_map(|(c, on)| on.then_some(c))
                .collect()
            }
            // `idx` and its alerts, for each window that has any.
            "session_alerts" => s
                .windows
                .iter()
                .filter_map(|id| m.windows.get(id))
                .filter(|w| w.activity_flag || w.bell_flag || w.silence_flag)
                .map(|w| {
                    format!(
                        "{}{}{}{}",
                        index_of(&w.id).unwrap_or(w.index),
                        if w.activity_flag { "#" } else { "" },
                        if w.bell_flag { "!" } else { "" },
                        if w.silence_flag { "~" } else { "" },
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
            // The windows before this one, most recent first.
            "session_stack" => std::iter::once(index_of(&s.active)?)
                .chain(s.last_windows.iter().rev().filter_map(index_of))
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(","),
            "window_stack_index" => s
                .last_windows
                .iter()
                .rev()
                .position(|id| *id == w.id)
                .map_or(0, |at| at + 1)
                .to_string(),
            "pane_input_off" => {
                return Some(flag(m.panes.get(&self.pane)?.input_off));
            }
            "pane_synchronized" => {
                flag(m.options.lookup("synchronize-panes", &self.option_chain()) == Some("on"))
            }
            "pane_start_path" => p.cwd.display().to_string(),
            "window_linked" => flag(w.links.len() > 1),
            "window_linked_sessions" => m.linked_units(w.id).to_string(),
            // ponytail: `client_width`/`client_height` need clients (M2);
            // `host` and `host_short` are `ServerVars`' (TC.11).
            // The emulator's screen state (`cursor_x`, `wrap_flag`, ...).
            _ => return p.handle.term_var(name),
        })
    }

    fn loop_items(&self, kind: LoopKind, sort: LoopSort) -> Option<Vec<LoopItem<'_>>> {
        let specs = loop_specs(self.model, self.pane, self.session, None, kind, sort)?;
        Some(
            specs
                .into_iter()
                .map(|s| LoopItem {
                    vars: Box::new(in_session(
                        FormatCtx::at(self.model, s.pane, s.level),
                        s.session,
                    )),
                    pane: Some(s.pane),
                    active: s.active,
                    extras: s.extras,
                })
                .collect(),
        )
    }

    fn name_exists(&self, kind: NameKind, name: &str) -> Option<bool> {
        let m = self.model;
        match kind {
            NameKind::Session => Some(m.sessions.values().any(|s| s.name == name)),
            NameKind::Window => {
                let p = m.panes.get(&self.pane)?;
                let s = m.sessions.get(&m.windows.get(&p.window)?.session)?;
                Some(
                    s.windows
                        .iter()
                        .filter_map(|id| m.windows.get(id))
                        .any(|w| w.name == name),
                )
            }
        }
    }

    fn pane_search(&self, term: &str, regex: bool, ignore: bool) -> Option<u32> {
        let p = self.model.panes.get(&self.pane)?;
        let visible = crate::types::CaptureOpts {
            start: None,
            end: None,
            escapes: false,
        };
        let text = p.handle.capture(&visible);
        Some(crate::cmd::format::search_lines(
            text.lines(),
            term,
            regex,
            ignore,
        ))
    }

    fn reexpand(&self, text: &str) -> String {
        use crate::compat::Identity;
        match self.model.identity {
            Identity::Tmux => {
                crate::compat::tmux::formats::to_native(
                    text,
                    crate::compat::tmux::formats::Jobs::Empty,
                )
                .value
            }
            Identity::Grofe => text.to_string(),
        }
    }
}

/// Seconds since the epoch, as tmux prints a time variable.
fn epoch_secs(t: std::time::SystemTime) -> String {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .to_string()
}

/// tmux 3.7b's `window_tree_mode.default_format`, `window_buffer_mode`'s and
/// `window_client_mode`'s (`window-tree.c`, `window-buffer.c`,
/// `window-client.c`): what `#{tree_mode_format}` and the like are.
const TREE_MODE_FORMAT: &str = concat!(
    "#{?pane_format,",
    "#{?pane_marked,#[reverse],}#{?pane_floating_flag,#[italics],}",
    "#{pane_current_command}#{pane_flags}",
    "#{?#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}},: \"#{pane_title}\",}",
    ",window_format,",
    "#{?window_marked_flag,#[reverse],}",
    "#{window_name}#{window_flags}",
    "#{?#{&&:#{==:#{window_panes},1},#{&&:#{pane_title},#{!=:#{pane_title},#{host_short}}}},: \"#{pane_title}\",}",
    ",",
    "#{session_windows} windows",
    "#{?session_grouped, (group #{session_group}: #{session_group_list}),}",
    "#{?session_attached, (attached),}",
    "}"
);
const BUFFER_MODE_FORMAT: &str = "#{t/p:buffer_created}: #{buffer_sample}";
const CLIENT_MODE_FORMAT: &str = "#{t/p:client_activity}: session #{session_name}";

/// tmux's `window_layout`: the layout string of the window's tiled and
/// floating panes (`layout::tmux_layout`).
pub(crate) fn window_layout(w: &crate::server::model::Window) -> String {
    use crate::server::layout::{FloatingCell, tmux_layout};
    let floating: Vec<FloatingCell> = w
        .floating
        .iter()
        .map(|f| FloatingCell {
            pane: f.pane,
            x: f.x,
            y: f.y,
            cols: f.cols,
            rows: f.rows,
            z: f.z,
        })
        .collect();
    tmux_layout(&w.layout.export(w.area()), &floating, !w.zoomed)
}

/// tmux's `window_flags`: `#` activity (written `##`, as tmux escapes it
/// there), `!` bell, `~` silence, `*` current, `-` last, `M` holds the
/// marked pane, `Z` zoomed, in tmux's order.
fn window_flags(
    m: &Model,
    s: &crate::server::model::Session,
    w: &crate::server::model::Window,
) -> String {
    let marked = m
        .marked_pane()
        .and(m.marked)
        .is_some_and(|(session, window, _)| session == s.id && window == w.id);
    let activity = if w.activity_flag { "##" } else { "" };
    let rest: String = [
        (w.bell_flag, '!'),
        (w.silence_flag, '~'),
        (s.active == w.id, '*'),
        (s.last_windows.last() == Some(&w.id), '-'),
        (marked, 'M'),
        (w.zoomed, 'Z'),
    ]
    .into_iter()
    .filter_map(|(on, c)| on.then_some(c))
    .collect();
    format!("{activity}{rest}")
}

pub(crate) use crate::cmd::format::escape_arg;

/// A pane's top-left cell in its window: the layout's, or the window's
/// origin for a zoomed window's active pane.
fn pane_origin(w: &crate::server::model::Window, pane: PaneId) -> Option<(u16, u16)> {
    if w.zoomed && w.active == pane {
        let r = w.zoom_rect();
        return Some((r.x, r.y));
    }
    w.tiled_rects()
        .into_iter()
        .find(|(id, _)| *id == pane)
        .map(|(_, r)| (r.x, r.y))
}

// TC.2b: list-valued options.
//
// A list option is tmux's array option: a sparse map from index to text,
// read and written with the same grammar tmux 3.7b uses, so the native
// form and FE.3's tmux form are one form:
//
// - `set name[N] value` sets element N; the list may have holes.
// - `set name value` replaces the whole list with element 0.
// - `set -a name value` fills the lowest free index (a hole first);
//   `set -a name[N] value` appends the text to element N.
// - `set -u name[N]` removes element N and leaves a hole: later indices
//   do not shift (tmux's answer, recorded). `set -u name` puts a global
//   list back to its default and removes a list set anywhere else.
// - `show name` prints `name[N] value` per element (none for an empty
//   list: the bare name); `show name[N]` prints `''` for a hole; `-v`
//   drops the names.
// - The index is read as C's `%u` (blanks, a sign, digits, wrapping
//   modulo 2^32); anything malformed, or above `i32::MAX`, is
//   `ambiguous option: <name>`. An index on a scalar is `not an array`
//   for `set` and shows the scalar's value for `show`.
//
// A lookup takes the whole list from the first owner that has one (a
// session's list hides the global one entirely), as tmux's does.

/// A built-in list option: the scope used when no flag is given, and its
/// global default, element by element from index 0.
struct ListBuiltin {
    name: &'static str,
    scope: OptionScope,
    default: &'static [&'static str],
}

// ponytail: `status-format`'s default row 0 is empty, which TC.11 renders
// as the built-in row (status-left, window list, status-right), since the
// native format language has no window-list loop to spell it with.
// grofe's own aliases have no default (tmux's six name verbs grofe lacks).
const LISTS: &[ListBuiltin] = &[
    ListBuiltin {
        name: "command-alias",
        scope: OptionScope::Global,
        default: &[],
    },
    ListBuiltin {
        name: "status-format",
        scope: OptionScope::Session,
        default: &[
            "",
            "#[align=left]#{R: ,#{n:#{session_name}}}P: #[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{P:#[range=pane|#{pane_id} #{E:pane-status-style}]#[push-default]#{T:window-pane-status-format}#[pop-default]#[norange list=on default]  ,#[range=pane|#{pane_id} list=focus #{?#{!=:#{E:pane-status-current-style},default},#{E:pane-status-current-style},#{E:pane-status-style}}]#[push-default]#{T:window-pane-current-status-format}#[pop-default]#[norange list=on default] }",
            "#[align=left]#{R: ,#{n:#{session_name}}}S: #[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{S:#[range=session|#{session_id} #{E:session-status-style}]#[push-default]#{session_name}#{session_alert}#[pop-default]#[norange list=on default]  ,#[range=session|#{session_id} list=focus #{?#{!=:#{E:session-status-current-style},default},#{E:session-status-current-style},#{E:session-status-style}}]#[push-default]#{session_name}*#{session_alert}#[pop-default]#[norange list=on default] }",
        ],
    },
    // TC.5: the client variables a session takes at new-session, attach
    // and switch-client, tmux 3.7b's default (recorded).
    ListBuiltin {
        name: "update-environment",
        scope: OptionScope::Session,
        default: &[
            "DISPLAY",
            "KRB5CCNAME",
            "MSYSTEM",
            "SSH_ASKPASS",
            "SSH_AUTH_SOCK",
            "SSH_AGENT_PID",
            "SSH_CONNECTION",
            "WAYLAND_DISPLAY",
            "WINDOWID",
            "XAUTHORITY",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "XDG_SESSION_TYPE",
        ],
    },
    // FE.1: tmux 3.7b's list and default (recorded); no effect, since the
    // probe decides what the outer terminal can do (spec D7, §4.5).
    ListBuiltin {
        name: "terminal-features",
        scope: OptionScope::Global,
        default: &[
            "xterm*:clipboard:ccolour:cstyle:focus:title",
            "screen*:title",
            "rxvt*:ignorefkeys",
        ],
    },
    // B1a: tmux 3.7b's (recorded); no effect, as `terminal-features`.
    ListBuiltin {
        name: "terminal-overrides",
        scope: OptionScope::Global,
        default: &["linux*:AX@"],
    },
    // B2a: tmux 3.7b's arrays with no effect (recorded defaults).
    ListBuiltin {
        name: "codepoint-widths",
        scope: OptionScope::Global,
        default: &[],
    },
    ListBuiltin {
        name: "pane-colours",
        scope: OptionScope::Window,
        default: &[],
    },
    ListBuiltin {
        name: "user-keys",
        scope: OptionScope::Global,
        default: &[],
    },
];

/// The separator tmux 3.7b splits a list option's value on when it is set
/// or appended without an index (recorded); `status-format` has none.
fn list_separator(name: &str) -> Option<char> {
    match name {
        "terminal-features" | "terminal-overrides" | "command-alias" | "codepoint-widths"
        | "pane-colours" | "user-keys" => Some(','),
        "update-environment" => Some(' '),
        _ => None,
    }
}

fn pieces(value: &str, sep: char) -> impl Iterator<Item = String> + '_ {
    value
        .split(sep)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// `op` as the edits tmux makes: a list with a separator takes a value set
/// or appended without an index as one element per piece, empty pieces
/// skipped (`set -as terminal-features ',x:RGB'` adds `x:RGB`).
pub(crate) fn by_separator(op: ListOp, base: &str) -> Vec<ListOp> {
    match (op, list_separator(base)) {
        (ListOp::Push(value), Some(sep)) => pieces(&value, sep).map(ListOp::Push).collect(),
        (ListOp::Replace(value), Some(sep)) => {
            vec![ListOp::Items((0..).zip(pieces(&value, sep)).collect())]
        }
        (op, _) => vec![op],
    }
}

fn list_builtin(name: &str) -> Option<&'static ListBuiltin> {
    LISTS.iter().find(|l| l.name == name)
}

/// Every built-in list option's name, in table order.
pub fn list_names() -> impl Iterator<Item = &'static str> {
    LISTS.iter().map(|l| l.name)
}

/// Options grofe accepts and stores but never reads (spec §4.5, "Options
/// by tmux name"), by tmux name. `show-config --explain` marks them
/// `(no effect)`. FE.1's rows: every such option in the corpus, each with
/// its `BUILTINS` or `LISTS` entry.
const NO_EFFECT: &[&str] = &[
    "aggressive-resize",
    "extended-keys",
    "extended-keys-format",
    "terminal-features",
    "terminal-overrides",
    "assume-paste-time",
    "key-table",
    "lock-after-time",
    "lock-command",
    "prompt-cursor-colour",
    "prompt-cursor-style",
    "prompt-command-cursor-style",
    "cursor-colour",
    "cursor-style",
    "alternate-screen",
    "copy-mode-mark-style",
    "copy-mode-selection-style",
    "copy-mode-current-line-number-style",
    "copy-mode-line-number-style",
    "copy-mode-line-numbers",
    "pane-scrollbars",
    "pane-scrollbars-style",
    "pane-scrollbars-position",
    "scroll-on-clear",
    "xterm-keys",
    "backspace",
    "default-client-command",
    "editor",
    "history-file",
    "input-buffer-size",
    "prompt-history-limit",
    "variation-selector-always-wide",
    "codepoint-widths",
    "pane-colours",
];

/// Whether grofe stores `name` without ever acting on it.
pub fn has_no_effect(name: &str) -> bool {
    NO_EFFECT.contains(&name)
}

/// One list option's elements at one owner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListEntry {
    pub items: BTreeMap<u32, String>,
    /// Set with `--persist`, as [`Entry::persist`].
    pub persist: bool,
}

fn default_list(l: &ListBuiltin) -> ListEntry {
    ListEntry {
        items: (0..).zip(l.default.iter().map(|s| s.to_string())).collect(),
        persist: false,
    }
}

fn default_lists() -> BTreeMap<String, ListEntry> {
    LISTS
        .iter()
        .map(|l| (l.name.to_string(), default_list(l)))
        .collect()
}

/// One change to a list option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListOp {
    /// Element `index` becomes the text, or gains it at its end.
    Set {
        index: u32,
        value: String,
        append: bool,
    },
    /// The text fills the lowest free index.
    Push(String),
    /// The list becomes just the text at index 0.
    Replace(String),
    /// The list becomes exactly these elements.
    Items(BTreeMap<u32, String>),
    /// Element `index` goes; later ones keep their indices.
    Remove(u32),
    /// Back to the default (global) or gone (elsewhere).
    Reset,
}

impl Options {
    /// The list `name` set at exactly `owner`.
    pub fn list(&self, owner: Owner, name: &str) -> Option<&ListEntry> {
        self.lists.get(&owner)?.get(name)
    }

    /// Every list set at exactly `owner`, by name.
    pub fn lists_at(&self, owner: Owner) -> impl Iterator<Item = (&String, &ListEntry)> {
        self.lists.get(&owner).into_iter().flatten()
    }

    /// The whole list from the first owner in `chain`, then global, that
    /// has one.
    pub fn lookup_list(&self, name: &str, chain: &[Owner]) -> Option<&BTreeMap<u32, String>> {
        chain
            .iter()
            .chain([&Owner::Global])
            .find_map(|o| self.list(*o, name))
            .map(|l| &l.items)
    }

    /// Applies `op` to the list `name` at `owner`. Returns whether the
    /// list changed; an unknown list name is an error.
    pub fn edit_list(
        &mut self,
        owner: Owner,
        name: &str,
        op: ListOp,
        persist: bool,
    ) -> Result<bool, String> {
        let l = list_builtin(name).ok_or_else(|| format!("invalid option: {name}"))?;
        // As tmux: removing from a list this owner doesn't have changes
        // nothing, and must not create an empty one that hides the parent's.
        if matches!(op, ListOp::Remove(_)) && self.list(owner, name).is_none() {
            return Ok(false);
        }
        let mut default = default_list(l);
        if let Some(items) = self.identity_list_defaults.get(l.name) {
            default.items = (0..).zip(items.iter().cloned()).collect();
        }
        let at = self.lists.entry(owner).or_default();
        if op == ListOp::Reset {
            let changed = if owner == Owner::Global {
                at.insert(name.to_string(), default.clone())
                    .map(|o| o.items)
                    != Some(default.items)
            } else {
                at.remove(name).is_some()
            };
            if at.is_empty() {
                self.lists.remove(&owner);
            }
            return Ok(changed);
        }
        let entry = at.entry(name.to_string()).or_default();
        // An unset keeps the list's `--persist`; a set stores it.
        if !matches!(op, ListOp::Remove(_)) {
            entry.persist = persist;
        }
        let before = entry.items.clone();
        let items = &mut entry.items;
        match op {
            ListOp::Set {
                index,
                value,
                append,
            } => {
                let item = items.entry(index).or_default();
                if append {
                    item.push_str(&value);
                } else {
                    *item = value;
                }
            }
            ListOp::Push(value) => {
                let free = (0..).find(|i| !items.contains_key(i)).unwrap_or(u32::MAX);
                items.insert(free, value);
            }
            ListOp::Replace(value) => *items = BTreeMap::from([(0, value)]),
            ListOp::Items(new) => *items = new,
            ListOp::Remove(index) => {
                items.remove(&index);
            }
            ListOp::Reset => {}
        }
        Ok(*items != before)
    }
}

/// `name` split into its base and `[N]` index, as tmux 3.7b's
/// `options_parse` reads it.
pub fn split_index(name: &str) -> Result<(&str, Option<u32>), String> {
    let Some(open) = name.find('[') else {
        return Ok((name, None));
    };
    let bad = || format!("ambiguous option: {name}");
    let inner = name[open + 1..]
        .strip_suffix(']')
        .filter(|s| !s.contains(']') && s.ends_with(|c: char| c.is_ascii_digit()))
        .ok_or_else(bad)?;
    let index = c_unsigned(inner).filter(|i| *i <= i32::MAX as u32);
    match index {
        Some(i) if open > 0 => Ok((&name[..open], Some(i))),
        _ => Err(bad()),
    }
}

/// C's `sscanf("%u")`: leading blanks, an optional sign, then digits up
/// to the first non-digit, wrapping modulo 2^32 (so `-1` is `u32::MAX`).
fn c_unsigned(s: &str) -> Option<u32> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (negative, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let n = s[..end].bytes().fold(0u32, |n, d| {
        n.wrapping_mul(10).wrapping_add(u32::from(d - b'0'))
    });
    Some(if negative { n.wrapping_neg() } else { n })
}

/// The error for an index on a name that is not a list: `set` says
/// `not an array` for a known scalar, else the name is unknown.
fn not_a_list(base: &str, full: &str) -> String {
    if base.starts_with('@') || builtin(base).is_some() {
        format!("not an array: {full}")
    } else {
        format!("invalid option: {full}")
    }
}

/// The element texts an edit sets.
fn op_values(op: &ListOp) -> Vec<&str> {
    match op {
        ListOp::Set { value, .. } | ListOp::Push(value) | ListOp::Replace(value) => {
            vec![value.as_str()]
        }
        ListOp::Items(items) => items.values().map(String::as_str).collect(),
        ListOp::Remove(_) | ListOp::Reset => Vec::new(),
    }
}

/// `set-option` on a list option, or on any name given an index.
fn set_list(
    model: &mut Model,
    events: &mut Events,
    ctx: &ClientCtx,
    c: &SetOption,
    base: &str,
    index: Option<u32>,
) -> Result<String, String> {
    if list_builtin(base).is_none() {
        return Err(not_a_list(base, &c.name));
    }
    let scope = default_scope(c.scope, Some(base));
    let owner = match owner(model, scope, &c.target, ctx) {
        Ok(owner) => owner,
        Err(_) if c.quiet => return Ok(String::new()),
        Err(e) => return Err(e),
    };
    if c.only_if_unset && !c.unset {
        let list = model.options.list(owner, base);
        let already = match index {
            Some(i) => list.is_some_and(|l| l.items.contains_key(&i)),
            None => list.is_some(),
        };
        if already {
            if c.quiet {
                return Ok(String::new());
            }
            return Err(format!("already set: {}", c.name));
        }
    }
    let op = match (c.unset, index, c.value.clone()) {
        (true, Some(i), _) => ListOp::Remove(i),
        (true, None, _) => ListOp::Reset,
        (false, _, None) => return Err("empty value".to_string()),
        (false, Some(index), Some(value)) => ListOp::Set {
            index,
            value,
            append: c.append,
        },
        (false, None, Some(value)) if c.append => ListOp::Push(value),
        (false, None, Some(value)) => ListOp::Replace(value),
    };
    // As tmux 3.7b (recorded, FE.1): a list with a separator takes a value
    // set or appended without an index as one element per piece, empty
    // pieces skipped (`set -as terminal-features ',x:RGB'` adds `x:RGB`).
    let ops = by_separator(op, base);
    if base == "pane-colours" {
        for value in ops.iter().flat_map(op_values) {
            colour_text(value).ok_or_else(|| format!("bad colour: {value}"))?;
        }
    }
    let element = index.map(|i| format!("{base}[{i}]"));
    let before = element.as_ref().and_then(|full| {
        let old = model.options.list(owner, base)?.items.get(&index?)?;
        Some((old.clone(), tmux_text(&model.options, owner, full, old)))
    });
    let mut changed = false;
    for op in ops {
        changed |= model.options.edit_list(owner, base, op, c.persist)?;
    }
    // In the tmux dialect, an element's text as sent, for `show` (FE.3).
    if let (Some(full), Some(i)) = (&element, index) {
        let stored = model
            .options
            .list(owner, base)
            .and_then(|l| l.items.get(&i).cloned())
            .unwrap_or_default();
        // Kept only for a rewritten value, or an append to text `show`
        // already prints in place of the value, as for a scalar.
        let piece = c.shown.clone().or(c.value.clone()).unwrap_or_default();
        let sent = match (&before, c.append) {
            (Some((raw, text)), true) if raw != text || c.shown.is_some() => {
                Some(format!("{text}{piece}"))
            }
            _ => c.shown.clone(),
        }
        .filter(|_| ctx.dialect == Dialect::Tmux && !c.unset);
        model.options.set_shown(owner, full, &stored, sent);
    }
    if owner == Owner::Global {
        model.options.mark_runtime(base);
    }
    if changed {
        let (name, value) = match index {
            Some(i) => (
                format!("{base}[{i}]"),
                model
                    .options
                    .list(owner, base)
                    .and_then(|l| l.items.get(&i).cloned()),
            ),
            None => (base.to_string(), None),
        };
        events.emit(Event::OptionChanged {
            scope: scope_name(owner),
            name,
            value,
        });
    }
    Ok(String::new())
}

/// A config list's elements, element N at index N.
fn list_items(items: &[String]) -> BTreeMap<u32, String> {
    (0..).zip(items.iter().cloned()).collect()
}

/// Sets a whole global list, emitting `option-changed` (value-less, as
/// for any whole-list change) on a change.
fn set_global_list(
    model: &mut Model,
    events: &mut Events,
    name: &str,
    items: BTreeMap<u32, String>,
) {
    match model
        .options
        .edit_list(Owner::Global, name, ListOp::Items(items), false)
    {
        Ok(true) => events.emit(Event::OptionChanged {
            scope: scope_name(Owner::Global),
            name: name.to_string(),
            value: None,
        }),
        Ok(false) => {}
        Err(e) => eprintln!("grofe config: {name}: {e}"),
    }
}

/// `apply_config` for lists (R10): a list the config no longer sets goes
/// back to what it was before the config set it; one set at runtime is
/// left alone.
fn apply_config_lists(model: &mut Model, events: &mut Events, resolved: &Resolved) {
    let configured: Vec<(&str, &[String])> = resolved
        .configured_lists()
        .map(|(n, v, _)| (n, v))
        .collect();
    let dropped: Vec<(String, BTreeMap<u32, String>)> = model
        .options
        .config_list_base
        .iter()
        .filter(|(name, _)| !configured.iter().any(|(n, _)| n == name))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    for (name, base) in dropped {
        model.options.config_list_base.remove(&name);
        set_global_list(model, events, &name, base);
    }
    for (name, items) in configured {
        if model.options.runtime.contains(name) {
            continue;
        }
        if !model.options.config_list_base.contains_key(name) {
            let base = model
                .options
                .list(Owner::Global, name)
                .map(|l| l.items.clone())
                .unwrap_or_default();
            model
                .options
                .config_list_base
                .insert(name.to_string(), base);
        }
        set_global_list(model, events, name, list_items(items));
    }
}

fn scope_name(owner: Owner) -> String {
    match owner {
        Owner::Global | Owner::Server | Owner::GlobalWindow => "global".to_string(),
        Owner::Session(id) => id.to_string(),
        Owner::Window(id) => id.to_string(),
        Owner::Pane(id) => id.to_string(),
    }
}

/// A list as `show-options` prints it: one line per element, or the bare
/// name for an empty list; with `value_only`, values only and nothing for
/// an empty list.
fn list_lines(name: &str, l: &ListEntry, value_only: bool) -> String {
    if l.items.is_empty() {
        return if value_only {
            String::new()
        } else {
            format!("{name}\n")
        };
    }
    l.items
        .iter()
        .map(|(i, v)| {
            if value_only {
                format!("{v}\n")
            } else {
                format!("{name}[{i}] {}\n", quote(v))
            }
        })
        .collect()
}

/// `show-options` of a list option, or of any name given an index.
// ponytail: `-v` here covers lists and indexed names only; a plain
// scalar's `-v` and every `-q` are TC.4's.
fn show_list(
    model: &Model,
    ctx: &ClientCtx,
    c: &ShowOptions,
    full: &str,
    base: &str,
    index: Option<u32>,
) -> Result<String, String> {
    let scope = default_scope(c.scope, Some(base));
    let owner = owner(model, scope, &c.target, ctx)?;
    let tmux = ctx.dialect == Dialect::Tmux;
    let line = |value: &str| {
        let value = if tmux {
            tmux_text(&model.options, owner, full, value)
        } else {
            value.to_string()
        };
        let value = value.as_str();
        if c.value_only {
            format!("{value}\n")
        } else {
            format!("{full} {}\n", quote(value))
        }
    };
    if list_builtin(base).is_none() {
        // As tmux: a scalar at an index shows its value under that name.
        // `-q` makes an unknown or unset one an empty answer.
        let invalid = || {
            if c.quiet {
                Ok(String::new())
            } else {
                Err(format!("invalid option: {full}"))
            }
        };
        if check_name(base).is_err() {
            return invalid();
        }
        return match model.options.get(owner, base) {
            Some(e) => Ok(line(&e.value)),
            None if base.starts_with('@') => invalid(),
            None => Ok(String::new()),
        };
    }
    let Some(l) = model.options.list(owner, base) else {
        return Ok(String::new());
    };
    Ok(match index {
        // tmux's default for an element grofe's list lacks (FE.3).
        Some(i) if tmux && owner == Owner::Global && !l.items.contains_key(&i) => {
            match spelled_default(&model.options, full, None) {
                Some(text) => line_raw(full, &text, c.value_only),
                None => line(""),
            }
        }
        Some(i) => line(l.items.get(&i).map_or("", String::as_str)),
        None if tmux && owner == Owner::Global => tmux_list_lines(model, base, None, c.value_only),
        None => list_lines(base, l, c.value_only),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::protocol::Dialect;
    use crate::server::model::{BASE_INDEX, HISTORY_LIMIT, PANE_BASE_INDEX};

    /// `source-file` globs only the path it is given: the requester's cwd
    /// is quoted first, as tmux's `cmd_source_file_quote_for_glob`, so a
    /// cwd named `[ab]` reads its own file, not `a/` or `b/`'s.
    #[test]
    fn source_file_takes_the_cwd_literally() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("[ab]*");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(cwd.join("f.conf"), "set -g @where cwd\n").unwrap();
        std::fs::write(dir.path().join("a/f.conf"), "set -g @where glob\n").unwrap();
        let mut state = crate::server::test_state();
        let ctx = ClientCtx {
            cwd,
            dialect: Dialect::Tmux,
            ..crate::server::test_ctx()
        };
        let mut run = |words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(&mut state, &ctx, &argv)
        };
        assert_eq!(run(&["source-file", "f.conf"]), Ok(String::new()));
        assert_eq!(run(&["show-options", "-gv", "@where"]), Ok("cwd\n".into()));
        assert_eq!(
            run(&["source-file", "f*.conf"]).map(|_| ()),
            Ok(()),
            "the given path is still a glob"
        );
    }

    /// TM.7: mouse keys sort as tmux 3.7b's key codes do (recorded in
    /// `tc_0b/display-formats` and `tm_0/list-keys-copy-mode-vi`).
    #[test]
    fn mouse_keys_sort_in_tmux_s_order() {
        use crate::server::input::parse_key;
        let recorded = [
            "MouseDown1Pane",
            "MouseDown1Status",
            "MouseDown1Border",
            "MouseDown1ScrollbarUp",
            "MouseDown1Control8",
            "MouseDown3Pane",
            "MouseDown3StatusLeft",
            "MouseDrag1Pane",
            "MouseDrag1Border",
            "MouseDragEnd1Pane",
            "WheelDownPane",
            "WheelDownStatus",
            "WheelUpPane",
            "DoubleClick1Pane",
            "TripleClick1Pane",
            "M-x",
            "M-MouseDown3Pane",
            "C-a",
        ];
        let mut keys: Vec<Key> = recorded.iter().map(|k| parse_key(k).unwrap()).collect();
        keys.reverse();
        keys.sort_by_key(tmux_key_order);
        let got: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        assert_eq!(got, recorded);
    }

    /// TC.7: `list-keys` for one key, `-1` for the first line, `-N` for
    /// notes in tmux 3.7b's form (recorded: `C-b c       Create a new
    /// window`, keys padded to the widest; without `-T` the prefix table,
    /// then the root table, every line after the prefix key).
    #[test]
    fn list_keys_one_key_first_and_notes() {
        let mut state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        let mut run = |words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(&mut state, &ctx, &argv)
        };
        // The stock keys carry tmux's notes too (TC.10); start without them.
        run(&["unbind-key", "-a", "-T", "prefix"]).expect("unbind");
        run(&["bind-key", "-N", "Kill the active pane", "x", "kill-pane"]).expect("bind");
        run(&["bind-key", "-N", "Next layout", "Space", "next-layout"]).expect("bind");
        run(&["bind-key", "y", "list-sessions"]).expect("bind");
        run(&["bind-key", "-n", "-N", "Root note", "F6", "list-sessions"]).expect("bind");

        let got = run(&["list-keys", "-T", "prefix", "x"]).expect("list-keys x");
        assert_eq!(got.lines().count(), 1, "{got}");
        assert!(got.starts_with("bind-key -T prefix x kill-pane"), "{got}");
        assert_eq!(
            run(&["list-keys", "-T", "prefix", "F12"]),
            Ok(String::new())
        );
        let first = run(&["list-keys", "-1", "-T", "prefix"]).expect("-1");
        assert_eq!(first.lines().count(), 1, "{first}");

        assert_eq!(
            run(&["list-keys", "-N"]),
            Ok(
                "C-b Space Next layout\nC-b x     Kill the active pane\nC-b F6    Root note\n"
                    .to_string()
            )
        );
        assert_eq!(
            run(&["list-keys", "-N", "-T", "root"]),
            Ok("F6 Root note\n".to_string())
        );
        assert_eq!(
            run(&["list-keys", "-N", "x"]),
            Ok("C-b x Kill the active pane\n".to_string())
        );
        // No attached client: `-1N` prints its line.
        assert_eq!(
            run(&["list-keys", "-1N", "x"]),
            Ok("C-b x Kill the active pane\n".to_string())
        );
        assert_eq!(run(&["list-keys", "-1N", "y"]), Ok(String::new()));
        assert_eq!(
            run(&["list-keys", "-1N", "NotAKey"]),
            Err("invalid key: NotAKey".to_string())
        );
    }

    /// `set -F` expands with the server's variables, as `display-message`
    /// does: a mouse key's command sees the mouse event (TC.10).
    #[tokio::test]
    async fn set_f_sees_the_mouse_variables() {
        let mut state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        let run = |state: &mut State, words: &[&str]| {
            let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
            crate::server::dispatch::run(state, &ctx, &argv)
        };
        run(&mut state, &["new-session", "-d", "-s", "a"]).expect("new-session");
        let pane = *state.model.panes.keys().next().expect("a pane");
        let target = crate::server::mouse::MouseTarget {
            client: crate::types::ClientId(1),
            cell: (3, 4),
            pane,
            target: pane.to_string(),
            range: None,
            pane_cell: Some((3, 4)),
            report: (3, 4),
            wheel: false,
            action: None,
            event: crate::server::input::Mouse {
                kind: crate::server::input::MouseKind::Move,
                col: 3,
                row: 4,
                mods: Default::default(),
            },
        };
        crate::server::mouse::MouseState::with_current(&mut state, Some(target), |state| {
            run(state, &["set", "-gF", "@m", "#{mouse_pane} #{mouse_x}"])
        })
        .expect("set -F");
        assert_eq!(
            run(&mut state, &["show-options", "-gv", "@m"]),
            Ok(format!("{pane} 3\n"))
        );
    }

    /// FE.1 ruling: under tmux identity each built-in default that differs
    /// is tmux 3.7b's (recorded); under grofe identity grofe's stays.
    #[test]
    fn tmux_identity_takes_tmux_s_defaults() {
        let recorded = [
            ("base-index", "0", "1"),
            ("pane-base-index", "0", "1"),
            ("mouse", "off", "on"),
            ("set-clipboard", "external", "external"),
            ("focus-events", "off", "on"),
            ("escape-time", "10", "25"),
            ("history-limit", "2000", "50000"),
            ("extended-keys", "off", "on"),
            ("renumber-windows", "off", "on"),
            ("mode-keys", "emacs", "vi"),
            ("status-keys", "emacs", "emacs"),
        ];
        let grofe = Options::new(false);
        let mut tmux = Options::new(false);
        tmux.use_tmux_defaults(None);
        for (name, tmux_default, grofe_default) in recorded {
            let get = |o: &Options| o.lookup(name, &[]).map(str::to_string);
            assert_eq!(get(&grofe).as_deref(), Some(grofe_default), "grofe {name}");
            assert_eq!(get(&tmux).as_deref(), Some(tmux_default), "tmux {name}");
        }
        // `set -gu` restores the identity's default.
        tmux.set(Owner::Global, "base-index", "5", false).unwrap();
        tmux.unset(Owner::Global, "base-index").unwrap();
        assert_eq!(tmux.lookup("base-index", &[]), Some("0"));
        // tmux's `$EDITOR` rule for the key tables.
        let mut vi = Options::new(false);
        vi.use_tmux_defaults(Some("/usr/local/bin/nvim"));
        assert_eq!(vi.lookup("status-keys", &[]), Some("vi"));
        assert_eq!(vi.lookup("mode-keys", &[]), Some("vi"));
        // A value the config set keeps winning, with tmux's as its base.
        let mut configured = Options::new(false);
        configured
            .config_base
            .insert("mouse".to_string(), "on".to_string());
        configured.set(Owner::Global, "mouse", "on", false).unwrap();
        configured.use_tmux_defaults(None);
        assert_eq!(configured.lookup("mouse", &[]), Some("on"));
        assert_eq!(configured.config_base["mouse"], "off");
    }

    /// The stub edit: `bind-key` checks the command in the request's
    /// dialect and keeps that dialect on the binding.
    #[test]
    fn bind_key_keeps_the_requests_dialect() {
        let mut state = crate::server::test_state();
        let bind = |key: &str| BindKey {
            table: "root".to_string(),
            repeat: false,
            note: None,
            key: key.to_string(),
            command: vec!["list-sessions".to_string()],
        };
        let grofe_ctx = crate::server::test_ctx;
        let grofe = grofe_ctx();
        bind_key(&mut state, &grofe, bind("F5")).expect("native bind");
        let f5 = parse_key("F5").expect("key");
        let b = state.registry.key_tables.exact("root", &f5).expect("bound");
        assert_eq!(b.dialect, Dialect::Grofe);
        let tmux = ClientCtx {
            dialect: Dialect::Tmux,
            ..grofe
        };
        // FE.1: a tmux-only form binds in the tmux dialect and keeps it.
        let ls = |key: &str| BindKey {
            command: vec!["ls".to_string()],
            ..bind(key)
        };
        bind_key(&mut state, &tmux, ls("F6")).expect("tmux bind");
        let f6 = parse_key("F6").expect("key");
        let b = state.registry.key_tables.exact("root", &f6).expect("bound");
        assert_eq!(b.dialect, Dialect::Tmux);
        let err = bind_key(&mut state, &grofe_ctx(), ls("F7")).unwrap_err();
        assert_eq!(err, "unknown command: ls");
    }

    /// TP.12: `#{start_time}` is when the server started, in Unix seconds,
    /// without a pane: continuum reads it to tell a fresh server (restore
    /// once) from one that has been running.
    #[test]
    fn start_time_is_the_server_start() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let model = Model::new(false);
        let got = FormatCtx::new(&model, PaneId(99)).get("start_time");
        let secs: u64 = got.expect("start_time set").parse().expect("a number");
        assert!(secs >= before && secs <= before + 5, "{secs} vs {before}");
    }

    /// TC.3: `#{version}` follows the model's identity (TC.1 sets it);
    /// `#{grofe_version}` does not. Neither needs a pane.
    #[test]
    fn version_follows_the_identity() {
        let mut model = Model::new(false);
        let grofe = env!("CARGO_PKG_VERSION");
        let get = |m: &Model, name: &str| FormatCtx::new(m, PaneId(99)).get(name);
        assert_eq!(get(&model, "version").as_deref(), Some(grofe));
        model.identity = crate::compat::Identity::Tmux;
        assert_eq!(get(&model, "version").as_deref(), Some("3.7b"));
        assert_eq!(get(&model, "grofe_version").as_deref(), Some(grofe));
    }

    #[test]
    fn same_value_set_is_a_no_op() {
        let mut o = Options::new(false);
        let s = Owner::Session(SessionId(0));
        assert_eq!(o.set(s, "@x", "a", false), Ok(true));
        assert_eq!(o.set(s, "@x", "a", false), Ok(false));
        assert_eq!(o.set(s, "@x", "b", false), Ok(true));
        // A flag option normalizes before comparing: `yes` is `on`.
        assert_eq!(o.set(Owner::Global, "allow-rename", "on", false), Ok(true));
        assert_eq!(
            o.set(Owner::Global, "allow-rename", "yes", false),
            Ok(false)
        );
    }

    #[test]
    fn persist_flag_is_stored() {
        let mut o = Options::new(false);
        let p = Owner::Pane(PaneId(3));
        o.set(p, "@state", "busy", true).unwrap();
        assert!(o.get(p, "@state").unwrap().persist);
        o.set(p, "@other", "x", false).unwrap();
        assert!(!o.get(p, "@other").unwrap().persist);
    }

    #[test]
    fn lookup_walks_pane_window_session_then_global() {
        let mut o = Options::new(false);
        let chain = [
            Owner::Pane(PaneId(0)),
            Owner::Window(WindowId(0)),
            Owner::Session(SessionId(0)),
        ];
        assert_eq!(o.lookup("@x", &chain), None);
        for (owner, v) in [
            (Owner::Global, "g"),
            (chain[2], "s"),
            (chain[1], "w"),
            (chain[0], "p"),
        ] {
            o.set(owner, "@x", v, false).unwrap();
            assert_eq!(o.lookup("@x", &chain), Some(v));
        }
        for (owner, v) in [(chain[0], "w"), (chain[1], "s"), (chain[2], "g")] {
            assert_eq!(o.unset(owner, "@x"), Ok(true));
            assert_eq!(o.lookup("@x", &chain), Some(v));
        }
    }

    #[test]
    fn builtins_validate_and_reset_to_default() {
        let mut o = Options::new(true);
        assert_eq!(o.lookup(REMAIN_ON_EXIT, &[]), Some("on"));
        assert_eq!(
            o.set(Owner::Global, "no-such", "1", false),
            Err("invalid option: no-such".to_string())
        );
        assert!(
            o.set(Owner::Global, REMAIN_ON_EXIT, "maybe", false)
                .is_err()
        );
        assert!(o.set(Owner::Global, "history-limit", "-3", false).is_err());
        o.set(Owner::Global, "history-limit", "10", false).unwrap();
        o.unset(Owner::Global, "history-limit").unwrap();
        assert_eq!(o.lookup("history-limit", &[]), Some("50000"));
    }

    #[test]
    fn builtin_defaults_match_the_model() {
        let o = Options::new(false);
        assert_eq!(
            o.lookup("history-limit", &[]),
            Some(HISTORY_LIMIT.to_string().as_str())
        );
        assert_eq!(
            o.lookup("base-index", &[]),
            Some(BASE_INDEX.to_string().as_str())
        );
        assert_eq!(
            o.lookup("pane-base-index", &[]),
            Some(PANE_BASE_INDEX.to_string().as_str())
        );
    }

    #[test]
    fn retain_live_drops_values_of_removed_objects() {
        let mut o = Options::new(false);
        o.set(Owner::Pane(PaneId(7)), "@x", "1", false).unwrap();
        o.retain_live(&Model::default());
        let owners: BTreeSet<Owner> = o.values.keys().copied().collect();
        assert_eq!(owners, BTreeSet::from([Owner::Global]));
    }

    /// The rows T3.0 adds (R4): tmux's names and scopes, spec §5.3's
    /// defaults (`set-clipboard` `external` as tmux's, an M5 ruling), and
    /// tmux's value checks.
    #[test]
    fn m3_rows_have_grofe_defaults_and_tmux_scopes() {
        let o = Options::new(false);
        for (name, default, scope) in [
            ("mouse", "on", OptionScope::Session),
            ("set-clipboard", "external", OptionScope::Global),
            ("focus-events", "on", OptionScope::Global),
            ("pane-border-style", "default", OptionScope::Window),
            (
                "pane-active-border-style",
                "#{?pane_in_mode,fg=yellow,#{?synchronize-panes,fg=red,fg=green}}",
                OptionScope::Window,
            ),
            ("status-position", "bottom", OptionScope::Session),
            ("allow-passthrough", "off", OptionScope::Window),
        ] {
            assert_eq!(o.lookup(name, &[]), Some(default), "{name}");
            assert_eq!(builtin(name).map(|b| b.scope), Some(scope), "{name}");
            assert!(builtin_names().any(|n| n == name), "{name}");
        }
        assert_eq!(validate("set-clipboard", "external"), Ok("external".into()));
        assert_eq!(
            validate("set-clipboard", "maybe"),
            Err("unknown value: maybe".to_string())
        );
        assert_eq!(validate("mouse", "yes"), Ok("on".into()));
        assert_eq!(validate("status-position", "top"), Ok("top".into()));
        assert_eq!(
            validate("status-position", "left"),
            Err("unknown value: left".to_string())
        );
        assert_eq!(builtin_names().count(), BUILTINS.len());
    }

    /// `set-titles`/`set-titles-string` (R15): tmux's own names, scope and
    /// `set-titles` default; `set-titles-string`'s default is tmux's own
    /// translated into `#{...}` form.
    #[test]
    fn set_titles_rows_are_session_scoped_and_off_by_default() {
        let o = Options::new(false);
        assert_eq!(o.lookup("set-titles", &[]), Some("off"));
        assert_eq!(
            builtin("set-titles").map(|b| b.scope),
            Some(OptionScope::Session)
        );
        assert_eq!(
            o.lookup("set-titles-string", &[]),
            Some(
                "#{session_name}:#{window_index}:#{window_name} - \"#{pane_title}\" #{session_alerts}"
            )
        );
        assert_eq!(
            builtin("set-titles-string").map(|b| b.scope),
            Some(OptionScope::Session)
        );
        assert_eq!(validate("set-titles", "yes"), Ok("on".into()));
        assert_eq!(
            validate("set-titles", "maybe"),
            Err("bad value: maybe".to_string())
        );
    }

    /// TC.2b: tmux 3.7b's index grammar (recorded on a private socket).
    #[test]
    fn list_indices_read_as_tmux_reads_them() {
        for (name, want) in [
            ("status-format", Some(("status-format", None))),
            ("status-format[1]", Some(("status-format", Some(1)))),
            ("status-format[+3]", Some(("status-format", Some(3)))),
            ("status-format[ 5]", Some(("status-format", Some(5)))),
            ("status-format[0x6]", Some(("status-format", Some(0)))),
            (
                "status-format[4294967296]",
                Some(("status-format", Some(0))),
            ),
            (
                "status-format[99999999999]",
                Some(("status-format", Some(1_215_752_191))),
            ),
            (
                "status-format[2147483647]",
                Some(("status-format", Some(i32::MAX as u32))),
            ),
            ("status-format[2147483648]", None),
            ("status-format[-1]", None),
            ("status-format[1a]", None),
            ("status-format[a1]", None),
            ("status-format[1]]", None),
            ("status-format[[1]", None),
            ("status-format[1][2]", None),
            ("[1]", None),
        ] {
            let got = split_index(name);
            match want {
                Some(w) => assert_eq!(got, Ok(w), "{name}"),
                None => assert_eq!(got, Err(format!("ambiguous option: {name}")), "{name}"),
            }
        }
    }

    /// A session's list hides the global one as a whole, as in tmux.
    #[test]
    fn a_list_lookup_takes_the_first_owner_with_a_list() {
        let mut o = Options::new(false);
        let s = Owner::Session(SessionId(0));
        let set = |index, value: &str| ListOp::Set {
            index,
            value: value.to_string(),
            append: false,
        };
        o.edit_list(Owner::Global, "status-format", set(1, "g"), false)
            .unwrap();
        let global = o.lookup_list("status-format", &[s]).cloned();
        let rows = global.clone().expect("a list");
        assert_eq!(rows[&0], "");
        assert_eq!(rows[&1], "g");
        assert_eq!(o.edit_list(s, "status-format", set(1, "s"), true), Ok(true));
        assert_eq!(
            o.edit_list(s, "status-format", set(1, "s"), true),
            Ok(false)
        );
        assert_eq!(
            o.lookup_list("status-format", &[s]),
            Some(&BTreeMap::from([(1, "s".to_string())]))
        );
        assert!(o.list(s, "status-format").unwrap().persist);
        assert_eq!(
            o.edit_list(s, "status-format", ListOp::Reset, false),
            Ok(true)
        );
        assert_eq!(o.lookup_list("status-format", &[s]).cloned(), global);
        assert_eq!(
            o.edit_list(Owner::Global, "status-format", ListOp::Reset, false),
            Ok(true)
        );
        assert_eq!(
            o.edit_list(Owner::Global, "nosuch", ListOp::Reset, false),
            Err("invalid option: nosuch".to_string())
        );
        assert!(list_names().all(|n| builtin(n).is_none()));
    }

    #[test]
    fn show_quotes_values_with_spaces() {
        assert_eq!(quote("a"), "a");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(""), "''");
    }

    /// Each case recorded from tmux 3.7b (`set -g @t <value>; show -g @t`)
    /// on a private socket.
    /// Recorded on tmux 3.7b (`set -g prefix X; show -gv prefix`).
    #[test]
    fn prefix_is_a_key_name_stored_as_tmux_spells_it() {
        for (given, stored) in [
            ("C-a", "C-a"),
            ("c-B", "C-B"),
            ("M-C-x", "C-M-x"),
            ("^a", "C-a"),
            ("pgup", "PPage"),
            ("none", "None"),
            ("C-S-a", "C-S-a"),
            ("S-Tab", "S-Tab"),
            ("BTab", "BTab"),
        ] {
            assert_eq!(validate("prefix", given), Ok(stored.to_string()), "{given}");
        }
        assert_eq!(validate("prefix", "foo"), Err("bad key: foo".to_string()));
    }

    #[test]
    fn show_quotes_like_tmux() {
        for (value, shown) in [
            ("a#b", "\"a#b\""),
            ("a{b", "\"a{b\""),
            ("#{x}", "\"#{x}\""),
            ("a}b", "\"a}b\""),
            ("ab", "ab"),
            ("a\"b", "'a\"b'"),
            ("a'b", "\"a'b\""),
            ("a$b", "\"a\\$b\""),
            ("a;b", "\"a;b\""),
            ("a\\b", "a\\\\b"),
            ("~a", "\\~a"),
            ("a~", "a~"),
            ("#", "\\#"),
            ("~", "\\~"),
            (" ", "\" \""),
            ("~a b", "\"\\~a b\""),
            ("a \"b", "\"a \\\"b\""),
            ("a `b", "\"a `b\""),
            ("a\tb", "a\\tb"),
            ("a\nb", "a\\nb"),
            ("a\u{1}b", "a\\001b"),
            ("a \\b", "\"a \\\\b\""),
            ("a,b", "a,b"),
            ("[a]", "[a]"),
            ("a%b", "\"a%b\""),
        ] {
            assert_eq!(quote(value), shown, "{value:?}");
        }
    }
}
