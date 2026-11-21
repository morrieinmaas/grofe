//! The option rows of the settings menu: what each shows and what Enter does.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{Layer, Provenance, Source};
use crate::server::customize::{Scope, set_command};
use crate::server::dispatch::options::{Kind, option_docs};
use crate::server::input::{Key, Routed};
use crate::server::prompt;
use crate::server::{ClientCtx, State, clients, dispatch};
use crate::types::ClientId;

use super::overlay;
use super::sections::{Section, section_of};

/// One listed option. Its value is read from the server each time.
#[derive(Clone)]
pub struct Row {
    pub name: &'static str,
    pub section: Section,
    pub doc: &'static str,
    pub default: &'static str,
    pub kind: Kind,
    /// Where the config sets it, `overrides <file>:<line>` when
    /// settings.toml hides a lower layer's value; empty for the default.
    pub note: String,
}

/// A key or layout row, listed beside the options.
pub struct Entry {
    pub section: Section,
    pub cols: Vec<String>,
    pub act: Act,
}

/// What Enter on an [`Entry`] works on.
pub enum Act {
    Key { table: String, key: Key },
    Layout { name: String, toml: String },
}

/// An option the menu set: its value before the menu first did, whether a
/// runtime `set -g` held it then, and the value the menu last set.
pub struct Edit {
    pub before: String,
    pub runtime: bool,
    pub last: String,
}

fn place(src: &Source, dir: Option<&Path>) -> String {
    let file = src.file.as_deref().unwrap_or(Path::new(""));
    let file = dir.and_then(|d| file.strip_prefix(d).ok()).unwrap_or(file);
    format!("{}:{}", file.display(), src.line)
}

/// Every built-in option by name, with its source as `prov` knows it.
pub fn build(prov: &Provenance, dir: Option<&Path>) -> Vec<Row> {
    let mut rows: Vec<Row> = option_docs()
        .map(|o| {
            let note = match (prov.overridden(o.toml), prov.get(o.toml)) {
                (Some(old), _) => format!("overrides {}", place(old, dir)),
                (None, Some(s)) if s.layer != Layer::Defaults => place(s, dir),
                _ => String::new(),
            };
            Row {
                name: o.name,
                section: section_of(o.name),
                doc: o.doc,
                default: o.default,
                kind: o.kind,
                note,
            }
        })
        .collect();
    rows.sort_by_key(|r| r.name);
    rows
}

/// The server's global value of an option.
pub fn value(state: &State, name: &str) -> String {
    state
        .model
        .options
        .lookup(name, &[])
        .unwrap_or_default()
        .to_string()
}

/// `set-option -g` of `name` to `to`.
pub fn set(state: &State, name: &str, to: &str) -> Vec<String> {
    set_command(state, Scope::GlobalSession, name, None, to)
}

/// Runs `set-option -g` of each of `changes` for `client`, and notes each
/// one that applied as the menu's; a refusal shows the option's own error
/// and changes nothing.
pub fn apply(state: &mut State, client: ClientId, changes: &[(&'static str, &str)]) {
    let Some(ctx) = state.clients.get(&client).map(|c| c.ctx.clone()) else {
        return;
    };
    for &(name, to) in changes {
        let before = value(state, name);
        let runtime = state.model.options.is_runtime(name);
        let argv = set(state, name, to);
        match dispatch::run(state, &ctx, &argv) {
            Ok(_) => {
                let last = value(state, name);
                let edit = Edit {
                    before,
                    runtime,
                    last,
                };
                overlay::note_set(state, client, name, edit);
            }
            Err(e) => clients::show_message(state, client, e.trim_end()),
        }
    }
    clients::mark_dirty(state, client);
}

/// Splits `set` into the options that go back to the config (the first) and
/// the rest, which [`discard`] handles.
pub fn split(
    state: &State,
    set: BTreeMap<&'static str, Edit>,
) -> (BTreeMap<&'static str, Edit>, BTreeMap<&'static str, Edit>) {
    set.into_iter().partition(|(name, edit)| {
        state.model.options.is_runtime(name) && value(state, name) == edit.last && !edit.runtime
    })
}

/// Sets the options of `released` back to their before values, runtime
/// marked, when they could not go back to the config.
pub fn put_back(
    state: &mut State,
    client: ClientId,
    ctx: &ClientCtx,
    released: &BTreeMap<&'static str, Edit>,
) {
    for (name, edit) in released {
        let argv = set(state, name, &edit.before);
        if let Err(e) = dispatch::run(state, ctx, &argv) {
            clients::show_message(state, client, e.trim_end());
        }
    }
}

/// Hands back the options of `set`, as Esc then `y` does, and returns those
/// that go back to the config. One whose live value is no longer what this
/// menu last set is left alone; another open menu that set it over this
/// one's value takes this one's before instead. One a runtime `set -g` held
/// before is set back to that value and stays runtime.
pub fn discard(
    state: &mut State,
    client: ClientId,
    ctx: &ClientCtx,
    set_by_menu: BTreeMap<&'static str, Edit>,
) -> Vec<&'static str> {
    let mut release = Vec::new();
    for (name, edit) in set_by_menu {
        let mine = state.model.options.is_runtime(name) && value(state, name) == edit.last;
        if !mine {
            hand_down(state, name, &edit);
        } else if edit.runtime {
            let argv = set(state, name, &edit.before);
            if let Err(e) = dispatch::run(state, ctx, &argv) {
                clients::show_message(state, client, e.trim_end());
            }
        } else {
            release.push(name);
        }
    }
    release
}

/// Gives `edit`'s before to each open menu that set `name` over the value
/// `edit` last set.
fn hand_down(state: &mut State, name: &str, edit: &Edit) {
    for menu in state.settings_menus.open.values_mut() {
        if let Some(e) = menu.set.get_mut(name)
            && e.runtime
            && e.before == edit.last
        {
            e.before = edit.before.clone();
            e.runtime = edit.runtime;
        }
    }
}

/// What Enter on `row` does: a flag toggles, a choice cycles, the rest ask.
pub fn enter(state: &mut State, client: ClientId, row: &Row) {
    let current = value(state, row.name);
    let next = match row.kind {
        Kind::Bool => Some(if current == "on" { "off" } else { "on" }),
        Kind::Choice(words) if !words.is_empty() => {
            let at = words.iter().position(|w| *w == current).unwrap_or(0);
            Some(words[(at + 1) % words.len()])
        }
        _ => None,
    };
    if let Some(to) = next {
        apply(state, client, &[(row.name, to)]);
        return;
    }
    let name = row.name;
    prompt::ask(
        state,
        client,
        &format!("({name}) "),
        &current,
        "command",
        false,
        Box::new(move |state, client, text| {
            // The menu may have closed (Esc elsewhere, a resize) meanwhile.
            if state.settings_menus.is_open(client) {
                apply(state, client, &[(name, text)]);
            }
            Routed::Consumed
        }),
    );
}
