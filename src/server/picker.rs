//! The agent picker, choose-agent (agents spec §5.2).
//!
//! A centred list of agent panes, most urgent first, with the selected pane
//! previewed live below. Rows are rebuilt from `State.agents` on every key
//! and every draw, so a state change shows at once.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::agents::{Agent, rank};
use super::clients;
use super::dispatch::agent;
use super::dispatch::options::{FormatCtx, ServerVars};
use super::input::{Input, InputHook, Routed};
use super::listdraw::{Rows, centred, draw_list};
use super::listview::{ListAction, ListState, filter};
use super::render::{Overlay, OverlayCtx, Screen};
use super::{State, passthrough, prompt};
use crate::cmd::expand;
use crate::protocol::Dialect;
use crate::types::{ClientId, PaneId, Rect};

/// Width and height of the picker, percent of the client.
const WIDTH_PCT: u16 = 80;
const HEIGHT_PCT: u16 = 70;

/// Open pickers, per client. Lives on `State.pickers`.
#[derive(Default)]
pub struct Pickers {
    open: BTreeMap<ClientId, ListState>,
    /// When a shown time next changes (see [`deadline`]).
    at: Option<Instant>,
}

impl Pickers {
    pub fn is_open(&self, id: ClientId) -> bool {
        self.open.contains_key(&id)
    }
}

/// The agent rows for a query: panes, their columns, and the display order.
struct View {
    panes: Vec<PaneId>,
    rows: Rows,
    visible: Vec<usize>,
}

pub fn open(state: &mut State, client: ClientId) -> Result<(), String> {
    if !state.clients.contains_key(&client) {
        return Err("no current client".to_string());
    }
    state.pickers.open.insert(client, ListState::default());
    rearm(state, Instant::now());
    clients::mark_dirty(state, client);
    Ok(())
}

/// A client that detached takes its picker with it.
pub fn client_gone(state: &mut State, id: ClientId) {
    state.pickers.open.remove(&id);
    rearm(state, Instant::now());
}

/// When the open pickers next need a redraw for their times.
pub fn deadline(state: &State) -> Option<Instant> {
    state.pickers.at
}

/// Redraws the open pickers when a shown time has changed.
pub fn tick(state: &mut State, now: Instant) {
    if !state.pickers.at.is_some_and(|at| at <= now) {
        return;
    }
    let ids: Vec<ClientId> = state.pickers.open.keys().copied().collect();
    for id in ids {
        clients::mark_dirty(state, id);
    }
    rearm(state, now);
}

/// Sets the next time change: the next whole second of the shortest time
/// shown under a minute, minute under an hour, else hour.
fn rearm(state: &mut State, now: Instant) {
    state.pickers.at = if state.pickers.open.is_empty() {
        None
    } else {
        state
            .agents
            .by_urgency()
            .into_iter()
            .filter(|p| state.model.panes.contains_key(p))
            .filter_map(|p| state.agents.get(p))
            .map(|a| {
                let secs = now.saturating_duration_since(a.since).as_secs();
                let step = match secs {
                    0..60 => 1,
                    60..3600 => 60,
                    _ => 3600,
                };
                a.since + Duration::from_secs((secs / step + 1) * step)
            })
            .min()
    };
}

/// The pane under the selection, if the picker is open and lists any.
pub fn selected_pane(state: &State, client: ClientId) -> Option<PaneId> {
    let st = state.pickers.open.get(&client)?;
    let v = view(state, &st.query);
    let at = *v.visible.get(st.selected)?;
    v.panes.get(at).copied()
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
    use super::agents::Rank;
    match rank(a) {
        Rank::Blocked => "⏸",
        Rank::Error => "✗",
        Rank::Unseen => "✓",
        Rank::Working => "●",
        Rank::Idle => "○",
        Rank::Done => "✓",
    }
}

fn home_short(state: &State, pane: PaneId) -> String {
    let Some(p) = state.model.panes.get(&pane) else {
        return String::new();
    };
    let cwd = p.cwd.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && cwd.starts_with(&home) => {
            format!("~{}", &cwd[home.len()..])
        }
        _ => cwd,
    }
}

fn view(state: &State, query: &str) -> View {
    let mut panes = Vec::new();
    let mut cols = Vec::new();
    for pane in state.agents.by_urgency() {
        let (Some(a), true) = (
            state.agents.get(pane),
            state.model.panes.contains_key(&pane),
        ) else {
            continue;
        };
        panes.push(pane);
        cols.push(vec![
            format!("{} {}", glyph(a), rank(a).as_str()),
            age(a.since),
            target_label(state, pane),
            a.kind.clone().unwrap_or_default(),
            a.blocked
                .clone()
                .or_else(|| a.msg.clone())
                .unwrap_or_default(),
            home_short(state, pane),
        ]);
    }
    let texts: Vec<String> = cols.iter().map(|c| c.join(" ")).collect();
    let visible = filter(&texts, query);
    if panes.is_empty() {
        return View {
            panes,
            rows: Rows {
                cols: vec![vec!["no agents".to_string()]],
            },
            visible: vec![0],
        };
    }
    View {
        panes,
        rows: Rows { cols },
        visible,
    }
}

/// The list box and the preview box under it, for a client of `size`.
fn layout(size: (u16, u16)) -> (Rect, Rect) {
    let area = centred(size, WIDTH_PCT, HEIGHT_PCT);
    let list_rows = (area.rows * 2 / 5).max(5).min(area.rows);
    let list = Rect {
        rows: list_rows,
        ..area
    };
    let preview = Rect {
        y: area.y + list_rows,
        rows: area.rows - list_rows,
        ..area
    };
    (list, preview)
}

/// Rows of the list box that hold agents: the box less its border and the
/// query line.
fn list_height(size: (u16, u16)) -> usize {
    usize::from(layout(size).0.rows).saturating_sub(3)
}

pub struct PickerHook;

impl InputHook for PickerHook {
    fn on_input(
        &self,
        state: &mut State,
        client: ClientId,
        input: &Input,
        _now: Instant,
    ) -> Option<Routed> {
        if !state.pickers.is_open(client) {
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
        let mut st = state.pickers.open.get(&client).cloned()?;
        let count = view(state, &st.query).visible.len();
        st.clamp(count, list_height(size));
        let action = st.on_key(key, count, true);
        let count = view(state, &st.query).visible.len();
        st.clamp(count, list_height(size));
        state.pickers.open.insert(client, st);
        clients::mark_dirty(state, client);
        let pane = selected_pane(state, client);
        Some(match action {
            ListAction::Close => close(state, client),
            ListAction::Enter => match pane {
                Some(pane) => enter(state, client, pane),
                None => Routed::Consumed,
            },
            ListAction::Ctrl(c) => ctrl(state, client, pane, c),
            _ => Routed::Consumed,
        })
    }
}

fn close(state: &mut State, client: ClientId) -> Routed {
    state.pickers.open.remove(&client);
    rearm(state, Instant::now());
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

/// `C-x` kill (asks first), `C-y` copy the target, `C-s` mark seen.
fn ctrl(state: &mut State, client: ClientId, pane: Option<PaneId>, c: char) -> Routed {
    let Some(pane) = pane else {
        return Routed::Consumed;
    };
    match c {
        'x' => {
            prompt::ask(
                state,
                client,
                &format!("Kill pane {}? (y/n) ", target_label(state, pane)),
                "",
                "command",
                true,
                Box::new(move |_state, _client, text| {
                    if text.eq_ignore_ascii_case("y") {
                        let argv = ["kill-pane", "-t", &pane.to_string()].map(String::from);
                        Routed::Command(vec![argv.to_vec()], Dialect::Grofe)
                    } else {
                        Routed::Consumed
                    }
                }),
            );
        }
        'y' => {
            let label = target_label(state, pane);
            passthrough::send_selection(state, client, label.as_bytes());
        }
        's' if state.agents.mark_seen(pane) => clients::mark_all_dirty(state),
        _ => {}
    }
    Routed::Consumed
}

pub struct PickerOverlay;

impl Overlay for PickerOverlay {
    fn draw(&self, state: &State, client: ClientId, _cx: &OverlayCtx, screen: &mut Screen) {
        let Some(st) = state.pickers.open.get(&client) else {
            return;
        };
        let (list, preview) = layout((screen.cols, screen.rows));
        let v = view(state, &st.query);
        draw_list(screen, list, "choose-agent", &v.rows, &v.visible, st, true);
        screen.cursor = None;
        if preview.rows < 3 {
            return;
        }
        let pane = v
            .visible
            .get(st.selected)
            .and_then(|&at| v.panes.get(at))
            .copied();
        let title = pane.map(|p| target_label(state, p)).unwrap_or_default();
        draw_list(
            screen,
            preview,
            &title,
            &Rows::default(),
            &[],
            &ListState::default(),
            false,
        );
        if let Some(pane) = pane {
            draw_preview(state, pane, preview, screen);
        }
    }
}

/// The bottom of `pane`'s screen inside the preview box's border.
fn draw_preview(state: &State, pane: PaneId, area: Rect, screen: &mut Screen) {
    let Some(p) = state.model.panes.get(&pane) else {
        return;
    };
    let snap = p.handle.snapshot_rows(0..p.rows);
    let (w, h) = (
        usize::from(area.cols).saturating_sub(2),
        usize::from(area.rows).saturating_sub(2),
    );
    let first = snap.rows.len().saturating_sub(h);
    for (j, row) in snap.rows[first..].iter().enumerate() {
        for (i, cell) in row.iter().take(w).enumerate() {
            // A wide character cut at the edge would lose its spacer.
            if i + 1 == w && cell.flags.contains(super::pane::CellFlags::WIDE_CHAR) {
                continue;
            }
            let (x, y) = (usize::from(area.x) + 1 + i, usize::from(area.y) + 1 + j);
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

    // The producers (agents, jump, listview, listdraw) are real code here;
    // the only seam is the clock, which the tests do not need to fake.

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

    /// A state with panes %a and %b in session `a`, a client on it, and the
    /// first pane blocked, the second idle.
    fn setup() -> (
        State,
        ClientId,
        PaneId,
        PaneId,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let mut state = test_state();
        run(&mut state, &["new-session", "-d", "-s", "a", "sleep 600"]);
        run(&mut state, &["new-window", "-t", "a", "sleep 600"]);
        let panes: Vec<PaneId> = state.model.panes.keys().copied().collect();
        let (p1, p2) = (panes[0], panes[1]);
        let now = Instant::now();
        state
            .agents
            .apply(p1, report(AgentState::Blocked, Some("ask")), true, now);
        state
            .agents
            .apply(p2, report(AgentState::Idle, None), true, now);
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
        (state, accepted.id, p1, p2, accepted.output)
    }

    fn press(state: &mut State, client: ClientId, name: &str) -> Option<Routed> {
        let key = parse_key(name).expect("key");
        PickerHook.on_input(state, client, &Input::Key(key), Instant::now())
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
        PickerOverlay.draw(state, client, &cx, &mut screen);
        screen
            .cells
            .chunks(100)
            .map(|r| r.iter().map(|c| c.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enter_jumps_and_closes() {
        let (mut state, client, p1, _p2, _out) = setup();
        open(&mut state, client).expect("open");
        assert_eq!(selected_pane(&state, client), Some(p1));
        press(&mut state, client, "Enter");
        assert!(!state.pickers.is_open(client));
        let window = state.model.panes[&p1].window;
        assert_eq!(state.model.windows[&window].active, p1);
        let session = state.clients[&client].session;
        assert_eq!(state.model.sessions[&session].active, window);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rows_live_after_state_change() {
        let (mut state, client, p1, p2, _out) = setup();
        open(&mut state, client).expect("open");
        assert_eq!(selected_pane(&state, client), Some(p1));
        // The idle pane starts to block: it now outranks nothing older, but
        // p1 going idle puts p2 first.
        let now = Instant::now();
        state
            .agents
            .apply(p1, report(AgentState::Idle, None), true, now);
        state
            .agents
            .apply(p2, report(AgentState::Blocked, None), true, now);
        assert_eq!(selected_pane(&state, client), Some(p2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_x_asks_then_kills() {
        let (mut state, client, p1, _p2, _out) = setup();
        open(&mut state, client).expect("open");
        press(&mut state, client, "C-x");
        assert!(state.prompts.is_open(client), "asks first");
        assert!(state.model.panes.contains_key(&p1));
        let key = parse_key("y").expect("key");
        let routed = crate::server::prompt::PromptHook
            .on_input(&mut state, client, &Input::Key(key), Instant::now())
            .expect("prompt answers");
        let Routed::Command(cmds, _) = routed else {
            panic!("a command");
        };
        let ctx = state.clients[&client].ctx.clone();
        dispatch::run(&mut state, &ctx, &cmds[0]).expect("kill");
        assert!(!state.model.panes.contains_key(&p1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_x_declined_keeps_pane() {
        let (mut state, client, p1, _p2, _out) = setup();
        open(&mut state, client).expect("open");
        press(&mut state, client, "C-x");
        let key = parse_key("n").expect("key");
        let routed = crate::server::prompt::PromptHook
            .on_input(&mut state, client, &Input::Key(key), Instant::now())
            .expect("prompt answers");
        assert!(matches!(routed, Routed::Consumed));
        assert!(state.model.panes.contains_key(&p1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_y_copies_target() {
        let (mut state, client, p1, _p2, mut out) = setup();
        open(&mut state, client).expect("open");
        press(&mut state, client, "C-y");
        clients::tick(&mut state, Instant::now());
        let bytes = out.try_recv().expect("a frame");
        let label = target_label(&state, p1);
        let osc = crate::server::passthrough::osc52("", label.as_bytes());
        assert!(
            bytes.windows(osc.len()).any(|w| w == osc.as_slice()),
            "OSC 52 of the target"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_s_marks_seen() {
        let (mut state, client, p1, _p2, _out) = setup();
        state
            .agents
            .apply(p1, report(AgentState::Done, None), false, Instant::now());
        assert!(state.agents.get(p1).is_some_and(|a| a.unseen));
        open(&mut state, client).expect("open");
        let first = selected_pane(&state, client).expect("a row");
        // Rows sort by rank; the unseen pane is whichever row is selected.
        let unseen = state.agents.iter().find(|(_, a)| a.unseen).map(|(p, _)| p);
        assert_eq!(unseen, Some(p1));
        if first != p1 {
            press(&mut state, client, "Down");
        }
        press(&mut state, client, "C-s");
        assert!(!state.agents.get(p1).expect("record").unseen);
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
        assert!(screen_text(&state, accepted.id).contains("no agents"));
        assert_eq!(selected_pane(&state, accepted.id), None);
        // Enter on the placeholder neither jumps nor closes.
        press(&mut state, accepted.id, "Enter");
        assert!(state.pickers.is_open(accepted.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_of_selected() {
        let (mut state, client, p1, p2, _out) = setup();
        open(&mut state, client).expect("open");
        // The preview box is the last one drawn: its top edge carries the
        // selected pane's target.
        let edge = |state: &State| {
            screen_text(state, client)
                .lines()
                .rfind(|l| l.contains('╭'))
                .map(String::from)
                .unwrap_or_default()
        };
        let title = |state: &State, p| format!(" {} ", target_label(state, p));
        assert!(edge(&state).contains(&title(&state, p1)));
        press(&mut state, client, "Down");
        assert_eq!(selected_pane(&state, client), Some(p2));
        assert!(edge(&state).contains(&title(&state, p2)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closes_when_client_detaches() {
        let (mut state, client, _p1, _p2, _out) = setup();
        open(&mut state, client).expect("open");
        state.clients.remove(&client);
        let routed = press(&mut state, client, "Down");
        assert!(routed.is_none(), "keys go on");
        assert!(!state.pickers.is_open(client));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typing_filters_and_escape_closes() {
        let (mut state, client, _p1, p2, _out) = setup();
        open(&mut state, client).expect("open");
        for c in ["i", "d", "l", "e"] {
            press(&mut state, client, c);
        }
        assert_eq!(selected_pane(&state, client), Some(p2), "idle matches");
        press(&mut state, client, "Escape");
        assert!(!state.pickers.is_open(client));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detach_removes_picker() {
        let (mut state, client, _p1, _p2, _out) = setup();
        open(&mut state, client).expect("open");
        clients::detach(&mut state, client);
        assert!(!state.pickers.is_open(client));
        assert_eq!(deadline(&state), None);
    }
}
