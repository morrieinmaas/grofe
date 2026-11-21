//! The settings menu's Keys section: bindings listed, rebound and unbound
//! live, and put back when the menu is cancelled.

use crate::server::State;
use crate::server::input::{Binding, BindingSource, Key, KeyCode, Routed};
use crate::server::prompt;
use crate::types::ClientId;
use crate::util::quote;

use super::overlay;
use super::rows::{Act, Entry};
use super::sections::Section;

/// The tables the menu lists.
const TABLES: [&str; 2] = ["prefix", "root"];

/// One rebind or unbind: what each key held before, and what settings.toml
/// needs to say to keep it (table, key, command; an empty command unbinds).
#[derive(Default)]
pub struct Edit {
    undo: Vec<(String, Key, Option<Binding>)>,
    pub binds: Vec<(String, String, String)>,
}

fn command(b: &Binding) -> String {
    let argv = |argv: &Vec<String>| {
        let words: Vec<String> = argv.iter().map(|a| quote::tmux_arg(a)).collect();
        words.join(" ")
    };
    let cmds: Vec<String> = b.cmds.iter().map(argv).collect();
    cmds.join(" ; ")
}

fn source(b: &Binding) -> String {
    match &b.source {
        BindingSource::Default => "default".to_string(),
        BindingSource::File { path, line } => format!("{}:{line}", path.display()),
        BindingSource::Runtime => "runtime".to_string(),
        BindingSource::Plugin(name) => format!("plugin {name}"),
    }
}

/// A row per binding: table, key, command, source.
pub fn entries(state: &State) -> Vec<Entry> {
    let tables = &state.registry.key_tables;
    let mut out = Vec::new();
    for table in TABLES {
        for key in tables.keys(table) {
            let Some(b) = tables.exact(table, &key).filter(|b| !b.cmds.is_empty()) else {
                continue;
            };
            out.push(Entry {
                section: Section::Keys,
                cols: vec![table.to_string(), key.to_string(), command(b), source(b)],
                act: Act::Key {
                    table: table.to_string(),
                    key,
                },
            });
        }
    }
    out
}

/// The key pressed after Enter on `old` in `table`: it takes over the
/// binding, after a `y` when it replaces another.
pub fn pressed(state: &mut State, client: ClientId, table: String, old: Key, new: Key) {
    let plain = !new.mods.ctrl && !new.mods.alt;
    if (new.code == KeyCode::Escape && plain) || new == old {
        return;
    }
    let Some(taken) = state.registry.key_tables.exact(&table, &new) else {
        rebind(state, client, &table, old, new);
        return;
    };
    let label = format!("replaces {} (y/n) ", command(taken));
    prompt::ask(
        state,
        client,
        &label,
        "",
        "command",
        true,
        Box::new(move |state, client, text| {
            if text == "y" {
                rebind(state, client, &table, old, new);
            }
            Routed::Consumed
        }),
    );
}

fn rebind(state: &mut State, client: ClientId, table: &str, old: Key, new: Key) {
    let tables = &mut state.registry.key_tables;
    let Some(mut b) = tables.exact(table, &old).cloned() else {
        return;
    };
    let cmd = command(&b);
    let edit = Edit {
        undo: vec![
            (table.to_string(), new, tables.exact(table, &new).cloned()),
            (table.to_string(), old, Some(b.clone())),
        ],
        binds: vec![
            (table.to_string(), old.to_string(), String::new()),
            (table.to_string(), new.to_string(), cmd),
        ],
    };
    tables.unbind(table, &old);
    b.source = BindingSource::Runtime;
    tables.insert(table, new, b);
    overlay::record(state, client, edit);
}

/// Unbinds `key` in `table`.
pub fn unbind(state: &mut State, client: ClientId, table: &str, key: Key) {
    let tables = &mut state.registry.key_tables;
    let Some(b) = tables.exact(table, &key).cloned() else {
        return;
    };
    let edit = Edit {
        undo: vec![(table.to_string(), key, Some(b))],
        binds: vec![(table.to_string(), key.to_string(), String::new())],
    };
    tables.unbind(table, &key);
    overlay::record(state, client, edit);
}

/// Puts every key back as it was before `edits`.
pub fn restore(state: &mut State, edits: &[Edit]) {
    let tables = &mut state.registry.key_tables;
    for (table, key, was) in edits.iter().rev().flat_map(|e| e.undo.iter().rev()) {
        match was {
            Some(b) => tables.insert(table, *key, b.clone()),
            None => {
                tables.unbind(table, key);
            }
        }
    }
}
