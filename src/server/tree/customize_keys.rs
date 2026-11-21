use super::*;

// --- Customize-mode keys ------------------------------------------------

/// `d`, `D`, `u` and `U`: reset or unset the current item, or the tagged
/// ones, after asking.
pub(super) fn custom_change(
    state: &mut State,
    client: ClientId,
    v: &View,
    current: usize,
    reset: bool,
    tagged: bool,
) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let tags: Vec<u64> = if tagged {
        tagged_lines(v, tree)
            .into_iter()
            .filter_map(|t| match t {
                Tag::Custom(n) => Some(n),
                _ => None,
            })
            .collect()
    } else {
        custom_tag(v, current).into_iter().collect()
    };
    if tags.is_empty() {
        return Routed::Consumed;
    }
    let entries: Vec<(Entry, Fs)> = tags
        .iter()
        .filter_map(|t| custom_entry(state, client, *t))
        .collect();
    let Some((first, _)) = entries.first() else {
        return Routed::Consumed;
    };
    let what = if reset { "Reset" } else { "Unset" };
    let label = if tagged {
        format!(
            "{what} {} tagged{}? ",
            tags.len(),
            if reset { " to default" } else { "" }
        )
    } else {
        let name = match first {
            Entry::Option { name, idx, .. } => match idx {
                Some(i) if !reset => format!("{name}[{i}]"),
                _ => name.clone(),
            },
            Entry::Key { key, .. } | Entry::Field { key, .. } => key.to_string(),
            Entry::Group => return Routed::Consumed,
        };
        if reset {
            // An array item does not reset; a whole option or a key does.
            if matches!(first, Entry::Option { idx: Some(_), .. }) {
                return Routed::Consumed;
            }
            format!("Reset {name} to default? ")
        } else {
            format!("Unset {name}? ")
        }
    };
    // Unsetting the key under the cursor, or resetting one with no
    // default, first folds it and steps up (`window_customize_unset_key`,
    // `window_customize_reset_key`).
    let goes = match first {
        Entry::Key { table, key } | Entry::Field { table, key, .. } => {
            !reset || !customize::key_has_default(state, table, key)
        }
        _ => false,
    };
    let step_up = (goes && !tagged).then(|| {
        (
            v.lines.get(current).map(|l| l.tag),
            current
                .checked_sub(1)
                .and_then(|i| v.lines.get(i))
                .map(|l| l.tag),
            current,
        )
    });
    ask_yes(state, client, &label, move |state| {
        if let Some((Some(here), Some(above), at)) = step_up
            && let Some(tree) = state.trees.open.get_mut(&client)
        {
            tree.expanded.insert(here, false);
            tree.current = at - 1;
            tree.offset = tree.offset.min(at - 1);
            tree.tag = Some(above);
        }
        let mut cmds = Vec::new();
        let mut reset_names = Vec::new();
        let mut reset_keys = Vec::new();
        for (entry, fs) in entries {
            match entry {
                Entry::Option { scope, name, idx } => {
                    if reset && idx.is_some() {
                        continue;
                    }
                    // A global option with a TOML key goes back to the
                    // config layers (AC2.7); a local value or a name
                    // settings.toml cannot hold is unset, as in tmux.
                    if reset
                        && customize::is_global(scope)
                        && crate::config::toml_key(&name).is_some()
                    {
                        reset_names.push(name);
                        continue;
                    }
                    cmds.extend(customize::unset_commands(
                        state, fs, scope, &name, idx, reset,
                    ));
                }
                Entry::Key { table, key } | Entry::Field { table, key, .. } if reset => {
                    reset_keys.push((table, key));
                }
                Entry::Key { table, key } | Entry::Field { table, key, .. } => {
                    customize::unbind_key(state, &table, &key);
                }
                _ => {}
            }
        }
        if !reset {
            rebuild(state, client, None);
            return Routed::Command(cmds, Dialect::Tmux);
        }
        // A reset runs now, not queued behind a hook's `run-shell`, so
        // nothing it unsets lands after the reload.
        if let Err(e) = run_now(state, client, &cmds) {
            error_message(state, client, &e);
        }
        if (!reset_names.is_empty() || !reset_keys.is_empty())
            && let Err(e) = customize::reset(state, &reset_names, &reset_keys)
        {
            error_message(state, client, &e);
        }
        rebuild(state, client, None);
        Routed::Consumed
    })
}

/// Runs `cmds` for `client` at once, in order, in tmux's dialect; the first
/// error stops the rest.
fn run_now(state: &mut State, client: ClientId, cmds: &[Vec<String>]) -> Result<(), String> {
    let Some(c) = state.clients.get(&client) else {
        return Ok(());
    };
    let ctx = ClientCtx {
        dialect: Dialect::Tmux,
        ..c.ctx.clone()
    };
    for argv in cmds {
        crate::server::dispatch::run_split(state, &ctx, argv)?;
    }
    Ok(())
}

/// `P`: saves the option or key on line `line` to settings.toml.
pub(super) fn custom_persist(state: &mut State, client: ClientId, v: &View, line: usize) -> Routed {
    let entry = custom_tag(v, line).and_then(|t| custom_entry(state, client, t));
    let Some((entry, _)) = entry else {
        return Routed::Consumed;
    };
    match customize::persist(state, &entry) {
        Ok(text) => clients::show_message(state, client, &text),
        Err(e) => error_message(state, client, &e),
    }
    Routed::Consumed
}

/// customize-mode's own keys; `None` leaves the key to the chooser.
pub(super) fn custom_key(
    state: &mut State,
    client: ClientId,
    key: &Key,
    v: &View,
    current: usize,
) -> Option<Routed> {
    let plain = key.mods == Default::default();
    let ch = match key.code {
        KeyCode::Char(c) if plain => Some(c),
        _ => None,
    };
    let enter = plain && key.code == KeyCode::Enter;
    let routed = match ch {
        _ if enter => custom_enter(state, client, v, current, false, true),
        Some('s') => custom_enter(state, client, v, current, false, true),
        // `w` sets a window option; tmux ignores it on a key.
        Some('w')
            if custom_tag(v, current)
                .and_then(|t| custom_entry(state, client, t))
                .is_some_and(|(e, _)| matches!(e, Entry::Key { .. } | Entry::Field { .. })) =>
        {
            Routed::Consumed
        }
        Some('w') => custom_enter(state, client, v, current, false, false),
        Some('S' | 'W') => custom_enter(state, client, v, current, true, false),
        Some('P') => custom_persist(state, client, v, current),
        Some('d') => custom_change(state, client, v, current, true, false),
        Some('D') => custom_change(state, client, v, current, true, true),
        Some('u') => custom_change(state, client, v, current, false, false),
        Some('U') => custom_change(state, client, v, current, false, true),
        Some('H') => {
            if let Some(tree) = state.trees.open.get_mut(&client)
                && let Kind::Customize { hide_global } = &mut tree.kind
            {
                *hide_global = !*hide_global;
            }
            rebuild(state, client, None);
            Routed::Consumed
        }
        _ => return None,
    };
    Some(routed)
}
