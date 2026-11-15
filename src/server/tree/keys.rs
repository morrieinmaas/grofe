use super::*;

// --- Keys ---------------------------------------------------------------

/// Takes a client's keys while its chooser is open, as tmux gives a pane
/// mode the keys the client's key table does not bind: after the prefix, the
/// prefix key itself, and a key bound in `root` still go to the tables.
pub struct TreeHook;

impl InputHook for TreeHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.trees.is_open(client) {
            return None;
        }
        let tree_pane = state.trees.open.get(&client).map(|t| t.pane)?;
        let Some(window) = state.model.panes.get(&tree_pane).map(|p| p.window) else {
            close(state, client);
            return None;
        };
        // Keys go to the chooser only while its pane is the client's
        // active pane, as tmux gives a mode the keys of its own pane.
        let model = &state.model;
        let shown = state
            .clients
            .get(&client)
            .and_then(|c| model.sessions.get(&c.session))
            .is_some_and(|s| {
                s.active == window
                    && model
                        .windows
                        .get(&window)
                        .is_some_and(|w| w.active == tree_pane)
            });
        if !shown {
            return None;
        }
        // The help popup takes every key and closes on it; the rest of the
        // input is eaten while it is up.
        if state.trees.open.get(&client).is_some_and(|t| t.help) {
            if matches!(input, Input::Focus(_)) {
                return None;
            }
            if matches!(input, Input::Key(_))
                && let Some(tree) = state.trees.open.get_mut(&client)
            {
                tree.help = false;
            }
            clients::mark_dirty(state, client);
            return Some(Routed::Consumed);
        }
        let key = match input {
            Input::Key(key) => *key,
            Input::Paste { .. } => return Some(Routed::Consumed),
            Input::Mouse(_) | Input::Focus(_) => return None,
        };
        if goes_to_tables(state, client, &key) {
            return None;
        }
        let routed = key_press(state, client, &key);
        clients::mark_dirty(state, client);
        Some(routed)
    }
}

/// Whether `key` is the client's to route through its key tables.
pub(super) fn goes_to_tables(state: &State, client: ClientId, key: &Key) -> bool {
    let Some(c) = state.clients.get(&client) else {
        return true;
    };
    if c.keys.table() != ROOT_TABLE {
        return true;
    }
    let settings = clients::key_settings(&state.model, c.session);
    if settings.is_prefix(key) {
        return true;
    }
    state.registry.key_tables.get(ROOT_TABLE, key).is_some()
}

/// The line a key chooses directly: the line whose key it is.
pub(super) fn line_of_key(key: &Key, keys: &[Option<String>]) -> Option<usize> {
    let name = key.to_string();
    keys.iter()
        .position(|k| k.as_deref() == Some(name.as_str()))
}

/// Rebuilds the tree for `client`, keeping `tag` as the current item
/// (`mode_tree_build`), then scrolling as `mode_tree_set_current` does.
pub(super) fn rebuild(state: &mut State, client: ClientId, tag: Option<Tag>) {
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let (items, _) = build(state, tree);
    let lines = lines_of(&items, tree);
    let rows = state
        .model
        .panes
        .get(&tree.pane)
        .map_or(0, |p| usize::from(p.rows));
    let tag = tag.or(tree.tag);
    let height = tree.list_height(rows, lines.len());
    let (current, offset) = set_current(&lines, tag, tree.current, tree.offset, tree.height);
    let tag = lines.get(current).map(|l| l.tag);
    if let Some(tree) = state.trees.open.get_mut(&client) {
        tree.current = current;
        tree.offset = offset;
        tree.height = height;
        tree.tag = tag;
        if height > 0 && current > height - 1 {
            tree.offset = tree.offset.max(current - height + 1);
        }
    }
}

/// The item `tag` and everything under it in `items`.
pub(super) fn find_item(items: &[Item], tag: Tag) -> Option<&Item> {
    for item in items {
        if item.tag == tag {
            return Some(item);
        }
        if let Some(found) = find_item(&item.children, tag) {
            return Some(found);
        }
    }
    None
}

pub(super) fn descendants(item: &Item, out: &mut Vec<Tag>) {
    for child in &item.children {
        out.push(child.tag);
        descendants(child, out);
    }
}

/// The pane an item stands for (a session's or window's active one).
pub(super) fn pane_of(state: &State, tag: Tag) -> Option<PaneId> {
    let model = &state.model;
    match tag {
        Tag::Pane(p) => Some(p),
        Tag::Window(w) => model.windows.get(&w).map(|w| w.active),
        Tag::Session(s) => model
            .sessions
            .get(&s)
            .and_then(|s| model.windows.get(&s.active))
            .map(|w| w.active),
        Tag::Client(_) | Tag::Buffer(_) | Tag::Custom(_) => None,
    }
}

/// What `%%` stands for in the command run for an item
/// (`window_tree_get_target` and the others): `=session:`,
/// `=session:index.`, `=session:index.%pane`, a client's tty, a buffer's
/// name.
pub(super) fn target_name(state: &State, session: Option<SessionId>, tag: Tag) -> Option<String> {
    let model = &state.model;
    match tag {
        Tag::Session(s) => Some(format!("={}:", model.sessions.get(&s)?.name)),
        Tag::Window(w) => {
            let s = session.or_else(|| model.windows.get(&w).map(|w| w.session))?;
            Some(format!(
                "={}:{}.",
                model.sessions.get(&s)?.name,
                model.index_in(s, w)?
            ))
        }
        Tag::Pane(p) => {
            let w = model.panes.get(&p)?.window;
            let s = session.or_else(|| model.windows.get(&w).map(|w| w.session))?;
            Some(format!(
                "={}:{}.%{}",
                model.sessions.get(&s)?.name,
                model.index_in(s, w)?,
                p.0
            ))
        }
        Tag::Client(c) => {
            let c = state.clients.get(&c)?;
            Some(c.tty.clone().unwrap_or_else(|| c.name.clone()))
        }
        Tag::Buffer(id) => Some(state.buffers.by_id(id)?.name.clone()),
        Tag::Custom(_) => None,
    }
}

/// `mode_tree_run_command`: `template` with `%%` the item's `name`,
/// split into the commands to run; a parse error shows on the status line
/// with a capital.
pub(super) fn template_commands(
    state: &mut State,
    client: ClientId,
    template: &str,
    names: &[String],
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for name in names {
        let command = crate::server::prompt::replace_template(template, name);
        if command.is_empty() {
            continue;
        }
        match cmd::split_command_string(&command) {
            Ok(cmds) => out.extend(cmds),
            Err(e) => {
                let mut text = e.to_string();
                if let Some(first) = text.get(..1) {
                    text = first.to_uppercase() + &text[1..];
                }
                clients::show_message(state, client, &text);
            }
        }
    }
    out
}

// --- customize-mode -----------------------------------------------------

/// Where the chooser's pane is, for customize-mode's scopes.
pub(super) fn custom_fs(state: &State, tree: &Tree) -> Option<Fs> {
    let model = &state.model;
    let window = model.panes.get(&tree.pane)?.window;
    let session = model.link_session(window, tree.session)?;
    Some(Fs {
        session,
        window,
        pane: tree.pane,
    })
}

/// Builds customize-mode's tree for `tree`, with the pane's variables
/// under its formats.
pub(super) fn custom_build(
    state: &State,
    tree: &Tree,
    filter: Option<&str>,
    hide_global: bool,
) -> Option<(customize::Built, Fs)> {
    let fs = custom_fs(state, tree)?;
    let format = tree.item_format();
    let filter = filter.map(|f| tree.native(f));
    let ctx = FormatCtx::new(&state.model, fs.pane);
    let vars = ServerVars::new(state, fs.pane, &ctx);
    Some((
        customize::items(state, fs, &format, filter.as_deref(), hide_global, &vars),
        fs,
    ))
}

pub(super) fn custom_nodes(nodes: Vec<customize::Node>) -> Vec<Item> {
    nodes
        .into_iter()
        .map(|n| Item {
            tag: Tag::Custom(n.tag),
            plain: n.text.is_none(),
            text: n.text.unwrap_or_default(),
            name: n.name,
            children: custom_nodes(n.children),
            no_tag: n.no_tag,
            as_parent: n.as_parent,
            start_expanded: Some(n.expanded),
            ..Item::plain_defaults()
        })
        .collect()
}

pub(super) fn custom_items(
    state: &State,
    tree: &Tree,
    filter: Option<&str>,
    hide_global: bool,
) -> Vec<Item> {
    match custom_build(state, tree, filter, hide_global) {
        Some((built, _)) => custom_nodes(built.nodes),
        None => Vec::new(),
    }
}

/// What the item with `tag` is, and where the pane is.
pub(super) fn custom_entry(state: &State, client: ClientId, tag: u64) -> Option<(Entry, Fs)> {
    let tree = state.trees.open.get(&client)?;
    let hide = matches!(tree.kind, Kind::Customize { hide_global: true });
    let (built, fs) = custom_build(state, tree, tree.filter.as_deref(), hide)?;
    built.entries.get(&tag).cloned().map(|e| (e, fs))
}

pub(super) fn custom_tag(v: &View, line: usize) -> Option<u64> {
    match v.lines.get(line)?.tag {
        Tag::Custom(t) => Some(t),
        _ => None,
    }
}

/// Asks `label` (one key), and runs `then` when it is `y`; at once with
/// `-y`.
pub(super) fn ask_yes(
    state: &mut State,
    client: ClientId,
    label: &str,
    then: impl FnOnce(&mut State) -> Routed + Send + 'static,
) -> Routed {
    if state.trees.open.get(&client).is_some_and(|t| t.prompt_yes) {
        return then(state);
    }
    crate::server::prompt::ask(
        state,
        client,
        label,
        "",
        "command",
        true,
        None,
        Box::new(move |state, _client, text| {
            let mut chars = text.chars();
            let yes = matches!(chars.next(), Some(c) if c.eq_ignore_ascii_case(&'y'))
                && chars.next().is_none();
            if yes { then(state) } else { Routed::Consumed }
        }),
    );
    Routed::Consumed
}

/// A parse error as a status message with a capital, as tmux shows one.
pub(super) fn error_message(state: &mut State, client: ClientId, e: &str) {
    let mut text = e.to_string();
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    clients::show_message(state, client, &text);
}

/// Enter, `s`, `w`, `S` and `W` on line `line`: sets the option, or edits
/// the key's row (`window_customize_set_option`, `_set_key`).
pub(super) fn custom_enter(
    state: &mut State,
    client: ClientId,
    v: &View,
    line: usize,
    global: bool,
    pane: bool,
) -> Routed {
    let Some(tag) = custom_tag(v, line) else {
        return Routed::Consumed;
    };
    let Some((entry, fs)) = custom_entry(state, client, tag) else {
        return Routed::Consumed;
    };
    match entry {
        Entry::Option { scope, name, idx } => {
            match customize::set(state, fs, scope, &name, idx, global, pane) {
                Some(Set::Command(argv)) => Routed::Command(vec![argv], Dialect::Tmux),
                Some(Set::Prompt {
                    label,
                    initial,
                    scope,
                    idx,
                }) => {
                    crate::server::prompt::ask(
                        state,
                        client,
                        &label,
                        &initial,
                        "command",
                        false,
                        None,
                        Box::new(move |state, _client, text| {
                            if text.is_empty() {
                                return Routed::Consumed;
                            }
                            let argv = customize::set_command(state, scope, &name, idx, text);
                            Routed::Command(vec![argv], Dialect::Tmux)
                        }),
                    );
                    Routed::Consumed
                }
                None => Routed::Consumed,
            }
        }
        Entry::Field { table, key, field } if !global => match field {
            Field::Repeat => {
                customize::toggle_key_repeat(state, &table, &key);
                Routed::Consumed
            }
            Field::Command => {
                let initial = customize::key_command_text(state, &table, &key);
                crate::server::prompt::ask(
                    state,
                    client,
                    &format!("({key}) "),
                    &initial,
                    "command",
                    false,
                    None,
                    Box::new(move |state, client, text| {
                        if text.is_empty() {
                            return Routed::Consumed;
                        }
                        match cmd::split_command_string(text) {
                            Ok(cmds) if !cmds.is_empty() => {
                                customize::set_key_commands(state, &table, &key, cmds)
                            }
                            Ok(_) => {}
                            Err(e) => error_message(state, client, &e.to_string()),
                        }
                        Routed::Consumed
                    }),
                );
                Routed::Consumed
            }
            Field::Note => {
                let initial = customize::key_note_text(state, &table, &key);
                crate::server::prompt::ask(
                    state,
                    client,
                    &format!("({key}) "),
                    &initial,
                    "command",
                    false,
                    None,
                    Box::new(move |state, _client, text| {
                        if !text.is_empty() {
                            customize::set_key_note(state, &table, &key, text);
                        }
                        Routed::Consumed
                    }),
                );
                Routed::Consumed
            }
        },
        _ => Routed::Consumed,
    }
}

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
        for (entry, fs) in entries {
            match entry {
                Entry::Option { scope, name, idx } => {
                    if reset && idx.is_some() {
                        continue;
                    }
                    cmds.extend(customize::unset_commands(
                        state, fs, scope, &name, idx, reset,
                    ));
                }
                Entry::Key { table, key } | Entry::Field { table, key, .. } if reset => {
                    customize::reset_key(state, &table, &key);
                }
                Entry::Key { table, key } | Entry::Field { table, key, .. } => {
                    customize::unbind_key(state, &table, &key);
                }
                _ => {}
            }
        }
        rebuild(state, client, None);
        Routed::Command(cmds, Dialect::Tmux)
    })
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

pub(super) fn key_press(state: &mut State, client: ClientId, key: &Key) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let v = view(state, tree);
    if v.lines.is_empty() {
        close(state, client);
        return Routed::Consumed;
    }
    let (mut current, mut offset) = (v.current, v.offset);
    let (height, n) = (v.height.max(1), v.lines.len());
    let before = v.lines[current].tag;
    let mut rebuild_to: Option<Option<Tag>> = None;
    let mut expanded: Vec<(Tag, bool)> = Vec::new();
    let mut preview_step = 0;
    let mut start_tags = None;
    let kind_tree = matches!(tree.kind, Kind::Tree { .. });
    let kind_buffers = matches!(tree.kind, Kind::Buffers);
    let kind_clients = matches!(tree.kind, Kind::Clients);

    let up = |c: &mut usize, o: &mut usize, wrap: bool| {
        if *c == 0 {
            if wrap {
                *c = n - 1;
                if n >= height {
                    *o = n - height;
                }
            }
        } else {
            *c -= 1;
            if *c < *o {
                *o -= 1;
            }
        }
    };
    let down = |c: &mut usize, o: &mut usize, wrap: bool| {
        if *c == n - 1 {
            if wrap {
                *c = 0;
                *o = 0;
            }
        } else {
            *c += 1;
            if *c > *o + height - 1 {
                *o += 1;
            }
        }
    };

    let kind_custom = matches!(tree.kind, Kind::Customize { .. });
    if let Some(i) = line_of_key(key, &v.keys) {
        if kind_custom {
            return custom_enter(state, client, &v, i, false, true);
        }
        return choose(state, client, i);
    }
    let (m, plain) = (key.mods, Default::default());
    let ctrl = |c: char| key.code == KeyCode::Char(c) && m.ctrl && !m.alt;
    let alt = |c: char| key.code == KeyCode::Char(c) && m.alt && !m.ctrl;
    let is = |c: char| key.code == KeyCode::Char(c) && m == plain;
    let shifted = |code: KeyCode| key.code == code && m.shift && !m.ctrl && !m.alt;
    let code = if m == plain { Some(key.code) } else { None };
    let line = &v.lines[current];
    let line_tag = line.tag;
    let line_session = line.session;
    if kind_custom && let Some(routed) = custom_key(state, client, key, &v, current) {
        return routed;
    }
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    if code == Some(KeyCode::F(1)) || ctrl('h') {
        open_help(state, client);
        return Routed::Consumed;
    }
    if is('q') || code == Some(KeyCode::Escape) || ctrl('[') || ctrl('g') {
        close(state, client);
        return Routed::Consumed;
    } else if code == Some(KeyCode::Enter) || ctrl('m') || (is('p') && kind_buffers) {
        return choose(state, client, current);
    } else if is('P') && kind_buffers {
        let tagged = tagged_lines(&v, tree);
        if tagged.is_empty() {
            return Routed::Consumed;
        }
        return choose_many(state, client, &tagged);
    } else if is('d') && kind_buffers {
        // `window_buffer_do_delete`: the selection moves off first.
        if let Tag::Buffer(id) = line_tag
            && let Some(name) = state.buffers.by_id(id).map(|b| b.name.clone())
        {
            if current + 1 < n {
                current += 1;
            } else {
                current = current.saturating_sub(1);
            }
            let next = v.lines.get(current).map(|l| l.tag);
            state.buffers.delete(&name);
            if state.buffers.is_empty() {
                close(state, client);
            } else if let Some(t) = state.trees.open.get_mut(&client) {
                t.tag = next;
                t.current = current.min(n.saturating_sub(2));
            }
        }
        return Routed::Consumed;
    } else if is('D') && (kind_buffers || kind_clients) {
        let tagged = tagged_lines(&v, tree);
        if kind_buffers {
            for tag in tagged {
                if let Tag::Buffer(id) = tag
                    && let Some(name) = state.buffers.by_id(id).map(|b| b.name.clone())
                {
                    state.buffers.delete(&name);
                }
            }
            if state.buffers.is_empty() {
                close(state, client);
            }
            return Routed::Consumed;
        }
        return detach_clients(state, client, &tagged, "detach-client");
    } else if (is('d') || is('x') || is('z')) && kind_clients {
        let verb = match key.code {
            KeyCode::Char('z') => "suspend-client",
            KeyCode::Char('x') => "detach-client-kill",
            _ => "detach-client",
        };
        return detach_clients(state, client, &[line_tag], verb);
    } else if (is('X') || is('Z')) && kind_clients {
        let verb = if is('Z') {
            "suspend-client"
        } else {
            "detach-client-kill"
        };
        let tagged = tagged_lines(&v, tree);
        return detach_clients(state, client, &tagged, verb);
    } else if code == Some(KeyCode::Up) || is('k') || ctrl('p') {
        up(&mut current, &mut offset, true);
    } else if code == Some(KeyCode::Down) || is('j') || ctrl('n') {
        down(&mut current, &mut offset, true);
    } else if shifted(KeyCode::Up) || is('K') {
        return swap_windows(state, client, &v, current, -1);
    } else if shifted(KeyCode::Down) || is('J') {
        return swap_windows(state, client, &v, current, 1);
    } else if code == Some(KeyCode::PageUp) || ctrl('b') {
        for _ in 0..height {
            if current == 0 {
                break;
            }
            up(&mut current, &mut offset, true);
        }
    } else if code == Some(KeyCode::PageDown) || ctrl('f') {
        for _ in 0..height {
            if current == n - 1 {
                break;
            }
            down(&mut current, &mut offset, true);
        }
    } else if is('g') || code == Some(KeyCode::Home) {
        current = 0;
        offset = 0;
    } else if is('G') || code == Some(KeyCode::End) {
        current = n - 1;
        offset = if current > height - 1 {
            current - height + 1
        } else {
            0
        };
    } else if is('t') && line.no_tag {
        return Routed::Consumed;
    } else if is('t') {
        // Parents and children are never both tagged: the current item's
        // parents and children are untagged first.
        let (items, _) = build(state, tree);
        let mut gone = Vec::new();
        let mut parent = line.parent;
        while let Some(p) = parent {
            gone.push(v.lines[p].tag);
            parent = v.lines[p].parent;
        }
        if let Some(item) = find_item(&items, line_tag) {
            descendants(item, &mut gone);
        }
        let on = !tree.tagged.contains(&line_tag);
        let Some(tagged) = state.trees.open.get_mut(&client).map(|t| &mut t.tagged) else {
            return Routed::Consumed;
        };
        if on {
            for tag in gone {
                tagged.remove(&tag);
            }
            tagged.insert(line_tag);
        } else {
            tagged.remove(&line_tag);
        }
        // tmux hands the key a mouse event, so the cursor moves on.
        down(&mut current, &mut offset, false);
    } else if is('T') {
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.tagged.clear();
        }
        return Routed::Consumed;
    } else if ctrl('t') {
        let all: BTreeSet<Tag> = v
            .lines
            .iter()
            .filter(|l| match l.parent {
                None => !l.no_tag,
                Some(p) => v.lines[p].no_tag,
            })
            .map(|l| l.tag)
            .collect();
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.tagged = all;
        }
        return Routed::Consumed;
    } else if is('O') {
        // `sort_next_order`: a mode without orders (customize) ignores `O`.
        let Some(order) = next_order(tree.kind.orders(), tree.order) else {
            return Routed::Consumed;
        };
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.order = order;
        }
        rebuild_to = Some(None);
    } else if is('r') {
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.reversed = !tree.reversed;
        }
        rebuild_to = Some(None);
    } else if code == Some(KeyCode::Left) || is('h') || is('-') {
        let target = if line.flat || !line.expanded {
            line.parent
        } else {
            Some(current)
        };
        match target {
            None => up(&mut current, &mut offset, false),
            Some(p) => {
                expanded.push((v.lines[p].tag, false));
                current = p;
                rebuild_to = Some(None);
            }
        }
    } else if code == Some(KeyCode::Right) || is('l') || is('+') {
        if line.flat || line.expanded {
            down(&mut current, &mut offset, false);
        } else {
            expanded.push((line_tag, true));
            rebuild_to = Some(None);
        }
    } else if alt('-') || alt('+') {
        for l in v.lines.iter().filter(|l| l.depth == 0) {
            expanded.push((l.tag, alt('+')));
        }
        rebuild_to = Some(None);
    } else if is('<') && kind_tree {
        preview_step = -1;
    } else if is('>') && kind_tree {
        preview_step = 1;
    } else if is('H') && kind_tree {
        start_tags = Some(tree.start);
        rebuild_to = Some(None);
    } else if is('v') {
        let next = match tree.preview {
            PreviewMode::Off => PreviewMode::Big,
            PreviewMode::Normal => PreviewMode::Off,
            PreviewMode::Big => PreviewMode::Normal,
        };
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.preview = next;
        }
        rebuild_to = Some(None);
    } else if is('?') || is('/') || ctrl('s') {
        return ask_search(state, client);
    } else if is('n') || is('N') {
        let forward = is('n');
        let found = {
            let Some(tree) = state.trees.open.get_mut(&client) else {
                return Routed::Consumed;
            };
            if let Some(s) = tree.search.as_mut() {
                s.forward = forward;
            }
            tree.search.clone()
        };
        if found.is_some() {
            search_set(state, client);
        }
        return Routed::Consumed;
    } else if is('f') {
        let initial = tree.filter.clone().unwrap_or_default();
        crate::server::prompt::ask(
            state,
            client,
            "(filter) ",
            &initial,
            "search",
            false,
            None,
            Box::new(move |state, client, text| {
                if let Some(tree) = state.trees.open.get_mut(&client) {
                    tree.filter = (!text.is_empty()).then(|| text.to_string());
                }
                rebuild(state, client, None);
                Routed::Consumed
            }),
        );
        return Routed::Consumed;
    } else if is('m') && kind_tree {
        let Some(pane) = pane_of(state, line_tag) else {
            return Routed::Consumed;
        };
        return Routed::Command(
            vec![vec![
                "select-pane".into(),
                "-m".into(),
                "-t".into(),
                format!("%{}", pane.0),
            ]],
            Dialect::Tmux,
        );
    } else if is('M') && kind_tree {
        return Routed::Command(vec![vec!["select-pane".into(), "-M".into()]], Dialect::Tmux);
    } else if is('x') && kind_tree {
        return kill_prompt(state, client, &[(line_tag, line_session)], false);
    } else if is('X') && kind_tree {
        let tagged = tagged_lines(&v, tree);
        if tagged.is_empty() {
            return Routed::Consumed;
        }
        let items: Vec<_> = v
            .lines
            .iter()
            .filter(|l| tagged.contains(&l.tag))
            .map(|l| (l.tag, l.session))
            .collect();
        return kill_prompt(state, client, &items, true);
    } else if is(':') {
        let tagged = tagged_lines(&v, tree);
        let label = if tagged.is_empty() {
            "(current) ".to_string()
        } else {
            format!("({} tagged) ", tagged.len())
        };
        let items: Vec<(Tag, Option<SessionId>)> = if tagged.is_empty() {
            vec![(line_tag, line_session)]
        } else {
            v.lines
                .iter()
                .filter(|l| tagged.contains(&l.tag))
                .map(|l| (l.tag, l.session))
                .collect()
        };
        crate::server::prompt::ask(
            state,
            client,
            &label,
            "",
            "command",
            false,
            None,
            Box::new(move |state, client, text| {
                if text.is_empty() {
                    return Routed::Consumed;
                }
                let names: Vec<String> = items
                    .iter()
                    .filter_map(|(t, s)| target_name(state, *s, *t))
                    .collect();
                let dialect = state
                    .trees
                    .open
                    .get(&client)
                    .map_or(Dialect::Tmux, |t| t.dialect);
                let cmds = template_commands(state, client, text, &names);
                Routed::Command(cmds, dialect)
            }),
        );
        return Routed::Consumed;
    }

    let rows = v.rows;
    let Some(tree) = state.trees.open.get_mut(&client) else {
        return Routed::Consumed;
    };
    for (tag, on) in expanded {
        tree.expanded.insert(tag, on);
    }
    tree.current = current;
    tree.offset = offset;
    tree.tag = v.lines.get(current).map(|l| l.tag);
    if let Some((s, w, _)) = start_tags {
        tree.expanded.insert(s, true);
        tree.expanded.insert(w, true);
    }
    if rebuild_to.is_some() {
        // `mode_tree_build`: the current item found again by its tag.
        let Some(tree) = state.trees.open.get(&client) else {
            return Routed::Consumed;
        };
        let (items, _) = build(state, tree);
        let lines = lines_of(&items, tree);
        let mut at = set_current(&lines, tree.tag, tree.current, tree.offset, tree.height);
        if let Some((_, w, p)) = start_tags {
            let find = |tag| lines.iter().position(|l| l.tag == tag);
            if find(p).is_some() {
                at = set_current(&lines, Some(p), at.0, at.1, tree.height);
            } else if find(w).is_some() {
                at = set_current(&lines, Some(w), at.0, at.1, tree.height);
            }
        }
        let height = tree.list_height(rows, lines.len());
        let tag = lines.get(at.0).map(|l| l.tag);
        let Some(tree) = state.trees.open.get_mut(&client) else {
            return Routed::Consumed;
        };
        (tree.current, tree.offset) = at;
        tree.height = height;
        tree.tag = tag;
        if height > 0 && tree.current > height - 1 {
            tree.offset = tree.offset.max(tree.current - height + 1);
        }
    }
    let preview_offset = match state.trees.open.get_mut(&client) {
        Some(tree) if tree.tag != Some(before) => {
            tree.preview_offset = 0;
            0
        }
        Some(tree) => tree.preview_offset + preview_step,
        None => return Routed::Consumed,
    };
    if preview_step != 0 {
        let clamped = clamp_preview_offset(state, client, preview_offset);
        if let Some(tree) = state.trees.open.get_mut(&client) {
            tree.preview_offset = clamped;
        }
    }
    Routed::Consumed
}

/// The tagged items that are lines now, in line order (tmux counts and
/// visits only the lines it lists).
pub(super) fn tagged_lines(v: &View, tree: &Tree) -> Vec<Tag> {
    v.lines
        .iter()
        .map(|l| l.tag)
        .filter(|t| tree.tagged.contains(t))
        .collect()
}

/// `mode_tree_search_set`'s prompt.
pub(super) fn ask_search(state: &mut State, client: ClientId) -> Routed {
    crate::server::prompt::ask(
        state,
        client,
        "(search) ",
        "",
        "search",
        false,
        None,
        Box::new(move |state, client, text| {
            if let Some(tree) = state.trees.open.get_mut(&client) {
                tree.search = (!text.is_empty()).then(|| Search {
                    text: text.to_string(),
                    icase: text
                        .chars()
                        .all(|c| c == c.to_lowercase().next().unwrap_or(c)),
                    forward: true,
                });
            }
            search_set(state, client);
            Routed::Consumed
        }),
    );
    Routed::Consumed
}

/// Whether the item `tag` matches the search (`window_tree_search`,
/// `window_buffer_search`; a client by name).
pub(super) fn search_matches(state: &State, tag: Tag, item: &Item, search: &Search) -> bool {
    let has = |hay: &str| {
        if search.icase {
            hay.to_lowercase().contains(&search.text.to_lowercase())
        } else {
            hay.contains(&search.text)
        }
    };
    match tag {
        Tag::Session(_) | Tag::Client(_) => has(&item.name),
        Tag::Window(w) => state.model.windows.get(&w).is_some_and(|w| has(&w.name)),
        Tag::Pane(p) => pane_command(state, p).is_some_and(|c| has(&c)),
        Tag::Buffer(id) => state
            .buffers
            .by_id(id)
            .is_some_and(|b| has(&b.name) || has(&String::from_utf8_lossy(&b.data))),
        Tag::Custom(_) => has(&item.name),
    }
}

/// The name of the program running in `pane`, which a pane search reads.
pub(super) fn pane_command(state: &State, pane: PaneId) -> Option<String> {
    let vars = FormatCtx::new(&state.model, pane);
    let text = cmd::expand(
        "#{pane_current_command}",
        &ServerVars::new(state, pane, &vars),
    );
    (!text.is_empty()).then_some(text)
}

/// `mode_tree_search_set`: moves to the next item that matches, searching
/// all items (also those folded away) from the current one on, round the
/// ends; its parents open.
pub(super) fn search_set(state: &mut State, client: ClientId) {
    let Some(tree) = state.trees.open.get(&client) else {
        return;
    };
    let Some(search) = tree.search.clone() else {
        return;
    };
    let (items, _) = build(state, tree);
    // Every item in tree order, with its parents.
    fn flatten<'a>(items: &'a [Item], parents: &mut Vec<Tag>, out: &mut Vec<(&'a Item, Vec<Tag>)>) {
        for item in items {
            out.push((item, parents.clone()));
            parents.push(item.tag);
            flatten(&item.children, parents, out);
            parents.pop();
        }
    }
    let mut all = Vec::new();
    flatten(&items, &mut Vec::new(), &mut all);
    let Some(from) = all.iter().position(|(i, _)| Some(i.tag) == tree.tag) else {
        return;
    };
    let count = all.len();
    let mut found = None;
    for step in 1..count {
        let i = if search.forward {
            (from + step) % count
        } else {
            (from + count - step) % count
        };
        let (item, parents) = &all[i];
        if search_matches(state, item.tag, item, &search) {
            found = Some((item.tag, parents.clone()));
            break;
        }
    }
    let Some((tag, parents)) = found else {
        return;
    };
    if let Some(tree) = state.trees.open.get_mut(&client) {
        for p in parents {
            tree.expanded.insert(p, true);
        }
    }
    rebuild(state, client, Some(tag));
}

/// `x` and `X`: asks `Kill session s? `, `Kill window n? `, `Kill pane n? `
/// or `Kill N tagged? `; `y` kills.
pub(super) fn kill_prompt(
    state: &mut State,
    client: ClientId,
    items: &[(Tag, Option<SessionId>)],
    tagged: bool,
) -> Routed {
    let model = &state.model;
    let label = if tagged {
        format!("Kill {} tagged? ", items.len())
    } else {
        let (tag, session) = items[0];
        match tag {
            Tag::Session(s) => match model.sessions.get(&s) {
                Some(s) => format!("Kill session {}? ", s.name),
                None => return Routed::Consumed,
            },
            Tag::Window(w) => {
                let s = session.or_else(|| model.windows.get(&w).map(|w| w.session));
                match s.and_then(|s| model.index_in(s, w)) {
                    Some(i) => format!("Kill window {i}? "),
                    None => return Routed::Consumed,
                }
            }
            Tag::Pane(p) => {
                let idx = model.panes.get(&p).and_then(|pane| {
                    let w = model.windows.get(&pane.window)?;
                    let at = w.pane_ids().iter().position(|x| *x == p)?;
                    Some(model.pane_base_index(w.id) + at as u32)
                });
                match idx {
                    Some(i) => format!("Kill pane {i}? "),
                    None => return Routed::Consumed,
                }
            }
            _ => return Routed::Consumed,
        }
    };
    let items: Vec<(Tag, Option<SessionId>)> = items.to_vec();
    let kill = move |state: &mut State, _client: ClientId, text: &str| {
        let mut chars = text.chars();
        let yes = matches!(chars.next(), Some(c) if c.eq_ignore_ascii_case(&'y'))
            && chars.next().is_none();
        if !yes {
            return Routed::Consumed;
        }
        let cmds: Vec<Vec<String>> = items
            .iter()
            .filter_map(|(t, s)| {
                let name = target_name(state, *s, *t)?;
                let verb = match t {
                    Tag::Session(_) => "kill-session",
                    Tag::Window(_) => "kill-window",
                    Tag::Pane(_) => "kill-pane",
                    _ => return None,
                };
                Some(vec![verb.to_string(), "-t".to_string(), name])
            })
            .collect();
        Routed::Command(cmds, Dialect::Tmux)
    };
    let yes = state.trees.open.get(&client).is_some_and(|t| t.prompt_yes);
    if yes {
        return kill(state, client, "y");
    }
    crate::server::prompt::ask(
        state,
        client,
        &label,
        "",
        "command",
        true,
        None,
        Box::new(kill),
    );
    Routed::Consumed
}

/// `window_client_do_detach` for each of `tags`.
pub(super) fn detach_clients(
    state: &mut State,
    client: ClientId,
    tags: &[Tag],
    verb: &str,
) -> Routed {
    let cmds: Vec<Vec<String>> = tags
        .iter()
        .filter_map(|t| {
            let Tag::Client(_) = t else {
                return None;
            };
            let name = target_name(state, None, *t)?;
            Some(match verb {
                "detach-client-kill" => {
                    vec!["detach-client".into(), "-P".into(), "-t".into(), name]
                }
                v => vec![v.into(), "-t".into(), name],
            })
        })
        .collect();
    let _ = client;
    Routed::Command(cmds, Dialect::Tmux)
}

/// `window_tree_swap`: with the current line a window, swaps it with the
/// next window line up or down at the same depth under the same session,
/// when the order is the index.
pub(super) fn swap_windows(
    state: &mut State,
    client: ClientId,
    v: &View,
    current: usize,
    direction: i32,
) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    if !matches!(tree.kind, Kind::Tree { .. }) || tree.order != Order::Index {
        return Routed::Consumed;
    }
    let depth = v.lines[current].depth;
    let mut other = current;
    loop {
        let next = other as i64 + i64::from(direction);
        if next < 0 || next as usize >= v.lines.len() {
            return Routed::Consumed;
        }
        other = next as usize;
        if v.lines[other].depth <= depth {
            break;
        }
    }
    if v.lines[other].depth != depth {
        return Routed::Consumed;
    }
    let (a, b) = (&v.lines[current], &v.lines[other]);
    let (Tag::Window(_), Tag::Window(_)) = (a.tag, b.tag) else {
        return Routed::Consumed;
    };
    if a.session != b.session {
        return Routed::Consumed;
    }
    let (Some(src), Some(dst)) = (
        target_name(state, a.session, a.tag),
        target_name(state, b.session, b.tag),
    ) else {
        return Routed::Consumed;
    };
    Routed::Command(
        vec![vec![
            "swap-window".into(),
            "-s".into(),
            src,
            "-t".into(),
            dst,
        ]],
        Dialect::Tmux,
    )
}

/// The chosen line: the chooser closes, then the command of the template
/// runs for its item (`%%`) on the client, as tmux's `mode_tree_run_command`
/// does.
pub(super) fn choose(state: &mut State, client: ClientId, line: usize) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let v = view(state, tree);
    let Some(l) = v.lines.get(line) else {
        return Routed::Consumed;
    };
    let names: Vec<String> = target_name(state, l.session, l.tag).into_iter().collect();
    run_chosen(state, client, names)
}

/// `P`: the command for each of the tagged items.
pub(super) fn choose_many(state: &mut State, client: ClientId, tags: &[Tag]) -> Routed {
    let names: Vec<String> = tags
        .iter()
        .filter_map(|t| target_name(state, None, *t))
        .collect();
    run_chosen(state, client, names)
}

pub(super) fn run_chosen(state: &mut State, client: ClientId, names: Vec<String>) -> Routed {
    let Some(tree) = state.trees.open.get(&client) else {
        return Routed::Consumed;
    };
    let (template, dialect) = match &tree.template {
        Some(t) => (t.clone(), tree.dialect),
        None => (tree.kind.default_template().to_string(), Dialect::Tmux),
    };
    close(state, client);
    let cmds = template_commands(state, client, &template, &names);
    if cmds.is_empty() {
        return Routed::Consumed;
    }
    Routed::Command(cmds, dialect)
}

/// One preview in a strip: the pane shown, the window or pane it stands
/// for (what its label and colour are formats of), and whether it is the
/// current window or active pane.
pub(super) type Preview = (PaneId, Tag, bool);

/// The previews a session or window line shows side by side, and which
/// is its current window or active pane.
pub(super) fn strip_of(state: &State, tag: Tag) -> Option<(Vec<Preview>, usize)> {
    let model = &state.model;
    match tag {
        Tag::Session(s) => {
            let s = model.sessions.get(&s)?;
            let entries: Vec<_> = s
                .windows
                .iter()
                .filter_map(|w| model.windows.get(w))
                .map(|w| (w.active, Tag::Window(w.id), w.id == s.active))
                .collect();
            let current = entries.iter().position(|e| e.2).unwrap_or(0);
            Some((entries, current))
        }
        Tag::Window(w) => {
            let w = model.windows.get(&w)?;
            let entries: Vec<_> = w
                .pane_ids()
                .into_iter()
                .map(|p| (p, Tag::Pane(p), p == w.active))
                .collect();
            let current = entries.iter().position(|e| e.2).unwrap_or(0);
            Some((entries, current))
        }
        Tag::Pane(_) | Tag::Client(_) | Tag::Buffer(_) | Tag::Custom(_) => None,
    }
}

/// `offset` kept inside the previews the current line has, as
/// `window_tree_draw_session` and `_window` clamp `data->offset`.
pub(super) fn clamp_preview_offset(state: &State, client: ClientId, offset: i32) -> i32 {
    let Some(tree) = state.trees.open.get(&client) else {
        return 0;
    };
    let v = view(state, tree);
    let Some(tag) = v.lines.get(v.current).map(|l| l.tag) else {
        return 0;
    };
    let cols = state
        .model
        .panes
        .get(&tree.pane)
        .map_or(0, |p| usize::from(p.cols));
    let Some((entries, current)) = strip_of(state, tag) else {
        return 0;
    };
    strip(entries.len(), current, cols.saturating_sub(4), offset).map_or(0, |g| g.offset)
}

/// How a strip of `total` previews fits `sx` columns, with the current
/// one in view and scrolled by `offset` (clamped): tmux's arithmetic in
/// `window_tree_draw_session`.
pub(super) struct Strip {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) left: bool,
    pub(super) right: bool,
    pub(super) each: usize,
    pub(super) remaining: usize,
    pub(super) offset: i32,
}

pub(super) fn strip(total: usize, current: usize, sx: usize, offset: i32) -> Option<Strip> {
    if total == 0 {
        return None;
    }
    let visible = if sx / total < 24 {
        (sx / 24).max(1)
    } else {
        total
    };
    let visible = visible.min(total);
    let (start, end) = if current < visible {
        (0, visible)
    } else if current >= total - visible {
        (total - visible, total)
    } else {
        let start = current - visible / 2;
        (start, start + visible)
    };
    let offset = offset.clamp(-(start as i32), (total - end) as i32);
    let start = (start as i32 + offset) as usize;
    let end = (end as i32 + offset) as usize;
    let (mut left, mut right) = (start != 0, end != total);
    if (left && right && sx <= 6) || ((left || right) && sx <= 3) {
        left = false;
        right = false;
    }
    let room = match (left, right) {
        (true, true) => sx - 6,
        (true, false) | (false, true) => sx - 3,
        (false, false) => sx,
    };
    let each = room / visible;
    if each == 0 {
        return None;
    }
    Some(Strip {
        start,
        end,
        left,
        right,
        each,
        remaining: room - visible * each,
        offset,
    })
}
