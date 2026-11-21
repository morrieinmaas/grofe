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

/// An option as it was before the menu first set it: its value, and
/// whether a runtime `set -g` held it, so a reload left it alone.
pub struct Before {
    pub value: String,
    pub runtime: bool,
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
        let before = Before {
            value: value(state, name),
            runtime: state.model.options.is_runtime(name),
        };
        let argv = set(state, name, to);
        match dispatch::run(state, &ctx, &argv) {
            Ok(_) => overlay::note_set(state, client, name, before),
            Err(e) => clients::show_message(state, client, e.trim_end()),
        }
    }
    clients::mark_dirty(state, client);
}

/// Sets each of `changed` back to its value before the menu set it. One
/// the runtime did not hold then goes back to the config: the next reload
/// applies it again.
pub fn restore(
    state: &mut State,
    client: ClientId,
    ctx: &ClientCtx,
    changed: &BTreeMap<&'static str, Before>,
) {
    for (name, before) in changed {
        let argv = set(state, name, &before.value);
        if let Err(e) = dispatch::run(state, ctx, &argv) {
            clients::show_message(state, client, e.trim_end());
        }
        if !before.runtime {
            state.model.options.release_runtime(name);
        }
    }
    clients::mark_dirty(state, client);
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
            apply(state, client, &[(name, text)]);
            Routed::Consumed
        }),
    );
}
