use super::*;

/// A built-in list option: the scope used when no flag is given, and its
/// global default, element by element from index 0.
pub(super) struct ListBuiltin {
    pub(super) name: &'static str,
    pub(super) scope: OptionScope,
    pub(super) default: &'static [&'static str],
}

// ponytail: `status-format`'s default row 0 is empty, which TC.11 renders
// as the built-in row (status-left, window list, status-right), since the
// native format language has no window-list loop to spell it with.
// grofe's own aliases have no default (tmux's six name verbs grofe lacks).
pub(super) const LISTS: &[ListBuiltin] = &[
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
    // grofe's own (`GROFE_ONLY`): copy mode's hints.
    ListBuiltin {
        name: "copy-hint-patterns",
        scope: OptionScope::Global,
        default: HINT_PATTERNS,
    },
];

/// The separator tmux 3.7b splits a list option's value on when it is set
/// or appended without an index (recorded); `status-format` has none.
pub(super) fn list_separator(name: &str) -> Option<char> {
    match name {
        "terminal-features" | "terminal-overrides" | "command-alias" | "codepoint-widths"
        | "pane-colours" | "user-keys" => Some(','),
        "update-environment" => Some(' '),
        _ => None,
    }
}

pub(super) fn pieces(value: &str, sep: char) -> impl Iterator<Item = String> + '_ {
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

pub(super) fn list_builtin(name: &str) -> Option<&'static ListBuiltin> {
    LISTS.iter().find(|l| l.name == name)
}

/// Every built-in list option tmux knows, in table order.
pub fn list_names() -> impl Iterator<Item = &'static str> {
    LISTS
        .iter()
        .map(|l| l.name)
        .filter(|n| !GROFE_ONLY_LISTS.contains(n))
}

/// The list options only grofe's own CLI and config know ([`GROFE_ONLY_LISTS`]).
pub fn grofe_only_list_names() -> impl Iterator<Item = &'static str> {
    GROFE_ONLY_LISTS.iter().copied()
}

/// Options grofe accepts and stores but never reads (spec §4.5, "Options
/// by tmux name"), by tmux name. `show-config --explain` marks them
/// `(no effect)`. FE.1's rows: every such option in the corpus, each with
/// its `BUILTINS` or `LISTS` entry.
pub(super) const NO_EFFECT: &[&str] = &[
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

pub(super) fn default_list(l: &ListBuiltin) -> ListEntry {
    ListEntry {
        items: (0..).zip(l.default.iter().map(|s| s.to_string())).collect(),
        persist: false,
    }
}

pub(super) fn default_lists() -> BTreeMap<String, ListEntry> {
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
        if name == "copy-hint-patterns" {
            for p in op_values(&op) {
                regex::Regex::new(p).map_err(|e| format!("bad hint pattern {p}: {e}"))?;
            }
        }
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
pub(super) fn c_unsigned(s: &str) -> Option<u32> {
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
pub(super) fn not_a_list(base: &str, full: &str) -> String {
    if base.starts_with('@') || builtin(base).is_some() {
        format!("not an array: {full}")
    } else {
        format!("invalid option: {full}")
    }
}

/// The element texts an edit sets.
pub(super) fn op_values(op: &ListOp) -> Vec<&str> {
    match op {
        ListOp::Set { value, .. } | ListOp::Push(value) | ListOp::Replace(value) => {
            vec![value.as_str()]
        }
        ListOp::Items(items) => items.values().map(String::as_str).collect(),
        ListOp::Remove(_) | ListOp::Reset => Vec::new(),
    }
}

/// `set-option` on a list option, or on any name given an index.
pub(super) fn set_list(
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
pub(super) fn list_items(items: &[String]) -> BTreeMap<u32, String> {
    (0..).zip(items.iter().cloned()).collect()
}

/// Sets a whole global list, emitting `option-changed` (value-less, as
/// for any whole-list change) on a change.
pub(super) fn set_global_list(
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
pub(super) fn apply_config_lists(model: &mut Model, events: &mut Events, resolved: &Resolved) {
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

pub(super) fn scope_name(owner: Owner) -> String {
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
pub(super) fn list_lines(name: &str, l: &ListEntry, value_only: bool) -> String {
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
                format!("{name}[{i}] {}\n", quote::tmux_arg(v))
            }
        })
        .collect()
}

/// `show-options` of a list option, or of any name given an index.
// ponytail: `-v` here covers lists and indexed names only; a plain
// scalar's `-v` and every `-q` are TC.4's.
pub(super) fn show_list(
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
            format!("{full} {}\n", quote::tmux_arg(value))
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
