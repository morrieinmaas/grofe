//! The settings menu overlay: the options in the list widget, one tab per
//! section. Every change applies at once; Esc asks before it puts back the
//! values captured at open. `C-s` and the Keys and Layouts rows are the
//! apply, keys and layouts tasks'.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::config::{self, Env};
use crate::server::State;
use crate::server::clients;
use crate::server::input::{Input, InputHook, KeyCode, Routed};
use crate::server::listdraw::{self, Rows};
use crate::server::listview::{self, ListAction, ListState};
use crate::server::prompt;
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::{ClientId, Rect};

use super::rows::{self, Row};
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
    state.settings_menus.open.insert(
        client,
        Menu {
            ui: ListState::default(),
            tab: 0,
            rows,
            base,
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
    let pool: Vec<usize> = (0..menu.rows.len())
        .filter(|&i| !query.is_empty() || menu.rows[i].section == section)
        .collect();
    let texts: Vec<String> = pool
        .iter()
        .map(|&i| format!("{} {} {}", menu.rows[i].name, values[i], menu.rows[i].doc))
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
fn escape(state: &mut State, client: ClientId, menu_changes: Vec<(&'static str, String)>) {
    if menu_changes.is_empty() {
        close(state, client);
        return;
    }
    let label = format!("discard {} changes? (y/n) ", menu_changes.len());
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
            close(state, client);
            let back: Vec<(&str, &str)> = menu_changes
                .iter()
                .map(|(name, old)| (*name, old.as_str()))
                .collect();
            rows::apply(state, client, &back);
            Routed::Consumed
        }),
    );
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
                escape(state, client, changes);
            }
            ListAction::Enter => {
                let at = shown.get(menu.ui.selected).copied()?;
                let row = menu.rows[at].clone();
                rows::enter(state, client, &row);
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
            menu.rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let mark = if vals[i] != menu.base[i] { "* " } else { "  " };
                    vec![
                        format!("{mark}{}", r.name),
                        truncate(&vals[i], 14),
                        r.note.clone(),
                        format!("default {}", truncate(r.default, 12)),
                    ]
                })
                .collect()
        };
        let area = list_area((screen.cols, screen.rows));
        let title = tabs(menu.tab, !menu.ui.query.is_empty());
        listdraw::draw_list(screen, area, &title, &Rows { cols }, &shown, &ui, true);
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
