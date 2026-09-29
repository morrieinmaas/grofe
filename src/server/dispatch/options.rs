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
// `#{b:pane_current_path}` (spec §4.5), `status-right` without the pane
// title, and the window formats without `#{window_flags}` (M3.6). tmux's
// escape-time is a server option; grofe's server scope is global.
//
// M3 rows (T3.0, R4) use tmux's names and scopes with spec §5.3's defaults
// where grofe differs: `mouse` on (tmux off), `set-clipboard` on (tmux
// external), `focus-events` on (tmux off), `pane-active-border-style`
// `fg=green` (tmux's is a format that also colours copy mode and
// synchronized panes). `status-position` takes only `bottom` until TC.11.
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
        default: "%H:%M %d-%b-%y",
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
        default: "#{window_index}:#{window_name} ",
    },
    Builtin {
        name: "window-status-current-format",
        kind: Kind::String,
        scope: OptionScope::Window,
        default: "#{window_index}:#{window_name}*",
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
        kind: Kind::Choice(&["bottom"]),
        scope: OptionScope::Session,
        default: "bottom",
    },
    // M3.6 (the stub edit): the server's identity override (spec D12).
    // TC.1 reads it once at start; until then nothing does.
    Builtin {
        name: "compat-identity",
        kind: Kind::Choice(&["auto", "tmux", "grofe"]),
        scope: OptionScope::Global,
        default: "auto",
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
            ..Options::default()
        }
    }

    /// Marks a global option as set at runtime, which the config then
    /// leaves alone.
    fn mark_runtime(&mut self, name: &str) {
        self.config_base.remove(name);
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
        name.and_then(builtin)
            .map(|b| b.scope)
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
        Command::ListSessions(c) => list_sessions(model, c),
        Command::ListWindows(c) => list_windows(model, ctx, c),
        Command::ListPanes(c) => list_panes(model, ctx, c),
        _ => Err("not an option command".to_string()),
    }
}

fn set_option(
    model: &mut Model,
    events: &mut Events,
    ctx: &ClientCtx,
    c: SetOption,
) -> Result<String, String> {
    let scope = default_scope(c.scope, Some(&c.name));
    let owner = owner(model, scope, &c.target, ctx)?;
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
    let did = model.options.set(owner, &c.name, &value, c.persist)?;
    changed(model, events, did);
    Ok(String::new())
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
    let scope = default_scope(c.scope, c.name.as_deref());
    let owner = owner(model, scope, &c.target, ctx)?;
    if let Some(name) = &c.name {
        check_name(name)?;
        // As tmux: a user option not set at this scope is an error.
        if name.starts_with('@') && model.options.get(owner, name).is_none() {
            return Err(format!("invalid option: {name}"));
        }
    }
    Ok(model
        .options
        .at(owner)
        .filter(|(name, _)| c.name.as_ref().is_none_or(|n| n == *name))
        .map(|(name, e)| format!("{name} {}\n", quote(&e.value)))
        .collect())
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
        Some(client) => expand(text, &clients::ClientVars::new(model, client, Some(vars))),
        None => expand(text, &vars),
    };
    Ok(format!("{out}\n"))
}

fn lines(model: &Model, fmt: &str, panes: impl Iterator<Item = PaneId>) -> String {
    panes
        .map(|p| format!("{}\n", expand(fmt, &FormatCtx::new(model, p))))
        .collect()
}

/// The pane a window's formats expand against: its active pane.
fn active_pane(model: &Model, window: WindowId) -> Option<PaneId> {
    model.windows.get(&window).map(|w| w.active)
}

fn list_sessions(model: &Model, c: ListSessions) -> Result<String, String> {
    let fmt = c.format.unwrap_or_default();
    let panes = model
        .sessions
        .values()
        .filter_map(|s| active_pane(model, s.active));
    Ok(lines(model, &fmt, panes))
}

fn list_windows(model: &Model, ctx: &ClientCtx, c: ListWindows) -> Result<String, String> {
    let session = model.resolve_session(&c.target, ctx)?;
    let fmt = c.format.unwrap_or_default();
    let panes = model.sessions[&session]
        .windows
        .iter()
        .filter_map(|w| active_pane(model, *w));
    Ok(lines(model, &fmt, panes))
}

fn list_panes(model: &Model, ctx: &ClientCtx, c: ListPanes) -> Result<String, String> {
    let window = model.resolve_window(&c.target, ctx)?;
    let fmt = c.format.unwrap_or_default();
    Ok(lines(
        model,
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

impl FormatVars for FormatCtx<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let m = self.model;
        if name.starts_with('@') || builtin(name).is_some() {
            return m
                .options
                .lookup(name, &chain(m, self.pane))
                .map(str::to_string);
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
            // ponytail: `client_width`/`client_height` need clients (M2);
            // `host_short` needs nix's `hostname` feature (not enabled).
            _ => return None,
        })
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
        assert_eq!(
            validate("status-position", "top"),
            Err("unknown value: top".to_string())
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
