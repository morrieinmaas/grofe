//! The agent grid (agents spec §5.4).

use std::collections::BTreeMap;
use std::time::Instant;

use alacritty_terminal::vte::ansi::{Color, NamedColor};
use unicode_width::UnicodeWidthChar;

use super::State;
use super::agents::{Agent, Rank, rank};
use super::clients;
use super::dispatch::agent;
use super::dispatch::options::{FormatCtx, ServerVars};
use super::input::{Input, InputHook, KeyCode, Routed};
use super::listview::{ListAction, ListState, filter};
use super::pane::{CellFlags, SnapCell};
use super::render::{Overlay, OverlayCtx, Screen};
use crate::cmd::expand;
use crate::types::{ClientId, PaneId};

/// Open grids, per client. Lives on `State.grids`. `ListState.selected` is
/// the tile, `top` the first tile row shown.
#[derive(Default)]
pub struct Grids {
    open: BTreeMap<ClientId, ListState>,
}

impl Grids {
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

pub fn open(state: &mut State, client: ClientId) -> Result<(), String> {
    if !state.clients.contains_key(&client) {
        return Err("no current client".to_string());
    }
    state.grids.open.insert(client, ListState::default());
    clients::mark_dirty(state, client);
    Ok(())
}

/// A client that detached takes its grid with it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.grids.open.remove(&id);
}

/// How the tiles sit on a client: columns, tile width, tile height, and the
/// tile rows that fit under the query line.
struct Layout {
    cols: usize,
    tile_w: usize,
    tile_h: usize,
    rows: usize,
}

fn layout(state: &State, size: (u16, u16)) -> Layout {
    let want = usize::from(state.agents_grid.tile_width).max(1);
    let cols = (usize::from(size.0) / want).max(1);
    let tile_h = usize::from(state.agents_grid.preview_lines) + 1;
    let avail = usize::from(size.1).saturating_sub(1);
    Layout {
        cols,
        tile_w: (usize::from(size.0) / cols).max(1),
        tile_h,
        rows: (avail / tile_h).max(1),
    }
}

fn target_label(state: &State, pane: PaneId) -> String {
    let vars = FormatCtx::new(&state.model, pane);
    expand(
        "#{session_name}:#{window_index}.#{pane_index}",
        &ServerVars::fresh(state, pane, &vars),
    )
}

fn age(since: Instant) -> String {
    let secs = since.elapsed().as_secs();
    match secs {
        0..60 => format!("0:{secs:02}"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
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

fn title(state: &State, pane: PaneId, a: &Agent) -> String {
    format!(
        "{} {} {} {}",
        glyph(a),
        target_label(state, pane),
        a.kind.clone().unwrap_or_default(),
        age(a.since)
    )
}

/// The agent panes in display order that match `query`, with their titles.
fn view(state: &State, query: &str) -> Vec<(PaneId, String)> {
    let all: Vec<(PaneId, String)> = state
        .agents
        .by_urgency()
        .into_iter()
        .filter(|p| state.model.panes.contains_key(p))
        .filter_map(|p| state.agents.get(p).map(|a| (p, title(state, p, a))))
        .collect();
    let texts: Vec<String> = all
        .iter()
        .map(|(p, t)| {
            let state_word = state.agents.get(*p).map_or("", |a| rank(a).as_str());
            format!("{t} {state_word}")
        })
        .collect();
    filter(&texts, query)
        .into_iter()
        .map(|i| all[i].clone())
        .collect()
}

type Tiles = (Vec<(PaneId, String)>, Layout, ListState);

fn tiles_of(state: &State, client: ClientId) -> Option<Tiles> {
    let st = state.grids.open.get(&client)?;
    let size = state.clients.get(&client)?.size;
    Some((view(state, &st.query), layout(state, size), st.clone()))
}

/// The panes whose tiles are on screen for `client`; empty if no grid is open.
pub fn visible_panes(state: &State, client: ClientId) -> Vec<PaneId> {
    let Some((tiles, lay, st)) = tiles_of(state, client) else {
        return Vec::new();
    };
    tiles
        .iter()
        .skip(st.top * lay.cols)
        .take(lay.rows * lay.cols)
        .map(|(p, _)| *p)
        .collect()
}

/// Keeps the selection inside the tiles and its row on screen.
fn clamp(st: &mut ListState, count: usize, lay: &Layout) {
    st.selected = st.selected.min(count.saturating_sub(1));
    let row = st.selected / lay.cols;
    if row < st.top {
        st.top = row;
    } else if row >= st.top + lay.rows {
        st.top = row + 1 - lay.rows;
    }
    let total_rows = count.div_ceil(lay.cols);
    st.top = st.top.min(total_rows.saturating_sub(lay.rows));
}

pub struct GridHook;

impl InputHook for GridHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.grids.is_open(client) {
            return None;
        }
        let Some(size) = state.clients.get(&client).map(|c| c.size) else {
            client_gone(state, client);
            return None;
        };
        let key = match input {
            Input::Focus(_) => return None,
            Input::Key(key) => key,
            Input::Mouse(_) | Input::Paste { .. } => return Some(Routed::Consumed),
        };
        let lay = layout(state, size);
        let mut st = state.grids.open.get(&client).cloned()?;
        let count = view(state, &st.query).len();
        let plain = !(key.mods.ctrl || key.mods.alt || key.mods.super_);
        let step = match key.code {
            KeyCode::Left if plain => Some((-1, 1)),
            KeyCode::Right if plain => Some((1, 1)),
            KeyCode::Up if plain => Some((-1, lay.cols)),
            KeyCode::Down if plain => Some((1, lay.cols)),
            _ => None,
        };
        let action = if let Some((dir, by)) = step {
            st.selected = if dir < 0 {
                st.selected.saturating_sub(by)
            } else {
                (st.selected + by).min(count.saturating_sub(1))
            };
            ListAction::Moved
        } else {
            st.on_key(key, count, true)
        };
        let count = view(state, &st.query).len();
        clamp(&mut st, count, &lay);
        state.grids.open.insert(client, st);
        clients::mark_dirty(state, client);
        Some(match action {
            ListAction::Close => close(state, client),
            ListAction::Enter => match selected_pane(state, client) {
                Some(pane) => enter(state, client, pane),
                None => Routed::Consumed,
            },
            _ => Routed::Consumed,
        })
    }
}

/// The pane under the selection, if the grid is open and shows any.
pub fn selected_pane(state: &State, client: ClientId) -> Option<PaneId> {
    let (tiles, _, st) = tiles_of(state, client)?;
    tiles.get(st.selected).map(|(p, _)| *p)
}

fn close(state: &mut State, client: ClientId) -> Routed {
    state.grids.open.remove(&client);
    clients::mark_dirty(state, client);
    Routed::Consumed
}

fn enter(state: &mut State, client: ClientId, pane: PaneId) -> Routed {
    let ctx = state.clients.get(&client).map(|c| c.ctx.clone());
    close(state, client);
    if let Some(ctx) = ctx
        && let Err(e) = agent::jump(state, &ctx, pane)
    {
        clients::show_message(state, client, &e);
    }
    Routed::Consumed
}

fn blank() -> SnapCell {
    SnapCell {
        c: ' ',
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: CellFlags::empty(),
        zerowidth: Vec::new(),
        underline_color: None,
        hyperlink: None,
    }
}

/// Draws `text` from `(x, y)`, never past column `end` (exclusive); a wide
/// character that does not fit whole is dropped.
fn put_text(screen: &mut Screen, x: usize, y: usize, end: usize, text: &str, flags: CellFlags) {
    let end = end.min(usize::from(screen.cols));
    if y >= usize::from(screen.rows) {
        return;
    }
    let mut x = x;
    for c in text.chars() {
        let w = match UnicodeWidthChar::width(c) {
            Some(w) if w > 0 => w,
            _ => continue,
        };
        if x + w > end {
            break;
        }
        let at = y * usize::from(screen.cols) + x;
        screen.cells[at] = SnapCell {
            c,
            flags: flags
                | if w == 2 {
                    CellFlags::WIDE_CHAR
                } else {
                    CellFlags::empty()
                },
            ..blank()
        };
        if w == 2 {
            screen.cells[at + 1] = SnapCell {
                flags: CellFlags::WIDE_CHAR_SPACER | flags,
                ..blank()
            };
        }
        x += w;
    }
}

pub struct GridOverlay;

impl Overlay for GridOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some(st) = state.grids.open.get(&client) else {
            return;
        };
        let lay = layout(state, (screen.cols, screen.rows));
        let tiles = view(state, &st.query);
        screen.cells.fill(blank());
        screen.cursor = None;
        let cols = usize::from(screen.cols);
        put_text(
            screen,
            0,
            0,
            cols,
            &format!("agents> {}", st.query),
            CellFlags::BOLD,
        );
        if tiles.is_empty() {
            put_text(screen, 0, 1, cols, "no agents", CellFlags::empty());
            return;
        }
        for (i, (pane, label)) in tiles
            .iter()
            .enumerate()
            .skip(st.top * lay.cols)
            .take(lay.rows * lay.cols)
        {
            let x = (i % lay.cols) * lay.tile_w;
            let y = 1 + (i / lay.cols - st.top) * lay.tile_h;
            let flags = if i == st.selected {
                CellFlags::INVERSE
            } else {
                CellFlags::BOLD
            };
            for dx in 0..lay.tile_w {
                put_text(screen, x + dx, y, x + dx + 1, " ", flags);
            }
            put_text(screen, x, y, x + lay.tile_w, label, flags);
            draw_body(
                state,
                *pane,
                (x, y + 1),
                (lay.tile_w, lay.tile_h - 1),
                screen,
            );
        }
    }
}

/// The bottom `h` rows of `pane`'s screen, cropped to `w` columns, at `at`.
fn draw_body(
    state: &State,
    pane: PaneId,
    at: (usize, usize),
    (w, h): (usize, usize),
    screen: &mut Screen,
) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    let snap = p.handle.snapshot_rows(0..p.rows);
    let first = snap.rows.len().saturating_sub(h);
    for (j, row) in snap.rows[first..].iter().enumerate() {
        for (i, cell) in row.iter().take(w).enumerate() {
            if i + 1 == w && cell.flags.contains(CellFlags::WIDE_CHAR) {
                continue;
            }
            let (x, y) = (at.0 + i, at.1 + j);
            if x < usize::from(screen.cols) && y < usize::from(screen.rows) {
                screen.cells[y * usize::from(screen.cols) + x] = cell.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientCtx;
    use crate::server::agents::{AgentState, Report, ReportState};
    use crate::server::clients::AttachRequest;
    use crate::server::input::parse_key;
    use crate::server::{dispatch, test_ctx, test_state};

    fn report(state: AgentState) -> Report {
        Report {
            state: ReportState::Set(state),
            kind: Some("claude".into()),
            msg: None,
            blocked: None,
            session_id: None,
            ts: 1,
        }
    }

    fn run(state: &mut State, args: &[&str]) {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        dispatch::run(state, &test_ctx(), &argv).expect("command");
    }

    /// `n` agent panes in session `a`, one client of `size`. The first
    /// pane is blocked, the rest idle.
    fn setup(
        n: usize,
        size: (u16, u16),
    ) -> (
        State,
        ClientId,
        Vec<PaneId>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "a", "sleep 600"]);
        for _ in 1..n {
            run(&mut state, &["new-window", "-t", "a", "sleep 600"]);
        }
        let panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
        let now = Instant::now();
        for (i, p) in panes.iter().enumerate() {
            let s = if i == 0 {
                AgentState::Blocked
            } else {
                AgentState::Idle
            };
            state.agents.apply(*p, report(s), true, now);
        }
        let accepted = clients::attach(
            &mut state,
            AttachRequest {
                ctx: ClientCtx {
                    client: None,
                    ..test_ctx()
                },
                size,
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
        (state, accepted.id, panes, accepted.output)
    }

    fn press(state: &mut State, client: ClientId, name: &str) -> Option<Routed> {
        let key = parse_key(name).expect("key");
        GridHook.on_input(state, client, &Input::Key(key), Instant::now())
    }

    fn screen_text(state: &State, client: ClientId, size: (u16, u16)) -> String {
        let mut screen = Screen::new(size.0, size.1);
        let cx = OverlayCtx {
            client,
            viewport: (0, 0),
            status_row: None,
            window_top: 0,
            panes: Vec::new(),
        };
        GridOverlay.draw(state, client, &cx, &mut screen);
        screen
            .cells
            .chunks(usize::from(size.0))
            .map(|r| r.iter().map(|c| c.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arrows_move_enter_jumps() {
        let (mut state, client, panes, _out) = setup(5, (130, 40));
        open(&mut state, client).expect("open");
        assert_eq!(selected_pane(&state, client), Some(panes[0]));
        press(&mut state, client, "Right");
        let second = selected_pane(&state, client).expect("tile");
        assert_ne!(second, panes[0]);
        press(&mut state, client, "Down");
        let fourth = selected_pane(&state, client).expect("tile");
        assert_ne!(fourth, second);
        press(&mut state, client, "Left");
        press(&mut state, client, "Up");
        assert_eq!(selected_pane(&state, client), Some(panes[0]));
        press(&mut state, client, "Right");
        press(&mut state, client, "Enter");
        assert!(!state.grids.is_open(client));
        let window = state.model.panes[&second].window;
        assert_eq!(state.model.windows[&window].active, second);
        let session = state.clients[&client].session;
        assert_eq!(state.model.sessions[&session].active, window);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn columns_and_scroll() {
        let (mut state, client, _panes, _out) = setup(5, (130, 40));
        let lay = layout(&state, (130, 40));
        assert_eq!(lay.cols, 2);
        // 40 rows hold three tile rows of 13: 5 agents are 3 rows, all shown.
        open(&mut state, client).expect("open");
        assert_eq!(visible_panes(&state, client).len(), 5);
        // A short client scrolls by tile row and keeps the selection shown.
        state.clients.get_mut(&client).expect("client").size = (130, 15);
        for _ in 0..4 {
            press(&mut state, client, "Down");
        }
        let shown = visible_panes(&state, client);
        assert_eq!(shown.len(), 1, "the last tile row holds one tile");
        assert!(shown.contains(&selected_pane(&state, client).expect("tile")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_filters() {
        let (mut state, client, panes, _out) = setup(2, (130, 40));
        open(&mut state, client).expect("open");
        for c in ["b", "l", "o", "c", "k"] {
            press(&mut state, client, c);
        }
        assert_eq!(visible_panes(&state, client), vec![panes[0]]);
        assert!(screen_text(&state, client, (130, 40)).contains("agents> block"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escape_closes() {
        let (mut state, client, _panes, _out) = setup(2, (130, 40));
        open(&mut state, client).expect("open");
        press(&mut state, client, "Escape");
        assert!(!state.grids.is_open(client));
        assert!(visible_panes(&state, client).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_says_no_agents() {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "a", "sleep 600"]);
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
        open(&mut state, accepted.id).expect("open");
        assert!(screen_text(&state, accepted.id, (100, 40)).contains("no agents"));
        press(&mut state, accepted.id, "Enter");
        assert!(state.grids.is_open(accepted.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tile_title_cut_by_width() {
        let (mut state, client, panes, _out) = setup(2, (130, 40));
        state.agents_grid.tile_width = 12;
        open(&mut state, client).expect("open");
        let lay = layout(&state, (130, 40));
        let (first, label) = view(&state, "").remove(0);
        assert!(first == panes[0] && label.chars().count() > lay.tile_w);
        let text = screen_text(&state, client, (130, 40));
        let row: Vec<char> = text.lines().nth(1).expect("title row").chars().collect();
        let cut: String = label.chars().take(lay.tile_w).collect();
        assert_eq!(row[..lay.tile_w].iter().collect::<String>(), cut);
        assert_eq!(row[lay.tile_w], '○', "next tile starts at its own column");
    }
}
