//! customize-mode (`prefix C`), tmux 3.7b's `window-customize.c`: the
//! options and the key bindings as a tree under [`crate::server::tree`]'s
//! chooser, with what each is on the preview and the keys that change it.
//!
//! This module knows what the tree holds ([`items`]), what an item is
//! ([`Entry`]), what to say about it ([`info`]) and what changing it takes
//! ([`set_commands`], [`unset_commands`]); the chooser draws and drives it.
//! Options come from tmux's table ([`table`], generated from
//! `options-table.c`) for their kind, scope, choices and description, and
//! from the server's options for their values.
// ponytail: tmux's other mouse and menu paths, `d` / `D` (reset) on a key
// (grofe cannot say what a key's default was), and the "The default is:"
// line of a key are not here; hooks are not listed, as tmux does not list
// them either.

mod table;

use std::collections::BTreeMap;

use crate::server::State;
use crate::server::dispatch::options::{self, Owner};
use crate::server::input::{BindingSource, Key};
use crate::types::{PaneId, SessionId, WindowId};

/// What an option holds (tmux's `options_table_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    String,
    Number,
    Flag,
    Choice,
    Colour,
    Key,
    Command,
}

/// One row of tmux's option table.
pub(super) struct OptInfo {
    pub name: &'static str,
    pub kind: Kind,
    /// `SERVER`, `SESSION`, `WINDOW` and `PANE` as bits.
    pub scope: u8,
    /// `ARRAY`, `STYLE` and `HOOK` as bits.
    pub flags: u8,
    pub choices: &'static [&'static str],
    pub unit: &'static str,
    pub text: &'static str,
    pub default: Option<&'static str>,
}

const SERVER: u8 = 1;
const SESSION: u8 = 2;
const WINDOW: u8 = 4;
const PANE: u8 = 8;
const ARRAY: u8 = 1;
const STYLE: u8 = 2;
const HOOK: u8 = 4;

fn info_of(name: &str) -> Option<&'static OptInfo> {
    table::TABLE.iter().find(|o| o.name == name)
}

/// Where an option is set, as tmux's `window_customize_scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Server,
    GlobalSession,
    Session(SessionId),
    GlobalWindow,
    Window(WindowId),
    Pane(PaneId),
}

/// The three rows under a key binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Command,
    Note,
    Repeat,
}

/// What an item of the tree is.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// `Server Options`, `Key Table - x` and the like.
    Group,
    /// An option, or one element of an array option.
    Option {
        scope: Scope,
        name: String,
        idx: Option<u32>,
    },
    Key {
        table: String,
        key: Key,
    },
    Field {
        table: String,
        key: Key,
        field: Field,
    },
}

/// One item of the tree, before the chooser makes lines of it.
pub struct Node {
    pub tag: u64,
    pub name: String,
    /// `None`: nothing after the name (the groups and the rows under a
    /// key).
    pub text: Option<String>,
    pub children: Vec<Node>,
    /// May not be tagged (`mode_tree_no_tag`).
    pub no_tag: bool,
    /// Its preview is its parent's (`mode_tree_draw_as_parent`).
    pub as_parent: bool,
    /// Starts expanded (the rows under a key do, the rest do not).
    pub expanded: bool,
}

/// Where in the server the chooser's pane is: tmux's `cmd_find_state`.
#[derive(Debug, Clone, Copy)]
pub struct Fs {
    pub session: SessionId,
    pub window: WindowId,
    pub pane: PaneId,
}

/// A tag for `id`, a text stable across rebuilds (FNV-1a).
fn tag_of(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Everything [`items`] knows about the tree it built.
pub struct Built {
    pub nodes: Vec<Node>,
    pub entries: BTreeMap<u64, Entry>,
}

/// The variables a `-F` format and a `-f` filter see for one item.
pub struct ItemVars<'a> {
    pub inner: &'a dyn crate::cmd::FormatVars,
    pub vars: Vec<(&'static str, String)>,
}

impl crate::cmd::FormatVars for ItemVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.vars
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
            .or_else(|| self.inner.get(name))
    }
}

/// tmux's `WINDOW_CUSTOMIZE_DEFAULT_FORMAT`.
pub const DEFAULT_FORMAT: &str = "#{?is_option,\
#{?option_is_global,,#[reverse](#{option_scope})#[default] }\
#[ignore]\
#{option_value}#{?option_unit, #{option_unit},},\
#{key}}";

/// An option's value as tmux's `options_to_string` has it.
fn value_of(state: &State, scope: Scope, name: &str, idx: Option<u32>) -> Option<String> {
    let opts = &state.model.options;
    let owners: &[Owner] = match scope {
        Scope::Server => &[Owner::Server, Owner::Global],
        Scope::GlobalSession => &[Owner::Global],
        Scope::Session(s) => &[Owner::Session(s)][..],
        Scope::GlobalWindow => &[Owner::GlobalWindow, Owner::Global],
        Scope::Window(w) => &[Owner::Window(w)][..],
        Scope::Pane(p) => &[Owner::Pane(p)][..],
    };
    for owner in owners {
        if let Some(i) = idx {
            if let Some(v) = opts.list(*owner, name).and_then(|l| l.items.get(&i)) {
                return Some(options::tmux_text(opts, *owner, &format!("{name}[{i}]"), v));
            }
        } else if let Some(e) = opts.get(*owner, name) {
            return Some(options::tmux_text(opts, *owner, name, &e.value));
        }
    }
    None
}

/// The elements of array option `name` at `scope`.
fn array_of(state: &State, scope: Scope, name: &str) -> BTreeMap<u32, String> {
    let owner = owner_of(scope);
    let opts = &state.model.options;
    opts.list(owner, name)
        .map(|l| l.items.clone())
        .or_else(|| {
            matches!(scope, Scope::Server | Scope::GlobalWindow)
                .then(|| opts.list(Owner::Global, name).map(|l| l.items.clone()))
                .flatten()
        })
        .unwrap_or_default()
}

fn owner_of(scope: Scope) -> Owner {
    match scope {
        Scope::Server => Owner::Global,
        Scope::GlobalSession => Owner::Global,
        Scope::Session(s) => Owner::Session(s),
        Scope::GlobalWindow => Owner::Global,
        Scope::Window(w) => Owner::Window(w),
        Scope::Pane(p) => Owner::Pane(p),
    }
}

fn is_global(scope: Scope) -> bool {
    matches!(
        scope,
        Scope::Server | Scope::GlobalSession | Scope::GlobalWindow
    )
}

/// A window's index in the session that first holds it.
fn window_index(state: &State, w: WindowId) -> u32 {
    state.model.windows.get(&w).map_or(0, |w| w.index)
}

/// `window_customize_scope_text`.
fn scope_text(state: &State, scope: Scope) -> String {
    let model = &state.model;
    match scope {
        Scope::Pane(p) => {
            let at = model
                .panes
                .get(&p)
                .and_then(|pane| model.windows.get(&pane.window))
                .and_then(|w| {
                    let ids = w.pane_ids();
                    Some(ids.iter().position(|x| *x == p)? as u32 + model.pane_base_index(w.id))
                });
            format!("pane {}", at.unwrap_or(0))
        }
        Scope::Session(s) => format!(
            "session {}",
            model.sessions.get(&s).map_or("", |s| s.name.as_str())
        ),
        Scope::Window(w) => format!("window {}", window_index(state, w)),
        _ => String::new(),
    }
}

/// Whether the option is set at `owner` (a scalar or a list).
fn is_set(state: &State, owner: Owner, name: &str) -> bool {
    let opts = &state.model.options;
    opts.get(owner, name).is_some() || opts.list(owner, name).is_some()
}

/// The tree of customize-mode for `fs`: the option groups, then a group
/// per key table. `format` and `filter` are the chooser's (in grofe's
/// format language); `vars` is the pane's own variables.
pub fn items(
    state: &State,
    fs: Fs,
    format: &str,
    filter: Option<&str>,
    hide_global: bool,
    vars: &dyn crate::cmd::FormatVars,
) -> Built {
    let mut entries = BTreeMap::new();
    let mut nodes = Vec::new();
    let groups: [(&str, u8, [Option<Scope>; 3]); 3] = [
        ("Server Options", SERVER, [Some(Scope::Server), None, None]),
        (
            "Session Options",
            SESSION,
            [
                Some(Scope::GlobalSession),
                Some(Scope::Session(fs.session)),
                None,
            ],
        ),
        (
            "Window & Pane Options",
            WINDOW,
            [
                Some(Scope::GlobalWindow),
                Some(Scope::Window(fs.window)),
                Some(Scope::Pane(fs.pane)),
            ],
        ),
    ];
    for (title, bit, scopes) in groups {
        let mut kids = Vec::new();
        // User options (`@x`) come first, then the table's by name.
        let mut names: Vec<String> = user_options(state, scopes)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut table_names: Vec<&OptInfo> = table::TABLE
            .iter()
            .filter(|o| o.scope & bit != 0 && o.flags & HOOK == 0)
            .filter(|o| known(state, o))
            .collect();
        table_names.sort_by_key(|o| o.name);
        names.extend(table_names.iter().map(|o| o.name.to_string()));
        for name in names {
            // The most specific scope that sets it.
            let mut scope = scopes[0].unwrap_or(Scope::Server);
            for s in scopes.iter().flatten() {
                if !is_global(*s) && is_set(state, owner_of(*s), &name) {
                    scope = *s;
                }
            }
            if let Some(node) = option_node(
                state,
                scope,
                &name,
                format,
                filter,
                hide_global,
                fs,
                vars,
                &mut entries,
            ) {
                kids.push(node);
            }
        }
        let tag = tag_of(&format!("group:{title}"));
        entries.insert(tag, Entry::Group);
        nodes.push(Node {
            tag,
            name: title.to_string(),
            text: None,
            children: kids,
            no_tag: true,
            as_parent: false,
            expanded: false,
        });
    }
    let tables = &state.registry.key_tables;
    let mut number = 0;
    for table in tables.table_names().take(256) {
        let mut keys = tables.keys(table);
        keys.sort_by_key(options::tmux_key_order);
        if keys.is_empty() {
            continue;
        }
        // A table's tag is its place among the tables (as tmux's is), so a
        // table that takes the place of a removed one inherits its fold.
        let group = format!("table#{number}");
        number += 1;
        let mut kids = Vec::new();
        for key in keys {
            if let Some(node) = key_node(
                state,
                table,
                &group,
                key,
                format,
                filter,
                vars,
                &mut entries,
            ) {
                kids.push(node);
            }
        }
        let tag = tag_of(&group);
        entries.insert(tag, Entry::Group);
        nodes.push(Node {
            tag,
            name: format!("Key Table - {table}"),
            text: None,
            children: kids,
            no_tag: true,
            as_parent: false,
            expanded: false,
        });
    }
    Built { nodes, entries }
}

/// Whether the server has the option at all (grofe lacks some of tmux's).
fn known(state: &State, o: &OptInfo) -> bool {
    let opts = &state.model.options;
    [Owner::Global, Owner::Server, Owner::GlobalWindow]
        .iter()
        .any(|owner| is_set(state, *owner, o.name))
        || opts.lookup(o.name, &[]).is_some()
}

/// The `@` options set at the owners of `scopes`.
fn user_options(state: &State, scopes: [Option<Scope>; 3]) -> Vec<String> {
    let opts = &state.model.options;
    let mut out = Vec::new();
    for s in scopes.iter().flatten() {
        let owners: Vec<Owner> = match s {
            Scope::Server => vec![Owner::Server],
            Scope::GlobalSession => vec![Owner::Global],
            Scope::GlobalWindow => vec![Owner::GlobalWindow],
            other => vec![owner_of(*other)],
        };
        for owner in owners {
            out.extend(
                opts.at(owner)
                    .map(|(n, _)| n.clone())
                    .filter(|n| n.starts_with('@')),
            );
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn option_node(
    state: &State,
    scope: Scope,
    name: &str,
    format: &str,
    filter: Option<&str>,
    hide_global: bool,
    fs: Fs,
    vars: &dyn crate::cmd::FormatVars,
    entries: &mut BTreeMap<u64, Entry>,
) -> Option<Node> {
    let info = info_of(name);
    let array = info.is_some_and(|i| i.flags & ARRAY != 0);
    if hide_global && is_global(scope) {
        return None;
    }
    let value = if array {
        String::new()
    } else {
        value_of(state, scope, name, None)?
    };
    let item_vars = |value: &str, name: &str| ItemVars {
        inner: vars,
        vars: vec![
            ("is_option", "1".to_string()),
            ("is_key", "0".to_string()),
            ("option_name", name.to_string()),
            ("option_is_global", u8::from(is_global(scope)).to_string()),
            ("option_is_array", u8::from(array).to_string()),
            ("option_scope", scope_text(state, scope)),
            ("option_unit", info.map_or("", |i| i.unit).to_string()),
            ("option_value", value.to_string()),
        ],
    };
    if let Some(f) = filter {
        let expanded = crate::cmd::expand(f, &item_vars(&value, name));
        if !crate::server::tree::format_true(&expanded) {
            return None;
        }
    }
    let _ = fs;
    let tag = tag_of(&format!("opt:{scope:?}:{name}"));
    entries.insert(
        tag,
        Entry::Option {
            scope,
            name: name.to_string(),
            idx: None,
        },
    );
    let mut children = Vec::new();
    if array {
        for (idx, v) in array_of(state, scope, name) {
            let element = format!("{name}[{idx}]");
            let text = crate::cmd::expand(format, &item_vars(&v, &element));
            let tag = tag_of(&format!("opt:{scope:?}:{element}"));
            entries.insert(
                tag,
                Entry::Option {
                    scope,
                    name: name.to_string(),
                    idx: Some(idx),
                },
            );
            children.push(Node {
                tag,
                name: element,
                text: Some(text),
                children: Vec::new(),
                no_tag: false,
                as_parent: false,
                expanded: true,
            });
        }
    }
    let text = (!array).then(|| crate::cmd::expand(format, &item_vars(&value, name)));
    Some(Node {
        tag,
        name: name.to_string(),
        text,
        children,
        no_tag: false,
        as_parent: false,
        expanded: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn key_node(
    state: &State,
    table: &str,
    group: &str,
    key: Key,
    format: &str,
    filter: Option<&str>,
    vars: &dyn crate::cmd::FormatVars,
    entries: &mut BTreeMap<u64, Entry>,
) -> Option<Node> {
    let b = state.registry.key_tables.exact(table, &key)?;
    let key_name = key.to_string();
    let mut item_vars = vec![
        ("is_option", "0".to_string()),
        ("is_key", "1".to_string()),
        ("key", key_name.clone()),
    ];
    if let Some(note) = &b.desc {
        item_vars.push(("key_note", note.clone()));
    }
    let iv = ItemVars {
        inner: vars,
        vars: item_vars,
    };
    if let Some(f) = filter {
        let expanded = crate::cmd::expand(f, &iv);
        if !crate::server::tree::format_true(&expanded) {
            return None;
        }
    }
    let name = crate::cmd::expand(format, &iv);
    let tag = tag_of(&format!("key:{table}:{key_name}"));
    let base = format!("{group}:{key_name}");
    entries.insert(
        tag,
        Entry::Key {
            table: table.to_string(),
            key,
        },
    );
    let command = b
        .shown
        .clone()
        .unwrap_or_else(|| options::tmux_command_line(&b.cmds));
    let mut row = |field: Field, name: &str, text: String| {
        let tag = tag_of(&format!("{base}:{name}"));
        entries.insert(
            tag,
            Entry::Field {
                table: table.to_string(),
                key,
                field,
            },
        );
        Node {
            tag,
            name: name.to_string(),
            text: Some(text),
            children: Vec::new(),
            no_tag: true,
            as_parent: true,
            expanded: true,
        }
    };
    let children = vec![
        row(Field::Command, "Command", format!("#[ignore]{command}")),
        row(
            Field::Note,
            "Note",
            b.desc
                .as_ref()
                .map(|n| format!("#[ignore]{n}"))
                .unwrap_or_default(),
        ),
        row(
            Field::Repeat,
            "Repeat",
            if b.repeat { "on" } else { "off" }.to_string(),
        ),
    ];
    Some(Node {
        tag,
        name,
        text: None,
        children,
        no_tag: false,
        as_parent: false,
        expanded: false,
    })
}

// --- What an item says ------------------------------------------------

/// A block of text laid out as `screen_write_text` does: words wrapped at
/// `width`, a call ending on the next line unless `more`, and nothing past
/// `height` lines.
pub struct Page {
    width: usize,
    height: usize,
    pub lines: Vec<Vec<(String, Option<crate::server::status::Style>)>>,
    col: usize,
}

impl Page {
    fn new(width: usize, height: usize) -> Page {
        Page {
            width,
            height,
            lines: vec![Vec::new()],
            col: 0,
        }
    }

    /// Whether the page is full.
    fn full(&self) -> bool {
        self.lines.len() > self.height
    }

    /// Writes `text`; `false` when it did not all fit.
    fn text(
        &mut self,
        text: &str,
        more: bool,
        style: Option<crate::server::status::Style>,
    ) -> bool {
        for (n, word) in text.split(' ').enumerate() {
            let w = unicode_width::UnicodeWidthStr::width(word);
            // Words of one call are a space apart; a call goes on where
            // the last one stopped.
            let lead = usize::from(n > 0);
            if self.col != 0 && self.col + lead + w > self.width {
                self.lines.push(Vec::new());
                self.col = 0;
            }
            if self.full() {
                return false;
            }
            let piece = if n > 0 && self.col != 0 {
                format!(" {word}")
            } else {
                word.to_string()
            };
            self.col += unicode_width::UnicodeWidthStr::width(piece.as_str());
            if let Some(line) = self.lines.last_mut() {
                line.push((piece, style));
            }
        }
        if !more {
            self.lines.push(Vec::new());
            self.col = 0;
        }
        true
    }

    /// `screen_write_cursormove(ctx, cx, s->cy + 1)`: skips a line.
    fn skip(&mut self) {
        self.lines.push(Vec::new());
        self.col = 0;
    }

    /// The cursor is on the last line the page may use.
    fn at_end(&self) -> bool {
        self.lines.len() >= self.height
    }
}

/// The preview of `entry`: `window_customize_draw` into a box `width` by
/// `height`.
pub fn info(
    state: &State,
    fs: Fs,
    entry: &Entry,
    width: usize,
    height: usize,
    vars: &dyn crate::cmd::FormatVars,
) -> Page {
    let mut page = Page::new(width.max(1), height);
    match entry {
        Entry::Key { table, key } | Entry::Field { table, key, .. } => {
            draw_key(state, table, key, &mut page);
        }
        Entry::Option { scope, name, idx } => {
            draw_option(state, fs, *scope, name, *idx, vars, &mut page)
        }
        Entry::Group => {}
    }
    page
}

fn draw_key(state: &State, table: &str, key: &Key, page: &mut Page) {
    let Some(b) = state.registry.key_tables.exact(table, key) else {
        return;
    };
    let note = b
        .desc
        .clone()
        .unwrap_or_else(|| "There is no note for this key.".to_string());
    let period = if !note.is_empty() && !note.ends_with('.') {
        "."
    } else {
        ""
    };
    if !page.text(&format!("{note}{period}"), false, None) {
        return;
    }
    page.skip();
    if page.at_end() {
        return;
    }
    if !page.text(&format!("This key is in the {table} table."), false, None)
        || !page.text(
            &format!(
                "This key {} repeat.",
                if b.repeat { "does" } else { "does not" }
            ),
            false,
            None,
        )
    {
        return;
    }
    page.skip();
    if page.at_end() {
        return;
    }
    let command = b
        .shown
        .clone()
        .unwrap_or_else(|| options::tmux_command_line(&b.cmds));
    page.text(&format!("Command: {command}"), false, None);
}

fn draw_option(
    state: &State,
    fs: Fs,
    scope: Scope,
    name: &str,
    idx: Option<u32>,
    vars: &dyn crate::cmd::FormatVars,
    page: &mut Page,
) {
    let info = info_of(name);
    let (space, unit) = match info {
        Some(i) if !i.unit.is_empty() => (" ", i.unit),
        _ => ("", ""),
    };
    let text = info
        .map(|i| i.text)
        .filter(|t| !t.is_empty())
        .unwrap_or("This option doesn't have a description.");
    if !page.text(text, false, None) {
        return;
    }
    page.skip();
    if page.at_end() {
        return;
    }
    let kind_text = match info {
        None => "user",
        Some(i) if i.scope & (WINDOW | PANE) == WINDOW | PANE => "window and pane",
        Some(i) if i.scope & WINDOW != 0 => "window",
        Some(i) if i.scope & SESSION != 0 => "session",
        Some(_) => "server",
    };
    if !page.text(&format!("This is a {kind_text} option."), false, None) {
        return;
    }
    let array = info.is_some_and(|i| i.flags & ARRAY != 0);
    if array {
        let ok = match idx {
            Some(i) => page.text(&format!("This is an array option, index {i}."), false, None),
            None => page.text("This is an array option.", false, None),
        };
        if !ok || idx.is_none() {
            return;
        }
    }
    page.skip();
    if page.at_end() {
        return;
    }
    let Some(value) = value_of(state, scope, name, idx) else {
        return;
    };
    let default = info
        .and_then(|i| i.default)
        .filter(|_| idx.is_none() && !array)
        .filter(|d| *d != value);
    if !page.text(&format!("Option value: {value}{space}{unit}"), false, None) {
        return;
    }
    if info.is_none_or(|i| i.kind == Kind::String) {
        let expanded = crate::cmd::expand(&value, vars);
        if expanded != value && !page.text(&format!("This expands to: {expanded}"), false, None) {
            return;
        }
    }
    if let Some(i) = info
        && i.kind == Kind::Choice
        && !page.text(
            &format!("Available values are: {}", i.choices.join(", ")),
            false,
            None,
        )
    {
        return;
    }
    if info.is_some_and(|i| i.kind == Kind::Colour) {
        let style = crate::server::status::parse_style(&format!("fg={value}"));
        if !page.text("This is a colour option: ", true, None)
            || !page.text("EXAMPLE", false, Some(style))
        {
            return;
        }
    }
    if info.is_some_and(|i| i.flags & STYLE != 0) {
        let style = crate::server::status::base_style(&value, vars);
        if !page.text("This is a style option: ", true, None)
            || !page.text("EXAMPLE", false, Some(style))
        {
            return;
        }
    }
    if let Some(d) = default
        && !page.text(&format!("The default is: {d}{space}{unit}"), false, None)
    {
        return;
    }
    page.skip();
    if page.full() || array {
        return;
    }
    // Where the value would come from if this one were unset.
    let parents: Vec<(Scope, &str)> = match scope {
        Scope::Pane(_) => vec![
            (Scope::Window(fs.window), "Window"),
            (Scope::GlobalWindow, "Global"),
        ],
        Scope::Window(_) => vec![(Scope::GlobalWindow, "Global")],
        Scope::Session(_) => vec![(Scope::GlobalSession, "Global")],
        _ => Vec::new(),
    };
    for (parent, label) in parents {
        if parent == scope {
            continue;
        }
        let set = match parent {
            Scope::GlobalWindow | Scope::GlobalSession | Scope::Server => true,
            other => is_set(state, owner_of(other), name),
        };
        if !set {
            continue;
        }
        if let Some(v) = value_of(state, parent, name, None) {
            let text = if label == "Window" {
                format!(
                    "Window value (from window {}): {v}{space}{unit}",
                    window_index(state, fs.window)
                )
            } else {
                format!("Global value: {v}{space}{unit}")
            };
            if !page.text(&text, false, None) {
                return;
            }
        }
    }
}

// --- Changing things --------------------------------------------------

/// The `set-option` flags and target that reach `scope`.
fn scope_args(state: &State, scope: Scope) -> Vec<String> {
    let model = &state.model;
    match scope {
        Scope::Server => vec!["-s".into()],
        Scope::GlobalSession => vec!["-g".into()],
        Scope::Session(s) => vec![
            "-t".into(),
            format!(
                "={}:",
                model.sessions.get(&s).map_or("", |s| s.name.as_str())
            ),
        ],
        Scope::GlobalWindow => vec!["-gw".into()],
        Scope::Window(w) => {
            let s = model.windows.get(&w).map(|w| w.session);
            let name = s
                .and_then(|s| model.sessions.get(&s))
                .map_or(String::new(), |s| s.name.clone());
            vec![
                "-w".into(),
                "-t".into(),
                format!("={name}:{}.", window_index(state, w)),
            ]
        }
        Scope::Pane(p) => vec!["-p".into(), "-t".into(), format!("%{}", p.0)],
    }
}

/// `set-option` for `scope`'s `name` (an element `idx` of an array) to
/// `value`.
pub fn set_command(
    state: &State,
    scope: Scope,
    name: &str,
    idx: Option<u32>,
    value: &str,
) -> Vec<String> {
    let mut argv = vec!["set-option".to_string()];
    argv.extend(scope_args(state, scope));
    argv.push(match idx {
        Some(i) => format!("{name}[{i}]"),
        None => name.to_string(),
    });
    argv.push(value.to_string());
    argv
}

/// The commands that remove `name` (or element `idx`) at `scope`, and at
/// every scope above it when `all` (a reset).
pub fn unset_commands(
    state: &State,
    fs: Fs,
    scope: Scope,
    name: &str,
    idx: Option<u32>,
    all: bool,
) -> Vec<Vec<String>> {
    let chain: Vec<Scope> = if all {
        match scope {
            Scope::Pane(_) => vec![scope, Scope::Window(fs.window), Scope::GlobalWindow],
            Scope::Window(_) => vec![scope, Scope::GlobalWindow],
            Scope::Session(_) => vec![scope, Scope::GlobalSession],
            other => vec![other],
        }
    } else {
        vec![scope]
    };
    chain
        .into_iter()
        .map(|s| {
            let mut argv = vec!["set-option".to_string(), "-u".to_string()];
            argv.extend(scope_args(state, s));
            argv.push(match idx {
                Some(i) => format!("{name}[{i}]"),
                None => name.to_string(),
            });
            argv
        })
        .collect()
}

/// What `s` and Enter (`global` 0, `pane` 1), `w` (0, 0) and `S`, `W`
/// (1, 0) set (`window_customize_set_option`): the scope to set at.
pub fn target_scope(entry_scope: Scope, fs: Fs, array: bool, global: bool, pane: bool) -> Scope {
    if array {
        return entry_scope;
    }
    if global {
        return match entry_scope {
            Scope::Session(_) => Scope::GlobalSession,
            Scope::Window(_) | Scope::Pane(_) => Scope::GlobalWindow,
            other => other,
        };
    }
    match entry_scope {
        Scope::Window(_) | Scope::Pane(_) | Scope::GlobalWindow => {
            if pane {
                Scope::Pane(fs.pane)
            } else {
                Scope::Window(fs.window)
            }
        }
        Scope::GlobalSession => Scope::Session(fs.session),
        other => other,
    }
}

/// What setting an option asks: nothing more (a flag or a choice, one
/// command), or the user for a value.
pub enum Set {
    Command(Vec<String>),
    Prompt {
        label: String,
        initial: String,
        scope: Scope,
        idx: Option<u32>,
    },
}

/// `window_customize_set_option` for `name` seen at `entry_scope`.
pub fn set(
    state: &State,
    fs: Fs,
    entry_scope: Scope,
    name: &str,
    idx: Option<u32>,
    global: bool,
    pane: bool,
) -> Option<Set> {
    let info = info_of(name);
    let array = info.is_some_and(|i| i.flags & ARRAY != 0);
    let pane = pane && info.is_none_or(|i| i.scope & PANE != 0);
    let scope = target_scope(entry_scope, fs, array, global, pane);
    // A new element starts from all of them (`options_to_string` with -1).
    let current = if array && idx.is_none() {
        let mut all = Vec::new();
        for i in array_of(state, scope, name).keys() {
            all.extend(value_of(state, scope, name, Some(*i)));
        }
        all.join(" ")
    } else {
        value_of(state, scope, name, idx)
            .or_else(|| value_of(state, entry_scope, name, idx))
            .unwrap_or_default()
    };
    if let Some(i) = info {
        match i.kind {
            Kind::Flag => {
                let on = current == "on" || current == "1" || current == "yes";
                return Some(Set::Command(set_command(
                    state,
                    scope,
                    name,
                    idx,
                    if on { "off" } else { "on" },
                )));
            }
            Kind::Choice if !i.choices.is_empty() => {
                let at = i.choices.iter().position(|c| *c == current).unwrap_or(0);
                let next = i.choices[(at + 1) % i.choices.len()];
                return Some(Set::Command(set_command(state, scope, name, idx, next)));
            }
            _ => {}
        }
    }
    let text = scope_text(state, scope);
    let space = if !text.is_empty() {
        ", for "
    } else if scope != Scope::Server {
        ", global"
    } else {
        ""
    };
    let label = if array {
        match idx {
            None => format!("({name}[+]{space}{text}) "),
            Some(i) => format!("({name}[{i}]{space}{text}) "),
        }
    } else {
        format!("({name}{space}{text}) ")
    };
    let idx = if array && idx.is_none() {
        let used = array_of(state, scope, name);
        Some((0..).find(|i| !used.contains_key(i)).unwrap_or(0))
    } else {
        idx
    };
    Some(Set::Prompt {
        label,
        initial: current,
        scope,
        idx,
    })
}

/// The bound command text for a key, as the Command row edits it.
pub fn key_command_text(state: &State, table: &str, key: &Key) -> String {
    state
        .registry
        .key_tables
        .exact(table, key)
        .map(|b| {
            b.shown
                .clone()
                .unwrap_or_else(|| options::tmux_command_line(&b.cmds))
        })
        .unwrap_or_default()
}

pub fn key_note_text(state: &State, table: &str, key: &Key) -> String {
    state
        .registry
        .key_tables
        .exact(table, key)
        .and_then(|b| b.desc.clone())
        .unwrap_or_default()
}

/// Replaces what `key` runs with `commands` (as tmux parsed from `text`).
pub fn set_key_commands(state: &mut State, table: &str, key: &Key, commands: Vec<Vec<String>>) {
    if let Some(b) = state.registry.key_tables.exact(table, key).cloned() {
        let mut b = b;
        b.cmds = commands;
        b.shown = None;
        b.dialect = crate::protocol::Dialect::Tmux;
        if b.source == BindingSource::Default {
            b.source = BindingSource::Default;
        }
        state.registry.key_tables.insert(table, *key, b);
    }
}

pub fn set_key_note(state: &mut State, table: &str, key: &Key, note: &str) {
    if let Some(mut b) = state.registry.key_tables.exact(table, key).cloned() {
        b.desc = Some(note.to_string());
        state.registry.key_tables.insert(table, *key, b);
    }
}

pub fn toggle_key_repeat(state: &mut State, table: &str, key: &Key) {
    if let Some(mut b) = state.registry.key_tables.exact(table, key).cloned() {
        b.repeat = !b.repeat;
        state.registry.key_tables.insert(table, *key, b);
    }
}

pub fn unbind_key(state: &mut State, table: &str, key: &Key) {
    state.registry.key_tables.unbind_runtime(table, key);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> Fs {
        Fs {
            session: SessionId(1),
            window: WindowId(2),
            pane: PaneId(3),
        }
    }

    #[test]
    fn the_table_knows_tmux_s_options() {
        let status_keys = info_of("status-keys").expect("status-keys");
        assert_eq!(status_keys.kind, Kind::Choice);
        assert_eq!(status_keys.choices, ["emacs", "vi"]);
        assert_eq!(status_keys.default, Some("emacs"));
        let command_alias = info_of("command-alias").expect("command-alias");
        assert_ne!(command_alias.flags & ARRAY, 0);
        assert!(table::TABLE.len() > 120);
    }

    #[test]
    fn tags_are_stable_and_distinct() {
        assert_eq!(tag_of("group:a"), tag_of("group:a"));
        assert_ne!(tag_of("group:a"), tag_of("group:b"));
    }

    /// `s` sets where the item is (a window's, then the pane's with
    /// `pane`), `S` the global one, as `window_customize_set_option`.
    #[test]
    fn the_scope_to_set_follows_the_keys() {
        let f = fs();
        assert_eq!(
            target_scope(Scope::GlobalSession, f, false, false, true),
            Scope::Session(SessionId(1))
        );
        assert_eq!(
            target_scope(Scope::GlobalWindow, f, false, false, true),
            Scope::Pane(PaneId(3))
        );
        assert_eq!(
            target_scope(Scope::GlobalWindow, f, false, false, false),
            Scope::Window(WindowId(2))
        );
        assert_eq!(
            target_scope(Scope::Pane(PaneId(3)), f, false, true, false),
            Scope::GlobalWindow
        );
        // An array option is set where it is.
        assert_eq!(
            target_scope(Scope::Server, f, true, true, true),
            Scope::Server
        );
    }

    /// A call goes on where the last stopped, with no extra space, and
    /// words wrap at the width.
    #[test]
    fn text_wraps_by_words_and_calls_join() {
        let mut page = Page::new(10, 5);
        assert!(page.text("This is: ", true, None));
        assert!(page.text("EXAMPLE", false, None));
        let rows: Vec<String> = page
            .lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        assert_eq!(rows[0], "This is: ");
        assert_eq!(rows[1], "EXAMPLE");
        let mut page = Page::new(8, 5);
        page.text("one two three", false, None);
        let rows: Vec<String> = page
            .lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        assert_eq!(rows[..2], ["one two", "three"]);
    }
}
