use super::*;

/// Where the server reads its config: the process environment (R5). Unit
/// tests never read the real config dir.
pub(super) fn config_env() -> config::Env {
    if cfg!(test) {
        return config::Env {
            config_dir: None,
            config_file: None,
            home: None,
        };
    }
    config::Env::from_process()
}

/// `config-file` (internal, asked by a client given `--config`): the files
/// the config loads from, one per line: the config file, or under `tmux -f`
/// those files that exist.
pub fn config_files() -> String {
    let files: Vec<PathBuf> = match crate::compat::start_config() {
        None => config_env().config_file.into_iter().collect(),
        Some(files) => files
            .into_iter()
            .filter(|f| f != Path::new("/dev/null") && f.exists())
            .collect(),
    };
    files.iter().map(|f| format!("{}\n", f.display())).collect()
}

/// The `tmux -f` files that count: a missing file or `/dev/null` is none.
fn start_files(files: Vec<PathBuf>) -> Vec<PathBuf> {
    files
        .into_iter()
        .filter(|f| f != Path::new("/dev/null") && f.exists() && !is_tmux_conf(f))
        .collect()
}

/// The config chain (spec §5.1), or with `tmux -f <file>...` those files
/// alone, in order (`compat::start_config`): a missing file or
/// `/dev/null` is no file, so none at all leaves the built-in defaults.
pub(super) fn load_config() -> Result<Resolved, Vec<config::ConfigError>> {
    let env = config_env();
    match crate::compat::start_config() {
        None => config::load(&env),
        Some(files) => {
            let only = config::Env {
                config_dir: None,
                ..env
            };
            config::load_with_file(&only, &start_files(files))
        }
    }
}

/// A `tmux -f` file is tmux config (not grofe TOML) when a CLI run as
/// `tmux` started the server, as `source-file` reads a file by dialect;
/// a `.toml` stays grofe's.
pub(super) fn is_tmux_conf(file: &Path) -> bool {
    crate::compat::started_as_tmux() && file.extension().is_none_or(|e| e != "toml")
}

/// Sources the `tmux -f` files that are tmux config, in order, after the
/// server took tmux's defaults. A missing file or `/dev/null` is no
/// config; an error is logged and the next file still loads.
pub fn source_start_files(state: &mut State) {
    let files = crate::compat::start_config().unwrap_or_default();
    for file in files {
        if file == Path::new("/dev/null") || !file.exists() || !is_tmux_conf(&file) {
            continue;
        }
        let ctx = ClientCtx {
            cwd: PathBuf::from("/"),
            pane: None,
            env: Vec::new(),
            client: None,
            dialect: Dialect::Tmux,
            request: Default::default(),
        };
        let c = SourceFile {
            quiet: false,
            format: false,
            parse_only: false,
            verbose: false,
            target: None,
            paths: vec![glob_escape(&file)],
        };
        match source_file(state, &ctx, c) {
            Ok(_) => {}
            Err(e) => eprintln!("grofe config: {}: {e}", file.display()),
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

pub(super) fn log_warnings(resolved: &Resolved) {
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

pub(super) fn join_errors(errors: &[config::ConfigError]) -> String {
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
pub(super) fn apply_config(state: &mut State, resolved: &Resolved) {
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
    crate::server::dispatch::agent::configure(state, resolved);
    crate::server::configure_surfaces(state, resolved);
    crate::server::clients::set_scan_interval(state, resolved.agents.scan_interval);
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
    // Luau last: a fresh runtime, then the packs, then `init.luau`.
    let config_dir = config_env().config_dir.unwrap_or_default();
    crate::server::lua_host::reset(state);
    crate::server::pack_load::load_all(state, &resolved.pack, &config_dir);
    crate::server::lua_host::load_init(state, &config_dir);
}

/// `value` with `$NAME`, `${NAME}` and a leading `~` expanded from the
/// global environment, as tmux expands a config line's words; an unset
/// name is empty.
pub(super) fn expand_env(state: &State, value: &str) -> String {
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
pub(super) fn set_global(model: &mut Model, events: &mut Events, name: &str, value: &str) {
    match model.options.set(Owner::Global, name, value, false) {
        Ok(true) => option_changed(model, events, Owner::Global, name),
        Ok(false) => {}
        Err(e) => eprintln!("grofe config: {name}: {e}"),
    }
}
