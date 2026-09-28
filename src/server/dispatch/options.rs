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

use std::collections::BTreeMap;
use std::num::IntErrorKind;

use crate::cmd::{
    Command, DisplayMessage, FormatVars, ListPanes, ListSessions, ListWindows, OptionScope,
    Resolve, SetOption, ShowOptions, Target, expand,
};
use crate::event::Event;
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::events::Events;
use crate::server::model::{Model, PANE_BASE_INDEX};
use crate::types::{PaneId, SessionId, WindowId};

pub const REMAIN_ON_EXIT: &str = "remain-on-exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    /// A whole number from 0 to `max` (tmux's bounds).
    Number {
        max: i64,
    },
    /// Free text, stored as given. Formats, styles and key names in it are
    /// checked where they are used (status line, key tables), not here.
    String,
    /// One of a fixed set of words (tmux's choice options).
    Choice(&'static [&'static str]),
}

/// A built-in option: its type, the scope `set`/`show` use when no scope
/// flag is given (tmux's), and its global default.
struct Builtin {
    name: &'static str,
    kind: Kind,
    scope: OptionScope,
    default: &'static str,
}

// ponytail: only the options M1 and M2 need. `history-limit`, `base-index`
// and `pane-base-index` are stored and shown, but the model still uses its
// constants for them until config (M3) wires options through.
//
// M2 rows use tmux's names and scopes (tmux 3.7b bounds). Their defaults are
// tmux's except where grofe differs on purpose or M2 formats cannot yet say
// the same thing: `escape-time` 25 (spec §5.3), `automatic-rename-format`
// `#{b:pane_current_path}` (spec §4.5), `status-right` without the pane
// title, and the window formats without `#{window_flags}` (M3.6). tmux's
// escape-time is a server option; grofe's server scope is global.
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
        kind: Kind::String,
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
];

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
        }
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
        Command::DisplayMessage(c) => display_message(model, ctx, c),
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
    // `option-changed` only for a real change (spec §4.2).
    let changed = |model: &Model, events: &mut Events| {
        events.emit(Event::OptionChanged {
            scope: match owner {
                Owner::Global => "global".to_string(),
                Owner::Session(id) => id.to_string(),
                Owner::Window(id) => id.to_string(),
                Owner::Pane(id) => id.to_string(),
            },
            name: c.name.clone(),
            value: model.options.get(owner, &c.name).map(|e| e.value.clone()),
        });
    };
    if c.unset {
        if model.options.unset(owner, &c.name)? {
            changed(model, events);
        }
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
    if model.options.set(owner, &c.name, &value, c.persist)? {
        changed(model, events);
    }
    Ok(String::new())
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

/// tmux's default `display-message` text, minus the clock (strftime is
/// M2).
const DEFAULT_MESSAGE: &str =
    "[#{session_name}] #{window_index}:#{window_name}, current pane #{pane_index}";

// ponytail: with no clients before M2, display-message prints its text
// with or without `-p`; M2 shows it on the client's status line.
fn display_message(model: &Model, ctx: &ClientCtx, c: DisplayMessage) -> Result<String, String> {
    let pane = model.resolve_pane(&c.target, ctx)?;
    let text = c.text.as_deref().unwrap_or(DEFAULT_MESSAGE);
    Ok(format!("{}\n", expand(text, &FormatCtx::new(model, pane))))
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
            "pane_id" => p.id.to_string(),
            "pane_uid" => p.uid.clone(),
            "pane_index" => {
                let pos = w.pane_ids().iter().position(|id| *id == p.id)?;
                (pos as u32 + PANE_BASE_INDEX).to_string()
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
            "pane_width" => p.cols.to_string(),
            "pane_height" => p.rows.to_string(),
            "pane_dead" => flag(p.dead.is_some()),
            // ponytail: `client_width`/`client_height` need clients (M2);
            // `host_short` needs nix's `hostname` feature (not enabled).
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::server::model::{BASE_INDEX, HISTORY_LIMIT};

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

    #[test]
    fn show_quotes_values_with_spaces() {
        assert_eq!(quote("a"), "a");
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(""), "''");
    }

    /// Each case recorded from tmux 3.7b (`set -g @t <value>; show -g @t`)
    /// on a private socket.
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
