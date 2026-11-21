//! The settings menu's Keys section: bindings listed, rebound and unbound
//! live, and put back when the menu is cancelled.

use crate::server::input::{Binding, BindingSource, Key, KeyCode, KeyTables, Routed};
use crate::server::prompt;
use crate::server::{State, clients};
use crate::types::ClientId;
use crate::util::quote;

use super::overlay;
use super::rows::{Act, Entry};
use super::sections::Section;

/// The tables the menu lists.
const TABLES: [&str; 2] = ["prefix", "root"];

/// A key a rebind or unbind touched, as it was before the menu first
/// touched it, and why settings.toml cannot say what it holds now (its
/// binding was a plugin's or not grofe-dialect), if it cannot.
pub struct Touched {
    pub table: String,
    pub key: Key,
    was: Option<Binding>,
    /// A runtime `unbind-key` held the key unbound before the menu came.
    held_unbound: bool,
    pub refusal: Option<&'static str>,
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

fn touched(
    tables: &KeyTables,
    table: &str,
    key: Key,
    was: Option<Binding>,
    refusal: Option<&'static str>,
) -> Touched {
    Touched {
        table: table.to_string(),
        key,
        was,
        held_unbound: tables.is_runtime_unbound(table, &key),
        refusal,
    }
}

/// Moves the binding of `old` to `new`. `old` stays unbound over a reload.
fn rebind(state: &mut State, client: ClientId, table: &str, old: Key, new: Key) {
    let refusal = overlay::refusal(state, client, table, &old);
    let tables = &mut state.registry.key_tables;
    let Some(mut b) = tables.exact(table, &old).cloned() else {
        clients::show_message(state, client, "no longer bound");
        return;
    };
    let was_new = tables.exact(table, &new).cloned();
    let touched = vec![
        touched(tables, table, old, Some(b.clone()), refusal),
        touched(tables, table, new, was_new, refusal),
    ];
    tables.unbind_runtime(table, &old);
    b.source = BindingSource::Runtime;
    tables.insert(table, new, b);
    overlay::record(state, client, touched);
}

/// Unbinds `key` in `table`, also over a reload.
pub fn unbind(state: &mut State, client: ClientId, table: &str, key: Key) {
    let refusal = overlay::refusal(state, client, table, &key);
    let tables = &mut state.registry.key_tables;
    let Some(b) = tables.exact(table, &key).cloned() else {
        clients::show_message(state, client, "no longer bound");
        return;
    };
    let touched = touched(tables, table, key, Some(b), refusal);
    tables.unbind_runtime(table, &key);
    overlay::record(state, client, vec![touched]);
}

/// Puts every key back as it was before the menu touched it; a key that
/// was bound is no longer held unbound over a reload.
pub fn restore(state: &mut State, touched: &[Touched]) {
    let tables = &mut state.registry.key_tables;
    for t in touched {
        match &t.was {
            Some(b) => {
                tables.release_runtime(&t.table, &t.key);
                tables.insert(&t.table, t.key, b.clone());
            }
            None if t.held_unbound => tables.unbind_runtime(&t.table, &t.key),
            None => {
                tables.unbind(&t.table, &t.key);
            }
        }
    }
}
