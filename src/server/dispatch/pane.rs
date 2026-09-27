//! pane commands: split, kill, select, resize, respawn, send-keys, capture-pane.

use crate::cmd::{
    self, CapturePane, Command, KillPane, ResizeAction, ResizePane, Resolve, RespawnPane,
    SelectPane, SendKeys, SendKeysItem, SplitWindow,
};
use crate::protocol::ClientCtx;
use crate::server::State;
use crate::server::dispatch::{command_argv, resolve_cwd, spawn_pane};
use crate::server::model::{HISTORY_LIMIT, Model, PANE_BASE_INDEX};
use crate::server::pane::{PaneHandle, PaneSpawn};
use crate::types::{Edge, PaneId, Rect, WindowId};

/// Runs a pane command routed here by `dispatch::route`.
pub fn run(state: &mut State, ctx: &ClientCtx, cmd: Command) -> Result<String, String> {
    match cmd {
        Command::SplitWindow(c) => split_window(state, ctx, c),
        Command::KillPane(c) => kill_pane(state, ctx, c),
        Command::SelectPane(c) => select_pane(state, ctx, c),
        Command::ResizePane(c) => resize_pane(state, ctx, c),
        Command::RespawnPane(c) => respawn_pane(state, ctx, c),
        Command::SendKeys(c) => send_keys(state, ctx, c),
        Command::CapturePane(c) => capture_pane(state, ctx, c),
        _ => Err("not a pane command".to_string()),
    }
}

/// `split-window`: a new pane next to the target, which becomes active.
/// `-c` is expanded against the target pane now (spec §4.5).
fn split_window(state: &mut State, ctx: &ClientCtx, c: SplitWindow) -> Result<String, String> {
    let model = &state.model;
    let target = model.resolve_pane(&c.target, ctx)?;
    let p = &model.panes[&target];
    let window = p.window;
    let size = (p.cols, p.rows);
    let s = &model.sessions[&model.windows[&window].session];
    let env = s.env.clone();
    let cwd = resolve_cwd(model, ctx, Some(target), c.cwd.as_deref(), &s.cwd);

    unzoom(&mut state.model, window);
    let new = spawn_pane(state, window, env, command_argv(c.command), cwd, size)?;
    let model = &mut state.model;
    let Some(w) = model.windows.get_mut(&window) else {
        model.panes.remove(&new);
        return Err(format!("can't find window: {window}"));
    };
    if w.layout
        .split(w.area(), target, c.dir, new, c.size)
        .is_err()
    {
        model.panes.remove(&new);
        return Err("size or position no space for a new pane".to_string());
    }
    w.active = new;
    model.relayout(window);
    Ok(String::new())
}

fn kill_pane(state: &mut State, ctx: &ClientCtx, c: KillPane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    unzoom(model, window);
    model.remove_pane(pane);
    Ok(String::new())
}

/// `select-pane`: the target, or its neighbour across `-L/-R/-U/-D`,
/// becomes its window's active pane. Past the window's edge it wraps
/// around, as tmux does.
fn select_pane(state: &mut State, ctx: &ClientCtx, c: SelectPane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let Some(edge) = c.edge else {
        if let Some(w) = model.windows.get_mut(&window) {
            w.active = pane;
        }
        return Ok(String::new());
    };
    unzoom(model, window);
    let Some(w) = model.windows.get_mut(&window) else {
        return Ok(String::new());
    };
    let area = w.area();
    let next = w
        .layout
        .neighbour(area, pane, edge)
        .or_else(|| wrapped_neighbour(w.layout.compute(area), area, pane, edge));
    if let Some(next) = next {
        w.active = next;
    }
    Ok(String::new())
}

/// The pane `select-pane` wraps to when `pane` has no neighbour across
/// `edge`: one on the window's opposite side, overlapping `pane` on the
/// cross axis (the one under `pane`'s centre line, else the first in
/// layout order).
// ponytail: tmux picks the most recently active of the candidates; with no
// per-pane activity history, the centre line stands in.
fn wrapped_neighbour(
    rects: Vec<(PaneId, Rect)>,
    area: Rect,
    pane: PaneId,
    edge: Edge,
) -> Option<PaneId> {
    let me = rects.iter().find(|(id, _)| *id == pane)?.1;
    // (start, len) of a rect on the cross axis.
    let cross = |r: &Rect| match edge {
        Edge::Left | Edge::Right => (r.y, r.rows),
        Edge::Up | Edge::Down => (r.x, r.cols),
    };
    let on_far_side = |r: &Rect| match edge {
        Edge::Left => r.x + r.cols == area.cols,
        Edge::Right => r.x == 0,
        Edge::Up => r.y + r.rows == area.rows,
        Edge::Down => r.y == 0,
    };
    let (me_start, me_len) = cross(&me);
    let centre = me_start + me_len / 2;
    let candidates: Vec<(PaneId, (u16, u16))> = rects
        .iter()
        .filter(|(id, r)| *id != pane && on_far_side(r))
        .map(|(id, r)| (*id, cross(r)))
        .filter(|(_, (s, l))| *s < me_start + me_len && me_start < s + l)
        .collect();
    candidates
        .iter()
        .find(|(_, (s, l))| (*s..s + l).contains(&centre))
        .or(candidates.first())
        .map(|(id, _)| *id)
}

/// `resize-pane`. As tmux: `-L n` moves the border on the pane's right
/// left by `n` cells, or the border on its left when it has no right
/// neighbour (`-R`, `-U`, `-D` likewise). `-Z` toggles zoom.
fn resize_pane(state: &mut State, ctx: &ClientCtx, c: ResizePane) -> Result<String, String> {
    let model = &mut state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let window = model.panes[&pane].window;
    let (edges, amount) = match c.action {
        ResizeAction::ToggleZoom => {
            toggle_zoom(model, window, pane);
            return Ok(String::new());
        }
        ResizeAction::Edges(edges, amount) => (edges, i32::from(amount)),
    };
    unzoom(model, window);
    let Some(w) = model.windows.get_mut(&window) else {
        return Ok(String::new());
    };
    let area = w.area();
    for edge in edges {
        // The border tmux moves: after the pane if there is one, else before.
        let (after, before, toward_after) = match edge {
            Edge::Left => (Edge::Right, Edge::Left, false),
            Edge::Right => (Edge::Right, Edge::Left, true),
            Edge::Up => (Edge::Down, Edge::Up, false),
            Edge::Down => (Edge::Down, Edge::Up, true),
        };
        let (border, delta) = if w.layout.neighbour(area, pane, after).is_some() {
            (after, if toward_after { amount } else { -amount })
        } else {
            (before, if toward_after { -amount } else { amount })
        };
        w.layout.resize(area, pane, border, delta);
    }
    model.relayout(window);
    Ok(String::new())
}

/// `-Z`: the pane fills the window, or the layout comes back. A window
/// with one pane never zooms, as in tmux.
fn toggle_zoom(model: &mut Model, window: WindowId, pane: PaneId) {
    let Some(w) = model.windows.get_mut(&window) else {
        return;
    };
    if w.zoomed {
        unzoom(model, window);
        return;
    }
    if w.pane_ids().len() < 2 {
        return;
    }
    w.zoomed = true;
    w.active = pane;
    let (cols, rows) = (w.cols, w.rows);
    if let Some(p) = model.panes.get_mut(&pane) {
        p.cols = cols;
        p.rows = rows;
        if let Err(e) = p.handle.resize(cols, rows) {
            eprintln!("grofe server: resizing {pane}: {e}");
        }
    }
}

/// Leaves zoom before a layout change, as tmux does.
fn unzoom(model: &mut Model, window: WindowId) {
    if let Some(w) = model.windows.get_mut(&window)
        && w.zoomed
    {
        w.zoomed = false;
        model.relayout(window);
    }
}

/// `respawn-pane`: a new process in the same pane (same `%N` and
/// `pane_uid`, spec §4.2), running the given command or the pane's resume
/// argv, in the pane's spawn cwd. A live pane needs `-k`.
fn respawn_pane(state: &mut State, ctx: &ClientCtx, c: RespawnPane) -> Result<String, String> {
    let model = &state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let p = &model.panes[&pane];
    if p.dead.is_none() && !c.kill {
        let w = &model.windows[&p.window];
        let index = w.pane_ids().iter().position(|id| *id == pane).unwrap_or(0) as u32;
        let session = &model.sessions[&w.session].name;
        return Err(format!(
            "respawn pane failed: pane {session}:{}.{} still active",
            w.index,
            index + PANE_BASE_INDEX
        ));
    }
    let argv = match c.command {
        Some(command) => command_argv(Some(command)),
        None => p.resume.clone(),
    };
    let spec = PaneSpawn {
        id: pane,
        argv: argv.clone(),
        cwd: p.cwd.clone(),
        env: model.sessions[&model.windows[&p.window].session]
            .env
            .clone(),
        cols: p.cols,
        rows: p.rows,
        history_limit: HISTORY_LIMIT,
        socket: state.socket.clone(),
    };
    let cwd = spec.cwd.display().to_string();
    let handle = PaneHandle::spawn(spec, state.pane_events.clone(), state.wake.clone())
        .map_err(|e| format!("respawn pane failed: can't start pane in {cwd}: {e}"))?;
    if let Some(p) = state.model.panes.get_mut(&pane) {
        // Dropping the old handle closes its master, which hangs up a live
        // process (`-k`), and stops its tasks, so it never reports an exit.
        // ponytail: an `Exited` the old reader already queued before `-k`
        // is indistinguishable from the new process's (the event carries
        // only the pane id); closing that needs a spawn generation in
        // `PaneEvent`.
        p.handle = handle;
        p.dead = None;
        p.argv = argv.clone();
        p.resume = argv;
    }
    Ok(String::new())
}

/// `send-keys`: key names encoded for the pane's current input modes,
/// literals as UTF-8, `-H` as raw bytes, all queued in one write.
fn send_keys(state: &mut State, ctx: &ClientCtx, c: SendKeys) -> Result<String, String> {
    let model = &state.model;
    let pane = model.resolve_pane(&c.target, ctx)?;
    let handle = &model.panes[&pane].handle;
    let modes = handle.modes();
    let mut bytes = Vec::new();
    for item in c.items {
        match item {
            SendKeysItem::Key(name) => match cmd::key_bytes(&name, &modes) {
                Some(b) => bytes.extend(b),
                None => bytes.extend(name.into_bytes()),
            },
            SendKeysItem::Literal(text) => bytes.extend(text.into_bytes()),
            SendKeysItem::Hex(b) => bytes.push(b),
        }
    }
    if bytes.is_empty() {
        return Ok(String::new());
    }
    handle
        .enqueue_input(bytes)
        .map_err(|e| format!("send-keys: {pane}: {e}"))?;
    Ok(String::new())
}

/// `capture-pane`: the pane's text between `-S` and `-E`.
// ponytail: without `-p` tmux fills a paste buffer; there are no buffers
// before M2, so the text is printed either way.
fn capture_pane(state: &mut State, ctx: &ClientCtx, c: CapturePane) -> Result<String, String> {
    let pane = state.model.resolve_pane(&c.target, ctx)?;
    Ok(state.model.panes[&pane].handle.capture(&c.opts))
}
