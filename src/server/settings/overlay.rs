//! The settings menu overlay: the options in the list widget, one tab per
//! section. Every change applies at once; Esc asks before it puts back the
//! values captured at open. `C-s` and the Keys and Layouts rows are the
//! apply, keys and layouts tasks'.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::config::{self, Env};
use crate::server::customize::{self, Scope};
use crate::server::input::{Input, InputHook, Key, KeyCode, Routed};
use crate::server::listdraw::{self, Rows};
use crate::server::listview::{self, ListAction, ListState};
use crate::server::prompt;
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::server::{State, clients};
use crate::types::{ClientId, Rect};

use super::apply::{self, Change};
use super::keys::{self, Edit};
use super::layouts;
use super::rows::{self, Act, Entry, Row};
use super::sections::Section;

/// The smallest client the menu is drawn on.
const MIN_SIZE: (u16, u16) = (50, 10);

/// Open menus, per client. Lives on `State.settings_menus`.
#[derive(Default)]
pub struct Menus {
    open: BTreeMap<ClientId, Menu>,
}

impl Menus {
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

struct Menu {
    ui: ListState,
    /// Index into [`Section::ALL`].
    tab: usize,
    rows: Vec<Row>,
    /// Each row's value when the menu opened.
    base: Vec<String>,
    /// Key rows, then layout rows; listed before the options.
    extra: Vec<Entry>,
    /// How many of `extra` are key rows.
    nkeys: usize,
    /// Rebinds and unbinds made so far.
    edits: Vec<Edit>,
    /// The key row waiting for the key to bind it to.
    pending: Option<(String, Key)>,
    /// The settings.toml diff C-s is asking about.
    diff: Option<Vec<String>>,
}

/// Opens the menu on `client`, or says the client is too small.
pub fn open(state: &mut State, client: ClientId) -> Result<(), String> {
    let size = state.clients.get(&client).ok_or("no client")?.size;
    if size.0 < MIN_SIZE.0 || size.1 < MIN_SIZE.1 {
        clients::show_message(state, client, "window too small");
        return Ok(());
    }
    let env = Env::from_process();
    let prov = config::load(&env).map(|r| r.provenance).unwrap_or_default();
    let rows = rows::build(&prov, env.config_dir.as_deref());
    let base = rows.iter().map(|r| rows::value(state, r.name)).collect();
    let mut extra = keys::entries(state);
    let nkeys = extra.len();
    extra.extend(layouts::entries());
    state.settings_menus.open.insert(
        client,
        Menu {
            ui: ListState::default(),
            tab: 0,
            rows,
            base,
            extra,
            nkeys,
            edits: Vec::new(),
            pending: None,
            diff: None,
        },
    );
    clients::mark_dirty(state, client);
    Ok(())
}

fn close(state: &mut State, client: ClientId) {
    state.settings_menus.open.remove(&client);
    clients::mark_dirty(state, client);
}

/// A client that detached takes its menu with it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.settings_menus.open.remove(&id);
}

/// Notes a rebind or unbind and lists the keys again.
pub(super) fn record(state: &mut State, client: ClientId, edit: Edit) {
    let fresh = keys::entries(state);
    if let Some(menu) = state.settings_menus.open.get_mut(&client) {
        let n = fresh.len();
        menu.extra.splice(..menu.nkeys, fresh);
        menu.nkeys = n;
        menu.edits.push(edit);
    }
    clients::mark_dirty(state, client);
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

/// Esc: close, or ask first when something changed.
fn escape(
    state: &mut State,
    client: ClientId,
    menu_changes: Vec<(&'static str, String)>,
    key_edits: usize,
) {
    if menu_changes.is_empty() && key_edits == 0 {
        close(state, client);
        return;
    }
    let label = format!("discard {} changes? (y/n) ", menu_changes.len() + key_edits);
    prompt::ask(
        state,
        client,
        &label,
        "",
        "command",
        true,
        Box::new(move |state, client, text| {
            if text != "y" {
                return Routed::Consumed;
            }
            let edits = state
                .settings_menus
                .open
                .get_mut(&client)
                .map(|m| std::mem::take(&mut m.edits))
                .unwrap_or_default();
            close(state, client);
            keys::restore(state, &edits);
            let back: Vec<(&str, &str)> = menu_changes
                .iter()
                .map(|(name, old)| (*name, old.as_str()))
                .collect();
            rows::apply(state, client, &back);
            Routed::Consumed
        }),
    );
}

/// Why a key the menu rebound or unbound cannot be saved, if it cannot: its
/// live binding, or what an earlier edit of the menu found about it.
pub(super) fn refusal(
    state: &State,
    client: ClientId,
    table: &str,
    key: &Key,
) -> Option<&'static str> {
    customize::binding_refusal(state, table, key).or_else(|| {
        let menu = state.settings_menus.open.get(&client)?;
        let touched = menu.edits.iter().flat_map(|e| &e.binds);
        let found = touched.filter(|b| b.table == table && b.key == *key);
        found.filter_map(|b| b.refusal).next()
    })
}

/// What C-s writes: the edits to settings.toml that make the file say what
/// the menu shows, the options and keys they hand over to it, and the
/// changes that stay live only, with why.
#[derive(Default)]
struct Plan {
    changes: Vec<Change>,
    names: Vec<&'static str>,
    keys: Vec<(String, Key)>,
    refused: Vec<String>,
}

fn collect(state: &State, menu: &Menu) -> Plan {
    let mut plan = Plan::default();
    for (r, old) in menu.rows.iter().zip(&menu.base) {
        let now = rows::value(state, r.name);
        if now == *old {
            continue;
        }
        match customize::refusal(Scope::GlobalSession, r.name, None) {
            Some(why) => plan.refused.push(format!("{}: not saved, {why}", r.name)),
            None => {
                plan.names.push(r.name);
                plan.changes.push(Change::Set {
                    key: r.toml.to_string(),
                    value: rows::item(r.toml, &now),
                });
            }
        }
    }
    let mut seen: Vec<(&str, Key)> = Vec::new();
    for t in menu.edits.iter().flat_map(|e| &e.binds) {
        if seen.contains(&(t.table.as_str(), t.key)) {
            continue;
        }
        seen.push((t.table.as_str(), t.key));
        let why = t
            .refusal
            .or_else(|| customize::binding_refusal(state, &t.table, &t.key));
        match why {
            Some(why) => plan
                .refused
                .push(format!("{} {}: not saved, {why}", t.table, t.key)),
            None => {
                plan.changes.push(Change::Bind {
                    table: t.table.clone(),
                    key: t.key.to_string(),
                    fields: customize::binding_fields(state, &t.table, &t.key),
                });
                plan.keys.push((t.table.clone(), t.key));
            }
        }
    }
    plan
}

/// C-s: shows the diff and asks before writing.
fn save(state: &mut State, client: ClientId) {
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    let plan = collect(state, menu);
    let mut lines = Vec::new();
    match customize::settings_dir() {
        Ok(dir) => match apply::diff(&dir, &plan.changes) {
            Ok(t) => lines.extend(t.lines().map(str::to_string)),
            Err(e) => {
                clients::show_message(state, client, &e);
                return;
            }
        },
        Err(why) => lines.push(why),
    }
    lines.extend(plan.refused.iter().map(|l| format!("! {l}")));
    if lines.is_empty() {
        clients::show_message(state, client, "nothing to save");
        return;
    }
    if let Some(menu) = state.settings_menus.open.get_mut(&client) {
        menu.diff = Some(lines);
    }
    prompt::ask(
        state,
        client,
        "write settings.toml? (y/n) ",
        "",
        "command",
        true,
        Box::new(move |state, client, text| {
            if let Some(menu) = state.settings_menus.open.get_mut(&client) {
                menu.diff = None;
            }
            if text == "y" {
                commit(state, client);
            }
            clients::mark_dirty(state, client);
            Routed::Consumed
        }),
    );
    clients::mark_dirty(state, client);
}

/// Writes settings.toml, hands the changed options and keys to it and
/// reloads. A reload that fails puts the file, the marks and the bindings
/// back; what was refused stays live and is not written.
fn commit(state: &mut State, client: ClientId) {
    let Ok(dir) = customize::settings_dir() else {
        return;
    };
    let Some(menu) = state.settings_menus.open.get(&client) else {
        return;
    };
    let plan = collect(state, menu);
    if plan.changes.is_empty() {
        return;
    }
    let keys: Vec<(&str, Key)> = plan.keys.iter().map(|(t, k)| (t.as_str(), *k)).collect();
    let result = customize::with_owned_reload(state, &plan.names, &keys, || {
        apply::write(&dir, &plan.changes)
    });
    match result {
        Ok(()) => {
            close(state, client);
            clients::show_message(state, client, "saved settings.toml");
        }
        Err(e) => clients::show_message(state, client, e.trim_end()),
    }
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
            return Some(Routed::Consumed);
        }
        if clients::key_settings(&state.model, session).is_prefix(&key) {
            return Some(Routed::Consumed);
        }
        let vals = values(state, state.settings_menus.open.get(&client)?);
        let menu = state.settings_menus.open.get_mut(&client)?;
        let plain = !key.mods.ctrl && !key.mods.alt;
        let step = match key.code {
            KeyCode::Tab | KeyCode::BackTab if plain => {
                let n = Section::ALL.len();
                menu.tab = if key.code == KeyCode::Tab {
                    (menu.tab + 1) % n
                } else {
                    (menu.tab + n - 1) % n
                };
                menu.ui.selected = 0;
                menu.ui.top = 0;
                ListAction::Moved
            }
            _ => {
                let shown = visible(menu, &vals);
                menu.ui.on_key(&key, shown.len(), true)
            }
        };
        let shown = visible(menu, &vals);
        menu.ui.clamp(shown.len(), list_height(size));
        match step {
            ListAction::Moved | ListAction::QueryChanged => clients::mark_dirty(state, client),
            ListAction::Close => {
                let changes = menu
                    .rows
                    .iter()
                    .zip(menu.base.iter().zip(&vals))
                    .filter(|(_, (old, now))| old != now)
                    .map(|(r, (old, _))| (r.name, old.clone()))
                    .collect();
                let edits = menu.edits.len();
                escape(state, client, changes, edits);
            }
            ListAction::Enter => {
                let at = shown.get(menu.ui.selected).copied()?;
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
            ListAction::Ctrl('s') => save(state, client),
            ListAction::Ctrl('d') => {
                let at = shown.get(menu.ui.selected).copied()?;
                if let Some(Act::Key { table, key }) = menu.extra.get(at).map(|e| &e.act) {
                    let (table, key) = (table.clone(), *key);
                    keys::unbind(state, client, &table, key);
                }
            }
            ListAction::Ctrl(_) | ListAction::None => {}
        }
        Some(Routed::Consumed)
    }
}

/// Draws a client's settings menu.
pub struct SettingsOverlay;

impl Overlay for SettingsOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some(menu) = state.settings_menus.open.get(&client) else {
            return;
        };
        let vals = values(state, menu);
        let mut shown = visible(menu, &vals);
        let mut ui = menu.ui.clone();
        ui.clamp(shown.len(), list_height((screen.cols, screen.rows)));
        let cols = if shown.is_empty() {
            shown = vec![0];
            vec![vec!["no matches".to_string()]]
        } else {
            let options = menu.rows.iter().enumerate().map(|(i, r)| {
                let mark = if vals[i] != menu.base[i] { "* " } else { "  " };
                vec![
                    format!("{mark}{}", r.name),
                    truncate(&vals[i], 14),
                    r.note.clone(),
                    format!("default {}", truncate(r.default, 12)),
                ]
            });
            menu.extra
                .iter()
                .map(|e| e.cols.clone())
                .chain(options)
                .collect()
        };
        let mut area = list_area((screen.cols, screen.rows));
        if let Some(diff) = &menu.diff {
            let cols = diff.iter().map(|l| vec![l.clone()]).collect();
            let all: Vec<usize> = (0..diff.len()).collect();
            let title = "settings.toml (y writes, n goes back)";
            let rows = Rows { cols };
            listdraw::draw_list(
                screen,
                area,
                title,
                &rows,
                &all,
                &ListState::default(),
                false,
            );
            screen.cursor = None;
            return;
        }
        let title = match &menu.pending {
            Some((table, key)) => format!("press the new key for {table} {key} (Esc cancels)"),
            None => tabs(menu.tab, !menu.ui.query.is_empty()),
        };
        let layout_tab = Section::ALL[menu.tab] == Section::Layouts && menu.ui.query.is_empty();
        let chosen = shown.get(ui.selected).and_then(|&i| menu.extra.get(i));
        let full = area;
        if layout_tab {
            area.cols = full.cols * 3 / 5;
        }
        listdraw::draw_list(screen, area, &title, &Rows { cols }, &shown, &ui, true);
        if let Some(Act::Layout { toml, .. }) = chosen.map(|e| &e.act).filter(|_| layout_tab) {
            let side = Rect {
                x: area.x + area.cols + 1,
                y: area.y + 2,
                cols: full.cols - area.cols - 2,
                rows: full.rows.saturating_sub(4),
            };
            layouts::draw_preview(screen, side, toml);
        }
        screen.cursor = None;
    }
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
