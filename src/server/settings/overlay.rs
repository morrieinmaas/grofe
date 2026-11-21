//! The settings menu overlay: the options in the list widget, one tab per
//! section. Every change applies at once; Esc asks before it puts back what
//! this menu changed. `C-s` and the Keys and Layouts rows are the apply,
//! keys and layouts tasks'.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::config::{self, Env};
use crate::server::customize;
use crate::server::input::{Input, InputHook, Key, KeyCode, Routed};
use crate::server::listdraw::{self, Rows};
use crate::server::listview::{self, ListAction, ListState};
use crate::server::prompt;
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::{State, clients};
use crate::types::{ClientId, Rect};

use super::apply;
use super::keys::{self, Touched};
use super::layouts;
use super::rows::{self, Act, Before, Entry, Row};
use super::sections::Section;

/// The smallest client the menu is drawn on.
const MIN_SIZE: (u16, u16) = (50, 10);

/// Open menus, per client. Lives on `State.settings_menus`.
#[derive(Default)]
pub struct Menus {
    pub(super) open: BTreeMap<ClientId, Menu>,
}

impl Menus {
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

pub(super) struct Menu {
    ui: ListState,
    /// Index into [`Section::ALL`].
    tab: usize,
    rows: Vec<Row>,
    /// Key rows, then layout rows; listed before the options.
    extra: Vec<Entry>,
    /// How many of `extra` are key rows.
    nkeys: usize,
    /// The options this menu set, as they were before it first did. Another
    /// client's change or a reload is not the menu's.
    pub(super) set: BTreeMap<&'static str, Before>,
    /// The keys this menu rebound or unbound, as they were before.
    pub(super) keys: Vec<Touched>,
    /// The key row waiting for the key to bind it to.
    pending: Option<(String, Key)>,
    /// The settings.toml diff C-s is asking about.
    pub(super) diff: Option<Vec<String>>,
}

fn too_small(size: (u16, u16)) -> bool {
    size.0 < MIN_SIZE.0 || size.1 < MIN_SIZE.1
}

/// Opens the menu on `client`, or says the client is too small. A client
/// with a menu open keeps it.
pub fn open(state: &mut State, client: ClientId) -> Result<(), String> {
    if state.settings_menus.is_open(client) {
        return Ok(());
    }
    let size = state.clients.get(&client).ok_or("no client")?.size;
    if too_small(size) {
        clients::show_message(state, client, "window too small");
        return Ok(());
    }
    let env = Env::from_process();
    let prov = config::load(&env).map(|r| r.provenance).unwrap_or_default();
    let rows = rows::build(&prov, env.config_dir.as_deref());
    let mut extra = keys::entries(state);
    let nkeys = extra.len();
    extra.extend(layouts::entries());
    state.settings_menus.open.insert(
        client,
        Menu {
            ui: ListState::default(),
            tab: 0,
            rows,
            extra,
            nkeys,
            set: BTreeMap::new(),
            keys: Vec::new(),
            pending: None,
            diff: None,
        },
    );
    clients::mark_dirty(state, client);
    Ok(())
}

pub(super) fn close(state: &mut State, client: ClientId) {
    state.settings_menus.open.remove(&client);
    clients::mark_dirty(state, client);
}

/// A client that detached takes its menu with it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.settings_menus.open.remove(&id);
}

/// A client resized below [`MIN_SIZE`] loses its menu, and is told why.
pub fn resized(state: &mut State, id: ClientId, size: (u16, u16)) {
    if state.settings_menus.is_open(id) && too_small(size) {
        close(state, id);
        clients::show_message(state, id, "window too small");
    }
}

/// Notes that this menu set `name`; the first time, what it was before.
pub(super) fn note_set(state: &mut State, client: ClientId, name: &'static str, before: Before) {
    if let Some(menu) = state.settings_menus.open.get_mut(&client) {
        menu.set.entry(name).or_insert(before);
    }
}

/// Notes the keys a rebind or unbind touched and lists the keys again.
pub(super) fn record(state: &mut State, client: ClientId, touched: Vec<Touched>) {
    let fresh = keys::entries(state);
    if let Some(menu) = state.settings_menus.open.get_mut(&client) {
        let n = fresh.len();
        menu.extra.splice(..menu.nkeys, fresh);
        menu.nkeys = n;
        for t in touched {
            if !menu
                .keys
                .iter()
                .any(|k| k.table == t.table && k.key == t.key)
            {
                menu.keys.push(t);
            }
        }
    }
    clients::mark_dirty(state, client);
}

/// Why a key the menu rebound or unbound cannot be saved, if it cannot: its
/// live binding, or what the menu found about it when it first touched it.
pub(super) fn refusal(
    state: &State,
    client: ClientId,
    table: &str,
    key: &Key,
) -> Option<&'static str> {
    customize::binding_refusal(state, table, key).or_else(|| {
        let menu = state.settings_menus.open.get(&client)?;
        let t = menu
            .keys
            .iter()
            .find(|t| t.table == table && t.key == *key)?;
        t.refusal
    })
}

fn values(state: &State, menu: &Menu) -> Vec<String> {
    menu.rows
        .iter()
        .map(|r| rows::value(state, r.name))
        .collect()
}

/// Rows the list shows, best first: the tab's rows, or with a query every
/// row that matches on name, value or description.
fn visible(menu: &Menu, values: &[String]) -> Vec<usize> {
    let section = Section::ALL[menu.tab];
    let query = &menu.ui.query;
    let nx = menu.extra.len();
    let section_of = |i: usize| match menu.extra.get(i) {
        Some(e) => e.section,
        None => menu.rows[i - nx].section,
    };
    let pool: Vec<usize> = (0..nx + menu.rows.len())
        .filter(|&i| !query.is_empty() || section_of(i) == section)
        .collect();
    let texts: Vec<String> = pool
        .iter()
        .map(|&i| match menu.extra.get(i) {
            Some(e) => e.cols.join(" "),
            None => format!(
                "{} {} {}",
                menu.rows[i - nx].name,
                values[i - nx],
                menu.rows[i - nx].doc
            ),
        })
        .collect();
    listview::filter(&texts, query)
        .into_iter()
        .map(|p| pool[p])
        .collect()
}

fn list_area(size: (u16, u16)) -> Rect {
    listdraw::centred(size, 90, 80)
}

fn list_height(size: (u16, u16)) -> usize {
    usize::from(list_area(size).rows.saturating_sub(3))
}

/// Esc: close, or ask first when this menu changed something.
fn escape(state: &mut State, client: ClientId) {
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    let n = menu.set.len() + menu.keys.len();
    if n == 0 {
        close(state, client);
        return;
    }
    prompt::ask(
        state,
        client,
        &format!("discard {n} changes? (y/n) "),
        "",
        "command",
        true,
        Box::new(|state, client, text| {
            if text == "y" {
                discard(state, client);
            }
            Routed::Consumed
        }),
    );
}

/// Closes the menu and puts back every value and binding it changed.
fn discard(state: &mut State, client: ClientId) {
    let Some(menu) = state.settings_menus.open.remove(&client) else {
        return;
    };
    clients::mark_dirty(state, client);
    keys::restore(state, &menu.keys);
    rows::restore(state, client, &menu.set);
}

/// Takes a client's keys while its settings menu is open.
pub struct SettingsHook;

impl InputHook for SettingsHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.settings_menus.is_open(client) {
            return None;
        }
        let Some(c) = state.clients.get(&client) else {
            state.settings_menus.open.remove(&client);
            return None;
        };
        let (session, size) = (c.session, c.size);
        let key = match input {
            Input::Key(key) => *key,
            Input::Paste { .. } | Input::Mouse(_) => return Some(Routed::Consumed),
            Input::Focus(_) => return None,
        };
        let menu = state.settings_menus.open.get_mut(&client)?;
        menu.diff = None;
        if let Some((table, old)) = menu.pending.take() {
            keys::pressed(state, client, table, old, key);
            clients::mark_dirty(state, client);
        } else if !clients::key_settings(&state.model, session).is_prefix(&key) {
            on_key(state, client, key, size);
        }
        Some(Routed::Consumed)
    }
}

/// A key for the list: it moves, filters, switches tabs, or acts on the
/// selected row.
fn on_key(state: &mut State, client: ClientId, key: Key, size: (u16, u16)) {
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    let shown = visible(menu, &values(state, menu));
    let Some(menu) = state.settings_menus.open.get_mut(&client) else {
        return;
    };
    let plain = !key.mods.ctrl && !key.mods.alt;
    let tabbed = plain && matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
    let step = if tabbed {
        let n = Section::ALL.len();
        menu.tab = match key.code {
            KeyCode::Tab => (menu.tab + 1) % n,
            _ => (menu.tab + n - 1) % n,
        };
        menu.ui.selected = 0;
        menu.ui.top = 0;
        ListAction::Moved
    } else {
        menu.ui.on_key(&key, shown.len(), true)
    };
    // A new tab or query starts at the top, which the draw scrolls to.
    if !tabbed && step != ListAction::QueryChanged {
        menu.ui.clamp(shown.len(), list_height(size));
    }
    let at = shown.get(menu.ui.selected).copied();
    match (step, at) {
        (ListAction::Moved | ListAction::QueryChanged, _) => clients::mark_dirty(state, client),
        (ListAction::Close, _) => escape(state, client),
        (ListAction::Enter, Some(at)) => enter(state, client, at),
        (ListAction::Ctrl('s'), _) => apply::save(state, client),
        (ListAction::Ctrl('d'), Some(at)) => unbind(state, client, at),
        _ => {}
    }
}

/// Enter on row `at`: a key row waits for the new key, a layout row sets
/// `default-layout`, an option row changes the option.
fn enter(state: &mut State, client: ClientId, at: usize) {
    let Some(menu) = state.settings_menus.open.get_mut(&client) else {
        return;
    };
    match menu.extra.get(at).map(|e| &e.act) {
        Some(Act::Key { table, key }) => {
            menu.pending = Some((table.clone(), *key));
            clients::mark_dirty(state, client);
        }
        Some(Act::Layout { name, .. }) => {
            let name = name.clone();
            rows::apply(state, client, &[("default-layout", &name)]);
        }
        None => {
            let row = menu.rows[at - menu.extra.len()].clone();
            rows::enter(state, client, &row);
        }
    }
}

/// C-d on row `at`: unbinds it when it is a key row.
fn unbind(state: &mut State, client: ClientId, at: usize) {
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    if let Some(Act::Key { table, key }) = menu.extra.get(at).map(|e| &e.act) {
        let (table, key) = (table.clone(), *key);
        keys::unbind(state, client, &table, key);
    }
}

/// Draws a client's settings menu.
pub struct SettingsOverlay;

impl Overlay for SettingsOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some(menu) = state.settings_menus.open.get(&client) else {
            return;
        };
        let size = (screen.cols, screen.rows);
        // A resize below it closes the menu (`resized`); until then, nothing.
        if too_small(size) {
            return;
        }
        match &menu.diff {
            Some(diff) => draw_diff(screen, list_area(size), diff),
            None => draw_menu(state, menu, screen, size),
        }
        screen.cursor = None;
    }
}

/// The settings.toml diff C-s asks about.
fn draw_diff(screen: &mut Screen, area: Rect, diff: &[String]) {
    let rows = Rows {
        cols: diff.iter().map(|l| vec![l.clone()]).collect(),
    };
    let all: Vec<usize> = (0..diff.len()).collect();
    let title = "settings.toml (y writes, n goes back)";
    let ui = ListState::default();
    listdraw::draw_list(screen, area, title, &rows, &all, &ui, false);
}

/// The tab bar and the rows; on the Layouts tab, the chosen layout's
/// preview beside them.
fn draw_menu(state: &State, menu: &Menu, screen: &mut Screen, size: (u16, u16)) {
    let vals = values(state, menu);
    let mut shown = visible(menu, &vals);
    let mut ui = menu.ui.clone();
    ui.clamp(shown.len(), list_height(size));
    let cols = if shown.is_empty() {
        shown = vec![0];
        vec![vec!["no matches".to_string()]]
    } else {
        row_cols(menu, &vals)
    };
    let title = match &menu.pending {
        Some((table, key)) => format!("press the new key for {table} {key} (Esc cancels)"),
        None => tabs(menu.tab, !menu.ui.query.is_empty()),
    };
    let layout_tab = Section::ALL[menu.tab] == Section::Layouts && menu.ui.query.is_empty();
    let chosen = shown.get(ui.selected).and_then(|&i| menu.extra.get(i));
    let full = list_area(size);
    let mut area = full;
    if layout_tab {
        area.cols = full.cols * 3 / 5;
    }
    listdraw::draw_list(screen, area, &title, &Rows { cols }, &shown, &ui, true);
    if let Some(Act::Layout { toml, .. }) = chosen.map(|e| &e.act).filter(|_| layout_tab) {
        draw_layout(screen, full, area, toml);
    }
}

/// Each row's columns, keys and layouts first; a row this menu changed is
/// marked `*`.
fn row_cols(menu: &Menu, vals: &[String]) -> Vec<Vec<String>> {
    let mark = |changed: bool| if changed { "* " } else { "  " };
    let extra = menu.extra.iter().map(|e| {
        let mut cols = e.cols.clone();
        if let (Act::Key { table, key }, Some(first)) = (&e.act, cols.first_mut()) {
            let changed = menu.keys.iter().any(|t| t.table == *table && t.key == *key);
            *first = format!("{}{first}", mark(changed));
        }
        cols
    });
    let options = menu.rows.iter().zip(vals).map(|(r, v)| {
        vec![
            format!("{}{}", mark(menu.set.contains_key(r.name)), r.name),
            truncate(v, 14),
            r.note.clone(),
            format!("default {}", truncate(r.default, 12)),
        ]
    });
    extra.chain(options).collect()
}

/// The preview of layout `toml` in what `list` leaves of `full`.
fn draw_layout(screen: &mut Screen, full: Rect, list: Rect, toml: &str) {
    let side = Rect {
        x: list.x + list.cols + 1,
        y: list.y + 2,
        cols: full.cols.saturating_sub(list.cols + 2),
        rows: full.rows.saturating_sub(4),
    };
    layouts::draw_preview(screen, side, toml);
}

/// The tab bar, the current tab in brackets unless a query searches all.
fn tabs(current: usize, searching: bool) -> String {
    Section::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| match i == current && !searching {
            true => format!("[{}]", s.title()),
            false => s.title().to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
