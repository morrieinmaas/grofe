//! The key menu (agents spec §5.5): a key table's bindings in the list
//! widget, grouped. A key bound in the table runs at once; `C-n`, `C-p`,
//! `C-f`, `Escape` and `C-c` belong to the menu.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::server::State;
use crate::server::clients;
use crate::server::input::{
    Input, InputHook, Key, KeyCode, KeySettings, KeyState, KeyTables, Routed,
};
use crate::server::listdraw::{self, Rows};
use crate::server::listview::{self, ListAction, ListState};
use crate::server::prompt;
use crate::server::render::{Overlay, OverlayCtx, Screen};
use crate::types::{ClientId, InputModes, Rect};

/// Group of a binding with none, listed last.
const OTHER: &str = "other";

/// Open menus, per client. Lives on `State.keymenus`.
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
    table: String,
    ui: ListState,
}

/// One listed binding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    key: Key,
    group: String,
    label: String,
}

/// What a key did to the menu.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Run this key's binding and close.
    Run(Key),
    /// Open this table's menu instead.
    Submenu(String),
    Close,
    Redraw,
    Ignore,
}

/// Opens `table`'s menu on `client`.
pub fn open(state: &mut State, client: ClientId, table: &str) -> Result<(), String> {
    if !state.clients.contains_key(&client) {
        return Err("no client".to_string());
    }
    if !state.registry.key_tables.exists(table) {
        return Err(format!("unknown key table: {table}"));
    }
    state.keymenus.open.insert(
        client,
        Menu {
            table: table.to_string(),
            ui: ListState::default(),
        },
    );
    clients::mark_dirty(state, client);
    Ok(())
}

fn close(state: &mut State, client: ClientId) {
    state.keymenus.open.remove(&client);
    clients::mark_dirty(state, client);
}

/// A client that detached takes its menu with it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.keymenus.open.remove(&id);
}

fn is_reserved(key: &Key) -> bool {
    let ctrl = key.mods.ctrl && !key.mods.alt;
    match key.code {
        KeyCode::Escape => true,
        KeyCode::Char('n' | 'p' | 'f' | 'c') => ctrl,
        _ => false,
    }
}

/// The table's bindings by group, groups in first-seen order and `other`
/// last. Mouse keys and `Any` are left out. `expand` turns a group or
/// description format into text.
fn build_rows(tables: &KeyTables, table: &str, expand: &dyn Fn(&str) -> String) -> Vec<Row> {
    let mut groups: Vec<String> = Vec::new();
    let mut found: Vec<Row> = Vec::new();
    for key in tables.keys(table) {
        if matches!(key.code, KeyCode::Mouse(_) | KeyCode::Any) {
            continue;
        }
        let Some(b) = tables.exact(table, &key) else {
            continue;
        };
        let group = expand(b.group.as_deref().unwrap_or(OTHER));
        if !groups.contains(&group) {
            groups.push(group.clone());
        }
        let label = match &b.desc {
            Some(desc) => expand(desc),
            None => b
                .cmds
                .iter()
                .map(|argv| argv.join(" "))
                .collect::<Vec<_>>()
                .join("; "),
        };
        found.push(Row { key, group, label });
    }
    groups.sort_by_key(|g| g == OTHER);
    let mut rows = Vec::with_capacity(found.len());
    for g in &groups {
        rows.extend(found.iter().filter(|r| &r.group == g).cloned());
    }
    rows
}

/// Positions in `rows` the query leaves visible, best first.
fn visible(rows: &[Row], query: &str) -> Vec<usize> {
    let texts: Vec<String> = rows
        .iter()
        .map(|r| format!("{} {} {}", r.label, r.group, r.key))
        .collect();
    listview::filter(&texts, query)
}

/// What `key` does in the menu of `table`.
fn step(ui: &mut ListState, rows: &[Row], tables: &KeyTables, table: &str, key: &Key) -> Step {
    let shown = visible(rows, &ui.query);
    let reserved = is_reserved(key);
    if !ui.filtering
        && !reserved
        && !matches!(key.code, KeyCode::Mouse(_) | KeyCode::Any)
        && let Some(b) = tables.get(table, key)
    {
        if let [argv] = b.cmds.as_slice()
            && let [cmd, flag, target] = argv.as_slice()
            && matches!(cmd.as_str(), "switch-client" | "switchc")
            && flag == "-T"
        {
            return Step::Submenu(target.clone());
        }
        return Step::Run(*key);
    }
    match ui.on_key(key, shown.len(), false) {
        ListAction::Close => Step::Close,
        ListAction::Moved | ListAction::QueryChanged => Step::Redraw,
        ListAction::Enter => match shown.get(ui.selected).and_then(|i| rows.get(*i)) {
            Some(row) => Step::Run(row.key),
            None => Step::Ignore,
        },
        ListAction::Ctrl(_) | ListAction::None => Step::Ignore,
    }
}

/// Presses `key` in `table` for a client as the key tables would: a `-r`
/// binding leaves the client in the table, any other returns it to root.
fn press(
    keys: &mut KeyState,
    tables: &KeyTables,
    table: &str,
    key: &Key,
    settings: &KeySettings,
    modes: &InputModes,
) -> Routed {
    keys.set_table(table);
    keys.route(&Input::Key(*key), Instant::now(), tables, settings, modes)
}

// --- Keys ---------------------------------------------------------------

/// Takes a client's keys while its key menu is open.
pub struct KeyMenuHook;

impl InputHook for KeyMenuHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.keymenus.is_open(client) {
            return None;
        }
        let Some(session) = state.clients.get(&client).map(|c| c.session) else {
            state.keymenus.open.remove(&client);
            return None;
        };
        let key = match input {
            Input::Key(key) => *key,
            Input::Paste { .. } | Input::Mouse(_) => return Some(Routed::Consumed),
            Input::Focus(_) => return None,
        };
        let table = state.keymenus.open.get(&client)?.table.clone();
        // The prefix key is not a binding to run from a menu: it would
        // switch the client to the prefix table under the menu.
        let settings = clients::key_settings(&state.model, session);
        if settings.is_prefix(&key) {
            return Some(Routed::Consumed);
        }
        let rows = rows_for(state, client, &table);
        let size = state.clients.get(&client).map_or((80, 24), |c| c.size);
        let outcome = {
            let menu = state.keymenus.open.get_mut(&client)?;
            let outcome = step(
                &mut menu.ui,
                &rows,
                &state.registry.key_tables,
                &table,
                &key,
            );
            let n = visible(&rows, &menu.ui.query).len();
            menu.ui.clamp(n, list_height(size));
            outcome
        };
        match outcome {
            Step::Ignore => {}
            Step::Redraw => clients::mark_dirty(state, client),
            Step::Close => close(state, client),
            Step::Submenu(target) => {
                if state.registry.key_tables.exists(&target) {
                    if let Some(menu) = state.keymenus.open.get_mut(&client) {
                        menu.table = target;
                        menu.ui = ListState::default();
                    }
                    clients::mark_dirty(state, client);
                } else {
                    close(state, client);
                }
            }
            Step::Run(run) => {
                close(state, client);
                let modes = active_pane(state, session)
                    .and_then(|p| state.model.panes.get(&p))
                    .map(|p| p.handle.modes())
                    .unwrap_or_default();
                let State {
                    clients, registry, ..
                } = &mut *state;
                let c = clients.get_mut(&client)?;
                return Some(press(
                    &mut c.keys,
                    &registry.key_tables,
                    &table,
                    &run,
                    &settings,
                    &modes,
                ));
            }
        }
        Some(Routed::Consumed)
    }
}

fn active_pane(state: &State, session: crate::types::SessionId) -> Option<crate::types::PaneId> {
    let model = &state.model;
    let window = model.sessions.get(&session)?.active;
    Some(model.windows.get(&window)?.active)
}

fn rows_for(state: &State, client: ClientId, table: &str) -> Vec<Row> {
    let Some(c) = state.clients.get(&client) else {
        return Vec::new();
    };
    let mut ctx = c.ctx.clone();
    ctx.pane = active_pane(state, c.session);
    build_rows(&state.registry.key_tables, table, &|text| {
        prompt::expand_command(state, &ctx, text)
    })
}

// --- Drawing ------------------------------------------------------------

/// The box's rows for the list: inside the border, minus the query line.
fn list_height(size: (u16, u16)) -> usize {
    usize::from(listdraw::centred(size, 70, 70).rows.saturating_sub(3))
}

/// Draws a client's key menu.
pub struct KeyMenuOverlay;

impl Overlay for KeyMenuOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some(menu) = state.keymenus.open.get(&client) else {
            return;
        };
        let rows = rows_for(state, client, &menu.table);
        let shown = visible(&rows, &menu.ui.query);
        let area: Rect = listdraw::centred((screen.cols, screen.rows), 70, 70);
        let mut ui = menu.ui.clone();
        ui.clamp(shown.len(), list_height((screen.cols, screen.rows)));
        let cols = rows
            .iter()
            .map(|r| vec![r.key.to_string(), r.group.clone(), r.label.clone()])
            .collect();
        let title = format!("key-menu {}", menu.table);
        listdraw::draw_list(
            screen,
            area,
            &title,
            &Rows { cols },
            &shown,
            &ui,
            ui.filtering,
        );
        screen.cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::input::{Binding, BindingSource, parse_key};

    fn key(name: &str) -> Key {
        parse_key(name).expect("key")
    }

    fn bind(
        t: &mut KeyTables,
        table: &str,
        k: &str,
        cmd: &[&str],
        group: Option<&str>,
        repeat: bool,
    ) {
        let argv = cmd.iter().map(|s| s.to_string()).collect();
        let mut b = Binding::new(vec![argv], repeat, BindingSource::Default);
        b.group = group.map(str::to_string);
        t.insert(table, key(k), b);
    }

    fn tables() -> KeyTables {
        let mut t = KeyTables::default();
        bind(
            &mut t,
            "prefix",
            "c",
            &["new-window"],
            Some("window"),
            false,
        );
        bind(&mut t, "prefix", "x", &["kill-pane"], Some("pane"), false);
        bind(&mut t, "prefix", "z", &["resize-pane", "-Z"], None, false);
        t
    }

    fn rows(t: &KeyTables) -> Vec<Row> {
        build_rows(t, "prefix", &|s| s.to_string())
    }

    fn press_step(ui: &mut ListState, t: &KeyTables, k: &str) -> Step {
        step(ui, &rows(t), t, "prefix", &key(k))
    }

    #[test]
    fn filter_ranks_by_description() {
        let mut t = tables();
        bind(&mut t, "prefix", "b", &["break-pane"], Some("pane"), false);
        t.insert("prefix", key("b"), {
            let mut b = Binding::new(
                vec![vec!["break-pane".into()]],
                false,
                BindingSource::Default,
            );
            b.group = Some("pane".into());
            b.desc = Some("Break pane to a new window".into());
            b
        });
        let mut n = Binding::new(
            vec![vec!["new-window".into()]],
            false,
            BindingSource::Default,
        );
        n.group = Some("window".into());
        n.desc = Some("Create a new window".into());
        t.insert("prefix", key("c"), n);
        let r = rows(&t);
        let mut ui = ListState::default();
        press_step(&mut ui, &t, "C-f");
        for c in ["n", "e", "w"] {
            press_step(&mut ui, &t, c);
        }
        let shown = visible(&r, &ui.query);
        assert_eq!(r[shown[0]].key, key("c"), "contiguous and earliest first");
        assert_eq!(press_step(&mut ui, &t, "Enter"), Step::Run(key("c")));
    }

    #[test]
    fn bound_key_runs_and_closes() {
        let t = tables();
        let mut ui = ListState::default();
        assert_eq!(press_step(&mut ui, &t, "c"), Step::Run(key("c")));
    }

    #[test]
    fn reserved_keys_win() {
        let mut t = tables();
        bind(
            &mut t,
            "prefix",
            "C-n",
            &["next-window"],
            Some("window"),
            false,
        );
        let mut ui = ListState::default();
        assert_eq!(press_step(&mut ui, &t, "C-n"), Step::Redraw);
        assert_eq!(ui.selected, 1);
        assert_eq!(press_step(&mut ui, &t, "Escape"), Step::Close);
        assert_eq!(press_step(&mut ui, &t, "C-c"), Step::Close);
        assert_eq!(press_step(&mut ui, &t, "C-f"), Step::Redraw);
        assert!(ui.filtering);
    }

    #[test]
    fn filter_then_enter_runs_selected() {
        let t = tables();
        let mut ui = ListState::default();
        press_step(&mut ui, &t, "C-f");
        for c in ["w", "i", "n"] {
            assert_eq!(press_step(&mut ui, &t, c), Step::Redraw);
        }
        assert_eq!(press_step(&mut ui, &t, "Enter"), Step::Run(key("c")));
    }

    #[test]
    fn repeat_binding_leaves_prefix_table() {
        let mut t = tables();
        bind(
            &mut t,
            "prefix",
            "n",
            &["next-window"],
            Some("window"),
            true,
        );
        let mut keys = KeyState::new();
        let settings = KeySettings::default();
        let modes = InputModes::default();
        let routed = press(&mut keys, &t, "prefix", &key("n"), &settings, &modes);
        assert!(matches!(routed, Routed::Bound(..)));
        assert_eq!(keys.table(), "prefix");
        let routed = press(&mut keys, &t, "prefix", &key("c"), &settings, &modes);
        assert!(matches!(routed, Routed::Bound(..)));
        assert_eq!(keys.table(), "root");
    }

    #[test]
    fn switch_client_t_opens_submenu() {
        let mut t = tables();
        bind(
            &mut t,
            "prefix",
            "[",
            &["switch-client", "-T", "copy"],
            Some("copy"),
            false,
        );
        let mut ui = ListState::default();
        assert_eq!(press_step(&mut ui, &t, "["), Step::Submenu("copy".into()));
    }

    #[test]
    fn no_group_goes_to_other() {
        let t = tables();
        let r = rows(&t);
        let groups: Vec<&str> = r.iter().map(|r| r.group.as_str()).collect();
        assert_eq!(groups, ["window", "pane", "other"]);
        assert_eq!(r[2].key, key("z"));
    }

    #[test]
    fn mouse_and_any_hidden() {
        let mut t = tables();
        bind(&mut t, "prefix", "Any", &["display", "x"], None, false);
        bind(
            &mut t,
            "prefix",
            "MouseDown1Pane",
            &["select-pane"],
            None,
            false,
        );
        let r = rows(&t);
        assert_eq!(r.len(), 3);
        assert!(
            r.iter()
                .all(|r| !matches!(r.key.code, KeyCode::Mouse(_) | KeyCode::Any))
        );
    }

    #[test]
    fn labels_are_formats() {
        let state = crate::server::test_state();
        let ctx = crate::server::test_ctx();
        let mut t = tables();
        let mut b = Binding::new(
            vec![vec!["agent".into(), "next".into()]],
            false,
            BindingSource::Default,
        );
        b.group = Some("agents".into());
        b.desc = Some("Agents (#{e|+:1,2})".into());
        t.insert("prefix", key("a"), b);
        let r = build_rows(&t, "prefix", &|s| prompt::expand_command(&state, &ctx, s));
        let agents = r.iter().find(|r| r.key == key("a")).expect("row");
        assert_eq!(agents.label, "Agents (3)");
    }
    /// A server with a client attached to session `a`, running one agent.
    fn client_state() -> (State, ClientId) {
        use crate::protocol::ClientCtx;
        use crate::server::clients::AttachRequest;
        use crate::server::{dispatch, test_ctx, test_state};
        let mut state = test_state();
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        dispatch::run(
            &mut state,
            &test_ctx(),
            &argv(&["new-session", "-d", "-s", "a", "sleep 600"]),
        )
        .expect("session");
        let accepted = clients::attach(
            &mut state,
            AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (100, 40),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("a".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        (state, accepted.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn label_expands_with_clients_pane() {
        use crate::server::agents::{AgentState, Report, ReportState};
        let (mut state, client) = client_state();
        let pane = *state.model.panes.keys().next().expect("pane");
        let report = Report {
            state: ReportState::Set(AgentState::Blocked),
            kind: Some("claude".into()),
            msg: None,
            blocked: None,
            session_id: None,
            ts: 1,
        };
        state.agents.apply(pane, report, true, Instant::now());
        let mut b = Binding::new(
            vec![vec!["choose-agent".into()]],
            false,
            BindingSource::Default,
        );
        b.group = Some("agents".into());
        b.desc = Some("Agents (#{agents_attention})".into());
        state.registry.key_tables.insert("prefix", key("U"), b);
        let rows = rows_for(&state, client, "prefix");
        let row = rows.iter().find(|r| r.key == key("U")).expect("row");
        assert_eq!(row.label, "Agents (1)");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prefix_key_is_ignored_in_menu() {
        let (mut state, client) = client_state();
        let mut b = Binding::new(
            vec![vec!["display-message".into(), "x".into()]],
            false,
            BindingSource::Default,
        );
        b.group = Some("other".into());
        state.registry.key_tables.insert("custom", key("C-b"), b);
        open(&mut state, client, "custom").expect("open");
        let routed =
            KeyMenuHook.on_input(&mut state, client, &Input::Key(key("C-b")), Instant::now());
        assert!(matches!(routed, Some(Routed::Consumed)));
        assert!(state.keymenus.is_open(client), "menu stays");
        assert_eq!(state.clients[&client].keys.table(), "root");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detach_removes_menu() {
        let (mut state, client) = client_state();
        open(&mut state, client, "prefix").expect("open");
        clients::detach(&mut state, client);
        assert!(!state.keymenus.is_open(client));
    }
}
