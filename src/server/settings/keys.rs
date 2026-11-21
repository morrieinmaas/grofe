//! The settings menu's Keys section: bindings listed, rebound and unbound
//! live, and handed back when the menu is cancelled.

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
    /// The binding the menu last left on the key; `None`, unbound.
    pub(super) last: Option<Binding>,
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
            // The menu may have closed (Esc elsewhere, a resize) meanwhile.
            if text == "y" && state.settings_menus.is_open(client) {
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
        last: None,
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
    let mut touched = vec![
        touched(tables, table, old, Some(b.clone()), refusal),
        touched(tables, table, new, was_new, refusal),
    ];
    tables.unbind_runtime(table, &old);
    b.source = BindingSource::Runtime;
    tables.insert(table, new, b.clone());
    touched[1].last = Some(b);
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

/// Splits `touched` into the keys that go back to the config (the first)
/// and the rest, which [`discard`] handles.
pub fn split(state: &State, touched: Vec<Touched>) -> (Vec<Touched>, Vec<Touched>) {
    let tables = &state.registry.key_tables;
    touched.into_iter().partition(|t| {
        let live = tables.exact(&t.table, &t.key);
        let mine = live == t.last.as_ref()
            && (live.is_some() || tables.is_runtime_unbound(&t.table, &t.key));
        let held = t.held_unbound || t.was.as_ref().is_some_and(|b| !b.source.is_config());
        mine && !held
    })
}

/// Puts the keys of `released` back to their before binding, runtime
/// marked, when they could not go back to the config.
pub fn put_back(state: &mut State, released: Vec<Touched>) {
    let tables = &mut state.registry.key_tables;
    for t in released {
        match t.was {
            Some(mut b) => {
                b.source = BindingSource::Runtime;
                tables.insert(&t.table, t.key, b);
            }
            None => tables.unbind_runtime(&t.table, &t.key),
        }
    }
}

/// Hands back the keys of `touched`, as Esc then `y` does, and returns
/// those that go back to the config. A key whose live binding is no longer
/// what this menu left is left alone; another open menu that touched it
/// after this one takes this one's before instead. One the runtime held
/// before (a runtime or plugin binding, a runtime unbind) goes back to that.
pub fn discard(state: &mut State, touched: Vec<Touched>) -> Vec<(String, Key)> {
    let mut release = Vec::new();
    for t in touched {
        let tables = &mut state.registry.key_tables;
        let live = tables.exact(&t.table, &t.key);
        let mine = live == t.last.as_ref()
            && (live.is_some() || tables.is_runtime_unbound(&t.table, &t.key));
        if !mine {
            hand_down(state, &t);
            continue;
        }
        let held = t.held_unbound || t.was.as_ref().is_some_and(|b| !b.source.is_config());
        match (held, t.was) {
            (false, _) => release.push((t.table, t.key)),
            (true, Some(b)) => tables.insert(&t.table, t.key, b),
            (true, None) => tables.unbind_runtime(&t.table, &t.key),
        }
    }
    release
}

/// Gives `t`'s before to each open menu that touched its key over what `t`
/// last left there.
fn hand_down(state: &mut State, t: &Touched) {
    for menu in state.settings_menus.open.values_mut() {
        let after = menu.keys.iter_mut().filter(|k| {
            k.table == t.table
                && k.key == t.key
                && k.was == t.last
                && k.held_unbound == t.last.is_none()
        });
        for k in after {
            k.was = t.was.clone();
            k.held_unbound = t.held_unbound;
        }
    }
}
