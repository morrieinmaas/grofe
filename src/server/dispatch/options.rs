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

use std::collections::{BTreeMap, BTreeSet};
use std::num::IntErrorKind;
use std::path::Path;

use crate::cmd::{
    BindKey, Command, DisplayMessage, FormatVars, ListKeys, ListPanes, ListSessions, ListWindows,
    OptionScope, Reload, Resolve, SetOption, ShowOptions, SourceFile, Target, UnbindKey, expand,
    split_command_string,
};
use crate::config::{self, Resolved, provenance::tilde};
use crate::event::Event;
use crate::protocol::ClientCtx;
use crate::server::events::Events;
use crate::server::input::{Binding, BindingSource, Key, KeyTables, parse_key};
use crate::server::model::Model;
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
// where grofe differs: `mouse` on (tmux off), `set-clipboard` on (tmux
// external), `focus-events` on (tmux off), `pane-active-border-style`
// `fg=green` (tmux's is a format that also colours copy mode and
// synchronized panes). `status-position` takes `top` from TC.11.
// `set-titles` (off) and `set-titles-string` match tmux's own defaults,
// the string translated into grofe's `#{...}`-only expander (R15).
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
        // A choice in tmux 3.7b (`off`, `on`, `failed`), so `yes` is refused
        // and a bad value is `unknown value`. ponytail: `failed` is not
        // supported until something needs it.
        kind: Kind::Choice(&["off", "on"]),
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
        default: "on",
    },
    Builtin {
        name: "focus-events",
        kind: Kind::Bool,
        scope: OptionScope::Global,
        default: "on",
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
        default: "fg=green",
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
];

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
}

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
            return self.set(owner, name, b.default, false);
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
            .chain([&Owner::Global])
            .find_map(|o| self.get(*o, name))
            .map(|e| e.value.as_str())
    }

    /// Drops values set on sessions, windows or panes that no longer exist.
    /// Runtime ids are never reused within a server, so stale values never
    /// apply to anything; this only frees them.
    pub fn retain_live(&mut self, model: &Model) {
        self.values.retain(|owner, _| match owner {
            Owner::Global => true,
            Owner::Session(id) => model.sessions.contains_key(id),
            Owner::Window(id) => model.windows.contains_key(id),
            Owner::Pane(id) => model.panes.contains_key(id),
        });
        self.lists.retain(|owner, _| match owner {
            Owner::Global => true,
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
        Kind::Bool => match value {
            "on" | "yes" | "true" | "1" => Ok("on".to_string()),
            "off" | "no" | "false" | "0" => Ok("off".to_string()),
            // tmux's wording for a flag option; choices say `unknown value`.
            _ => Err(format!("bad value: {value}")),
        },
        Kind::String => Ok(value.to_string()),
        Kind::Key => crate::server::input::parse_prefix(value)
            .map(|key| key.map_or_else(|| "None".to_string(), |key| key.to_string())),
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
            if n < 0 {
                Err(too_small())
            } else if n > max {
                Err(too_large())
            } else {
                Ok(n.to_string())
            }
        }
    }
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
        OptionScope::Session => Owner::Session(model.resolve_session(target, ctx)?),
        OptionScope::Window => Owner::Window(model.resolve_window(target, ctx)?),
        OptionScope::Pane => Owner::Pane(model.resolve_pane(target, ctx)?),
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
        Command::SetOption(c) => set_option(model, &mut state.events, ctx, c),
        Command::ShowOptions(c) => show_options(model, ctx, c),
        Command::DisplayMessage(c) => display_message(state, ctx, c),
        Command::ListSessions(c) => list_sessions(state, c),
        Command::ListWindows(c) => list_windows(state, ctx, c),
        Command::ListPanes(c) => list_panes(state, ctx, c),
        _ => Err("not an option command".to_string()),
    }
}

fn set_option(
    model: &mut Model,
    events: &mut Events,
    ctx: &ClientCtx,
    mut c: SetOption,
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
    if c.expand
        && let Some(value) = &c.value
    {
        // ponytail: `-F` expands against the target pane's `FormatCtx`,
        // without the server's and client's variables over it.
        let pane = model.resolve_pane(&c.target, ctx).ok();
        c.value = Some(match pane {
            Some(p) => expand(value, &FormatCtx::new(model, p)),
            None => expand(value, &NoVars),
        });
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
        let did = model.options.unset(owner, &c.name)?;
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
    // `-a` appends to a string's value at this scope (tmux: style options
    // join with a comma); any other kind takes the value as given.
    let value = match model.options.get(owner, &c.name) {
        Some(old) if c.append && is_text(&c.name) => {
            let separator = if c.name.ends_with("-style") { "," } else { "" };
            format!("{}{separator}{value}", old.value)
        }
        _ => value,
    };
    let did = model.options.set(owner, &c.name, &value, c.persist)?;
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
    events.emit(Event::OptionChanged {
        scope: match owner {
            Owner::Global => "global".to_string(),
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
        Owner::Global => Vec::new(),
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
    // `-v` (tmux 3.7b): each value alone and unquoted.
    let line = |name: &str, value: &str| {
        if c.value_only {
            format!("{value}\n")
        } else {
            format!("{name} {}\n", quote(value))
        }
    };
    let mut lines: Vec<(&str, String)> = model
        .options
        .at(owner)
        .filter(|(name, _)| c.name.as_ref().is_none_or(|n| n == *name))
        .map(|(name, e)| (name.as_str(), line(name, &e.value)))
        .collect();
    if c.name.is_none() {
        lines.extend(
            model
                .options
                .lists_at(owner)
                .map(|(name, l)| (name.as_str(), list_lines(name, l, c.value_only))),
        );
        lines.sort_by(|a, b| a.0.cmp(b.0));
    }
    Ok(lines.into_iter().map(|(_, l)| l).collect())
}

/// A value as `show-options` prints it, as tmux 3.7b's `args_escape` does:
/// `''` when empty; double quotes when it holds any of `` #';${}%`` (with
/// `"`, `$` and `\` escaped inside); else single quotes when it holds `"`;
/// a lone special character or `~` backslash-escaped; a leading `~`
/// escaped; control characters C-style or octal.
fn quote(value: &str) -> String {
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
    for c in value.chars() {
        match c {
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

/// Applies the config chain once at server start. On any error the
/// server keeps the built-in defaults and logs every error (R9).
pub fn apply_config_at_start(state: &mut State) {
    match config::load(&config_env()) {
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
    let resolved = config::load(&config_env()).map_err(|errors| join_errors(&errors))?;
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
    let configured: Vec<(&str, &str)> = resolved.configured().map(|(n, v, _)| (n, v)).collect();
    let dropped: Vec<(String, String)> = model
        .options
        .config_base
        .iter()
        .filter(|(name, _)| !configured.iter().any(|(n, _)| n == name))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    for (name, base) in dropped {
        model.options.config_base.remove(&name);
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
    crate::server::hooks::configure(state, resolved);
    crate::server::menu::configure(state, resolved);
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
    let key = match &c.key {
        Some(name) => Some(parse_key(name).map_err(|_| format!("unknown key: {name}"))?),
        None => None,
    };
    if !tables.exists(&c.table) {
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

/// `list-keys [-T table]` in grofe's native form, one line per binding:
/// `bind-key [-r] -T <table> <key> <command>` as tmux spells it, then
/// `  # <source>`, with `: <desc>` and `[<name>]` when set. The source is
/// `default`, `<file>:<line>` (`~` for `$HOME`), `runtime` or
/// `plugin <name>`, so a binding from outside the config table shows.
// ponytail: tmux's padded `list-keys` layout is FE.4's.
pub fn list_keys(state: &mut State, _ctx: &ClientCtx, c: ListKeys) -> Result<String, String> {
    let tables = &state.registry.key_tables;
    let names: Vec<&str> = match &c.table {
        Some(t) if !tables.exists(t) => return Err(format!("table {t} doesn't exist")),
        Some(t) => vec![t.as_str()],
        None => tables.table_names().collect(),
    };
    let home = config_env().home;
    let mut out = String::new();
    for table in names {
        for key in tables.keys(table) {
            let Some(b) = tables.exact(table, &key) else {
                continue;
            };
            out.push_str(&binding_line(table, &key, b, home.as_deref()));
            out.push('\n');
        }
    }
    Ok(out)
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

/// `source-file [-q] path`: loads one TOML file (and its includes) at
/// runtime (spec §5.1). A relative path is the requester's cwd's, as in
/// tmux; the file's own includes resolve against it. Its values and
/// bindings count as runtime (R10): `reload` keeps them. All errors or
/// nothing, as `reload`; `-q` hides only a missing file.
pub fn source_file(state: &mut State, ctx: &ClientCtx, c: SourceFile) -> Result<String, String> {
    let path = ctx.cwd.join(&c.path);
    if !path.exists() {
        if c.quiet {
            return Ok(String::new());
        }
        return Err(format!("No such file or directory: {}", path.display()));
    }
    // ponytail: Lua config is layer 5, T6.4's (M6).
    if path.extension().is_some_and(|e| e == "lua") {
        return crate::server::dispatch::not_implemented("source-file of a Lua file (M6)");
    }
    let resolved = config::load_one(&path, &config_env()).map_err(|e| join_errors(&e))?;
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

/// tmux's default `display-message` text, minus the clock (strftime is
/// M2).
const DEFAULT_MESSAGE: &str =
    "[#{session_name}] #{window_index}:#{window_name}, current pane #{pane_index}";

/// `display-message`: the text expanded against the target pane and the
/// current client (the one whose binding ran it, else the most recently
/// active one, as tmux's `cmd_find_best_client`), so `#{client_width}`
/// and the other `client_*` variables work.
// ponytail: printed with or without `-p`; showing it on the client's
// status line without `-p` is M3.6's (spec §4.5 listing row).
fn display_message(state: &State, ctx: &ClientCtx, c: DisplayMessage) -> Result<String, String> {
    let model = &state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let text = c.text.as_deref().unwrap_or(DEFAULT_MESSAGE);
    let vars = FormatCtx::new(model, pane);
    let out = match clients::current_client(state, ctx).and_then(|id| state.clients.get(&id)) {
        Some(client) => {
            let client = clients::ClientVars::new(model, client, Some(vars));
            expand(text, &ServerVars::new(state, pane, &client))
        }
        None => expand(text, &ServerVars::new(state, pane, &vars)),
    };
    Ok(format!("{out}\n"))
}

fn lines(state: &State, fmt: &str, panes: impl Iterator<Item = PaneId>) -> String {
    panes
        .map(|p| {
            let vars = FormatCtx::new(&state.model, p);
            format!("{}\n", expand(fmt, &ServerVars::new(state, p, &vars)))
        })
        .collect()
}

/// The pane a window's formats expand against: its active pane.
fn active_pane(model: &Model, window: WindowId) -> Option<PaneId> {
    model.windows.get(&window).map(|w| w.active)
}

fn list_sessions(state: &State, c: ListSessions) -> Result<String, String> {
    let model = &state.model;
    let fmt = c.format.unwrap_or_default();
    let panes = model
        .sessions
        .values()
        .filter_map(|s| active_pane(model, s.active));
    Ok(lines(state, &fmt, panes))
}

fn list_windows(state: &State, ctx: &ClientCtx, c: ListWindows) -> Result<String, String> {
    let model = &state.model;
    let session = model.resolve_session(&c.target, ctx)?;
    let fmt = c.format.unwrap_or_default();
    let panes = model.sessions[&session]
        .windows
        .iter()
        .filter_map(|w| active_pane(model, *w));
    Ok(lines(state, &fmt, panes))
}

fn list_panes(state: &State, ctx: &ClientCtx, c: ListPanes) -> Result<String, String> {
    let model = &state.model;
    let window = model.resolve_window(&c.target, ctx)?;
    let fmt = c.format.unwrap_or_default();
    Ok(lines(
        state,
        &fmt,
        model.windows[&window].pane_ids().into_iter(),
    ))
}

/// Format variables (spec §4.5) for one pane, its window and its session.
pub struct FormatCtx<'a> {
    model: &'a Model,
    pane: PaneId,
}

impl<'a> FormatCtx<'a> {
    pub fn new(model: &'a Model, pane: PaneId) -> FormatCtx<'a> {
        FormatCtx { model, pane }
    }
}

fn flag(b: bool) -> String {
    if b { "1" } else { "0" }.to_string()
}

/// Format variables that need the server state, not only the model
/// (`socket_path`, `pane_title`, `pane_tty`, and TC.11's `host` and
/// `host_short`), over `inner` for the rest.
/// Built where dispatch holds `&State`, the way `ClientVars` layers the
/// client's variables.
pub struct ServerVars<'a> {
    state: &'a State,
    pane: PaneId,
    inner: &'a dyn FormatVars,
}

impl<'a> ServerVars<'a> {
    pub fn new(state: &'a State, pane: PaneId, inner: &'a dyn FormatVars) -> ServerVars<'a> {
        ServerVars { state, pane, inner }
    }
}

impl FormatVars for ServerVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
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
            _ => self.inner.get(name),
        }
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
            return m
                .options
                .lookup(name, &chain(m, self.pane))
                .map(str::to_string);
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
            "pid" => return Some(std::process::id().to_string()),
            // Session groups are out of scope (spec §4.5).
            "session_grouped" => return Some("0".to_string()),
            // ponytail: tmux's layout string (with its checksum) is FE.2's
            // codec; there is no native one.
            "window_layout" => return Some(String::new()),
            // ponytail: stubs until the features exist: copy mode (M5),
            // floating panes (`new-pane`, TC.6), and the status range under
            // the mouse, which only a mouse-bound command sees (TC.10).
            // `session_group` is empty with no groups. `client_tty` needs
            // the tty in the attach hello (FE.2).
            "copy_cursor_word" | "pane_mode" | "mouse_status_range" | "session_group"
            | "client_tty" => return Some(String::new()),
            "pane_in_mode" | "pane_floating_flag" => return Some("0".to_string()),
            _ => {}
        }
        let p = m.panes.get(&self.pane)?;
        let w = m.windows.get(&p.window)?;
        let s = m.sessions.get(&w.session)?;
        Some(match name {
            "session_name" => s.name.clone(),
            "session_id" => s.id.to_string(),
            "window_index" => w.index.to_string(),
            "window_name" => w.name.clone(),
            "window_id" => w.id.to_string(),
            "window_active" => flag(s.active == w.id),
            "window_zoomed_flag" => flag(w.zoomed),
            "window_width" => w.cols.to_string(),
            "window_bell_flag" => flag(w.bell_flag),
            "window_height" => w.rows.to_string(),
            "window_panes" => w.layout.compute(w.area()).len().to_string(),
            "session_windows" => s.windows.len().to_string(),
            "session_created" => s
                .created
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
                .to_string(),
            "session_attached" => s.attached.to_string(),
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
            "pane_current_path" => p
                .handle
                .current_path()
                .unwrap_or_else(|| p.cwd.clone())
                .display()
                .to_string(),
            "pane_left" => pane_origin(w, p.id)?.0.to_string(),
            "pane_top" => pane_origin(w, p.id)?.1.to_string(),
            "pane_width" => p.cols.to_string(),
            "pane_height" => p.rows.to_string(),
            "pane_dead" => flag(p.dead.is_some()),
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
            "history_size" => p.handle.history_size().to_string(),
            "window_flags" => window_flags(s, w),
            // ponytail: `client_width`/`client_height` need clients (M2);
            // `host` and `host_short` are `ServerVars`' (TC.11).
            _ => return None,
        })
    }
}

/// tmux's `window_flags`: `!` bell, `*` current, `-` last, `Z` zoomed, in
/// tmux's order. grofe tracks no activity, silence or marked pane, so
/// `#`, `~` and `M` never show.
fn window_flags(s: &crate::server::model::Session, w: &crate::server::model::Window) -> String {
    [
        (w.bell_flag, '!'),
        (s.active == w.id, '*'),
        (s.last_windows.last() == Some(&w.id), '-'),
        (w.zoomed, 'Z'),
    ]
    .into_iter()
    .filter_map(|(on, c)| on.then_some(c))
    .collect()
}

/// One word of `pane_start_command`, quoted as tmux's `args_escape`
/// quotes it (checked against tmux 3.7b): double quotes around a word
/// with any of ` #';${}%`, else single quotes around one with a space or
/// `"`; a lone special character, or a leading `~`, gets a backslash;
/// control characters and backslashes are escaped C-style.
fn escape_arg(s: &str) -> String {
    const DQUOTED: &str = " #';${}%";
    const SQUOTED: &str = " \"";
    if s.is_empty() {
        return "''".to_string();
    }
    let quote = if s.contains(|c| DQUOTED.contains(c)) {
        Some('"')
    } else if s.contains(|c| SQUOTED.contains(c)) {
        Some('\'')
    } else {
        None
    };
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c != ' '
        && (quote.is_some() || c == '~')
    {
        return format!("\\{c}");
    }
    let bytes = s.as_bytes();
    let mut body = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        match c {
            '\\' => body.push_str("\\\\"),
            '\n' => body.push_str("\\n"),
            '\t' => body.push_str("\\t"),
            '\r' => body.push_str("\\r"),
            '\x07' => body.push_str("\\a"),
            '\x08' => body.push_str("\\b"),
            '\x0b' => body.push_str("\\v"),
            '\x0c' => body.push_str("\\f"),
            c if c.is_ascii_control() => body.push_str(&format!("\\{:03o}", c as u32)),
            '"' if quote == Some('"') => body.push_str("\\\""),
            // `$` only where a shell would start a variable after it.
            '$' if quote == Some('"')
                && bytes.get(i + 1).is_some_and(|&n| {
                    n == b'_' || n == b'{' || n.is_ascii_alphabetic() || n >= 0x80
                }) =>
            {
                body.push_str("\\$")
            }
            c => body.push(c),
        }
    }
    let tilde = if body.starts_with('~') { "\\" } else { "" };
    match quote {
        Some('\'') => format!("'{body}'"),
        Some(q) => format!("{q}{tilde}{body}{q}"),
        None => format!("{tilde}{body}"),
    }
}

/// A pane's top-left cell in its window: the layout's, or the window's
/// origin for a zoomed window's active pane.
fn pane_origin(w: &crate::server::model::Window, pane: PaneId) -> Option<(u16, u16)> {
    if w.zoomed && w.active == pane {
        return Some((0, 0));
    }
    w.layout
        .compute(w.area())
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
        default: &[""],
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
];

fn list_builtin(name: &str) -> Option<&'static ListBuiltin> {
    LISTS.iter().find(|l| l.name == name)
}

/// Every built-in list option's name, in table order.
pub fn list_names() -> impl Iterator<Item = &'static str> {
    LISTS.iter().map(|l| l.name)
}

/// Options grofe accepts and stores but never reads (spec §4.5, "Options
/// by tmux name"), by tmux name. `show-config --explain` marks them
/// `(no effect)`. FE.1 adds its rows (`terminal-features`,
/// `extended-keys-format`, `default-terminal`) with their `BUILTINS`
/// entries; the list stays here so no later task edits this contract.
const NO_EFFECT: &[&str] = &[];

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
        let at = self.lists.entry(owner).or_default();
        if op == ListOp::Reset {
            let changed = if owner == Owner::Global {
                let default = default_list(l);
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
    let changed = model.options.edit_list(owner, base, op, c.persist)?;
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
        Owner::Global => "global".to_string(),
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
    let line = |value: &str| {
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
        Some(i) => line(l.items.get(&i).map_or("", String::as_str)),
        None => list_lines(base, l, c.value_only),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::protocol::Dialect;
    use crate::server::model::{BASE_INDEX, HISTORY_LIMIT, PANE_BASE_INDEX};

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
        let grofe = crate::server::test_ctx();
        bind_key(&mut state, &grofe, bind("F5")).expect("native bind");
        let f5 = parse_key("F5").expect("key");
        let b = state.registry.key_tables.exact("root", &f5).expect("bound");
        assert_eq!(b.dialect, Dialect::Grofe);
        let tmux = ClientCtx {
            dialect: Dialect::Tmux,
            ..grofe
        };
        let err = bind_key(&mut state, &tmux, bind("F6")).unwrap_err();
        assert!(err.contains("tmux front end"), "{err}");
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
    /// defaults, and tmux's value checks.
    #[test]
    fn m3_rows_have_grofe_defaults_and_tmux_scopes() {
        let o = Options::new(false);
        for (name, default, scope) in [
            ("mouse", "on", OptionScope::Session),
            ("set-clipboard", "on", OptionScope::Global),
            ("focus-events", "on", OptionScope::Global),
            ("pane-border-style", "default", OptionScope::Window),
            ("pane-active-border-style", "fg=green", OptionScope::Window),
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
        assert_eq!(
            global,
            Some(BTreeMap::from([(0, String::new()), (1, "g".to_string())]))
        );
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
