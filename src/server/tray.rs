//! The notification tray, choose-notification (agents spec §6.2).
//!
//! A centred list of the notifications in `State.notify`, newest first.
//! Rows are rebuilt from the store on every key and draw, so a dismissal
//! elsewhere shows at once.

use std::collections::BTreeMap;
use std::time::Instant;

use super::State;
use super::clients;
use super::dispatch::agent;
use super::dispatch::options::{FormatCtx, ServerVars};
use super::input::{Input, InputHook, Mouse, MouseButton, MouseKind, Routed};
use super::listdraw::{Rows, centred, draw_list};
use super::listview::{ListAction, ListState};
use super::notify::Notification;
use super::render::{Overlay, OverlayCtx, Screen};
use crate::cmd::expand;
use crate::types::{ClientId, PaneId, Rect};

/// Width and height of the tray, percent of the client.
const WIDTH_PCT: u16 = 80;
const HEIGHT_PCT: u16 = 50;

/// Open trays, per client. Lives on `State.trays`.
#[derive(Default)]
pub struct Trays {
    open: BTreeMap<ClientId, ListState>,
}

impl Trays {
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

pub fn open(state: &mut State, client: ClientId) -> Result<(), String> {
    if !state.clients.contains_key(&client) {
        return Err("no current client".to_string());
    }
    state.trays.open.insert(client, ListState::default());
    clients::mark_dirty(state, client);
    Ok(())
}

/// A client that detached takes its tray with it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.trays.open.remove(&id);
}

fn target_label(state: &State, pane: PaneId) -> String {
    if !state.model.panes.contains_key(&pane) {
        return pane.to_string();
    }
    let vars = FormatCtx::new(&state.model, pane);
    expand(
        "#{session_name}:#{window_index}.#{pane_index}",
        &ServerVars::fresh(state, pane, &vars),
    )
}

fn age(at: Instant) -> String {
    let secs = at.elapsed().as_secs();
    match secs {
        0..60 => format!("0:{secs:02}"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

fn rows(state: &State) -> Rows {
    let items: &[Notification] = state.notify.items();
    if items.is_empty() {
        return Rows {
            cols: vec![vec!["no notifications".to_string()]],
        };
    }
    let cols = items
        .iter()
        .map(|n| {
            vec![
                if n.read { " " } else { "●" }.to_string(),
                age(n.at),
                target_label(state, n.pane),
                n.kind.clone().unwrap_or_default(),
                n.state.clone(),
                n.msg.clone().unwrap_or_default(),
                "x".to_string(),
            ]
        })
        .collect();
    Rows { cols }
}

fn count(state: &State) -> usize {
    state.notify.items().len()
}

fn area(size: (u16, u16)) -> Rect {
    centred(size, WIDTH_PCT, HEIGHT_PCT)
}

/// Rows of the box that hold notifications: the box less its border.
fn list_height(size: (u16, u16)) -> usize {
    usize::from(area(size).rows).saturating_sub(2)
}

/// The screen column of the `x` cell, matching `draw_list`'s column layout.
fn x_column(rows: &Rows, area: Rect) -> usize {
    let ncols = rows.cols.iter().map(Vec::len).max().unwrap_or(0);
    let mut x = usize::from(area.x) + 1;
    for c in 0..ncols.saturating_sub(1) {
        let w = rows
            .cols
            .iter()
            .filter_map(|r| r.get(c))
            .map(|t| unicode_width::UnicodeWidthStr::width(t.as_str()))
            .max()
            .unwrap_or(0);
        x += w + 2;
    }
    x
}

pub struct TrayHook;

impl InputHook for TrayHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.trays.is_open(client) {
            return None;
        }
        let Some(size) = state.clients.get(&client).map(|c| c.size) else {
            client_gone(state, client);
            return None;
        };
        let mut st = state.trays.open.get(&client).cloned()?;
        st.clamp(count(state), list_height(size));
        let routed = match input {
            Input::Focus(_) => return None,
            Input::Paste { .. } => Routed::Consumed,
            Input::Mouse(m) => {
                click(state, size, &mut st, m);
                Routed::Consumed
            }
            Input::Key(key) => match st.on_key(key, count(state), false) {
                ListAction::Close => {
                    state.trays.open.remove(&client);
                    clients::mark_dirty(state, client);
                    return Some(Routed::Consumed);
                }
                ListAction::Enter => {
                    if let Some(n) = state.notify.items().get(st.selected).cloned() {
                        state.trays.open.remove(&client);
                        enter(state, client, &n);
                        clients::mark_dirty(state, client);
                        return Some(Routed::Consumed);
                    }
                    Routed::Consumed
                }
                ListAction::Ctrl('x') => {
                    if let Some(id) = state.notify.items().get(st.selected).map(|n| n.id) {
                        state.notify.dismiss(id);
                    }
                    Routed::Consumed
                }
                ListAction::Ctrl('l') => {
                    state.notify.clear();
                    Routed::Consumed
                }
                _ => Routed::Consumed,
            },
        };
        st.clamp(count(state), list_height(size));
        state.trays.open.insert(client, st);
        clients::mark_all_dirty(state);
        Some(routed)
    }
}

/// A left click selects the row; on its `x` it dismisses the row.
fn click(state: &mut State, size: (u16, u16), st: &mut ListState, m: &Mouse) {
    if m.kind != MouseKind::Down(MouseButton::Left) {
        return;
    }
    let a = area(size);
    let first = a.y.saturating_add(1);
    if m.row < first || usize::from(m.row - first) >= list_height(size) {
        return;
    }
    let at = st.top + usize::from(m.row - first);
    let Some(id) = state.notify.items().get(at).map(|n| n.id) else {
        return;
    };
    st.selected = at;
    if usize::from(m.col) == x_column(&rows(state), a) {
        state.notify.dismiss(id);
    }
}

/// Jumps to the notification's pane and marks it read.
fn enter(state: &mut State, client: ClientId, n: &Notification) {
    let ctx = state.clients.get(&client).map(|c| c.ctx.clone());
    if let Some(ctx) = ctx
        && let Err(e) = agent::jump(state, &ctx, n.pane)
    {
        clients::show_message(state, client, &e);
    }
    state.notify.seen(n.pane);
}

pub struct TrayOverlay;

impl Overlay for TrayOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some(st) = state.trays.open.get(&client) else {
            return;
        };
        let r = rows(state);
        let visible: Vec<usize> = (0..r.cols.len()).collect();
        draw_list(
            screen,
            area((screen.cols, screen.rows)),
            "choose-notification",
            &r,
            &visible,
            st,
            false,
        );
        screen.cursor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientCtx;
    use crate::server::clients::AttachRequest;
    use crate::server::input::{Mods, parse_key};
    use crate::server::{dispatch, test_ctx, test_state};
    use std::time::Duration;

    // The Notifier (T1) is real code here; the clock is the only seam and
    // the tests pass explicit instants.

    fn run(state: &mut State, args: &[&str]) {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv).expect("command");
    }

    fn raise(state: &mut State, pane: PaneId) {
        state.notify.on_change(
            pane,
            &format!("u{pane}"),
            Some("claude"),
            Some("working"),
            Some("blocked"),
            Some("perm"),
            Duration::from_secs(5),
            false,
            true,
            Instant::now(),
        );
    }

    /// Panes %a and %b in session `a`, a client on it, one notification each
    /// (the second newest, so it is listed first).
    fn setup() -> (State, ClientId, PaneId, PaneId) {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "a", "sleep 600"]);
        run(&mut state, &["new-window", "-t", "a", "sleep 600"]);
        let panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
        let (p1, p2) = (panes[0], panes[1]);
        raise(&mut state, p1);
        raise(&mut state, p2);
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
        // Keep the receiver alive for the test's length.
        std::mem::forget(accepted.output);
        (state, accepted.id, p1, p2)
    }

    fn press(state: &mut State, client: ClientId, name: &str) -> Option<Routed> {
        let key = parse_key(name).expect("key");
        TrayHook.on_input(state, client, &Input::Key(key), Instant::now())
    }

    fn screen_text(state: &State, client: ClientId) -> String {
        let mut screen = Screen::new(100, 40);
        let cx = OverlayCtx {
            client,
            viewport: (0, 0),
            status_row: None,
            window_top: 0,
            panes: Vec::new(),
        };
        TrayOverlay.draw(state, client, &cx, &mut screen);
        screen
            .cells
            .chunks(100)
            .map(|r| r.iter().map(|c| c.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enter_jumps_and_marks_read() {
        let (mut state, client, p1, p2) = setup();
        open(&mut state, client).expect("open");
        // Newest first: the first row is p2's.
        press(&mut state, client, "Down");
        press(&mut state, client, "Enter");
        assert!(!state.trays.is_open(client));
        let window = state.model.panes[&p1].window;
        assert_eq!(state.model.windows[&window].active, p1);
        let items = state.notify.items();
        assert!(items.iter().find(|n| n.pane == p1).expect("p1").read);
        assert!(!items.iter().find(|n| n.pane == p2).expect("p2").read);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_x_dismisses() {
        let (mut state, client, _p1, p2) = setup();
        open(&mut state, client).expect("open");
        press(&mut state, client, "C-x");
        let items = state.notify.items();
        assert_eq!(items.len(), 1);
        assert!(items.iter().all(|n| n.pane != p2));
        assert!(state.trays.is_open(client));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_l_clears() {
        let (mut state, client, _p1, _p2) = setup();
        open(&mut state, client).expect("open");
        press(&mut state, client, "C-l");
        assert!(state.notify.items().is_empty());
        assert!(screen_text(&state, client).contains("no notifications"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn click_x_dismisses() {
        let (mut state, client, _p1, p2) = setup();
        open(&mut state, client).expect("open");
        // Find the x cell on the first row from the drawn screen.
        let text = screen_text(&state, client);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("perm"))
            .expect("a row");
        let col =
            line.chars().count() - 1 - line.chars().rev().position(|c| c == 'x').expect("an x");
        let click = |col: usize| {
            Input::Mouse(Mouse {
                kind: MouseKind::Down(MouseButton::Left),
                col: col as u16,
                row: row as u16,
                mods: Mods::default(),
            })
        };
        // A click beside the x only selects.
        TrayHook.on_input(&mut state, client, &click(col - 3), Instant::now());
        assert_eq!(state.notify.items().len(), 2);
        TrayHook.on_input(&mut state, client, &click(col), Instant::now());
        let items = state.notify.items();
        assert_eq!(items.len(), 1);
        assert!(items.iter().all(|n| n.pane != p2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_says_no_notifications() {
        let (mut state, client, _p1, _p2) = setup();
        state.notify.clear();
        open(&mut state, client).expect("open");
        assert!(screen_text(&state, client).contains("no notifications"));
        press(&mut state, client, "Enter");
        assert!(
            state.trays.is_open(client),
            "Enter on the placeholder stays"
        );
        press(&mut state, client, "Escape");
        assert!(!state.trays.is_open(client));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closes_when_client_detaches() {
        let (mut state, client, _p1, _p2) = setup();
        open(&mut state, client).expect("open");
        state.clients.remove(&client);
        assert!(press(&mut state, client, "Down").is_none());
        assert!(!state.trays.is_open(client));
    }
}
