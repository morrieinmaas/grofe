//! The agent sidebar (agents spec §5.3).
//!
//! A strip at the right edge of a client: a one-cell separator, then the
//! agents grouped by session, two lines each. It is drawn by the server over
//! the columns [`reserved`] takes out of the window (`clients::window_size`),
//! and redrawn when agent state changes (those already mark every client
//! dirty), not on pane output.
//!
//! ponytail: right side only; a left side needs a `window_left` at every
//! `window_top` site.

use std::collections::BTreeMap;
use std::time::Instant;

use unicode_width::UnicodeWidthChar;

use super::State;
use super::agents::{Agent, Rank, rank};
use super::clients;
use super::dispatch::agent;
use super::dispatch::options::{FormatCtx, ServerVars};
use super::input::{Input, InputHook, MouseButton, MouseKind, Routed};
use super::listview::{ListAction, ListState};
use super::pane::{CellFlags, SnapCell};
use super::render::{Overlay, OverlayCtx, Screen};
use super::status::Style;
pub use crate::cmd::SidebarOp;
use crate::cmd::expand;
use crate::types::{ClientId, PaneId};

/// Per client: whether the strip was switched on or off, and its list.
#[derive(Default)]
struct Entry {
    /// `None` until the client says: then `show-on-attach` decides.
    shown: Option<bool>,
    focused: bool,
    list: ListState,
}

/// Sidebar state, per client. Lives on `State.sidebars`.
#[derive(Default)]
pub struct Sidebars {
    clients: BTreeMap<ClientId, Entry>,
}

fn shown(state: &State, client: ClientId) -> bool {
    state
        .sidebars
        .clients
        .get(&client)
        .and_then(|e| e.shown)
        .unwrap_or(state.agents_sidebar.show_on_attach)
}

/// The columns the strip takes from a client `client_cols` wide: its width
/// and the separator, or 0 when hidden or the client is too narrow.
pub fn reserved(state: &State, client: ClientId, client_cols: u16) -> u16 {
    let cfg = &state.agents_sidebar;
    let taken = cfg.width.saturating_add(1);
    if shown(state, client) && client_cols >= cfg.min_client_width && client_cols > taken {
        taken
    } else {
        0
    }
}

/// True when `client` has the strip showing and its keys go to it.
pub fn is_focused(state: &State, client: ClientId) -> bool {
    let cols = state.clients.get(&client).map_or(0, |c| c.size.0);
    reserved(state, client, cols) > 0
        && state
            .sidebars
            .clients
            .get(&client)
            .is_some_and(|e| e.focused)
}

/// `agent-sidebar on|off|toggle|focus` for `client`. Resizes the window
/// when the reserved columns change.
pub fn set(state: &mut State, client: ClientId, how: SidebarOp) -> Result<(), String> {
    let Some(cols) = state.clients.get(&client).map(|c| c.size.0) else {
        return Err("no current client".to_string());
    };
    let now_shown = shown(state, client);
    let want = match how {
        SidebarOp::On | SidebarOp::Focus => true,
        SidebarOp::Off => false,
        SidebarOp::Toggle => !now_shown,
    };
    let entry = state.sidebars.clients.entry(client).or_default();
    entry.shown = Some(want);
    entry.focused = want && how == SidebarOp::Focus;
    if how == SidebarOp::Focus && reserved(state, client, cols) == 0 {
        if let Some(e) = state.sidebars.clients.get_mut(&client) {
            e.focused = false;
        }
        clients::changed(state);
        return Err("client too narrow for the sidebar".to_string());
    }
    clients::changed(state);
    Ok(())
}

/// A client that detached takes its sidebar with it.
pub fn client_gone(state: &mut State, client: ClientId) {
    state.sidebars.clients.remove(&client);
}

/// One agent row, with the session it is listed under.
struct Row {
    pane: PaneId,
    session: String,
}

fn field(state: &State, pane: PaneId, format: &str) -> String {
    let vars = FormatCtx::new(&state.model, pane);
    expand(format, &ServerVars::fresh(state, pane, &vars))
}

/// Agents grouped by session, the client's own session first, then by the
/// most urgent agent in each; urgency order inside a group.
fn rows(state: &State, client: ClientId) -> Vec<Row> {
    let own = state
        .clients
        .get(&client)
        .and_then(|c| state.model.sessions.get(&c.session))
        .map(|s| s.name.clone());
    let mut groups: Vec<(String, Vec<PaneId>)> = Vec::new();
    for pane in state.agents.by_urgency() {
        if !state.model.panes.contains_key(&pane) {
            continue;
        }
        let session = field(state, pane, "#{session_name}");
        match groups.iter_mut().find(|(s, _)| *s == session) {
            Some((_, panes)) => panes.push(pane),
            None => groups.push((session, vec![pane])),
        }
    }
    if let Some(at) = groups.iter().position(|(s, _)| Some(s) == own.as_ref()) {
        let first = groups.remove(at);
        groups.insert(0, first);
    }
    groups
        .into_iter()
        .flat_map(|(session, panes)| {
            panes.into_iter().map(move |pane| Row {
                pane,
                session: session.clone(),
            })
        })
        .collect()
}

/// What a strip line shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    Header(String),
    /// An agent (index into the rows); takes two lines.
    Agent(usize),
}

/// Places the rows from `top` into `height` lines: a header line when a
/// session starts, then two lines per agent that fits whole.
fn place(rows: &[Row], top: usize, height: usize) -> Vec<(usize, Slot)> {
    let mut slots = Vec::new();
    let mut y = 0;
    for (i, row) in rows.iter().enumerate().skip(top) {
        if i == top || rows[i - 1].session != row.session {
            if y >= height {
                break;
            }
            slots.push((y, Slot::Header(row.session.clone())));
            y += 1;
        }
        if y + 2 > height {
            break;
        }
        slots.push((y, Slot::Agent(i)));
        y += 2;
    }
    slots
}

/// The first row shown, moved so the selected row is on screen, and the
/// placed lines.
fn layout(rows: &[Row], list: &ListState, height: usize) -> (usize, Vec<(usize, Slot)>) {
    let selected = list.selected.min(rows.len().saturating_sub(1));
    let mut top = list.top.min(selected);
    loop {
        let slots = place(rows, top, height);
        let seen = slots.iter().any(|(_, s)| *s == Slot::Agent(selected));
        if seen || top >= selected {
            return (top, slots);
        }
        top += 1;
    }
}

/// Where the strip is on `client`'s screen: first column of the separator,
/// first row, height.
fn geometry(state: &State, client: ClientId, cols: u16, rows: u16) -> Option<(u16, u16, u16)> {
    let c = state.clients.get(&client)?;
    let taken = reserved(state, client, cols);
    if taken == 0 {
        return None;
    }
    let geom = clients::status_geometry(&state.model, c.session, rows);
    let top = if geom.top { geom.lines } else { 0 };
    Some((cols - taken, top, rows - geom.lines))
}

fn glyph(a: &Agent) -> &'static str {
    match rank(a) {
        Rank::Blocked => "⏸",
        Rank::Error => "✗",
        Rank::Unseen | Rank::Done => "✓",
        Rank::Working => "●",
        Rank::Idle => "○",
    }
}

pub struct SidebarHook;

impl InputHook for SidebarHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        let Some((cols, term_rows)) = state.clients.get(&client).map(|c| c.size) else {
            client_gone(state, client);
            return None;
        };
        let (x0, top, height) = geometry(state, client, cols, term_rows)?;
        let rows = rows(state, client);
        let mut list = state
            .sidebars
            .clients
            .get(&client)
            .map(|e| e.list.clone())
            .unwrap_or_default();
        list.page = (usize::from(height) / 2).max(1);
        match input {
            Input::Mouse(m) if m.col >= x0 => {
                if m.kind == MouseKind::Down(MouseButton::Left) && m.row >= top {
                    let y = usize::from(m.row - top);
                    let (_, slots) = layout(&rows, &list, usize::from(height));
                    let hit = slots.iter().find_map(|(at, slot)| match slot {
                        Slot::Agent(i) if y == *at || y == *at + 1 => Some(*i),
                        _ => None,
                    });
                    if let Some(i) = hit {
                        list.selected = i;
                        store(state, client, list, false);
                        jump(state, client, rows[i].pane);
                    }
                }
                Some(Routed::Consumed)
            }
            Input::Key(key) if is_focused(state, client) => {
                let action = list.on_key(key, rows.len(), false);
                list.selected = list.selected.min(rows.len().saturating_sub(1));
                list.top = layout(&rows, &list, usize::from(height)).0;
                let picked = (action == ListAction::Enter)
                    .then(|| rows.get(list.selected).map(|r| r.pane))
                    .flatten();
                let focused = !matches!(action, ListAction::Close | ListAction::Enter);
                store(state, client, list, focused);
                if let Some(pane) = picked {
                    jump(state, client, pane);
                }
                Some(Routed::Consumed)
            }
            _ => None,
        }
    }
}

fn store(state: &mut State, client: ClientId, list: ListState, focused: bool) {
    let entry = state.sidebars.clients.entry(client).or_default();
    entry.list = list;
    entry.focused = focused && entry.focused;
    clients::mark_dirty(state, client);
}

fn jump(state: &mut State, client: ClientId, pane: PaneId) {
    let Some(ctx) = state.clients.get(&client).map(|c| c.ctx.clone()) else {
        return;
    };
    if let Err(e) = agent::jump(state, &ctx, pane) {
        clients::show_message(state, client, &e);
    }
}

pub struct SidebarOverlay;

impl Overlay for SidebarOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some((x0, top, height)) = geometry(state, client, screen.cols, screen.rows) else {
            return;
        };
        let (x0, top, height) = (usize::from(x0), usize::from(top), usize::from(height));
        let end = usize::from(screen.cols);
        let plain = Style::default();
        let mut dim = plain;
        dim.attrs |= CellFlags::DIM;
        let mut bold = plain;
        bold.attrs |= CellFlags::BOLD;
        for y in top..top + height {
            for x in x0..end {
                put_cell(screen, x, y, ' ', &plain);
            }
            put_cell(screen, x0, y, '│', &dim);
        }
        let entry = state.sidebars.clients.get(&client);
        let focused = is_focused(state, client);
        let mut list = entry.map(|e| e.list.clone()).unwrap_or_default();
        let rows = rows(state, client);
        list.page = (height / 2).max(1);
        if rows.is_empty() {
            put_text(screen, x0 + 2, top, end, "no agents", &dim);
            return;
        }
        let selected = list.selected.min(rows.len() - 1);
        let (_, slots) = layout(&rows, &list, height);
        for (at, slot) in slots {
            let y = top + at;
            match slot {
                Slot::Header(name) => {
                    put_text(screen, x0 + 2, y, end, &name, &bold);
                }
                Slot::Agent(i) => {
                    let Some(a) = state.agents.get(rows[i].pane) else {
                        continue;
                    };
                    let mut style = plain;
                    let mut msg_style = dim;
                    if focused && i == selected {
                        style.attrs |= CellFlags::INVERSE;
                        msg_style.attrs |= CellFlags::INVERSE;
                    }
                    for line in [y, y + 1] {
                        for x in x0 + 1..end {
                            put_cell(screen, x, line, ' ', &style);
                        }
                    }
                    let target = field(state, rows[i].pane, "#{window_index}.#{pane_index}");
                    let kind = a.kind.as_deref().unwrap_or("");
                    let first = format!(" {} {target} {kind}", glyph(a));
                    put_text(screen, x0 + 1, y, end, &first, &style);
                    let msg = a.blocked.as_deref().or(a.msg.as_deref()).unwrap_or("");
                    put_text(screen, x0 + 1, y + 1, end, &format!("   {msg}"), &msg_style);
                }
            }
        }
    }
}

fn put_cell(screen: &mut Screen, x: usize, y: usize, c: char, style: &Style) {
    put_with(screen, x, y, c, style, CellFlags::empty());
}

fn put_with(screen: &mut Screen, x: usize, y: usize, c: char, style: &Style, flags: CellFlags) {
    if x >= usize::from(screen.cols) || y >= usize::from(screen.rows) {
        return;
    }
    let at = y * usize::from(screen.cols) + x;
    screen.cells[at] = SnapCell {
        c,
        fg: style.fg,
        bg: style.bg,
        flags: style.attrs | flags,
        zerowidth: Vec::new(),
        underline_color: style.underline_color,
        hyperlink: None,
    };
}

/// Draws `text` from `x`, cut at column `end` (exclusive). A wide character
/// that does not fit whole is dropped, not split.
fn put_text(screen: &mut Screen, mut x: usize, y: usize, end: usize, text: &str, style: &Style) {
    for c in text.chars() {
        let w = match UnicodeWidthChar::width(c) {
            Some(w) if w > 0 => w,
            _ => continue,
        };
        if x + w > end {
            break;
        }
        if w == 2 {
            put_with(screen, x, y, c, style, CellFlags::WIDE_CHAR);
            put_with(screen, x + 1, y, ' ', style, CellFlags::WIDE_CHAR_SPACER);
        } else {
            put_cell(screen, x, y, c, style);
        }
        x += w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientCtx;
    use crate::server::agents::{AgentState, Report, ReportState};
    use crate::server::clients::AttachRequest;
    use crate::server::input::{Mods, Mouse, parse_key};
    use crate::server::{dispatch, test_ctx, test_state};

    // The producers (agents, jump, listview, clients::changed) are real code
    // here; nothing is faked.

    fn report(state: AgentState, msg: Option<&str>) -> Report {
        Report {
            state: ReportState::Set(state),
            kind: Some("claude".into()),
            msg: msg.map(String::from),
            blocked: None,
            session_id: None,
            ts: 1,
        }
    }

    fn run(state: &mut State, args: &[&str]) {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv).expect("command");
    }

    /// Sessions `a` (panes p1, p2 in two windows) and `b` (p3), a client of
    /// `cols` columns on `b`. p1 blocked, p2 idle, p3 working.
    fn setup(
        cols: u16,
    ) -> (
        State,
        ClientId,
        [PaneId; 3],
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "a", "sleep 600"]);
        run(&mut state, &["new-window", "-t", "a", "sleep 600"]);
        run(&mut state, &["new-session", "-d", "-s", "b", "sleep 600"]);
        let panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
        let p = [panes[0], panes[1], panes[2]];
        let now = Instant::now();
        state
            .agents
            .apply(p[0], report(AgentState::Blocked, Some("ask me")), true, now);
        state
            .agents
            .apply(p[1], report(AgentState::Idle, None), true, now);
        state
            .agents
            .apply(p[2], report(AgentState::Working, Some("busy")), true, now);
        let accepted = clients::attach(
            &mut state,
            AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size: (cols, 50),
                term: "xterm".into(),
                caps: Default::default(),
                pid: None,
                target: Some("b".into()),
                detach_others: false,
                nest: false,
                tty: None,
                created: false,
                extras: Default::default(),
            },
        )
        .expect("attach");
        state.agents_sidebar.width = 32;
        state.agents_sidebar.min_client_width = 120;
        state.agents_sidebar.show_on_attach = false;
        (state, accepted.id, p, accepted.output)
    }

    fn press(state: &mut State, client: ClientId, name: &str) -> Option<Routed> {
        let key = parse_key(name).expect("key");
        SidebarHook.on_input(state, client, &Input::Key(key), Instant::now())
    }

    fn click(state: &mut State, client: ClientId, col: u16, row: u16) -> Option<Routed> {
        let m = Mouse {
            kind: MouseKind::Down(MouseButton::Left),
            col,
            row,
            mods: Mods::default(),
        };
        SidebarHook.on_input(state, client, &Input::Mouse(m), Instant::now())
    }

    fn lines(state: &State, client: ClientId, cols: u16) -> Vec<String> {
        let mut screen = Screen::new(cols, 50);
        let cx = OverlayCtx {
            client,
            viewport: (0, 0),
            status_row: None,
            window_top: 0,
            panes: Vec::new(),
        };
        SidebarOverlay.draw(state, client, &cx, &mut screen);
        screen
            .cells
            .chunks(usize::from(cols))
            .map(|r| r.iter().map(|c| c.c).collect::<String>())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hidden_at_attach() {
        let (state, client, _p, _out) = setup(160);
        assert_eq!(reserved(&state, client, 160), 0);
        assert!(lines(&state, client, 160).iter().all(|l| !l.contains('│')));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn show_on_attach_config() {
        let (mut state, client, _p, _out) = setup(160);
        state.agents_sidebar.show_on_attach = true;
        assert_eq!(reserved(&state, client, 160), 33);
        // An explicit off beats the default.
        set(&mut state, client, SidebarOp::Off).expect("off");
        assert_eq!(reserved(&state, client, 160), 0);
        set(&mut state, client, SidebarOp::Toggle).expect("toggle");
        assert_eq!(reserved(&state, client, 160), 33);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn narrow_client_hides() {
        let (mut state, client, _p, _out) = setup(100);
        set(&mut state, client, SidebarOp::On).expect("on");
        assert_eq!(reserved(&state, client, 100), 0);
        assert_eq!(reserved(&state, client, 119), 0);
        assert_eq!(reserved(&state, client, 120), 33);
        assert!(set(&mut state, client, SidebarOp::Focus).is_err());
        assert!(!is_focused(&state, client));
        assert!(press(&mut state, client, "Down").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rows_grouped_by_session_two_lines() {
        let (mut state, client, _p, _out) = setup(160);
        set(&mut state, client, SidebarOp::On).expect("on");
        let l = lines(&state, client, 160);
        let strip: Vec<String> = l.iter().map(|r| r.chars().skip(127).collect()).collect();
        assert!(strip[0].starts_with('│'), "separator column");
        // The client's own session b first, then a with its two agents.
        assert!(strip[0].contains(" b"));
        assert!(strip[1].contains("● 1.1 claude"), "{:?}", strip[1]);
        assert!(strip[2].contains("busy"));
        assert!(strip[3].contains(" a"));
        assert!(strip[4].contains("⏸ 1.1 claude"));
        assert!(strip[5].contains("ask me"));
        assert!(strip[6].contains("○ 2.1 claude"));
        assert!(strip[7].trim_matches(['│', ' ']).is_empty());
        // The status row stays out of the strip.
        assert!(!strip[49].contains('│'));
        // Nothing left of the separator.
        assert!(l[1].chars().take(127).all(|c| c == ' '));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn click_row_jumps() {
        let (mut state, client, p, _out) = setup(160);
        set(&mut state, client, SidebarOp::On).expect("on");
        // p2 (session a, second window) is listed on lines 6 and 7.
        let routed = click(&mut state, client, 140, 7);
        assert!(matches!(routed, Some(Routed::Consumed)));
        let window = state.model.panes[&p[1]].window;
        let session = state.clients[&client].session;
        assert_eq!(state.model.sessions[&session].active, window);
        // A click on a header selects nothing but is still consumed.
        assert!(matches!(
            click(&mut state, client, 140, 0),
            Some(Routed::Consumed)
        ));
        // A click left of the strip goes on to the panes.
        assert!(click(&mut state, client, 10, 7).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focus_routes_keys_escape_returns() {
        let (mut state, client, p, _out) = setup(160);
        assert!(press(&mut state, client, "Down").is_none(), "not focused");
        set(&mut state, client, SidebarOp::Focus).expect("focus");
        assert!(is_focused(&state, client));
        assert!(matches!(
            press(&mut state, client, "Down"),
            Some(Routed::Consumed)
        ));
        assert_eq!(state.sidebars.clients[&client].list.selected, 1);
        assert!(matches!(
            press(&mut state, client, "x"),
            Some(Routed::Consumed)
        ));
        assert!(matches!(
            press(&mut state, client, "Escape"),
            Some(Routed::Consumed)
        ));
        assert!(!is_focused(&state, client));
        assert!(press(&mut state, client, "Down").is_none(), "keys back");
        // The strip stays shown.
        assert_eq!(reserved(&state, client, 160), 33);
        // Enter jumps to the selected row (p1, blocked, second in the list).
        set(&mut state, client, SidebarOp::Focus).expect("focus");
        press(&mut state, client, "Enter");
        let window = state.model.panes[&p[0]].window;
        let session = state.clients[&client].session;
        assert_eq!(state.model.sessions[&session].active, window);
        assert!(!is_focused(&state, client));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_gone_drops_state() {
        let (mut state, client, _p, _out) = setup(160);
        set(&mut state, client, SidebarOp::On).expect("on");
        client_gone(&mut state, client);
        assert_eq!(reserved(&state, client, 160), 0);
        assert!(set(&mut state, ClientId(999), SidebarOp::On).is_err());
    }
}
