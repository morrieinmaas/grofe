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
use super::borders::{PaneBox, PaneLines};
use super::clients;
use super::dispatch::agent;
use super::dispatch::options::{self, FormatCtx, ServerVars};
use super::input::{Input, InputHook, MouseButton, MouseKind, Routed};
use super::listview::{ListAction, ListState};
use super::pane::{CellFlags, SnapCell};
use super::render::{self, Frame, Overlay, OverlayCtx, PaneLooks, Screen};
use super::status::{self, Style};
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

#[cfg(test)]
impl Sidebars {
    pub(crate) fn has(&self, id: ClientId) -> bool {
        self.clients.contains_key(&id)
    }
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

/// One agent entry: its pane and the window it is labelled with, after
/// the session's name when that is not the client's own.
struct Row {
    pane: PaneId,
    label: String,
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
            let prefix = if Some(&session) == own.as_ref() {
                String::new()
            } else {
                format!("{session}:")
            };
            panes.into_iter().map(move |pane| Row {
                pane,
                label: format!("{prefix}{}", field(state, pane, "#{window_name}")),
            })
        })
        .collect()
}

/// The lines an entry takes: its name and its detail, then a blank line
/// before the next one.
const ENTRY: usize = 3;

/// How many entries fit whole in `height` lines (the last needs no blank
/// line after it); at least one, as a page.
fn visible(height: usize) -> usize {
    ((height + 1) / ENTRY).max(1)
}

/// The first of `count` entries shown in `height` lines: the list's top,
/// moved just enough to keep the selected entry on screen.
fn first(count: usize, list: &ListState, height: usize) -> usize {
    let selected = list.selected.min(count.saturating_sub(1));
    let top = list.top.min(selected);
    top.max((selected + 1).saturating_sub(visible(height)))
}

/// The entry on line `y` of `height` content lines, from entry `top` on,
/// if `y` is one of its two lines (not the blank one after it).
fn entry_at(count: usize, top: usize, height: usize, y: usize) -> Option<usize> {
    let k = y / ENTRY;
    let i = top + k;
    (y % ENTRY < 2 && k * ENTRY + 2 <= height && i < count).then_some(i)
}

/// The columns the strip reserves on `client`'s screen: first column of
/// the separator, first row, height (the rows the window has).
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

/// Where the strip draws: the box round its entries, as screen columns
/// and rows, ends exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Strip {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    /// Framed as a pane is (spec 2026-11-05-frames.md), when the client's
    /// window is: the box is the frame's inside. Otherwise the box starts
    /// right of a one-cell separator.
    framed: bool,
}

impl Strip {
    fn height(&self) -> usize {
        self.y1 - self.y0
    }
}

/// The strip in the columns [`geometry`] reserves. Framed like one more
/// pane when the client's window is framed: the window's own outer gap
/// before it, `pane-gap` after it, and the same rows the window's frames
/// take (the gap on every edge but the status line's).
fn strip(state: &State, client: ClientId, cols: u16, rows: u16) -> Option<Strip> {
    let (x0, top, height) = geometry(state, client, cols, rows)?;
    let (x0, top, height, end) = (
        usize::from(x0),
        usize::from(top),
        usize::from(height),
        usize::from(cols),
    );
    let unframed = Strip {
        x0: x0 + 1,
        y0: top,
        x1: end,
        y1: top + height,
        framed: false,
    };
    let c = state.clients.get(&client)?;
    let model = &state.model;
    let Some(w) = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))
        .filter(|w| w.frames && w.framed())
    else {
        return Some(unframed);
    };
    let gap = usize::from(w.pane_gap);
    let geom = clients::status_geometry(model, c.session, rows);
    let (above, below) = match (geom.lines > 0, geom.top) {
        (false, _) => (gap, gap),
        (true, true) => (0, gap),
        (true, false) => (gap, 0),
    };
    let (fy0, fy1) = (top + above, (top + height).saturating_sub(below));
    let fx1 = end.saturating_sub(gap);
    if fx1 < x0 + 4 || fy1 < fy0 + 3 {
        return Some(unframed);
    }
    Some(Strip {
        x0: x0 + 1,
        y0: fy0 + 1,
        x1: fx1 - 1,
        y1: fy1 - 1,
        framed: true,
    })
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

/// What an entry's second line says: what the agent is blocked on, else
/// what it last said, else its state.
fn detail(a: &Agent) -> String {
    if let Some(reason) = a.blocked.as_deref() {
        return format!("blocked: {reason}");
    }
    match (a.msg.as_deref(), rank(a)) {
        (Some(text), _) if !text.is_empty() => text.to_string(),
        (_, Rank::Unseen) => "done, unseen".to_string(),
        (_, r) => r.as_str().to_string(),
    }
}

/// The glyph's colour for the agent's state: a blocked agent's is its
/// frame's (`agent-blocked-border-style`).
// ponytail: the other states take fixed named colours, as message pills
// do, so the terminal's theme picks the shades; no option per state yet.
fn state_style(a: &Agent, blocked: Style, dim: Style) -> Style {
    let named = |spec: &str| crate::server::status::parse_style(spec);
    match rank(a) {
        Rank::Blocked => blocked,
        Rank::Error => named("fg=red"),
        Rank::Unseen | Rank::Done => named("fg=green"),
        Rank::Working => named("fg=cyan"),
        Rank::Idle => dim,
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
        let inside = strip(state, client, cols, term_rows)?;
        let rows = rows(state, client);
        let mut list = state
            .sidebars
            .clients
            .get(&client)
            .map(|e| e.list.clone())
            .unwrap_or_default();
        list.page = visible(inside.height());
        match input {
            Input::Mouse(m) if m.col >= x0 && m.row >= top && m.row - top < height => {
                let (x, y) = (usize::from(m.col), usize::from(m.row));
                let on_box = (inside.x0..inside.x1).contains(&x) && y >= inside.y0;
                if m.kind == MouseKind::Down(MouseButton::Left)
                    && on_box
                    && let Some(i) = entry_at(
                        rows.len(),
                        first(rows.len(), &list, inside.height()),
                        inside.height(),
                        y - inside.y0,
                    )
                {
                    list.selected = i;
                    store(state, client, list, false);
                    jump(state, client, rows[i].pane);
                }
                Some(Routed::Consumed)
            }
            Input::Key(key) if is_focused(state, client) => {
                let action = list.on_key(key, rows.len(), false);
                list.selected = list.selected.min(rows.len().saturating_sub(1));
                list.top = first(rows.len(), &list, inside.height());
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

/// The options the strip is drawn with, from the client's window as its
/// panes' frames are: their lines, title switch and tab ends, the border
/// styles (active while the strip has the keys), `mode-style` for the
/// selected entry and `agent-blocked-border-style` for a blocked glyph.
struct Look {
    lines: PaneLines,
    title: bool,
    ends: String,
    frame: Style,
    mode: Style,
    blocked: Style,
}

fn look(state: &State, client: ClientId, focused: bool) -> Option<Look> {
    let model = &state.model;
    let c = state.clients.get(&client)?;
    let w = model
        .sessions
        .get(&c.session)
        .and_then(|s| model.windows.get(&s.active))?;
    let chain = options::chain(model, w.active);
    let opt = |name: &str| model.options.lookup(name, &chain).unwrap_or("");
    let mut looks = PaneLooks {
        lines: PaneLines::parse(opt("pane-border-lines")),
        ..Default::default()
    };
    render::apply_frames(model, w, &mut looks);
    let inner = FormatCtx::new(model, w.active);
    let vars = ServerVars::fresh(state, w.active, &inner);
    let style = |name: &str| status::base_style(opt(name), &vars);
    let border = if focused {
        "pane-active-border-style"
    } else {
        "pane-border-style"
    };
    Some(Look {
        lines: looks.lines,
        title: looks.frame_title,
        ends: opt("pane-frame-tab-ends").to_string(),
        frame: style(border),
        mode: style("mode-style"),
        blocked: style("agent-blocked-border-style"),
    })
}

pub struct SidebarOverlay;

impl Overlay for SidebarOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some((x0, top, height)) = geometry(state, client, screen.cols, screen.rows) else {
            return;
        };
        let Some(inside) = strip(state, client, screen.cols, screen.rows) else {
            return;
        };
        let focused = is_focused(state, client);
        let Some(look) = look(state, client, focused) else {
            return;
        };
        let (x0, top, height) = (usize::from(x0), usize::from(top), usize::from(height));
        let plain = Style::default();
        let mut dim = plain;
        dim.attrs |= CellFlags::DIM;
        for y in top..top + height {
            for x in x0..usize::from(screen.cols) {
                put_cell(screen, x, y, ' ', &plain);
            }
            if !inside.framed {
                put_cell(screen, x0, y, '│', &dim);
            }
        }
        let rows = rows(state, client);
        if inside.framed {
            let (l, r) = render::tab_ends(&look.ends);
            let title = format!("{l} {} {r} agents", rows.len());
            let content = PaneBox {
                id: PaneId(0),
                x: inside.x0 as i32,
                y: inside.y0 as i32,
                w: (inside.x1 - inside.x0) as i32,
                h: inside.height() as i32,
                index: 0,
            };
            let frame = Frame {
                lines: look.lines,
                style: look.frame,
                title: look.title.then_some(title.as_str()),
                tab: None,
                marked: false,
            };
            render::draw_frame(screen, &content, &frame);
        }
        // Text keeps a cell clear of each side of the box.
        let (tx, end) = (inside.x0 + 1, inside.x1.saturating_sub(1));
        if rows.is_empty() {
            put_text(screen, tx, inside.y0, end, "no agents", &dim);
            return;
        }
        let mut list = state
            .sidebars
            .clients
            .get(&client)
            .map(|e| e.list.clone())
            .unwrap_or_default();
        list.selected = list.selected.min(rows.len() - 1);
        let h = inside.height();
        let from = first(rows.len(), &list, h);
        for (k, row) in rows.iter().enumerate().skip(from) {
            let y = inside.y0 + (k - from) * ENTRY;
            if y + 2 > inside.y1 {
                break;
            }
            let Some(a) = state.agents.get(row.pane) else {
                continue;
            };
            let selected = focused && k == list.selected;
            let base = if selected { look.mode } else { plain };
            if selected {
                for line in [y, y + 1] {
                    for x in inside.x0..inside.x1 {
                        put_cell(screen, x, line, ' ', &base);
                    }
                }
            }
            let glyph_style = if selected {
                base
            } else {
                state_style(a, look.blocked, dim)
            };
            let mut name = base;
            name.attrs |= CellFlags::BOLD;
            let mut muted = base;
            muted.attrs |= CellFlags::DIM;
            let kind = a.kind.as_deref().unwrap_or("agent");
            let at = put_text(screen, tx, y, end, glyph(a), &glyph_style);
            let label = format!(" {kind} · {}", row.label);
            put_fit(screen, at, y, end, &label, &name);
            put_fit(screen, tx + 2, y + 1, end, &detail(a), &muted);
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
/// Returns the column after the text.
fn put_text(
    screen: &mut Screen,
    mut x: usize,
    y: usize,
    end: usize,
    text: &str,
    style: &Style,
) -> usize {
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
    x
}

/// As [`put_text`], but text cut at `end` ends in an ellipsis.
fn put_fit(screen: &mut Screen, x: usize, y: usize, end: usize, text: &str, style: &Style) {
    let room = end.saturating_sub(x);
    let width = |t: &str| {
        t.chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum::<usize>()
    };
    if width(text) <= room {
        put_text(screen, x, y, end, text, style);
        return;
    }
    let at = put_text(screen, x, y, end.saturating_sub(1), text, style);
    put_text(screen, at, y, end, "…", style);
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
        run(
            &mut state,
            &["new-session", "-d", "-s", "a", "-n", "api", "sleep 600"],
        );
        run(
            &mut state,
            &["new-window", "-t", "a", "-n", "web", "sleep 600"],
        );
        run(
            &mut state,
            &["new-session", "-d", "-s", "b", "-n", "infra", "sleep 600"],
        );
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

    fn screen(state: &State, client: ClientId, cols: u16) -> Screen {
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
    }

    fn lines(state: &State, client: ClientId, cols: u16) -> Vec<String> {
        screen(state, client, cols)
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

    /// The strip's columns (from the separator column, 127) of each line.
    fn strip_of(l: &[String]) -> Vec<String> {
        l.iter().map(|r| r.chars().skip(127).collect()).collect()
    }

    fn cell(screen: &Screen, x: usize, y: usize) -> &SnapCell {
        &screen.cells[y * usize::from(screen.cols) + x]
    }

    // 160 columns less 33: the window is 127 wide and ends in its one-cell
    // gap, so the strip's frame runs from column 127 to 158 with the gap
    // at 159; rows 1 to 48 under the window's top gap, meeting the status
    // line on row 49. Entries take three lines from row 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn framed_like_a_pane() {
        let (mut state, client, _p, _out) = setup(160);
        set(&mut state, client, SidebarOp::On).expect("on");
        let l = lines(&state, client, 160);
        let strip = strip_of(&l);
        assert!(strip[0].trim().is_empty(), "top gap: {:?}", strip[0]);
        assert!(strip[1].starts_with("╭─ ( 3 ) agents ─"), "{:?}", strip[1]);
        assert!(strip[1].ends_with("─╮ "), "{:?}", strip[1]);
        let at = |y: usize| strip[y].trim_end().to_string();
        assert_eq!(at(2), format!("│ ● claude · infra{}│", " ".repeat(13)));
        assert_eq!(at(3).trim_matches(['│', ' ']), "busy");
        assert_eq!(at(4).trim_matches(['│', ' ']), "", "blank between entries");
        assert_eq!(at(5).trim_matches(['│', ' ']), "⏸ claude · a:api");
        assert_eq!(at(6).trim_matches(['│', ' ']), "ask me");
        assert_eq!(at(7).trim_matches(['│', ' ']), "");
        assert_eq!(at(8).trim_matches(['│', ' ']), "○ claude · a:web");
        assert_eq!(at(9).trim_matches(['│', ' ']), "idle");
        assert!(strip[47].starts_with('│') && strip[47].ends_with("│ "));
        assert!(strip[48].starts_with("╰─") && strip[48].ends_with("─╯ "));
        assert!(strip[49].trim().is_empty(), "the status line's row");
        assert!(l[2].chars().take(127).all(|c| c == ' '), "window untouched");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn styled_from_options() {
        let (mut state, client, _p, _out) = setup(160);
        set(&mut state, client, SidebarOp::On).expect("on");
        run(
            &mut state,
            &["set", "-g", "agent-blocked-border-style", "fg=magenta"],
        );
        let s = screen(&state, client, 160);
        let magenta = status::parse_style("fg=magenta").fg;
        assert_eq!(cell(&s, 129, 5).c, '⏸');
        assert_eq!(cell(&s, 129, 5).fg, magenta, "blocked glyph");
        assert!(cell(&s, 131, 5).flags.contains(CellFlags::BOLD), "name");
        assert!(cell(&s, 131, 6).flags.contains(CellFlags::DIM), "detail");
        let green = status::parse_style("fg=green").fg;
        assert_ne!(cell(&s, 127, 1).fg, green, "inactive frame");
        set(&mut state, client, SidebarOp::Focus).expect("focus");
        let s = screen(&state, client, 160);
        assert_eq!(cell(&s, 127, 1).fg, green, "pane-active-border-style");
        assert_eq!(cell(&s, 158, 20).c, '│');
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selected_entry_takes_mode_style() {
        let (mut state, client, _p, _out) = setup(160);
        run(&mut state, &["set", "-g", "mode-style", "bg=blue,fg=white"]);
        set(&mut state, client, SidebarOp::Focus).expect("focus");
        press(&mut state, client, "Down");
        let s = screen(&state, client, 160);
        let blue = status::parse_style("bg=blue").bg;
        for y in [5, 6] {
            for x in 128..158 {
                assert_eq!(cell(&s, x, y).bg, blue, "({x}, {y})");
            }
        }
        for y in [2, 3, 4, 7, 8] {
            assert_ne!(cell(&s, 140, y).bg, blue, "row {y}");
        }
        assert_ne!(cell(&s, 127, 5).bg, blue, "frame keeps its style");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn long_detail_ends_in_ellipsis() {
        let (mut state, client, p, _out) = setup(160);
        let long = "x".repeat(80);
        state.agents.apply(
            p[2],
            report(AgentState::Working, Some(&long)),
            true,
            Instant::now(),
        );
        set(&mut state, client, SidebarOp::On).expect("on");
        let strip = strip_of(&lines(&state, client, 160));
        assert_eq!(strip[3], format!("│   {}… │ ", "x".repeat(25)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unframed_window_keeps_the_separator() {
        let (mut state, client, _p, _out) = setup(160);
        run(&mut state, &["set", "-g", "pane-frames", "off"]);
        set(&mut state, client, SidebarOp::On).expect("on");
        let strip = strip_of(&lines(&state, client, 160));
        assert!(strip[0].starts_with("│ ● claude · infra"), "{:?}", strip[0]);
        assert!(strip[1].contains("busy"));
        assert!(strip[3].starts_with("│ ⏸ claude · a:api"));
        assert!(strip[48].starts_with('│'));
        assert!(strip[49].trim().is_empty());
    }

    #[test]
    fn entries_scroll_three_lines_each() {
        assert_eq!(visible(8), 3);
        assert_eq!(visible(7), 2);
        assert_eq!(visible(1), 1);
        let list = |selected, top| ListState {
            selected,
            top,
            ..Default::default()
        };
        assert_eq!(first(10, &list(0, 0), 8), 0);
        assert_eq!(first(10, &list(3, 0), 8), 1);
        assert_eq!(first(10, &list(9, 0), 8), 7);
        assert_eq!(first(10, &list(2, 5), 8), 2, "up past the top");
        assert_eq!(entry_at(10, 1, 8, 0), Some(1));
        assert_eq!(entry_at(10, 1, 8, 1), Some(1));
        assert_eq!(entry_at(10, 1, 8, 2), None, "blank line");
        assert_eq!(entry_at(10, 1, 8, 6), Some(3));
        assert_eq!(entry_at(10, 1, 8, 7), Some(3));
        assert_eq!(entry_at(10, 1, 7, 6), None, "cut off");
        assert_eq!(entry_at(2, 0, 8, 6), None, "past the last");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn click_entry_jumps() {
        let (mut state, client, p, _out) = setup(160);
        set(&mut state, client, SidebarOp::On).expect("on");
        let active = |state: &State| state.model.sessions[&state.clients[&client].session].active;
        let before = active(&state);
        for row in [1, 7] {
            assert!(matches!(
                click(&mut state, client, 140, row),
                Some(Routed::Consumed)
            ));
            assert_eq!(active(&state), before, "row {row} is no entry");
        }
        assert!(matches!(
            click(&mut state, client, 140, 9),
            Some(Routed::Consumed)
        ));
        assert_eq!(active(&state), state.model.panes[&p[1]].window);
        assert_eq!(state.sidebars.clients[&client].list.selected, 2);
        // Now on session `a`, whose agents come first: p1 on rows 2 and 3.
        click(&mut state, client, 130, 3);
        assert_eq!(active(&state), state.model.panes[&p[0]].window);
        assert!(click(&mut state, client, 10, 8).is_none());
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
