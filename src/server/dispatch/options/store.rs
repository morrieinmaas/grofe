use super::*;

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
    /// Set with `--persist`: the snapshot includes it.
    pub persist: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub(super) values: BTreeMap<Owner, BTreeMap<String, Entry>>,
    /// Global options the config set, each with the value it had before
    /// (a built-in default or a start-time seed), so `reload` can put back
    /// a value the config no longer sets (R10).
    pub(super) config_base: BTreeMap<String, String>,
    /// Global options set at runtime (`set -g`, `source-file`): the config
    /// no longer touches them (spec §5.1 layer 6, R10).
    pub(super) runtime: BTreeSet<String>,
    /// List options (TC.2b), beside the scalar store so [`Entry`] keeps
    /// its shape.
    pub(super) lists: BTreeMap<Owner, BTreeMap<String, ListEntry>>,
    /// Global lists the config set, each with the elements it had before,
    /// as `config_base` for scalars.
    pub(super) config_list_base: BTreeMap<String, BTreeMap<u32, String>>,
    /// Built-in defaults that differ under the server's identity
    /// ([`Options::use_tmux_defaults`]): what `set -gu` restores.
    pub(super) identity_defaults: BTreeMap<&'static str, String>,
    /// The same for list options.
    pub(super) identity_list_defaults: BTreeMap<&'static str, Vec<String>>,
    /// What the tmux front end was sent for a value it rewrote into
    /// grofe's format spelling (FE.3), by owner and name (`status-format[1]`
    /// for an element): the stored value it stands for, and the text as
    /// sent. A tmux-dialect `show` prints the text while the value is
    /// still that one (continuum and canopy read values back).
    pub(super) shown: BTreeMap<(Owner, String), (String, String)>,
}

/// tmux 3.7b's default `command-alias` entries (recorded: `show -gv
/// command-alias` on a `-f /dev/null` server), grofe's under tmux identity.
pub(super) const TMUX_COMMAND_ALIASES: &[&str] = &[
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
    /// (the `tmux_default` column), so a script sees what tmux would. `editor` is
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
        let defaults = BUILTINS
            .iter()
            .filter_map(|b| Some((b.name, b.tmux_default?.to_string())))
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
    pub(super) fn default_of(&self, b: &'static Builtin) -> &str {
        self.identity_defaults
            .get(b.name)
            .map_or(b.default, String::as_str)
    }

    /// Whether the user set global option `name`: the config sets it, or a
    /// runtime `set -g` (or `source-file`) did, even to its default value.
    pub fn set_globally(&self, name: &str) -> bool {
        self.config_base.contains_key(name) || self.runtime.contains(name)
    }

    /// Whether a runtime `set -g` holds global option `name`.
    pub fn is_runtime(&self, name: &str) -> bool {
        self.runtime.contains(name)
    }

    /// Marks a global option as set at runtime, which the config then
    /// leaves alone.
    pub(super) fn mark_runtime(&mut self, name: &str) {
        self.config_base.remove(name);
        self.config_list_base.remove(name);
        self.runtime.insert(name.to_string());
    }

    /// Takes global option `name` out of the runtime's hands, so the next
    /// `reload` applies the config's value, or the built-in default when the
    /// config sets none: the mark goes and the default becomes the option's
    /// config base. The value stays until the reload, which announces only a
    /// real change. Whether it was marked.
    pub fn release_runtime(&mut self, name: &str) -> bool {
        if !self.runtime.remove(name) {
            return false;
        }
        if let Some(b) = builtin(name) {
            let default = self.default_of(b).to_string();
            self.config_base.insert(name.to_string(), default);
        }
        true
    }

    /// Undoes [`Self::release_runtime`], when the reload that was to hand
    /// the option to the config failed.
    pub fn restore_runtime(&mut self, name: &str) {
        self.mark_runtime(name);
    }

    /// Sets `name` at `owner`. Returns whether the value changed: setting
    /// an option to its current value is a no-op (spec §4.2), so the
    /// caller emits `option-changed` only on `true`. `persist` (`--persist`)
    /// marks the option for the snapshot either way, and the mark is
    /// sticky (spec §6): a set without it keeps it, and only
    /// [`Options::clear_persist`] (`--no-persist`) or an unset clears it.
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
            e.persist |= persist;
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
            let changed = self.set(owner, name, &default, false);
            self.clear_persist(owner, name);
            return changed;
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

    /// `--no-persist`: drops `name`'s sticky `--persist` mark at `owner`.
    pub fn clear_persist(&mut self, owner: Owner, name: &str) {
        if let Some(e) = self.values.get_mut(&owner).and_then(|at| at.get_mut(name)) {
            e.persist = false;
        }
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

pub(super) fn bool_str(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// An error unless `name` is a user option or a known built-in.
pub(super) fn check_name(name: &str) -> Result<(), String> {
    if name.starts_with('@') || builtin(name).is_some() {
        Ok(())
    } else {
        Err(format!("invalid option: {name}"))
    }
}

/// The normalized value to store for `name`, or an error for an unknown
/// option or a value of the wrong type, worded as tmux words it.
pub(super) fn validate(name: &str, value: &str) -> Result<String, String> {
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
pub(super) const STYLE_OPTIONS: &[&str] = &[
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
pub(super) fn number_minimum(name: &str) -> i64 {
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
pub(super) fn colour_text(value: &str) -> Option<String> {
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
pub(super) fn size_text(value: &str) -> bool {
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
pub(super) fn default_scope(scope: Option<OptionScope>, name: Option<&str>) -> OptionScope {
    scope.unwrap_or_else(|| {
        name.and_then(|n| {
            builtin(n)
                .map(|b| b.scope)
                .or_else(|| list_builtin(n).map(|l| l.scope))
        })
        .unwrap_or(OptionScope::Session)
    })
}

pub(super) fn owner(
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
            let inert = split_index(&c.name)
                .ok()
                .map(|(base, _)| base.to_string())
                .filter(|base| has_no_effect(base) && !c.unset);
            let result = set_option(&mut state.model, &mut state.events, ctx, c);
            if let (Ok(_), Some(name)) = (&result, inert) {
                clients::warn_inert_option(state, &name);
            }
            result
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
pub(super) fn expand_value(state: &State, ctx: &ClientCtx, target: &Target, value: &str) -> String {
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

pub(super) fn set_option(
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
    if ctx.dialect == Dialect::Tmux && is_grofe_only(base) {
        return quiet(Err(format!("invalid option: {}", c.name)));
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
    if c.no_persist {
        model.options.clear_persist(owner, &c.name);
    }
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
pub(super) fn is_text(name: &str) -> bool {
    name.starts_with('@') || builtin(name).is_some_and(|b| b.kind == Kind::String)
}

/// `option-changed`, emitted only for a real change (spec §4.2).
pub(super) fn option_changed(model: &mut Model, events: &mut Events, owner: Owner, name: &str) {
    if name == "graphics" {
        crate::server::graphics::sync_option(model);
    }
    if matches!(
        name,
        "pane-frames" | "pane-gap" | "status" | "status-position"
    ) {
        model.sync_frames();
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
pub(super) fn owner_chain(model: &Model, owner: Owner) -> Vec<Owner> {
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

pub(super) fn show_options(
    model: &Model,
    ctx: &ClientCtx,
    c: ShowOptions,
) -> Result<String, String> {
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
            let unset_user = name.starts_with('@') && model.options.get(owner, name).is_none();
            if unset_user || (ctx.dialect == Dialect::Tmux && is_grofe_only(name)) {
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
                quote::tmux_arg(value)
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
    let wanted = |name: &str| {
        (!only_server || builtin_scope(name) == Some(OptionScope::Global))
            && !(tmux && is_grofe_only(name))
    };
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
                    .chain(grofe_only_names().filter(|_| !tmux))
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
pub(super) const SPELLED_DEFAULTS: &[&str] = &[
    "pane-active-border-style",
    "set-titles-string",
    "status-format",
    "status-right",
    "window-status-current-format",
    "window-status-format",
];

/// tmux's default text for `name` (`base[N]` for an element) when the
/// global `value` (`None`: no such element) is grofe's default for it.
pub(super) fn spelled_default(
    options: &Options,
    name: &str,
    value: Option<&str>,
) -> Option<String> {
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
pub(super) fn line_raw(name: &str, value: &str, value_only: bool) -> String {
    if value_only {
        format!("{value}\n")
    } else {
        format!("{name} {}\n", quote::tmux_arg(value))
    }
}

/// A global list's lines in the tmux dialect: grofe's elements as sent,
/// plus tmux's default rows grofe's list lacks, by index. `tmux_rows` are
/// tmux's own indices for the list, when a table lists them.
pub(super) fn tmux_list_lines(
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
pub(super) fn list_rows(table: crate::compat::tmux::formats::Table, base: &str) -> Vec<u32> {
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
pub(super) fn tmux_inherited_listing(model: &Model, owner: Owner, value_only: bool) -> String {
    use crate::compat::tmux::formats::{Table, tmux_table, unquote};
    let table = match owner {
        Owner::Window(_) => Table::Window,
        _ => Table::Session,
    };
    let o = &model.options;
    let parents = &owner_chain(model, owner)[1..];
    let star = |name: &str, text: &str| match value_only {
        true => format!("{text}\n"),
        false => format!("{name}* {}\n", quote::tmux_arg(text)),
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
pub(super) fn tmux_listing(model: &Model, owner: Owner, value_only: bool) -> String {
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
